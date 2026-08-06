// src/runtime_config.rs
//
// Remote-settable AI/automation config — the write half of what
// `config_get` / `GET /footprint` already expose read-only. Reached over
// either the MQTT command channel (`config_get_ai_rules`/`config_set_ai_rules`,
// src/commands.rs) or `GET`/`POST /config/ai-rules` (src/core/metrics.rs) —
// both funnel through `apply_and_persist` below so neither channel can
// accept something the other would reject, and neither can accept something
// a fresh boot from the TOML file wouldn't also accept: this reuses
// `config::validate_ai_rules`, the exact function `config::validate` itself
// calls at boot.
//
// Trust model: gated by the SAME `[security].command_token` bearer check
// every other command-channel/HTTP-write action already uses (reboot,
// export, stream_start, ptz_move, ...) — this doesn't introduce a new trust
// boundary, it extends an already-high-trust credential to one more thing
// it can do. It also doesn't grant any capability a technician with
// filesystem access to the device didn't already have via the static TOML
// (an installer could already point `webhook_url`/`gpio_chip` anywhere);
// what changes is WHO can set it and how far away they can be.
//
// Persistence mirrors src/identity.rs's already-established pattern
// (persist to a file, re-read it at next boot, ignore/warn on anything
// invalid or missing rather than failing to boot) applied to a richer
// payload than a single string: `[system].ai_rules_override_file` holds the
// last remotely-applied rule set as JSON. `resolve()` below runs from
// main.rs right after `identity::resolve()` — the same spot in the boot
// sequence — and, if a valid override exists, REPLACES `config.ai.rules`
// for that run; a missing, corrupt, or now-invalid override file just
// means the TOML's own `ai.rules` wins, with a WARN explaining why (a bad
// remote push must never brick the device, same philosophy as a
// missing/incompatible AI model never crash-looping the camera).
//
// Live application: main.rs's analytics thread owns `RuleEngine` as a
// plain `&mut`-owned value, not `Mutex`-shared state like `PtzController`
// (deliberate — see that thread's own comments on the two different
// concurrency shapes this firmware uses). So an update hands off through
// `RuleUpdateSlot` instead: the command/HTTP handler thread calls `set()`,
// the analytics thread polls `take_pending()` once per frame and rebuilds
// `RuleEngine` from the new rule set — cheap enough to check every frame,
// and avoids taking a lock for the entire evaluate() hot path the way
// wrapping the whole engine in a `Mutex` would.
//
// Known limitation, by design not oversight: a BRAND-NEW rule's Home
// Assistant discovery topic will NOT appear until the next reboot (HA
// discovery configs publish once at startup off the boot-time rule name
// list, src/homeassistant.rs) — editing an EXISTING rule's zone/schedule/
// enabled state still reflects live, since that reuses the already-
// discovered topic. Full dynamic HA re-discovery is a larger, separate
// piece of work than "let the AI rules themselves be remotely settable,"
// and wasn't asked for.
use crate::config::{validate_ai_rules, AiRule, AppConfig};
use parking_lot::Mutex;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

/// Cross-thread handoff: the command/HTTP handler thread sets a new rule
/// list here; the analytics thread consumes (and clears) it once per frame.
/// `Clone` is cheap (an `Arc` bump) so every consumer (MQTT command context,
/// HTTP server context, the analytics thread) holds its own handle to the
/// same underlying slot.
#[derive(Clone, Default)]
pub struct RuleUpdateSlot(Arc<Mutex<Option<Vec<AiRule>>>>);

impl RuleUpdateSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Analytics-thread side: returns the new rule set exactly once if one
    /// is pending, otherwise `None`. Cheap enough to call every frame —
    /// `parking_lot::Mutex` has no poisoning to handle and this is held for
    /// a single `Option::take()`, never across any I/O or inference work.
    pub fn take_pending(&self) -> Option<Vec<AiRule>> {
        self.0.lock().take()
    }
}

/// Boot-time: if a previously-applied override exists at
/// `config.system.ai_rules_override_file` and still validates against this
/// boot's `ai.labels`, it replaces `config.ai.rules` for this run. Called
/// from main.rs right after `identity::resolve()`.
pub fn resolve(config: &mut AppConfig) {
    let path = config.system.ai_rules_override_file.clone();
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        // No override yet is the common, expected case on a device that's
        // never had a remote rule change applied — nothing to log.
        Err(_) => return,
    };
    let rules: Vec<AiRule> = match serde_json::from_str(&contents) {
        Ok(r) => r,
        Err(e) => {
            warn!("Ignoring corrupt ai.rules override at {path}: {e}");
            return;
        }
    };
    match validate_ai_rules(&rules, &config.ai.labels) {
        Ok(()) => {
            info!(
                count = rules.len(),
                path = %path,
                "Applying persisted ai.rules override from a prior remote config change"
            );
            config.ai.rules = rules;
        }
        Err(e) => warn!(
            "Ignoring invalid persisted ai.rules override at {path} ({e}); \
             keeping this device's own config file's ai.rules for this boot"
        ),
    }
}

/// Validates, persists to `override_path`, and hands `rules` to the running
/// analytics thread via `slot`. Returns the number of rules applied, or the
/// same actionable error `config::validate_ai_rules` would give — nothing
/// is written to disk or applied unless validation passes first.
///
/// Two things this deliberately guards against, found by an adversarial
/// review of the first version of this function rather than up front:
///
/// - **No-op resubmissions must not touch the live engine.** If `rules`
///   is identical to what's already in effect, persisting and handing off
///   again would still cause the analytics thread to rebuild `RuleEngine`
///   from scratch — which resets every in-flight loiter dwell timer and
///   line-cross side-state (`ai/rules.rs`'s `CompiledRule.tracks`), since
///   that engine has no way to know "this is the same ruleset, don't
///   reset." A fleet config-sync loop re-applying desired state on a
///   schedule (or anyone simply polling the endpoint) would otherwise
///   silently suppress loitering alerts by resetting timers faster than
///   `dwell_s` on every call. So: compare against `current_rules()` first,
///   and treat an identical resubmission as a successful no-op.
/// - **Two near-simultaneous callers (MQTT + HTTP) must not interleave.**
///   The check-then-persist-then-apply sequence runs while holding the
///   slot's own lock (not just the final handoff), so two callers racing
///   each other can't leave disk holding one caller's rules while the
///   live engine ends up running the other's.
pub fn apply_and_persist(
    rules: Vec<AiRule>,
    labels: &[String],
    override_path: &str,
    static_config_path: &str,
    slot: &RuleUpdateSlot,
) -> Result<usize, String> {
    validate_ai_rules(&rules, labels)?;
    let count = rules.len();

    let mut pending = slot.0.lock();
    if rules == current_rules(override_path, static_config_path) {
        return Ok(count);
    }
    persist(override_path, &rules).map_err(|e| format!("failed to persist override: {e}"))?;
    info!(
        count,
        path = %override_path,
        "ai.rules updated via remote config endpoint"
    );
    *pending = Some(rules);
    Ok(count)
}

/// Best-effort "what's actually running" view for `config_get_ai_rules` /
/// `GET /config/ai-rules`: the persisted override if one exists (the exact
/// truth once any remote update has ever been applied), otherwise whatever
/// is baked into the static config file at `static_config_path`.
pub fn current_rules(override_path: &str, static_config_path: &str) -> Vec<AiRule> {
    if let Ok(contents) = fs::read_to_string(override_path) {
        if let Ok(rules) = serde_json::from_str::<Vec<AiRule>>(&contents) {
            return rules;
        }
    }
    fs::read_to_string(static_config_path)
        .ok()
        .and_then(|c| toml::from_str::<AppConfig>(&c).ok())
        .map(|c| c.ai.rules)
        .unwrap_or_default()
}

fn persist(path: &str, rules: &[AiRule]) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string_pretty(rules).unwrap_or_else(|_| "[]".to_string());
    fs::write(path, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence_rule(name: &str) -> AiRule {
        AiRule {
            name: name.to_string(),
            enabled: true,
            classes: vec!["person".to_string()],
            min_confidence: None,
            zone: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]],
            mode: "presence".to_string(),
            direction: String::new(),
            dwell_s: 0,
            schedule: Default::default(),
            actions: vec!["snapshot".to_string()],
            webhook_url: String::new(),
            gpio_chip: String::new(),
            gpio_line: 0,
            gpio_pulse_ms: 500,
        }
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "runtime-config-test-{name}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn apply_and_persist_rejects_invalid_rule_without_writing_or_applying() {
        let path = temp_path("invalid");
        let slot = RuleUpdateSlot::new();
        let mut bad_rule = presence_rule("bad");
        bad_rule.mode = "not_a_real_mode".to_string();

        let err = apply_and_persist(
            vec![bad_rule],
            &[],
            path.to_str().unwrap(),
            "/nonexistent/config.toml",
            &slot,
        )
        .expect_err("invalid mode must be rejected");
        assert!(err.contains("mode must be"));
        assert!(!path.exists(), "must not persist an invalid rule set");
        assert!(
            slot.take_pending().is_none(),
            "must not apply an invalid rule set"
        );
    }

    #[test]
    fn apply_and_persist_persists_and_hands_off_a_valid_rule_set() {
        let path = temp_path("valid");
        let slot = RuleUpdateSlot::new();

        let count = apply_and_persist(
            vec![presence_rule("front_door")],
            &["person".to_string()],
            path.to_str().unwrap(),
            "/nonexistent/config.toml",
            &slot,
        )
        .expect("valid rule set must be accepted");

        assert_eq!(count, 1);
        assert!(path.exists(), "must persist to the override file");
        let pending = slot
            .take_pending()
            .expect("must hand off to the analytics thread");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "front_door");
        assert!(
            slot.take_pending().is_none(),
            "a pending update is consumed exactly once"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_and_persist_skips_persist_and_handoff_on_a_no_op_resubmission() {
        let path = temp_path("noop");
        let slot = RuleUpdateSlot::new();
        let labels = ["person".to_string()];

        apply_and_persist(
            vec![presence_rule("front_door")],
            &labels,
            path.to_str().unwrap(),
            "/nonexistent/config.toml",
            &slot,
        )
        .expect("first apply must succeed");
        // Consume the first handoff, then re-submit the EXACT same rule set
        // -- e.g. a fleet config-sync loop reconciling to the same desired
        // state, or a client polling the endpoint.
        slot.take_pending();
        let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();

        let count = apply_and_persist(
            vec![presence_rule("front_door")],
            &labels,
            path.to_str().unwrap(),
            "/nonexistent/config.toml",
            &slot,
        )
        .expect("an identical resubmission is a successful no-op, not an error");

        assert_eq!(count, 1);
        assert!(
            slot.take_pending().is_none(),
            "an unchanged rule set must NOT be handed to the analytics thread -- \
             doing so would reset every in-flight loiter/line-cross track for no reason"
        );
        let mtime_after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "an unchanged rule set must not rewrite the override file either"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_and_persist_applies_a_genuine_change_even_after_a_prior_no_op() {
        let path = temp_path("real-change");
        let slot = RuleUpdateSlot::new();
        let labels = ["person".to_string()];

        apply_and_persist(
            vec![presence_rule("front_door")],
            &labels,
            path.to_str().unwrap(),
            "/nonexistent/config.toml",
            &slot,
        )
        .expect("first apply must succeed");
        slot.take_pending();

        let count = apply_and_persist(
            vec![presence_rule("back_door")],
            &labels,
            path.to_str().unwrap(),
            "/nonexistent/config.toml",
            &slot,
        )
        .expect("a genuinely different rule set must still apply");

        assert_eq!(count, 1);
        let pending = slot
            .take_pending()
            .expect("a real change must still be handed to the analytics thread");
        assert_eq!(pending[0].name, "back_door");

        std::fs::remove_file(&path).ok();
    }

    /// Loads a real, already-validated shipped preset rather than
    /// hand-constructing an `AppConfig` — every field stays satisfied as
    /// the schema evolves, same fixture the preset-regression test in
    /// config.rs itself relies on.
    fn a_real_app_config() -> AppConfig {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config/presets/home-front-door-security.toml");
        crate::config::load_config(path).expect("shipped preset must load")
    }

    #[test]
    fn resolve_ignores_a_missing_override_file() {
        let mut config = a_real_app_config();
        config.system.ai_rules_override_file = temp_path("missing").to_string_lossy().to_string();
        let original = config.ai.rules.clone();

        resolve(&mut config);

        assert_eq!(config.ai.rules.len(), original.len());
    }

    #[test]
    fn resolve_ignores_a_corrupt_override_file() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "not valid json").unwrap();
        let mut config = a_real_app_config();
        config.system.ai_rules_override_file = path.to_string_lossy().to_string();
        let original = config.ai.rules.clone();

        resolve(&mut config);

        assert_eq!(config.ai.rules.len(), original.len());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn resolve_applies_a_valid_persisted_override() {
        let path = temp_path("apply");
        std::fs::write(
            &path,
            serde_json::to_string(&vec![presence_rule("persisted_rule")]).unwrap(),
        )
        .unwrap();
        let mut config = a_real_app_config();
        config.system.ai_rules_override_file = path.to_string_lossy().to_string();
        config.ai.labels = vec!["person".to_string()];

        resolve(&mut config);

        assert_eq!(config.ai.rules.len(), 1);
        assert_eq!(config.ai.rules[0].name, "persisted_rule");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn current_rules_prefers_the_override_over_the_static_config() {
        let override_path = temp_path("current");
        std::fs::write(
            &override_path,
            serde_json::to_string(&vec![presence_rule("from_override")]).unwrap(),
        )
        .unwrap();

        let rules = current_rules(override_path.to_str().unwrap(), "/nonexistent/config.toml");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "from_override");
        std::fs::remove_file(&override_path).ok();
    }

    #[test]
    fn current_rules_falls_back_to_empty_when_neither_source_exists() {
        let rules = current_rules("/nonexistent/override.json", "/nonexistent/config.toml");
        assert!(rules.is_empty());
    }
}
