//! Register bytecode. Calls use the Lua convention: the callee sits in
//! register F and its arguments in F+1..F+1+argc, so a call reuses the
//! caller's register window as the callee's frame with zero copying.

use super::value::Value;

#[derive(Debug, Clone, Copy)]
pub enum Instr {
    LoadConst { dst: u16, idx: u16 },
    LoadInt { dst: u16, val: i32 },
    LoadUndef { dst: u16 },
    LoadBool { dst: u16, val: bool },
    Move { dst: u16, src: u16 },
    GetGlobal { dst: u16, atom: u16 },
    SetGlobal { atom: u16, src: u16 },
    Add { dst: u16, a: u16, b: u16 },
    Sub { dst: u16, a: u16, b: u16 },
    Mul { dst: u16, a: u16, b: u16 },
    Div { dst: u16, a: u16, b: u16 },
    Mod { dst: u16, a: u16, b: u16 },
    Pow { dst: u16, a: u16, b: u16 },
    Neg { dst: u16, src: u16 },
    Not { dst: u16, src: u16 },
    /// dst = ToNumber(src) — the unary `+` operator
    ToNum { dst: u16, src: u16 },
    Lt { dst: u16, a: u16, b: u16 },
    LtEq { dst: u16, a: u16, b: u16 },
    Gt { dst: u16, a: u16, b: u16 },
    GtEq { dst: u16, a: u16, b: u16 },
    StrictEq { dst: u16, a: u16, b: u16 },
    StrictNotEq { dst: u16, a: u16, b: u16 },
    LooseEq { dst: u16, a: u16, b: u16 },
    LooseNotEq { dst: u16, a: u16, b: u16 },
    Jump { target: u32 },
    JumpIfFalse { cond: u16, target: u32 },
    JumpIfTrue { cond: u16, target: u32 },
    /// jump if reg is null/undefined (optional chaining bail)
    JumpIfNullish { cond: u16, target: u32 },
    /// jump if reg is NOT null/undefined (`??` short-circuit)
    JumpIfNotNullish { cond: u16, target: u32 },
    Call { func: u16, argc: u8 },
    Return { src: u16 },
    ReturnUndef,
    /// Instantiate proto as a closure, capturing per proto.captures.
    Closure { dst: u16, proto: u16 },
    /// Box the current value of `reg` into a fresh heap cell and leave
    /// the cell reference in `reg` (function prologue, captured locals).
    CellWrap { reg: u16 },
    /// dst = *cell(src)
    LoadCell { dst: u16, src: u16 },
    /// *cell(dst) = src
    StoreCell { dst: u16, src: u16 },
    GetUpval { dst: u16, idx: u16 },
    SetUpval { idx: u16, src: u16 },
    NewObject { dst: u16 },
    /// `new Promise(executor)`: `executor` holds the executor fn; after,
    /// the same reg holds the new promise. The VM calls executor(resolve,
    /// reject) with resolvers bound to that promise (P3b).
    NewPromise { executor: u16 },
    /// dst = a RegExp from string constants pat/flags (regex literal).
    NewRegex { dst: u16, pat: u16, flags: u16 },
    /// bitwise ops (JS ToInt32/ToUint32 semantics)
    Shl { dst: u16, a: u16, b: u16 },
    Shr { dst: u16, a: u16, b: u16 },
    UShr { dst: u16, a: u16, b: u16 },
    BitAnd { dst: u16, a: u16, b: u16 },
    BitOr { dst: u16, a: u16, b: u16 },
    BitXor { dst: u16, a: u16, b: u16 },
    BitNot { dst: u16, src: u16 },
    /// dst = the `arguments` array (function prologue; reads the
    /// current frame's actual argument count).
    Arguments { dst: u16 },
    /// dst = a fresh object whose [[Prototype]] is ctor.prototype
    /// (the allocation half of `new`; the call half is CallThis).
    NewInstance { dst: u16, ctor: u16 },
    /// dst = a if a is an object else b (a ctor's explicit object
    /// return wins over the fresh instance, per spec).
    SelectObj { dst: u16, a: u16, b: u16 },
    /// dst = `a in b` — own-property membership.
    In { dst: u16, a: u16, b: u16 },
    /// dst = `delete obj[key]` — removes an own property (the object
    /// moves to a fresh shape, so inline caches self-invalidate).
    Delete { dst: u16, obj: u16, key: u16 },
    /// dst = `a instanceof b` — built-in ctors matched by identity;
    /// user functions yield false (the engine has no prototype chains).
    InstanceOf { dst: u16, a: u16, b: u16 },
    /// dst = obj.atom — `ic` indexes the VM's inline-cache table; a
    /// monomorphic hit turns the lookup into one shape check + slot read.
    GetProp { dst: u16, obj: u16, atom: u16, ic: u16 },
    /// obj.atom = src (adds the property via a shape transition if new)
    SetProp { obj: u16, atom: u16, src: u16, ic: u16 },
    NewArray { dst: u16 },
    /// arr.push(src) without the method dispatch (array literals)
    ArrayPush { arr: u16, src: u16 },
    /// dst = a fresh array of `obj`'s enumerable keys as strings, in
    /// insertion order (array indices first). Backs `for..in`.
    ForInKeys { dst: u16, obj: u16 },
    /// for-of source: arrays pass through; Map/Set and objects with
    /// an `@@iterator` are materialized into a fresh array.
    IterMaterialize { dst: u16, obj: u16 },
    /// dst = obj[key] (integer keys on arrays)
    GetIndex { dst: u16, obj: u16, key: u16 },
    SetIndex { obj: u16, key: u16, src: u16 },
    /// obj.atom(args) — callee in `obj`, args right after, this = obj.
    /// Dispatches builtins (push/sort/toFixed/…) or a stored function.
    CallMethod { obj: u16, atom: u16, argc: u8 },
    LoadThis { dst: u16 },
    /// dst = typeof src (as an interned string)
    TypeOf { dst: u16, src: u16 },
    /// like GetGlobal but yields undefined instead of throwing
    /// (the `typeof x` special case)
    GetGlobalSafe { dst: u16, atom: u16 },
    /// Throw a ReferenceError if `src` holds the TDZ marker — emitted
    /// for reads/writes of a `let`/`const` that may run before its
    /// declaration. `atom` names the variable for the error message.
    TdzCheck { src: u16, atom: u16 },
    /// Arm an exception handler in the current frame: a throw while it
    /// is armed resumes at `catch_ip` with the thrown value in `exc`.
    PushHandler { catch_ip: u32, exc: u16 },
    /// Disarm the innermost handler (normal try-block completion).
    PopHandler,
    /// Throw the value in `src`.
    Throw { src: u16 },
    /// Like Call, but the callee gets `this` from register `recv` —
    /// computed member calls `o[k](...)` keep `o` as the receiver.
    CallThis { func: u16, recv: u16, argc: u8 },
}

/// Where a closure's captured variable comes from, resolved at compile
/// time against the *enclosing* frame at Closure-instruction time.
#[derive(Debug, Clone, Copy)]
pub enum CapSrc {
    /// A cell-wrapped local register of the enclosing function.
    LocalCell(u16),
    /// An upvalue of the enclosing function (transitive capture).
    Upval(u16),
}

pub struct FuncProto {
    pub name: String,
    pub nparams: u8,
    pub nregs: u16,
    pub code: Vec<Instr>,
    /// Pre-baked constants (numbers; function values are made by
    /// the Closure instruction, never stored here).
    pub consts: Vec<Value>,
    /// What this function captures from its enclosing frame.
    pub captures: Vec<CapSrc>,
    /// Arrow functions capture `this` lexically at creation time
    /// instead of receiving it from the call site.
    pub is_arrow: bool,
    /// The body references `arguments`: calls must keep extra args
    /// (beyond nparams) alive for the prologue's Arguments instruction.
    pub uses_arguments: bool,
    /// Present = this is a lazy stub: the body has not been compiled
    /// yet (code is empty). The VM compiles it on first call and
    /// redirects to the compiled proto. `captures` above are real —
    /// they were pre-resolved at deferral time so the Closure
    /// instruction works unchanged.
    pub lazy: Option<Box<LazySrc>>,
}

/// Everything needed to compile a deferred function body on first
/// call (lazy compilation — most bundle functions never run).
pub struct LazySrc {
    pub lit: std::rc::Rc<super::ast::FuncLit>,
    pub is_arrow: bool,
    /// capture names in upval order (parallel to FuncProto.captures);
    /// captured enclosing spill objects appear under their %spillN name
    pub upval_names: Vec<String>,
    /// free names that live in an enclosing function's spill object:
    /// (variable name, that spill object's %spillN upval name)
    pub spill_names: Vec<(String, String)>,
}

pub struct Module {
    pub protos: Vec<FuncProto>,
    /// atom index -> name (globals, and later property keys)
    pub atoms: Vec<String>,
    /// interned string literals; Value::string(idx) points here
    pub strings: Vec<String>,
    /// proto index of the top-level script code
    pub main: u32,
    /// number of property-access inline-cache sites in the module
    pub n_ics: u16,
}
