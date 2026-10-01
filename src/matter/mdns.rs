// src/matter/mdns.rs
//
// mDNS responder for Matter commissioning discovery — the built-in,
// pure-Rust implementation (`rs_matter::transport::network::mdns::builtin`),
// not the platform-specific `astro-dnssd` (macOS)/`zeroconf`/`avahi` (Linux
// D-Bus) backends those crates' own feature flags would pull in. Verified
// directly against rs-matter 0.3.0's own example
// (examples/src/common/mdns.rs) and real installed source
// (src/transport/network/mdns/builtin.rs) before writing this — the
// builtin responder uses `if-addrs` for cross-platform interface
// enumeration and a raw multicast UDP socket via `socket2` (already a
// dependency of this firmware), so it needs zero extra system libraries
// and works identically on the macOS dev host and the Radxa Linux target,
// same "no platform split unless a real driver needs it" discipline as
// the rest of this firmware's non-hardware subsystems.
use rs_matter::crypto::Crypto;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::transport::network::mdns::builtin::{BuiltinMdns, Host};
use rs_matter::transport::network::mdns::{
    MDNS_IPV4_BROADCAST_ADDR, MDNS_IPV6_BROADCAST_ADDR, MDNS_SOCKET_DEFAULT_BIND_ADDR,
};
use rs_matter::transport::network::{Ipv4Addr, Ipv6Addr};
use rs_matter::Matter;

use socket2::{Domain, Protocol, Socket, Type};
use std::net::UdpSocket;
use tracing::{debug, error, warn};

/// Picks the interface to advertise on, plus its IPv4 and (if any) one
/// IPv6 address. `Host` (rs-matter's builtin-mDNS advertisement struct)
/// carries a single `Ipv6Addr`, not a list — verified against the real
/// installed struct definition rather than assumed from the reference's
/// own richer internal representation.
fn pick_interface() -> Result<(Ipv4Addr, Ipv6Addr, u32), Error> {
    let all = if_addrs::get_if_addrs().map_err(|_| ErrorCode::StdIoError)?;
    debug!("Available network interfaces: {:?}", all);

    let find_ipv6_candidate = |ipv6_filter: fn(std::net::Ipv6Addr) -> bool| {
        all.iter()
            .filter(|ia| !ia.is_loopback())
            .filter_map(|ia| match ia.addr {
                if_addrs::IfAddr::V6(ref v6) if ipv6_filter(v6.ip) => {
                    Some((ia.name.clone(), v6.ip, ia.index.unwrap_or(0)))
                }
                _ => None,
            })
            .find_map(|(iname, ipv6, index)| {
                all.iter()
                    .filter(|ia2| ia2.name == iname)
                    .find_map(|ia2| match ia2.addr {
                        if_addrs::IfAddr::V4(ref v4) => Some((iname.clone(), v4.ip, ipv6, index)),
                        _ => None,
                    })
            })
    };

    // Last resort: an "eth*"/"eno*"-named interface even without IPv6 —
    // common on the Radxa's onboard Ethernet/WiFi if router advertisements
    // haven't assigned a link-local address yet.
    let find_fallback_candidate = || {
        all.iter()
            .filter(|ia| !ia.is_loopback())
            .filter(|ia| {
                ia.name.starts_with("eth")
                    || ia.name.starts_with("eno")
                    || ia.name.starts_with("wlan")
                    || ia.name.starts_with("en") // macOS dev host (en0)
            })
            .map(|ia| match ia.addr {
                if_addrs::IfAddr::V4(ref v4) => (
                    ia.name.clone(),
                    v4.ip,
                    std::net::Ipv6Addr::UNSPECIFIED,
                    ia.index.unwrap_or(0),
                ),
                if_addrs::IfAddr::V6(ref v6) => (
                    ia.name.clone(),
                    std::net::Ipv4Addr::UNSPECIFIED,
                    v6.ip,
                    ia.index.unwrap_or(0),
                ),
            })
            .next()
    };

    let (iname, ip, ipv6, index) = find_ipv6_candidate(|ip| ip.is_unicast_link_local())
        .or_else(|| find_ipv6_candidate(|_| true))
        .or_else(|| {
            warn!("Matter mDNS: no interface with a suitable IPv6 address found");
            find_fallback_candidate()
        })
        .ok_or_else(|| {
            error!("Matter mDNS: cannot find a network interface to advertise on");
            ErrorCode::StdIoError
        })?;

    let ipv6_addr: Ipv6Addr = ipv6.octets().into();

    debug!(
        interface = %iname,
        ipv4 = %ip,
        ipv6 = %ipv6,
        "Matter mDNS: selected interface"
    );
    Ok((ip.octets().into(), ipv6_addr, index))
}

/// Runs the mDNS responder until the process shuts down. `hostname` should
/// be stable across reboots — it's what shows up in a Matter controller's
/// discovery log — so callers pass the firmware's own `device_id`, not a
/// random/regenerated value.
pub async fn run<C: Crypto>(matter: &Matter<'_>, crypto: C, hostname: &str) -> Result<(), Error> {
    let (ipv4_addr, ipv6_addr, interface) = pick_interface()?;

    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|_| ErrorCode::StdIoError)?;
    socket
        .set_reuse_address(true)
        .map_err(|_| ErrorCode::StdIoError)?;
    // mDNS's well-known port (5353) is routinely already held by the OS's
    // own responder or another app (mDNSResponder/Bonjour, Chrome's own
    // mDNS, Avahi, …) — real, confirmed on this exact dev host (a Google/
    // Chrome process already had it bound during testing). SO_REUSEADDR
    // alone isn't enough for two listeners to share one UDP port on macOS/
    // BSD; SO_REUSEPORT is the flag actual coexisting mDNS responders set.
    // Harmless no-op on platforms without it (Windows).
    #[cfg(unix)]
    socket.set_reuse_port(true).map_err(|_| ErrorCode::StdIoError)?;
    socket.set_only_v6(false).map_err(|_| ErrorCode::StdIoError)?;
    socket
        .bind(&MDNS_SOCKET_DEFAULT_BIND_ADDR.into())
        .map_err(|_| ErrorCode::StdIoError)?;
    let socket = async_io::Async::<UdpSocket>::new_nonblocking(socket.into())
        .map_err(|_| ErrorCode::StdIoError)?;

    socket
        .get_ref()
        .join_multicast_v6(&MDNS_IPV6_BROADCAST_ADDR, interface)
        .map_err(|_| ErrorCode::StdIoError)?;
    // IPv4 multicast join on an IPv6-domain socket (`IP_ADD_MEMBERSHIP` on
    // an `AF_INET6` fd) is a real, confirmed platform inconsistency, not a
    // typo: it fails with EINVAL on this dev host's macOS/BSD IPv6 stack
    // even with `IPV6_V6ONLY` off, while Linux is generally more lenient
    // about the same call on a dual-stack socket. Rather than aborting the
    // whole Matter node over IPv4 mDNS specifically — the IPv6 join above
    // already covers link-local IPv6 discovery, which is what this same
    // socket needs for its OWN send/receive either way — degrade to
    // IPv6-only mDNS and say so, instead of silently limping or crashing.
    // Revisit with a dedicated IPv4-domain socket if a real IPv4-only
    // controller can't discover this device in practice.
    if let Err(e) = socket
        .get_ref()
        .join_multicast_v4(&MDNS_IPV4_BROADCAST_ADDR, &ipv4_addr)
    {
        warn!(
            "Matter mDNS: IPv4 multicast join failed ({e}) — continuing IPv6-only. \
             Known macOS/BSD IP_ADD_MEMBERSHIP-on-AF_INET6 limitation; expected to work \
             on the Linux target."
        );
    }

    BuiltinMdns::new()
        .run(
            &socket,
            &socket,
            &Host {
                hostname,
                ip: ipv4_addr,
                // rs-matter 0.4's `Host.ipv6` is a slice of addresses
                // (0.3.0 took a single one); we still advertise just one.
                ipv6: core::slice::from_ref(&ipv6_addr),
            },
            Some(ipv4_addr),
            Some(interface),
            matter,
            crypto,
        )
        .await
}
