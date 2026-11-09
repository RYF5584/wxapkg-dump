// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Lightweight AST produced by the miniature parser.
//!
//! The shape mirrors the compiled artefacts closely enough that decompilation can
//! walk it directly, without a JavaScript engine.

/// A literal value.
#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// Any numeric literal.
    Num(f64),
    /// String literal, already unescaped.
    Str(String),
}

/// An expression node.
#[derive(Debug, Clone, PartialEq)]
pub enum Ast {
    /// Literal value.
    Lit(Lit),
    /// Identifier reference.
    Ident(String),
    /// Template literal, raw text including backticks.
    Template(String),
    /// Array literal; `None` entries are elisions.
    Array(Vec<Option<Ast>>),
    /// Object literal: `(key, value)` pairs where the key is `None` for computed holes.
    Object(Vec<(Option<Ast>, Ast)>),
    /// Member access; the flag is `true` for computed (`a[b]`) access.
    Member(Box<Ast>, Box<Ast>, bool),
    /// Function call.
    Call(Box<Ast>, Vec<Ast>),
    /// Prefix unary operator.
    Unary(String, Box<Ast>),
    /// Postfix `++` / `--`.
    Postfix(String, Box<Ast>),
    /// Binary operator.
    Binary(String, Box<Ast>, Box<Ast>),
    /// Assignment (including compound operators).
    Assign(String, Box<Ast>, Box<Ast>),
    /// Conditional expression.
    Cond(Box<Ast>, Box<Ast>, Box<Ast>),
    /// Function expression.
    Function {
        /// Optional name.
        name: Option<String>,
        /// Parameter names.
        params: Vec<String>,
        /// Body statements.
        body: Vec<Stmt>,
    },
}

/// A statement node.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `var` / `let` / `const` declaration.
    Var {
        /// Declaration keyword.
        kind: String,
        /// `(name, initialiser)` pairs.
        decls: Vec<(String, Option<Ast>)>,
    },
    /// Expression statement.
    Expr(Ast),
    /// Block statement.
    Block(Vec<Stmt>),
    /// `if` / `else` chain.
    If {
        /// Condition.
        test: Ast,
        /// Consequent statements.
        cons: Vec<Stmt>,
        /// Alternate statements, when present.
        alt: Option<Vec<Stmt>>,
    },
    /// `try` / `catch` / `finally`.
    Try {
        /// Protected block.
        body: Vec<Stmt>,
        /// Optional catch binding name.
        param: Option<String>,
        /// Catch block.
        catch: Vec<Stmt>,
        /// Optional finaliser block.
        finally: Option<Vec<Stmt>>,
    },
    /// `return` statement.
    Return(Option<Ast>),
    /// `function` declaration.
    FuncDecl {
        /// Function name.
        name: Option<String>,
        /// Parameter names.
        params: Vec<String>,
        /// Body statements.
        body: Vec<Stmt>,
    },
}

impl Ast {
    /// Returns the identifier name when this node is an [`Ast::Ident`].
    pub fn as_ident(&self) -> Option<&str> {
        match self {
            Self::Ident(name) => Some(name),
            _ => None,
        }
    }

    /// Flattens this node to a literal when possible.
    pub fn as_lit(&self) -> Option<&Lit> {
        match self {
            Self::Lit(lit) => Some(lit),
            _ => None,
        }
    }

    /// Returns the element list when this node is an [`Ast::Array`].
    pub fn as_array(&self) -> Option<&[Option<Ast>]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Returns the parameter list and body when this node is a function.
    pub fn as_function(&self) -> Option<(&[String], &[Stmt])> {
        match self {
            Self::Function { params, body, .. } => Some((params, body)),
            _ => None,
        }
    }
}

impl Stmt {
    /// Returns the statements of a block, or a slice of the single statement itself.
    pub fn as_block(&self) -> &[Stmt] {
        match self {
            Self::Block(stmts) => stmts,
            other => std::slice::from_ref(other),
        }
    }
}
