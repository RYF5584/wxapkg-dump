// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Miniature JavaScript parser driven by [`crate::js::lexer`].
//!
//! Coverage is intentionally narrow and matches what `wcc` emits: declarations,
//! conditionals, loops are *not* needed, `try`/`catch`, `return`, function forms,
//! calls, member access, assignments, ternaries, arrays and object literals.

use super::ast::{Ast, Lit, Stmt};
use super::lexer::{tokenize, Token, TokenKind};
use super::{parse_js_number, unescape_js_string};

/// Raised when the token stream cannot be interpreted.
#[derive(Debug, Clone, thiserror::Error)]
#[error("parse error at byte {offset}: expected {expected}, found {found:?}")]
pub struct ParseError {
    /// Byte offset of the offending token.
    pub offset: usize,
    /// What the parser expected.
    pub expected: String,
    /// The token that was actually found.
    pub found: String,
}

const ASSIGN_OPS: &[&str] = &[
    "=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<=", ">>=", ">>>=", "&&=", "||=", "??=",
];

/// Binary operator precedence levels, lowest first.
const BIN_LEVELS: &[&[&str]] = &[
    &["??"],
    &["||"],
    &["&&"],
    &["|"],
    &["^"],
    &["&"],
    &["==", "!=", "===", "!=="],
    &["<", ">", "<=", ">=", "instanceof", "in"],
    &["<<", ">>", ">>>"],
    &["+", "-"],
    &["*", "/", "%"],
    &["**"],
];

/// Recursive-descent parser over a token vector.
pub struct Parser {
    toks: Vec<Token>,
    i: usize,
}

/// Parses the whole `src[start..end]` into a statement list.
pub fn parse(src: &str, start: usize, end: Option<usize>) -> Result<Vec<Stmt>, ParseError> {
    Parser::new(src, start, end).parse_program(None)
}

impl Parser {
    /// Tokenises `src[start..end]` and prepares a parser.
    pub fn new(src: &str, start: usize, end: Option<usize>) -> Self {
        Self {
            toks: tokenize(src, start, end).collect(),
            i: 0,
        }
    }

    fn eof(&self) -> &Token {
        // `tokenize` always appends EOF, so this index is always valid.
        &self.toks[self.toks.len() - 1]
    }

    /// The token stream always ends with EOF, so `toks` is never empty.
    fn peek_curr(&self) -> &Token {
        self.toks.get(self.i).unwrap_or_else(|| self.eof())
    }

    fn at_punct(&self, value: &str) -> bool {
        let t = self.peek_curr();
        t.kind == TokenKind::Punct && t.value == value
    }

    fn at_ident(&self, value: &str) -> bool {
        let t = self.peek_curr();
        t.kind == TokenKind::Ident && t.value == value
    }

    fn next(&mut self) -> Token {
        let t = self.peek_curr().clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn expect(&mut self, value: &str) -> Result<Token, ParseError> {
        if self.at_punct(value) {
            return Ok(self.next());
        }
        Err(self.err(format!("`{value}`")))
    }

    fn expect_ident(&mut self, value: &str) -> Result<Token, ParseError> {
        if self.at_ident(value) {
            return Ok(self.next());
        }
        Err(self.err(format!("keyword `{value}`")))
    }

    fn eat(&mut self, value: &str) -> bool {
        if self.at_punct(value) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn eat_ident(&mut self, value: &str) -> bool {
        if self.at_ident(value) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn err(&self, expected: String) -> ParseError {
        let t = self.peek_curr();
        ParseError {
            offset: t.start,
            expected,
            found: t.value.clone(),
        }
    }

    fn skip_semi(&mut self) {
        while self.at_punct(";") {
            self.i += 1;
        }
    }

    /// Parses statements until EOF or the optional closing delimiter.
    pub fn parse_program(&mut self, stop: Option<&str>) -> Result<Vec<Stmt>, ParseError> {
        let mut stmts = Vec::new();
        loop {
            self.skip_semi();
            let t = self.peek_curr();
            if t.kind == TokenKind::Eof {
                break;
            }
            if let Some(stop) = stop {
                if t.kind == TokenKind::Punct && t.value == stop {
                    break;
                }
            }
            if let Some(stmt) = self.parse_statement()? {
                stmts.push(stmt);
            }
        }
        Ok(stmts)
    }

    fn parse_statement(&mut self) -> Result<Option<Stmt>, ParseError> {
        let t = self.peek_curr().clone();
        if t.kind == TokenKind::Punct && t.value == "{" {
            return Ok(Some(Stmt::Block(self.parse_block()?)));
        }
        if t.kind == TokenKind::Ident {
            match t.value.as_str() {
                "var" | "let" | "const" => return self.parse_var().map(Some),
                "if" => return self.parse_if().map(Some),
                "try" => return self.parse_try().map(Some),
                "return" => {
                    self.next();
                    let cur = self.peek_curr();
                    if self.at_punct(";") || self.at_punct("}") || cur.kind == TokenKind::Eof {
                        return Ok(Some(Stmt::Return(None)));
                    }
                    let expr = self.parse_expression(0)?;
                    self.skip_semi();
                    return Ok(Some(Stmt::Return(Some(expr))));
                }
                "function" => {
                    self.next();
                    let name = if self.peek_curr().kind == TokenKind::Ident {
                        Some(self.next().value)
                    } else {
                        None
                    };
                    let (params, body) = self.parse_function_rest()?;
                    return Ok(Some(Stmt::FuncDecl { name, params, body }));
                }
                "throw" | "delete" | "void" | "typeof" | "new" => {
                    let expr = self.parse_expression(0)?;
                    self.skip_semi();
                    return Ok(Some(Stmt::Expr(expr)));
                }
                _ => {}
            }
        }
        let expr = self.parse_expression(0)?;
        self.skip_semi();
        Ok(Some(Stmt::Expr(expr)))
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, ParseError> {
        self.expect("{")?;
        let stmts = self.parse_program(Some("}"))?;
        self.expect("}")?;
        Ok(stmts)
    }

    fn parse_var(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.next().value;
        let mut decls = Vec::new();
        loop {
            let name_tok = self.next();
            if name_tok.kind != TokenKind::Ident {
                return Err(ParseError {
                    offset: name_tok.start,
                    expected: "identifier".to_owned(),
                    found: name_tok.value,
                });
            }
            let init = if self.eat("=") {
                Some(self.parse_expression(0)?)
            } else {
                None
            };
            decls.push((name_tok.value, init));
            if !self.eat(",") {
                break;
            }
        }
        self.skip_semi();
        Ok(Stmt::Var { kind: kw, decls })
    }

    fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        self.expect_ident("if")?;
        self.expect("(")?;
        let test = self.parse_expression(0)?;
        self.expect(")")?;
        let cons_stmt = self.parse_statement()?.unwrap_or(Stmt::Block(Vec::new()));
        let cons = cons_stmt.as_block().to_vec();

        let mut alt = None;
        if self.eat_ident("else") {
            if self.at_ident("if") {
                alt = Some(vec![self.parse_if()?]);
            } else {
                let stmt = self.parse_statement()?.unwrap_or(Stmt::Block(Vec::new()));
                alt = Some(vec![stmt]);
            }
        }
        Ok(Stmt::If { test, cons, alt })
    }

    fn parse_try(&mut self) -> Result<Stmt, ParseError> {
        self.expect_ident("try")?;
        let body = self.parse_block()?;
        let mut param = None;
        let mut catch = Vec::new();
        if self.eat_ident("catch") {
            if self.eat("(") {
                param = Some(self.next().value);
                self.expect(")")?;
            }
            catch = self.parse_block()?;
        }
        let finally = if self.eat_ident("finally") {
            Some(self.parse_block()?)
        } else {
            None
        };
        Ok(Stmt::Try {
            body,
            param,
            catch,
            finally,
        })
    }

    fn parse_function_rest(&mut self) -> Result<(Vec<String>, Vec<Stmt>), ParseError> {
        self.expect("(")?;
        let mut params = Vec::new();
        while !self.at_punct(")") {
            let tok = self.next();
            if tok.kind == TokenKind::Ident {
                params.push(tok.value);
            } else if !(tok.kind == TokenKind::Punct && tok.value == ",") {
                break;
            }
            if self.peek_curr().kind == TokenKind::Eof {
                break;
            }
        }
        self.expect(")")?;
        let body = self.parse_block()?;
        Ok((params, body))
    }

    /// Parses an expression, honouring the precedence table above.
    pub fn parse_expression(&mut self, min_level: usize) -> Result<Ast, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            let t = self.peek_curr().clone();
            if t.kind == TokenKind::Punct && ASSIGN_OPS.contains(&t.value.as_str()) {
                if min_level > 0 {
                    break;
                }
                let op = self.next().value;
                let right = self.parse_expression(0)?;
                left = Ast::Assign(op, Box::new(left), Box::new(right));
                continue;
            }
            if t.kind == TokenKind::Punct && t.value == "?" {
                if min_level > 0 {
                    break;
                }
                self.next();
                let cons = self.parse_expression(0)?;
                self.expect(":")?;
                let alt = self.parse_expression(0)?;
                left = Ast::Cond(Box::new(left), Box::new(cons), Box::new(alt));
                continue;
            }

            let op = if t.kind == TokenKind::Punct {
                BIN_LEVELS.iter().enumerate().find_map(|(level, ops)| {
                    if ops.contains(&t.value.as_str()) {
                        (level >= min_level).then(|| (t.value.clone(), level))
                    } else {
                        None
                    }
                })
            } else if t.kind == TokenKind::Ident && matches!(t.value.as_str(), "instanceof" | "in")
            {
                BIN_LEVELS.iter().enumerate().find_map(|(level, ops)| {
                    if ops.contains(&t.value.as_str()) {
                        (level >= min_level).then(|| (t.value.clone(), level))
                    } else {
                        None
                    }
                })
            } else {
                None
            };

            let Some((op, level)) = op else { break };
            self.next();
            let right = self.parse_expression(level + 1)?;
            left = Ast::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Ast, ParseError> {
        let t = self.peek_curr().clone();
        if t.kind == TokenKind::Punct
            && matches!(t.value.as_str(), "!" | "-" | "+" | "~" | "++" | "--")
        {
            self.next();
            return Ok(Ast::Unary(t.value, Box::new(self.parse_unary()?)));
        }
        if t.kind == TokenKind::Ident && matches!(t.value.as_str(), "typeof" | "void" | "delete") {
            self.next();
            return Ok(Ast::Unary(t.value, Box::new(self.parse_unary()?)));
        }
        if t.kind == TokenKind::Ident && t.value == "new" {
            self.next();
            let callee = self.parse_unary()?;
            return Ok(Ast::Unary("new".to_owned(), Box::new(callee)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Ast, ParseError> {
        let mut expr = self.parse_primary()?;
        loop {
            let t = self.peek_curr().clone();
            if t.kind == TokenKind::Punct && t.value == "." {
                self.next();
                let prop = self.next();
                expr = Ast::Member(
                    Box::new(expr),
                    Box::new(Ast::Lit(Lit::Str(prop.value))),
                    false,
                );
            } else if t.kind == TokenKind::Punct && t.value == "[" {
                self.next();
                let idx = self.parse_expression(0)?;
                self.expect("]")?;
                expr = Ast::Member(Box::new(expr), Box::new(idx), true);
            } else if t.kind == TokenKind::Punct && t.value == "(" {
                self.next();
                let mut args = Vec::new();
                while !self.at_punct(")") {
                    args.push(self.parse_expression(0)?);
                    if !self.eat(",") {
                        break;
                    }
                }
                self.expect(")")?;
                expr = Ast::Call(Box::new(expr), args);
            } else if t.kind == TokenKind::Punct && matches!(t.value.as_str(), "++" | "--") {
                self.next();
                expr = Ast::Postfix(t.value, Box::new(expr));
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Ast, ParseError> {
        let t = self.peek_curr().clone();
        match t.kind {
            TokenKind::Num => {
                self.next();
                Ok(Ast::Lit(Lit::Num(parse_js_number(&t.value))))
            }
            TokenKind::Str => {
                self.next();
                Ok(Ast::Lit(Lit::Str(unescape_js_string(&t.value))))
            }
            TokenKind::Template => {
                self.next();
                Ok(Ast::Template(t.value))
            }
            TokenKind::Ident => match t.value.as_str() {
                "function" => {
                    self.next();
                    let name = if self.peek_curr().kind == TokenKind::Ident {
                        Some(self.next().value)
                    } else {
                        None
                    };
                    let (params, body) = self.parse_function_rest()?;
                    Ok(Ast::Function { name, params, body })
                }
                "true" | "false" => {
                    self.next();
                    Ok(Ast::Lit(Lit::Bool(t.value == "true")))
                }
                "null" => {
                    self.next();
                    Ok(Ast::Lit(Lit::Null))
                }
                _ => {
                    self.next();
                    Ok(Ast::Ident(t.value))
                }
            },
            TokenKind::Punct if t.value == "(" => {
                self.next();
                let expr = self.parse_expression(0)?;
                self.expect(")")?;
                Ok(expr)
            }
            TokenKind::Punct if t.value == "[" => {
                self.next();
                let mut elems = Vec::new();
                while !self.at_punct("]") {
                    if self.at_punct(",") {
                        self.next();
                        elems.push(None);
                        continue;
                    }
                    elems.push(Some(self.parse_expression(0)?));
                    if !self.eat(",") {
                        break;
                    }
                }
                self.expect("]")?;
                Ok(Ast::Array(elems))
            }
            TokenKind::Punct if t.value == "{" => {
                self.next();
                let mut props = Vec::new();
                while !self.at_punct("}") {
                    let k = self.next();
                    let key = match k.kind {
                        TokenKind::Str => Some(Ast::Lit(Lit::Str(unescape_js_string(&k.value)))),
                        TokenKind::Num => Some(Ast::Lit(Lit::Num(parse_js_number(&k.value)))),
                        TokenKind::Ident => Some(Ast::Lit(Lit::Str(k.value.clone()))),
                        TokenKind::Punct if k.value == "[" => {
                            let expr = self.parse_expression(0)?;
                            self.expect("]")?;
                            Some(expr)
                        }
                        _ => None,
                    };
                    let value = if self.eat(":") {
                        self.parse_expression(0)?
                    } else {
                        match &key {
                            Some(Ast::Lit(Lit::Str(name))) => Ast::Ident(name.clone()),
                            _ => Ast::Lit(Lit::Null),
                        }
                    };
                    props.push((key, value));
                    if !self.eat(",") {
                        break;
                    }
                }
                self.expect("}")?;
                Ok(Ast::Object(props))
            }
            _ => Err(ParseError {
                offset: t.start,
                expected: "expression".to_owned(),
                found: format!("{:?} {:?}", t.kind, t.value),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_var_and_call() {
        let src = "var m0 = function(e,s,r,gg){ var z = gz$gwx_1(); _v(); };";
        let stmts = parse(src, 0, None).unwrap();
        assert_eq!(stmts.len(), 1);
        let Stmt::Var { decls, .. } = &stmts[0] else {
            panic!("expected var");
        };
        assert_eq!(decls[0].0, "m0");
    }

    #[test]
    fn parses_if_else_chain() {
        let src = "if(a){b()}else if(c){d()}else{e()}";
        let stmts = parse(src, 0, None).unwrap();
        let Stmt::If { alt, .. } = &stmts[0] else {
            panic!("expected if");
        };
        let alt = alt.as_ref().unwrap();
        assert!(matches!(alt[0], Stmt::If { .. }));
    }

    #[test]
    fn parses_object_literal_with_shorthand() {
        let src = "var o = {path: './a.wxss', flag: true, x};";
        let stmts = parse(src, 0, None).unwrap();
        let Stmt::Var { decls, .. } = &stmts[0] else {
            panic!("expected var");
        };
        let Some(Ast::Object(props)) = decls[0].1.as_ref() else {
            panic!("expected object");
        };
        assert_eq!(props.len(), 3);
    }

    #[test]
    fn reports_errors_with_offsets() {
        let err = parse("var = 1", 0, None).unwrap_err();
        assert_eq!(err.offset, 4);
    }
}
