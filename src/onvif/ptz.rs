// src/onvif/ptz.rs
//
// ONVIF PTZ service (ver20/ptz/wsdl subset): the operations a VMS/mobile
// client needs for joystick-style control plus presets — ContinuousMove,
// Stop, SetPreset, GotoPreset, GetPresets. Dispatches straight into the
// shared PtzController (src/ptz/), the exact same instance the MQTT
// command channel uses (see commands.rs), so ONVIF and MQTT control never
// race each other or disagree about "is it moving."
use super::services::fault;
use crate::ptz::PtzController;
use crate::security::inner_text;

use std::sync::Arc;
use tracing::warn;

/// `None` when `body` names no PTZ operation, so the caller (services.rs's
/// `dispatch`) falls through to the Device/Media table unchanged. A
/// recognized PTZ operation always gets an answer — a SOAP fault when `ptz`
/// is `None` (feature disabled), never silence — so a client gets a clear
/// "not supported here" instead of a request that just does nothing.
pub(super) fn dispatch(body: &str, ptz: Option<&Arc<PtzController>>) -> Option<String> {
    if body.contains("ContinuousMove") {
        Some(continuous_move(body, ptz))
    } else if body.contains("SetPreset") {
        Some(set_preset(body, ptz))
    } else if body.contains("GotoPreset") {
        Some(goto_preset(body, ptz))
    } else if body.contains("GetPresets") {
        Some(get_presets(ptz))
    } else if body.contains("Stop") {
        Some(stop(ptz))
    } else {
        None
    }
}

fn continuous_move(body: &str, ptz: Option<&Arc<PtzController>>) -> String {
    let Some(ptz) = ptz else {
        return disabled_fault();
    };
    let pan = attr_f32(body, "PanTilt", "x").unwrap_or(0.0);
    let tilt = attr_f32(body, "PanTilt", "y").unwrap_or(0.0);
    let zoom = attr_f32(body, "Zoom", "x").unwrap_or(0.0);
    match ptz.continuous_move(pan, tilt, zoom) {
        Ok(()) => "<tptz:ContinuousMoveResponse/>".to_string(),
        Err(e) => {
            warn!("PTZ ContinuousMove failed: {e}");
            fault("ter:Action", &format!("PTZ move failed: {e}"))
        }
    }
}

fn stop(ptz: Option<&Arc<PtzController>>) -> String {
    let Some(ptz) = ptz else {
        return disabled_fault();
    };
    match ptz.stop() {
        Ok(()) => "<tptz:StopResponse/>".to_string(),
        Err(e) => {
            warn!("PTZ Stop failed: {e}");
            fault("ter:Action", &format!("PTZ stop failed: {e}"))
        }
    }
}

fn goto_preset(body: &str, ptz: Option<&Arc<PtzController>>) -> String {
    let Some(ptz) = ptz else {
        return disabled_fault();
    };
    let Some(preset) = preset_token(body) else {
        return fault("ter:InvalidArgs", "PresetToken missing or not numeric");
    };
    match ptz.goto_preset(preset) {
        Ok(()) => "<tptz:GotoPresetResponse/>".to_string(),
        Err(e) => {
            warn!("PTZ GotoPreset failed: {e}");
            fault("ter:Action", &format!("PTZ goto-preset failed: {e}"))
        }
    }
}

fn set_preset(body: &str, ptz: Option<&Arc<PtzController>>) -> String {
    let Some(ptz) = ptz else {
        return disabled_fault();
    };
    let Some(preset) = preset_token(body) else {
        return fault("ter:InvalidArgs", "PresetToken missing or not numeric");
    };
    match ptz.set_preset(preset) {
        Ok(()) => format!(
            "<tptz:SetPresetResponse><tptz:PresetToken>{preset}</tptz:PresetToken></tptz:SetPresetResponse>"
        ),
        Err(e) => {
            warn!("PTZ SetPreset failed: {e}");
            fault("ter:Action", &format!("PTZ set-preset failed: {e}"))
        }
    }
}

fn get_presets(ptz: Option<&Arc<PtzController>>) -> String {
    let Some(ptz) = ptz else {
        return disabled_fault();
    };
    let items: String = ptz
        .presets()
        .into_iter()
        .map(|p| format!("<tptz:Preset token=\"{p}\"><tt:Name>preset_{p}</tt:Name></tptz:Preset>"))
        .collect();
    format!("<tptz:GetPresetsResponse>{items}</tptz:GetPresetsResponse>")
}

fn disabled_fault() -> String {
    fault(
        "ter:ActionNotSupported",
        "PTZ is not enabled on this device",
    )
}

/// `<tptz:PresetToken>N</tptz:PresetToken>` — Pelco-D presets are a small
/// numeric range (this driver uses u8), not the free-form string token
/// ONVIF's schema technically allows.
fn preset_token(body: &str) -> Option<u8> {
    inner_text(body, "PresetToken")?.trim().parse().ok()
}

/// Attribute value on the first `<...:local_name ... attr="value" .../>`
/// element. ContinuousMove's Velocity carries pan/tilt/zoom as XML
/// attributes (`<tt:PanTilt x="0.5" y="0.0"/>`), not inner text, so
/// `inner_text` (security.rs) doesn't cover this case — kept just as tiny
/// and just as tolerant of "good enough for well-formed ONVIF bodies."
fn attr_f32(body: &str, local_name: &str, attr: &str) -> Option<f32> {
    let tag_start = body.find(local_name)?;
    let tag_end = body[tag_start..].find(['>', '/']).map(|i| tag_start + i)?;
    let tag = &body[tag_start..tag_end];
    let attr_pat = format!("{attr}=\"");
    let attr_start = tag.find(&attr_pat)? + attr_pat.len();
    let attr_end = tag[attr_start..].find('"')? + attr_start;
    tag[attr_start..attr_end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_ignores_non_ptz_operations() {
        assert!(dispatch("<tds:GetDeviceInformation/>", None).is_none());
    }

    #[test]
    fn continuous_move_without_ptz_returns_fault_not_silence() {
        let xml = dispatch(
            "<tptz:ContinuousMove><tptz:Velocity><tt:PanTilt x=\"0.5\" y=\"0.0\"/></tptz:Velocity></tptz:ContinuousMove>",
            None,
        )
        .unwrap();
        assert!(xml.contains("ActionNotSupported"));
    }

    #[test]
    fn attr_f32_reads_the_right_element_and_attribute() {
        let body = r#"<tt:PanTilt x="0.5" y="-0.25" space="..."/><tt:Zoom x="0.75"/>"#;
        assert_eq!(attr_f32(body, "PanTilt", "x"), Some(0.5));
        assert_eq!(attr_f32(body, "PanTilt", "y"), Some(-0.25));
        assert_eq!(attr_f32(body, "Zoom", "x"), Some(0.75));
    }

    #[test]
    fn attr_f32_missing_attribute_returns_none() {
        let body = r#"<tt:PanTilt x="0.5"/>"#;
        assert_eq!(attr_f32(body, "PanTilt", "y"), None);
    }

    #[test]
    fn preset_token_parses_numeric_body() {
        let body = "<tptz:SetPreset><tptz:PresetToken>7</tptz:PresetToken></tptz:SetPreset>";
        assert_eq!(preset_token(body), Some(7));
    }

    #[test]
    fn preset_token_rejects_non_numeric() {
        let body = "<tptz:SetPreset><tptz:PresetToken>home</tptz:PresetToken></tptz:SetPreset>";
        assert_eq!(preset_token(body), None);
    }
}
