//! Recursive-descent parser with precedence climbing for expressions.
//!
//! Handles the JS grammar quirks that matter for real pages: ASI
//! (automatic semicolon insertion) from the lexer's newline flags,
//! arrow-function lookahead, the `in`-operator / for-in ambiguity,
//! optional chaining, and template holes (re-lexed recursively).

use std::rc::Rc;
use super::ast::*;
use super::lexer::{tokenize, LexError, Token, Tok, TplElem, P};

pub struct ParseError {
    pub msg: String,
    pub line: u32,
}

impl std::fmt::Debug for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> ParseError {
        ParseError { msg: e.msg, line: e.line }
    }
}

pub fn parse_program(src: &str) -> Result<Vec<Stmt>, ParseError> {
    let mut p = Parser::new(Rc::new(tokenize(src)?));
    let mut out = Vec::new();
    while !p.at_eof() {
        out.push(p.stmt()?);
    }
    Ok(out)
}

/// A numeric literal used as a property key stringifies like `String(n)`
/// (`2` -> "2", `0.5` -> "0.5"), not with a trailing `.0`.
fn num_key(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Debug/name hint for a `FuncLit` built from a class member — only a
/// static key carries a usable name; a computed key is anonymous.
fn prop_name_hint(p: &MemberProp) -> Option<String> {
    match p {
        MemberProp::Static(s) => Some(s.clone()),
        MemberProp::Computed(_) => None,
    }
}

/// Compare two member keys for accessor get/set pairing. Computed keys
/// never pair (each becomes its own defineProperty).
fn prop_key_eq(a: &MemberProp, b: &MemberProp) -> bool {
    matches!((a, b),
        (MemberProp::Static(x), MemberProp::Static(y)) if x == y)
}

/// The key expression for `Object.defineProperty(proto, <key>, ...)`.
fn prop_to_key_expr(p: MemberProp) -> Expr {
    match p {
        MemberProp::Static(s) => Expr::Str(s),
        MemberProp::Computed(e) => *e,
    }
}

/// Parse a lazily-captured function body (first call). The token
/// stream is the original file's; positions stay absolute so error
/// lines match the source.
pub fn parse_lazy_body(
    lz: &LazyTokens,
) -> Result<Vec<Stmt>, ParseError> {
    let mut p = Parser::new(lz.toks.clone());
    p.pos = lz.start;
    let mut out = Vec::new();
    while p.pos < lz.end {
        out.push(p.stmt()?);
    }
    Ok(out)
}

struct Parser {
    toks: Rc<Vec<Token>>,
    pos: usize,
    /// Disables the `in` binary operator (inside a for-loop head).
    no_in: bool,
    /// Inside an `async` function body — enables `await` as an operator.
    in_async: bool,
    /// Counter for compiler-generated names (destructuring temps).
    tmp_n: usize,
    /// Number of binding decls the last top-level pattern emitted (so
    /// var_decl_list can splice the source temp in front of them).
    last_pattern_len: usize,
    /// Inside a `class ... extends` body: the temp holding the parent
    /// constructor, so `super` can be rewritten against it.
    class_super: Option<String>,
    /// Inside a `function*` body — `yield` parses as an expression
    /// (a marker call the generator transform consumes).
    in_generator: bool,
}

enum OpKind {
    Bin(BinOp),
    Log(LogOp),
}

impl Parser {
    fn new(toks: Rc<Vec<Token>>) -> Parser {
        Parser {
            toks, pos: 0, no_in: false, in_async: false, tmp_n: 0,
            last_pattern_len: 0, class_super: None,
            in_generator: false,
        }
    }

    fn fresh_tmp(&mut self, prefix: &str) -> String {
        self.tmp_n += 1;
        format!("__{}{}", prefix, self.tmp_n)
    }

    /// Parse a class into an expression. Plain classes (no extends,
    /// statics, or accessors) keep the legacy desugar: a constructor
    /// that attaches methods as own properties. Classes with extends/
    /// static/get/set desugar to an IIFE that builds the constructor,
    /// wires `C.prototype = Object.create(parent.prototype)`, and
    /// attaches prototype/static members; `super` is rewritten against
    /// the captured parent (see `class_super`).
    fn class_lit(
        &mut self,
        name: Option<String>,
    ) -> Result<Expr, ParseError> {
        let parent = if self.eat_ident("extends") {
            Some(self.assign_expr()?)
        } else {
            None
        };
        let sup = if parent.is_some() {
            Some(self.fresh_tmp("sup"))
        } else {
            None
        };
        let saved_super = self.class_super.take();
        self.class_super = sup.clone();
        let result = self.class_body(name.clone(), &sup);
        self.class_super = saved_super;
        let (ctor, methods, accessors, statics) = result?;
        // Every class desugars to the IIFE form with methods on the
        // prototype. (Instance-attached methods break inheritance: a
        // parent ctor run via super() re-attaches ITS methods onto the
        // child instance as own props, shadowing child overrides.)
        let cname = name.unwrap_or_else(|| "__class".to_string());
        let cident = || Expr::Ident(cname.clone());
        let cproto = || Expr::Member {
            obj: Box::new(Expr::Ident(cname.clone())),
            prop: MemberProp::Static("prototype".to_string()),
            optional: false,
        };
        let mut body: Vec<Stmt> = Vec::new();
        body.push(Stmt::VarDecl {
            kind: DeclKind::Var,
            decls: vec![(cname.clone(), Some(Expr::Func(Rc::new(ctor))))],
        });
        if let Some(supn) = &sup {
            // C.prototype = Object.create(sup.prototype)
            body.push(Stmt::Expr(Expr::Assign(
                AssignOp::Plain,
                Box::new(cproto()),
                Box::new(Expr::Call {
                    callee: Box::new(Expr::Member {
                        obj: Box::new(Expr::Ident("Object".to_string())),
                        prop: MemberProp::Static("create".to_string()),
                        optional: false,
                    }),
                    args: vec![Expr::Member {
                        obj: Box::new(Expr::Ident(supn.clone())),
                        prop: MemberProp::Static("prototype".to_string()),
                        optional: false,
                    }],
                    optional: false,
                }),
            )));
            body.push(Stmt::Expr(Expr::Assign(
                AssignOp::Plain,
                Box::new(Expr::Member {
                    obj: Box::new(cproto()),
                    prop: MemberProp::Static("constructor".to_string()),
                    optional: false,
                }),
                Box::new(cident()),
            )));
        }
        for (key, f) in methods {
            body.push(Stmt::Expr(Expr::Assign(
                AssignOp::Plain,
                Box::new(Expr::Member {
                    obj: Box::new(cproto()),
                    prop: key,
                    optional: false,
                }),
                Box::new(Expr::Func(Rc::new(f))),
            )));
        }
        for (aname, getter, setter) in accessors {
            // Object.defineProperty(C.prototype, name, {get, set})
            let mut props = vec![Prop {
                key: PropKey::Ident("configurable".to_string()),
                value: Expr::Bool(true),
            }];
            if let Some(g) = getter {
                props.push(Prop {
                    key: PropKey::Ident("get".to_string()),
                    value: Expr::Func(Rc::new(g)),
                });
            }
            if let Some(s) = setter {
                props.push(Prop {
                    key: PropKey::Ident("set".to_string()),
                    value: Expr::Func(Rc::new(s)),
                });
            }
            body.push(Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Member {
                    obj: Box::new(Expr::Ident("Object".to_string())),
                    prop: MemberProp::Static("defineProperty".to_string()),
                    optional: false,
                }),
                args: vec![
                    cproto(),
                    prop_to_key_expr(aname),
                    Expr::Object(props),
                ],
                optional: false,
            }));
        }
        for (skey, sval) in statics {
            body.push(Stmt::Expr(Expr::Assign(
                AssignOp::Plain,
                Box::new(Expr::Member {
                    obj: Box::new(cident()),
                    prop: skey,
                    optional: false,
                }),
                Box::new(sval),
            )));
        }
        body.push(Stmt::Return(Some(cident())));
        let (params, args) = match (sup, parent) {
            (Some(s), Some(p)) => (vec![s], vec![p]),
            _ => (Vec::new(), Vec::new()),
        };
        Ok(Expr::Call {
            callee: Box::new(Expr::Func(Rc::new(FuncLit {
                name: None,
                params,
                body,
                is_async: false,
                lazy_body: None,
            }))),
            args,
            optional: false,
        })
    }

    /// Parse a class member name into a `MemberProp`: a plain identifier
    /// (incl. `#private`), a string or numeric literal key, or a computed
    /// `[expr]` key evaluated at class-definition time.
    fn member_key(&mut self) -> Result<MemberProp, ParseError> {
        if self.eat_punct(P::LBracket) {
            let e = self.assign_expr()?;
            self.expect_punct(P::RBracket)?;
            return Ok(MemberProp::Computed(Box::new(e)));
        }
        match self.bump() {
            Tok::Ident(n) => Ok(MemberProp::Static(n)),
            Tok::Str(s) => Ok(MemberProp::Static(s)),
            Tok::Num(n) => Ok(MemberProp::Static(num_key(n))),
            t => Err(self.err(format!("bad class member name: {t:?}"))),
        }
    }

    /// Parse `{ ...members... }` of a class. Returns (ctor, methods,
    /// accessors (name, get, set), statics). Field declarations become
    /// `this.f = v` statements prepended to the ctor body.
    #[allow(clippy::type_complexity)]
    fn class_body(
        &mut self,
        name: Option<String>,
        sup: &Option<String>,
    ) -> Result<
        (
            FuncLit,
            Vec<(MemberProp, FuncLit)>,
            Vec<(MemberProp, Option<FuncLit>, Option<FuncLit>)>,
            Vec<(MemberProp, Expr)>,
        ),
        ParseError,
    > {
        self.expect_punct(P::LBrace)?;
        let mut ctor: Option<(Vec<String>, Vec<Stmt>)> = None;
        let mut methods: Vec<(MemberProp, FuncLit)> = Vec::new();
        let mut accessors: Vec<(MemberProp, Option<FuncLit>, Option<FuncLit>)> =
            Vec::new();
        let mut statics: Vec<(MemberProp, Expr)> = Vec::new();
        let mut field_stmts: Vec<Stmt> = Vec::new();
        while !self.eat_punct(P::RBrace) {
            if self.eat_punct(P::Semi) {
                continue;
            }
            let is_static = matches!(self.kind(), Tok::Ident(k) if k == "static")
                && !matches!(self.kind_at(1), Some(Tok::Punct(P::LParen)));
            if is_static {
                self.pos += 1;
            }
            // async method: `async m() {}` / `async *m() {}`. Not a member
            // literally named `async` (followed by `(`, `=`, `;`, `}`), and
            // not `async` split from its name by a newline (ASI makes that a
            // field `async` plus a separate method).
            let is_async = matches!(self.kind(), Tok::Ident(k) if k == "async")
                && !matches!(
                    self.kind_at(1),
                    Some(Tok::Punct(P::LParen)) | Some(Tok::Punct(P::Assign))
                        | Some(Tok::Punct(P::Semi)) | Some(Tok::Punct(P::RBrace))
                )
                && !self.nl_before_at(1);
            if is_async {
                self.pos += 1;
            }
            // generator method: `*gen() {...}` (name may be computed)
            if self.at_punct(P::Star) {
                self.pos += 1;
                let key = self.member_key()?;
                let f = self.func_lit_g(prop_name_hint(&key), is_async, true)?;
                if is_static {
                    statics.push((key, Expr::Func(Rc::new(f))));
                } else {
                    methods.push((key, f));
                }
                continue;
            }
            // get/set accessor (unless it's a method literally named
            // get/set, i.e. followed by `(`)
            let acc = match self.kind() {
                Tok::Ident(k) if (k == "get" || k == "set")
                    && !matches!(
                        self.kind_at(1),
                        Some(Tok::Punct(P::LParen))
                    ) =>
                {
                    let is_get = k == "get";
                    self.pos += 1;
                    Some(is_get)
                }
                _ => None,
            };
            let key = self.member_key()?;
            // field: `name = expr;` or bare `name;`
            if acc.is_none() && !self.at_punct(P::LParen) {
                let value = if self.eat_punct(P::Assign) {
                    self.assign_expr()?
                } else {
                    Expr::Ident("undefined".to_string())
                };
                self.eat_punct(P::Semi);
                if is_static {
                    statics.push((key, value));
                } else {
                    field_stmts.push(Stmt::Expr(Expr::Assign(
                        AssignOp::Plain,
                        Box::new(Expr::Member {
                            obj: Box::new(Expr::This),
                            prop: key,
                            optional: false,
                        }),
                        Box::new(value),
                    )));
                }
                continue;
            }
            let (params, prologue) = self.arrow_params()?;
            let saved = self.in_async;
            self.in_async = is_async;
            let body = self.block();
            self.in_async = saved;
            let mut body = body?;
            if !prologue.is_empty() {
                let mut full = prologue;
                full.append(&mut body);
                body = full;
            }
            let f = FuncLit {
                name: prop_name_hint(&key),
                params,
                body,
                is_async,
                lazy_body: None,
            };
            if let Some(is_get) = acc {
                if is_static {
                    // static accessor: approximate as a plain static
                    statics.push((key, Expr::Func(Rc::new(f))));
                } else if let Some(slot) = accessors.iter_mut().find(
                    |(n, _, _)| prop_key_eq(n, &key),
                ) {
                    if is_get {
                        slot.1 = Some(f);
                    } else {
                        slot.2 = Some(f);
                    }
                } else if is_get {
                    accessors.push((key, Some(f), None));
                } else {
                    accessors.push((key, None, Some(f)));
                }
            } else if is_static {
                statics.push((key, Expr::Func(Rc::new(f))));
            } else if matches!(&key, MemberProp::Static(n) if n == "constructor")
            {
                ctor = Some((f.params, f.body));
            } else {
                methods.push((key, f));
            }
        }
        let (params, mut cbody) = ctor.unwrap_or_else(|| {
            // default ctor of a derived class forwards to the parent
            let body = if let Some(supn) = sup {
                vec![Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Member {
                        obj: Box::new(Expr::Ident(supn.clone())),
                        prop: MemberProp::Static("apply".to_string()),
                        optional: false,
                    }),
                    args: vec![
                        Expr::This,
                        Expr::Ident("arguments".to_string()),
                    ],
                    optional: false,
                })]
            } else {
                Vec::new()
            };
            (Vec::new(), body)
        });
        if !field_stmts.is_empty() {
            field_stmts.append(&mut cbody);
            cbody = field_stmts;
        }
        Ok((
            FuncLit { name, params, body: cbody, is_async: false,
                      lazy_body: None },
            methods,
            accessors,
            statics,
        ))
    }

    fn kind(&self) -> &Tok {
        &self.toks[self.pos].kind
    }

    fn kind_at(&self, off: usize) -> Option<&Tok> {
        self.toks.get(self.pos + off).map(|t| &t.kind)
    }

    fn nl_before(&self) -> bool {
        self.toks[self.pos].nl_before
    }

    fn nl_before_at(&self, off: usize) -> bool {
        self.toks.get(self.pos + off).map(|t| t.nl_before).unwrap_or(false)
    }

    fn line(&self) -> u32 {
        self.toks[self.pos].line
    }

    fn at_eof(&self) -> bool {
        matches!(self.kind(), Tok::Eof)
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].kind.clone();
        if !matches!(t, Tok::Eof) {
            self.pos += 1;
        }
        t
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        ParseError { msg: msg.into(), line: self.line() }
    }

    fn at_punct(&self, p: P) -> bool {
        matches!(self.kind(), Tok::Punct(q) if *q == p)
    }

    fn at_punct_at(&self, off: usize, p: P) -> bool {
        matches!(self.kind_at(off), Some(Tok::Punct(q)) if *q == p)
    }

    /// `break`/`continue` label: a same-line identifier, or None.
    fn optional_label(&mut self) -> Option<String> {
        if self.nl_before() {
            return None;
        }
        if let Tok::Ident(name) = self.kind() {
            if !is_keyword(name) {
                let name = name.clone();
                self.pos += 1;
                return Some(name);
            }
        }
        None
    }

    fn eat_punct(&mut self, p: P) -> bool {
        if self.at_punct(p) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, p: P) -> Result<(), ParseError> {
        if self.eat_punct(p) {
            Ok(())
        } else {
            Err(self.err(format!("expected {p:?}, found {:?}", self.kind())))
        }
    }

    fn at_ident(&self, s: &str) -> bool {
        matches!(self.kind(), Tok::Ident(n) if n == s)
    }

    fn eat_ident(&mut self, s: &str) -> bool {
        if self.at_ident(s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.kind() {
            Tok::Ident(n) => {
                let n = n.clone();
                self.pos += 1;
                Ok(n)
            }
            t => Err(self.err(format!("expected identifier, found {t:?}"))),
        }
    }

    /// ASI: a statement may end with `;`, `}`, EOF, or a line break.
    fn consume_semi(&mut self) -> Result<(), ParseError> {
        if self.eat_punct(P::Semi) {
            return Ok(());
        }
        if matches!(self.kind(), Tok::Eof | Tok::Punct(P::RBrace))
            || self.nl_before()
        {
            return Ok(());
        }
        Err(self.err(format!("expected ; before {:?}", self.kind())))
    }

    // ---- statements ----

    fn stmt(&mut self) -> Result<Stmt, ParseError> {
        // labeled statement: `ident:` not a keyword, followed by ':'
        if let Tok::Ident(name) = self.kind() {
            if !is_keyword(name) && self.at_punct_at(1, P::Colon) {
                let label = name.clone();
                self.pos += 2; // ident ':'
                let body = Box::new(self.stmt()?);
                return Ok(Stmt::Labeled { label, body });
            }
        }
        match self.kind() {
            Tok::Punct(P::LBrace) => {
                self.pos += 1;
                Ok(Stmt::Block(self.stmts_until_rbrace()?))
            }
            Tok::Punct(P::Semi) => {
                self.pos += 1;
                Ok(Stmt::Empty)
            }
            Tok::Ident(kw) => match kw.as_str() {
                "var" | "let" | "const" => self.var_decl_stmt(),
                "function" => {
                    self.pos += 1;
                    let is_gen = self.eat_punct(P::Star);
                    let name = self.expect_ident()?;
                    let f =
                        self.func_lit_g(Some(name), false, is_gen)?;
                    Ok(Stmt::FuncDecl(Rc::new(f)))
                }
                "async" if matches!(self.kind_at(1), Some(Tok::Ident(k))
                    if k == "function") => {
                    self.pos += 2; // async function
                    let name = self.expect_ident()?;
                    let f = self.func_lit(Some(name), true)?;
                    Ok(Stmt::FuncDecl(Rc::new(f)))
                }
                "class" => {
                    self.pos += 1;
                    let name = self.expect_ident()?;
                    let e = self.class_lit(Some(name.clone()))?;
                    Ok(Stmt::VarDecl {
                        kind: DeclKind::Var,
                        decls: vec![(name, Some(e))],
                    })
                }
                "return" => {
                    self.pos += 1;
                    let arg = if matches!(
                        self.kind(),
                        Tok::Eof | Tok::Punct(P::RBrace) | Tok::Punct(P::Semi)
                    ) || self.nl_before()
                    {
                        None
                    } else {
                        Some(self.expr()?)
                    };
                    self.consume_semi()?;
                    Ok(Stmt::Return(arg))
                }
                "if" => self.if_stmt(),
                "while" => {
                    self.pos += 1;
                    self.expect_punct(P::LParen)?;
                    let test = self.expr()?;
                    self.expect_punct(P::RParen)?;
                    let body = Box::new(self.stmt()?);
                    Ok(Stmt::While { test, body })
                }
                "with" => {
                    self.pos += 1;
                    self.expect_punct(P::LParen)?;
                    let obj = self.expr()?;
                    self.expect_punct(P::RParen)?;
                    let body = Box::new(self.stmt()?);
                    Ok(Stmt::With { obj, body })
                }
                "do" => {
                    self.pos += 1;
                    let body = Box::new(self.stmt()?);
                    if !self.eat_ident("while") {
                        return Err(self.err("expected while after do body"));
                    }
                    self.expect_punct(P::LParen)?;
                    let test = self.expr()?;
                    self.expect_punct(P::RParen)?;
                    let _ = self.eat_punct(P::Semi);
                    Ok(Stmt::DoWhile { body, test })
                }
                "for" => self.for_stmt(),
                "break" => {
                    self.pos += 1;
                    let label = self.optional_label();
                    self.consume_semi()?;
                    Ok(Stmt::Break(label))
                }
                "continue" => {
                    self.pos += 1;
                    let label = self.optional_label();
                    self.consume_semi()?;
                    Ok(Stmt::Continue(label))
                }
                "throw" => {
                    self.pos += 1;
                    if self.nl_before() {
                        return Err(self.err("newline after throw"));
                    }
                    let e = self.expr()?;
                    self.consume_semi()?;
                    Ok(Stmt::Throw(e))
                }
                "try" => self.try_stmt(),
                "switch" => self.switch_stmt(),
                _ => self.expr_stmt(),
            },
            _ => self.expr_stmt(),
        }
    }

    fn expr_stmt(&mut self) -> Result<Stmt, ParseError> {
        let e = self.expr()?;
        self.consume_semi()?;
        Ok(Stmt::Expr(e))
    }

    fn stmts_until_rbrace(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut out = Vec::new();
        while !self.eat_punct(P::RBrace) {
            if self.at_eof() {
                return Err(self.err("unexpected end of input, expected }"));
            }
            out.push(self.stmt()?);
        }
        Ok(out)
    }

    fn block(&mut self) -> Result<Vec<Stmt>, ParseError> {
        self.expect_punct(P::LBrace)?;
        self.stmts_until_rbrace()
    }

    fn decl_kind(kw: &str) -> DeclKind {
        match kw {
            "var" => DeclKind::Var,
            "let" => DeclKind::Let,
            _ => DeclKind::Const,
        }
    }

    fn var_decl_stmt(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.expect_ident()?;
        let kind = Parser::decl_kind(&kw);
        let decls = self.var_decl_list()?;
        self.consume_semi()?;
        Ok(Stmt::VarDecl { kind, decls })
    }

    fn var_decl_list(
        &mut self,
    ) -> Result<Vec<(String, Option<Expr>)>, ParseError> {
        let mut decls = Vec::new();
        loop {
            if self.at_punct(P::LBrace) || self.at_punct(P::LBracket) {
                // destructuring: `{a, b: c} = e` / `[x, , ...r] = e`
                let tmp = self.fresh_tmp("d");
                self.pattern_binds(&tmp, &mut decls, true)?;
                self.expect_punct(P::Assign)?;
                let src = self.assign_expr()?;
                // the source temp is prepended so bindings can read it
                decls.insert(
                    decls.len() - self.last_pattern_len,
                    (tmp, Some(src)),
                );
            } else {
                let name = self.expect_ident()?;
                let init = if self.eat_punct(P::Assign) {
                    Some(self.assign_expr()?)
                } else {
                    None
                };
                decls.push((name, init));
            }
            if !self.eat_punct(P::Comma) {
                return Ok(decls);
            }
        }
    }

    /// Emit `(binding, access-from-tmp)` decls for a binding pattern.
    /// `top` toggles bookkeeping so var_decl_list can splice the source
    /// temp in front of this pattern's bindings.
    fn pattern_binds(
        &mut self,
        tmp: &str,
        out: &mut Vec<(String, Option<Expr>)>,
        top: bool,
    ) -> Result<(), ParseError> {
        let before = out.len();
        if self.at_punct(P::LBrace) {
            self.obj_pattern(tmp, out)?;
        } else {
            self.arr_pattern(tmp, out)?;
        }
        if top {
            self.last_pattern_len = out.len() - before;
        }
        Ok(())
    }

    fn obj_pattern(
        &mut self,
        tmp: &str,
        out: &mut Vec<(String, Option<Expr>)>,
    ) -> Result<(), ParseError> {
        self.expect_punct(P::LBrace)?;
        let mut seen: Vec<String> = Vec::new();
        while !self.at_punct(P::RBrace) {
            if self.eat_punct(P::DotDotDot) {
                // rest: copy the source, then drop already-bound keys
                let name = self.expect_ident()?;
                out.push((name.clone(), Some(Expr::Call {
                    callee: Box::new(Expr::Member {
                        obj: Box::new(Expr::Ident("Object".to_string())),
                        prop: MemberProp::Static("assign".to_string()),
                        optional: false,
                    }),
                    args: vec![
                        Expr::Object(Vec::new()),
                        Expr::Ident(tmp.to_string()),
                    ],
                    optional: false,
                })));
                for k in &seen {
                    out.push((self.fresh_tmp("dl"), Some(Expr::Unary(
                        UnOp::Delete,
                        Box::new(Expr::Member {
                            obj: Box::new(Expr::Ident(name.clone())),
                            prop: MemberProp::Static(k.clone()),
                            optional: false,
                        }),
                    ))));
                }
                break;
            }
            let key = self.expect_ident()?;
            seen.push(key.clone());
            let member = Expr::Member {
                obj: Box::new(Expr::Ident(tmp.to_string())),
                prop: MemberProp::Static(key.clone()),
                optional: false,
            };
            if self.eat_punct(P::Colon) {
                // { key: <nested pattern or ident> }
                if self.at_punct(P::LBrace) || self.at_punct(P::LBracket) {
                    let inner = self.fresh_tmp("d");
                    out.push((inner.clone(), Some(member)));
                    self.pattern_binds(&inner, out, false)?;
                } else {
                    let bind = self.expect_ident()?;
                    let init = self.maybe_default(member)?;
                    out.push((bind, Some(init)));
                }
            } else {
                let init = self.maybe_default(member)?;
                out.push((key, Some(init)));
            }
            if !self.eat_punct(P::Comma) {
                break;
            }
        }
        self.expect_punct(P::RBrace)?;
        Ok(())
    }

    fn arr_pattern(
        &mut self,
        tmp: &str,
        out: &mut Vec<(String, Option<Expr>)>,
    ) -> Result<(), ParseError> {
        self.expect_punct(P::LBracket)?;
        let mut i = 0usize;
        while !self.at_punct(P::RBracket) {
            if self.eat_punct(P::Comma) {
                i += 1; // elision `[, x]`
                continue;
            }
            let index_expr = |n: usize| Expr::Member {
                obj: Box::new(Expr::Ident(tmp.to_string())),
                prop: MemberProp::Computed(Box::new(Expr::Num(n as f64))),
                optional: false,
            };
            if self.eat_punct(P::DotDotDot) {
                let name = self.expect_ident()?;
                out.push((name, Some(Expr::Call {
                    callee: Box::new(Expr::Member {
                        obj: Box::new(Expr::Ident(tmp.to_string())),
                        prop: MemberProp::Static("slice".to_string()),
                        optional: false,
                    }),
                    args: vec![Expr::Num(i as f64)],
                    optional: false,
                })));
                break;
            }
            if self.at_punct(P::LBrace) || self.at_punct(P::LBracket) {
                let inner = self.fresh_tmp("d");
                out.push((inner.clone(), Some(index_expr(i))));
                self.pattern_binds(&inner, out, false)?;
            } else {
                let name = self.expect_ident()?;
                let init = self.maybe_default(index_expr(i))?;
                out.push((name, Some(init)));
            }
            i += 1;
            if !self.eat_punct(P::Comma) {
                break;
            }
        }
        self.expect_punct(P::RBracket)?;
        Ok(())
    }

    /// After an access expr, parse an optional `= default` and wrap as
    /// `access !== undefined ? access : default` (access is a temp
    /// member read, so evaluating it twice is side-effect-free).
    fn maybe_default(&mut self, access: Expr) -> Result<Expr, ParseError> {
        if self.eat_punct(P::Assign) {
            let def = self.assign_expr()?;
            Ok(Expr::Cond(
                Box::new(Expr::Binary(
                    BinOp::StrictNotEq,
                    Box::new(access.clone()),
                    Box::new(Expr::Ident("undefined".to_string())),
                )),
                Box::new(access),
                Box::new(def),
            ))
        } else {
            Ok(access)
        }
    }

    fn if_stmt(&mut self) -> Result<Stmt, ParseError> {
        self.pos += 1;
        self.expect_punct(P::LParen)?;
        let test = self.expr()?;
        self.expect_punct(P::RParen)?;
        let cons = Box::new(self.stmt()?);
        let alt = if self.eat_ident("else") {
            Some(Box::new(self.stmt()?))
        } else {
            None
        };
        Ok(Stmt::If { test, cons, alt })
    }

    fn for_stmt(&mut self) -> Result<Stmt, ParseError> {
        self.pos += 1;
        // `for await (const x of asyncIterable)`: accept the syntax,
        // lower as a plain for-of. Without async iterators each value
        // arrives un-awaited — the common polyfill/SDK loops still make
        // progress, and a parse failure would drop the whole module.
        if matches!(self.kind(), Tok::Ident(k) if k == "await") {
            self.pos += 1;
        }
        self.expect_punct(P::LParen)?;

        // for (var x in/of obj)
        if matches!(self.kind(), Tok::Ident(k)
            if matches!(k.as_str(), "var" | "let" | "const"))
        {
            let kw = self.expect_ident()?;
            let kind = Parser::decl_kind(&kw);
            // for (const [a, b] of pairs) / for (const {k} of items):
            // bind a temp, unpack at the top of the body
            if self.at_punct(P::LBrace) || self.at_punct(P::LBracket) {
                let tmp = self.fresh_tmp("fi");
                let mut binds: Vec<(String, Option<Expr>)> = Vec::new();
                self.pattern_binds(&tmp, &mut binds, false)?;
                let inof = self.expect_ident()?;
                if inof != "in" && inof != "of" {
                    return Err(self.err(format!(
                        "expected in/of after for-pattern, found {inof}"
                    )));
                }
                let of = inof == "of";
                let obj = self.expr()?;
                self.expect_punct(P::RParen)?;
                let body = self.stmt()?;
                let full = vec![
                    Stmt::VarDecl { kind, decls: binds },
                    body,
                ];
                return Ok(Stmt::ForIn {
                    decl_kind: Some(kind),
                    var: tmp,
                    obj,
                    body: Box::new(Stmt::Block(full)),
                    of,
                });
            }
            if let (Some(Tok::Ident(_)), Some(Tok::Ident(inof))) =
                (self.kind_at(0), self.kind_at(1))
            {
                if inof == "in" || inof == "of" {
                    let of = inof == "of";
                    let var = self.expect_ident()?;
                    self.pos += 1;
                    let obj = self.expr()?;
                    self.expect_punct(P::RParen)?;
                    let body = Box::new(self.stmt()?);
                    return Ok(Stmt::ForIn {
                        decl_kind: Some(kind), var, obj, body, of,
                    });
                }
            }
            self.no_in = true;
            let decls = self.var_decl_list();
            self.no_in = false;
            let init = Some(Box::new(Stmt::VarDecl { kind, decls: decls? }));
            self.expect_punct(P::Semi)?;
            return self.for_tail(init);
        }

        // for (;;) — empty init
        if self.eat_punct(P::Semi) {
            return self.for_tail(None);
        }

        // for (expr ...) — either classic init or `x in/of obj`
        self.no_in = true;
        let first = self.expr();
        self.no_in = false;
        let first = first?;
        if let Tok::Ident(inof) = self.kind() {
            if inof == "in" || inof == "of" {
                let of = inof == "of";
                let Expr::Ident(var) = first else {
                    return Err(
                        self.err("unsupported for-in target (need a name)")
                    );
                };
                self.pos += 1;
                let obj = self.expr()?;
                self.expect_punct(P::RParen)?;
                let body = Box::new(self.stmt()?);
                return Ok(Stmt::ForIn {
                    decl_kind: None, var, obj, body, of,
                });
            }
        }
        self.expect_punct(P::Semi)?;
        self.for_tail(Some(Box::new(Stmt::Expr(first))))
    }

    fn for_tail(
        &mut self,
        init: Option<Box<Stmt>>,
    ) -> Result<Stmt, ParseError> {
        let test = if self.at_punct(P::Semi) {
            None
        } else {
            Some(self.expr()?)
        };
        self.expect_punct(P::Semi)?;
        let update = if self.at_punct(P::RParen) {
            None
        } else {
            Some(self.expr()?)
        };
        self.expect_punct(P::RParen)?;
        let body = Box::new(self.stmt()?);
        Ok(Stmt::For { init, test, update, body })
    }

    fn try_stmt(&mut self) -> Result<Stmt, ParseError> {
        self.pos += 1;
        let block = self.block()?;
        let catch = if self.eat_ident("catch") {
            let param = if self.eat_punct(P::LParen) {
                let p = self.expect_ident()?;
                self.expect_punct(P::RParen)?;
                Some(p)
            } else {
                None
            };
            Some(CatchClause { param, body: self.block()? })
        } else {
            None
        };
        let finally = if self.eat_ident("finally") {
            Some(self.block()?)
        } else {
            None
        };
        if catch.is_none() && finally.is_none() {
            return Err(self.err("try needs catch or finally"));
        }
        Ok(Stmt::Try { block, catch, finally })
    }

    fn switch_stmt(&mut self) -> Result<Stmt, ParseError> {
        self.pos += 1;
        self.expect_punct(P::LParen)?;
        let disc = self.expr()?;
        self.expect_punct(P::RParen)?;
        self.expect_punct(P::LBrace)?;
        let mut cases = Vec::new();
        while !self.eat_punct(P::RBrace) {
            let test = if self.eat_ident("case") {
                let e = self.expr()?;
                self.expect_punct(P::Colon)?;
                Some(e)
            } else if self.eat_ident("default") {
                self.expect_punct(P::Colon)?;
                None
            } else {
                return Err(self.err("expected case or default"));
            };
            let mut body = Vec::new();
            while !matches!(self.kind(), Tok::Punct(P::RBrace) | Tok::Eof)
                && !self.at_ident("case")
                && !self.at_ident("default")
            {
                body.push(self.stmt()?);
            }
            cases.push(SwitchCase { test, body });
        }
        Ok(Stmt::Switch { disc, cases })
    }

    // ---- expressions ----

    fn expr(&mut self) -> Result<Expr, ParseError> {
        let first = self.assign_expr()?;
        if !self.at_punct(P::Comma) {
            return Ok(first);
        }
        let mut seq = vec![first];
        while self.eat_punct(P::Comma) {
            seq.push(self.assign_expr()?);
        }
        Ok(Expr::Seq(seq))
    }

    fn assign_expr(&mut self) -> Result<Expr, ParseError> {
        // `yield [expr]` inside a generator: a marker call the
        // generator transform turns into a state-machine suspension
        if self.in_generator {
            if let Tok::Ident(k) = self.kind() {
                if k == "yield" {
                    self.pos += 1;
                    if self.eat_punct(P::Star) {
                        return Err(self.err(
                            "yield* delegation not yet supported",
                        ));
                    }
                    let arg = if matches!(
                        self.kind(),
                        Tok::Punct(P::Semi) | Tok::Punct(P::RParen)
                            | Tok::Punct(P::RBracket)
                            | Tok::Punct(P::RBrace)
                            | Tok::Punct(P::Comma) | Tok::Eof
                    ) || self.nl_before()
                    {
                        Expr::Ident("undefined".to_string())
                    } else {
                        self.assign_expr()?
                    };
                    return Ok(Expr::Call {
                        callee: Box::new(Expr::Ident(
                            "__gg_yield".to_string(),
                        )),
                        args: vec![arg],
                        optional: false,
                    });
                }
            }
        }
        // `async` prefix: async function expr / async arrow
        if let Tok::Ident(k) = self.kind() {
            if k == "async" && !self.nl_before_at(1) {
                match self.kind_at(1) {
                    Some(Tok::Ident(n)) if n == "function" => {
                        self.pos += 2; // async function
                        let name = match self.kind() {
                            Tok::Ident(nm) if !self.at_punct(P::LParen) => {
                                let nm = nm.clone();
                                self.pos += 1;
                                Some(nm)
                            }
                            _ => None,
                        };
                        return Ok(Expr::Func(Rc::new(
                            self.func_lit(name, true)?,
                        )));
                    }
                    // async x => ...
                    Some(Tok::Ident(n)) if !is_keyword(n)
                        && matches!(self.kind_at(2), Some(Tok::Punct(P::Arrow))) => {
                        let param = n.clone();
                        self.pos += 3; // async ident =>
                        return self.arrow_body(vec![param], Vec::new(), true);
                    }
                    // async ( ... ) => ...
                    Some(Tok::Punct(P::LParen))
                        if self.is_arrow_ahead_from(1) => {
                        self.pos += 1; // async
                        let (params, prologue) = self.arrow_params()?;
                        self.expect_punct(P::Arrow)?;
                        return self.arrow_body(params, prologue, true);
                    }
                    _ => {}
                }
            }
        }
        // arrow lookahead: `x => ...` and `(a, b) => ...`
        if let Tok::Ident(name) = self.kind() {
            if !is_keyword(name)
                && matches!(self.kind_at(1), Some(Tok::Punct(P::Arrow)))
            {
                let params = vec![name.clone()];
                self.pos += 2;
                return self.arrow_body(params, Vec::new(), false);
            }
        }
        if self.at_punct(P::LParen) && self.is_arrow_ahead() {
            let (params, prologue) = self.arrow_params()?;
            self.expect_punct(P::Arrow)?;
            return self.arrow_body(params, prologue, false);
        }

        let left = self.cond_expr()?;
        let op = match self.kind() {
            Tok::Punct(P::Assign) => AssignOp::Plain,
            Tok::Punct(P::PlusEq) => AssignOp::Bin(BinOp::Add),
            Tok::Punct(P::MinusEq) => AssignOp::Bin(BinOp::Sub),
            Tok::Punct(P::StarEq) => AssignOp::Bin(BinOp::Mul),
            Tok::Punct(P::SlashEq) => AssignOp::Bin(BinOp::Div),
            Tok::Punct(P::PercentEq) => AssignOp::Bin(BinOp::Mod),
            Tok::Punct(P::StarStarEq) => AssignOp::Bin(BinOp::Pow),
            Tok::Punct(P::ShlEq) => AssignOp::Bin(BinOp::Shl),
            Tok::Punct(P::ShrEq) => AssignOp::Bin(BinOp::Shr),
            Tok::Punct(P::UShrEq) => AssignOp::Bin(BinOp::UShr),
            Tok::Punct(P::AmpEq) => AssignOp::Bin(BinOp::BitAnd),
            Tok::Punct(P::PipeEq) => AssignOp::Bin(BinOp::BitOr),
            Tok::Punct(P::CaretEq) => AssignOp::Bin(BinOp::BitXor),
            Tok::Punct(P::AmpAmpEq) => AssignOp::Log(LogOp::And),
            Tok::Punct(P::PipePipeEq) => AssignOp::Log(LogOp::Or),
            Tok::Punct(P::QuestionQuestionEq) => {
                AssignOp::Log(LogOp::Nullish)
            }
            _ => return Ok(left),
        };
        if !matches!(left, Expr::Ident(_) | Expr::Member { .. }) {
            // destructuring assignment: `[a, b] = x` / `({k} = o)`
            // desugars to ((__da) => { a = __da[0]; ...; return __da })(x)
            if matches!(op, AssignOp::Plain)
                && matches!(left, Expr::Array(_) | Expr::Object(_))
            {
                let tmp = self.fresh_tmp("da");
                let mut stmts = Vec::new();
                if Self::destr_into(
                    &left,
                    &Expr::Ident(tmp.clone()),
                    &mut stmts,
                ) {
                    self.pos += 1;
                    let right = self.assign_expr()?;
                    stmts.push(Stmt::Return(Some(Expr::Ident(
                        tmp.clone(),
                    ))));
                    return Ok(Expr::Call {
                        callee: Box::new(Expr::Arrow(Rc::new(FuncLit {
                            name: None,
                            params: vec![tmp],
                            body: stmts,
                            is_async: false,
                lazy_body: None,
                        }))),
                        args: vec![right],
                        optional: false,
                    });
                }
            }
            return Err(self.err("invalid assignment target"));
        }
        self.pos += 1;
        let right = self.assign_expr()?;
        Ok(Expr::Assign(op, Box::new(left), Box::new(right)))
    }

    /// Emit assignments unpacking `src` according to a literal used as
    /// a destructuring pattern. Returns false on unsupported shapes
    /// (the caller then reports the usual error).
    fn destr_into(pat: &Expr, src: &Expr, out: &mut Vec<Stmt>) -> bool {
        match pat {
            Expr::Array(els) => els.iter().enumerate().all(|(i, el)| {
                let item = Expr::Member {
                    obj: Box::new(src.clone()),
                    prop: MemberProp::Computed(Box::new(Expr::Num(
                        i as f64,
                    ))),
                    optional: false,
                };
                Self::destr_target(el, item, out)
            }),
            Expr::Object(props) => props.iter().all(|p| {
                let item = Expr::Member {
                    obj: Box::new(src.clone()),
                    prop: match &p.key {
                        PropKey::Ident(k) | PropKey::Str(k) => {
                            MemberProp::Static(k.clone())
                        }
                        PropKey::Num(k) => MemberProp::Computed(
                            Box::new(Expr::Num(*k)),
                        ),
                        PropKey::Computed(k) => MemberProp::Computed(
                            Box::new(k.clone()),
                        ),
                    },
                    optional: false,
                };
                Self::destr_target(&p.value, item, out)
            }),
            _ => false,
        }
    }

    fn destr_target(t: &Expr, item: Expr, out: &mut Vec<Stmt>) -> bool {
        match t {
            // an elision hole parses as `undefined` — skip it
            Expr::Ident(n) if n == "undefined" => true,
            Expr::Ident(_) | Expr::Member { .. } => {
                out.push(Stmt::Expr(Expr::Assign(
                    AssignOp::Plain,
                    Box::new(t.clone()),
                    Box::new(item),
                )));
                true
            }
            // `a = def` inside the pattern: default value
            Expr::Assign(AssignOp::Plain, target, def)
                if matches!(
                    **target,
                    Expr::Ident(_) | Expr::Member { .. }
                ) =>
            {
                let picked = Expr::Cond(
                    Box::new(Expr::Binary(
                        BinOp::StrictEq,
                        Box::new(item.clone()),
                        Box::new(Expr::Ident("undefined".to_string())),
                    )),
                    Box::new((**def).clone()),
                    Box::new(item),
                );
                out.push(Stmt::Expr(Expr::Assign(
                    AssignOp::Plain,
                    target.clone(),
                    Box::new(picked),
                )));
                true
            }
            // nested pattern: unpack the member chain
            Expr::Array(_) | Expr::Object(_) => {
                Self::destr_into(t, &item, out)
            }
            _ => false,
        }
    }

    /// At a `(`: does the matching `)` have `=>` right after it?
    fn is_arrow_ahead(&self) -> bool {
        self.is_arrow_ahead_from(0)
    }

    /// Like is_arrow_ahead but starts scanning at pos+off (the `(`).
    fn is_arrow_ahead_from(&self, off: usize) -> bool {
        let mut depth = 0usize;
        let mut i = self.pos + off;
        while let Some(t) = self.toks.get(i) {
            match t.kind {
                Tok::Punct(P::LParen) => depth += 1,
                Tok::Punct(P::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(
                            self.toks.get(i + 1).map(|t| &t.kind),
                            Some(Tok::Punct(P::Arrow))
                        );
                    }
                }
                Tok::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// Parse a parameter list, returning the flat param names plus any
    /// prologue statements that must run at function entry (default
    /// values and destructuring-pattern params desugar to these).
    fn arrow_params(
        &mut self,
    ) -> Result<(Vec<String>, Vec<Stmt>), ParseError> {
        self.expect_punct(P::LParen)?;
        let mut params = Vec::new();
        let mut prologue = Vec::new();
        if !self.eat_punct(P::RParen) {
            loop {
                if self.eat_punct(P::DotDotDot) {
                    // rest param: our `arguments` is a real array, so
                    // `(...rest)` desugars to arguments.slice(n)
                    let name = self.expect_ident()?;
                    let n = params.len();
                    prologue.push(Stmt::VarDecl {
                        kind: DeclKind::Var,
                        decls: vec![(name, Some(Expr::Call {
                            callee: Box::new(Expr::Member {
                                obj: Box::new(Expr::Ident(
                                    "arguments".to_string(),
                                )),
                                prop: MemberProp::Static(
                                    "slice".to_string(),
                                ),
                                optional: false,
                            }),
                            args: vec![Expr::Num(n as f64)],
                            optional: false,
                        }))],
                    });
                    self.expect_punct(P::RParen)?;
                    break;
                }
                if self.at_punct(P::LBrace) || self.at_punct(P::LBracket) {
                    // destructuring param: bind a temp, unpack in body
                    let tmp = self.fresh_tmp("p");
                    let mut binds: Vec<(String, Option<Expr>)> = Vec::new();
                    self.pattern_binds(&tmp, &mut binds, false)?;
                    params.push(tmp);
                    prologue.push(Stmt::VarDecl {
                        kind: DeclKind::Var, decls: binds,
                    });
                } else {
                    let name = self.expect_ident()?;
                    if self.eat_punct(P::Assign) {
                        // default: name = name === undefined ? def : name
                        let def = self.assign_expr()?;
                        prologue.push(Stmt::Expr(Expr::Assign(
                            AssignOp::Plain,
                            Box::new(Expr::Ident(name.clone())),
                            Box::new(Expr::Cond(
                                Box::new(Expr::Binary(
                                    BinOp::StrictNotEq,
                                    Box::new(Expr::Ident(name.clone())),
                                    Box::new(Expr::Ident(
                                        "undefined".to_string())),
                                )),
                                Box::new(Expr::Ident(name.clone())),
                                Box::new(def),
                            )),
                        )));
                    }
                    params.push(name);
                }
                if self.eat_punct(P::RParen) {
                    break;
                }
                self.expect_punct(P::Comma)?;
            }
        }
        Ok((params, prologue))
    }

    fn arrow_body(
        &mut self,
        params: Vec<String>,
        prologue: Vec<Stmt>,
        is_async: bool,
    ) -> Result<Expr, ParseError> {
        if !is_async && prologue.is_empty()
            && self.class_super.is_none() && self.at_punct(P::LBrace)
        {
            if let Some(lz) = self.try_lazy_body()? {
                return Ok(Expr::Arrow(Rc::new(FuncLit {
                    name: None,
                    params,
                    body: Vec::new(),
                    is_async: false,
                    lazy_body: Some(lz),
                })));
            }
        }
        let saved = self.in_async;
        self.in_async = is_async;
        let body = if self.at_punct(P::LBrace) {
            self.block()
        } else {
            self.assign_expr().map(|e| vec![Stmt::Return(Some(e))])
        };
        self.in_async = saved;
        let mut body = body?;
        if !prologue.is_empty() {
            let mut full = prologue;
            full.append(&mut body);
            body = full;
        }
        Ok(Expr::Arrow(Rc::new(FuncLit {
            name: None,
            params,
            body,
            is_async,
            lazy_body: None,
        })))
    }

    fn cond_expr(&mut self) -> Result<Expr, ParseError> {
        let test = self.binary_expr(1)?;
        if !self.eat_punct(P::Question) {
            return Ok(test);
        }
        let cons = self.assign_expr()?;
        self.expect_punct(P::Colon)?;
        let alt = self.assign_expr()?;
        Ok(Expr::Cond(Box::new(test), Box::new(cons), Box::new(alt)))
    }

    /// (precedence, op, right-associative)
    fn peek_binop(&self) -> Option<(u8, OpKind, bool)> {
        use BinOp::*;
        use LogOp::*;
        let (prec, op, right) = match self.kind() {
            Tok::Punct(p) => match p {
                P::QuestionQuestion => (1, OpKind::Log(Nullish), false),
                P::PipePipe => (2, OpKind::Log(Or), false),
                P::AmpAmp => (3, OpKind::Log(And), false),
                P::Pipe => (4, OpKind::Bin(BitOr), false),
                P::Caret => (5, OpKind::Bin(BitXor), false),
                P::Amp => (6, OpKind::Bin(BitAnd), false),
                P::EqEq => (7, OpKind::Bin(EqEq), false),
                P::NotEq => (7, OpKind::Bin(NotEq), false),
                P::EqEqEq => (7, OpKind::Bin(StrictEq), false),
                P::NotEqEq => (7, OpKind::Bin(StrictNotEq), false),
                P::Lt => (8, OpKind::Bin(Lt), false),
                P::Gt => (8, OpKind::Bin(Gt), false),
                P::LtEq => (8, OpKind::Bin(LtEq), false),
                P::GtEq => (8, OpKind::Bin(GtEq), false),
                P::Shl => (9, OpKind::Bin(Shl), false),
                P::Shr => (9, OpKind::Bin(Shr), false),
                P::UShr => (9, OpKind::Bin(UShr), false),
                P::Plus => (10, OpKind::Bin(Add), false),
                P::Minus => (10, OpKind::Bin(Sub), false),
                P::Star => (11, OpKind::Bin(Mul), false),
                P::Slash => (11, OpKind::Bin(Div), false),
                P::Percent => (11, OpKind::Bin(Mod), false),
                P::StarStar => (12, OpKind::Bin(Pow), true),
                _ => return None,
            },
            Tok::Ident(k) => match k.as_str() {
                "in" if !self.no_in => (8, OpKind::Bin(In), false),
                "instanceof" => (8, OpKind::Bin(InstanceOf), false),
                _ => return None,
            },
            _ => return None,
        };
        Some((prec, op, right))
    }

    fn binary_expr(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut left = self.unary_expr()?;
        while let Some((prec, op, right_assoc)) = self.peek_binop() {
            if prec < min_prec {
                break;
            }
            self.pos += 1;
            let next_min = if right_assoc { prec } else { prec + 1 };
            let right = self.binary_expr(next_min)?;
            left = match op {
                OpKind::Bin(b) => {
                    Expr::Binary(b, Box::new(left), Box::new(right))
                }
                OpKind::Log(l) => {
                    Expr::Logical(l, Box::new(left), Box::new(right))
                }
            };
        }
        Ok(left)
    }

    fn unary_expr(&mut self) -> Result<Expr, ParseError> {
        let op = match self.kind() {
            Tok::Punct(P::Not) => Some(UnOp::Not),
            Tok::Punct(P::Tilde) => Some(UnOp::BitNot),
            Tok::Punct(P::Plus) => Some(UnOp::Pos),
            Tok::Punct(P::Minus) => Some(UnOp::Neg),
            Tok::Ident(k) => match k.as_str() {
                "typeof" => Some(UnOp::Typeof),
                "void" => Some(UnOp::Void),
                "delete" => Some(UnOp::Delete),
                _ => None,
            },
            _ => None,
        };
        if let Some(op) = op {
            self.pos += 1;
            let operand = self.unary_expr()?;
            return Ok(Expr::Unary(op, Box::new(operand)));
        }
        // `await E` — only an operator inside an async function
        if self.in_async {
            if let Tok::Ident(k) = self.kind() {
                if k == "await" {
                    self.pos += 1;
                    let operand = self.unary_expr()?;
                    return Ok(Expr::Await(Box::new(operand)));
                }
            }
        }
        if self.at_punct(P::PlusPlus) || self.at_punct(P::MinusMinus) {
            let op = if self.at_punct(P::PlusPlus) {
                UpdateOp::Inc
            } else {
                UpdateOp::Dec
            };
            self.pos += 1;
            let target = self.unary_expr()?;
            if !matches!(target, Expr::Ident(_) | Expr::Member { .. }) {
                return Err(self.err("invalid ++/-- target"));
            }
            return Ok(Expr::Update {
                op, prefix: true, target: Box::new(target),
            });
        }
        self.postfix_expr()
    }

    fn postfix_expr(&mut self) -> Result<Expr, ParseError> {
        let e = self.call_member_expr()?;
        // ASI: `a\n++b` is two statements — postfix never crosses a line
        if (self.at_punct(P::PlusPlus) || self.at_punct(P::MinusMinus))
            && !self.nl_before()
        {
            let op = if self.at_punct(P::PlusPlus) {
                UpdateOp::Inc
            } else {
                UpdateOp::Dec
            };
            self.pos += 1;
            if !matches!(e, Expr::Ident(_) | Expr::Member { .. }) {
                return Err(self.err("invalid ++/-- target"));
            }
            return Ok(Expr::Update {
                op, prefix: false, target: Box::new(e),
            });
        }
        Ok(e)
    }

    fn call_member_expr(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.primary_expr()?;
        loop {
            match self.kind() {
                Tok::Punct(P::Dot) => {
                    self.pos += 1;
                    let name = self.expect_ident()?;
                    e = Expr::Member {
                        obj: Box::new(e),
                        prop: MemberProp::Static(name),
                        optional: false,
                    };
                }
                Tok::Punct(P::QuestionDot) => {
                    self.pos += 1;
                    if self.eat_punct(P::LParen) {
                        let (args, spread) = self.arguments_spread()?;
                        e = if spread {
                            // f?.(...a) -> f?.apply(this_arg, arr);
                            // o.m?.(...a) keeps o as the receiver (o is
                            // re-evaluated — fine for simple receivers)
                            let this_arg = match &e {
                                Expr::Member { obj, .. } => {
                                    (**obj).clone()
                                }
                                _ => Expr::Ident(
                                    "undefined".to_string(),
                                ),
                            };
                            Expr::Call {
                                callee: Box::new(Expr::Member {
                                    obj: Box::new(e),
                                    prop: MemberProp::Static(
                                        "apply".to_string(),
                                    ),
                                    optional: true,
                                }),
                                args: {
                                    let mut v = vec![this_arg];
                                    v.extend(args);
                                    v
                                },
                                optional: false,
                            }
                        } else {
                            Expr::Call {
                                callee: Box::new(e), args, optional: true,
                            }
                        };
                    } else if self.eat_punct(P::LBracket) {
                        let k = self.expr()?;
                        self.expect_punct(P::RBracket)?;
                        e = Expr::Member {
                            obj: Box::new(e),
                            prop: MemberProp::Computed(Box::new(k)),
                            optional: true,
                        };
                    } else {
                        let name = self.expect_ident()?;
                        e = Expr::Member {
                            obj: Box::new(e),
                            prop: MemberProp::Static(name),
                            optional: true,
                        };
                    }
                }
                Tok::Punct(P::LBracket) => {
                    self.pos += 1;
                    let k = self.expr()?;
                    self.expect_punct(P::RBracket)?;
                    e = Expr::Member {
                        obj: Box::new(e),
                        prop: MemberProp::Computed(Box::new(k)),
                        optional: false,
                    };
                }
                Tok::Punct(P::LParen) => {
                    self.pos += 1;
                    let (mut args, spread) = self.arguments_spread()?;
                    if spread {
                        // f(...a) -> f.apply(undefined, argsArray);
                        // o.m(...a) -> o.m.apply(o, argsArray)
                        let this_arg = match &e {
                            Expr::Member { obj, .. } => (**obj).clone(),
                            _ => Expr::Ident("undefined".to_string()),
                        };
                        let arr = args.pop().unwrap();
                        let apply = Expr::Member {
                            obj: Box::new(e),
                            prop: MemberProp::Static("apply".to_string()),
                            optional: false,
                        };
                        e = Expr::Call {
                            callee: Box::new(apply),
                            args: vec![this_arg, arr],
                            optional: false,
                        };
                    } else {
                        e = Expr::Call {
                            callee: Box::new(e), args, optional: false,
                        };
                    }
                }
                _ => return Ok(e),
            }
        }
    }

    /// `(` already consumed; parses to `)`.
    fn arguments(&mut self) -> Result<Vec<Expr>, ParseError> {
        let (args, spread) = self.arguments_spread()?;
        if spread {
            // reachable only via ?.() with a spread arg — rare; the
            // normal-call path handles spread by desugaring to .apply.
            return Err(self.err("spread in optional call not supported"));
        }
        Ok(args)
    }

    /// Parse args; the bool is true when a `...spread` was present, in
    /// which case the returned Vec has a single element: an expression
    /// evaluating to the fully-assembled arguments array.
    fn arguments_spread(
        &mut self,
    ) -> Result<(Vec<Expr>, bool), ParseError> {
        let mut parts: Vec<(bool, Expr)> = Vec::new();
        if self.eat_punct(P::RParen) {
            return Ok((Vec::new(), false));
        }
        loop {
            let spread = self.eat_punct(P::DotDotDot);
            parts.push((spread, self.assign_expr()?));
            if self.eat_punct(P::RParen) {
                break;
            }
            self.expect_punct(P::Comma)?;
            if self.eat_punct(P::RParen) {
                break;
            }
        }
        if !parts.iter().any(|(s, _)| *s) {
            return Ok((parts.into_iter().map(|(_, e)| e).collect(), false));
        }
        // assemble [a, ...b, c] -> [].concat([a], b, [c])
        let mut segs: Vec<Expr> = Vec::new();
        let mut buf: Vec<Expr> = Vec::new();
        for (spread, e) in parts {
            if spread {
                if !buf.is_empty() {
                    segs.push(Expr::Array(std::mem::take(&mut buf)));
                }
                segs.push(e);
            } else {
                buf.push(e);
            }
        }
        if !buf.is_empty() {
            segs.push(Expr::Array(buf));
        }
        let arr = Expr::Call {
            callee: Box::new(Expr::Member {
                obj: Box::new(Expr::Array(Vec::new())),
                prop: MemberProp::Static("concat".to_string()),
                optional: false,
            }),
            args: segs,
            optional: false,
        };
        Ok((vec![arr], true))
    }

    fn primary_expr(&mut self) -> Result<Expr, ParseError> {
        match self.bump() {
            Tok::Num(n) => Ok(Expr::Num(n)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Regex { pattern, flags } => {
                Ok(Expr::Regex { pattern, flags })
            }
            Tok::Template(parts) => {
                let mut out = Vec::new();
                for part in parts {
                    out.push(match part {
                        TplElem::Chunk(s) => TplPart::Chunk(s),
                        TplElem::ExprSrc(src) => {
                            let mut sub = Parser::new(Rc::new(tokenize(&src)?));
                            let e = sub.expr()?;
                            if !sub.at_eof() {
                                return Err(self.err(
                                    "unexpected tokens in template hole",
                                ));
                            }
                            TplPart::Expr(Box::new(e))
                        }
                    });
                }
                Ok(Expr::Template(out))
            }
            Tok::Ident(name) => match name.as_str() {
                "this" => Ok(Expr::This),
                "true" => Ok(Expr::Bool(true)),
                "false" => Ok(Expr::Bool(false)),
                "null" => Ok(Expr::Null),
                "function" => {
                    let is_gen = self.eat_punct(P::Star);
                    let name = match self.kind() {
                        Tok::Ident(n) if !self.at_punct(P::LParen) => {
                            let n = n.clone();
                            self.pos += 1;
                            Some(n)
                        }
                        _ => None,
                    };
                    Ok(Expr::Func(Rc::new(
                        self.func_lit_g(name, false, is_gen)?,
                    )))
                }
                "new" => {
                    let callee = self.member_only_expr()?;
                    let (args, spread) = if self.eat_punct(P::LParen) {
                        self.arguments_spread()?
                    } else {
                        (Vec::new(), false)
                    };
                    if spread {
                        // new C(...a) — the Babel _construct shape our
                        // bound-constructor new already understands:
                        // new (C.bind.apply(C, [null].concat(a)))()
                        let arr = args.into_iter().next().unwrap();
                        let bind_args = Expr::Call {
                            callee: Box::new(Expr::Member {
                                obj: Box::new(Expr::Array(vec![
                                    Expr::Null,
                                ])),
                                prop: MemberProp::Static(
                                    "concat".to_string(),
                                ),
                                optional: false,
                            }),
                            args: vec![arr],
                            optional: false,
                        };
                        let bound = Expr::Call {
                            callee: Box::new(Expr::Member {
                                obj: Box::new(Expr::Member {
                                    obj: Box::new(callee.clone()),
                                    prop: MemberProp::Static(
                                        "bind".to_string(),
                                    ),
                                    optional: false,
                                }),
                                prop: MemberProp::Static(
                                    "apply".to_string(),
                                ),
                                optional: false,
                            }),
                            args: vec![callee, bind_args],
                            optional: false,
                        };
                        return Ok(Expr::New {
                            callee: Box::new(bound),
                            args: Vec::new(),
                        });
                    }
                    Ok(Expr::New { callee: Box::new(callee), args })
                }
                "class" => {
                    let cname = match self.kind() {
                        Tok::Ident(n) if n != "extends"
                            && !self.at_punct(P::LBrace) => {
                            let n = n.clone();
                            self.pos += 1;
                            Some(n)
                        }
                        _ => None,
                    };
                    self.class_lit(cname)
                }
                "super" if self.class_super.is_some() => {
                    let sup = self.class_super.clone().unwrap();
                    let this_call = |target: Expr,
                                     args: Vec<Expr>,
                                     spread: bool| {
                        // target(args) with `this` bound to the instance;
                        // a spread arg list arrives as one assembled
                        // array, which is exactly apply's shape
                        let verb = if spread { "apply" } else { "call" };
                        let mut full = vec![Expr::This];
                        full.extend(args);
                        Expr::Call {
                            callee: Box::new(Expr::Member {
                                obj: Box::new(target),
                                prop: MemberProp::Static(verb.to_string()),
                                optional: false,
                            }),
                            args: full,
                            optional: false,
                        }
                    };
                    if self.eat_punct(P::LParen) {
                        // super(args) -> parent.call(this, args)
                        let (args, spread) = self.arguments_spread()?;
                        Ok(this_call(Expr::Ident(sup), args, spread))
                    } else if self.eat_punct(P::Dot) {
                        let pname = self.expect_ident()?;
                        let target = Expr::Member {
                            obj: Box::new(Expr::Member {
                                obj: Box::new(Expr::Ident(sup)),
                                prop: MemberProp::Static(
                                    "prototype".to_string(),
                                ),
                                optional: false,
                            }),
                            prop: MemberProp::Static(pname),
                            optional: false,
                        };
                        if self.eat_punct(P::LParen) {
                            // super.m(args) -> parent.prototype.m.call(this)
                            let (args, spread) = self.arguments_spread()?;
                            Ok(this_call(target, args, spread))
                        } else {
                            Ok(target)
                        }
                    } else {
                        // bare `super` (super['k'] etc.): the prototype
                        Ok(Expr::Member {
                            obj: Box::new(Expr::Ident(sup)),
                            prop: MemberProp::Static(
                                "prototype".to_string(),
                            ),
                            optional: false,
                        })
                    }
                }
                _ => Ok(Expr::Ident(name)),
            },
            Tok::Punct(P::LParen) => {
                // parens lift the for-head no-in restriction
                let saved_no_in = self.no_in;
                self.no_in = false;
                let e = self.expr();
                self.no_in = saved_no_in;
                let e = e?;
                self.expect_punct(P::RParen)?;
                Ok(e)
            }
            Tok::Punct(P::LBracket) => self.array_lit(),
            Tok::Punct(P::LBrace) => self.object_lit(),
            t => Err(self.err(format!("unexpected token {t:?}"))),
        }
    }

    /// `new` callee: member accesses bind tighter than the call parens.
    fn member_only_expr(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.primary_expr()?;
        loop {
            match self.kind() {
                Tok::Punct(P::Dot) => {
                    self.pos += 1;
                    let name = self.expect_ident()?;
                    e = Expr::Member {
                        obj: Box::new(e),
                        prop: MemberProp::Static(name),
                        optional: false,
                    };
                }
                Tok::Punct(P::LBracket) => {
                    self.pos += 1;
                    let k = self.expr()?;
                    self.expect_punct(P::RBracket)?;
                    e = Expr::Member {
                        obj: Box::new(e),
                        prop: MemberProp::Computed(Box::new(k)),
                        optional: false,
                    };
                }
                _ => return Ok(e),
            }
        }
    }

    /// `[` already consumed.
    fn array_lit(&mut self) -> Result<Expr, ParseError> {
        // (is_spread, expr) so `[a, ...b, c]` can desugar to concat
        let mut parts: Vec<(bool, Expr)> = Vec::new();
        if self.eat_punct(P::RBracket) {
            return Ok(Expr::Array(Vec::new()));
        }
        loop {
            // elision: a bare comma is a hole ([,x] / [1,,3])
            if self.at_punct(P::Comma) {
                self.pos += 1;
                parts.push((false, Expr::Ident("undefined".to_string())));
                if self.eat_punct(P::RBracket) {
                    break;
                }
                continue;
            }
            let spread = self.eat_punct(P::DotDotDot);
            parts.push((spread, self.assign_expr()?));
            if self.eat_punct(P::RBracket) {
                break;
            }
            self.expect_punct(P::Comma)?;
            if self.eat_punct(P::RBracket) {
                break;
            }
        }
        if !parts.iter().any(|(s, _)| *s) {
            return Ok(Expr::Array(
                parts.into_iter().map(|(_, e)| e).collect(),
            ));
        }
        // [a, ...b, c] -> [].concat([a], b, [c])
        let mut segs: Vec<Expr> = Vec::new();
        let mut buf: Vec<Expr> = Vec::new();
        for (spread, e) in parts {
            if spread {
                if !buf.is_empty() {
                    segs.push(Expr::Array(std::mem::take(&mut buf)));
                }
                segs.push(e);
            } else {
                buf.push(e);
            }
        }
        if !buf.is_empty() {
            segs.push(Expr::Array(buf));
        }
        Ok(Expr::Call {
            callee: Box::new(Expr::Member {
                obj: Box::new(Expr::Array(Vec::new())),
                prop: MemberProp::Static("concat".to_string()),
                optional: false,
            }),
            args: segs,
            optional: false,
        })
    }

    /// `{` already consumed.
    fn object_lit(&mut self) -> Result<Expr, ParseError> {
        let mut props = Vec::new();
        // spread segments: `{a, ...b, c}` desugars to
        // Object.assign({}, {a}, b, {c})
        let mut segs: Vec<Expr> = Vec::new();
        let mut has_spread = false;
        // accessor entries: (key, getter, setter)
        let mut accs: Vec<(String, Option<FuncLit>, Option<FuncLit>)> =
            Vec::new();
        while !self.eat_punct(P::RBrace) {
            // generator method: { *gen() {...} }
            if self.at_punct(P::Star)
                && matches!(self.kind_at(1),
                    Some(Tok::Ident(_)) | Some(Tok::Str(_)))
                && matches!(self.kind_at(2), Some(Tok::Punct(P::LParen)))
            {
                self.pos += 1;
                let key = match self.bump() {
                    Tok::Ident(n) => n,
                    Tok::Str(s) => s,
                    _ => unreachable!(),
                };
                let f =
                    self.func_lit_g(Some(key.clone()), false, true)?;
                props.push(Prop {
                    key: PropKey::Ident(key),
                    value: Expr::Func(Rc::new(f)),
                });
                if !self.eat_punct(P::Comma) {
                    self.expect_punct(P::RBrace)?;
                    break;
                }
                continue;
            }
            // getter/setter shorthand: { get name() {...} }
            if matches!(self.kind(), Tok::Ident(g)
                    if g == "get" || g == "set")
                && matches!(self.kind_at(1),
                    Some(Tok::Ident(_)) | Some(Tok::Str(_)))
                && matches!(self.kind_at(2), Some(Tok::Punct(P::LParen)))
            {
                let is_get =
                    matches!(self.kind(), Tok::Ident(g) if g == "get");
                self.pos += 1;
                let key = match self.bump() {
                    Tok::Ident(n) => n,
                    Tok::Str(s) => s,
                    _ => unreachable!(),
                };
                let f = self.func_lit(Some(key.clone()), false)?;
                if let Some(slot) =
                    accs.iter_mut().find(|(k, _, _)| *k == key)
                {
                    if is_get {
                        slot.1 = Some(f);
                    } else {
                        slot.2 = Some(f);
                    }
                } else if is_get {
                    accs.push((key, Some(f), None));
                } else {
                    accs.push((key, None, Some(f)));
                }
                if !self.eat_punct(P::Comma) {
                    self.expect_punct(P::RBrace)?;
                    break;
                }
                continue;
            }
            if self.eat_punct(P::DotDotDot) {
                has_spread = true;
                if !props.is_empty() {
                    segs.push(Expr::Object(std::mem::take(&mut props)));
                }
                segs.push(self.assign_expr()?);
                if !self.eat_punct(P::Comma) {
                    self.expect_punct(P::RBrace)?;
                    break;
                }
                continue;
            }
            let prop = match self.bump() {
                Tok::Ident(name) => {
                    if self.at_punct(P::LParen) {
                        // method shorthand: { foo() { ... } }
                        let f = self.func_lit(Some(name.clone()), false)?;
                        Prop {
                            key: PropKey::Ident(name),
                            value: Expr::Func(Rc::new(f)),
                        }
                    } else if self.eat_punct(P::Colon) {
                        Prop {
                            key: PropKey::Ident(name),
                            value: self.assign_expr()?,
                        }
                    } else if self.eat_punct(P::Assign) {
                        // shorthand default `{ a = 1 }` — meaningful as
                        // a destructuring pattern; as a plain literal it
                        // evaluates like `{ a: a = 1 }`
                        let def = self.assign_expr()?;
                        Prop {
                            key: PropKey::Ident(name.clone()),
                            value: Expr::Assign(
                                AssignOp::Plain,
                                Box::new(Expr::Ident(name)),
                                Box::new(def),
                            ),
                        }
                    } else {
                        // shorthand: { a }
                        Prop {
                            key: PropKey::Ident(name.clone()),
                            value: Expr::Ident(name),
                        }
                    }
                }
                Tok::Str(s) => {
                    if self.at_punct(P::LParen) {
                        // string-keyed method: { "foo"() {...} }
                        let f = self.func_lit(Some(s.clone()), false)?;
                        Prop {
                            key: PropKey::Str(s),
                            value: Expr::Func(Rc::new(f)),
                        }
                    } else {
                        self.expect_punct(P::Colon)?;
                        Prop {
                            key: PropKey::Str(s),
                            value: self.assign_expr()?,
                        }
                    }
                }
                Tok::Num(n) => {
                    if self.at_punct(P::LParen) {
                        // number-keyed method: { 0() {...} }
                        let f = self.func_lit(None, false)?;
                        Prop {
                            key: PropKey::Num(n),
                            value: Expr::Func(Rc::new(f)),
                        }
                    } else {
                        self.expect_punct(P::Colon)?;
                        Prop {
                            key: PropKey::Num(n),
                            value: self.assign_expr()?,
                        }
                    }
                }
                Tok::Punct(P::LBracket) => {
                    let k = self.assign_expr()?;
                    self.expect_punct(P::RBracket)?;
                    if self.at_punct(P::LParen) {
                        // computed method: { [k]() {...} }
                        let f = self.func_lit(None, false)?;
                        Prop {
                            key: PropKey::Computed(k),
                            value: Expr::Func(Rc::new(f)),
                        }
                    } else {
                        self.expect_punct(P::Colon)?;
                        Prop {
                            key: PropKey::Computed(k),
                            value: self.assign_expr()?,
                        }
                    }
                }
                t => {
                    return Err(
                        self.err(format!("bad object literal key: {t:?}"))
                    )
                }
            };
            props.push(prop);
            if !self.eat_punct(P::Comma) {
                self.expect_punct(P::RBrace)?;
                break;
            }
        }
        let base = if !has_spread {
            Expr::Object(props)
        } else {
            if !props.is_empty() {
                segs.push(Expr::Object(props));
            }
            let mut args = vec![Expr::Object(Vec::new())];
            args.extend(segs);
            Expr::Call {
                callee: Box::new(Expr::Member {
                    obj: Box::new(Expr::Ident("Object".to_string())),
                    prop: MemberProp::Static("assign".to_string()),
                    optional: false,
                }),
                args,
                optional: false,
            }
        };
        if accs.is_empty() {
            return Ok(base);
        }
        // accessors: (function(){ var o = base;
        //   Object.defineProperty(o, k, {get, set}); ... return o })()
        let tmp = self.fresh_tmp("obj");
        let mut body = vec![Stmt::VarDecl {
            kind: DeclKind::Var,
            decls: vec![(tmp.clone(), Some(base))],
        }];
        for (key, getter, setter) in accs {
            let mut dprops = vec![Prop {
                key: PropKey::Ident("configurable".to_string()),
                value: Expr::Bool(true),
            }];
            if let Some(g) = getter {
                dprops.push(Prop {
                    key: PropKey::Ident("get".to_string()),
                    value: Expr::Func(Rc::new(g)),
                });
            }
            if let Some(s) = setter {
                dprops.push(Prop {
                    key: PropKey::Ident("set".to_string()),
                    value: Expr::Func(Rc::new(s)),
                });
            }
            body.push(Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Member {
                    obj: Box::new(Expr::Ident("Object".to_string())),
                    prop: MemberProp::Static("defineProperty".to_string()),
                    optional: false,
                }),
                args: vec![
                    Expr::Ident(tmp.clone()),
                    Expr::Str(key),
                    Expr::Object(dprops),
                ],
                optional: false,
            }));
        }
        body.push(Stmt::Return(Some(Expr::Ident(tmp))));
        Ok(Expr::Call {
            callee: Box::new(Expr::Func(Rc::new(FuncLit {
                name: None,
                params: Vec::new(),
                body,
                is_async: false,
                lazy_body: None,
            }))),
            args: Vec::new(),
            optional: false,
        })
    }

    /// The `yield E` marker the generator body parser emits.
    fn yield_marker(e: &Expr) -> Option<Expr> {
        if let Expr::Call { callee, args, .. } = e {
            if matches!(&**callee, Expr::Ident(n) if n == "__gg_yield") {
                return Some(args.first().cloned().unwrap_or_else(
                    || Expr::Ident("undefined".to_string()),
                ));
            }
        }
        None
    }

    /// Canonical statement-level yields:
    /// `yield E;` / `x = yield E;` (var-decl form arrives here already
    /// converted to an assignment by hoist_gen_vars).
    fn yield_shape(s: &Stmt) -> Option<(Option<Expr>, Expr)> {
        if let Stmt::Expr(e) = s {
            if let Some(v) = Self::yield_marker(e) {
                if !Self::expr_has_marker(&v) {
                    return Some((None, v));
                }
            }
            if let Expr::Assign(AssignOp::Plain, t, val) = e {
                if let Some(v) = Self::yield_marker(val) {
                    if !Self::expr_has_marker(&v)
                        && !Self::expr_has_marker(t)
                    {
                        return Some((Some((**t).clone()), v));
                    }
                }
            }
        }
        None
    }

    fn expr_has_marker(e: &Expr) -> bool {
        match e {
            Expr::Call { callee, args, .. } => {
                matches!(&**callee, Expr::Ident(n) if n == "__gg_yield")
                    || Self::expr_has_marker(callee)
                    || args.iter().any(Self::expr_has_marker)
            }
            Expr::Func(_) | Expr::Arrow(_) => false,
            Expr::Unary(_, a) | Expr::Await(a) => {
                Self::expr_has_marker(a)
            }
            Expr::Update { target, .. } => Self::expr_has_marker(target),
            Expr::Binary(_, a, b) | Expr::Logical(_, a, b) => {
                Self::expr_has_marker(a) || Self::expr_has_marker(b)
            }
            Expr::Assign(_, a, b) => {
                Self::expr_has_marker(a) || Self::expr_has_marker(b)
            }
            Expr::Cond(c, a, b) => {
                Self::expr_has_marker(c)
                    || Self::expr_has_marker(a)
                    || Self::expr_has_marker(b)
            }
            Expr::Member { obj, prop, .. } => {
                Self::expr_has_marker(obj)
                    || matches!(prop, MemberProp::Computed(k)
                        if Self::expr_has_marker(k))
            }
            Expr::New { callee, args } => {
                Self::expr_has_marker(callee)
                    || args.iter().any(Self::expr_has_marker)
            }
            Expr::Array(v) | Expr::Seq(v) => {
                v.iter().any(Self::expr_has_marker)
            }
            Expr::Object(props) => props.iter().any(|p| {
                Self::expr_has_marker(&p.value)
                    || matches!(&p.key, PropKey::Computed(k)
                        if Self::expr_has_marker(k))
            }),
            Expr::Template(parts) => parts.iter().any(|p| {
                matches!(p, TplPart::Expr(e)
                    if Self::expr_has_marker(e))
            }),
            _ => false,
        }
    }

    fn stmt_has_marker(s: &Stmt) -> bool {
        match s {
            Stmt::Expr(e) | Stmt::Throw(e) => Self::expr_has_marker(e),
            Stmt::VarDecl { decls, .. } => decls.iter().any(|(_, i)| {
                i.as_ref().is_some_and(Self::expr_has_marker)
            }),
            Stmt::Return(e) => {
                e.as_ref().is_some_and(Self::expr_has_marker)
            }
            Stmt::If { test, cons, alt } => {
                Self::expr_has_marker(test)
                    || Self::stmt_has_marker(cons)
                    || alt.as_deref().is_some_and(Self::stmt_has_marker)
            }
            Stmt::While { test, body } => {
                Self::expr_has_marker(test) || Self::stmt_has_marker(body)
            }
            Stmt::DoWhile { body, test } => {
                Self::expr_has_marker(test) || Self::stmt_has_marker(body)
            }
            Stmt::For { init, test, update, body } => {
                init.as_deref().is_some_and(Self::stmt_has_marker)
                    || test.as_ref().is_some_and(Self::expr_has_marker)
                    || update
                        .as_ref()
                        .is_some_and(Self::expr_has_marker)
                    || Self::stmt_has_marker(body)
            }
            Stmt::ForIn { obj, body, .. } => {
                Self::expr_has_marker(obj) || Self::stmt_has_marker(body)
            }
            Stmt::Block(v) => v.iter().any(Self::stmt_has_marker),
            Stmt::Labeled { body, .. } => Self::stmt_has_marker(body),
            Stmt::Try { block, catch, finally } => {
                block.iter().any(Self::stmt_has_marker)
                    || catch.as_ref().is_some_and(|c| {
                        c.body.iter().any(Self::stmt_has_marker)
                    })
                    || finally.as_ref().is_some_and(|f| {
                        f.iter().any(Self::stmt_has_marker)
                    })
            }
            Stmt::Switch { disc, cases } => {
                Self::expr_has_marker(disc)
                    || cases.iter().any(|c| {
                        c.body.iter().any(Self::stmt_has_marker)
                    })
            }
            _ => false,
        }
    }

    /// Hoist `var` names out of a generator body statement (locals
    /// must live in the enclosing function so they survive across
    /// next() calls); declarations become plain assignments.
    fn hoist_gen_vars(s: Stmt, names: &mut Vec<String>) -> Stmt {
        match s {
            Stmt::VarDecl { decls, .. } => {
                let mut assigns: Vec<Stmt> = Vec::new();
                for (n, init) in decls {
                    names.push(n.clone());
                    if let Some(e) = init {
                        assigns.push(Stmt::Expr(Expr::Assign(
                            AssignOp::Plain,
                            Box::new(Expr::Ident(n)),
                            Box::new(e),
                        )));
                    }
                }
                match assigns.len() {
                    0 => Stmt::Empty,
                    1 => assigns.pop().unwrap(),
                    _ => Stmt::Block(assigns),
                }
            }
            Stmt::Block(v) => Stmt::Block(
                v.into_iter()
                    .map(|s| Self::hoist_gen_vars(s, names))
                    .collect(),
            ),
            Stmt::If { test, cons, alt } => Stmt::If {
                test,
                cons: Box::new(Self::hoist_gen_vars(*cons, names)),
                alt: alt
                    .map(|a| Box::new(Self::hoist_gen_vars(*a, names))),
            },
            Stmt::While { test, body } => Stmt::While {
                test,
                body: Box::new(Self::hoist_gen_vars(*body, names)),
            },
            Stmt::DoWhile { body, test } => Stmt::DoWhile {
                body: Box::new(Self::hoist_gen_vars(*body, names)),
                test,
            },
            Stmt::For { init, test, update, body } => Stmt::For {
                init: init
                    .map(|i| Box::new(Self::hoist_gen_vars(*i, names))),
                test,
                update,
                body: Box::new(Self::hoist_gen_vars(*body, names)),
            },
            Stmt::ForIn { decl_kind, var, obj, body, of } => {
                if decl_kind.is_some() {
                    names.push(var.clone());
                }
                Stmt::ForIn {
                    decl_kind: None,
                    var,
                    obj,
                    body: Box::new(Self::hoist_gen_vars(*body, names)),
                    of,
                }
            }
            Stmt::Labeled { label, body } => Stmt::Labeled {
                label,
                body: Box::new(Self::hoist_gen_vars(*body, names)),
            },
            Stmt::Try { block, catch, finally } => Stmt::Try {
                block: block
                    .into_iter()
                    .map(|s| Self::hoist_gen_vars(s, names))
                    .collect(),
                catch: catch.map(|mut c| {
                    c.body = c
                        .body
                        .into_iter()
                        .map(|s| Self::hoist_gen_vars(s, names))
                        .collect();
                    c
                }),
                finally: finally.map(|f| {
                    f.into_iter()
                        .map(|s| Self::hoist_gen_vars(s, names))
                        .collect()
                }),
            },
            Stmt::Switch { disc, cases } => Stmt::Switch {
                disc,
                cases: cases
                    .into_iter()
                    .map(|mut c| {
                        c.body = c
                            .body
                            .into_iter()
                            .map(|s| Self::hoist_gen_vars(s, names))
                            .collect();
                        c
                    })
                    .collect(),
            },
            other => other,
        }
    }

    /// `function*` body -> resumable state machine. Yields must sit at
    /// statement level of the top-level body (`yield E;`,
    /// `x = yield E;`, `var x = yield E;`); a yield nested inside
    /// control flow is a clear error, never silently-wrong code.
    fn generator_transform(
        &mut self,
        body: Vec<Stmt>,
    ) -> Result<Vec<Stmt>, ParseError> {
        let mut names: Vec<String> = Vec::new();
        let body: Vec<Stmt> = body
            .into_iter()
            .map(|s| Self::hoist_gen_vars(s, &mut names))
            .collect();
        let st_name = self.fresh_tmp("gst");
        let sent = self.fresh_tmp("gsent");
        let done = self.fresh_tmp("gdone");
        let step = self.fresh_tmp("gstep");
        let it = self.fresh_tmp("git");
        let undef = || Expr::Ident("undefined".to_string());
        let iter_obj = |v: Expr, d: bool| {
            Expr::Object(vec![
                Prop {
                    key: PropKey::Ident("value".to_string()),
                    value: v,
                },
                Prop {
                    key: PropKey::Ident("done".to_string()),
                    value: Expr::Bool(d),
                },
            ])
        };
        let set_state = |name: &str, n: f64| {
            Stmt::Expr(Expr::Assign(
                AssignOp::Plain,
                Box::new(Expr::Ident(name.to_string())),
                Box::new(Expr::Num(n)),
            ))
        };
        // segmentation at canonical yields
        let mut segments: Vec<Vec<Stmt>> = Vec::new();
        let mut cur: Vec<Stmt> = Vec::new();
        for s in body {
            if let Some((target, value)) = Self::yield_shape(&s) {
                let next = (segments.len() + 1) as f64;
                cur.push(set_state(&st_name, next));
                cur.push(Stmt::Return(Some(iter_obj(value, false))));
                segments.push(std::mem::take(&mut cur));
                if let Some(t) = target {
                    cur.push(Stmt::Expr(Expr::Assign(
                        AssignOp::Plain,
                        Box::new(t),
                        Box::new(Expr::Ident(sent.clone())),
                    )));
                }
            } else if Self::stmt_has_marker(&s) {
                return Err(self.err(
                    "yield inside nested control flow not yet \
                     supported (statement-level yields only)",
                ));
            } else if let Stmt::Return(e) = s {
                cur.push(set_state(&st_name, -1.0));
                cur.push(Stmt::Return(Some(
                    iter_obj(e.unwrap_or_else(undef), true),
                )));
                segments.push(std::mem::take(&mut cur));
            } else {
                cur.push(s);
            }
        }
        cur.push(set_state(&st_name, -1.0));
        cur.push(Stmt::Return(Some(iter_obj(undef(), true))));
        segments.push(cur);
        let cases: Vec<SwitchCase> = segments
            .into_iter()
            .enumerate()
            .map(|(i, body)| SwitchCase {
                test: Some(Expr::Num(i as f64)),
                body,
            })
            .chain(std::iter::once(SwitchCase {
                test: None,
                body: vec![Stmt::Return(Some(iter_obj(undef(), true)))],
            }))
            .collect();
        // assembled wrapper
        let mut out: Vec<Stmt> = Vec::new();
        if !names.is_empty() {
            out.push(Stmt::VarDecl {
                kind: DeclKind::Var,
                decls: names.into_iter().map(|n| (n, None)).collect(),
            });
        }
        out.push(Stmt::VarDecl {
            kind: DeclKind::Var,
            decls: vec![
                (st_name.clone(), Some(Expr::Num(0.0))),
                (sent.clone(), None),
                (done.clone(), Some(Expr::Bool(false))),
            ],
        });
        out.push(Stmt::FuncDecl(Rc::new(FuncLit {
            name: Some(step.clone()),
            params: Vec::new(),
            body: vec![Stmt::Switch {
                disc: Expr::Ident(st_name.clone()),
                cases,
            }],
            is_async: false,
                lazy_body: None,
        })));
        let next_fn = FuncLit {
            name: None,
            params: vec!["__v".to_string()],
            body: vec![
                Stmt::If {
                    test: Expr::Ident(done.clone()),
                    cons: Box::new(Stmt::Return(Some(
                        iter_obj(undef(), true),
                    ))),
                    alt: None,
                },
                Stmt::Expr(Expr::Assign(
                    AssignOp::Plain,
                    Box::new(Expr::Ident(sent.clone())),
                    Box::new(Expr::Ident("__v".to_string())),
                )),
                Stmt::VarDecl {
                    kind: DeclKind::Var,
                    decls: vec![(
                        "__r".to_string(),
                        Some(Expr::Call {
                            callee: Box::new(Expr::Ident(step.clone())),
                            args: Vec::new(),
                            optional: false,
                        }),
                    )],
                },
                Stmt::If {
                    test: Expr::Member {
                        obj: Box::new(Expr::Ident("__r".to_string())),
                        prop: MemberProp::Static("done".to_string()),
                        optional: false,
                    },
                    cons: Box::new(Stmt::Expr(Expr::Assign(
                        AssignOp::Plain,
                        Box::new(Expr::Ident(done.clone())),
                        Box::new(Expr::Bool(true)),
                    ))),
                    alt: None,
                },
                Stmt::Return(Some(Expr::Ident("__r".to_string()))),
            ],
            is_async: false,
                lazy_body: None,
        };
        let ret_fn = FuncLit {
            name: None,
            params: vec!["__v".to_string()],
            body: vec![
                Stmt::Expr(Expr::Assign(
                    AssignOp::Plain,
                    Box::new(Expr::Ident(done.clone())),
                    Box::new(Expr::Bool(true)),
                )),
                Stmt::Return(Some(iter_obj(
                    Expr::Ident("__v".to_string()),
                    true,
                ))),
            ],
            is_async: false,
                lazy_body: None,
        };
        let throw_fn = FuncLit {
            name: None,
            params: vec!["__e".to_string()],
            body: vec![
                Stmt::Expr(Expr::Assign(
                    AssignOp::Plain,
                    Box::new(Expr::Ident(done.clone())),
                    Box::new(Expr::Bool(true)),
                )),
                Stmt::Throw(Expr::Ident("__e".to_string())),
            ],
            is_async: false,
                lazy_body: None,
        };
        out.push(Stmt::VarDecl {
            kind: DeclKind::Var,
            decls: vec![(
                it.clone(),
                Some(Expr::Object(vec![
                    Prop {
                        key: PropKey::Ident("next".to_string()),
                        value: Expr::Func(Rc::new(next_fn)),
                    },
                    Prop {
                        key: PropKey::Str("return".to_string()),
                        value: Expr::Func(Rc::new(ret_fn)),
                    },
                    Prop {
                        key: PropKey::Str("throw".to_string()),
                        value: Expr::Func(Rc::new(throw_fn)),
                    },
                ])),
            )],
        });
        // it['@@iterator'] = function () { return it; }
        out.push(Stmt::Expr(Expr::Assign(
            AssignOp::Plain,
            Box::new(Expr::Member {
                obj: Box::new(Expr::Ident(it.clone())),
                prop: MemberProp::Static("@@iterator".to_string()),
                optional: false,
            }),
            Box::new(Expr::Func(Rc::new(FuncLit {
                name: None,
                params: Vec::new(),
                body: vec![Stmt::Return(Some(Expr::Ident(it.clone())))],
                is_async: false,
                lazy_body: None,
            }))),
        )));
        out.push(Stmt::Return(Some(Expr::Ident(it))));
        Ok(out)
    }

    /// Lazy parsing: capture the upcoming `{...}` body as a token
    /// range without building an AST, collecting candidate free
    /// identifiers on the way. Returns None (position restored) for
    /// bodies too small to be worth a stub. Call with pos at `{`.
    fn try_lazy_body(
        &mut self,
    ) -> Result<Option<LazyTokens>, ParseError> {
        let brace = self.pos;
        self.expect_punct(P::LBrace)?;
        let start = self.pos;
        let mut depth = 1usize;
        let mut free = std::collections::HashSet::new();
        let mut prev_dot = false;
        loop {
            let t = &self.toks[self.pos];
            match &t.kind {
                Tok::Eof => {
                    return Err(self.err(
                        "unexpected end of input in function body",
                    ));
                }
                Tok::Punct(P::LBrace) => {
                    depth += 1;
                    prev_dot = false;
                }
                Tok::Punct(P::RBrace) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    prev_dot = false;
                }
                Tok::Punct(P::Dot | P::QuestionDot) => prev_dot = true,
                Tok::Ident(s) => {
                    // everything counts — even keyword-shaped names
                    // just resolve to nothing at deferral time
                    if !prev_dot {
                        free.insert(s.clone());
                    }
                    prev_dot = false;
                }
                Tok::Template(parts) => {
                    for p in parts {
                        if let TplElem::ExprSrc(hole) = p {
                            collect_words(hole, &mut free);
                        }
                    }
                    prev_dot = false;
                }
                _ => prev_dot = false,
            }
            self.pos += 1;
        }
        let end = self.pos;
        self.pos += 1; // the closing `}`
        if end - start < 24 {
            // tiny body: eager parse+compile beats stub bookkeeping
            self.pos = brace;
            return Ok(None);
        }
        let mut free_ids: Vec<String> = free.into_iter().collect();
        free_ids.sort(); // deterministic capture order
        Ok(Some(LazyTokens {
            toks: self.toks.clone(),
            start,
            end,
            free_ids,
        }))
    }

    /// Parses `(params) { body }` (name handled by the caller).
    fn func_lit(
        &mut self,
        name: Option<String>,
        is_async: bool,
    ) -> Result<FuncLit, ParseError> {
        self.func_lit_g(name, is_async, false)
    }

    fn func_lit_g(
        &mut self,
        name: Option<String>,
        is_async: bool,
        is_gen: bool,
    ) -> Result<FuncLit, ParseError> {
        let (params, prologue) = self.arrow_params()?;
        // lazy-parse eligible: no parse-time body rewrites pending
        // (async/generator desugar, destructured/default/rest param
        // prologue, class `super` rewriting)
        if !is_async && !is_gen && prologue.is_empty()
            && self.class_super.is_none() && self.at_punct(P::LBrace)
        {
            if let Some(lz) = self.try_lazy_body()? {
                return Ok(FuncLit {
                    name,
                    params,
                    body: Vec::new(),
                    is_async: false,
                    lazy_body: Some(lz),
                });
            }
        }
        let saved = self.in_async;
        self.in_async = is_async;
        let saved_gen = self.in_generator;
        self.in_generator = is_gen;
        // the for-head no-in restriction never crosses a function
        // boundary (`for(var B=function(){..."x" in y...};;)`)
        let saved_no_in = self.no_in;
        self.no_in = false;
        let body = self.block();
        self.no_in = saved_no_in;
        self.in_async = saved;
        self.in_generator = saved_gen;
        let mut body = body?;
        if !prologue.is_empty() {
            let mut full = prologue;
            full.append(&mut body);
            body = full;
        }
        if is_gen {
            body = self.generator_transform(body)?;
        }
        Ok(FuncLit { name, params, body, is_async, lazy_body: None })
    }
}

/// Identifier-shaped words in a template hole's raw source. Purely
/// conservative: property names and string contents get in too, which
/// only ever adds an unused capture candidate.
fn collect_words(
    src: &str,
    out: &mut std::collections::HashSet<String>,
) {
    let mut cur = String::new();
    for ch in src.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            cur.push(ch);
        } else if !cur.is_empty() {
            let w = std::mem::take(&mut cur);
            if !w.chars().next().unwrap().is_ascii_digit() {
                out.insert(w);
            }
        }
    }
    if !cur.is_empty() && !cur.chars().next().unwrap().is_ascii_digit() {
        out.insert(cur);
    }
}

fn is_keyword(s: &str) -> bool {
    matches!(s, "var" | "let" | "const" | "function" | "return" | "if"
        | "else" | "for" | "while" | "do" | "break" | "continue" | "new"
        | "delete" | "typeof" | "instanceof" | "in" | "of" | "this"
        | "true" | "false" | "null" | "class" | "extends" | "super"
        | "try" | "catch" | "finally" | "throw" | "switch" | "case"
        | "default" | "void" | "yield" | "async" | "await")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Vec<Stmt> {
        parse_program(src).unwrap()
    }

    fn expr(src: &str) -> Expr {
        let mut stmts = parse(src);
        assert_eq!(stmts.len(), 1, "want single statement: {src}");
        match stmts.pop().unwrap() {
            Stmt::Expr(e) => e,
            s => panic!("not an expression statement: {s:?}"),
        }
    }

    fn b(e: Expr) -> Box<Expr> {
        Box::new(e)
    }

    fn id(s: &str) -> Expr {
        Expr::Ident(s.to_string())
    }

    fn num(n: f64) -> Expr {
        Expr::Num(n)
    }

    #[test]
    fn precedence() {
        assert_eq!(
            expr("1 + 2 * 3"),
            Expr::Binary(
                BinOp::Add,
                b(num(1.0)),
                b(Expr::Binary(BinOp::Mul, b(num(2.0)), b(num(3.0)))),
            )
        );
        assert_eq!(
            expr("(1 + 2) * 3"),
            Expr::Binary(
                BinOp::Mul,
                b(Expr::Binary(BinOp::Add, b(num(1.0)), b(num(2.0)))),
                b(num(3.0)),
            )
        );
        // ** is right-associative
        assert_eq!(
            expr("2 ** 3 ** 2"),
            Expr::Binary(
                BinOp::Pow,
                b(num(2.0)),
                b(Expr::Binary(BinOp::Pow, b(num(3.0)), b(num(2.0)))),
            )
        );
        assert_eq!(
            expr("a || b && c"),
            Expr::Logical(
                LogOp::Or,
                b(id("a")),
                b(Expr::Logical(LogOp::And, b(id("b")), b(id("c")))),
            )
        );
    }

    #[test]
    fn assignment() {
        assert_eq!(
            expr("a = b = 1"),
            Expr::Assign(
                AssignOp::Plain,
                b(id("a")),
                b(Expr::Assign(AssignOp::Plain, b(id("b")), b(num(1.0)))),
            )
        );
        assert_eq!(
            expr("x += 2"),
            Expr::Assign(AssignOp::Bin(BinOp::Add), b(id("x")), b(num(2.0)))
        );
        assert!(parse_program("1 = 2").is_err());
    }

    #[test]
    fn ternary_nests_right() {
        assert_eq!(
            expr("a ? b : c ? d : e"),
            Expr::Cond(
                b(id("a")),
                b(id("b")),
                b(Expr::Cond(b(id("c")), b(id("d")), b(id("e")))),
            )
        );
    }

    #[test]
    fn member_call_chain() {
        // a.b(1)[c](2)
        let e = expr("a.b(1)[c](2)");
        let Expr::Call { callee, args, optional: false } = e else {
            panic!()
        };
        assert_eq!(args, vec![num(2.0)]);
        let Expr::Member { obj, prop: MemberProp::Computed(k), .. } = *callee
        else {
            panic!()
        };
        assert_eq!(*k, id("c"));
        let Expr::Call { callee, args, .. } = *obj else { panic!() };
        assert_eq!(args, vec![num(1.0)]);
        assert_eq!(
            *callee,
            Expr::Member {
                obj: b(id("a")),
                prop: MemberProp::Static("b".to_string()),
                optional: false,
            }
        );
    }

    #[test]
    fn optional_chaining() {
        let e = expr("a?.b?.(c)");
        let Expr::Call { callee, optional: true, .. } = e else { panic!() };
        let Expr::Member { optional: true, .. } = *callee else { panic!() };
    }

    #[test]
    fn arrows() {
        assert_eq!(
            expr("x => x + 1"),
            Expr::Arrow(Rc::new(FuncLit {
                name: None,
                params: vec!["x".to_string()],
                body: vec![Stmt::Return(Some(Expr::Binary(
                    BinOp::Add,
                    b(id("x")),
                    b(num(1.0)),
                )))],
                is_async: false,
                lazy_body: None,
            }))
        );
        let Expr::Arrow(f) = expr("(a, b) => { return a; }") else {
            panic!()
        };
        assert_eq!(f.params, vec!["a", "b"]);
        assert_eq!(f.body, vec![Stmt::Return(Some(id("a")))]);
        let Expr::Arrow(f) = expr("() => 0") else { panic!() };
        assert!(f.params.is_empty());
        // not an arrow: plain parenthesized expression still works
        assert_eq!(expr("(a)"), id("a"));
    }

    #[test]
    fn object_literal() {
        let e = expr("({ a: 1, b, c() { return 1; }, 'd-e': 2, [k]: 3 })");
        let Expr::Object(props) = e else { panic!() };
        assert_eq!(props.len(), 5);
        assert_eq!(props[0].key, PropKey::Ident("a".to_string()));
        assert_eq!(props[1].value, id("b"));
        assert!(matches!(props[2].value, Expr::Func(_)));
        assert_eq!(props[3].key, PropKey::Str("d-e".to_string()));
        assert!(matches!(props[4].key, PropKey::Computed(_)));
    }

    #[test]
    fn array_literal() {
        assert_eq!(
            expr("[1, 2, 3,]"),
            Expr::Array(vec![num(1.0), num(2.0), num(3.0)])
        );
    }

    #[test]
    fn control_flow() {
        // dangling else binds to the inner if
        let stmts = parse("if (a) if (b) c(); else d();");
        let Stmt::If { alt: None, cons, .. } = &stmts[0] else { panic!() };
        let Stmt::If { alt: Some(_), .. } = &**cons else { panic!() };

        let stmts = parse("for (var i = 0; i < 5; i++) f(i);");
        let Stmt::For { init: Some(init), test: Some(_), update: Some(_), .. } =
            &stmts[0]
        else {
            panic!()
        };
        assert!(matches!(&**init, Stmt::VarDecl { .. }));

        let stmts = parse("for (var k in obj) {} for (x of arr) {}");
        let Stmt::ForIn { of: false, decl_kind: Some(DeclKind::Var), .. } =
            &stmts[0]
        else {
            panic!()
        };
        let Stmt::ForIn { of: true, decl_kind: None, var, .. } = &stmts[1]
        else {
            panic!()
        };
        assert_eq!(var, "x");

        // `in` still works as an operator outside for-heads
        assert!(matches!(
            expr("'x' in o"),
            Expr::Binary(BinOp::In, _, _)
        ));

        let stmts = parse("do f(); while (x)");
        assert!(matches!(stmts[0], Stmt::DoWhile { .. }));
    }

    #[test]
    fn asi() {
        let stmts = parse("a\nb");
        assert_eq!(stmts.len(), 2);

        let stmts = parse("return\nx");
        assert_eq!(stmts[0], Stmt::Return(None));
        assert_eq!(stmts[1], Stmt::Expr(id("x")));

        // postfix ++ never crosses a line: `a\n++b` is a, then ++b
        let stmts = parse("a\n++b");
        assert_eq!(stmts.len(), 2);
        assert!(matches!(
            stmts[1],
            Stmt::Expr(Expr::Update { prefix: true, .. })
        ));

        assert!(parse_program("x = 1 y = 2").is_err());
    }

    #[test]
    fn try_throw_switch() {
        let stmts = parse(
            "try { f(); } catch (e) { g(e); } finally { h(); }",
        );
        let Stmt::Try { catch: Some(c), finally: Some(_), .. } = &stmts[0]
        else {
            panic!()
        };
        assert_eq!(c.param.as_deref(), Some("e"));

        let stmts = parse("switch (x) { case 1: f(); break; default: g(); }");
        let Stmt::Switch { cases, .. } = &stmts[0] else { panic!() };
        assert_eq!(cases.len(), 2);
        assert!(cases[0].test.is_some() && cases[1].test.is_none());

        assert!(matches!(parse("throw e;")[0], Stmt::Throw(_)));
    }

    #[test]
    fn template_holes_are_parsed() {
        let Expr::Template(parts) = expr("`a${1 + 2}b`") else { panic!() };
        assert_eq!(parts.len(), 3);
        let TplPart::Expr(e) = &parts[1] else { panic!() };
        assert!(matches!(**e, Expr::Binary(BinOp::Add, _, _)));
    }

    #[test]
    fn new_expressions() {
        // new binds the member chain, then the call args
        let e = expr("new Foo(1).bar()");
        let Expr::Call { callee, .. } = e else { panic!() };
        let Expr::Member { obj, .. } = *callee else { panic!() };
        let Expr::New { callee, args } = *obj else { panic!() };
        assert_eq!(*callee, id("Foo"));
        assert_eq!(args, vec![num(1.0)]);

        let Expr::New { args, .. } = expr("new Bar") else { panic!() };
        assert!(args.is_empty());
    }

    #[test]
    fn functions_and_unary() {
        let stmts = parse("function add(a, b) { return a + b; }");
        let Stmt::FuncDecl(f) = &stmts[0] else { panic!() };
        assert_eq!(f.name.as_deref(), Some("add"));
        assert_eq!(f.params, vec!["a", "b"]);

        assert!(matches!(
            expr("typeof x === 'string'"),
            Expr::Binary(BinOp::StrictEq, _, _)
        ));
        assert_eq!(
            expr("delete a.b"),
            Expr::Unary(
                UnOp::Delete,
                b(Expr::Member {
                    obj: b(id("a")),
                    prop: MemberProp::Static("b".to_string()),
                    optional: false,
                }),
            )
        );
        assert!(matches!(
            expr("i++"),
            Expr::Update { prefix: false, op: UpdateOp::Inc, .. }
        ));
        assert!(matches!(
            expr("--i"),
            Expr::Update { prefix: true, op: UpdateOp::Dec, .. }
        ));
    }

    #[test]
    fn parses_our_benchmark_files() {
        for f in ["../bench/js/engine_bench.js", "../bench/js/dom_bench.js"] {
            let src = std::fs::read_to_string(f).unwrap();
            let stmts = parse_program(&src).unwrap();
            assert!(stmts.len() > 5, "{f}: {} stmts", stmts.len());
        }
    }

    #[test]
    fn sequence_and_var_lists() {
        assert!(matches!(expr("a, b, c"), Expr::Seq(v) if v.len() == 3));
        let stmts = parse("var a = 1, b, c = 2;");
        let Stmt::VarDecl { decls, .. } = &stmts[0] else { panic!() };
        assert_eq!(decls.len(), 3);
        assert!(decls[1].1.is_none());
    }
}
