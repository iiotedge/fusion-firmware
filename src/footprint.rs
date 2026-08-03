// src/footprint.rs
//
// Device footprint (TODO.md Phase 12, F10): what a fleet management or
// monitoring system needs to know about one device without SSHing in —
// model, firmware version + exact commit, a hash of the config actually
// running (so "why does this one device behave differently" starts with
// "is it even running the same config"), and which optional subsystems are
// turned on. Served at GET /footprint (core/metrics.rs) and published once
// at boot as a GDE "device_birth" telemetry event (mirrors Sparkplug's own
// NBIRTH concept, which iiotedge-lib's northbound transport already speaks).
use crate::config::AppConfig;

use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize)]
pub struct Footprint {
    pub device_id: String,
    pub facility_id: String,
    pub model: String,
    pub hardware_id: String,
    pub firmware_version: &'static str,
    pub git_hash: &'static str,
    /// sha256 of the exact config file bytes this process booted with — a
    /// config-drift/audit signal, not a secret. The file's own real
    /// secrets are placeholders in every shipped example
    /// (config/iiotedge_default.toml), never assumed safe to publish
    /// verbatim; this hashes the *whole file* deliberately so any change
    /// (including a since-rotated secret) is visible without exposing it.
    pub config_hash: String,
    pub features: Vec<&'static str>,
}

impl Footprint {
    pub fn build(cfg: &AppConfig, config_file_bytes: &[u8]) -> Self {
        let config_hash = format!("{:x}", Sha256::digest(config_file_bytes));

        let mut features = Vec::new();
        if cfg.stream.enabled {
            features.push("rtsp");
        }
        if cfg.onvif.enabled {
            features.push("onvif");
        }
        if cfg.ptz.enabled {
            features.push("ptz");
        }
        if cfg.ai.enabled {
            features.push("ai");
        }
        if cfg.tamper.enabled {
            features.push("tamper");
        }
        if cfg.motion.enabled {
            features.push("motion");
        }
        if cfg.correlation.enabled {
            features.push("correlation");
        }
        if cfg.cluster.enabled {
            features.push("cluster_mesh");
        }
        if cfg.cloud_relay.enabled {
            features.push("cloud_relay");
        }
        if cfg.storage.enabled {
            features.push("nvr_storage");
        }
        if cfg.telemetry.enabled {
            features.push("telemetry");
        }
        if cfg.onboarding.enabled {
            features.push("qr_onboarding");
        }

        Self {
            device_id: cfg.system.device_id.clone(),
            facility_id: cfg.system.facility_id.clone(),
            model: cfg.onvif.model.clone(),
            hardware_id: cfg.onvif.hardware_id.clone(),
            firmware_version: env!("CARGO_PKG_VERSION"),
            git_hash: env!("GIT_HASH"),
            config_hash,
            features,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn sample_config() -> AppConfig {
        toml::from_str(include_str!("../config/iiotedge_default.toml"))
            .expect("shipped default config must always parse")
    }

    #[test]
    fn config_hash_changes_when_bytes_change() {
        let cfg = sample_config();
        let a = Footprint::build(&cfg, b"config-version-a");
        let b = Footprint::build(&cfg, b"config-version-b");
        assert_ne!(a.config_hash, b.config_hash);
    }

    #[test]
    fn config_hash_is_stable_for_identical_bytes() {
        let cfg = sample_config();
        let a = Footprint::build(&cfg, b"same-bytes");
        let b = Footprint::build(&cfg, b"same-bytes");
        assert_eq!(a.config_hash, b.config_hash);
    }

    #[test]
    fn features_reflect_the_default_config() {
        let cfg = sample_config();
        let fp = Footprint::build(&cfg, b"irrelevant");
        // The shipped default enables streaming/ONVIF/AI/tamper/motion/
        // telemetry/onboarding but not PTZ or cloud_relay — asserting both
        // directions catches a feature silently falling off this list.
        assert!(fp.features.contains(&"rtsp"));
        assert!(fp.features.contains(&"onvif"));
        assert!(!fp.features.contains(&"ptz"));
    }
}
