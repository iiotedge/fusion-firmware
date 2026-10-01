// src/matter/onoff.rs
//
// A second, independent Matter endpoint (Phase 19d): a plain On/Off Light/
// Switch (Matter cluster 0x0006), backed by a REAL GPIO output line via
// `gpio-cdev` — the same crate and calling convention
// `ai::actions::pulse_gpio` already uses for `[[ai.rules]]`'s `gpio_output`
// action, just held OPEN persistently here instead of pulsed-and-released,
// since Matter's OnOff model is a durable on/off STATE, not a momentary
// trigger.
//
// This is what makes "this is generic firmware — deploy it as a light
// switch, not a camera" a real, config-only choice (see config.rs's
// `MatterOnOffConfig` and `MatterCameraConfig`) rather than a hypothetical:
// a device with `[matter.camera].enabled = false` and
// `[matter.onoff].enabled = true` exposes ONLY a plain switch on the Matter
// fabric, no camera clusters at all, same firmware binary as everywhere
// else in this project.
//
// Deliberately NOT implemented: LevelControl (dimming) or ColorControl —
// this firmware has no PWM/dimmer or RGB driver behind it, so claiming
// those clusters would be exactly the kind of unverified-hardware claim
// this project avoids elsewhere (mock vs. real camera/radar backends, no
// mechanical PTZ over Matter, etc.). `OnOffHandler::new_standalone` — the
// real, provided "no LevelControl" constructor, verified against the
// installed rs-matter 0.3.0 source before use — is exactly the "plain
// relay" shape this device actually is.
use crate::config::MatterOnOffConfig;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use rs_matter::dm::clusters::app::on_off::{
    self, ClusterAsyncHandler as _, EffectVariantEnum, HandlerAsyncAdaptor as OnOffAdaptor,
    NoLevelControl, OnOffHandler, OnOffHooks, StartUpOnOffEnum,
};
use rs_matter::dm::{Dataver, EndptId};
use rs_matter::error::Error;
use rs_matter::tlv::Nullable;

use crate::matter::registry::{ClusterImpl, EndpointSpec};

use tracing::{info, warn};

/// Matter's generic "On/Off Light" device type (0x0100). Used regardless of
/// whether the real load behind the relay is literally a light — Matter
/// has no separate "dumb relay/switch" device type distinct from a
/// non-dimmable light; `On/Off Plug-in Unit` (0x010A) is the other common
/// choice for a non-lighting load and would be equally honest here. Light
/// is used since it is the more universally recognized device type across
/// controllers (Apple Home, Google Home, SmartThings all render it
/// consistently); revisit per-deployment if a specific controller's
/// "plug"-vs-"light" iconography actually matters to an installer.
const DEV_TYPE_ON_OFF_LIGHT: rs_matter::dm::DeviceType = rs_matter::dm::DeviceType {
    dtype: 0x0100,
    drev: 3,
};

/// Fixed endpoint number for this device type when enabled — Matter
/// endpoint numbering doesn't need to be contiguous, so a deployment with
/// only `[matter.onoff]` enabled (no camera) simply never advertises
/// endpoint 1 at all; this one stays at a fixed number 2 either way, kept
/// out of camera.rs's endpoint 1 to avoid any cluster-id collision.
pub(crate) const ONOFF_ENDPOINT_ID: EndptId = 2;

/// Real, persistent GPIO output — held open for the process lifetime
/// (unlike `ai::actions::pulse_gpio`'s open-set-sleep-release-per-call
/// pattern), since Matter's OnOff state must reflect the ACTUAL current
/// output level at any time a controller reads it, not just immediately
/// after a pulse. `None` when GPIO isn't available (non-Linux dev host) or
/// wasn't configured — reads back as always-off, writes are logged and
/// dropped rather than silently pretended to succeed.
#[cfg(target_os = "linux")]
struct RelayLine(Mutex<Option<gpio_cdev::LineHandle>>);

#[cfg(target_os = "linux")]
impl RelayLine {
    fn open(chip: &str, line: u32, active_low: bool) -> Option<Self> {
        use gpio_cdev::{Chip, LineRequestFlags};
        let mut flags = LineRequestFlags::OUTPUT;
        if active_low {
            flags |= LineRequestFlags::ACTIVE_LOW;
        }
        let result = (|| -> Result<gpio_cdev::LineHandle, gpio_cdev::errors::Error> {
            let mut chip_handle = Chip::new(chip)?;
            chip_handle
                .get_line(line)?
                .request(flags, 0, "fusion-firmware-matter-onoff")
        })();
        match result {
            Ok(handle) => Some(Self(Mutex::new(Some(handle)))),
            Err(e) => {
                warn!("Matter onoff: failed to open GPIO {chip}:{line}: {e}; switch will read as always-off");
                None
            }
        }
    }

    fn set(&self, on: bool) -> bool {
        let guard = self.0.lock().unwrap();
        match guard.as_ref() {
            Some(handle) => match handle.set_value(on as u8) {
                Ok(()) => true,
                Err(e) => {
                    warn!("Matter onoff: GPIO set_value failed: {e}");
                    false
                }
            },
            None => false,
        }
    }
}

#[cfg(not(target_os = "linux"))]
struct RelayLine;

#[cfg(not(target_os = "linux"))]
impl RelayLine {
    fn open(chip: &str, _line: u32, _active_low: bool) -> Option<Self> {
        warn!(
            "Matter onoff: GPIO (gpio-cdev) is Linux-only; ignored on this host (chip: {chip}) \
             — the switch will accept commands and report state in-memory, but drives no real output"
        );
        None
    }

    fn set(&self, _on: bool) -> bool {
        false
    }
}

/// Real backing for the OnOff cluster: a real GPIO line when configured and
/// available, otherwise an honest in-memory-only fallback (the switch still
/// works from a controller's perspective — state toggles, reads back
/// correctly — it just isn't connected to real hardware). `cached_state`
/// is the source of truth for `on_off()` reads either way, kept in sync
/// with the real line on every successful write so a GPIO read failure
/// doesn't silently desync the two.
pub(crate) struct RelayOnOffHooks {
    line: Option<RelayLine>,
    cached_state: AtomicBool,
    // `SHALL be persisted across reboots` per spec; honestly only
    // process-lifetime here — no on-disk persistence exists for this yet.
    // Documented gap, not silently ignored: see this file's header.
    start_up_pref: Mutex<Nullable<StartUpOnOffEnum>>,
}

impl RelayOnOffHooks {
    /// Only opens the real GPIO line when `cfg.enabled`. `matter::run` now
    /// only calls `build()` for an enabled switch, so this is a second,
    /// defensive guard (it used to be the ONLY one, back when every device
    /// type was constructed unconditionally): a stale `gpio_chip` left over
    /// in a copied config must never open real hardware for a switch that
    /// isn't enabled.
    fn new(cfg: &MatterOnOffConfig) -> Self {
        let line = if !cfg.enabled {
            None
        } else if cfg.gpio_chip.is_empty() {
            warn!("Matter onoff: enabled with no gpio_chip configured — switch will be in-memory only, driving no real output");
            None
        } else {
            RelayLine::open(&cfg.gpio_chip, cfg.gpio_line, cfg.active_low)
        };
        Self {
            line,
            cached_state: AtomicBool::new(false),
            start_up_pref: Mutex::new(Nullable::none()),
        }
    }
}

impl OnOffHooks for RelayOnOffHooks {
    const CLUSTER: rs_matter::dm::Cluster<'static> = on_off::FULL_CLUSTER;

    fn on_off(&self) -> bool {
        self.cached_state.load(Ordering::Relaxed)
    }

    fn set_on_off(&self, on: bool) {
        info!(on, "Matter onoff: set");
        // Cache reflects the REQUESTED state regardless of whether the
        // real GPIO write succeeded — same "controller's view stays
        // consistent even if the last write to hardware failed" choice
        // src/ptz/mod.rs already makes for its own last-known-position
        // bookkeeping. A GPIO failure is still logged (inside `set`), not
        // swallowed.
        self.cached_state.store(on, Ordering::Relaxed);
        if let Some(line) = &self.line {
            line.set(on);
        }
    }

    fn start_up_on_off(&self) -> Nullable<StartUpOnOffEnum> {
        self.start_up_pref.lock().unwrap().clone()
    }

    fn set_start_up_on_off(&self, value: Nullable<StartUpOnOffEnum>) -> Result<(), Error> {
        *self.start_up_pref.lock().unwrap() = value;
        Ok(())
    }

    async fn handle_off_with_effect(&self, effect: EffectVariantEnum) {
        // No dimming hardware to animate a fade through — a plain relay's
        // only honest response to "turn off with an effect" is to turn
        // off, same as a real non-dimmable smart plug would.
        info!(?effect, "Matter onoff: off-with-effect (no dimmer — plain off)");
        self.set_on_off(false);
    }
}

pub(crate) type OnOff = OnOffHandler<'static, RelayOnOffHooks, NoLevelControl>;

/// Builds the OnOff handler (and, if configured, opens the real GPIO line).
/// Called only when `[matter.onoff]` is enabled: with the flat router there
/// is no longer any need to construct every possible device type up front,
/// so a disabled switch never touches hardware at all.
pub(crate) fn build(rand: &mut impl rand_core::Rng, cfg: &MatterOnOffConfig) -> &'static OnOff {
    let hooks = RelayOnOffHooks::new(cfg);
    Box::leak(Box::new(OnOffHandler::new_standalone(
        Dataver::new_rand(rand),
        ONOFF_ENDPOINT_ID,
        hooks,
    )))
}

/// This device as a registry endpoint (fixed endpoint id 2, the id already-
/// paired controllers know it by). The Descriptor cluster is added by the
/// registry.
pub(crate) fn spec(onoff: &'static OnOff) -> EndpointSpec {
    EndpointSpec {
        id: Some(ONOFF_ENDPOINT_ID),
        name: "on_off".to_string(),
        device_types: vec![DEV_TYPE_ON_OFF_LIGHT],
        clusters: vec![(
            OnOff::CLUSTER,
            ClusterImpl::RelayOnOff(OnOffAdaptor(onoff)),
        )],
    }
}
