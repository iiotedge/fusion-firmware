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
use tracing::{debug, info, warn};

/// One address of one network interface, as the OS reported it. A plain struct
/// so interface selection is a pure function (unit-tested without a network).
#[derive(Clone, Debug)]
struct IfAddr {
    name: String,
    index: u32,
    ip: std::net::IpAddr,
    loopback: bool,
}

/// What this node advertises: the interface, its IPv4 address and EVERY usable
/// IPv6 address on it. Matter runs over IPv6, so a controller resolving this node
/// needs AAAA records — an advertisement without them is a node most controllers
/// cannot reach.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Advert {
    name: String,
    interface: u32,
    ipv4: std::net::Ipv4Addr,
    /// Link-local first, then the others in address order; capped at
    /// `MAX_IPV6_ADDRS` (an mDNS response must stay small). EMPTY while the OS
    /// has not listed an IPv6 address yet (see `choose`) — the responder then
    /// runs IPv4-only and is restarted with AAAA records once one appears.
    ipv6: Vec<std::net::Ipv6Addr>,
}

const MAX_IPV6_ADDRS: usize = 4;
/// How often the selected interface's addresses are re-checked for changes.
const RECHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Interfaces that are never where a LAN controller lives: container bridges,
/// tunnels, VM networks, Apple's peer-to-peer links.
fn looks_virtual(name: &str) -> bool {
    ["docker", "veth", "br-", "virbr", "vmnet", "tun", "tap", "utun", "awdl", "llw", "bridge", "zt", "tailscale"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn usable_ipv6(ip: &std::net::Ipv6Addr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast()
}

/// Picks the interface to advertise on: one with an IPv4 address, the usable IPv6
/// addresses it has listed (possibly none), preferring real LAN interfaces over
/// container/VM/tunnel ones.
///
/// Two hard-won facts shape this:
/// * `if-addrs` omits link-local (`fe80::`) addresses unless built with its
///   `link-local` feature (enabled in Cargo.toml). On a LAN with no IPv6 router a
///   node's ONLY IPv6 address is link-local, so without the feature nothing was
///   ever listed and the node advertised no AAAA record at all — unreachable for
///   Matter controllers, which need IPv6. (Found on the real board; it had worked
///   earlier only while the LAN happened to hand out a global address.)
/// * Addresses can still change under a running node (DHCP renewal, an IPv6
///   router appearing, a re-plugged cable, duplicate-address detection finishing
///   late). So an interface without IPv6 is still a valid STARTING point, the
///   caller re-runs this every few seconds, and the responder is restarted when
///   the answer changes.
fn choose(addrs: &[IfAddr]) -> Option<Advert> {
    let mut names: Vec<&str> = Vec::new();
    for a in addrs.iter().filter(|a| !a.loopback) {
        if !names.contains(&a.name.as_str()) {
            names.push(&a.name);
        }
    }
    let mut candidates: Vec<Advert> = names
        .into_iter()
        .filter_map(|name| {
            let on_iface = || addrs.iter().filter(move |a| a.name == name && !a.loopback);
            // Prefer a routable IPv4 over an APIPA 169.254.x.x one.
            let ipv4 = on_iface()
                .filter_map(|a| match a.ip {
                    std::net::IpAddr::V4(v4) if !v4.is_unspecified() => Some(v4),
                    _ => None,
                })
                .min_by_key(|v4| v4.is_link_local())?;
            let mut ipv6: Vec<std::net::Ipv6Addr> = on_iface()
                .filter_map(|a| match a.ip {
                    std::net::IpAddr::V6(v6) if usable_ipv6(&v6) => Some(v6),
                    _ => None,
                })
                .collect();
            ipv6.sort_by_key(|v6| (!v6.is_unicast_link_local(), *v6));
            ipv6.dedup();
            ipv6.truncate(MAX_IPV6_ADDRS);
            Some(Advert {
                name: name.to_string(),
                interface: on_iface().map(|a| a.index).next().unwrap_or(0),
                ipv4,
                ipv6,
            })
        })
        .collect();
    // Real LAN interfaces before virtual ones, then ones that already have IPv6;
    // otherwise the OS's own order.
    candidates.sort_by_key(|c| (looks_virtual(&c.name), c.ipv6.is_empty()));
    candidates.into_iter().next()
}

/// BSD-derived kernels (macOS) store a link-local address's interface index
/// inside the address itself (`fe80:000c::1` for interface 12). That is a
/// kernel-internal form — advertised as is, it is a different, unreachable
/// address — so clear it. Linux never embeds it, where this is a no-op.
fn without_embedded_scope(ip: std::net::IpAddr) -> std::net::IpAddr {
    match ip {
        std::net::IpAddr::V6(v6) if v6.is_unicast_link_local() => {
            let mut octets = v6.octets();
            octets[2] = 0;
            octets[3] = 0;
            std::net::IpAddr::V6(octets.into())
        }
        other => other,
    }
}

/// The OS's current addresses.
fn snapshot() -> Result<Vec<IfAddr>, Error> {
    let all = if_addrs::get_if_addrs().map_err(|_| ErrorCode::StdIoError)?;
    debug!("Available network interfaces: {:?}", all);
    Ok(all
        .iter()
        .map(|ia| IfAddr {
            name: ia.name.clone(),
            index: ia.index.unwrap_or(0),
            ip: without_embedded_scope(ia.ip()),
            loopback: ia.is_loopback(),
        })
        .collect())
}

/// Waits until some interface has an IPv4 address (DHCP can lag the service at
/// boot). IPv6 is NOT waited for — see `choose`: if it shows up later, the
/// periodic re-check picks it up and the responder is restarted.
async fn wait_for_interface() -> Advert {
    let mut logged = false;
    loop {
        if let Some(advert) = snapshot().ok().and_then(|addrs| choose(&addrs)) {
            return advert;
        }
        if !logged {
            warn!("Matter mDNS: no network interface has an IPv4 address yet — waiting");
            logged = true;
        }
        async_io::Timer::after(std::time::Duration::from_secs(1)).await;
    }
}

/// Returns when the addresses of the advertised interface (or which interface to
/// use) change — DHCP renewal, IPv6 arriving late, a cable re-plugged.
async fn wait_for_change(current: &Advert) {
    loop {
        async_io::Timer::after(RECHECK_EVERY).await;
        // A failed enumeration is transient: keep what we have.
        let Ok(addrs) = snapshot() else { continue };
        if choose(&addrs).as_ref() != Some(current) {
            return;
        }
    }
}

/// Runs the mDNS responder until the process shuts down. `hostname` should
/// be stable across reboots — it's what shows up in a Matter controller's
/// discovery log — so callers pass the firmware's own `device_id`, not a
/// random/regenerated value.
///
/// A supervisor around the responder: it waits for a usable interface, serves
/// on it, and RESTARTS the responder whenever that interface's addresses change
/// (an IPv6 prefix appearing, DHCP renewal, a re-plugged cable; the responder
/// re-reads Matter's service list on start and re-announces every 30 s, so a
/// restart loses nothing). A responder error — a bind failure, an
/// interface that vanished — is retried, not fatal: it used to end the whole
/// Matter node.
pub async fn run<C: Crypto + Copy>(matter: &Matter<'_>, crypto: C, hostname: &str) -> Result<(), Error> {
    enum Next {
        AddressesChanged,
        Stopped(Result<(), Error>),
    }
    loop {
        let advert = wait_for_interface().await;
        info!(
            interface = %advert.name,
            ipv4 = %advert.ipv4,
            ipv6 = ?advert.ipv6,
            "Matter mDNS: advertising"
        );
        if advert.ipv6.is_empty() {
            warn!(
                "Matter mDNS: the OS lists no IPv6 address on {} yet — advertising IPv4 only for now \
                 and re-checking every {}s (Matter controllers need the AAAA records IPv6 brings)",
                advert.name,
                RECHECK_EVERY.as_secs()
            );
        }
        let next = futures_lite::future::or(
            async { Next::Stopped(serve(matter, crypto, hostname, &advert).await) },
            async {
                wait_for_change(&advert).await;
                Next::AddressesChanged
            },
        )
        .await;
        match next {
            Next::AddressesChanged => {
                info!("Matter mDNS: the interface's addresses changed — re-advertising");
            }
            Next::Stopped(Ok(())) => return Ok(()),
            Next::Stopped(Err(e)) => {
                warn!("Matter mDNS responder stopped ({e:?}); retrying in 5 s");
                async_io::Timer::after(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

/// Serve mDNS on `advert`'s interface until an error. Never returns `Ok` in
/// practice: the responder runs for as long as the sockets do.
async fn serve<C: Crypto>(matter: &Matter<'_>, crypto: C, hostname: &str, advert: &Advert) -> Result<(), Error> {
    let ipv4_addr: Ipv4Addr = advert.ipv4.octets().into();
    let ipv6_addrs: Vec<Ipv6Addr> = advert.ipv6.iter().map(|a| a.octets().into()).collect();
    let interface = advert.interface;

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

    if let Err(e) = socket
        .get_ref()
        .join_multicast_v6(&MDNS_IPV6_BROADCAST_ADDR, interface)
    {
        if !advert.ipv6.is_empty() {
            return Err(ErrorCode::StdIoError.into());
        }
        // Nothing to advertise over IPv6 yet, so not being able to listen on it
        // yet is expected (the restart on address change joins it).
        debug!("Matter mDNS: IPv6 multicast join failed ({e}) while there is no IPv6 address");
    }
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
        .join_multicast_v4(&MDNS_IPV4_BROADCAST_ADDR, &advert.ipv4)
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
                // rs-matter 0.4's `Host.ipv6` is a slice: every usable address
                // gets an AAAA record.
                ipv6: &ipv6_addrs,
            },
            Some(ipv4_addr),
            Some(interface),
            matter,
            crypto,
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr as V4, Ipv6Addr as V6};

    fn addr(name: &str, index: u32, ip: IpAddr) -> IfAddr {
        IfAddr {
            name: name.to_string(),
            index,
            ip,
            loopback: false,
        }
    }
    fn v4(name: &str, index: u32, a: u8, b: u8, c: u8, d: u8) -> IfAddr {
        addr(name, index, IpAddr::V4(V4::new(a, b, c, d)))
    }
    fn v6(name: &str, index: u32, text: &str) -> IfAddr {
        addr(name, index, IpAddr::V6(text.parse().unwrap()))
    }

    #[test]
    fn a_normal_lan_interface_is_advertised_with_its_link_local_address() {
        // The Radxa's `end1`, as measured.
        let got = choose(&[
            v4("end1", 2, 192, 168, 1, 17),
            v6("end1", 2, "fe80::5783:11df:3ea4:329c"),
        ])
        .unwrap();
        assert_eq!(got.interface, 2);
        assert_eq!(got.ipv4, V4::new(192, 168, 1, 17));
        assert_eq!(got.ipv6, vec!["fe80::5783:11df:3ea4:329c".parse::<V6>().unwrap()]);
    }

    #[test]
    fn an_interface_without_ipv6_yet_is_advertised_ipv4_only_then_upgraded() {
        // An interface with no listed IPv6 is a valid STARTING point (the node is
        // reachable over IPv4 meanwhile), because addresses can arrive later (an
        // IPv6 router appearing, late duplicate-address detection, DHCP). Picking
        // once and never looking again is what must not happen...
        let before = choose(&[v4("end1", 2, 192, 168, 1, 17)]).unwrap();
        assert!(before.ipv6.is_empty());
        // ...but the periodic re-check must see the address arrive as a CHANGE, which
        // is what restarts the responder with AAAA records.
        let after = choose(&[v4("end1", 2, 192, 168, 1, 17), v6("end1", 2, "fe80::1")]).unwrap();
        assert_eq!(after.ipv6.len(), 1);
        assert_ne!(before, after, "IPv6 appearing must read as a change");
    }

    #[test]
    fn an_interface_with_ipv6_but_no_ipv4_is_skipped() {
        // macOS awdl0 / llw0 style links (no IPv4: nothing for a LAN controller).
        assert_eq!(choose(&[v6("awdl0", 9, "fe80::1")]), None);
        let got = choose(&[v6("awdl0", 9, "fe80::1"), v4("en0", 12, 10, 0, 0, 5), v6("en0", 12, "fe80::2")]).unwrap();
        assert_eq!(got.name, "en0");
    }

    #[test]
    fn every_usable_ipv6_address_is_advertised_link_local_first() {
        let got = choose(&[
            v4("eth0", 3, 10, 0, 0, 9),
            v6("eth0", 3, "2001:db8::10"),
            v6("eth0", 3, "fe80::9"),
            v6("eth0", 3, "fd00::5"),
            v6("eth0", 3, "::1"),         // loopback: not usable
            v6("eth0", 3, "::"),          // unspecified: not usable
            v6("eth0", 3, "ff02::fb"),    // multicast: not usable
        ])
        .unwrap();
        let text: Vec<String> = got.ipv6.iter().map(|a| a.to_string()).collect();
        assert_eq!(text, ["fe80::9", "2001:db8::10", "fd00::5"]);
    }

    #[test]
    fn the_address_list_is_capped() {
        let mut addrs = vec![v4("eth0", 3, 10, 0, 0, 9), v6("eth0", 3, "fe80::1")];
        for i in 0..10 {
            addrs.push(v6("eth0", 3, &format!("2001:db8::{i:x}")));
        }
        assert_eq!(choose(&addrs).unwrap().ipv6.len(), MAX_IPV6_ADDRS);
    }

    #[test]
    fn real_lan_interfaces_beat_virtual_ones_whatever_the_os_order() {
        let got = choose(&[
            v4("docker0", 4, 172, 17, 0, 1),
            v6("docker0", 4, "fe80::d"),
            v4("eth0", 3, 192, 168, 1, 9),
            v6("eth0", 3, "fe80::e"),
        ])
        .unwrap();
        assert_eq!(got.name, "eth0");
        // A box with ONLY a virtual interface still advertises on it.
        assert_eq!(choose(&[v4("docker0", 4, 172, 17, 0, 1), v6("docker0", 4, "fe80::d")]).unwrap().name, "docker0");
        // A real LAN interface that has no IPv6 YET still beats a virtual one that
        // does: the re-check upgrades it within seconds, whereas the virtual one
        // would advertise an address no LAN controller can use.
        let got = choose(&[
            v4("docker0", 4, 172, 17, 0, 1),
            v6("docker0", 4, "fe80::d"),
            v4("eth0", 3, 192, 168, 1, 9),
        ])
        .unwrap();
        assert_eq!(got.name, "eth0");
    }

    #[test]
    fn among_real_interfaces_one_with_ipv6_is_preferred() {
        let got = choose(&[
            v4("eth0", 3, 192, 168, 1, 9),
            v4("wlan0", 5, 192, 168, 1, 10),
            v6("wlan0", 5, "fe80::5"),
        ])
        .unwrap();
        assert_eq!(got.name, "wlan0");
    }

    #[test]
    fn loopback_is_never_chosen_and_a_routable_ipv4_beats_apipa() {
        let mut lo4 = v4("lo", 1, 127, 0, 0, 1);
        lo4.loopback = true;
        let mut lo6 = v6("lo", 1, "::1");
        lo6.loopback = true;
        assert_eq!(choose(&[lo4, lo6]), None);
        let got = choose(&[
            v4("eth0", 3, 169, 254, 7, 7),
            v4("eth0", 3, 192, 168, 1, 9),
            v6("eth0", 3, "fe80::e"),
        ])
        .unwrap();
        assert_eq!(got.ipv4, V4::new(192, 168, 1, 9));
    }

    #[test]
    fn a_bsd_embedded_interface_index_is_stripped_from_link_local_addresses() {
        // macOS: fe80:000c::1 means "fe80::1 on interface 12"; advertising it as is
        // would publish an address nothing can reach.
        let embedded: IpAddr = "fe80:c::10ed:3ebe:a861:5b0d".parse().unwrap();
        let expect: IpAddr = "fe80::10ed:3ebe:a861:5b0d".parse().unwrap();
        assert_eq!(without_embedded_scope(embedded), expect);
        // Linux addresses (already clean) and everything else are untouched.
        assert_eq!(without_embedded_scope(expect), expect);
        let global: IpAddr = "2001:db8:1234::1".parse().unwrap();
        assert_eq!(without_embedded_scope(global), global);
        let v4: IpAddr = "192.168.1.9".parse().unwrap();
        assert_eq!(without_embedded_scope(v4), v4);
    }

    #[test]
    fn the_same_addresses_in_a_different_order_do_not_look_like_a_change() {
        // Otherwise every re-check could needlessly restart the responder.
        let a = choose(&[v4("eth0", 3, 10, 0, 0, 9), v6("eth0", 3, "fd00::5"), v6("eth0", 3, "fe80::9")]).unwrap();
        let b = choose(&[v6("eth0", 3, "fe80::9"), v4("eth0", 3, 10, 0, 0, 9), v6("eth0", 3, "fd00::5")]).unwrap();
        assert_eq!(a, b);
        // A genuinely new address IS a change.
        let c = choose(&[v4("eth0", 3, 10, 0, 0, 9), v6("eth0", 3, "fe80::9"), v6("eth0", 3, "fd00::5"), v6("eth0", 3, "2001:db8::1")]).unwrap();
        assert_ne!(a, c);
    }
}
