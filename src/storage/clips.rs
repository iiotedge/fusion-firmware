// src/storage/clips.rs
//
// Event-triggered clips, NVR style: when an event fires, wait for the
// post-roll to finish recording, then **hardlink** the recording chunks that
// overlap [event − pre_roll, event + post_roll] into
// `<storage>/clips/<reason>_<timestamp>/` next to a JSON manifest.
//
// Hardlinks cost no disk space while the source chunk still exists, and keep
// the bytes alive after the circular recorder rotates the original out — the
// clip becomes the retained copy exactly when it matters. No re-encoding, no
// I/O spikes.
use crate::config::StorageConfig;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, Utc};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Extra settle time after the post-roll so splitmuxsink has rotated past
/// the last relevant chunk (its moov atom is final once the next chunk opens).
const FINALIZE_MARGIN_S: u64 = 5;

struct ClipJob {
    reason: String,
    event_time: DateTime<Utc>,
    payload: serde_json::Value,
}

/// Cheap cloneable handle; the extraction worker owns the waiting and I/O.
#[derive(Clone)]
pub struct ClipExtractor {
    tx: mpsc::SyncSender<ClipJob>,
    /// Per-reason debounce: one clip covers the whole pre+post window, so a
    /// storm of identical events collapses into a single clip.
    recent: std::sync::Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
    window_s: u64,
}

impl ClipExtractor {
    /// None when clips are disabled. `device_id` scopes chunk-filename parsing.
    pub fn spawn(cfg: &StorageConfig, device_id: &str) -> Option<Self> {
        if !cfg.enabled || !cfg.clips_enabled {
            return None;
        }
        // Small queue: clip extraction is rare and sequential by design.
        let (tx, rx) = mpsc::sync_channel::<ClipJob>(16);
        let worker_cfg = cfg.clone();
        let worker_device = device_id.to_string();
        let spawned = thread::Builder::new()
            .name("clip_extractor".to_string())
            .spawn(move || {
                for job in rx {
                    wait_for_post_roll(&job, &worker_cfg);
                    if let Err(e) = extract(&worker_cfg, &worker_device, &job) {
                        warn!("clip extraction for '{}' failed: {e}", job.reason);
                    }
                }
            });
        if let Err(e) = spawned {
            warn!("Failed to spawn clip extractor: {e}");
            return None;
        }
        info!(
            pre_roll_s = cfg.clip_pre_roll_s,
            post_roll_s = cfg.clip_post_roll_s,
            "Event clip extraction active"
        );
        Some(Self {
            tx,
            recent: std::sync::Arc::new(Mutex::new(HashMap::new())),
            window_s: cfg.clip_post_roll_s.max(1),
        })
    }

    /// Request a clip around "now" for this event. Non-blocking; duplicate
    /// events inside the post-roll window collapse into the pending clip.
    pub fn request(&self, reason: &str, payload: serde_json::Value) {
        let now = Utc::now();
        {
            let mut recent = self.recent.lock();
            if let Some(last) = recent.get(reason) {
                if (now - *last).num_seconds() < self.window_s as i64 {
                    debug!("clip for '{reason}' already pending; skipping duplicate");
                    return;
                }
            }
            recent.insert(reason.to_string(), now);
        }
        let job = ClipJob {
            reason: reason.to_string(),
            event_time: now,
            payload,
        };
        if self.tx.try_send(job).is_err() {
            warn!("clip queue full; dropping clip request for '{reason}'");
        }
    }
}

fn wait_for_post_roll(job: &ClipJob, cfg: &StorageConfig) {
    let ready_at = job.event_time
        + ChronoDuration::seconds(
            (cfg.clip_post_roll_s + u64::from(cfg.chunk_seconds) + FINALIZE_MARGIN_S) as i64,
        );
    let wait = (ready_at - Utc::now()).num_milliseconds();
    if wait > 0 {
        thread::sleep(Duration::from_millis(wait as u64));
    }
}

fn extract(cfg: &StorageConfig, device_id: &str, job: &ClipJob) -> std::io::Result<()> {
    let window_start = job.event_time - ChronoDuration::seconds(cfg.clip_pre_roll_s as i64);
    let window_end = job.event_time + ChronoDuration::seconds(cfg.clip_post_roll_s as i64);

    let chunks = chunks_overlapping(
        Path::new(&cfg.path),
        device_id,
        u64::from(cfg.chunk_seconds),
        window_start,
        window_end,
    )?;
    if chunks.is_empty() {
        warn!(
            "no recording chunks cover {} — was the recorder running?",
            job.event_time
        );
        return Ok(());
    }

    let clip_dir = Path::new(&cfg.path).join("clips").join(format!(
        "{}_{}",
        job.event_time.format("%Y%m%dT%H%M%SZ"),
        sanitize_reason(&job.reason)
    ));
    fs::create_dir_all(&clip_dir)?;

    let mut linked = Vec::new();
    for chunk in &chunks {
        let file_name = chunk.file_name().unwrap_or_default();
        let target = clip_dir.join(file_name);
        if target.exists() {
            continue;
        }
        // Hardlink keeps the bytes alive past chunk rotation for free; fall
        // back to a copy on filesystems without hardlink support.
        if fs::hard_link(chunk, &target).is_err() {
            fs::copy(chunk, &target)?;
        }
        linked.push(file_name.to_string_lossy().into_owned());
    }

    let manifest = serde_json::json!({
        "device_id": device_id,
        "reason": job.reason,
        "event_time": job.event_time.to_rfc3339(),
        "window_start": window_start.to_rfc3339(),
        "window_end": window_end.to_rfc3339(),
        "chunk_seconds": cfg.chunk_seconds,
        "files": linked,
        "event": job.payload,
    });
    fs::write(
        clip_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    )?;

    info!(
        clip = %clip_dir.display(),
        chunks = linked.len(),
        reason = %job.reason,
        "Event clip extracted"
    );
    Ok(())
}

/// Chunks whose [start, start+chunk_len] window overlaps [from, to]. Chunk
/// start time is parsed from the `<device_id>_<UTC>.mp4` filename written by
/// the recorder's format-location handler.
fn chunks_overlapping(
    dir: &Path,
    device_id: &str,
    chunk_seconds: u64,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> std::io::Result<Vec<PathBuf>> {
    let mut hits = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mp4") {
            continue;
        }
        let Some(start) = chunk_start_time(&path, device_id) else {
            continue;
        };
        let end = start + ChronoDuration::seconds(chunk_seconds as i64);
        if start <= to && end >= from {
            hits.push(path);
        }
    }
    hits.sort();
    Ok(hits)
}

/// Parse `<device_id>_<%Y%m%dT%H%M%SZ>.mp4` → chunk start time.
fn chunk_start_time(path: &Path, device_id: &str) -> Option<DateTime<Utc>> {
    let stem = path.file_stem()?.to_str()?;
    let timestamp = stem.strip_prefix(&format!("{device_id}_"))?;
    NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|naive| naive.and_utc())
}

fn sanitize_reason(reason: &str) -> String {
    reason
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Rotate `<storage>/clips`: remove whole oldest clip directories while the
/// tree exceeds max_clips_mb. Sizes are apparent (hardlinked bytes count even
/// when shared with a live chunk) — a deliberate, conservative approximation.
pub(super) fn rotate_clips(cfg: &StorageConfig) -> std::io::Result<()> {
    let clips_root = Path::new(&cfg.path).join("clips");
    if !clips_root.is_dir() {
        return Ok(());
    }

    let mut clips: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    for entry in fs::read_dir(&clips_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let mut size = 0u64;
        for file in fs::read_dir(&path)? {
            if let Ok(meta) = file?.metadata() {
                size += meta.len();
            }
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        clips.push((path, size, mtime));
    }

    clips.sort_by_key(|(_, _, mtime)| *mtime);
    let mut total: u64 = clips.iter().map(|(_, size, _)| size).sum();
    let cap = cfg.max_clips_mb * 1024 * 1024;
    for (path, size, _) in &clips {
        if total <= cap {
            break;
        }
        fs::remove_dir_all(path)?;
        debug!("Rotated out clip {}", path.display());
        total = total.saturating_sub(*size);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chunk_start_from_filename() {
        let path = Path::new("recordings/cam_line1_inspect_04_20260716T070102Z.mp4");
        let start = chunk_start_time(path, "cam_line1_inspect_04").unwrap();
        assert_eq!(start.to_rfc3339(), "2026-07-16T07:01:02+00:00");
        // Wrong device prefix → no parse.
        assert!(chunk_start_time(path, "other_device").is_none());
    }

    #[test]
    fn overlap_window_selects_correct_chunks() {
        let dir = std::env::temp_dir().join(format!("clip_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let device = "cam";
        for ts in ["20260716T070000Z", "20260716T070100Z", "20260716T070200Z"] {
            fs::write(dir.join(format!("{device}_{ts}.mp4")), b"x").unwrap();
        }
        let event = NaiveDateTime::parse_from_str("20260716T070130Z", "%Y%m%dT%H%M%SZ")
            .unwrap()
            .and_utc();
        // ±20 s window around 07:01:30 → only the 07:01:00 chunk overlaps.
        let hits = chunks_overlapping(
            &dir,
            device,
            60,
            event - ChronoDuration::seconds(20),
            event + ChronoDuration::seconds(20),
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].to_string_lossy().contains("20260716T070100Z"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
