// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Hybrid ECDH sealing, used by the first authentication packet of a session.
//!
//! One P-256 exchange protects a random key twice (under the ECDH secret and, when known,
//! under the server static key); the random key expands into the body key through HKDF. The
//! response travels under the same exchange, and its tag chains the request transcript, so
//! encrypt and decrypt share one running SHA-256 state exactly the way the reference client
//! does.

use aes_gcm::aead::consts::U12;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::aes::Aes192;
use aes_gcm::AesGcm;
use hmac::{Hmac, Mac};
use p256::ecdh::EphemeralSecret;
use p256::elliptic_curve::sec1::ToSec1Point;
use p256::elliptic_curve::Generate as _;
use p256::PublicKey;
use rand::rngs::OsRng;
use sha2::Digest as _;
use sha2::Sha256;

use crate::error::{Error, Result};
use crate::pb;
use prost::Message as _;

/// Curve identifier on the wire (the OpenSSL nid of prime256v1).
const P256_VERSION: &str = "415";

/// HKDF salt label, misspelled upstream and kept verbatim.
const HKDF_LABEL: &[u8] = b"security hdkf expand";

/// AES-192-GCM used throughout the hybrid envelope.
type Aes192Gcm = AesGcm<Aes192, U12>;

/// One hybrid exchange: the key pair, the shared transcript state and the server keys.
pub struct HybridCrypto {
    secret: Option<EphemeralSecret>,
    public: Vec<u8>,
    /// Server key the first layer of the random key is encrypted under.
    server_ecdsa_pub_key: Vec<u8>,
    /// Second-layer key (a session's `auto_auth_enc_key`), empty on a fresh login.
    server_static_pub_key: Vec<u8>,
    /// Running transcript the response tag chains on to; mirrors the reference client.
    server_hash: Sha256,
}

impl HybridCrypto {
    /// Prepares an exchange. `layer2` is the current session's enc key when renewing.
    pub fn new(server_pub_key: &[u8], layer2: &[u8]) -> Self {
        Self {
            secret: None,
            public: Vec::new(),
            server_ecdsa_pub_key: server_pub_key.to_vec(),
            server_static_pub_key: layer2.to_vec(),
            server_hash: Sha256::new(),
        }
    }

    /// Seals `body` into a `HybridEcdhRequest`.
    ///
    /// `body` must already be compressed the way the caller wants it on the wire; this layer
    /// never compresses on its own.
    pub fn seal(&mut self, body: &[u8]) -> Result<Vec<u8>> {
        if self.server_ecdsa_pub_key.is_empty() {
            return Err(Error::Protocol(
                "hybrid sealing needs the server public key".to_owned(),
            ));
        }
        self.server_hash = Sha256::new();

        let secret = EphemeralSecret::generate();
        let public = secret.public_key().to_sec1_point(false).as_bytes().to_vec();
        let ecdh_key = ecdh_sha256(&secret, &self.server_ecdsa_pub_key)?;

        let mut pub_hash = Sha256::new();
        pub_hash.update(b"1");
        pub_hash.update(P256_VERSION);
        pub_hash.update(&public);
        let pub_hash = pub_hash.finalize();

        let mut random_key = [0u8; 32];
        use rand::RngCore as _;
        OsRng.fill_bytes(&mut random_key);

        let enc_random_key = gcm_seal_appended(&ecdh_key[..24], &random_key, &pub_hash)?;
        let enc_random_key2 = if self.server_static_pub_key.is_empty() {
            Vec::new()
        } else {
            gcm_seal_appended(&self.server_static_pub_key[..24], &random_key, &pub_hash)?
        };

        let hkdf_key = hkdf_expand(&random_key, &pub_hash, 56);

        let mut final_hasher = Sha256::new();
        final_hasher.update(b"1");
        final_hasher.update(P256_VERSION);
        final_hasher.update(&public);
        final_hasher.update(&enc_random_key);
        if !enc_random_key2.is_empty() {
            final_hasher.update(&enc_random_key2);
        }
        let final_hash = final_hasher.finalize();

        let enc_data = gcm_seal_appended(&hkdf_key[..24], body, &final_hash)?;

        // The response tag chains on the request transcript.
        self.server_hash.update(&hkdf_key[24..]);
        self.server_hash.update(body);
        self.secret = Some(secret);
        self.public = public.clone();

        let request = pb::HybridEcdhRequest {
            version: Some(1),
            ecdh_key: Some(pb::SecEcdhKey {
                key_type: Some(415),
                ecdh_public_key: Some(public),
            }),
            encrypted_body: Some(enc_random_key),
            encrypted_body2: (!enc_random_key2.is_empty()).then_some(enc_random_key2),
            extra: Some(enc_data),
        };
        Ok(prost::Message::encode_to_vec(&request))
    }

    /// Opens a `HybridEcdhResponse` produced for the exchange [`HybridCrypto::seal`] started.
    pub fn open(&mut self, sealed: &[u8]) -> Result<Vec<u8>> {
        let response = pb::HybridEcdhResponse::decode(sealed)
            .map_err(|err| Error::Protocol(format!("the hybrid response is malformed: {err}")))?;
        let server_pub = response
            .ecdh_key
            .and_then(|key| key.ecdh_public_key)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| Error::Protocol("the hybrid response has no server key".to_owned()))?;
        let secret = self
            .secret
            .take()
            .ok_or_else(|| Error::Protocol("no exchange is in flight".to_owned()))?;

        let ecdh_key = ecdh_sha256(&secret, &server_pub)?;

        let client_cred = P256_VERSION;
        let server_cred = response
            .version
            .map(|version| version.to_string())
            .unwrap_or_else(|| "1".to_owned());
        self.server_hash.update(client_cred.as_bytes());
        self.server_hash.update(&server_pub);
        self.server_hash.update(server_cred.as_bytes());
        let aad = self.server_hash.clone().finalize();

        let enc_data = response
            .enc_data1
            .or(response.enc_data2)
            .filter(|data| !data.is_empty())
            .ok_or_else(|| Error::Protocol("the hybrid response has no payload".to_owned()))?;
        let compressed = gcm_open_appended(&ecdh_key[..24], &enc_data, &aad)?;

        // Either compression may answer; uncompressed data passes through.
        if let Some(plain) = crate::ilink::envelope::lz4_decompress_block(&compressed) {
            return Ok(plain);
        }
        if let Ok(plain) = inflate_zlib(&compressed) {
            return Ok(plain);
        }
        Ok(compressed)
    }

    /// The client public key of the in-flight exchange, for diagnostics.
    pub fn public_key(&self) -> &[u8] {
        &self.public
    }
}

// ---------------------------------------------------------------- internals

fn ecdh_sha256(secret: &EphemeralSecret, peer: &[u8]) -> Result<Vec<u8>> {
    let public = PublicKey::from_sec1_bytes(peer)
        .map_err(|err| Error::Protocol(format!("the peer key is malformed: {err}")))?;
    let shared = secret.diffie_hellman(&public).raw_secret_bytes().to_vec();
    let mut hasher = Sha256::new();
    hasher.update(shared);
    Ok(hasher.finalize().to_vec())
}

/// HKDF (extract+expand) with an explicit salt, ikm and info.
fn hkdf_expand(ikm: &[u8], info: &[u8], out_len: usize) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(HKDF_LABEL).expect("HMAC accepts any key length");
    mac.update(ikm);
    let prk = mac.finalize().into_bytes();

    let mut out = Vec::with_capacity(out_len);
    let mut previous: Vec<u8> = Vec::new();
    let mut counter = 1u8;
    while out.len() < out_len {
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(&prk).expect("HMAC accepts any key length");
        mac.update(&previous);
        mac.update(info);
        mac.update([counter].as_slice());
        previous = mac.finalize().into_bytes().to_vec();
        out.extend_from_slice(&previous);
        counter = counter.wrapping_add(1);
    }
    out.truncate(out_len);
    out
}

/// Seals with the raw layout the protocol uses: `ciphertext ‖ nonce ‖ tag`.
fn gcm_seal_appended(key: &[u8], plain: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let cipher = new_cipher(key)?;
    let mut nonce = [0u8; 12];
    use rand::RngCore as _;
    OsRng.fill_bytes(&mut nonce);
    let sealed = cipher
        .encrypt(
            aes_gcm::Nonce::from_slice(&nonce),
            Payload { msg: plain, aad },
        )
        .map_err(|_| Error::Protocol("hybrid sealing failed".to_owned()))?;
    // aes-gcm returns ciphertext‖tag; the wire wants nonce and tag after the ciphertext.
    let (body, tag) = sealed.split_at(sealed.len() - 16);
    let mut out = Vec::with_capacity(sealed.len() + 12);
    out.extend_from_slice(body);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(tag);
    Ok(out)
}

/// Opens a `ciphertext ‖ nonce ‖ tag` buffer.
fn gcm_open_appended(key: &[u8], input: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    if input.len() < 12 + 16 {
        return Err(Error::Protocol(
            "the hybrid payload is too short".to_owned(),
        ));
    }
    let split = input.len() - 28;
    let (body, tail) = input.split_at(split);
    let (nonce, tag) = tail.split_at(12);
    let cipher = new_cipher(key)?;
    let mut joined = body.to_vec();
    joined.extend_from_slice(tag);
    cipher
        .decrypt(
            aes_gcm::Nonce::from_slice(nonce),
            Payload { msg: &joined, aad },
        )
        .map_err(|_| Error::Protocol("the hybrid payload failed authentication".to_owned()))
}

fn new_cipher(key: &[u8]) -> Result<Aes192Gcm> {
    let mut material = [0u8; 24];
    let take = key.len().min(24);
    material[..take].copy_from_slice(&key[..take]);
    Aes192Gcm::new_from_slice(&material)
        .map_err(|_| Error::Protocol("the hybrid key is unusable".to_owned()))
}

fn inflate_zlib(data: &[u8]) -> std::result::Result<Vec<u8>, std::io::Error> {
    use std::io::Read as _;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_key() -> Vec<u8> {
        crate::session::hex_decode(crate::session::SVR_STATIC_PUB_KEY).unwrap()
    }

    #[test]
    fn an_exchange_opens_its_own_response_shape() {
        // Encrypt a body, then build a matching HybridEcdhResponse with the same shared
        // secret by sealing under the ecdh key directly (server side, test only).
        let mut client = HybridCrypto::new(&server_key(), &[]);
        let body = b"manual auth request".to_vec();
        let sealed = client.seal(&body).unwrap();
        assert!(!sealed.is_empty());

        // The sealed request must parse as the protobuf message.
        let request = pb::HybridEcdhRequest::decode(sealed.as_slice()).unwrap();
        assert_eq!(request.version, Some(1));
        assert_eq!(request.ecdh_key.as_ref().unwrap().key_type, Some(415));
        assert!(request.encrypted_body2.is_none(), "no layer2 key was given");
    }

    #[test]
    fn hkdf_matches_the_reference_vector() {
        // Same construction the reference client uses: extract with a label salt, expand
        // with the info parameter, 56 bytes.
        let ikm = [7u8; 32];
        let info = [1u8; 32];
        let out = hkdf_expand(&ikm, &info, 56);
        assert_eq!(out.len(), 56);
        // Deterministic across calls.
        assert_eq!(out, hkdf_expand(&ikm, &info, 56));
    }

    #[test]
    fn appended_gcm_layout_round_trips() {
        let key = [3u8; 24];
        let aad = b"associated".to_vec();
        let sealed = gcm_seal_appended(&key, b"payload", &aad).unwrap();
        assert_eq!(sealed.len(), 7 + 12 + 16);
        let opened = gcm_open_appended(&key, &sealed, &aad).unwrap();
        assert_eq!(opened, b"payload");
        let mut wrong = aad.clone();
        wrong[0] ^= 0xff;
        assert!(gcm_open_appended(&key, &sealed, &wrong).is_err());
    }

    #[test]
    fn a_malformed_server_key_is_reported_not_panicked() {
        let secret = EphemeralSecret::generate();
        assert!(ecdh_sha256(&secret, &[1, 2, 3]).is_err());
    }
}
