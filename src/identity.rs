// src/identity.rs
//
// Hardware-derived device identity (TODO.md Phase 12, F10): a device_id
// that's stable across reboots and re-flashes without requiring every unit
// in a fleet to have a unique value hand-baked into its config. Explicit
// [system].device_id always wins — this only fills the gap when it's
// empty, so an existing deployed config that already sets one is
// unaffected.
//
// Resolution order: Linux device-tree serial-number (SoC-unique, survives
// an SD card/eMMC re-flash) -> primary network interface MAC address
// (widely available, less ideal since NICs can be swapped) -> a random
// UUID persisted to the identity file (last resort — works even on a dev
// host or in a container with neither of the above).
use std::fs;
use std::path::Path;
use tracing::{info, warn};

/// Returns the device_id to use: `configured` verbatim if non-empty,
/// otherwise whatever was previously persisted to `identity_path`, or a
/// freshly derived one (persisted for next boot).
pub fn resolve(configured: &str, identity_path: &str) -> String {
    if !configured.is_empty() {
        return configured.to_string();
    }

    if let Some(existing) = read_persisted(identity_path) {
        return existing;
    }

    let derived = device_tree_serial()
        .or_else(primary_mac_address)
        .unwrap_or_else(|| format!("gen-{}", uuid::Uuid::new_v4()));

    match persist(identity_path, &derived) {
        Ok(()) => info!(
            device_id = %derived,
            path = %identity_path,
            "system.device_id empty; derived and persisted device identity"
        ),
        Err(e) => warn!(
            "system.device_id empty and could not persist derived id to \
             {identity_path}: {e} (will re-derive, possibly differently, on next boot)"
        ),
    }
    derived
}

fn read_persisted(path: &str) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    let id = contents.trim();
    (!id.is_empty()).then(|| id.to_string())
}

fn persist(path: &str, device_id: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, device_id)
}

#[cfg(target_os = "linux")]
fn device_tree_serial() -> Option<String> {
    // Populated by the bootloader on virtually every ARM SBC (Rockchip,
    // i.MX) from a /chosen/serial-number device-tree node — survives a
    // storage re-flash, unlike anything kept in the filesystem itself.
    let raw = fs::read("/sys/firmware/devicetree/base/serial-number").ok()?;
    let text = String::from_utf8_lossy(&raw);
    let trimmed = text.trim_end_matches('\0').trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(not(target_os = "linux"))]
fn device_tree_serial() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn primary_mac_address() -> Option<String> {
    let mut ifaces: Vec<String> = fs::read_dir("/sys/class/net")
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name != "lo")
        .collect();
    // Deterministic (eth0 before wlan0 before an unpredictable directory
    // enumeration order), not because interface naming order carries any
    // real meaning — just so re-derivation on a future boot picks the same
    // one if the identity file was somehow lost.
    ifaces.sort();

    for iface in ifaces {
        if let Ok(raw) = fs::read_to_string(format!("/sys/class/net/{iface}/address")) {
            let mac = raw.trim();
            // All-zero is what an unconfigured/virtual interface commonly
            // reports — not usable as a unique identifier.
            if !mac.is_empty() && mac != "00:00:00:00:00:00" {
                return Some(mac.replace(':', "").to_lowercase());
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn primary_mac_address() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_configured_id_always_wins() {
        assert_eq!(resolve("cam-42", "/nonexistent/path"), "cam-42");
    }

    #[test]
    fn empty_configured_id_reads_back_a_persisted_one() {
        let dir = std::env::temp_dir().join(format!("ptz-identity-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("identity.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "already-persisted-id\n").unwrap();

        assert_eq!(resolve("", path.to_str().unwrap()), "already-persisted-id");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_configured_id_with_nothing_persisted_derives_and_persists() {
        let dir = std::env::temp_dir().join(format!("ptz-identity-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested/identity.txt");

        let first = resolve("", path.to_str().unwrap());
        assert!(!first.is_empty());
        // Persisted, so a second call returns the exact same id rather than
        // deriving (and potentially generating) a different one.
        let second = resolve("", path.to_str().unwrap());
        assert_eq!(first, second);

        fs::remove_dir_all(&dir).ok();
    }
}
