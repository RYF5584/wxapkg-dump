// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! The authenticated session one account carries.
//!
//! This is the credential set the iLink handshake issues after a confirmed scan. Binary
//! fields are stored as lowercase hex so the file stays inspectable and diffable; they are
//! decoded only where the protocol needs bytes. The shape deliberately matches what the
//! legacy shared library stored, so an existing session imports unchanged.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Server static ECDH public key every fresh login starts from (uncompressed P-256 point).
pub const SVR_STATIC_PUB_KEY: &str = "04ef87876d6478b15f1796eab12068610541173b7176b67f1dcc86683e901acd44d18b4ac36938251d0812dd0cf842aa2d6cbb8115712d1c0087dcefc14a44cd58";

/// Default iLink host.
pub const DEFAULT_HOST: &str = "shortcloud.weixin.com";

/// App id the iLink service knows this client by.
pub const ILINK_APP_ID: &str = "ilinkapp_060000b7b93f6c";

/// Client version the auth header carries.
pub const ILINK_CLIENT_VERSION: u32 = 0x2800_1032;

/// Session type requested at ManualAuth.
pub const SESSION_TYPE: u32 = 5;

/// Token set the OAuth exchange issues, kept to re-establish a session without scanning.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UalCredentials {
    /// Bearer token for the access API.
    #[serde(default)]
    pub access_token: String,
    /// Stable identity at the access API.
    #[serde(default)]
    pub open_id: String,
    /// Long-lived secret that mints new access tokens.
    #[serde(default)]
    pub refresh_token: String,
    /// Login channel, always `WX`.
    #[serde(default)]
    pub login_type: String,
    /// Granted scopes.
    #[serde(default)]
    pub scope: String,
    /// Access-token lifetime in seconds, when reported.
    #[serde(default)]
    pub expires_in: i64,
    /// Cookies the access service issued; every signed call sends them back.
    #[serde(default)]
    pub cookies: Vec<(String, String)>,
}

/// The iLink session of one signed-in account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Account number as the iLink service reports it.
    pub uin: u64,
    /// App id the session belongs to.
    #[serde(default)]
    pub ilink_app_id: String,
    /// Key that seals runtime-transfer bodies (hex).
    #[serde(default)]
    pub app_session_key: String,
    /// Key that unwraps renewed sessions (hex).
    #[serde(default)]
    pub auto_auth_enc_key: String,
    /// Long-lived renewal credential (hex).
    #[serde(default)]
    pub auto_auth_key: String,
    /// Transport session key (hex).
    #[serde(default)]
    pub session_key: String,
    /// Device fingerprint this session is bound to (hex).
    #[serde(default)]
    pub device_id: String,
    /// Server-side routing id, echoed in every packet (hex).
    #[serde(default)]
    pub server_id: String,
    /// Client version the header carries.
    #[serde(default)]
    pub client_ver: u32,
    /// iLink entry host.
    #[serde(default)]
    pub host: String,
    /// Server static ECDH public key the next renewal needs (hex).
    #[serde(default)]
    pub svr_static_pub_key: String,
    /// Seconds the session stays valid, when reported.
    #[serde(default)]
    pub expire_time: u32,
    /// Unix seconds of the last successful use.
    #[serde(default)]
    pub update_time: i64,
    /// OAuth tokens, kept so the account can be refreshed without a new scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ual: Option<UalCredentials>,
    /// Base64 buffer the confirmed scan produced; reusable for one ManualAuth.
    #[serde(default)]
    pub login_buffer: String,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            uin: 0,
            ilink_app_id: ILINK_APP_ID.to_owned(),
            app_session_key: String::new(),
            auto_auth_enc_key: String::new(),
            auto_auth_key: String::new(),
            session_key: String::new(),
            device_id: String::new(),
            server_id: String::new(),
            client_ver: ILINK_CLIENT_VERSION,
            host: DEFAULT_HOST.to_owned(),
            svr_static_pub_key: SVR_STATIC_PUB_KEY.to_owned(),
            expire_time: 0,
            update_time: 0,
            ual: None,
            login_buffer: String::new(),
        }
    }
}

impl Session {
    /// Whether the session has the minimum a runtime transfer needs.
    pub fn is_usable(&self) -> bool {
        self.uin != 0 && !self.app_session_key.is_empty() && !self.device_id.is_empty()
    }

    /// The iLink host, falling back to the default.
    pub fn host(&self) -> &str {
        if self.host.is_empty() {
            DEFAULT_HOST
        } else {
            &self.host
        }
    }

    /// Decodes a stored hex field; an empty field yields empty bytes.
    pub fn hex(&self, field: &str) -> Result<Vec<u8>> {
        hex_decode(field)
    }
}

/// Decodes a hex string, tolerating an `0x` prefix and uppercase.
pub fn hex_decode(text: &str) -> Result<Vec<u8>> {
    let trimmed = text
        .strip_prefix("0x")
        .unwrap_or(text)
        .trim()
        .to_ascii_lowercase();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if !trimmed.len().is_multiple_of(2) || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Protocol(format!(
            "a stored credential is not valid hex: {}…",
            &text[..text.len().min(16)]
        )));
    }
    (0..trimmed.len() / 2)
        .map(|i| u8::from_str_radix(&trimmed[i * 2..i * 2 + 2], 16))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|err| Error::Protocol(format!("a stored credential is not valid hex: {err}")))
}

/// Encodes bytes as lowercase hex.
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Derives the deterministic device id a login id maps to, so re-signing the same account
/// keeps the same fingerprint the server already knows.
pub fn device_id_from_key(device_key: &str) -> Vec<u8> {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"yyb-device-v1:");
    hasher.update(device_key.as_bytes());
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_rejects_noise() {
        let bytes = [0xde, 0xad, 0xbe, 0xef];
        let text = hex_encode(&bytes);
        assert_eq!(text, "deadbeef");
        assert_eq!(hex_decode(&text).unwrap(), bytes);
        assert_eq!(hex_decode("0xDEAD").unwrap(), vec![0xde, 0xad]);
        assert!(hex_decode("xyz").is_err());
        assert!(hex_decode("").unwrap().is_empty());
    }

    #[test]
    fn a_session_needs_identity_and_keys() {
        let mut session = Session::default();
        assert!(!session.is_usable());
        session.uin = 7;
        session.app_session_key = "ab".to_owned();
        assert!(!session.is_usable(), "still missing the device id");
        session.device_id = "cd".to_owned();
        assert!(session.is_usable());
        assert_eq!(session.host(), DEFAULT_HOST);
    }

    #[test]
    fn the_same_device_key_yields_the_same_device() {
        let first = device_id_from_key("dandantoy");
        let second = device_id_from_key("dandantoy");
        assert_eq!(first, second);
        assert_eq!(first.len(), 32);
        assert_ne!(first, device_id_from_key("other"));
    }
}
