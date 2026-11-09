// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! WXSS formatter.
//!
//! Explodes rule blocks so each declaration sits on its own line, normalising the
//! spacing inside `prop: value;` pairs. At-rules (`@media`, `@import`, `@keyframes`)
//! are handled recursively and unknown syntax is preserved verbatim.

use crate::beautify::FormatOptions;

/// Formats WXSS / CSS.
pub fn beautify_wxss(source: &str, options: &FormatOptions) -> String {
    let mut out = String::with_capacity(source.len() + source.len() / 4);
    format_block(source.trim(), 0, options, &mut out);
    let mut result = out.trim_end().to_owned();
    if !result.is_empty() {
        result.push('\n');
    }
    result
}

fn format_block(source: &str, level: usize, options: &FormatOptions, out: &mut String) {
    let indent = options.indent_unit();
    let mut cursor = 0usize;

    while cursor < source.len() {
        let rest = &source[cursor..];
        let trimmed_start = rest.trim_start();
        cursor += rest.len() - trimmed_start.len();
        if cursor >= source.len() {
            break;
        }

        // Comments and at-statements without a block.
        if source[cursor..].starts_with("/*") {
            let Some(end) = source[cursor..].find("*/") else {
                out.push_str(&indent.repeat(level));
                out.push_str(source[cursor..].trim_end());
                out.push('\n');
                return;
            };
            out.push_str(&indent.repeat(level));
            out.push_str(source[cursor..cursor + end + 2].trim());
            out.push('\n');
            cursor += end + 2;
            continue;
        }

        let Some(open) =
            find_unquoted(source, cursor, '{').or_else(|| find_unquoted(source, cursor, ';'))
        else {
            let tail = source[cursor..].trim();
            if !tail.is_empty() {
                out.push_str(&indent.repeat(level));
                out.push_str(tail);
                out.push('\n');
            }
            return;
        };

        if source.as_bytes()[open] == b';' {
            let stmt = source[cursor..=open].trim();
            if !stmt.is_empty() {
                out.push_str(&indent.repeat(level));
                out.push_str(&normalize_statement(stmt));
                out.push('\n');
            }
            cursor = open + 1;
            continue;
        }

        let selector = source[cursor..open].trim();
        let Some(close) = find_matching_brace(source, open) else {
            out.push_str(&indent.repeat(level));
            out.push_str(source[cursor..].trim_end());
            out.push('\n');
            return;
        };
        let body = source[open + 1..close].to_owned();

        if !selector.is_empty() {
            out.push_str(&indent.repeat(level));
            out.push_str(&normalize_selector(selector));
            out.push_str(" {\n");
        }

        if body.contains('{') && !body.contains(':') {
            // Nested at-rule without declarations (e.g. `@keyframes x { from {...} }`).
        }
        if body.contains('{') {
            format_block(
                &body,
                level + usize::from(!selector.is_empty()),
                options,
                out,
            );
        } else {
            emit_declarations(
                &body,
                level + usize::from(!selector.is_empty()),
                options,
                out,
            );
        }

        if !selector.is_empty() {
            out.push_str(&indent.repeat(level));
            out.push_str("}\n");
        }
        cursor = close + 1;
    }
}

fn emit_declarations(body: &str, level: usize, options: &FormatOptions, out: &mut String) {
    let indent = options.indent_unit();
    for piece in split_declarations(body) {
        let trimmed = piece.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push_str(&indent.repeat(level));
        let mut line = normalize_statement(trimmed);
        if !line.ends_with(';') {
            line.push(';');
        }
        out.push_str(&line);
        out.push('\n');
        let _ = options;
    }
}

fn split_declarations(body: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0i32;

    for ch in body.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => {
                quote = None;
                current.push(c);
            }
            (Some(_), c) => current.push(c),
            (None, c @ ('"' | '\'')) => {
                quote = Some(c);
                current.push(c);
            }
            (None, '(') => {
                depth += 1;
                current.push(ch);
            }
            (None, ')') => {
                depth -= 1;
                current.push(ch);
            }
            (None, ';') if depth <= 0 => {
                parts.push(std::mem::take(&mut current));
            }
            (None, c) => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

/// Collapses runs of whitespace and normalises `prop:value` to `prop: value`.
fn normalize_statement(stmt: &str) -> String {
    let collapsed = collapse_ws(stmt);
    match collapsed.split_once(':') {
        Some((prop, value)) if !prop.contains(' ') || prop.trim() == prop => {
            let value = value.trim();
            let prop = prop.trim();
            if value.is_empty() {
                format!("{prop}:")
            } else {
                format!("{prop}: {value}")
            }
        }
        _ => collapsed,
    }
}

fn normalize_selector(selector: &str) -> String {
    let collapsed = collapse_ws(selector);
    collapsed
        .split(',')
        .map(|part| part.trim().to_owned())
        .collect::<Vec<_>>()
        .join(",\n")
}

fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote: Option<char> = None;
    let mut last_space = false;
    for ch in text.chars() {
        match (quote, ch) {
            (Some(q), c) => {
                out.push(c);
                if c == q {
                    quote = None;
                }
                last_space = false;
            }
            (None, c @ ('"' | '\'')) => {
                quote = Some(c);
                out.push(c);
                last_space = false;
            }
            (None, c) if c.is_whitespace() => {
                if !last_space && !out.is_empty() {
                    out.push(' ');
                    last_space = true;
                }
            }
            (None, c) => {
                out.push(c);
                last_space = false;
            }
        }
    }
    out.trim().to_owned()
}

fn find_unquoted(source: &str, from: usize, needle: char) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (offset, ch) in source[from..].char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, c @ ('"' | '\'')) => quote = Some(c),
            (None, c) if c == needle => return Some(from + offset),
            _ => {}
        }
    }
    None
}

fn find_matching_brace(source: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (offset, ch) in source[open..].char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, c @ ('"' | '\'')) => quote = Some(c),
            (None, '{') => depth += 1,
            (None, '}') => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn fmt(src: &str) -> String {
        beautify_wxss(src, &FormatOptions::default())
    }

    #[test]
    fn explodes_rule_blocks() {
        let out = fmt(".a{color:#111;font-size:12px}");
        assert_eq!(out, ".a {\n  color: #111;\n  font-size: 12px;\n}\n");
    }

    #[test]
    fn keeps_rpx_units_intact() {
        let out = fmt(".a{padding:16rpx 24rpx}");
        assert!(out.contains("padding: 16rpx 24rpx;"), "{out}");
    }

    #[test]
    fn handles_at_media_nesting() {
        let out = fmt("@media (min-width:300px){.a{color:red}}");
        assert!(out.contains("@media (min-width:300px) {"), "{out}");
        assert!(out.contains("  .a {"), "{out}");
    }

    #[test]
    fn commas_in_selectors_break_onto_lines() {
        let out = fmt(".a,.b{color:red}");
        assert!(out.contains(".a,\n.b {"), "{out}");
    }

    #[test]
    fn quoted_semicolons_do_not_split_declarations() {
        let out = fmt(r#".a{content:"x;y";color:red}"#);
        assert!(out.contains(r#"content: "x;y";"#), "{out}");
        assert!(out.contains("color: red;"), "{out}");
    }
}
