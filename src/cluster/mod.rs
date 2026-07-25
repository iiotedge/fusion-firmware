// src/cluster/mod.rs
//
// Cluster mode (F9): multi-camera coordination with NO cloud dependency —
// cameras on the same site discover each other and exchange events entirely
// over the local network or Bluetooth LE, so coordination keeps working
// through a WAN/internet outage (the whole point of "offline mode").
//
// Portability note: everything in `cluster/` is deliberately written against
// plain types (no GStreamer, no firmware config structs beyond the small
// `ClusterConfig`) behind the `ClusterTransport` trait, so this module can be
// lifted into `iiotedge-lib` verbatim later. It lives here for now because
// the policy is "consume the lib, don't modify it" — this is new SDK-shaped
// surface, not a firmware-specific concern, but it needs to prove itself in
// one deployment before it becomes a library contract.
//
// Two transports, usable together (WiFi primary, BLE fallback where there's
// no WiFi at all) or independently:
//   * wifi.rs      — broker-less UDP multicast on the LAN. No MQTT broker,
//                    no internet: any local WiFi/Ethernet segment works,
//                    including an isolated site network or a camera-hosted AP.
//   * bluetooth.rs — BLE advertisement + GATT (BlueZ/Linux). For sites with
//                    no WiFi network reachable at all.
// BlueZ/D-Bus (via `bluer`) is Linux-only, same pattern as the RKNN NPU
// backend (ai/backends/mod.rs): the module — and bluer itself — only exist
// on Linux builds; spawn() below stubs the "unavailable" path elsewhere.
#[cfg(target_os = "linux")]
pub mod bluetooth;
pub mod fusion;
pub mod wifi;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Everything a transport needs to move cluster messages. Deliberately
/// synchronous + minimal (send/recv of opaque bytes) so any medium — UDP,
/// BLE, a future LoRa transport — can implement it without the caller
/// caring about the wire details.
pub trait ClusterTransport: Send {
    fn name(&self) -> &'static str;
    /// Broadcast a message to all reachable peers. Non-blocking best-effort;
    /// errors are logged by the caller, never fatal to the cluster loop.
    fn send(&mut self, message: &[u8]) -> std::io::Result<()>;
    /// Drain any messages received since the last call. Non-blocking (an
    /// empty Vec means "nothing right now", not "transport dead").
    fn poll_recv(&mut self) -> Vec<Vec<u8>>;
}

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

/// One message on the cluster bus. Kept small and JSON-encoded (debuggable,
/// consistent with the rest of the firmware's GDE-JSON habit); BLE framing
/// in bluetooth.rs chunks this further to fit link MTU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterMessage {
    pub node_id: String,
    pub timestamp_ns: u64,
    #[serde(flatten)]
    pub kind: MessageKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageKind {
    /// Periodic presence broadcast: identity + capability + health summary.
    Announce {
        facility_id: String,
        capabilities: Vec<String>,
        priority: u32,
        healthy: bool,
    },
    /// A local camera event worth telling the cluster about (tamper, motion,
    /// AI detection, correlation). Compact by design — this is a
    /// notification, not the evidence itself; peers that react pull
    /// snapshots/clips through their own normal evidence path.
    Event {
        kind: String,
        summary: String,
        source_device: String,
    },
    /// Active liveness probe, broadcast periodically alongside (but out of
    /// phase with) Announce — every peer replies immediately with Pong.
    /// Distinct from Announce so liveness survives a single lost datagram
    /// on either mechanism, and gives faster detection than waiting out a
    /// full PEER_TIMEOUT window.
    Ping { id: String },
    /// Reply to a Ping. Carries the same `id` purely for future RTT/
    /// diagnostics use; today receipt alone is what matters (refreshes the
    /// sender's last-seen).
    Pong { id: String },
    /// A command targeted at one peer ("*" = every peer). `payload` is the
    /// exact `{"cmd","id","token",...}` object the MQTT command channel
    /// already accepts (see `commands::handle_command`) — cluster-relayed
    /// commands run through that SAME audited execution path, not a
    /// parallel one. Gated on the receiving end by
    /// `[cluster].accept_remote_commands` (default off).
    Command {
        target: String,
        payload: serde_json::Value,
    },
    /// The exact ack `handle_command` produced, relayed back onto the bus
    /// so whoever issued the command (or anyone else) can see the result.
    CommandAck { ack: serde_json::Value },
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

// ---------------------------------------------------------------------------
// Node registry + leader election
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PeerInfo {
    priority: u32,
    healthy: bool,
    last_seen: Instant,
}

/// Peer liveness beyond this age is dropped from the registry and from
/// leader consideration.
const PEER_TIMEOUT: Duration = Duration::from_secs(30);

/// Live view of the cluster: known peers and the current leader. Cheap to
/// clone (Arc-backed); read from anywhere without touching the bus threads.
#[derive(Clone)]
pub struct ClusterView {
    inner: Arc<parking_lot::Mutex<Registry>>,
    local_node_id: String,
    local_priority: u32,
}

struct Registry {
    peers: HashMap<String, PeerInfo>,
}

impl ClusterView {
    /// True when this node currently holds the coordinator role.
    ///
    /// Election rule (simplified, deterministic bully): among all peers
    /// heard from within `PEER_TIMEOUT` (plus self), the highest `priority`
    /// wins; ties break on node_id so exactly one node is ever chosen. No
    /// election *messages* are needed — every node computes the same answer
    /// from the same Announce history, and a leader that goes silent drops
    /// out of consideration within one timeout window, so the next-highest
    /// peer takes over automatically.
    pub fn is_leader(&self) -> bool {
        self.leader_id() == self.local_node_id
    }

    pub fn leader_id(&self) -> String {
        let registry = self.inner.lock();
        let mut best = (self.local_priority, self.local_node_id.clone());
        for (id, peer) in &registry.peers {
            if !peer.healthy || peer.last_seen.elapsed() > PEER_TIMEOUT {
                continue;
            }
            let candidate = (peer.priority, id.clone());
            if candidate > best {
                best = candidate;
            }
        }
        best.1
    }

    pub fn peer_count(&self) -> usize {
        let registry = self.inner.lock();
        registry
            .peers
            .values()
            .filter(|p| p.last_seen.elapsed() <= PEER_TIMEOUT)
            .count()
    }

    /// Every currently-live peer, for topology views (the `/cluster/status`
    /// HTTP endpoint, multi-camera detection fusion) that need more than a
    /// count — `peer_count`/`leader_id` alone can't enumerate who's actually
    /// out there.
    pub fn peers(&self) -> Vec<PeerSummary> {
        let registry = self.inner.lock();
        registry
            .peers
            .iter()
            .filter(|(_, p)| p.last_seen.elapsed() <= PEER_TIMEOUT)
            .map(|(node_id, p)| PeerSummary {
                node_id: node_id.clone(),
                priority: p.priority,
                healthy: p.healthy,
                last_seen_ms_ago: p.last_seen.elapsed().as_millis() as u64,
            })
            .collect()
    }

    fn observe_announce(&self, node_id: &str, priority: u32, healthy: bool) {
        if node_id == self.local_node_id {
            return;
        }
        let mut registry = self.inner.lock();
        registry.peers.insert(
            node_id.to_string(),
            PeerInfo {
                priority,
                healthy,
                last_seen: Instant::now(),
            },
        );
    }

    /// Refreshes an already-known peer's liveness without needing their
    /// priority/healthy state (Ping/Pong carry neither) — never creates a
    /// phantom entry for a peer we haven't seen a real Announce from yet.
    fn touch_peer(&self, node_id: &str) {
        if node_id == self.local_node_id {
            return;
        }
        let mut registry = self.inner.lock();
        if let Some(peer) = registry.peers.get_mut(node_id) {
            peer.last_seen = Instant::now();
        }
    }
}

/// A snapshot of one live peer, for topology views.
#[derive(Debug, Clone)]
pub struct PeerSummary {
    pub node_id: String,
    pub priority: u32,
    pub healthy: bool,
    pub last_seen_ms_ago: u64,
}

// ---------------------------------------------------------------------------
// Bus: owns the transport(s), runs the announce/receive loop
// ---------------------------------------------------------------------------

/// A peer event delivered to the firmware for local reaction (e.g. "peer
/// tamper alarm → take a confirmatory snapshot here too").
pub struct PeerEvent {
    pub source_device: String,
    pub kind: String,
    pub summary: String,
}

/// A command a peer has sent us, addressed to this node (or "*"), already
/// filtered by `run_bus` — a consumer only ever sees commands actually
/// meant for this device, never every command on the wire.
pub struct PeerCommand {
    pub source_device: String,
    /// The exact `{"cmd","id","token",...}` object — hand this straight to
    /// `commands::handle_command` unchanged.
    pub payload: serde_json::Value,
}

/// The ack a peer sent back after executing a command (ours or anyone
/// else's — the bus doesn't track who originated what, callers match on
/// `ack["id"]` if they need to correlate).
pub struct PeerCommandAck {
    pub source_device: String,
    pub ack: serde_json::Value,
}

#[derive(Clone)]
pub struct ClusterHandle {
    view: ClusterView,
    peer_events_rx: crossbeam::channel::Receiver<PeerEvent>,
    peer_commands_rx: crossbeam::channel::Receiver<PeerCommand>,
    command_acks_rx: crossbeam::channel::Receiver<PeerCommandAck>,
    outbound_tx: crossbeam::channel::Sender<MessageKind>,
}

impl ClusterHandle {
    pub fn view(&self) -> ClusterView {
        self.view.clone()
    }

    /// Publish a local event onto the cluster bus (best-effort, non-blocking).
    pub fn publish_event(&self, kind: &str, summary: String, source_device: &str) {
        let msg = MessageKind::Event {
            kind: kind.to_string(),
            summary,
            source_device: source_device.to_string(),
        };
        if self.outbound_tx.try_send(msg).is_err() {
            debug!("cluster outbound queue full; dropping event '{kind}'");
        }
    }

    /// Non-blocking drain of events peers have published.
    pub fn drain_peer_events(&self) -> Vec<PeerEvent> {
        self.peer_events_rx.try_iter().collect()
    }

    /// Send a command to one peer (`target` = its node_id) or every peer
    /// (`target = "*"`) — `payload` should be the same
    /// `{"cmd","id","token",...}` shape a platform MQTT command uses, so
    /// the receiving device runs it through the identical execution path.
    /// Fire-and-forget: best-effort delivery only, no built-in retry (UDP
    /// multicast/BLE advertising have no delivery guarantee) — watch
    /// `drain_command_acks` for confirmation if the caller needs it.
    pub fn send_command(&self, target: &str, payload: serde_json::Value) {
        let msg = MessageKind::Command {
            target: target.to_string(),
            payload,
        };
        if self.outbound_tx.try_send(msg).is_err() {
            debug!("cluster outbound queue full; dropping command to '{target}'");
        }
    }

    /// Broadcasts the result of a command this device just executed (see
    /// `PeerCommand`) so the issuer — or anyone else — can see it.
    pub fn send_command_ack(&self, ack: serde_json::Value) {
        let msg = MessageKind::CommandAck { ack };
        if self.outbound_tx.try_send(msg).is_err() {
            debug!("cluster outbound queue full; dropping command ack");
        }
    }

    /// Non-blocking drain of commands peers have addressed to this device.
    pub fn drain_peer_commands(&self) -> Vec<PeerCommand> {
        self.peer_commands_rx.try_iter().collect()
    }

    /// Non-blocking drain of command acks peers have broadcast.
    pub fn drain_command_acks(&self) -> Vec<PeerCommandAck> {
        self.command_acks_rx.try_iter().collect()
    }
}

/// Start the cluster bus over every enabled transport. Returns None when
/// cluster mode is disabled or no transport could start — the camera runs
/// standalone exactly as before.
pub fn spawn(
    cfg: &crate::config::ClusterConfig,
    node_id: &str,
    facility_id: &str,
) -> Option<ClusterHandle> {
    if !cfg.enabled {
        return None;
    }

    let mut transports: Vec<Box<dyn ClusterTransport>> = Vec::new();
    if cfg.wifi.enabled {
        match wifi::WifiTransport::new(&cfg.wifi) {
            Ok(t) => transports.push(Box::new(t)),
            Err(e) => warn!("cluster WiFi transport unavailable: {e}"),
        }
    }
    if cfg.bluetooth.enabled {
        #[cfg(target_os = "linux")]
        {
            match bluetooth::BluetoothTransport::new(&cfg.bluetooth) {
                Ok(t) => transports.push(Box::new(t)),
                Err(e) => warn!("cluster Bluetooth transport unavailable: {e}"),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            warn!(
                "cluster.bluetooth is enabled but Bluetooth LE (BlueZ/bluer) is Linux-only; \
                 ignored on this host"
            );
        }
    }
    if transports.is_empty() {
        warn!("cluster mode enabled but no transport could start; running standalone");
        return None;
    }
    let transport_names: Vec<&str> = transports.iter().map(|t| t.name()).collect();
    info!(transports = ?transport_names, "Cluster mode active");

    let view = ClusterView {
        inner: Arc::new(parking_lot::Mutex::new(Registry {
            peers: HashMap::new(),
        })),
        local_node_id: node_id.to_string(),
        local_priority: cfg.priority,
    };

    let (peer_events_tx, peer_events_rx) = crossbeam::channel::bounded(64);
    let (peer_commands_tx, peer_commands_rx) = crossbeam::channel::bounded(64);
    let (command_acks_tx, command_acks_rx) = crossbeam::channel::bounded(64);
    let (outbound_tx, outbound_rx) = crossbeam::channel::bounded(64);

    let announce_interval = Duration::from_secs(cfg.announce_interval_s.max(1));
    let worker_view = view.clone();
    let worker_node_id = node_id.to_string();
    let worker_facility = facility_id.to_string();
    let priority = cfg.priority;
    let healthy_flag = Arc::new(AtomicBool::new(true));
    let accept_remote_commands = cfg.accept_remote_commands;

    let spawned = std::thread::Builder::new()
        .name("cluster_bus".to_string())
        .spawn(move || {
            run_bus(BusParams {
                transports,
                view: worker_view,
                node_id: worker_node_id,
                facility_id: worker_facility,
                priority,
                healthy: healthy_flag,
                announce_interval,
                accept_remote_commands,
                outbound_rx,
                peer_events_tx,
                peer_commands_tx,
                command_acks_tx,
            })
        });
    if let Err(e) = spawned {
        warn!("Failed to spawn cluster bus thread: {e}");
        return None;
    }

    Some(ClusterHandle {
        view,
        peer_events_rx,
        peer_commands_rx,
        command_acks_rx,
        outbound_tx,
    })
}

/// Everything `run_bus` needs — bundled rather than a long positional-arg
/// list now that Phase A (commands/ping-pong) added several more inputs.
struct BusParams {
    transports: Vec<Box<dyn ClusterTransport>>,
    view: ClusterView,
    node_id: String,
    facility_id: String,
    priority: u32,
    healthy: Arc<AtomicBool>,
    announce_interval: Duration,
    /// Gates whether inbound `Command`s addressed to us are surfaced to
    /// `drain_peer_commands` at all — off by default
    /// (`[cluster].accept_remote_commands`). Joining the mesh (seeing
    /// peers, sharing events) is a different risk level from letting peers
    /// remote-control this device, so this is a separate opt-in from
    /// `[cluster].enabled`.
    accept_remote_commands: bool,
    outbound_rx: crossbeam::channel::Receiver<MessageKind>,
    peer_events_tx: crossbeam::channel::Sender<PeerEvent>,
    peer_commands_tx: crossbeam::channel::Sender<PeerCommand>,
    command_acks_tx: crossbeam::channel::Sender<PeerCommandAck>,
}

/// Minimal xorshift64 PRNG for broadcast-timing jitter — not a
/// cryptographic requirement, just needs to spread multiple devices'
/// periodic broadcasts out of lockstep (see `Jitter::jittered`). Seeded
/// from wall-clock plus a hash of the node id so two nodes started in the
/// same instant still diverge.
struct Jitter(u64);

impl Jitter {
    fn new(seed_str: &str) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self(Self::mix_seed(seed, seed_str))
    }

    /// Folds `id` into `seed` (FNV-ish byte mixing). Split out from `new`
    /// as a pure function purely so it's independently testable — the
    /// wall-clock half of the seed can't be pinned down in a test.
    fn mix_seed(mut seed: u64, id: &str) -> u64 {
        for b in id.bytes() {
            seed ^= u64::from(b).wrapping_mul(0x100_0000_01b3);
        }
        seed.max(1)
    }

    /// Next pseudo-random value in [0, 1.0).
    fn next_unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    /// `base` randomized by up to `±frac` (e.g. `frac = 0.2` = ±20%) — the
    /// application-level half of collision avoidance: the WiFi/BLE radios
    /// already handle collisions at the MAC layer (802.11 CSMA/CA, BLE's
    /// own advertising backoff), but without this, every device on a site
    /// configured with the same `announce_interval_s` would drift toward
    /// broadcasting in lockstep over time (a classic periodic-protocol
    /// "thundering herd").
    fn jittered(&mut self, base: Duration, frac: f64) -> Duration {
        let delta = base.as_secs_f64() * frac * (self.next_unit() * 2.0 - 1.0);
        Duration::from_secs_f64((base.as_secs_f64() + delta).max(0.1))
    }
}

fn run_bus(params: BusParams) {
    let BusParams {
        mut transports,
        view,
        node_id,
        facility_id,
        priority,
        healthy,
        announce_interval,
        accept_remote_commands,
        outbound_rx,
        peer_events_tx,
        peer_commands_tx,
        command_acks_tx,
    } = params;

    let mut jitter = Jitter::new(&node_id);
    let mut last_announce = Instant::now();
    let mut announce_due_in = Duration::from_millis(1); // fire almost immediately on boot
    // Ping runs on the same cadence but out of phase with Announce (started
    // half an interval later) so the two periodic broadcasts don't land on
    // the same tick.
    let mut last_ping = Instant::now() + announce_interval / 2;
    let mut ping_due_in = jitter.jittered(announce_interval, 0.2);

    loop {
        // Outbound: announce + ping on their own jittered schedules, plus
        // any queued local events/commands/acks.
        if last_announce.elapsed() >= announce_due_in {
            last_announce = Instant::now();
            announce_due_in = jitter.jittered(announce_interval, 0.2);
            broadcast(
                &mut transports,
                &ClusterMessage {
                    node_id: node_id.clone(),
                    timestamp_ns: now_ns(),
                    kind: MessageKind::Announce {
                        facility_id: facility_id.clone(),
                        capabilities: vec!["camera".to_string()],
                        priority,
                        healthy: healthy.load(Ordering::Relaxed),
                    },
                },
            );
        }
        if last_ping.elapsed() >= ping_due_in {
            last_ping = Instant::now();
            ping_due_in = jitter.jittered(announce_interval, 0.2);
            broadcast(
                &mut transports,
                &ClusterMessage {
                    node_id: node_id.clone(),
                    timestamp_ns: now_ns(),
                    kind: MessageKind::Ping {
                        id: now_ns().to_string(),
                    },
                },
            );
        }
        for kind in outbound_rx.try_iter() {
            broadcast(
                &mut transports,
                &ClusterMessage {
                    node_id: node_id.clone(),
                    timestamp_ns: now_ns(),
                    kind,
                },
            );
        }

        // Inbound: poll every transport, update the registry, forward
        // events/commands/acks to whoever's draining them. Ping replies are
        // queued (`pending_pongs`) rather than broadcast inline — `transport`
        // below already holds transports mutably borrowed for the scan, so
        // a second concurrent `&mut transports` for an immediate reply
        // wouldn't borrow-check; sent right after the scan completes instead.
        let mut pending_pongs: Vec<String> = Vec::new();
        for transport in transports.iter_mut() {
            for raw in transport.poll_recv() {
                let Ok(msg) = serde_json::from_slice::<ClusterMessage>(&raw) else {
                    continue;
                };
                if msg.node_id == node_id {
                    continue; // our own broadcast looped back (multicast, BLE mesh)
                }
                match msg.kind {
                    MessageKind::Announce {
                        priority, healthy, ..
                    } => view.observe_announce(&msg.node_id, priority, healthy),
                    MessageKind::Event {
                        kind,
                        summary,
                        source_device,
                    } => {
                        debug!(peer = %msg.node_id, kind = %kind, "cluster event received");
                        let _ = peer_events_tx.try_send(PeerEvent {
                            source_device,
                            kind,
                            summary,
                        });
                    }
                    MessageKind::Ping { id } => {
                        view.touch_peer(&msg.node_id);
                        pending_pongs.push(id);
                    }
                    MessageKind::Pong { .. } => {
                        view.touch_peer(&msg.node_id);
                    }
                    MessageKind::Command { target, payload } => {
                        if !accept_remote_commands {
                            continue;
                        }
                        if target == node_id || target == "*" {
                            debug!(peer = %msg.node_id, "cluster command received");
                            let _ = peer_commands_tx.try_send(PeerCommand {
                                source_device: msg.node_id.clone(),
                                payload,
                            });
                        }
                    }
                    MessageKind::CommandAck { ack } => {
                        let _ = command_acks_tx.try_send(PeerCommandAck {
                            source_device: msg.node_id.clone(),
                            ack,
                        });
                    }
                }
            }
        }
        for id in pending_pongs {
            broadcast(
                &mut transports,
                &ClusterMessage {
                    node_id: node_id.clone(),
                    timestamp_ns: now_ns(),
                    kind: MessageKind::Pong { id },
                },
            );
        }

        std::thread::sleep(Duration::from_millis(200));
    }
}

fn broadcast(transports: &mut [Box<dyn ClusterTransport>], message: &ClusterMessage) {
    let Ok(bytes) = serde_json::to_vec(message) else {
        return;
    };
    for transport in transports {
        if let Err(e) = transport.send(&bytes) {
            debug!(transport = transport.name(), "cluster send failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(local_priority: u32) -> ClusterView {
        ClusterView {
            inner: Arc::new(parking_lot::Mutex::new(Registry {
                peers: HashMap::new(),
            })),
            local_node_id: "self".to_string(),
            local_priority,
        }
    }

    #[test]
    fn self_leads_with_no_peers() {
        let v = view(10);
        assert!(v.is_leader());
        assert_eq!(v.leader_id(), "self");
    }

    #[test]
    fn higher_priority_peer_becomes_leader() {
        let v = view(10);
        v.observe_announce("peer-a", 20, true);
        assert!(!v.is_leader());
        assert_eq!(v.leader_id(), "peer-a");
    }

    #[test]
    fn unhealthy_peer_never_leads() {
        let v = view(10);
        v.observe_announce("peer-a", 999, false);
        assert!(v.is_leader());
    }

    #[test]
    fn tie_breaks_on_node_id() {
        let v = view(10); // local_node_id = "self"
        v.observe_announce("zzz-higher-id", 10, true);
        // Equal priority: lexicographically greater id wins.
        assert_eq!(v.leader_id(), "zzz-higher-id");
        v.observe_announce("aaa-lower-id", 10, true);
        assert_eq!(v.leader_id(), "zzz-higher-id");
    }

    #[test]
    fn peer_count_ignores_self() {
        let v = view(10);
        assert_eq!(v.peer_count(), 0);
        v.observe_announce("peer-a", 5, true);
        v.observe_announce("peer-b", 5, true);
        assert_eq!(v.peer_count(), 2);
    }

    #[test]
    fn peers_reports_priority_and_health_per_node() {
        let v = view(10);
        v.observe_announce("peer-a", 42, true);
        v.observe_announce("peer-b", 7, false);
        let mut peers = v.peers();
        peers.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].node_id, "peer-a");
        assert_eq!(peers[0].priority, 42);
        assert!(peers[0].healthy);
        assert_eq!(peers[1].node_id, "peer-b");
        assert_eq!(peers[1].priority, 7);
        assert!(!peers[1].healthy);
    }

    #[test]
    fn peers_never_includes_self() {
        let v = view(10);
        v.observe_announce("peer-a", 5, true);
        assert!(v.peers().iter().all(|p| p.node_id != "self"));
    }

    #[test]
    fn touch_peer_refreshes_known_peer_without_changing_its_state() {
        let v = view(10);
        v.observe_announce("peer-a", 42, true);
        v.touch_peer("peer-a");
        let peers = v.peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].priority, 42);
        assert!(peers[0].healthy);
    }

    #[test]
    fn touch_peer_never_creates_a_phantom_entry() {
        // Ping/Pong carry no priority/healthy — a Pong from a peer we've
        // never seen an Announce from must not fabricate a registry entry.
        let v = view(10);
        v.touch_peer("never-announced");
        assert_eq!(v.peer_count(), 0);
    }

    #[test]
    fn touch_peer_on_self_is_a_no_op() {
        let v = view(10);
        v.touch_peer("self");
        assert_eq!(v.peer_count(), 0);
        assert!(v.peers().is_empty());
    }

    #[test]
    fn jitter_stays_within_requested_fraction_of_base() {
        let mut j = Jitter::new("node-under-test");
        let base = Duration::from_secs(10);
        let frac = 0.2;
        for _ in 0..500 {
            let d = j.jittered(base, frac).as_secs_f64();
            assert!(
                (8.0..=12.0).contains(&d),
                "{d} outside ±{}% of {base:?}",
                frac * 100.0
            );
        }
    }

    #[test]
    fn jitter_never_goes_below_the_floor_for_a_tiny_base() {
        let mut j = Jitter::new("node-under-test");
        let base = Duration::from_millis(50); // below the 0.1s floor
        for _ in 0..500 {
            assert!(j.jittered(base, 0.2).as_secs_f64() >= 0.1);
        }
    }

    #[test]
    fn jitter_seed_mixing_is_deterministic_given_the_same_inputs() {
        assert_eq!(
            Jitter::mix_seed(12345, "same-node"),
            Jitter::mix_seed(12345, "same-node")
        );
    }

    #[test]
    fn jitter_seed_mixing_incorporates_the_node_id() {
        assert_ne!(
            Jitter::mix_seed(12345, "node-a"),
            Jitter::mix_seed(12345, "node-b")
        );
    }

    #[test]
    fn message_kind_ping_pong_round_trip_through_json() {
        let msg = ClusterMessage {
            node_id: "cam-1".to_string(),
            timestamp_ns: 123,
            kind: MessageKind::Ping {
                id: "ping-1".to_string(),
            },
        };
        let encoded = serde_json::to_value(&msg).unwrap();
        assert_eq!(encoded["type"], "ping");
        assert_eq!(encoded["id"], "ping-1");
        assert_eq!(encoded["node_id"], "cam-1");
        let decoded: ClusterMessage = serde_json::from_value(encoded).unwrap();
        assert!(matches!(decoded.kind, MessageKind::Ping { id } if id == "ping-1"));

        let pong = ClusterMessage {
            node_id: "cam-2".to_string(),
            timestamp_ns: 456,
            kind: MessageKind::Pong {
                id: "ping-1".to_string(),
            },
        };
        let encoded = serde_json::to_value(&pong).unwrap();
        assert_eq!(encoded["type"], "pong");
        let decoded: ClusterMessage = serde_json::from_value(encoded).unwrap();
        assert!(matches!(decoded.kind, MessageKind::Pong { id } if id == "ping-1"));
    }

    #[test]
    fn message_kind_command_carries_target_and_arbitrary_payload() {
        let msg = ClusterMessage {
            node_id: "cam-1".to_string(),
            timestamp_ns: 0,
            kind: MessageKind::Command {
                target: "cam-2".to_string(),
                payload: serde_json::json!({"cmd": "snapshot", "id": "abc", "token": "t"}),
            },
        };
        let encoded = serde_json::to_value(&msg).unwrap();
        assert_eq!(encoded["type"], "command");
        assert_eq!(encoded["target"], "cam-2");
        assert_eq!(encoded["payload"]["cmd"], "snapshot");
        let decoded: ClusterMessage = serde_json::from_value(encoded).unwrap();
        match decoded.kind {
            MessageKind::Command { target, payload } => {
                assert_eq!(target, "cam-2");
                assert_eq!(payload["cmd"], "snapshot");
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn message_kind_command_ack_round_trips_the_handler_output_verbatim() {
        let ack_body = serde_json::json!({"ok": true, "id": "abc", "cmd": "snapshot"});
        let msg = ClusterMessage {
            node_id: "cam-2".to_string(),
            timestamp_ns: 0,
            kind: MessageKind::CommandAck {
                ack: ack_body.clone(),
            },
        };
        let encoded = serde_json::to_value(&msg).unwrap();
        assert_eq!(encoded["type"], "command_ack");
        let decoded: ClusterMessage = serde_json::from_value(encoded).unwrap();
        assert!(matches!(decoded.kind, MessageKind::CommandAck { ack } if ack == ack_body));
    }
}
