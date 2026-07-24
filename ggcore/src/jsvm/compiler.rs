//! AST -> register bytecode.
//!
//! Scope model: script-level `var`/`function` (and top-level `let`/
//! `const`, so later scripts on the page still see them) are globals;
//! function locals live in registers. `let`/`const` bind to the
//! innermost block through a per-function stack of lexical scopes and
//! shadow outer bindings. A local that any nested function references
//! is promoted to a heap cell (CellWrap) — function-scoped vars at
//! entry, block lets at block entry, so each pass through a block
//! makes a fresh cell — and closures capture cell references. A
//! `for (let ...)` loop re-wraps its captured variables' cells at the
//! continue target: each iteration's closures see that iteration's
//! value. Assigning to a `const` is a compile error; reading a
//! `let`/`const` before its declaration trips a runtime TDZ check
//! (not enforced across `switch` fall-through).
//!
//! Exceptions: `try` arms a VM handler per protected region (one for
//! the catch, one for the finally); `finally` bodies are compiled
//! inline on every exit path — normal fall-through, the exception
//! re-throw path, and each `return`/`break`/`continue` that jumps out
//! (so a `return` in a finally overrides the pending completion, like
//! real JS).
//!
//! Not yet: `??`, optional chaining.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::ast::*;
use super::bytecode::{CapSrc, FuncProto, Instr, LazySrc, Module};
use super::value::Value;

pub struct CompileError {
    pub msg: String,
}

impl std::fmt::Debug for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compile error: {}", self.msg)
    }
}

/// Compile a deferred function body (first call). Produces a fresh
/// single-root Module whose `main` is the compiled body; the VM loads
/// it and redirects calls of the stub proto to it. Nested function
/// literals inside the body defer again, so deep bundles stay lazy.
pub fn compile_lazy(src: &LazySrc) -> Result<Module, CompileError> {
    let mut c = Compiler {
        module: Module {
            protos: Vec::new(),
            atoms: Vec::new(),
            strings: Vec::new(),
            main: 0,
            n_ics: 0,
        },
        atom_map: HashMap::new(),
        string_map: HashMap::new(),
        fns: Vec::new(),
        const_globals: HashSet::new(),
    };
    let lit = if let Some(lz) = &src.lit.lazy_body {
        // lazy-parsed: build the body AST now, from the original
        // token stream
        let body = super::parser::parse_lazy_body(lz)
            .map_err(|e| CompileError { msg: format!("{e:?}") })?;
        Rc::new(FuncLit {
            name: src.lit.name.clone(),
            params: src.lit.params.clone(),
            body,
            // the deferred stub keeps the original async flag — dropping
            // it here skipped await normalization for every lazily
            // compiled async function
            is_async: src.lit.is_async,
            lazy_body: None,
        })
    } else {
        src.lit.clone()
    };
    let idx = c.compile_func_now(&lit, src.is_arrow, Some(src))?;
    c.module.main = idx;
    Ok(c.module)
}

pub fn compile(stmts: &[Stmt]) -> Result<Module, CompileError> {
    // `new` compiles directly to Construct; keeping construction as one
    // VM operation is required for Proxy [[Construct]] semantics.
    let mut c = Compiler {
        module: Module {
            protos: Vec::new(),
            atoms: Vec::new(),
            strings: Vec::new(),
            main: 0,
            n_ics: 0,
        },
        atom_map: HashMap::new(),
        string_map: HashMap::new(),
        fns: Vec::new(),
        const_globals: HashSet::new(),
    };
    // Main proto: register 0 holds the last expression-statement value
    // so eval() can return it.
    let mut f = FnCtx::new("<main>".to_string(), 0, true);
    f.locals_end = 1;
    f.tmp_top = 1;
    f.nregs = 1;
    // block-scoped let/const in the script need cells when captured
    f.captured = captured_names(stmts);
    f.emit(Instr::LoadUndef { dst: 0 });
    c.fns.push(f);
    // Function declarations are hoisted: create them before running the
    // body so `foo(); function foo(){}` and mutual recursion work.
    c.hoist_func_decls(stmts)?;
    // Top-level `var` names are hoisted as defined-with-undefined so
    // `var X = X || {}` (Naver pc.veta NBP_CORP) reads undefined
    // instead of throwing ReferenceError. DeclGlobal keeps any value
    // an earlier script already set.
    let mut top_vars = Vec::new();
    hoist(stmts, &mut top_vars);
    let mut seen = HashSet::new();
    for name in top_vars {
        if seen.insert(name.clone()) {
            let a = c.atom(&name);
            c.fx().emit(Instr::DeclGlobal { atom: a });
        }
    }
    for s in stmts {
        c.stmt(s)?;
        let f = c.fx();
        f.tmp_top = f.locals_end;
    }
    c.fx().emit(Instr::Return { src: 0 });
    let main_ctx = c.fns.pop().unwrap();
    let main = c.push_proto(main_ctx);
    c.module.main = main;
    Ok(c.module)
}

struct Compiler {
    module: Module,
    atom_map: HashMap<String, u16>,
    string_map: HashMap<String, u32>,
    /// Enclosing-function stack; last = the one being compiled.
    fns: Vec<FnCtx>,
    /// Script-level `const` names (they compile to globals so later
    /// scripts see them); assignment within this script is rejected.
    const_globals: HashSet<String>,
}

/// How a name was declared — drives const and TDZ rules.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BindKind {
    /// `var`, parameters, hoisted function declarations
    Var,
    Let,
    Const,
}

/// One declared variable of the function being compiled.
#[derive(Clone, Copy)]
struct Binding {
    reg: u8,
    /// the register holds a heap-cell reference (captured by a closure)
    is_cell: bool,
    kind: BindKind,
    /// let/const: false until the declaration statement has been
    /// compiled; reads/writes emitted while false get a TdzCheck
    initialized: bool,
}

/// A lexical scope. scopes[0] is the function scope (params, `var`s,
/// hoisted functions, top-of-body `let`/`const`); each block, lexical
/// for-loop head, and switch body pushes another. Registers a scope
/// allocated are released when it closes.
struct Scope {
    bindings: HashMap<String, Binding>,
    /// locals_end to restore when the scope closes
    prev_locals_end: u8,
    /// lexicals this scope spilled to the %spillN object (register
    /// exhaustion); removed from the spill map when the scope closes
    spilled_here: Vec<String>,
}

struct FnCtx {
    name: String,
    nparams: u8,
    is_main: bool,
    code: Vec<Instr>,
    consts: Vec<Value>,
    scopes: Vec<Scope>,
    /// names any function nested in this one references (name-based,
    /// so conservative under shadowing): bindings with these names
    /// live in heap cells so closures can alias them
    captured: HashSet<String>,
    /// the body references `arguments` (prologue materializes it)
    uses_arguments: bool,
    /// unique binding name of this function's spill object, once made
    spill_name: Option<String>,
    /// overflow local -> property atom on the %spillN object
    spilled: HashMap<String, u16>,
    /// lazy-session root only: free names that live in an *enclosing*
    /// (already-compiled) function's spill object, recorded at deferral
    /// time — name -> that spill object's %spillN upval name
    lazy_spills: HashMap<String, String>,
    /// captures of *this* function from its parent, in upvalue order
    upvals: Vec<CapSrc>,
    upval_map: HashMap<String, u16>,
    /// registers below this are locals; temps start here
    locals_end: u8,
    tmp_top: u8,
    nregs: u8,
    /// one per enclosing `break`-able construct (loop or switch)
    loops: Vec<LoopCtx>,
    /// a `label:` seen just before the next loop pushes its LoopCtx
    pending_label: Option<String>,
    /// one per enclosing `try` construct (see TryCtx)
    trys: Vec<TryCtx>,
    /// function declarations already created at function entry (hoisted),
    /// so the in-place statement is a no-op — see hoist_func_decls
    hoisted_fns: HashSet<String>,
    /// arrow function: captures `this` lexically (proto.is_arrow)
    is_arrow: bool,
}

/// A break/continue target scope. `switch` catches `break` but is
/// transparent to `continue` (which belongs to the enclosing loop).
/// `trys_depth` is fx.trys.len() at entry: a break/continue must run
/// the finallies (and disarm the handlers) of every `try` entered
/// since.
struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    is_switch: bool,
    trys_depth: usize,
    /// the label a `label: for(...)` gave this loop, if any
    label: Option<String>,
}

impl LoopCtx {
    fn loop_(trys_depth: usize) -> LoopCtx {
        LoopCtx {
            breaks: Vec::new(),
            continues: Vec::new(),
            is_switch: false,
            trys_depth,
            label: None,
        }
    }
    fn switch(trys_depth: usize) -> LoopCtx {
        LoopCtx {
            breaks: Vec::new(),
            continues: Vec::new(),
            is_switch: true,
            trys_depth,
            label: None,
        }
    }
}

/// An enclosing `try` while compiling inside it: how many handlers it
/// currently has armed at this point in the code (a jump out must
/// disarm them) and its `finally` body (inlined at each early exit).
#[derive(Clone)]
struct TryCtx {
    handlers: u8,
    finally: Option<Rc<Vec<Stmt>>>,
}

impl FnCtx {
    fn new(name: String, nparams: u8, is_main: bool) -> FnCtx {
        FnCtx {
            name,
            nparams,
            is_main,
            code: Vec::new(),
            consts: Vec::new(),
            scopes: vec![Scope {
                bindings: HashMap::new(),
                prev_locals_end: nparams,
                spilled_here: Vec::new(),
            }],
            captured: HashSet::new(),
            uses_arguments: false,
            spill_name: None,
            spilled: HashMap::new(),
            lazy_spills: HashMap::new(),
            upvals: Vec::new(),
            upval_map: HashMap::new(),
            locals_end: nparams,
            tmp_top: nparams,
            nregs: nparams,
            loops: Vec::new(),
            pending_label: None,
            trys: Vec::new(),
            hoisted_fns: HashSet::new(),
            is_arrow: false,
        }
    }

    /// The visible binding for `name`, innermost scope first.
    fn lookup(&self, name: &str) -> Option<Binding> {
        self.scopes
            .iter()
            .rev()
            .find_map(|s| s.bindings.get(name))
            .copied()
    }

    fn lookup_mut(&mut self, name: &str) -> Option<&mut Binding> {
        self.scopes
            .iter_mut()
            .rev()
            .find_map(|s| s.bindings.get_mut(name))
    }

    fn push_scope(&mut self) {
        self.scopes.push(Scope {
            bindings: HashMap::new(),
            prev_locals_end: self.locals_end,
            spilled_here: Vec::new(),
        });
    }

    fn pop_scope(&mut self) {
        let s = self.scopes.pop().unwrap();
        self.locals_end = s.prev_locals_end;
        self.tmp_top = self.locals_end;
        for n in &s.spilled_here {
            self.spilled.remove(n);
        }
    }

    /// Reserve the next local register for a binding in the innermost
    /// scope.
    fn declare(
        &mut self,
        name: &str,
        kind: BindKind,
        initialized: bool,
    ) -> Result<u8, CompileError> {
        if self.locals_end >= 250 {
            return Err(CompileError {
                msg: format!("too many locals in {}", self.name),
            });
        }
        let r = self.locals_end;
        self.locals_end += 1;
        if self.tmp_top < self.locals_end {
            self.tmp_top = self.locals_end;
        }
        if self.nregs < self.locals_end {
            self.nregs = self.locals_end;
        }
        self.scopes.last_mut().unwrap().bindings.insert(
            name.to_string(),
            Binding { reg: r, is_cell: false, kind, initialized },
        );
        Ok(r)
    }

    fn emit(&mut self, i: Instr) -> usize {
        self.code.push(i);
        self.code.len() - 1
    }

    fn alloc(&mut self) -> Result<u8, CompileError> {
        if self.tmp_top >= 250 {
            return Err(CompileError {
                msg: format!(
                    "expression too deep in {} (locals={}, tmp={}, \
                     code={}, last={:?})",
                    self.name,
                    self.locals_end,
                    self.tmp_top,
                    self.code.len(),
                    &self.code[self.code.len().saturating_sub(6)..]
                ),
            });
        }
        let r = self.tmp_top;
        self.tmp_top += 1;
        if self.tmp_top > self.nregs {
            self.nregs = self.tmp_top;
        }
        Ok(r)
    }

    fn const_idx(&mut self, v: Value) -> u16 {
        if let Some(i) = self.consts.iter().position(|c| *c == v) {
            return i as u16;
        }
        self.consts.push(v);
        (self.consts.len() - 1) as u16
    }

    fn patch(&mut self, site: usize) {
        let target = self.code.len() as u32;
        self.patch_to(site, target);
    }

    fn patch_to(&mut self, site: usize, target: u32) {
        match &mut self.code[site] {
            Instr::Jump { target: t }
            | Instr::JumpIfFalse { target: t, .. }
            | Instr::JumpIfTrue { target: t, .. }
            | Instr::JumpIfNullish { target: t, .. }
            | Instr::JumpIfNotNullish { target: t, .. }
            | Instr::PushHandler { catch_ip: t, .. } => *t = target,
            _ => unreachable!("patching a non-jump"),
        }
    }
}

/// Where a name lives, from the perspective of the current function.
#[derive(Clone, Copy)]
enum Place {
    Reg(u8),
    Cell(u8),
    Up(u16),
    Global(u16),
    /// Overflow local spilled into a hidden %spillN object (always
    /// cell-wrapped so closures can capture the whole spill area):
    /// (cell register, property atom) in the current function.
    SpillCell(u8, u16),
    /// Same, but the spill object lives in an enclosing function:
    /// (upvalue index, property atom).
    SpillUp(u16, u16),
}

/// Collect function-scoped names (`var`, `function`, `var` for-heads)
/// without descending into nested functions. `let`/`const` are block-
/// scoped and excluded — the compiler binds them where they appear.
/// Fresh unique binding name for a function's hidden spill object.
/// `%` keeps it out of reach of any source-level identifier.
fn next_spill_name() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SPILL_SEQ: AtomicUsize = AtomicUsize::new(0);
    format!("%spill{}", SPILL_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn hoist(stmts: &[Stmt], out: &mut Vec<String>) {
    for s in stmts {
        match s {
            Stmt::VarDecl { kind: DeclKind::Var, decls } => {
                out.extend(decls.iter().map(|(n, _)| n.clone()));
            }
            Stmt::FuncDecl(f) => out.extend(f.name.clone()),
            Stmt::Block(b) => hoist(b, out),
            Stmt::Labeled { body, .. } => {
                hoist(std::slice::from_ref(body), out)
            }
            Stmt::If { cons, alt, .. } => {
                hoist(std::slice::from_ref(cons), out);
                if let Some(alt) = alt {
                    hoist(std::slice::from_ref(alt), out);
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                hoist(std::slice::from_ref(body), out);
            }
            Stmt::For { init, body, .. } => {
                if let Some(init) = init {
                    hoist(std::slice::from_ref(init), out);
                }
                hoist(std::slice::from_ref(body), out);
            }
            Stmt::ForIn { var, body, decl_kind, .. } => {
                if *decl_kind == Some(DeclKind::Var) {
                    out.push(var.clone());
                }
                hoist(std::slice::from_ref(body), out);
            }
            Stmt::Try { block, catch, finally } => {
                hoist(block, out);
                if let Some(c) = catch {
                    hoist(&c.body, out);
                }
                if let Some(f) = finally {
                    hoist(f, out);
                }
            }
            Stmt::Switch { cases, .. } => {
                for c in cases {
                    hoist(&c.body, out);
                }
            }
            _ => {}
        }
    }
}

// ---- free-variable analysis (shallow: stops at nested functions) ----

fn scan_stmt<'a>(
    s: &'a Stmt,
    ids: &mut HashSet<String>,
    lits: &mut Vec<&'a FuncLit>,
) {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => scan_expr(e, ids, lits),
        Stmt::VarDecl { decls, .. } => {
            for (_, init) in decls {
                if let Some(init) = init {
                    scan_expr(init, ids, lits);
                }
            }
        }
        Stmt::FuncDecl(f) => lits.push(f),
        Stmt::Return(Some(e)) => scan_expr(e, ids, lits),
        Stmt::Return(None) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty => {}
        Stmt::If { test, cons, alt } => {
            scan_expr(test, ids, lits);
            scan_stmt(cons, ids, lits);
            if let Some(alt) = alt {
                scan_stmt(alt, ids, lits);
            }
        }
        Stmt::While { test, body } | Stmt::DoWhile { body, test } => {
            scan_expr(test, ids, lits);
            scan_stmt(body, ids, lits);
        }
        Stmt::With { obj, body } => {
            scan_expr(obj, ids, lits);
            scan_stmt(body, ids, lits);
        }
        Stmt::For { init, test, update, body } => {
            if let Some(init) = init {
                scan_stmt(init, ids, lits);
            }
            if let Some(test) = test {
                scan_expr(test, ids, lits);
            }
            if let Some(update) = update {
                scan_expr(update, ids, lits);
            }
            scan_stmt(body, ids, lits);
        }
        Stmt::ForIn { var, obj, body, decl_kind, .. } => {
            if decl_kind.is_none() {
                ids.insert(var.clone());
            }
            scan_expr(obj, ids, lits);
            scan_stmt(body, ids, lits);
        }
        Stmt::Block(body) => {
            for s in body {
                scan_stmt(s, ids, lits);
            }
        }
        Stmt::Labeled { body, .. } => scan_stmt(body, ids, lits),
        Stmt::Try { block, catch, finally } => {
            for s in block {
                scan_stmt(s, ids, lits);
            }
            if let Some(c) = catch {
                for s in &c.body {
                    scan_stmt(s, ids, lits);
                }
            }
            if let Some(f) = finally {
                for s in f {
                    scan_stmt(s, ids, lits);
                }
            }
        }
        Stmt::Switch { disc, cases } => {
            scan_expr(disc, ids, lits);
            for c in cases {
                if let Some(t) = &c.test {
                    scan_expr(t, ids, lits);
                }
                for s in &c.body {
                    scan_stmt(s, ids, lits);
                }
            }
        }
    }
}

fn scan_expr<'a>(
    e: &'a Expr,
    ids: &mut HashSet<String>,
    lits: &mut Vec<&'a FuncLit>,
) {
    match e {
        Expr::Ident(n) => {
            ids.insert(n.clone());
        }
        Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null
        | Expr::This | Expr::Regex { .. } => {}
        Expr::Template(parts) => {
            for p in parts {
                if let TplPart::Expr(e) = p {
                    scan_expr(e, ids, lits);
                }
            }
        }
        Expr::Array(items) => {
            for e in items {
                scan_expr(e, ids, lits);
            }
        }
        Expr::Object(props) => {
            for p in props {
                if let PropKey::Computed(k) = &p.key {
                    scan_expr(k, ids, lits);
                }
                scan_expr(&p.value, ids, lits);
            }
        }
        Expr::Func(f) | Expr::Arrow(f) => lits.push(f),
        Expr::Unary(_, e) | Expr::Await(e) => scan_expr(e, ids, lits),
        Expr::Update { target, .. } => scan_expr(target, ids, lits),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b) => {
            scan_expr(a, ids, lits);
            scan_expr(b, ids, lits);
        }
        Expr::Cond(a, b, c) => {
            scan_expr(a, ids, lits);
            scan_expr(b, ids, lits);
            scan_expr(c, ids, lits);
        }
        Expr::Assign(_, a, b) => {
            scan_expr(a, ids, lits);
            scan_expr(b, ids, lits);
        }
        Expr::Member { obj, prop, .. } => {
            scan_expr(obj, ids, lits);
            if let MemberProp::Computed(k) = prop {
                scan_expr(k, ids, lits);
            }
        }
        Expr::Call { callee, args, .. } => {
            scan_expr(callee, ids, lits);
            for a in args {
                scan_expr(a, ids, lits);
            }
        }
        Expr::New { callee, args } => {
            scan_expr(callee, ids, lits);
            for a in args {
                scan_expr(a, ids, lits);
            }
        }
        Expr::Seq(items) => {
            for e in items {
                scan_expr(e, ids, lits);
            }
        }
    }
}

/// `let`/`const` names declared directly in this statement list (they
/// bind in the block that owns the list, not in nested blocks).
fn lexical_names(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        if let Stmt::VarDecl {
            kind: DeclKind::Let | DeclKind::Const,
            decls,
        } = s
        {
            out.extend(decls.iter().map(|(n, _)| n.clone()));
        }
    }
}

/// Names a function references that it does not bind itself
/// (conservatively includes what its nested functions reference).
fn free_vars(lit: &FuncLit) -> HashSet<String> {
    if let Some(lz) = &lit.lazy_body {
        // lazy-parsed body: token-level over-approximation (locals and
        // even keyword-shaped words included). Names that aren't real
        // enclosing bindings resolve to Global at deferral and cost
        // nothing; a missed real reference would mis-bind, so the scan
        // errs wide.
        let mut ids: HashSet<String> =
            lz.free_ids.iter().cloned().collect();
        for p in &lit.params {
            ids.remove(p);
        }
        return ids;
    }
    let mut ids = HashSet::new();
    let mut lits = Vec::new();
    for s in &lit.body {
        scan_stmt(s, &mut ids, &mut lits);
    }
    for nested in lits {
        ids.extend(free_vars(nested));
    }
    let mut bound: HashSet<String> = lit.params.iter().cloned().collect();
    let mut hoisted = Vec::new();
    hoist(&lit.body, &mut hoisted);
    bound.extend(hoisted);
    // Top-of-body let/const shadow for the whole function. Deeper-block
    // lets are deliberately ignored here: their references count as
    // free, which at worst promotes an identically named enclosing
    // binding to a cell it didn't need — harmless, never the reverse.
    lexical_names(&lit.body, &mut bound);
    ids.retain(|n| !bound.contains(n));
    ids
}

/// Names referenced by any function nested somewhere in `body`.
/// Bindings with these names must live in heap cells so closures can
/// alias them.
fn captured_names(body: &[Stmt]) -> HashSet<String> {
    let mut ids = HashSet::new();
    let mut nested = Vec::new();
    for s in body {
        scan_stmt(s, &mut ids, &mut nested);
    }
    let mut captured = HashSet::new();
    for l in nested {
        captured.extend(free_vars(l));
    }
    captured
}

/// Might evaluating `e` write to a variable or object (assign, update,
/// or a call that could)? Used to decide when an earlier operand held
/// in a local register must be snapshotted before a later operand runs.
fn has_side_effects(e: &Expr) -> bool {
    match e {
        Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null
        | Expr::This | Expr::Ident(_) | Expr::Regex { .. }
        | Expr::Func(_) | Expr::Arrow(_) => false,
        Expr::Assign(..) | Expr::Update { .. } | Expr::Call { .. }
        | Expr::New { .. } | Expr::Await(_) => true,
        Expr::Unary(_, a) => has_side_effects(a),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b) => {
            has_side_effects(a) || has_side_effects(b)
        }
        Expr::Cond(a, b, c) => {
            has_side_effects(a) || has_side_effects(b) || has_side_effects(c)
        }
        Expr::Member { obj, prop, .. } => {
            has_side_effects(obj)
                || matches!(prop, MemberProp::Computed(k)
                    if has_side_effects(k))
        }
        Expr::Seq(items) | Expr::Array(items) => {
            items.iter().any(has_side_effects)
        }
        Expr::Object(props) => props.iter().any(|p| has_side_effects(&p.value)),
        Expr::Template(parts) => parts.iter().any(|p| matches!(
            p, TplPart::Expr(e) if has_side_effects(e))),
    }
}

/// Rewrites every `new Ctor(args)` in the tree into
/// `(function(){ var o={}; Ctor.call(o, args); return o; })()`. The
/// ctor runs with `this`=o (via .call), so `this.x=` and attached
/// methods land on the instance. A ctor that returns an object is
/// (rarely) not honored — o always wins. `new Promise(fn)` stays an
/// Expr::New: it has a dedicated instruction (see Expr::New codegen).
/// `n` numbers the generated temps so nested `new`s don't collide.
fn lower_new_expr(e: &mut Expr, n: &mut usize) {
    // children first, so `new Foo(new Bar())` lowers inside-out
    match e {
        Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null
        | Expr::This | Expr::Ident(_) | Expr::Regex { .. } => {}
        Expr::Template(parts) => {
            for p in parts {
                if let TplPart::Expr(x) = p {
                    lower_new_expr(x, n);
                }
            }
        }
        Expr::Array(items) | Expr::Seq(items) => {
            for x in items {
                lower_new_expr(x, n);
            }
        }
        Expr::Object(props) => {
            for p in props {
                if let PropKey::Computed(k) = &mut p.key {
                    lower_new_expr(k, n);
                }
                lower_new_expr(&mut p.value, n);
            }
        }
        Expr::Func(f) | Expr::Arrow(f) => {
            for s in &mut Rc::make_mut(f).body {
                lower_new_stmt(s, n);
            }
        }
        Expr::Unary(_, a) | Expr::Await(a)
        | Expr::Update { target: a, .. } => lower_new_expr(a, n),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b)
        | Expr::Assign(_, a, b) => {
            lower_new_expr(a, n);
            lower_new_expr(b, n);
        }
        Expr::Cond(a, b, c) => {
            lower_new_expr(a, n);
            lower_new_expr(b, n);
            lower_new_expr(c, n);
        }
        Expr::Member { obj, prop, .. } => {
            lower_new_expr(obj, n);
            if let MemberProp::Computed(k) = prop {
                lower_new_expr(k, n);
            }
        }
        Expr::Call { callee, args, .. } | Expr::New { callee, args } => {
            lower_new_expr(callee, n);
            for a in args {
                lower_new_expr(a, n);
            }
        }
    }
    let plain_new = matches!(
        e,
        Expr::New { callee, .. }
            if !matches!(&**callee, Expr::Ident(name) if name == "Promise")
    );
    if !plain_new {
        return;
    }
    let Expr::New { callee, args } = std::mem::replace(e, Expr::Null)
    else {
        unreachable!()
    };
    *n += 1;
    let o = format!("__new{n}");
    let mut call_args = vec![Expr::Ident(o.clone())];
    call_args.extend(args);
    let body = vec![
        Stmt::VarDecl {
            kind: DeclKind::Var,
            decls: vec![(o.clone(), Some(Expr::Object(Vec::new())))],
        },
        Stmt::Expr(Expr::Call {
            callee: Box::new(Expr::Member {
                obj: callee,
                prop: MemberProp::Static("call".to_string()),
                optional: false,
            }),
            args: call_args,
            optional: false,
        }),
        Stmt::Return(Some(Expr::Ident(o))),
    ];
    let iife = Expr::Func(Rc::new(FuncLit {
        name: None, params: Vec::new(), body, is_async: false,
        lazy_body: None,
    }));
    *e = Expr::Call {
        callee: Box::new(iife), args: Vec::new(), optional: false,
    };
}

fn lower_new_stmt(s: &mut Stmt, n: &mut usize) {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) | Stmt::Return(Some(e)) => {
            lower_new_expr(e, n);
        }
        Stmt::Return(None) | Stmt::Break(_) | Stmt::Continue(_)
        | Stmt::Empty => {}
        Stmt::Labeled { body, .. } => lower_new_stmt(body, n),
        Stmt::VarDecl { decls, .. } => {
            for (_, init) in decls {
                if let Some(e) = init {
                    lower_new_expr(e, n);
                }
            }
        }
        Stmt::FuncDecl(f) => {
            for s in &mut Rc::make_mut(f).body {
                lower_new_stmt(s, n);
            }
        }
        Stmt::If { test, cons, alt } => {
            lower_new_expr(test, n);
            lower_new_stmt(cons, n);
            if let Some(a) = alt {
                lower_new_stmt(a, n);
            }
        }
        Stmt::While { test, body } | Stmt::DoWhile { body, test } => {
            lower_new_expr(test, n);
            lower_new_stmt(body, n);
        }
        Stmt::With { obj, body } => {
            lower_new_expr(obj, n);
            lower_new_stmt(body, n);
        }
        Stmt::For { init, test, update, body } => {
            if let Some(i) = init {
                lower_new_stmt(i, n);
            }
            if let Some(t) = test {
                lower_new_expr(t, n);
            }
            if let Some(u) = update {
                lower_new_expr(u, n);
            }
            lower_new_stmt(body, n);
        }
        Stmt::ForIn { obj, body, .. } => {
            lower_new_expr(obj, n);
            lower_new_stmt(body, n);
        }
        Stmt::Block(stmts) => {
            for s in stmts {
                lower_new_stmt(s, n);
            }
        }
        Stmt::Try { block, catch, finally } => {
            for s in block {
                lower_new_stmt(s, n);
            }
            if let Some(c) = catch {
                for s in &mut c.body {
                    lower_new_stmt(s, n);
                }
            }
            if let Some(f) = finally {
                for s in f {
                    lower_new_stmt(s, n);
                }
            }
        }
        Stmt::Switch { disc, cases } => {
            lower_new_expr(disc, n);
            for c in cases {
                if let Some(t) = &mut c.test {
                    lower_new_expr(t, n);
                }
                for s in &mut c.body {
                    lower_new_stmt(s, n);
                }
            }
        }
    }
}

// ---- async/await desugar (P3c) ----
// Rewrite an async function body into a synchronous one returning a
// Promise, lifting top-level `await`s into `.then` continuations. Awaits
// anywhere else are left in place and error at codegen (safe, not silent).

fn ast_ident(s: &str) -> Expr {
    Expr::Ident(s.to_string())
}
fn ast_member(obj: Expr, name: &str) -> Expr {
    Expr::Member {
        obj: Box::new(obj),
        prop: MemberProp::Static(name.to_string()),
        optional: false,
    }
}
fn ast_call(callee: Expr, args: Vec<Expr>) -> Expr {
    Expr::Call { callee: Box::new(callee), args, optional: false }
}
fn ast_arrow(params: Vec<String>, body: Vec<Stmt>) -> Expr {
    Expr::Arrow(Rc::new(FuncLit {
        name: None, params, body, is_async: false,
        lazy_body: None,
    }))
}

// --- await normalization ------------------------------------------------
// Before chaining, every `await` is hoisted so it only ever appears as
// `var __awN = await E;` with E itself await-free. This makes awaits in
// expression position (`x = await f() + 1`, `if (await ok())`, call
// arguments, ...) reduce to the shapes chain_async knows. Short-circuit
// operands and ternary arms are hoisted too, which evaluates them
// unconditionally — an approximation, but strictly better than the
// compile error these produced before.

fn expr_has_await(e: &Expr) -> bool {
    match e {
        Expr::Await(_) => true,
        Expr::Func(_) | Expr::Arrow(_) => false, // their own context
        Expr::Unary(_, a) => expr_has_await(a),
        Expr::Update { target, .. } => expr_has_await(target),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b) => {
            expr_has_await(a) || expr_has_await(b)
        }
        Expr::Assign(_, a, b) => expr_has_await(a) || expr_has_await(b),
        Expr::Cond(c, a, b) => {
            expr_has_await(c) || expr_has_await(a) || expr_has_await(b)
        }
        Expr::Member { obj, prop, .. } => {
            expr_has_await(obj)
                || matches!(prop, MemberProp::Computed(k)
                    if expr_has_await(k))
        }
        Expr::Call { callee, args, .. } | Expr::New { callee, args } => {
            expr_has_await(callee) || args.iter().any(expr_has_await)
        }
        Expr::Array(v) | Expr::Seq(v) => v.iter().any(expr_has_await),
        Expr::Object(props) => props.iter().any(|p| {
            expr_has_await(&p.value)
                || matches!(&p.key, PropKey::Computed(k)
                    if expr_has_await(k))
        }),
        Expr::Template(parts) => parts.iter().any(|p| {
            matches!(p, TplPart::Expr(e) if expr_has_await(e))
        }),
        _ => false,
    }
}

fn stmt_has_await(s: &Stmt) -> bool {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => expr_has_await(e),
        Stmt::VarDecl { decls, .. } => decls
            .iter()
            .any(|(_, init)| init.as_ref().is_some_and(expr_has_await)),
        Stmt::Return(e) => e.as_ref().is_some_and(expr_has_await),
        Stmt::If { test, cons, alt } => {
            expr_has_await(test)
                || stmt_has_await(cons)
                || alt.as_deref().is_some_and(stmt_has_await)
        }
        Stmt::While { test, body } => {
            expr_has_await(test) || stmt_has_await(body)
        }
        Stmt::DoWhile { body, test } => {
            expr_has_await(test) || stmt_has_await(body)
        }
        Stmt::For { init, test, update, body } => {
            init.as_deref().is_some_and(stmt_has_await)
                || test.as_ref().is_some_and(expr_has_await)
                || update.as_ref().is_some_and(expr_has_await)
                || stmt_has_await(body)
        }
        Stmt::ForIn { obj, body, .. } => {
            expr_has_await(obj) || stmt_has_await(body)
        }
        Stmt::Block(v) => v.iter().any(stmt_has_await),
        Stmt::Labeled { body, .. } => stmt_has_await(body),
        Stmt::Try { block, catch, finally } => {
            block.iter().any(stmt_has_await)
                || catch
                    .as_ref()
                    .is_some_and(|c| c.body.iter().any(stmt_has_await))
                || finally
                    .as_ref()
                    .is_some_and(|f| f.iter().any(stmt_has_await))
        }
        Stmt::Switch { disc, cases } => {
            expr_has_await(disc)
                || cases
                    .iter()
                    .any(|c| c.body.iter().any(stmt_has_await))
        }
        _ => false,
    }
}

/// Rewrite `e`, hoisting every await into `pre` as
/// `var __awN = await X;` (evaluation order), leaving temp reads.
fn hoist_expr(e: Expr, pre: &mut Vec<Stmt>, n: &mut usize) -> Expr {
    if !expr_has_await(&e) {
        return e;
    }
    match e {
        Expr::Await(inner) => {
            let inner = hoist_expr(*inner, pre, n);
            *n += 1;
            let name = format!("__aw{n}");
            pre.push(Stmt::VarDecl {
                kind: DeclKind::Var,
                decls: vec![(
                    name.clone(),
                    Some(Expr::Await(Box::new(inner))),
                )],
            });
            Expr::Ident(name)
        }
        Expr::Unary(op, a) => {
            Expr::Unary(op, Box::new(hoist_expr(*a, pre, n)))
        }
        Expr::Update { op, prefix, target } => Expr::Update {
            op,
            prefix,
            target: Box::new(hoist_expr(*target, pre, n)),
        },
        Expr::Binary(op, a, b) => {
            let a = hoist_expr(*a, pre, n);
            let b = hoist_expr(*b, pre, n);
            Expr::Binary(op, Box::new(a), Box::new(b))
        }
        Expr::Logical(op, a, b) => {
            let a = hoist_expr(*a, pre, n);
            let b = hoist_expr(*b, pre, n);
            Expr::Logical(op, Box::new(a), Box::new(b))
        }
        Expr::Assign(op, t, v) => {
            let t = hoist_expr(*t, pre, n);
            let v = hoist_expr(*v, pre, n);
            Expr::Assign(op, Box::new(t), Box::new(v))
        }
        Expr::Cond(c, a, b) => {
            let c = hoist_expr(*c, pre, n);
            let a = hoist_expr(*a, pre, n);
            let b = hoist_expr(*b, pre, n);
            Expr::Cond(Box::new(c), Box::new(a), Box::new(b))
        }
        Expr::Member { obj, prop, optional } => Expr::Member {
            obj: Box::new(hoist_expr(*obj, pre, n)),
            prop: match prop {
                MemberProp::Computed(k) => MemberProp::Computed(Box::new(
                    hoist_expr(*k, pre, n),
                )),
                p => p,
            },
            optional,
        },
        Expr::Call { callee, args, optional } => Expr::Call {
            callee: Box::new(hoist_expr(*callee, pre, n)),
            args: args
                .into_iter()
                .map(|a| hoist_expr(a, pre, n))
                .collect(),
            optional,
        },
        Expr::New { callee, args } => Expr::New {
            callee: Box::new(hoist_expr(*callee, pre, n)),
            args: args
                .into_iter()
                .map(|a| hoist_expr(a, pre, n))
                .collect(),
        },
        Expr::Array(v) => Expr::Array(
            v.into_iter().map(|a| hoist_expr(a, pre, n)).collect(),
        ),
        Expr::Seq(v) => Expr::Seq(
            v.into_iter().map(|a| hoist_expr(a, pre, n)).collect(),
        ),
        Expr::Object(props) => Expr::Object(
            props
                .into_iter()
                .map(|p| Prop {
                    key: match p.key {
                        PropKey::Computed(k) => {
                            PropKey::Computed(hoist_expr(k, pre, n))
                        }
                        k => k,
                    },
                    value: hoist_expr(p.value, pre, n),
                })
                .collect(),
        ),
        Expr::Template(parts) => Expr::Template(
            parts
                .into_iter()
                .map(|p| match p {
                    TplPart::Expr(e) => {
                        TplPart::Expr(Box::new(hoist_expr(*e, pre, n)))
                    }
                    c => c,
                })
                .collect(),
        ),
        other => other,
    }
}

fn boxed_block(v: Vec<Stmt>) -> Box<Stmt> {
    Box::new(Stmt::Block(v))
}

/// Normalize one statement (see module comment); returns replacement
/// statements.
fn normalize_stmt(s: Stmt, n: &mut usize) -> Vec<Stmt> {
    if !stmt_has_await(&s) {
        return vec![s];
    }
    let mut pre = Vec::new();
    match s {
        Stmt::Expr(e) => {
            let e = hoist_expr(e, &mut pre, n);
            pre.push(Stmt::Expr(e));
            pre
        }
        Stmt::Throw(e) => {
            let e = hoist_expr(e, &mut pre, n);
            pre.push(Stmt::Throw(e));
            pre
        }
        Stmt::Return(Some(e)) => {
            let e = hoist_expr(e, &mut pre, n);
            pre.push(Stmt::Return(Some(e)));
            pre
        }
        Stmt::VarDecl { kind, decls } => {
            let mut out = Vec::new();
            for (name, init) in decls {
                match init {
                    Some(Expr::Await(inner)) => {
                        // already the canonical shape; hoist only inside
                        let mut p2 = Vec::new();
                        let inner = hoist_expr(*inner, &mut p2, n);
                        out.append(&mut p2);
                        out.push(Stmt::VarDecl {
                            kind,
                            decls: vec![(
                                name,
                                Some(Expr::Await(Box::new(inner))),
                            )],
                        });
                    }
                    Some(init) => {
                        let mut p2 = Vec::new();
                        let init = hoist_expr(init, &mut p2, n);
                        out.append(&mut p2);
                        out.push(Stmt::VarDecl {
                            kind,
                            decls: vec![(name, Some(init))],
                        });
                    }
                    None => out.push(Stmt::VarDecl {
                        kind,
                        decls: vec![(name, None)],
                    }),
                }
            }
            out
        }
        Stmt::If { test, cons, alt } => {
            let test = hoist_expr(test, &mut pre, n);
            let cons = boxed_block(normalize_stmt(*cons, n));
            let alt =
                alt.map(|a| boxed_block(normalize_stmt(*a, n)));
            pre.push(Stmt::If { test, cons, alt });
            pre
        }
        // Loops keep their awaits (in the test or the body) — the
        // chain pass rewrites whole loops into recursive promise
        // chains; only the body statements get normalized here.
        Stmt::While { test, body } => {
            let body_v = normalize_stmt(*body, n);
            vec![Stmt::While { test, body: boxed_block(body_v) }]
        }
        Stmt::DoWhile { body, test } => {
            let body_v = normalize_stmt(*body, n);
            vec![Stmt::DoWhile { body: boxed_block(body_v), test }]
        }
        Stmt::For { init, test, update, body } => {
            let mut out = Vec::new();
            if let Some(i) = init {
                out.extend(normalize_stmt(*i, n));
            }
            let body_v = normalize_stmt(*body, n);
            out.push(Stmt::For {
                init: None,
                test,
                update,
                body: boxed_block(body_v),
            });
            out
        }
        Stmt::ForIn { decl_kind, var, obj, body, of } => {
            let obj = hoist_expr(obj, &mut pre, n);
            let body = boxed_block(normalize_stmt(*body, n));
            pre.push(Stmt::ForIn { decl_kind, var, obj, body, of });
            pre
        }
        Stmt::Block(v) => {
            vec![Stmt::Block(normalize_body(v, n))]
        }
        Stmt::Labeled { label, body } => {
            let body = boxed_block(normalize_stmt(*body, n));
            vec![Stmt::Labeled { label, body }]
        }
        Stmt::Try { block, catch, finally } => {
            let block = normalize_body(block, n);
            let catch = catch.map(|mut c| {
                c.body = normalize_body(c.body, n);
                c
            });
            let finally = finally.map(|f| normalize_body(f, n));
            vec![Stmt::Try { block, catch, finally }]
        }
        Stmt::Switch { disc, cases } => {
            // chain_async can't lift awaits out of a switch. When every
            // case is fallthrough-free, lower the whole switch to an
            // if/else-if chain over a once-evaluated discriminant — the
            // chainer already handles If. Otherwise keep the switch
            // (awaits inside will still error, same as before).
            if cases.iter().any(|c| c.body.iter().any(stmt_has_await)) {
                if let Some(lowered) =
                    switch_to_if_chain(disc.clone(), &cases, n)
                {
                    for s in lowered {
                        pre.extend(normalize_stmt(s, n));
                    }
                    return pre;
                }
            }
            let disc = hoist_expr(disc, &mut pre, n);
            let cases = cases
                .into_iter()
                .map(|mut c| {
                    c.body = normalize_body(c.body, n);
                    c
                })
                .collect();
            pre.push(Stmt::Switch { disc, cases });
            pre
        }
        other => vec![other],
    }
}

/// `switch` -> `var __swN = disc; if (__swN === t1) B1 else if ... else
/// BD` when every case body is fallthrough-free: empty, or ending in an
/// unconditional break/return/throw, with no OTHER unlabeled break that
/// would have exited the switch (an unlabeled continue targets the
/// enclosing loop either way and needs no rewrite). Returns None when
/// any case needs real fallthrough semantics.
fn switch_to_if_chain(
    disc: Expr,
    cases: &[SwitchCase],
    n: &mut usize,
) -> Option<Vec<Stmt>> {
    // an unlabeled break at switch level (outside nested loops/switches)
    fn has_switch_break(stmts: &[Stmt]) -> bool {
        fn walk(s: &Stmt) -> bool {
            match s {
                Stmt::Break(None) => true,
                Stmt::If { cons, alt, .. } => {
                    walk(cons)
                        || alt.as_deref().is_some_and(walk)
                }
                Stmt::Block(v) => v.iter().any(walk),
                Stmt::Labeled { body, .. } => walk(body),
                Stmt::Try { block, catch, finally } => {
                    block.iter().any(walk)
                        || catch
                            .as_ref()
                            .is_some_and(|c| c.body.iter().any(walk))
                        || finally
                            .as_ref()
                            .is_some_and(|f| f.iter().any(walk))
                }
                // a nested loop/switch owns its own breaks
                _ => false,
            }
        }
        stmts.iter().any(walk)
    }
    // `case X: { ...; break; }` nests the terminator inside a block —
    // look through trailing blocks for both the check and the strip
    fn ends_unconditionally(stmts: &[Stmt]) -> bool {
        match stmts.last() {
            None
            | Some(Stmt::Break(None))
            | Some(Stmt::Return(_))
            | Some(Stmt::Throw(_)) => true,
            Some(Stmt::Block(v)) => ends_unconditionally(v),
            _ => false,
        }
    }
    fn strip_trailing_break(stmts: &mut Vec<Stmt>) {
        match stmts.last_mut() {
            Some(Stmt::Break(None)) => {
                stmts.pop();
            }
            Some(Stmt::Block(v)) => strip_trailing_break(v),
            _ => {}
        }
    }
    let mut bodies: Vec<(Option<Expr>, Vec<Stmt>)> = Vec::new();
    for (i, c) in cases.iter().enumerate() {
        let mut body = c.body.clone();
        // empty non-terminal case WITHOUT its own break shares the next
        // body (`case A: case B: {...}`): allow the empty-fallthrough
        // idiom by chaining into the following case's condition
        if body.is_empty() && i + 1 < cases.len() {
            bodies.push((c.test.clone(), Vec::new()));
            continue;
        }
        if !ends_unconditionally(&body) && i + 1 < cases.len() {
            return None; // real fallthrough into the next case
        }
        strip_trailing_break(&mut body);
        if has_switch_break(&body) {
            return None; // a conditional switch-exit we can't express
        }
        bodies.push((c.test.clone(), body));
    }
    *n += 1;
    let dv = format!("__sw{n}");
    let mut out = vec![Stmt::VarDecl {
        kind: DeclKind::Var,
        decls: vec![(dv.clone(), Some(disc))],
    }];
    // merge empty cases into the next non-empty one (OR of tests)
    let mut merged: Vec<(Vec<Expr>, Vec<Stmt>)> = Vec::new();
    let mut pending: Vec<Expr> = Vec::new();
    let mut default_body: Option<Vec<Stmt>> = None;
    for (test, body) in bodies {
        match test {
            Some(t) => {
                pending.push(t);
                if !body.is_empty() || pending.is_empty() {
                    merged.push((std::mem::take(&mut pending), body));
                }
            }
            None => {
                // default: captured separately (order-independent
                // since nothing falls through)
                if body.is_empty() && default_body.is_none() {
                    default_body = Some(Vec::new());
                } else {
                    default_body = Some(body);
                }
            }
        }
    }
    if !pending.is_empty() {
        // trailing empty labels with no body: nothing to run
        pending.clear();
    }
    let mut chain: Option<Stmt> = default_body.map(Stmt::Block);
    for (tests, body) in merged.into_iter().rev() {
        let mut cond: Option<Expr> = None;
        for t in tests {
            let cmp = Expr::Binary(
                BinOp::StrictEq,
                Box::new(ast_ident(&dv)),
                Box::new(t),
            );
            cond = Some(match cond {
                None => cmp,
                Some(prev) => Expr::Logical(
                    LogOp::Or,
                    Box::new(prev),
                    Box::new(cmp),
                ),
            });
        }
        let Some(cond) = cond else { continue };
        chain = Some(Stmt::If {
            test: cond,
            cons: Box::new(Stmt::Block(body)),
            alt: chain.map(Box::new),
        });
    }
    if let Some(c) = chain {
        out.push(c);
    }
    Some(out)
}

fn normalize_body(body: Vec<Stmt>, n: &mut usize) -> Vec<Stmt> {
    body.into_iter()
        .flat_map(|s| normalize_stmt(s, n))
        .collect()
}

/// A statement chain_async can lift: does it or its nested control
/// flow contain a statement-level await (post-normalization)?
fn chainable_await_inside(s: &Stmt) -> bool {
    stmt_has_await(s)
}

/// Statements of a block (or the single statement itself).
fn block_stmts(s: &Stmt) -> Vec<Stmt> {
    match s {
        Stmt::Block(v) => v.clone(),
        other => vec![other.clone()],
    }
}

/// Statement kinds that stop an IIFE wrap. `RET` = return (allowed
/// when the wrapped construct is the function's last statement — the
/// value then propagates through the promise chain); `BRK` =
/// break/continue (never wrappable: they can't cross a function
/// boundary).
fn exit_kinds(stmts: &[Stmt]) -> (bool, bool) {
    let mut ret = false;
    let mut brk = false;
    fn walk(s: &Stmt, ret: &mut bool, brk: &mut bool) {
        match s {
            Stmt::Return(_) => *ret = true,
            Stmt::Break(_) | Stmt::Continue(_) => *brk = true,
            Stmt::If { cons, alt, .. } => {
                walk(cons, ret, brk);
                if let Some(a) = alt {
                    walk(a, ret, brk);
                }
            }
            Stmt::Block(v) => v.iter().for_each(|s| walk(s, ret, brk)),
            Stmt::Labeled { body, .. } => walk(body, ret, brk),
            Stmt::Try { block, catch, finally } => {
                block.iter().for_each(|s| walk(s, ret, brk));
                if let Some(c) = catch {
                    c.body.iter().for_each(|s| walk(s, ret, brk));
                }
                if let Some(f) = finally {
                    f.iter().for_each(|s| walk(s, ret, brk));
                }
            }
            // break/continue inside a nested loop/switch bind to that
            // construct, not to the wrap — don't flag them
            Stmt::While { .. } | Stmt::DoWhile { .. }
            | Stmt::For { .. } | Stmt::ForIn { .. }
            | Stmt::Switch { .. } => {
                // returns still escape a nested loop
                fn ret_only(s: &Stmt, ret: &mut bool) {
                    let mut b2 = false;
                    walk(s, ret, &mut b2);
                }
                match s {
                    Stmt::While { body, .. }
                    | Stmt::DoWhile { body, .. }
                    | Stmt::For { body, .. }
                    | Stmt::ForIn { body, .. } => ret_only(body, ret),
                    Stmt::Switch { cases, .. } => cases
                        .iter()
                        .flat_map(|c| c.body.iter())
                        .for_each(|s| ret_only(s, ret)),
                    _ => unreachable!(),
                }
            }
            _ => {}
        }
    }
    stmts.iter().for_each(|s| walk(s, &mut ret, &mut brk));
    (ret, brk)
}

/// Branches that early-exit can't be IIFE-wrapped (loop bodies).
fn has_early_exit(stmts: &[Stmt]) -> bool {
    let (ret, brk) = exit_kinds(stmts);
    ret || brk
}

enum AwaitStmt {
    Bind(String, Expr), // var x = await E
    Discard(Expr),      // await E;
    Ret(Expr),          // return await E
}

fn stmt_await(s: &Stmt) -> Option<AwaitStmt> {
    match s {
        Stmt::VarDecl { decls, .. } if decls.len() == 1 => match &decls[0] {
            (name, Some(Expr::Await(e))) => {
                Some(AwaitStmt::Bind(name.clone(), (**e).clone()))
            }
            _ => None,
        },
        Stmt::Expr(Expr::Await(e)) => Some(AwaitStmt::Discard((**e).clone())),
        Stmt::Return(Some(Expr::Await(e))) => {
            Some(AwaitStmt::Ret((**e).clone()))
        }
        _ => None,
    }
}

/// `Promise.resolve(e)`
fn ast_presolve(e: Expr) -> Expr {
    ast_call(ast_member(ast_ident("Promise"), "resolve"), vec![e])
}

/// `(() => { body })()`
fn ast_iife(body: Vec<Stmt>) -> Expr {
    ast_call(ast_arrow(Vec::new(), body), vec![])
}

fn chain_async(stmts: &[Stmt], n: &mut usize) -> Vec<Stmt> {
    let j = stmts.iter().position(|s| {
        stmt_await(s).is_some() || chainable_await_inside(s)
    });
    let Some(j) = j else {
        return stmts.to_vec(); // all synchronous
    };
    let mut out: Vec<Stmt> = stmts[..j].to_vec();
    if let Some(aw) = stmt_await(&stmts[j]) {
        match aw {
            // return await E == return E (the outer .then adopts it)
            AwaitStmt::Ret(e) => out.push(Stmt::Return(Some(e))),
            AwaitStmt::Bind(name, e) => {
                let rest = chain_async(&stmts[j + 1..], n);
                let cont = ast_arrow(vec![name], rest);
                out.push(Stmt::Return(Some(
                    ast_call(ast_member(e, "then"), vec![cont]),
                )));
            }
            AwaitStmt::Discard(e) => {
                let rest = chain_async(&stmts[j + 1..], n);
                let cont = ast_arrow(vec!["__await".to_string()], rest);
                out.push(Stmt::Return(Some(
                    ast_call(ast_member(e, "then"), vec![cont]),
                )));
            }
        }
        return out;
    }
    // control flow containing awaits (post-normalization)
    match &stmts[j] {
        // a bare block: splice its statements into the stream
        Stmt::Block(v) => {
            let mut merged = v.clone();
            merged.extend_from_slice(&stmts[j + 1..]);
            out.extend(chain_async(&merged, n));
        }
        // if with awaited branch(es): each branch becomes an async
        // IIFE; rest continues after whichever promise it returns.
        // Branches that early-exit (return/break/continue) can't be
        // wrapped — leave the statement alone (codegen will report
        // the await) rather than silently change semantics.
        Stmt::If { test, cons, alt } => {
            let cons_v = block_stmts(cons);
            let alt_v = alt.as_deref().map(block_stmts);
            let rest_stmts = &stmts[j + 1..];
            let (ret, brk) = {
                let (r1, b1) = exit_kinds(&cons_v);
                let (r2, b2) = alt_v
                    .as_deref()
                    .map(exit_kinds)
                    .unwrap_or((false, false));
                (r1 || r2, b1 || b2)
            };
            // returns are fine when nothing follows: the branch value
            // becomes the function's promise result
            if brk || (ret && !rest_stmts.is_empty()) {
                out.push(stmts[j].clone());
                out.extend(chain_async(rest_stmts, n));
                return out;
            }
            let a = ast_iife(chain_async(&cons_v, n));
            let b = match alt_v {
                Some(av) => ast_iife(chain_async(&av, n)),
                None => ast_ident("undefined"),
            };
            let sel =
                Expr::Cond(Box::new(test.clone()), Box::new(a), Box::new(b));
            if rest_stmts.is_empty() {
                out.push(Stmt::Return(Some(ast_presolve(sel))));
            } else {
                let rest = chain_async(rest_stmts, n);
                out.push(Stmt::Return(Some(ast_call(
                    ast_member(ast_presolve(sel), "then"),
                    vec![ast_arrow(Vec::new(), rest)],
                ))));
            }
        }
        // while/for with awaits: a recursive promise chain
        // `var __loop = function(){ if(!t) return;
        //    return Promise.resolve(IIFE(body)).then(__loop); }`
        Stmt::While { test, body } | Stmt::For { test: Some(test),
            update: None, init: None, body } => {
            let body_v = block_stmts(body);
            if has_early_exit(&body_v) {
                out.push(stmts[j].clone());
                out.extend(chain_async(&stmts[j + 1..], n));
                return out;
            }
            *n += 1;
            let loop_name = format!("__loop{n}");
            let mut lf: Vec<Stmt> = Vec::new();
            let mut t = test.clone();
            if expr_has_await(&t) {
                t = hoist_expr(t, &mut lf, n);
            }
            lf.push(Stmt::If {
                test: Expr::Unary(UnOp::Not, Box::new(t)),
                cons: Box::new(Stmt::Return(None)),
                alt: None,
            });
            lf.push(Stmt::Return(Some(ast_call(
                ast_member(
                    ast_presolve(ast_iife(chain_async(&body_v, n))),
                    "then",
                ),
                vec![ast_ident(&loop_name)],
            ))));
            let lf = chain_async(&lf, n);
            out.push(Stmt::VarDecl {
                kind: DeclKind::Var,
                decls: vec![(
                    loop_name.clone(),
                    // an arrow so `this` inside the loop body is the
                    // enclosing method's receiver (naver: `for (…) await
                    // this.displayAd(e)`); a regular fn would rebind it
                    Some(Expr::Arrow(Rc::new(FuncLit {
                        name: None,
                        params: Vec::new(),
                        body: lf,
                        is_async: false,
                        lazy_body: None,
                    }))),
                )],
            });
            let rest = chain_async(&stmts[j + 1..], n);
            out.push(Stmt::Return(Some(ast_call(
                ast_member(
                    ast_presolve(ast_call(ast_ident(&loop_name), vec![])),
                    "then",
                ),
                vec![ast_arrow(Vec::new(), rest)],
            ))));
        }
        // general for: like while, with the update run before recursing
        Stmt::For { init, test, update, body } => {
            let body_v = block_stmts(body);
            if has_early_exit(&body_v) {
                out.push(stmts[j].clone());
                out.extend(chain_async(&stmts[j + 1..], n));
                return out;
            }
            if let Some(i) = init {
                out.push((**i).clone());
            }
            *n += 1;
            let loop_name = format!("__loop{n}");
            let mut lf: Vec<Stmt> = Vec::new();
            if let Some(t0) = test {
                let mut t = t0.clone();
                if expr_has_await(&t) {
                    t = hoist_expr(t, &mut lf, n);
                }
                lf.push(Stmt::If {
                    test: Expr::Unary(UnOp::Not, Box::new(t)),
                    cons: Box::new(Stmt::Return(None)),
                    alt: None,
                });
            }
            // then-continuation: update, then recurse
            let mut cont: Vec<Stmt> = Vec::new();
            if let Some(u) = update {
                let mut u2 = u.clone();
                let mut upre = Vec::new();
                if expr_has_await(&u2) {
                    u2 = hoist_expr(u2, &mut upre, n);
                }
                cont.extend(upre);
                cont.push(Stmt::Expr(u2));
            }
            cont.push(Stmt::Return(Some(ast_call(
                ast_ident(&loop_name),
                vec![],
            ))));
            let cont = chain_async(&cont, n);
            lf.push(Stmt::Return(Some(ast_call(
                ast_member(
                    ast_presolve(ast_iife(chain_async(&body_v, n))),
                    "then",
                ),
                vec![ast_arrow(Vec::new(), cont)],
            ))));
            let lf = chain_async(&lf, n);
            out.push(Stmt::VarDecl {
                kind: DeclKind::Var,
                decls: vec![(
                    loop_name.clone(),
                    // an arrow so `this` inside the loop body is the
                    // enclosing method's receiver (naver: `for (…) await
                    // this.displayAd(e)`); a regular fn would rebind it
                    Some(Expr::Arrow(Rc::new(FuncLit {
                        name: None,
                        params: Vec::new(),
                        body: lf,
                        is_async: false,
                        lazy_body: None,
                    }))),
                )],
            });
            let rest = chain_async(&stmts[j + 1..], n);
            out.push(Stmt::Return(Some(ast_call(
                ast_member(
                    ast_presolve(ast_call(ast_ident(&loop_name), vec![])),
                    "then",
                ),
                vec![ast_arrow(Vec::new(), rest)],
            ))));
        }
        // try/catch with awaits: promise rejection carries the error
        // `Promise.resolve(IIFE(block)).catch(e => IIFE(handler))
        //    .then(() => rest)`; finally chains as an extra .then-both
        Stmt::Try { block, catch, finally } => {
            let rest_stmts = &stmts[j + 1..];
            let (ret, brk) = {
                let (mut r, mut b) = exit_kinds(block);
                if let Some(c) = catch {
                    let (r2, b2) = exit_kinds(&c.body);
                    r |= r2;
                    b |= b2;
                }
                if let Some(f) = finally {
                    let (r2, b2) = exit_kinds(f);
                    r |= r2;
                    b |= b2;
                }
                (r, b)
            };
            if brk || (ret && !rest_stmts.is_empty()) {
                out.push(stmts[j].clone());
                out.extend(chain_async(rest_stmts, n));
                return out;
            }
            let mut chain =
                ast_presolve(ast_iife(chain_async(block, n)));
            if let Some(c) = catch {
                let param = c
                    .param
                    .clone()
                    .unwrap_or_else(|| "__err".to_string());
                chain = ast_call(
                    ast_member(chain, "catch"),
                    vec![ast_arrow(
                        vec![param],
                        chain_async(&c.body, n),
                    )],
                );
            }
            if let Some(f) = finally {
                // run on both paths (value/handled-error alike)
                let fin = ast_arrow(Vec::new(), chain_async(f, n));
                chain = ast_call(
                    ast_member(chain, "then"),
                    vec![fin.clone(), fin],
                );
            }
            if rest_stmts.is_empty() {
                out.push(Stmt::Return(Some(chain)));
            } else {
                let rest = chain_async(rest_stmts, n);
                out.push(Stmt::Return(Some(ast_call(
                    ast_member(chain, "then"),
                    vec![ast_arrow(Vec::new(), rest)],
                ))));
            }
        }
        // for-of with an awaiting body: desugar to an explicit
        // iterator while-loop (`var it = obj[Symbol.iterator]();
        // while (!(step = it.next()).done) { V = step.value; BODY }`)
        // which the while arm above already chains. `for await` reduces
        // to the same shape (values arrive un-awaited — see for_stmt).
        Stmt::ForIn { decl_kind, var, obj, body, of: true } => {
            let body_v = block_stmts(body);
            if has_early_exit(&body_v) {
                out.push(stmts[j].clone());
                out.extend(chain_async(&stmts[j + 1..], n));
                return out;
            }
            *n += 1;
            let it_name = format!("__it{n}");
            let step_name = format!("__step{n}");
            // var it = (obj)[Symbol.iterator]()
            let sym_iter =
                ast_member(ast_ident("Symbol"), "iterator");
            let it_call = ast_call(
                Expr::Member {
                    obj: Box::new(obj.clone()),
                    prop: MemberProp::Computed(Box::new(sym_iter)),
                    optional: false,
                },
                vec![],
            );
            out.push(Stmt::VarDecl {
                kind: DeclKind::Var,
                decls: vec![(it_name.clone(), Some(it_call))],
            });
            out.push(Stmt::VarDecl {
                kind: DeclKind::Var,
                decls: vec![(step_name.clone(), None)],
            });
            // while (!(step = it.next()).done)
            let next_call =
                ast_call(ast_member(ast_ident(&it_name), "next"), vec![]);
            let assign = Expr::Assign(
                AssignOp::Plain,
                Box::new(ast_ident(&step_name)),
                Box::new(next_call),
            );
            let test = Expr::Unary(
                UnOp::Not,
                Box::new(ast_member(assign, "done")),
            );
            // loop body: bind the value, then the original body
            let value_read =
                ast_member(ast_ident(&step_name), "value");
            let mut loop_body = vec![Stmt::VarDecl {
                kind: decl_kind.unwrap_or(DeclKind::Var),
                decls: vec![(var.clone(), Some(value_read))],
            }];
            loop_body.extend(body_v);
            let mut merged = vec![Stmt::While {
                test,
                body: Box::new(Stmt::Block(loop_body)),
            }];
            merged.extend_from_slice(&stmts[j + 1..]);
            out.extend(chain_async(&merged, n));
        }
        // other constructs (do-while with await bodies) are beyond
        // this chain — keep them; codegen reports the await cleanly
        other => {
            out.push(other.clone());
            out.extend(chain_async(&stmts[j + 1..], n));
        }
    }
    out
}

/// Body of the desugared (now synchronous) async function:
/// `return Promise.resolve().then(() => { <chain> });`
/// The outer wrapper turns a synchronous throw into a rejection too.
fn desugar_async(body: &[Stmt]) -> Vec<Stmt> {
    let mut aw_n = 0usize;
    let body = normalize_body(body.to_vec(), &mut aw_n);
    let inner = chain_async(&body, &mut aw_n);
    let presolve =
        ast_call(ast_member(ast_ident("Promise"), "resolve"), vec![]);
    let wrapper = ast_call(
        ast_member(presolve, "then"),
        vec![ast_arrow(Vec::new(), inner)],
    );
    vec![Stmt::Return(Some(wrapper))]
}

/// Does this member/call spine contain any `?.` link? If so the whole
/// expression must compile as one optional chain with a shared bail.
fn chain_has_optional(e: &Expr) -> bool {
    match e {
        Expr::Member { obj, optional, .. } => {
            *optional || chain_has_optional(obj)
        }
        Expr::Call { callee, optional, .. } => {
            *optional || chain_has_optional(callee)
        }
        _ => false,
    }
}

impl Compiler {
    fn fx(&mut self) -> &mut FnCtx {
        self.fns.last_mut().unwrap()
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, CompileError> {
        Err(CompileError { msg: msg.into() })
    }

    fn atom(&mut self, name: &str) -> u16 {
        if let Some(&i) = self.atom_map.get(name) {
            return i;
        }
        let i = self.module.atoms.len() as u16;
        self.module.atoms.push(name.to_string());
        self.atom_map.insert(name.to_string(), i);
        i
    }

    fn next_ic(&mut self) -> Result<u16, CompileError> {
        if self.module.n_ics == u16::MAX {
            return self.err("too many property-access sites");
        }
        let i = self.module.n_ics;
        self.module.n_ics += 1;
        Ok(i)
    }

    fn string_const(&mut self, s: &str) -> Value {
        if let Some(&i) = self.string_map.get(s) {
            return Value::string(i);
        }
        let i = self.module.strings.len() as u32;
        self.module.strings.push(s.to_string());
        self.string_map.insert(s.to_string(), i);
        Value::string(i)
    }

    fn push_proto(&mut self, f: FnCtx) -> u32 {
        self.module.protos.push(FuncProto {
            name: f.name,
            nparams: f.nparams,
            nregs: f.nregs,
            code: f.code,
            consts: f.consts,
            captures: f.upvals,
            is_arrow: f.is_arrow,
            uses_arguments: f.uses_arguments,
            lazy: None,
        });
        (self.module.protos.len() - 1) as u32
    }

    /// Resolve a name against the scope stack, threading captures
    /// through intermediate functions as needed.
    fn resolve(&mut self, name: &str) -> Place {
        let top = self.fns.len() - 1;
        if let Some(b) = self.fns[top].lookup(name) {
            return if b.is_cell {
                Place::Cell(b.reg)
            } else {
                Place::Reg(b.reg)
            };
        }
        if let Some(&a) = self.fns[top].spilled.get(name) {
            // route through this function's own spill cell
            let sn = self.fns[top].spill_name.clone().unwrap();
            return match self.resolve(&sn) {
                Place::Cell(r) => Place::SpillCell(r, a),
                _ => unreachable!("own spill object is a local cell"),
            };
        }
        if let Some(&u) = self.fns[top].upval_map.get(name) {
            return Place::Up(u);
        }
        if let Some(sn) = self.fns[top].lazy_spills.get(name).cloned() {
            // deferral recorded this name as living in an enclosing
            // function's spill object; route through that object
            let a = self.atom(name);
            return match self.resolve(&sn) {
                Place::Up(u) => Place::SpillUp(u, a),
                Place::Cell(r) => Place::SpillCell(r, a),
                _ => unreachable!("lazy spill object is a cell/upval"),
            };
        }
        let mut found = None;
        for i in (0..top).rev() {
            if self.fns[i].lookup(name).is_some()
                || self.fns[i].upval_map.contains_key(name)
                || self.fns[i].spilled.contains_key(name)
                || self.fns[i].lazy_spills.contains_key(name)
            {
                found = Some(i);
                break;
            }
        }
        let Some(level) = found else {
            let a = self.atom(name);
            return Place::Global(a);
        };
        if let Some(&a) = self.fns[level].spilled.get(name) {
            // an enclosing function's overflow var: capture that
            // function's spill object (its unique %spillN binding rides
            // the ordinary cell-capture chain) and read the property
            let sn = self.fns[level].spill_name.clone().unwrap();
            return match self.resolve(&sn) {
                Place::Up(u) => Place::SpillUp(u, a),
                Place::Cell(r) => Place::SpillCell(r, a),
                _ => unreachable!("spill object resolves to cell/upval"),
            };
        }
        if let Some(sn) = self.fns[level].lazy_spills.get(name).cloned()
        {
            // same, but the spill object arrived through a lazy
            // session root's recorded capture environment
            let a = self.atom(name);
            return match self.resolve(&sn) {
                Place::Up(u) => Place::SpillUp(u, a),
                Place::Cell(r) => Place::SpillCell(r, a),
                _ => unreachable!("lazy spill object is a cell/upval"),
            };
        }
        for i in (level + 1)..=top {
            if self.fns[i].upval_map.contains_key(name) {
                continue;
            }
            let src = if let Some(b) = self.fns[i - 1].lookup(name) {
                // declaration must have promoted it; captures need a cell
                debug_assert!(b.is_cell);
                CapSrc::LocalCell(b.reg)
            } else {
                CapSrc::Upval(self.fns[i - 1].upval_map[name])
            };
            let f = &mut self.fns[i];
            let idx = f.upvals.len() as u16;
            f.upvals.push(src);
            f.upval_map.insert(name.to_string(), idx);
        }
        Place::Up(self.fns[top].upval_map[name])
    }

    /// The binding `resolve` would land on, wherever it lives in the
    /// function stack. Threaded upvalues are skipped on purpose: they
    /// alias a still-open binding that this walk finds directly.
    fn find_binding(&self, name: &str) -> Option<Binding> {
        self.fns.iter().rev().find_map(|f| f.lookup(name))
    }

    /// Could this read/write run before the binding's declaration?
    /// True only between scope entry and the declaration statement, so
    /// initialized code paths never pay for a check.
    fn tdz_pending(&self, name: &str) -> bool {
        self.find_binding(name).is_some_and(|b| {
            matches!(b.kind, BindKind::Let | BindKind::Const)
                && !b.initialized
        })
    }

    fn emit_tdz_check(&mut self, name: &str, src: u8) {
        let atom = self.atom(name);
        self.fx().emit(Instr::TdzCheck { src, atom });
    }

    fn check_assignable(&self, name: &str) -> Result<(), CompileError> {
        let is_const = match self.find_binding(name) {
            Some(b) => b.kind == BindKind::Const,
            None => self.const_globals.contains(name),
        };
        if is_const {
            return Err(CompileError {
                msg: format!("assignment to constant variable '{name}'"),
            });
        }
        Ok(())
    }

    /// Create one block-scoped binding in the innermost scope. The
    /// register starts as the TDZ marker; captured names get a heap
    /// cell now (fresh per pass through the block) so closures created
    /// before the declaration still capture the right binding.
    fn declare_lexical_one(
        &mut self,
        name: &str,
        kind: DeclKind,
    ) -> Result<(), CompileError> {
        let bind_kind = match kind {
            DeclKind::Let => BindKind::Let,
            DeclKind::Const => BindKind::Const,
            DeclKind::Var => unreachable!("var is function-scoped"),
        };
        // Register exhaustion (minified mega-functions with hundreds
        // of lexicals): spill the binding into the %spillN heap object
        // instead of failing the whole compile. Trades TDZ/const
        // re-assignment checks for those bindings — undefined-before-
        // init instead of a throw — which valid minified code never
        // observes. The spill object must live in the FUNCTION scope
        // (its register survives block exits), so a fresh one can only
        // be created while scopes[0] is innermost. Spill well before
        // the 250 ceiling: locals and expression temps share the
        // register file, and mega-functions need deep temp headroom.
        // A name already spilled by an enclosing block falls through
        // to a register declaration (inner shadow must not clobber the
        // outer spill slot).
        {
            let f = self.fns.last().unwrap();
            if f.locals_end >= 160
                && (f.spill_name.is_some() || f.scopes.len() == 1)
                && !f.spilled.contains_key(name)
            {
                let a = self.atom(name);
                let f = self.fns.last_mut().unwrap();
                if f.spill_name.is_none() {
                    let sn = next_spill_name();
                    let r = f.declare(&sn, BindKind::Var, true)?;
                    f.emit(Instr::NewObject { dst: r });
                    f.emit(Instr::CellWrap { reg: r });
                    f.lookup_mut(&sn).unwrap().is_cell = true;
                    f.spill_name = Some(sn);
                }
                f.spilled.insert(name.to_string(), a);
                f.scopes
                    .last_mut()
                    .unwrap()
                    .spilled_here
                    .push(name.to_string());
                return Ok(());
            }
        }
        let f = self.fns.last_mut().unwrap();
        if f.scopes.last().unwrap().bindings.contains_key(name) {
            return Err(CompileError {
                msg: format!(
                    "identifier '{name}' has already been declared"
                ),
            });
        }
        let is_cell = f.captured.contains(name);
        let r = f.declare(name, bind_kind, false)?;
        let k = f.const_idx(Value::TDZ);
        f.emit(Instr::LoadConst { dst: r, idx: k });
        if is_cell {
            f.emit(Instr::CellWrap { reg: r });
            f.lookup_mut(name).unwrap().is_cell = true;
        }
        Ok(())
    }

    /// Bind the let/const declarations that appear directly in a block
    /// body, up front — forward references (TDZ, closures) need the
    /// binding to exist from block entry.
    fn declare_lexical(
        &mut self,
        stmts: &[Stmt],
    ) -> Result<(), CompileError> {
        for s in stmts {
            let Stmt::VarDecl { kind, decls } = s else { continue };
            if *kind == DeclKind::Var {
                continue;
            }
            for (name, _) in decls {
                self.declare_lexical_one(name, *kind)?;
            }
        }
        Ok(())
    }

    fn load_place(&mut self, p: Place, dst: u8) {
        match p {
            Place::Reg(r) => {
                if r != dst {
                    self.fx().emit(Instr::Move { dst, src: r });
                }
            }
            Place::Cell(r) => {
                self.fx().emit(Instr::LoadCell { dst, src: r });
            }
            Place::Up(u) => {
                self.fx().emit(Instr::GetUpval { dst, idx: u });
            }
            Place::Global(a) => {
                self.fx().emit(Instr::GetGlobal { dst, atom: a });
            }
            Place::SpillCell(r, a) => {
                // dst doubles as scratch: the VM reads obj before
                // writing dst, so LoadCell into dst then GetProp works
                self.fx().emit(Instr::LoadCell { dst, src: r });
                let ic = self.next_ic().unwrap_or(0);
                self.fx().emit(Instr::GetProp {
                    dst, obj: dst, atom: a, ic,
                });
            }
            Place::SpillUp(u, a) => {
                self.fx().emit(Instr::GetUpval { dst, idx: u });
                let ic = self.next_ic().unwrap_or(0);
                self.fx().emit(Instr::GetProp {
                    dst, obj: dst, atom: a, ic,
                });
            }
        }
    }

    fn store_place(&mut self, p: Place, src: u8) {
        match p {
            Place::Reg(r) => {
                if r != src {
                    self.fx().emit(Instr::Move { dst: r, src });
                }
            }
            Place::Cell(r) => {
                self.fx().emit(Instr::StoreCell { dst: r, src });
            }
            Place::Up(u) => {
                self.fx().emit(Instr::SetUpval { idx: u, src });
            }
            Place::Global(a) => {
                self.fx().emit(Instr::SetGlobal { atom: a, src });
            }
            Place::SpillCell(r, a) => {
                let t = self.fx().alloc().unwrap_or(src);
                self.fx().emit(Instr::LoadCell { dst: t, src: r });
                let ic = self.next_ic().unwrap_or(0);
                self.fx().emit(Instr::SetProp {
                    obj: t, atom: a, src, ic,
                });
            }
            Place::SpillUp(u, a) => {
                let t = self.fx().alloc().unwrap_or(src);
                self.fx().emit(Instr::GetUpval { dst: t, idx: u });
                let ic = self.next_ic().unwrap_or(0);
                self.fx().emit(Instr::SetProp {
                    obj: t, atom: a, src, ic,
                });
            }
        }
    }

    fn store_name(&mut self, name: &str, src: u8) {
        let p = self.resolve(name);
        self.store_place(p, src);
    }

    fn compile_func(
        &mut self,
        lit: &Rc<FuncLit>,
        is_arrow: bool,
    ) -> Result<u32, CompileError> {
        // async fn -> a plain fn whose body returns a promise chain
        if lit.is_async {
            let desugared = Rc::new(FuncLit {
                name: lit.name.clone(),
                params: lit.params.clone(),
                body: desugar_async(&lit.body),
                is_async: false,
                lazy_body: None,
            });
            return self.compile_func(&desugared, is_arrow);
        }
        if lit.params.len() > 200 {
            return self.err("too many parameters");
        }
        // Lazy compilation: most bundle functions are never called, so
        // defer body codegen to first call. Trivial bodies compile now
        // (a stub would cost more than the codegen it saves). A
        // lazy-PARSED body has no AST yet, so it always defers.
        if lit.lazy_body.is_some() || lit.body.len() > 1 {
            return self.defer_func(lit, is_arrow);
        }
        self.compile_func_now(lit, is_arrow, None)
    }

    /// The lazy path: skip body codegen entirely. Free variables are
    /// resolved against the enclosing scopes NOW (threading upvalues
    /// through intermediate functions exactly as eager compilation
    /// would at first reference), so the emitted Closure instruction
    /// and the enclosing function's cell layout are identical to the
    /// eager result. The VM compiles the stashed AST on first call.
    fn defer_func(
        &mut self,
        lit: &Rc<FuncLit>,
        is_arrow: bool,
    ) -> Result<u32, CompileError> {
        let name =
            lit.name.clone().unwrap_or_else(|| "<anon>".to_string());
        let mut free: Vec<String> = free_vars(lit).into_iter().collect();
        free.sort(); // deterministic capture order
        self.fns.push(FnCtx::new(name.clone(),
                                 lit.params.len() as u8, false));
        let mut spill_names: Vec<(String, String)> = Vec::new();
        for n in &free {
            match self.resolve(n) {
                Place::Up(_) | Place::Global(_) => {}
                Place::SpillUp(u, _) => {
                    // the %spillN object itself became an upval of the
                    // deferred fn; remember which name routes through it
                    let top = self.fns.len() - 1;
                    let sn = self.fns[top]
                        .upval_map
                        .iter()
                        .find(|&(_, &i)| i == u)
                        .map(|(k, _)| k.clone())
                        .expect("captured spill object has a name");
                    spill_names.push((n.clone(), sn));
                }
                _ => unreachable!(
                    "an empty deferred ctx resolves no locals"
                ),
            }
        }
        let f = self.fns.pop().unwrap();
        // upval names in capture order (parallel to f.upvals)
        let mut upval_names = vec![String::new(); f.upvals.len()];
        for (n, &i) in &f.upval_map {
            upval_names[i as usize] = n.clone();
        }
        self.module.protos.push(FuncProto {
            name,
            nparams: lit.params.len() as u8,
            nregs: 0,
            code: Vec::new(),
            consts: Vec::new(),
            captures: f.upvals,
            is_arrow,
            // conservative until the body is compiled and says
            // otherwise; callers re-read the compiled proto anyway
            uses_arguments: true,
            lazy: Some(Box::new(LazySrc {
                lit: lit.clone(),
                is_arrow,
                upval_names,
                spill_names,
            })),
        });
        Ok((self.module.protos.len() - 1) as u32)
    }

    fn compile_func_now(
        &mut self,
        lit: &Rc<FuncLit>,
        is_arrow: bool,
        seed: Option<&LazySrc>,
    ) -> Result<u32, CompileError> {
        let name = lit.name.clone().unwrap_or_else(|| "<anon>".to_string());
        let mut f = FnCtx::new(name, lit.params.len() as u8, false);
        f.is_arrow = is_arrow;
        if let Some(sd) = seed {
            // lazy-session root: recreate the capture environment
            // recorded at deferral time. The CapSrc entries are
            // placeholders — the closure already exists (built from
            // the stub proto); only the name -> upval index mapping
            // matters here.
            for (i, n) in sd.upval_names.iter().enumerate() {
                f.upvals.push(CapSrc::Upval(i as u16));
                f.upval_map.insert(n.clone(), i as u16);
            }
            for (n, sn) in &sd.spill_names {
                f.lazy_spills.insert(n.clone(), sn.clone());
            }
        }
        for (i, p) in lit.params.iter().enumerate() {
            // duplicate params overwrite: last one wins, like sloppy JS
            f.scopes[0].bindings.insert(
                p.clone(),
                Binding {
                    reg: i as u8,
                    is_cell: false,
                    kind: BindKind::Var,
                    initialized: true,
                },
            );
        }
        let mut names = Vec::new();
        hoist(&lit.body, &mut names);
        // `arguments`: synthesized first thing in the prologue (before
        // spill objects can clobber extra-arg registers), unless the
        // body declares its own binding with that name
        {
            let mut body_ids = HashSet::new();
            let mut nested = Vec::new();
            for s in &lit.body {
                scan_stmt(s, &mut body_ids, &mut nested);
            }
            if body_ids.contains("arguments")
                && !names.iter().any(|n| n == "arguments")
                && !lit.params.iter().any(|p| p == "arguments")
            {
                let r = f.declare("arguments", BindKind::Var, true)?;
                f.emit(Instr::Arguments { dst: r });
                f.uses_arguments = true;
            }
        }
        // ES named-function-expression semantics: the function's own
        // name binds to itself inside the body (Babel _classCallCheck
        // guards do `this instanceof t` from within `function t()`),
        // unless a param or hoisted var shadows it
        if !is_arrow {
            if let Some(n) = &lit.name {
                if !n.is_empty()
                    && !lit.params.iter().any(|p| p == n)
                    && !names.iter().any(|m| m == n)
                {
                    let r = f.declare(n, BindKind::Var, true)?;
                    f.emit(Instr::LoadSelf { dst: r });
                }
            }
        }
        for n in names {
            if f.lookup(&n).is_none() && !f.spilled.contains_key(&n) {
                // minified mega-functions exceed the u8 register file:
                // spill overflow vars into a hidden heap object. The
                // object is cell-wrapped so nested closures can reach
                // captured overflow vars through it (the object itself
                // is shared state — vars need no per-name cells).
                // spill early: registers are one shared 250-slot budget
                // for locals AND expression temps; minified bundles need
                // deep temp headroom (nested call chains), so keep
                // locals lean and let overflow vars live on the heap
                if f.locals_end >= 120 {
                    if f.spill_name.is_none() {
                        let sn = next_spill_name();
                        let r = f.declare(&sn, BindKind::Var, true)?;
                        f.emit(Instr::NewObject { dst: r });
                        f.emit(Instr::CellWrap { reg: r });
                        f.lookup_mut(&sn).unwrap().is_cell = true;
                        f.spill_name = Some(sn);
                    }
                    let a = self.atom(&n);
                    f.spilled.insert(n, a);
                } else {
                    f.declare(&n, BindKind::Var, true)?;
                }
            }
        }

        // promote captured function-scope locals to cells and wrap
        // them on entry
        f.captured = captured_names(&lit.body);
        let captured = std::mem::take(&mut f.captured);
        let mut cell_regs: Vec<u8> = Vec::new();
        for (n, b) in f.scopes[0].bindings.iter_mut() {
            if captured.contains(n) {
                b.is_cell = true;
                cell_regs.push(b.reg);
            }
        }
        f.captured = captured;
        cell_regs.sort_unstable();
        for &r in &cell_regs {
            f.emit(Instr::CellWrap { reg: r });
        }

        self.fns.push(f);
        // top-of-body let/const share the function scope (their block
        // is the whole body)
        self.declare_lexical(&lit.body)?;
        self.hoist_func_decls(&lit.body)?;
        for s in &lit.body {
            self.stmt(s)?;
            let f = self.fx();
            f.tmp_top = f.locals_end;
        }
        self.fx().emit(Instr::ReturnUndef);
        let done = self.fns.pop().unwrap();
        Ok(self.push_proto(done))
    }

    /// Create the closures for a body's direct function declarations at
    /// function entry (JS hoists whole functions, not just their names).
    fn hoist_func_decls(
        &mut self,
        body: &[Stmt],
    ) -> Result<(), CompileError> {
        for s in body {
            if let Stmt::FuncDecl(lit) = s {
                let r = self.make_closure(lit, false)?;
                let name = lit.name.clone().unwrap();
                self.store_name(&name, r);
                self.fx().hoisted_fns.insert(name);
                let f = self.fx();
                f.tmp_top = f.locals_end;
            }
        }
        Ok(())
    }

    fn make_closure(
        &mut self,
        lit: &Rc<FuncLit>,
        is_arrow: bool,
    ) -> Result<u8, CompileError> {
        let idx = self.compile_func(lit, is_arrow)?;
        if idx > u16::MAX as u32 {
            return self.err("too many functions");
        }
        let dst = self.fx().alloc()?;
        self.fx().emit(Instr::Closure { dst, proto: idx as u16 });
        Ok(dst)
    }

    // ---- statements ----

    fn stmt(&mut self, s: &Stmt) -> Result<(), CompileError> {
        match s {
            Stmt::Expr(e) => {
                let r = self.expr(e)?;
                if self.fx().is_main {
                    self.fx().emit(Instr::Move { dst: 0, src: r });
                }
                Ok(())
            }
            Stmt::VarDecl { kind: DeclKind::Var, decls } => {
                for (name, init) in decls {
                    let Some(init) = init else {
                        // `var u;` at script level: mark the global
                        // defined (reads yield undefined, not a
                        // ReferenceError) without clobbering any
                        // existing value
                        if self.fx().is_main
                            && self.fx().lookup(name).is_none()
                        {
                            let r = self.fx().alloc()?;
                            let a = self.atom(name);
                            self.fx().emit(Instr::GetGlobalSafe {
                                dst: r,
                                atom: a,
                            });
                            self.fx().emit(Instr::SetGlobal {
                                atom: a,
                                src: r,
                            });
                        }
                        continue;
                    };
                    let r = self.expr(init)?;
                    self.store_name(name, r);
                }
                Ok(())
            }
            Stmt::VarDecl { kind, decls } => {
                // let / const. Script-level declarations compile to
                // globals (later <script>s share them, like var); any
                // other position uses the block binding made by
                // declare_lexical.
                let top_of_script =
                    self.fx().is_main && self.fx().scopes.len() == 1;
                for (name, init) in decls {
                    if *kind == DeclKind::Const && init.is_none() {
                        return self.err(format!(
                            "missing initializer in const \
                             declaration of '{name}'"
                        ));
                    }
                    if top_of_script {
                        let r = match init {
                            Some(e) => self.expr(e)?,
                            None => {
                                let r = self.fx().alloc()?;
                                self.fx()
                                    .emit(Instr::LoadUndef { dst: r });
                                r
                            }
                        };
                        self.store_name(name, r);
                        if *kind == DeclKind::Const {
                            self.const_globals.insert(name.clone());
                        }
                        continue;
                    }
                    if self
                        .fx()
                        .scopes
                        .last()
                        .unwrap()
                        .bindings
                        .get(name)
                        .is_none()
                        && !self.fx().spilled.contains_key(name)
                    {
                        // `if (c) let x` style (not valid JS): tolerate
                        // by binding into the enclosing scope
                        self.declare_lexical_one(name, *kind)?;
                    }
                    let Some(b) = self.fx().lookup(name) else {
                        // spilled lexical: no register binding — route
                        // the initializer through the spill object
                        let rv = match init {
                            Some(e) => self.expr(e)?,
                            None => {
                                let r = self.fx().alloc()?;
                                self.fx()
                                    .emit(Instr::LoadUndef { dst: r });
                                r
                            }
                        };
                        self.store_name(name, rv);
                        continue;
                    };
                    let rv = match init {
                        Some(e) => self.expr(e)?,
                        None => {
                            let r = self.fx().alloc()?;
                            self.fx().emit(Instr::LoadUndef { dst: r });
                            r
                        }
                    };
                    if b.is_cell {
                        self.fx().emit(Instr::StoreCell {
                            dst: b.reg,
                            src: rv,
                        });
                    } else if rv != b.reg {
                        self.fx()
                            .emit(Instr::Move { dst: b.reg, src: rv });
                    }
                    self.fx().lookup_mut(name).unwrap().initialized =
                        true;
                }
                Ok(())
            }
            Stmt::FuncDecl(lit) => {
                let name = lit.name.clone().unwrap();
                // already created at function entry (hoist_func_decls)
                if self.fx().hoisted_fns.contains(&name) {
                    return Ok(());
                }
                let r = self.make_closure(lit, false)?;
                self.store_name(&name, r);
                Ok(())
            }
            Stmt::Return(arg) => {
                match arg {
                    Some(e) => {
                        let r = self.expr(e)?;
                        if self.fx().trys.is_empty() {
                            self.fx().emit(Instr::Return { src: r });
                        } else {
                            // JS computes the return value before the
                            // finallies run — park it in a register the
                            // inlined finally code cannot touch
                            let save = {
                                let f = self.fx();
                                let save = f.locals_end;
                                if save >= 250 {
                                    return Err(CompileError {
                                        msg: format!(
                                            "too many locals in {}",
                                            f.name
                                        ),
                                    });
                                }
                                f.locals_end = save + 1;
                                if f.tmp_top < f.locals_end {
                                    f.tmp_top = f.locals_end;
                                }
                                if f.nregs < f.locals_end {
                                    f.nregs = f.locals_end;
                                }
                                f.emit(Instr::Move { dst: save, src: r });
                                save
                            };
                            self.unwind_trys(0)?;
                            let f = self.fx();
                            f.emit(Instr::Return { src: save });
                            f.locals_end = save;
                            f.tmp_top = save;
                        }
                    }
                    None => {
                        self.unwind_trys(0)?;
                        self.fx().emit(Instr::ReturnUndef);
                    }
                }
                Ok(())
            }
            Stmt::Block(body) => self.block_body(body),
            Stmt::With { obj, body } => {
                let rc = self.expr(obj)?;
                self.fx().emit(Instr::WithEnter { obj: rc });
                self.fx().tmp_top = self.fx().locals_end;
                self.stmt(body)?;
                self.fx().emit(Instr::WithExit);
                Ok(())
            }
            Stmt::If { test, cons, alt } => {
                let rc = self.expr(test)?;
                let jf = self
                    .fx()
                    .emit(Instr::JumpIfFalse { cond: rc, target: 0 });
                let f = self.fx();
                f.tmp_top = f.locals_end;
                self.stmt(cons)?;
                if let Some(alt) = alt {
                    let jend = self.fx().emit(Instr::Jump { target: 0 });
                    self.fx().patch(jf);
                    self.stmt(alt)?;
                    self.fx().patch(jend);
                } else {
                    self.fx().patch(jf);
                }
                Ok(())
            }
            Stmt::While { test, body } => {
                let start = self.fx().code.len() as u32;
                let rc = self.expr(test)?;
                let jf = self
                    .fx()
                    .emit(Instr::JumpIfFalse { cond: rc, target: 0 });
                let f = self.fx();
                f.tmp_top = f.locals_end;
                let td = f.trys.len();
                f.loops.push(LoopCtx::loop_(td));
                let lbl = f.pending_label.take();
                f.loops.last_mut().unwrap().label = lbl;
                self.stmt(body)?;
                self.fx().emit(Instr::Jump { target: start });
                self.finish_loop(start, jf.into());
                Ok(())
            }
            Stmt::DoWhile { body, test } => {
                let start = self.fx().code.len() as u32;
                {
                    let f = self.fx();
                    let td = f.trys.len();
                    f.loops.push(LoopCtx::loop_(td));
                let lbl = f.pending_label.take();
                f.loops.last_mut().unwrap().label = lbl;
                }
                self.stmt(body)?;
                let test_pos = self.fx().code.len() as u32;
                let rc = self.expr(test)?;
                self.fx().emit(Instr::JumpIfTrue { cond: rc, target: start });
                self.finish_loop(test_pos, None);
                Ok(())
            }
            Stmt::For { init, test, update, body } => {
                // `for (let/const ...)` binds in the loop's own scope
                let lexical_init = matches!(
                    init.as_deref(),
                    Some(Stmt::VarDecl {
                        kind: DeclKind::Let | DeclKind::Const,
                        ..
                    })
                );
                if lexical_init {
                    self.fx().push_scope();
                    if let Some(Stmt::VarDecl { kind, decls }) =
                        init.as_deref()
                    {
                        for (name, _) in decls {
                            self.declare_lexical_one(name, *kind)?;
                        }
                    }
                }
                if let Some(init) = init {
                    self.stmt(init)?;
                    let f = self.fx();
                    f.tmp_top = f.locals_end;
                }
                // captured for-let variables get a fresh cell each
                // iteration (at the continue target, below) so each
                // iteration's closures see that iteration's value
                let refresh: Vec<u8> = if lexical_init {
                    let f = self.fns.last().unwrap();
                    let mut regs: Vec<u8> = f
                        .scopes
                        .last()
                        .unwrap()
                        .bindings
                        .values()
                        .filter(|b| b.is_cell && b.kind == BindKind::Let)
                        .map(|b| b.reg)
                        .collect();
                    regs.sort_unstable();
                    regs
                } else {
                    Vec::new()
                };
                let start = self.fx().code.len() as u32;
                let jf = match test {
                    Some(test) => {
                        let rc = self.expr(test)?;
                        let jf = self
                            .fx()
                            .emit(Instr::JumpIfFalse { cond: rc, target: 0 });
                        let f = self.fx();
                        f.tmp_top = f.locals_end;
                        Some(jf)
                    }
                    None => None,
                };
                {
                    let f = self.fx();
                    let td = f.trys.len();
                    f.loops.push(LoopCtx::loop_(td));
                let lbl = f.pending_label.take();
                f.loops.last_mut().unwrap().label = lbl;
                }
                self.stmt(body)?;
                {
                    let f = self.fx();
                    f.tmp_top = f.locals_end;
                }
                let update_pos = self.fx().code.len() as u32;
                for &r in &refresh {
                    // r = fresh cell holding the current value: the
                    // spec's per-iteration environment copy, made
                    // before the update expression runs
                    self.fx().emit(Instr::LoadCell { dst: r, src: r });
                    self.fx().emit(Instr::CellWrap { reg: r });
                }
                if let Some(update) = update {
                    self.expr(update)?;
                    let f = self.fx();
                    f.tmp_top = f.locals_end;
                }
                self.fx().emit(Instr::Jump { target: start });
                self.finish_loop(update_pos, jf);
                if lexical_init {
                    self.fx().pop_scope();
                }
                Ok(())
            }
            Stmt::Labeled { label, body } => {
                // loops take the label onto their own LoopCtx; anything
                // else (labeled block, labeled switch — the minified
                // early-exit staple `a:{...break a}`) gets a break-only
                // wrapper context here
                if matches!(
                    **body,
                    Stmt::While { .. }
                        | Stmt::DoWhile { .. }
                        | Stmt::For { .. }
                        | Stmt::ForIn { .. }
                ) {
                    self.fx().pending_label = Some(label.clone());
                    self.stmt(body)?;
                    self.fx().pending_label = None;
                } else {
                    let td = self.fx().trys.len();
                    self.fx().loops.push(LoopCtx {
                        breaks: Vec::new(),
                        continues: Vec::new(),
                        is_switch: true, // transparent to `continue`
                        trys_depth: td,
                        label: Some(label.clone()),
                    });
                    self.stmt(body)?;
                    let ctx = self.fx().loops.pop().unwrap();
                    for site in ctx.breaks {
                        self.fx().patch(site);
                    }
                }
                Ok(())
            }
            Stmt::Break(label) => {
                let pos = match label {
                    None => self.fx().loops.len().checked_sub(1),
                    Some(l) => self.fx().loops.iter()
                        .rposition(|c| c.label.as_deref() == Some(l)),
                };
                let Some(pos) = pos else {
                    return self.err("break target not found");
                };
                let td = self.fx().loops[pos].trys_depth;
                self.unwind_trys(td)?;
                let site = self.fx().emit(Instr::Jump { target: 0 });
                self.fx().loops[pos].breaks.push(site);
                Ok(())
            }
            Stmt::Continue(label) => {
                // `continue` belongs to a *loop* (skips switch frames)
                let pos = match label {
                    None => self.fx().loops.iter()
                        .rposition(|c| !c.is_switch),
                    Some(l) => self.fx().loops.iter().rposition(|c| {
                        c.label.as_deref() == Some(l) && !c.is_switch
                    }),
                };
                let Some(pos) = pos else {
                    return self.err("continue target not found");
                };
                let td = self.fx().loops[pos].trys_depth;
                self.unwind_trys(td)?;
                let site = self.fx().emit(Instr::Jump { target: 0 });
                self.fx().loops[pos].continues.push(site);
                Ok(())
            }
            Stmt::ForIn { var, obj, body, of, decl_kind } => {
                // Loop-carried state (the array, its length, the cursor)
                // must live in the *locals* region, not temps — nested
                // statements reset tmp_top to locals_end each step and
                // would otherwise clobber it. Reserve three locals for
                // the duration of the loop, then release them.
                let base = self.fx().locals_end;
                if base as usize + 3 >= 250 {
                    return self.err("for-in/of nested too deep");
                }
                let (arr, len, idx) = (base, base + 1, base + 2);
                {
                    let f = self.fx();
                    f.locals_end = base + 3;
                    f.tmp_top = f.locals_end;
                    if f.nregs < f.locals_end {
                        f.nregs = f.locals_end;
                    }
                }

                // arr = the array to walk: the iterable itself (for-of)
                // or its key list (for-in).
                let rsrc = self.expr(obj)?;
                if *of {
                    self.fx().emit(Instr::IterMaterialize {
                        dst: arr,
                        obj: rsrc,
                    });
                } else {
                    self.fx().emit(Instr::ForInKeys { dst: arr, obj: rsrc });
                }
                self.fx().tmp_top = base + 3;

                // for (let/const x ...) scopes x to the loop; the loop
                // machinery itself (re)assigns it, so it starts
                // initialized and a captured one is re-celled per
                // iteration below.
                let loop_var = match decl_kind {
                    Some(DeclKind::Let) | Some(DeclKind::Const) => {
                        self.fx().push_scope();
                        let kind = if *decl_kind == Some(DeclKind::Const)
                        {
                            BindKind::Const
                        } else {
                            BindKind::Let
                        };
                        let is_cell = self.fx().captured.contains(var);
                        let r = self.fx().declare(var, kind, true)?;
                        if is_cell {
                            self.fx().lookup_mut(var).unwrap().is_cell =
                                true;
                        }
                        Some((r, is_cell))
                    }
                    None => {
                        self.check_assignable(var)?;
                        None
                    }
                    Some(DeclKind::Var) => None,
                };
                let lend = self.fx().locals_end;

                // len = arr.length;  idx = 0
                let length_atom = self.atom("length");
                let ic = self.next_ic()?;
                self.fx().emit(Instr::GetProp {
                    dst: len, obj: arr, atom: length_atom, ic,
                });
                self.fx().emit(Instr::LoadInt { dst: idx, val: 0 });

                // cond: idx < len
                let start = self.fx().code.len() as u32;
                let cond = self.fx().alloc()?;
                self.fx().emit(Instr::Lt { dst: cond, a: idx, b: len });
                let jf = self
                    .fx()
                    .emit(Instr::JumpIfFalse { cond, target: 0 });
                self.fx().tmp_top = lend;

                // var = arr[idx]
                let elem = self.fx().alloc()?;
                self.fx().emit(Instr::GetIndex {
                    dst: elem, obj: arr, key: idx,
                });
                match loop_var {
                    Some((r, is_cell)) => {
                        self.fx().emit(Instr::Move { dst: r, src: elem });
                        if is_cell {
                            // fresh cell per iteration: closures made
                            // in the body capture this element only
                            self.fx().emit(Instr::CellWrap { reg: r });
                        }
                    }
                    None => self.store_name(var, elem),
                }
                self.fx().tmp_top = lend;

                // body
                {
                    let f = self.fx();
                    let td = f.trys.len();
                    f.loops.push(LoopCtx::loop_(td));
                let lbl = f.pending_label.take();
                f.loops.last_mut().unwrap().label = lbl;
                }
                self.stmt(body)?;
                {
                    let f = self.fx();
                    f.tmp_top = f.locals_end;
                }

                // continue target: idx = idx + 1
                let cont = self.fx().code.len() as u32;
                let one = self.fx().alloc()?;
                self.fx().emit(Instr::LoadInt { dst: one, val: 1 });
                self.fx().emit(Instr::Add { dst: idx, a: idx, b: one });
                self.fx().tmp_top = lend;
                self.fx().emit(Instr::Jump { target: start });

                self.finish_loop(cont, Some(jf));

                // release the loop scope and the synthetic locals
                if loop_var.is_some() {
                    self.fx().pop_scope();
                }
                let f = self.fx();
                f.locals_end = base;
                f.tmp_top = base;
                Ok(())
            }
            Stmt::Switch { disc, cases } => {
                // All cases share one block scope (JS switch
                // semantics); bind their let/const up front. TDZ is
                // not enforced across fall-through into a later case.
                self.fx().push_scope();
                for case in cases {
                    self.declare_lexical(&case.body)?;
                }
                // Evaluate the discriminant once into a stable temp.
                let rd = self.expr(disc)?;
                let rdisc = self.fx().alloc()?;
                self.fx().emit(Instr::Move { dst: rdisc, src: rd });
                self.fx().tmp_top = rdisc + 1;

                {
                    let f = self.fx();
                    let td = f.trys.len();
                    f.loops.push(LoopCtx::switch(td));
                }

                // Pass 1: `disc === caseval` chain, each jumping to its
                // body on a hit. `default:` records no comparison.
                let mut entry: Vec<Option<usize>> = Vec::with_capacity(cases.len());
                let mut default_case: Option<usize> = None;
                for (ci, case) in cases.iter().enumerate() {
                    match &case.test {
                        Some(t) => {
                            let rt = self.expr(t)?;
                            let req = self.fx().alloc()?;
                            self.fx().emit(Instr::StrictEq {
                                dst: req, a: rdisc, b: rt,
                            });
                            let j = self.fx().emit(Instr::JumpIfTrue {
                                cond: req, target: 0,
                            });
                            entry.push(Some(j));
                            self.fx().tmp_top = rdisc + 1;
                        }
                        None => {
                            default_case = Some(ci);
                            entry.push(None);
                        }
                    }
                }
                // No match: fall to `default:` if present, else past the end.
                let no_match = self.fx().emit(Instr::Jump { target: 0 });
                {
                    let f = self.fx();
                    f.tmp_top = f.locals_end;
                }

                // Pass 2: bodies laid out consecutively so fall-through
                // (a case without `break`) just flows into the next.
                let mut body_start: Vec<u32> = Vec::with_capacity(cases.len());
                for (ci, case) in cases.iter().enumerate() {
                    let here = self.fx().code.len() as u32;
                    body_start.push(here);
                    if let Some(Some(site)) = entry.get(ci) {
                        let site = *site;
                        self.fx().patch_to(site, here);
                    }
                    for s in &case.body {
                        self.stmt(s)?;
                        let f = self.fx();
                        f.tmp_top = f.locals_end;
                    }
                }
                let end = self.fx().code.len() as u32;
                let target = match default_case {
                    Some(ci) => body_start[ci],
                    None => end,
                };
                self.fx().patch_to(no_match, target);

                let ctx = self.fx().loops.pop().unwrap();
                for site in ctx.breaks {
                    self.fx().patch_to(site, end);
                }
                self.fx().pop_scope();
                Ok(())
            }
            Stmt::Throw(e) => {
                let r = self.expr(e)?;
                self.fx().emit(Instr::Throw { src: r });
                Ok(())
            }
            Stmt::Try { block, catch, finally } => {
                // Synthetic scope holding the exception slots for the
                // whole statement ('%' names cannot collide with source
                // identifiers, and the registers must survive the
                // inlined finally/catch code running above them).
                self.fx().push_scope();
                let fin = finally.as_ref().map(|f| Rc::new(f.clone()));

                // The finally handler arms first (outermost): its
                // exception path reruns F, then rethrows.
                let fin_arm = match &fin {
                    Some(_) => {
                        let rex = self
                            .fx()
                            .declare("%exc-fin", BindKind::Var, true)?;
                        let site = self.fx().emit(Instr::PushHandler {
                            catch_ip: 0,
                            exc: rex,
                        });
                        Some((site, rex))
                    }
                    None => None,
                };
                // The catch handler arms second (innermost).
                let cat_arm = match catch {
                    Some(_) => {
                        let rex = self
                            .fx()
                            .declare("%exc-cat", BindKind::Var, true)?;
                        let site = self.fx().emit(Instr::PushHandler {
                            catch_ip: 0,
                            exc: rex,
                        });
                        Some((site, rex))
                    }
                    None => None,
                };
                let n_armed =
                    (fin_arm.is_some() as u8) + (cat_arm.is_some() as u8);
                self.fx().trys.push(TryCtx {
                    handlers: n_armed,
                    finally: fin.clone(),
                });

                self.block_body(block)?;

                let mut j_join = None;
                if let Some((site_c, rexc)) = cat_arm {
                    self.fx().emit(Instr::PopHandler);
                    // inside the catch body only the finally handler
                    // (if any) is still armed
                    self.fx().trys.last_mut().unwrap().handlers =
                        fin_arm.is_some() as u8;
                    j_join =
                        Some(self.fx().emit(Instr::Jump { target: 0 }));

                    // catch entry: the unwinder put the thrown value
                    // in rexc; bind the param over that register
                    self.fx().patch(site_c);
                    let c = catch.as_ref().unwrap();
                    self.fx().push_scope();
                    if let Some(param) = &c.param {
                        let is_cell =
                            self.fx().captured.contains(param);
                        if is_cell {
                            self.fx()
                                .emit(Instr::CellWrap { reg: rexc });
                        }
                        self.fx()
                            .scopes
                            .last_mut()
                            .unwrap()
                            .bindings
                            .insert(
                                param.clone(),
                                Binding {
                                    reg: rexc,
                                    is_cell,
                                    kind: BindKind::Let,
                                    initialized: true,
                                },
                            );
                    }
                    self.declare_lexical(&c.body)?;
                    for s in &c.body {
                        self.stmt(s)?;
                        let f = self.fx();
                        f.tmp_top = f.locals_end;
                    }
                    self.fx().pop_scope();
                }
                if let Some(j) = j_join {
                    self.fx().patch(j);
                }

                // this try is over for return/break purposes; its
                // finally tail below already runs on those paths
                self.fx().trys.pop();

                if let Some((site_f, rexf)) = fin_arm {
                    let f_stmts = fin.as_ref().unwrap().clone();
                    self.fx().emit(Instr::PopHandler);
                    self.block_body(&f_stmts)?; // normal path
                    let j_end =
                        self.fx().emit(Instr::Jump { target: 0 });
                    self.fx().patch(site_f); // exception path
                    self.block_body(&f_stmts)?;
                    self.fx().emit(Instr::Throw { src: rexf });
                    self.fx().patch(j_end);
                }

                self.fx().pop_scope();
                Ok(())
            }
            Stmt::Empty => Ok(()),
        }
    }

    /// Compile a statement list in its own block scope (shared by
    /// Stmt::Block, catch bodies, and inlined finally bodies).
    fn block_body(&mut self, body: &[Stmt]) -> Result<(), CompileError> {
        self.fx().push_scope();
        self.declare_lexical(body)?;
        for s in body {
            self.stmt(s)?;
            let f = self.fx();
            f.tmp_top = f.locals_end;
        }
        self.fx().pop_scope();
        Ok(())
    }

    /// Emit what a jump out of enclosing `try`s needs: disarm their
    /// handlers and inline their finally bodies, innermost first. The
    /// trys stack is restored afterwards — the jump is only one exit
    /// path; the statement keeps compiling on the main path.
    fn unwind_trys(&mut self, to_depth: usize) -> Result<(), CompileError> {
        if self.fx().trys.len() <= to_depth {
            return Ok(());
        }
        let saved = self.fx().trys.clone();
        while self.fx().trys.len() > to_depth {
            let t = self.fx().trys.pop().unwrap();
            for _ in 0..t.handlers {
                self.fx().emit(Instr::PopHandler);
            }
            if let Some(f) = t.finally {
                self.block_body(&f)?;
            }
        }
        self.fx().trys = saved;
        Ok(())
    }

    /// Pop the loop context: continues -> `continue_target`, patch the
    /// loop-exit jump (if any), then breaks -> here.
    fn finish_loop(&mut self, continue_target: u32, exit_jump: Option<usize>) {
        let ctx = self.fx().loops.pop().unwrap();
        let (breaks, continues) = (ctx.breaks, ctx.continues);
        for site in continues {
            match &mut self.fx().code[site] {
                Instr::Jump { target } => *target = continue_target,
                _ => unreachable!(),
            }
        }
        if let Some(jf) = exit_jump {
            self.fx().patch(jf);
        }
        for site in breaks {
            self.fx().patch(site);
        }
    }

    // ---- expressions ----

    /// Compile an optional chain (`a?.b`, `a?.b.c`, `a?.b?.c`, `a?.[k]`,
    /// `a?.b()`, `fn?.()`): every `?.` link that sees null/undefined
    /// short-circuits the whole rest of the chain to `undefined`.
    fn optional_chain(&mut self, e: &Expr) -> Result<u8, CompileError> {
        let mut bails: Vec<usize> = Vec::new();
        let dst = self.chain_into(e, &mut bails)?;
        if bails.is_empty() {
            return Ok(dst); // no optional link actually present
        }
        let jend = self.fx().emit(Instr::Jump { target: 0 });
        for b in &bails {
            self.fx().patch(*b);
        }
        // any bailed link lands here: the chain's value is undefined
        self.fx().emit(Instr::LoadUndef { dst });
        self.fx().patch(jend);
        Ok(dst)
    }

    /// Compile one link of a chain, pushing a bail jump for each `?.`
    /// that must short-circuit to the shared undefined epilogue.
    fn chain_into(
        &mut self,
        e: &Expr,
        bails: &mut Vec<usize>,
    ) -> Result<u8, CompileError> {
        match e {
            Expr::Member { obj, prop, optional } => {
                let base = self.chain_into(obj, bails)?;
                if *optional {
                    bails.push(self.fx().emit(
                        Instr::JumpIfNullish { cond: base, target: 0 },
                    ));
                }
                let dst = self.fx().alloc()?;
                match prop {
                    MemberProp::Static(name) => {
                        let atom = self.atom(name);
                        let ic = self.next_ic()?;
                        self.fx().emit(Instr::GetProp {
                            dst, obj: base, atom, ic,
                        });
                    }
                    MemberProp::Computed(key) => {
                        let rk = self.expr(key)?;
                        self.fx()
                            .emit(Instr::GetIndex { dst, obj: base, key: rk });
                    }
                }
                Ok(dst)
            }
            Expr::Call { callee, args, optional } => {
                // `a?.b(args)` — method call whose receiver may be nullish
                if let Expr::Member { obj, prop, optional: mopt } = &**callee {
                    // `a.b?.(args)` guards the method VALUE: fetch it
                    // explicitly, bail if nullish, then CallThis with
                    // the original receiver
                    if *optional {
                        let recv = self.chain_into(obj, bails)?;
                        if *mopt {
                            bails.push(self.fx().emit(
                                Instr::JumpIfNullish {
                                    cond: recv, target: 0,
                                },
                            ));
                        }
                        // evaluate a computed key BEFORE allocating rf:
                        // CallThis needs args contiguous at rf+1..
                        let rk = match prop {
                            MemberProp::Computed(kexpr) => {
                                Some(self.expr(kexpr)?)
                            }
                            MemberProp::Static(_) => None,
                        };
                        let rf = self.fx().alloc()?;
                        match prop {
                            MemberProp::Static(name) => {
                                let atom = self.atom(name);
                                let ic = self.next_ic()?;
                                self.fx().emit(Instr::GetProp {
                                    dst: rf, obj: recv, atom, ic,
                                });
                            }
                            MemberProp::Computed(_) => {
                                self.fx().emit(Instr::GetIndex {
                                    dst: rf,
                                    obj: recv,
                                    key: rk.unwrap(),
                                });
                            }
                        }
                        bails.push(self.fx().emit(
                            Instr::JumpIfNullish { cond: rf, target: 0 },
                        ));
                        for a in args {
                            let ra = self.fx().alloc()?;
                            self.expr_to(a, ra)?;
                        }
                        self.fx().emit(Instr::CallThis {
                            func: rf,
                            recv,
                            argc: args.len() as u8,
                        });
                        return Ok(rf);
                    }
                    let MemberProp::Static(name) = prop else {
                        return self
                            .err("optional computed method call not yet");
                    };
                    let recv = self.chain_into(obj, bails)?;
                    if *mopt {
                        bails.push(self.fx().emit(
                            Instr::JumpIfNullish { cond: recv, target: 0 },
                        ));
                    }
                    let atom = self.atom(name);
                    // CallMethod needs the receiver then args, contiguous
                    let rf = self.fx().alloc()?;
                    self.fx().emit(Instr::Move { dst: rf, src: recv });
                    for a in args {
                        let ra = self.fx().alloc()?;
                        self.expr_to(a, ra)?;
                    }
                    self.fx().emit(Instr::CallMethod {
                        obj: rf, atom, argc: args.len() as u8,
                    });
                    self.fx().tmp_top = rf + 1;
                    Ok(rf)
                } else {
                    // `fn?.(args)` — guard the callee value, then plain Call
                    let rf = self.fx().alloc()?;
                    let cv = self.chain_into(callee, bails)?;
                    self.fx().emit(Instr::Move { dst: rf, src: cv });
                    for a in args {
                        let ra = self.fx().alloc()?;
                        self.expr_to(a, ra)?;
                    }
                    if *optional {
                        bails.push(self.fx().emit(
                            Instr::JumpIfNullish { cond: rf, target: 0 },
                        ));
                    }
                    self.fx().emit(Instr::Call {
                        func: rf, argc: args.len() as u8,
                    });
                    self.fx().tmp_top = rf + 1;
                    Ok(rf)
                }
            }
            // the base of the chain (an ident, call result, etc.)
            _ => self.expr(e),
        }
    }

    /// Compile into `dst`, using only registers >= dst as scratch.
    fn expr_to(&mut self, e: &Expr, dst: u8) -> Result<(), CompileError> {
        let saved = self.fx().tmp_top;
        {
            let f = self.fx();
            f.tmp_top = f.tmp_top.max(dst + 1);
        }
        let r = self.expr(e)?;
        if r != dst {
            self.fx().emit(Instr::Move { dst, src: r });
        }
        self.fx().tmp_top = saved;
        Ok(())
    }

    fn expr(&mut self, e: &Expr) -> Result<u8, CompileError> {
        match e {
            Expr::Num(n) => {
                let r = self.fx().alloc()?;
                self.load_num(r, *n);
                Ok(r)
            }
            Expr::Bool(b) => {
                let r = self.fx().alloc()?;
                self.fx().emit(Instr::LoadBool { dst: r, val: *b });
                Ok(r)
            }
            Expr::Null => {
                let r = self.fx().alloc()?;
                let k = self.fx().const_idx(Value::NULL);
                self.fx().emit(Instr::LoadConst { dst: r, idx: k });
                Ok(r)
            }
            Expr::Str(s) => {
                let v = self.string_const(s);
                let r = self.fx().alloc()?;
                let k = self.fx().const_idx(v);
                self.fx().emit(Instr::LoadConst { dst: r, idx: k });
                Ok(r)
            }
            Expr::Regex { pattern, flags } => {
                let pv = self.string_const(pattern);
                let fv = self.string_const(flags);
                let pk = self.fx().const_idx(pv);
                let fk = self.fx().const_idx(fv);
                let r = self.fx().alloc()?;
                self.fx().emit(Instr::NewRegex { dst: r, pat: pk, flags: fk });
                Ok(r)
            }
            Expr::Ident(name) => match name.as_str() {
                "undefined" => {
                    let r = self.fx().alloc()?;
                    self.fx().emit(Instr::LoadUndef { dst: r });
                    Ok(r)
                }
                _ => {
                    // a read compiled before the declaration statement
                    // may execute in the TDZ — guard it
                    let tdz = self.tdz_pending(name);
                    match self.resolve(name) {
                        Place::Reg(r) => {
                            if tdz {
                                self.emit_tdz_check(name, r);
                            }
                            Ok(r)
                        }
                        p => {
                            let dst = self.fx().alloc()?;
                            self.load_place(p, dst);
                            if tdz {
                                self.emit_tdz_check(name, dst);
                            }
                            Ok(dst)
                        }
                    }
                }
            },
            Expr::Func(lit) => self.make_closure(lit, false),
            Expr::Arrow(lit) => self.make_closure(lit, true),
            Expr::Unary(op, operand) => self.unary(*op, operand),
            Expr::Update { op, prefix, target } => {
                self.update(*op, *prefix, target)
            }
            Expr::Binary(op, a, b) => {
                let ra = self.expr(a)?;
                // If `ra` aliases a local variable's register and
                // evaluating `b` might write to it (e.g. `x + (x = 3)`,
                // `arr[i] + i++`), snapshot it first so we read the
                // left operand's original value.
                let ra = if ra < self.fx().locals_end
                    && has_side_effects(b)
                {
                    let tmp = self.fx().alloc()?;
                    self.fx().emit(Instr::Move { dst: tmp, src: ra });
                    tmp
                } else {
                    ra
                };
                let rb = self.expr(b)?;
                // reuse the left operand's temp as the destination:
                // a long chain (a+b+c+...) then needs O(1) temps
                // instead of one per term (minified bundles have
                // thousand-term expressions)
                let dst = if ra >= self.fx().locals_end {
                    ra
                } else {
                    self.fx().alloc()?
                };
                let Some(i) = bin_instr(*op, dst, ra, rb) else {
                    return self.err(format!("operator {op:?} not yet"));
                };
                self.fx().emit(i);
                let f = self.fx();
                f.tmp_top = (dst + 1).max(f.locals_end);
                Ok(dst)
            }
            Expr::Object(props) => {
                let dst = self.fx().alloc()?;
                self.fx().emit(Instr::NewObject { dst });
                for p in props {
                    match &p.key {
                        PropKey::Ident(s) | PropKey::Str(s) => {
                            let name = s.clone();
                            let rv = self.fx().alloc()?;
                            self.expr_to(&p.value, rv)?;
                            let atom = self.atom(&name);
                            let ic = self.next_ic()?;
                            self.fx().emit(Instr::SetProp {
                                obj: dst, atom, src: rv, ic,
                            });
                            self.fx().tmp_top = rv;
                        }
                        // `{[expr]: v}` and `{0: v}` — dynamic key via SetIndex
                        PropKey::Computed(_) | PropKey::Num(_) => {
                            let rk = self.fx().alloc()?;
                            match &p.key {
                                PropKey::Computed(k) => self.expr_to(k, rk)?,
                                PropKey::Num(n) => self.load_num(rk, *n),
                                _ => unreachable!(),
                            }
                            let rv = self.fx().alloc()?;
                            self.expr_to(&p.value, rv)?;
                            self.fx().emit(Instr::SetIndex {
                                obj: dst, key: rk, src: rv,
                            });
                            self.fx().tmp_top = rk;
                        }
                    }
                }
                Ok(dst)
            }
            Expr::Member { obj, prop, optional } => {
                if *optional || chain_has_optional(obj) {
                    return self.optional_chain(e);
                }
                let ro = self.expr(obj)?;
                match prop {
                    MemberProp::Static(name) => {
                        let atom = self.atom(name);
                        let ic = self.next_ic()?;
                        let dst = self.fx().alloc()?;
                        self.fx().emit(Instr::GetProp {
                            dst, obj: ro, atom, ic,
                        });
                        Ok(dst)
                    }
                    MemberProp::Computed(key) => {
                        let rk = self.expr(key)?;
                        let dst = self.fx().alloc()?;
                        self.fx().emit(Instr::GetIndex {
                            dst, obj: ro, key: rk,
                        });
                        Ok(dst)
                    }
                }
            }
            Expr::Array(items) => {
                let dst = self.fx().alloc()?;
                self.fx().emit(Instr::NewArray { dst });
                for item in items {
                    let rv = self.fx().alloc()?;
                    self.expr_to(item, rv)?;
                    self.fx().emit(Instr::ArrayPush { arr: dst, src: rv });
                    self.fx().tmp_top = rv;
                }
                Ok(dst)
            }
            Expr::This => {
                let dst = self.fx().alloc()?;
                self.fx().emit(Instr::LoadThis { dst });
                Ok(dst)
            }
            Expr::Logical(op, a, b) => {
                let dst = self.fx().alloc()?;
                self.expr_to(a, dst)?;
                let j = match op {
                    LogOp::And => self
                        .fx()
                        .emit(Instr::JumpIfFalse { cond: dst, target: 0 }),
                    LogOp::Or => self
                        .fx()
                        .emit(Instr::JumpIfTrue { cond: dst, target: 0 }),
                    // a ?? b : keep a unless it is null/undefined
                    LogOp::Nullish => self.fx().emit(
                        Instr::JumpIfNotNullish { cond: dst, target: 0 },
                    ),
                };
                self.expr_to(b, dst)?;
                self.fx().patch(j);
                Ok(dst)
            }
            Expr::Cond(test, cons, alt) => {
                let rc = self.expr(test)?;
                let dst = self.fx().alloc()?;
                let jf = self
                    .fx()
                    .emit(Instr::JumpIfFalse { cond: rc, target: 0 });
                self.expr_to(cons, dst)?;
                let jend = self.fx().emit(Instr::Jump { target: 0 });
                self.fx().patch(jf);
                self.expr_to(alt, dst)?;
                self.fx().patch(jend);
                Ok(dst)
            }
            Expr::Assign(op, target, value) => {
                self.assign(*op, target, value)
            }
            Expr::Call { callee, args, optional } => {
                if args.len() > 200 {
                    return self.err("too many arguments");
                }
                if *optional || chain_has_optional(callee) {
                    return self.optional_chain(e);
                }
                // method call: callee in a register, args right after,
                // this = the receiver (same window layout as Call)
                if let Expr::Member { obj, prop, optional: mopt } = &**callee
                {
                    if *mopt {
                        return self.err("?. calls not yet compilable");
                    }
                    let MemberProp::Static(name) = prop else {
                        // o[k](args): fetch the callee dynamically but
                        // keep o as the receiver (`this`). Builtin
                        // methods (push, string methods, DOM) are not
                        // reachable this way yet — GetIndex only sees
                        // stored properties.
                        let MemberProp::Computed(kexpr) = prop else {
                            unreachable!()
                        };
                        let recv = self.fx().alloc()?;
                        self.expr_to(obj, recv)?;
                        let rk = self.fx().alloc()?;
                        self.expr_to(kexpr, rk)?;
                        let rf = self.fx().alloc()?;
                        self.fx().emit(Instr::GetIndex {
                            dst: rf,
                            obj: recv,
                            key: rk,
                        });
                        for a in args {
                            let ra = self.fx().alloc()?;
                            self.expr_to(a, ra)?;
                        }
                        self.fx().emit(Instr::CallThis {
                            func: rf,
                            recv,
                            argc: args.len() as u8,
                        });
                        self.fx().tmp_top = rf + 1;
                        return Ok(rf);
                    };
                    let atom = self.atom(name);
                    let rf = self.fx().alloc()?;
                    self.expr_to(obj, rf)?;
                    for a in args {
                        let ra = self.fx().alloc()?;
                        self.expr_to(a, ra)?;
                    }
                    self.fx().emit(Instr::CallMethod {
                        obj: rf,
                        atom,
                        argc: args.len() as u8,
                    });
                    self.fx().tmp_top = rf + 1;
                    return Ok(rf);
                }
                let rf = self.fx().alloc()?;
                self.expr_to(callee, rf)?;
                for a in args {
                    let ra = self.fx().alloc()?;
                    self.expr_to(a, ra)?;
                }
                self.fx()
                    .emit(Instr::Call { func: rf, argc: args.len() as u8 });
                self.fx().tmp_top = rf + 1;
                Ok(rf)
            }
            Expr::Seq(items) => {
                // non-final items are evaluated for effect only:
                // release their temps, or a 400-term comma sequence
                // (core-js entry: `e(1),e(2),...`) eats the register file
                let mut last = 0;
                for (i, e) in items.iter().enumerate() {
                    let checkpoint = self.fx().tmp_top;
                    last = self.expr(e)?;
                    if i + 1 < items.len() {
                        let f = self.fx();
                        f.tmp_top = checkpoint.max(f.locals_end);
                    }
                }
                Ok(last)
            }
            Expr::Template(parts) => {
                // Seed the accumulator with a guaranteed string so every
                // `Add` concatenates (and a lone `${x}` coerces to string).
                let dst = self.fx().alloc()?;
                let rest = match parts.first() {
                    Some(TplPart::Chunk(s)) => {
                        let v = self.string_const(s);
                        let k = self.fx().const_idx(v);
                        self.fx().emit(Instr::LoadConst { dst, idx: k });
                        &parts[1..]
                    }
                    _ => {
                        let v = self.string_const("");
                        let k = self.fx().const_idx(v);
                        self.fx().emit(Instr::LoadConst { dst, idx: k });
                        &parts[..]
                    }
                };
                for part in rest {
                    let r = match part {
                        TplPart::Chunk(s) => {
                            let v = self.string_const(s);
                            let rr = self.fx().alloc()?;
                            let k = self.fx().const_idx(v);
                            self.fx().emit(Instr::LoadConst { dst: rr, idx: k });
                            rr
                        }
                        TplPart::Expr(e) => {
                            let rr = self.fx().alloc()?;
                            self.expr_to(e, rr)?;
                            rr
                        }
                    };
                    self.fx().emit(Instr::Add { dst, a: dst, b: r });
                    self.fx().tmp_top = dst + 1;
                }
                Ok(dst)
            }
            Expr::New { callee, args } => {
                if let Expr::Ident(name) = &**callee {
                    if name == "Promise" && !args.is_empty() {
                        let rexec = self.fx().alloc()?;
                        self.expr_to(&args[0], rexec)?;
                        self.fx()
                            .emit(Instr::NewPromise { executor: rexec });
                        return Ok(rexec);
                    }
                }
                // Keep ctor + arguments contiguous (the ordinary call
                // convention), then let the VM perform [[Construct]].
                // This is observably different from a method call for a
                // callable Proxy, whose `construct` trap must win over
                // its `apply` trap.
                let rc = self.fx().alloc()?;
                self.expr_to(callee, rc)?;
                for a in args {
                    let ra = self.fx().alloc()?;
                    self.expr_to(a, ra)?;
                }
                self.fx().emit(Instr::Construct {
                    ctor: rc,
                    argc: args.len() as u8,
                });
                self.fx().tmp_top = rc + 1;
                Ok(rc)
            }
            Expr::Await(_) => self.err(
                "await only supported at statement level in an async fn \
                 (var x = await E; / await E; / return await E;)",
            ),
            e => self.err(format!("not yet compilable: {e:?}")),
        }
    }

    fn load_num(&mut self, dst: u8, n: f64) {
        if n.fract() == 0.0
            && n >= i32::MIN as f64
            && n <= i32::MAX as f64
            && !(n == 0.0 && n.is_sign_negative())
        {
            self.fx().emit(Instr::LoadInt { dst, val: n as i32 });
        } else {
            let k = self.fx().const_idx(Value::number(n));
            self.fx().emit(Instr::LoadConst { dst, idx: k });
        }
    }

    fn unary(
        &mut self,
        op: UnOp,
        operand: &Expr,
    ) -> Result<u8, CompileError> {
        if op == UnOp::Typeof {
            // typeof must not throw on undeclared globals
            let rs = match operand {
                Expr::Ident(name) if name != "undefined" => {
                    match self.resolve(name) {
                        Place::Global(atom) => {
                            let r = self.fx().alloc()?;
                            self.fx()
                                .emit(Instr::GetGlobalSafe { dst: r, atom });
                            r
                        }
                        p => {
                            let r = self.fx().alloc()?;
                            self.load_place(p, r);
                            // typeof does NOT spare TDZ reads, only
                            // undeclared globals
                            if self.tdz_pending(name) {
                                self.emit_tdz_check(name, r);
                            }
                            r
                        }
                    }
                }
                _ => self.expr(operand)?,
            };
            let dst = self.fx().alloc()?;
            self.fx().emit(Instr::TypeOf { dst, src: rs });
            return Ok(dst);
        }
        if op == UnOp::Delete {
            if let Expr::Member { obj, prop, .. } = operand {
                let ro = self.expr(obj)?;
                let rk = match prop {
                    MemberProp::Static(name) => {
                        let v = self.string_const(name);
                        let r = self.fx().alloc()?;
                        let k = self.fx().const_idx(v);
                        self.fx().emit(Instr::LoadConst { dst: r, idx: k });
                        r
                    }
                    MemberProp::Computed(k) => self.expr(k)?,
                };
                let dst = self.fx().alloc()?;
                self.fx().emit(Instr::Delete { dst, obj: ro, key: rk });
                return Ok(dst);
            }
            // `delete ident` never removes a binding; evaluating the
            // ident could throw ReferenceError, so don't
            if !matches!(operand, Expr::Ident(_)) {
                self.expr(operand)?;
            }
            let dst = self.fx().alloc()?;
            self.fx().emit(Instr::LoadBool { dst, val: true });
            return Ok(dst);
        }
        let rs = self.expr(operand)?;
        let dst = self.fx().alloc()?;
        match op {
            UnOp::Neg => self.fx().emit(Instr::Neg { dst, src: rs }),
            UnOp::Not => self.fx().emit(Instr::Not { dst, src: rs }),
            UnOp::Pos => {
                // +x is ToNumber(x): +"42" -> 42, +{} -> NaN
                self.fx().emit(Instr::ToNum { dst, src: rs })
            }
            UnOp::Void => self.fx().emit(Instr::LoadUndef { dst }),
            UnOp::BitNot => self.fx().emit(Instr::BitNot { dst, src: rs }),
            op => return self.err(format!("unary {op:?} not yet")),
        };
        Ok(dst)
    }

    fn update(
        &mut self,
        op: UpdateOp,
        prefix: bool,
        target: &Expr,
    ) -> Result<u8, CompileError> {
        if let Expr::Member { obj, prop, .. } = target {
            // o.x++ / o[k]-- : read, ToNumber, add/sub 1, write back;
            // postfix yields the old (numeric) value, prefix the new
            let ro = self.expr(obj)?;
            let mut computed_key = None;
            let cur = self.fx().alloc()?;
            match prop {
                MemberProp::Static(name) => {
                    let atom = self.atom(name);
                    let ic = self.next_ic()?;
                    self.fx().emit(Instr::GetProp {
                        dst: cur, obj: ro, atom, ic,
                    });
                }
                MemberProp::Computed(k) => {
                    let rk = self.expr(k)?;
                    computed_key = Some(rk);
                    self.fx().emit(Instr::GetIndex {
                        dst: cur, obj: ro, key: rk,
                    });
                }
            }
            self.fx().emit(Instr::ToNum { dst: cur, src: cur });
            let one = self.fx().alloc()?;
            self.fx().emit(Instr::LoadInt { dst: one, val: 1 });
            let newv = self.fx().alloc()?;
            match op {
                UpdateOp::Inc => {
                    self.fx().emit(Instr::Add { dst: newv, a: cur, b: one });
                }
                UpdateOp::Dec => {
                    self.fx().emit(Instr::Sub { dst: newv, a: cur, b: one });
                }
            }
            match prop {
                MemberProp::Static(name) => {
                    let atom = self.atom(name);
                    let ic = self.next_ic()?;
                    self.fx().emit(Instr::SetProp {
                        obj: ro, atom, src: newv, ic,
                    });
                }
                MemberProp::Computed(_) => {
                    let rk = computed_key.unwrap();
                    self.fx().emit(Instr::SetIndex {
                        obj: ro, key: rk, src: newv,
                    });
                }
            }
            return Ok(if prefix { newv } else { cur });
        }
        let Expr::Ident(name) = target else {
            return self.err("++/-- on this target not yet");
        };
        self.check_assignable(name)?;
        let tdz = self.tdz_pending(name);
        let place = self.resolve(name);
        let one = self.fx().alloc()?;
        self.fx().emit(Instr::LoadInt { dst: one, val: 1 });
        if let Place::Reg(reg) = place {
            // fast path: pure register variable
            if tdz {
                self.emit_tdz_check(name, reg);
            }
            if prefix {
                self.emit_update(op, reg, reg, one);
                Ok(reg)
            } else {
                let old = self.fx().alloc()?;
                self.fx().emit(Instr::Move { dst: old, src: reg });
                self.emit_update(op, reg, reg, one);
                Ok(old)
            }
        } else {
            let cur = self.fx().alloc()?;
            self.load_place(place, cur);
            if tdz {
                self.emit_tdz_check(name, cur);
            }
            if prefix {
                self.emit_update(op, cur, cur, one);
                self.store_place(place, cur);
                Ok(cur)
            } else {
                let updated = self.fx().alloc()?;
                self.emit_update(op, updated, cur, one);
                self.store_place(place, updated);
                Ok(cur)
            }
        }
    }

    fn emit_update(&mut self, op: UpdateOp, dst: u8, a: u8, b: u8) {
        match op {
            UpdateOp::Inc => self.fx().emit(Instr::Add { dst, a, b }),
            UpdateOp::Dec => self.fx().emit(Instr::Sub { dst, a, b }),
        };
    }

    fn assign(
        &mut self,
        op: AssignOp,
        target: &Expr,
        value: &Expr,
    ) -> Result<u8, CompileError> {
        // Logical assignment short-circuits: `a ||= b` evaluates and assigns
        // b only when a is falsy (`&&=` when truthy, `??=` when nullish).
        // Desugar to `a <logop> (a = b)` and reuse the logical + plain-assign
        // paths. (`a` is read for the test and again in the assign target;
        // for member targets the object subexpression is evaluated twice —
        // an accepted deviation from the once-evaluated reference spec.)
        if let AssignOp::Log(logop) = op {
            let plain = Expr::Assign(
                AssignOp::Plain,
                Box::new(target.clone()),
                Box::new(value.clone()),
            );
            let logical = Expr::Logical(
                logop,
                Box::new(target.clone()),
                Box::new(plain),
            );
            return self.expr(&logical);
        }
        match target {
            Expr::Ident(name) => match op {
                AssignOp::Plain => {
                    self.check_assignable(name)?;
                    let place = self.resolve(name);
                    if self.tdz_pending(name) {
                        // write may run in the TDZ: value first, then
                        // check the binding's current (marker) value
                        let rv = self.expr(value)?;
                        let cur = self.fx().alloc()?;
                        self.load_place(place, cur);
                        self.emit_tdz_check(name, cur);
                        self.store_place(place, rv);
                        return Ok(rv);
                    }
                    if let Place::Reg(reg) = place {
                        self.expr_to(value, reg)?;
                        Ok(reg)
                    } else {
                        let rv = self.expr(value)?;
                        self.store_place(place, rv);
                        Ok(rv)
                    }
                }
                AssignOp::Bin(bop) => {
                    self.check_assignable(name)?;
                    // the read inside `combined` carries the TDZ check
                    let combined = Expr::Binary(
                        bop,
                        Box::new(target.clone()),
                        Box::new(value.clone()),
                    );
                    let rv = self.expr(&combined)?;
                    self.store_name(name, rv);
                    if let Place::Reg(reg) = self.resolve(name) {
                        Ok(reg)
                    } else {
                        Ok(rv)
                    }
                }
                AssignOp::Log(_) => unreachable!("logical assignment desugared in assign()"),
            },
            Expr::Member { obj, prop, optional } => {
                if *optional {
                    return self.err("?. assignment is invalid");
                }
                let MemberProp::Static(name) = prop else {
                    // computed: o[k] = v / o[k] op= v
                    let MemberProp::Computed(key) = prop else {
                        unreachable!()
                    };
                    let ro = self.expr(obj)?;
                    let rk = self.expr(key)?;
                    return match op {
                        AssignOp::Plain => {
                            let rv = self.fx().alloc()?;
                            self.expr_to(value, rv)?;
                            self.fx().emit(Instr::SetIndex {
                                obj: ro, key: rk, src: rv,
                            });
                            Ok(rv)
                        }
                        AssignOp::Bin(bop) => {
                            let cur = self.fx().alloc()?;
                            self.fx().emit(Instr::GetIndex {
                                dst: cur, obj: ro, key: rk,
                            });
                            let rv = self.expr(value)?;
                            let dst = self.fx().alloc()?;
                            let Some(i) = bin_instr(bop, dst, cur, rv)
                            else {
                                return self.err(format!(
                                    "operator {bop:?} not yet"
                                ));
                            };
                            self.fx().emit(i);
                            self.fx().emit(Instr::SetIndex {
                                obj: ro, key: rk, src: dst,
                            });
                            Ok(dst)
                        }
                        AssignOp::Log(_) => {
                            unreachable!("logical assignment desugared in assign()")
                        }
                    };
                };
                // evaluate the object exactly once
                let ro = self.expr(obj)?;
                let atom = self.atom(name);
                match op {
                    AssignOp::Plain => {
                        let rv = self.fx().alloc()?;
                        self.expr_to(value, rv)?;
                        let ic = self.next_ic()?;
                        self.fx().emit(Instr::SetProp {
                            obj: ro, atom, src: rv, ic,
                        });
                        Ok(rv)
                    }
                    AssignOp::Bin(bop) => {
                        let cur = self.fx().alloc()?;
                        let ic_get = self.next_ic()?;
                        self.fx().emit(Instr::GetProp {
                            dst: cur, obj: ro, atom, ic: ic_get,
                        });
                        let rv = self.expr(value)?;
                        let dst = self.fx().alloc()?;
                        let Some(i) = bin_instr(bop, dst, cur, rv) else {
                            return self
                                .err(format!("operator {bop:?} not yet"));
                        };
                        self.fx().emit(i);
                        let ic_set = self.next_ic()?;
                        self.fx().emit(Instr::SetProp {
                            obj: ro, atom, src: dst, ic: ic_set,
                        });
                        Ok(dst)
                    }
                    AssignOp::Log(_) => {
                        unreachable!("logical assignment desugared in assign()")
                    }
                }
            }
            _ => self.err("invalid assignment target"),
        }
    }
}

/// Register-to-register instruction for a binary operator, if the VM
/// has one.
fn bin_instr(op: BinOp, dst: u8, a: u8, b: u8) -> Option<Instr> {
    Some(match op {
        BinOp::In => Instr::In { dst, a, b },
        BinOp::InstanceOf => Instr::InstanceOf { dst, a, b },
        BinOp::Add => Instr::Add { dst, a, b },
        BinOp::Sub => Instr::Sub { dst, a, b },
        BinOp::Mul => Instr::Mul { dst, a, b },
        BinOp::Div => Instr::Div { dst, a, b },
        BinOp::Mod => Instr::Mod { dst, a, b },
        BinOp::Pow => Instr::Pow { dst, a, b },
        BinOp::Lt => Instr::Lt { dst, a, b },
        BinOp::LtEq => Instr::LtEq { dst, a, b },
        BinOp::Gt => Instr::Gt { dst, a, b },
        BinOp::GtEq => Instr::GtEq { dst, a, b },
        BinOp::StrictEq => Instr::StrictEq { dst, a, b },
        BinOp::StrictNotEq => Instr::StrictNotEq { dst, a, b },
        BinOp::EqEq => Instr::LooseEq { dst, a, b },
        BinOp::NotEq => Instr::LooseNotEq { dst, a, b },
        BinOp::Shl => Instr::Shl { dst, a, b },
        BinOp::Shr => Instr::Shr { dst, a, b },
        BinOp::UShr => Instr::UShr { dst, a, b },
        BinOp::BitAnd => Instr::BitAnd { dst, a, b },
        BinOp::BitOr => Instr::BitOr { dst, a, b },
        BinOp::BitXor => Instr::BitXor { dst, a, b },
    })
}
