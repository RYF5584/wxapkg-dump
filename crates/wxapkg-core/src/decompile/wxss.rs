// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Reconstruction of `.wxss` sources from `wcc` compiled style sheets.
//!
//! `wcc` turns every style sheet into a segment array that the page runtime feeds to
//! `setCssToHead`:
//!
//! ```js
//! __wxAppCode__['component/xxx/index.wxss'] = setCssToHead(
//!     [".", [1], "item_box{color:#bfbfbf}\n.", [1], "item_box{font-size:", [0, 28], "}"],
//!     undefined,
//!     {path: "./component/xxx/index.wxss"}
//! );
//! ```
//!
//! The runtime's `makeup()` walks that array from the end and applies three opcodes:
//!
//! | opcode | meaning |
//! |---|---|
//! | `[0, N]` | emit `Nrpx` (the compiler hoists `rpx` out for device scaling) |
//! | `[1]` | insert the scope suffix; the default call passes an empty string |
//! | `[2, X]` | inline inclusion of `__COMMON_STYLESHEETS__[X]`, or of a nested array |

use std::collections::BTreeMap;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::js::{match_delimiter, split_top_level_args, Ast, Lit, Parser};

/// Maximum recursion depth for nested `[2, ...]` inclusions.
const MAX_DEPTH: usize = 32;

/// Matches `__COMMON_STYLESHEETS__.hasOwnProperty('k') = [` and the bracket form.
static RE_COMMON_1: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"__COMMON_STYLESHEETS__\s*(?:\.hasOwnProperty\(|\s*\[\s*)['"]([^'"]+)['"]\s*\]?\s*\)?\s*=\s*(\[)"#,
    )
    .expect("valid pattern")
});

/// Matches `__COMMON_STYLESHEETS__['k'] = [`.
static RE_COMMON_2: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"__COMMON_STYLESHEETS__\s*\[\s*['"]([^'"]+)['"]\s*\]\s*=\s*(\[)"#)
        .expect("valid pattern")
});

/// Matches `__wxAppCode__['x.wxss'] = setCssToHead(`.
static RE_APPCODE_WXSS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"__wxAppCode__\s*\[\s*(['"])([^'"]+\.wxss)['"]\s*\]\s*=\s*setCssToHead\s*\("#)
        .expect("valid pattern")
});

/// Matches a bare `setCssToHead(` that is not preceded by an identifier character.
static RE_SETCSS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|[^\w$])(setCssToHead\s*\()").expect("valid pattern"));

/// A value decoded from the compiled segment array.
#[derive(Debug, Clone, PartialEq)]
pub enum CssSeg {
    /// A literal string fragment.
    Str(String),
    /// A literal number.
    Num(f64),
    /// A nested array (either a segment list or an opcode).
    Array(Vec<CssSeg>),
    /// An object literal, keyed by property name.
    Object(BTreeMap<String, CssSeg>),
    /// A bare identifier reference.
    Ident(String),
    /// Anything the miniature decoder could not represent.
    Unknown,
}

/// One reconstructed style sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WxssUnit {
    /// Relative style sheet path.
    pub path: String,
    /// Reconstructed CSS text.
    pub css: String,
}

/// Extracts every `__COMMON_STYLESHEETS__` entry, keyed by its path.
pub fn extract_common_sheets(src: &str) -> BTreeMap<String, Vec<CssSeg>> {
    let mut out = BTreeMap::new();
    for re in [&*RE_COMMON_1, &*RE_COMMON_2] {
        for caps in re.captures_iter(src) {
            let (Some(key), Some(arr)) = (caps.get(1), caps.get(2)) else {
                continue;
            };
            let Some(end) = match_delimiter(src, arr.start()) else {
                continue;
            };
            if let Some(seg) = parse_array_literal(src, arr.start(), end) {
                out.insert(key.as_str().to_owned(), seg);
            }
        }
    }
    out
}

fn parse_array_literal(src: &str, brace: usize, _end: usize) -> Option<Vec<CssSeg>> {
    let mut parser = Parser::new(src, brace, None);
    let ast = parser.parse_expression(0).ok()?;
    match ast_to_seg(&ast) {
        CssSeg::Array(items) => Some(items),
        _ => None,
    }
}

/// Converts a parsed expression into a [`CssSeg`] value.
pub fn ast_to_seg(ast: &Ast) -> CssSeg {
    match ast {
        Ast::Lit(Lit::Str(s)) => CssSeg::Str(s.clone()),
        Ast::Lit(Lit::Num(n)) => CssSeg::Num(*n),
        Ast::Lit(Lit::Bool(b)) => CssSeg::Str(b.to_string()),
        Ast::Lit(Lit::Null) => CssSeg::Str("null".to_owned()),
        Ast::Ident(name) => CssSeg::Ident(name.clone()),
        Ast::Template(raw) => CssSeg::Str(raw.trim_matches('`').to_owned()),
        Ast::Array(items) => CssSeg::Array(
            items
                .iter()
                .filter_map(|i| i.as_ref().map(ast_to_seg))
                .collect(),
        ),
        Ast::Object(props) => {
            let mut map = BTreeMap::new();
            for (k, v) in props {
                let key = match k {
                    Some(Ast::Lit(Lit::Str(s))) => s.clone(),
                    Some(Ast::Lit(Lit::Num(n))) => format_num(*n),
                    Some(Ast::Ident(name)) => name.clone(),
                    _ => continue,
                };
                map.insert(key, ast_to_seg(v));
            }
            CssSeg::Object(map)
        }
        Ast::Binary(op, a, b) if op == "+" => CssSeg::Str(format!(
            "{}{}",
            seg_to_text(&ast_to_seg(a)),
            seg_to_text(&ast_to_seg(b))
        )),
        Ast::Unary(op, inner) if op == "-" => {
            if let CssSeg::Num(n) = ast_to_seg(inner) {
                CssSeg::Num(-n)
            } else {
                CssSeg::Unknown
            }
        }
        _ => CssSeg::Unknown,
    }
}

/// Renders a segment array the way the runtime's `makeup()` does.
pub fn render_css(seg: &CssSeg, common: &BTreeMap<String, Vec<CssSeg>>, depth: usize) -> String {
    if depth > MAX_DEPTH {
        return String::new();
    }
    match seg {
        CssSeg::Str(s) => s.clone(),
        CssSeg::Num(n) => format_num(*n),
        CssSeg::Ident(s) => s.clone(),
        CssSeg::Object(_) | CssSeg::Unknown => String::new(),
        CssSeg::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    CssSeg::Str(s) => out.push_str(s),
                    CssSeg::Num(n) => out.push_str(&format_num(*n)),
                    CssSeg::Ident(s) => out.push_str(s),
                    CssSeg::Object(_) => {}
                    CssSeg::Unknown => {}
                    CssSeg::Array(inner) => {
                        let op = inner.first();
                        match op {
                            Some(CssSeg::Num(0.0)) => {
                                let val = inner.get(1).map_or(0.0, seg_num);
                                out.push_str(&format!("{}rpx", format_num(val)));
                            }
                            Some(CssSeg::Num(1.0)) => {
                                // Scope suffix: the default runtime call passes "".
                            }
                            Some(CssSeg::Num(2.0)) => {
                                if let Some(refv) = inner.get(1) {
                                    match refv {
                                        CssSeg::Str(path) => {
                                            if let Some(sheet) = common.get(path) {
                                                out.push_str(&render_css(
                                                    &CssSeg::Array(sheet.clone()),
                                                    common,
                                                    depth + 1,
                                                ));
                                            } else {
                                                out.push_str(&format!("@import \"{path}\";\n"));
                                            }
                                        }
                                        nested => {
                                            out.push_str(&render_css(nested, common, depth + 1))
                                        }
                                    }
                                }
                            }
                            None => {}
                            _ => {
                                // Unknown opcode: keep whatever is readable.
                                let rest = CssSeg::Array(inner[1..].to_vec());
                                out.push_str(&render_css(&rest, common, depth + 1));
                            }
                        }
                    }
                }
            }
            out
        }
    }
}

fn seg_num(seg: &CssSeg) -> f64 {
    match seg {
        CssSeg::Num(n) => *n,
        CssSeg::Str(s) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn seg_to_text(seg: &CssSeg) -> String {
    match seg {
        CssSeg::Str(s) => s.clone(),
        CssSeg::Num(n) => format_num(*n),
        CssSeg::Ident(s) => s.clone(),
        _ => String::new(),
    }
}

/// Formats a number the way JavaScript's `String(n)` does for integral values.
pub fn format_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        format!("{}", v as i64)
    } else {
        let mut s = format!("{v}");
        if s.contains('e') {
            s = format!("{v:?}");
        }
        s
    }
}

/// Reconstructs every `.wxss` referenced from `app-wxss.js` / `page-frame.js`.
///
/// Returns the units keyed by normalised relative path plus any warnings.
pub fn extract_wxss(src: &str) -> (BTreeMap<String, String>, Vec<String>) {
    // One substring scan skips the whole machinery for bundles without styles; most
    // modules of a big bundle have none.
    if !src.contains("setCssToHead") && !src.contains("__COMMON_STYLESHEETS__") {
        return (BTreeMap::new(), Vec::new());
    }
    let common = extract_common_sheets(src);
    let mut out = BTreeMap::new();
    let mut warns = Vec::new();
    let mut seen: Vec<(usize, usize)> = Vec::new();

    for caps in RE_APPCODE_WXSS.captures_iter(src) {
        let (Some(key), Some(whole)) = (caps.get(2), caps.get(0)) else {
            continue;
        };
        let Some(paren) = src[whole.start()..whole.end()]
            .rfind('(')
            .map(|o| whole.start() + o)
        else {
            continue;
        };
        let Some(end) = match_delimiter(src, paren) else {
            continue;
        };
        seen.push((whole.start(), end + 1));
        let args = parse_call_args(src, paren);
        if let Some(css) = css_from_args(&args, &common, &mut warns, key.as_str()) {
            out.insert(normalize_path(key.as_str()), css);
        }
    }

    // A bare `setCssToHead([...], undefined, {path: "./app.wxss"})` is `app.wxss`.
    for caps in RE_SETCSS.captures_iter(src) {
        let Some(inner) = caps.get(1) else { continue };
        let Some(paren) = inner.as_str().rfind('(').map(|o| inner.start() + o) else {
            continue;
        };
        let Some(_end) = match_delimiter(src, paren) else {
            continue;
        };
        if seen
            .iter()
            .any(|(a, b)| *a <= inner.start() && inner.start() < *b)
        {
            continue;
        }
        let args = parse_call_args(src, paren);
        let Some(path) = path_of(&args) else { continue };
        if let Some(css) = css_from_args(&args, &common, &mut warns, &path) {
            out.entry(normalize_path(&path)).or_insert(css);
        }
    }

    (out, warns)
}

fn parse_call_args(src: &str, paren_index: usize) -> Vec<CssSeg> {
    split_top_level_args(src, paren_index)
        .into_iter()
        .map(|(s, e)| {
            let mut parser = Parser::new(src, s, Some(e));
            parser
                .parse_expression(0)
                .map_or(CssSeg::Unknown, |ast| ast_to_seg(&ast))
        })
        .collect()
}

fn path_of(args: &[CssSeg]) -> Option<String> {
    for arg in args {
        if let CssSeg::Object(map) = arg {
            if let Some(CssSeg::Str(path)) = map.get("path") {
                return Some(path.clone());
            }
        }
    }
    None
}

fn css_from_args(
    args: &[CssSeg],
    common: &BTreeMap<String, Vec<CssSeg>>,
    _warns: &mut Vec<String>,
    _path: &str,
) -> Option<String> {
    let arr = args.iter().find_map(|a| match a {
        CssSeg::Array(items) => Some(items.clone()),
        _ => None,
    })?;
    Some(restore_implicit_root(render_css(
        &CssSeg::Array(arr),
        common,
        0,
    )))
}

/// Restores the implicit root selector around declarations that `wcc` emitted bare.
///
/// `:root { --animate-duration: 1s }` compiles to `\n{--animate-duration:1s}` with the
/// selector stripped. A selector-less block is invalid WXSS, so it is written back as
/// `page { … }`, which is the root element the variables are inherited from.
fn restore_implicit_root(css: String) -> String {
    let mut out = String::with_capacity(css.len() + 8);
    let mut rest = css.as_str();
    if rest.starts_with('{') {
        out.push_str("page");
    }
    while let Some(pos) = rest.find("\n{") {
        out.push_str(&rest[..pos + 1]);
        out.push_str("page");
        rest = &rest[pos + 1..];
    }
    out.push_str(rest);
    out
}

/// Strips leading `./` and `/` from a style sheet path.
pub fn normalize_path(p: &str) -> String {
    p.trim_start_matches("./")
        .trim_start_matches('/')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn renders_segments_with_rpx_and_text() {
        let src = r#".a{font-size:28rpx}"#;
        // Emulate the compiled array: [".a{font-size:", [0,28], "}"]
        let args = vec![CssSeg::Array(vec![
            CssSeg::Str(".a{font-size:".to_owned()),
            CssSeg::Array(vec![CssSeg::Num(0.0), CssSeg::Num(28.0)]),
            CssSeg::Str("}".to_owned()),
        ])];
        let css = css_from_args(&args, &BTreeMap::new(), &mut Vec::new(), "a.wxss").unwrap();
        assert_eq!(css, ".a{font-size:28rpx}");
        assert!(!src.is_empty());
    }

    #[test]
    fn extracts_appcode_wxss_with_path() {
        let src = r#"
__wxAppCode__['pages/index/index.wxss'] = setCssToHead(
  [".page{color:#333}\n"],
  undefined,
  {path: "./pages/index/index.wxss"}
);
"#;
        let (units, warns) = extract_wxss(src);
        assert!(warns.is_empty());
        assert_eq!(units["pages/index/index.wxss"], ".page{color:#333}\n");
    }

    #[test]
    fn a_selectorless_block_becomes_the_page_root() {
        let args = vec![CssSeg::Array(vec![CssSeg::Str(
            ".a{color:red}\n{--x:1s;--y:2}\n".to_owned(),
        )])];
        let css = css_from_args(&args, &BTreeMap::new(), &mut Vec::new(), "a").unwrap();
        assert_eq!(css, ".a{color:red}\npage{--x:1s;--y:2}\n");
    }

    #[test]
    fn inlines_common_sheets() {
        let src = r#"
__COMMON_STYLESHEETS__['./common.wxss'] = ["html{height:100%}"];
__wxAppCode__['app.wxss'] = setCssToHead([".a{}", [2, "./common.wxss"]], undefined, {path: "./app.wxss"});
"#;
        let (units, _) = extract_wxss(src);
        assert!(units["app.wxss"].contains("html{height:100%}"));
    }

    #[test]
    fn scope_suffix_opcode_is_empty_by_default() {
        let args = vec![CssSeg::Array(vec![
            CssSeg::Str(".a".to_owned()),
            CssSeg::Array(vec![CssSeg::Num(1.0)]),
            CssSeg::Str("{x:1}".to_owned()),
        ])];
        let css = css_from_args(&args, &BTreeMap::new(), &mut Vec::new(), "a").unwrap();
        assert_eq!(css, ".a{x:1}");
    }

    #[test]
    fn numbers_render_without_trailing_zeros() {
        assert_eq!(format_num(1.0), "1");
        assert_eq!(format_num(1.5), "1.5");
        assert_eq!(format_num(-2.0), "-2");
    }
}
