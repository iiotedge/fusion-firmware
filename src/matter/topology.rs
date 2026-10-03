// src/matter/topology.rs
//
// Tells controllers when this node's surface changed.
//
// Matter has an attribute for it: BasicInformation `ConfigurationVersion`. The
// spec says the device MUST bump it whenever its exposed fixed-quality surface
// changes (endpoints added or removed, a firmware update that adds or removes
// functionality), and a controller that already paired the node uses it to
// decide whether to read the endpoint structure again. rs-matter cannot see such
// a change on its own ("the application drives the bump"), and on this firmware
// the surface is decided by a config file: adding a `[[matter.endpoints]]` entry
// to an already-paired node is exactly the case, and without a bump a controller
// may keep showing the old accessory until it is removed and added again.
//
// So the node remembers a signature of what it exposes (its endpoints, their
// device types, clusters, features, supported attributes/commands/events, and the
// firmware build) in `<state_dir>/topology`, and bumps ConfigurationVersion once
// at boot when the signature differs from the last boot's.

use std::path::{Path, PathBuf};

use rs_matter::dm::Endpoint;
use tracing::{info, warn};

/// FNV-1a, 64 bit. Written out rather than `DefaultHasher`, whose algorithm is not
/// guaranteed to stay the same between Rust releases: a toolchain upgrade must not
/// look like a change in what the node exposes.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn num(&mut self, n: u64) {
        self.bytes(&n.to_le_bytes());
    }
}

/// A stable fingerprint of everything a controller can learn about this node by
/// reading its structure: which endpoints exist, what they are, which clusters
/// each serves with which features, and which attributes, commands and events
/// those clusters really support. `build` is the firmware build, so an upgrade
/// that changes behaviour inside a cluster is also a change.
pub(crate) fn signature(build: &str, endpoints: &[Endpoint<'_>]) -> u64 {
    let mut h = Fnv::new();
    h.bytes(build.as_bytes());
    for ep in endpoints {
        h.num(u64::from(ep.id));
        for dt in ep.device_types {
            h.num(u64::from(dt.dtype));
            h.num(u64::from(dt.drev));
        }
        // Separators, so "device types [1,2] then clusters [3]" and "[1] then [2,3]"
        // cannot hash alike.
        h.bytes(b"|clusters|");
        for cl in ep.clusters {
            h.num(u64::from(cl.id));
            h.num(u64::from(cl.revision));
            h.num(u64::from(cl.feature_map));
            h.bytes(b"|attrs|");
            for a in cl
                .attributes
                .iter()
                .filter(|a| (cl.with_attrs)(a, cl.revision, cl.feature_map))
            {
                h.num(u64::from(a.id));
            }
            h.bytes(b"|cmds|");
            for c in cl
                .commands
                .iter()
                .filter(|c| (cl.with_cmds)(c, cl.revision, cl.feature_map))
            {
                h.num(u64::from(c.id));
            }
            h.bytes(b"|events|");
            for e in cl
                .events
                .iter()
                .filter(|e| (cl.with_events)(e, cl.revision, cl.feature_map))
            {
                h.num(u64::from(e.id));
            }
        }
        h.bytes(b"|client|");
        for c in ep.client_clusters {
            h.num(u64::from(*c));
        }
        h.bytes(b"|end|");
    }
    h.0
}

/// What to do at boot, given what was recorded last time.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Same surface as last boot.
    Nothing,
    /// A fresh node nobody has paired: record it, there is no controller to tell.
    Remember,
    /// The surface changed since a boot controllers may have seen: bump.
    Bump,
}

pub(crate) fn decide(recorded: Option<u64>, now: u64, commissioned: bool) -> Action {
    match recorded {
        Some(prev) if prev == now => Action::Nothing,
        Some(_) => Action::Bump,
        // Paired by a build that did not record a signature (this one is the first
        // that does): it is unknowable whether the surface changed, and a spurious
        // bump costs a controller one re-read.
        None if commissioned => Action::Bump,
        None => Action::Remember,
    }
}

fn file(state_dir: &str) -> PathBuf {
    Path::new(state_dir).join("topology")
}

fn recorded(path: &Path) -> Option<u64> {
    u64::from_str_radix(std::fs::read_to_string(path).ok()?.trim(), 16).ok()
}

/// Compare the surface with the last boot's and, if it changed, bump
/// `ConfigurationVersion` through `bump` (which persists and notifies). The new
/// signature is recorded only once the bump went through, so a failure is retried
/// at the next boot instead of being forgotten.
pub(crate) fn apply(
    state_dir: &str,
    now: u64,
    commissioned: bool,
    bump: impl FnOnce() -> Result<u32, rs_matter::error::Error>,
) {
    let path = file(state_dir);
    let remember = || {
        if let Err(e) = std::fs::write(&path, format!("{now:016x}\n")) {
            warn!(
                "Matter: could not record the node's topology in {}: {e}",
                path.display()
            );
        }
    };
    match decide(recorded(&path), now, commissioned) {
        Action::Nothing => {}
        Action::Remember => remember(),
        Action::Bump => match bump() {
            Ok(version) => {
                info!(
                    configuration_version = version,
                    "Matter: the endpoints or firmware changed since the last boot - ConfigurationVersion bumped so controllers read the node's structure again"
                );
                remember();
            }
            Err(e) => warn!(
                "Matter: could not bump ConfigurationVersion: {e:?} (will retry at the next boot)"
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rs_matter::dm::clusters::app::on_off;
    use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
    use rs_matter::dm::{Cluster, DeviceType};
    use std::cell::Cell;

    const LIGHT: DeviceType = DeviceType {
        dtype: 0x0100,
        drev: 3,
    };
    const PLUG: DeviceType = DeviceType {
        dtype: 0x010A,
        drev: 3,
    };

    fn endpoint<'a>(id: u16, dt: &'a [DeviceType], clusters: &'a [Cluster<'a>]) -> Endpoint<'a> {
        Endpoint::new(id, dt, clusters)
    }

    #[test]
    fn the_signature_is_stable_and_reads_every_part_of_the_surface() {
        let on_off_cluster = [on_off::FULL_CLUSTER];
        let desc_cluster = [desc::DescHandler::CLUSTER];
        let base = [endpoint(1, &[LIGHT], &on_off_cluster)];
        let s = signature("build-a", &base);
        assert_eq!(s, signature("build-a", &base), "deterministic");

        assert_ne!(s, signature("build-b", &base), "a different firmware build");
        assert_ne!(
            s,
            signature("build-a", &[endpoint(2, &[LIGHT], &on_off_cluster)]),
            "a different endpoint id"
        );
        assert_ne!(
            s,
            signature("build-a", &[endpoint(1, &[PLUG], &on_off_cluster)]),
            "a different device type"
        );
        assert_ne!(
            s,
            signature("build-a", &[endpoint(1, &[LIGHT], &desc_cluster)]),
            "a different cluster"
        );
        assert_ne!(
            s,
            signature(
                "build-a",
                &[
                    endpoint(1, &[LIGHT], &on_off_cluster),
                    endpoint(2, &[PLUG], &on_off_cluster)
                ]
            ),
            "an endpoint added"
        );
        assert_ne!(s, signature("build-a", &[]), "every endpoint removed");
        let narrowed = [on_off::FULL_CLUSTER.with_features(on_off::FULL_CLUSTER.feature_map ^ 1)];
        assert_ne!(
            s,
            signature("build-a", &[endpoint(1, &[LIGHT], &narrowed)]),
            "a different feature map"
        );
    }

    #[test]
    fn a_fresh_node_is_remembered_and_a_known_changed_one_is_bumped() {
        assert_eq!(decide(None, 7, false), Action::Remember, "nobody to tell");
        assert_eq!(
            decide(None, 7, true),
            Action::Bump,
            "paired by a build that recorded nothing"
        );
        assert_eq!(decide(Some(7), 7, true), Action::Nothing);
        assert_eq!(decide(Some(7), 7, false), Action::Nothing);
        assert_eq!(decide(Some(6), 7, true), Action::Bump);
        assert_eq!(decide(Some(6), 7, false), Action::Bump);
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fusion-topology-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn apply_bumps_once_per_change_and_records_it() {
        let dir = scratch();
        let state = dir.to_str().unwrap();
        let bumps = Cell::new(0);
        let bump = || {
            bumps.set(bumps.get() + 1);
            Ok(bumps.get() + 1)
        };

        apply(state, 1, false, bump);
        assert_eq!(bumps.get(), 0, "a fresh node is only remembered");
        assert_eq!(recorded(&file(state)), Some(1));

        apply(state, 1, true, || -> Result<u32, _> {
            panic!("unchanged must not bump")
        });

        apply(state, 2, true, bump);
        assert_eq!(bumps.get(), 1, "the surface changed");
        assert_eq!(recorded(&file(state)), Some(2));

        apply(state, 2, true, || -> Result<u32, _> {
            panic!("a second boot with the same surface must not bump")
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_node_paired_before_anything_was_recorded_is_bumped_once() {
        let dir = scratch();
        let state = dir.to_str().unwrap();
        let bumps = Cell::new(0);
        apply(state, 9, true, || {
            bumps.set(bumps.get() + 1);
            Ok(2)
        });
        assert_eq!(bumps.get(), 1);
        apply(state, 9, true, || -> Result<u32, _> { panic!("only once") });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_bump_is_retried_at_the_next_boot_not_forgotten() {
        let dir = scratch();
        let state = dir.to_str().unwrap();
        apply(state, 1, false, || Ok(1));
        apply(state, 2, true, || {
            Err(rs_matter::error::ErrorCode::Failure.into())
        });
        assert_eq!(
            recorded(&file(state)),
            Some(1),
            "the new signature was NOT recorded"
        );
        let bumps = Cell::new(0);
        apply(state, 2, true, || {
            bumps.set(bumps.get() + 1);
            Ok(2)
        });
        assert_eq!(bumps.get(), 1, "retried");
        assert_eq!(recorded(&file(state)), Some(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_record_counts_as_none() {
        let dir = scratch();
        std::fs::write(file(dir.to_str().unwrap()), "not hex at all").unwrap();
        assert_eq!(recorded(&file(dir.to_str().unwrap())), None);
        assert_eq!(recorded(&dir.join("missing")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
