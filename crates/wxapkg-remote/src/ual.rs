// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! The access API that bridges a confirmed scan to the login buffer.
//!
//! Three plain-HTTPS calls: the OAuth code is exchanged for tokens (the answer lives in
//! cookies set across a redirect chain), the tokens identify the user, and a signed request
//! mints the one-time buffer the iLink handshake consumes.

use std::time::Duration;

use md5::Digest as _;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::oauth::{agent, USER_AGENT};
use crate::session::UalCredentials;

/// Base URL of the access service.
const BASE_URL: &str = "https://yybadaccess.3g.qq.com";
/// Shared signing key of the PC client.
const ACCESS_KEY: &str = "wgrdg373hy26ww2";
/// OAuth-token exchange endpoint.
const PATH_OAUTH: &str = "/pc_yyb/pcyyb_oauth";
/// Signed user-profile endpoint.
const PATH_USER_INFO: &str = "/pc_yyb/pcyyb_get_user_info";
/// Signed login-buffer endpoint.
const PATH_LOGIN_BUFFER: &str = "/pc_yyb_auth/pcyyb_get_wx_login_buffer_auth";
/// Signed token-refresh endpoint.
const PATH_REFRESH_TOKEN: &str = "/pc_yyb/pcyyb_refresh_token";
/// Session keepalive endpoint.
const PATH_CHECK_LOGIN: &str = "/pc_yyb/pcyyb_check_login";

/// What the profile call reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserProfile {
    /// Display name.
    pub nick_name: String,
    /// Avatar URL.
    pub avatar: String,
    /// Stable identity at the access service.
    pub open_id: String,
}

/// Exchanges the confirmed scan's code for tokens.
///
/// The service answers the exchange with redirects that set the credential cookies; every
/// hop is followed and the cookies collected, exactly like the reference client.
pub fn exchange_code(code: &str, state: &str) -> Result<UalCredentials> {
    let mut credentials = UalCredentials {
        login_type: "WX".to_owned(),
        ..UalCredentials::default()
    };
    let mut cookies: Vec<(String, String)> = Vec::new();
    let mut url = format!(
        "{BASE_URL}{PATH_OAUTH}?login_type=WX&code={}&state={}",
        encode(code),
        encode(state)
    );

    for _ in 0..10 {
        let mut request = crate::oauth::agent_no_redirects(Duration::from_secs(20))?
            .get(&url)
            .set("User-Agent", USER_AGENT);
        for (name, value) in &cookies {
            request = request.set("Cookie", &format!("{name}={value}"));
        }
        let response = request
            .call()
            .map_err(|err| Error::Network(format!("the token exchange failed: {err}")))?;
        for cookie in response.all("set-cookie") {
            if let Some((name, value)) = split_cookie(cookie) {
                absorb(&mut credentials, &name, &value);
                match cookies.iter_mut().find(|(known, _)| known == &name) {
                    Some(slot) => slot.1 = value,
                    None => cookies.push((name, value)),
                }
            }
        }
        let location = response.header("location").unwrap_or_default().to_owned();
        let _ = response.into_reader().read_to_end(&mut Vec::new());
        if location.is_empty() {
            break;
        }
        url = if location.starts_with("http://") || location.starts_with("https://") {
            location
        } else {
            format!("{BASE_URL}{location}")
        };
    }

    if credentials.access_token.is_empty() {
        // One final sweep: some hops set the cookies on the same response twice.
        for (name, value) in &cookies {
            absorb(&mut credentials, name, value);
        }
    }
    if credentials.access_token.is_empty() {
        return Err(Error::Upstream(
            "the token exchange returned no access token".to_owned(),
        ));
    }
    credentials.cookies = cookies;
    Ok(credentials)
}

/// Fetches the profile behind a token set.
pub fn user_profile(credentials: &UalCredentials) -> Result<UserProfile> {
    #[derive(Deserialize)]
    struct Answer {
        #[serde(default)]
        ret: i64,
        #[serde(default)]
        code: i64,
        #[serde(default)]
        nick_name: String,
        #[serde(default)]
        head_img_url: String,
        #[serde(default)]
        open_id: String,
    }

    let (timestamp, nonce, signature) = sign("");
    let body = agent(Duration::from_secs(20))?
        .get(&format!("{BASE_URL}{PATH_USER_INFO}"))
        .set("Content-Type", "application/json")
        .set("Ual-Access-Businessid", "pc_yyb")
        .set("Ual-Access-Timestamp", &timestamp)
        .set("Ual-Access-Nonce", &nonce)
        .set("Ual-Access-Signature", &signature)
        .set("Ual-Access-Login-Type", "2")
        .set("Ual-Access-Access-Token", &credentials.access_token)
        .set("Ual-Access-Openid", &credentials.open_id)
        .call()
        .map_err(|err| Error::Network(format!("the profile request failed: {err}")))?;
    let answer: Answer = serde_json::from_reader(body.into_reader())
        .map_err(|err| Error::Protocol(format!("the profile answer is malformed: {err}")))?;
    if answer.ret != 0 || answer.code != 0 {
        return Err(Error::Upstream(format!(
            "the profile request was refused (ret {}, code {})",
            answer.ret, answer.code
        )));
    }
    Ok(UserProfile {
        nick_name: answer.nick_name,
        avatar: answer.head_img_url,
        open_id: answer.open_id,
    })
}

/// Runs the keepalive the reference client uses, updating tokens through cookies or body.
pub fn check_login(credentials: &mut UalCredentials) -> Result<()> {
    #[derive(Default, Deserialize)]
    struct UserInfo {
        #[serde(default)]
        access_token: String,
        #[serde(default)]
        refresh_token: String,
        #[serde(default)]
        open_id: String,
    }
    #[derive(Deserialize)]
    struct Answer {
        #[serde(default)]
        ret: i64,
        #[serde(default)]
        code: i64,
        #[serde(default)]
        user_info: Option<UserInfo>,
    }

    let body = serde_json::json!({
        "userInfo": {
            "openId": credentials.open_id,
            "refreshToken": credentials.refresh_token,
            "accessToken": credentials.access_token,
            "loginType": credentials.login_type,
        }
    });
    let body = serde_json::to_string(&body)
        .map_err(|err| Error::Protocol(format!("the keepalive could not be encoded: {err}")))?;
    let cookies = cookie_header(credentials);
    let mut request = agent(Duration::from_secs(20))?
        .post(&format!("{BASE_URL}{PATH_CHECK_LOGIN}"))
        .set("Content-Type", "application/json")
        .set("Ual-Access-Businessid", "pc_yb");
    if !cookies.is_empty() {
        request = request.set("Cookie", &cookies);
    }
    let response = request
        .send_string(&body)
        .map_err(|err| Error::Network(format!("the keepalive request failed: {err}")))?;
    absorb_cookies(credentials, &response);
    let answer: Answer = serde_json::from_reader(response.into_reader())
        .map_err(|err| Error::Protocol(format!("the keepalive answer is malformed: {err}")))?;
    if answer.ret != 0 || answer.code != 0 {
        return Err(Error::Upstream(format!(
            "the keepalive was refused (ret {}, code {})",
            answer.ret, answer.code
        )));
    }
    if let Some(user_info) = answer.user_info {
        if !user_info.access_token.is_empty() {
            credentials.access_token = user_info.access_token;
        }
        if !user_info.refresh_token.is_empty() {
            credentials.refresh_token = user_info.refresh_token;
        }
        if !user_info.open_id.is_empty() {
            credentials.open_id = user_info.open_id;
        }
    }
    Ok(())
}

/// Mints a fresh access token from the refresh token, updating `credentials` in place.
pub fn refresh_token(credentials: &mut UalCredentials) -> Result<()> {
    #[derive(Default, Deserialize)]
    struct UserInfo {
        #[serde(default)]
        access_token: String,
        #[serde(default)]
        refresh_token: String,
        #[serde(default)]
        open_id: String,
    }
    #[derive(Deserialize)]
    struct Answer {
        #[serde(default)]
        ret: i64,
        #[serde(default)]
        code: i64,
        #[serde(default)]
        user_info: Option<UserInfo>,
    }

    let body = serde_json::json!({
        "userInfo": {
            "openId": credentials.open_id,
            "refreshToken": credentials.refresh_token,
            "accessToken": credentials.access_token,
            "loginType": credentials.login_type,
        }
    });
    let body = serde_json::to_string(&body).map_err(|err| {
        Error::Protocol(format!("the refresh request could not be encoded: {err}"))
    })?;
    let (timestamp, nonce, signature) = sign(&body);
    let response = agent(Duration::from_secs(20))?
        .post(&format!("{BASE_URL}{PATH_REFRESH_TOKEN}"))
        .set("Content-Type", "application/json")
        .set("Ual-Access-Businessid", "pc_yyb")
        .set("Ual-Access-Timestamp", &timestamp)
        .set("Ual-Access-Nonce", &nonce)
        .set("Ual-Access-Signature", &signature)
        .set("Ual-Access-Openid", &credentials.open_id)
        .send_string(&body)
        .map_err(|err| Error::Network(format!("the refresh request failed: {err}")))?;
    // A refused refresh answers with a null user_info; a successful one may still hand
    // the new token over through cookies.
    for cookie in response.all("set-cookie") {
        if let Some((name, value)) = split_cookie(cookie) {
            if name == "accesstoken" {
                credentials.access_token = value;
            }
        }
    }
    let answer: Answer = serde_json::from_reader(response.into_reader())
        .map_err(|err| Error::Protocol(format!("the refresh answer is malformed: {err}")))?;
    if answer.ret != 0 || answer.code != 0 {
        return Err(Error::Upstream(format!(
            "the refresh was refused (ret {}, code {})",
            answer.ret, answer.code
        )));
    }
    if let Some(user_info) = answer.user_info {
        if !user_info.access_token.is_empty() {
            credentials.access_token = user_info.access_token;
        }
        if !user_info.refresh_token.is_empty() {
            credentials.refresh_token = user_info.refresh_token;
        }
        if !user_info.open_id.is_empty() {
            credentials.open_id = user_info.open_id;
        }
    }
    Ok(())
}

/// Mints the one-time buffer the iLink handshake needs.
pub fn login_buffer(device_key: &str, credentials: &UalCredentials) -> Result<String> {
    #[derive(Deserialize)]
    struct Answer {
        #[serde(default)]
        ret: i64,
        #[serde(default)]
        code: i64,
        #[serde(default)]
        msg: String,
        #[serde(default)]
        ext_info: Option<ExtInfo>,
    }
    #[derive(Default, Deserialize)]
    struct ExtInfo {
        #[serde(default)]
        list_s: ListS,
    }
    #[derive(Default, Deserialize)]
    struct ListS {
        #[serde(default)]
        login_buffer: Field,
    }
    #[derive(Default, Deserialize)]
    struct Field {
        #[serde(default)]
        value: Vec<String>,
    }

    let payload = serde_json::json!({
        "extInfo": {
            "listS": {
                "unionid": {"value": [null]},
                "user_id": {"value": [device_key]},
                "access_token": {"value": [credentials.access_token]},
            },
            "listI": {
                "user_type": {"value": [1]},
            },
        }
    });
    let body = serde_json::to_string(&payload).map_err(|err| {
        Error::Protocol(format!("the buffer request could not be encoded: {err}"))
    })?;
    let (timestamp, nonce, signature) = sign(&body);
    let cookies = cookie_header(credentials);
    let mut request = agent(Duration::from_secs(20))?
        .post(&format!("{BASE_URL}{PATH_LOGIN_BUFFER}"))
        .set("Content-Type", "application/json")
        .set("Ual-Access-Businessid", "pc_yyb_auth")
        .set("Ual-Access-Timestamp", &timestamp)
        .set("Ual-Access-Nonce", &nonce)
        .set("Ual-Access-Signature", &signature);
    if !cookies.is_empty() {
        request = request.set("Cookie", &cookies);
    }
    let response = request
        .send_string(&body)
        .map_err(|err| Error::Network(format!("the buffer request failed: {err}")))?;
    let answer: Answer = serde_json::from_reader(response.into_reader())
        .map_err(|err| Error::Protocol(format!("the buffer answer is malformed: {err}")))?;
    let refused = answer.ret != 0 || answer.code != 0;
    if refused {
        return Err(Error::Upstream(format!(
            "the buffer request was refused (ret {}, code {}, msg {})",
            answer.ret, answer.code, answer.msg
        )));
    }
    answer
        .ext_info
        .and_then(|ext| ext.list_s.login_buffer.value.into_iter().next())
        .filter(|buffer| !buffer.is_empty())
        .ok_or_else(|| Error::Upstream("the buffer answer carried no buffer".to_owned()))
}

// ---------------------------------------------------------------- internals

/// Cookie header the access service expects, from the credential cookies.
fn cookie_header(credentials: &UalCredentials) -> String {
    credentials
        .cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Absorbs the cookies of a response into the credentials.
fn absorb_cookies(credentials: &mut UalCredentials, response: &ureq::Response) {
    for cookie in response.all("set-cookie") {
        let Some((name, value)) = split_cookie(cookie) else {
            continue;
        };
        match credentials
            .cookies
            .iter_mut()
            .find(|(known, _)| known == &name)
        {
            Some(slot) => slot.1.clone_from(&value),
            None => credentials.cookies.push((name.clone(), value.clone())),
        }
        absorb(credentials, &name, &value);
    }
}

/// `md5(body + timestamp + key + nonce)`, the scheme every signed call uses.
fn sign(body: &str) -> (String, String, String) {
    let timestamp = crate::store::now() * 1000;
    let nonce = rand::Rng::gen_range(&mut rand::rngs::OsRng, 0..10_000);
    let digest = {
        let mut hasher = md5::Md5::new();
        hasher.update(body.as_bytes());
        hasher.update(timestamp.to_string().as_bytes());
        hasher.update(ACCESS_KEY.as_bytes());
        hasher.update(nonce.to_string().as_bytes());
        hasher.finalize()
    };
    (
        timestamp.to_string(),
        nonce.to_string(),
        digest.iter().map(|b| format!("{b:02x}")).collect(),
    )
}

/// Pulls `name=value` out of a `Set-Cookie` header, dropping attributes.
fn split_cookie(header: &str) -> Option<(String, String)> {
    let pair = header.split(';').next()?.trim();
    let (name, value) = pair.split_once('=')?;
    Some((name.trim().to_owned(), value.trim().to_owned()))
}

fn absorb(credentials: &mut UalCredentials, name: &str, value: &str) {
    match name {
        "accesstoken" => credentials.access_token = value.to_owned(),
        "openid" => credentials.open_id = value.to_owned(),
        "refreshtoken" => credentials.refresh_token = value.to_owned(),
        "logintype" => credentials.login_type = value.to_owned(),
        "scope" => credentials.scope = value.to_owned(),
        _ => {}
    }
}

/// Query encoding for the two values the exchange sends.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_split_into_name_and_value() {
        let (name, value) = split_cookie("accesstoken=abc; Path=/; HttpOnly").unwrap();
        assert_eq!(name, "accesstoken");
        assert_eq!(value, "abc");
        assert!(split_cookie("no pair here").is_none());
    }

    #[test]
    fn credential_cookies_are_absorbed_by_name() {
        let mut credentials = UalCredentials::default();
        absorb(&mut credentials, "accesstoken", "t");
        absorb(&mut credentials, "openid", "o");
        absorb(&mut credentials, "refreshtoken", "r");
        absorb(&mut credentials, "unrelated", "x");
        assert_eq!(credentials.access_token, "t");
        assert_eq!(credentials.open_id, "o");
        assert_eq!(credentials.refresh_token, "r");
    }

    #[test]
    fn the_signature_is_deterministic_for_fixed_inputs() {
        // The scheme itself: md5(body + timestamp + key + nonce), hex encoded.
        let mut hasher = md5::Md5::new();
        hasher.update(b"body");
        hasher.update(b"123");
        hasher.update(ACCESS_KEY.as_bytes());
        hasher.update(b"42");
        let expected: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let body = "body";
        let timestamp = "123".to_owned();
        let nonce = "42".to_owned();
        let digest = {
            let mut hasher = md5::Md5::new();
            hasher.update(body.as_bytes());
            hasher.update(timestamp.as_bytes());
            hasher.update(ACCESS_KEY.as_bytes());
            hasher.update(nonce.as_bytes());
            hasher.finalize()
        };
        let actual: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 32);
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(encode("a+b c"), "a%2Bb%20c");
        assert_eq!(encode("plain-1_2.3~4"), "plain-1_2.3~4");
    }
}
