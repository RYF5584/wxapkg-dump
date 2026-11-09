// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! JavaScript / WXS formatter.
//!
//! Operates on the token stream produced by [`crate::js::tokenize`], so strings,
//! template literals, comments and regular expression literals are copied verbatim and
//! can never be mis-parsed — the classic failure mode of regex-based beautifiers.
//!
//! The output rules follow `js-beautify`'s defaults: statements break after `;` and
//! block braces, binary operators are surrounded by spaces, call parentheses hug their
//! callee, member access never gets spaces around the dot, and object-literal colons are
//! written without a leading space.

use crate::beautify::FormatOptions;
use crate::js::{tokenize, Token, TokenKind};

fn is_word(tok: &Token) -> bool {
    matches!(
        tok.kind,
        TokenKind::Ident | TokenKind::Num | TokenKind::Str | TokenKind::Template | TokenKind::Regex
    )
}

fn is_binary_operator(value: &str) -> bool {
    matches!(
        value,
        "=" | "+="
            | "-="
            | "*="
            | "/="
            | "%="
            | "&="
            | "|="
            | "^="
            | "<<="
            | ">>="
            | ">>>="
            | "&&="
            | "||="
            | "??="
            | "=="
            | "!="
            | "==="
            | "!=="
            | "<"
            | ">"
            | "<="
            | ">="
            | "+"
            | "-"
            | "*"
            | "/"
            | "%"
            | "**"
            | "&&"
            | "||"
            | "??"
            | "&"
            | "|"
            | "^"
            | "<<"
            | ">>"
            | ">>>"
            | "=>"
    )
}

/// Keywords that must keep a space before a following `(`.
fn keyword_needs_paren_space(word: &str) -> bool {
    matches!(
        word,
        "if" | "for"
            | "while"
            | "switch"
            | "catch"
            | "with"
            | "return"
            | "typeof"
            | "delete"
            | "void"
            | "in"
            | "of"
            | "new"
    )
}

fn needs_space_before_brace(prev: Option<&Token>) -> bool {
    match prev {
        None => false,
        Some(p) => match p.kind {
            TokenKind::Punct => !matches!(p.value.as_str(), "(" | "[" | "{" | ";" | ":" | ","),
            TokenKind::Eof => false,
            _ => true,
        },
    }
}

fn is_glue_word(value: &str) -> bool {
    matches!(value, "else" | "catch" | "finally" | "while")
}

/// Whether `{` that follows `prev` opens an object literal rather than a block.
fn opens_object_literal(prev: Option<&Token>) -> bool {
    match prev {
        None => false,
        Some(p) => match p.kind {
            TokenKind::Punct => !matches!(p.value.as_str(), ")" | "]" | "}" | "{" | ";"),
            TokenKind::Ident => {
                matches!(
                    p.value.as_str(),
                    "return"
                        | "typeof"
                        | "case"
                        | "in"
                        | "of"
                        | "new"
                        | "do"
                        | "else"
                        | "yield"
                        | "await"
                ) && !matches!(p.value.as_str(), "do" | "else")
            }
            TokenKind::Eof => false,
            _ => true,
        },
    }
}

/// Formats `source` as readable JavaScript.
pub fn beautify_js(source: &str, options: &FormatOptions) -> String {
    let indent_unit = options.indent_unit();
    let mut out = String::with_capacity(source.len() + source.len() / 4);
    let mut at_line_start = true;
    let mut level: usize = 0;
    let mut paren_depth: i32 = 0;
    let mut question_depth: i32 = 0;
    let mut prev: Option<Token> = None;
    let mut blank_lines = 0usize;

    let tokens: Vec<Token> = tokenize(source, 0, None).collect();

    // Short, single-level object literals are kept on one line, matching js-beautify.
    let mut inline_close: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    for index in 0..tokens.len() {
        if tokens[index].kind != TokenKind::Punct || tokens[index].value != "{" {
            continue;
        }
        let prev = index.checked_sub(1).map(|i| &tokens[i]);
        if !opens_object_literal(prev) {
            continue;
        }
        let mut depth = 0i32;
        let mut count = 0usize;
        let mut eligible = true;
        let mut close = None;
        for (offset, candidate) in tokens[index..].iter().enumerate() {
            if candidate.kind == TokenKind::Eof {
                eligible = false;
                break;
            }
            if candidate.kind == TokenKind::Punct {
                match candidate.value.as_str() {
                    "{" => {
                        depth += 1;
                        if depth > 1 {
                            eligible = false;
                        }
                    }
                    "}" => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(index + offset);
                            break;
                        }
                    }
                    ";" => eligible = false,
                    _ => {}
                }
            }
            count += 1;
            if count > 14 {
                eligible = false;
            }
        }
        if eligible {
            if let Some(close) = close {
                inline_close.insert(index, close);
            }
        }
    }
    let mut brace_stack: Vec<bool> = Vec::new();

    macro_rules! emit {
        ($text:expr) => {{
            if at_line_start {
                if blank_lines > 0 {
                    out.push('\n');
                    blank_lines = 0;
                }
                out.push_str(&indent_unit.repeat(level));
                at_line_start = false;
            }
            out.push_str($text);
        }};
    }

    for index in 0..tokens.len() {
        let tok = tokens[index].clone();
        if tok.kind == TokenKind::Eof {
            break;
        }
        let next = tokens.get(index + 1);

        if options.preserve_line_breaks && !at_line_start {
            if let Some(previous) = &prev {
                let between = source.get(previous.end..tok.start).unwrap_or("");
                let breaks = between.matches('\n').count();
                if breaks > 0 {
                    blank_lines = breaks.saturating_sub(1);
                    newline(&mut out, &mut at_line_start);
                }
            }
        }

        match tok.kind {
            TokenKind::Punct => match tok.value.as_str() {
                "{" => {
                    if needs_space_before_brace(prev.as_ref()) {
                        emit!(" ");
                    }
                    emit!("{");
                    let inline = inline_close.contains_key(&index);
                    brace_stack.push(inline);
                    if !inline {
                        level += 1;
                        newline(&mut out, &mut at_line_start);
                    }
                }
                "}" => {
                    let inline = brace_stack.pop().unwrap_or(false);
                    if !inline {
                        level = level.saturating_sub(1);
                        newline(&mut out, &mut at_line_start);
                    }
                    emit!("}");
                    let glue = inline
                        || next.is_some_and(|n| {
                            (n.kind == TokenKind::Punct
                                && matches!(n.value.as_str(), ";" | "," | ")" | "]" | ":"))
                                || (n.kind == TokenKind::Ident && is_glue_word(&n.value))
                        });
                    if !glue {
                        newline(&mut out, &mut at_line_start);
                    }
                }
                "(" => {
                    let space = prev.as_ref().is_some_and(|p| {
                        p.kind == TokenKind::Ident && keyword_needs_paren_space(&p.value)
                    });
                    if space {
                        emit!(" ");
                    }
                    emit!("(");
                    paren_depth += 1;
                }
                ")" => {
                    paren_depth -= 1;
                    emit!(")");
                }
                "[" => {
                    emit!("[");
                    paren_depth += 1;
                }
                "]" => {
                    paren_depth -= 1;
                    emit!("]");
                }
                ";" => {
                    emit!(";");
                    if paren_depth <= 0 {
                        newline(&mut out, &mut at_line_start);
                        question_depth = 0;
                    } else {
                        emit!(" ");
                    }
                }
                "," => {
                    emit!(", ");
                    question_depth = 0;
                }
                "." | "?." | "..." | "++" | "--" => {
                    emit!(&tok.value);
                }
                "?" => {
                    emit!(" ? ");
                    question_depth += 1;
                }
                ":" => {
                    if question_depth > 0 {
                        question_depth -= 1;
                        emit!(" : ");
                    } else {
                        emit!(": ");
                    }
                }
                other if is_binary_operator(other) => {
                    emit!(" ");
                    emit!(other);
                    emit!(" ");
                }
                other => {
                    emit!(other);
                }
            },
            TokenKind::Ident => {
                let glue = prev
                    .as_ref()
                    .is_some_and(|p| p.kind == TokenKind::Punct && p.value == "}")
                    && is_glue_word(&tok.value);
                if glue || prev.as_ref().is_some_and(is_word) {
                    emit!(" ");
                }
                let value = tok.value.clone();
                emit!(&value);
                // `else {` takes its space from the brace rule.
                let brace_follows =
                    next.is_some_and(|n| n.kind == TokenKind::Punct && n.value == "{");
                let is_if_chain =
                    next.is_some_and(|n| n.kind == TokenKind::Ident && n.value == "if");
                let at_end = next.is_none_or(|n| n.kind == TokenKind::Eof);
                if matches!(value.as_str(), "else" | "try" | "finally" | "do")
                    && !brace_follows
                    && !is_if_chain
                    && !at_end
                {
                    emit!(" ");
                }
            }
            _ => {
                if prev.as_ref().is_some_and(is_word) {
                    emit!(" ");
                }
                let value = tok.value.clone();
                emit!(&value);
            }
        }

        prev = Some(tok);
    }

    while out.ends_with(' ') {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn newline(out: &mut String, at_line_start: &mut bool) {
    while out.ends_with(' ') {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    *at_line_start = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn fmt(src: &str) -> String {
        beautify_js(src, &FormatOptions::default())
    }

    #[test]
    fn breaks_statements_and_indents_blocks() {
        let out = fmt("function a(){var b=1;return b;}");
        assert_eq!(out, "function a() {\n  var b = 1;\n  return b;\n}\n");
    }

    #[test]
    fn keeps_strings_verbatim() {
        let src = r#"var a="x  y;z{}";"#;
        assert!(fmt(src).contains(r#""x  y;z{}""#));
    }

    #[test]
    fn else_stays_on_the_closing_brace_line() {
        let out = fmt("if(a){b()}else{c()}");
        assert!(out.contains("} else {"), "{out}");
    }

    #[test]
    fn object_literals_use_a_compact_colon() {
        let out = fmt("var o={a:1,b:2};");
        assert!(out.contains("a: 1"), "{out}");
        assert!(out.contains("b: 2"), "{out}");
        assert!(out.contains("};\n"), "{out}");
    }

    #[test]
    fn a_ternary_with_a_leading_dot_number_stays_valid() {
        // Minimised wcc output writes `a?1!==b&&c?.5:1`; the beautified text must remain
        // parseable, which means `?.` cannot be treated as optional chaining here.
        let out = fmt("var a=1!==b&&c?.5:1;");
        assert!(out.contains("? .5 : 1"), "{out}");
        assert!(!out.contains("?."), "{out}");
    }

    #[test]
    fn member_access_has_no_spaces() {
        let out = fmt("a.b.c();");
        assert!(out.contains("a.b.c()"), "{out}");
    }

    #[test]
    fn for_headers_do_not_break_on_semicolons() {
        let out = fmt("for(var i=0;i<3;i++){x()}");
        assert!(out.contains("for (var i = 0; i < 3; i++)"), "{out}");
    }

    #[test]
    fn regex_literals_survive() {
        let out = fmt("var r=/a\\/b/g;");
        assert!(out.contains("/a\\/b/g"), "{out}");
    }

    #[test]
    fn a_regex_after_a_for_header_survives_verbatim() {
        // A real bundle (`plugins/wxParse/showdown.js`) put a regex directly after a
        // brace-less `for` header; splitting it produced invalid JavaScript.
        let out = fmt("for(a=0;a<b;++a)/^[ \\t]{0,3}\\|/.test(a);");
        assert!(out.contains("/^[ \\t]{0,3}\\|/"), "{out}");
        assert!(!out.contains("\\ |"), "{out}");
    }

    #[test]
    fn arrow_functions_keep_their_shape() {
        let out = fmt("const f=(a)=>a+1;");
        assert!(out.contains("(a) => a + 1"), "{out}");
    }

    #[test]
    fn ternaries_keep_spaced_colons() {
        let out = fmt("const x=a?b:c;");
        assert!(out.contains("a ? b : c"), "{out}");
    }

    #[test]
    fn nested_call_arguments_stay_inline() {
        let out = fmt("f(g(1,2),{k:3});");
        assert!(out.contains("f(g(1, 2), {k: 3})"), "{out}");
    }

    #[test]
    fn source_line_breaks_are_preserved() {
        let out = fmt("var a = 1;\nvar b = 2;");
        assert_eq!(out, "var a = 1;\nvar b = 2;\n");
    }
}
