// src/onvif/mod.rs
//
// ONVIF network services (Profile S baseline): WS-Discovery so VMS/NVR
// software finds the camera, and SOAP device/media services so it can read
// device info and resolve the RTSP stream URI. Everything here is derived
// from config — no hardware or vendor specifics.
pub mod discovery;
pub mod services;

use crate::config::AppConfig;
use crate::stream::encoder::Codec;
use std::net::UdpSocket;
use std::sync::Arc;
use std::thread::JoinHandle;
use tracing::{info, warn};

/// Immutable device identity + media description shared by the discovery and
/// SOAP service threads. Built once from config at boot.
pub struct OnvifDeviceInfo {
    pub device_uuid: uuid::Uuid,
    pub manufacturer: String,
    pub model: String,
    pub hardware_id: String,
    pub serial_number: String,
    pub firmware_version: &'static str,
    pub facility_id: String,
    pub http_port: u16,
    pub rtsp_port: u16,
    pub rtsp_path: String,
    pub video_encoding: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl OnvifDeviceInfo {
    pub fn from_config(cfg: &AppConfig) -> Self {
        // v5 UUID from the device id: stable across reboots, unique per
        // device — exactly what WS-Discovery endpoint references need.
        let device_uuid =
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, cfg.system.device_id.as_bytes());

        let video_encoding = Codec::parse(&cfg.stream.codec)
            .map(|c| c.onvif_name())
            .unwrap_or("H264");

        Self {
            device_uuid,
            manufacturer: cfg.onvif.manufacturer.clone(),
            model: cfg.onvif.model.clone(),
            hardware_id: cfg.onvif.hardware_id.clone(),
            serial_number: cfg.system.device_id.clone(),
            firmware_version: env!("CARGO_PKG_VERSION"),
            facility_id: cfg.system.facility_id.clone(),
            http_port: cfg.onvif.port,
            rtsp_port: cfg.stream.rtsp_port,
            rtsp_path: cfg.stream.rtsp_path.clone(),
            video_encoding,
            width: cfg.camera.width,
            height: cfg.camera.height,
            fps: cfg.camera.fps,
            bitrate_kbps: cfg.stream.bitrate_kbps,
        }
    }
}

/// Start the ONVIF service threads. Failures are logged, not fatal: a camera
/// that can't announce itself must still capture, record and stream.
pub fn spawn(cfg: &AppConfig, access: crate::security::AccessControl) -> Vec<JoinHandle<()>> {
    if !cfg.onvif.enabled {
        info!("ONVIF services disabled by config");
        return Vec::new();
    }

    let device = Arc::new(OnvifDeviceInfo::from_config(cfg));
    let mut handles = Vec::new();

    let http_device = device.clone();
    match std::thread::Builder::new()
        .name("onvif_http".to_string())
        .spawn(move || services::run(http_device, access))
    {
        Ok(handle) => handles.push(handle),
        Err(e) => warn!("Failed to spawn ONVIF HTTP service: {e}"),
    }

    if cfg.onvif.discovery_enabled {
        match std::thread::Builder::new()
            .name("onvif_discovery".to_string())
            .spawn(move || discovery::run(device))
        {
            Ok(handle) => handles.push(handle),
            Err(e) => warn!("Failed to spawn WS-Discovery responder: {e}"),
        }
    }

    handles
}

/// Best-effort primary LAN address, used in discovery XAddrs where the peer
/// needs a routable IP (unlike HTTP responses, where the Host header wins).
pub(crate) fn local_ip() -> String {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}
