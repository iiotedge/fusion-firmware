// src/matter/attestation.rs
//
// Device Attestation: how a controller convinces itself that this node is the
// product it says it is. While a node is being added, the controller asks it for
// three things and checks them against what the node reports about itself:
//
//   DAC  the node's own certificate (one per device), its private key stays in the node
//   PAI  the intermediate certificate that issued the DAC (one per product line)
//   CD   a Certification Declaration signed by the CSA: "vendor V's products P are certified"
//
// and the vendor id / product id that appear in the DAC, the PAI and the CD must be
// the ones in BasicInformation. When the chain ends at a root the controller trusts
// (CSA's list of approved roots) the accessory is "certified"; with anything else,
// such as rs-matter's TEST credentials, Apple Home adds its "has not been certified
// to work with HomeKit" notice and Google/Alexa want developer settings.
//
// `[matter].attestation = "test"` (default) keeps using rs-matter's test set, vendor
// 0xFFF1 / product 0x8001. `"files"` reads a real set from four files. A mismatched
// set - the key of another device, a CD for another product, a PAI that did not issue
// the DAC - makes commissioning fail with a controller's unhelpful "unable to add",
// so everything checkable offline is checked here, at boot and in `--check-config`,
// and reported in plain words. What cannot be checked offline is the CD's signature
// and whether the PAI's root is one the controller trusts: that is the certification
// process's job, not this module's.

use std::path::Path;

use rs_matter::crypto::{
    default_crypto, CanonPkcPublicKey, CanonPkcPublicKeyRef, CanonPkcSecretKeyRef, Crypto,
    CryptoSensitiveRef, PublicKey, SigningSecretKey,
};
use rs_matter::dm::clusters::dev_att::DeviceAttestation;
use rs_matter::dm::devices::test::TEST_DEV_ATT;
use rs_matter::tlv::TLVElement;
use rs_matter::utils::sync::DynBase;

use crate::config::{MatterConfig, MATTER_TEST_PRODUCT_ID, MATTER_TEST_VENDOR_ID};

/// The most bytes the Matter spec allows each item (6.2.2): a longer one cannot be sent to a controller.
const MAX_DAC_BYTES: usize = 600;
const MAX_PAI_BYTES: usize = 600;
const MAX_CD_BYTES: usize = 541;

/// What the node presents to a controller during commissioning.
pub(crate) enum Attestation {
    /// rs-matter's test set (vendor 0xFFF1, product 0x8001).
    Test,
    /// A real set loaded from files and verified.
    Files(Box<Files>),
}

pub(crate) struct Files {
    cd: Vec<u8>,
    pai: Vec<u8>,
    dac: Vec<u8>,
    dac_public_key: [u8; 65],
    dac_private_key: [u8; 32],
}

impl DynBase for Attestation {}

impl DeviceAttestation for Attestation {
    fn cert_declaration(&self) -> &[u8] {
        match self {
            Self::Test => TEST_DEV_ATT.cert_declaration(),
            Self::Files(f) => &f.cd,
        }
    }

    fn pai(&self) -> &[u8] {
        match self {
            Self::Test => TEST_DEV_ATT.pai(),
            Self::Files(f) => &f.pai,
        }
    }

    fn dac(&self) -> &[u8] {
        match self {
            Self::Test => TEST_DEV_ATT.dac(),
            Self::Files(f) => &f.dac,
        }
    }

    fn dac_pub_key(&self) -> CanonPkcPublicKeyRef<'_> {
        match self {
            Self::Test => TEST_DEV_ATT.dac_pub_key(),
            Self::Files(f) => CanonPkcPublicKeyRef::new(&f.dac_public_key),
        }
    }

    fn dac_priv_key(&self) -> CanonPkcSecretKeyRef<'_> {
        match self {
            Self::Test => TEST_DEV_ATT.dac_priv_key(),
            Self::Files(f) => CanonPkcSecretKeyRef::new(&f.dac_private_key),
        }
    }
}

/// What `load` found, for the log and for `--check-config`.
#[derive(Debug)]
pub struct Summary {
    pub mode: &'static str,
    pub vendor_id: u16,
    pub product_id: u16,
    /// Things that are legal but worth fixing (a world-readable private key).
    pub warnings: Vec<String>,
}

impl Summary {
    /// One line, e.g. `attestation=files vid=0x1234 pid=0x00A1`.
    pub fn line(&self) -> String {
        format!(
            "attestation={} vid=0x{:04X} pid=0x{:04X}",
            self.mode, self.vendor_id, self.product_id
        )
    }
}

pub(crate) struct Loaded {
    pub attestation: Attestation,
    pub summary: Summary,
}

// ---- DER, just enough --------------------------------------------------------------------

/// One DER element: its tag, its content, and the whole encoding (header included -
/// what a signature over a structure covers).
#[derive(Clone, Copy)]
struct Der<'a> {
    tag: u8,
    content: &'a [u8],
    whole: &'a [u8],
}

fn der_read(data: &[u8]) -> Result<(Der<'_>, &[u8]), String> {
    let (&tag, rest) = data.split_first().ok_or("truncated")?;
    let (&first, rest) = rest.split_first().ok_or("truncated")?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7F);
        if n == 0 || n > 3 || rest.len() < n {
            return Err("an unsupported length encoding".to_string());
        }
        (
            rest[..n]
                .iter()
                .fold(0usize, |acc, b| (acc << 8) | usize::from(*b)),
            &rest[n..],
        )
    };
    if rest.len() < len {
        return Err("truncated".to_string());
    }
    let header = data.len() - rest.len();
    Ok((
        Der {
            tag,
            content: &rest[..len],
            whole: &data[..header + len],
        },
        &rest[len..],
    ))
}

fn der_children(mut content: &[u8]) -> Result<Vec<Der<'_>>, String> {
    let mut out = Vec::new();
    while !content.is_empty() {
        let (item, rest) = der_read(content)?;
        out.push(item);
        content = rest;
    }
    Ok(out)
}

/// `data` is exactly one element with `tag`.
fn der_single<'a>(data: &'a [u8], tag: u8, what: &str) -> Result<Vec<Der<'a>>, String> {
    let (top, rest) = der_read(data).map_err(|e| format!("{what}: {e}"))?;
    if top.tag != tag || !rest.is_empty() {
        return Err(format!(
            "{what} is not a single DER structure of the expected kind"
        ));
    }
    der_children(top.content).map_err(|e| format!("{what}: {e}"))
}

// ---- X.509, just enough --------------------------------------------------------------------

/// Matter's own attribute types in a certificate subject: 1.3.6.1.4.1.37244.2.1 / .2.2
/// carry the vendor id and product id as four hex digits.
const OID_MATTER_VENDOR_ID: [u8; 10] = [0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x02, 0x01];
const OID_MATTER_PRODUCT_ID: [u8; 10] =
    [0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x02, 0x02];

struct Cert<'a> {
    /// The complete TBSCertificate encoding: what the issuer's signature covers.
    tbs: &'a [u8],
    issuer: &'a [u8],
    subject: &'a [u8],
    public_key: [u8; 65],
    /// ECDSA r || s.
    signature: [u8; 64],
    vendor_id: Option<u16>,
    product_id: Option<u16>,
}

fn parse_cert<'a>(der: &'a [u8], what: &str) -> Result<Cert<'a>, String> {
    let ctx = |e: String| format!("{what}: {e}");
    let parts = der_single(der, 0x30, what)?;
    if parts.len() != 3 || parts[0].tag != 0x30 || parts[1].tag != 0x30 || parts[2].tag != 0x03 {
        return Err(format!("{what} is not an X.509 certificate (expected tbsCertificate, signatureAlgorithm, signature)"));
    }
    let fields = der_children(parts[0].content).map_err(ctx)?;
    // [0] version is optional; then serial, signature algorithm, issuer, validity, subject, public key.
    let base = usize::from(fields.first().is_some_and(|f| f.tag == 0xA0));
    if fields.len() < base + 6 {
        return Err(format!(
            "{what} is not an X.509 certificate (too few fields)"
        ));
    }
    let (issuer, subject, spki) = (fields[base + 2], fields[base + 4], fields[base + 5]);

    let spki_parts = der_children(spki.content).map_err(|e| format!("{what}: {e}"))?;
    let key_bits = spki_parts
        .get(1)
        .filter(|b| b.tag == 0x03)
        .ok_or_else(|| format!("{what} has no public key"))?;
    if key_bits.content.len() != 66 || key_bits.content[0] != 0 || key_bits.content[1] != 0x04 {
        return Err(format!(
            "{what}'s public key is not an uncompressed P-256 point"
        ));
    }
    let mut public_key = [0u8; 65];
    public_key.copy_from_slice(&key_bits.content[1..]);

    if parts[2].content.first() != Some(&0) {
        return Err(format!("{what}'s signature has unused bits"));
    }
    let signature =
        raw_signature(&parts[2].content[1..]).map_err(|e| format!("{what}'s signature: {e}"))?;

    let (vendor_id, product_id) =
        matter_ids(subject.content).map_err(|e| format!("{what}'s subject: {e}"))?;
    Ok(Cert {
        tbs: parts[0].whole,
        issuer: issuer.whole,
        subject: subject.whole,
        public_key,
        signature,
        vendor_id,
        product_id,
    })
}

/// The vendor and product id in a certificate subject (each optional).
fn matter_ids(name: &[u8]) -> Result<(Option<u16>, Option<u16>), String> {
    let (mut vendor, mut product) = (None, None);
    for rdn in der_children(name)? {
        for attribute in der_children(rdn.content)? {
            let parts = der_children(attribute.content)?;
            let (Some(oid), Some(value)) = (parts.first(), parts.get(1)) else {
                continue;
            };
            let slot = if oid.tag == 0x06 && oid.content == OID_MATTER_VENDOR_ID {
                &mut vendor
            } else if oid.tag == 0x06 && oid.content == OID_MATTER_PRODUCT_ID {
                &mut product
            } else {
                continue;
            };
            let text = std::str::from_utf8(value.content)
                .map_err(|_| "a vendor/product id that is not text")?;
            *slot = Some(
                u16::from_str_radix(text, 16)
                    .map_err(|_| format!("'{text}' is not a four-digit hex id"))?,
            );
        }
    }
    Ok((vendor, product))
}

/// A DER ECDSA-Sig-Value as the raw 64 bytes r || s.
fn raw_signature(der: &[u8]) -> Result<[u8; 64], String> {
    let ints = der_single(der, 0x30, "the signature")?;
    if ints.len() != 2 || ints.iter().any(|i| i.tag != 0x02) {
        return Err("is not two integers".to_string());
    }
    let mut out = [0u8; 64];
    for (i, int) in ints.iter().enumerate() {
        let mut v = int.content;
        while v.len() > 32 && v[0] == 0 {
            v = &v[1..];
        }
        if v.len() > 32 {
            return Err("has an integer wider than 256 bits".to_string());
        }
        out[i * 32 + (32 - v.len())..(i + 1) * 32].copy_from_slice(v);
    }
    Ok(out)
}

// ---- the private key -------------------------------------------------------------------------

fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text
        .bytes()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
    {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(format!("'{}' is not base64", c as char)),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 0x3F) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The 32-byte P-256 private scalar from what a tool or a factory produced: PEM or
/// DER, SEC1 (`EC PRIVATE KEY`) or PKCS#8 (`PRIVATE KEY`), or the bare scalar.
fn parse_private_key(bytes: &[u8]) -> Result<[u8; 32], String> {
    let text = std::str::from_utf8(bytes).ok().map(str::trim);
    let der: Vec<u8> = match text {
        Some(t) if t.starts_with("-----BEGIN") => {
            if t.contains("ENCRYPTED") {
                return Err(
                    "is an encrypted key; decrypt it first (the service has no passphrase to give)"
                        .to_string(),
                );
            }
            let body: String = t.lines().filter(|l| !l.starts_with("-----")).collect();
            base64_decode(&body)?
        }
        _ => bytes.to_vec(),
    };
    if der.len() == 32 {
        return Ok(der.as_slice().try_into().expect("32 bytes"));
    }
    let top = der_single(&der, 0x30, "the key")?;
    let version = top.first().filter(|v| v.tag == 0x02).map(|v| v.content);
    let sec1_items: Vec<Der<'_>>;
    let items: &[Der<'_>] = match version {
        // SEC1 ECPrivateKey: SEQUENCE { INTEGER 1, OCTET STRING key, ... }
        Some([1]) => &top,
        // PKCS#8: SEQUENCE { INTEGER 0, AlgorithmIdentifier, OCTET STRING { SEC1 } }
        Some([0]) => {
            let inner = top
                .get(2)
                .filter(|o| o.tag == 0x04)
                .ok_or("is a PKCS#8 key without its private key")?;
            sec1_items = der_single(inner.content, 0x30, "the PKCS#8 key")?;
            &sec1_items
        }
        _ => return Err("is neither a SEC1 nor a PKCS#8 EC private key".to_string()),
    };
    let scalar = items
        .get(1)
        .filter(|o| o.tag == 0x04)
        .ok_or("has no private scalar")?;
    scalar.content.try_into().map_err(|_| {
        format!(
            "holds a {}-byte scalar, a P-256 key has 32",
            scalar.content.len()
        )
    })
}

// ---- the Certification Declaration -------------------------------------------------------------

/// The vendor id and the product ids a Certification Declaration (a CMS SignedData
/// whose payload is a small TLV structure) declares as certified.
fn cd_ids(der: &[u8]) -> Result<(u16, Vec<u16>), String> {
    let content_info = der_single(der, 0x30, "the file")?;
    let signed = content_info
        .get(1)
        .filter(|c| c.tag == 0xA0)
        .ok_or("it is not a CMS SignedData")?;
    let signed_data = der_single(signed.content, 0x30, "its SignedData")?;
    let encapsulated = signed_data
        .get(2)
        .filter(|e| e.tag == 0x30)
        .ok_or("it has no signed content")?;
    let parts = der_children(encapsulated.content)?;
    let wrapper = parts
        .get(1)
        .filter(|p| p.tag == 0xA0)
        .ok_or("it has no signed content")?;
    let (payload, _) = der_read(wrapper.content)?;
    if payload.tag != 0x04 {
        return Err("its signed content is not an OCTET STRING".to_string());
    }
    let elements = TLVElement::new(payload.content)
        .structure()
        .map_err(|e| format!("its TLV payload is unreadable: {e:?}"))?;
    let vendor = elements
        .find_ctx(1)
        .and_then(|e| e.u16())
        .map_err(|e| format!("it names no vendor id: {e:?}"))?;
    let mut products = Vec::new();
    for item in elements
        .find_ctx(2)
        .and_then(|e| e.array())
        .map_err(|e| format!("it names no product ids: {e:?}"))?
        .iter()
    {
        products.push(
            item.and_then(|e| e.u16())
                .map_err(|e| format!("it has a bad product id: {e:?}"))?,
        );
    }
    Ok((vendor, products))
}

// ---- loading and checking ------------------------------------------------------------------

fn read(path: &str, key: &str, max: Option<usize>) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("matter.{key} '{path}': {e}"))?;
    if bytes.is_empty() {
        return Err(format!("matter.{key} '{path}' is empty"));
    }
    if let Some(max) = max {
        if bytes.len() > max {
            return Err(format!(
                "matter.{key} '{path}' is {} bytes; the Matter spec allows at most {max}, and a controller cannot be sent more",
                bytes.len()
            ));
        }
    }
    Ok(bytes)
}

/// Load and check what `cfg` says this node presents. Test mode needs no files;
/// `files` mode reads the four of them and verifies everything that can be
/// verified offline (module header). The error says which file and what is wrong.
pub(crate) fn load(cfg: &MatterConfig) -> Result<Loaded, String> {
    if cfg.attestation != "files" {
        return Ok(Loaded {
            attestation: Attestation::Test,
            summary: Summary {
                mode: "test",
                vendor_id: MATTER_TEST_VENDOR_ID,
                product_id: MATTER_TEST_PRODUCT_ID,
                warnings: Vec::new(),
            },
        });
    }

    let dac = read(&cfg.dac_file, "dac_file", Some(MAX_DAC_BYTES))?;
    let pai = read(&cfg.pai_file, "pai_file", Some(MAX_PAI_BYTES))?;
    let cd = read(&cfg.cd_file, "cd_file", Some(MAX_CD_BYTES))?;
    let key_bytes = read(&cfg.dac_key_file, "dac_key_file", None)?;
    let key = parse_private_key(&key_bytes)
        .map_err(|e| format!("matter.dac_key_file '{}' {e}", cfg.dac_key_file))?;

    let dac_cert = parse_cert(&dac, &format!("matter.dac_file '{}'", cfg.dac_file))?;
    let pai_cert = parse_cert(&pai, &format!("matter.pai_file '{}'", cfg.pai_file))?;
    let ids = |v: Option<u16>| v.map_or("none".to_string(), |v| format!("0x{v:04X}"));

    // The ids the controller will compare: the DAC's, the PAI's and the CD's against BasicInformation's.
    if dac_cert.vendor_id != Some(cfg.vendor_id) || dac_cert.product_id != Some(cfg.product_id) {
        return Err(format!(
            "the DAC '{}' is for vendor {} product {}, but matter.vendor_id is 0x{:04X} and matter.product_id is 0x{:04X}; a controller compares them and refuses the node. Set the ids to the DAC's, or use the DAC issued for these ids",
            cfg.dac_file,
            ids(dac_cert.vendor_id),
            ids(dac_cert.product_id),
            cfg.vendor_id,
            cfg.product_id
        ));
    }
    if pai_cert.vendor_id.is_some_and(|v| v != cfg.vendor_id)
        || pai_cert.product_id.is_some_and(|p| p != cfg.product_id)
    {
        return Err(format!(
            "the PAI '{}' is for vendor {} product {}, which does not fit matter.vendor_id 0x{:04X} / matter.product_id 0x{:04X}",
            cfg.pai_file,
            ids(pai_cert.vendor_id),
            ids(pai_cert.product_id),
            cfg.vendor_id,
            cfg.product_id
        ));
    }
    let (cd_vendor, cd_products) = cd_ids(&cd).map_err(|e| {
        format!(
            "matter.cd_file '{}' is not a usable Certification Declaration: {e}",
            cfg.cd_file
        )
    })?;
    if cd_vendor != cfg.vendor_id || !cd_products.contains(&cfg.product_id) {
        return Err(format!(
            "the Certification Declaration '{}' certifies vendor 0x{cd_vendor:04X} products {}, which does not include matter.vendor_id 0x{:04X} / matter.product_id 0x{:04X}",
            cfg.cd_file,
            cd_products.iter().map(|p| format!("0x{p:04X}")).collect::<Vec<_>>().join(", "),
            cfg.vendor_id,
            cfg.product_id
        ));
    }

    // The chain: the PAI is the DAC's issuer, and its key really signed the DAC.
    if dac_cert.issuer != pai_cert.subject {
        return Err(format!(
            "the DAC '{}' was not issued by the PAI '{}' (its issuer is a different name); use the PAI that signed this DAC",
            cfg.dac_file, cfg.pai_file
        ));
    }
    let crypto = default_crypto(rand::rng(), CanonPkcSecretKeyRef::new(&key));
    let pai_key = crypto
        .pub_key(CanonPkcPublicKeyRef::new(&pai_cert.public_key))
        .map_err(|e| format!("the PAI's public key is not usable: {e:?}"))?;
    let signed_by_pai = pai_key
        .verify(dac_cert.tbs, CryptoSensitiveRef::new(&dac_cert.signature))
        .map_err(|e| format!("could not verify the DAC against the PAI: {e:?}"))?;
    if !signed_by_pai {
        return Err(format!(
            "the DAC '{}' carries a signature that the PAI '{}' did not make (a corrupted or substituted file)",
            cfg.dac_file, cfg.pai_file
        ));
    }

    // The key is the DAC's.
    let mut derived = CanonPkcPublicKey::new();
    crypto
        .singleton_singing_secret_key()
        .and_then(|k| k.pub_key())
        .and_then(|p| p.write_canon(&mut derived))
        .map_err(|e| {
            format!(
                "matter.dac_key_file '{}' is not a valid P-256 private key: {e:?}",
                cfg.dac_key_file
            )
        })?;
    if derived.access() != &dac_cert.public_key {
        return Err(format!(
            "the private key '{}' does not belong to the DAC '{}' (its public key is a different one); every device has its own DAC and key",
            cfg.dac_key_file, cfg.dac_file
        ));
    }

    let mut warnings = Vec::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&cfg.dac_key_file) {
            if meta.permissions().mode() & 0o077 != 0 {
                warnings.push(format!(
                    "matter.dac_key_file '{}' is readable by other users (mode {:o}); run chmod 600 on it",
                    cfg.dac_key_file,
                    meta.permissions().mode() & 0o777
                ));
            }
        }
    }

    let dac_public_key = dac_cert.public_key;
    Ok(Loaded {
        attestation: Attestation::Files(Box::new(Files {
            cd,
            pai,
            dac,
            dac_public_key,
            dac_private_key: key,
        })),
        summary: Summary {
            mode: "files",
            vendor_id: cfg.vendor_id,
            product_id: cfg.product_id,
            warnings,
        },
    })
}

/// `--check-config`: what `load` would conclude, without keeping anything.
pub fn check(cfg: &MatterConfig) -> Result<Summary, String> {
    load(cfg).map(|l| l.summary)
}

// ---- the test set as files ------------------------------------------------------------------

/// A SEC1 `ECPrivateKey` for P-256 with its public key, DER.
fn sec1_der(private: &[u8; 32], public: &[u8; 65]) -> Vec<u8> {
    let mut der = vec![0x30, 0x77, 0x02, 0x01, 0x01, 0x04, 0x20];
    der.extend_from_slice(private);
    der.extend_from_slice(&[
        0xA0, 0x0A, 0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07, 0xA1, 0x44, 0x03,
        0x42, 0x00,
    ]);
    der.extend_from_slice(public);
    der
}

fn pem(label: &str, der: &[u8]) -> String {
    let b64 = base64_encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("ascii"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// `--matter-test-attestation DIR`: write rs-matter's TEST attestation set as the
/// four files `attestation = "files"` reads, with the config lines that point at
/// them. For trying the mechanism and for seeing what the files look like - these
/// certificates are public (every build has them), so they prove nothing to a
/// controller and Apple Home still marks the accessory uncertified.
pub fn export_test_files(dir: &Path) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let write = |name: &str, bytes: &[u8]| -> Result<String, String> {
        let path = dir.join(name);
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path.display().to_string())
    };
    let dac = write("dac.der", TEST_DEV_ATT.dac())?;
    let pai = write("pai.der", TEST_DEV_ATT.pai())?;
    let cd = write("cd.der", TEST_DEV_ATT.cert_declaration())?;
    let key = write(
        "dac.key",
        pem(
            "EC PRIVATE KEY",
            &sec1_der(
                TEST_DEV_ATT.dac_priv_key().access(),
                TEST_DEV_ATT.dac_pub_key().access(),
            ),
        )
        .as_bytes(),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{key}: {e}"))?;
    }
    Ok(format!(
        "[matter]\nattestation = \"files\"\nvendor_id = 0x{MATTER_TEST_VENDOR_ID:04X}\nproduct_id = 0x{MATTER_TEST_PRODUCT_ID:04X}\ndac_file = \"{dac}\"\ndac_key_file = \"{key}\"\npai_file = \"{pai}\"\ncd_file = \"{cd}\"\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fusion-attestation-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A config pointing at the exported test set in `dir`.
    fn files_config(dir: &Path) -> MatterConfig {
        export_test_files(dir).unwrap();
        MatterConfig {
            attestation: "files".into(),
            dac_file: dir.join("dac.der").display().to_string(),
            dac_key_file: dir.join("dac.key").display().to_string(),
            pai_file: dir.join("pai.der").display().to_string(),
            cd_file: dir.join("cd.der").display().to_string(),
            ..MatterConfig::default()
        }
    }

    fn test_key() -> [u8; 32] {
        *TEST_DEV_ATT.dac_priv_key().access()
    }

    fn test_pub() -> [u8; 65] {
        *TEST_DEV_ATT.dac_pub_key().access()
    }

    #[test]
    fn the_exported_test_set_loads_and_presents_the_same_material() {
        let dir = scratch("roundtrip");
        let loaded = load(&files_config(&dir)).unwrap();
        assert_eq!(
            loaded.summary.line(),
            "attestation=files vid=0xFFF1 pid=0x8001"
        );
        assert!(
            loaded.summary.warnings.is_empty(),
            "{:?}",
            loaded.summary.warnings
        );
        let a = &loaded.attestation;
        assert_eq!(a.dac(), TEST_DEV_ATT.dac());
        assert_eq!(a.pai(), TEST_DEV_ATT.pai());
        assert_eq!(a.cert_declaration(), TEST_DEV_ATT.cert_declaration());
        assert_eq!(a.dac_priv_key().access(), &test_key());
        assert_eq!(a.dac_pub_key().access(), &test_pub());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_mode_needs_no_files_and_is_vendor_fff1() {
        let loaded = load(&MatterConfig::default()).unwrap();
        assert_eq!(
            loaded.summary.line(),
            "attestation=test vid=0xFFF1 pid=0x8001"
        );
        assert_eq!(loaded.attestation.dac(), TEST_DEV_ATT.dac());
    }

    #[test]
    fn the_certificates_say_what_the_config_expects() {
        let dac = parse_cert(TEST_DEV_ATT.dac(), "dac").unwrap();
        assert_eq!(
            (dac.vendor_id, dac.product_id),
            (Some(0xFFF1), Some(0x8001))
        );
        assert_eq!(dac.public_key, test_pub());
        let pai = parse_cert(TEST_DEV_ATT.pai(), "pai").unwrap();
        assert_eq!(
            (pai.vendor_id, pai.product_id),
            (Some(0xFFF1), None),
            "a PAI may name a vendor only"
        );
        assert_eq!(
            dac.issuer, pai.subject,
            "the test DAC was issued by the test PAI"
        );
        let (vendor, products) = cd_ids(TEST_DEV_ATT.cert_declaration()).unwrap();
        assert_eq!(vendor, 0xFFF1);
        assert!(products.contains(&0x8001), "{products:?}");
    }

    #[test]
    fn every_private_key_format_gives_the_same_scalar() {
        let key = test_key();
        let sec1 = sec1_der(&key, &test_pub());
        let mut pkcs8 = vec![
            0x30, 0x81, 0x87, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE,
            0x3D, 0x02, 0x01, 0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07, 0x04,
            0x6D, 0x30, 0x6B, 0x02, 0x01, 0x01, 0x04, 0x20,
        ];
        pkcs8.extend_from_slice(&key);
        pkcs8.extend_from_slice(&[0xA1, 0x44, 0x03, 0x42, 0x00]);
        pkcs8.extend_from_slice(&test_pub());
        assert_eq!(parse_private_key(&key).unwrap(), key, "the bare scalar");
        assert_eq!(parse_private_key(&sec1).unwrap(), key, "SEC1 DER");
        assert_eq!(parse_private_key(&pkcs8).unwrap(), key, "PKCS#8 DER");
        assert_eq!(
            parse_private_key(pem("EC PRIVATE KEY", &sec1).as_bytes()).unwrap(),
            key,
            "SEC1 PEM"
        );
        assert_eq!(
            parse_private_key(pem("PRIVATE KEY", &pkcs8).as_bytes()).unwrap(),
            key,
            "PKCS#8 PEM"
        );
        let padded = format!("\n  {}  \n", pem("PRIVATE KEY", &pkcs8));
        assert_eq!(
            parse_private_key(padded.as_bytes()).unwrap(),
            key,
            "surrounding whitespace"
        );
    }

    #[test]
    fn a_bad_private_key_file_is_explained() {
        assert!(parse_private_key(
            b"-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----"
        )
        .unwrap_err()
        .contains("encrypted"));
        assert!(parse_private_key(&[1, 2, 3]).is_err());
        assert!(
            parse_private_key(TEST_DEV_ATT.dac())
                .unwrap_err()
                .contains("neither a SEC1 nor a PKCS#8"),
            "a certificate is not a key"
        );
        assert!(
            parse_private_key(
                pem("EC PRIVATE KEY", &sec1_der(&test_key(), &test_pub())[..60]).as_bytes()
            )
            .is_err(),
            "truncated"
        );
    }

    #[test]
    fn base64_round_trips_every_length() {
        for len in 0..40 {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data, "{len}");
        }
        assert!(base64_decode("not base64!").is_err());
    }

    #[test]
    fn another_vendor_or_product_id_is_refused_with_both_sides_named() {
        let dir = scratch("ids");
        let mut cfg = files_config(&dir);
        cfg.vendor_id = 0x1234;
        let err = load(&cfg).err().unwrap();
        assert!(
            err.contains("0xFFF1") && err.contains("0x1234") && err.contains("DAC"),
            "{err}"
        );
        let mut cfg = files_config(&dir);
        cfg.product_id = 0x00A1;
        assert!(load(&cfg).err().unwrap().contains("0x8001"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_that_is_not_the_dacs_is_refused() {
        let dir = scratch("key");
        let cfg = files_config(&dir);
        let mut other = test_key();
        other[31] ^= 0x01;
        std::fs::write(&cfg.dac_key_file, other).unwrap();
        let err = load(&cfg).err().unwrap();
        assert!(err.contains("does not belong to the DAC"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dac_the_pai_did_not_sign_is_refused() {
        let dir = scratch("sig");
        let cfg = files_config(&dir);
        let mut dac = TEST_DEV_ATT.dac().to_vec();
        dac[16] ^= 0x01; // inside the serial number: still valid DER, no longer what the PAI signed
        std::fs::write(&cfg.dac_file, dac).unwrap();
        let err = load(&cfg).err().unwrap();
        assert!(
            err.contains("signature") && err.contains("did not make"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pai_that_is_not_the_issuer_is_refused() {
        let dir = scratch("issuer");
        let cfg = files_config(&dir);
        let mut pai = TEST_DEV_ATT.pai().to_vec();
        let at = pai.windows(7).position(|w| w == b"Dev PAI").unwrap();
        pai[at] = b'X'; // another subject name
        std::fs::write(&cfg.pai_file, pai).unwrap();
        let err = load(&cfg).err().unwrap();
        assert!(err.contains("was not issued by the PAI"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_empty_oversized_and_garbled_files_are_named() {
        let dir = scratch("files");
        let cfg = files_config(&dir);
        let missing = MatterConfig {
            cd_file: dir.join("nope.der").display().to_string(),
            ..cfg.clone()
        };
        let err = load(&missing).err().unwrap();
        assert!(
            err.contains("matter.cd_file") && err.contains("nope.der"),
            "{err}"
        );
        std::fs::write(&cfg.dac_file, b"").unwrap();
        assert!(load(&cfg).err().unwrap().contains("is empty"));
        std::fs::write(&cfg.dac_file, vec![0x30; 601]).unwrap();
        assert!(load(&cfg).err().unwrap().contains("at most 600"));
        std::fs::write(&cfg.dac_file, b"not a certificate at all").unwrap();
        assert!(load(&cfg).err().unwrap().contains("matter.dac_file"));
        std::fs::write(&cfg.dac_file, TEST_DEV_ATT.dac()).unwrap();
        std::fs::write(&cfg.cd_file, TEST_DEV_ATT.pai()).unwrap();
        assert!(
            load(&cfg)
                .err()
                .unwrap()
                .contains("Certification Declaration"),
            "a certificate is not a declaration"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_world_readable_key_is_a_warning_not_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perm");
        let cfg = files_config(&dir);
        std::fs::set_permissions(&cfg.dac_key_file, std::fs::Permissions::from_mode(0o644))
            .unwrap();
        let loaded = load(&cfg).unwrap();
        assert_eq!(loaded.summary.warnings.len(), 1);
        assert!(loaded.summary.warnings[0].contains("chmod 600"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
