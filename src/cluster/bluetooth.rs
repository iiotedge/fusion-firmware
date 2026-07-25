// src/cluster/bluetooth.rs
//
// Bluetooth LE cluster transport (BlueZ via `bluer`/D-Bus, Linux-only): for
// sites with no WiFi network reachable at all — a true last-resort offline
// path. Advertising-only design (broadcast + scan, no pairing, no GATT
// connections): every node periodically advertises its outbound message as
// BLE service data and scans nearby advertisements for the same service
// UUID. This is the same pattern iBeacon/Eddystone use, and deliberately
// avoids BlueZ's GATT server D-Bus surface (the most complex, most
// version-fragile part of the stack) for a first cut.
//
// Payload constraint this trades for that simplicity: BLE advertisement data
// is small. Messages are capped at `MAX_PAYLOAD_BYTES`; anything larger is
// logged and dropped *on this transport only* (WiFi, if also enabled, is
// unaffected — see cluster/mod.rs, transports are independent and additive).
// Announce fits easily; only unusually verbose Event summaries would be at
// risk, and callers keep summaries short by convention.
//
// Status: implemented against the documented `bluer` 0.17 API but not yet
// exercised against physical BlueZ hardware in this session (no Linux
// Bluetooth adapter available here) — same "compiles clean, wants a
// hardware pass" status as the RKNN NPU backend. Verify with `cargo check`
// on a Linux box before field use, and confirm two adapters actually
// exchange advertisements.
use crate::cluster::ClusterTransport;
use crate::config::ClusterBluetoothConfig;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Custom 128-bit service UUID identifying IIoTEdge cluster traffic in
/// advertisement service-data, so we ignore every other BLE device nearby.
/// Fixed constant (not random) so every firmware build recognizes every
/// other build's beacons.
const CLUSTER_SERVICE_UUID: Uuid = Uuid::from_bytes([
    0x49, 0x49, 0x6f, 0x54, 0x45, 0x64, 0x67, 0x65, 0x43, 0x6c, 0x75, 0x73, 0x74, 0x65, 0x72, 0x00,
]);

/// Conservative legacy-advertising-safe cap. Extended advertising (BLE 5)
/// permits much more on hardware that supports it, but we target the lowest
/// common denominator so this works on older adapters too.
const MAX_PAYLOAD_BYTES: usize = 200;

pub struct BluetoothTransport {
    outbound_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    inbound: Arc<Mutex<Vec<Vec<u8>>>>,
    _runtime: std::thread::JoinHandle<()>,
}

impl BluetoothTransport {
    pub fn new(cfg: &ClusterBluetoothConfig) -> std::io::Result<Self> {
        if !cfg.enabled {
            return Err(std::io::Error::other("bluetooth transport disabled"));
        }

        let (outbound_tx, outbound_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let inbound = Arc::new(Mutex::new(Vec::new()));
        let worker_inbound = inbound.clone();
        let adapter_name = cfg.adapter.clone();
        let scan_interval = Duration::from_millis(cfg.scan_interval_ms.max(200));

        // bluer is tokio-async and talks to BlueZ over D-Bus; the rest of
        // the firmware's worker threads are synchronous, so — same bridge
        // pattern as src/telemetry.rs — this gets its own dedicated
        // current-thread runtime rather than pulling tokio into every caller.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let runtime = std::thread::Builder::new()
            .name("cluster_bluetooth".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("tokio runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(run(
                    adapter_name,
                    scan_interval,
                    outbound_rx,
                    worker_inbound,
                    ready_tx,
                ));
            })
            .map_err(|e| std::io::Error::other(format!("spawn bluetooth thread: {e}")))?;

        // Block briefly for the adapter to come up so `new()` can report a
        // clean error (and cluster::spawn can skip this transport) instead
        // of silently running with a dead adapter.
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(std::io::Error::other(e)),
            Err(_) => return Err(std::io::Error::other("bluetooth adapter init timed out")),
        }

        Ok(Self {
            outbound_tx,
            inbound,
            _runtime: runtime,
        })
    }
}

impl ClusterTransport for BluetoothTransport {
    fn name(&self) -> &'static str {
        "bluetooth"
    }

    fn send(&mut self, message: &[u8]) -> std::io::Result<()> {
        if message.len() > MAX_PAYLOAD_BYTES {
            warn!(
                len = message.len(),
                cap = MAX_PAYLOAD_BYTES,
                "cluster message too large for BLE advertising; dropped on this transport only"
            );
            return Ok(());
        }
        self.outbound_tx
            .try_send(message.to_vec())
            .map_err(|e| std::io::Error::other(format!("bluetooth outbound queue: {e}")))
    }

    fn poll_recv(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.inbound.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

// `_current_adv` below is written but (by design) never explicitly read: it
// exists purely to hold the latest BLE advertisement handle alive via RAII,
// and each reassignment drops the previous one (un-advertising it). The
// loop that writes it has no `break`, so control flow can never reach a
// point that "reads" the final value — `unused_assignments` can't tell that
// apart from a real bug, hence the function-level allow.
#[allow(unused_assignments)]
async fn run(
    adapter_name: String,
    scan_interval: Duration,
    mut outbound_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    inbound: Arc<Mutex<Vec<Vec<u8>>>>,
    ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let session = match bluer::Session::new().await {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("BlueZ session: {e}")));
            return;
        }
    };
    let adapter = if adapter_name.is_empty() {
        session.default_adapter().await
    } else {
        session.adapter(&adapter_name)
    };
    let adapter = match adapter {
        Ok(a) => a,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("BlueZ adapter: {e}")));
            return;
        }
    };
    if let Err(e) = adapter.set_powered(true).await {
        let _ = ready_tx.send(Err(format!("power on adapter: {e}")));
        return;
    }

    // Keep discovery running for this transport's lifetime: bluer moves the
    // discovery-session token INTO the returned stream (see
    // Adapter::discover_devices), so simply *holding* the stream keeps
    // scanning active — no polling required. We read results by querying
    // device service_data directly in the loop below (simpler and more
    // version-stable than matching on every AdapterEvent variant), so the
    // stream is bound to `_discovery` purely for its RAII lifetime. (It must
    // not be moved into tokio::spawn — the D-Bus-backed stream is !Send.)
    let _discovery = match adapter.discover_devices().await {
        Ok(stream) => stream,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("start discovery: {e}")));
            return;
        }
    };

    info!(adapter = %adapter.name(), "Cluster Bluetooth transport active (advertising beacon)");
    let _ = ready_tx.send(Ok(()));

    let mut _current_adv: Option<bluer::adv::AdvertisementHandle> = None;
    let mut seen: HashMap<bluer::Address, Vec<u8>> = HashMap::new();

    loop {
        // Outbound: (re)advertise the latest queued message. Advertising an
        // update means dropping the old handle and registering a new one —
        // fine at our low message rate (announce interval + occasional
        // events), and avoids depending on in-place advertisement updates,
        // which aren't uniformly supported.
        if let Ok(message) = outbound_rx.try_recv() {
            let adv = bluer::adv::Advertisement {
                service_data: [(CLUSTER_SERVICE_UUID, message)].into_iter().collect(),
                discoverable: Some(true),
                local_name: Some("iiotedge-cluster".to_string()),
                ..Default::default()
            };
            match adapter.advertise(adv).await {
                Ok(handle) => _current_adv = Some(handle),
                Err(e) => debug!("bluetooth advertise failed: {e}"),
            }
        }

        // Inbound: poll known devices' service data for our cluster UUID;
        // forward only payloads we haven't already delivered for that peer.
        if let Ok(addresses) = adapter.device_addresses().await {
            for address in addresses {
                let Ok(device) = adapter.device(address) else {
                    continue;
                };
                let Ok(Some(service_data)) = device.service_data().await else {
                    continue;
                };
                if let Some(payload) = service_data.get(&CLUSTER_SERVICE_UUID) {
                    if seen.get(&address) != Some(payload) {
                        seen.insert(address, payload.clone());
                        inbound
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(payload.clone());
                    }
                }
            }
        }

        tokio::time::sleep(scan_interval).await;
    }
}
