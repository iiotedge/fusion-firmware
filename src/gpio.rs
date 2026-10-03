// src/gpio.rs
//
// Shared GPIO access (Linux `gpio-cdev`), Phase 19g.
//
// This firmware grew three independent GPIO call sites (the SD-export button's
// edge events in storage/export.rs, the AI-rule pulse in ai/actions.rs, and the
// Matter relay), each with its own open/active-low handling and none able to
// READ an input's level. The generic Matter endpoints need both directions on
// an arbitrary number of lines, so the plumbing lives here once:
//
//   GpioOut  a persistent output (held open for the process lifetime — an
//            OnOff relay is a durable STATE, not a momentary pulse)
//   GpioIn   a digital input, read by level
//
// The kernel arbitrates ownership: requesting a line another consumer already
// holds fails with EBUSY, which surfaces here as an error string naming the
// chip/line rather than two features silently fighting over one pin.
//
// Linux-only (the character-device GPIO ABI); on other hosts `open` returns a
// clear error so a dev-host run degrades to "virtual"/"no reading" instead of
// failing to build.

#[cfg(target_os = "linux")]
mod imp {
    use std::sync::Mutex;

    use gpio_cdev::{Chip, LineHandle, LineRequestFlags};

    fn request(
        chip: &str,
        line: u32,
        flags: LineRequestFlags,
        initial: u8,
        consumer: &str,
    ) -> Result<LineHandle, String> {
        let mut c = Chip::new(chip).map_err(|e| format!("open {chip}: {e}"))?;
        c.get_line(line)
            .map_err(|e| format!("{chip} line {line}: {e}"))?
            .request(flags, initial, consumer)
            .map_err(|e| format!("request {chip} line {line}: {e}"))
    }

    pub struct GpioOut {
        handle: Mutex<LineHandle>,
        label: String,
    }

    impl GpioOut {
        /// Request `line` as an output, driven to `initial` (logical: honours
        /// `active_low`).
        pub fn open(
            chip: &str,
            line: u32,
            active_low: bool,
            consumer: &str,
            initial: bool,
        ) -> Result<Self, String> {
            let mut flags = LineRequestFlags::OUTPUT;
            if active_low {
                flags |= LineRequestFlags::ACTIVE_LOW;
            }
            let handle = request(chip, line, flags, u8::from(initial), consumer)?;
            Ok(Self {
                handle: Mutex::new(handle),
                label: format!("{chip}:{line}"),
            })
        }

        pub fn set(&self, on: bool) -> Result<(), String> {
            self.handle
                .lock()
                .unwrap()
                .set_value(u8::from(on))
                .map_err(|e| format!("set {}: {e}", self.label))
        }

        pub fn label(&self) -> &str {
            &self.label
        }
    }

    pub struct GpioIn {
        handle: Mutex<LineHandle>,
        label: String,
    }

    impl GpioIn {
        pub fn open(
            chip: &str,
            line: u32,
            active_low: bool,
            consumer: &str,
        ) -> Result<Self, String> {
            let mut flags = LineRequestFlags::INPUT;
            if active_low {
                flags |= LineRequestFlags::ACTIVE_LOW;
            }
            let handle = request(chip, line, flags, 0, consumer)?;
            Ok(Self {
                handle: Mutex::new(handle),
                label: format!("{chip}:{line}"),
            })
        }

        pub fn level(&self) -> Option<bool> {
            self.handle.lock().unwrap().get_value().ok().map(|v| v != 0)
        }

        pub fn label(&self) -> &str {
            &self.label
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    const UNSUPPORTED: &str = "GPIO needs Linux (gpio-cdev); not available on this host";

    pub struct GpioOut;

    impl GpioOut {
        pub fn open(_: &str, _: u32, _: bool, _: &str, _: bool) -> Result<Self, String> {
            Err(UNSUPPORTED.to_string())
        }
        pub fn set(&self, _: bool) -> Result<(), String> {
            Err(UNSUPPORTED.to_string())
        }
        pub fn label(&self) -> &str {
            ""
        }
    }

    pub struct GpioIn;

    impl GpioIn {
        pub fn open(_: &str, _: u32, _: bool, _: &str) -> Result<Self, String> {
            Err(UNSUPPORTED.to_string())
        }
        pub fn level(&self) -> Option<bool> {
            None
        }
        pub fn label(&self) -> &str {
            ""
        }
    }
}

pub use imp::{GpioIn, GpioOut};

#[cfg(all(test, not(target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn non_linux_hosts_get_a_clear_error_not_a_panic() {
        let e = GpioOut::open("/dev/gpiochip0", 1, false, "t", false)
            .err()
            .unwrap();
        assert!(e.contains("Linux"), "{e}");
        assert!(GpioIn::open("/dev/gpiochip0", 1, false, "t").is_err());
    }
}
