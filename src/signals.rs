// src/signals.rs
//
// Generic signal layer (Phase 19g.1): named readings — a number or a
// boolean — from heterogeneous sources, behind one trait. Matter sensor
// endpoints (src/matter/sensors.rs) read from it today; nothing here is
// Matter-specific, so MQTT/Home Assistant/etc. can consume it later.
//
// WHY. This firmware is generic: the same binary is a camera, a light
// controller or an industrial edge gateway depending on config. Matter
// sensors therefore can't be wired to specific hardware in code — they are
// bound, by a short spec string in config, to WHATEVER produces the number:
//
//   builtin:<name>          signals this firmware already computes for real
//                           (SoC temperature, motion/tamper flags, ...)
//   sysfs:<path>            any Linux sysfs file holding a number — this alone
//                           covers IIO / hwmon / 1-Wire / thermal sensors
//                           (BME280, SHT3x, BH1750, DS18B20, ...) with no
//                           per-chip code
//   gpio_in:<chip>:<line>[:active_low]   a digital input (PIR, reed switch)
//   push:<name>             a value pushed in from outside (HTTP/MQTT/script),
//                           so any gateway or PLC can feed a Matter sensor
//
// NEVER FABRICATED. Every source reports `None` when it has no reading (file
// missing, GPIO unreadable, pushed value stale) and the Matter side turns that
// into a Matter `null`, never a made-up 0. Every source also carries a
// `Provenance`; synthetic data (the mock camera/radar backends) is `Mock`, and
// a Matter sensor refuses to expose `Mock` data unless explicitly allowed, so a
// real controller is never shown synthetic readings as if they were real.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// A reading: a number or a boolean.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Num(f64),
}

impl Value {
    /// Numeric view: booleans are 0/1.
    pub fn as_f64(self) -> f64 {
        match self {
            Value::Bool(b) => f64::from(u8::from(b)),
            Value::Num(n) => n,
        }
    }

    /// Boolean view: any non-zero number is true.
    pub fn as_bool(self) -> bool {
        match self {
            Value::Bool(b) => b,
            Value::Num(n) => n != 0.0,
        }
    }
}

/// Where a reading ultimately comes from — used to keep synthetic data out of
/// real controllers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// Measured by this firmware on this device.
    Real,
    /// Pushed in by something else (HTTP/MQTT/script): the integrator vouches
    /// for it, we cannot.
    External,
    /// Synthetic (mock camera/radar backends, test hooks).
    Mock,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    pub value: Value,
}

pub trait Source: Send + Sync {
    /// Current reading, or `None` when there isn't one (never a placeholder).
    fn read(&self) -> Option<Reading>;
    fn provenance(&self) -> Provenance;
    /// Short human-readable description for logs.
    fn describe(&self) -> String;
}

/// A shared boolean flag some other thread already maintains.
pub struct FlagSource {
    flag: Arc<AtomicBool>,
    provenance: Provenance,
    label: String,
}

impl FlagSource {
    pub fn new(label: &str, flag: Arc<AtomicBool>, provenance: Provenance) -> Self {
        Self {
            flag,
            provenance,
            label: label.to_string(),
        }
    }
}

impl Source for FlagSource {
    fn read(&self) -> Option<Reading> {
        Some(Reading {
            value: Value::Bool(self.flag.load(Ordering::Relaxed)),
        })
    }
    fn provenance(&self) -> Provenance {
        self.provenance
    }
    fn describe(&self) -> String {
        format!("flag:{}", self.label)
    }
}

type ReadFn = Box<dyn Fn() -> Option<Value> + Send + Sync>;

/// A reading computed on demand by a closure (e.g. the SoC temperature).
pub struct FnSource {
    f: ReadFn,
    provenance: Provenance,
    label: String,
}

impl FnSource {
    pub fn new(
        label: &str,
        provenance: Provenance,
        f: impl Fn() -> Option<Value> + Send + Sync + 'static,
    ) -> Self {
        Self {
            f: Box::new(f),
            provenance,
            label: label.to_string(),
        }
    }
}

impl Source for FnSource {
    fn read(&self) -> Option<Reading> {
        (self.f)().map(|value| Reading { value })
    }
    fn provenance(&self) -> Provenance {
        self.provenance
    }
    fn describe(&self) -> String {
        format!("fn:{}", self.label)
    }
}

/// A value pushed in from outside the process. `None` until the first push,
/// and again once the last push is older than `max_age` (when set) — a sensor
/// that stopped reporting must read as "no data", not as its last value.
pub struct PushedSource {
    name: String,
    cell: Mutex<Option<(Value, Instant)>>,
    max_age: Option<Duration>,
}

impl PushedSource {
    fn new(name: &str, max_age: Option<Duration>) -> Self {
        Self {
            name: name.to_string(),
            cell: Mutex::new(None),
            max_age,
        }
    }

    pub fn set(&self, value: Value) {
        *self.cell.lock().unwrap() = Some((value, Instant::now()));
    }
}

impl Source for PushedSource {
    fn read(&self) -> Option<Reading> {
        let (value, at) = (*self.cell.lock().unwrap())?;
        if let Some(max_age) = self.max_age {
            if at.elapsed() > max_age {
                return None;
            }
        }
        Some(Reading { value })
    }
    fn provenance(&self) -> Provenance {
        Provenance::External
    }
    fn describe(&self) -> String {
        format!("push:{}", self.name)
    }
}

/// A number read from a Linux sysfs (or procfs) file on every call. The kernel
/// exposes most I2C/SPI/1-Wire sensors this way (IIO, hwmon, w1, thermal), so
/// one generic source covers a large class of real hardware.
pub struct SysfsSource {
    path: PathBuf,
}

impl SysfsSource {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Source for SysfsSource {
    fn read(&self) -> Option<Reading> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let n: f64 = text.trim().parse().ok()?;
        n.is_finite().then_some(Reading { value: Value::Num(n) })
    }
    fn provenance(&self) -> Provenance {
        Provenance::Real
    }
    fn describe(&self) -> String {
        format!("sysfs:{}", self.path.display())
    }
}

/// A digital input line (PIR, reed switch, door contact), read by level.
pub struct GpioInSource {
    input: crate::gpio::GpioIn,
}

impl GpioInSource {
    fn open(chip: &str, line: u32, active_low: bool) -> Result<Self, String> {
        Ok(Self {
            input: crate::gpio::GpioIn::open(chip, line, active_low, "fusion-firmware-input")?,
        })
    }
}

impl Source for GpioInSource {
    fn read(&self) -> Option<Reading> {
        self.input.level().map(|b| Reading {
            value: Value::Bool(b),
        })
    }
    fn provenance(&self) -> Provenance {
        Provenance::Real
    }
    fn describe(&self) -> String {
        format!("gpio_in:{}", self.input.label())
    }
}

/// Where a command goes: the OUTPUT side of the signal layer. A Matter
/// actuator (light, plug, fan, ...) writes its state here and the sink does
/// whatever "making it so" means for this deployment.
pub trait Sink: Send + Sync {
    /// Drive the output. Booleans for on/off; numbers for levels.
    fn write(&self, value: Value) -> Result<(), String>;
    fn describe(&self) -> String;
}

/// Accepts commands and does nothing: for endpoints with no hardware behind
/// them (a bench/demo device). Must be asked for explicitly (`sink = "virtual"`)
/// — an actuator with NO sink configured is a config error, never a silent
/// no-op, so a missing line can't masquerade as a working light.
pub struct VirtualSink;

impl Sink for VirtualSink {
    fn write(&self, _value: Value) -> Result<(), String> {
        Ok(())
    }
    fn describe(&self) -> String {
        "virtual (no hardware)".to_string()
    }
}

/// Publishes the commanded state into the bus as `push:<name>`, so anything
/// else on the device (or an HTTP client polling `GET /signals`) can see and
/// react to it — and a sensor can use it as a `push:<name>` source.
pub struct SignalSink {
    cell: Arc<PushedSource>,
    name: String,
}

impl Sink for SignalSink {
    fn write(&self, value: Value) -> Result<(), String> {
        self.cell.set(value);
        Ok(())
    }
    fn describe(&self) -> String {
        format!("signal:{}", self.name)
    }
}

/// A persistent GPIO output (relay, MOSFET, indicator).
pub struct GpioOutSink {
    out: crate::gpio::GpioOut,
}

impl Sink for GpioOutSink {
    fn write(&self, value: Value) -> Result<(), String> {
        self.out.set(value.as_bool())
    }
    fn describe(&self) -> String {
        format!("gpio:{}", self.out.label())
    }
}

/// A parsed `source = "..."` spec (syntax only — nothing is opened yet, so
/// config validation can reject a typo without touching hardware).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSpec {
    Builtin(String),
    Push(String),
    Sysfs(PathBuf),
    GpioIn {
        chip: String,
        line: u32,
        active_low: bool,
    },
}

/// Signal names are used in URLs and logs: keep them boring.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

pub fn parse_spec(spec: &str) -> Result<SourceSpec, String> {
    let (scheme, rest) = spec
        .split_once(':')
        .ok_or_else(|| format!("source '{spec}' must look like scheme:value (builtin:, push:, sysfs:, gpio_in:)"))?;
    match scheme {
        "builtin" | "push" => {
            if !valid_name(rest) {
                return Err(format!(
                    "source '{spec}': name must be 1-64 chars of letters, digits, '_', '-', '.'"
                ));
            }
            Ok(if scheme == "builtin" {
                SourceSpec::Builtin(rest.to_string())
            } else {
                SourceSpec::Push(rest.to_string())
            })
        }
        "sysfs" => {
            if !rest.starts_with('/') {
                return Err(format!("source '{spec}': sysfs path must be absolute"));
            }
            Ok(SourceSpec::Sysfs(PathBuf::from(rest)))
        }
        "gpio_in" => {
            let mut parts = rest.split(':');
            let chip = parts.next().unwrap_or_default();
            let line = parts
                .next()
                .and_then(|l| l.parse::<u32>().ok())
                .ok_or_else(|| format!("source '{spec}': expected gpio_in:<chip>:<line>[:active_low]"))?;
            let active_low = match parts.next() {
                None => false,
                Some("active_low") => true,
                Some(other) => {
                    return Err(format!("source '{spec}': unknown gpio_in option '{other}'"))
                }
            };
            if chip.is_empty() || parts.next().is_some() {
                return Err(format!("source '{spec}': expected gpio_in:<chip>:<line>[:active_low]"));
            }
            Ok(SourceSpec::GpioIn {
                chip: chip.to_string(),
                line,
                active_low,
            })
        }
        other => Err(format!(
            "source '{spec}': unknown scheme '{other}' (builtin, push, sysfs, gpio_in)"
        )),
    }
}

/// A parsed `sink = "..."` spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkSpec {
    Gpio {
        chip: String,
        line: u32,
        active_low: bool,
    },
    Signal(String),
    Virtual,
}

pub fn parse_sink_spec(spec: &str) -> Result<SinkSpec, String> {
    if spec == "virtual" {
        return Ok(SinkSpec::Virtual);
    }
    let (scheme, rest) = spec.split_once(':').ok_or_else(|| {
        format!("sink '{spec}' must be virtual, signal:<name> or gpio:<chip>:<line>[:active_low]")
    })?;
    match scheme {
        "signal" => {
            if !valid_name(rest) {
                return Err(format!(
                    "sink '{spec}': name must be 1-64 chars of letters, digits, '_', '-', '.'"
                ));
            }
            Ok(SinkSpec::Signal(rest.to_string()))
        }
        "gpio" => match parse_spec(&format!("gpio_in:{rest}"))? {
            // Same `<chip>:<line>[:active_low]` grammar as the gpio_in source.
            SourceSpec::GpioIn {
                chip,
                line,
                active_low,
            } => Ok(SinkSpec::Gpio {
                chip,
                line,
                active_low,
            }),
            _ => unreachable!("gpio_in always parses to GpioIn"),
        },
        other => Err(format!(
            "sink '{spec}': unknown scheme '{other}' (virtual, signal, gpio)"
        )),
    }
}

/// The registry of named signals. Created once, early in `main()`, and shared
/// (`Arc`) with everything that wants to publish or consume signals.
#[derive(Default)]
pub struct SignalBus {
    builtins: RwLock<HashMap<String, Arc<dyn Source>>>,
    pushed: RwLock<HashMap<String, Arc<PushedSource>>>,
}

impl SignalBus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_builtin(&self, name: &str, source: Arc<dyn Source>) {
        self.builtins.write().unwrap().insert(name.to_string(), source);
    }

    pub fn builtin_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.builtins.read().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    /// Get-or-create the pushed signal `name`, so a Matter endpoint can be bound
    /// to a signal before anything has pushed to it (it just reads `None`).
    pub fn pushed(&self, name: &str) -> Arc<PushedSource> {
        if let Some(existing) = self.pushed.read().unwrap().get(name) {
            return existing.clone();
        }
        self.pushed
            .write()
            .unwrap()
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(PushedSource::new(name, None)))
            .clone()
    }

    /// Push a value into `push:<name>` (HTTP/MQTT/command-channel entry point).
    pub fn push(&self, name: &str, value: Value) -> Result<(), String> {
        if !valid_name(name) {
            return Err(format!("invalid signal name '{name}'"));
        }
        self.pushed(name).set(value);
        Ok(())
    }

    /// Every named signal and its current reading, sorted by name — the debug
    /// view behind `GET /signals`. `kind` is "builtin" or "push".
    pub fn readings(&self) -> Vec<(String, &'static str, Option<Value>)> {
        let mut out: Vec<(String, &'static str, Option<Value>)> = Vec::new();
        for (name, src) in self.builtins.read().unwrap().iter() {
            out.push((name.clone(), "builtin", src.read().map(|r| r.value)));
        }
        for (name, src) in self.pushed.read().unwrap().iter() {
            out.push((name.clone(), "push", src.read().map(|r| r.value)));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
        out
    }

    /// Turn a spec into a live source, opening hardware if needed.
    pub fn resolve(&self, spec: &SourceSpec) -> Result<Arc<dyn Source>, String> {
        match spec {
            SourceSpec::Builtin(name) => self
                .builtins
                .read()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "unknown builtin signal '{name}' (available: {})",
                        self.builtin_names().join(", ")
                    )
                }),
            SourceSpec::Push(name) => Ok(self.pushed(name)),
            SourceSpec::Sysfs(path) => Ok(Arc::new(SysfsSource::new(path.clone()))),
            SourceSpec::GpioIn {
                chip,
                line,
                active_low,
            } => Ok(Arc::new(GpioInSource::open(chip, *line, *active_low)?)),
        }
    }

    /// Turn a sink spec into a live sink, opening hardware if needed. A GPIO
    /// output is requested already driven to `initial` (off), so a relay never
    /// glitches on at boot.
    pub fn resolve_sink(&self, spec: &SinkSpec) -> Result<Arc<dyn Sink>, String> {
        match spec {
            SinkSpec::Virtual => Ok(Arc::new(VirtualSink)),
            SinkSpec::Signal(name) => Ok(Arc::new(SignalSink {
                cell: self.pushed(name),
                name: name.clone(),
            })),
            SinkSpec::Gpio {
                chip,
                line,
                active_low,
            } => Ok(Arc::new(GpioOutSink {
                out: crate::gpio::GpioOut::open(chip, *line, *active_low, "fusion-firmware-output", false)?,
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_conversions() {
        assert_eq!(Value::Bool(true).as_f64(), 1.0);
        assert_eq!(Value::Bool(false).as_f64(), 0.0);
        assert!(Value::Num(0.5).as_bool());
        assert!(!Value::Num(0.0).as_bool());
    }

    #[test]
    fn parses_every_scheme() {
        assert_eq!(parse_spec("builtin:soc_temp_c").unwrap(), SourceSpec::Builtin("soc_temp_c".into()));
        assert_eq!(parse_spec("push:boiler.temp-1").unwrap(), SourceSpec::Push("boiler.temp-1".into()));
        assert_eq!(
            parse_spec("sysfs:/sys/class/hwmon/hwmon0/temp1_input").unwrap(),
            SourceSpec::Sysfs(PathBuf::from("/sys/class/hwmon/hwmon0/temp1_input"))
        );
        assert_eq!(
            parse_spec("gpio_in:/dev/gpiochip0:17").unwrap(),
            SourceSpec::GpioIn { chip: "/dev/gpiochip0".into(), line: 17, active_low: false }
        );
        assert_eq!(
            parse_spec("gpio_in:/dev/gpiochip1:3:active_low").unwrap(),
            SourceSpec::GpioIn { chip: "/dev/gpiochip1".into(), line: 3, active_low: true }
        );
    }

    #[test]
    fn rejects_malformed_specs() {
        for bad in [
            "", "soc_temp_c", "bogus:x", "builtin:", "builtin:has space", "push:a/b",
            "sysfs:relative/path", "gpio_in:/dev/gpiochip0", "gpio_in:/dev/gpiochip0:x",
            "gpio_in:/dev/gpiochip0:1:nope", "gpio_in:/dev/gpiochip0:1:active_low:extra",
        ] {
            assert!(parse_spec(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn pushed_source_is_none_until_pushed_and_expires() {
        let src = PushedSource::new("t", Some(Duration::from_millis(30)));
        assert!(src.read().is_none(), "no reading before the first push");
        src.set(Value::Num(21.5));
        assert_eq!(src.read().unwrap().value, Value::Num(21.5));
        std::thread::sleep(Duration::from_millis(60));
        assert!(src.read().is_none(), "a stale push must read as no data");
    }

    #[test]
    fn bus_push_and_resolve_share_one_signal() {
        let bus = SignalBus::new();
        let src = bus.resolve(&parse_spec("push:room_temp").unwrap()).unwrap();
        assert!(src.read().is_none());
        bus.push("room_temp", Value::Num(19.0)).unwrap();
        assert_eq!(src.read().unwrap().value, Value::Num(19.0));
        assert_eq!(src.provenance(), Provenance::External);
        assert!(bus.push("bad name", Value::Num(1.0)).is_err());
    }

    #[test]
    fn unknown_builtin_lists_what_is_available() {
        let bus = SignalBus::new();
        bus.register_builtin(
            "motion",
            Arc::new(FlagSource::new("motion", Arc::new(AtomicBool::new(true)), Provenance::Real)),
        );
        let err = bus.resolve(&parse_spec("builtin:nope").unwrap()).err().unwrap();
        assert!(err.contains("motion"), "{err}");
        let ok = bus.resolve(&parse_spec("builtin:motion").unwrap()).unwrap();
        assert_eq!(ok.read().unwrap().value, Value::Bool(true));
    }

    #[test]
    fn sysfs_source_reads_numbers_and_never_invents_one() {
        let dir = std::env::temp_dir().join(format!("fusion-sysfs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("temp");
        std::fs::write(&good, "42000\n").unwrap();
        assert_eq!(SysfsSource::new(good).read().unwrap().value, Value::Num(42000.0));
        let junk = dir.join("junk");
        std::fs::write(&junk, "not a number").unwrap();
        assert!(SysfsSource::new(junk).read().is_none());
        assert!(SysfsSource::new(dir.join("missing")).read().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_every_sink_scheme_and_rejects_bad_ones() {
        assert_eq!(parse_sink_spec("virtual").unwrap(), SinkSpec::Virtual);
        assert_eq!(parse_sink_spec("signal:lamp").unwrap(), SinkSpec::Signal("lamp".into()));
        assert_eq!(
            parse_sink_spec("gpio:/dev/gpiochip0:17").unwrap(),
            SinkSpec::Gpio { chip: "/dev/gpiochip0".into(), line: 17, active_low: false }
        );
        assert_eq!(
            parse_sink_spec("gpio:/dev/gpiochip1:3:active_low").unwrap(),
            SinkSpec::Gpio { chip: "/dev/gpiochip1".into(), line: 3, active_low: true }
        );
        for bad in ["", "gpio", "gpio:/dev/gpiochip0", "signal:", "signal:a b", "mqtt:x", "gpio:/dev/gpiochip0:1:nope"] {
            assert!(parse_sink_spec(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn signal_sink_publishes_the_commanded_state_to_the_bus() {
        let bus = SignalBus::new();
        let sink = bus.resolve_sink(&parse_sink_spec("signal:lamp").unwrap()).unwrap();
        sink.write(Value::Bool(true)).unwrap();
        let seen = bus.resolve(&parse_spec("push:lamp").unwrap()).unwrap();
        assert_eq!(seen.read().unwrap().value, Value::Bool(true));
        sink.write(Value::Bool(false)).unwrap();
        assert_eq!(seen.read().unwrap().value, Value::Bool(false));
    }

    #[test]
    fn virtual_sink_accepts_everything() {
        let bus = SignalBus::new();
        let sink = bus.resolve_sink(&SinkSpec::Virtual).unwrap();
        assert!(sink.write(Value::Num(0.5)).is_ok());
    }

    #[test]
    fn flag_and_fn_sources_report_provenance() {
        let flag = Arc::new(AtomicBool::new(false));
        let f = FlagSource::new("tamper", flag.clone(), Provenance::Mock);
        assert_eq!(f.read().unwrap().value, Value::Bool(false));
        flag.store(true, Ordering::Relaxed);
        assert_eq!(f.read().unwrap().value, Value::Bool(true));
        assert_eq!(f.provenance(), Provenance::Mock);
        let n = FnSource::new("none", Provenance::Real, || None);
        assert!(n.read().is_none());
    }
}
