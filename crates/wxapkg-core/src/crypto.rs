// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Static decryption of PC / macOS WeChat Mini Program packages.
//!
//! Windows WeChat stores applet bundles under `WeChat Files/Applet/wx<appid>/` and
//! macOS WeChat 4.x under `.../applet/packages/wx<appid>/<version>/`. Those files
//! start with the 6 byte `V1MMWX` magic and are encrypted with a key derived from
//! the *installation directory name* (`wx` + 16 hex digits), which is also the
//! Mini Program appid. No login session or device identity is involved.
//!
//! Layout of an encrypted bundle (`N = 1024`):
//!
//! ```text
//! [0, 6)          magic "V1MMWX"
//! [6, 6 + N)      AES-256-CBC(body[0..N]) with key = PBKDF2-HMAC-SHA1(key, "saltiest", 1000, 32)
//! [6 + N, EOF)    body[i] ^= key[key.len() - 2]  (fallback 0x66)
//! ```
//!
//! The first 1023 bytes of the decrypted head are the real container prefix; the
//! remaining byte is AES padding and is dropped.

use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use once_cell::sync::Lazy;
use regex::Regex;

use crate::error::{Error, Result};

/// Six byte magic that marks a statically encrypted bundle.
pub const V1MMWX_MAGIC: &[u8; 6] = b"V1MMWX";

/// Length of the AES-encrypted head segment (one AES block short of 1024 bytes).
const ENCRYPTED_HEAD_LEN: usize = 1024;

/// PBKDF2 salt shared by every platform build of WeChat.
const PBKDF2_SALT: &[u8] = b"saltiest";

/// PBKDF2 iteration count.
const PBKDF2_ROUNDS: u32 = 1000;

/// AES-CBC initialisation vector used by WeChat.
const AES_IV: &[u8; 16] = b"the iv: 16 bytes";
/// The IV, visible to this crate's own round-trip tests.
#[cfg(test)]
pub(crate) const AES_IV_FOR_TEST: &[u8; 16] = AES_IV;

/// XOR fallback key when the derived key is shorter than two bytes.
const XOR_FALLBACK: u8 = 0x66;

/// Matches `wx` followed by exactly 16 hexadecimal digits.
static WXID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"wx[0-9a-fA-F]{16}").expect("WXID_RE is a valid pattern"));

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// Returns `true` when `data` starts with the `V1MMWX` magic.
pub fn is_encrypted(data: &[u8]) -> bool {
    data.len() > 14 && data.starts_with(V1MMWX_MAGIC)
}

/// Extracts a `wx`-prefixed appid from an arbitrary path string.
///
/// Both `/` and `\` are accepted as separators so Windows paths work everywhere.
pub fn find_wxid(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    WXID_RE.find(&normalized).map(|m| m.as_str().to_owned())
}

/// Derives the 32 byte AES key from the installation directory name.
pub fn derive_key(wxid: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(wxid.as_bytes(), PBKDF2_SALT, PBKDF2_ROUNDS, &mut key);
    key
}

/// Decrypts a `V1MMWX` bundle in place, returning the plaintext container bytes.
///
/// `map_path` is only used to enrich error messages.
pub fn decrypt(wxid: &str, data: &[u8], map_path: &str) -> Result<Vec<u8>> {
    if !is_encrypted(data) {
        return Err(Error::DecryptionFailed {
            path: map_path.to_owned(),
            reason: "input does not carry the V1MMWX magic".to_owned(),
        });
    }
    let key = derive_key(wxid);

    let head_end = 6 + ENCRYPTED_HEAD_LEN;
    if data.len() < head_end {
        return Err(Error::DecryptionFailed {
            path: map_path.to_owned(),
            reason: format!(
                "truncated bundle: {} bytes is shorter than the {head_end} byte encrypted head",
                data.len()
            ),
        });
    }
    let mut head = data[6..head_end].to_vec();
    Aes256CbcDec::new_from_slices(&key, AES_IV)
        .expect("AES-256 key and IV lengths are fixed")
        .decrypt_padded_mut::<NoPadding>(&mut head)
        .map_err(|err| Error::DecryptionFailed {
            path: map_path.to_owned(),
            reason: format!("AES-CBC block decryption failed: {err}"),
        })?;

    // The plaintext head is 1023 bytes; the trailing byte is PKCS#7 padding that the
    // official implementation discards rather than validates.
    let xor_key = if wxid.len() >= 2 {
        wxid.as_bytes()[wxid.len() - 2]
    } else {
        XOR_FALLBACK
    };

    let mut out = Vec::with_capacity(data.len() - 6 - 1);
    out.extend_from_slice(&head[..ENCRYPTED_HEAD_LEN - 1]);
    out.extend(data[head_end..].iter().map(|b| b ^ xor_key));
    Ok(out)
}

/// Decrypts a bundle guessing the key from the file path.
///
/// This mirrors the behaviour users expect when pointing the tool at a WeChat data
/// directory: the directory name itself is the key.
pub fn decrypt_auto(path: &str, data: &[u8]) -> Result<Vec<u8>> {
    let wxid = find_wxid(path).ok_or_else(|| Error::MissingDecryptionKey {
        path: path.to_owned(),
        reason: "the path contains no `wx` + 16 hex digits segment; pass the key explicitly"
            .to_owned(),
    })?;
    let plain = decrypt(&wxid, data, path)?;
    if plain.first() != Some(&crate::container::MAGIC) {
        return Err(Error::DecryptionFailed {
            path: path.to_owned(),
            reason: format!(
                "after decryption with key {wxid} the leading byte is 0x{:02X} instead of 0xBE; \
                 the key is probably wrong",
                plain.first().copied().unwrap_or(0)
            ),
        });
    }
    Ok(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wxid_is_extracted_from_both_separator_styles() {
        assert_eq!(
            find_wxid(
                r"C:\Users\me\Documents\WeChat Files\Applet\wx0000000000000002\3\main.wxapkg"
            )
            .as_deref(),
            Some("wx0000000000000002")
        );
        assert_eq!(
            find_wxid("/Users/me/applet/packages/wx0000000000000004/7/app.wxapkg").as_deref(),
            Some("wx0000000000000004")
        );
        assert_eq!(find_wxid("/tmp/plain.wxapkg"), None);
    }

    #[test]
    fn pbkdf2_key_is_deterministic() {
        let a = derive_key("wx0000000000000002");
        let b = derive_key("wx0000000000000002");
        assert_eq!(a, b);
        assert_ne!(a, derive_key("wx0000000000000004"));
    }

    #[test]
    fn non_encrypted_input_is_rejected() {
        let err = decrypt("wx0000000000000002", b"\xBE\x00\x00\x00\x00", "x").unwrap_err();
        assert!(matches!(err, Error::DecryptionFailed { .. }));
    }
}
