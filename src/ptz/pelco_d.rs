// src/ptz/pelco_d.rs
//
// Pelco-D over RS-485/RS-232 — Pelco's own de facto standard, the most
// widely supported PTZ motor-control protocol (every mainstream PTZ head
// speaks it natively or has a compatibility mode). 7-byte frames:
// [0xFF, address, cmd1, cmd2, data1, data2, checksum], checksum = sum of
// address+cmd1+cmd2+data1+data2 mod 256. Byte layout and command codes
// below are Pelco's published spec, not vendor-specific guesswork.
use crate::config::PtzConfig;
use crate::core::error::{EdgeError, EdgeResult};
use crate::ptz::PtzDriver;

use std::io::Write;
use std::time::Duration;
use tracing::debug;

const SYNC: u8 = 0xFF;

// CMD2 bits — pan/tilt direction. Combine freely (e.g. pan+tilt for a
// diagonal move); independent of the zoom bits below, so one frame can
// carry pan+tilt+zoom together.
const PAN_RIGHT: u8 = 0x02;
const PAN_LEFT: u8 = 0x04;
const TILT_UP: u8 = 0x08;
const TILT_DOWN: u8 = 0x10;
const SET_PRESET: u8 = 0x03;
const GOTO_PRESET: u8 = 0x07;

// CMD1 bits — zoom direction.
const ZOOM_TELE: u8 = 0x20;
const ZOOM_WIDE: u8 = 0x40;

/// Pan/tilt speed range per Pelco-D's base spec (0x00-0x3F). Some vendors
/// extend into 0x40-0xFF as a "turbo" range — deliberately not used here,
/// 0x00-0x3F is what every Pelco-D-compatible head is guaranteed to accept.
const MAX_SPEED: u8 = 0x3F;

pub struct PelcoDDriver {
    port: Box<dyn serialport::SerialPort>,
    address: u8,
}

impl PelcoDDriver {
    pub fn new(cfg: &PtzConfig) -> EdgeResult<Self> {
        let port = serialport::new(&cfg.serial_device, cfg.baud_rate)
            .timeout(Duration::from_millis(200))
            .open()
            .map_err(|e| {
                EdgeError::HardwareFault(format!(
                    "ptz: open serial device '{}' at {} baud: {e}",
                    cfg.serial_device, cfg.baud_rate
                ))
            })?;
        Ok(Self {
            port,
            address: cfg.address,
        })
    }

    fn send(&mut self, cmd1: u8, cmd2: u8, data1: u8, data2: u8) -> EdgeResult<()> {
        let f = frame(self.address, cmd1, cmd2, data1, data2);
        debug!(frame = ?f, "Pelco-D frame");
        self.port
            .write_all(&f)
            .map_err(|e| EdgeError::HardwareFault(format!("ptz: serial write: {e}")))
    }
}

/// Builds one 7-byte Pelco-D frame, checksum included. A pure function so
/// the protocol encoding is unit-testable without a real serial port.
fn frame(address: u8, cmd1: u8, cmd2: u8, data1: u8, data2: u8) -> [u8; 7] {
    let sum = u16::from(address)
        + u16::from(cmd1)
        + u16::from(cmd2)
        + u16::from(data1)
        + u16::from(data2);
    [SYNC, address, cmd1, cmd2, data1, data2, sum as u8]
}

/// Maps an ONVIF-convention axis value to a Pelco-D speed byte. Magnitude
/// only — direction comes from which CMD1/CMD2 bit the caller sets.
fn speed(value: f32) -> u8 {
    (value.abs().clamp(0.0, 1.0) * f32::from(MAX_SPEED)).round() as u8
}

impl PtzDriver for PelcoDDriver {
    fn continuous_move(&mut self, pan: f32, tilt: f32, zoom: f32) -> EdgeResult<()> {
        let mut cmd1 = 0u8;
        let mut cmd2 = 0u8;
        let mut data1 = 0u8;
        let mut data2 = 0u8;

        if pan > 0.0 {
            cmd2 |= PAN_RIGHT;
            data1 = speed(pan);
        } else if pan < 0.0 {
            cmd2 |= PAN_LEFT;
            data1 = speed(pan);
        }
        if tilt > 0.0 {
            cmd2 |= TILT_UP;
            data2 = speed(tilt);
        } else if tilt < 0.0 {
            cmd2 |= TILT_DOWN;
            data2 = speed(tilt);
        }
        // Base Pelco-D zoom is direction-only, no speed byte.
        if zoom > 0.0 {
            cmd1 |= ZOOM_TELE;
        } else if zoom < 0.0 {
            cmd1 |= ZOOM_WIDE;
        }

        self.send(cmd1, cmd2, data1, data2)
    }

    fn stop(&mut self) -> EdgeResult<()> {
        self.send(0, 0, 0, 0)
    }

    fn goto_preset(&mut self, preset: u8) -> EdgeResult<()> {
        self.send(0, GOTO_PRESET, 0, preset)
    }

    fn set_preset(&mut self, preset: u8) -> EdgeResult<()> {
        self.send(0, SET_PRESET, 0, preset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_checksum_is_sum_of_body_mod_256() {
        let f = frame(1, 0x00, PAN_RIGHT, MAX_SPEED, 0x00);
        assert_eq!(f[0], SYNC);
        assert_eq!(f[1], 1);
        assert_eq!(
            f[6],
            (1u16 + u16::from(PAN_RIGHT) + u16::from(MAX_SPEED)) as u8
        );
    }

    #[test]
    fn checksum_wraps_on_overflow() {
        // Every field maxed out overflows a u8 sum; checksum must wrap
        // (mod 256), not panic or saturate.
        let f = frame(0xFF, 0xFF, 0xFF, 0xFF, 0xFF);
        let expected = ((0xFFu16 * 5) % 256) as u8;
        assert_eq!(f[6], expected);
    }

    #[test]
    fn speed_maps_full_range_and_clamps_out_of_bounds_input() {
        assert_eq!(speed(0.0), 0);
        assert_eq!(speed(1.0), MAX_SPEED);
        assert_eq!(speed(-1.0), MAX_SPEED); // magnitude only; sign is direction, not encoded here
        assert_eq!(speed(2.5), MAX_SPEED); // out-of-range input clamps rather than overflowing
    }

    #[test]
    fn pan_right_frame_carries_expected_bits() {
        let f = frame(0, 0x00, PAN_RIGHT, MAX_SPEED, 0x00);
        assert_eq!(f[2], 0x00); // cmd1: no zoom
        assert_eq!(f[3], PAN_RIGHT); // cmd2
        assert_eq!(f[4], MAX_SPEED); // data1: pan speed
        assert_eq!(f[5], 0x00); // data2: no tilt
    }

    #[test]
    fn stop_frame_is_all_zero_body() {
        let f = frame(5, 0, 0, 0, 0);
        assert_eq!(f, [SYNC, 5, 0, 0, 0, 0, 5]);
    }
}
