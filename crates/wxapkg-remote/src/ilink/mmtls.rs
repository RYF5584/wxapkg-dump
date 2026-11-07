// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! MMTLS: the transport the iLink requests travel over.
//!
//! A port of the reference implementation, kept deliberately close to it so the two can be
//! compared line by line. The handshake is a P-256 ECDH exchange over a plain HTTP POST to
//! `/mmtls/<unix time>`; everything after it is AES-GCM in five-byte records with a
//! sequence-xored nonce. No TLS, no shared library.
//!
//! Only the short (per-request) channel is implemented — that is all the four calls this
//! crate needs use.

use std::io::Read as _;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::Aes128Gcm;
use hmac::{Hmac, Mac};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::SecretKey;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Handshake timeout, matching the reference client.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// One short-channel record.
struct Record {
    kind: u8,
    payload: Vec<u8>,
}

/// Handshake and short-channel state.
#[derive(Clone)]
pub struct MmtlsChannel {
    host: String,
    client_seq: u32,
    server_seq: u32,
    /// Only the server→client half is used: every request travels on its own short channel,
    /// whose keys are derived from `PSK_ACCESS` instead.
    dec_key: Vec<u8>,
    dec_iv: Vec<u8>,
    psk_access_key: Vec<u8>,
    early_data: Vec<u8>,
    new_send_hash: Vec<u8>,
    short_enc_key: Vec<u8>,
    short_enc_iv: Vec<u8>,
    short_dec_key: Vec<u8>,
    short_dec_iv: Vec<u8>,
}

impl MmtlsChannel {
    /// Performs the handshake and returns a channel for `host`.
    pub fn connect(host: &str) -> Result<Self> {
        let v1 = SecretKey::random(&mut OsRng);
        let v2 = SecretKey::random(&mut OsRng);
        let v1_pub = v1.public_key().to_encoded_point(false).as_bytes().to_vec();
        let v2_pub = v2.public_key().to_encoded_point(false).as_bytes().to_vec();

        let mut hello = Vec::with_capacity(160);
        hello.extend_from_slice(&[0x01, 0x03, 0xf1, 0x01, 0xc0, 0x2b]);
        hello.extend_from_slice(&random_bytes(32));
        hello.extend_from_slice(&unix_seconds().to_be_bytes());
        hello.extend_from_slice(&[
            0x00, 0x00, 0x00, 0xa2, 0x01, 0x00, 0x00, 0x00, 0x9d, 0x00, 0x10, 0x02, 0x00, 0x00,
            0x00, 0x47, 0x00, 0x00, 0x00, 0x01,
        ]);
        hello.extend_from_slice(&(v1_pub.len() as i16).to_be_bytes());
        hello.extend_from_slice(&v1_pub);
        hello.extend_from_slice(&[0x00, 0x00, 0x00, 0x47, 0x00, 0x00, 0x00, 0x02]);
        hello.extend_from_slice(&(v2_pub.len() as i16).to_be_bytes());
        hello.extend_from_slice(&v2_pub);
        hello.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);

        let mut packed = Vec::with_capacity(hello.len() + 4);
        packed.extend_from_slice(&(hello.len() as i32).to_be_bytes());
        packed.extend_from_slice(&hello);

        let mut wire = Vec::with_capacity(packed.len() + 5);
        wire.extend_from_slice(&[0x16, 0xf1, 0x03]);
        wire.extend_from_slice(&(packed.len() as i16).to_be_bytes());
        wire.extend_from_slice(&packed);

        let mut transcript = packed.clone();
        let response = post_raw(
            host,
            &format!("/mmtls/{}", unix_seconds()),
            &wire,
            HANDSHAKE_TIMEOUT,
        )?;

        let (kind, payload, mut offset) = parse_record(&response, 0)?;
        if kind != 0x16 || payload.len() < 58 {
            return Err(Error::Protocol(format!(
                "MMTLS: unexpected server hello type={kind:#02x} len={}",
                payload.len()
            )));
        }
        let server_pub = &payload[58..];
        let use_second =
            u32::from_be_bytes([payload[52], payload[53], payload[54], payload[55]]) == 2;
        let private = if use_second { &v2 } else { &v1 };
        transcript.extend_from_slice(&payload);

        let server_hash = sha256(&transcript);
        let shared = ecdh(private, server_pub)?;
        let ecdh_hash = sha256(&shared);

        let mut info = b"handshake key expansion".to_vec();
        info.extend_from_slice(&server_hash);
        let keys = hkdf_sha256(&ecdh_hash, &info, 56);

        let mut channel = Self {
            host: host.to_owned(),
            client_seq: 1,
            server_seq: 1,
            dec_key: keys[16..32].to_vec(),
            dec_iv: keys[44..56].to_vec(),
            psk_access_key: Vec::new(),
            early_data: Vec::new(),
            new_send_hash: Vec::new(),
            short_enc_key: Vec::new(),
            short_enc_iv: Vec::new(),
            short_dec_key: Vec::new(),
            short_dec_iv: Vec::new(),
        };

        let part2 = channel.decrypt_handshake(&response, offset)?;
        transcript.extend_from_slice(&part2.0);
        // PSK_ACCESS covers the transcript through part 2, which is what the reference does
        // before part 3 is folded in.
        channel.psk_access_key = {
            let mut info = b"PSK_ACCESS".to_vec();
            info.extend_from_slice(&sha256(&transcript));
            hkdf_sha256(&ecdh_hash, &info, 32)
        };
        offset = part2.1;

        let part3 = channel.decrypt_handshake(&response, offset)?;
        transcript.extend_from_slice(&part3.0);
        offset = part3.1;

        // A third record is consumed for its side effect on the record sequence.
        let _ = channel.decrypt_handshake(&response, offset)?;

        if part3.0.len() < 10 {
            return Err(Error::Protocol(
                "MMTLS: early data part too short".to_owned(),
            ));
        }
        let early_len =
            u32::from_be_bytes([part3.0[6], part3.0[7], part3.0[8], part3.0[9]]) as usize;
        if part3.0.len() < 10 + early_len {
            return Err(Error::Protocol(
                "MMTLS: early data length invalid".to_owned(),
            ));
        }
        channel.early_data = part3.0[6..10 + early_len].to_vec();
        Ok(channel)
    }

    /// Host this channel is bound to.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Sends one request over the short channel and returns the decrypted payload.
    pub fn post(&mut self, cgi_path: &str, data: &[u8]) -> Result<Vec<u8>> {
        let wire = encode_short_payload(&self.host, cgi_path, data);
        let encrypted = self.encrypt_short(&wire)?;
        let response = post_raw(
            &self.host,
            &format!("/mmtls/{}", unix_seconds()),
            &encrypted,
            std::time::Duration::from_secs(20),
        )?;
        self.decrypt_short(&response)
    }

    fn decrypt_handshake(&mut self, data: &[u8], offset: usize) -> Result<(Vec<u8>, usize)> {
        let (_, payload, next) = parse_record(data, offset)?;
        let header = &data[offset..offset + 5];
        let mut aad = Vec::with_capacity(13);
        aad.extend_from_slice(&(self.server_seq as u64).to_be_bytes());
        aad.extend_from_slice(header);
        let (nonce, seq) = xor_nonce(&self.dec_iv, self.server_seq);
        self.server_seq = seq;
        let plain = open(&self.dec_key, &nonce, &payload, &aad)?;
        Ok((plain, next))
    }

    fn encrypt_short(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        self.client_seq = 1;
        self.server_seq = 1;
        let now = unix_seconds();

        let mut hello = Vec::with_capacity(80 + self.early_data.len());
        hello.extend_from_slice(&[0x01, 0x03, 0xf1, 0x01, 0x00, 0xa8]);
        hello.extend_from_slice(&random_bytes(32));
        hello.extend_from_slice(&now.to_be_bytes());
        hello.extend_from_slice(&[
            0x00, 0x00, 0x00, 0x6f, 0x01, 0x00, 0x00, 0x00, 0x6a, 0x00, 0x0f, 0x01,
        ]);
        hello.extend_from_slice(&self.early_data);

        let mut hello_content = Vec::with_capacity(hello.len() + 4);
        hello_content.extend_from_slice(&(hello.len() as i32).to_be_bytes());
        hello_content.extend_from_slice(&hello);

        let short_hash = sha256(&hello_content);
        let mut info = b"early data key expansion".to_vec();
        info.extend_from_slice(&short_hash);
        let keys = hkdf_sha256(&self.psk_access_key, &info, 28);
        self.short_enc_key = keys[0..16].to_vec();
        self.short_enc_iv = keys[16..28].to_vec();

        let mut out = Vec::new();
        out.extend_from_slice(&[0x19, 0xf1, 0x03]);
        out.extend_from_slice(&(hello_content.len() as i16).to_be_bytes());
        out.extend_from_slice(&hello_content);

        let mut part2_plain = vec![
            0x00, 0x00, 0x00, 0x10, 0x08, 0x00, 0x00, 0x00, 0x0b, 0x01, 0x00, 0x00, 0x00, 0x06,
            0x00, 0x12,
        ];
        part2_plain.extend_from_slice(&now.to_be_bytes());

        out.extend_from_slice(&self.seal_short(0x19, &part2_plain)?);
        out.extend_from_slice(&self.seal_short(0x17, data)?);
        out.extend_from_slice(&self.seal_short(0x15, &[0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x01])?);

        self.new_send_hash = hello_content;
        self.new_send_hash.extend_from_slice(&part2_plain);
        Ok(out)
    }

    fn seal_short(&mut self, kind: u8, plain: &[u8]) -> Result<Vec<u8>> {
        let length = plain.len() + 16;
        let mut aad = Vec::with_capacity(13);
        aad.extend_from_slice(&(self.client_seq as u64).to_be_bytes());
        aad.extend_from_slice(&[kind, 0xf1, 0x03, (length >> 8) as u8, length as u8]);
        let (nonce, seq) = xor_nonce(&self.short_enc_iv, self.client_seq);
        let sealed = seal(&self.short_enc_key, &nonce, plain, &aad)?;
        self.client_seq = seq;

        let mut out = Vec::with_capacity(5 + sealed.len());
        out.extend_from_slice(&[kind, 0xf1, 0x03, (length >> 8) as u8, length as u8]);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    fn decrypt_short(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let records = split_records(data)?;
        let mut out = Vec::new();
        for (index, record) in records.iter().enumerate() {
            if index == 0 && record.kind == 0x16 {
                let mut material = self.new_send_hash.clone();
                material.extend_from_slice(&record.payload);
                let response_hash = sha256(&material);
                let mut info = b"handshake key expansion".to_vec();
                info.extend_from_slice(&response_hash);
                let keys = hkdf_sha256(&self.psk_access_key, &info, 28);
                self.short_dec_key = keys[0..16].to_vec();
                self.short_dec_iv = keys[16..28].to_vec();
                continue;
            }
            if record.kind != 0x17 {
                continue;
            }
            if self.short_dec_key.is_empty() {
                return Err(Error::Protocol(
                    "MMTLS: response arrived before the short channel was keyed".to_owned(),
                ));
            }
            let length = record.payload.len();
            let mut aad = Vec::with_capacity(13);
            aad.extend_from_slice(&(index as u64).to_be_bytes());
            aad.extend_from_slice(&[record.kind, 0xf1, 0x03, (length >> 8) as u8, length as u8]);
            let (nonce, _) = xor_nonce(&self.short_dec_iv, index as u32);
            out.extend_from_slice(&open(&self.short_dec_key, &nonce, &record.payload, &aad)?);
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------ helpers

fn post_raw(host: &str, path: &str, body: &[u8], timeout: std::time::Duration) -> Result<Vec<u8>> {
    let mut builder = ureq::AgentBuilder::new()
        .timeout(timeout)
        .user_agent("MicroMessenger Client");
    // Test channel: when <files>/mmtls_proxy exists and is non-empty, forward through that HTTP proxy (for debugging mobile-network problems).
    if let Some(p) = crate::wxa::flag_value("mmtls_proxy") {
        if !p.is_empty() {
            if let Ok(proxy) = ureq::Proxy::new(&p) {
                builder = builder.proxy(proxy);
            }
        }
    }
    let agent = builder.build();
    let url = format!("http://{host}{path}");
    let response = agent
        .post(&url)
        .set("Accept", "*/*")
        .set("Cache-Control", "no-cache")
        .set("Connection", "close")
        .set("Content-Type", "application/octet-stream")
        .send_bytes(body)
        .map_err(|err| match err {
            ureq::Error::Status(code, _) => Error::Network(format!("HTTP {code} from {url}")),
            ureq::Error::Transport(transport) => Error::Network(transport.to_string()),
        })?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|err| Error::Network(err.to_string()))?;
    Ok(bytes)
}

fn parse_record(data: &[u8], offset: usize) -> Result<(u8, Vec<u8>, usize)> {
    if data.len() < offset + 5 {
        return Err(Error::Protocol("MMTLS: record header too short".to_owned()));
    }
    let length = u16::from_be_bytes([data[offset + 3], data[offset + 4]]) as usize;
    let end = offset + 5 + length;
    if end > data.len() {
        return Err(Error::Protocol("MMTLS: record length overflow".to_owned()));
    }
    Ok((data[offset], data[offset + 5..end].to_vec(), end))
}

fn split_records(data: &[u8]) -> Result<Vec<Record>> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let (kind, payload, next) = parse_record(data, offset)?;
        out.push(Record { kind, payload });
        offset = next;
    }
    Ok(out)
}

fn encode_short_payload(host: &str, cgi_path: &str, data: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(cgi_path.len() + host.len() + data.len() + 8);
    inner.extend_from_slice(&(cgi_path.len() as i16).to_be_bytes());
    inner.extend_from_slice(cgi_path.as_bytes());
    inner.extend_from_slice(&(host.len() as i16).to_be_bytes());
    inner.extend_from_slice(host.as_bytes());
    inner.extend_from_slice(&(data.len() as i32).to_be_bytes());
    inner.extend_from_slice(data);

    let mut out = Vec::with_capacity(inner.len() + 4);
    out.extend_from_slice(&(inner.len() as i32).to_be_bytes());
    out.extend_from_slice(&inner);
    out
}

fn xor_nonce(iv: &[u8], seq: u32) -> (Vec<u8>, u32) {
    let mut nonce = vec![0u8; 12];
    nonce[..8].copy_from_slice(&iv[..8]);
    let tail = u32::from_be_bytes([iv[8], iv[9], iv[10], iv[11]]) ^ seq;
    nonce[8..12].copy_from_slice(&tail.to_be_bytes());
    (nonce, seq + 1)
}

fn seal(key: &[u8], nonce: &[u8], plain: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes128Gcm::new_from_slice(key)
        .map_err(|err| Error::Protocol(format!("AES key rejected: {err}")))?;
    cipher
        .encrypt(
            aes_gcm::Nonce::from_slice(nonce),
            Payload { msg: plain, aad },
        )
        .map_err(|_| Error::Protocol("AES-GCM sealing failed".to_owned()))
}

fn open(key: &[u8], nonce: &[u8], ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes128Gcm::new_from_slice(key)
        .map_err(|err| Error::Protocol(format!("AES key rejected: {err}")))?;
    cipher
        .decrypt(
            aes_gcm::Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| Error::Protocol("AES-GCM opening failed".to_owned()))
}

/// HMAC-SHA256 expansion, exactly the construction the reference implements.
fn hkdf_sha256(prk: &[u8], info: &[u8], length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length);
    let mut block: Vec<u8> = Vec::new();
    let mut counter = 1u8;
    while out.len() < length {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(prk).expect("hmac accepts any key");
        mac.update(&block);
        mac.update(info);
        mac.update(&[counter]);
        block = mac.finalize().into_bytes().to_vec();
        out.extend_from_slice(&block);
        counter = counter.wrapping_add(1);
    }
    out.truncate(length);
    out
}

fn ecdh(private: &SecretKey, public: &[u8]) -> Result<Vec<u8>> {
    let public = p256::PublicKey::from_sec1_bytes(public)
        .map_err(|err| Error::Protocol(format!("peer key rejected: {err}")))?;
    let shared = p256::ecdh::diffie_hellman(private.to_nonzero_scalar(), public.as_affine());
    Ok(shared.raw_secret_bytes().to_vec())
}

fn sha256(data: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

fn random_bytes(count: usize) -> Vec<u8> {
    use rand::RngCore as _;
    let mut bytes = vec![0u8; count];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
}

fn unix_seconds() -> i32 {
    i32::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or(0),
    )
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonces_xor_the_sequence_into_the_last_word() {
        let iv = vec![1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 5];
        let (nonce, next) = xor_nonce(&iv, 3);
        assert_eq!(&nonce[..8], &iv[..8]);
        assert_eq!(&nonce[8..], &[0, 0, 0, 6]);
        assert_eq!(next, 4);
    }

    #[test]
    fn the_expansion_is_deterministic_and_sized() {
        let a = hkdf_sha256(b"key", b"info", 56);
        let b = hkdf_sha256(b"key", b"info", 56);
        assert_eq!(a, b);
        assert_eq!(a.len(), 56);
        assert_ne!(a, hkdf_sha256(b"key", b"other", 56));
    }

    #[test]
    fn records_round_trip_through_the_parser() {
        let mut data = vec![0x17, 0xf1, 0x03, 0x00, 0x03];
        data.extend_from_slice(b"abc");
        let (kind, payload, next) = parse_record(&data, 0).unwrap();
        assert_eq!(kind, 0x17);
        assert_eq!(payload, b"abc");
        assert_eq!(next, data.len());
    }

    #[test]
    fn a_short_payload_carries_host_and_path() {
        let wire = encode_short_payload("h", "/p", b"x");
        let inner_len = u32::from_be_bytes([wire[0], wire[1], wire[2], wire[3]]) as usize;
        assert_eq!(inner_len, wire.len() - 4);
        assert!(wire.windows(2).any(|w| w == b"/p"));
    }

    /// Exercises the short channel end to end: keys derive, records decrypt.
    #[test]
    #[ignore = "requires network access"]
    fn the_short_channel_decrypts_a_response() {
        let mut channel = MmtlsChannel::connect("shortcloud.weixin.com").expect("handshake");
        // An empty body is not a valid request, but the server still answers with a frame we
        // must be able to decrypt. That is the property under test.
        match channel.post("/ilink/ilinkapp/mp/wxaruntime_transfer", &[]) {
            Ok(bytes) => assert!(bytes.len() < 4096, "unexpectedly large response"),
            Err(err) => {
                let text = err.to_string();
                assert!(
                    !text.contains("AES-GCM opening failed"),
                    "the short channel could not be decrypted: {text}"
                );
            }
        }
    }

    /// Talks to the real host. Run with `cargo test -p wxapkg-remote -- --ignored`.
    #[test]
    #[ignore = "requires network access"]
    fn the_handshake_completes_against_the_live_host() {
        let channel = MmtlsChannel::connect("shortcloud.weixin.com").expect("handshake");
        assert_eq!(channel.host(), "shortcloud.weixin.com");
        assert!(!channel.psk_access_key.is_empty());
        assert!(!channel.early_data.is_empty());
    }
}
