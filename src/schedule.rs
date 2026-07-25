// src/schedule.rs
//
// Weekly arming schedule (shift/calendar recording). Evaluates whether "now"
// (device local time) falls inside any configured window. Used to gate
// recording — the standard "record only during shifts" NVR feature.
//
// Windows may cross midnight (start > end), e.g. a night shift 22:00–06:00.
use crate::config::{ScheduleConfig, ScheduleWindow};

use chrono::{Datelike, Local, Timelike, Weekday};
use tracing::warn;

pub struct Schedule {
    enabled: bool,
    windows: Vec<CompiledWindow>,
}

struct CompiledWindow {
    days: u8, // bitmask, bit 0 = Monday
    start_min: u32,
    end_min: u32,
}

impl Schedule {
    pub fn new(cfg: &ScheduleConfig) -> Self {
        if !cfg.enabled {
            return Self {
                enabled: false,
                windows: Vec::new(),
            };
        }
        let windows = cfg.windows.iter().filter_map(compile).collect::<Vec<_>>();
        if windows.is_empty() {
            warn!("schedule enabled but no valid windows — camera will be DISARMED at all times");
        }
        Self {
            enabled: true,
            windows,
        }
    }

    /// True when disabled (always armed) or inside a window right now.
    pub fn armed_now(&self) -> bool {
        if !self.enabled {
            return true;
        }
        let now = Local::now();
        let day_bit = 1u8 << now.weekday().num_days_from_monday();
        let minute_of_day = now.hour() * 60 + now.minute();
        self.windows
            .iter()
            .any(|w| w.days & day_bit != 0 && in_window(minute_of_day, w.start_min, w.end_min))
    }
}

fn in_window(now: u32, start: u32, end: u32) -> bool {
    if start <= end {
        now >= start && now < end
    } else {
        // Crosses midnight: armed from start..24:00 and 00:00..end.
        now >= start || now < end
    }
}

fn compile(w: &ScheduleWindow) -> Option<CompiledWindow> {
    let mut days = 0u8;
    for day in &w.days {
        match parse_day(day) {
            Some(wd) => days |= 1u8 << wd.num_days_from_monday(),
            None => warn!("schedule: unknown day '{day}' (use mon..sun); ignoring"),
        }
    }
    let start_min = parse_hhmm(&w.start)?;
    let end_min = parse_hhmm(&w.end)?;
    if days == 0 {
        warn!("schedule window has no valid days; ignoring");
        return None;
    }
    Some(CompiledWindow {
        days,
        start_min,
        end_min,
    })
}

fn parse_day(s: &str) -> Option<Weekday> {
    match s.to_lowercase().as_str() {
        "mon" | "monday" => Some(Weekday::Mon),
        "tue" | "tuesday" => Some(Weekday::Tue),
        "wed" | "wednesday" => Some(Weekday::Wed),
        "thu" | "thursday" => Some(Weekday::Thu),
        "fri" | "friday" => Some(Weekday::Fri),
        "sat" | "saturday" => Some(Weekday::Sat),
        "sun" | "sunday" => Some(Weekday::Sun),
        _ => None,
    }
}

fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 24 || m > 59 {
        warn!("schedule: invalid time '{s}'");
        return None;
    }
    Some(h * 60 + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_is_always_armed() {
        let s = Schedule::new(&ScheduleConfig::default());
        assert!(s.armed_now());
    }

    #[test]
    fn window_membership_including_midnight_crossing() {
        // Day shift 06:00–22:00.
        assert!(in_window(8 * 60, 6 * 60, 22 * 60));
        assert!(!in_window(23 * 60, 6 * 60, 22 * 60));
        assert!(!in_window(5 * 60, 6 * 60, 22 * 60));
        // Night shift 22:00–06:00 (crosses midnight).
        assert!(in_window(23 * 60, 22 * 60, 6 * 60));
        assert!(in_window(2 * 60, 22 * 60, 6 * 60));
        assert!(!in_window(12 * 60, 22 * 60, 6 * 60));
    }

    #[test]
    fn compiles_day_bitmask_and_time() {
        let w = compile(&ScheduleWindow {
            days: vec!["mon".into(), "fri".into(), "bogus".into()],
            start: "06:30".into(),
            end: "22:00".into(),
        })
        .unwrap();
        assert_eq!(w.days, 0b0001_0001); // Mon (bit0) + Fri (bit4)
        assert_eq!(w.start_min, 390);
        assert_eq!(w.end_min, 1320);
    }
}
