// src/security.rs
//
// Access control shared by the network services (F2/F8 hardening):
// a small user store gating RTSP + ONVIF, and the ONVIF WS-UsernameToken
// digest check. Command-channel auth (bearer token) lives with the command
// channel; TLS for it comes from the same edge.toml the telemetry engine uses.
use crate::config::{SecurityConfig, UserConfig};

use base64::Engine;
use sha1::{Digest, Sha1};
use tracing::{info, warn};

/// Compiled view of the user store with the service enable flags.
#[derive(Clone)]
pub struct AccessControl {
    users: Vec<UserConfig>,
    rtsp_auth: bool,
    onvif_auth: bool,
}

impl AccessControl {
    pub fn new(cfg: &SecurityConfig) -> Self {
        // Fail loud, not silent: an "auth on, no users" config would lock
        // everyone out — treat it as misconfiguration and stay open with a
        // warning rather than brick remote access.
        if (cfg.rtsp_auth || cfg.onvif_auth) && cfg.users.is_empty() {
            warn!(
                "security.rtsp_auth/onvif_auth enabled but security.users is empty — \
                 access control disabled (add [[security.users]] to enforce)"
            );
        }
        if !cfg.rtsp_auth {
            warn!("RTSP authentication is OFF — anyone on the network can view the stream");
        }
        if !cfg.onvif_auth {
            warn!("ONVIF authentication is OFF — device services are unauthenticated");
        }
        Self {
            users: cfg.users.clone(),
            rtsp_auth: cfg.rtsp_auth,
            onvif_auth: cfg.onvif_auth,
        }
    }

    pub fn rtsp_enforced(&self) -> bool {
        self.rtsp_auth && !self.users.is_empty()
    }

    pub fn onvif_enforced(&self) -> bool {
        self.onvif_auth && !self.users.is_empty()
    }

    pub fn users(&self) -> &[UserConfig] {
        &self.users
    }

    fn password_for(&self, username: &str) -> Option<&str> {
        self.users
            .iter()
            .find(|u| u.username == username)
            .map(|u| u.password.as_str())
    }

    /// Verify an ONVIF WS-Security UsernameToken.
    ///
    /// PasswordText: password matches directly.
    /// PasswordDigest: Base64(SHA1(nonce_bytes + created + password)) — the
    /// ONVIF-standard digest, where nonce is the Base64-decoded token nonce.
    pub fn verify_ws_token(&self, token: &WsUsernameToken) -> bool {
        let Some(password) = self.password_for(&token.username) else {
            return false;
        };
        match &token.password_kind {
            PasswordKind::Text => token.password_value == password,
            PasswordKind::Digest => {
                let Ok(nonce_bytes) =
                    base64::engine::general_purpose::STANDARD.decode(&token.nonce_b64)
                else {
                    return false;
                };
                let mut hasher = Sha1::new();
                hasher.update(&nonce_bytes);
                hasher.update(token.created.as_bytes());
                hasher.update(password.as_bytes());
                let expected = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
                // Constant-time-ish: lengths equal + byte compare. These are
                // short server-side digests, not a timing oracle worth arming.
                expected == token.password_value
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordKind {
    Text,
    Digest,
}

/// Parsed WS-Security UsernameToken fields from a SOAP header.
#[derive(Debug, Clone)]
pub struct WsUsernameToken {
    pub username: String,
    pub password_value: String,
    pub password_kind: PasswordKind,
    pub nonce_b64: String,
    pub created: String,
}

/// Extract a UsernameToken from a raw SOAP request body (namespace-agnostic:
/// matches on local element names, ignoring prefixes — VMS vendors vary).
pub fn parse_ws_token(body: &str) -> Option<WsUsernameToken> {
    let username = inner_text(body, "Username")?;
    let password_value = inner_text(body, "Password")?;
    let password_kind = if body.contains("#PasswordDigest") {
        PasswordKind::Digest
    } else {
        PasswordKind::Text
    };
    Some(WsUsernameToken {
        username,
        password_value,
        password_kind,
        nonce_b64: inner_text(body, "Nonce").unwrap_or_default(),
        created: inner_text(body, "Created").unwrap_or_default(),
    })
}

/// Text between the first `<...:Name ...>` open tag (any/no prefix) and its
/// matching close. Deliberately tiny — good enough for well-formed ONVIF
/// headers without pulling a full XML parser onto the hot path. Also used
/// by onvif::ptz for PresetToken extraction (same tolerance for "good
/// enough on well-formed ONVIF bodies", not a general XML parser).
pub(crate) fn inner_text(body: &str, local_name: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(rel) = body[search_from..].find(local_name) {
        let name_at = search_from + rel;
        let name_end = name_at + local_name.len();
        // Preceded by '<' or '<prefix:' (element open), and followed by a tag
        // delimiter so "Username" doesn't match inside "UsernameToken".
        let before = body[..name_at].rfind('<');
        let followed_by_delim = body[name_end..]
            .chars()
            .next()
            .map(|c| c == '>' || c == '/' || c.is_whitespace())
            .unwrap_or(false);
        let is_element = before
            .map(|b| {
                let between = &body[b + 1..name_at];
                between.is_empty() || between.ends_with(':')
            })
            .unwrap_or(false);
        if is_element && followed_by_delim {
            let open_end = body[name_at..].find('>')? + name_at;
            let rest = &body[open_end + 1..];
            let close_rel = rest.find("</")?;
            return Some(rest[..close_rel].trim().to_string());
        }
        search_from = name_end;
    }
    None
}

/// Log the effective security posture once at boot.
pub fn log_posture(access: &AccessControl, command_token_set: bool) {
    info!(
        rtsp_auth = access.rtsp_enforced(),
        onvif_auth = access.onvif_enforced(),
        command_auth = command_token_set,
        users = access.users().len(),
        "Security posture"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(users: Vec<(&str, &str)>) -> AccessControl {
        AccessControl {
            users: users
                .into_iter()
                .map(|(u, p)| UserConfig {
                    username: u.into(),
                    password: p.into(),
                    role: "admin".into(),
                })
                .collect(),
            rtsp_auth: true,
            onvif_auth: true,
        }
    }

    #[test]
    fn parses_and_verifies_password_text() {
        let body = r##"<s:Header><Security><UsernameToken>
            <Username>admin</Username>
            <Password Type="#PasswordText">secret</Password>
            </UsernameToken></Security></s:Header>"##;
        let token = parse_ws_token(body).unwrap();
        assert_eq!(token.username, "admin");
        assert_eq!(token.password_kind, PasswordKind::Text);
        assert!(access(vec![("admin", "secret")]).verify_ws_token(&token));
        assert!(!access(vec![("admin", "wrong")]).verify_ws_token(&token));
    }

    #[test]
    fn verifies_password_digest() {
        // Build a valid digest for nonce/created/password, then verify it.
        let password = "s3cr3t";
        let nonce_bytes = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let created = "2026-07-17T00:00:00Z";
        let nonce_b64 = base64::engine::general_purpose::STANDARD.encode(nonce_bytes);
        let mut hasher = Sha1::new();
        hasher.update(nonce_bytes);
        hasher.update(created.as_bytes());
        hasher.update(password.as_bytes());
        let digest = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());

        let body = format!(
            r##"<UsernameToken><Username>op</Username>
               <Password Type="#PasswordDigest">{digest}</Password>
               <Nonce>{nonce_b64}</Nonce><Created>{created}</Created></UsernameToken>"##
        );
        let token = parse_ws_token(&body).unwrap();
        assert_eq!(token.password_kind, PasswordKind::Digest);
        assert!(access(vec![("op", password)]).verify_ws_token(&token));
        // Wrong password → different digest → reject.
        assert!(!access(vec![("op", "nope")]).verify_ws_token(&token));
    }

    #[test]
    fn missing_token_is_none() {
        assert!(parse_ws_token("<s:Body><GetDeviceInformation/></s:Body>").is_none());
    }
}
