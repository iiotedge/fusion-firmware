// src/correlation.rs
//
// Machine-data ↔ video correlation (F7): the original Industry 4.0 ask —
// "when the machine says X, which frames show it?"
//
// Mechanism: a `Processor` registered in the iiotedge-lib engine's ingest
// path sees every southbound machine event (serial, CAN, Modbus, …) BEFORE
// persistence. Config rules match on source id (and optionally payload
// content); hits cross a bounded channel to the analytics thread, which pairs
// them with the current frame — capture timestamp against machine timestamp —
// and fires the evidence actions (snapshot, clip, event index, GDE
// correlated_event with both timestamps and their delta).
//
// The Processor contract demands cheap, non-blocking work: matching is a
// prefix + substring check and a try_send; the heavy lifting happens on the
// analytics thread.
use crate::config::CorrelationConfig;

use crossbeam::channel::{bounded, Receiver, Sender, TrySendError};
use iiotedge_core::traits::Processor;
use iiotedge_core::types::UnifiedPayload;
use tracing::{debug, info, warn};

/// Camera-origin events use this source prefix; never correlate against our
/// own telemetry (feedback loop).
const OWN_SOURCE_PREFIX: &str = "camera/";
/// Cap on the payload excerpt carried into the correlated event.
const PREVIEW_BYTES: usize = 256;

/// A machine event that matched a rule, en route to the analytics thread.
pub struct CorrelationHit {
    pub rule: String,
    pub source_id: String,
    pub machine_timestamp_ns: u64,
    pub payload_preview: String,
    pub snapshot: bool,
    pub clip: bool,
}

pub struct CorrelationProcessor {
    rules: Vec<CompiledRule>,
    tx: Sender<CorrelationHit>,
}

struct CompiledRule {
    name: String,
    source_prefix: String,
    contains: Option<String>,
    snapshot: bool,
    clip: bool,
}

impl CorrelationProcessor {
    /// Returns None when disabled or no rules are configured (inert).
    /// The receiver end goes to the analytics thread.
    pub fn new(
        cfg: &CorrelationConfig,
    ) -> Option<(std::sync::Arc<Self>, Receiver<CorrelationHit>)> {
        if !cfg.enabled || cfg.rules.is_empty() {
            return None;
        }
        let rules: Vec<CompiledRule> = cfg
            .rules
            .iter()
            .map(|r| CompiledRule {
                name: r.name.clone(),
                source_prefix: r.source_prefix.clone(),
                contains: (!r.contains.is_empty()).then(|| r.contains.clone()),
                snapshot: r.snapshot,
                clip: r.clip,
            })
            .collect();
        info!(
            rules = rules.len(),
            tolerance_ms = cfg.tolerance_ms,
            "Machine-data correlation active"
        );
        // Bounded: a machine-event storm must never back-pressure the lib's
        // ingest path; overflow hits are dropped with a warning.
        let (tx, rx) = bounded(64);
        Some((std::sync::Arc::new(Self { rules, tx }), rx))
    }

    fn match_rule(&self, payload: &UnifiedPayload) -> Option<&CompiledRule> {
        if payload.source_id.starts_with(OWN_SOURCE_PREFIX) {
            return None;
        }
        self.rules.iter().find(|rule| {
            payload.source_id.starts_with(&rule.source_prefix)
                && match &rule.contains {
                    Some(needle) => contains_bytes(&payload.payload, needle.as_bytes()),
                    None => true,
                }
        })
    }
}

impl Processor for CorrelationProcessor {
    fn process(&self, payload: UnifiedPayload) -> Option<UnifiedPayload> {
        if let Some(rule) = self.match_rule(&payload) {
            let preview_len = payload.payload.len().min(PREVIEW_BYTES);
            let hit = CorrelationHit {
                rule: rule.name.clone(),
                source_id: payload.source_id.clone(),
                machine_timestamp_ns: payload.timestamp_ns,
                payload_preview: String::from_utf8_lossy(&payload.payload[..preview_len])
                    .into_owned(),
                snapshot: rule.snapshot,
                clip: rule.clip,
            };
            match self.tx.try_send(hit) {
                Ok(()) => debug!(rule = %rule.name, source = %payload.source_id, "correlation hit"),
                Err(TrySendError::Full(_)) => {
                    warn!("correlation queue full; dropping hit for '{}'", rule.name)
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
        // Always pass the event through — correlation observes, never filters.
        Some(payload)
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CorrelationRule;
    use bytes::Bytes;
    use iiotedge_core::types::{ContentType, ProtocolType};

    fn processor(
        rules: Vec<CorrelationRule>,
    ) -> (
        std::sync::Arc<CorrelationProcessor>,
        Receiver<CorrelationHit>,
    ) {
        CorrelationProcessor::new(&CorrelationConfig {
            enabled: true,
            tolerance_ms: 200,
            rules,
        })
        .expect("enabled with rules")
    }

    fn machine_event(source: &str, body: &str) -> UnifiedPayload {
        UnifiedPayload::now(
            source,
            ProtocolType::HostApp,
            ContentType::Text,
            Bytes::from(body.to_string()),
        )
    }

    #[test]
    fn matches_prefix_and_content_and_passes_through() {
        let (proc_, rx) = processor(vec![CorrelationRule {
            name: "reject".into(),
            source_prefix: "serial/scanner1".into(),
            contains: "REJECT".into(),
            snapshot: true,
            clip: true,
        }]);

        // Non-matching content: no hit, event passes through.
        let passed = proc_.process(machine_event("serial/scanner1/line", "OK part 42"));
        assert!(passed.is_some());
        assert!(rx.try_recv().is_err());

        // Matching event → one hit with preview.
        proc_.process(machine_event("serial/scanner1/line", "REJECT part 43"));
        let hit = rx.try_recv().expect("hit expected");
        assert_eq!(hit.rule, "reject");
        assert!(hit.payload_preview.contains("REJECT"));
        assert!(hit.snapshot && hit.clip);
    }

    #[test]
    fn never_matches_own_camera_events() {
        let (proc_, rx) = processor(vec![CorrelationRule {
            name: "everything".into(),
            source_prefix: "".into(),
            contains: String::new(),
            snapshot: false,
            clip: false,
        }]);
        proc_.process(machine_event("camera/dev1/ai_event", "{}"));
        assert!(rx.try_recv().is_err(), "own events must never correlate");
    }
}
