// src/ai/actions.rs
//
// Executes the two genuinely new action kinds a fired AiRule can request
// (TODO.md Phase 16c) — webhook POST and GPIO output pulse.
// telemetry_event/snapshot/clip/cluster_broadcast reuse existing
// subsystems directly at the main.rs call site (no new code needed for
// those); this module exists only for `webhook` and `gpio_output`.
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{debug, warn};

/// Fire-and-forget webhook dispatch: a bounded channel + one worker
/// thread, mirroring `storage::clips::ClipExtractor`'s shape — a slow or
/// unreachable endpoint must never stall the analytics thread trying to
/// notify it.
pub struct WebhookDispatcher {
    tx: mpsc::SyncSender<(String, serde_json::Value)>,
}

impl WebhookDispatcher {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::sync_channel::<(String, serde_json::Value)>(16);
        let spawned = thread::Builder::new()
            .name("rule_webhook".to_string())
            .spawn(move || {
                for (url, payload) in rx {
                    match ureq::post(&url)
                        .config()
                        .timeout_global(Some(Duration::from_secs(5)))
                        .build()
                        .send_json(&payload)
                    {
                        Ok(_) => debug!(url = %url, "rule webhook delivered"),
                        Err(e) => warn!(url = %url, "rule webhook POST failed: {e}"),
                    }
                }
            });
        if let Err(e) = spawned {
            warn!("failed to spawn rule_webhook thread: {e}");
        }
        Self { tx }
    }

    /// Non-blocking; drops (with a warning) if the queue of 16 pending
    /// requests is already full rather than backing up the caller.
    pub fn request(&self, url: &str, payload: serde_json::Value) {
        if let Err(e) = self.tx.try_send((url.to_string(), payload)) {
            warn!("rule webhook queue full, dropping request to {url}: {e}");
        }
    }
}

/// Pulses a GPIO output line high for `pulse_ms` then releases it, on its
/// own short-lived thread — a multi-hundred-ms sleep must never block the
/// analytics thread. Mirrors `storage::export::spawn_button`'s cfg-gating
/// and error style, but requests OUTPUT instead of INPUT/events.
#[cfg(target_os = "linux")]
pub fn pulse_gpio(chip: String, line: u32, pulse_ms: u64) {
    thread::spawn(move || {
        use gpio_cdev::{Chip, LineRequestFlags};
        let result = (|| -> Result<(), gpio_cdev::errors::Error> {
            let mut chip_handle = Chip::new(&chip)?;
            let handle = chip_handle.get_line(line)?.request(
                LineRequestFlags::OUTPUT,
                0,
                "iiotedge-rule-action",
            )?;
            handle.set_value(1)?;
            thread::sleep(Duration::from_millis(pulse_ms));
            handle.set_value(0)?;
            Ok(())
        })();
        if let Err(e) = result {
            warn!("rule gpio_output pulse on {chip}:{line} failed: {e}");
        }
    });
}

#[cfg(not(target_os = "linux"))]
pub fn pulse_gpio(chip: String, _line: u32, _pulse_ms: u64) {
    warn!("rule gpio_output action requested but GPIO (gpio-cdev) is Linux-only; ignored on this host (chip: {chip})");
}
