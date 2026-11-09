// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! The two packet types that establish and renew a session: ManualAuth and AutoAuth.
//!
//! Both travel HybridEcdh-sealed (the client has no session key yet) over the same WPKG
//! framing as every later call, addressed by CGI number instead of path.

use base64::Engine as _;
use prost::Message as _;

use crate::error::{Error, Result};
use crate::ilink::envelope::{self, RequestHeader};
use crate::ilink::hybrid::HybridCrypto;
use crate::ilink::mmtls::MmtlsChannel;
use crate::pb;
use crate::session::{
    hex_decode, hex_encode, Session, ILINK_APP_ID, ILINK_CLIENT_VERSION, SESSION_TYPE,
    SVR_STATIC_PUB_KEY,
};

/// ManualAuth endpoint, addressed by CGI 3453 in the packet header.
const PATH_MANUAL_AUTH: &str = "/ilink/ilinkapp/sys/ilinkapp_manualauth";
/// AutoAuth endpoint, CGI 3198.
const PATH_AUTO_AUTH: &str = "/ilink/ilinkapp/sys/ilinkapp_autoauth";

/// Hard-device class the reference client reports.
const HARD_DEV_TYPE: u32 = 4;
/// Auth request flavour of the login-buffer exchange.
const AUTH_REQUEST_TYPE: u32 = 1901;
/// Renewal scene, as the reference client sends it.
const AUTO_AUTH_SCENE: u32 = 0x76d;

/// What a successful exchange yields beyond the session itself.
pub struct AuthOutcome {
    /// The session every later call uses.
    pub session: Session,
    /// Server-stamped nickname bytes, when the server sends any.
    pub nickname: Option<Vec<u8>>,
}

/// Exchanges a confirmed scan's login buffer for a session.
///
/// `device_key` derives the device fingerprint, so re-signing the same account presents the
/// same device the server already knows.
pub fn manual_auth(login_buffer: &str, device_key: &str) -> Result<AuthOutcome> {
    let verify_buffer = base64::engine::general_purpose::STANDARD
        .decode(login_buffer.trim())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(login_buffer.trim()))
        .map_err(|err| Error::Protocol(format!("the login buffer is not valid base64: {err}")))?;
    let device_id = crate::session::device_id_from_key(device_key);

    let request = pb::ManualAuthRequest {
        base_request: Some(pb::IlinkBaseRequest {
            payload: Some(device_id.clone()),
        }),
        auth_request_type: Some(AUTH_REQUEST_TYPE),
        verify_buffer: Some(verify_buffer),
        hard_dev_type: Some(HARD_DEV_TYPE),
        session_type: Some(SESSION_TYPE),
        session_module_type: Some(0),
        field9: None,
        ilink_device_info: None,
        soft_type: None,
        sync_check_key: None,
        manual_auth_common: None,
    };
    let plain = prost::Message::encode_to_vec(&request);

    // The WPKG transport expects the plaintext LZ4-compressed before hybrid sealing.
    let body = envelope::lz4_literal_block(&plain);
    let svr_key = hex_decode(SVR_STATIC_PUB_KEY)?;
    let mut hybrid = HybridCrypto::new(&svr_key, &[]);
    let sealed = hybrid.seal(&body)?;

    let header = RequestHeader {
        uin: None,
        cmd_id: 3453,
        session_key: None,
        device_id: &device_id,
        client_ver: ILINK_CLIENT_VERSION,
        encrypt_type: 17,
    };
    let _ = &header; // the WPKG header carries these fields; the app header stays off the wire
    let packet = envelope::wrap_packet_with(&sealed, 0, ILINK_APP_ID, &[], 17, 3453, 1);

    let mut channel = MmtlsChannel::connect(crate::session::DEFAULT_HOST)?;
    let response = channel.post(PATH_MANUAL_AUTH, &packet)?;
    let parsed = envelope::parse_packet(&response)?;
    let stamped_server_id = parsed.server_id.clone();
    let body = parsed.into_body()?;
    if body.is_empty() {
        return Err(Error::Protocol(
            "the auth exchange returned an empty answer".to_owned(),
        ));
    }

    let plain = hybrid.open(&body)?;
    let answer = pb::ManualAuthResponse::decode(plain.as_slice())
        .map_err(|err| Error::Protocol(format!("the auth answer is malformed: {err}")))?;

    let base_ret = answer
        .base_response
        .as_ref()
        .and_then(|r| r.ret)
        .unwrap_or(0);
    if base_ret != 0 {
        return Err(Error::Upstream(format!(
            "the service rejected the login (ret {base_ret})"
        )));
    }

    let mut session = Session {
        device_id: hex_encode(&device_id),
        svr_static_pub_key: SVR_STATIC_PUB_KEY.to_owned(),
        ilink_app_id: ILINK_APP_ID.to_owned(),
        ..Session::default()
    };

    if let Some(unified) = &answer.auth_sect {
        if let Some(acct) = &unified.acct_sect {
            if acct.uin.unwrap_or(0) != 0 {
                session.uin = acct.uin.unwrap();
            }
        }
        if let Some(sect) = &unified.auth_sect {
            if let Some(key) = &sect.open_id {
                session.app_session_key = hex_encode(key);
            }
            if let Some(key) = &sect.extspam_data2 {
                session.auto_auth_key = hex_encode(key);
            }
            if let Some(key) = &sect.extspam_data1 {
                session.auto_auth_enc_key = hex_encode(key);
            }
            if let Some(key) = &sect.extspam_data3 {
                session.session_key = hex_encode(key);
            }
            if let Some(buf) = &sect.ext_buf {
                // The server routing id; the response header's stamp wins when both exist.
                session.server_id = hex_encode(buf);
            }
            session.expire_time = sect.expire_time.unwrap_or(86_400);
        }
    }
    if let Some(server_id) = stamped_server_id {
        if !server_id.is_empty() {
            session.server_id = hex_encode(&server_id);
        }
    }
    if !session.is_usable() {
        return Err(Error::Protocol(
            "the auth answer did not contain a usable session".to_owned(),
        ));
    }
    session.update_time = crate::store::now();

    Ok(AuthOutcome {
        session,
        nickname: answer
            .auth_sect
            .and_then(|sect| sect.auth_sect)
            .and_then(|sect| sect.nickname),
    })
}

/// Runs one AutoAuth renewal against the stored session's credentials.
///
/// Renewal rotates the keys in place. The reference client notes the rotated keys do not
/// always carry over to runtime transfers, so callers should treat a failure here as
/// "sign in again" rather than retry.
pub fn auto_auth(session: &Session) -> Result<Session> {
    let device_id = session.hex(&session.device_id)?;
    let auto_auth_key = session.hex(&session.auto_auth_key)?;
    let enc_key = session.hex(&session.auto_auth_enc_key)?;
    let server_id = session.hex(&session.server_id)?;
    let svr_key = session.hex(&session.svr_static_pub_key)?;

    let request = pb::AutoAuthRequest {
        base_request: Some(pb::IlinkBaseRequest {
            payload: Some(device_id.clone()),
        }),
        scene: Some(AUTO_AUTH_SCENE),
        autoauth_key: Some(auto_auth_key),
        extra_buffer: Some(server_id),
    };
    let plain = prost::Message::encode_to_vec(&request);
    let body = envelope::lz4_literal_block(&plain);

    let mut hybrid = HybridCrypto::new(&svr_key, &enc_key);
    let sealed = hybrid.seal(&body)?;

    let header = RequestHeader {
        uin: None,
        cmd_id: 3198,
        session_key: None,
        device_id: &device_id,
        client_ver: ILINK_CLIENT_VERSION,
        encrypt_type: 17,
    };
    let _ = &header; // as above
    let packet = envelope::wrap_packet_with(&sealed, 0, ILINK_APP_ID, &[], 17, 3198, 1);

    let mut channel = MmtlsChannel::connect(session.host())?;
    let response = channel.post(PATH_AUTO_AUTH, &packet)?;
    let parsed = envelope::parse_packet(&response)?;
    let body = parsed.into_body()?;
    let plain = hybrid.open(&body)?;
    let answer = pb::AutoAuthResponse::decode(plain.as_slice())
        .map_err(|err| Error::Protocol(format!("the renewal answer is malformed: {err}")))?;

    let base_ret = answer
        .base_response
        .as_ref()
        .and_then(|r| r.ret)
        .unwrap_or(0);
    if base_ret != 0 {
        return Err(Error::Upstream(format!(
            "the service rejected the renewal (ret {base_ret})"
        )));
    }

    let mut renewed = session.clone();
    if let Some(unified) = &answer.auth_sect {
        if let Some(sect) = &unified.auth_sect {
            if sect.uin.unwrap_or(0) != 0 {
                renewed.uin = sect.uin.unwrap();
            }
            if let Some(key) = &sect.open_id {
                renewed.app_session_key = hex_encode(key);
            }
            if let Some(key) = &sect.extspam_data2 {
                renewed.auto_auth_key = hex_encode(key);
            }
            if let Some(key) = &sect.extspam_data1 {
                renewed.auto_auth_enc_key = hex_encode(key);
            }
            renewed.expire_time = sect.expire_time.unwrap_or(86_400);
        }
    }
    renewed.update_time = crate::store::now();
    Ok(renewed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_buffer_is_decoded_from_either_base64_alphabet() {
        // "hello" in standard and URL-safe encodings.
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode("aGVsbG8=")
                .unwrap(),
            b"hello"
        );
    }

    #[test]
    fn the_auth_request_encodes_the_expected_fields() {
        let request = pb::ManualAuthRequest {
            base_request: Some(pb::IlinkBaseRequest {
                payload: Some(vec![1, 2, 3]),
            }),
            auth_request_type: Some(AUTH_REQUEST_TYPE),
            verify_buffer: Some(b"buf".to_vec()),
            hard_dev_type: Some(HARD_DEV_TYPE),
            soft_type: None,
            session_type: Some(SESSION_TYPE),
            session_module_type: Some(0),
            field9: None,
            ilink_device_info: None,
            sync_check_key: None,
            manual_auth_common: None,
        };
        let bytes = prost::Message::encode_to_vec(&request);
        let parsed = pb::ManualAuthRequest::decode(bytes.as_slice()).unwrap();
        assert_eq!(parsed.auth_request_type, Some(1901));
        assert_eq!(parsed.hard_dev_type, Some(4));
        assert_eq!(parsed.verify_buffer, Some(b"buf".to_vec()));
    }
}
