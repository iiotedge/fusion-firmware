// src/tamper.rs
//
// Camera tamper detection (F4): classic industrial video-analytics tampers
// computed from a downsampled luma grid — cheap enough to run alongside
// inference on the analytics thread at a few Hz.
//
//   blackout      mean luma collapses (lens covered / cable in the dark)
//   blinding      mean luma saturates (flashlight / laser attack)
//   occlusion     spatial detail collapses (lens covered / heavy defocus)
//   freeze        consecutive frames identical (stuck sensor or pipeline)
//   scene_change  view diverges from a slow-adapting reference (camera moved)
//
// Each condition runs a sustain/recover state machine in *analyzed frames*
// (deterministic and unit-testable): `alarm_after_s × analysis_fps` frames to
// alarm, `recover_after_s × analysis_fps` clear frames to recover.
use crate::config::{CameraConfig, TamperConfig};
use crate::core::error::{EdgeError, EdgeResult};

use std::time::{Duration, Instant};

/// Downsampled luma grid resolution. 64×36 keeps the math trivial while
/// still localizing scene structure.
const GRID_W: usize = 64;
const GRID_H: usize = 36;
/// EMA weight for the scene reference: slow enough that lighting drift and
/// a person walking through don't move it, so a repointed camera alarms.
/// (A permanently moved camera self-recovers after the reference converges —
/// tens of minutes at 4 Hz.)
const REFERENCE_ALPHA: f32 = 0.02;
/// Analyzed frames before the reference is trusted for scene-change checks.
const REFERENCE_WARMUP: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TamperKind {
    Blackout,
    Blinding,
    Occlusion,
    Freeze,
    SceneChange,
}

impl TamperKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TamperKind::Blackout => "blackout",
            TamperKind::Blinding => "blinding",
            TamperKind::Occlusion => "occlusion",
            TamperKind::Freeze => "freeze",
            TamperKind::SceneChange => "scene_change",
        }
    }
}

/// An alarm state transition — emitted once when a tamper begins (active)
/// and once when it recovers (!active).
#[derive(Debug)]
pub struct TamperTransition {
    pub kind: TamperKind,
    pub active: bool,
    /// The measured statistic that crossed (or released) the threshold.
    pub value: f32,
    pub threshold: f32,
}

/// Sustain/recover debouncer counted in analyzed frames.
struct Condition {
    alarmed: bool,
    hit_streak: u32,
    clear_streak: u32,
}

impl Condition {
    fn new() -> Self {
        Self {
            alarmed: false,
            hit_streak: 0,
            clear_streak: 0,
        }
    }

    fn update(&mut self, hit: bool, alarm_after: u32, recover_after: u32) -> Option<bool> {
        if hit {
            self.hit_streak += 1;
            self.clear_streak = 0;
            if !self.alarmed && self.hit_streak >= alarm_after {
                self.alarmed = true;
                return Some(true);
            }
        } else {
            self.clear_streak += 1;
            self.hit_streak = 0;
            if self.alarmed && self.clear_streak >= recover_after {
                self.alarmed = false;
                return Some(false);
            }
        }
        None
    }
}

enum LumaLayout {
    Nv12,
    Yuy2,
}

pub struct TamperDetector {
    cfg: TamperConfig,
    layout: LumaLayout,
    frame_w: usize,
    frame_h: usize,
    min_interval: Duration,
    last_analysis: Option<Instant>,

    previous: Option<Vec<f32>>,
    reference: Option<Vec<f32>>,
    analyzed_frames: u32,
    alarm_after: u32,
    recover_after: u32,

    blackout: Condition,
    blinding: Condition,
    occlusion: Condition,
    freeze: Condition,
    scene_change: Condition,
}

impl TamperDetector {
    pub fn new(cfg: &TamperConfig, camera: &CameraConfig) -> EdgeResult<Self> {
        let layout = match camera.format.to_uppercase().as_str() {
            "NV12" => LumaLayout::Nv12,
            "YUYV" | "YUY2" => LumaLayout::Yuy2,
            other => {
                return Err(EdgeError::AiPanic(format!(
                    "tamper analytics support NV12/YUYV frames; camera.format is '{other}'"
                )))
            }
        };
        let fps = cfg.analysis_fps.max(1);
        Ok(Self {
            cfg: cfg.clone(),
            layout,
            frame_w: camera.width as usize,
            frame_h: camera.height as usize,
            min_interval: Duration::from_secs_f64(1.0 / f64::from(fps)),
            last_analysis: None,
            previous: None,
            reference: None,
            analyzed_frames: 0,
            alarm_after: (cfg.alarm_after_s * fps).max(1),
            recover_after: (cfg.recover_after_s * fps).max(1),
            blackout: Condition::new(),
            blinding: Condition::new(),
            occlusion: Condition::new(),
            freeze: Condition::new(),
            scene_change: Condition::new(),
        })
    }

    /// Analyze one frame (rate-limited internally). Returns state
    /// *transitions* only — steady alarm states emit nothing.
    pub fn analyze(&mut self, frame: &[u8]) -> Vec<TamperTransition> {
        if let Some(last) = self.last_analysis {
            if last.elapsed() < self.min_interval {
                return Vec::new();
            }
        }
        self.last_analysis = Some(Instant::now());
        self.analyze_now(frame)
    }

    /// The rate-unlimited core, used directly by tests.
    fn analyze_now(&mut self, frame: &[u8]) -> Vec<TamperTransition> {
        let Some(grid) = self.sample_grid(frame) else {
            return Vec::new();
        };
        self.analyzed_frames += 1;

        let mean = grid.iter().sum::<f32>() / grid.len() as f32;
        let detail = horizontal_detail(&grid);
        let freeze_diff = self
            .previous
            .as_ref()
            .map(|prev| mean_abs_diff(&grid, prev));
        let scene_diff = self
            .reference
            .as_ref()
            .filter(|_| self.analyzed_frames > REFERENCE_WARMUP)
            .map(|reference| mean_abs_diff(&grid, reference));

        let mut transitions = Vec::new();
        let mut push = |cond: Option<bool>, kind, value: f32, threshold: f32| {
            if let Some(active) = cond {
                transitions.push(TamperTransition {
                    kind,
                    active,
                    value,
                    threshold,
                });
            }
        };

        let is_dark = mean < self.cfg.dark_luma_max;
        push(
            self.blackout
                .update(is_dark, self.alarm_after, self.recover_after),
            TamperKind::Blackout,
            mean,
            self.cfg.dark_luma_max,
        );
        push(
            self.blinding.update(
                mean > self.cfg.bright_luma_min,
                self.alarm_after,
                self.recover_after,
            ),
            TamperKind::Blinding,
            mean,
            self.cfg.bright_luma_min,
        );
        // Occlusion = uniform image at normal brightness (blackout/blinding
        // already cover the extremes; avoid double alarms).
        push(
            self.occlusion.update(
                detail < self.cfg.low_detail_min && !is_dark && mean <= self.cfg.bright_luma_min,
                self.alarm_after,
                self.recover_after,
            ),
            TamperKind::Occlusion,
            detail,
            self.cfg.low_detail_min,
        );
        if let Some(diff) = freeze_diff {
            push(
                self.freeze.update(
                    diff < self.cfg.freeze_diff_max,
                    self.alarm_after,
                    self.recover_after,
                ),
                TamperKind::Freeze,
                diff,
                self.cfg.freeze_diff_max,
            );
        }
        if let Some(diff) = scene_diff {
            push(
                self.scene_change.update(
                    diff > self.cfg.scene_change_min,
                    self.alarm_after,
                    self.recover_after,
                ),
                TamperKind::SceneChange,
                diff,
                self.cfg.scene_change_min,
            );
        }

        // Update temporal state: previous is always the last frame; the
        // reference adapts slowly toward the current view.
        match self.reference.as_mut() {
            Some(reference) => {
                for (r, g) in reference.iter_mut().zip(&grid) {
                    *r += REFERENCE_ALPHA * (g - *r);
                }
            }
            None => self.reference = Some(grid.clone()),
        }
        self.previous = Some(grid);

        transitions
    }

    /// True while any tamper condition is in the alarmed state.
    pub fn any_active(&self) -> bool {
        self.blackout.alarmed
            || self.blinding.alarmed
            || self.occlusion.alarmed
            || self.freeze.alarmed
            || self.scene_change.alarmed
    }

    /// Sample the luma plane into a GRID_W×GRID_H grid of f32.
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

/// Mean horizontal neighbor difference — a crude but robust detail metric.
fn horizontal_detail(grid: &[f32]) -> f32 {
    let mut sum = 0.0;
    let mut count = 0u32;
    for row in grid.chunks_exact(GRID_W) {
        for pair in row.windows(2) {
            sum += (pair[0] - pair[1]).abs();
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        sum / count as f32
    }
}

fn mean_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / n as f32
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

    fn config() -> TamperConfig {
        TamperConfig {
            analysis_fps: 1,
            alarm_after_s: 3,
            recover_after_s: 2,
            ..TamperConfig::default()
        }
    }

    /// NV12 frame with a per-column gradient scaled around `base` luma.
    fn textured_frame(base: u8) -> Vec<u8> {
        let (w, h) = (128usize, 72usize);
        let mut frame = vec![128u8; w * h * 3 / 2];
        for y in 0..h {
            for x in 0..w {
                frame[y * w + x] = base.saturating_add((x % 64) as u8);
            }
        }
        frame
    }

    fn flat_frame(luma: u8) -> Vec<u8> {
        let (w, h) = (128usize, 72usize);
        let mut frame = vec![128u8; w * h * 3 / 2];
        frame[..w * h].fill(luma);
        frame
    }

    #[test]
    fn blackout_alarms_after_sustain_and_recovers() {
        let mut det = TamperDetector::new(&config(), &camera()).unwrap();
        // Warm up with a normal textured scene (also primes freeze immunity
        // by feeding *changing* frames).
        for i in 0..5 {
            let t = det.analyze_now(&textured_frame(60 + i));
            assert!(t.iter().all(|tr| tr.kind != TamperKind::Blackout));
        }
        // Dark frames: alarm on the 3rd consecutive hit (alarm_after=3).
        assert!(det.analyze_now(&flat_frame(5)).is_empty());
        assert!(det
            .analyze_now(&flat_frame(5))
            .iter()
            .all(|tr| tr.kind != TamperKind::Blackout));
        let third: Vec<_> = det.analyze_now(&flat_frame(5));
        assert!(third
            .iter()
            .any(|tr| tr.kind == TamperKind::Blackout && tr.active));
        assert!(det.any_active());

        // Recovery after 2 clear frames.
        let mut recovered = false;
        for i in 0..3 {
            let t = det.analyze_now(&textured_frame(60 + i));
            recovered |= t
                .iter()
                .any(|tr| tr.kind == TamperKind::Blackout && !tr.active);
        }
        assert!(recovered);
    }

    #[test]
    fn occlusion_triggers_on_flat_midtone() {
        let mut det = TamperDetector::new(&config(), &camera()).unwrap();
        for i in 0..4 {
            det.analyze_now(&textured_frame(60 + i));
        }
        let mut alarmed = false;
        for _ in 0..4 {
            alarmed |= det
                .analyze_now(&flat_frame(120))
                .iter()
                .any(|tr| tr.kind == TamperKind::Occlusion && tr.active);
        }
        assert!(alarmed);
    }

    #[test]
    fn freeze_triggers_on_identical_frames() {
        let mut det = TamperDetector::new(&config(), &camera()).unwrap();
        let frozen = textured_frame(60);
        for i in 0..3 {
            det.analyze_now(&textured_frame(60 + 3 * i)); // moving scene
        }
        let mut alarmed = false;
        for _ in 0..5 {
            alarmed |= det
                .analyze_now(&frozen)
                .iter()
                .any(|tr| tr.kind == TamperKind::Freeze && tr.active);
        }
        assert!(alarmed);
    }

    #[test]
    fn scene_change_triggers_after_reference_warmup() {
        let mut det = TamperDetector::new(&config(), &camera()).unwrap();
        for i in 0..(REFERENCE_WARMUP + 2) {
            det.analyze_now(&textured_frame(40 + (i % 3) as u8));
        }
        // "Repoint" the camera: inverted-brightness scene.
        let mut alarmed = false;
        for _ in 0..4 {
            alarmed |= det
                .analyze_now(&textured_frame(180))
                .iter()
                .any(|tr| tr.kind == TamperKind::SceneChange && tr.active);
        }
        assert!(alarmed);
    }
}
