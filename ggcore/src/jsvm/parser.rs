//! Recursive-descent parser with precedence climbing for expressions.
//!
//! Handles the JS grammar quirks that matter for real pages: ASI
//! (automatic semicolon insertion) from the lexer's newline flags,
//! arrow-function lookahead, the `in`-operator / for-in ambiguity,
//! optional chaining, and template holes (re-lexed recursively).

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
    let mut p = Parser::new(tokenize(src)?);
    let mut out = Vec::new();
    while !p.at_eof() {
        out.push(p.stmt()?);
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Token>,
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
}

enum OpKind {
    Bin(BinOp),
    Log(LogOp),
}

impl Parser {
    fn new(toks: Vec<Token>) -> Parser {
        Parser {
            toks, pos: 0, no_in: false, in_async: false, tmp_n: 0,
            last_pattern_len: 0,
        }
    }

    fn fresh_tmp(&mut self, prefix: &str) -> String {
        self.tmp_n += 1;
        format!("__{}{}", prefix, self.tmp_n)
    }

    /// Parse a class body into a constructor function. Methods attach as
    /// own properties (`this.m = function(){...}`), so method calls bind
    /// `this` to the instance. `extends`/`super`/static/get/set are not
    /// supported yet.
    fn class_lit(
        &mut self,
        name: Option<String>,
    ) -> Result<FuncLit, ParseError> {
        if self.eat_ident("extends") {
            return Err(self.err("class extends not supported yet"));
        }
        self.expect_punct(P::LBrace)?;
        let mut ctor_params: Vec<String> = Vec::new();
        let mut ctor_prologue: Vec<Stmt> = Vec::new();
        let mut method_stmts: Vec<Stmt> = Vec::new();
        let mut ctor_body: Vec<Stmt> = Vec::new();
        while !self.eat_punct(P::RBrace) {
            if self.eat_punct(P::Semi) {
                continue;
            }
            if matches!(self.kind(), Tok::Ident(k) if k == "static") {
                return Err(self.err("static class members not supported yet"));
            }
            let mname = self.expect_ident()?;
            let (params, prologue) = self.arrow_params()?;
            let saved = self.in_async;
            self.in_async = false;
            let body = self.block();
            self.in_async = saved;
            let mut body = body?;
            if !prologue.is_empty() {
                let mut full = prologue;
                full.append(&mut body);
                body = full;
            }
            if mname == "constructor" {
                ctor_params = params;
                ctor_body = body;
            } else {
                // this.mname = function(params){ body };
                method_stmts.push(Stmt::Expr(Expr::Assign(
                    AssignOp::Plain,
                    Box::new(Expr::Member {
                        obj: Box::new(Expr::This),
                        prop: MemberProp::Static(mname.clone()),
                        optional: false,
                    }),
                    Box::new(Expr::Func(Box::new(FuncLit {
                        name: Some(mname),
                        params,
                        body,
                        is_async: false,
                    }))),
                )));
            }
        }
        // ctor body = attach methods first, then run constructor()
        ctor_prologue.append(&mut method_stmts);
        ctor_prologue.append(&mut ctor_body);
        Ok(FuncLit {
            name,
            params: ctor_params,
            body: ctor_prologue,
            is_async: false,
        })
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
                    let name = self.expect_ident()?;
                    let f = self.func_lit(Some(name), false)?;
                    Ok(Stmt::FuncDecl(Box::new(f)))
                }
                "async" if matches!(self.kind_at(1), Some(Tok::Ident(k))
                    if k == "function") => {
                    self.pos += 2; // async function
                    let name = self.expect_ident()?;
                    let f = self.func_lit(Some(name), true)?;
                    Ok(Stmt::FuncDecl(Box::new(f)))
                }
                "class" => {
                    self.pos += 1;
                    let name = self.expect_ident()?;
                    let f = self.class_lit(Some(name))?;
                    Ok(Stmt::FuncDecl(Box::new(f)))
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
        while !self.at_punct(P::RBrace) {
            let key = self.expect_ident()?;
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
        self.expect_punct(P::LParen)?;

        // for (var x in/of obj)
        if matches!(self.kind(), Tok::Ident(k)
            if matches!(k.as_str(), "var" | "let" | "const"))
        {
            let kw = self.expect_ident()?;
            let kind = Parser::decl_kind(&kw);
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
                        return Ok(Expr::Func(Box::new(
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
            return Err(self.err("invalid assignment target"));
        }
        self.pos += 1;
        let right = self.assign_expr()?;
        Ok(Expr::Assign(op, Box::new(left), Box::new(right)))
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
                    return Err(self.err(
                        "rest parameters (...args) not yet supported",
                    ));
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
        Ok(Expr::Arrow(Box::new(FuncLit {
            name: None,
            params,
            body,
            is_async,
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
                        let args = self.arguments()?;
                        e = Expr::Call {
                            callee: Box::new(e), args, optional: true,
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
                            let mut sub = Parser::new(tokenize(&src)?);
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
                    let name = match self.kind() {
                        Tok::Ident(n) if !self.at_punct(P::LParen) => {
                            let n = n.clone();
                            self.pos += 1;
                            Some(n)
                        }
                        _ => None,
                    };
                    Ok(Expr::Func(Box::new(self.func_lit(name, false)?)))
                }
                "new" => {
                    let callee = self.member_only_expr()?;
                    let args = if self.eat_punct(P::LParen) {
                        self.arguments()?
                    } else {
                        Vec::new()
                    };
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
                    Ok(Expr::Func(Box::new(self.class_lit(cname)?)))
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
        while !self.eat_punct(P::RBrace) {
            let prop = match self.bump() {
                Tok::Ident(name) => {
                    if self.at_punct(P::LParen) {
                        // method shorthand: { foo() { ... } }
                        let f = self.func_lit(Some(name.clone()), false)?;
                        Prop {
                            key: PropKey::Ident(name),
                            value: Expr::Func(Box::new(f)),
                        }
                    } else if self.eat_punct(P::Colon) {
                        Prop {
                            key: PropKey::Ident(name),
                            value: self.assign_expr()?,
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
                    self.expect_punct(P::Colon)?;
                    Prop { key: PropKey::Str(s), value: self.assign_expr()? }
                }
                Tok::Num(n) => {
                    self.expect_punct(P::Colon)?;
                    Prop { key: PropKey::Num(n), value: self.assign_expr()? }
                }
                Tok::Punct(P::LBracket) => {
                    let k = self.assign_expr()?;
                    self.expect_punct(P::RBracket)?;
                    self.expect_punct(P::Colon)?;
                    Prop {
                        key: PropKey::Computed(k),
                        value: self.assign_expr()?,
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
        Ok(Expr::Object(props))
    }

    /// Parses `(params) { body }` (name handled by the caller).
    fn func_lit(
        &mut self,
        name: Option<String>,
        is_async: bool,
    ) -> Result<FuncLit, ParseError> {
        let (params, prologue) = self.arrow_params()?;
        let saved = self.in_async;
        self.in_async = is_async;
        // the for-head no-in restriction never crosses a function
        // boundary (`for(var B=function(){..."x" in y...};;)`)
        let saved_no_in = self.no_in;
        self.no_in = false;
        let body = self.block();
        self.no_in = saved_no_in;
        self.in_async = saved;
        let mut body = body?;
        if !prologue.is_empty() {
            let mut full = prologue;
            full.append(&mut body);
            body = full;
        }
        Ok(FuncLit { name, params, body, is_async })
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
            Expr::Arrow(Box::new(FuncLit {
                name: None,
                params: vec!["x".to_string()],
                body: vec![Stmt::Return(Some(Expr::Binary(
                    BinOp::Add,
                    b(id("x")),
                    b(num(1.0)),
                )))],
                is_async: false,
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
