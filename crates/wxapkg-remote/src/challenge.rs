// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Pure-computation implementation of the WeChat cloud gateway tcbapi_challenge_start / challenge_end.
//!
//! Ported from yyb-go gatewayv3sdk/challenge (an algorithm chain verified byte for byte against five real wasm samples):
//! ```text
//! qbase_req = Base64( blob ‖ tail )
//! blob      = Header(147) ‖ 0x00000000(4) ‖ payLen(4,BE) ‖ payload
//! payload   = SM4-CBC( keySM4, iv=Header[1:17], PKCS7(body) )
//! body      = {"cursor":[],"signals":<device-fingerprint JSON>}
//! tail      = HMAC-SM3( keyHMAC, blob )
//! ```
//! Fingerprint JSON keys are alphabetical (serde_json uses BTreeMap by default); floats use JavaScript's shortest round-trip form.
//! Integer values (Go json omits the fraction for integral/zero fractions) go through js_num normalisation.
//!
//! Usage: `pass_risk` chains start→transfer→end→transfer and returns the risk-control result.

use serde_json::{json, Value};

// ---------------- Fixed keys / headers (constant samples compiled into the wasm) ----------------

// Keys and headers moved into the obfuscated sec layer (sec::key_sm4 / key_hmac / default_header).
fn key_sm4() -> [u8; 16] {
    crate::sec::key_sm4()
}
fn key_hmac() -> [u8; 16] {
    crate::sec::key_hmac()
}

fn default_header() -> Vec<u8> {
    crate::sec::default_header()
}

// ---------------- Deterministic PRNG (SplitMix64; distribution matches Go math/rand usage) ----------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E3779B97F4A7C15)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    /// [0, n)
    fn intn(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.next_u64() % n
    }
    /// Double in [0, 1) with a 53-bit mantissa (matches Go rand.Float64).
    fn float64(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) / 9007199254740992.0
    }
    fn float_range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + self.float64() * (hi - lo)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn seed_from(appid: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in appid.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^ now_ms().wrapping_mul(2654435761)
}

// ---------------- SM4 ----------------

const SM4_SBOX: [u8; 256] = [
    0xd6, 0x90, 0xe9, 0xfe, 0xcc, 0xe1, 0x3d, 0xb7, 0x16, 0xb6, 0x14, 0xc2, 0x28, 0xfb, 0x2c, 0x05,
    0x2b, 0x67, 0x9a, 0x76, 0x2a, 0xbe, 0x04, 0xc3, 0xaa, 0x44, 0x13, 0x26, 0x49, 0x86, 0x06, 0x99,
    0x9c, 0x42, 0x50, 0xf4, 0x91, 0xef, 0x98, 0x7a, 0x33, 0x54, 0x0b, 0x43, 0xed, 0xcf, 0xac, 0x62,
    0xe4, 0xb3, 0x1c, 0xa9, 0xc9, 0x08, 0xe8, 0x95, 0x80, 0xdf, 0x94, 0xfa, 0x75, 0x8f, 0x3f, 0xa6,
    0x47, 0x07, 0xa7, 0xfc, 0xf3, 0x73, 0x17, 0xba, 0x83, 0x59, 0x3c, 0x19, 0xe6, 0x85, 0x4f, 0xa8,
    0x68, 0x6b, 0x81, 0xb2, 0x71, 0x64, 0xda, 0x8b, 0xf8, 0xeb, 0x0f, 0x4b, 0x70, 0x56, 0x9d, 0x35,
    0x1e, 0x24, 0x0e, 0x5e, 0x63, 0x58, 0xd1, 0xa2, 0x25, 0x22, 0x7c, 0x3b, 0x01, 0x21, 0x78, 0x87,
    0xd4, 0x00, 0x46, 0x57, 0x9f, 0xd3, 0x27, 0x52, 0x4c, 0x36, 0x02, 0xe7, 0xa0, 0xc4, 0xc8, 0x9e,
    0xea, 0xbf, 0x8a, 0xd2, 0x40, 0xc7, 0x38, 0xb5, 0xa3, 0xf7, 0xf2, 0xce, 0xf9, 0x61, 0x15, 0xa1,
    0xe0, 0xae, 0x5d, 0xa4, 0x9b, 0x34, 0x1a, 0x55, 0xad, 0x93, 0x32, 0x30, 0xf5, 0x8c, 0xb1, 0xe3,
    0x1d, 0xf6, 0xe2, 0x2e, 0x82, 0x66, 0xca, 0x60, 0xc0, 0x29, 0x23, 0xab, 0x0d, 0x53, 0x4e, 0x6f,
    0xd5, 0xdb, 0x37, 0x45, 0xde, 0xfd, 0x8e, 0x2f, 0x03, 0xff, 0x6a, 0x72, 0x6d, 0x6c, 0x5b, 0x51,
    0x8d, 0x1b, 0xaf, 0x92, 0xbb, 0xdd, 0xbc, 0x7f, 0x11, 0xd9, 0x5c, 0x41, 0x1f, 0x10, 0x5a, 0xd8,
    0x0a, 0xc1, 0x31, 0x88, 0xa5, 0xcd, 0x7b, 0xbd, 0x2d, 0x74, 0xd0, 0x12, 0xb8, 0xe5, 0xb4, 0xb0,
    0x89, 0x69, 0x97, 0x4a, 0x0c, 0x96, 0x77, 0x7e, 0x65, 0xb9, 0xf1, 0x09, 0xc5, 0x6e, 0xc6, 0x84,
    0x18, 0xf0, 0x7d, 0xec, 0x3a, 0xdc, 0x4d, 0x20, 0x79, 0xee, 0x5f, 0x3e, 0xd7, 0xcb, 0x39, 0x48,
];
const SM4_FK: [u32; 4] = [0xa3b1bac6, 0x56aa3350, 0x677d9197, 0xb27022dc];

fn rotl32(x: u32, n: u32) -> u32 {
    x.rotate_left(n)
}

fn sm4_tau(a: u32) -> u32 {
    (SM4_SBOX[((a >> 24) & 0xff) as usize] as u32) << 24
        | (SM4_SBOX[((a >> 16) & 0xff) as usize] as u32) << 16
        | (SM4_SBOX[((a >> 8) & 0xff) as usize] as u32) << 8
        | (SM4_SBOX[(a & 0xff) as usize] as u32)
}

fn sm4_lenc(b: u32) -> u32 {
    b ^ rotl32(b, 2) ^ rotl32(b, 10) ^ rotl32(b, 18) ^ rotl32(b, 24)
}
fn sm4_lkey(b: u32) -> u32 {
    b ^ rotl32(b, 13) ^ rotl32(b, 23)
}

pub fn sm4_key_expand_pub(key: &[u8; 16]) -> [u32; 32] {
    sm4_key_expand(key)
}

fn sm4_key_expand(key: &[u8; 16]) -> [u32; 32] {
    let mut k = [0u32; 4];
    for i in 0..4 {
        k[i] = u32::from_be_bytes([key[i * 4], key[i * 4 + 1], key[i * 4 + 2], key[i * 4 + 3]])
            ^ SM4_FK[i];
    }
    let mut rk = [0u32; 32];
    for (i, slot) in rk.iter_mut().enumerate() {
        let mut ck = 0u32;
        for j in 0..4 {
            ck = (ck << 8) | ((((i * 4 + j) * 7) & 0xff) as u32);
        }
        let t = k[1] ^ k[2] ^ k[3] ^ ck;
        let v = k[0] ^ sm4_lkey(sm4_tau(t));
        *slot = v;
        k = [k[1], k[2], k[3], v];
    }
    rk
}

pub fn sm4_enc_block_pub(rk: &[u32; 32], inp: &[u8]) -> [u8; 16] {
    sm4_enc_block(rk, inp)
}

fn sm4_enc_block(rk: &[u32; 32], inp: &[u8]) -> [u8; 16] {
    let mut x = [0u32; 4];
    for i in 0..4 {
        x[i] = u32::from_be_bytes([inp[i * 4], inp[i * 4 + 1], inp[i * 4 + 2], inp[i * 4 + 3]]);
    }
    for key in rk {
        let t = x[1] ^ x[2] ^ x[3] ^ *key;
        let v = x[0] ^ sm4_lenc(sm4_tau(t));
        x = [x[1], x[2], x[3], v];
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&x[3].to_be_bytes());
    out[4..8].copy_from_slice(&x[2].to_be_bytes());
    out[8..12].copy_from_slice(&x[1].to_be_bytes());
    out[12..16].copy_from_slice(&x[0].to_be_bytes());
    out
}

pub fn sm4_cbc_encrypt_pub(key: &[u8; 16], iv: &[u8], data: &[u8]) -> Vec<u8> {
    sm4_cbc_encrypt(key, iv, data)
}

fn sm4_cbc_encrypt(key: &[u8; 16], iv: &[u8], data: &[u8]) -> Vec<u8> {
    let rk = sm4_key_expand(key);
    let mut out = vec![0u8; data.len()];
    let mut prev: [u8; 16] = iv.try_into().unwrap();
    for off in (0..data.len()).step_by(16) {
        let mut x = [0u8; 16];
        for j in 0..16 {
            x[j] = data[off + j] ^ prev[j];
        }
        let enc = sm4_enc_block(&rk, &x);
        out[off..off + 16].copy_from_slice(&enc);
        prev = enc;
    }
    out
}

fn pkcs7_pad(buf: &[u8]) -> Vec<u8> {
    let mut pad = 16 - (buf.len() % 16);
    if pad == 0 {
        pad = 16;
    }
    let mut out = buf.to_vec();
    out.resize(buf.len() + pad, pad as u8);
    out
}

// ---------------- SM3 / HMAC-SM3 ----------------

fn sm3_rotl(x: u32, n: u32) -> u32 {
    x.rotate_left(n)
}
fn sm3_p0(x: u32) -> u32 {
    x ^ sm3_rotl(x, 9) ^ sm3_rotl(x, 17)
}
fn sm3_p1(x: u32) -> u32 {
    x ^ sm3_rotl(x, 15) ^ sm3_rotl(x, 23)
}

fn sm3_block(state: &mut [u32; 8], p: &[u8]) {
    let mut w = [0u32; 68];
    let mut w1 = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([p[i * 4], p[i * 4 + 1], p[i * 4 + 2], p[i * 4 + 3]]);
    }
    for i in 16..68 {
        w[i] = sm3_p1(w[i - 16] ^ w[i - 9] ^ sm3_rotl(w[i - 3], 15))
            ^ sm3_rotl(w[i - 13], 7)
            ^ w[i - 6];
    }
    for i in 0..64 {
        w1[i] = w[i] ^ w[i + 4];
    }
    let (mut a, mut b, mut cc, mut d) = (state[0], state[1], state[2], state[3]);
    let (mut e, mut f, mut g, mut h) = (state[4], state[5], state[6], state[7]);
    for j in 0..64usize {
        let tj: u32 = if j < 16 { 0x79cc4519 } else { 0x7a879d8a };
        let ss1 = sm3_rotl(
            sm3_rotl(a, 12)
                .wrapping_add(e)
                .wrapping_add(sm3_rotl(tj, (j % 32) as u32)),
            7,
        );
        let ss2 = ss1 ^ sm3_rotl(a, 12);
        let (ff, gg) = if j < 16 {
            (a ^ b ^ cc, e ^ f ^ g)
        } else {
            ((a & b) | (a & cc) | (b & cc), (e & f) | (!e & g))
        };
        let tt1 = ff.wrapping_add(d).wrapping_add(ss2).wrapping_add(w1[j]);
        let tt2 = gg.wrapping_add(h).wrapping_add(ss1).wrapping_add(w[j]);
        d = cc;
        cc = sm3_rotl(b, 9);
        b = a;
        a = tt1;
        h = g;
        g = sm3_rotl(f, 19);
        f = e;
        e = sm3_p0(tt2);
    }
    state[0] ^= a;
    state[1] ^= b;
    state[2] ^= cc;
    state[3] ^= d;
    state[4] ^= e;
    state[5] ^= f;
    state[6] ^= g;
    state[7] ^= h;
}

fn sm3_sum(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x7380166f, 0x4914b2b9, 0x172442d7, 0xda8a0600, 0xa96f30bc, 0x163138aa, 0xe38dee4d,
        0xb0fb0e4e,
    ];
    let mut length = (data.len() as u64) * 8;
    let mut chunks = data.chunks_exact(64);
    for c in &mut chunks {
        sm3_block(&mut state, c);
    }
    let rem = chunks.remainder();
    let mut pad = [0u8; 128];
    pad[..rem.len()].copy_from_slice(rem);
    pad[rem.len()] = 0x80;
    // Length-field positions: [56..64] for one block, [120..128] for two (absolute indices; padding includes the remainder).
    let len_pos = if rem.len() < 56 { 56usize } else { 120usize };
    let be = length.to_be_bytes();
    pad[len_pos..len_pos + 8].copy_from_slice(&be);
    let total = len_pos + 8;
    let padded = &pad[..total];
    for c in padded.chunks_exact(64) {
        sm3_block(&mut state, c);
    }
    length = 0; // 占位避免未使用告警
    let _ = length;
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&state[i].to_be_bytes());
    }
    out
}

pub fn hmac_sm3_pub(key: &[u8], msg: &[u8]) -> [u8; 32] {
    hmac_sm3(key, msg)
}

fn hmac_sm3(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let key = if key.len() > 64 {
        sm3_sum(key).to_vec()
    } else {
        key.to_vec()
    };
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..key.len() {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }
    let mut inner = Vec::with_capacity(96);
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_sum = sm3_sum(&inner);
    let mut outer = Vec::with_capacity(96);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_sum);
    sm3_sum(&outer)
}

// ---------------- qbase_req assembly ----------------

fn build_qbase_req(header: &[u8], fp_json: &[u8]) -> String {
    let iv = &header[1..17];
    let mut body = Vec::with_capacity(fp_json.len() + 24);
    body.extend_from_slice(b"{\"cursor\":[],\"signals\":");
    body.extend_from_slice(fp_json);
    body.push(b'}');

    let padded = pkcs7_pad(&body);
    let payload = sm4_cbc_encrypt(&key_sm4(), iv, &padded);

    let mut blob = Vec::with_capacity(header.len() + 8 + payload.len() + 32);
    blob.extend_from_slice(header);
    blob.extend_from_slice(&[0, 0, 0, 0]);
    blob.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    blob.extend_from_slice(&payload);

    let tail = hmac_sm3(&key_hmac(), &blob);
    blob.extend_from_slice(&tail);
    base64_encode(&blob)
}

fn base64_encode(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
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
}

// ---------------- Device profiles (Android pool; matches real getSystemInfoSync output) ----------------

struct HwProfile {
    brand: &'static str,
    model: &'static str,
    renderer: &'static str,
    vendor: &'static str,
    pixel_ratio: f64,
    screen_w: i64,
    screen_h: i64,
    status_bar_h: i64,
    nav_height: i64,
    /// (OS version, UA build)
    oses: &'static [(&'static str, &'static str)],
}

const GPU_MALI: (f64, i64) = (1024.0, 4096); // point_size_max, max_texture_size
const GPU_ADRENO: (f64, i64) = (511.9375, 4096);

static ANDROID_DEVICES: &[HwProfile] = &[
    HwProfile {
        brand: "google",
        model: "Pixel 9 Pro",
        renderer: "Mali-G715",
        vendor: "ARM",
        pixel_ratio: 3.0,
        screen_w: 427,
        screen_h: 952,
        status_bar_h: 24,
        nav_height: 44,
        oses: &[("15", "AP4A.250105.002"), ("16", "BP4A.250605.014")],
    },
    HwProfile {
        brand: "google",
        model: "Pixel 8",
        renderer: "Mali-G715",
        vendor: "ARM",
        pixel_ratio: 2.625,
        screen_w: 411,
        screen_h: 914,
        status_bar_h: 24,
        nav_height: 44,
        oses: &[
            ("14", "UD1A.231105.004"),
            ("15", "AP4A.250105.002"),
            ("16", "BP4A.250605.014"),
        ],
    },
    HwProfile {
        brand: "samsung",
        model: "SM-S918B",
        renderer: "Adreno (TM) 740",
        vendor: "Qualcomm",
        pixel_ratio: 3.75,
        screen_w: 384,
        screen_h: 824,
        status_bar_h: 26,
        nav_height: 44,
        oses: &[
            ("13", "TP1A.220624.014"),
            ("14", "UP1A.231005.007"),
            ("15", "AP3A.241105.013"),
            ("16", "BP2A.250605.031.A3"),
        ],
    },
    HwProfile {
        brand: "samsung",
        model: "SM-S928B",
        renderer: "Adreno (TM) 750",
        vendor: "Qualcomm",
        pixel_ratio: 3.75,
        screen_w: 384,
        screen_h: 824,
        status_bar_h: 26,
        nav_height: 44,
        oses: &[
            ("14", "UP1A.231005.007"),
            ("15", "AP3A.241105.013"),
            ("16", "BP2A.250605.031.A3"),
        ],
    },
    HwProfile {
        brand: "xiaomi",
        model: "23127PN0CC",
        renderer: "Adreno (TM) 750",
        vendor: "Qualcomm",
        pixel_ratio: 2.875,
        screen_w: 417,
        screen_h: 929,
        status_bar_h: 40,
        nav_height: 44,
        oses: &[("14", "UKQ1.230917.001"), ("15", "AP3A.241105.013")],
    },
    HwProfile {
        brand: "oppo",
        model: "PHZ110",
        renderer: "Immortalis-G720",
        vendor: "ARM",
        pixel_ratio: 2.8125,
        screen_w: 449,
        screen_h: 988,
        status_bar_h: 36,
        nav_height: 44,
        oses: &[("14", "UKQ1.230917.001"), ("15", "AP3A.240617.008")],
    },
    HwProfile {
        brand: "vivo",
        model: "V2310A",
        renderer: "Immortalis-G720",
        vendor: "ARM",
        pixel_ratio: 2.8125,
        screen_w: 448,
        screen_h: 996,
        status_bar_h: 36,
        nav_height: 44,
        oses: &[("14", "UKQ1.230917.001"), ("15", "AP3A.240617.008")],
    },
    HwProfile {
        brand: "oneplus",
        model: "PJE110",
        renderer: "Adreno (TM) 740",
        vendor: "Qualcomm",
        pixel_ratio: 2.8125,
        screen_w: 449,
        screen_h: 988,
        status_bar_h: 36,
        nav_height: 44,
        oses: &[("14", "UKQ1.230917.001")],
    },
];

#[allow(dead_code)] // mirrors the protocol device record; some fields only document it
struct Device {
    spec_ver: String,
    build: String,
    hw: &'static HwProfile,
    chlid: String,
    sdk_version: String,
    wx_version: String,
    ua: String,
    fp: Value,
}

fn js_num(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

fn pick_device(rnd: &mut Rng, want_ver: &str) -> (&'static HwProfile, String, String) {
    let mut candidates: Vec<&HwProfile> = ANDROID_DEVICES
        .iter()
        .filter(|hw| {
            hw.oses
                .iter()
                .any(|(v, _)| *v == want_ver || v.starts_with(&format!("{want_ver}.")))
        })
        .collect();
    if candidates.is_empty() {
        candidates = ANDROID_DEVICES.iter().collect();
    }
    let hw = candidates[rnd.intn(candidates.len() as u64) as usize];
    let matched: Vec<(&str, &str)> = hw
        .oses
        .iter()
        .filter(|(v, _)| *v == want_ver || v.starts_with(&format!("{want_ver}.")))
        .map(|(v, b)| (*v, *b))
        .collect();
    let (ver, build) = if !matched.is_empty() {
        let m = matched[rnd.intn(matched.len() as u64) as usize];
        (m.0.to_owned(), m.1.to_owned())
    } else {
        let m = &hw.oses[rnd.intn(hw.oses.len() as u64) as usize];
        (m.0.to_owned(), m.1.to_owned())
    };
    (hw, ver.to_owned(), build)
}

fn build_ua(hw: &HwProfile, ver: &str, build: &str, wx_ver: &str) -> String {
    format!(
        "Mozilla/5.0 (Linux; Android {}; {} Build/{}) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/116.0.0.0 Mobile Safari/537.36 MMWebView/20240603 abi/arm64 WeChat/{} Language/zh_CN",
        ver, hw.model, build, wx_ver
    )
}

fn gen_samples(rnd: &mut Rng, base: i64, offset: i64, n: usize, kind: u8) -> Vec<Value> {
    let mut out = Vec::with_capacity(n);
    let mut ts = base + offset;
    // Initial idle state.
    let mut x = (rnd.float64() - 0.5) * 0.1;
    let mut z = (rnd.float64() - 0.5) * 0.1;
    let mut y = 9.8 + (rnd.float64() - 0.5) * 0.2;
    let mut alpha = rnd.float64() * 360.0;
    let mut beta = (rnd.float64() - 0.5) * 20.0;
    let mut gamma = (rnd.float64() - 0.5) * 20.0;
    let mut heading = rnd.float64() * 360.0;
    for _ in 0..n {
        let obj = match kind {
            0 => {
                x = walk(rnd, x, 0.12, 0.02);
                z = walk(rnd, z, 0.12, 0.02);
                y = 9.8 + walk(rnd, y - 9.8, 0.12, 0.03);
                json!({"ts": ts, "x": round4(x), "y": round4(y), "z": round4(z)})
            }
            1 => {
                let gx = walk(rnd, 0.0, 0.05, 0.015);
                let gy = walk(rnd, 0.0, 0.05, 0.015);
                let gz = walk(rnd, 0.0, 0.05, 0.015);
                json!({"ts": ts, "x": round4(gx), "y": round4(gy), "z": round4(gz)})
            }
            2 => {
                alpha = (alpha + (rnd.float64() - 0.5) * 6.0 + 360.0) % 360.0;
                beta = walk(rnd, beta, 12.0, 1.2);
                gamma = walk(rnd, gamma, 12.0, 1.2);
                json!({"ts": ts, "alpha": round4(alpha), "beta": round4(beta), "gamma": round4(gamma)})
            }
            _ => {
                heading = (heading + (rnd.float64() - 0.5) * 10.0 + 360.0) % 360.0;
                json!({"ts": ts, "direction": round4(heading)})
            }
        };
        out.push(obj);
        ts += 90 + rnd.intn(26) as i64;
    }
    out
}

fn walk(rnd: &mut Rng, cur: f64, limit: f64, step: f64) -> f64 {
    let mut c = cur + (rnd.float64() - 0.5) * step;
    if c > limit {
        c = limit;
    }
    if c < -limit {
        c = -limit;
    }
    c
}

fn round3(x: f64) -> f64 {
    let v = (x * 1000.0).round() / 1000.0;
    if v == 0.0 {
        0.0
    } else {
        v
    }
}
fn round4(x: f64) -> f64 {
    let v = (x * 10000.0).round() / 10000.0;
    if v == 0.0 {
        0.0
    } else {
        v
    }
}

const ROUTER_OUIS: [&str; 8] = [
    "50:C7:BF", "3C:37:86", "34:6B:D3", "D8:07:B6", "88:C3:97", "A4:FA:76", "64:CC:2E", "C8:0C:C8",
];

fn rand_bssid(rnd: &mut Rng) -> String {
    const HX: &[u8; 16] = b"0123456789ABCDEF";
    let oui = ROUTER_OUIS[rnd.intn(8) as usize];
    let mut s = String::new();
    for i in 0..3 {
        if i > 0 {
            s.push(':');
        }
        s.push(HX[rnd.intn(16) as usize] as char);
        s.push(HX[rnd.intn(16) as usize] as char);
    }
    format!("{}:{}", oui, s)
}

fn rand_ssid(rnd: &mut Rng) -> String {
    const CS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let suffix = |rnd: &mut Rng, n: usize| -> String {
        (0..n).map(|_| CS[rnd.intn(36) as usize] as char).collect()
    };
    match rnd.intn(4) {
        0 => format!("TP-LINK_5G_{}", suffix(rnd, 4)),
        1 => format!("HUAWEI-5G-{}", suffix(rnd, 4)),
        2 => format!("ChinaNet-{}", suffix(rnd, 4)),
        _ => format!("MERCURY_5G_{}", suffix(rnd, 4)),
    }
}

fn gpu_name(hw: &HwProfile) -> &'static str {
    if hw.renderer.contains("Adreno") {
        "adreno"
    } else if hw.renderer.contains("Immortalis") {
        "immortalis"
    } else {
        "mali"
    }
}

/// Generate one device (fingerprint F + UA) with the same structure as Go GenerateDeviceSeeded.
fn generate_device(
    appid: &str,
    want_ver: &str,
    sdk_bare: &str,
    _sdk_ts: u64,
    wx_version: &str,
) -> Device {
    let mut rnd = Rng::new(seed_from(appid));
    let (hw, ver, build) = pick_device(&mut rnd, want_ver);
    let chlid = new_chlid(&mut rnd);
    let ua = build_ua(hw, &ver, &build, wx_version);

    let now = now_ms() as i64;
    let mut sensor_off = || 1200 + rnd.intn(14800) as i64;
    let accel_off = sensor_off();
    let gyro_off = sensor_off();
    let motion_off = sensor_off();
    let compass_off = sensor_off();
    let mut max_off = accel_off;
    for v in [gyro_off, motion_off, compass_off] {
        if v > max_off {
            max_off = v;
        }
    }
    let sample_count = 48 + rnd.intn(6) as usize;
    let span = max_off + (sample_count as i64 - 1) * 115 + 240;
    let window_start = now - span - 2000 - rnd.intn(6000) as i64;

    let gpu_kind = gpu_name(hw);
    let (pt_max, _tex) = match gpu_kind {
        "adreno" => GPU_ADRENO,
        _ => GPU_MALI,
    };
    let _ = pt_max;

    let w = hw.screen_w;
    let h = hw.screen_h;
    let top = hw.status_bar_h;
    let bottom = h;
    let window_h = h - hw.status_bar_h - hw.nav_height;

    let fp = json!({
        "_chlid": chlid,
        "accelerometer": gen_samples(&mut rnd, window_start, accel_off, sample_count, 0),
        "accessibility": {"isOpen": false, "ts": (window_start + 800 + rnd.intn((span - 1600).max(1) as u64) as i64)},
        "appBaseInfo": {"SDKVersion": sdk_bare, "language": "zh_CN", "version": wx_version},
        "battery": {"isCharging": rnd.float64() < 0.28, "level": 15 + rnd.intn(86), "ts": (window_start + 800 + rnd.intn((span - 1600).max(1) as u64) as i64)},
        "brightness": {"ts": (window_start + 800 + rnd.intn((span - 1600).max(1) as u64) as i64), "value": round4(rnd.intn(256) as f64 / 255.0)},
        "compass": gen_samples(&mut rnd, window_start, compass_off, sample_count, 3),
        "connectedWifi": {"ts": (window_start + 800 + rnd.intn((span - 1600).max(1) as u64) as i64), "wifi": {"BSSID": rand_bssid(&mut rnd), "SSID": rand_ssid(&mut rnd)}},
        "gpu": {
            "aliased_point_size_range": [1.0, match gpu_kind { "adreno" => 511.9375, _ => 1024.0 }],
            "max_combined_texture_image_units": 32,
            "max_fragment_uniform_vectors": 256,
            "max_texture_image_units": 16,
            "max_texture_size": 4096,
            "max_varying_vectors": 15,
            "max_vertex_attribs": 16,
            "max_vertex_texture_image_units": 16,
            "max_vertex_uniform_vectors": 256,
            "max_viewport_dims": [4096, 4096],
            "renderbuffer_size": 4096,
            "renderer": hw.renderer,
            "texture_size": 4096,
            "vendor": hw.vendor,
        },
        "gyro": gen_samples(&mut rnd, window_start, gyro_off, sample_count, 1),
        "motion": gen_samples(&mut rnd, window_start, motion_off, sample_count, 2),
        "network": {"networkType": "wifi", "ts": (window_start + 800 + rnd.intn((span - 1600).max(1) as u64) as i64)},
        "perf": [window_start, 0, 0, span, 0, 0],
        "startWifi": {"ts": (window_start + 800 + rnd.intn((span - 1600).max(1) as u64) as i64)},
        "systemInfo": {
            "SDKVersion": sdk_bare,
            "brand": hw.brand,
            "language": "zh_CN",
            "model": hw.model,
            "pixelRatio": js_num(hw.pixel_ratio),
            "platform": "android",
            "safeArea": {"bottom": bottom, "height": bottom - top, "left": 0, "right": w, "top": top, "width": w},
            "screenHeight": h,
            "screenWidth": w,
            "system": format!("Android {}", ver),
            "version": wx_version,
        },
        "ur": [round3(rnd.float_range(3.0, 9.0)), round3(rnd.float_range(3.0, 9.0)), round3(rnd.float_range(3.0, 9.0)), round3(rnd.float64())],
        "window": {
            "screenHeight": h,
            "screenWidth": w,
            "statusBarHeight": hw.status_bar_h,
            "windowHeight": window_h,
            "windowWidth": w,
        },
    });

    Device {
        spec_ver: ver,
        build,
        hw,
        chlid,
        sdk_version: sdk_bare.to_owned(),
        wx_version: wx_version.to_owned(),
        ua,
        fp,
    }
}

fn new_chlid(rnd: &mut Rng) -> String {
    let ts = now_ms() & 0xFFFF_FFFF_FFFF;
    let mut b = [0u8; 16];
    b[0] = (ts >> 40) as u8;
    b[1] = (ts >> 32) as u8;
    b[2] = (ts >> 24) as u8;
    b[3] = (ts >> 16) as u8;
    b[4] = (ts >> 8) as u8;
    b[5] = ts as u8;
    b[6] = (rnd.next_u64() as u8 & 0x0F) | 0x70;
    b[7] = rnd.next_u64() as u8;
    b[8] = (rnd.next_u64() as u8 & 0x3F) | 0x80;
    b[9] = rnd.next_u64() as u8;
    let rb = rnd.next_u64().to_be_bytes();
    b[10] = rb[0];
    b[11] = rb[1];
    b[12] = rb[2];
    b[13] = rb[3];
    b[14] = rb[4];
    b[15] = rb[5];
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn cli_req_id(rnd: &mut Rng) -> String {
    format!("{}_{}", now_ms(), shortest_f64(rnd.float64()))
}

/// Shortest representation in the style of JS Math.random (same shape as Go %v).
fn shortest_f64(x: f64) -> String {
    let s = format!("{}", x);
    s
}

fn sdk_version_meta(sdk_bare: &str, sdk_ts: u64) -> String {
    format!(
        "wx-miniprogram-sdk/{} ({} platform/pyramid backend/wasm version/1.0.0)",
        sdk_bare, sdk_ts
    )
}

fn random_header(rnd: &mut Rng) -> Vec<u8> {
    let mut h = default_header();
    for i in 0..16 {
        h[1 + i] = rnd.next_u64() as u8;
    }
    h
}

/// challenge_start inner JSON (the WeChat JS request body).
#[allow(dead_code)] // kept as the reference implementation of the protocol
fn build_challenge_start(
    appid: &str,
    sdk_bare: &str,
    sdk_ts: u64,
    wx_version: &str,
    rnd: &mut Rng,
) -> (String, String, Value) {
    let chlid = new_chlid(rnd);
    let _ = chlid;
    let device = generate_device(appid, "16", sdk_bare, sdk_ts, wx_version);
    let chlid = device.chlid.clone();
    let data = json!({
        "cli_req_id": cli_req_id(rnd),
        "qbase_api_name": "tcbapi_challenge_start",
        "qbase_meta": {
            "chlid": chlid,
            "filter_user_info": false,
            "sdk_version": sdk_version_meta(sdk_bare, sdk_ts),
        },
        "qbase_options": {},
    });
    let body = json!({"api_name": "qbase_commapi", "data": data});
    (body.to_string(), chlid, device.fp)
}

/// challenge_end inner JSON (pure-computation fingerprint qbase_req).
#[allow(dead_code)] // kept as the reference implementation of the protocol
fn build_challenge_end(
    device_fp: &Value,
    chlid: &str,
    sdk_bare: &str,
    sdk_ts: u64,
    rnd: &mut Rng,
) -> String {
    let mut header = random_header(rnd);
    // Randomise the IV (header[1:17]).
    for i in 0..16 {
        header[1 + i] = rnd.next_u64() as u8;
    }
    let fp_json = device_fp.to_string();
    let qr = build_qbase_req(&header, fp_json.as_bytes());
    let _ = &mut header;
    let data = json!({
        "cli_req_id": cli_req_id(rnd),
        "qbase_api_name": "tcbapi_challenge_end",
        "qbase_meta": {
            "chlid": chlid,
            "filter_user_info": false,
            "sdk_version": sdk_version_meta(sdk_bare, sdk_ts),
        },
        "qbase_options": {},
        "qbase_req": qr,
    });
    json!({"api_name": "qbase_commapi", "data": data}).to_string()
}

// ---------------- High level: build and parse the start/end bodies ----------------

/// One challenge session: start/end bodies, session id and device UA (same seed, same fingerprint).
pub struct ChallengePair {
    /// Inner JSON for js-operatewxdata (the content of f3).
    pub inner_start: String,
    /// Inner JSON for challenge_end (pure-computation fingerprint qbase_req).
    pub inner_end: String,
    pub chlid: String,
    pub ua: String,
    pub system: String,
    pub sdk_meta: String,
}

/// Build a start+end request pair (same device, chlid and fingerprint; the chain is byte-verified against wasm samples).
pub fn build_pair(appid: &str, sdk_bare: &str, sdk_ts: u64, wx_version: &str) -> ChallengePair {
    let mut rnd = Rng::new(seed_from(&format!("{}|{}", appid, now_ms())));
    let device = generate_device(appid, "16", sdk_bare, sdk_ts, wx_version);
    let chlid = device.chlid.clone();
    let sdk_meta = sdk_version_meta(sdk_bare, sdk_ts);

    let start_data = json!({
        "cli_req_id": cli_req_id(&mut rnd),
        "qbase_api_name": "tcbapi_challenge_start",
        "qbase_meta": {
            "chlid": chlid,
            "filter_user_info": false,
            "sdk_version": sdk_meta,
        },
        "qbase_options": {},
    });
    let inner_start = json!({"api_name": "qbase_commapi", "data": start_data}).to_string();

    let mut header = random_header(&mut rnd);
    let fp_json = device.fp.to_string();
    let qr = build_qbase_req(&header, fp_json.as_bytes());
    let _ = &mut header;

    let end_data = json!({
        "cli_req_id": cli_req_id(&mut rnd),
        "qbase_api_name": "tcbapi_challenge_end",
        "qbase_meta": {
            "chlid": chlid,
            "filter_user_info": false,
            "sdk_version": sdk_meta,
        },
        "qbase_options": {},
        "qbase_req": qr,
    });
    let inner_end = json!({"api_name": "qbase_commapi", "data": end_data}).to_string();

    ChallengePair {
        inner_start,
        inner_end,
        chlid,
        ua: device.ua,
        system: format!("Android {}", device.spec_ver),
        sdk_meta,
    }
}

/// js-operatewxdata inner protobuf: {f2: appid, f3: json} (transfer injects f1).
pub fn inner_pb(appid: &str, inner_json: &str) -> Vec<u8> {
    let mut out = Vec::new();
    // f2 (len-delimited)
    out.push(0x12);
    push_len(&mut out, appid.len());
    out.extend_from_slice(appid.as_bytes());
    // f3 (len-delimited)
    out.push(0x1A);
    push_len(&mut out, inner_json.len());
    out.extend_from_slice(inner_json.as_bytes());
    out
}

fn push_len(out: &mut Vec<u8>, mut n: usize) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// Extract the f3 JSON text from an unwrapped js-operatewxdata response (protobuf).
pub fn resp_f3_json(peeled: &[u8]) -> Option<String> {
    let fields = parse_pb(peeled)?;
    for (f, v) in fields {
        if f == 3 {
            return String::from_utf8(v).ok();
        }
    }
    None
}

fn parse_pb(b: &[u8]) -> Option<Vec<(u32, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let (tag, ni) = read_varint(b, i)?;
        i = ni;
        let f = (tag >> 3) as u32;
        let wt = tag & 7;
        match wt {
            0 => {
                let (_, ni) = read_varint(b, i)?;
                i = ni;
            }
            1 => i += 8,
            2 => {
                let (len, ni) = read_varint(b, i)?;
                i = ni;
                if i + len as usize > b.len() {
                    return None;
                }
                out.push((f, b[i..i + len as usize].to_vec()));
                i += len as usize;
            }
            5 => i += 4,
            _ => return None,
        }
    }
    Some(out)
}

fn read_varint(b: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    let mut sh = 0u32;
    loop {
        let c = *b.get(i)?;
        v |= ((c & 0x7f) as u64) << sh;
        i += 1;
        if c & 0x80 == 0 {
            break;
        }
        sh += 7;
        if sh > 63 {
            return None;
        }
    }
    Some((v, i))
}

/// Extract `errcode` from js-operatewxdata response text (also escaped \"errcode\":N).
pub fn extract_errcode(text: &str) -> Option<i64> {
    find_errcode(text)
}

fn find_errcode(s: &str) -> Option<i64> {
    let needle = "errcode";
    let mut idx = 0usize;
    while let Some(p) = s[idx..].find(needle) {
        let at = idx + p + needle.len();
        let rest = s[at..].trim_start_matches(['\\', '"', ':', ' ']);
        let start = rest.find(|c: char| c.is_ascii_digit() || c == '-')?;
        let rest2 = &rest[start..];
        let end = rest2
            .find(|c: char| !c.is_ascii_digit() && c != '-')
            .unwrap_or(rest2.len());
        if let Ok(v) = rest2[..end].parse::<i64>() {
            return Some(v);
        }
        idx = at;
    }
    None
}

// ---------------- Self-test vectors ----------------

pub fn selftest_sm3_abc() -> [u8; 32] {
    sm3_sum(b"abc")
}

pub fn selftest_sm4_ecb() -> [u8; 16] {
    // GB/T 32905 standard vector: key = plain = 0123456789abcdeffedcba9876543210
    // Expected ciphertext: 681edf34d206965e86b3e94f536e4246
    let key: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32,
        0x10,
    ];
    let plain = key;
    let rk = sm4_key_expand(&key);
    sm4_enc_block(&rk, &plain)
}

/// Build a challenge_end inner JSON that passes risk control for the runtime's own chlid.
/// The server binds risk state to the chlid registered by challenge_start; end must use the same chlid
/// and a fingerprint (qbase_req) that validates. The wasm fingerprint fails, so this replaces it with the pure one.
pub fn build_end_for_chlid(chlid: &str, sdk_bare: &str, sdk_ts: u64, wx_version: &str) -> String {
    let mut rnd = Rng::new(seed_from(&format!("{}|{}", chlid, sdk_ts)));
    let mut device = generate_device(chlid, "16", sdk_bare, sdk_ts, wx_version);
    // Critical: _chlid in the fingerprint must equal qbase_meta.chlid (the one registered by challenge_start).
    if let Some(obj) = device.fp.as_object_mut() {
        obj.insert(
            "_chlid".to_owned(),
            serde_json::Value::String(chlid.to_owned()),
        );
    }
    let mut header = random_header(&mut rnd);
    let fp_json = device.fp.to_string();
    let qr = build_qbase_req(&header, fp_json.as_bytes());
    let _ = &mut header;
    let data = json!({
        "cli_req_id": cli_req_id(&mut rnd),
        "qbase_api_name": "tcbapi_challenge_end",
        "qbase_meta": {
            "chlid": chlid,
            "filter_user_info": false,
            "sdk_version": sdk_version_meta(sdk_bare, sdk_ts),
        },
        "qbase_options": {},
        "qbase_req": qr,
    });
    json!({"api_name": "qbase_commapi", "data": data}).to_string()
}
