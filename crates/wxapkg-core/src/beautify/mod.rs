// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Source beautification.
//!
//! Decompilation recovers *correct* code, but the JavaScript extracted from
//! `app-service.js` is whatever the developer uploaded — frequently minified or
//! machine-generated. The formatters here turn that into something a human can read
//! without changing semantics:
//!
//! * [`js`] re-indents and re-spaces a token stream (a mini `js-beautify`);
//! * [`markup`] normalises WXML tag nesting and attribute lists;
//! * [`style`] explodes WXSS rule blocks;
//! * [`json`] is a thin wrapper over `serde_json`'s pretty printer.
//!
//! Every formatter is *lossless* with respect to the token stream: no token is added,
//! removed or reordered.

pub mod js;
pub mod json;
pub mod markup;
pub mod style;

use serde::{Deserialize, Serialize};

pub use js::beautify_js;
pub use json::beautify_json;
pub use markup::beautify_wxml;
pub use style::beautify_wxss;

/// Formatting options shared by every beautifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FormatOptions {
    /// Number of spaces per indentation level.
    pub indent_width: usize,
    /// Use hard tabs instead of spaces.
    pub use_tabs: bool,
    /// Preferred maximum line width before a long construct is broken up.
    pub max_width: usize,
    /// Reorder attributes on WXML elements (`class`, `style`, `id` first).
    pub sort_wxml_attrs: bool,
    /// Put each WXSS declaration on its own line.
    pub style_one_decl_per_line: bool,
    /// Keep the original line breaks found in the source.
    pub preserve_line_breaks: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            indent_width: 2,
            use_tabs: false,
            max_width: 110,
            sort_wxml_attrs: false,
            style_one_decl_per_line: true,
            preserve_line_breaks: true,
        }
    }
}

impl FormatOptions {
    /// One indentation unit as a `String`.
    pub fn indent_unit(&self) -> String {
        if self.use_tabs {
            "\t".to_owned()
        } else {
            " ".repeat(self.indent_width)
        }
    }

    /// Indentation prefix for `level`.
    pub fn indent(&self, level: usize) -> String {
        self.indent_unit().repeat(level)
    }
}

/// Detects the language of `path` and beautifies accordingly.
///
/// Returns the input unchanged for extensions the toolkit does not format.
pub fn beautify_by_extension(path: &str, source: &str, options: &FormatOptions) -> String {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".json") {
        return beautify_json(source, options);
    }
    if lower.ends_with(".wxml") {
        return beautify_wxml(source, options);
    }
    if lower.ends_with(".wxss") {
        return beautify_wxss(source, options);
    }
    if lower.ends_with(".js") || lower.ends_with(".wxs") || lower.ends_with(".ts") {
        return beautify_js(source, options);
    }
    source.to_owned()
}

/// Whether [`beautify_by_extension`] would change anything for `path`.
pub fn supports(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".json", ".wxml", ".wxss", ".js", ".wxs", ".ts"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_by_extension() {
        assert!(supports("pages/index/index.js"));
        assert!(supports("APP.WXSS"));
        assert!(!supports("logo.png"));
        let opts = FormatOptions::default();
        assert_eq!(
            beautify_by_extension("a.png", "raw", &opts),
            "raw".to_owned()
        );
    }
}
