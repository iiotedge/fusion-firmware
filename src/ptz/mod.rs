// src/ptz/mod.rs
//
// PTZ (pan/tilt/zoom) motor control: a `PtzDriver` trait + registry
// mirroring the camera HAL (src/hal/mod.rs) exactly — one backend module
// per protocol, selected by config, so adding a new PTZ protocol never
// touches the ONVIF service or MQTT command call sites. Pelco-D
// (src/ptz/pelco_d.rs) is the only backend today; ONVIF passthrough (relay
// commands to an upstream IP camera's own PTZ service, for rtsp_in-proxied
// cameras) is planned once that HAL backend exists (TODO.md Phase 2) — same
// `register_driver()` call, no rewrite of this controller or its callers.
pub mod pelco_d;

use crate::config::PtzConfig;
use crate::core::error::{EdgeError, EdgeResult};

use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// One PTZ backend. Speeds follow ONVIF's own convention (-1.0..=1.0, sign
/// is direction, 0.0 is stopped on that axis) so the ONVIF service and the
/// MQTT command handler both speak this directly — no per-backend
/// translation layer.
pub trait PtzDriver: Send {
    fn continuous_move(&mut self, pan: f32, tilt: f32, zoom: f32) -> EdgeResult<()>;
    fn stop(&mut self) -> EdgeResult<()>;
    /// Recalls a position the mechanism itself stored (Pelco-D presets live
    /// on the PTZ head, not in this firmware).
    fn goto_preset(&mut self, preset: u8) -> EdgeResult<()>;
    fn set_preset(&mut self, preset: u8) -> EdgeResult<()>;
}

type PtzCtor = fn(&PtzConfig) -> EdgeResult<Box<dyn PtzDriver>>;

fn registry() -> &'static Mutex<HashMap<&'static str, PtzCtor>> {
    static REGISTRY: std::sync::OnceLock<Mutex<HashMap<&'static str, PtzCtor>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Register a PTZ backend under `name` (matched against `[ptz].driver`).
/// Mirrors `hal::register_source` — see that module's doc comment for the
/// reasoning (re-registration replaces rather than errors, builtins
/// registered fresh on every construction rather than needing a separate
/// startup step).
pub(crate) fn register_driver(name: &'static str, ctor: PtzCtor) {
    registry().lock().insert(name, ctor);
}

fn register_builtins() {
    register_driver("pelco_d", |cfg| {
        pelco_d::PelcoDDriver::new(cfg).map(|d| Box::new(d) as Box<dyn PtzDriver>)
    });
}

/// Thread-safe PTZ handle shared by the ONVIF PTZ service and the MQTT
/// command channel — both dispatch through this one instance, so there is a
/// single source of truth for "is it currently moving" and one safety
/// watchdog, not two independent ones racing each other.
pub struct PtzController {
    driver: Mutex<Box<dyn PtzDriver>>,
    move_timeout: Duration,
    /// Timestamp of the most recent `continuous_move` call while still
    /// moving; `None` once stopped. The safety watchdog auto-stops once
    /// this goes stale by `move_timeout` — see `spawn_safety_watchdog`.
    last_move_at: Mutex<Option<Instant>>,
    /// Presets this firmware has *set*, purely for ONVIF's `GetPresets` —
    /// Pelco-D is write-only (no readback), so a preset configured before
    /// this process started, or by another controller, won't appear here.
    presets: Mutex<BTreeSet<u8>>,
}

impl PtzController {
    /// `None` when disabled — every call site holds an `Option<Arc<Self>>`
    /// and treats "no PTZ configured" as a normal, expected state.
    pub fn new(cfg: &PtzConfig) -> EdgeResult<Option<Arc<Self>>> {
        if !cfg.enabled {
            return Ok(None);
        }
        register_builtins();
        let ctor = registry().lock().get(cfg.driver.as_str()).copied();
        let ctor = ctor.ok_or_else(|| {
            EdgeError::HardwareFault(format!(
                "unknown ptz.driver '{}' (built-in: pelco_d)",
                cfg.driver
            ))
        })?;
        let driver = ctor(cfg)?;

        let controller = Arc::new(Self {
            driver: Mutex::new(driver),
            move_timeout: Duration::from_secs(cfg.move_timeout_s.max(1)),
            last_move_at: Mutex::new(None),
            presets: Mutex::new(BTreeSet::new()),
        });
        Arc::clone(&controller).spawn_safety_watchdog();
        info!(driver = %cfg.driver, device = %cfg.serial_device, "PTZ controller ready");
        Ok(Some(controller))
    }

    /// A client that sends ContinuousMove and then crashes, loses network,
    /// or simply never sends Stop would otherwise leave the mechanism
    /// moving indefinitely — a real safety concern for a physical motor,
    /// not just a protocol nicety. Polls at a coarse interval since this is
    /// a last-resort safety net, not a tight control loop.
    fn spawn_safety_watchdog(self: Arc<Self>) {
        let spawned = std::thread::Builder::new()
            .name("ptz_watchdog".to_string())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(500));
                let stale = self
                    .last_move_at
                    .lock()
                    .is_some_and(|since| since.elapsed() > self.move_timeout);
                if stale {
                    warn!("PTZ ContinuousMove timed out with no refresh/Stop; auto-stopping");
                    if let Err(e) = self.stop() {
                        warn!("PTZ safety auto-stop failed: {e}");
                    }
                }
            });
        if let Err(e) = spawned {
            warn!("Failed to spawn PTZ safety watchdog (move_timeout_s will not be enforced): {e}");
        }
    }

    pub fn continuous_move(&self, pan: f32, tilt: f32, zoom: f32) -> EdgeResult<()> {
        self.driver.lock().continuous_move(pan, tilt, zoom)?;
        *self.last_move_at.lock() = Some(Instant::now());
        Ok(())
    }

    pub fn stop(&self) -> EdgeResult<()> {
        self.driver.lock().stop()?;
        *self.last_move_at.lock() = None;
        Ok(())
    }

    pub fn goto_preset(&self, preset: u8) -> EdgeResult<()> {
        self.driver.lock().goto_preset(preset)
    }

    pub fn set_preset(&self, preset: u8) -> EdgeResult<()> {
        self.driver.lock().set_preset(preset)?;
        self.presets.lock().insert(preset);
        Ok(())
    }

    pub fn presets(&self) -> Vec<u8> {
        self.presets.lock().iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeDriver {
        moves: Vec<(f32, f32, f32)>,
        stopped: bool,
    }

    impl PtzDriver for FakeDriver {
        fn continuous_move(&mut self, pan: f32, tilt: f32, zoom: f32) -> EdgeResult<()> {
            self.moves.push((pan, tilt, zoom));
            self.stopped = false;
            Ok(())
        }
        fn stop(&mut self) -> EdgeResult<()> {
            self.stopped = true;
            Ok(())
        }
        fn goto_preset(&mut self, _preset: u8) -> EdgeResult<()> {
            Ok(())
        }
        fn set_preset(&mut self, _preset: u8) -> EdgeResult<()> {
            Ok(())
        }
    }

    fn controller_with(driver: FakeDriver) -> PtzController {
        PtzController {
            driver: Mutex::new(Box::new(driver)),
            move_timeout: Duration::from_secs(5),
            last_move_at: Mutex::new(None),
            presets: Mutex::new(BTreeSet::new()),
        }
    }

    #[test]
    fn new_returns_none_when_disabled() {
        let cfg = PtzConfig::default();
        assert!(!cfg.enabled);
        assert!(PtzController::new(&cfg).unwrap().is_none());
    }

    #[test]
    fn new_rejects_unknown_driver() {
        let cfg = PtzConfig {
            enabled: true,
            driver: "not_a_real_driver".to_string(),
            serial_device: "/dev/null".to_string(),
            ..PtzConfig::default()
        };
        assert!(PtzController::new(&cfg).is_err());
    }

    #[test]
    fn set_preset_is_recorded_and_visible() {
        let ctl = controller_with(FakeDriver {
            moves: Vec::new(),
            stopped: true,
        });
        ctl.set_preset(3).unwrap();
        ctl.set_preset(1).unwrap();
        assert_eq!(ctl.presets(), vec![1, 3]);
    }

    #[test]
    fn stop_clears_last_move_at() {
        let ctl = controller_with(FakeDriver {
            moves: Vec::new(),
            stopped: true,
        });
        ctl.continuous_move(1.0, 0.0, 0.0).unwrap();
        assert!(ctl.last_move_at.lock().is_some());
        ctl.stop().unwrap();
        assert!(ctl.last_move_at.lock().is_none());
    }
}
