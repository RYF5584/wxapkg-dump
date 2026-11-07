// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Character-level JavaScript lexer.
//!
//! The lexer is intentionally forgiving: malformed input degrades into slightly
//! different tokens rather than an error, because compiled bundles routinely contain
//! fragments that are only valid in the context of the whole file.

use once_cell::sync::Lazy;

/// Classification of a lexical unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Identifier or keyword.
    Ident,
    /// Numeric literal.
    Num,
    /// Single- or double-quoted string literal (value includes the quotes).
    Str,
    /// Template literal (value includes the backticks).
    Template,
    /// Regular expression literal (value includes the delimiters and flags).
    Regex,
    /// Punctuation or operator.
    Punct,
    /// End of input marker.
    Eof,
}

/// A lexical unit with its byte range inside the source buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Token classification.
    pub kind: TokenKind,
    /// Raw source text of the token.
    pub value: String,
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl Token {
    fn new(kind: TokenKind, value: impl Into<String>, start: usize, end: usize) -> Self {
        Self {
            kind,
            value: value.into(),
            start,
            end,
        }
    }
}

/// Keywords after which a `/` must start a regular expression, not a division.
static REGEX_PREV_KEYWORDS: Lazy<std::collections::HashSet<&'static str>> = Lazy::new(|| {
    [
        "return",
        "typeof",
        "instanceof",
        "in",
        "of",
        "new",
        "delete",
        "void",
        "throw",
        "case",
        "do",
        "else",
        "yield",
        "await",
    ]
    .into_iter()
    .collect()
});

/// Multi-character operators, longest first so greedy matching is correct.
static PUNCT: Lazy<Vec<&'static str>> = Lazy::new(|| {
    let mut ops = vec![
        ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==",
        "!=", "<=", ">=", "&&", "||", "??", "?.", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=",
        "|=", "^=", "<<", ">>", "**",
    ];
    ops.sort_by_key(|op| std::cmp::Reverse(op.len()));
    ops
});

#[inline]
const fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

#[inline]
const fn is_ident_body(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

#[inline]
fn char_len_at(src: &str, i: usize) -> usize {
    src[i..].chars().next().map_or(1, char::len_utf8)
}

fn skip_ws_and_comments(src: &str, mut i: usize, n: usize) -> usize {
    let bytes = src.as_bytes();
    while i < n {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0x0b => i += 1,
            0xC2 | 0xC3 if i + 1 < n && matches!(bytes[i + 1], 0xA0) => i += 2, // NBSP
            0xEF if i + 2 < n && bytes[i + 1] == 0xBB && bytes[i + 2] == 0xBF => i += 3, // BOM
            b'/' if i + 1 < n && bytes[i + 1] == b'/' => {
                i = match src[i + 2..n].find('\n') {
                    Some(off) => i + 2 + off + 1,
                    None => n,
                };
            }
            b'/' if i + 1 < n && bytes[i + 1] == b'*' => {
                i = match src[i + 2..n].find("*/") {
                    Some(off) => i + 2 + off + 2,
                    None => n,
                };
            }
            _ => break,
        }
    }
    i
}

fn read_string(src: &str, mut i: usize, n: usize) -> usize {
    let bytes = src.as_bytes();
    let quote = bytes[i];
    i += 1;
    while i < n {
        let c = bytes[i];
        if c == b'\\' {
            i += 1;
            if i < n {
                i += char_len_at(src, i);
            }
            continue;
        }
        if c == quote {
            return i + 1;
        }
        if c == b'\n' {
            // Unterminated single-line string: recover at the line break.
            return i;
        }
        i += char_len_at(src, i);
    }
    n
}

fn read_template(src: &str, mut i: usize, n: usize) -> usize {
    let bytes = src.as_bytes();
    i += 1;
    let mut depth: Vec<u8> = Vec::new();
    while i < n {
        let c = bytes[i];
        if c == b'\\' {
            i += 1;
            if i < n {
                i += char_len_at(src, i);
            }
            continue;
        }
        if c == b'`' && depth.is_empty() {
            return i + 1;
        }
        if c == b'$' && i + 1 < n && bytes[i + 1] == b'{' {
            depth.push(b'}');
            i += 2;
            continue;
        }
        if !depth.is_empty() {
            match c {
                b'{' => depth.push(b'}'),
                b'(' => depth.push(b')'),
                b'[' => depth.push(b']'),
                other if Some(&other) == depth.last() => {
                    depth.pop();
                }
                _ => {}
            }
        }
        i += 1;
    }
    n
}

/// Consumes a complete numeric literal.
///
/// A naive `while is_ident_body` loop stops at the sign of an exponent, so `1e-4` becomes
/// `1e`, `-`, `4` and any reformatter that re-inserts spaces turns valid code into a syntax
/// error. The scanner therefore follows the ECMAScript numeric grammar: radix prefixes,
/// digit separators, a fraction, an optional signed exponent, and a BigInt suffix.
fn read_number(src: &str, start: usize, n: usize) -> usize {
    let bytes = src.as_bytes();
    let mut i = start;

    if bytes[i] == b'0'
        && i + 1 < n
        && matches!(bytes[i + 1], b'x' | b'X' | b'b' | b'B' | b'o' | b'O')
    {
        i += 2;
        while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i < n && bytes[i] == b'n' {
            i += 1;
        }
        return i;
    }

    while i < n && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
        i += 1;
    }
    if i < n && bytes[i] == b'.' {
        i += 1;
        while i < n && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
            i += 1;
        }
    }
    if i < n && matches!(bytes[i], b'e' | b'E') {
        let mut lookahead = i + 1;
        if lookahead < n && matches!(bytes[lookahead], b'+' | b'-') {
            lookahead += 1;
        }
        // Only treat `e` as an exponent marker when digits actually follow; otherwise the
        // input is malformed and is left alone rather than swallowed.
        if lookahead < n && bytes[lookahead].is_ascii_digit() {
            i = lookahead;
            while i < n && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                i += 1;
            }
        }
    }
    if i < n && bytes[i] == b'n' {
        i += 1;
    }
    i
}

fn read_regex(src: &str, mut i: usize, n: usize) -> usize {
    let bytes = src.as_bytes();
    i += 1;
    let mut in_class = false;
    while i < n {
        let c = bytes[i];
        if c == b'\\' {
            i += 1;
            if i < n {
                i += char_len_at(src, i);
            }
            continue;
        }
        if c == b'\n' {
            return i; // illegal regex literal; bail out at the line break
        }
        match c {
            b'[' => in_class = true,
            b']' => in_class = false,
            b'/' if !in_class => {
                i += 1;
                // Flags are ASCII letters by grammar; accepting arbitrary identifier bytes
                // would let the cursor stop inside a multi-byte character, and the caller
                // slices `src[s..i]` afterwards.
                while i < n && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                return i;
            }
            _ => {}
        }
        i += 1;
    }
    n
}

fn regex_allowed(prev: Option<&Token>, after_control_paren: bool) -> bool {
    let Some(prev) = prev else {
        return true;
    };
    match prev.kind {
        TokenKind::Num | TokenKind::Str | TokenKind::Template | TokenKind::Regex => false,
        TokenKind::Ident => REGEX_PREV_KEYWORDS.contains(prev.value.as_str()),
        TokenKind::Punct => match prev.value.as_str() {
            // `if (x) /re/.test(y)` starts a statement; `(a) / b` is division. Only the
            // closing parenthesis of a control header can precede a regex literal.
            ")" => after_control_paren,
            "]" | "++" | "--" => false,
            _ => true,
        },
        TokenKind::Eof => true,
    }
}

/// Whether `word` introduces a parenthesised control header.
fn is_control_keyword(word: &str) -> bool {
    matches!(word, "if" | "for" | "while" | "with")
}

/// Lazily tokenises `src[start..end]`, always terminating with a [`TokenKind::Eof`].
pub fn tokenize(src: &str, start: usize, end: Option<usize>) -> TokenStream<'_> {
    TokenStream {
        src,
        i: start,
        n: end.unwrap_or(src.len()),
        prev: None,
        parens: Vec::new(),
        after_control_paren: false,
        done: false,
    }
}

/// Iterator produced by [`tokenize`].
pub struct TokenStream<'a> {
    src: &'a str,
    i: usize,
    n: usize,
    prev: Option<Token>,
    /// One entry per open `(`: whether it follows `if`/`for`/`while`/`with`.
    parens: Vec<bool>,
    /// Whether the previous token was the `)` closing a control header.
    after_control_paren: bool,
    done: bool,
}

impl Iterator for TokenStream<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        if self.done {
            return None;
        }
        self.i = skip_ws_and_comments(self.src, self.i, self.n);
        if self.i >= self.n {
            self.done = true;
            return Some(Token::new(TokenKind::Eof, "", self.n, self.n));
        }

        let s = self.i;
        let bytes = self.src.as_bytes();
        let c = bytes[s];
        let tok = if is_ident_start(c) {
            // The first character may be multi-byte (a CJK identifier, for example), so advance by character length;
            // otherwise the cursor stops inside a character and the next char_len_at's `src[i..]` panics.
            let mut i = s + char_len_at(self.src, s);
            while i < self.n && is_ident_body(bytes[i]) {
                i += if bytes[i] >= 0x80 {
                    char_len_at(self.src, i)
                } else {
                    1
                };
            }
            self.i = i;
            Token::new(TokenKind::Ident, &self.src[s..i], s, i)
        } else if c.is_ascii_digit()
            || (c == b'.' && s + 1 < self.n && bytes[s + 1].is_ascii_digit())
        {
            let i = read_number(self.src, s, self.n);
            self.i = i;
            Token::new(TokenKind::Num, &self.src[s..i], s, i)
        } else if c == b'\'' || c == b'"' {
            let i = read_string(self.src, s, self.n);
            self.i = i;
            Token::new(TokenKind::Str, self.src.get(s..i).unwrap_or(""), s, i)
        } else if c == b'`' {
            let i = read_template(self.src, s, self.n);
            self.i = i;
            Token::new(TokenKind::Template, self.src.get(s..i).unwrap_or(""), s, i)
        } else if c == b'/' && regex_allowed(self.prev.as_ref(), self.after_control_paren) {
            let i = read_regex(self.src, s, self.n);
            self.i = i;
            Token::new(TokenKind::Regex, self.src.get(s..i).unwrap_or(""), s, i)
        } else {
            let mut matched = None;
            for op in PUNCT.iter() {
                if !self.src[s..self.n].starts_with(op) {
                    continue;
                }
                // `?.` is optional chaining only when what follows is not a decimal digit.
                // The ECMAScript lexical grammar excludes that case precisely so that
                // `a?.5:b` keeps parsing as the ternary `a ? .5 : b`; treating it as `a?.5`
                // silently produces invalid JavaScript once the token is reproduced.
                if *op == "?."
                    && self
                        .src
                        .as_bytes()
                        .get(s + 2)
                        .is_some_and(u8::is_ascii_digit)
                {
                    continue;
                }
                matched = Some(*op);
                break;
            }
            let (value, len) = match matched {
                Some(op) => (op.to_owned(), op.len()),
                None => {
                    let len = char_len_at(self.src, s);
                    (self.src[s..s + len].to_owned(), len)
                }
            };
            let i = s + len;
            self.i = i;
            Token::new(TokenKind::Punct, value, s, i)
        };

        if tok.kind == TokenKind::Punct {
            if tok.value == "(" {
                let control = self
                    .prev
                    .as_ref()
                    .is_some_and(|p| p.kind == TokenKind::Ident && is_control_keyword(&p.value));
                self.parens.push(control);
                self.after_control_paren = false;
            } else if tok.value == ")" {
                self.after_control_paren = self.parens.pop().unwrap_or(false);
            } else {
                self.after_control_paren = false;
            }
        } else {
            self.after_control_paren = false;
        }

        self.prev = Some(tok.clone());
        Some(tok)
    }
}

const OPENERS: &[(u8, u8)] = &[(b'{', b'}'), (b'(', b')'), (b'[', b']')];

/// Given `src[open_index]` being one of `{([`, returns the byte offset of its match.
///
/// Returns `None` when the delimiter is unbalanced.
pub fn match_delimiter(src: &str, open_index: usize) -> Option<usize> {
    let open = *src.as_bytes().get(open_index)?;
    let close = OPENERS.iter().find(|(o, _)| *o == open).map(|(_, c)| *c)?;
    let mut depth = 0i32;
    for tok in tokenize(src, open_index, None) {
        if tok.kind != TokenKind::Punct {
            continue;
        }
        if tok.value.as_bytes().first() == Some(&open) && tok.value.len() == 1 {
            depth += 1;
        } else if tok.value.as_bytes().first() == Some(&close) && tok.value.len() == 1 {
            depth -= 1;
            if depth == 0 {
                return Some(tok.start);
            }
        }
    }
    None
}

/// Iterates over top-level (brace depth zero) calls to `callee`.
///
/// Yields `(callee_start, open_paren_index)`.
pub fn iter_top_level_calls<'a>(
    src: &'a str,
    callee: &'a str,
) -> impl Iterator<Item = (usize, usize)> + 'a {
    let mut depth = 0i32;
    let mut expect_callee = false;
    let mut prev: Option<Token> = None;
    let mut pending: Option<(usize, usize)> = None;

    tokenize(src, 0, None).filter_map(move |tok| {
        if tok.kind == TokenKind::Eof {
            return None;
        }
        match tok.kind {
            TokenKind::Punct => {
                let v = tok.value.as_bytes()[0];
                if v == b'(' || v == b'{' || v == b'[' {
                    if depth == 0 && v == b'(' && expect_callee {
                        pending = Some((prev.as_ref().map_or(tok.start, |p| p.start), tok.start));
                    }
                    depth += 1;
                    expect_callee = false;
                } else if v == b')' || v == b'}' || v == b']' {
                    depth -= 1;
                    expect_callee = false;
                } else {
                    expect_callee = false;
                }
            }
            TokenKind::Ident => expect_callee = tok.value == callee && depth == 0,
            _ => expect_callee = false,
        }
        prev = Some(tok);
        pending.take()
    })
}

/// Splits the argument list of a call whose `(` sits at `open_paren_index`.
///
/// Returns the byte ranges of each top-level argument.
pub fn split_top_level_args(src: &str, open_paren_index: usize) -> Vec<(usize, usize)> {
    let Some(close) = match_delimiter(src, open_paren_index) else {
        return Vec::new();
    };
    let mut args = Vec::new();
    let mut depth = 0i32;
    let mut cur = open_paren_index + 1;
    for tok in tokenize(src, open_paren_index + 1, Some(close)) {
        if tok.kind == TokenKind::Eof {
            break;
        }
        if tok.kind != TokenKind::Punct {
            continue;
        }
        let v = tok.value.as_bytes()[0];
        if v == b'(' || v == b'{' || v == b'[' {
            depth += 1;
        } else if v == b')' || v == b'}' || v == b']' {
            depth -= 1;
        } else if v == b',' && depth == 0 {
            args.push((cur, tok.start));
            cur = tok.end;
        }
    }
    if cur < close && !src.get(cur..close).unwrap_or("").trim().is_empty() {
        args.push((cur, close));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(TokenKind, String)> {
        tokenize(src, 0, None).map(|t| (t.kind, t.value)).collect()
    }

    #[test]
    fn optional_chaining_never_swallows_a_leading_dot_number() {
        // `q?.5:1` is the ternary `q ? .5 : 1`; `q?.x` really is optional chaining.
        let ternary = kinds("a=q?.5:1;");
        assert!(
            !ternary
                .iter()
                .any(|(k, v)| *k == TokenKind::Punct && v == "?."),
            "`?.` must not be produced for a leading-dot number: {ternary:?}"
        );
        let texts: Vec<&str> = ternary.iter().map(|(_, v)| v.as_str()).collect();
        assert!(
            texts.contains(&".5"),
            "the numeric literal must stay intact: {texts:?}"
        );
        assert_eq!(texts[3], "?", "{texts:?}");
        assert_eq!(texts[5], ":", "{texts:?}");

        // Real optional chaining and optional calls must still be recognised.
        let optional = kinds("b=c?.d;e=f?.(g);");
        let optional_texts: Vec<&str> = optional.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(
            optional_texts.iter().filter(|v| **v == "?.").count(),
            2,
            "{optional_texts:?}"
        );
    }

    #[test]
    fn multibyte_identifiers_are_lexed_whole() {
        // Regression: an identifier starting with non-ASCII once left the cursor inside a character (`let mut i = s + 1`),
        // making char_len_at's `src[i..]` panic with "not a char boundary".
        // The real package wx345ee4b4e565aa02 triggered it with a CJK identifier.
        for (source, expected) in [
            ("var 推送=1;", "推送"),
            ("var 推送名称=1;", "推送名称"),
            ("var a推b=1;", "a推b"),
            ("var 🚀x=1;", "🚀x"),
        ] {
            let toks = kinds(source);
            assert!(
                toks.iter()
                    .any(|(k, v)| *k == TokenKind::Ident && v == expected),
                "{source:?} should produce the identifier {expected:?}: {toks:?}"
            );
        }
    }

    #[test]
    fn recognises_regex_after_return_and_keywords() {
        let toks = kinds("return /a[/]b/gi;");
        assert_eq!(toks[1].0, TokenKind::Regex);
        assert_eq!(toks[1].1, "/a[/]b/gi");
    }

    #[test]
    fn division_is_not_a_regex() {
        let toks = kinds("var a = b / c;");
        assert!(toks.iter().any(|(k, v)| *k == TokenKind::Punct && v == "/"));
        assert!(!toks.iter().any(|(k, _)| *k == TokenKind::Regex));
    }

    #[test]
    fn a_regex_after_a_for_header_is_not_division() {
        // Minimised bundles write `for(...;++a)/^[ \t]{0,3}\|/.test(...)` with no braces.
        // Treating the slash after `)` as division splits the literal and the beautified
        // file no longer parses.
        let source = "for(a=0;a<b;++a)/^[ \\t]{0,3}\\|/.test(a);";
        let toks = kinds(source);
        let regex = toks
            .iter()
            .find(|(k, _)| *k == TokenKind::Regex)
            .expect("a regex literal should follow the control header");
        assert_eq!(regex.1, "/^[ \\t]{0,3}\\|/", "{toks:?}");
    }

    #[test]
    fn a_regex_after_a_nested_control_header_is_one_token() {
        let toks = kinds("if((a&&b))/x{2}/.test(c);");
        let regex = toks
            .iter()
            .find(|(k, _)| *k == TokenKind::Regex)
            .expect("the outer control parenthesis closes the header");
        assert_eq!(regex.1, "/x{2}/", "{toks:?}");
    }

    #[test]
    fn division_after_an_ordinary_parenthesis_stays_division() {
        let toks = kinds("x=(a+b)/c;");
        assert!(toks.iter().any(|(k, v)| *k == TokenKind::Punct && v == "/"));
        assert!(!toks.iter().any(|(k, _)| *k == TokenKind::Regex));
    }

    #[test]
    fn template_literals_span_nested_braces() {
        let toks = kinds("x = `a${ {b: 1} }c`");
        let template = toks
            .iter()
            .find(|(k, _)| *k == TokenKind::Template)
            .unwrap();
        assert_eq!(template.1, "`a${ {b: 1} }c`");
    }

    #[test]
    fn numeric_literals_stay_whole() {
        // `1e-4` must be one token: splitting it at the sign produces `1e - 4` after
        // reformatting, which is not valid JavaScript.
        let toks = kinds("a=1e-4;b=1E+3;c=1.5e-7;d=.5;e=0x1F;f=0b1010;g=1_000;h=123n;i=0o17;");
        let numbers: Vec<&str> = toks
            .iter()
            .filter(|(k, _)| *k == TokenKind::Num)
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            numbers,
            vec!["1e-4", "1E+3", "1.5e-7", ".5", "0x1F", "0b1010", "1_000", "123n", "0o17"],
            "{toks:?}"
        );
        // A malformed `1e` is left as a single token rather than eating the next operator.
        let malformed = kinds("a=1e-4");
        assert!(malformed.iter().any(|(_, v)| v == "1e-4"), "{malformed:?}");
    }

    #[test]
    fn regex_flags_never_stop_inside_a_character() {
        // A multi-byte character right after the closing `/` must not be swallowed as a flag.
        let toks = kinds("var a = /x/中文;");
        let regex = toks
            .iter()
            .find(|(k, _)| *k == TokenKind::Regex)
            .expect("a regex literal should be produced");
        assert_eq!(regex.1, "/x/");
        assert!(toks
            .iter()
            .any(|(k, v)| *k == TokenKind::Ident && v == "中文"));
    }

    #[test]
    fn comments_are_skipped() {
        let toks = kinds("a /* c */ + // x\n b");
        assert_eq!(toks.len(), 4); // a, +, b, EOF
    }

    #[test]
    fn delimiters_match_nested() {
        let src = "define('a', function(){ return {x:[1,2]}; })";
        let open = src.find('(').unwrap();
        let close = match_delimiter(src, open).unwrap();
        assert_eq!(&src[close..close + 1], ")");
    }

    #[test]
    fn top_level_calls_and_args() {
        let src = "Z([1,'a']); Z(f(1,2), 'x');";
        let calls: Vec<_> = iter_top_level_calls(src, "Z").collect();
        assert_eq!(calls.len(), 2);
        let args = split_top_level_args(src, calls[1].1);
        assert_eq!(args.len(), 2);
        assert_eq!(&src[args[1].0..args[1].1], " 'x'");
    }
}
