// src/onvif/services.rs
//
// ONVIF SOAP services (Device Management + Media, Profile S subset) served
// over HTTP. Requests are dispatched on the operation's local name — the
// small, deterministic subset a VMS needs to enumerate the camera and
// resolve its RTSP URI. WS-Security auth enforcement is Phase 10 scope.
use super::{local_ip, OnvifDeviceInfo};
use crate::ptz::PtzController;
use chrono::{Datelike, Timelike, Utc};
use std::sync::Arc;
use tiny_http::{Header, Response, Server};
use tracing::{debug, info, warn};

pub fn run(
    device: Arc<OnvifDeviceInfo>,
    access: crate::security::AccessControl,
    ptz: Option<Arc<PtzController>>,
) {
    let server = match Server::http(("0.0.0.0", device.http_port)) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "ONVIF HTTP service disabled: cannot bind port {}: {}",
                device.http_port, e
            );
            return;
        }
    };
    info!(
        "ONVIF device service at http://0.0.0.0:{}/onvif/device_service",
        device.http_port
    );

    for mut request in server.incoming_requests() {
        let mut body = String::new();
        if let Err(e) = request.as_reader().read_to_string(&mut body) {
            warn!("ONVIF: failed to read request body: {e}");
            continue;
        }

        // Host header (address the client actually reached us on) beats any
        // local guess — it survives NAT and multi-homed devices.
        let host = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Host"))
            .map(|h| h.value.as_str().to_string())
            .unwrap_or_else(|| format!("{}:{}", local_ip(), device.http_port));
        let host_ip = host.split(':').next().unwrap_or(&host).to_string();

        // WS-UsernameToken enforcement (unauthenticated discovery-style calls
        // are still refused when auth is on — a VMS always sends the token).
        let xml = if access.onvif_enforced() && !authorized(&body, &access) {
            debug!("ONVIF request rejected: missing/invalid WS-UsernameToken");
            envelope(&fault(
                "ter:NotAuthorized",
                "Sender not authorized (WS-UsernameToken required)",
            ))
        } else {
            dispatch(&body, &host, &host_ip, &device, ptz.as_ref())
        };
        let response = Response::from_string(xml).with_header(
            Header::from_bytes(
                &b"Content-Type"[..],
                &b"application/soap+xml; charset=utf-8"[..],
            )
            .expect("static header is valid"),
        );
        if let Err(e) = request.respond(response) {
            warn!("ONVIF: failed to send response: {e}");
        }
    }
}

/// True when the request carries a WS-UsernameToken that verifies against the
/// user store.
fn authorized(body: &str, access: &crate::security::AccessControl) -> bool {
    crate::security::parse_ws_token(body)
        .map(|token| access.verify_ws_token(&token))
        .unwrap_or(false)
}

type Handler = fn(&Ctx) -> String;

fn dispatch(
    body: &str,
    host: &str,
    host_ip: &str,
    device: &OnvifDeviceInfo,
    ptz: Option<&Arc<PtzController>>,
) -> String {
    let service_xaddr = format!("http://{host}/onvif/device_service");
    let operations: &[(&str, Handler)] = &[
        ("GetSystemDateAndTime", get_system_date_and_time),
        ("GetDeviceInformation", get_device_information),
        ("GetCapabilities", get_capabilities),
        ("GetServices", get_services),
        ("GetScopes", get_scopes),
        ("GetProfiles", get_profiles),
        ("GetVideoSources", get_video_sources),
        ("GetStreamUri", get_stream_uri),
        ("GetHostname", get_hostname),
    ];

    let ctx = Ctx {
        device,
        service_xaddr,
        host_ip: host_ip.to_string(),
    };

    // PTZ operations need request-body parameters (velocities, preset
    // tokens) the generic `Handler = fn(&Ctx) -> String` signature doesn't
    // carry — handled separately rather than changing every existing
    // handler's signature just for this one service.
    if let Some(xml) = super::ptz::dispatch(body, ptz) {
        debug!("ONVIF request: PTZ operation");
        return envelope(&xml);
    }

    for (name, handler) in operations {
        if body.contains(name) {
            debug!("ONVIF request: {name}");
            return envelope(&handler(&ctx));
        }
    }

    debug!("ONVIF request: unsupported operation");
    envelope(&fault(
        "ter:ActionNotSupported",
        "Operation not implemented",
    ))
}

struct Ctx<'a> {
    device: &'a OnvifDeviceInfo,
    service_xaddr: String,
    host_ip: String,
}

fn envelope(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:ter="http://www.onvif.org/ver10/error">
<SOAP-ENV:Body>{body}</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#
    )
}

pub(super) fn fault(subcode: &str, reason: &str) -> String {
    format!(
        "<SOAP-ENV:Fault><SOAP-ENV:Code><SOAP-ENV:Value>SOAP-ENV:Receiver</SOAP-ENV:Value>\
         <SOAP-ENV:Subcode><SOAP-ENV:Value>{subcode}</SOAP-ENV:Value></SOAP-ENV:Subcode></SOAP-ENV:Code>\
         <SOAP-ENV:Reason><SOAP-ENV:Text xml:lang=\"en\">{reason}</SOAP-ENV:Text></SOAP-ENV:Reason></SOAP-ENV:Fault>"
    )
}

fn get_system_date_and_time(_ctx: &Ctx) -> String {
    let now = Utc::now();
    format!(
        "<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime>\
         <tt:DateTimeType>NTP</tt:DateTimeType><tt:DaylightSavings>false</tt:DaylightSavings>\
         <tt:UTCDateTime>\
         <tt:Time><tt:Hour>{}</tt:Hour><tt:Minute>{}</tt:Minute><tt:Second>{}</tt:Second></tt:Time>\
         <tt:Date><tt:Year>{}</tt:Year><tt:Month>{}</tt:Month><tt:Day>{}</tt:Day></tt:Date>\
         </tt:UTCDateTime>\
         </tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>",
        now.hour(),
        now.minute(),
        now.second(),
        now.year(),
        now.month(),
        now.day(),
    )
}

fn get_device_information(ctx: &Ctx) -> String {
    let d = ctx.device;
    format!(
        "<tds:GetDeviceInformationResponse>\
         <tds:Manufacturer>{}</tds:Manufacturer>\
         <tds:Model>{}</tds:Model>\
         <tds:FirmwareVersion>{}</tds:FirmwareVersion>\
         <tds:SerialNumber>{}</tds:SerialNumber>\
         <tds:HardwareId>{}</tds:HardwareId>\
         </tds:GetDeviceInformationResponse>",
        d.manufacturer, d.model, d.firmware_version, d.serial_number, d.hardware_id
    )
}

fn get_capabilities(ctx: &Ctx) -> String {
    let ptz = if ctx.device.ptz_enabled {
        format!(
            "<tt:PTZ><tt:XAddr>{}</tt:XAddr></tt:PTZ>",
            ctx.service_xaddr
        )
    } else {
        String::new()
    };
    format!(
        "<tds:GetCapabilitiesResponse><tds:Capabilities>\
         <tt:Device><tt:XAddr>{x}</tt:XAddr></tt:Device>\
         <tt:Media><tt:XAddr>{x}</tt:XAddr>\
         <tt:StreamingCapabilities>\
         <tt:RTPMulticast>false</tt:RTPMulticast>\
         <tt:RTP_TCP>true</tt:RTP_TCP>\
         <tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP>\
         </tt:StreamingCapabilities></tt:Media>\
         {ptz}\
         </tds:Capabilities></tds:GetCapabilitiesResponse>",
        x = ctx.service_xaddr
    )
}

fn get_services(ctx: &Ctx) -> String {
    let ptz = if ctx.device.ptz_enabled {
        format!(
            "<tds:Service><tds:Namespace>http://www.onvif.org/ver20/ptz/wsdl</tds:Namespace>\
             <tds:XAddr>{x}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>60</tt:Minor></tds:Version></tds:Service>",
            x = ctx.service_xaddr
        )
    } else {
        String::new()
    };
    format!(
        "<tds:GetServicesResponse>\
         <tds:Service><tds:Namespace>http://www.onvif.org/ver10/device/wsdl</tds:Namespace>\
         <tds:XAddr>{x}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>60</tt:Minor></tds:Version></tds:Service>\
         <tds:Service><tds:Namespace>http://www.onvif.org/ver10/media/wsdl</tds:Namespace>\
         <tds:XAddr>{x}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>60</tt:Minor></tds:Version></tds:Service>\
         {ptz}\
         </tds:GetServicesResponse>",
        x = ctx.service_xaddr
    )
}

fn get_scopes(ctx: &Ctx) -> String {
    let d = ctx.device;
    let scope = |uri: String| {
        format!(
            "<tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>{uri}</tt:ScopeItem></tds:Scopes>"
        )
    };
    format!(
        "<tds:GetScopesResponse>{}{}{}{}</tds:GetScopesResponse>",
        scope("onvif://www.onvif.org/type/video_encoder".to_string()),
        scope("onvif://www.onvif.org/type/Network_Video_Transmitter".to_string()),
        scope(format!("onvif://www.onvif.org/name/{}", d.serial_number)),
        scope(format!("onvif://www.onvif.org/location/{}", d.facility_id)),
    )
}

fn get_profiles(ctx: &Ctx) -> String {
    let d = ctx.device;
    // A profile's PTZConfiguration is what tells a client's UI "this stream
    // supports PTZ" — without it, most clients won't even show move
    // controls even if the PTZ service itself works.
    let ptz_config = if d.ptz_enabled {
        "<tt:PTZConfiguration token=\"ptz_main\"><tt:Name>PTZ</tt:Name>\
         <tt:UseCount>1</tt:UseCount><tt:NodeToken>ptz_node_main</tt:NodeToken>\
         </tt:PTZConfiguration>"
    } else {
        ""
    };
    // Note: strict Profile S (media ver10) only enumerates JPEG/MPEG4/H264;
    // H265 here matches the widely tolerated extension until the media2
    // service lands (TODO.md Phase 10).
    format!(
        "<trt:GetProfilesResponse><trt:Profiles token=\"profile_main\" fixed=\"true\">\
         <tt:Name>MainStream</tt:Name>\
         <tt:VideoSourceConfiguration token=\"vsconf_main\"><tt:Name>VideoSource</tt:Name>\
         <tt:UseCount>1</tt:UseCount><tt:SourceToken>video_source_main</tt:SourceToken>\
         <tt:Bounds x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\"/></tt:VideoSourceConfiguration>\
         <tt:VideoEncoderConfiguration token=\"venc_main\"><tt:Name>VideoEncoder</tt:Name>\
         <tt:UseCount>1</tt:UseCount><tt:Encoding>{enc}</tt:Encoding>\
         <tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution>\
         <tt:Quality>5</tt:Quality>\
         <tt:RateControl><tt:FrameRateLimit>{fps}</tt:FrameRateLimit>\
         <tt:EncodingInterval>1</tt:EncodingInterval><tt:BitrateLimit>{kbps}</tt:BitrateLimit></tt:RateControl>\
         <tt:SessionTimeout>PT60S</tt:SessionTimeout></tt:VideoEncoderConfiguration>\
         {ptz_config}\
         </trt:Profiles></trt:GetProfilesResponse>",
        w = d.width,
        h = d.height,
        fps = d.fps,
        kbps = d.bitrate_kbps,
        enc = d.video_encoding,
    )
}

fn get_video_sources(ctx: &Ctx) -> String {
    let d = ctx.device;
    format!(
        "<trt:GetVideoSourcesResponse><trt:VideoSources token=\"video_source_main\">\
         <tt:Framerate>{fps}</tt:Framerate>\
         <tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution>\
         </trt:VideoSources></trt:GetVideoSourcesResponse>",
        fps = d.fps,
        w = d.width,
        h = d.height,
    )
}

fn get_stream_uri(ctx: &Ctx) -> String {
    let d = ctx.device;
    format!(
        "<trt:GetStreamUriResponse><trt:MediaUri>\
         <tt:Uri>rtsp://{ip}:{port}{path}</tt:Uri>\
         <tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>\
         <tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>\
         <tt:Timeout>PT60S</tt:Timeout>\
         </trt:MediaUri></trt:GetStreamUriResponse>",
        ip = ctx.host_ip,
        port = d.rtsp_port,
        path = d.rtsp_path,
    )
}

fn get_hostname(ctx: &Ctx) -> String {
    format!(
        "<tds:GetHostnameResponse><tds:HostnameInformation>\
         <tt:FromDHCP>false</tt:FromDHCP><tt:Name>{}</tt:Name>\
         </tds:HostnameInformation></tds:GetHostnameResponse>",
        ctx.device.serial_number
    )
}
