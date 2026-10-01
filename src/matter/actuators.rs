// src/matter/actuators.rs
//
// Config-driven Matter actuators (Phase 19g.4): `[[matter.endpoints]]` entries
// of kind on_off_light / on_off_plug / fan, each bound by a `sink` spec to
// whatever makes the command real — a GPIO relay line, a named signal other
// software acts on, or nothing (virtual). See src/signals.rs.
//
// This is the write-side twin of sensors.rs: where a sensor READS a source and
// tells subscribers when it changes, an actuator is COMMANDED by a controller,
// drives its sink, and only then reports the new state.
//
//   on_off_light  Matter "On/Off Light" (0x0100), OnOff with the LIGHTING feature
//   on_off_plug   Matter "On/Off Plug-in Unit" (0x010A), plain OnOff
//   fan           Matter "Fan" (0x002B), FanControl
//
// One device can be several of these at once (a light AND a fan on one board is
// just two entries), and each is its own Matter endpoint with its own sink.
//
// HONESTY RULES (the same ones the sensors follow):
//   * The reported state moves only if the sink ACCEPTED the command. A failed
//     GPIO write leaves the state where it really is; the controller's next
//     read/report shows the truth instead of a requested-but-never-happened
//     value (the older relay in onoff.rs caches the request regardless).
//   * A fan offers only the speeds it really has (`fan_speeds`). One GPIO line
//     is on or off, so a `gpio:` fan is single-speed; offering Low/Medium/High
//     there would just be three names for the same relay state.
//   * Every actuator starts OFF and says so through its sink at boot (a GPIO
//     line is requested already driven off; a signal sink publishes the initial
//     off state), so nothing is left undefined or glitches on at power-up.
//
// KNOWN CONFORMANCE GAP (shared with the legacy light/relay): the spec also
// makes Groups (and, for lights and plugs, Scenes Management) mandatory on
// these device types. rs-matter's Groups needs its `groups` feature, which
// pulls in multicast Groupcast and does not build on the macOS dev host, so the
// endpoints advertise Identify + the device cluster only. Controllers (Apple
// Home, Google Home, matter.js) work fine without them; tracked in TODO.md.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rs_matter::dm::clusters::app::on_off::{
    self, ClusterAsyncHandler as _, EffectVariantEnum, HandlerAsyncAdaptor as OnOffAdaptor,
    NoLevelControl, OnOffHandler, OnOffHooks, StartUpOnOffEnum,
};
use rs_matter::dm::clusters::decl::fan_control::{
    self, FanModeEnum, FanModeSequenceEnum, StepRequest,
};
use rs_matter::dm::{
    Async, Cluster, Dataver, DeviceType, EndptId, InvokeContext, ReadContext, WriteContext,
};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::tlv::Nullable;
use rs_matter::with;

use crate::config::{effective_endpoint_name, MatterEndpointConfig, MatterEndpointKind};
use crate::matter::light::LightOnOffHooks;
use crate::matter::registry::{identify_cluster, ClusterImpl, EndpointSpec};
use crate::signals::{parse_sink_spec, Sink, SignalBus, SinkSpec, Value};

use tracing::{info, warn};

// ---------------------------------------------------------------------------
// On/Off light and plug
// ---------------------------------------------------------------------------

/// On/Off light: the LIGHTING feature (OnTime / OffWaitTime / StartUpOnOff and
/// the timed/effect commands). Same metadata the legacy light uses, which
/// mirrors rs-matter's own known-good test fixture.
const LIGHTING_CLUSTER: Cluster<'static> = <LightOnOffHooks as OnOffHooks>::CLUSTER;

/// On/Off plug: just OnOff and the three basic commands. No LIGHTING feature, so
/// none of its attributes/commands are advertised (advertising them without
/// the feature would itself be non-conformant).
const PLUG_CLUSTER: Cluster<'static> = on_off::FULL_CLUSTER
    .with_revision(6)
    .with_attrs(with!(required))
    .with_cmds(with!(
        on_off::CommandId::Off | on_off::CommandId::On | on_off::CommandId::Toggle
    ));

/// OnOff backing for a light (`LIGHTING = true`) or a plug (`false`): drives
/// its sink, and reports the state the sink last accepted.
pub(crate) struct SinkOnOffHooks<const LIGHTING: bool> {
    sink: Arc<dyn Sink>,
    name: String,
    on: AtomicBool,
    // StartUpOnOff is only exposed with the LIGHTING feature. Spec says it
    // SHALL persist across reboots; honestly process-lifetime only today (the
    // same documented gap as the legacy light/relay).
    start_up_pref: Mutex<Nullable<StartUpOnOffEnum>>,
}

impl<const LIGHTING: bool> SinkOnOffHooks<LIGHTING> {
    fn new(sink: Arc<dyn Sink>, name: &str) -> Self {
        // Say "off" through the sink so a consumer never sees it undefined.
        if let Err(e) = sink.write(Value::Bool(false)) {
            warn!(name = %name, "Matter: initial off write to {} failed: {e}", sink.describe());
        }
        Self {
            sink,
            name: name.to_string(),
            on: AtomicBool::new(false),
            start_up_pref: Mutex::new(Nullable::none()),
        }
    }
}

impl<const LIGHTING: bool> OnOffHooks for SinkOnOffHooks<LIGHTING> {
    const CLUSTER: Cluster<'static> = if LIGHTING { LIGHTING_CLUSTER } else { PLUG_CLUSTER };

    fn on_off(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    fn set_on_off(&self, on: bool) {
        match self.sink.write(Value::Bool(on)) {
            Ok(()) => {
                self.on.store(on, Ordering::Relaxed);
                info!(name = %self.name, on, "Matter: on/off set");
            }
            // The hook can't report failure to the cluster, so leave the state
            // untouched: the next report then shows what the hardware is
            // actually doing rather than what was asked for.
            Err(e) => warn!(
                name = %self.name,
                "Matter: on/off write to {} failed: {e}; state left unchanged",
                self.sink.describe()
            ),
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
        // No dimmer to animate a fade: the honest response to "off with an
        // effect" is a plain off.
        info!(name = %self.name, ?effect, "Matter: off-with-effect (no dimmer — plain off)");
        self.set_on_off(false);
    }
}

pub(crate) type LightOnOff = OnOffHandler<'static, SinkOnOffHooks<true>, NoLevelControl>;
pub(crate) type PlugOnOff = OnOffHandler<'static, SinkOnOffHooks<false>, NoLevelControl>;

// ---------------------------------------------------------------------------
// Fan
// ---------------------------------------------------------------------------

/// The speeds a fan REALLY has — what `FanModeSequence` advertises. Auto/Smart
/// modes are not offered: nothing here implements an automatic fan policy, and
/// advertising one would be a claim with no behaviour behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FanSteps {
    /// Off / High. One speed: a plain relay.
    Single,
    /// Off / Low / High.
    Dual,
    /// Off / Low / Medium / High.
    Triple,
}

impl FanSteps {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "" | "off_high" => Some(Self::Single),
            "off_low_high" => Some(Self::Dual),
            "off_low_med_high" => Some(Self::Triple),
            _ => None,
        }
    }

    pub(crate) fn names() -> &'static str {
        "off_high, off_low_high, off_low_med_high"
    }

    fn sequence(self) -> FanModeSequenceEnum {
        match self {
            Self::Single => FanModeSequenceEnum::OffHigh,
            Self::Dual => FanModeSequenceEnum::OffLowHigh,
            Self::Triple => FanModeSequenceEnum::OffLowMedHigh,
        }
    }

    /// The real speed steps, lowest first, with the percent each one stands for
    /// (a three-speed fan is 33 / 66 / 100 — the same bands other stacks use).
    fn steps(self) -> &'static [(FanModeEnum, u8)] {
        match self {
            Self::Single => &[(FanModeEnum::High, 100)],
            Self::Dual => &[(FanModeEnum::Low, 50), (FanModeEnum::High, 100)],
            Self::Triple => &[
                (FanModeEnum::Low, 33),
                (FanModeEnum::Medium, 66),
                (FanModeEnum::High, 100),
            ],
        }
    }

    /// `FanMode` write -> the state it implies, or `None` when this fan has no
    /// such mode (a constraint error to the controller).
    fn apply_mode(self, mode: FanModeEnum) -> Option<FanState> {
        if mode == FanModeEnum::Off {
            return Some(FanState::OFF);
        }
        let &(_, pct) = self.steps().iter().find(|(m, _)| *m == mode)?;
        Some(FanState {
            mode,
            setting: pct,
            current: pct,
        })
    }

    /// `PercentSetting` write (1..=100) -> the state it implies: the lowest
    /// real step that covers the request. The setting keeps the value the
    /// controller asked for; `current` is the speed the fan will actually run.
    fn apply_percent(self, percent: u8) -> FanState {
        if percent == 0 {
            return FanState::OFF;
        }
        let steps = self.steps();
        let &(mode, step_pct) = steps
            .iter()
            .find(|(_, pct)| *pct >= percent)
            .or_else(|| steps.last())
            .expect("every fan has at least one speed");
        FanState {
            mode,
            setting: percent,
            current: step_pct,
        }
    }
}

/// What the three coupled attributes (FanMode / PercentSetting / PercentCurrent)
/// read as. They always move together, so they live in one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FanState {
    mode: FanModeEnum,
    /// What the controller asked for.
    setting: u8,
    /// The speed the fan actually runs at.
    current: u8,
}

impl FanState {
    const OFF: Self = Self {
        mode: FanModeEnum::Off,
        setting: 0,
        current: 0,
    };
}

pub(crate) struct FanHandler {
    dataver: Dataver,
    steps: FanSteps,
    state: Mutex<FanState>,
    sink: Arc<dyn Sink>,
    name: String,
}

impl FanHandler {
    fn new(dataver: Dataver, steps: FanSteps, sink: Arc<dyn Sink>, name: &str) -> Self {
        // The sink gets the running speed as a percent (0 = off). A GPIO sink
        // reads that as on/off; a `signal:` sink publishes the number.
        if let Err(e) = sink.write(Value::Num(0.0)) {
            warn!(name = %name, "Matter: initial off write to {} failed: {e}", sink.describe());
        }
        Self {
            dataver,
            steps,
            state: Mutex::new(FanState::OFF),
            sink,
            name: name.to_string(),
        }
    }

    /// Move to `next`, driving the sink first. On a sink failure nothing
    /// changes and the controller is told so. Returns the previous state so the
    /// caller can notify exactly the attributes that moved.
    fn transition(&self, next: FanState) -> Result<FanState, Error> {
        let mut state = self.state.lock().unwrap();
        let prev = *state;
        if next.current != prev.current {
            if let Err(e) = self.sink.write(Value::Num(f64::from(next.current))) {
                warn!(
                    name = %self.name,
                    "Matter: fan write to {} failed: {e}; speed left at {}%",
                    self.sink.describe(),
                    prev.current
                );
                return Err(Error::new(ErrorCode::Failure));
            }
            info!(name = %self.name, percent = next.current, "Matter: fan speed set");
        }
        *state = next;
        Ok(prev)
    }

    /// Apply a write and tell subscribers about every attribute it moved — the
    /// written one AND the ones it cascades to (typed setters notify nothing
    /// by themselves).
    fn apply(&self, ctx: impl WriteContext, next: FanState) -> Result<(), Error> {
        let prev = self.transition(next)?;
        if prev.mode != next.mode {
            ctx.notify_own_attr_changed(fan_control::AttributeId::FanMode as _);
        }
        if prev.setting != next.setting {
            ctx.notify_own_attr_changed(fan_control::AttributeId::PercentSetting as _);
        }
        if prev.current != next.current {
            ctx.notify_own_attr_changed(fan_control::AttributeId::PercentCurrent as _);
        }
        Ok(())
    }

    fn snapshot(&self) -> FanState {
        *self.state.lock().unwrap()
    }
}

impl fan_control::ClusterHandler for FanHandler {
    // No MultiSpeed/Auto/Rocking/Wind/Step/AirflowDirection features, so just
    // the four mandatory attributes and no commands.
    const CLUSTER: Cluster<'static> = fan_control::FULL_CLUSTER
        .with_attrs(with!(required))
        .with_cmds(with!());

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn fan_mode(&self, _ctx: impl ReadContext) -> Result<FanModeEnum, Error> {
        Ok(self.snapshot().mode)
    }

    fn fan_mode_sequence(&self, _ctx: impl ReadContext) -> Result<FanModeSequenceEnum, Error> {
        Ok(self.steps.sequence())
    }

    fn percent_setting(&self, _ctx: impl ReadContext) -> Result<Nullable<u8>, Error> {
        Ok(Nullable::some(self.snapshot().setting))
    }

    fn percent_current(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(self.snapshot().current)
    }

    fn set_fan_mode(&self, ctx: impl WriteContext, value: FanModeEnum) -> Result<(), Error> {
        let next = self
            .steps
            .apply_mode(value)
            .ok_or_else(|| Error::new(ErrorCode::ConstraintError))?;
        self.apply(ctx, next)
    }

    fn set_percent_setting(
        &self,
        ctx: impl WriteContext,
        value: Nullable<u8>,
    ) -> Result<(), Error> {
        // null means "automatic" — a mode this fan doesn't have.
        let percent = value
            .into_option()
            .filter(|p| *p <= 100)
            .ok_or_else(|| Error::new(ErrorCode::ConstraintError))?;
        self.apply(ctx, self.steps.apply_percent(percent))
    }

    fn handle_step(&self, _ctx: impl InvokeContext, _request: StepRequest<'_>) -> Result<(), Error> {
        // The STEP feature isn't advertised, so a conformant controller never
        // sends this.
        Err(ErrorCode::CommandNotFound.into())
    }
}

// ---------------------------------------------------------------------------
// Building endpoints
// ---------------------------------------------------------------------------

fn not_an_actuator(kind: &str) -> String {
    format!("kind '{kind}' is not an actuator")
}

/// Build one actuator `[[matter.endpoints]]` entry. Fails (so the caller can
/// skip just this endpoint and keep the rest of the node up) on a sink that
/// won't open or a fan that claims speeds its sink can't deliver.
pub(crate) fn build_endpoint<R: rand_core::Rng>(
    cfg: &MatterEndpointConfig,
    index: usize,
    id: EndptId,
    bus: &SignalBus,
    rand: &mut R,
) -> Result<EndpointSpec, String> {
    let kind = MatterEndpointKind::parse(&cfg.kind)
        .ok_or_else(|| format!("unknown kind '{}'", cfg.kind))?;
    let name = effective_endpoint_name(cfg, index);

    let sink_spec = parse_sink_spec(&cfg.sink)?;
    let sink = bus.resolve_sink(&sink_spec)?;

    let (device_type, meta, imp) = match kind {
        MatterEndpointKind::OnOffLight => {
            let hooks = SinkOnOffHooks::<true>::new(sink.clone(), &name);
            let handler: &'static LightOnOff = Box::leak(Box::new(OnOffHandler::new_standalone(
                Dataver::new_rand(rand),
                id,
                hooks,
            )));
            (
                DeviceType { dtype: 0x0100, drev: 3 },
                LightOnOff::CLUSTER,
                ClusterImpl::SinkLight(OnOffAdaptor(handler)),
            )
        }
        MatterEndpointKind::OnOffPlug => {
            let hooks = SinkOnOffHooks::<false>::new(sink.clone(), &name);
            let handler: &'static PlugOnOff = Box::leak(Box::new(OnOffHandler::new_standalone(
                Dataver::new_rand(rand),
                id,
                hooks,
            )));
            (
                DeviceType { dtype: 0x010A, drev: 4 },
                PlugOnOff::CLUSTER,
                ClusterImpl::SinkPlug(OnOffAdaptor(handler)),
            )
        }
        MatterEndpointKind::Fan => {
            let steps = FanSteps::parse(&cfg.fan_speeds).ok_or_else(|| {
                format!(
                    "unknown fan_speeds '{}' (available: {})",
                    cfg.fan_speeds,
                    FanSteps::names()
                )
            })?;
            if matches!(sink_spec, SinkSpec::Gpio { .. }) && steps != FanSteps::Single {
                return Err(format!(
                    "fan_speeds '{}' needs more than one output level, but a gpio: sink is a single \
                     on/off line — use fan_speeds = \"off_high\" or a signal: sink",
                    cfg.fan_speeds
                ));
            }
            let handler = FanHandler::new(Dataver::new_rand(rand), steps, sink.clone(), &name);
            (
                DeviceType { dtype: 0x002B, drev: 4 },
                <FanHandler as fan_control::ClusterHandler>::CLUSTER,
                ClusterImpl::Fan(Async(fan_control::HandlerAdaptor(handler))),
            )
        }
        MatterEndpointKind::Temperature
        | MatterEndpointKind::Humidity
        | MatterEndpointKind::Pressure
        | MatterEndpointKind::Flow
        | MatterEndpointKind::Illuminance
        | MatterEndpointKind::Occupancy
        | MatterEndpointKind::Contact
        | MatterEndpointKind::GenericSwitch => return Err(not_an_actuator(&cfg.kind)),
    };

    info!(
        endpoint = id,
        kind = %cfg.kind,
        name = %name,
        sink = %sink.describe(),
        "Matter: actuator endpoint"
    );

    Ok(EndpointSpec {
        id,
        dynamic: true,
        name,
        device_types: vec![device_type],
        clusters: vec![identify_cluster(rand), (meta, imp)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that records what it was told and can be made to fail.
    struct RecordingSink {
        writes: Mutex<Vec<Value>>,
        fail: AtomicBool,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                writes: Mutex::new(Vec::new()),
                fail: AtomicBool::new(false),
            })
        }
        fn log(&self) -> Vec<Value> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl Sink for RecordingSink {
        fn write(&self, value: Value) -> Result<(), String> {
            if self.fail.load(Ordering::Relaxed) {
                return Err("line stuck".to_string());
            }
            self.writes.lock().unwrap().push(value);
            Ok(())
        }
        fn describe(&self) -> String {
            "recording".to_string()
        }
    }

    fn fan(steps: FanSteps, sink: Arc<RecordingSink>) -> FanHandler {
        FanHandler::new(Dataver::new(1), steps, sink, "test fan")
    }

    #[test]
    fn fan_steps_parse_and_name_the_real_sequences() {
        assert_eq!(FanSteps::parse(""), Some(FanSteps::Single));
        assert_eq!(FanSteps::parse("off_high"), Some(FanSteps::Single));
        assert_eq!(FanSteps::parse("off_low_high"), Some(FanSteps::Dual));
        assert_eq!(FanSteps::parse("off_low_med_high"), Some(FanSteps::Triple));
        assert_eq!(FanSteps::parse("off_low_med_high_auto"), None);
        assert_eq!(FanSteps::Single.sequence(), FanModeSequenceEnum::OffHigh);
        assert_eq!(FanSteps::Dual.sequence(), FanModeSequenceEnum::OffLowHigh);
        assert_eq!(FanSteps::Triple.sequence(), FanModeSequenceEnum::OffLowMedHigh);
    }

    #[test]
    fn single_speed_fan_is_on_or_off() {
        let s = FanSteps::Single;
        assert_eq!(s.apply_percent(0), FanState::OFF);
        for p in [1, 40, 100] {
            let st = s.apply_percent(p);
            assert_eq!((st.mode, st.setting, st.current), (FanModeEnum::High, p, 100));
        }
        assert_eq!(s.apply_mode(FanModeEnum::Off), Some(FanState::OFF));
        assert_eq!(
            s.apply_mode(FanModeEnum::High),
            Some(FanState { mode: FanModeEnum::High, setting: 100, current: 100 })
        );
        // Speeds this fan doesn't have, and the deprecated/auto modes, are refused.
        for m in [FanModeEnum::Low, FanModeEnum::Medium, FanModeEnum::On, FanModeEnum::Auto, FanModeEnum::Smart] {
            assert_eq!(s.apply_mode(m), None, "{m:?}");
        }
    }

    #[test]
    fn three_speed_fan_uses_equal_bands() {
        let s = FanSteps::Triple;
        let band = |p: u8| {
            let st = s.apply_percent(p);
            (st.mode, st.current)
        };
        assert_eq!(band(1), (FanModeEnum::Low, 33));
        assert_eq!(band(33), (FanModeEnum::Low, 33));
        assert_eq!(band(34), (FanModeEnum::Medium, 66));
        assert_eq!(band(66), (FanModeEnum::Medium, 66));
        assert_eq!(band(67), (FanModeEnum::High, 100));
        assert_eq!(band(100), (FanModeEnum::High, 100));
        // The setting keeps what was asked for, the current is the real speed.
        let st = s.apply_percent(40);
        assert_eq!((st.setting, st.current), (40, 66));
        // A mode write sets both to that step.
        assert_eq!(
            s.apply_mode(FanModeEnum::Medium),
            Some(FanState { mode: FanModeEnum::Medium, setting: 66, current: 66 })
        );
    }

    #[test]
    fn two_speed_fan_splits_at_half() {
        let s = FanSteps::Dual;
        assert_eq!(s.apply_percent(50).mode, FanModeEnum::Low);
        assert_eq!(s.apply_percent(51).mode, FanModeEnum::High);
        assert_eq!(s.apply_mode(FanModeEnum::Medium), None);
    }

    #[test]
    fn fan_starts_off_and_tells_its_sink() {
        let sink = RecordingSink::new();
        let f = fan(FanSteps::Triple, sink.clone());
        assert_eq!(f.snapshot(), FanState::OFF);
        assert_eq!(sink.log(), vec![Value::Num(0.0)]);
    }

    #[test]
    fn fan_transition_drives_the_sink_with_the_running_speed() {
        let sink = RecordingSink::new();
        let f = fan(FanSteps::Triple, sink.clone());
        let prev = f.transition(FanSteps::Triple.apply_percent(40)).unwrap();
        assert_eq!(prev, FanState::OFF);
        // 40% requested -> the Medium step actually runs -> the sink sees 66.
        assert_eq!(sink.log().last(), Some(&Value::Num(66.0)));
        assert_eq!(f.snapshot().setting, 40);
        assert_eq!(f.snapshot().current, 66);
    }

    #[test]
    fn fan_only_writes_the_sink_when_the_running_speed_changes() {
        let sink = RecordingSink::new();
        let f = fan(FanSteps::Single, sink.clone());
        f.transition(FanSteps::Single.apply_percent(40)).unwrap();
        let writes = sink.log().len();
        // 40 -> 70 is still "High": the setting moves, the hardware does not.
        f.transition(FanSteps::Single.apply_percent(70)).unwrap();
        assert_eq!(sink.log().len(), writes);
        assert_eq!(f.snapshot().setting, 70);
    }

    #[test]
    fn fan_keeps_its_state_when_the_sink_fails() {
        let sink = RecordingSink::new();
        let f = fan(FanSteps::Single, sink.clone());
        sink.fail.store(true, Ordering::Relaxed);
        let err = f.transition(FanSteps::Single.apply_percent(100)).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Failure);
        assert_eq!(f.snapshot(), FanState::OFF, "a command that never reached the hardware must not be reported");
    }

    #[test]
    fn onoff_hooks_report_only_what_the_sink_accepted() {
        let sink = RecordingSink::new();
        let hooks = SinkOnOffHooks::<false>::new(sink.clone(), "plug");
        assert!(!hooks.on_off());
        assert_eq!(sink.log(), vec![Value::Bool(false)], "starts by saying off");

        hooks.set_on_off(true);
        assert!(hooks.on_off());
        assert_eq!(sink.log().last(), Some(&Value::Bool(true)));

        sink.fail.store(true, Ordering::Relaxed);
        hooks.set_on_off(false);
        assert!(hooks.on_off(), "a failed write must leave the state where the hardware is");
    }

    #[test]
    fn plug_and_light_advertise_different_features() {
        // The plug is plain OnOff; the light carries LIGHTING and its attributes.
        assert_eq!(PLUG_CLUSTER.feature_map & on_off::Feature::LIGHTING.bits(), 0);
        assert_ne!(LIGHTING_CLUSTER.feature_map & on_off::Feature::LIGHTING.bits(), 0);
        assert!(PLUG_CLUSTER.attribute(on_off::AttributeId::OnTime as _).is_none());
        assert!(LIGHTING_CLUSTER.attribute(on_off::AttributeId::OnTime as _).is_some());
        assert!(PLUG_CLUSTER.command(on_off::CommandId::Toggle as _).is_some());
        assert!(PLUG_CLUSTER.command(on_off::CommandId::OffWithEffect as _).is_none());
    }
}
