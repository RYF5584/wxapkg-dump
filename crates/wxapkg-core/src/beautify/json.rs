// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! JSON formatter used for Mini Program configuration files.

use crate::beautify::FormatOptions;

/// Pretty-prints JSON.
///
/// When the input cannot be parsed the original text is returned unchanged, because a
/// malformed `app-config.json` is still more useful raw than dropped.
pub fn beautify_json(source: &str, options: &FormatOptions) -> String {
    if source.trim().is_empty() {
        return source.to_owned();
    }
    match serde_json::from_str::<serde_json::Value>(source) {
        Ok(value) => {
            let indent: &[u8] = if options.use_tabs { b"\t" } else { b"  " };
            let mut buf = Vec::with_capacity(source.len() * 2);
            let formatter = serde_json::ser::PrettyFormatter::with_indent(indent);
            let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
            use serde::Serialize as _;
            if value.serialize(&mut ser).is_err() {
                return source.to_owned();
            }
            let mut text = String::from_utf8(buf).unwrap_or_else(|_| source.to_owned());
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text
        }
        Err(_) => source.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn pretty_prints_compact_json() {
        let out = beautify_json(r#"{"a":1,"b":[2,3]}"#, &FormatOptions::default());
        assert_eq!(out, "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ]\n}\n");
    }

    #[test]
    fn keeps_non_json_verbatim() {
        let src = "{not: valid json,,}";
        assert_eq!(
            beautify_json(src, &FormatOptions::default()),
            src.to_owned()
        );
    }
}
