// src/storage/export.rs
//
// Evidence export (F3): mirror recordings/clips/snapshots to removable
// SD/USB media and/or upload them to a site server over FTPS — the two
// classic industrial evacuation paths for edge video evidence.
//
// One engine, two targets behind the `ExportTarget` trait:
//   * LocalDirTarget — copy onto mounted media (auto while present, or
//     manual full-sync via the command channel / GPIO button)
//   * FtpTarget — FTPS (rustls) or plain FTP, per-batch connections
//
// Only *finished* files are exported: the in-flight chunk is skipped, clips
// only after their manifest exists. Export state (relative paths) persists
// per target in the storage dir, so restarts never re-upload; entries whose
// local file has rotated away are pruned.
use crate::config::{FtpExportConfig, SdExportConfig, StorageConfig};
use crate::core::error::{EdgeError, EdgeResult};
use crate::core::metrics::Metrics;

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Manual "export everything now" request (command channel / GPIO button),
/// consumed by every export worker on its next tick.
#[derive(Clone, Default)]
pub struct ExportTrigger {
    fired: Arc<AtomicBool>,
}

impl ExportTrigger {
    pub fn fire(&self) {
        self.fired.store(true, Ordering::Relaxed);
    }

    fn take(&self) -> bool {
        self.fired.swap(false, Ordering::Relaxed)
    }
}

/// A destination for evidence files.
trait ExportTarget: Send {
    fn name(&self) -> &'static str;
    /// Cheap availability check before a batch (media mounted / server up).
    fn available(&mut self) -> bool;
    fn upload(&mut self, local: &Path, remote_rel: &str) -> EdgeResult<()>;
    /// Batch end: flush/disconnect.
    fn finish(&mut self);
}

/// What a target wants exported.
#[derive(Clone, Copy)]
struct Selection {
    chunks: bool,
    clips: bool,
    snapshots: bool,
    delete_chunks_after: bool,
}

/// Spawn workers for every enabled target. Returns the shared manual trigger
/// (also wired to the GPIO button when configured).
pub fn spawn(cfg: &StorageConfig, device_id: &str, metrics: Arc<Metrics>) -> Option<ExportTrigger> {
    if !cfg.enabled || (!cfg.sd.enabled && !cfg.ftp.enabled) {
        return None;
    }
    let trigger = ExportTrigger::default();

    if cfg.sd.enabled {
        let worker = Worker {
            storage_dir: PathBuf::from(&cfg.path),
            selection: Selection {
                chunks: cfg.sd.upload_chunks,
                clips: cfg.sd.upload_clips,
                snapshots: cfg.sd.upload_snapshots,
                delete_chunks_after: false,
            },
            interval: Duration::from_secs(cfg.sd.scan_interval_s.max(5)),
            auto: cfg.sd.auto,
            trigger: trigger.clone(),
            metrics: metrics.clone(),
        };
        let target = LocalDirTarget {
            root: Path::new(&cfg.sd.mount_path).join(device_id),
        };
        spawn_worker("export_sd", worker, target);
        info!(mount = %cfg.sd.mount_path, auto = cfg.sd.auto, "SD/USB export active");
        spawn_button(&cfg.sd, trigger.clone());
    }

    if cfg.ftp.enabled {
        let worker = Worker {
            storage_dir: PathBuf::from(&cfg.path),
            selection: Selection {
                chunks: cfg.ftp.upload_chunks,
                clips: cfg.ftp.upload_clips,
                snapshots: cfg.ftp.upload_snapshots,
                delete_chunks_after: cfg.ftp.delete_after_upload,
            },
            interval: Duration::from_secs(cfg.ftp.scan_interval_s.max(10)),
            auto: true,
            trigger: trigger.clone(),
            metrics,
        };
        let target = FtpTarget {
            cfg: cfg.ftp.clone(),
            remote_base: format!("{}/{}", cfg.ftp.remote_dir.trim_end_matches('/'), device_id),
            session: None,
        };
        if cfg.ftp.insecure_skip_verify {
            warn!(
                "FTP export accepts ANY server certificate (insecure_skip_verify) — \
                 fine for bring-up, replace with a trusted certificate for production"
            );
        }
        spawn_worker("export_ftp", worker, target);
        info!(host = %cfg.ftp.host, tls = cfg.ftp.tls, "FTP export active");
    }

    Some(trigger)
}

struct Worker {
    storage_dir: PathBuf,
    selection: Selection,
    interval: Duration,
    /// false = only manual triggers cause syncs.
    auto: bool,
    trigger: ExportTrigger,
    metrics: Arc<Metrics>,
}

fn spawn_worker(name: &'static str, worker: Worker, mut target: impl ExportTarget + 'static) {
    let spawned = thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let mut last_pass = Instant::now() - worker.interval; // first pass soon
            loop {
                let manual = worker.trigger.take();
                let due = worker.auto && last_pass.elapsed() >= worker.interval;
                if manual || due {
                    last_pass = Instant::now();
                    if target.available() {
                        if let Err(e) = run_batch(&worker, &mut target, manual) {
                            warn!(target = target.name(), "export batch failed: {e}");
                        }
                        target.finish();
                    } else if manual {
                        warn!(
                            target = target.name(),
                            "manual export requested but target is unavailable"
                        );
                    }
                }
                thread::sleep(Duration::from_secs(1));
            }
        });
    if let Err(e) = spawned {
        warn!("failed to spawn {name}: {e}");
    }
}

fn run_batch(worker: &Worker, target: &mut dyn ExportTarget, manual: bool) -> EdgeResult<()> {
    let candidates = collect_candidates(&worker.storage_dir, worker.selection).map_err(|e| {
        EdgeError::StreamFault(format!("scan {}: {e}", worker.storage_dir.display()))
    })?;

    let mut state = ExportState::load(&worker.storage_dir, target.name());
    state.prune(&candidates);

    let mut uploaded = 0u64;
    for (local, rel) in &candidates {
        if state.contains(rel) {
            continue;
        }
        match target.upload(local, rel) {
            Ok(()) => {
                state.record(rel);
                uploaded += 1;
                worker.metrics.files_exported.inc();
                debug!(target = target.name(), file = %rel, "exported");
                if worker.selection.delete_chunks_after && rel.starts_with("chunks/") {
                    if let Err(e) = fs::remove_file(local) {
                        warn!("delete after upload {}: {e}", local.display());
                    }
                }
            }
            Err(e) => {
                // Stop the batch on the first failure (server/media issue);
                // remaining files retry next pass.
                return Err(EdgeError::StreamFault(format!("upload {rel}: {e}")));
            }
        }
    }
    if uploaded > 0 || manual {
        info!(
            target = target.name(),
            uploaded,
            pending = candidates.len() as u64 - state.len().min(candidates.len() as u64),
            "export pass complete"
        );
    }
    Ok(())
}

/// (local path, relative remote path) for every finished evidence file.
fn collect_candidates(
    storage_dir: &Path,
    selection: Selection,
) -> std::io::Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();

    if selection.chunks {
        let mut chunks: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
        for entry in fs::read_dir(storage_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("mp4") {
                if let Ok(meta) = entry.metadata() {
                    chunks.push((path, meta.modified().unwrap_or(std::time::UNIX_EPOCH)));
                }
            }
        }
        // Newest chunk is still being written — never export it.
        chunks.sort_by_key(|(_, mtime)| *mtime);
        chunks.pop();
        for (path, _) in chunks {
            if let Some(name) = file_name(&path) {
                out.push((path.clone(), format!("chunks/{name}")));
            }
        }
    }

    if selection.snapshots {
        let dir = storage_dir.join("snapshots");
        if dir.is_dir() {
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) == Some("jpg") {
                    if let Some(name) = file_name(&path) {
                        out.push((path.clone(), format!("snapshots/{name}")));
                    }
                }
            }
        }
    }

    if selection.clips {
        let root = storage_dir.join("clips");
        if root.is_dir() {
            for entry in fs::read_dir(&root)? {
                let clip_dir = entry?.path();
                // A clip is complete once its manifest exists.
                if !clip_dir.is_dir() || !clip_dir.join("manifest.json").exists() {
                    continue;
                }
                let Some(clip_name) = file_name(&clip_dir) else {
                    continue;
                };
                for file in fs::read_dir(&clip_dir)? {
                    let path = file?.path();
                    if let Some(name) = file_name(&path) {
                        out.push((path.clone(), format!("clips/{clip_name}/{name}")));
                    }
                }
            }
        }
    }

    Ok(out)
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// Export state (per target, survives restarts)
// ---------------------------------------------------------------------------

struct ExportState {
    path: PathBuf,
    done: HashSet<String>,
}

impl ExportState {
    fn load(storage_dir: &Path, target: &str) -> Self {
        let path = storage_dir.join(format!(".exported_{target}.log"));
        let done = fs::read_to_string(&path)
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
        Self { path, done }
    }

    fn contains(&self, rel: &str) -> bool {
        self.done.contains(rel)
    }

    fn len(&self) -> u64 {
        self.done.len() as u64
    }

    fn record(&mut self, rel: &str) {
        self.done.insert(rel.to_string());
        let line = format!("{rel}\n");
        let appended = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = appended {
            warn!("export state write failed: {e}");
        }
    }

    /// Drop entries for files that rotation already deleted, and compact the
    /// on-disk log to match.
    fn prune(&mut self, candidates: &[(PathBuf, String)]) {
        let live: HashSet<&str> = candidates.iter().map(|(_, rel)| rel.as_str()).collect();
        let before = self.done.len();
        self.done.retain(|rel| live.contains(rel.as_str()));
        if self.done.len() != before {
            let mut body: Vec<&str> = self.done.iter().map(String::as_str).collect();
            body.sort_unstable();
            if let Err(e) = fs::write(&self.path, body.join("\n") + "\n") {
                warn!("export state compact failed: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Target: removable SD/USB media
// ---------------------------------------------------------------------------

struct LocalDirTarget {
    root: PathBuf,
}

impl ExportTarget for LocalDirTarget {
    fn name(&self) -> &'static str {
        "sd"
    }

    fn available(&mut self) -> bool {
        // Parent mount point must already exist (media inserted + mounted);
        // we create only our device subdirectory.
        let Some(mount) = self.root.parent() else {
            return false;
        };
        mount.is_dir() && fs::create_dir_all(&self.root).is_ok()
    }

    fn upload(&mut self, local: &Path, remote_rel: &str) -> EdgeResult<()> {
        let dest = self.root.join(remote_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| EdgeError::StreamFault(format!("mkdir {}: {e}", parent.display())))?;
        }
        fs::copy(local, &dest)
            .map_err(|e| EdgeError::StreamFault(format!("copy to {}: {e}", dest.display())))?;
        // Flush through to the media so an unplugged card still has the file.
        if let Ok(file) = fs::File::open(&dest) {
            let _ = file.sync_all();
        }
        Ok(())
    }

    fn finish(&mut self) {}
}

// ---------------------------------------------------------------------------
// Target: FTPS / FTP server
// ---------------------------------------------------------------------------

struct FtpTarget {
    cfg: FtpExportConfig,
    remote_base: String,
    session: Option<suppaftp::RustlsFtpStream>,
}

impl FtpTarget {
    fn connect(&mut self) -> EdgeResult<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let address = format!("{}:{}", self.cfg.host, self.cfg.port);
        let stream = suppaftp::RustlsFtpStream::connect(&address)
            .map_err(|e| EdgeError::StreamFault(format!("connect {address}: {e}")))?;

        let mut stream = if self.cfg.tls {
            let connector =
                suppaftp::RustlsConnector::from(tls_config(self.cfg.insecure_skip_verify)?);
            stream
                .into_secure(connector, &self.cfg.host)
                .map_err(|e| EdgeError::StreamFault(format!("FTPS handshake: {e}")))?
        } else {
            stream
        };

        stream
            .login(&self.cfg.username, &self.cfg.password)
            .map_err(|e| EdgeError::StreamFault(format!("FTP login: {e}")))?;
        stream
            .transfer_type(suppaftp::types::FileType::Binary)
            .map_err(|e| EdgeError::StreamFault(format!("FTP binary mode: {e}")))?;
        self.session = Some(stream);
        Ok(())
    }
}

impl ExportTarget for FtpTarget {
    fn name(&self) -> &'static str {
        "ftp"
    }

    fn available(&mut self) -> bool {
        match self.connect() {
            Ok(()) => true,
            Err(e) => {
                debug!("FTP unavailable: {e}");
                false
            }
        }
    }

    fn upload(&mut self, local: &Path, remote_rel: &str) -> EdgeResult<()> {
        self.connect()?;
        let session = self.session.as_mut().expect("connected above");
        let remote_path = format!("{}/{}", self.remote_base, remote_rel);

        // mkdir -p (best effort — MKD on an existing dir errors, which is fine).
        let mut dir = String::new();
        if let Some((dirs, _file)) = remote_path.rsplit_once('/') {
            for part in dirs.split('/').filter(|p| !p.is_empty()) {
                dir.push('/');
                dir.push_str(part);
                let _ = session.mkdir(&dir);
            }
        }

        let mut file = fs::File::open(local)
            .map_err(|e| EdgeError::StreamFault(format!("open {}: {e}", local.display())))?;
        session.put_file(&remote_path, &mut file).map_err(|e| {
            // A failed transfer poisons the control connection state — drop
            // the session so the next attempt reconnects cleanly.
            self.session = None;
            EdgeError::StreamFault(format!("STOR {remote_path}: {e}"))
        })?;
        Ok(())
    }

    fn finish(&mut self) {
        if let Some(mut session) = self.session.take() {
            let _ = session.quit();
        }
    }
}

/// rustls client config for FTPS: webpki (Mozilla) roots, or a
/// certificate-blind verifier when explicitly configured.
fn tls_config(insecure_skip_verify: bool) -> EdgeResult<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| EdgeError::StreamFault(format!("TLS versions: {e}")))?;

    let config = if insecure_skip_verify {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// Certificate-blind verifier for self-signed factory FTP appliances.
/// Selected only by explicit `insecure_skip_verify = true` (loudly logged).
#[derive(Debug)]
struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

// ---------------------------------------------------------------------------
// Export button (GPIO, Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn spawn_button(cfg: &SdExportConfig, trigger: ExportTrigger) {
    if cfg.button_gpio_chip.trim().is_empty() {
        return;
    }
    let chip_path = cfg.button_gpio_chip.clone();
    let line_offset = cfg.button_gpio_line;
    let active_low = cfg.button_active_low;
    let spawned = thread::Builder::new()
        .name("export_button".to_string())
        .spawn(move || {
            use gpio_cdev::{Chip, EventRequestFlags, LineRequestFlags};
            let mut chip = match Chip::new(&chip_path) {
                Ok(chip) => chip,
                Err(e) => {
                    warn!("export button disabled: open {chip_path}: {e}");
                    return;
                }
            };
            let line = match chip.get_line(line_offset) {
                Ok(line) => line,
                Err(e) => {
                    warn!("export button disabled: line {line_offset}: {e}");
                    return;
                }
            };
            let edge = if active_low {
                EventRequestFlags::FALLING_EDGE
            } else {
                EventRequestFlags::RISING_EDGE
            };
            let events = match line.events(LineRequestFlags::INPUT, edge, "iiotedge-export") {
                Ok(events) => events,
                Err(e) => {
                    warn!("export button disabled: request events: {e}");
                    return;
                }
            };
            info!(chip = %chip_path, line = line_offset, "Export button armed");
            let mut last_press = Instant::now() - Duration::from_secs(2);
            for event in events {
                if event.is_err() {
                    continue;
                }
                // Debounce: mechanical buttons bounce for a few ms.
                if last_press.elapsed() >= Duration::from_millis(500) {
                    last_press = Instant::now();
                    info!("Export button pressed — manual export triggered");
                    trigger.fire();
                }
            }
        });
    if let Err(e) = spawned {
        warn!("failed to spawn export button thread: {e}");
    }
}

#[cfg(not(target_os = "linux"))]
fn spawn_button(cfg: &SdExportConfig, _trigger: ExportTrigger) {
    if !cfg.button_gpio_chip.trim().is_empty() {
        warn!("export button is Linux-only (gpio-cdev); ignored on this host");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("export_test_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn candidates_skip_inflight_chunk_and_incomplete_clips() {
        let dir = temp_dir("cand");
        fs::write(dir.join("cam_a.mp4"), b"old").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(dir.join("cam_b.mp4"), b"newest-in-flight").unwrap();
        fs::create_dir_all(dir.join("snapshots")).unwrap();
        fs::write(dir.join("snapshots/s1.jpg"), b"jpg").unwrap();
        fs::create_dir_all(dir.join("clips/done")).unwrap();
        fs::write(dir.join("clips/done/manifest.json"), b"{}").unwrap();
        fs::write(dir.join("clips/done/part.mp4"), b"clip").unwrap();
        fs::create_dir_all(dir.join("clips/pending")).unwrap();
        fs::write(dir.join("clips/pending/part.mp4"), b"incomplete").unwrap();

        let sel = Selection {
            chunks: true,
            clips: true,
            snapshots: true,
            delete_chunks_after: false,
        };
        let mut rels: Vec<String> = collect_candidates(&dir, sel)
            .unwrap()
            .into_iter()
            .map(|(_, rel)| rel)
            .collect();
        rels.sort();
        assert_eq!(
            rels,
            vec![
                "chunks/cam_a.mp4",
                "clips/done/manifest.json",
                "clips/done/part.mp4",
                "snapshots/s1.jpg",
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_survives_reload_and_prunes_rotated_files() {
        let dir = temp_dir("state");
        let mut state = ExportState::load(&dir, "test");
        state.record("chunks/a.mp4");
        state.record("chunks/b.mp4");

        let mut reloaded = ExportState::load(&dir, "test");
        assert!(reloaded.contains("chunks/a.mp4"));
        assert!(reloaded.contains("chunks/b.mp4"));

        // Only b still exists locally → a is pruned from memory and disk.
        let live = vec![(dir.join("b.mp4"), "chunks/b.mp4".to_string())];
        reloaded.prune(&live);
        assert!(!reloaded.contains("chunks/a.mp4"));
        let on_disk = fs::read_to_string(dir.join(".exported_test.log")).unwrap();
        assert_eq!(on_disk.trim(), "chunks/b.mp4");
        let _ = fs::remove_dir_all(&dir);
    }
}
