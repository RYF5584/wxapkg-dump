// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! WXML reconstruction from `wcc` compiled template render functions.
//!
//! Every `.wxml` is compiled into a render function that drives a tiny virtual DOM:
//!
//! ```js
//! var m0 = function (e, s, r, gg) {
//!   var z = gz$gwx_1();            // z = operand array for this template group
//!   var oB = _v();                 // virtual node (equivalent to <block>)
//!   _(r, oB);                      // attach to the root
//!   if (_oz(z, 9, e, s, gg)) {     // condition lives in the operand array
//!     oB.wxVkey = 1;               // branch #1 of this virtual node
//!     var xC = _v(); _(oB, xC);
//!     var eN = _mz(z, 'view', ['class', 3, 'id', 1], [], e, s, gg);
//!     var cF = _oz(z, 5, e, s, gg);
//!     _(eN, cF);                   // text child {{ ... }}
//!   }
//!   return r;
//! };
//! ```
//!
//! Runtime helpers and their semantics (read straight out of the `$gwx` runtime that
//! ships inside `app-service.js`):
//!
//! | helper | effect |
//! |---|---|
//! | `_v()` | create a virtual node |
//! | `_n(tag)` | create an element node |
//! | `_(a, b)` | push `b` into `a.children` |
//! | `_mz(z, tag, attrs, ...)` | create an element and set attributes from `[name, base, name, delta, ...]` |
//! | `_rz(z, node, name, i, ...)` | set a single attribute from `z[i]` |
//! | `_oz(z, i, ...)` | evaluate `z[i]`: conditions, text, template names |
//! | `_1z(z, i, ...)` | same, carrying type information for template data |
//! | `_2z(z, i, fn, ..., f, item, index, key)` | `wx:for` loop |
//! | `_gd(mod, name, ...)` | look up a named `<template>` render function |
//! | `V.wxVkey = N` | mark branch `N` of a virtual node |
//! | `V.wxXCkey = N` | node kind, used only for diffing |
//!
//! Operand array entries are opcode-coded ASTs of the original expressions:
//!
//! | opcode | meaning |
//! |---|---|
//! | `[1, v]` / `[3, v]` | literal number / string |
//! | `[2, op], a, b` | binary, unary or ternary operation |
//! | `[4, a]` | parenthesised expression |
//! | `[5, ...]` | array literal |
//! | `[6, a, b]` | member access (`a.b` or `a[b]`) |
//! | `[7, [3, 'name']]` | variable reference |
//! | `[8, key, value]` | single-property object |
//! | `[9, ...]` | object spread merge |
//! | `[10, a]` | transparent wrapper |
//! | `[11, ...]` | string concatenation |
//! | `[12, fn, ...]` | function call |
//!
//! Decompilation is therefore "interpret this miniature VM, then serialise the
//! resulting virtual DOM". Templates whose operand group was never shipped (newer
//! `wcc` lazy-loads the arrays through `__WXML_GLOBAL__.ops_cached`) still yield
//! correct structure; only the bound expressions degrade to `undefined`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::rc::Rc;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::decompile::wxss::format_num;
use crate::js::{match_delimiter, Ast, Lit, Parser, Stmt};

// ---------------------------------------------------------------------------
// Virtual DOM
// ---------------------------------------------------------------------------

/// Shared, mutable handle to a DOM node.
pub type NodeRef = Rc<RefCell<Node>>;

/// Attribute value form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrVal {
    /// An expression that still needs `{{ }}` wrapping decisions.
    Expr(String),
    /// A value already in its final textual form.
    Final(String),
}

/// A single attribute on an element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    /// Attribute name.
    pub name: String,
    /// Attribute value.
    pub value: AttrVal,
}

/// Element node.
#[derive(Debug, Clone, Default)]
pub struct Elem {
    /// Tag name.
    pub tag: String,
    /// Attributes.
    pub attrs: Vec<Attr>,
    /// Child nodes.
    pub children: Vec<NodeRef>,
    /// Hoisted `wx:if` directive: `(directive, condition)`.
    pub cond: Option<(String, Option<String>)>,
}

/// Virtual (`<block>`-like) node, possibly carrying an `if`/`elif`/`else` chain.
#[derive(Debug, Clone, Default)]
pub struct Virtual {
    /// Unconditional children.
    pub children: Vec<NodeRef>,
    /// Branch list.
    pub branches: Vec<Branch>,
}

/// One branch of a virtual node.
#[derive(Debug, Clone, Default)]
pub struct Branch {
    /// Branch condition; `None` denotes `wx:else`.
    pub cond: Option<String>,
    /// Merge key, used to pair repeated branches.
    pub key: Option<String>,
    /// Branch children.
    pub children: Vec<NodeRef>,
}

/// `wx:for` node.
#[derive(Debug, Clone, Default)]
pub struct ForNode {
    /// Iterated expression.
    pub expr: String,
    /// `wx:for-item` name.
    pub item: String,
    /// `wx:for-index` name.
    pub index: String,
    /// `wx:key` value.
    pub key: String,
    /// Loop body.
    pub children: Vec<NodeRef>,
    /// Hoisted condition.
    pub cond: Option<String>,
}

/// `<template is="...">` node.
#[derive(Debug, Clone, Default)]
pub struct TemplateNode {
    /// Template name or expression.
    pub name: String,
    /// Final `data` attribute payload: `key: value` pairs or `...identifier`.
    pub data: Option<String>,
    /// Hoisted condition.
    pub cond: Option<String>,
}

/// Text node; already in final form.
#[derive(Debug, Clone, Default)]
pub struct Text {
    /// Node text.
    pub text: String,
}

/// Fallback raw node.
#[derive(Debug, Clone, Default)]
pub struct Raw {
    /// Raw markup.
    pub html: String,
}

/// A virtual DOM node.
#[derive(Debug, Clone)]
pub enum Node {
    /// Element.
    Elem(Elem),
    /// Virtual node.
    Virtual(Virtual),
    /// Loop node.
    For(Box<ForNode>),
    /// Template node.
    Template(TemplateNode),
    /// Text node.
    Text(Text),
    /// Raw markup.
    Raw(Raw),
}

impl Node {
    /// Creates a new shared element node.
    pub fn elem(tag: impl Into<String>) -> NodeRef {
        Rc::new(RefCell::new(Self::Elem(Elem {
            tag: tag.into(),
            ..Elem::default()
        })))
    }

    /// Creates a new shared virtual node.
    pub fn virtual_node() -> NodeRef {
        Rc::new(RefCell::new(Self::Virtual(Virtual::default())))
    }

    /// Pushes a child into this node when it accepts children.
    pub fn push_child(node: &NodeRef, child: NodeRef) {
        match &mut *node.borrow_mut() {
            Self::Elem(e) => e.children.push(child),
            Self::Virtual(v) => v.children.push(child),
            Self::For(f) => f.children.push(child),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Operand values
// ---------------------------------------------------------------------------

/// One entry of a `gz$gwx_N` operand array.
///
/// The structure is the decoded JavaScript array literal, so nested arrays keep
/// their opcode prefix and are interpreted by [`operand_text`].
#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    /// `null` / `undefined`.
    Null,
    /// Boolean literal.
    Bool(bool),
    /// Numeric literal.
    Num(f64),
    /// String literal.
    Str(String),
    /// Array, in opcode-coded form.
    Arr(Vec<Operand>),
    /// Object literal.
    Obj(BTreeMap<String, Operand>),
}

impl Operand {
    /// Numeric opcode when this operand is an array whose first element is a number.
    fn opcode(&self) -> Option<i64> {
        match self {
            Self::Arr(items) => match items.first() {
                Some(Self::Num(n)) => Some(*n as i64),
                _ => None,
            },
            _ => None,
        }
    }

    /// Returns the payload string when this operand is a `[3, 'name']` string literal.
    fn as_str_op(&self) -> Option<&str> {
        if self.opcode() == Some(3) {
            if let Self::Arr(items) = self {
                if let Some(Self::Str(s)) = items.get(1) {
                    return Some(s);
                }
            }
        }
        None
    }
}

/// Renders a scalar operand the way JavaScript's `String()` would.
fn literal_text(v: &Operand) -> String {
    match v {
        Operand::Null => "null".to_owned(),
        Operand::Bool(b) => b.to_string(),
        Operand::Num(n) => format_num(*n),
        Operand::Str(s) => quote_js(s),
        Operand::Arr(items) if items.is_empty() => "[]".to_owned(),
        _ => String::new(),
    }
}

/// Quotes a string the way it must appear inside a WXML mustache.
pub fn quote_js(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("'{escaped}'")
}

/// Binary operator precedence, mirroring the JavaScript table.
fn precedence(op: &str) -> i32 {
    match op {
        "||" | "??" => 1,
        "&&" => 2,
        "|" => 3,
        "^" => 4,
        "&" => 5,
        "==" | "!=" | "===" | "!==" => 6,
        "<" | ">" | "<=" | ">=" => 7,
        "<<" | ">>" => 8,
        "+" | "-" => 9,
        "*" | "/" | "%" => 10,
        _ => 9,
    }
}

static JS_IDENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z_$][\w$]*$").expect("valid pattern"));

/// Renders an operand as WXML expression text, honouring the opcode table.
pub fn operand_text(op: &Operand, depth: usize, parent_prec: i32) -> String {
    if depth > 60 {
        return "/*too-deep*/".to_owned();
    }
    let Operand::Arr(items) = op else {
        return literal_text(op);
    };
    if items.is_empty() {
        return literal_text(op);
    }

    // Case 1: the array itself is an opcode carrier, e.g. `[3, 'hello']`.
    if let Some(code) = op.opcode() {
        return match code {
            1 | 3 => items.get(1).map_or(String::new(), literal_text),
            11 => {
                let parts: Vec<String> = items[1..]
                    .iter()
                    .map(|x| operand_text(x, depth + 1, 9))
                    .collect();
                bin_join("+", &parts, 9, parent_prec)
            }
            _ => items.get(1).map_or(String::new(), literal_text),
        };
    }

    // Case 2: the first element is itself an array carrying the opcode, e.g.
    // `[[2, '==='], a, b]`.
    let Some(head) = items.first() else {
        return String::new();
    };
    let Some(code) = head.opcode() else {
        return String::new();
    };
    let head_items = match head {
        Operand::Arr(h) => h,
        _ => return String::new(),
    };

    match code {
        2 => {
            let Some(Operand::Str(operator)) = head_items.get(1) else {
                return String::new();
            };
            match operator.as_str() {
                "?:" => {
                    let test = op_at(items, 1, depth, 1);
                    let cons = op_at(items, 2, depth, 1);
                    let alt = op_at(items, 3, depth, 1);
                    format!("{test} ? {cons} : {alt}")
                }
                "!" | "~" => {
                    let v = op_at(items, 1, depth, 11);
                    format!("{operator}{v}")
                }
                "-" if items.len() == 2 => {
                    let v = op_at(items, 1, depth, 11);
                    format!("-{v}")
                }
                "&&" | "||" | "??" => {
                    let prec = precedence(operator);
                    let parts = vec![op_at(items, 1, depth, prec), op_at(items, 2, depth, prec)];
                    bin_join(operator, &parts, prec, parent_prec)
                }
                _ => {
                    let prec = precedence(operator);
                    let parts = vec![op_at(items, 1, depth, prec), op_at(items, 2, depth, prec)];
                    bin_join(operator, &parts, prec, parent_prec)
                }
            }
        }
        4 => {
            let inner = op_at(items, 1, depth, 0);
            format!("({inner})")
        }
        5 => {
            let rendered: Vec<String> = items[1..]
                .iter()
                .map(|x| operand_text(x, depth + 1, 0))
                .filter(|s| !s.is_empty())
                .collect();
            format!("[{}]", rendered.join(", "))
        }
        6 => {
            let obj = op_at(items, 1, depth, 12);
            let key_op = items.get(2);
            match key_op.and_then(Operand::as_str_op) {
                Some(name) if JS_IDENT_RE.is_match(name) => format!("{obj}.{name}"),
                _ => {
                    let key = key_op.map_or(String::new(), |k| operand_text(k, depth + 1, 0));
                    format!("{obj}[{key}]")
                }
            }
        }
        7 => match items.get(1) {
            Some(Operand::Arr(sub)) if matches!(sub.first(), Some(Operand::Num(n)) if *n as i64 == 11) => {
                "$gwxScope".to_owned()
            }
            Some(sub @ Operand::Arr(_)) => sub
                .as_str_op()
                .map_or_else(|| operand_text(sub, depth + 1, 0), str::to_owned),
            Some(other) => operand_text(other, depth + 1, 0),
            None => String::new(),
        },
        8 => {
            let key = match items.get(1) {
                Some(Operand::Str(s)) => s.clone(),
                Some(Operand::Num(n)) => format_num(*n),
                _ => String::new(),
            };
            let val = op_at(items, 2, depth, 0);
            format!("{{{key}: {val}}}")
        }
        9 => {
            let rendered: Vec<String> = items[1..]
                .iter()
                .map(|x| operand_text(x, depth + 1, 0))
                .filter(|s| !s.is_empty())
                .map(|s| format!("...{s}"))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
        10 => op_at(items, 1, depth, parent_prec),
        12 => {
            let callee = op_at(items, 1, depth, 12);
            let args: Vec<String> = items[2..]
                .iter()
                .map(|x| operand_text(x, depth + 1, 0))
                .collect();
            format!("{callee}({})", args.join(", "))
        }
        _ => String::new(),
    }
}

fn op_at(items: &[Operand], index: usize, depth: usize, parent_prec: i32) -> String {
    items
        .get(index)
        .map_or(String::new(), |x| operand_text(x, depth + 1, parent_prec))
}

fn bin_join(operator: &str, parts: &[String], prec: i32, parent_prec: i32) -> String {
    let kept: Vec<&str> = parts
        .iter()
        .map(String::as_str)
        .filter(|p| !p.is_empty())
        .collect();
    if kept.is_empty() {
        return String::new();
    }
    let joined = kept.join(&format!(" {operator} "));
    if prec < parent_prec {
        format!("({joined})")
    } else {
        joined
    }
}

/// Renders a template `data` operand as the attribute's inner payload.
///
/// The attribute is not a single object expression: it is a comma-separated list of
/// `key: value` entries, with `...identifier` allowed for spreading a bound object.
/// The compiled operand tree still describes an object literal, so its spreads of
/// nested literals have to be flattened; `data="{{{a: 1}}}"` and
/// `data="{{...{a: 1}}}"` are both rejected by the DevTools compiler.
fn operand_template_data(op: &Operand, depth: usize) -> String {
    match template_data_entries(op, depth) {
        Some(entries) => entries.join(", "),
        None => {
            let text = operand_text(op, depth, 0);
            if text.is_empty() {
                String::new()
            } else {
                format!("...{text}")
            }
        }
    }
}

/// Flattens one object operand into template `data` entries.
fn template_data_entries(op: &Operand, depth: usize) -> Option<Vec<String>> {
    if depth > 60 {
        return None;
    }
    match op {
        Operand::Obj(map) => Some(
            map.iter()
                .map(|(key, value)| format!("{key}: {}", operand_text(value, depth + 1, 0)))
                .collect(),
        ),
        Operand::Arr(items) => match object_opcode(op) {
            Some(8) => {
                let key = match items.get(1) {
                    Some(Operand::Str(s)) => s.clone(),
                    Some(Operand::Num(n)) => format_num(*n),
                    _ => return None,
                };
                let value = op_at(items, 2, depth, 0);
                Some(vec![format!("{key}: {value}")])
            }
            Some(9) => {
                let mut entries = Vec::new();
                for child in &items[1..] {
                    match template_data_entries(child, depth + 1) {
                        Some(inner) => entries.extend(inner),
                        None => {
                            let text = operand_text(child, depth + 1, 0);
                            if !text.is_empty() {
                                entries.push(format!("...{text}"));
                            }
                        }
                    }
                }
                Some(entries)
            }
            _ => None,
        },
        _ => None,
    }
}

/// Opcode carried either by the array itself or by its nested head element.
fn object_opcode(op: &Operand) -> Option<i64> {
    if let Operand::Arr(items) = op {
        if let Some(code) = items.first().and_then(Operand::opcode) {
            return Some(code);
        }
    }
    op.opcode()
}

/// Turns a plain expression into a template `data` spread.
fn spread_data_text(text: String) -> String {
    let expr = text.trim();
    if expr.is_empty() || expr.starts_with("...") {
        return expr.to_owned();
    }
    if expr.starts_with('{') && expr.ends_with('}') {
        // Fallback for a literal that never went through [`operand_template_data`]:
        // `data` entries are written without the surrounding braces.
        return expr.get(1..expr.len() - 1).unwrap_or("").to_owned();
    }
    format!("...{expr}")
}

/// Decodes the argument of a `Z(...)` call into an [`Operand`].
///
/// `ops_so_far` resolves self-references such as `Z(z[1])`.
fn eval_op_ast(ast: &Ast, ops_so_far: &[Operand]) -> Operand {
    match ast {
        Ast::Lit(Lit::Str(s)) => Operand::Str(s.clone()),
        Ast::Lit(Lit::Num(n)) => Operand::Num(*n),
        Ast::Lit(Lit::Bool(b)) => Operand::Bool(*b),
        Ast::Lit(Lit::Null) => Operand::Null,
        Ast::Template(raw) => Operand::Str(raw.trim_matches('`').to_owned()),
        Ast::Array(items) => Operand::Arr(
            items
                .iter()
                .filter_map(|i| i.as_ref())
                .map(|i| eval_op_ast(i, ops_so_far))
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
                map.insert(key, eval_op_ast(v, ops_so_far));
            }
            Operand::Obj(map)
        }
        Ast::Member(base, key, true) => {
            if base.as_ident() == Some("z") {
                if let Ast::Lit(Lit::Num(n)) = &**key {
                    let idx = *n as usize;
                    if let Some(entry) = ops_so_far.get(idx) {
                        return entry.clone();
                    }
                }
            }
            Operand::Null
        }
        Ast::Unary(op, inner) => {
            let value = eval_op_ast(inner, ops_so_far);
            match (op.as_str(), value) {
                ("-", Operand::Num(n)) => Operand::Num(-n),
                ("!", v) => Operand::Bool(matches!(v, Operand::Null | Operand::Bool(false))),
                (_, v) => v,
            }
        }
        Ast::Ident(name) if name == "z" => Operand::Null,
        _ => Operand::Null,
    }
}

/// Renders an arbitrary AST as source text.
///
/// Used for the structural checks that decide how to unfold non-`_oz` conditions.
fn render_ast_text(ast: &Ast) -> String {
    match ast {
        Ast::Lit(Lit::Null) => "null".to_owned(),
        Ast::Lit(Lit::Bool(b)) => b.to_string(),
        Ast::Lit(Lit::Num(n)) => format_num(*n),
        Ast::Lit(Lit::Str(s)) => quote_js(s),
        Ast::Ident(name) => name.clone(),
        Ast::Template(raw) => raw.clone(),
        Ast::Binary(op, a, b) => format!("{} {op} {}", render_ast_text(a), render_ast_text(b)),
        Ast::Unary(op, a) => format!("{op}{}", render_ast_text(a)),
        Ast::Postfix(op, a) => format!("{}{op}", render_ast_text(a)),
        Ast::Member(base, key, computed) => {
            if *computed {
                format!("{}[{}]", render_ast_text(base), render_ast_text(key))
            } else {
                match &**key {
                    Ast::Lit(Lit::Str(s)) => format!("{}.{s}", render_ast_text(base)),
                    other => format!("{}.{}", render_ast_text(base), render_ast_text(other)),
                }
            }
        }
        Ast::Call(callee, args) => {
            let inner: Vec<String> = args.iter().map(render_ast_text).collect();
            format!("{}({})", render_ast_text(callee), inner.join(","))
        }
        Ast::Cond(c, a, b) => format!(
            "{}?{}:{}",
            render_ast_text(c),
            render_ast_text(a),
            render_ast_text(b)
        ),
        Ast::Assign(op, a, b) => {
            format!("{} {op} {}", render_ast_text(a), render_ast_text(b))
        }
        Ast::Array(items) => {
            let inner: Vec<String> = items.iter().flatten().map(render_ast_text).collect();
            format!("[{}]", inner.join(","))
        }
        Ast::Object(props) => {
            let inner: Vec<String> = props
                .iter()
                .map(|(k, v)| {
                    let key = k.as_ref().map_or(String::new(), render_ast_text);
                    format!("{key}:{}", render_ast_text(v))
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Ast::Function { .. } => "function(){}".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Group operand extraction
// ---------------------------------------------------------------------------

/// Matches `function gz$gwx_N(){`.
static RE_GZ_DECL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"function\s+(gz\$gwx[\w$]*)\s*\(\s*\)\s*\{").expect("valid pattern"));

/// Matches `(function(z){`.
static RE_IIFE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\(\s*function\s*\(\s*(\w+)\s*\)\s*\{").expect("valid pattern"));

/// Matches `__WXML_GLOBAL__.ops_cached.<NAME>`.
static RE_OPS_TARGET: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"__WXML_GLOBAL__\.ops_cached\.([\w$]+)").expect("valid pattern"));

/// Matches a local `function Z(ops){ <target>.push(ops) }` helper.
static RE_PUSH_HELPER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"function\s+(\w+)\s*\(\s*(\w+)\s*\)\s*\{\s*[\w$.]+\s*\.\s*push\s*\(\s*(\w+)\s*\)")
        .expect("valid pattern")
});

fn parse_ops(src: &str, fn_start: usize, fn_end: usize) -> Vec<Operand> {
    let body = &src[fn_start..fn_end];
    let mut ops = Vec::new();
    let Ok(stmts) = crate::js::parse(body, 0, None) else {
        return ops;
    };
    walk_stmts(&stmts, &mut ops, "Z");
    ops
}

fn walk_stmts(stmts: &[Stmt], ops: &mut Vec<Operand>, zname: &str) {
    for st in stmts {
        walk_stmt(st, ops, zname);
    }
}

fn walk_stmt(st: &Stmt, ops: &mut Vec<Operand>, zname: &str) {
    match st {
        Stmt::Expr(e) => walk_expr(e, ops, zname),
        Stmt::Var { decls, .. } => {
            for (_, init) in decls {
                if let Some(init) = init {
                    walk_expr(init, ops, zname);
                }
            }
        }
        Stmt::Block(inner) => walk_stmts(inner, ops, zname),
        Stmt::If { test, cons, alt } => {
            walk_expr(test, ops, zname);
            walk_stmts(cons, ops, zname);
            if let Some(alt) = alt {
                walk_stmts(alt, ops, zname);
            }
        }
        Stmt::Try {
            body,
            catch,
            finally,
            ..
        } => {
            walk_stmts(body, ops, zname);
            walk_stmts(catch, ops, zname);
            if let Some(f) = finally {
                walk_stmts(f, ops, zname);
            }
        }
        Stmt::Return(Some(e)) => walk_expr(e, ops, zname),
        Stmt::Return(None) => {}
        Stmt::FuncDecl { body, .. } => walk_stmts(body, ops, zname),
    }
}

fn walk_expr(e: &Ast, ops: &mut Vec<Operand>, zname: &str) {
    match e {
        Ast::Call(callee, args) => {
            if callee.as_ident() == Some(zname) {
                if let Some(first) = args.first() {
                    let decoded = eval_op_ast(first, ops);
                    ops.push(decoded);
                }
                return;
            }
            walk_expr(callee, ops, zname);
            for a in args {
                walk_expr(a, ops, zname);
            }
        }
        Ast::Function { body, .. } => walk_stmts(body, ops, zname),
        Ast::Binary(_, a, b) | Ast::Assign(_, a, b) => {
            walk_expr(a, ops, zname);
            walk_expr(b, ops, zname);
        }
        Ast::Unary(_, a) | Ast::Postfix(_, a) => walk_expr(a, ops, zname),
        Ast::Cond(c, a, b) => {
            walk_expr(c, ops, zname);
            walk_expr(a, ops, zname);
            walk_expr(b, ops, zname);
        }
        Ast::Array(items) => {
            for i in items.iter().flatten() {
                walk_expr(i, ops, zname);
            }
        }
        Ast::Object(props) => {
            for (_, v) in props {
                walk_expr(v, ops, zname);
            }
        }
        Ast::Member(base, key, _) => {
            walk_expr(base, ops, zname);
            walk_expr(key, ops, zname);
        }
        _ => {}
    }
}

/// Scans the lazy-loading IIFEs that populate `__WXML_GLOBAL__.ops_cached.<NAME>`.
pub fn build_iife_ops(src: &str) -> BTreeMap<String, Vec<Operand>> {
    let mut out: BTreeMap<String, Vec<Operand>> = BTreeMap::new();
    if !src.contains("ops_cached") {
        return out;
    }

    // Every ops-cached target is collected once; each IIFE then locates its own target by
    // binary search instead of rescanning the whole bundle (which made this quadratic).
    let targets: Vec<usize> = RE_OPS_TARGET.find_iter(src).map(|t| t.start()).collect();
    for m in RE_IIFE.find_iter(src) {
        let opener = m.end() - 1;
        let Some(close) = match_delimiter(src, opener) else {
            continue;
        };
        let search_end = (close + 150).min(src.len());
        // The first target at or after `close`, within the search window.
        let index = targets.partition_point(|start| *start < close);
        let Some(target) = targets
            .get(index)
            .copied()
            .filter(|start| *start < search_end)
        else {
            continue;
        };

        // The target offset points at the prefix; the group name follows it.
        let prefix_len = "__WXML_GLOBAL__.ops_cached.".len();
        let name_start = target + prefix_len;
        let name_end = src[name_start..]
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
            .map(|offset| name_start + offset)
            .unwrap_or(src.len().min(name_start + 64));
        let name = src[name_start..name_end].to_owned();
        let body = &src[opener + 1..close];
        let Some(helper) = RE_PUSH_HELPER.captures(body) else {
            continue;
        };
        let (Some(fn_name), Some(param), Some(arg)) = (helper.get(1), helper.get(2), helper.get(3))
        else {
            continue;
        };
        if param.as_str() != arg.as_str() {
            continue;
        }
        let Ok(stmts) = crate::js::parse(body, 0, None) else {
            continue;
        };
        let mut ops = Vec::new();
        walk_stmts(&stmts, &mut ops, fn_name.as_str());
        out.entry(name).or_default().extend(ops);
    }
    out
}

fn merged_group_ops(
    src: &str,
    m: &regex::Match<'_>,
    iife: &BTreeMap<String, Vec<Operand>>,
) -> Vec<Operand> {
    let name = group_name(m.as_str());
    let Some(brace) = src[m.start()..].find('{').map(|o| m.start() + o) else {
        return Vec::new();
    };
    let Some(end) = match_delimiter(src, brace) else {
        return Vec::new();
    };
    let mut ops = parse_ops(src, m.start(), end + 1);
    if let Some(extra) = iife.get(&name) {
        ops.extend(extra.iter().cloned());
    }
    ops
}

/// Collects the operand groups defined by `src` itself.
///
/// The appservice and webview bundles of the same package each carry their own copy of
/// a group, and those copies are *not* identical: newer `wcc` output stacks additional
/// operands in between, shifting every subsequent index. Pairing a render function
/// with the wrong copy silently corrupts the binding expressions, so groups are always
/// resolved bundle-locally first and only then from the shared pool.
pub fn local_groups(src: &str) -> BTreeMap<String, Vec<Operand>> {
    local_variants(src)
        .into_iter()
        .filter_map(|(name, variants)| variants.into_iter().max_by_key(Vec::len).map(|v| (name, v)))
        .collect()
}

/// Collects **every** distinct variant of each operand group defined by `src`.
///
/// A build that concatenates several render pipelines (appservice + webview) repeats a
/// group name with different operand counts, and the additional operands shift every
/// later index. Keeping all variants lets the decompiler pick the one that actually
/// covers the indices a given render function references, instead of silently binding
/// expressions to the wrong slot.
pub fn local_variants(src: &str) -> BTreeMap<String, Vec<Vec<Operand>>> {
    if !src.contains("gz$") && !src.contains("ops_cached") {
        return BTreeMap::new();
    }
    let iife = build_iife_ops(src);
    let mut out: BTreeMap<String, Vec<Vec<Operand>>> = BTreeMap::new();
    for m in RE_GZ_DECL.find_iter(src) {
        let name = group_name(m.as_str());
        push_variant(
            out.entry(name).or_default(),
            merged_group_ops(src, &m, &iife),
        );
    }
    for (name, ops) in iife {
        push_variant(out.entry(name).or_default(), ops);
    }
    out
}

fn push_variant(slot: &mut Vec<Vec<Operand>>, candidate: Vec<Operand>) {
    if candidate.is_empty() || slot.contains(&candidate) {
        return;
    }
    slot.push(candidate);
}

/// Harvests every `gz$gwx*` operand group from `src` into `pool`, keeping all variants.
///
/// The pool is only a fallback for groups whose definition lives in a different chunk
/// than the render function that references them.
pub fn collect_ops(src: &str, pool: &mut OperandPool) -> usize {
    // Operand groups only exist where the gz$ render helpers do.
    if !src.contains("gz$") && !src.contains("ops_cached") {
        return 0;
    }
    let mut n = 0;
    for (name, variants) in local_variants(src) {
        let slot = pool.entry(name).or_default();
        let before = slot.len();
        for variant in variants {
            push_variant(slot, variant);
        }
        if slot.len() != before {
            n += 1;
        }
    }
    n
}

/// All known variants of every operand group, keyed by group name.
pub type OperandPool = BTreeMap<String, Vec<Vec<Operand>>>;

/// Picks the variant that covers the render function without over-allocating.
///
/// A variant shorter than the highest index the function references would degrade its
/// bindings to `undefined`, so the smallest *covering* variant wins; when none covers
/// it, the longest available one is used so that as much as possible still resolves.
pub fn select_variant(variants: &[Vec<Operand>], max_index: Option<usize>) -> Vec<Operand> {
    let Some(max_index) = max_index else {
        return variants
            .iter()
            .max_by_key(|v| v.len())
            .cloned()
            .unwrap_or_default();
    };
    let needed = max_index + 1;
    let mut covering: Vec<&Vec<Operand>> = variants.iter().filter(|v| v.len() >= needed).collect();
    covering.sort_by_key(|v| v.len());
    covering
        .first()
        .map(|v| (*v).clone())
        .or_else(|| variants.iter().max_by_key(|v| v.len()).cloned())
        .unwrap_or_default()
}

/// Highest operand index a render function can reference.
///
/// Statement and expression forms are scanned structurally; attribute offset tables
/// passed to `_mz` are folded with the same base/delta rule the runtime applies.
pub fn max_operand_index(body: &[Stmt]) -> Option<usize> {
    let mut max: Option<usize> = None;
    fn bump(max: &mut Option<usize>, value: i64) {
        if value >= 0 {
            let value = value as usize;
            *max = Some(max.map_or(value, |m: usize| m.max(value)));
        }
    }
    fn walk_expr(e: &Ast, max: &mut Option<usize>) {
        match e {
            Ast::Call(callee, args) => {
                if let Some(name) = callee.as_ident() {
                    match name {
                        "_oz" | "_1z" | "_2z" | "_rz" => {
                            if let Some(Ast::Lit(Lit::Num(n))) = args.get(1) {
                                bump(max, *n as i64);
                            }
                            if name == "_rz" {
                                if let Some(Ast::Lit(Lit::Num(n))) = args.get(3) {
                                    bump(max, *n as i64);
                                }
                            }
                        }
                        "_mz" => {
                            if let Some(Ast::Array(items)) = args.get(2) {
                                let flat: Vec<i64> = items
                                    .iter()
                                    .flatten()
                                    .filter_map(|i| match i {
                                        Ast::Lit(Lit::Num(n)) => Some(*n as i64),
                                        _ => None,
                                    })
                                    .collect();
                                let mut base = 0i64;
                                let mut i = 1usize;
                                while i < flat.len() {
                                    let offset = flat[i];
                                    let index = if base == 0 {
                                        base = offset;
                                        offset
                                    } else {
                                        base + offset
                                    };
                                    bump(max, index);
                                    i += 2;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                walk_expr(callee, max);
                for a in args {
                    walk_expr(a, max);
                }
            }
            Ast::Function { body, .. } => walk_stmts(body, max),
            Ast::Binary(_, a, b) | Ast::Assign(_, a, b) => {
                walk_expr(a, max);
                walk_expr(b, max);
            }
            Ast::Unary(_, a) | Ast::Postfix(_, a) => walk_expr(a, max),
            Ast::Cond(c, a, b) => {
                walk_expr(c, max);
                walk_expr(a, max);
                walk_expr(b, max);
            }
            Ast::Array(items) => {
                for i in items.iter().flatten() {
                    walk_expr(i, max);
                }
            }
            Ast::Object(props) => {
                for (_, v) in props {
                    walk_expr(v, max);
                }
            }
            Ast::Member(base, key, _) => {
                walk_expr(base, max);
                walk_expr(key, max);
            }
            _ => {}
        }
    }
    fn walk_stmts(stmts: &[Stmt], max: &mut Option<usize>) {
        for st in stmts {
            match st {
                Stmt::Expr(e) => walk_expr(e, max),
                Stmt::Var { decls, .. } => {
                    for (_, init) in decls {
                        if let Some(init) = init {
                            walk_expr(init, max);
                        }
                    }
                }
                Stmt::Block(inner) => walk_stmts(inner, max),
                Stmt::If { test, cons, alt } => {
                    walk_expr(test, max);
                    walk_stmts(cons, max);
                    if let Some(alt) = alt {
                        walk_stmts(alt, max);
                    }
                }
                Stmt::Try {
                    body,
                    catch,
                    finally,
                    ..
                } => {
                    walk_stmts(body, max);
                    walk_stmts(catch, max);
                    if let Some(f) = finally {
                        walk_stmts(f, max);
                    }
                }
                Stmt::Return(Some(e)) => walk_expr(e, max),
                Stmt::Return(None) => {}
                Stmt::FuncDecl { body, .. } => walk_stmts(body, max),
            }
        }
    }
    walk_stmts(body, &mut max);
    max
}

fn group_name(matched: &str) -> String {
    let rest = matched.find("gz$gwx").map_or(matched, |i| &matched[i..]);
    rest.split('(').next().unwrap_or(rest).trim().to_owned()
}

// ---------------------------------------------------------------------------
// Unit extraction
// ---------------------------------------------------------------------------

/// Matches the `x = [` path table.
static RE_X_ARRAY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|[^\w$])x\s*=\s*\[").expect("valid pattern"));

/// Matches `e_[x[i]] = {f:mN`.
static RE_E_ASSIGN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"e_\[x\[(\d+)\]\]\s*=\s*\{f:(m\d+)").expect("valid pattern"));

/// Matches `d_[x[i]]["name"] = function(`.
static RE_D_NAMED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"d_\[x\[(\d+)\]\]\s*\[\s*(['"])([^'"]+)['"]\s*\]\s*=\s*function\s*\("#)
        .expect("valid pattern")
});

/// Matches `var mN = function(`.
static RE_M_DECL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:^|[^\w$])var\s+(m\d+)\s*=\s*function\s*\(").expect("valid pattern")
});

/// A function body together with its parameter names.
#[derive(Debug, Clone)]
pub struct FunctionBody {
    /// Parameter names.
    pub params: Vec<String>,
    /// Statements.
    pub body: Vec<Stmt>,
}

/// A fully extracted template unit.
#[derive(Debug, Clone)]
pub struct TemplateUnit {
    /// Module path, e.g. `./component/xxx/index.wxml`.
    pub path: String,
    /// Operand group.
    pub ops: Vec<Operand>,
    /// Main render function.
    pub main: FunctionBody,
    /// Named `<template>` members.
    pub named: BTreeMap<String, FunctionBody>,
}

/// Extracts every template unit from an `app-service.js` source string.
pub fn extract_units(src: &str, ops_pool: Option<&OperandPool>) -> Vec<TemplateUnit> {
    // 1) The `x` path array maps template indices to module paths.
    let mut x_paths: Vec<String> = Vec::new();
    if let Some(m) = RE_X_ARRAY.find(src) {
        let arr_start = m.end() - 1;
        if let Some(arr_end) = match_delimiter(src, arr_start) {
            let mut parser = Parser::new(src, arr_start, Some(arr_end + 1));
            if let Ok(Ast::Array(items)) = parser.parse_expression(0) {
                x_paths = items
                    .iter()
                    .flatten()
                    .filter_map(|i| match i {
                        Ast::Lit(Lit::Str(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
            }
        }
    }

    // 2) Operand groups: bundle-local definitions take precedence over the pool, and
    //    the variant that covers the render function's index range wins.
    let local = local_variants(src);
    let resolve_ops = |name: &str, max_index: Option<usize>| -> Vec<Operand> {
        let from_local = local.get(name);
        if let Some(variants) = from_local {
            return select_variant(variants, max_index);
        }
        ops_pool
            .and_then(|pool| pool.get(name))
            .map_or_else(Vec::new, |variants| select_variant(variants, max_index))
    };

    // 3) `e_[x[i]] = {f:mN}` index to render function name.
    let mut idx_to_m: BTreeMap<usize, String> = BTreeMap::new();
    for caps in RE_E_ASSIGN.captures_iter(src) {
        if let (Some(i), Some(name)) = (caps.get(1), caps.get(2)) {
            if let Ok(idx) = i.as_str().parse::<usize>() {
                idx_to_m.insert(idx, name.as_str().to_owned());
            }
        }
    }

    // 4) Named templates: `d_[x[i]]["name"] = function(...)`.
    let mut named_map: BTreeMap<usize, BTreeMap<String, FunctionBody>> = BTreeMap::new();
    for caps in RE_D_NAMED.captures_iter(src) {
        let (Some(i), Some(name), Some(whole)) = (caps.get(1), caps.get(3), caps.get(0)) else {
            continue;
        };
        let Ok(idx) = i.as_str().parse::<usize>() else {
            continue;
        };
        // `whole` ends right after the `(` of `function(`; walking forward from
        // there yields the parameter list and the body.
        let Some(paren_idx) = src[whole.end() - 1..]
            .find('(')
            .map(|o| whole.end() - 1 + o)
        else {
            continue;
        };
        let Some(header_end) = match_delimiter(src, paren_idx) else {
            continue;
        };
        let Some(brace) = src[header_end..].find('{').map(|o| header_end + o) else {
            continue;
        };
        let Some(body_end) = match_delimiter(src, brace) else {
            continue;
        };
        if let Some(fb) = parse_function_at(src, paren_idx, body_end + 1) {
            named_map
                .entry(idx)
                .or_default()
                .insert(name.as_str().to_owned(), fb);
        }
    }

    // 5) Main render functions: `var mN = function(...)`.
    let mut m_body: BTreeMap<String, FunctionBody> = BTreeMap::new();
    for caps in RE_M_DECL.captures_iter(src) {
        let (Some(name), Some(whole)) = (caps.get(1), caps.get(0)) else {
            continue;
        };
        let Some(brace) = src[whole.end() - 1..]
            .find('{')
            .map(|o| whole.end() - 1 + o)
        else {
            continue;
        };
        let Some(end) = match_delimiter(src, brace) else {
            continue;
        };
        if let Some(fb) = parse_function_at(src, whole.end(), end + 1) {
            m_body.insert(name.as_str().to_owned(), fb);
        }
    }

    // 6) Assemble.
    let mut units = Vec::new();
    for (i, path) in x_paths.iter().enumerate() {
        let Some(mname) = idx_to_m.get(&i) else {
            continue;
        };
        let Some(fb) = m_body.get(mname) else {
            continue;
        };
        let ops_name = z_fn_of_body(&fb.body);
        let max_index = max_operand_index(&fb.body);
        units.push(TemplateUnit {
            path: path.clone(),
            ops: resolve_ops(&ops_name, max_index),
            main: fb.clone(),
            named: named_map.remove(&i).unwrap_or_default(),
        });
    }
    units
}

/// Parses the `function` expression that ends at or before `limit`.
///
/// `limit` must sit *before* the body, otherwise an inner nested function would be
/// picked up instead of the outer one.
fn parse_function_at(src: &str, limit: usize, body_end: usize) -> Option<FunctionBody> {
    let slice_end = limit.min(src.len());
    let fn_at = src[..slice_end].rfind("function")?;
    let mut parser = Parser::new(src, fn_at, Some(body_end));
    match parser.parse_expression(0) {
        Ok(Ast::Function { params, body, .. }) => Some(FunctionBody { params, body }),
        _ => None,
    }
}

/// Finds `var z = <name>()` at the top level of a render function body.
fn z_fn_of_body(body: &[Stmt]) -> String {
    for st in body {
        if let Stmt::Var { decls, .. } = st {
            for (name, init) in decls {
                if name == "z" {
                    if let Some(Ast::Call(callee, _)) = init {
                        if let Some(ident) = callee.as_ident() {
                            return ident.to_owned();
                        }
                    }
                }
            }
        }
    }
    String::new()
}

/// A decompiled template ready to be written to disk.
#[derive(Debug, Clone)]
pub struct DecompiledTemplate {
    /// Relative wxml path.
    pub path: String,
    /// Reconstructed WXML source.
    pub wxml: String,
    /// Number of operands in the group.
    pub ops_count: usize,
    /// Warnings raised while interpreting.
    pub warnings: Vec<String>,
}

/// Decompiles an entire `app-service.js`, returning `path -> wxml` plus warnings.
pub fn decompile_app_service(
    src: &str,
    with_templates: bool,
    ops_pool: Option<&OperandPool>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut out = BTreeMap::new();
    let mut warnings = Vec::new();
    for unit in extract_units(src, ops_pool) {
        let template = decompile_one(&unit, with_templates);
        warnings.extend(template.warnings.iter().cloned());
        out.insert(normalize_path(&template.path), template.wxml);
    }
    (out, warnings)
}

/// Decompiles a single template unit.
pub fn decompile_one(unit: &TemplateUnit, with_templates: bool) -> DecompiledTemplate {
    let mut warnings = Vec::new();
    let mut blocks: Vec<String> = Vec::new();

    if unit.ops.is_empty() {
        warnings.push(format!(
            "{}: the operand array is empty (newer lazy-loading / Taro-style bundles ship no \
             operands for this group); bound expressions are rendered as undefined",
            unit.path
        ));
    }

    if with_templates {
        for (name, fb) in &unit.named {
            let mut interp = Interpreter::new(unit.ops.clone(), unit.path.clone());
            let root = Node::virtual_node();
            interp.run(&fb.body, &root, &fb.params);
            warnings.extend(interp.warnings.clone());
            let inner = render_children(&root.borrow(), 1);
            blocks.push(format!("<template name=\"{name}\">\n{inner}\n</template>"));
        }
    }

    let mut interp = Interpreter::new(unit.ops.clone(), unit.path.clone());
    let root = Node::virtual_node();
    interp.run(&unit.main.body, &root, &unit.main.params);
    warnings.extend(interp.warnings.clone());
    blocks.extend(
        render_children(&root.borrow(), 0)
            .split('\n')
            .map(str::to_owned),
    );

    let mut text = blocks.join("\n");
    // Raw fragments copied out of the bundle can still carry a bare directive; no path
    // into the emitter may produce a template the DevTools refuses to compile.
    text = sanitize_bare_directives(&text);
    while text.ends_with('\n') {
        text.pop();
    }
    text.push('\n');

    DecompiledTemplate {
        path: unit.path.clone(),
        wxml: text,
        ops_count: unit.ops.len(),
        warnings,
    }
}

fn render_children(node: &Node, indent: usize) -> String {
    let children = match node {
        Node::Virtual(v) => v.children.clone(),
        Node::Elem(e) => e.children.clone(),
        Node::For(f) => f.children.clone(),
        _ => Vec::new(),
    };
    let mut out = String::new();
    for child in children {
        emit(&child, indent, &mut out);
    }
    out.trim_end_matches('\n').to_owned()
}

/// Gives every directive attribute an explicit value.
///
/// Raw WXML fragments from the bundle occasionally contain `<view wx:if>`; the DevTools
/// rejects the whole template with "value not set". `wx:for` gets an empty array, the
/// conditional directives get `undefined` (falsy, and visibly "unknown").
fn sanitize_bare_directives(source: &str) -> String {
    // Raw fragments can also carry `data="{{...{a: b}}}"`; object literals are not spread.
    let source = rewrite_template_data_literals(source);
    let mut out = String::with_capacity(source.len() + 32);
    let mut index = 0usize;
    let source = source.as_str();
    while index < source.len() {
        let mut replaced = false;
        for (name, fallback) in [
            ("wx:if", "{{undefined}}"),
            ("wx:elif", "{{undefined}}"),
            ("wx:for", "{{[]}}"),
        ] {
            if !source[index..].starts_with(name) {
                continue;
            }
            let before_ok = index == 0
                || matches!(source[..index].chars().next_back(), Some(ch) if ch.is_whitespace() || ch == '<');
            if !before_ok {
                continue;
            }
            let rest = &source[index + name.len()..];
            let bare = !matches!(
                rest.chars().next(),
                Some(ch) if ch == '=' || ch.is_ascii_alphanumeric() || ch == ':' || ch == '_' || ch == '-'
            );
            if !bare {
                continue;
            }
            out.push_str(name);
            out.push_str("=\"");
            out.push_str(fallback);
            out.push('"');
            index += name.len();
            replaced = true;
            break;
        }
        if replaced {
            continue;
        }
        let ch = source[index..].chars().next().unwrap_or(' ');
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// Rewrites `data="{{...{…}}}"` in raw fragments to `data="{{…}}"`.
///
/// The `data` attribute takes a comma-separated entry list, not an object expression.
/// Nested spreads of literals are left for `verify` to report rather than guessed at.
fn rewrite_template_data_literals(source: &str) -> String {
    const PREFIX: &str = "data=\"{{...{";
    let mut out = source.to_owned();
    let mut search = 0usize;
    while let Some(offset) = out[search..].find(PREFIX) {
        let start = search + offset;
        let open = start + PREFIX.len() - 1;
        let Some(close) = matching_brace(&out, open) else {
            search = open + 1;
            continue;
        };
        let inner = out[open + 1..close].to_owned();
        let object = format!("{{{inner}}}");
        let flattened = flatten_object_spreads(&object);
        let payload = flattened
            .strip_prefix('{')
            .and_then(|text| text.strip_suffix('}'))
            .unwrap_or(&flattened)
            .to_owned();
        out.replace_range(open - 3..close + 1, &payload);
        search = open - 3 + payload.len();
    }
    out
}

/// Returns the byte offset of the `}` matching the `{` at `open`, skipping strings.
fn matching_brace(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0i32;
    let mut quote = None;
    let mut i = open;
    while i < bytes.len() {
        let byte = bytes[i];
        if let Some(q) = quote {
            if byte == b'\\' {
                i += 2;
                continue;
            }
            if byte == q {
                quote = None;
            }
        } else {
            match byte {
                b'\'' | b'"' | b'`' => quote = Some(byte),
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Strips leading `./` and `/`.
pub fn normalize_path(p: &str) -> String {
    p.trim_start_matches("./")
        .trim_start_matches('/')
        .to_owned()
}

// ---------------------------------------------------------------------------
// Serialisation
// ---------------------------------------------------------------------------

fn attr_order(name: &str) -> u8 {
    match name {
        "class" => 0,
        "style" => 1,
        "id" => 2,
        _ => 99,
    }
}

fn emit(node: &NodeRef, indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    match &*node.borrow() {
        Node::Text(t) => {
            if t.text.trim().is_empty() {
                return;
            }
            let _ = writeln!(out, "{pad}{}", t.text);
        }
        Node::Virtual(v) => {
            if v.branches.is_empty() {
                for ch in &v.children {
                    emit(ch, indent, out);
                }
                return;
            }
            let mut branches = v.branches.clone();
            // A nested `else { if (b) … }` can be observed before the inner branch's
            // `elif`; `wx:elif` after `wx:else` is rejected by the DevTools. Conditions
            // keep their relative order, `else` always moves to the end.
            branches.sort_by_key(|b| b.cond.is_none());
            // The compiler can record the same `else` twice (once per hoisted child).
            // Keep a single else branch and fold the rest into it.
            let mut folded: Vec<NodeRef> = Vec::new();
            let mut seen_else = false;
            let mut unique = Vec::new();
            for mut branch in branches {
                if branch.cond.is_none() {
                    if seen_else {
                        folded.append(&mut branch.children);
                        continue;
                    }
                    seen_else = true;
                }
                unique.push(branch);
            }
            if let Some(pos) = unique.iter().position(|b| b.cond.is_none()) {
                unique[pos].children.extend(folded);
            }
            let branches = unique;
            for (i, branch) in branches.iter().enumerate() {
                let directive = if i == 0 {
                    "wx:if"
                } else if branch.cond.is_some() {
                    "wx:elif"
                } else {
                    "wx:else"
                };
                emit_conditional(
                    directive,
                    branch.cond.clone(),
                    &branch.children,
                    indent,
                    out,
                );
            }
            for ch in &v.children {
                emit(ch, indent, out);
            }
        }
        Node::Elem(_) => emit_elem(node, indent, out),
        Node::For(f) => emit_for(f, indent, out),
        Node::Template(t) => {
            let mut attrs: Vec<(String, Option<String>)> = Vec::new();
            if let Some(cond) = &t.cond {
                attrs.push(("wx:if".to_owned(), Some(format!("{{{{{cond}}}}}"))));
            }
            attrs.push(("is".to_owned(), Some(template_name(&t.name))));
            if let Some(data) = &t.data {
                attrs.push(("data".to_owned(), Some(format!("{{{{{data}}}}}"))));
            }
            let _ = writeln!(out, "{pad}<template{} />", fmt_attrs(&attrs));
        }
        Node::Raw(r) => {
            let _ = writeln!(out, "{pad}{}", r.html);
        }
    }
}

fn emit_conditional(
    directive: &str,
    cond: Option<String>,
    kids: &[NodeRef],
    indent: usize,
    out: &mut String,
) {
    let pad = "  ".repeat(indent);
    // Lifting a single element child produces output much closer to the original.
    if kids.len() == 1 && matches!(&*kids[0].borrow(), Node::Elem(_)) {
        let prev = {
            let mut child = kids[0].borrow_mut();
            match &mut *child {
                Node::Elem(e) => {
                    let prev = e.cond.take();
                    e.cond = Some((directive.to_owned(), cond.clone()));
                    prev
                }
                _ => None,
            }
        };
        emit_elem(&kids[0], indent, out);
        if let Node::Elem(e) = &mut *kids[0].borrow_mut() {
            e.cond = prev;
        }
        return;
    }
    let head = match &cond {
        Some(c) => format!("{directive}=\"{{{{{c}}}}}\""),
        None => directive.to_owned(),
    };
    let _ = writeln!(out, "{pad}<block {head}>");
    for ch in kids {
        emit(ch, indent + 1, out);
    }
    let _ = writeln!(out, "{pad}</block>");
}

fn emit_elem(node: &NodeRef, indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    let (tag, cond, attrs_snapshot, children) = {
        let n = node.borrow();
        let Node::Elem(e) = &*n else {
            return;
        };
        (
            e.tag.clone(),
            e.cond.clone(),
            e.attrs.clone(),
            e.children.clone(),
        )
    };

    let mut fmt: Vec<(String, Option<String>)> = Vec::new();
    if let Some((directive, cond)) = &cond {
        fmt.push((
            directive.clone(),
            cond.as_ref().map(|c| format!("{{{{{c}}}}}")),
        ));
    }

    let mut ordered = attrs_snapshot;
    ordered.sort_by_key(|a| attr_order(&a.name));
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for attr in ordered {
        if !seen.insert(attr.name.clone()) {
            continue;
        }
        let value = match &attr.value {
            AttrVal::Final(v) => v.clone(),
            AttrVal::Expr(e) => attr_value(e),
        };
        fmt.push((attr.name.clone(), Some(value)));
    }

    let normalized_tag = if tag == "wx-wx-scope" { "block" } else { &tag };
    if children.is_empty() {
        let _ = writeln!(out, "{pad}<{normalized_tag}{} />", fmt_attrs(&fmt));
        return;
    }
    let _ = writeln!(out, "{pad}<{normalized_tag}{}>", fmt_attrs(&fmt));
    for ch in &children {
        emit(ch, indent + 1, out);
    }
    let _ = writeln!(out, "{pad}</{normalized_tag}>");
}

fn emit_for(node: &ForNode, indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    let expr = if node.expr.trim().is_empty() {
        "undefined"
    } else {
        node.expr.as_str()
    };
    let mut attrs: Vec<(String, String)> = vec![("wx:for".to_owned(), format!("{{{{{expr}}}}}"))];
    if !node.item.is_empty() && node.item != "item" {
        attrs.push(("wx:for-item".to_owned(), node.item.clone()));
    }
    if !node.index.is_empty() && node.index != "index" {
        attrs.push(("wx:for-index".to_owned(), node.index.clone()));
    }
    if !node.key.is_empty() {
        // `wx:key` takes a property name, not a mustache expression; the runtime warns
        // about `wx:key="{{item}}"` and suggests the bare name.
        let key = node
            .key
            .trim()
            .strip_prefix("{{")
            .and_then(|s| s.strip_suffix("}}"))
            .unwrap_or(node.key.trim())
            .trim();
        if !key.is_empty() {
            attrs.push(("wx:key".to_owned(), key.to_owned()));
        }
    }

    if node.children.len() == 1 {
        if let Node::Elem(child) = &*node.children[0].borrow() {
            let mut merged: Vec<Attr> = attrs
                .iter()
                .map(|(n, v)| Attr {
                    name: n.clone(),
                    value: AttrVal::Final(v.clone()),
                })
                .collect();
            merged.extend(child.attrs.clone());
            let holder = Rc::new(RefCell::new(Node::Elem(Elem {
                tag: child.tag.clone(),
                attrs: merged,
                children: child.children.clone(),
                cond: child.cond.clone(),
            })));
            emit_elem(&holder, indent, out);
            return;
        }
    }

    let formatted: Vec<(String, Option<String>)> =
        attrs.into_iter().map(|(n, v)| (n, Some(v))).collect();
    let _ = writeln!(out, "{pad}<block{}>", fmt_attrs(&formatted));
    for ch in &node.children {
        emit(ch, indent + 1, out);
    }
    let _ = writeln!(out, "{pad}</block>");
}

fn attr_value(expr: &str) -> String {
    let flat = flatten_object_spreads(expr.trim());
    let s = flat.as_str();
    if s.is_empty() {
        return String::new();
    }
    if s.starts_with("{{") && s.ends_with("}}") {
        return s.to_owned();
    }
    if is_literal_expr(s) {
        return if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
            s[1..s.len() - 1].to_owned()
        } else {
            s.to_owned()
        };
    }
    // WXML rejects a bare object literal in an attribute; parentheses disambiguate it.
    if s.starts_with('{') && s.ends_with('}') {
        return format!("{{{{({s})}}}}");
    }
    format!("{{{{{s}}}}}")
}

/// Merges `...{a: 1}` entries into their enclosing object/array expression.
///
/// WXML accepts object literals only when the nesting stays shallow; flattening literal
/// spreads keeps deeply generated Taro expressions inside the parser's limit while
/// preserving key order and last-wins semantics. Spreads of identifiers are left alone.
fn flatten_object_spreads(source: &str) -> String {
    let pairs = brace_pairs(source);
    let mut out = String::with_capacity(source.len());
    flatten_range(source, &pairs, 0, source.len(), &mut out);
    out
}

/// Maps every `{` to its matching `}` (and back) in one pass.
fn brace_pairs(source: &str) -> Vec<usize> {
    const NONE: usize = usize::MAX;
    let bytes = source.as_bytes();
    let mut pairs = vec![NONE; bytes.len()];
    let mut stack = Vec::new();
    let mut quote = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => quote = Some(c),
            b'{' => stack.push(i),
            b'}' => {
                if let Some(open) = stack.pop() {
                    pairs[open] = i;
                    pairs[i] = open;
                }
            }
            _ => {}
        }
        i += 1;
    }
    pairs
}

fn flatten_range(source: &str, pairs: &[usize], start: usize, end: usize, out: &mut String) {
    let mut i = start;
    let mut quote: Option<char> = None;
    while i < end {
        let Some(ch) = source[i..end].chars().next() else {
            break;
        };
        if let Some(q) = quote {
            if ch == '\\' {
                out.push(ch);
                let mut next = i + 1;
                if let Some(escaped) = source[next..end].chars().next() {
                    out.push(escaped);
                    next += escaped.len_utf8();
                }
                i = next;
                continue;
            }
            out.push(ch);
            if ch == q {
                quote = None;
            }
            i += ch.len_utf8();
            continue;
        }
        if matches!(ch, '\'' | '"' | '`') {
            quote = Some(ch);
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if source[i..].starts_with("...{") {
            let open = i + 3;
            let close = pairs.get(open).copied().unwrap_or(usize::MAX);
            if close != usize::MAX && close < end {
                flatten_range(source, pairs, open + 1, close, out);
                i = close + 1;
                continue;
            }
        }
        out.push(ch);
        i += ch.len_utf8();
    }
}

static TMPL_NAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z_$][\w$.-]*$").expect("valid pattern"));

static LITERAL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^-?\d+(\.\d+)?$").expect("valid pattern"));

fn template_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    if name.starts_with("{{") && name.ends_with("}}") {
        return name.to_owned();
    }
    if TMPL_NAME_RE.is_match(name) {
        return name.to_owned();
    }
    format!("{{{{{name}}}}}")
}

fn is_literal_expr(s: &str) -> bool {
    if is_plain_string_literal(s) {
        return true;
    }
    if LITERAL_RE.is_match(s) {
        return true;
    }
    matches!(s, "true" | "false" | "null")
}

/// A string literal that is not a concatenation such as `'height:' + x + ';'`.
fn is_plain_string_literal(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 || !matches!(bytes[0], b'\'' | b'"') || bytes[0] != bytes[bytes.len() - 1] {
        return false;
    }
    let quote = bytes[0];
    let mut i = 1usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return false;
        }
        i += 1;
    }
    true
}

fn fmt_attrs(attrs: &[(String, Option<String>)]) -> String {
    let mut out = String::new();
    for (name, value) in attrs {
        match value {
            Some(v) if !v.is_empty() => {
                let _ = write!(out, " {name}=\"{}\"", v.replace('"', "&quot;"));
            }
            // A bare `wx:if` (or `bindtap`) is not valid WXML: the DevTools rejects the
            // template with "value not set". Directives always get a mustache; attributes
            // that only make sense with a value are dropped.
            _ if name == "wx:for" => {
                let _ = write!(out, " {name}=\"{{{{[]}}}}\"");
            }
            _ if matches!(name.as_str(), "wx:key" | "wx:for-item" | "wx:for-index") => {}
            // `wx:else` is a bare directive by grammar; a value is at best ignored.
            _ if name == "wx:else" => {
                let _ = write!(out, " {name}");
            }
            _ if name.starts_with("wx:") || is_event_attribute(name) => {
                let _ = write!(out, " {name}=\"{{{{undefined}}}}\"");
            }
            _ => {
                let _ = write!(out, " {name}");
            }
        }
    }
    out
}

/// Event handler attributes (`bindtap`, `catchtap`, `bind:__l`, `mut-bind:change`).
fn is_event_attribute(name: &str) -> bool {
    name.starts_with("bind")
        || name.starts_with("catch")
        || name.starts_with("mut-bind")
        || name.starts_with("capture-bind")
        || name.starts_with("capture-catch")
}

// ---------------------------------------------------------------------------
// Interpreter
// ---------------------------------------------------------------------------

/// Runtime value produced while interpreting a render function.
#[derive(Debug, Clone)]
enum Val {
    Node(NodeRef),
    Op(usize),
    Template(String),
    Func(Vec<String>, Vec<Stmt>),
    Int(i64),
    Str(String),
    Other,
}

/// Bookkeeping for the branch currently being executed.
#[derive(Default)]
struct BranchCtx {
    node: Option<NodeRef>,
    snapshot: Option<usize>,
}

/// Marker error used to unwind a `return` statement.
#[derive(Debug)]
struct Returned;

struct Interpreter {
    ops: Vec<Operand>,
    warnings: Vec<String>,
    module_path: String,
    branch_ctx: Option<BranchCtx>,
    pending_node: Option<NodeRef>,
    oob_range: Option<(usize, usize)>,
}

impl Interpreter {
    fn new(ops: Vec<Operand>, module_path: String) -> Self {
        Self {
            ops,
            warnings: Vec::new(),
            module_path,
            branch_ctx: None,
            pending_node: None,
            oob_range: None,
        }
    }

    fn run(&mut self, body: &[Stmt], root: &NodeRef, params: &[String]) {
        let mut scope: BTreeMap<String, Val> = BTreeMap::new();
        // The third parameter is the parent container by `wcc` convention.
        if params.len() >= 3 {
            scope.insert(params[2].clone(), Val::Node(root.clone()));
        }
        let _ = self.exec_block(body, &mut scope, root);
        if let Some((lo, hi)) = self.oob_range.take() {
            self.warnings.push(format!(
                "{}: template references operand indices {lo}..={hi}, outside the {} entries this \
                 group provides; bound expressions fall back to undefined",
                self.module_path,
                self.ops.len()
            ));
        }
    }

    fn op_text(&mut self, idx: usize) -> String {
        if idx >= self.ops.len() {
            if !self.ops.is_empty() {
                let (lo, hi) = self.oob_range.unwrap_or((idx, idx));
                self.oob_range = Some((lo.min(idx), hi.max(idx)));
            }
            return "undefined".to_owned();
        }
        operand_text(&self.ops[idx], 0, 0)
    }

    fn op_data_text(&mut self, idx: usize) -> String {
        if idx >= self.ops.len() {
            if !self.ops.is_empty() {
                let (lo, hi) = self.oob_range.unwrap_or((idx, idx));
                self.oob_range = Some((lo.min(idx), hi.max(idx)));
            }
            return String::new();
        }
        operand_template_data(&self.ops[idx], 0)
    }

    fn exec_block(
        &mut self,
        stmts: &[Stmt],
        scope: &mut BTreeMap<String, Val>,
        root: &NodeRef,
    ) -> Result<(), Returned> {
        for st in stmts {
            self.exec_stmt(st, scope, root)?;
        }
        Ok(())
    }

    fn exec_stmt(
        &mut self,
        st: &Stmt,
        scope: &mut BTreeMap<String, Val>,
        root: &NodeRef,
    ) -> Result<(), Returned> {
        match st {
            Stmt::Var { decls, .. } => {
                for (name, init) in decls {
                    let v = init
                        .as_ref()
                        .map_or(Val::Other, |e| self.eval(e, scope, root));
                    scope.insert(name.clone(), v);
                }
            }
            Stmt::Expr(e) => {
                self.eval(e, scope, root);
            }
            Stmt::Block(inner) => self.exec_block(inner, scope, root)?,
            Stmt::Return(maybe) => {
                if let Some(e) = maybe {
                    self.eval(e, scope, root);
                }
                return Err(Returned);
            }
            Stmt::If { test, cons, alt } => self.exec_if(test, cons, alt.as_deref(), scope, root),
            Stmt::Try { body, .. } => self.exec_block(body, scope, root)?,
            Stmt::FuncDecl { name, params, body } => {
                scope.insert(
                    name.clone().unwrap_or_default(),
                    Val::Func(params.clone(), body.clone()),
                );
            }
        }
        Ok(())
    }

    fn exec_if(
        &mut self,
        test: &Ast,
        cons: &[Stmt],
        alt: Option<&[Stmt]>,
        scope: &mut BTreeMap<String, Val>,
        root: &NodeRef,
    ) {
        let Some(idx) = self.oz_index(test, scope) else {
            self.exec_unknown_if(test, cons, alt, scope, root);
            return;
        };
        let cond = self.op_text(idx);
        self.exec_branch(Some(cond), None, cons, scope, root);

        let Some(alt) = alt else { return };
        if alt.len() == 1 {
            if let Stmt::If {
                test: ntest,
                cons: ncons,
                alt: nalt,
            } = &alt[0]
            {
                if let Some(nidx) = self.oz_index(ntest, scope) {
                    let ncond = self.op_text(nidx);
                    self.exec_branch(Some(ncond), None, ncons, scope, root);
                    if let Some(nalt) = nalt {
                        self.exec_branch(None, None, nalt, scope, root);
                    }
                    return;
                }
                self.exec_unknown_if(ntest, ncons, nalt.as_deref(), scope, root);
                return;
            }
        }
        self.exec_branch(None, None, alt, scope, root);
    }

    fn exec_unknown_if(
        &mut self,
        test: &Ast,
        cons: &[Stmt],
        alt: Option<&[Stmt]>,
        scope: &mut BTreeMap<String, Val>,
        root: &NodeRef,
    ) {
        let txt = render_ast_text(test);
        if txt.contains("p_[") {
            return; // `if (p_[b]) { ...; return }` recursion guard
        }
        if ["_w(", "_gd(", "gdc", "hG"].iter().any(|m| txt.contains(m)) {
            let _ = self.exec_block(cons, scope, root);
            return;
        }
        // Unknown shape: expand both branches so no content is lost.
        let _ = self.exec_block(cons, scope, root);
        if let Some(alt) = alt {
            let _ = self.exec_block(alt, scope, root);
        }
    }

    fn exec_branch(
        &mut self,
        cond: Option<String>,
        key: Option<String>,
        stmts: &[Stmt],
        scope: &mut BTreeMap<String, Val>,
        root: &NodeRef,
    ) {
        self.branch_ctx = Some(BranchCtx::default());
        self.pending_node = None;

        // A `return` inside a branch still contributes that branch's children.
        let _ = self.exec_block(stmts, scope, root);
        let ctx = self.branch_ctx.take().unwrap_or_default();

        let Some(node) = ctx.node.clone().or_else(|| self.pending_node.clone()) else {
            return;
        };
        if !matches!(&*node.borrow(), Node::Virtual(_)) {
            return;
        }

        let snapshot = ctx.snapshot.unwrap_or_else(|| match &*node.borrow() {
            Node::Virtual(v) => v.children.len(),
            _ => 0,
        });
        let kids: Vec<NodeRef> = {
            let mut n = node.borrow_mut();
            match &mut *n {
                Node::Virtual(v) => {
                    let at = snapshot.min(v.children.len());
                    v.children.split_off(at)
                }
                _ => Vec::new(),
            }
        };
        {
            let mut n = node.borrow_mut();
            if let Node::Virtual(v) = &mut *n {
                merge_branch(&mut v.branches, cond, key, kids);
            }
        }
    }

    fn oz_index(&self, test: &Ast, scope: &BTreeMap<String, Val>) -> Option<usize> {
        let Ast::Call(callee, args) = test else {
            return None;
        };
        if callee.as_ident() != Some("_oz") {
            return None;
        }
        match args.get(1)? {
            Ast::Lit(Lit::Num(n)) => Some(*n as usize),
            Ast::Ident(name) => match scope.get(name) {
                Some(Val::Int(v)) if *v >= 0 => Some(*v as usize),
                _ => None,
            },
            _ => None,
        }
    }

    fn eval(&mut self, e: &Ast, scope: &mut BTreeMap<String, Val>, root: &NodeRef) -> Val {
        match e {
            Ast::Lit(Lit::Str(s)) => Val::Str(s.clone()),
            Ast::Lit(Lit::Num(n)) => Val::Int(*n as i64),
            Ast::Lit(Lit::Bool(b)) => Val::Int(i64::from(*b)),
            Ast::Lit(Lit::Null) => Val::Other,
            Ast::Ident(name) => scope.get(name).cloned().unwrap_or(Val::Other),
            Ast::Function { params, body, .. } => Val::Func(params.clone(), body.clone()),
            Ast::Array(items) => {
                for item in items.iter().flatten() {
                    self.eval(item, scope, root);
                }
                Val::Other
            }
            Ast::Member(..) => Val::Other,
            Ast::Binary(op, a, b) => {
                let left = self.eval(a, scope, root);
                let right = self.eval(b, scope, root);
                if op == "||" || op == "&&" {
                    match left {
                        Val::Other => right,
                        other => other,
                    }
                } else {
                    left
                }
            }
            Ast::Assign(op, target, value) => {
                let val = self.eval(value, scope, root);
                if let Ast::Ident(name) = &**target {
                    if op == "=" {
                        scope.insert(name.clone(), val.clone());
                    }
                    return val;
                }
                if let Ast::Member(base, prop, _) = &**target {
                    let base_val = self.eval(base, scope, root);
                    let prop_name = match &**prop {
                        Ast::Lit(Lit::Str(s)) => Some(s.as_str()),
                        _ => None,
                    };
                    if let (Val::Node(node), Some(prop_name)) = (&base_val, prop_name) {
                        if prop_name == "wxVkey" && matches!(&*node.borrow(), Node::Virtual(_)) {
                            if let Some(ctx) = self.branch_ctx.as_mut() {
                                if ctx.node.is_none() {
                                    ctx.snapshot = match &*node.borrow() {
                                        Node::Virtual(v) => Some(v.children.len()),
                                        _ => None,
                                    };
                                    ctx.node = Some(node.clone());
                                    self.pending_node = Some(node.clone());
                                }
                            } else {
                                self.pending_node = Some(node.clone());
                            }
                        }
                    }
                    return val;
                }
                val
            }
            Ast::Cond(c, a, b) => {
                self.eval(c, scope, root);
                self.eval(a, scope, root);
                self.eval(b, scope, root);
                Val::Other
            }
            Ast::Call(..) => self.call(e, scope, root),
            Ast::Unary(_, inner) => self.eval(inner, scope, root),
            Ast::Postfix(_, inner) => self.eval(inner, scope, root),
            Ast::Template(_) => Val::Str(render_ast_text(e)),
            Ast::Object(props) => {
                for (_, v) in props {
                    self.eval(v, scope, root);
                }
                Val::Other
            }
        }
    }

    fn call(&mut self, e: &Ast, scope: &mut BTreeMap<String, Val>, root: &NodeRef) -> Val {
        let Ast::Call(callee, args) = e else {
            return Val::Other;
        };
        let name = callee.as_ident();

        match name {
            Some("_v") => return Val::Node(Node::virtual_node()),
            Some("_n") => {
                let tag = arg_lit(args, 0).map_or_else(|| "view".to_owned(), |l| lit_to_string(&l));
                return Val::Node(Node::elem(tag));
            }
            Some("_mz") => return self.do_mz(args, scope, root),
            Some("_rz") => return self.do_rz(args, scope, root),
            Some("_oz" | "_1z") => {
                let idx = arg_int(args, 1).unwrap_or(0).max(0) as usize;
                return Val::Op(idx);
            }
            Some("_gd") => {
                let nm = args
                    .get(1)
                    .map_or_else(String::new, |a| self.op_or_lit(a, scope));
                return Val::Template(nm);
            }
            Some("_w" | "_wl" | "_wp" | "$gwn" | "$gwl") => return Val::Other,
            Some("_") => return self.do_append(args, scope, root),
            Some("_2z") => return self.do_for(args, scope, root),
            _ => {}
        }

        let Some(fn_name) = name else {
            return self.call_value(callee, args, scope, root);
        };

        match scope.get(fn_name).cloned() {
            Some(Val::Template(t)) => self.invoke_named_template(&t, args, scope, root),
            Some(Val::Func(params, body)) => {
                self.invoke_func(&params, &body, args, scope, root);
                Val::Other
            }
            _ => {
                for a in args {
                    self.eval(a, scope, root);
                }
                Val::Other
            }
        }
    }

    fn call_value(
        &mut self,
        callee: &Ast,
        args: &[Ast],
        scope: &mut BTreeMap<String, Val>,
        root: &NodeRef,
    ) -> Val {
        match callee {
            Ast::Member(base, _, _) => {
                let base_val = self.eval(base, scope, root);
                if let Val::Template(t) = base_val {
                    return self.invoke_named_template(&t, args, scope, root);
                }
                for a in args {
                    self.eval(a, scope, root);
                }
                Val::Other
            }
            Ast::Ident(n) => match scope.get(n).cloned() {
                Some(Val::Template(t)) => self.invoke_named_template(&t, args, scope, root),
                Some(Val::Func(params, body)) => {
                    self.invoke_func(&params, &body, args, scope, root);
                    Val::Other
                }
                _ => {
                    for a in args {
                        self.eval(a, scope, root);
                    }
                    Val::Other
                }
            },
            Ast::Call(..) => {
                self.eval(callee, scope, root);
                for a in args {
                    self.eval(a, scope, root);
                }
                Val::Other
            }
            _ => Val::Other,
        }
    }

    fn invoke_named_template(
        &mut self,
        name: &str,
        args: &[Ast],
        scope: &mut BTreeMap<String, Val>,
        root: &NodeRef,
    ) -> Val {
        let data = args.first().and_then(|a| self.template_data_text(a, scope));
        let father = args.get(2).map(|a| self.eval(a, scope, root));
        let node = Rc::new(RefCell::new(Node::Template(TemplateNode {
            name: name.to_owned(),
            data,
            cond: None,
        })));
        if let Some(Val::Node(father)) = father {
            Node::push_child(&father, node.clone());
        }
        Val::Node(node)
    }

    fn template_data_text(&mut self, e: &Ast, scope: &mut BTreeMap<String, Val>) -> Option<String> {
        let txt = match e {
            Ast::Binary(op, a, _) if op == "||" => self.template_data_expr(a, scope),
            other => self.template_data_expr(other, scope),
        };
        if txt.is_empty() {
            None
        } else {
            Some(txt)
        }
    }

    /// Renders the template `data` payload, flattening object-literal spreads.
    fn template_data_expr(&mut self, e: &Ast, scope: &mut BTreeMap<String, Val>) -> String {
        match e {
            Ast::Call(callee, args)
                if callee.as_ident().is_some_and(|n| n == "_1z" || n == "_oz") =>
            {
                arg_int(args, 1)
                    .map(|idx| self.op_data_text(idx.max(0) as usize))
                    .unwrap_or_default()
            }
            Ast::Ident(name) => match scope.get(name).cloned() {
                Some(Val::Op(idx)) => self.op_data_text(idx),
                _ => spread_data_text(name.clone()),
            },
            Ast::Binary(op, a, _) if op == "||" => self.template_data_expr(a, scope),
            other => spread_data_text(self.eval_expr_text(other, scope)),
        }
    }

    fn invoke_func(
        &mut self,
        params: &[String],
        body: &[Stmt],
        args: &[Ast],
        scope: &mut BTreeMap<String, Val>,
        root: &NodeRef,
    ) {
        let mut local: BTreeMap<String, Val> = BTreeMap::new();
        for (pname, pval) in params.iter().zip(args.iter()) {
            let v = self.eval(pval, scope, root);
            local.insert(pname.clone(), v);
        }
        let _ = self.exec_block(body, &mut local, root);
    }

    fn eval_expr_text(&mut self, e: &Ast, scope: &mut BTreeMap<String, Val>) -> String {
        match e {
            Ast::Call(callee, args) => {
                let Some(n) = callee.as_ident() else {
                    return String::new();
                };
                if n == "_1z" || n == "_oz" {
                    if let Some(idx) = arg_int(args, 1) {
                        return self.op_text(idx.max(0) as usize);
                    }
                }
                let inner: Vec<String> =
                    args.iter().map(|a| self.eval_expr_text(a, scope)).collect();
                format!("{n}({})", inner.join(", "))
            }
            Ast::Ident(name) => match scope.get(name).cloned() {
                Some(Val::Op(idx)) => self.op_text(idx),
                _ => name.clone(),
            },
            Ast::Lit(l) => literal_text(&lit_to_operand(l)),
            Ast::Binary(_, a, _) => self.eval_expr_text(a, scope),
            _ => String::new(),
        }
    }

    fn op_or_lit(&mut self, e: &Ast, scope: &mut BTreeMap<String, Val>) -> String {
        match e {
            Ast::Lit(l) => literal_text(&lit_to_operand(l)),
            Ast::Ident(name) => match scope.get(name).cloned() {
                Some(Val::Op(idx)) => match self.ops.get(idx) {
                    Some(op @ Operand::Str(_)) => match op {
                        Operand::Str(s) => s.clone(),
                        _ => operand_text(op, 0, 0),
                    },
                    Some(other) => operand_text(other, 0, 0),
                    None => String::new(),
                },
                Some(Val::Str(s)) => s,
                _ => self.eval_expr_text(e, scope),
            },
            _ => self.eval_expr_text(e, scope),
        }
    }

    fn do_mz(&mut self, args: &[Ast], scope: &mut BTreeMap<String, Val>, root: &NodeRef) -> Val {
        if args.len() < 4 {
            return Val::Node(Node::elem("view"));
        }
        let _ = root;
        let tag = arg_lit(args, 1).map_or_else(|| "view".to_owned(), |l| lit_to_string(&l));
        let mut flat: Vec<Val> = Vec::new();
        if let Some(Ast::Array(items)) = args.get(2) {
            for item in items.iter().flatten() {
                flat.push(self.static_value(item, scope));
            }
        }

        let mut node = Elem {
            tag,
            ..Elem::default()
        };
        let mut base = 0i64;
        let mut i = 0usize;
        while i + 1 < flat.len() {
            let aname = match &flat[i] {
                Val::Str(s) => s.clone(),
                Val::Int(v) => v.to_string(),
                _ => String::new(),
            };
            let Val::Int(aoff) = flat[i + 1] else {
                i += 2;
                continue;
            };
            let opidx = if base == 0 {
                base = aoff;
                aoff
            } else {
                base + aoff
            };
            if opidx >= 0 {
                let text = self.op_text(opidx as usize);
                node.attrs.push(Attr {
                    name: aname,
                    value: AttrVal::Expr(text),
                });
            }
            i += 2;
        }
        Val::Node(Rc::new(RefCell::new(Node::Elem(node))))
    }

    fn do_rz(&mut self, args: &[Ast], scope: &mut BTreeMap<String, Val>, root: &NodeRef) -> Val {
        if args.len() < 4 {
            return Val::Other;
        }
        let node_val = self.eval(&args[1], scope, root);
        let aname = arg_lit(args, 2).map(|l| lit_to_string(&l));
        let idx = arg_int(args, 3).unwrap_or(0).max(0) as usize;
        let text = self.op_text(idx);
        if let (Val::Node(node), Some(aname)) = (&node_val, aname) {
            if let Node::Elem(e) = &mut *node.borrow_mut() {
                e.attrs.push(Attr {
                    name: aname,
                    value: AttrVal::Expr(text),
                });
            }
        }
        node_val
    }

    fn do_append(
        &mut self,
        args: &[Ast],
        scope: &mut BTreeMap<String, Val>,
        root: &NodeRef,
    ) -> Val {
        let parent = args.first().map(|a| self.eval(a, scope, root));
        let child = args
            .get(1)
            .map_or(Val::Other, |a| self.eval(a, scope, root));

        let target = match parent {
            Some(Val::Node(n)) => Some(n),
            _ => self.pending_node.clone(),
        };
        let Some(target) = target else { return child };

        match &child {
            Val::Node(n) => Node::push_child(&target, n.clone()),
            Val::Op(idx) => {
                if let Some(raw) = self.ops.get(*idx) {
                    let text = self.text_of(raw);
                    Node::push_child(&target, Rc::new(RefCell::new(Node::Text(Text { text }))));
                }
            }
            Val::Str(s) => Node::push_child(
                &target,
                Rc::new(RefCell::new(Node::Text(Text { text: s.clone() }))),
            ),
            Val::Int(v) => Node::push_child(
                &target,
                Rc::new(RefCell::new(Node::Text(Text {
                    text: v.to_string(),
                }))),
            ),
            _ => {}
        }
        child
    }

    fn do_for(&mut self, args: &[Ast], scope: &mut BTreeMap<String, Val>, root: &NodeRef) -> Val {
        if args.len() < 9 {
            return Val::Other;
        }
        let idx = arg_int(args, 1).unwrap_or(0).max(0) as usize;
        let fn_ast = args.get(2);
        let father_expr = args.get(6);
        let item = arg_lit(args, 7).map(|l| lit_to_string(&l));
        let index = arg_lit(args, 8).map(|l| lit_to_string(&l));
        let key = arg_lit(args, 9).map(|l| lit_to_string(&l));

        let father = father_expr.map(|a| self.eval(a, scope, root));
        let expr = self.op_text(idx);

        let mut node = ForNode {
            expr,
            item: item.unwrap_or_else(|| "item".to_owned()),
            index: index.unwrap_or_else(|| "index".to_owned()),
            key: key.unwrap_or_default(),
            children: Vec::new(),
            cond: None,
        };

        if let Some(fn_ast) = fn_ast {
            let fn_val = self.eval(fn_ast, scope, root);
            if let Val::Func(params, body) = fn_val {
                let holder = Node::virtual_node();
                let mut local: BTreeMap<String, Val> = BTreeMap::new();
                for (pos, pname) in params.iter().enumerate() {
                    local.insert(
                        pname.clone(),
                        if pos == 2 {
                            Val::Node(holder.clone())
                        } else {
                            Val::Other
                        },
                    );
                }
                let _ = self.exec_block(&body, &mut local, &holder);
                node.children = match &*holder.borrow() {
                    Node::Virtual(v) => v.children.clone(),
                    _ => Vec::new(),
                };
            }
        }

        let node = Rc::new(RefCell::new(Node::For(Box::new(node))));
        if let Some(Val::Node(father)) = father {
            Node::push_child(&father, node.clone());
        }
        Val::Node(node)
    }

    fn text_of(&self, raw: &Operand) -> String {
        match raw {
            Operand::Str(s) => {
                if s.contains("{{") {
                    format!("{{{{{s}}}}}")
                } else {
                    escape_text(s)
                }
            }
            Operand::Num(n) => escape_text(&format_num(*n)),
            Operand::Bool(b) => b.to_string(),
            Operand::Null => "null".to_owned(),
            other => {
                // An operand we could not resolve used to render as `{{}}`, which is not a
                // valid mustache and makes the DevTools fail the build; `undefined` keeps the
                // syntax intact and states the truth about the lost binding.
                let text = operand_text(other, 0, 0);
                if text.trim().is_empty() {
                    "{{undefined}}".to_owned()
                } else {
                    format!("{{{{{text}}}}}")
                }
            }
        }
    }

    fn static_value(&self, e: &Ast, scope: &BTreeMap<String, Val>) -> Val {
        match e {
            Ast::Lit(Lit::Str(s)) => Val::Str(s.clone()),
            Ast::Lit(Lit::Num(n)) => Val::Int(*n as i64),
            Ast::Ident(name) => scope.get(name).cloned().unwrap_or(Val::Other),
            _ => Val::Other,
        }
    }
}

fn merge_branch(
    branches: &mut Vec<Branch>,
    cond: Option<String>,
    key: Option<String>,
    kids: Vec<NodeRef>,
) {
    for branch in branches.iter_mut() {
        if branch.cond == cond && branch.key == key && branch.children.is_empty() {
            branch.children = kids;
            return;
        }
    }
    branches.push(Branch {
        cond,
        key,
        children: kids,
    });
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn lit_to_operand(l: &Lit) -> Operand {
    match l {
        Lit::Null => Operand::Null,
        Lit::Bool(b) => Operand::Bool(*b),
        Lit::Num(n) => Operand::Num(*n),
        Lit::Str(s) => Operand::Str(s.clone()),
    }
}

fn lit_to_string(l: &Lit) -> String {
    match l {
        Lit::Null => "null".to_owned(),
        Lit::Bool(b) => b.to_string(),
        Lit::Num(n) => format_num(*n),
        Lit::Str(s) => s.clone(),
    }
}

fn arg_lit(args: &[Ast], i: usize) -> Option<Lit> {
    match args.get(i)? {
        Ast::Lit(l) => Some(l.clone()),
        _ => None,
    }
}

fn arg_int(args: &[Ast], i: usize) -> Option<i64> {
    match arg_lit(args, i)? {
        Lit::Num(n) => Some(n as i64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICE: &str = r#"
x = ["./pages/index/index.wxml"];
function gz$gwx_1(){var z=[];Z([3,'hello']);Z([7,[3,'msg']]);return z}
var m0 = function(e,s,r,gg){
  var z=gz$gwx_1();
  var oB=_v(); _(r,oB);
  if(_oz(z,0,e,s,gg)){oB.wxVkey=1
    var xC=_v(); _(oB,xC);
    var eN=_mz(z,'view',['class',1],['id',2],e,s,gg);
    var cF=_oz(z,1,e,s,gg);
    _(eN,cF);
    _(xC,eN);
  }
  return r
};
e_[x[0]] = {f:m0};
"#;

    #[test]
    fn extracts_paths_and_groups() {
        let units = extract_units(SERVICE, None);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].path, "./pages/index/index.wxml");
        assert_eq!(units[0].ops.len(), 2);
    }

    #[test]
    fn numeric_opcodes_render_as_literals() {
        let op = Operand::Arr(vec![Operand::Num(3.0), Operand::Str("hello".to_owned())]);
        assert_eq!(operand_text(&op, 0, 0), "'hello'");
        // Real `wcc` output nests the opcode: `Z([[7],[3,'msg']])`.
        let var_op = Operand::Arr(vec![
            Operand::Arr(vec![Operand::Num(7.0)]),
            Operand::Arr(vec![Operand::Num(3.0), Operand::Str("msg".to_owned())]),
        ]);
        assert_eq!(operand_text(&var_op, 0, 0), "msg");
    }

    #[test]
    fn binary_and_member_opcodes_render() {
        let eq = Operand::Arr(vec![
            Operand::Arr(vec![Operand::Num(2.0), Operand::Str("===".to_owned())]),
            Operand::Arr(vec![
                Operand::Arr(vec![Operand::Num(7.0)]),
                Operand::Arr(vec![Operand::Num(3.0), Operand::Str("a".to_owned())]),
            ]),
            Operand::Arr(vec![Operand::Num(1.0), Operand::Num(1.0)]),
        ]);
        assert_eq!(operand_text(&eq, 0, 0), "a === 1");

        let member = Operand::Arr(vec![
            Operand::Arr(vec![Operand::Num(6.0)]),
            Operand::Arr(vec![
                Operand::Arr(vec![Operand::Num(7.0)]),
                Operand::Arr(vec![Operand::Num(3.0), Operand::Str("user".to_owned())]),
            ]),
            Operand::Arr(vec![Operand::Num(3.0), Operand::Str("name".to_owned())]),
        ]);
        assert_eq!(operand_text(&member, 0, 0), "user.name");
    }

    #[test]
    fn string_concat_uses_plus() {
        // `Z([[2,'+'],[1,'height:'],[[7],[3,'f']]])`
        let concat = Operand::Arr(vec![
            Operand::Arr(vec![Operand::Num(2.0), Operand::Str("+".to_owned())]),
            Operand::Arr(vec![Operand::Num(1.0), Operand::Str("height:".to_owned())]),
            Operand::Arr(vec![
                Operand::Arr(vec![Operand::Num(7.0)]),
                Operand::Arr(vec![Operand::Num(3.0), Operand::Str("f".to_owned())]),
            ]),
        ]);
        assert_eq!(operand_text(&concat, 0, 0), "'height:' + f");
    }

    #[test]
    fn ternary_and_call_opcodes_render() {
        let ternary = Operand::Arr(vec![
            Operand::Arr(vec![Operand::Num(2.0), Operand::Str("?:".to_owned())]),
            Operand::Arr(vec![
                Operand::Arr(vec![Operand::Num(7.0)]),
                Operand::Arr(vec![Operand::Num(3.0), Operand::Str("ok".to_owned())]),
            ]),
            Operand::Arr(vec![Operand::Num(1.0), Operand::Num(1.0)]),
            Operand::Arr(vec![Operand::Num(1.0), Operand::Num(0.0)]),
        ]);
        assert_eq!(operand_text(&ternary, 0, 0), "ok ? 1 : 0");

        let call = Operand::Arr(vec![
            Operand::Arr(vec![Operand::Num(12.0)]),
            Operand::Arr(vec![
                Operand::Arr(vec![Operand::Num(7.0)]),
                Operand::Arr(vec![Operand::Num(3.0), Operand::Str("fn".to_owned())]),
            ]),
            Operand::Arr(vec![Operand::Num(3.0), Operand::Str("x".to_owned())]),
        ]);
        assert_eq!(operand_text(&call, 0, 0), "fn('x')");
    }

    #[test]
    fn decompiles_structure_and_bindings() {
        let (map, warnings) = decompile_app_service(SERVICE, true, None);
        let wxml = &map["pages/index/index.wxml"];
        assert!(wxml.contains("<view"), "{wxml}");
        assert!(wxml.contains("wx:if"), "{wxml}");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn quotes_are_escaped() {
        assert_eq!(quote_js("it's"), r"'it\'s'");
    }

    #[test]
    fn lazy_iife_groups_are_harvested() {
        let src = r#"
(function(z){var a=11;function Z(ops){z.push(ops)}
Z([3,'a']);Z([1,2]);
})(__WXML_GLOBAL__.ops_cached.$gwx_9)
"#;
        let iife = build_iife_ops(src);
        assert_eq!(iife["$gwx_9"].len(), 2);
    }

    #[test]
    fn object_spread_flattening_preserves_utf8() {
        let plain = "placeholder: '粘贴视频链接（抖音、B站）'";
        assert_eq!(flatten_object_spreads(plain), plain);
        assert_eq!(
            flatten_object_spreads("{...{a: '中文'}, ...{b: 1}}"),
            "{a: '中文', b: 1}"
        );
    }

    #[test]
    fn template_data_flattens_object_literals() {
        fn spread(children: Vec<Operand>) -> Operand {
            let mut items = vec![Operand::Arr(vec![Operand::Num(9.0)])];
            items.extend(children);
            Operand::Arr(items)
        }
        fn entry(key: &str, value: Operand) -> Operand {
            Operand::Arr(vec![
                Operand::Arr(vec![Operand::Num(8.0)]),
                Operand::Str(key.to_owned()),
                value,
            ])
        }
        let data = spread(vec![
            entry("content", Operand::Str("content".to_owned())),
            spread(vec![entry("name", Operand::Str("x".to_owned()))]),
        ]);
        assert_eq!(
            operand_template_data(&data, 0),
            "content: 'content', name: 'x'"
        );

        let identifier = Operand::Arr(vec![
            Operand::Arr(vec![Operand::Num(7.0)]),
            Operand::Arr(vec![Operand::Num(3.0), Operand::Str("item".to_owned())]),
        ]);
        assert_eq!(operand_template_data(&identifier, 0), "...item");
    }

    #[test]
    fn raw_template_data_object_literals_are_rewritten() {
        let html = r#"<template is="x" data="{{...{content: content}}}" />"#;
        let fixed = sanitize_bare_directives(html);
        assert!(fixed.contains(r#"data="{{content: content}}""#), "{fixed}");
    }

    #[test]
    fn else_branches_are_emitted_after_elif() {
        let root = Node::virtual_node();
        {
            let mut node = root.borrow_mut();
            let Node::Virtual(v) = &mut *node else {
                panic!("virtual node expected");
            };
            v.branches.push(Branch {
                cond: Some("a".to_owned()),
                ..Branch::default()
            });
            v.branches.push(Branch::default());
            v.branches.push(Branch {
                cond: Some("b".to_owned()),
                ..Branch::default()
            });
            v.branches.push(Branch {
                children: vec![Node::elem("view")],
                ..Branch::default()
            });
        }
        let mut out = String::new();
        emit(&root, 0, &mut out);
        let if_pos = out.find("wx:if=\"{{a}}\"").unwrap();
        let elif_pos = out.find("wx:elif=\"{{b}}\"").unwrap();
        let else_pos = out.find("wx:else").unwrap();
        assert!(if_pos < elif_pos && elif_pos < else_pos, "{out}");
        assert!(!out.contains("wx:else=\"{{undefined}}\""), "{out}");
        assert_eq!(out.matches("wx:else").count(), 1, "{out}");
    }

    #[test]
    fn raw_fragments_are_sanitised_too() {
        let html = r#"<view wx:if><block wx:for><text wx:elif>x</text></block></view>"#;
        let fixed = sanitize_bare_directives(html);
        assert!(!fixed.contains("<view wx:if>"), "{fixed}");
        assert!(fixed.contains(r#"wx:if="{{undefined}}""#), "{fixed}");
        assert!(fixed.contains(r#"wx:for="{{[]}}""#), "{fixed}");
        assert!(fixed.contains(r#"wx:elif="{{undefined}}""#), "{fixed}");
        // A properly valued attribute is left alone.
        let ok = sanitize_bare_directives(r#"<view wx:if="{{a}}" wx:for-item="x" />"#);
        assert_eq!(ok, r#"<view wx:if="{{a}}" wx:for-item="x" />"#);
    }

    #[test]
    fn bare_directives_are_never_emitted() {
        assert_eq!(
            fmt_attrs(&[("wx:if".to_owned(), None)]),
            " wx:if=\"{{undefined}}\""
        );
        assert_eq!(
            fmt_attrs(&[("wx:for".to_owned(), Some(String::new()))]),
            " wx:for=\"{{[]}}\""
        );
        assert_eq!(
            fmt_attrs(&[("bindtap".to_owned(), None)]),
            " bindtap=\"{{undefined}}\""
        );
        assert_eq!(fmt_attrs(&[("disabled".to_owned(), None)]), " disabled");
        assert_eq!(fmt_attrs(&[("wx:key".to_owned(), None)]), "");
    }

    #[test]
    fn empty_operand_group_reports_a_warning() {
        let src = "x=['./a.wxml'];var m0=function(e,s,r,gg){var z=gz$gwx_1();return r};e_[x[0]]={f:m0};function gz$gwx_1(){var z=[];return z}";
        let (map, warns) = decompile_app_service(src, true, None);
        assert!(map.contains_key("a.wxml"));
        assert!(
            warns.iter().any(|w| w.contains("operand array is empty")),
            "{warns:?}"
        );
    }

    #[test]
    fn for_loops_become_wx_for() {
        let src = r#"
x = ["./a.wxml"];
function gz$gwx_1(){var z=[];Z([3,'list']);Z([3,'item']);return z}
var m0 = function(e,s,r,gg){
  var z=gz$gwx_1();
  var oB=_v(); _(r,oB);
  _2z(z,0,function(item,index,f){var cE=_mz(z,'view',['class',0],[],e,s,gg);_(f,cE)},0,0,0,oB,'item','index');
  return r
};
e_[x[0]] = {f:m0};
"#;
        let (map, _) = decompile_app_service(src, true, None);
        let wxml = &map["a.wxml"];
        assert!(wxml.contains("wx:for"), "{wxml}");
    }
}
