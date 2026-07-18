//! AST for gg-js. Identifiers stay as Strings here; the bytecode
//! compiler interns them into atoms.

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    This,
    Ident(String),
    Template(Vec<TplPart>),
    Regex { pattern: String, flags: String },
    Array(Vec<Expr>),
    Object(Vec<Prop>),
    Func(std::rc::Rc<FuncLit>),
    Arrow(std::rc::Rc<FuncLit>),
    Unary(UnOp, Box<Expr>),
    /// `await E` — only valid inside an async function; the compiler
    /// lifts it into a `.then` continuation (see compiler desugar).
    Await(Box<Expr>),
    Update { op: UpdateOp, prefix: bool, target: Box<Expr> },
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Logical(LogOp, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Assign(AssignOp, Box<Expr>, Box<Expr>),
    Member { obj: Box<Expr>, prop: MemberProp, optional: bool },
    Call { callee: Box<Expr>, args: Vec<Expr>, optional: bool },
    New { callee: Box<Expr>, args: Vec<Expr> },
    Seq(Vec<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TplPart {
    Chunk(String),
    Expr(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum MemberProp {
    Static(String),
    Computed(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub key: PropKey,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropKey {
    Ident(String),
    Str(String),
    Num(f64),
    Computed(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuncLit {
    pub name: Option<String>,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
    /// `async function` — the compiler desugars the body to a promise
    /// chain (no suspendable VM).
    pub is_async: bool,
    /// Lazy parsing: Some = `body` is empty and the real body is this
    /// token range, parsed at first call (together with lazy
    /// compilation). Only plain functions qualify — async/generator
    /// bodies and `super`-rewritten class methods parse eagerly.
    pub lazy_body: Option<LazyTokens>,
}

/// A function body captured as a token range instead of an AST.
/// `free_ids` conservatively over-approximates every identifier the
/// body references (only property names after `.`/`?.` are excluded;
/// template `${}` holes are word-scanned from their raw source) —
/// the compiler pre-resolves captures from it, where naming too much
/// is harmless and missing a real reference would mis-bind a global.
#[derive(Debug, Clone, PartialEq)]
pub struct LazyTokens {
    pub toks: std::rc::Rc<Vec<super::lexer::Token>>,
    /// token index just after the body's `{`
    pub start: usize,
    /// token index of the matching `}`
    pub end: usize,
    pub free_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Pos,
    Not,
    BitNot,
    Typeof,
    Void,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    Inc,
    Dec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Mod, Pow,
    Shl, Shr, UShr,
    Lt, Gt, LtEq, GtEq, In, InstanceOf,
    EqEq, NotEq, StrictEq, StrictNotEq,
    BitAnd, BitOr, BitXor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogOp {
    And,
    Or,
    Nullish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Plain,
    Bin(BinOp),
    Log(LogOp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclKind {
    Var,
    Let,
    Const,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Expr(Expr),
    VarDecl { kind: DeclKind, decls: Vec<(String, Option<Expr>)> },
    FuncDecl(std::rc::Rc<FuncLit>),
    Return(Option<Expr>),
    If { test: Expr, cons: Box<Stmt>, alt: Option<Box<Stmt>> },
    While { test: Expr, body: Box<Stmt> },
    DoWhile { body: Box<Stmt>, test: Expr },
    For {
        init: Option<Box<Stmt>>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: Box<Stmt>,
    },
    /// Covers both for-in (`of: false`) and for-of (`of: true`).
    ForIn {
        decl_kind: Option<DeclKind>,
        var: String,
        obj: Expr,
        body: Box<Stmt>,
        of: bool,
    },
    Block(Vec<Stmt>),
    /// `label: stmt` — names the enclosed statement for labeled
    /// break/continue.
    Labeled { label: String, body: Box<Stmt> },
    Break(Option<String>),
    Continue(Option<String>),
    Throw(Expr),
    Try {
        block: Vec<Stmt>,
        catch: Option<CatchClause>,
        finally: Option<Vec<Stmt>>,
    },
    Switch { disc: Expr, cases: Vec<SwitchCase> },
    Empty,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatchClause {
    pub param: Option<String>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwitchCase {
    /// None = `default:`
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}
