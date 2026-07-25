// src/motion.rs
//
// Zone motion detection (classic NVR trigger): frame-to-frame luma change
// inside configured zones, debounced with a sustain/recover machine like
// tamper. Cheap enough to run on the analytics thread at a few Hz. Emits
// motion start/stop transitions per zone and exposes an "any motion" flag the
// media thread uses to gate recording (storage.record_mode = "motion").
use crate::config::{CameraConfig, MotionConfig, MotionZone};

use std::time::{Duration, Instant};

/// Downsampled luma grid (same idea as tamper); coarse is plenty for motion.
const GRID_W: usize = 64;
const GRID_H: usize = 36;

/// Per-zone start/stop transition.
#[derive(Debug)]
pub struct MotionTransition {
    pub zone: String,
    pub active: bool,
    pub score: f32,
}

struct Zone {
    name: String,
    // Grid-cell bounds (inclusive-exclusive).
    gx0: usize,
    gy0: usize,
    gx1: usize,
    gy1: usize,
    active: bool,
    hit_streak: u32,
    clear_streak: u32,
}

enum LumaLayout {
    Nv12,
    Yuy2,
}

pub struct MotionDetector {
    layout: LumaLayout,
    frame_w: usize,
    frame_h: usize,
    min_interval: Duration,
    last_analysis: Option<Instant>,
    previous: Option<Vec<f32>>,
    sensitivity: f32,
    alarm_after: u32,
    recover_after: u32,
    zones: Vec<Zone>,
}

impl MotionDetector {
    pub fn new(cfg: &MotionConfig, camera: &CameraConfig) -> Option<Self> {
        if !cfg.enabled {
            return None;
        }
        let layout = match camera.format.to_uppercase().as_str() {
            "NV12" => LumaLayout::Nv12,
            "YUYV" | "YUY2" => LumaLayout::Yuy2,
            _ => return None, // unsupported pixel format; motion stays off
        };
        let fps = cfg.analysis_fps.max(1);
        // No zones configured ⇒ one full-frame zone.
        let zone_specs: Vec<MotionZone> = if cfg.zones.is_empty() {
            vec![MotionZone {
                name: "frame".into(),
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            }]
        } else {
            cfg.zones.clone()
        };
        let zones = zone_specs.iter().map(compile_zone).collect();

        Some(Self {
            layout,
            frame_w: camera.width as usize,
            frame_h: camera.height as usize,
            min_interval: Duration::from_secs_f64(1.0 / f64::from(fps)),
            last_analysis: None,
            previous: None,
            sensitivity: cfg.sensitivity.max(0.1),
            alarm_after: cfg.alarm_after_frames.max(1),
            recover_after: (cfg.recover_after_s * fps).max(1),
            zones,
        })
    }

    /// Analyze one frame (rate-limited). Returns zone transitions only.
    pub fn analyze(&mut self, frame: &[u8]) -> Vec<MotionTransition> {
        if let Some(last) = self.last_analysis {
            if last.elapsed() < self.min_interval {
                return Vec::new();
            }
        }
        self.last_analysis = Some(Instant::now());
        self.analyze_now(frame)
    }

    fn analyze_now(&mut self, frame: &[u8]) -> Vec<MotionTransition> {
        let Some(grid) = self.sample_grid(frame) else {
            return Vec::new();
        };
        let Some(prev) = self.previous.take() else {
            self.previous = Some(grid);
            return Vec::new();
        };

        let alarm_after = self.alarm_after;
        let recover_after = self.recover_after;
        let sensitivity = self.sensitivity;
        let mut transitions = Vec::new();

        for zone in &mut self.zones {
            let score = zone_diff(&grid, &prev, zone);
            let moving = score > sensitivity;
            if let Some(active) = step(zone, moving, alarm_after, recover_after) {
                transitions.push(MotionTransition {
                    zone: zone.name.clone(),
                    active,
                    score,
                });
            }
        }

        self.previous = Some(grid);
        transitions
    }

    pub fn any_active(&self) -> bool {
        self.zones.iter().any(|z| z.active)
    }

    fn sample_grid(&self, frame: &[u8]) -> Option<Vec<f32>> {
        let needed = match self.layout {
            LumaLayout::Nv12 => self.frame_w * self.frame_h,
            LumaLayout::Yuy2 => self.frame_w * self.frame_h * 2,
        };
        if frame.len() < needed || self.frame_w < GRID_W || self.frame_h < GRID_H {
            return None;
        }
        let mut grid = Vec::with_capacity(GRID_W * GRID_H);
        for gy in 0..GRID_H {
            let sy = gy * self.frame_h / GRID_H;
            for gx in 0..GRID_W {
                let sx = gx * self.frame_w / GRID_W;
                let luma = match self.layout {
                    LumaLayout::Nv12 => frame[sy * self.frame_w + sx],
                    LumaLayout::Yuy2 => frame[(sy * self.frame_w + sx) * 2],
                };
                grid.push(f32::from(luma));
            }
        }
        Some(grid)
    }
}

/// Sustain/recover step; returns Some(active) only on a transition.
fn step(zone: &mut Zone, moving: bool, alarm_after: u32, recover_after: u32) -> Option<bool> {
    if moving {
        zone.hit_streak += 1;
        zone.clear_streak = 0;
        if !zone.active && zone.hit_streak >= alarm_after {
            zone.active = true;
            return Some(true);
        }
    } else {
        zone.clear_streak += 1;
        zone.hit_streak = 0;
        if zone.active && zone.clear_streak >= recover_after {
            zone.active = false;
            return Some(false);
        }
    }
    None
}

/// Mean absolute luma difference across the zone's grid cells.
fn zone_diff(grid: &[f32], prev: &[f32], zone: &Zone) -> f32 {
    let mut sum = 0.0;
    let mut count = 0u32;
    for gy in zone.gy0..zone.gy1 {
        for gx in zone.gx0..zone.gx1 {
            let idx = gy * GRID_W + gx;
            sum += (grid[idx] - prev[idx]).abs();
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        sum / count as f32
    }
}

fn compile_zone(z: &MotionZone) -> Zone {
    let clamp = |v: f32| v.clamp(0.0, 1.0);
    let gx0 = (clamp(z.x) * GRID_W as f32) as usize;
    let gy0 = (clamp(z.y) * GRID_H as f32) as usize;
    let gx1 = ((clamp(z.x + z.w) * GRID_W as f32) as usize)
        .max(gx0 + 1)
        .min(GRID_W);
    let gy1 = ((clamp(z.y + z.h) * GRID_H as f32) as usize)
        .max(gy0 + 1)
        .min(GRID_H);
    Zone {
        name: z.name.clone(),
        gx0,
        gy0,
        gx1,
        gy1,
        active: false,
        hit_streak: 0,
        clear_streak: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera() -> CameraConfig {
        CameraConfig {
            r#type: "MOCK".into(),
            device_node: String::new(),
            source_params: String::new(),
            width: 128,
            height: 72,
            fps: 30,
            format: "NV12".into(),
            auto_exposure: false,
            exposure_time_us: 0,
            gain: 0,
        }
    }

    fn config() -> MotionConfig {
        MotionConfig {
            enabled: true,
            analysis_fps: 1,
            sensitivity: 10.0,
            alarm_after_frames: 2,
            recover_after_s: 2,
            zones: Vec::new(),
        }
    }

    fn flat(luma: u8) -> Vec<u8> {
        let (w, h) = (128usize, 72usize);
        let mut f = vec![128u8; w * h * 3 / 2];
        f[..w * h].fill(luma);
        f
    }

    #[test]
    fn detects_motion_and_recovers() {
        let mut det = MotionDetector::new(&config(), &camera()).unwrap();
        // Prime + steady scene → no motion.
        assert!(det.analyze_now(&flat(40)).is_empty());
        assert!(det.analyze_now(&flat(40)).is_empty());
        // Big luma jump twice → motion starts (alarm_after=2).
        assert!(det.analyze_now(&flat(200)).is_empty()); // 1st over-threshold
        let t = det.analyze_now(&flat(60)); // change again (200->60)
        assert!(t.iter().any(|tr| tr.active));
        assert!(det.any_active());
        // Steady scene → recovers after 2 clear frames.
        let mut recovered = false;
        for _ in 0..4 {
            recovered |= det.analyze_now(&flat(60)).iter().any(|tr| !tr.active);
        }
        assert!(recovered && !det.any_active());
    }

    #[test]
    fn disabled_returns_none() {
        let mut cfg = config();
        cfg.enabled = false;
        assert!(MotionDetector::new(&cfg, &camera()).is_none());
    }

    #[test]
    fn zone_compiles_within_grid() {
        let z = compile_zone(&MotionZone {
            name: "q".into(),
            x: 0.5,
            y: 0.5,
            w: 0.5,
            h: 0.5,
        });
        assert_eq!((z.gx0, z.gy0), (32, 18));
        assert_eq!((z.gx1, z.gy1), (GRID_W, GRID_H));
    }
}
