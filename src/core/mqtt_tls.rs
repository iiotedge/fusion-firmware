// src/core/mqtt_tls.rs
//
// Shared rumqttc TLS setup for the small dedicated MQTT connections this
// firmware opens alongside the iiotedge-lib telemetry engine's own uplink
// (src/commands.rs's command channel, src/homeassistant.rs's discovery
// publisher) — both ride the SAME edge.toml [northbound.tls] identity as
// the telemetry engine, so uplink/downlink/discovery can never disagree
// about trust. Raw-PEM Simple config avoids coupling to any rustls
// version (matches the telemetry engine's own choice).
use iiotedge_core::config::TlsConfig;
use rumqttc::{MqttOptions, TlsConfiguration, Transport};
use tracing::warn;

/// Applies `[northbound.tls]` to `options` if enabled; logs and leaves the
/// connection plaintext on any read failure rather than failing the caller
/// outright — a bad TLS file shouldn't take down a secondary MQTT
/// connection any harder than it already does for the command channel.
pub fn apply(options: &mut MqttOptions, tls: &TlsConfig, context: &str) {
    if !tls.enabled {
        return;
    }
    let ca = match std::fs::read(&tls.ca_path) {
        Ok(ca) => ca,
        Err(e) => {
            warn!(
                "{context}: cannot read TLS ca_path '{}': {e}; connecting plaintext",
                tls.ca_path
            );
            return;
        }
    };
    let client_auth = match (
        tls.client_cert_path.is_empty(),
        tls.client_key_path.is_empty(),
    ) {
        (false, false) => {
            match (
                std::fs::read(&tls.client_cert_path),
                std::fs::read(&tls.client_key_path),
            ) {
                (Ok(cert), Ok(key)) => Some((cert, key)),
                _ => {
                    warn!("{context}: failed to read client cert/key; using server-auth TLS only");
                    None
                }
            }
        }
        _ => None,
    };
    options.set_transport(Transport::Tls(TlsConfiguration::Simple {
        ca,
        alpn: None,
        client_auth,
    }));
}
