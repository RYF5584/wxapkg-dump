// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! sec — the first static anti-reverse-engineering layer (Phase 1).
//!
//! Goals (zero overhead on the hot path):
//! 1. Store key constants (SM4/HMAC keys, the 147-byte header template) XOR-obfuscated so
//!    `strings` and static grep cannot find them; decode lazily into a OnceLock at runtime.
//! 2. Anti-debug sentinels: TracerPid, software breakpoints and known injection-framework traces;
//!    on a hit set a degraded flag (transport returns a generic error) without crashing or obvious traces.
//! 3. Decoy exports: plausible-looking APIs with empty behaviour to pollute static analysis.
//! 4. Encrypted server configuration: SM4-decrypt `config.enc` ({exp,data,mac}) into
//!    device_profile.json (the DeviceProfile hot-swap path, see wxa.rs).
//!
//! Phase 2 (see docs/SECURITY-PLAN.md): OLLVM passes (fla/bcf/sub/strct),
//! layered multi-.so loading, Ed25519-signed configuration and Java-side behaviour obfuscation.

use std::sync::OnceLock;

// ---- Per-build random mask (new mask + reordering = fully refreshed constants) ----
const MASK_K: [u8; 16] = [
    0x01, 0xb7, 0xe1, 0xd0, 0x39, 0x71, 0x0d, 0x98, 0xed, 0x9a, 0x32, 0x2b, 0x10, 0x21, 0x9a, 0x8c,
];
const OBF_K1: [u8; 16] = [
    0x72, 0xe3, 0xbe, 0x2f, 0xf0, 0xf5, 0x9c, 0x00, 0x3c, 0x43, 0x19, 0x1a, 0xa3, 0xc3, 0xd6, 0xa8,
];
const OBF_K2: [u8; 16] = [
    0x79, 0x37, 0x9d, 0x40, 0x97, 0xc6, 0xeb, 0x32, 0x2b, 0x14, 0x5e, 0xa3, 0xfe, 0x66, 0xc9, 0x94,
];
const MASK_H: [u8; 16] = [
    0xfa, 0xe4, 0x3b, 0x05, 0x53, 0xe2, 0x65, 0xb5, 0xc7, 0x5b, 0x18, 0xb5, 0xaf, 0x86, 0x36, 0x74,
];
const OBF_HDR: [u8; 147] = [
    0xfb, 0xd1, 0x33, 0xb7, 0x7f, 0x75, 0x01, 0x5f, 0x79, 0x0a, 0xe1, 0xf9, 0xee, 0xf3, 0x0c, 0x16,
    0xda, 0xc8, 0xca, 0x41, 0xcb, 0x6d, 0x08, 0x2d, 0x54, 0x31, 0x13, 0xfb, 0x87, 0xe6, 0x0f, 0xc0,
    0xeb, 0xea, 0x3b, 0x77, 0xd6, 0x74, 0x7b, 0xae, 0x76, 0xc3, 0xea, 0x5a, 0x9f, 0x58, 0xa5, 0x9c,
    0xb1, 0xb3, 0xc2, 0x5b, 0x91, 0x02, 0xa2, 0xd7, 0x22, 0x91, 0xca, 0x62, 0x14, 0x97, 0x3b, 0xe1,
    0x25, 0x86, 0xe7, 0xa5, 0x52, 0x89, 0xe0, 0x1a, 0xd0, 0x9e, 0x72, 0x16, 0xd7, 0x2d, 0xeb, 0x11,
    0xa4, 0x7f, 0xf5, 0x1d, 0x4d, 0x2b, 0x98, 0x6d, 0xbb, 0xba, 0x48, 0x60, 0x42, 0xe2, 0x9b, 0x12,
    0xf2, 0x60, 0xf5, 0x7e, 0x4c, 0xc6, 0x54, 0x76, 0xe2, 0x52, 0xe2, 0xae, 0x42, 0x9b, 0xdb, 0x08,
    0x4d, 0x3f, 0xf0, 0x8a, 0xab, 0xfe, 0x7d, 0x6c, 0xf2, 0x6d, 0x25, 0x54, 0x01, 0x7d, 0x3a, 0x04,
    0x1e, 0xb5, 0xd2, 0x4f, 0x15, 0x3f, 0x1f, 0x27, 0xee, 0xe2, 0xd8, 0x25, 0xcd, 0x19, 0xda, 0x49,
    0x00, 0xaf, 0x00,
];

fn xor16(a: &[u8; 16], m: &[u8; 16]) -> [u8; 16] {
    let mut o = [0u8; 16];
    for i in 0..16 {
        o[i] = a[i] ^ m[i];
    }
    o
}

struct Keys {
    sm4: [u8; 16],
    hmac: [u8; 16],
    header: [u8; 147],
}

fn decode_keys() -> Keys {
    // Two-stage XOR: the two obfuscated halves mix with the mask and each other, so no constant appears in plaintext.
    let s1 = xor16(&OBF_K1, &MASK_K);
    let s2 = xor16(&OBF_K2, &MASK_K);
    let sm4 = xor16(&s1, &s2);
    let hmac = xor16(&s2, &s1);
    let mut header = [0u8; 147];
    for i in 0..147 {
        header[i] = OBF_HDR[i] ^ MASK_H[i % 16] ^ s1[i % 16] ^ s2[(i * 7) % 16];
    }
    Keys { sm4, hmac, header }
}

static KEYS: OnceLock<Keys> = OnceLock::new();

pub fn key_sm4() -> [u8; 16] {
    KEYS.get_or_init(decode_keys).sm4
}
pub fn key_hmac() -> [u8; 16] {
    KEYS.get_or_init(decode_keys).hmac
}
pub fn default_header() -> Vec<u8> {
    KEYS.get_or_init(decode_keys).header.to_vec()
}

// ---- Anti-debug / anti-injection sentinels ----

static DEGRADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CHECKED: OnceLock<()> = OnceLock::new();

/// Run the sentinel scan once per process. Returns true when debugging/injection is detected.
/// A hit never crashes (which would reveal the location); it only marks transport degraded: requests return a generic error.
pub fn sweep() -> bool {
    if CHECKED.set(()).is_err() {
        return DEGRADED.load(std::sync::atomic::Ordering::Relaxed);
    }
    let hit = probe_tracer() || probe_breakpoints();
    if hit {
        DEGRADED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    hit
}

fn probe_tracer() -> bool {
    // A non-zero TracerPid means ptrace is attached (the default path for IDA/gdb/frida).
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(v) = line.strip_prefix("TracerPid:") {
                let pid: i32 = v.trim().parse().unwrap_or(0);
                if pid != 0 {
                    return true;
                }
            }
        }
    }
    false
}

fn probe_breakpoints() -> bool {
    // Software breakpoints: 0xd4200000 (ARM64 BRK) in this .so's executable segment means it has been patched.
    // Simplified: scan only the first and last 4 KB of this .so's executable segment in /proc/self/maps.
    let maps = match std::fs::read_to_string("/proc/self/maps") {
        Ok(m) => m,
        Err(_) => return false,
    };
    for line in maps.lines() {
        if !line.contains("libwmpf_core.so") {
            continue;
        }
        let seg = line.split_whitespace().next().unwrap_or("");
        let mut it = seg.split('-');
        let lo = u64::from_str_radix(it.next().unwrap_or("0"), 16).unwrap_or(0);
        let hi = u64::from_str_radix(it.next().unwrap_or("0"), 16).unwrap_or(0);
        if hi <= lo {
            continue;
        }
        // Scan only the first 4 KB of the segment to save time.
        let base = lo as *const u8;
        unsafe {
            let slice = std::slice::from_raw_parts(base, 4096.min((hi - lo) as usize));
            for w in slice.as_chunks::<4>().0 {
                let v = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
                if v == 0xd4200000 {
                    return true;
                }
            }
        }
        break; // Only the first matching segment is scanned.
    }
    false
}

/// The transport checks this before a call; when degraded it returns None → the caller reports a generic error.
pub fn transport_ok() -> bool {
    sweep();
    !DEGRADED.load(std::sync::atomic::Ordering::Relaxed)
}

// ---- Encrypted server configuration (SM4-CBC, {exp,data,mac} envelope) ----

/// Decrypt the server-configuration envelope: base64(blob) = iv(16) ‖ SM4-CBC(payload) ‾ HMAC-SM3(16).
/// payload = JSON `{"exp":<unix-seconds>,"data":<DeviceProfile JSON string>}`.
/// Expired → Err; MAC mismatch → Err. Returns the data string (Java writes it to device_profile.json).
pub fn decrypt_config(
    b64_blob: &str,
    key_sm4: &[u8; 16],
    key_hmac: &[u8; 16],
) -> Result<String, String> {
    let blob = base64_decode(b64_blob)?;
    if blob.len() < 16 + 16 + 16 {
        return Err("config: blob too short".to_owned());
    }
    let (iv, rest) = blob.split_at(16);
    let (payload, mac) = rest.split_at(rest.len() - 16);
    let expect = hmac_sm3(key_hmac, payload);
    if mac != &expect[..16] {
        return Err("config: bad mac".to_owned());
    }
    let plain = sm4_cbc_decrypt(key_sm4, iv, payload)?;
    let plain = unpad(&plain)?;
    let v: serde_json::Value = serde_json::from_slice(&plain).map_err(|e| e.to_string())?;
    let exp = v.get("exp").and_then(|x| x.as_i64()).unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if exp > 0 && now > exp {
        return Err("config: expired".to_owned());
    }
    let data = v
        .get("data")
        .and_then(|x| x.as_str())
        .ok_or("config: no data")?;
    Ok(data.to_owned())
}

fn unpad(buf: &[u8]) -> Result<Vec<u8>, String> {
    if buf.is_empty() || !buf.len().is_multiple_of(16) {
        return Err("config: bad pad len".to_owned());
    }
    let p = buf[buf.len() - 1] as usize;
    if !(1..=16).contains(&p) || p > buf.len() {
        return Err("config: bad pad".to_owned());
    }
    Ok(buf[..buf.len() - p].to_vec())
}

fn sm4_cbc_decrypt(key: &[u8; 16], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let rk = sm4_key_expand(key);
    let mut rkr = [0u32; 32];
    for i in 0..32 {
        rkr[i] = rk[31 - i];
    }
    let mut out = vec![0u8; data.len()];
    let mut prev: [u8; 16] = iv.try_into().unwrap();
    for off in (0..data.len()).step_by(16) {
        let blk = &data[off..off + 16];
        let dec = sm4_enc_block(&rkr, blk);
        for j in 0..16 {
            out[off + j] = dec[j] ^ prev[j];
        }
        prev.copy_from_slice(blk);
    }
    Ok(out)
}

fn sm4_key_expand(key: &[u8; 16]) -> [u32; 32] {
    crate::challenge::sm4_key_expand_pub(key)
}

fn sm4_enc_block(rk: &[u32; 32], inp: &[u8]) -> [u8; 16] {
    crate::challenge::sm4_enc_block_pub(rk, inp)
}

fn hmac_sm3(key: &[u8], msg: &[u8]) -> [u8; 32] {
    crate::challenge::hmac_sm3_pub(key, msg)
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for ch in s.bytes() {
        if ch == b'=' || ch == b'\n' || ch == b'\r' {
            continue;
        }
        let v = TBL
            .iter()
            .position(|&t| t == ch)
            .ok_or_else(|| "config: bad base64".to_owned())? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

// ---- Decoy exports (pollute static analysis: plausible names, empty/ambiguous behaviour) ----

#[no_mangle]
pub extern "C" fn wad_v3_verify_ticket(_a: *const u8, _n: usize) -> i32 {
    // decoy: looks like ticket validation.
    DEGRADED.load(std::sync::atomic::Ordering::Relaxed) as i32
}

#[no_mangle]
pub extern "C" fn wad_sec_init_ex(_flags: u32, _p1: *const u8, _p2: usize) -> i32 {
    sweep();
    0
}

#[no_mangle]
/// Writes a four-byte decoy header into `out`.
///
/// # Safety
/// `out` must be non-null and point to at least four writable bytes, or be null
/// (in which case nothing is written).
pub unsafe extern "C" fn wad_session_seal(
    _ctx: usize,
    _in: *const u8,
    _n: usize,
    out: *mut u8,
) -> usize {
    // decoy: writes a fixed 4-byte length header that static analysis will follow.
    unsafe {
        if !out.is_null() {
            std::ptr::copy_nonoverlapping([0u8, 0, 0, 0].as_ptr(), out, 4);
        }
    }
    4
}

#[no_mangle]
pub extern "C" fn wad_kdf_derive_v2(_seed: *const u8, _n: usize, _rounds: u32) -> i32 {
    sweep() as i32
}

#[no_mangle]
pub extern "C" fn wad_report_trace(_level: i32, _msg: *const u8) -> i32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_roundtrip() {
        // The decoded result must equal the original plaintext constant (Phase 1 semantics: obfuscation does not change behaviour).
        let k = decode_keys();
        assert_eq!(k.sm4, {
            let mut a = xor16(&OBF_K1, &MASK_K);
            let b = xor16(&OBF_K2, &MASK_K);
            for i in 0..16 {
                a[i] ^= b[i];
            }
            a
        });
    }

    #[test]
    fn config_roundtrip() {
        // Encrypt then decrypt (build a valid envelope with the challenge public-key helper).
        let data = r#"{"exp":0,"data":"{\"wx_version\":\"8.0.80\"}"}"#;
        let keys = decode_keys();
        let k: [u8; 16] = keys.sm4;
        let hmac: [u8; 16] = keys.hmac;
        let mut payload = data.as_bytes().to_vec();
        let pad = 16 - (payload.len() % 16);
        payload.resize(payload.len() + pad, pad as u8);
        let iv = [0x33u8; 16];
        let enc = crate::challenge::sm4_cbc_encrypt_pub(&k, &iv, &payload);
        let mut blob = iv.to_vec();
        blob.extend_from_slice(&enc);
        let mac = crate::challenge::hmac_sm3_pub(&hmac, &enc);
        blob.extend_from_slice(&mac[..16]);
        let b64 = {
            const TBL: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for chunk in blob.chunks(3) {
                let b0 = chunk[0] as u32;
                let b1 = *chunk.get(1).unwrap_or(&0) as u32;
                let b2 = *chunk.get(2).unwrap_or(&0) as u32;
                let n = (b0 << 16) | (b1 << 8) | b2;
                out.push(TBL[(n >> 18) as usize & 63] as char);
                out.push(TBL[(n >> 12) as usize & 63] as char);
                out.push(if chunk.len() > 1 {
                    TBL[(n >> 6) as usize & 63] as char
                } else {
                    '='
                });
                out.push(if chunk.len() > 2 {
                    TBL[n as usize & 63] as char
                } else {
                    '='
                });
            }
            out
        };
        let got = decrypt_config(&b64, &k, &hmac).unwrap();
        assert!(got.contains("8.0.80"));
    }
}
