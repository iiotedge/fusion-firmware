// src/storage.rs
//
// NVR-style local recording: a continuously running pipeline encodes the
// camera feed into fixed-length MP4 chunks (splitmuxsink), with a janitor
// thread enforcing size/age rotation — the classic industrial circular
// recorder. Runs independently of RTSP clients so evidence exists even when
// nobody is watching. Event-triggered clip extraction builds on these chunks
// in Phase 5.
pub mod clips;
pub mod export;

use crate::config::{AppConfig, StorageConfig};
use crate::core::error::{EdgeError, EdgeResult};
use crate::stream::encoder;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Cloneable handle exposing which chunk file is currently being written —
/// the event indexer records it so every event resolves to its evidence file.
#[derive(Clone, Default)]
pub struct ChunkTracker {
    current: Arc<parking_lot::Mutex<Option<String>>>,
}

impl ChunkTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current_chunk(&self) -> Option<String> {
        self.current.lock().clone()
    }
}

pub struct ChunkRecorder {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    /// Audio input, when configured — must receive EOS at shutdown too:
    /// splitmuxsink finalizes only after EOS on every input pad.
    audio_src: Option<gst_app::AppSrc>,
}

impl ChunkRecorder {
    /// Build and start the recording pipeline. The chunk directory is created
    /// if missing; chunk files are named `<device_id>_<UTC timestamp>.mp4` so
    /// they sort chronologically and survive restarts without collisions.
    /// `text_overlays` is the same fragment the RTSP pipeline uses, so the
    /// recorded evidence matches the live view. `tracker` learns each new
    /// chunk filename as splitmuxsink opens it. `audio` adds a sound track
    /// (splitmuxsink audio pad) fed by the shared microphone fanout.
    pub fn new(
        cfg: &AppConfig,
        text_overlays: &str,
        tracker: ChunkTracker,
        audio: Option<&crate::audio::AudioPlan>,
    ) -> EdgeResult<Self> {
        let storage = &cfg.storage;
        fs::create_dir_all(&storage.path)
            .map_err(|e| EdgeError::StreamFault(format!("create {}: {e}", storage.path)))?;

        gst::init().map_err(|e| EdgeError::StreamFault(format!("gstreamer init: {e}")))?;

        let plan = encoder::plan_encoder(&cfg.stream)?;
        let (caps, decode_fragment) = crate::media::source_caps(&cfg.camera);

        // splitmuxsink rotates the container every max-size-time nanoseconds,
        // always cutting on a keyframe so every chunk is independently playable.
        let audio_branch = audio
            .map(|plan| plan.recorder_branch.as_str())
            .unwrap_or("");
        let launch = format!(
            "appsrc name=recsrc is-live=true format=time do-timestamp=true block=false \
             ! queue max-size-buffers=4 leaky=downstream \
             ! {decode_fragment}videoconvert \
             ! {text_overlays}{encode} \
             ! splitmuxsink name=recsink muxer=mp4mux max-size-time={chunk_ns}{audio_branch}",
            encode = plan.encode_fragment,
            chunk_ns = u64::from(storage.chunk_seconds) * 1_000_000_000,
        );
        debug!(launch = %launch, "Chunk recorder pipeline");

        let pipeline = gst::parse::launch(&launch)
            .map_err(|e| EdgeError::StreamFault(format!("recorder pipeline parse: {e}")))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| EdgeError::StreamFault("recorder is not a Pipeline".into()))?;

        let appsrc = pipeline
            .by_name("recsrc")
            .ok_or_else(|| EdgeError::StreamFault("appsrc 'recsrc' missing".into()))?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| EdgeError::StreamFault("'recsrc' is not an appsrc".into()))?;
        appsrc
            .set_caps(Some(&gst::Caps::from_str(&caps).map_err(|e| {
                EdgeError::StreamFault(format!("recorder caps: {e}"))
            })?));

        // Timestamped chunk names via the format-location signal.
        let sink = pipeline
            .by_name("recsink")
            .ok_or_else(|| EdgeError::StreamFault("splitmuxsink 'recsink' missing".into()))?;
        let dir = storage.path.clone();
        let device_id = cfg.system.device_id.clone();
        sink.connect("format-location", false, move |_args| {
            let name = format!(
                "{dir}/{device_id}_{}.mp4",
                chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
            );
            *tracker.current.lock() = Some(name.clone());
            Some(name.to_value())
        });

        // Attach the recorder's audio input to the microphone fanout.
        let mut audio_src = None;
        if let Some(plan) = audio {
            match pipeline
                .by_name("audrec")
                .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            {
                Some(src) => {
                    plan.fanout.add(src.clone());
                    audio_src = Some(src);
                }
                None => warn!("audio configured but recorder has no 'audrec'"),
            }
        }

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| EdgeError::StreamFault("recorder refused to start".into()))?;

        info!(
            dir = %storage.path,
            chunk_seconds = storage.chunk_seconds,
            max_total_mb = storage.max_total_mb,
            encoder = %plan.element,
            "Chunk recorder running"
        );

        spawn_rotation_janitor(storage.clone());

        Ok(Self {
            pipeline,
            appsrc,
            audio_src,
        })
    }

    /// Feed one frame into the recorder. Takes a refcounted gst::Buffer
    /// shared with the RTSP path. Non-blocking; the leaky queue drops under
    /// congestion.
    pub fn push(&self, buffer: gst::Buffer, frame_id: u64) -> EdgeResult<()> {
        if let Err(flow) = self.appsrc.push_buffer(buffer) {
            return Err(EdgeError::StreamFault(format!(
                "recorder rejected frame {frame_id}: {flow:?}"
            )));
        }
        Ok(())
    }
}

impl Drop for ChunkRecorder {
    fn drop(&mut self) {
        // EOS lets splitmuxsink finalize the in-flight chunk's moov atom —
        // but finalization is asynchronous: wait for the EOS message to
        // travel the pipeline before tearing it down, or the last chunk is
        // left truncated ("moov atom not found").
        let _ = self.appsrc.end_of_stream();
        if let Some(audio) = &self.audio_src {
            let _ = audio.end_of_stream();
        }
        if let Some(bus) = self.pipeline.bus() {
            let _ = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        info!("Chunk recorder stopped; in-flight chunk finalized");
    }
}

/// Append-only event↔evidence index: one JSON line per camera event with
/// the chunk file that was recording at that moment (and the snapshot, when
/// one was taken), so the platform can resolve any event to its footage.
pub struct EventIndexer {
    index_path: PathBuf,
    tracker: ChunkTracker,
}

impl EventIndexer {
    pub fn new(storage_dir: &str, tracker: ChunkTracker) -> Self {
        if let Err(e) = fs::create_dir_all(storage_dir) {
            warn!("event index directory {storage_dir}: {e}");
        }
        Self {
            index_path: Path::new(storage_dir).join("events.jsonl"),
            tracker,
        }
    }

    pub fn record(&self, kind: &str, snapshot: Option<&str>, payload: &serde_json::Value) {
        let entry = serde_json::json!({
            "ts_ms": chrono::Utc::now().timestamp_millis(),
            "kind": kind,
            "chunk": self.tracker.current_chunk(),
            "snapshot": snapshot,
            "payload": payload,
        });
        let line = format!("{entry}\n");
        let result = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.index_path)
            .and_then(|mut file| file.write_all(line.as_bytes()));
        if let Err(e) = result {
            warn!("event index write failed: {e}");
        }
    }
}

/// Write an event snapshot into `<storage>/snapshots/`; returns its path.
/// Snapshots participate in the same size/age rotation as chunks.
pub fn save_snapshot(
    storage_dir: &str,
    device_id: &str,
    reason: &str,
    jpeg: &[u8],
) -> EdgeResult<String> {
    let dir = Path::new(storage_dir).join("snapshots");
    fs::create_dir_all(&dir)
        .map_err(|e| EdgeError::StreamFault(format!("create {}: {e}", dir.display())))?;
    let path = dir.join(format!(
        "{device_id}_{}_{reason}.jpg",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
    ));
    fs::write(&path, jpeg)
        .map_err(|e| EdgeError::StreamFault(format!("write {}: {e}", path.display())))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Background rotation: keep total size under max_total_mb (and optionally
/// drop chunks older than max_age_hours), always sparing the newest file —
/// splitmuxsink is still writing it.
fn spawn_rotation_janitor(cfg: StorageConfig) {
    let result = thread::Builder::new()
        .name("storage_janitor".to_string())
        .spawn(move || loop {
            if let Err(e) = rotate_once(&cfg) {
                warn!("Storage rotation pass failed: {e}");
            }
            if let Err(e) = clips::rotate_clips(&cfg) {
                warn!("Clip rotation pass failed: {e}");
            }
            thread::sleep(Duration::from_secs(30));
        });
    if let Err(e) = result {
        warn!("Failed to spawn storage janitor: {e}");
    }
}

fn rotate_once(cfg: &StorageConfig) -> std::io::Result<()> {
    // Chunks live in the root, snapshots in snapshots/ — both count against
    // the same rotation budget.
    let mut files = scan_media(Path::new(&cfg.path), "mp4")?;
    let snapshot_dir = Path::new(&cfg.path).join("snapshots");
    if snapshot_dir.is_dir() {
        files.extend(scan_media(&snapshot_dir, "jpg")?);
    }
    if files.len() <= 1 {
        return Ok(());
    }
    // Oldest first.
    files.sort_by_key(|(_, _, mtime)| *mtime);

    // The newest chunk is the one splitmuxsink is still writing — never
    // delete it (completed snapshots carry no such hazard).
    let in_flight = files
        .iter()
        .filter(|(path, _, _)| path.extension().and_then(|e| e.to_str()) == Some("mp4"))
        .max_by_key(|(_, _, mtime)| *mtime)
        .map(|(path, _, _)| path.clone());

    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    let cap_bytes = cfg.max_total_mb * 1024 * 1024;
    let now = std::time::SystemTime::now();

    for (path, size, mtime) in &files {
        if Some(path) == in_flight.as_ref() {
            continue;
        }
        let too_big = total > cap_bytes;
        let too_old = cfg.max_age_hours > 0
            && now
                .duration_since(*mtime)
                .map(|age| age.as_secs() > cfg.max_age_hours * 3600)
                .unwrap_or(false);
        if !too_big && !too_old {
            continue;
        }
        remove_chunk(path)?;
        total = total.saturating_sub(*size);
    }
    Ok(())
}

type MediaFile = (PathBuf, u64, std::time::SystemTime);

fn scan_media(dir: &Path, extension: &str) -> std::io::Result<Vec<MediaFile>> {
    Ok(fs::read_dir(dir)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(extension) {
                return None;
            }
            let meta = entry.metadata().ok()?;
            Some((path, meta.len(), meta.modified().ok()?))
        })
        .collect())
}

fn remove_chunk(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)?;
    debug!("Rotated out recording chunk {}", path.display());
    Ok(())
}

/// Storage health monitor: publishes used/free gauges for /metrics and warns
/// when the filesystem's free space crosses the configured watermark —
/// rotation caps bound OUR footprint, not the disk other software shares.
pub fn spawn_health_monitor(
    cfg: StorageConfig,
    metrics: std::sync::Arc<crate::core::metrics::Metrics>,
) {
    if !cfg.enabled {
        return;
    }
    let spawned = thread::Builder::new()
        .name("storage_health".to_string())
        .spawn(move || {
            let mut below_watermark = false;
            loop {
                let used: u64 = scan_media(Path::new(&cfg.path), "mp4")
                    .map(|files| files.iter().map(|(_, size, _)| size).sum())
                    .unwrap_or(0);
                metrics.storage_used_mb.set((used / (1024 * 1024)) as i64);

                if let Some(free) = free_bytes(Path::new(&cfg.path)) {
                    let free_mb = free / (1024 * 1024);
                    metrics.storage_free_mb.set(free_mb as i64);
                    let low = free_mb < cfg.min_free_mb;
                    if low && !below_watermark {
                        warn!(
                            free_mb,
                            watermark_mb = cfg.min_free_mb,
                            "Storage free space below watermark"
                        );
                    } else if !low && below_watermark {
                        info!(free_mb, "Storage free space recovered above watermark");
                    }
                    below_watermark = low;
                }
                thread::sleep(Duration::from_secs(30));
            }
        });
    if let Err(e) = spawned {
        warn!("failed to spawn storage health monitor: {e}");
    }
}

/// Free bytes on the filesystem containing `path` (unix statvfs).
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes into the zeroed struct on success; cpath is a
    // valid NUL-terminated path for the duration of the call.
    unsafe {
        let mut stats: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(cpath.as_ptr(), &mut stats) != 0 {
            return None;
        }
        Some(stats.f_bavail as u64 * stats.f_frsize as u64)
    }
}
