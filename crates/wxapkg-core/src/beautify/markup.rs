// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! WXML formatter.
//!
//! Re-indents a tag stream and normalises attribute spacing. The formatter is
//! deliberately conservative: text content that contains no tag is kept on a single
//! line, and `<block>` wrappers are not removed, because WXML semantics depend on
//! explicit block boundaries.

use crate::beautify::FormatOptions;

/// Formats WXML / HTML-ish markup.
pub fn beautify_wxml(source: &str, options: &FormatOptions) -> String {
    let mut out = String::with_capacity(source.len() + source.len() / 3);
    let mut level: usize = 0;
    let indent = options.indent_unit();

    for chunk in split_markup(source) {
        match chunk {
            Chunk::Tag {
                raw,
                closing,
                self_closing,
                tag,
            } => {
                if closing {
                    level = level.saturating_sub(1);
                }
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&indent.repeat(level));
                out.push_str(&normalize_tag(&raw, options));
                out.push('\n');
                if !closing && !self_closing && !is_void_tag(&tag) {
                    level += 1;
                }
            }
            Chunk::Text(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                for line in trimmed.split('\n') {
                    if line.trim().is_empty() {
                        continue;
                    }
                    out.push_str(&indent.repeat(level));
                    out.push_str(line.trim_end());
                    out.push('\n');
                }
            }
            Chunk::Comment(raw) => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&indent.repeat(level));
                out.push_str(raw.trim());
                out.push('\n');
            }
            Chunk::Doctype(raw) => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&indent.repeat(level));
                out.push_str(raw.trim());
                out.push('\n');
            }
        }
    }

    let trimmed = out.trim_end();
    let mut result = trimmed.to_owned();
    if !result.is_empty() {
        result.push('\n');
    }
    result
}

enum Chunk {
    Tag {
        raw: String,
        closing: bool,
        self_closing: bool,
        tag: String,
    },
    Text(String),
    Comment(String),
    Doctype(String),
}

fn is_void_tag(tag: &str) -> bool {
    matches!(
        tag,
        "image" | "input" | "br" | "hr" | "img" | "import" | "include" | "wxs"
    )
}

/// Splits markup into tags, text, comments and doctypes.
fn split_markup(source: &str) -> Vec<Chunk> {
    let bytes = source.as_bytes();
    let mut chunks = Vec::new();
    let mut i = 0usize;
    let mut text_start = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if text_start < i {
            chunks.push(Chunk::Text(source[text_start..i].to_owned()));
        }
        let rest = &source[i..];

        if let Some(len) = comment_len(rest) {
            chunks.push(Chunk::Comment(source[i..i + len].to_owned()));
            i += len;
            text_start = i;
            continue;
        }
        if rest.len() > 2 && (rest[1..].starts_with('!') || rest[1..].starts_with('?')) {
            let end = find_tag_end(rest).unwrap_or(rest.len().saturating_sub(1));
            chunks.push(Chunk::Doctype(source[i..i + end + 1].to_owned()));
            i += end + 1;
            text_start = i;
            continue;
        }

        let Some(end) = find_tag_end(rest) else {
            i += 1;
            continue;
        };
        let raw = source[i..i + end + 1].to_owned();
        let inner = raw[1..raw.len() - 1].trim();
        let closing = inner.starts_with('/');
        let body = inner.trim_start_matches('/').trim();
        let self_closing = body.ends_with('/') || raw.starts_with("</");
        let tag = body
            .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .next()
            .unwrap_or("")
            .to_owned();
        chunks.push(Chunk::Tag {
            raw,
            closing,
            self_closing,
            tag,
        });
        i += end + 1;
        text_start = i;
    }
    if text_start < bytes.len() {
        chunks.push(Chunk::Text(source[text_start..].to_owned()));
    }
    chunks
}

fn comment_len(rest: &str) -> Option<usize> {
    if !rest.starts_with("<!--") {
        return None;
    }
    rest.find("-->").map(|end| end + 3)
}

/// Finds the offset of the `>` that closes a tag, skipping quoted attribute values.
fn find_tag_end(rest: &str) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut brace_depth = 0i32;
    for (offset, byte) in rest.as_bytes().iter().enumerate() {
        match (quote, byte) {
            (Some(q), b) if b == &q => quote = None,
            (Some(_), _) => {}
            (None, b @ (b'"' | b'\'')) => quote = Some(*b),
            (None, b'{') => brace_depth += 1,
            (None, b'}') => brace_depth -= 1,
            (None, b'>') if brace_depth <= 0 => return Some(offset),
            _ => {}
        }
    }
    None
}

/// Normalises whitespace inside a tag without touching attribute values.
fn normalize_tag(raw: &str, _options: &FormatOptions) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with("<!--") {
        return trimmed.to_owned();
    }
    let has_angle = trimmed.starts_with('<') && trimmed.ends_with('>');
    if !has_angle {
        return trimmed.to_owned();
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    let (closing, inner) = match inner.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, inner),
    };
    let self_closing = inner.trim_end().ends_with('/');
    let body = inner.trim_end().trim_end_matches('/').trim();

    // Attribute granularity: keep `name="value"` together even if the value has spaces.
    let mut attributes: Vec<String> = Vec::new();
    let mut pending = String::new();
    let mut quote: Option<char> = None;
    for ch in body.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => {
                quote = None;
                pending.push(c);
            }
            (Some(_), c) => pending.push(c),
            (None, c @ ('"' | '\'')) => {
                quote = Some(c);
                pending.push(c);
            }
            (None, c) if c.is_whitespace() => {
                if !pending.is_empty() {
                    attributes.push(std::mem::take(&mut pending));
                }
            }
            (None, c) => pending.push(c),
        }
    }
    if !pending.is_empty() {
        attributes.push(pending);
    }

    if closing {
        return format!("</{}>", attributes.first().cloned().unwrap_or_default());
    }
    if attributes.is_empty() {
        return format!("<{} />", body);
    }
    let name = attributes.remove(0);
    let mut out = format!("<{name}");
    for attr in attributes {
        out.push(' ');
        out.push_str(&attr);
    }
    if self_closing {
        out.push_str(" />");
    } else {
        out.push('>');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn fmt(src: &str) -> String {
        beautify_wxml(src, &FormatOptions::default())
    }

    #[test]
    fn indents_nested_elements() {
        let out = fmt("<view><text>hi</text></view>");
        assert_eq!(out, "<view>\n  <text>\n    hi\n  </text>\n</view>\n");
    }

    #[test]
    fn self_closing_elements_do_not_open_a_scope() {
        let out = fmt("<view><image src=\"a.png\"/></view>");
        assert_eq!(out, "<view>\n  <image src=\"a.png\" />\n</view>\n");
    }

    #[test]
    fn braces_inside_attributes_do_not_close_the_tag() {
        let out = fmt(r#"<view class="{{ a > b ? 'x' : 'y' }}">t</view>"#);
        assert!(out.contains(r#"class="{{ a > b ? 'x' : 'y' }}""#), "{out}");
    }

    #[test]
    fn comments_are_preserved() {
        let out = fmt("<view><!-- note --></view>");
        assert!(out.contains("<!-- note -->"), "{out}");
    }

    #[test]
    fn void_tags_are_recognised() {
        let out = fmt("<view><import src=\"a.wxml\"></view>");
        assert!(out.contains("<import src=\"a.wxml\">"), "{out}");
    }
}
