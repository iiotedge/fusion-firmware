// src/telemetry.rs
//
// Bridge to the iiotedge-lib SDK (consumed as-is, never modified here).
//
// The lib is tokio-based while the video path runs on real-time threads, so
// the whole engine — SQLite store-and-forward buffer, MQTT northbound
// (GDE JSON envelope or Sparkplug B), TLS, and the config-declared
// southbound machine drivers (serial, CAN, Modbus, …) — lives on its own
// runtime thread. The rest of the firmware only ever touches the cheap,
// non-blocking `Telemetry` handle: persist-first semantics mean an accepted
// event is written to disk before any network attempt, so broker outages and
// power loss never lose data.
use crate::config::AppConfig;

use iiotedge_core::traits::Processor;
use iiotedge_core::types::{ContentType, ProtocolType, UnifiedPayload};
use iiotedge_core::{EdgeConfig, EngineBuilder, Ingestor};
use iiotedge_protocols::{build_drivers, MqttTransport};
use iiotedge_security::build_client_tls;
use iiotedge_storage::SqliteBuffer;

use bytes::Bytes;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct Telemetry {
    ingestor: Ingestor,
    device_id: String,
}

impl Telemetry {
    /// Boot the SDK engine from the [telemetry].edge_config file. `processors`
    /// hook into the engine's ingest path (e.g. the correlation tap). Returns
    /// None (with a log) when telemetry is disabled or the engine cannot
    /// start — the camera keeps capturing/streaming/recording regardless.
    pub fn start(cfg: &AppConfig, processors: Vec<Arc<dyn Processor>>) -> Option<Self> {
        if !cfg.telemetry.enabled {
            info!("Telemetry disabled by config");
            return None;
        }

        let edge_config_path = cfg.telemetry.edge_config.clone();
        let (handle_tx, handle_rx) = mpsc::channel::<Result<Ingestor, String>>();

        let spawned = thread::Builder::new()
            .name("telemetry_engine".to_string())
            .spawn(move || run_engine(&edge_config_path, processors, handle_tx));
        if let Err(e) = spawned {
            error!("Failed to spawn telemetry engine thread: {e}");
            return None;
        }

        match handle_rx.recv() {
            Ok(Ok(ingestor)) => {
                info!("Telemetry engine started (persist-first pipeline up)");
                Some(Self {
                    ingestor,
                    device_id: cfg.system.device_id.clone(),
                })
            }
            Ok(Err(reason)) => {
                error!("Telemetry engine failed to start: {reason}");
                None
            }
            Err(_) => {
                error!("Telemetry engine thread died before reporting readiness");
                None
            }
        }
    }

    /// Publish a camera event as a GDE JSON payload. `kind` becomes the
    /// source-id suffix (`camera/<device>/<kind>`), which the northbound
    /// transport maps to topic/metric names. Never blocks: on a full ingest
    /// queue the event is dropped with a warning (backpressure).
    pub fn publish_json(&self, kind: &str, body: serde_json::Value) {
        let source_id = format!("camera/{}/{}", self.device_id, kind);
        let payload = UnifiedPayload::now(
            source_id,
            ProtocolType::HostApp,
            ContentType::Json,
            Bytes::from(body.to_string()),
        );
        if let Err(e) = self.ingestor.try_ingest(payload) {
            warn!("Telemetry ingest rejected {kind} event: {e}");
        }
    }
}

/// Runs on the dedicated engine thread: builds a tokio runtime, composes the
/// SDK engine exactly like the lib's own composition root, then parks
/// forever servicing the pipeline.
fn run_engine(
    config_path: &str,
    processors: Vec<Arc<dyn Processor>>,
    ready: mpsc::Sender<Result<Ingestor, String>>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(format!("tokio runtime: {e}")));
            return;
        }
    };

    runtime.block_on(async move {
        let config = match EdgeConfig::from_file(config_path) {
            Ok(c) => c,
            Err(e) => {
                let _ = ready.send(Err(format!("load {config_path}: {e}")));
                return;
            }
        };

        if config.northbound.transport != "mqtt" {
            let _ = ready.send(Err(format!(
                "northbound.transport '{}' not enabled in this firmware build (mqtt only)",
                config.northbound.transport
            )));
            return;
        }

        let buffer = match SqliteBuffer::open(&config.buffer) {
            Ok(b) => Arc::new(b),
            Err(e) => {
                let _ = ready.send(Err(format!("open store-and-forward buffer: {e}")));
                return;
            }
        };

        let tls = match build_client_tls(&config.northbound.tls) {
            Ok(t) => t,
            Err(e) => {
                let _ = ready.send(Err(format!("TLS config: {e}")));
                return;
            }
        };

        let drivers = match build_drivers(&config) {
            Ok(d) => d,
            Err(e) => {
                let _ = ready.send(Err(format!("southbound drivers: {e}")));
                return;
            }
        };
        if !drivers.is_empty() {
            info!("Southbound machine drivers configured: {}", drivers.len());
        }

        let transport = Arc::new(MqttTransport::new(
            config.northbound.mqtt.clone(),
            config.node.clone(),
            tls,
        ));

        let mut builder = EngineBuilder::new(config)
            .buffer(buffer)
            .transport(transport)
            .drivers(drivers);
        for processor in processors {
            builder = builder.processor(processor);
        }
        let engine = match builder.start().await {
            Ok(handle) => handle,
            Err(e) => {
                let _ = ready.send(Err(format!("engine start: {e}")));
                return;
            }
        };

        let _ = ready.send(Ok(engine.ingestor()));

        // The engine's tasks run on this runtime; keep it alive for the
        // process lifetime. Graceful engine shutdown arrives with the
        // firmware-wide shutdown rework (TODO.md Phase 13).
        std::future::pending::<()>().await;
    });
}
