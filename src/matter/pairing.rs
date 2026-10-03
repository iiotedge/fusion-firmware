// src/matter/pairing.rs
//
// What a person needs in order to add this node to a controller (Apple Home,
// Google Home, Home Assistant, ...), and the three places that show it: the
// boot log, `--matter-qr` on the command line, and `GET
// /onboarding/matter-qr.png`.
//
// All three show the SAME standard setup code, built from `[matter]`'s setup
// passcode, discriminator and vendor/product id (the public test values unless the
// config says otherwise). It carries those and nothing else: 22 characters, a
// 25x25-module QR symbol a phone reads from across the room.
//
// rs-matter's own printer (`Matter::print_standard_qr_text`) is not used for
// that reason. It documents "no optional data" but copies BasicInformation's
// serial number into the code as optional TLV, which made this node's code 73
// characters - a 33x33 symbol, and one matter.js refuses to decode (its Base38
// reader rejects any length that is a multiple of five). Its QR art
// (`print_standard_qr_code`) goes through the log, so journald stamps every row
// with a timestamp and a level and nothing can scan it.

use std::io::IsTerminal;

use rs_matter::pairing::qr::{no_optional_data, CommFlowType, QrPayload};
use rs_matter::pairing::DiscoveryCapabilities;
use rs_matter::sc::pase::{Spake2pVerifierPassword, Spake2pVerifierPasswordRef, MAX_COMM_WINDOW_TIMEOUT_SECS};
use rs_matter::BasicCommData;
use tracing::info;

use crate::config::MatterConfig;

/// The commissioning data this node is added with: the setup passcode and the
/// discriminator from `[matter]` (the public test pair by default).
pub fn comm_data(cfg: &MatterConfig) -> BasicCommData {
    BasicCommData {
        password: Spake2pVerifierPassword::new_from_ref(Spake2pVerifierPasswordRef::new(&cfg.setup_passcode.to_le_bytes())),
        discriminator: cfg.discriminator,
    }
}

/// Everything a person needs to add this node to a controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    /// `MT:...`, what the QR code encodes.
    pub qr_text: String,
    /// `XXXX-XXX-XXXX`, what to type where there is no camera to scan with.
    pub manual_code: String,
}

impl Pairing {
    /// The setup code of the node `[matter]` describes: its passcode, discriminator
    /// and vendor/product id.
    pub fn from_config(cfg: &MatterConfig) -> Result<Self, rs_matter::error::Error> {
        Self::new(comm_data(cfg), cfg.vendor_id, cfg.product_id)
    }

    /// The setup code of a default config: the public test passcode and ids.
    #[cfg(test)]
    pub fn standard() -> Result<Self, rs_matter::error::Error> {
        Self::from_config(&MatterConfig::default())
    }

    fn new(comm: BasicCommData, vid: u16, pid: u16) -> Result<Self, rs_matter::error::Error> {
        let manual_code = comm.compute_pretty_pairing_code().as_str().to_string();
        let payload = QrPayload::new(
            DiscoveryCapabilities::IP,
            CommFlowType::Standard,
            comm,
            vid,
            pid,
            // Empty on purpose: a serial number here becomes optional TLV (see above).
            "",
            no_optional_data,
        );
        let mut buf = [0u8; 128];
        let (text, _) = payload.as_str(&mut buf)?;
        Ok(Self {
            qr_text: text.to_string(),
            manual_code,
        })
    }
}

/// The QR symbol as a square of dark and light modules.
struct Modules {
    width: usize,
    dark: Vec<bool>,
}

impl Modules {
    fn of(text: &str) -> Result<Self, String> {
        let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
        Ok(Self {
            width: code.width(),
            dark: code
                .to_colors()
                .into_iter()
                .map(|c| c == qrcode::Color::Dark)
                .collect(),
        })
    }

    /// Light outside the symbol, so the quiet zone falls out of the indexing.
    fn dark(&self, x: i32, y: i32) -> bool {
        let w = self.width as i32;
        (0..w).contains(&x) && (0..w).contains(&y) && self.dark[(y * w + x) as usize]
    }
}

/// Quiet zone around the symbol, in modules (the QR standard asks for four).
const QUIET_ZONE: i32 = 4;
/// Fixed cube colours, not palette 0/15: a terminal theme may remap those, and a
/// QR code with the wrong contrast does not scan.
const BLACK: u8 = 16;
const WHITE: u8 = 231;

/// The setup code as a QR code a terminal can show and a phone can scan.
///
/// Two module rows per text row (an upper half block whose foreground is the
/// upper module and whose background is the lower one), so modules stay square
/// in a cell that is twice as tall as it is wide. Black on white with an
/// explicit white quiet zone whatever the terminal theme: the usual
/// "dark terminal, inverted code" trick scans on some phones and not others.
/// Meant for a terminal, so no journald timestamps in front of the rows.
pub fn terminal_qr(text: &str) -> Result<String, String> {
    Ok(render_terminal(&Modules::of(text)?))
}

fn render_terminal(modules: &Modules) -> String {
    let shade = |dark: bool| if dark { BLACK } else { WHITE };
    let edge = modules.width as i32 + QUIET_ZONE;
    let mut out = String::new();
    let mut y = -QUIET_ZONE;
    while y < edge {
        for x in -QUIET_ZONE..edge {
            out.push_str(&format!(
                "\x1b[38;5;{};48;5;{}m\u{2580}",
                shade(modules.dark(x, y)),
                shade(modules.dark(x, y + 1))
            ));
        }
        out.push_str("\x1b[0m\n");
        y += 2;
    }
    out
}

/// What `--matter-qr` prints: the manual code, the payload and the QR art.
pub fn operator_report(pairing: &Pairing) -> Result<String, String> {
    Ok(format!(
        "Manual pairing code: {}\nQR payload:          {}\n\n{}\nScan it with the controller's \"add device\" camera, or type the manual code.\n\
         Pairing is open for {} minutes after the firmware starts, and again whenever the last\n\
         controller removes this device; restart the service to open it by hand.\n",
        pairing.manual_code,
        pairing.qr_text,
        terminal_qr(&pairing.qr_text)?,
        MAX_COMM_WINDOW_TIMEOUT_SECS / 60,
    ))
}

/// What the boot log says when the node can be added.
///
/// The journal gets the code and where to find a scannable QR, never QR art:
/// journald stamps each row, so a code printed there cannot be scanned. A
/// developer running in a terminal does get the art, drawn cleanly.
pub fn announce_pairing_open(pairing: &Pairing, reopened: bool) {
    let why = if reopened {
        "the last controller was removed, pairing is open again"
    } else {
        "not commissioned yet, pairing is open"
    };
    info!(
        pairing_code = %pairing.manual_code,
        qr_payload = %pairing.qr_text,
        "Matter: {why} for {} minutes - add this device from a controller app with the pairing code, \
         or scan the QR from `{} --matter-qr` or GET /onboarding/matter-qr.png?token=<command_token>",
        MAX_COMM_WINDOW_TIMEOUT_SECS / 60,
        std::env::args().next().unwrap_or_else(|| "<firmware>".to_string()),
    );
    if std::io::stdout().is_terminal() {
        if let Ok(art) = terminal_qr(&pairing.qr_text) {
            println!("\n{art}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Matter's Base38: three bytes become five characters, two become four, one two.
    fn base38_decode(text: &str) -> Vec<u8> {
        const ALPHABET: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ-.";
        let digit = |c: char| ALPHABET.find(c).expect("a Base38 character") as u32;
        let chars: Vec<char> = text.chars().collect();
        let mut bytes = Vec::new();
        for chunk in chars.chunks(5) {
            let value = chunk.iter().rev().fold(0u32, |acc, &c| acc * 38 + digit(c));
            let n = match chunk.len() {
                5 => 3,
                4 => 2,
                2 => 1,
                other => panic!("{other} characters is not a Base38 group"),
            };
            bytes.extend_from_slice(&value.to_le_bytes()[..n]);
        }
        bytes
    }

    /// The payload's bit fields, read independently of the encoder under test
    /// (Matter Core spec 5.1.3.1: little-endian, version | vid | pid | flow |
    /// rendezvous | discriminator | passcode | padding).
    struct Fields {
        version: u8,
        vid: u16,
        pid: u16,
        flow: u8,
        discriminator: u16,
        passcode: u32,
    }

    fn fields(qr_text: &str) -> (Fields, usize) {
        let data = base38_decode(qr_text.strip_prefix("MT:").expect("MT: prefix"));
        let mut raw = [0u8; 16];
        raw[..11].copy_from_slice(&data[..11]);
        let v = u128::from_le_bytes(raw);
        let bits = |shift: u32, width: u32| ((v >> shift) & ((1u128 << width) - 1)) as u32;
        (
            Fields {
                version: bits(0, 3) as u8,
                vid: bits(3, 16) as u16,
                pid: bits(19, 16) as u16,
                flow: bits(35, 2) as u8,
                discriminator: bits(45, 12) as u16,
                passcode: bits(57, 27),
            },
            data.len() - 11,
        )
    }

    #[test]
    fn the_setup_code_is_the_plain_standard_one_with_no_optional_data() {
        let p = Pairing::standard().unwrap();
        // 3 + 19 characters: 11 bytes of Base38. The serial-number TLV rs-matter's
        // own printer adds made it 73 characters.
        assert_eq!(p.qr_text.len(), 22, "{}", p.qr_text);
        let (f, tlv_bytes) = fields(&p.qr_text);
        assert_eq!(tlv_bytes, 0, "no optional TLV data");
        assert_eq!(f.version, 0);
        assert_eq!(f.flow, 0, "standard commissioning flow");
        assert_eq!((f.vid, f.pid), (0xFFF1, 0x8001));
        assert_eq!(f.discriminator, 3840);
        assert_eq!(f.passcode, 20202021);
    }

    #[test]
    fn the_manual_code_and_the_qr_code_agree_on_the_passcode() {
        let p = Pairing::standard().unwrap();
        // The test commissioning data every deployed firmware has always used.
        assert_eq!(p.manual_code, "3497-011-2332");
    }

    #[test]
    fn the_symbol_is_small_enough_to_scan_off_a_screen() {
        let m = Modules::of(&Pairing::standard().unwrap().qr_text).unwrap();
        assert!(m.width <= 25, "version-2 symbol, not {}x{}", m.width, m.width);
    }

    /// Reads the art back into modules, the way a scanner's eye would.
    fn parse_terminal(art: &str) -> Vec<Vec<(u8, u8)>> {
        art.lines()
            .map(|line| {
                line.trim_end_matches("\x1b[0m")
                    .split('\u{2580}')
                    .filter(|cell| !cell.is_empty())
                    .map(|cell| {
                        let p: Vec<&str> = cell.trim_end_matches('m').split(';').collect();
                        assert_eq!((p[0], p[1], p[3], p[4]), ("\x1b[38", "5", "48", "5"), "{cell:?}");
                        (p[2].parse().unwrap(), p[5].parse().unwrap())
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_terminal_art_is_the_symbol_with_a_white_quiet_zone() {
        let m = Modules::of(&Pairing::standard().unwrap().qr_text).unwrap();
        let rows = parse_terminal(&render_terminal(&m));
        let edge = m.width as i32 + QUIET_ZONE;
        assert_eq!(rows.len(), ((m.width as i32 + 2 * QUIET_ZONE + 1) / 2) as usize);
        let mut y = -QUIET_ZONE;
        for row in &rows {
            assert_eq!(row.len(), (m.width as i32 + 2 * QUIET_ZONE) as usize, "every row is the same width");
            for (i, &(upper, lower)) in row.iter().enumerate() {
                let x = i as i32 - QUIET_ZONE;
                let shade = |dark| if dark { BLACK } else { WHITE };
                assert_eq!((upper, lower), (shade(m.dark(x, y)), shade(m.dark(x, y + 1))), "cell {x},{y}");
            }
            y += 2;
        }
        assert!(y >= edge, "the whole symbol and its bottom margin were drawn");
        // The margin is white on every side, never the terminal's own background.
        assert!(rows[0].iter().all(|&c| c == (WHITE, WHITE)), "top margin");
        assert!(rows.iter().all(|r| r[0] == (WHITE, WHITE) && r[r.len() - 1] == (WHITE, WHITE)), "side margins");
    }

    #[test]
    fn the_terminal_art_uses_only_fixed_black_and_white() {
        let art = terminal_qr(&Pairing::standard().unwrap().qr_text).unwrap();
        for (upper, lower) in parse_terminal(&art).into_iter().flatten() {
            assert!([BLACK, WHITE].contains(&upper) && [BLACK, WHITE].contains(&lower));
        }
    }

    #[test]
    fn the_operator_report_has_the_codes_and_the_art_and_no_log_prefix() {
        let p = Pairing::standard().unwrap();
        let report = operator_report(&p).unwrap();
        assert!(report.contains(&p.manual_code) && report.contains(&p.qr_text));
        assert!(report.contains('\u{2580}'));
        assert!(!report.contains("INFO"), "the report is for a terminal, not the log");
    }
}
