// src/matter/generic_switch.rs
//
// Config-driven Matter Generic Switch (Phase 19g): a push button or a toggle
// switch on any boolean source — a GPIO input, a value pushed over HTTP, a
// built-in flag — with the Switch cluster's EVENTS, which is how Matter
// controllers learn that a button was pressed (a press is something that
// happened, not a state to read back).
//
//   switch_mode = "momentary"   a push button. InitialPress on press,
//                               ShortRelease / LongPress + LongRelease on
//                               release, MultiPressOngoing / MultiPressComplete
//                               for double/triple presses. Features MS, MSR,
//                               MSL and MSM.
//   switch_mode = "latching"    a toggle/rocker. SwitchLatched on every change.
//                               Feature LS.
//
// The source is true while the button is pressed / the switch is in its second
// position (`invert` flips that, for an active-low button).
//
// WHERE THE RULES COME FROM. Which event fires when, in what order, with what
// counts, is spec behaviour, and a button that gets it subtly wrong confuses
// every automation built on it (a double press that also looks like two single
// presses, a long press that is also a short one). `Machine` is a port of the
// Matter spec's behaviour as implemented — and conformance-tested — in
// matter.js's `SwitchServer`, kept PURE (the caller supplies the time) so every
// sequence is unit-tested deterministically instead of with sleeps.
//
// HONESTY RULES (the same ones the sensors follow):
//   * A source with no reading produces no events and changes nothing — a
//     button is never reported pressed (or released) on no evidence.
//   * A latching switch reports its position only once the source has one
//     (CurrentPosition is an error status until then, never a guessed 0). A
//     momentary switch rests at position 0, which is what "not pressed" means.
//   * The first reading of a latching switch is adopted silently: finding the
//     switch already in some position at boot is not a "move".
//   * Contact bounce shorter than `debounce_ms` is ignored, and the timing of a
//     debounced edge is the time the edge FIRST appeared, so debouncing never
//     turns a short press into a long one.
use core::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rs_matter::dm::clusters::decl::switch::{
    self, InitialPress, LongPress, LongRelease, MultiPressComplete, MultiPressOngoing,
    ShortRelease, SwitchLatched,
};
use rs_matter::dm::{Async, AttrId, Cluster, ClusterId, Dataver, DeviceType, EndptId, HandlerContext, ReadContext};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::with;

use crate::config::{
    effective_endpoint_name, effective_poll_ms, MatterEndpointConfig, MatterEndpointKind,
    SWITCH_DEFAULT_DEBOUNCE_MS, SWITCH_DEFAULT_LONG_PRESS_MS, SWITCH_DEFAULT_MULTI_PRESS_MAX,
    SWITCH_DEFAULT_MULTI_PRESS_MS,
};
use crate::matter::registry::{identify_cluster, ClusterImpl, EndpointSpec};
use crate::matter::sensors::resolve_source;
use crate::signals::{SignalBus, Source};

use tracing::{info, warn};

const SWITCH_CLUSTER_ID: ClusterId = 59;
const ATTR_CURRENT_POSITION: AttrId = 1;
/// A momentary switch rests here; pressed is 1. A latching switch's two
/// positions are 0 and 1.
const NEUTRAL: u8 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchMode {
    Momentary,
    Latching,
}

impl SwitchMode {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "" | "momentary" => Some(Self::Momentary),
            "latching" => Some(Self::Latching),
            _ => None,
        }
    }

    pub(crate) fn names() -> &'static str {
        "momentary, latching"
    }
}

/// Timing of a momentary switch.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// Held at least this long = a long press.
    pub(crate) long_press: Duration,
    /// A press this soon after a release continues the multi-press sequence.
    pub(crate) multi_press: Duration,
    /// Presses counted before the sequence is abandoned.
    pub(crate) multi_press_max: u8,
    /// Level changes shorter than this are contact bounce.
    pub(crate) debounce: Duration,
}

impl Timing {
    fn from_config(cfg: &MatterEndpointConfig) -> Self {
        Self {
            long_press: Duration::from_millis(cfg.long_press_ms.unwrap_or(SWITCH_DEFAULT_LONG_PRESS_MS)),
            multi_press: Duration::from_millis(cfg.multi_press_ms.unwrap_or(SWITCH_DEFAULT_MULTI_PRESS_MS)),
            multi_press_max: cfg.multi_press_max.unwrap_or(SWITCH_DEFAULT_MULTI_PRESS_MAX),
            debounce: Duration::from_millis(cfg.debounce_ms.unwrap_or(SWITCH_DEFAULT_DEBOUNCE_MS)),
        }
    }
}

/// One Switch-cluster event, in the order the spec generates them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchEvent {
    SwitchLatched { new_position: u8 },
    InitialPress { new_position: u8 },
    LongPress { new_position: u8 },
    ShortRelease { previous_position: u8 },
    LongRelease { previous_position: u8 },
    MultiPressOngoing { new_position: u8, presses: u8 },
    MultiPressComplete { previous_position: u8, total: u8 },
}

/// Debounce + press logic. Pure: time is an argument, so a test can play a whole
/// double-press in microseconds.
pub(crate) struct Machine {
    mode: SwitchMode,
    timing: Timing,
    /// The debounced position — what CurrentPosition reads. `None` only for a
    /// latching switch that has not yet had a reading.
    stable: Option<u8>,
    /// A different level has been seen since this instant but is not yet stable.
    candidate: Option<(u8, Instant)>,

    // Momentary press tracking.
    previously_reported: u8,
    long_deadline: Option<Instant>,
    long_position: Option<u8>,
    long_fired: bool,
    multi_deadline: Option<Instant>,
    presses: u8,
    aborted: bool,
    previous_multi_position: Option<u8>,
}

impl Machine {
    pub(crate) fn new(mode: SwitchMode, timing: Timing) -> Self {
        Self {
            mode,
            timing,
            stable: match mode {
                SwitchMode::Momentary => Some(NEUTRAL),
                SwitchMode::Latching => None,
            },
            candidate: None,
            previously_reported: NEUTRAL,
            long_deadline: None,
            long_position: None,
            long_fired: false,
            multi_deadline: None,
            presses: 1,
            aborted: false,
            previous_multi_position: None,
        }
    }

    /// The debounced position, if known.
    pub(crate) fn position(&self) -> Option<u8> {
        self.stable
    }

    /// Advance with one sample of the raw level (`None` = the source has no
    /// reading right now) taken at `now`; the events it produced are appended to
    /// `out` in order.
    pub(crate) fn step(&mut self, raw: Option<bool>, now: Instant, out: &mut Vec<SwitchEvent>) {
        match raw {
            Some(level) => {
                if let Some((position, edge)) = self.debounce(u8::from(level), now) {
                    // Anything due BEFORE the edge happened before it; then the edge.
                    self.timers(edge, out);
                    self.position_changed(position, edge, out);
                }
            }
            // A gap in the readings voids a pending edge: we have no evidence
            // for what happened in between, so it must not be confirmed later
            // and back-dated.
            None => self.candidate = None,
        }
        // Timers may only run up to an edge that is still being debounced: it
        // may turn out to be real, and then it happened BEFORE any deadline that
        // falls inside the debounce window (else a 790 ms press whose release
        // is confirmed 30 ms later would be reported as a long press).
        let horizon = match self.candidate {
            Some((_, since)) => since.min(now),
            None => now,
        };
        self.timers(horizon, out);
    }

    /// `Some((position, when it first appeared))` once `level` has held for the
    /// debounce time.
    fn debounce(&mut self, level: u8, now: Instant) -> Option<(u8, Instant)> {
        let Some(stable) = self.stable else {
            // First reading of a latching switch: adopt it, silently.
            self.stable = Some(level);
            return None;
        };
        if level == stable {
            self.candidate = None;
            return None;
        }
        let since = match self.candidate {
            Some((c, t)) if c == level => t,
            _ => {
                self.candidate = Some((level, now));
                now
            }
        };
        if now.saturating_duration_since(since) >= self.timing.debounce {
            self.candidate = None;
            self.stable = Some(level);
            Some((level, since))
        } else {
            None
        }
    }

    fn position_changed(&mut self, position: u8, at: Instant, out: &mut Vec<SwitchEvent>) {
        match self.mode {
            SwitchMode::Latching => out.push(SwitchEvent::SwitchLatched {
                new_position: position,
            }),
            SwitchMode::Momentary => self.momentary_changed(position, at, out),
        }
    }

    fn momentary_changed(&mut self, position: u8, at: Instant, out: &mut Vec<SwitchEvent>) {
        let pressed = position != NEUTRAL;
        if pressed && !self.aborted {
            out.push(SwitchEvent::InitialPress {
                new_position: position,
            });
        }

        // A release is short if the long-press timer was still running, long if
        // it had fired.
        let mut sequence_finished = false;
        if !pressed {
            if self.long_deadline.is_some() {
                out.push(SwitchEvent::ShortRelease {
                    previous_position: self.previously_reported,
                });
            } else if self.long_fired {
                out.push(SwitchEvent::LongRelease {
                    previous_position: self.previously_reported,
                });
                self.multi_deadline = None;
                sequence_finished = true;
            }
        }
        self.long_deadline = None;
        self.long_fired = false;
        self.long_position = None;
        if pressed {
            self.long_position = Some(position);
            self.long_deadline = Some(at + self.timing.long_press);
        }

        // A press while the multi-press window is open continues the sequence.
        if self.multi_deadline.is_some() && pressed && !self.aborted && !sequence_finished {
            self.presses = self.presses.saturating_add(1);
            out.push(SwitchEvent::MultiPressOngoing {
                new_position: position,
                presses: self.presses,
            });
            if self.presses > self.timing.multi_press_max {
                // Past what the switch can count: report the abandoned sequence
                // (total 0 = "too many") and go quiet until it ends.
                self.aborted = true;
                out.push(SwitchEvent::MultiPressComplete {
                    previous_position: position,
                    total: 0,
                });
                sequence_finished = true;
            }
        }
        self.multi_deadline = None;
        if !sequence_finished {
            self.multi_deadline = Some(at + self.timing.multi_press);
        }
        if self.previously_reported != NEUTRAL {
            self.previous_multi_position = Some(self.previously_reported);
        }
        self.previously_reported = position;
    }

    /// Fire whichever timers are due at `now`, earliest first (they are
    /// independent, so they fire in deadline order).
    fn timers(&mut self, now: Instant, out: &mut Vec<SwitchEvent>) {
        loop {
            let next = match (self.long_deadline, self.multi_deadline) {
                (Some(l), Some(m)) if l <= m => Some((l, true)),
                (Some(_), Some(m)) => Some((m, false)),
                (Some(l), None) => Some((l, true)),
                (None, Some(m)) => Some((m, false)),
                (None, None) => None,
            };
            match next {
                Some((deadline, is_long)) if deadline <= now => {
                    if is_long {
                        self.long_expired(out);
                    } else {
                        self.multi_expired(out);
                    }
                }
                _ => break,
            }
        }
    }

    fn long_expired(&mut self, out: &mut Vec<SwitchEvent>) {
        self.long_deadline = None;
        let Some(position) = self.long_position else {
            return;
        };
        // Held down as the 2nd+ press of a sequence is not a long press.
        if self.presses > 1 {
            return;
        }
        out.push(SwitchEvent::LongPress {
            new_position: position,
        });
        self.long_fired = true;
        self.multi_deadline = None;
    }

    fn multi_expired(&mut self, out: &mut Vec<SwitchEvent>) {
        self.multi_deadline = None;
        let Some(previous) = self.previous_multi_position else {
            return;
        };
        // Still being held: the sequence isn't over.
        if self.long_deadline.is_some() {
            return;
        }
        if !self.aborted {
            out.push(SwitchEvent::MultiPressComplete {
                previous_position: previous,
                total: self.presses,
            });
        }
        self.presses = 1;
        self.aborted = false;
        self.previous_multi_position = None;
    }
}

// ---------------------------------------------------------------------------
// Cluster metadata
// ---------------------------------------------------------------------------

/// Momentary: MS + MSR + MSL + MSM, and exactly the events those features imply
/// (`MultiPressMax` is conditional on MSM). No ACTION_SWITCH, so
/// MultiPressOngoing is part of the set.
const MOMENTARY_CLUSTER: Cluster<'static> = switch::FULL_CLUSTER
    .with_features(
        switch::Feature::MOMENTARY_SWITCH.bits()
            | switch::Feature::MOMENTARY_SWITCH_RELEASE.bits()
            | switch::Feature::MOMENTARY_SWITCH_LONG_PRESS.bits()
            | switch::Feature::MOMENTARY_SWITCH_MULTI_PRESS.bits(),
    )
    .with_attrs(with!(required; switch::AttributeId::MultiPressMax))
    .with_cmds(with!())
    .with_events(with!(
        switch::EventId::InitialPress
            | switch::EventId::LongPress
            | switch::EventId::ShortRelease
            | switch::EventId::LongRelease
            | switch::EventId::MultiPressOngoing
            | switch::EventId::MultiPressComplete
    ));

const LATCHING_CLUSTER: Cluster<'static> = switch::FULL_CLUSTER
    .with_features(switch::Feature::LATCHING_SWITCH.bits())
    .with_attrs(with!(required))
    .with_cmds(with!())
    .with_events(with!(switch::EventId::SwitchLatched));

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub(crate) struct GenericSwitchHandler {
    endpoint: EndptId,
    dataver: Dataver,
    mode: SwitchMode,
    source: Arc<dyn Source>,
    invert: bool,
    poll: Duration,
    multi_press_max: u8,
    machine: Mutex<Machine>,
}

impl GenericSwitchHandler {
    fn cluster(mode: SwitchMode) -> Cluster<'static> {
        match mode {
            SwitchMode::Momentary => MOMENTARY_CLUSTER,
            SwitchMode::Latching => LATCHING_CLUSTER,
        }
    }

    /// Sample the source every `poll`, run the state machine, tell subscribers
    /// when the position moves and emit the events it produces. Never returns on
    /// its own.
    async fn watch<C: HandlerContext>(&self, ctx: C) -> Result<(), Error> {
        let mut events = Vec::new();
        let mut last = self.machine.lock().unwrap().position();
        loop {
            async_io::Timer::after(self.poll).await;
            let raw = self
                .source
                .read()
                .map(|r| r.value.as_bool() != self.invert);
            let position = {
                let mut machine = self.machine.lock().unwrap();
                machine.step(raw, Instant::now(), &mut events);
                machine.position()
            };
            if position != last {
                last = position;
                ctx.notify_attr_changed(self.endpoint, SWITCH_CLUSTER_ID, ATTR_CURRENT_POSITION);
            }
            for event in events.drain(..) {
                self.emit(&ctx, event);
            }
        }
    }

    fn emit<C: HandlerContext>(&self, ctx: &C, event: SwitchEvent) {
        let endpoint = self.endpoint;
        let result = match event {
            SwitchEvent::SwitchLatched { new_position } => {
                SwitchLatched::emit_for(ctx, endpoint, |b| b.new_position(new_position)?.end())
            }
            SwitchEvent::InitialPress { new_position } => {
                InitialPress::emit_for(ctx, endpoint, |b| b.new_position(new_position)?.end())
            }
            SwitchEvent::LongPress { new_position } => {
                LongPress::emit_for(ctx, endpoint, |b| b.new_position(new_position)?.end())
            }
            SwitchEvent::ShortRelease { previous_position } => {
                ShortRelease::emit_for(ctx, endpoint, |b| b.previous_position(previous_position)?.end())
            }
            SwitchEvent::LongRelease { previous_position } => {
                LongRelease::emit_for(ctx, endpoint, |b| b.previous_position(previous_position)?.end())
            }
            SwitchEvent::MultiPressOngoing { new_position, presses } => {
                MultiPressOngoing::emit_for(ctx, endpoint, |b| {
                    b.new_position(new_position)?
                        .current_number_of_presses_counted(presses)?
                        .end()
                })
            }
            SwitchEvent::MultiPressComplete { previous_position, total } => {
                MultiPressComplete::emit_for(ctx, endpoint, |b| {
                    b.previous_position(previous_position)?
                        .total_number_of_presses_counted(total)?
                        .end()
                })
            }
        };
        if let Err(e) = result {
            warn!(endpoint, ?event, "Matter: switch event not emitted: {e:?}");
        }
    }
}

impl switch::ClusterHandler for GenericSwitchHandler {
    const CLUSTER: Cluster<'static> = MOMENTARY_CLUSTER;

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    /// Two positions either way: idle/pressed, or the latching switch's two.
    fn number_of_positions(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        Ok(2)
    }

    fn current_position(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        self.machine
            .lock()
            .unwrap()
            .position()
            .ok_or_else(|| Error::new(ErrorCode::Failure))
    }

    fn multi_press_max(&self, _ctx: impl ReadContext) -> Result<u8, Error> {
        match self.mode {
            SwitchMode::Momentary => Ok(self.multi_press_max),
            SwitchMode::Latching => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    fn run(&self, ctx: impl HandlerContext) -> impl Future<Output = Result<(), Error>> {
        self.watch(ctx)
    }
}

/// Build one `generic_switch` `[[matter.endpoints]]` entry. Fails (so the caller
/// can skip just this endpoint and keep the rest of the node up) on an unusable
/// source.
pub(crate) fn build_endpoint<R: rand_core::Rng>(
    cfg: &MatterEndpointConfig,
    index: usize,
    id: EndptId,
    bus: &SignalBus,
    rand: &mut R,
) -> Result<EndpointSpec, String> {
    let name = effective_endpoint_name(cfg, index);
    let mode = SwitchMode::parse(&cfg.switch_mode)
        .ok_or_else(|| format!("unknown switch_mode '{}'", cfg.switch_mode))?;
    let source = resolve_source(cfg, bus)?;
    let timing = Timing::from_config(cfg);

    let handler = GenericSwitchHandler {
        endpoint: id,
        dataver: Dataver::new_rand(rand),
        mode,
        source: source.clone(),
        invert: cfg.invert,
        poll: Duration::from_millis(effective_poll_ms(MatterEndpointKind::GenericSwitch, cfg)),
        multi_press_max: timing.multi_press_max,
        machine: Mutex::new(Machine::new(mode, timing)),
    };

    info!(
        endpoint = id,
        kind = %cfg.kind,
        name = %name,
        mode = ?mode,
        source = %source.describe(),
        "Matter: switch endpoint"
    );

    Ok(EndpointSpec {
        id,
        dynamic: true,
        name,
        device_types: vec![DeviceType { dtype: 0x000F, drev: 3 }],
        clusters: vec![
            identify_cluster(rand),
            (
                GenericSwitchHandler::cluster(mode),
                ClusterImpl::Switch(Async(switch::HandlerAdaptor(handler))),
            ),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use SwitchEvent as E;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn timing() -> Timing {
        Timing {
            long_press: ms(800),
            multi_press: ms(300),
            multi_press_max: 3,
            debounce: ms(30),
        }
    }

    /// Plays a script against a `Machine` in virtual time, sampling every 10 ms
    /// like the real poll loop.
    struct Rig {
        m: Machine,
        t0: Instant,
        at_ms: u64,
        events: Vec<SwitchEvent>,
    }

    impl Rig {
        fn new(mode: SwitchMode) -> Self {
            Self {
                m: Machine::new(mode, timing()),
                t0: Instant::now(),
                at_ms: 0,
                events: Vec::new(),
            }
        }

        /// Hold the raw level for `for_ms`.
        fn hold(&mut self, level: Option<bool>, for_ms: u64) -> &mut Self {
            let end = self.at_ms + for_ms;
            while self.at_ms < end {
                self.m.step(level, self.t0 + ms(self.at_ms), &mut self.events);
                self.at_ms += 10;
            }
            self
        }

        fn press(&mut self, for_ms: u64) -> &mut Self {
            self.hold(Some(true), for_ms)
        }

        fn release(&mut self, for_ms: u64) -> &mut Self {
            self.hold(Some(false), for_ms)
        }

        fn take(&mut self) -> Vec<SwitchEvent> {
            std::mem::take(&mut self.events)
        }
    }

    #[test]
    fn a_short_press_is_press_release_then_one_press_complete() {
        let mut r = Rig::new(SwitchMode::Momentary);
        r.release(50).press(150).release(500);
        assert_eq!(
            r.take(),
            vec![
                E::InitialPress { new_position: 1 },
                E::ShortRelease { previous_position: 1 },
                E::MultiPressComplete { previous_position: 1, total: 1 },
            ]
        );
        assert_eq!(r.m.position(), Some(0));
    }

    #[test]
    fn a_double_press_counts_two() {
        let mut r = Rig::new(SwitchMode::Momentary);
        r.release(50).press(100).release(100).press(100).release(500);
        assert_eq!(
            r.take(),
            vec![
                E::InitialPress { new_position: 1 },
                E::ShortRelease { previous_position: 1 },
                E::InitialPress { new_position: 1 },
                E::MultiPressOngoing { new_position: 1, presses: 2 },
                E::ShortRelease { previous_position: 1 },
                E::MultiPressComplete { previous_position: 1, total: 2 },
            ]
        );
    }

    #[test]
    fn a_long_press_is_long_press_and_long_release_not_a_short_one() {
        let mut r = Rig::new(SwitchMode::Momentary);
        r.release(50).press(1_000).release(500);
        assert_eq!(
            r.take(),
            vec![
                E::InitialPress { new_position: 1 },
                E::LongPress { new_position: 1 },
                E::LongRelease { previous_position: 1 },
            ],
            "no ShortRelease and no MultiPressComplete after a long press"
        );
    }

    #[test]
    fn a_slow_press_under_the_long_threshold_is_still_short() {
        let mut r = Rig::new(SwitchMode::Momentary);
        r.release(50).press(500).release(500);
        assert_eq!(
            r.take(),
            vec![
                E::InitialPress { new_position: 1 },
                E::ShortRelease { previous_position: 1 },
                E::MultiPressComplete { previous_position: 1, total: 1 },
            ]
        );
    }

    #[test]
    fn a_long_hold_as_the_second_press_is_not_a_long_press() {
        let mut r = Rig::new(SwitchMode::Momentary);
        r.release(50).press(100).release(100).press(1_000).release(600);
        let ev = r.take();
        assert!(!ev.iter().any(|e| matches!(e, E::LongPress { .. } | E::LongRelease { .. })), "{ev:?}");
        assert!(ev.contains(&E::MultiPressComplete { previous_position: 1, total: 2 }), "{ev:?}");
    }

    #[test]
    fn pressing_past_the_max_abandons_the_sequence() {
        let mut r = Rig::new(SwitchMode::Momentary);
        r.release(50);
        for _ in 0..4 {
            r.press(80).release(80);
        }
        r.release(600);
        let ev = r.take();
        // The 4th press exceeds multi_press_max (3): ongoing(4), then complete(0).
        assert!(ev.contains(&E::MultiPressOngoing { new_position: 1, presses: 4 }), "{ev:?}");
        assert!(ev.contains(&E::MultiPressComplete { previous_position: 1, total: 0 }), "{ev:?}");
        // ...and the abandoned sequence never also reports a normal total.
        assert_eq!(
            ev.iter().filter(|e| matches!(e, E::MultiPressComplete { .. })).count(),
            1,
            "{ev:?}"
        );
        // The machine recovers: a fresh single press afterwards is counted from 1.
        r.press(100).release(500);
        assert_eq!(
            r.take(),
            vec![
                E::InitialPress { new_position: 1 },
                E::ShortRelease { previous_position: 1 },
                E::MultiPressComplete { previous_position: 1, total: 1 },
            ]
        );
    }

    #[test]
    fn contact_bounce_shorter_than_the_debounce_is_ignored() {
        let mut r = Rig::new(SwitchMode::Momentary);
        r.release(100).press(20).release(500);
        assert_eq!(r.take(), vec![], "a 20 ms glitch under the 30 ms debounce is not a press");
        assert_eq!(r.m.position(), Some(0));
    }

    #[test]
    fn debouncing_does_not_stretch_a_short_press_into_a_long_one() {
        // Held 790 ms: under the 800 ms long-press time. The release is only
        // CONFIRMED 30 ms later, but the edge is timed from when it appeared.
        let mut r = Rig::new(SwitchMode::Momentary);
        r.release(100).press(790).release(500);
        let ev = r.take();
        assert!(ev.contains(&E::ShortRelease { previous_position: 1 }), "{ev:?}");
        assert!(!ev.iter().any(|e| matches!(e, E::LongPress { .. })), "{ev:?}");
    }

    #[test]
    fn a_glitch_during_a_press_does_not_end_it() {
        // Pressed, a 20 ms contact dropout (under the debounce), pressed again:
        // still ONE press, which goes on to be a proper long press from its real
        // start.
        let mut r = Rig::new(SwitchMode::Momentary);
        r.release(100).press(300).release(20).press(600).release(500);
        assert_eq!(
            r.take(),
            vec![
                E::InitialPress { new_position: 1 },
                E::LongPress { new_position: 1 },
                E::LongRelease { previous_position: 1 },
            ]
        );
    }

    #[test]
    fn a_gap_in_the_readings_voids_a_pending_edge() {
        let mut r = Rig::new(SwitchMode::Momentary);
        // The press is first seen, then the source goes quiet before it is
        // confirmed; when readings return the level is still "pressed". The edge
        // is NOT back-dated to the first sighting — it is timed afresh.
        r.release(100).press(10).hold(None, 5_000);
        assert_eq!(r.take(), vec![], "unconfirmed edge produces nothing");
        r.press(100);
        assert_eq!(r.take(), vec![E::InitialPress { new_position: 1 }]);
        // Timed from the fresh edge: well under the long-press time, no LongPress yet.
        r.press(300);
        assert_eq!(r.take(), vec![]);
    }

    #[test]
    fn no_reading_means_no_events_and_no_change() {
        let mut r = Rig::new(SwitchMode::Momentary);
        r.hold(None, 500);
        assert_eq!(r.take(), vec![]);
        assert_eq!(r.m.position(), Some(0), "a momentary switch rests at 0");
        // A reading that disappears mid-press doesn't invent a release either.
        r.press(100).hold(None, 2_000);
        let ev = r.take();
        assert!(!ev.iter().any(|e| matches!(e, E::ShortRelease { .. } | E::LongRelease { .. })), "{ev:?}");
    }

    #[test]
    fn a_button_already_held_at_boot_is_a_press() {
        let mut r = Rig::new(SwitchMode::Momentary);
        r.press(100);
        assert_eq!(r.take(), vec![E::InitialPress { new_position: 1 }]);
    }

    #[test]
    fn a_latching_switch_adopts_its_first_position_silently_then_reports_moves() {
        let mut r = Rig::new(SwitchMode::Latching);
        assert_eq!(r.m.position(), None, "unknown until the source has a reading");
        r.hold(None, 100);
        assert_eq!(r.m.position(), None);
        r.press(100);
        assert_eq!(r.m.position(), Some(1));
        assert_eq!(r.take(), vec![], "finding it already switched on is not a move");
        r.release(100);
        r.press(100);
        assert_eq!(
            r.take(),
            vec![
                E::SwitchLatched { new_position: 0 },
                E::SwitchLatched { new_position: 1 },
            ]
        );
    }

    #[test]
    fn a_latching_switch_debounces_too() {
        let mut r = Rig::new(SwitchMode::Latching);
        r.release(100).press(20).release(200);
        assert_eq!(r.take(), vec![]);
        assert_eq!(r.m.position(), Some(0));
    }

    #[test]
    fn modes_parse() {
        assert_eq!(SwitchMode::parse(""), Some(SwitchMode::Momentary));
        assert_eq!(SwitchMode::parse("momentary"), Some(SwitchMode::Momentary));
        assert_eq!(SwitchMode::parse("latching"), Some(SwitchMode::Latching));
        assert_eq!(SwitchMode::parse("toggle"), None);
    }

    #[test]
    fn momentary_advertises_its_features_and_every_event_they_imply() {
        let f = |bit: switch::Feature| MOMENTARY_CLUSTER.feature_map & bit.bits() != 0;
        assert!(f(switch::Feature::MOMENTARY_SWITCH));
        assert!(f(switch::Feature::MOMENTARY_SWITCH_RELEASE));
        assert!(f(switch::Feature::MOMENTARY_SWITCH_LONG_PRESS));
        assert!(f(switch::Feature::MOMENTARY_SWITCH_MULTI_PRESS));
        assert!(!f(switch::Feature::LATCHING_SWITCH));
        assert!(!f(switch::Feature::ACTION_SWITCH));
        for e in [
            switch::EventId::InitialPress,
            switch::EventId::LongPress,
            switch::EventId::ShortRelease,
            switch::EventId::LongRelease,
            switch::EventId::MultiPressOngoing,
            switch::EventId::MultiPressComplete,
        ] {
            assert!(MOMENTARY_CLUSTER.event(e as _).is_some(), "{e:?}");
        }
        assert!(MOMENTARY_CLUSTER.event(switch::EventId::SwitchLatched as _).is_none());
        assert!(MOMENTARY_CLUSTER.attribute(switch::AttributeId::MultiPressMax as _).is_some());
    }

    #[test]
    fn latching_advertises_only_its_own_feature_and_event() {
        assert_ne!(LATCHING_CLUSTER.feature_map & switch::Feature::LATCHING_SWITCH.bits(), 0);
        assert_eq!(LATCHING_CLUSTER.feature_map & switch::Feature::MOMENTARY_SWITCH.bits(), 0);
        assert!(LATCHING_CLUSTER.event(switch::EventId::SwitchLatched as _).is_some());
        assert!(LATCHING_CLUSTER.event(switch::EventId::InitialPress as _).is_none());
        assert!(LATCHING_CLUSTER.attribute(switch::AttributeId::MultiPressMax as _).is_none());
    }
}
