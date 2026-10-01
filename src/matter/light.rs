// src/matter/light.rs
//
// A third, independent Matter endpoint (Phase 19e): a full "Extended Color
// Light" (Matter device type 0x010D) — On/Off + LevelControl (dimming) +
// ColorControl (hue/saturation, XY, color temperature, color loop), coupled
// together the way a real Matter bulb is (turning it on restores the last
// level, `MoveToLevelWithOnOff` implicitly turns it on, etc.).
//
// This is deliberately a SEPARATE endpoint/device slot from onoff.rs's
// plain relay/switch (own endpoint id 3, no cluster-id collision with
// endpoint 2's OnOff) rather than a "mode" of it: a deployment can enable
// the relay, this light, both, or neither, independently. See
// `[matter.light]` in config.rs.
//
// Hardware reality, stated plainly: this board has no PWM/dimmer or RGB/CT
// LED driver today. Every attribute and command below is real and
// spec-compliant — verified against the installed rs-matter 0.3.0 source,
// mirroring its own internal test fixtures
// (`on_off::test::TestOnOffDeviceLogic`, `level_control::test::
// TestLevelControlDeviceLogic`, `color_control::test::
// TestColorControlDeviceLogic`) almost verbatim for the cluster metadata,
// since those are the crate's own known-good configurations — a controller
// can turn this light on/off, dim it, and set its hue/saturation/XY/color
// temperature and see it stick. It just doesn't drive a physical light
// yet. `set_device_level`/`set_device_color` below are the two functions
// to extend the day a real PWM or addressable-LED driver exists — same
// "real hardware if configured, honest in-memory fallback otherwise"
// pattern `onoff::RelayOnOffHooks` already uses for the relay.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use rs_matter::dm::clusters::app::color_control::{
    self, ClusterAsyncHandler as _, ColorCapabilitiesBitmap, ColorControlHandler,
    ColorControlHooks, HandlerAsyncAdaptor as ColorAdaptor,
    AttributeDefaults as ColorAttributeDefaults, SetDeviceColor,
};
use rs_matter::dm::clusters::app::level_control::{
    self, AttributeDefaults as LevelAttributeDefaults, ClusterAsyncHandler as _,
    HandlerAsyncAdaptor as LevelAdaptor, LevelControlHandler, LevelControlHooks,
};
use rs_matter::dm::clusters::app::on_off::{
    self, ClusterAsyncHandler as _, EffectVariantEnum, HandlerAsyncAdaptor as OnOffAdaptor,
    OnOffHandler, OnOffHooks, StartUpOnOffEnum,
};
use rs_matter::dm::devices::DEV_TYPE_EXTENDED_COLOR_LIGHT;
use rs_matter::dm::{Cluster, Dataver, EndptId};
use rs_matter::error::Error;
use rs_matter::tlv::Nullable;
use rs_matter::with;

use crate::matter::registry::{ClusterImpl, EndpointSpec};

use tracing::info;

/// Fixed endpoint number, independent of `onoff::ONOFF_ENDPOINT_ID` (2) and
/// the camera's endpoint 1 — no cluster-id collisions
/// regardless of which subset of `[matter.camera]`/`[matter.onoff]`/
/// `[matter.light]` a given deployment enables.
pub(crate) const LIGHT_ENDPOINT_ID: EndptId = 3;

/// On/Off half of the light, LIGHTING-featured (Matter's "On/Off Light"
/// device type mandates the `LT` feature — see `DEV_TYPE_EXTENDED_COLOR_LIGHT`
/// which composes it). Cluster metadata mirrors rs-matter's own
/// `on_off::test::TestOnOffDeviceLogic` exactly.
pub(crate) struct LightOnOffHooks {
    on: AtomicBool,
    start_up_pref: Mutex<Nullable<StartUpOnOffEnum>>,
}

impl LightOnOffHooks {
    fn new() -> Self {
        Self {
            on: AtomicBool::new(false),
            start_up_pref: Mutex::new(Nullable::none()),
        }
    }
}

impl OnOffHooks for LightOnOffHooks {
    const CLUSTER: Cluster<'static> = on_off::FULL_CLUSTER
        .with_revision(6)
        .with_features(on_off::Feature::LIGHTING.bits())
        .with_attrs(with!(
            required;
            on_off::AttributeId::OnOff
                | on_off::AttributeId::GlobalSceneControl
                | on_off::AttributeId::OnTime
                | on_off::AttributeId::OffWaitTime
                | on_off::AttributeId::StartUpOnOff
        ))
        .with_cmds(with!(
            on_off::CommandId::Off
                | on_off::CommandId::On
                | on_off::CommandId::Toggle
                | on_off::CommandId::OffWithEffect
                | on_off::CommandId::OnWithRecallGlobalScene
                | on_off::CommandId::OnWithTimedOff
        ));

    fn on_off(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    fn set_on_off(&self, on: bool) {
        info!(on, "Matter light: on/off set");
        self.on.store(on, Ordering::Relaxed);
    }

    fn start_up_on_off(&self) -> Nullable<StartUpOnOffEnum> {
        self.start_up_pref.lock().unwrap().clone()
    }

    fn set_start_up_on_off(&self, value: Nullable<StartUpOnOffEnum>) -> Result<(), Error> {
        *self.start_up_pref.lock().unwrap() = value;
        Ok(())
    }

    async fn handle_off_with_effect(&self, effect: EffectVariantEnum) {
        info!(?effect, "Matter light: off-with-effect (in-memory only, no dimmer to animate a fade)");
        self.set_on_off(false);
    }
}

/// Dimming. Feature set (`ON_OFF` only, matching rs-matter's own
/// `level_control::test::TestLevelControlDeviceLogic` fixture) couples
/// `MoveToLevelWithOnOff`/etc. with the OnOff cluster above.
pub(crate) struct InMemoryLevelHooks {
    current_level: Mutex<Option<u8>>,
    start_up_level: Mutex<Option<u8>>,
}

impl InMemoryLevelHooks {
    fn new() -> Self {
        Self {
            // Matter's `LIGHTING`-adjacent convention: a light defaults to
            // "full brightness" rather than `None` (which reads as
            // "unknown/not applicable").
            current_level: Mutex::new(Some(254)),
            start_up_level: Mutex::new(None),
        }
    }
}

impl LevelControlHooks for InMemoryLevelHooks {
    const MIN_LEVEL: u8 = 1;
    const MAX_LEVEL: u8 = 254;
    const FASTEST_RATE: u8 = 50;
    const CLUSTER: Cluster<'static> = level_control::FULL_CLUSTER
        .with_revision(6)
        .with_features(level_control::Feature::ON_OFF.bits())
        .with_attrs(with!(
            required;
            level_control::AttributeId::CurrentLevel
                | level_control::AttributeId::MinLevel
                | level_control::AttributeId::MaxLevel
                | level_control::AttributeId::OnLevel
                | level_control::AttributeId::Options
        ))
        .with_cmds(with!(
            level_control::CommandId::MoveToLevel
                | level_control::CommandId::Move
                | level_control::CommandId::Step
                | level_control::CommandId::Stop
                | level_control::CommandId::MoveToLevelWithOnOff
                | level_control::CommandId::MoveWithOnOff
                | level_control::CommandId::StepWithOnOff
                | level_control::CommandId::StopWithOnOff
        ));

    fn set_device_level(&self, level: u8) -> Result<Option<u8>, ()> {
        info!(level, "Matter light: level set (in-memory only, no PWM/dimmer driver)");
        Ok(Some(level))
    }

    fn current_level(&self) -> Option<u8> {
        *self.current_level.lock().unwrap()
    }

    fn set_current_level(&self, level: Option<u8>) {
        *self.current_level.lock().unwrap() = level;
    }

    fn start_up_current_level(&self) -> Result<Option<u8>, Error> {
        Ok(*self.start_up_level.lock().unwrap())
    }

    fn set_start_up_current_level(&self, value: Option<u8>) -> Result<(), Error> {
        *self.start_up_level.lock().unwrap() = value;
        Ok(())
    }
}

/// Color: hue+saturation, XY, color temperature, and color loop all
/// enabled — the full feature set, matching rs-matter's own
/// `color_control::test::TestColorControlDeviceLogic` fixture and exactly
/// what `DEV_TYPE_EXTENDED_COLOR_LIGHT`'s own doc comment says it mandates.
/// `set_device_color` just records whichever color space the controller
/// (or the cluster's own transition/color-loop engine) last resolved to —
/// this is the one function a real RGB/CT LED driver would take over.
pub(crate) struct InMemoryColorHooks {
    last: Mutex<SetDeviceColor>,
    start_up_ct_mireds: Mutex<Nullable<u16>>,
}

impl InMemoryColorHooks {
    fn new() -> Self {
        Self {
            last: Mutex::new(SetDeviceColor::HueSaturation {
                enhanced_hue: 0,
                saturation: 0,
            }),
            start_up_ct_mireds: Mutex::new(Nullable::none()),
        }
    }
}

impl ColorControlHooks for InMemoryColorHooks {
    const CLUSTER: Cluster<'static> = color_control::FULL_CLUSTER
        .with_features(
            color_control::Feature::HUE_AND_SATURATION.bits()
                | color_control::Feature::ENHANCED_HUE.bits()
                | color_control::Feature::COLOR_LOOP.bits()
                | color_control::Feature::XY.bits()
                | color_control::Feature::COLOR_TEMPERATURE.bits(),
        )
        .with_attrs(with!(
            required;
            color_control::AttributeId::CurrentHue
                | color_control::AttributeId::CurrentSaturation
                | color_control::AttributeId::RemainingTime
                | color_control::AttributeId::CurrentX
                | color_control::AttributeId::CurrentY
                | color_control::AttributeId::ColorTemperatureMireds
                | color_control::AttributeId::ColorMode
                | color_control::AttributeId::Options
                | color_control::AttributeId::NumberOfPrimaries
                | color_control::AttributeId::EnhancedCurrentHue
                | color_control::AttributeId::EnhancedColorMode
                | color_control::AttributeId::ColorLoopActive
                | color_control::AttributeId::ColorLoopDirection
                | color_control::AttributeId::ColorLoopTime
                | color_control::AttributeId::ColorLoopStartEnhancedHue
                | color_control::AttributeId::ColorLoopStoredEnhancedHue
                | color_control::AttributeId::ColorCapabilities
                | color_control::AttributeId::ColorTempPhysicalMinMireds
                | color_control::AttributeId::ColorTempPhysicalMaxMireds
                | color_control::AttributeId::CoupleColorTempToLevelMinMireds
                | color_control::AttributeId::StartUpColorTemperatureMireds
        ))
        .with_cmds(with!(
            color_control::CommandId::MoveToHue
                | color_control::CommandId::MoveHue
                | color_control::CommandId::StepHue
                | color_control::CommandId::MoveToSaturation
                | color_control::CommandId::MoveSaturation
                | color_control::CommandId::StepSaturation
                | color_control::CommandId::MoveToHueAndSaturation
                | color_control::CommandId::MoveToColor
                | color_control::CommandId::MoveColor
                | color_control::CommandId::StepColor
                | color_control::CommandId::MoveToColorTemperature
                | color_control::CommandId::EnhancedMoveToHue
                | color_control::CommandId::EnhancedMoveHue
                | color_control::CommandId::EnhancedStepHue
                | color_control::CommandId::EnhancedMoveToHueAndSaturation
                | color_control::CommandId::ColorLoopSet
                | color_control::CommandId::StopMoveStep
                | color_control::CommandId::MoveColorTemperature
                | color_control::CommandId::StepColorTemperature
        ));

    const COLOR_CAPABILITIES: ColorCapabilitiesBitmap = ColorCapabilitiesBitmap::from_bits_truncate(
        ColorCapabilitiesBitmap::HUE_SATURATION.bits()
            | ColorCapabilitiesBitmap::ENHANCED_HUE.bits()
            | ColorCapabilitiesBitmap::COLOR_LOOP.bits()
            | ColorCapabilitiesBitmap::XY.bits()
            | ColorCapabilitiesBitmap::COLOR_TEMPERATURE.bits(),
    );

    // A plausible tunable-white range (roughly 6500K-2000K) for a virtual
    // bulb with no real physical limits — not measured from any real LED,
    // just a sane default so `ColorTempPhysicalMin/MaxMireds` aren't 0/0.
    const COLOR_TEMP_PHYSICAL_MIN_MIREDS: u16 = 153;
    const COLOR_TEMP_PHYSICAL_MAX_MIREDS: u16 = 500;

    fn set_device_color(&self, target: SetDeviceColor) -> Result<(), ()> {
        info!(?target, "Matter light: color set (in-memory only, no RGB/CT driver)");
        *self.last.lock().unwrap() = target;
        Ok(())
    }

    fn start_up_color_temperature_mireds(&self) -> Result<Nullable<u16>, Error> {
        Ok(self.start_up_ct_mireds.lock().unwrap().clone())
    }

    fn set_start_up_color_temperature_mireds(&self, value: Nullable<u16>) -> Result<(), Error> {
        *self.start_up_ct_mireds.lock().unwrap() = value;
        Ok(())
    }
}

pub(crate) type LightOnOff = OnOffHandler<'static, LightOnOffHooks, InMemoryLevelHooks>;
pub(crate) type LightLevel = LevelControlHandler<'static, InMemoryLevelHooks, LightOnOffHooks>;
pub(crate) type LightColor =
    ColorControlHandler<'static, InMemoryColorHooks, LightOnOffHooks, InMemoryLevelHooks>;

pub(crate) struct LightHandlers {
    pub(crate) onoff: &'static LightOnOff,
    pub(crate) level: &'static LightLevel,
    pub(crate) color: &'static LightColor,
}

/// Builds and cross-couples the three handlers (On/Off, Level, Color) that
/// make up the light endpoint. Only called when `[matter.light]` is enabled.
pub(crate) fn build(rand: &mut impl rand_core::Rng) -> LightHandlers {
    let onoff: &'static LightOnOff = Box::leak(Box::new(OnOffHandler::new(
        Dataver::new_rand(rand),
        LIGHT_ENDPOINT_ID,
        LightOnOffHooks::new(),
    )));
    let level: &'static LightLevel = Box::leak(Box::new(LevelControlHandler::new(
        Dataver::new_rand(rand),
        LIGHT_ENDPOINT_ID,
        InMemoryLevelHooks::new(),
        LevelAttributeDefaults::new(),
    )));
    let color: &'static LightColor = Box::leak(Box::new(ColorControlHandler::new(
        Dataver::new_rand(rand),
        LIGHT_ENDPOINT_ID,
        InMemoryColorHooks::new(),
        ColorAttributeDefaults::new(),
    )));

    // Cross-couple now that all three are 'static: turning the light on/off
    // interacts correctly with its last level, `MoveToLevelWithOnOff`
    // implicitly turns it on, etc. — real Matter Lighting-device behavior,
    // not something a controller has to fake by sending both commands.
    onoff.init(Some(level));
    level.init(Some(onoff));
    color.init(Some(onoff));

    LightHandlers { onoff, level, color }
}

/// This device as a registry endpoint (fixed endpoint id 3). The Descriptor
/// cluster is added by the registry; OnOff, LevelControl and ColorControl are
/// listed in the order they have always been advertised.
pub(crate) fn spec(light: &LightHandlers) -> EndpointSpec {
    EndpointSpec {
        id: Some(LIGHT_ENDPOINT_ID),
        name: "light".to_string(),
        device_types: vec![DEV_TYPE_EXTENDED_COLOR_LIGHT],
        clusters: vec![
            (
                LightOnOff::CLUSTER,
                ClusterImpl::LightOnOff(OnOffAdaptor(light.onoff)),
            ),
            (
                LightLevel::CLUSTER,
                ClusterImpl::LightLevel(LevelAdaptor(light.level)),
            ),
            (
                LightColor::CLUSTER,
                ClusterImpl::LightColor(ColorAdaptor(light.color)),
            ),
        ],
    }
}
