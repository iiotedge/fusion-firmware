// src/radar/mock.rs
//
// Development radar for hosts/fleets with no real radar hardware attached —
// same role as hal/mock_cam.rs for the camera HAL. Honors [radar].type =
// "mock" explicitly (unlike the camera HAL, radar isn't auto-injected on
// non-Linux hosts, since it's an optional additive sensor, not something a
// dev host needs a stand-in for by default).
//
// Renders a single deterministic track sweeping across the configured FOV
// and back, closing then opening in range, so every analytics code path
// (zone presence, speed sign flip, directional counting) has something
// real to exercise end-to-end without hardware.
use crate::config::RadarConfig;
use crate::core::error::EdgeResult;
use crate::radar::{RadarDetections, RadarFrame, RadarHealth, RadarSource, RadarTrack};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::info;

pub struct MockRadar {
    range_min_m: f32,
    range_max_m: f32,
    fov_deg: f32,
    frame_interval: Duration,
    tick: u64,
}

impl MockRadar {
    pub fn new(cfg: &RadarConfig) -> Self {
        Self {
            range_min_m: cfg.range_min_m,
            range_max_m: cfg.range_max_m.max(cfg.range_min_m + 1.0),
            fov_deg: cfg.fov_deg.max(1.0),
            // No frame-rate field on RadarConfig (unlike camera's fps) —
            // point/track-list radars typically run a fixed internal scan
            // rate (10-20 Hz is common); 10 Hz is a reasonable stand-in for
            // a mock, not a claim about any real unit's actual rate.
            frame_interval: Duration::from_millis(100),
            tick: 0,
        }
    }

    fn synthetic_track(&self) -> RadarTrack {
        // Azimuth sweeps -fov/2..+fov/2 and back (triangle wave); range
        // closes from max to min and back out of phase with azimuth, so a
        // zone/line-cross rule sees the track both cross laterally AND
        // approach/recede over the course of one sweep.
        let period = 100u64; // ticks for one full sweep-and-back
        let phase = (self.tick % period) as f32 / period as f32; // 0..1
        let triangle = if phase < 0.5 {
            phase * 2.0
        } else {
            2.0 - phase * 2.0
        }; // 0..1..0
        let azimuth_deg = -self.fov_deg / 2.0 + triangle * self.fov_deg;
        let range_m = self.range_max_m - triangle * (self.range_max_m - self.range_min_m);
        // Closing (negative-range-derivative) while triangle is rising,
        // receding while falling — matches this firmware's own "positive =
        // closing" velocity convention (see RadarPoint's doc comment).
        let velocity_mps = if phase < 0.5 { 1.5 } else { -1.5 };

        RadarTrack {
            id: 1,
            range_m,
            azimuth_deg,
            velocity_mps,
            rcs_dbsm: 8.0, // plausible vehicle-scale RCS; not calibrated to any real target
            class_hint: Some("vehicle".to_string()),
        }
    }
}

impl RadarSource for MockRadar {
    fn initialize(&mut self) -> EdgeResult<()> {
        info!(
            range_min_m = self.range_min_m,
            range_max_m = self.range_max_m,
            fov_deg = self.fov_deg,
            "Mock radar initialized"
        );
        Ok(())
    }

    fn start(&mut self) -> EdgeResult<()> {
        info!("Starting mock radar (synthetic sweeping track)");
        Ok(())
    }

    fn next_frame(&mut self) -> EdgeResult<RadarFrame> {
        thread::sleep(self.frame_interval);
        self.tick += 1;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Ok(RadarFrame {
            timestamp_ns: now,
            detections: RadarDetections::Tracks(vec![self.synthetic_track()]),
        })
    }

    fn stop(&mut self) -> EdgeResult<()> {
        info!("Stopping mock radar");
        Ok(())
    }

    fn health(&self) -> RadarHealth {
        RadarHealth::default() // always clean — nothing to simulate failing yet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RadarConfig {
        RadarConfig {
            enabled: true,
            r#type: "mock".to_string(),
            range_min_m: 2.0,
            range_max_m: 50.0,
            fov_deg: 90.0,
            ..Default::default()
        }
    }

    #[test]
    fn sweeps_azimuth_within_the_configured_fov() {
        let mut radar = MockRadar::new(&cfg());
        radar.initialize().unwrap();
        for _ in 0..30 {
            radar.tick += 1;
            let track = radar.synthetic_track();
            assert!(track.azimuth_deg >= -45.0 - 1e-3 && track.azimuth_deg <= 45.0 + 1e-3);
        }
    }

    #[test]
    fn range_stays_within_configured_bounds() {
        let mut radar = MockRadar::new(&cfg());
        for _ in 0..200 {
            radar.tick += 1;
            let track = radar.synthetic_track();
            assert!(track.range_m >= 2.0 - 1e-3 && track.range_m <= 50.0 + 1e-3);
        }
    }

    #[test]
    fn velocity_sign_flips_between_closing_and_receding_halves() {
        let mut radar = MockRadar::new(&cfg());
        radar.tick = 10; // rising half (closing)
        assert!(radar.synthetic_track().velocity_mps > 0.0);
        radar.tick = 60; // falling half (receding)
        assert!(radar.synthetic_track().velocity_mps < 0.0);
    }
}
