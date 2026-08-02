// src/cluster/wifi.rs
//
// WiFi/LAN cluster transport: broker-less UDP multicast. No MQTT broker, no
// internet, no DNS — any local WiFi or Ethernet segment works, including a
// site network with no WAN uplink or a camera-hosted access point. This is
// what makes cluster mode "offline capable" over WiFi: it never dials out.
//
// Design mirrors ONVIF's WS-Discovery responder (same multicast pattern,
// same platform primitives) — one non-blocking UDP socket, join a multicast
// group, send/recv datagrams. A single UDP datagram per message keeps this
// simple; ClusterMessage JSON comfortably fits one packet (well under the
// LAN MTU), so no fragmentation/reassembly is needed.
use crate::cluster::ClusterTransport;
use crate::config::ClusterWifiConfig;

use socket2::{Domain, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use tracing::info;

// Only the #[cfg(test)] blocking-recv helper below needs Duration; keep the
// import test-only so release builds don't warn.
#[cfg(test)]
use std::time::Duration;

pub struct WifiTransport {
    socket: UdpSocket,
    group: SocketAddrV4,
}

impl WifiTransport {
    pub fn new(cfg: &ClusterWifiConfig) -> std::io::Result<Self> {
        let group: Ipv4Addr = cfg.multicast_group.parse().map_err(|_| {
            std::io::Error::other(format!("invalid multicast_group '{}'", cfg.multicast_group))
        })?;
        if !group.is_multicast() {
            return Err(std::io::Error::other(format!(
                "'{}' is not a multicast address (224.0.0.0/4)",
                cfg.multicast_group
            )));
        }

        // SO_REUSEADDR (+ SO_REUSEPORT where available) lets every camera
        // process on the box — or every test instance on this machine —
        // bind the same multicast port simultaneously; std::net::UdpSocket
        // doesn't set this on any platform, so we build the socket via
        // socket2 first and hand it to std afterward.
        let raw = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
        raw.set_reuse_address(true)?;
        #[cfg(unix)]
        raw.set_reuse_port(true)?;
        raw.set_nonblocking(true)?;
        let bind_addr: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, cfg.port).into();
        raw.bind(&bind_addr.into())?;
        let socket: UdpSocket = raw.into();
        socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)?;
        socket.set_multicast_loop_v4(true)?; // needed for same-host testing/multi-camera-on-one-box
        socket.set_multicast_ttl_v4(cfg.ttl)?;

        info!(
            group = %group,
            port = cfg.port,
            "Cluster WiFi transport bound (broker-less UDP multicast)"
        );

        Ok(Self {
            socket,
            group: SocketAddrV4::new(group, cfg.port),
        })
    }
}

impl ClusterTransport for WifiTransport {
    fn name(&self) -> &'static str {
        "wifi"
    }

    fn send(&mut self, message: &[u8]) -> std::io::Result<()> {
        self.socket.send_to(message, self.group)?;
        Ok(())
    }

    fn poll_recv(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((len, _peer)) => out.push(buf[..len].to_vec()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break, // transient recv error; try again next poll
            }
        }
        out
    }
}

/// Blocking variant used only by tests, so a two-instance exchange test
/// doesn't need to busy-poll.
#[cfg(test)]
impl WifiTransport {
    fn recv_blocking(&mut self, timeout: Duration) -> Option<Vec<u8>> {
        self.socket.set_nonblocking(false).ok()?;
        self.socket.set_read_timeout(Some(timeout)).ok()?;
        let mut buf = [0u8; 4096];
        let result = self
            .socket
            .recv_from(&mut buf)
            .ok()
            .map(|(len, _)| buf[..len].to_vec());
        let _ = self.socket.set_nonblocking(true);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(port: u16) -> ClusterWifiConfig {
        ClusterWifiConfig {
            enabled: true,
            multicast_group: "239.255.77.77".to_string(),
            port,
            ttl: 1,
        }
    }

    #[test]
    fn rejects_non_multicast_address() {
        let mut bad = cfg(0);
        bad.multicast_group = "10.0.0.1".to_string();
        assert!(WifiTransport::new(&bad).is_err());
    }

    #[test]
    fn two_instances_exchange_datagrams_on_loopback() {
        // Distinct ports would defeat the point (multicast group membership
        // is what we're testing), so both instances share one port and rely
        // on SO_REUSEADDR/SO_REUSEPORT + multicast loopback — exactly the
        // production "two cameras on one gateway box" scenario (Phase 2),
        // just simulated on one host. Confirmed both sides receive when the
        // join has propagated (macOS/BSD multicast delivers to every
        // SO_REUSEPORT member that joined the group, same as Linux) — but
        // the very first IGMP join on a fresh group can take a moment to
        // wire up locally, so retry a few times rather than assume the
        // first send lands instantly. Budget kept generous (10 x 500ms) —
        // this observably needs more margin than 5 x 300ms under a full
        // parallel `cargo test` run (CPU-contended IGMP join propagation),
        // even though it's reliable in isolation.
        let port = 45890;
        let mut a = WifiTransport::new(&cfg(port)).expect("bind a");
        let mut b = WifiTransport::new(&cfg(port)).expect("bind b");

        let mut received = None;
        for _ in 0..10 {
            a.send(b"hello-from-a").unwrap();
            if let Some(msg) = b.recv_blocking(Duration::from_millis(500)) {
                received = Some(msg);
                break;
            }
        }
        assert_eq!(
            received.expect("b should receive a's datagram within 10 retries"),
            b"hello-from-a"
        );
    }
}
