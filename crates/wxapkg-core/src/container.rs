// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! `wxapkg-dump` container parsing, aligned 1:1 with the official WeChat reader.
//!
//! Ground truth (decompiled WeChat Android build `8072`):
//!
//! * `com.tencent.mm.plugin.appbrand.appcache.x8` — filesystem level reader
//! * `k71.d` (tag `MicroMsg.WxaPkgNormalImpl`) — format v0
//! * `k71.c` (tag `MicroMsg.WxaPkgMixedImpl`) — format v1
//!
//! Dispatch (`x8.a()`): seek to offset 1, read a big-endian `u32` version.
//! `0` selects format v0 (14 byte header), `1` selects format v1 (18 byte header),
//! anything else is rejected outright.
//!
//! **Format v0** — the de-facto standard:
//!
//! ```text
//! 0     u8     0xBE
//! 1..5  u32be  version == 0
//! 5..9  u32be  index length
//! 9..13 u32be  body length
//! 13    u8     0xED
//! 14..  index  u32be count, then count * { u32be name_len, name, u32be offset, u32be size }
//!       body   raw file contents, addressed by *absolute* offset, never compressed
//! ```
//!
//! **Format v1** — mixed packages where entries live in two segments:
//!
//! ```text
//! 0..17        header (0xBE, version == 1, index len, body len, ..., 0xED at 17)
//! 18..         index: u32be count, then count * { u32be name_len, name, u8 enc_type,
//!                       u32be relative_offset, u32be size }
//! ```
//!
//! `enc_type == 1` addresses the first segment (`header + index`), `enc_type == 0`
//! addresses the second one (`header + index + body`). Offsets are therefore
//! *relative*, which is the single most common source of bugs in naive unpackers.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::crypto;
use crate::error::{Error, IoContext, Result};

/// Leading magic byte of every container.
pub const MAGIC: u8 = 0xBE;
/// Trailing marker byte of every container header.
pub const END_MARK: u8 = 0xED;
/// Size of the format v0 header.
pub const HDR_V0: usize = 14;
/// Size of the format v1 header.
pub const HDR_V1: usize = 18;

/// Hard limit on a single entry name, matching the official reader's sanity check.
const MAX_NAME_LEN: u32 = 8192;

/// Byte sequences that indicate decompressed, human readable content.
const PLAIN_SIGNATURES: &[&[u8]] = &[
    b"{",
    b"<",
    b"[",
    b"/*",
    b"//",
    b"\n",
    b"\r",
    b" ",
    b"\t",
    b"\x89PNG",
    b"\xff\xd8\xff",
    b"GIF8",
    b"RIFF",
    b"<?xm",
    b"var ",
    b"function",
    b"const ",
    b"let ",
    b"import",
    b"export",
    b"wx.",
    b"@charset",
    b"/* v",
    b"data:",
];

/// A single file record inside a package.
#[derive(Debug, Clone, Serialize)]
pub struct PkgEntry {
    /// Entry name as stored in the index (usually a POSIX style relative path).
    pub name: String,
    /// Absolute offset of the payload inside the container buffer.
    pub offset: u64,
    /// Payload length in bytes.
    pub size: u64,
    /// v1 segment selector: `0` = second segment, `1` = first segment, `-1` = v0.
    pub enc_type: i8,
}

impl PkgEntry {
    /// Lower-cased extension including the leading dot, or an empty string.
    pub fn ext(&self) -> String {
        let base = self.name.rsplit('/').next().unwrap_or(&self.name);
        match base.rsplit_once('.') {
            Some((_, ext)) => format!(".{}", ext.to_ascii_lowercase()),
            None => String::new(),
        }
    }

    /// Whether this entry is a JavaScript file.
    pub fn is_js(&self) -> bool {
        self.name.to_ascii_lowercase().ends_with(".js")
    }
}

/// A parsed container.
#[derive(Debug, Clone)]
pub struct Wxapkg {
    /// Path the container was read from; may be synthetic for in-memory buffers.
    pub path: PathBuf,
    /// Raw container buffer (already decrypted when it was `V1MMWX`).
    pub data: Vec<u8>,
    /// Container format version.
    pub version: u32,
    /// Declared index region length.
    pub index_len: u32,
    /// Declared body region length.
    pub body_len: u32,
    /// Parsed entry table.
    pub entries: Vec<PkgEntry>,
    /// Non-fatal observations gathered during parsing.
    pub notes: Vec<String>,
}

/// Serialisable structural summary of a container.
#[derive(Debug, Clone, Serialize)]
pub struct PkgSummary {
    /// Container path.
    pub path: String,
    /// Format version.
    pub version: u32,
    /// Declared index length.
    #[serde(rename = "indexLen")]
    pub index_len: u32,
    /// Declared body length.
    #[serde(rename = "bodyLen")]
    pub body_len: u32,
    /// Total container size in bytes.
    pub size: u64,
    /// Number of entries.
    #[serde(rename = "fileCount")]
    pub file_count: usize,
    /// Parser notes.
    pub notes: Vec<String>,
}

impl Wxapkg {
    /// Returns `true` when `data` looks like a container at all.
    pub fn sniff(data: &[u8]) -> bool {
        data.len() > HDR_V0 && data.first() == Some(&MAGIC)
    }

    /// Parses a container from disk.
    ///
    /// When the file carries the `V1MMWX` magic it is decrypted transparently,
    /// using `wxid` when supplied or the path-derived key otherwise.
    pub fn open(path: impl AsRef<Path>, wxid: Option<&str>) -> Result<Self> {
        let path = path.as_ref();
        let data = std::fs::read(path).with_path(path)?;
        Self::parse(path.to_string_lossy().as_ref(), data, wxid)
    }

    /// Parses a container from an in-memory buffer.
    ///
    /// `label` is used for diagnostics and stored as [`Wxapkg::path`].
    pub fn parse(label: &str, mut data: Vec<u8>, wxid: Option<&str>) -> Result<Self> {
        if data.len() <= HDR_V0 {
            return Err(Error::invalid_container(
                label,
                format!("file is too small ({} bytes) to be a container", data.len()),
            ));
        }

        let mut decrypt_note = None;
        if crypto::is_encrypted(&data) {
            let key = wxid.map(str::to_owned).or_else(|| crypto::find_wxid(label));
            let Some(key) = key else {
                return Err(Error::MissingDecryptionKey {
                    path: label.to_owned(),
                    reason: "the container is V1MMWX encrypted but the label carries no \
                             `wx` + 16 hex digits segment; pass the key explicitly"
                        .to_owned(),
                });
            };
            let plain = crypto::decrypt(&key, &data, label)?;
            if plain.first() != Some(&MAGIC) {
                return Err(Error::DecryptionFailed {
                    path: label.to_owned(),
                    reason: format!(
                        "key {key} yields leading byte 0x{:02X} instead of 0xBE",
                        plain.first().copied().unwrap_or(0)
                    ),
                });
            }
            data = plain;
            decrypt_note = Some(format!("decrypted V1MMWX bundle with key {key}"));
        }

        if data.first() != Some(&MAGIC) {
            return Err(Error::invalid_container(
                label,
                format!(
                    "leading byte is 0x{:02X} instead of 0xBE",
                    data.first().copied().unwrap_or(0)
                ),
            ));
        }

        let version = read_u32(&data, 1, label)?;
        let mut pkg = match version {
            0 => Self::parse_v0(label, data)?,
            1 => Self::parse_v1(label, data)?,
            other => {
                return Err(Error::invalid_container(
                    label,
                    format!(
                    "unsupported container version {other} (the official reader rejects it too)"
                ),
                ))
            }
        };
        if let Some(note) = decrypt_note {
            pkg.notes.insert(0, note);
        }
        Ok(pkg)
    }

    fn parse_v0(label: &str, data: Vec<u8>) -> Result<Self> {
        if data.len() < HDR_V0 {
            return Err(Error::invalid_container(label, "truncated v0 header"));
        }
        let index_len = read_u32(&data, 5, label)?;
        let body_len = read_u32(&data, 9, label)?;
        if data[13] != END_MARK {
            return Err(Error::invalid_container(
                label,
                format!("v0 header end marker is 0x{:02X} instead of 0xED", data[13]),
            ));
        }

        let end = HDR_V0 + index_len as usize;
        if end > data.len() {
            return Err(Error::invalid_container(
                label,
                format!(
                    "index region overruns the file (14 + {index_len} > {})",
                    data.len()
                ),
            ));
        }

        let entries = read_index_v0(&data, HDR_V0, end, label)?;
        let mut pkg = Self {
            path: PathBuf::from(label),
            data,
            version: 0,
            index_len,
            body_len,
            entries,
            notes: Vec::new(),
        };

        // The official reader never validates body length, but a mismatch means the
        // file was tampered with or truncated, which is worth reporting.
        let declared = HDR_V0 + index_len as usize + body_len as usize;
        if declared != pkg.data.len() {
            pkg.notes.push(format!(
                "header declares 14+{index_len}+{body_len}={declared} bytes, file holds {} \
                 (difference {})",
                pkg.data.len(),
                pkg.data.len() as i64 - declared as i64
            ));
        }
        Ok(pkg)
    }

    fn parse_v1(label: &str, data: Vec<u8>) -> Result<Self> {
        if data.len() < HDR_V1 {
            return Err(Error::invalid_container(label, "truncated v1 header"));
        }
        let index_len = read_u32(&data, 5, label)?;
        let body_len = read_u32(&data, 9, label)?;
        if data[17] != END_MARK {
            return Err(Error::invalid_container(
                label,
                format!("v1 header end marker is 0x{:02X} instead of 0xED", data[17]),
            ));
        }

        let end = HDR_V1 + index_len as usize;
        if end > data.len() {
            return Err(Error::invalid_container(
                label,
                "index region overruns the file",
            ));
        }
        if end - HDR_V1 < 4 {
            return Err(Error::invalid_container(
                label,
                "v1 index region is shorter than 4 bytes",
            ));
        }

        let count = read_u32(&data, HDR_V1, label)?;
        let seg1_base = end;
        let seg2_base = end + body_len as usize;
        let mut pos = HDR_V1 + 4;
        let mut entries = Vec::with_capacity(count as usize);
        let mut notes = Vec::new();

        for i in 0..count {
            if pos + 4 > end {
                return Err(Error::invalid_container(
                    label,
                    format!("v1 index truncated at entry {i}/{count}"),
                ));
            }
            let name_len = read_u32(&data, pos, label)?;
            pos += 4;
            if name_len > MAX_NAME_LEN || pos + name_len as usize + 9 > end {
                return Err(Error::invalid_container(
                    label,
                    format!("v1 index entry {i} has an implausible name length ({name_len})"),
                ));
            }
            let name = decode_name(&data[pos..pos + name_len as usize]);
            pos += name_len as usize;
            let enc_type = data[pos] as i8;
            pos += 1;
            let rel = read_u32(&data, pos, label)?;
            let size = read_u32(&data, pos + 4, label)?;
            pos += 8;

            let offset = match enc_type {
                1 => seg1_base as u64 + rel as u64,
                0 => seg2_base as u64 + rel as u64,
                other => {
                    notes.push(format!(
                        "entry {name:?} has illegal enc_type={other}; treated as absolute offset"
                    ));
                    rel as u64
                }
            };
            entries.push(PkgEntry {
                name,
                offset,
                size: size as u64,
                enc_type,
            });
        }

        Ok(Self {
            path: PathBuf::from(label),
            data,
            version: 1,
            index_len,
            body_len,
            entries,
            notes,
        })
    }

    /// Reads the raw bytes of `entry`.
    ///
    /// With `auto_inflate`, entries that look like zlib streams (and only those
    /// whose output passes a plain-text sanity check) are decompressed. This never
    /// damages regular text that merely happens to start with `0x78`.
    pub fn read_entry(
        &mut self,
        entry: &PkgEntry,
        auto_inflate: bool,
        strict: bool,
    ) -> Result<Vec<u8>> {
        let start = entry.offset as usize;
        let end = start.saturating_add(entry.size as usize);
        if end > self.data.len() {
            if strict {
                return Err(Error::invalid_container(
                    self.path.to_string_lossy().to_string(),
                    format!(
                        "entry {:?} at {}+{} exceeds the {} byte file",
                        entry.name,
                        entry.offset,
                        entry.size,
                        self.data.len()
                    ),
                ));
            }
            return Ok(self.data[start.min(self.data.len())..].to_vec());
        }
        let blob = &self.data[start..end];
        if !auto_inflate || blob.len() < 2 {
            return Ok(blob.to_vec());
        }
        if blob[0] != 0x78 || !matches!(blob[1], 0x01 | 0x5E | 0x9C | 0xDA) {
            return Ok(blob.to_vec());
        }

        let inflated = try_inflate(blob);

        match inflated {
            Some(out) if looks_plain(&out[..out.len().min(16)]) => {
                self.notes.push(format!(
                    "{:?}: inflate {} -> {} bytes",
                    entry.name,
                    blob.len(),
                    out.len()
                ));
                Ok(out)
            }
            _ => Ok(blob.to_vec()),
        }
    }

    /// Convenience wrapper that clones the entry so the borrow checker stays happy.
    pub fn read_entry_owned(
        &mut self,
        index: usize,
        auto_inflate: bool,
        strict: bool,
    ) -> Result<Vec<u8>> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| Error::EntryNotFound {
                path: self.path.to_string_lossy().to_string(),
                name: format!("#{index}"),
            })?
            .clone();
        self.read_entry(&entry, auto_inflate, strict)
    }

    /// Looks up an entry by exact name.
    pub fn find_entry(&self, name: &str) -> Option<&PkgEntry> {
        let needle = name.trim_start_matches('/');
        self.entries
            .iter()
            .find(|e| e.name.trim_start_matches('/') == needle)
    }

    /// Iterates over all entries yielding `(name, bytes)`.
    pub fn iter_files(&mut self, auto_inflate: bool) -> Vec<(String, Vec<u8>)> {
        let entries = self.entries.clone();
        let mut out = Vec::with_capacity(entries.len());
        for entry in &entries {
            if let Ok(blob) = self.read_entry(entry, auto_inflate, false) {
                out.push((entry.name.clone(), blob));
            }
        }
        out
    }

    /// Structural summary suitable for JSON output.
    pub fn summary(&self) -> PkgSummary {
        PkgSummary {
            path: self.path.to_string_lossy().to_string(),
            version: self.version,
            index_len: self.index_len,
            body_len: self.body_len,
            size: self.data.len() as u64,
            file_count: self.entries.len(),
            notes: self.notes.clone(),
        }
    }

    /// Best-effort read of the embedded `app-config.json`, when present.
    pub fn app_config(&mut self) -> Option<serde_json::Value> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.name.ends_with("app-config.json"))
            .cloned()?;
        let blob = self.read_entry(&entry, false, false).ok()?;
        serde_json::from_slice(&blob).ok()
    }

    /// Best-effort display name declared by the bundle itself.
    ///
    /// WeChat does not ship the Mini Program's registered name, but many builds carry a
    /// navigation bar title or an explicit app name. When none is present the caller should
    /// fall back to the appid: the name simply is not in the container.
    pub fn display_name(&mut self) -> Option<String> {
        let config = self.app_config()?;
        let mut candidates: Vec<&str> = Vec::new();
        if let Some(title) = config
            .get("window")
            .and_then(|window| window.get("navigationBarTitleText"))
            .and_then(serde_json::Value::as_str)
        {
            candidates.push(title);
        }
        for key in ["appname", "appName", "name", "title"] {
            if let Some(value) = config.get(key).and_then(serde_json::Value::as_str) {
                candidates.push(value);
            }
        }
        if let Some(name) = config
            .get("plugin")
            .and_then(|plugin| plugin.get("name"))
            .and_then(serde_json::Value::as_str)
        {
            candidates.push(name);
        }

        candidates
            .into_iter()
            .map(str::trim)
            .find(|name| {
                !name.is_empty()
                    && name.chars().count() <= 64
                    // A title that is only punctuation or a placeholder is not a name.
                    && name.chars().any(|c| c.is_alphanumeric() || !c.is_ascii())
                    && !name.eq_ignore_ascii_case("wechat")
            })
            .map(str::to_owned)
    }

    /// Locates the launcher icon inside the container, if it ships one.
    ///
    /// `app-config.json` names the icon in most builds; when it does not, the smallest image
    /// in the container is used instead, because screenshots and marketing art are shipped
    /// much larger than the launcher asset.
    pub fn find_icon(&mut self) -> Option<(String, Vec<u8>)> {
        /// Nothing above this is an icon; a bigger file is art or a screenshot.
        const ICON_MAX_BYTES: u64 = 512 * 1024;

        if let Some(config) = self.app_config() {
            for candidate in icon_candidates(&config) {
                let entry = self.find_entry(&candidate).cloned().or_else(|| {
                    self.entries
                        .iter()
                        .find(|entry| entry.name.ends_with(&candidate))
                        .cloned()
                });
                if let Some(entry) = entry {
                    if entry.size <= ICON_MAX_BYTES {
                        if let Ok(bytes) = self.read_entry(&entry, false, false) {
                            if is_image(&bytes) {
                                return Some((entry.name.clone(), bytes));
                            }
                        }
                    }
                }
            }
        }

        let mut candidates: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| entry.size <= ICON_MAX_BYTES && has_image_extension(&entry.name))
            .cloned()
            .collect();
        candidates.sort_by_key(|entry| entry.size);
        for entry in candidates {
            if let Ok(bytes) = self.read_entry(&entry, false, false) {
                if is_image(&bytes) {
                    return Some((entry.name.clone(), bytes));
                }
            }
        }
        None
    }
}

/// Icon paths referenced by `app-config.json`, most specific first.
fn icon_candidates(config: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["logo", "icon", "image", "iconPath", "selectedIconPath"] {
        collect_reference(config.get(key), &mut out);
    }
    if let Some(list) = config
        .get("tabBar")
        .and_then(|tab| tab.get("list"))
        .and_then(serde_json::Value::as_array)
    {
        for item in list {
            collect_reference(item.get("iconPath"), &mut out);
            collect_reference(item.get("selectedIconPath"), &mut out);
        }
    }
    out
}

/// Records one image reference, skipping remote and inline ones.
fn collect_reference(value: Option<&serde_json::Value>, out: &mut Vec<String>) {
    let Some(text) = value.and_then(serde_json::Value::as_str) else {
        return;
    };
    if text.is_empty() || text.starts_with("http") || text.starts_with("data:") {
        return;
    }
    let name = text
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_owned();
    if has_image_extension(&name) && !out.contains(&name) {
        out.push(name);
    }
}

/// Whether a name carries an image extension.
pub fn has_image_extension(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Whether the bytes are one of the image formats the toolkit accepts as an icon.
pub fn is_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF8")
        || bytes.starts_with(b"\x00\x00\x01\x00")
        || (bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP")
}

/// Attempts a bounded zlib / raw-deflate inflate.
///
/// The 64 MiB ceiling keeps a hostile bundle from exhausting memory.
fn try_inflate(blob: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;

    let limit = 64 * 1024 * 1024;
    let mut out = Vec::new();
    if flate2::read::ZlibDecoder::new(blob)
        .take(limit)
        .read_to_end(&mut out)
        .is_ok()
        && !out.is_empty()
    {
        return Some(out);
    }
    out.clear();
    if flate2::read::DeflateDecoder::new(blob)
        .take(limit)
        .read_to_end(&mut out)
        .is_ok()
        && !out.is_empty()
    {
        return Some(out);
    }
    None
}

fn read_index_v0(data: &[u8], start: usize, end: usize, label: &str) -> Result<Vec<PkgEntry>> {
    if end - start < 4 {
        return Err(Error::invalid_container(
            label,
            "v0 index region is shorter than 4 bytes",
        ));
    }
    let count = read_u32(data, start, label)?;
    let mut pos = start + 4;
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        if pos + 4 > end {
            return Err(Error::invalid_container(
                label,
                format!("v0 index truncated at entry {i}/{count}"),
            ));
        }
        let name_len = read_u32(data, pos, label)?;
        pos += 4;
        if name_len > MAX_NAME_LEN || pos + name_len as usize + 8 > end {
            return Err(Error::invalid_container(
                label,
                format!("v0 index entry {i} has an implausible name length ({name_len})"),
            ));
        }
        let name = decode_name(&data[pos..pos + name_len as usize]);
        pos += name_len as usize;
        let offset = read_u32(data, pos, label)? as u64;
        let size = read_u32(data, pos + 4, label)? as u64;
        pos += 8;
        out.push(PkgEntry {
            name,
            offset,
            size,
            enc_type: -1,
        });
    }
    Ok(out)
}

fn read_u32(data: &[u8], offset: usize, label: &str) -> Result<u32> {
    let slice = data.get(offset..offset + 4).ok_or_else(|| {
        Error::invalid_container(label, format!("cannot read u32 at offset {offset}"))
    })?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Decodes an entry name the way the Android runtime does (`new String(bytes)`).
///
/// The platform default on Android is UTF-8; GBK appears in a few older bundles and
/// is therefore used as the first fallback.
pub fn decode_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            let (cow, _, had_errors) = encoding_rs::GBK.decode(raw);
            if !had_errors {
                return cow.into_owned();
            }
            String::from_utf8_lossy(raw).into_owned()
        }
    }
}

/// Heuristic: does this byte prefix look like plain text or a known file signature?
pub fn looks_plain(head: &[u8]) -> bool {
    if head.is_empty() {
        return false;
    }
    if PLAIN_SIGNATURES.iter().any(|sig| head.starts_with(sig)) {
        return true;
    }
    head.iter()
        .all(|&b| (0x20..0x7F).contains(&b) || matches!(b, b'\t' | b'\n' | b'\r'))
}

/// Reads a container from the first bytes of `data` without a full parse.
///
/// Returns `(version, index_len, body_len)`.
pub fn peek_header(data: &[u8]) -> Result<(u32, u32, u32)> {
    if data.len() < HDR_V0 + 1 {
        return Err(Error::invalid_container(
            "<buffer>",
            "buffer too short to hold a header",
        ));
    }
    if data[0] != MAGIC {
        return Err(Error::invalid_container(
            "<buffer>",
            format!("leading byte is 0x{:02X} instead of 0xBE", data[0]),
        ));
    }
    Ok((
        read_u32(data, 1, "<buffer>")?,
        read_u32(data, 5, "<buffer>")?,
        read_u32(data, 9, "<buffer>")?,
    ))
}

/// Normalises an entry name for cross-platform path joining.
pub fn normalize_entry_name(name: &str) -> Cow<'_, str> {
    if name.contains('\\') {
        Cow::Owned(name.replace('\\', "/"))
    } else {
        Cow::Borrowed(name)
    }
}

#[cfg(test)]
mod icon_tests {
    use super::*;

    #[test]
    fn icon_candidates_ignore_remote_urls() {
        let config = serde_json::json!({
            "logo": "https://example.com/logo.png",
            "tabBar": { "list": [ { "iconPath": "images/tab.png" } ] },
        });
        assert_eq!(icon_candidates(&config), vec!["images/tab.png".to_owned()]);
    }

    #[test]
    fn image_detection_uses_the_signature() {
        assert!(is_image(b"\x89PNG\r\n\x1a\n"));
        assert!(is_image(b"\xff\xd8\xff\xe0"));
        assert!(!is_image(b"not an image"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal but structurally valid v0 container in memory.
    fn synth_v0(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut index = Vec::new();
        index.extend_from_slice(&(files.len() as u32).to_be_bytes());
        let mut body = Vec::new();
        let mut base = HDR_V0 + 4 + files.iter().map(|(n, _)| 4 + n.len() + 8).sum::<usize>();
        for (name, payload) in files {
            index.extend_from_slice(&(name.len() as u32).to_be_bytes());
            index.extend_from_slice(name.as_bytes());
            index.extend_from_slice(&((base + body.len()) as u32).to_be_bytes());
            index.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            body.extend_from_slice(payload);
        }
        let mut out = Vec::new();
        out.push(MAGIC);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(index.len() as u32).to_be_bytes());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.push(END_MARK);
        out.extend_from_slice(&index);
        out.extend_from_slice(&body);
        let _ = &mut base;
        out
    }

    #[test]
    fn v0_roundtrip_reads_payloads() {
        let buf = synth_v0(&[
            ("app.js", b"App({})"),
            ("pages/index/index.js", b"Page({data:{a:1}})"),
        ]);
        let mut pkg = Wxapkg::parse("mem.wxapkg", buf, None).unwrap();
        assert_eq!(pkg.version, 0);
        assert_eq!(pkg.entries.len(), 2);
        let blob = pkg
            .read_entry(&pkg.entries[1].clone(), false, true)
            .unwrap();
        assert_eq!(blob, b"Page({data:{a:1}})");
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = vec![0u8; 64];
        buf[0] = 0x11;
        assert!(matches!(
            Wxapkg::parse("bad.wxapkg", buf, None),
            Err(Error::InvalidContainer { .. })
        ));
    }

    #[test]
    fn rejects_unknown_version() {
        let mut buf = vec![0u8; 64];
        buf[0] = MAGIC;
        buf[1..5].copy_from_slice(&7u32.to_be_bytes());
        assert!(matches!(
            Wxapkg::parse("v7.wxapkg", buf, None),
            Err(Error::InvalidContainer { .. })
        ));
    }

    #[test]
    fn gbk_names_fall_back_gracefully() {
        let (gbk, _, _) = encoding_rs::GBK.encode("页面/index.js");
        assert_eq!(decode_name(&gbk), "页面/index.js");
    }
}
