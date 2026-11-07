// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! QR login against WeChat's Open Platform, over plain HTTPS.
//!
//! This is the part of remote download that needs no proprietary transport: the QR code is
//! served by `open.weixin.qq.com`, and its state is polled from the long-poll endpoint. The
//! exchange that turns a confirmed scan into a usable session speaks iLink/MMTLS and is
//! described in `docs/remote.md`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::now;

/// Application id the desktop App Store client authorizes as.
const OAUTH_APP_ID: &str = "wxd44977328b36e647";
/// Where the QR page hands the scan back to.
const OAUTH_REDIRECT_URI: &str = "https://yybadaccess.3g.qq.com/pc_yyb/pcyyb_oauth?login_type=WX";
/// Page that renders a login QR.
const QR_CONNECT_URL: &str = "https://open.weixin.qq.com/connect/qrconnect";
/// Long-poll endpoint that reports the scan state.
const QR_LONG_POLL_URL: &str = "https://long.open.weixin.qq.com/connect/l/qrconnect";
/// Base of the rendered QR bitmaps.
const QR_IMAGE_BASE: &str = "https://open.weixin.qq.com/connect/qrcode/";

/// Browser-ish user agent; the QR page tailors its markup otherwise.
pub(crate) const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// How long a QR code stays valid, in seconds.
pub const QR_LIFETIME: u32 = 240;

/// A login waiting for the user to scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginSession {
    /// Identifier of this login attempt (the QR uuid).
    pub session_id: String,
    /// Account being re-authenticated, when the caller named one.
    #[serde(default)]
    pub login_id: Option<String>,
    /// URL of the rendered QR bitmap.
    pub qr_url: String,
    /// OAuth state parameter, echoed back by the confirm page.
    pub state: String,
    /// Unix seconds when the QR was issued.
    pub started_at: i64,
    /// Validity window, in seconds.
    pub expires_in: u32,
}

impl LoginSession {
    /// Seconds left before the QR expires (never negative).
    pub fn remaining(&self) -> u32 {
        let elapsed = now() - self.started_at;
        if elapsed <= 0 {
            return self.expires_in;
        }
        (i64::from(self.expires_in) - elapsed).max(0) as u32
    }
}

/// What the user has done with the QR so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStatus {
    /// Not scanned yet.
    Waiting,
    /// Scanned on the phone; waiting for the confirmation tap.
    Scanned,
    /// Confirmed. `code` is exchanged for a session by the iLink step.
    Authorized {
        /// Authorization code returned by the confirm page.
        code: String,
        /// Account that authorized, when the upstream names it.
        login_id: Option<String>,
    },
    /// The QR went stale.
    Expired,
    /// The user declined on the phone.
    Cancelled,
}

impl LoginStatus {
    /// The wire name the window uses.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Scanned => "scanned",
            Self::Authorized { .. } => "authorized",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Starts a login: fetches a QR uuid and the bitmap URL for it.
pub fn begin(login_id: Option<&str>) -> Result<LoginSession> {
    let state = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    );
    let url = format!(
        "{QR_CONNECT_URL}?appid={OAUTH_APP_ID}&redirect_uri={}&response_type=code&scope={}&self_redirect=true&state={state}&fast_login=1",
        percent_encode(OAUTH_REDIRECT_URI),
        percent_encode("snsapi_login,snsapi_runtime_pcsdk"),
    );

    let body = get(&url, Duration::from_secs(20))?;
    let html = String::from_utf8_lossy(&body);
    let uuid = parse_uuid(&html)
        .ok_or_else(|| Error::Protocol("the QR page did not contain a login uuid".to_owned()))?;

    Ok(LoginSession {
        session_id: uuid.clone(),
        login_id: login_id.map(str::to_owned),
        qr_url: format!("{QR_IMAGE_BASE}{uuid}"),
        state,
        started_at: now(),
        expires_in: QR_LIFETIME,
    })
}

/// Downloads the QR bitmap.
pub fn image(qr_url: &str) -> Result<Vec<u8>> {
    let bytes = get(qr_url, Duration::from_secs(20))?;
    let looks_like_image = bytes.starts_with(b"\x89PNG")
        || bytes.starts_with(&[0xff, 0xd8])
        || bytes.starts_with(b"GIF8");
    if !looks_like_image {
        return Err(Error::Protocol(
            "the QR endpoint returned something that is not an image".to_owned(),
        ));
    }
    Ok(bytes)
}

/// Polls the scan state once. The endpoint may block for up to ~25 seconds.
pub fn poll(session: &LoginSession) -> Result<LoginStatus> {
    let url = format!(
        "{QR_LONG_POLL_URL}?uuid={}&_={}",
        session.session_id,
        now_millis()
    );
    let body = get_with_referer(&url, Duration::from_secs(35), QR_CONNECT_URL)?;
    let text = String::from_utf8_lossy(&body);
    let parsed = parse_poll(&text)?;
    match parsed.code {
        408 => Ok(LoginStatus::Waiting),
        404 => Ok(LoginStatus::Scanned),
        405 => Ok(LoginStatus::Authorized {
            code: parsed.auth_code.unwrap_or_default(),
            login_id: session.login_id.clone(),
        }),
        402 => Ok(LoginStatus::Expired),
        403 => Ok(LoginStatus::Cancelled),
        other => Err(Error::Protocol(format!("unknown scan state {other}"))),
    }
}

// ---------------------------------------------------------------- internals

/// One parsed long-poll answer.
struct PollAnswer {
    code: i32,
    auth_code: Option<String>,
}

fn parse_poll(text: &str) -> Result<PollAnswer> {
    // `window.wx_errcode=408;window.wx_code='';`
    let re = regex::Regex::new(r"window\.wx_errcode=(\d+);\s*window\.wx_code='([^']*)'")
        .map_err(|err| Error::Protocol(err.to_string()))?;
    if let Some(captures) = re.captures(text) {
        let code = captures[1]
            .parse::<i32>()
            .map_err(|err| Error::Protocol(err.to_string()))?;
        let auth = captures.get(2).map(|m| m.as_str().to_owned());
        return Ok(PollAnswer {
            code,
            auth_code: auth.filter(|value| !value.is_empty()),
        });
    }

    // Some deployments answer JSON instead.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(code) = value.get("errcode").and_then(|code| code.as_i64()) {
            let auth = value
                .get("code")
                .and_then(|code| code.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            return Ok(PollAnswer {
                code: i32::try_from(code).unwrap_or(0),
                auth_code: auth,
            });
        }
    }

    Err(Error::Protocol(format!(
        "the scan state was not understood: {}",
        text.chars().take(120).collect::<String>()
    )))
}

/// Pulls the login uuid out of the QR page.
fn parse_uuid(html: &str) -> Option<String> {
    const PATTERNS: [&str; 5] = [
        r#"window\.uuid\s*=\s*["']([a-zA-Z0-9_-]+)["']"#,
        r#"uuid\s*[:=]\s*["']([a-zA-Z0-9_-]+)["']"#,
        r#"/connect/qrcode/([a-zA-Z0-9_-]+)"#,
        r#""uuid"\s*:\s*"([a-zA-Z0-9_-]+)""#,
        r#"id="wx_qrcode_img"[^>]*src="[^"]*qrcode/([a-zA-Z0-9_-]+)""#,
    ];
    for pattern in PATTERNS {
        let Ok(re) = regex::Regex::new(pattern) else {
            continue;
        };
        if let Some(captures) = re.captures(html) {
            if let Some(uuid) = captures.get(1) {
                return Some(uuid.as_str().to_owned());
            }
        }
    }
    None
}

pub(crate) fn agent(timeout: Duration) -> Result<ureq::Agent> {
    agent_with(timeout, 5)
}

/// An agent that does not follow redirects: the token exchange must see every hop,
/// because the credential cookies ride on the 3xx responses themselves.
pub(crate) fn agent_no_redirects(timeout: Duration) -> Result<ureq::Agent> {
    agent_with(timeout, 0)
}

fn agent_with(timeout: Duration, redirects: u32) -> Result<ureq::Agent> {
    // rustls: pure Rust TLS, compiles for every target including Android.
    let mut builder = ureq::AgentBuilder::new()
        .timeout(timeout)
        .redirects(redirects)
        .user_agent(USER_AGENT);
    // Honour the conventional proxy variables; a shared library would have ignored them.
    if let Some(proxy) = proxy_from_env() {
        builder = builder.proxy(proxy);
    }
    Ok(builder.build())
}

fn proxy_from_env() -> Option<ureq::Proxy> {
    for key in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        if let Some(value) = std::env::var_os(key) {
            let text = value.to_string_lossy();
            if !text.trim().is_empty() {
                if let Ok(proxy) = ureq::Proxy::new(text.trim()) {
                    return Some(proxy);
                }
            }
        }
    }
    None
}

fn get(url: &str, timeout: Duration) -> Result<Vec<u8>> {
    let request = agent(timeout)?.get(url);
    read(request, timeout)
}

fn get_with_referer(url: &str, timeout: Duration, referer: &str) -> Result<Vec<u8>> {
    let request = agent(timeout)?.get(url).set("Referer", referer);
    read(request, timeout)
}

fn read(request: ureq::Request, timeout: Duration) -> Result<Vec<u8>> {
    let response = request.timeout(timeout).call().map_err(|err| match err {
        ureq::Error::Status(code, _) => Error::Network(format!("HTTP {code}")),
        ureq::Error::Transport(transport) => Error::Network(transport.to_string()),
    })?;
    let mut bytes = Vec::new();
    use std::io::Read as _;
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|err| Error::Network(err.to_string()))?;
    Ok(bytes)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| i64::try_from(value.as_millis()).unwrap_or(0))
        .unwrap_or(0)
}

/// Percent-encodes one query value, leaving the characters the upstream expects.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte))
            }
            b'/' | b':' | b'?' | b'=' | b'&' | b',' => out.push(char::from(byte)),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_found_in_the_shapes_the_page_uses() {
        assert_eq!(
            parse_uuid(r#"<script>window.uuid = "031AbC-dEf";</script>"#),
            Some("031AbC-dEf".to_owned())
        );
        assert_eq!(
            parse_uuid(r#"<img id="wx_qrcode_img" src="https://x/qrcode/081xyz_2">"#),
            Some("081xyz_2".to_owned())
        );
        assert_eq!(parse_uuid("<html>no code here</html>"), None);
    }

    #[test]
    fn poll_states_map_to_what_the_window_shows() {
        let waiting = parse_poll("window.wx_errcode=408;window.wx_code=''").unwrap();
        assert_eq!(waiting.code, 408);
        assert!(waiting.auth_code.is_none());

        let scanned = parse_poll("window.wx_errcode=404;window.wx_code=''").unwrap();
        assert_eq!(scanned.code, 404);

        let confirmed = parse_poll("window.wx_errcode=405;window.wx_code='CODE-1'").unwrap();
        assert_eq!(confirmed.code, 405);
        assert_eq!(confirmed.auth_code.as_deref(), Some("CODE-1"));

        assert!(parse_poll("<html>nonsense</html>").is_err());
    }

    #[test]
    fn json_poll_answers_are_understood_too() {
        let answer = parse_poll(r#"{"errcode":405,"code":"ABC"}"#).unwrap();
        assert_eq!(answer.code, 405);
        assert_eq!(answer.auth_code.as_deref(), Some("ABC"));
    }

    #[test]
    fn a_session_reports_the_time_it_has_left() {
        let session = LoginSession {
            session_id: "x".to_owned(),
            login_id: None,
            qr_url: String::new(),
            state: String::new(),
            started_at: now() - 60,
            expires_in: 240,
        };
        let remaining = session.remaining();
        assert!((170..=180).contains(&remaining), "{remaining}");
    }

    #[test]
    fn query_values_keep_the_characters_the_service_expects() {
        assert_eq!(percent_encode("https://a/b?c=d,e"), "https://a/b?c=d,e");
        assert_eq!(percent_encode("a b"), "a%20b");
    }

    /// Hits the real QR endpoint. Run with `cargo test -p wxapkg-remote -- --ignored`.
    #[test]
    #[ignore = "requires network access"]
    fn a_qr_can_be_issued_and_its_bitmap_downloaded() {
        let session = begin(None).expect("begin");
        assert!(!session.session_id.is_empty());
        assert!(session.qr_url.starts_with(QR_IMAGE_BASE));
        let bytes = image(&session.qr_url).expect("image");
        assert!(bytes.len() > 2_000, "bitmap looks empty");
        let first = poll(&session).expect("poll");
        assert!(
            matches!(first, LoginStatus::Waiting | LoginStatus::Scanned),
            "an untouched QR must be waiting, got {first:?}"
        );
    }
}
