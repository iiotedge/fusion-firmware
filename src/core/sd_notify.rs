// src/core/sd_notify.rs
//
// systemd sd_notify protocol (TODO.md Phase 12, F10): READY=1 once boot
// completes, then a periodic WATCHDOG=1 ping tied to the SAME internal
// watchdog (core/watchdog.rs) already tracking real worker-thread liveness
// — so systemd's own watchdog restart only fires when this firmware's own
// supervisor would also have restarted it, never a second, independent
// notion of "healthy." Hand-rolled rather than a crate: the protocol is one
// datagram write to a Unix socket named in $NOTIFY_SOCKET, nothing a
// dependency meaningfully simplifies.
#[cfg(target_os = "linux")]
mod imp {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    use tracing::debug;

    fn send(message: &str) {
        // Not running under systemd (or the unit isn't Type=notify) — the
        // normal case for `make run`/CI/a dev host. Silent, not a warning:
        // this path runs on every single sd_notify call site.
        let Ok(notify_socket) = std::env::var("NOTIFY_SOCKET") else {
            return;
        };
        let Ok(socket) = UnixDatagram::unbound() else {
            return;
        };

        // NOTIFY_SOCKET names an abstract-namespace socket ("@name", no
        // leading NUL in the env var itself — that's systemd's own
        // convention) or, far more commonly, a real filesystem path.
        let result = match notify_socket.strip_prefix('@') {
            Some(name) => SocketAddr::from_abstract_name(name.as_bytes())
                .and_then(|addr| socket.send_to_addr(message.as_bytes(), &addr).map(|_| ())),
            None => socket
                .send_to(message.as_bytes(), &notify_socket)
                .map(|_| ()),
        };
        if let Err(e) = result {
            debug!("sd_notify send failed (non-fatal, firmware keeps running): {e}");
        }
    }

    pub fn ready() {
        send("READY=1");
    }

    pub fn watchdog_ping() {
        send("WATCHDOG=1");
    }

    /// How often to ping, derived from systemd's own `WatchdogSec=` (which
    /// it exposes back to us as `$WATCHDOG_USEC`) rather than a hardcoded
    /// guess that could drift from whatever the unit file actually says.
    /// `None` when the unit isn't watchdog-enabled at all.
    pub fn watchdog_interval() -> Option<std::time::Duration> {
        let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
        // Recommended systemd practice: ping at roughly half the configured
        // timeout, so one delayed tick alone can't trip a restart.
        Some(std::time::Duration::from_micros(usec / 2).max(std::time::Duration::from_secs(1)))
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub fn ready() {}
    pub fn watchdog_ping() {}
    pub fn watchdog_interval() -> Option<std::time::Duration> {
        None
    }
}

pub use imp::{ready, watchdog_interval, watchdog_ping};
