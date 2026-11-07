// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! The iLink request envelope: header, WPKG frame, session-key encryption.
//!
//! Everything here is a faithful port of the reference client, reduced to what the four
//! calls need. Bodies are protobuf; this module only writes and reads the envelope, so a
//! caller supplies already-encoded bytes.

use aes_gcm::aead::consts::U12;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::aes::Aes192;
use aes_gcm::AesGcm;
use rand::RngCore as _;

use crate::error::{Error, Result};

/// AES-192-GCM with the account's app session key.
type Aes192Gcm = AesGcm<Aes192, U12>;

/// Encryption type understood by the header: AES-192-GCM with the app session key.
pub const ENCRYPT_TYPE_SESSION_AES: u32 = 16;
/// Compression the client advertises.
const COMPRESS_LZ4: u64 = 4;
/// Fixed packing mode.
const PACKING_MODE: u32 = 4;

/// The header prepended to every request body.
pub struct RequestHeader<'a> {
    /// Caller identity; absent while the session is still being established.
    pub uin: Option<u64>,
    /// CGI command number; zero for the runtime transfer endpoint.
    pub cmd_id: u32,
    /// Session key (also identifies the session to the server); absent during auth.
    pub session_key: Option<&'a [u8]>,
    /// Device fingerprint.
    pub device_id: &'a [u8],
    /// Client version the server expects.
    pub client_ver: u32,
    /// How the body was sealed.
    pub encrypt_type: u32,
}

/// A parsed WPKG response.
pub struct WpkgResponse {
    /// Transport-level return code (0 is success).
    pub ret: i64,
    /// Application-level return code (0 is success).
    pub app_ret: i64,
    /// Session id the server stamped on the answer, when present.
    pub session_id: Option<Vec<u8>>,
    /// Server routing id the server stamped on the answer, when present.
    pub server_id: Option<Vec<u8>>,
    /// Payload after the header.
    pub body: Vec<u8>,
}

impl WpkgResponse {
    /// Consumes the response, rejecting both transport and application failures.
    pub fn into_body(self) -> Result<Vec<u8>> {
        if self.ret != 0 {
            return Err(Error::Protocol(format!(
                "WPKG rejected the request (ret {})",
                self.ret
            )));
        }
        if self.app_ret != 0 {
            return Err(Error::Protocol(format!(
                "the service rejected the request (app_ret {})",
                self.app_ret
            )));
        }
        Ok(self.body)
    }
}

/// Encrypts a body with the session key (AES-192-GCM, LZ4-compressed).
///
/// The wire layout is `ciphertext ‖ nonce ‖ tag`, the order the reference client writes.
pub fn seal_body(session_key: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
    let key = shrink_key(session_key, 24);
    let compressed = lz4_literal_block(plain);
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);

    let cipher = Aes192Gcm::new_from_slice(&key)
        .map_err(|err| Error::Protocol(format!("session key rejected: {err}")))?;
    let sealed = cipher
        .encrypt(aes_gcm::Nonce::from_slice(&nonce), compressed.as_slice())
        .map_err(|_| Error::Protocol("session sealing failed".to_owned()))?;
    // aes-gcm returns ciphertext‖tag; the nonce belongs between them.
    let (body, tag) = sealed.split_at(sealed.len() - 16);
    let mut out = Vec::with_capacity(sealed.len() + 12);
    out.extend_from_slice(body);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(tag);
    Ok(out)
}

/// Decrypts a response body. The plaintext may be zlib, LZ4 or raw.
pub fn open_body(session_key: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    const OVERHEAD: usize = 12 + 16;
    if sealed.len() < OVERHEAD {
        return Err(Error::Protocol("session response is too short".to_owned()));
    }
    let key = shrink_key(session_key, 24);
    let split = sealed.len() - OVERHEAD;
    let (body, tail) = sealed.split_at(split);
    let (nonce, tag) = tail.split_at(12);
    let mut joined = body.to_vec();
    joined.extend_from_slice(tag);
    let cipher = Aes192Gcm::new_from_slice(&key)
        .map_err(|err| Error::Protocol(format!("session key rejected: {err}")))?;
    let compressed = cipher
        .decrypt(aes_gcm::Nonce::from_slice(nonce), joined.as_slice())
        .map_err(|_| Error::Protocol("session opening failed".to_owned()))?;
    Ok(decompress(&compressed))
}

/// `varint(len) + header + body`, the frame every CGI receives.
pub fn build_frame(header: &RequestHeader<'_>, body: &[u8]) -> Vec<u8> {
    let header_bytes = encode_header(header);
    let mut frame = Vec::with_capacity(header_bytes.len() + body.len() + 8);
    frame.extend_from_slice(&varint(header_bytes.len() as u64));
    frame.extend_from_slice(&header_bytes);
    frame.extend_from_slice(body);
    frame
}

/// Wraps a frame in the native request packet the transport carries, runtime-transfer style.
pub fn wrap_packet(
    frame: &[u8],
    uin: u64,
    session_id: &str,
    server_id: &[u8],
    encrypt_type: u32,
) -> Vec<u8> {
    wrap_packet_with(frame, uin, session_id, server_id, encrypt_type, 0, 1)
}

/// [`wrap_packet`] with an explicit outer command and sequence, which the auth endpoints
/// need (they are addressed by CGI number, not by path).
pub fn wrap_packet_with(
    frame: &[u8],
    uin: u64,
    session_id: &str,
    server_id: &[u8],
    encrypt_type: u32,
    cmd_id: u32,
    seq: u32,
) -> Vec<u8> {
    let header = wpkg_header(uin, session_id, server_id, encrypt_type);
    let mut payload = Vec::with_capacity(header.len() + frame.len() + 16);
    payload.extend_from_slice(&header);
    payload.push(0);
    payload.extend_from_slice(&varint(header.len() as u64 + 1));
    payload.extend_from_slice(frame);

    let mut out = Vec::with_capacity(payload.len() + 16);
    out.extend_from_slice(&(payload.len() as u32 + 16).to_be_bytes());
    out.extend_from_slice(&0x1110u16.to_be_bytes());
    out.extend_from_slice(&0x076du16.to_be_bytes());
    out.extend_from_slice(&cmd_id.to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Parses a native response packet and its WPKG header.
pub fn parse_packet(data: &[u8]) -> Result<WpkgResponse> {
    if data.len() < 16 {
        return Err(Error::Protocol("response packet is too short".to_owned()));
    }
    let length = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if length < 16 || length > data.len() {
        return Err(Error::Protocol(format!(
            "response length {length} does not fit {} bytes",
            data.len()
        )));
    }
    let magic_hi = u16::from_be_bytes([data[4], data[5]]);
    let magic_lo = u16::from_be_bytes([data[6], data[7]]);
    if magic_hi != 0x1110 || magic_lo != 0x076d {
        return Err(Error::Protocol("response magic mismatch".to_owned()));
    }
    parse_wpkg_body(&data[16..length])
}

// ------------------------------------------------------------------ internals

fn encode_header(header: &RequestHeader<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    field_bool(&mut out, 1, true);
    if let Some(uin) = header.uin {
        field_varint(&mut out, 2, uin);
    }
    field_varint(&mut out, 3, u64::from(header.cmd_id));
    if let Some(key) = header.session_key {
        field_bytes(&mut out, 4, key);
    }
    field_varint(&mut out, 5, u64::from(header.encrypt_type));
    field_varint(&mut out, 6, u64::from(PACKING_MODE));
    field_bytes(&mut out, 7, header.device_id);
    field_varint(&mut out, 8, u64::from(header.client_ver));
    out
}

fn wpkg_header(uin: u64, session_id: &str, server_id: &[u8], encrypt_type: u32) -> Vec<u8> {
    // Numeric fields 1..30 (with gaps), exactly as the reference emits them.
    let numeric: [(u64, u64); 24] = [
        (1, 1),
        (2, uin),
        (3, 0),
        (4, 0),
        (5, 0x80101),
        (6, 11),
        (7, 0),
        (8, 0),
        (9, 0),
        (10, 1),
        (11, 0),
        (12, 0),
        (13, 0),
        (17, 0),
        (18, 1),
        (20, 1504),
        (21, 0),
        (22, uin),
        (23, 0),
        (25, u64::from(encrypt_type)),
        (26, COMPRESS_LZ4),
        (28, 1),
        (29, 1),
        (30, 0),
    ];

    let mut out = Vec::new();
    out.extend_from_slice(&varint(1));
    for (tag, value) in numeric.iter() {
        out.push(*tag as u8);
        out.extend_from_slice(&varint(*value));
    }
    out.push(0);
    // Length-delimited fields, in tag order: 14 (empty), 24 (session id), 27 (server id).
    out.push(14);
    out.extend_from_slice(&varint(0));
    out.push(24);
    out.extend_from_slice(&varint(session_id.len() as u64));
    out.extend_from_slice(session_id.as_bytes());
    out.push(27);
    out.extend_from_slice(&varint(server_id.len() as u64));
    out.extend_from_slice(server_id);
    out
}

fn parse_wpkg_body(data: &[u8]) -> Result<WpkgResponse> {
    let mut pos = 0usize;
    if read_varint(data, &mut pos) != Some(1) {
        return Err(Error::Protocol(
            "response does not start a WPKG body".to_owned(),
        ));
    }
    let mut ret = 0i64;
    let mut app_ret = 0i64;
    loop {
        let field = read_varint(data, &mut pos)
            .ok_or_else(|| Error::Protocol("truncated WPKG numeric header".to_owned()))?;
        if field == 0 {
            break;
        }
        let value = read_varint(data, &mut pos)
            .ok_or_else(|| Error::Protocol("truncated WPKG numeric value".to_owned()))?;
        match field {
            4 => ret = value as i64,
            23 => app_ret = value as i64,
            _ => {}
        }
    }
    let mut session_id = None;
    let mut server_id = None;
    loop {
        let field = read_varint(data, &mut pos)
            .ok_or_else(|| Error::Protocol("truncated WPKG header".to_owned()))?;
        if field == 0 {
            break;
        }
        let length = read_varint(data, &mut pos)
            .ok_or_else(|| Error::Protocol("truncated WPKG field length".to_owned()))?
            as usize;
        if pos + length > data.len() {
            return Err(Error::Protocol(
                "WPKG field runs past the response".to_owned(),
            ));
        }
        match field {
            24 => session_id = Some(data[pos..pos + length].to_vec()),
            27 => server_id = Some(data[pos..pos + length].to_vec()),
            _ => {}
        }
        pos += length;
    }
    // The header ends with its own encoded length; the payload starts after it.
    let Some(_header_len) = read_varint(data, &mut pos) else {
        return Ok(WpkgResponse {
            ret,
            app_ret,
            session_id,
            server_id,
            body: Vec::new(),
        });
    };
    Ok(WpkgResponse {
        ret,
        app_ret,
        session_id,
        server_id,
        body: data[pos..].to_vec(),
    })
}

fn field_varint(out: &mut Vec<u8>, number: u64, value: u64) {
    out.extend_from_slice(&varint(number << 3));
    out.extend_from_slice(&varint(value));
}

fn field_bool(out: &mut Vec<u8>, number: u64, value: bool) {
    field_varint(out, number, u64::from(value));
}

fn field_bytes(out: &mut Vec<u8>, number: u64, value: &[u8]) {
    out.extend_from_slice(&varint((number << 3) | 2));
    out.extend_from_slice(&varint(value.len() as u64));
    out.extend_from_slice(value);
}

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0;
    while *pos < data.len() {
        let byte = data[*pos];
        *pos += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

fn shrink_key(key: &[u8], size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    let take = key.len().min(size);
    out[..take].copy_from_slice(&key[..take]);
    out
}

/// LZ4 block with only a literal run: valid output, simple to produce.
pub fn lz4_literal_block(src: &[u8]) -> Vec<u8> {
    if src.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(src.len() + 8);
    if src.len() < 15 {
        out.push((src.len() as u8) << 4);
    } else {
        out.push(0xf0);
        let mut remaining = src.len() - 15;
        while remaining >= 255 {
            out.push(255);
            remaining -= 255;
        }
        out.push(remaining as u8);
    }
    out.extend_from_slice(src);
    out
}

/// Decompresses a response: zlib first, then an LZ4 block, then as-is.
fn decompress(data: &[u8]) -> Vec<u8> {
    if let Ok(plain) = inflate_zlib(data) {
        return plain;
    }
    if let Some(plain) = lz4_decompress_block(data) {
        return plain;
    }
    data.to_vec()
}

fn inflate_zlib(data: &[u8]) -> std::result::Result<Vec<u8>, std::io::Error> {
    use std::io::Read as _;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

/// Minimal LZ4 block decoder (literals + matches), enough for server payloads.
pub fn lz4_decompress_block(src: &[u8]) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut input = 0usize;
    while input < src.len() {
        let token = src[input];
        input += 1;

        let mut literal_len = usize::from(token >> 4);
        if literal_len == 15 {
            loop {
                let extra = *src.get(input)?;
                input += 1;
                literal_len += usize::from(extra);
                if extra != 255 {
                    break;
                }
            }
        }
        if literal_len > 0 {
            let end = input.checked_add(literal_len)?;
            out.extend_from_slice(src.get(input..end)?);
            input = end;
        }
        if input >= src.len() {
            return Some(out);
        }

        let offset = usize::from(*src.get(input)?) | (usize::from(*src.get(input + 1)?) << 8);
        input += 2;
        if offset == 0 || offset > out.len() {
            return None;
        }
        let mut match_len = usize::from(token & 0x0f);
        if match_len == 15 {
            loop {
                let extra = *src.get(input)?;
                input += 1;
                match_len += usize::from(extra);
                if extra != 255 {
                    break;
                }
            }
        }
        match_len += 4;
        let start = out.len() - offset;
        for index in 0..match_len {
            let byte = out[start + index];
            out.push(byte);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel_key() -> Vec<u8> {
        (0u8..24).collect()
    }

    #[test]
    fn varints_round_trip() {
        for value in [0u64, 1, 127, 128, 300, 1 << 20, u64::MAX] {
            let bytes = varint(value);
            let mut pos = 0;
            assert_eq!(read_varint(&bytes, &mut pos), Some(value));
            assert_eq!(pos, bytes.len());
        }
    }

    #[test]
    fn a_body_sealed_with_the_session_key_opens_again() {
        let key = channel_key();
        let plain = b"hello iLink".to_vec();
        let sealed = seal_body(&key, &plain).unwrap();
        assert_ne!(sealed, plain);
        assert_eq!(open_body(&key, &sealed).unwrap(), plain);
    }

    #[test]
    fn a_different_key_cannot_open_the_body() {
        let sealed = seal_body(&channel_key(), b"secret").unwrap();
        let mut other = channel_key();
        other[0] ^= 0xff;
        assert!(open_body(&other, &sealed).is_err());
    }

    #[test]
    fn literal_blocks_decode_to_the_original() {
        for size in [1usize, 14, 15, 300, 4096] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let block = lz4_literal_block(&data);
            assert_eq!(lz4_decompress_block(&block).unwrap(), data, "size {size}");
        }
    }

    #[test]
    fn a_response_header_separates_codes_from_the_body() {
        let mut data = varint(1);
        for (field, value) in [(1u64, 1u64), (2, 42), (4, 0), (23, 0)] {
            data.extend_from_slice(&varint(field));
            data.extend_from_slice(&varint(value));
        }
        data.push(0);
        data.push(24);
        data.extend_from_slice(&varint(2));
        data.extend_from_slice(b"id");
        data.push(27);
        data.extend_from_slice(&varint(2));
        data.extend_from_slice(b"sv");
        data.push(0);
        data.extend_from_slice(&varint(9));
        data.extend_from_slice(b"payload");

        let parsed = parse_wpkg_body(&data).unwrap();
        assert_eq!(parsed.ret, 0);
        assert_eq!(parsed.app_ret, 0);
        assert_eq!(parsed.session_id.as_deref(), Some(b"id".as_slice()));
        assert_eq!(parsed.server_id.as_deref(), Some(b"sv".as_slice()));
        assert_eq!(parsed.body, b"payload");
    }

    #[test]
    fn a_rejected_request_reports_both_codes() {
        let mut data = varint(1);
        for (field, value) in [(4u64, 0u64), (23, 24)] {
            data.extend_from_slice(&varint(field));
            data.extend_from_slice(&varint(value));
        }
        data.push(0);
        data.push(0);

        let parsed = parse_wpkg_body(&data).unwrap();
        assert_eq!(parsed.app_ret, 24);
        assert!(parsed.into_body().is_err());
    }

    #[test]
    fn a_frame_carries_the_header_then_the_body() {
        let header = RequestHeader {
            uin: Some(7),
            cmd_id: 0,
            session_key: Some(b"k".as_slice()),
            device_id: b"d",
            client_ver: 1,
            encrypt_type: ENCRYPT_TYPE_SESSION_AES,
        };
        let frame = build_frame(&header, b"body");
        let mut pos = 0;
        let header_len = read_varint(&frame, &mut pos).unwrap() as usize;
        assert_eq!(&frame[pos + header_len..], b"body");
        // The header encodes uin (field 2) and the session key (field 4).
        let encoded = &frame[pos..pos + header_len];
        assert!(encoded.windows(2).any(|pair| pair == [0x10, 0x07]));
        assert!(encoded.windows(2).any(|pair| pair == [0x22, 0x01]));
    }

    /// Builds a real request from a stored session and sends it through MMTLS.
    ///
    /// `WXAPKG_TEST_SESSION` points at the JSON the bridge stores for an account; the test
    /// is ignored because it needs a live session and network access.
    #[test]
    #[ignore = "requires a live session and network access"]
    fn a_real_session_reaches_the_service() {
        use crate::ilink::mmtls::MmtlsChannel;

        let path =
            std::env::var("WXAPKG_TEST_SESSION").unwrap_or_else(|_| "/tmp/session.json".to_owned());
        let text = std::fs::read_to_string(&path).expect("session json");
        let value: serde_json::Value = serde_json::from_str(&text).expect("session json parses");
        let auth = &value["auth_data"];
        let hex = |key: &str| -> Vec<u8> {
            let text = auth[key].as_str().unwrap_or_default();
            (0..text.len() / 2)
                .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap_or(0))
                .collect()
        };
        let uin = auth["uin"].as_u64().unwrap_or(0);
        let client_ver = auth["client_ver"].as_u64().unwrap_or(0x80101) as u32;
        let host = auth["host"].as_str().unwrap_or("shortcloud.weixin.com");
        let session_key = hex("app_session_key");
        let device_id = hex("device_id");
        let server_id = hex("server_id");
        let app_id = auth["ilink_app_id"].as_str().unwrap_or_default();
        assert!(!session_key.is_empty() && !device_id.is_empty());

        let mut channel = MmtlsChannel::connect(host).expect("handshake");
        let header = RequestHeader {
            uin: Some(uin),
            cmd_id: 0,
            session_key: Some(session_key.as_slice()),
            device_id: &device_id,
            client_ver,
            encrypt_type: ENCRYPT_TYPE_SESSION_AES,
        };
        // An empty body is not a valid request; a decodable rejection still proves that the
        // header, the session encryption and the packet shape are all accepted.
        let sealed = seal_body(&session_key, &[]).expect("seal");
        let frame = build_frame(&header, &sealed);
        let packet = wrap_packet(&frame, uin, app_id, &server_id, ENCRYPT_TYPE_SESSION_AES);
        let response = channel
            .post("/ilink/ilinkapp/mp/wxaruntime_transfer", &packet)
            .expect("transport");
        let parsed = parse_packet(&response).expect("wpkg response");
        eprintln!(
            "wpkg ret={} app_ret={} body={} bytes",
            parsed.ret,
            parsed.app_ret,
            parsed.body.len()
        );
        // The transport must accept the frame; that is what proves the header fields, the
        // packet shape and the session encryption are understood. An empty body is not a
        // valid request, so the application layer answers with -14 (invalid Buf2Resp).
        assert_eq!(parsed.ret, 0, "transport-level rejection");
        if parsed.app_ret == 0 {
            let plain = open_body(&session_key, &parsed.body).expect("session body decrypts");
            assert!(!plain.is_empty() || parsed.body.is_empty());
        } else {
            assert_eq!(parsed.app_ret, -14, "unexpected application error");
        }
    }

    #[test]
    fn a_wrapped_packet_starts_with_its_length_and_magic() {
        let frame = vec![1, 2, 3];
        let packet = wrap_packet(&frame, 9, "session", b"server", ENCRYPT_TYPE_SESSION_AES);
        let length = u32::from_be_bytes([packet[0], packet[1], packet[2], packet[3]]) as usize;
        assert_eq!(length, packet.len());
        assert_eq!(&packet[4..8], &[0x11, 0x10, 0x07, 0x6d]);
    }
}
