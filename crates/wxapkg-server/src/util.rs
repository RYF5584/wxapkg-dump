// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Small shared helpers used by the newer handlers.
//!
//! These exist so that the inventory, artifact and environment endpoints agree on how a
//! query string is read, how a file's media type is guessed, and how a directory is
//! measured — three things that would otherwise drift apart between handlers.

use std::path::Path;

use walkdir::WalkDir;

/// Maximum directory depth explored when measuring or counting, matching discovery.
const MAX_DEPTH: usize = 14;

/// Decodes one `application/x-www-form-urlencoded` component.
fn decode_component(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 3 <= bytes.len() => {
                let pair = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match pair {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Splits a raw query string into decoded pairs, preserving repeated keys.
///
/// `serde_urlencoded` collapses repeated keys, but `?roots=/a&roots=/b` is the only sane
/// way to pass a list of directories, so the parsing is done by hand.
pub fn parse_query(raw: &str) -> Vec<(String, String)> {
    raw.trim_start_matches('?')
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((key, value)) => (decode_component(key), decode_component(value)),
            None => (decode_component(part), String::new()),
        })
        .collect()
}

/// All values supplied for `key`, in order.
pub fn query_all<'a>(pairs: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|value| !value.is_empty())
        .collect()
}

/// The last value supplied for `key`, which is what a caller expects when they repeat it.
pub fn query_value<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query_all(pairs, key).pop()
}

/// Reads a boolean flag, accepting the usual spellings.
pub fn query_flag(pairs: &[(String, String)], key: &str, default: bool) -> bool {
    match query_value(pairs, key) {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => default,
    }
}

/// Parses a numeric query parameter, ignoring an unparsable value.
pub fn query_number<T: std::str::FromStr>(pairs: &[(String, String)], key: &str) -> Option<T> {
    query_value(pairs, key).and_then(|value| value.parse().ok())
}

/// Guesses a media type from the content signature first, then from the extension.
///
/// Sniffing first matters because containers store assets without their extension being
/// authoritative, while a plain extension lookup is wrong for extensionless entries.
pub fn content_type_for(name: &str, head: &[u8]) -> &'static str {
    if head.starts_with(b"\x89PNG") {
        return "image/png";
    }
    if head.starts_with(b"\xff\xd8\xff") {
        return "image/jpeg";
    }
    if head.starts_with(b"GIF8") {
        return "image/gif";
    }
    if head.len() > 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        return "image/webp";
    }
    if head.starts_with(b"\x00\x00\x01\x00") {
        return "image/x-icon";
    }

    let lower = name.to_ascii_lowercase();
    match lower.rsplit_once('.').map(|(_, ext)| ext) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json; charset=utf-8",
        Some("js" | "mjs" | "wxs") => "text/javascript; charset=utf-8",
        Some("css" | "wxss") => "text/css; charset=utf-8",
        Some("wxml") => "text/xml; charset=utf-8",
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("xml") => "text/xml; charset=utf-8",
        Some("txt" | "md" | "map") => "text/plain; charset=utf-8",
        Some("zip") => "application/zip",
        Some("wxapkg") => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

/// Whether a byte prefix looks binary.
///
/// A NUL byte is the cheapest reliable tell: no text format this tool produces contains one.
pub fn looks_binary(head: &[u8]) -> bool {
    head.contains(&0)
}

/// Recursive size of a directory, in bytes.
pub fn dir_size(root: &Path) -> u64 {
    WalkDir::new(root)
        .max_depth(MAX_DEPTH)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

/// Modification time of `path` as a Unix timestamp.
pub fn modified_at(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|delta| delta.as_secs() as i64)
}

/// Current Unix timestamp.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_keys_are_preserved() {
        let pairs = parse_query("appid=wx1&roots=/a&roots=/b&limit=3");
        assert_eq!(query_all(&pairs, "roots"), vec!["/a", "/b"]);
        assert_eq!(query_value(&pairs, "appid"), Some("wx1"));
        assert_eq!(query_number::<usize>(&pairs, "limit"), Some(3));
    }

    #[test]
    fn percent_escapes_are_decoded() {
        let pairs = parse_query("dir=%2Ftmp%2Fa%20b");
        assert_eq!(query_value(&pairs, "dir"), Some("/tmp/a b"));
    }

    #[test]
    fn flags_accept_the_usual_spellings() {
        let pairs = parse_query("includeRaw=1");
        assert!(query_flag(&pairs, "includeRaw", false));
        assert!(!query_flag(&pairs, "missing", false));
        assert!(query_flag(&pairs, "missing", true));
    }

    #[test]
    fn media_types_come_from_the_signature_first() {
        assert_eq!(content_type_for("logo.bin", b"\x89PNG\r\n"), "image/png");
        assert_eq!(
            content_type_for("app.js", b"var x"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type_for("blob", b"\x00\x01"),
            "application/octet-stream"
        );
    }

    #[test]
    fn binary_sniffing_sees_nul_bytes() {
        assert!(looks_binary(b"abc\0def"));
        assert!(!looks_binary(b"abc def"));
    }
}
