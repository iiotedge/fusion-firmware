// src/health.rs
//
// System health sampling (gap analysis 2026-07-17): enclosed edge devices
// cook and throttle silently. This reads SoC temperature, CPU load and the
// Rockchip/RPi thermal-throttle flag from Linux sysfs/procfs, feeds the
// Prometheus gauges, and returns a snapshot for the GDE health beacon.
//
// All reads are best-effort: a missing sysfs node yields None, never an
// error — the firmware runs on boards that expose different subsets.
use crate::core::metrics::Metrics;

use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{info, warn};

/// A point-in-time system health reading. Fields are read directly by the
/// health beacon (src/main.rs) into the GDE payload.
#[derive(Debug, Default, Clone)]
pub struct HealthSnapshot {
    pub soc_temp_c: Option<f64>,
    pub cpu_load_percent: Option<f64>,
    pub mem_used_percent: Option<f64>,
    pub throttled: bool,
}

/// Latest reading, refreshed by the sampler thread; read by the health beacon.
#[derive(Clone, Default)]
pub struct HealthMonitor {
    latest: Arc<parking_lot::Mutex<HealthSnapshot>>,
}

impl HealthMonitor {
    pub fn snapshot(&self) -> HealthSnapshot {
        self.latest.lock().clone()
    }
}

/// Start the sampler. Warm-over-threshold logs a warning edge (like the
/// storage watermark) so a cooking device is visible without scraping metrics.
pub fn spawn(metrics: Arc<Metrics>, warn_temp_c: f64) -> HealthMonitor {
    let monitor = HealthMonitor::default();
    let shared = monitor.latest.clone();
    let spawned = thread::Builder::new()
        .name("system_health".to_string())
        .spawn(move || {
            let mut cpu_prev = read_cpu_times();
            let mut was_hot = false;
            loop {
                thread::sleep(Duration::from_secs(10));

                let cpu_now = read_cpu_times();
                let cpu_load = cpu_load_percent(cpu_prev, cpu_now);
                cpu_prev = cpu_now;

                let snap = HealthSnapshot {
                    soc_temp_c: read_soc_temp_c(),
                    cpu_load_percent: cpu_load,
                    mem_used_percent: read_mem_used_percent(),
                    throttled: read_throttled(),
                };

                if let Some(t) = snap.soc_temp_c {
                    metrics.soc_temp_millicelsius.set((t * 1000.0) as i64);
                    let hot = t >= warn_temp_c;
                    if hot && !was_hot {
                        warn!(soc_temp_c = t, warn_temp_c, "SoC temperature high");
                    } else if !hot && was_hot {
                        info!(soc_temp_c = t, "SoC temperature back to normal");
                    }
                    was_hot = hot;
                }
                if let Some(l) = snap.cpu_load_percent {
                    metrics.cpu_load_percent.set(l as i64);
                }
                if let Some(m) = snap.mem_used_percent {
                    metrics.mem_used_percent.set(m as i64);
                }
                metrics.throttled.set(i64::from(snap.throttled));

                *shared.lock() = snap;
            }
        });
    if let Err(e) = spawned {
        warn!("failed to spawn system health sampler: {e}");
    }
    monitor
}

/// Hottest thermal zone in °C (`/sys/class/thermal/thermal_zone*/temp`,
/// millidegrees). Covers Rockchip (soc-thermal), i.MX and generic boards.
fn read_soc_temp_c() -> Option<f64> {
    let mut hottest: Option<f64> = None;
    let entries = std::fs::read_dir("/sys/class/thermal").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("thermal_zone") {
            continue;
        }
        if let Ok(raw) = std::fs::read_to_string(entry.path().join("temp")) {
            if let Ok(milli) = raw.trim().parse::<f64>() {
                let c = milli / 1000.0;
                hottest = Some(hottest.map_or(c, |h: f64| h.max(c)));
            }
        }
    }
    hottest
}

/// Aggregate (non-idle) jiffies and total, from the `cpu` line of /proc/stat.
fn read_cpu_times() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0); // idle + iowait
    let total: u64 = fields.iter().sum();
    Some((total - idle, total))
}

/// Busy delta / total delta between two /proc/stat readings.
fn cpu_load_percent(prev: Option<(u64, u64)>, now: Option<(u64, u64)>) -> Option<f64> {
    let (busy0, total0) = prev?;
    let (busy1, total1) = now?;
    let total_delta = total1.checked_sub(total0)?;
    if total_delta == 0 {
        return None;
    }
    let busy_delta = busy1.saturating_sub(busy0);
    Some((busy_delta as f64 / total_delta as f64) * 100.0)
}

/// Used memory percent from /proc/meminfo (MemTotal vs MemAvailable).
fn read_mem_used_percent() -> Option<f64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut available = None;
    for line in info.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = v.split_whitespace().next()?.parse::<f64>().ok();
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            available = v.split_whitespace().next()?.parse::<f64>().ok();
        }
    }
    let (total, available) = (total?, available?);
    if total <= 0.0 {
        return None;
    }
    Some(((total - available) / total) * 100.0)
}

/// Best-effort throttle flag. RPi-style `vcgencmd` isn't present on Rockchip;
/// we infer throttling from the cooling-device state (a fan/throttle governor
/// with a non-zero cur_state indicates active thermal mitigation).
fn read_throttled() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/thermal") else {
        return false;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("cooling_device")
        {
            continue;
        }
        let state = std::fs::read_to_string(entry.path().join("cur_state"))
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0);
        if state > 0 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_load_computes_busy_fraction() {
        // 50 busy of 100 total delta = 50%.
        let load = cpu_load_percent(Some((100, 200)), Some((150, 300)));
        assert_eq!(load, Some(50.0));
        // No time passed → None (avoid divide-by-zero).
        assert_eq!(cpu_load_percent(Some((1, 2)), Some((1, 2))), None);
    }

    #[test]
    fn default_snapshot_is_empty() {
        let snap = HealthSnapshot::default();
        assert!(snap.soc_temp_c.is_none());
        assert!(!snap.throttled);
    }
}
