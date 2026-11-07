// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! `define()` module extraction, `__wxAppCode__` registry parsing and `require()`
//! path rewriting.
//!
//! Slicing modules out of a bundle requires knowing where each function body starts
//! and ends without tripping over strings, comments or regular expressions — hence
//! the reliance on [`crate::js::match_delimiter`] rather than naive brace counting.

use std::collections::BTreeMap;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::js::{match_delimiter, unescape_js_string};

/// A `define(...)` module found inside a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsModule {
    /// Registered module path, e.g. `pages/index/index.js`.
    pub path: String,
    /// Byte offset of the start of the whole `define(...)` call.
    pub start: usize,
    /// Byte offset just after the opening brace of the function body.
    pub body_start: usize,
    /// Byte offset of the closing brace of the function body.
    pub body_end: usize,
    /// Byte offset just after the closing parenthesis of `define(...)`.
    pub end: usize,
}

/// Matches the head of a `define("path", ...)` call.
static DEFINE_HEAD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"\bdefine\s*\(\s*("([^"]*)"|'([^']*)')\s*,\s*"#).expect("valid pattern")
});

/// Matches `__wxAppCode__['x.json'] = {`.
static APPCODE_JSON: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"__wxAppCode__\s*\[\s*(['"])([^'"]+\.json)['"]\s*\]\s*=\s*(\{)"#)
        .expect("valid pattern")
});

/// Matches a `require("...")` call, tolerating mixed quotes.
static REQUIRE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\brequire\s*\(\s*(['"])([^'"]*)['"]\s*\)"#).expect("valid pattern"));

/// Collects every `define(...)` module in `src`, sorted by position.
///
/// Duplicate module paths keep the first occurrence, matching the runtime's
/// first-registration-wins semantics.
pub fn find_defines(src: &str) -> Vec<JsModule> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for caps in DEFINE_HEAD.captures_iter(src) {
        let Some(m) = caps.get(0) else { continue };
        let path = match (caps.get(2), caps.get(3)) {
            (Some(v), _) => v.as_str(),
            (_, Some(v)) => v.as_str(),
            _ => continue,
        };
        if !seen.insert(path.to_owned()) {
            continue;
        }

        let Some(fn_at) = src[m.end()..].find("function").map(|off| m.end() + off) else {
            continue;
        };
        if fn_at - m.end() > 200 {
            continue;
        }
        let Some(paren) = src[fn_at..].find('(').map(|off| fn_at + off) else {
            continue;
        };
        let Some(paren_end) = match_delimiter(src, paren) else {
            continue;
        };
        let Some(brace) = src[paren_end..].find('{').map(|off| paren_end + off) else {
            continue;
        };
        // The body must follow the parameter list directly (whitespace allowed).
        if !src[paren_end + 1..brace].trim().is_empty() {
            continue;
        }
        let Some(body_end) = match_delimiter(src, brace) else {
            continue;
        };
        let Some(close) = src[body_end..].find(')').map(|off| body_end + off) else {
            continue;
        };

        let line_start = src[..m.start()].rfind('\n').map_or(0, |i| i + 1);
        let start = if src[line_start..m.start()].trim().is_empty() {
            line_start
        } else {
            m.start()
        };

        out.push(JsModule {
            path: path.to_owned(),
            start,
            body_start: brace + 1,
            body_end,
            end: close + 1,
        });
    }

    out.sort_by_key(|m| m.start);
    out
}

/// Returns `(module path -> body source, removed ranges)` for every module.
///
/// The removed ranges are used to carve the `define` shells out of the bundle when
/// writing the `_raw` copy.
pub fn strip_define_bodies(src: &str) -> (BTreeMap<String, String>, Vec<(usize, usize)>) {
    let mut bodies = BTreeMap::new();
    let mut spans = Vec::new();
    for module in find_defines(src) {
        bodies.insert(
            module.path.clone(),
            dedent(&src[module.body_start..module.body_end]),
        );
        spans.push((module.start, module.end));
    }
    (bodies, spans)
}

/// Removes the common leading indentation WeChat adds when wrapping module bodies.
pub fn dedent(body: &str) -> String {
    let lines: Vec<&str> = body.split('\n').collect();
    if lines.len() < 2 {
        return body.trim_matches('\n').to_owned();
    }
    let body_lines: Vec<&str> = if lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines[1..lines.len() - 1].to_vec()
    } else {
        lines[1..].to_vec()
    };
    if body_lines.is_empty() {
        return body.trim_matches('\n').to_owned();
    }

    let cut = body_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches(['\t', ' ']).len())
        .min();
    let Some(cut) = cut else {
        return body.trim_matches('\n').to_owned();
    };

    let mut fixed: Vec<String> = Vec::with_capacity(body_lines.len());
    for line in body_lines {
        if line.trim().is_empty() {
            fixed.push(String::new());
        } else if line[..cut.min(line.len())].trim().is_empty() {
            fixed.push(line[cut.min(line.len())..].to_owned());
        } else {
            fixed.push(line.trim_start().to_owned());
        }
    }
    let mut joined = fixed.join("\n");
    while joined.starts_with('\n') {
        joined.remove(0);
    }
    while joined.ends_with('\n') {
        joined.pop();
    }
    joined.push('\n');
    joined
}

/// Extracts the raw object-literal text of every `__wxAppCode__['x.json'] = {...}`.
pub fn find_appcode_json(src: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for caps in APPCODE_JSON.captures_iter(src) {
        let (Some(key), Some(brace)) = (caps.get(2), caps.get(3)) else {
            continue;
        };
        if let Some(end) = match_delimiter(src, brace.start()) {
            out.insert(key.as_str().to_owned(), src[brace.start()..=end].to_owned());
        }
    }
    out
}

/// Parses a JavaScript string literal without evaluating any code.
pub fn parse_js_string_literal(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'\'' | b'"') && bytes[bytes.len() - 1] == bytes[0] {
        return Some(unescape_js_string(trimmed));
    }
    None
}

/// Everything a `require()` rewrite needs to know, indexed once.
///
/// The resolution rules are cheap per call but the npm fallback used to scan (and re-format)
/// every known module for every specifier, which made a 200 KB project take half a minute.
/// Building the lookup tables once turns the whole rewrite into a hash probe per call.
pub struct ModuleIndex {
    known: std::collections::HashSet<String>,
    /// Package name → its shortest JavaScript file, used as an `index.js` stand-in.
    npm_entries: std::collections::HashMap<String, String>,
}

impl ModuleIndex {
    /// Indexes the module paths a bundle provides.
    pub fn new(known: std::collections::HashSet<String>) -> Self {
        let mut npm_entries: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for path in &known {
            let Some(rest) = path.strip_prefix("miniprogram_npm/") else {
                continue;
            };
            if !path.ends_with(".js") {
                continue;
            }
            let package = match rest.strip_prefix('@') {
                Some(scoped) => {
                    // `@scope/name/...` keeps two segments.
                    let mut parts = scoped.splitn(3, '/');
                    let scope = parts.next().unwrap_or_default();
                    let name = parts.next().unwrap_or_default();
                    format!("@{scope}/{name}")
                }
                None => rest.split('/').next().unwrap_or_default().to_owned(),
            };
            match npm_entries.get(&package) {
                Some(existing) if existing.len() <= path.len() => {}
                _ => {
                    npm_entries.insert(package, path.clone());
                }
            }
        }
        Self { known, npm_entries }
    }

    /// Whether the bundle provides `path`.
    pub fn contains(&self, path: &str) -> bool {
        self.known.contains(path)
    }

    /// Resolves a raw specifier to a module path known to the bundle.
    fn resolve(&self, spec: &str, cur_dir: &str) -> Option<String> {
        if spec.starts_with("plugin://") || spec.starts_with("plugin-private://") {
            return None;
        }

        let mut candidates: Vec<String> = Vec::new();
        if let Some(stripped) = spec.strip_prefix('/') {
            candidates.push(stripped.to_owned());
        } else if spec.starts_with("./") || spec.starts_with("../") {
            candidates.push(posix_normalize(&format!("{cur_dir}/{spec}")));
        } else {
            // Bare specifier: a webpack-era npm reference such as `require("@base/cookie")`.
            // Its Mini Program counterpart lives under `miniprogram_npm/<pkg>/...`.
            let np = format!("miniprogram_npm/{spec}");
            if np.ends_with(".js") {
                candidates.push(np.clone());
            }
            candidates.push(format!("{np}/index.js"));
            candidates.push(format!("{np}.js"));
            candidates.push(spec.to_owned());
            candidates.push(posix_normalize(&format!("{cur_dir}/{spec}")));
        }

        for candidate in candidates.clone() {
            if !candidate.ends_with(".js") {
                candidates.push(format!("{candidate}.js"));
            }
        }
        for candidate in &candidates {
            if self.known.contains(candidate) {
                return Some(candidate.clone());
            }
        }

        // npm packages without an `index.js` entry: the shortest file stands in for `main`.
        if !spec.starts_with("./") && !spec.starts_with("../") && !spec.starts_with('/') {
            let package = spec.trim_end_matches('/');
            if let Some(entry) = self.npm_entries.get(package) {
                return Some(entry.clone());
            }
        }
        None
    }
}

/// Rewrites every resolvable `require()` argument to a path relative to `current`.
///
/// WeChat's module system accepts root-relative specifiers such as `require("common.js")`,
/// but DevTools resolves relative to the *current file*. Normalising every reference
/// keeps the reconstructed project directly runnable.
///
/// Returns the rewritten source and the number of substitutions made.
pub fn rewrite_requires(code: &str, current: &str, index: &ModuleIndex) -> (String, usize) {
    let cur_dir = posix_dirname(current);
    let mut changed = 0usize;

    let rewritten = REQUIRE.replace_all(code, |caps: &regex::Captures<'_>| {
        let Some(p) = caps.get(2) else {
            return caps[0].to_owned();
        };
        let spec = p.as_str();
        let Some(target) = index.resolve(spec, &cur_dir) else {
            return caps[0].to_owned();
        };
        if target == current {
            return caps[0].to_owned();
        }
        let rel = posix_relpath(&target, &cur_dir);
        let rel = if rel.starts_with('.') {
            rel
        } else {
            format!("./{rel}")
        };
        if rel == spec {
            return caps[0].to_owned();
        }
        changed += 1;
        format!("require(\"{rel}\")")
    });

    (rewritten.into_owned(), changed)
}

/// Lists every `require()` specifier found in `code`, in source order.
pub fn scan_requires(code: &str) -> Vec<String> {
    REQUIRE
        .captures_iter(code)
        .filter_map(|c| c.get(2).map(|m| m.as_str().to_owned()))
        .collect()
}

fn posix_dirname(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_owned(),
        None => String::new(),
    }
}

/// Minimal POSIX relative path computation (no filesystem access involved).
fn posix_relpath(target: &str, base: &str) -> String {
    let target_parts: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();
    let base_parts: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();

    let common = target_parts
        .iter()
        .zip(base_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut out: Vec<String> = Vec::new();
    for _ in common..base_parts.len() {
        out.push("..".to_owned());
    }
    out.extend(target_parts[common..].iter().map(|s| (*s).to_owned()));
    if out.is_empty() {
        ".".to_owned()
    } else {
        out.join("/")
    }
}

/// Normalises `.` and `..` segments without touching the filesystem.
fn posix_normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const BUNDLE: &str = r#"
define("pages/index/index.js", function(require, module, exports, window, document, frames, self, location, navigator, localStorage) {
	Page({
		data: { msg: "hi" },
		onLoad: function() { this.setData({ msg: "hi " + require("common.js").suffix }); },
	});
});
define("common.js", function(require, module, exports) {
	exports.suffix = "!";
});
__wxAppCode__['pages/index/index.json'] = {"usingComponents":{}};
"#;

    #[test]
    fn finds_both_modules() {
        let mods = find_defines(BUNDLE);
        assert_eq!(mods.len(), 2);
        assert_eq!(mods[0].path, "pages/index/index.js");
        assert_eq!(mods[1].path, "common.js");
    }

    #[test]
    fn strips_bodies_without_losing_semicolons() {
        let (bodies, spans) = strip_define_bodies(BUNDLE);
        assert_eq!(spans.len(), 2);
        let index = bodies.get("pages/index/index.js").unwrap();
        assert!(index.contains("Page({"));
        assert!(index.contains("onLoad: function()"));
    }

    #[test]
    fn rewrites_root_relative_requires() {
        let (bodies, _) = strip_define_bodies(BUNDLE);
        let known: std::collections::HashSet<String> = bodies.keys().cloned().collect();
        let index = ModuleIndex::new(known);
        let (code, n) = rewrite_requires(
            &bodies["pages/index/index.js"],
            "pages/index/index.js",
            &index,
        );
        assert_eq!(n, 1);
        assert!(code.contains(r#"require("../../common.js")"#), "{code}");
    }

    #[test]
    fn extracts_appcode_json() {
        let map = find_appcode_json(BUNDLE);
        assert_eq!(map.len(), 1);
        assert_eq!(map["pages/index/index.json"], r#"{"usingComponents":{}}"#);
    }

    #[test]
    fn scan_requires_lists_specs() {
        let specs = scan_requires(r#"a(require("x"), require('./y.js'))"#);
        assert_eq!(specs, vec!["x".to_owned(), "./y.js".to_owned()]);
    }

    #[test]
    fn dedents_nested_bodies() {
        let src = "function(){\n\t\t\tvar a = 1;\n\t\t\tvar b = 2;\n\t\t}";
        let (bodies, _) = strip_define_bodies(&format!("define('a.js', {src});"));
        assert_eq!(bodies["a.js"], "var a = 1;\nvar b = 2;\n");
    }

    #[test]
    fn relpath_handles_parent_traversal() {
        assert_eq!(posix_relpath("common.js", "pages/index"), "../../common.js");
        assert_eq!(posix_relpath("pages/a/b.js", "pages/a"), "b.js");
    }
}
