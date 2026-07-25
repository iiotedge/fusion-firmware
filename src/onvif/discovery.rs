// src/onvif/discovery.rs
//
// WS-Discovery responder (ONVIF §7): listens on the well-known multicast
// group 239.255.255.250:3702 and answers Probe messages with a ProbeMatch
// pointing at our device service, so VMS/NVR software can find the camera
// without manual IP entry.
use super::{local_ip, OnvifDeviceInfo};
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::Arc;
use tracing::{debug, info, warn};

const WS_DISCOVERY_PORT: u16 = 3702;
const WS_DISCOVERY_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);

pub fn run(device: Arc<OnvifDeviceInfo>) {
    let socket = match UdpSocket::bind(("0.0.0.0", WS_DISCOVERY_PORT)) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "WS-Discovery disabled: cannot bind UDP {}: {} (is another ONVIF stack running?)",
                WS_DISCOVERY_PORT, e
            );
            return;
        }
    };
    if let Err(e) = socket.join_multicast_v4(&WS_DISCOVERY_GROUP, &Ipv4Addr::UNSPECIFIED) {
        warn!("WS-Discovery: failed to join multicast group: {e}");
        return;
    }

    info!(
        "WS-Discovery responder listening on {}:{}",
        WS_DISCOVERY_GROUP, WS_DISCOVERY_PORT
    );

    let mut buf = [0u8; 8192];
    loop {
        let (len, peer) = match socket.recv_from(&mut buf) {
            Ok(x) => x,
            Err(e) => {
                warn!("WS-Discovery recv error: {e}");
                continue;
            }
        };

        let body = String::from_utf8_lossy(&buf[..len]);
        // Respond to Probe requests only — never to other devices' answers.
        if !body.contains("Probe") || body.contains("ProbeMatch") {
            continue;
        }

        let relates_to = extract_message_id(&body).unwrap_or_default();
        let response = probe_match(&device, &relates_to);
        match socket.send_to(response.as_bytes(), peer) {
            Ok(_) => debug!("Answered WS-Discovery probe from {peer}"),
            Err(e) => warn!("WS-Discovery: failed to answer {peer}: {e}"),
        }
    }
}

/// Pull the value of the (arbitrarily prefixed) MessageID header out of the
/// probe so our answer can reference it in RelatesTo.
fn extract_message_id(body: &str) -> Option<String> {
    let start = body.find("MessageID")?;
    let rest = &body[start..];
    let open = rest.find('>')?;
    let close = rest[open + 1..].find('<')?;
    Some(rest[open + 1..open + 1 + close].trim().to_string())
}

fn probe_match(device: &OnvifDeviceInfo, relates_to: &str) -> String {
    let xaddr = format!(
        "http://{}:{}/onvif/device_service",
        local_ip(),
        device.http_port
    );
    let scopes = format!(
        "onvif://www.onvif.org/type/video_encoder \
         onvif://www.onvif.org/type/Network_Video_Transmitter \
         onvif://www.onvif.org/name/{} \
         onvif://www.onvif.org/location/{} \
         onvif://www.onvif.org/hardware/{}",
        percent_encode(&device.serial_number),
        percent_encode(&device.facility_id),
        percent_encode(&device.hardware_id),
    );

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
<SOAP-ENV:Header>
<wsa:MessageID>urn:uuid:{message_id}</wsa:MessageID>
<wsa:RelatesTo>{relates_to}</wsa:RelatesTo>
<wsa:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:To>
<wsa:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</wsa:Action>
</SOAP-ENV:Header>
<SOAP-ENV:Body>
<d:ProbeMatches>
<d:ProbeMatch>
<wsa:EndpointReference><wsa:Address>urn:uuid:{device_uuid}</wsa:Address></wsa:EndpointReference>
<d:Types>dn:NetworkVideoTransmitter</d:Types>
<d:Scopes>{scopes}</d:Scopes>
<d:XAddrs>{xaddr}</d:XAddrs>
<d:MetadataVersion>1</d:MetadataVersion>
</d:ProbeMatch>
</d:ProbeMatches>
</SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#,
        message_id = uuid::Uuid::new_v4(),
        relates_to = relates_to,
        device_uuid = device.device_uuid,
        scopes = scopes,
        xaddr = xaddr,
    )
}

/// Minimal escaping for scope URIs (spaces are the realistic offender in
/// model/facility names).
fn percent_encode(s: &str) -> String {
    s.replace(' ', "%20")
}
