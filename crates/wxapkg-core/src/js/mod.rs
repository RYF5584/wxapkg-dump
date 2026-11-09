// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! JavaScript tooling: a character-level lexer plus a miniature parser.
//!
//! Neither component aims for full ECMAScript compliance. They cover exactly the
//! syntax surface that WeChat's `wcc` compiler emits for Mini Programs:
//!
//! * the lexer recognises strings, template literals, comments and the `/`
//!   regular-expression-vs-division ambiguity, which is all that is needed to slice
//!   `define(...)` module bodies and `$gwx` render functions out of multi-megabyte
//!   bundles;
//! * the parser understands the statement and expression forms found in compiled
//!   templates (`var`/`let`/`const`, `if`/`else`, `try`/`catch`, `return`, function
//!   declarations and expressions, calls, member access, assignments, ternaries,
//!   arrays and object literals).

mod ast;
mod lexer;
mod parser;

pub use ast::{Ast, Lit, Stmt};
pub use lexer::{
    iter_top_level_calls, match_delimiter, split_top_level_args, tokenize, Token, TokenKind,
};
pub use parser::{parse, ParseError, Parser};

/// Unescapes a JavaScript string literal, including the surrounding quotes.
///
/// The input must be the raw source text as produced by the lexer. Backtick literals
/// are returned verbatim apart from the delimiters, mirroring the behaviour of the
/// WeChat runtime for template strings that contain no interpolation.
pub fn unescape_js_string(raw: &str) -> String {
    if raw.len() < 2 {
        return raw.to_owned();
    }
    let quote = raw.as_bytes()[0];
    if quote == b'`' {
        return raw[1..raw.len() - 1].to_owned();
    }
    let body = &raw[1..raw.len() - 1];
    if !body.contains('\\') {
        return body.to_owned();
    }

    let mut out = String::with_capacity(body.len());
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Defence in depth: every offset this loop produces is a boundary, but a caller can
        // hand in any byte offset and the function must never panic.
        if !body.is_char_boundary(i) {
            i += 1;
            continue;
        }
        if bytes[i] != b'\\' || i + 1 >= bytes.len() {
            let Some(ch) = body[i..].chars().next() else {
                break;
            };
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let next = bytes[i + 1];
        match next {
            b'u' => {
                // \u{XXXXXX}
                if i + 2 < bytes.len() && bytes[i + 2] == b'{' && body.is_char_boundary(i + 3) {
                    if let Some(close) = body[i + 3..].find('}') {
                        if let Ok(cp) = u32::from_str_radix(&body[i + 3..i + 3 + close], 16) {
                            if let Some(ch) = char::from_u32(cp) {
                                out.push(ch);
                                i = i + 3 + close + 1;
                                continue;
                            }
                        }
                    }
                }
                // The four hex digits are only ASCII when the literal is well formed; a
                // malformed escape such as a `\u` followed by a non-hex character must not slice through it.
                if i + 6 <= bytes.len() && body.is_char_boundary(i + 6) {
                    if let Ok(cp) = u32::from_str_radix(&body[i + 2..i + 6], 16) {
                        if let Some(ch) = char::from_u32(cp) {
                            out.push(ch);
                            i += 6;
                            continue;
                        }
                    }
                }
            }
            b'x' if i + 4 <= bytes.len() && body.is_char_boundary(i + 4) => {
                if let Ok(cp) = u32::from_str_radix(&body[i + 2..i + 4], 16) {
                    if let Some(ch) = char::from_u32(cp) {
                        out.push(ch);
                        i += 4;
                        continue;
                    }
                }
            }
            _ => {}
        }
        // `\` followed by anything unrecognised keeps that character verbatim. The advance
        // has to be one *character*, not two bytes: a non-ASCII escape is multi-byte, and skipping
        // only the backslash-plus-one-byte would land inside the character.
        out.push(simple_escape(next));
        i += 1 + body[i + 1..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

const fn simple_escape(b: u8) -> char {
    match b {
        b'n' => '\n',
        b't' => '\t',
        b'r' => '\r',
        b'b' => '\u{8}',
        b'f' => '\u{c}',
        b'v' => '\u{b}',
        b'0' => '\0',
        b'\n' => '\0',
        other => other as char,
    }
}

/// Parses a JavaScript numeric literal, tolerating `_` separators and radix prefixes.
pub fn parse_js_number(raw: &str) -> f64 {
    // A BigInt literal carries an `n` suffix that no numeric parser accepts.
    let s = raw.replace('_', "").trim_end_matches('n').to_owned();
    let lower = s.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix("0x") {
        return i64::from_str_radix(hex, 16).map_or(0.0, |v| v as f64);
    }
    if let Some(bin) = lower.strip_prefix("0b") {
        return i64::from_str_radix(bin, 2).map_or(0.0, |v| v as f64);
    }
    if let Some(oct) = lower.strip_prefix("0o") {
        return i64::from_str_radix(oct, 8).map_or(0.0, |v| v as f64);
    }
    s.parse::<f64>().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_common_sequences() {
        assert_eq!(unescape_js_string(r"'a\nb'"), "a\nb");
        assert_eq!(unescape_js_string(r#""\u4e2d\u6587""#), "中文");
        assert_eq!(unescape_js_string(r"'\x41'"), "A");
        assert_eq!(unescape_js_string("`raw`"), "raw");
    }

    #[test]
    fn malformed_escapes_never_split_a_character() {
        // These would panic on a byte slice that lands inside a multi-byte character.
        assert_eq!(unescape_js_string(r#"'\u4e2d'"#), "中");
        assert_eq!(unescape_js_string(r#"'\u中'"#), "u中");
        assert_eq!(unescape_js_string(r#"'\x中'"#), "x中");
        assert_eq!(unescape_js_string(r#"'\u{4e2d}'"#), "中");

        // Anything at all must be handled without panicking, including sequences that are
        // not escapes at all. The exact output of a malformed literal does not matter.
        for input in [
            "'\\中文'",
            r#"'\u'"#,
            r#"'\u{'"#,
            r#"'\u{zzz}'"#,
            r#"'\x'"#,
            r#"'\xzz'"#,
            "'\\'",
            "`模板${x}中`",
            r#"'\uFFFF'"#,
            r#"'\u{D800}'"#,
        ] {
            let _ = unescape_js_string(input);
        }
    }

    #[test]
    fn parses_radix_numbers() {
        assert_eq!(parse_js_number("0x1F"), 31.0);
        assert_eq!(parse_js_number("0b1011"), 11.0);
        assert_eq!(parse_js_number("1_000"), 1000.0);
        assert_eq!(parse_js_number("1.5e3"), 1500.0);
        assert_eq!(parse_js_number("1e-4"), 0.0001);
        assert_eq!(parse_js_number("123n"), 123.0);
    }
}
