// src/snmp/ber.rs
//
// Minimal ASN.1 BER encode/decode — just the subset SNMPv2c messages
// actually use (INTEGER, OCTET STRING, NULL, OBJECT IDENTIFIER, SEQUENCE,
// plus the application-tagged Counter32/Gauge32/TimeTicks and the
// context-tagged PDU/exception types). Hand-rolled rather than a generic
// ASN.1 crate: SNMP's own subset is small and fixed, and a general-purpose
// decoder would accept encodings SNMP itself never produces.
//
// Verified against real `snmpget`/`snmpwalk` (net-snmp), not just
// round-tripped against this module's own encoder/decoder — see
// src/snmp/mod.rs's tests for the OID table integration; the encoding
// primitives here are unit-tested for exact byte output against known-good
// BER encodings.

pub const TAG_INTEGER: u8 = 0x02;
pub const TAG_OCTET_STRING: u8 = 0x04;
// Only used from #[cfg(test)] request-builder helpers (this file's own
// tests and snmp/mod.rs's) -- a real client sends NULL placeholder values
// in Get/GetNext requests, which this agent never needs to construct
// outside of building a fake request to test against.
#[allow(dead_code)]
pub const TAG_NULL: u8 = 0x05;
pub const TAG_OID: u8 = 0x06;
pub const TAG_SEQUENCE: u8 = 0x30;

pub const TAG_COUNTER32: u8 = 0x41;
pub const TAG_GAUGE32: u8 = 0x42;
pub const TAG_TIME_TICKS: u8 = 0x43;

pub const TAG_NO_SUCH_OBJECT: u8 = 0x80;
pub const TAG_END_OF_MIB_VIEW: u8 = 0x82;

pub const PDU_GET_REQUEST: u8 = 0xA0;
pub const PDU_GET_NEXT_REQUEST: u8 = 0xA1;
pub const PDU_GET_RESPONSE: u8 = 0xA2;
pub const PDU_SET_REQUEST: u8 = 0xA3;
pub const PDU_TRAP_V2: u8 = 0xA7;

/// One decoded TLV: `tag`, and `content` is exactly the value bytes
/// (length already consumed) — never the remaining buffer.
pub struct Tlv<'a> {
    pub tag: u8,
    pub content: &'a [u8],
}

/// Wraps `content` in a tag+length header. Definite-length only (short form
/// under 128 bytes, long form otherwise) — SNMP BER never uses indefinite
/// length.
pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    encode_length(content.len(), &mut out);
    out.extend_from_slice(content);
    out
}

fn encode_length(len: usize, out: &mut Vec<u8>) {
    if len < 128 {
        out.push(len as u8);
        return;
    }
    let bytes = len.to_be_bytes();
    let first_nonzero = bytes
        .iter()
        .position(|&b| b != 0)
        .unwrap_or(bytes.len() - 1);
    let significant = &bytes[first_nonzero..];
    out.push(0x80 | significant.len() as u8);
    out.extend_from_slice(significant);
}

/// Reads one TLV off the front of `data`, returning it plus whatever
/// follows. `None` on truncated/malformed input — every SNMP request from
/// the network goes through this, so it must never panic on garbage.
pub fn read_tlv(data: &[u8]) -> Option<(Tlv<'_>, &[u8])> {
    let tag = *data.first()?;
    let len_byte = *data.get(1)?;
    let (len, header_len): (usize, usize) = if len_byte & 0x80 == 0 {
        (usize::from(len_byte), 2)
    } else {
        let n = usize::from(len_byte & 0x7F);
        if n == 0 || n > 8 {
            return None; // indefinite-length or absurd — not valid SNMP BER
        }
        let len_bytes = data.get(2..2 + n)?;
        let mut len: usize = 0;
        for &b in len_bytes {
            len = (len << 8) | usize::from(b);
        }
        (len, 2 + n)
    };
    let content = data.get(header_len..header_len + len)?;
    let rest = data.get(header_len + len..)?;
    Some((Tlv { tag, content }, rest))
}

pub fn encode_integer(value: i64) -> Vec<u8> {
    tlv(TAG_INTEGER, &minimal_signed_bytes(value))
}

pub fn decode_integer(content: &[u8]) -> Option<i64> {
    if content.is_empty() {
        return None;
    }
    let mut value: i64 = if content[0] & 0x80 != 0 { -1 } else { 0 };
    for &b in content {
        value = (value << 8) | i64::from(b);
    }
    Some(value)
}

/// BER INTEGER content: two's-complement, big-endian, minimal length (no
/// redundant leading 0x00/0xFF bytes beyond what's needed to keep the sign
/// bit correct).
fn minimal_signed_bytes(value: i64) -> Vec<u8> {
    let mut bytes = value.to_be_bytes().to_vec();
    while bytes.len() > 1 {
        let keep_dropping = (bytes[0] == 0x00 && bytes[1] & 0x80 == 0)
            || (bytes[0] == 0xFF && bytes[1] & 0x80 != 0);
        if !keep_dropping {
            break;
        }
        bytes.remove(0);
    }
    bytes
}

/// Counter32/Gauge32/TimeTicks: BER-encoded like INTEGER (so a leading
/// 0x00 is inserted when the top bit of the minimal u32 representation
/// would otherwise look like a negative number), but the abstract value is
/// always non-negative — `tag` picks which application type.
pub fn encode_unsigned32(tag: u8, value: u32) -> Vec<u8> {
    let mut bytes = value.to_be_bytes().to_vec();
    while bytes.len() > 1 && bytes[0] == 0x00 && bytes[1] & 0x80 == 0 {
        bytes.remove(0);
    }
    if bytes[0] & 0x80 != 0 {
        bytes.insert(0, 0x00);
    }
    tlv(tag, &bytes)
}

// This agent only ever encodes Counter32/Gauge32/TimeTicks (in responses);
// it never needs to decode one back out of a request. Kept for its own
// round-trip unit tests, which is the only real caller.
#[allow(dead_code)]
pub fn decode_unsigned32(content: &[u8]) -> Option<u32> {
    if content.is_empty() || content.len() > 5 {
        return None;
    }
    let mut value: u32 = 0;
    for &b in content {
        value = (value << 8) | u32::from(b);
    }
    Some(value)
}

pub fn encode_octet_string(value: &[u8]) -> Vec<u8> {
    tlv(TAG_OCTET_STRING, value)
}

// Only used from #[cfg(test)] request-builder helpers -- see TAG_NULL.
#[allow(dead_code)]
pub fn encode_null() -> Vec<u8> {
    tlv(TAG_NULL, &[])
}

pub fn encode_sequence(elements: &[Vec<u8>]) -> Vec<u8> {
    let mut content = Vec::new();
    for e in elements {
        content.extend_from_slice(e);
    }
    tlv(TAG_SEQUENCE, &content)
}

/// `oid` must be at least 2 sub-identifiers, per X.690 (the first two are
/// always combined into a single first byte).
pub fn encode_oid(oid: &[u32]) -> Vec<u8> {
    debug_assert!(oid.len() >= 2, "OID needs at least 2 sub-identifiers");
    let mut body = Vec::new();
    encode_base128(oid[0] * 40 + oid[1], &mut body);
    for &sub in &oid[2..] {
        encode_base128(sub, &mut body);
    }
    tlv(TAG_OID, &body)
}

pub fn decode_oid(mut body: &[u8]) -> Option<Vec<u32>> {
    let (first, rest) = decode_base128(body)?;
    body = rest;
    let (x, y) = if first < 80 {
        (first / 40, first % 40)
    } else {
        (2, first - 80)
    };
    let mut oid = vec![x, y];
    while !body.is_empty() {
        let (v, rest) = decode_base128(body)?;
        oid.push(v);
        body = rest;
    }
    Some(oid)
}

fn encode_base128(value: u32, out: &mut Vec<u8>) {
    let mut groups = vec![(value & 0x7F) as u8];
    let mut remaining = value >> 7;
    while remaining > 0 {
        groups.push(((remaining & 0x7F) as u8) | 0x80);
        remaining >>= 7;
    }
    groups.reverse();
    out.extend_from_slice(&groups);
}

fn decode_base128(data: &[u8]) -> Option<(u32, &[u8])> {
    let mut value: u32 = 0;
    let mut i = 0;
    loop {
        let byte = *data.get(i)?;
        value = value.checked_shl(7)?.checked_add(u32::from(byte & 0x7F))?;
        i += 1;
        if byte & 0x80 == 0 {
            break;
        }
    }
    Some((value, &data[i..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_encoding_matches_known_ber_bytes() {
        // Widely-cited reference values for BER INTEGER minimal encoding.
        assert_eq!(encode_integer(0), vec![0x02, 0x01, 0x00]);
        assert_eq!(encode_integer(127), vec![0x02, 0x01, 0x7F]);
        // 128 needs a leading 0x00 -- 0x80 alone would read as -128.
        assert_eq!(encode_integer(128), vec![0x02, 0x02, 0x00, 0x80]);
        assert_eq!(encode_integer(-1), vec![0x02, 0x01, 0xFF]);
        assert_eq!(encode_integer(-128), vec![0x02, 0x01, 0x80]);
    }

    #[test]
    fn integer_round_trips_through_decode() {
        for v in [
            0i64,
            1,
            -1,
            127,
            128,
            -128,
            -129,
            65535,
            -65536,
            i64::from(i32::MAX),
        ] {
            let encoded = encode_integer(v);
            let (parsed, rest) = read_tlv(&encoded).unwrap();
            assert!(rest.is_empty());
            assert_eq!(decode_integer(parsed.content), Some(v));
        }
    }

    #[test]
    fn unsigned32_inserts_leading_zero_when_top_bit_set() {
        // 0xFFFFFFFF must not be encodable as a bare 4-byte 0xFF.. (that
        // would decode as a negative INTEGER) -- BER requires the padding
        // byte for any application type built on INTEGER's rules.
        let encoded = encode_unsigned32(TAG_COUNTER32, 0xFFFF_FFFF);
        assert_eq!(
            encoded,
            vec![TAG_COUNTER32, 0x05, 0x00, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        let (parsed, _) = read_tlv(&encoded).unwrap();
        assert_eq!(decode_unsigned32(parsed.content), Some(0xFFFF_FFFF));
    }

    #[test]
    fn unsigned32_round_trips() {
        for v in [0u32, 1, 127, 128, 255, 256, u32::from(u16::MAX), u32::MAX] {
            let encoded = encode_unsigned32(TAG_GAUGE32, v);
            let (parsed, _) = read_tlv(&encoded).unwrap();
            assert_eq!(decode_unsigned32(parsed.content), Some(v));
        }
    }

    #[test]
    fn oid_encoding_matches_known_ber_bytes() {
        // sysDescr.0 = 1.3.6.1.2.1.1.1.0 -- a textbook example, easy to
        // cross-check against any BER reference.
        let oid = [1, 3, 6, 1, 2, 1, 1, 1, 0];
        let encoded = encode_oid(&oid);
        assert_eq!(
            encoded,
            vec![0x06, 0x08, 0x2B, 0x06, 0x01, 0x02, 0x01, 0x01, 0x01, 0x00]
        );
    }

    #[test]
    fn oid_round_trips_including_large_subidentifiers() {
        for oid in [
            vec![1, 3, 6, 1, 2, 1, 1, 1, 0],
            vec![1, 3, 6, 1, 4, 1, 99999, 1, 1, 0],
            vec![2, 999],
            vec![0, 0],
        ] {
            let encoded = encode_oid(&oid);
            let (parsed, rest) = read_tlv(&encoded).unwrap();
            assert!(rest.is_empty());
            assert_eq!(decode_oid(parsed.content), Some(oid));
        }
    }

    #[test]
    fn read_tlv_handles_long_form_length() {
        let content = vec![0xAB; 200];
        let encoded = tlv(TAG_OCTET_STRING, &content);
        // 200 >= 128, so: tag, 0x81 (1 length byte follows), 200, ...200 bytes
        assert_eq!(&encoded[..3], &[TAG_OCTET_STRING, 0x81, 200]);
        let (parsed, rest) = read_tlv(&encoded).unwrap();
        assert!(rest.is_empty());
        assert_eq!(parsed.content, content.as_slice());
    }

    #[test]
    fn read_tlv_rejects_truncated_input_without_panicking() {
        assert!(read_tlv(&[]).is_none());
        assert!(read_tlv(&[TAG_INTEGER]).is_none());
        assert!(read_tlv(&[TAG_INTEGER, 0x05, 0x01, 0x02]).is_none()); // claims 5, has 2
    }
}
