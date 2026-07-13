//! Register bytecode. Calls use the Lua convention: the callee sits in
//! register F and its arguments in F+1..F+1+argc, so a call reuses the
//! caller's register window as the callee's frame with zero copying.

use super::value::Value;

#[derive(Debug, Clone, Copy)]
pub enum Instr {
    LoadConst { dst: u8, idx: u16 },
    LoadInt { dst: u8, val: i32 },
    LoadUndef { dst: u8 },
    LoadBool { dst: u8, val: bool },
    Move { dst: u8, src: u8 },
    GetGlobal { dst: u8, atom: u16 },
    SetGlobal { atom: u16, src: u8 },
    Add { dst: u8, a: u8, b: u8 },
    Sub { dst: u8, a: u8, b: u8 },
    Mul { dst: u8, a: u8, b: u8 },
    Div { dst: u8, a: u8, b: u8 },
    Mod { dst: u8, a: u8, b: u8 },
    Pow { dst: u8, a: u8, b: u8 },
    Neg { dst: u8, src: u8 },
    Not { dst: u8, src: u8 },
    /// dst = ToNumber(src) — the unary `+` operator
    ToNum { dst: u8, src: u8 },
    Lt { dst: u8, a: u8, b: u8 },
    LtEq { dst: u8, a: u8, b: u8 },
    Gt { dst: u8, a: u8, b: u8 },
    GtEq { dst: u8, a: u8, b: u8 },
    StrictEq { dst: u8, a: u8, b: u8 },
    StrictNotEq { dst: u8, a: u8, b: u8 },
    LooseEq { dst: u8, a: u8, b: u8 },
    LooseNotEq { dst: u8, a: u8, b: u8 },
    Jump { target: u32 },
    JumpIfFalse { cond: u8, target: u32 },
    JumpIfTrue { cond: u8, target: u32 },
    /// jump if reg is null/undefined (optional chaining bail)
    JumpIfNullish { cond: u8, target: u32 },
    /// jump if reg is NOT null/undefined (`??` short-circuit)
    JumpIfNotNullish { cond: u8, target: u32 },
    Call { func: u8, argc: u8 },
    Return { src: u8 },
    ReturnUndef,
    /// Instantiate proto as a closure, capturing per proto.captures.
    Closure { dst: u8, proto: u16 },
    /// Box the current value of `reg` into a fresh heap cell and leave
    /// the cell reference in `reg` (function prologue, captured locals).
    CellWrap { reg: u8 },
    /// dst = *cell(src)
    LoadCell { dst: u8, src: u8 },
    /// *cell(dst) = src
    StoreCell { dst: u8, src: u8 },
    GetUpval { dst: u8, idx: u16 },
    SetUpval { idx: u16, src: u8 },
    NewObject { dst: u8 },
    /// `new Promise(executor)`: `executor` holds the executor fn; after,
    /// the same reg holds the new promise. The VM calls executor(resolve,
    /// reject) with resolvers bound to that promise (P3b).
    NewPromise { executor: u8 },
    /// dst = a RegExp from string constants pat/flags (regex literal).
    NewRegex { dst: u8, pat: u16, flags: u16 },
    /// bitwise ops (JS ToInt32/ToUint32 semantics)
    Shl { dst: u8, a: u8, b: u8 },
    Shr { dst: u8, a: u8, b: u8 },
    UShr { dst: u8, a: u8, b: u8 },
    BitAnd { dst: u8, a: u8, b: u8 },
    BitOr { dst: u8, a: u8, b: u8 },
    BitXor { dst: u8, a: u8, b: u8 },
    BitNot { dst: u8, src: u8 },
    /// dst = the `arguments` array (function prologue; reads the
    /// current frame's actual argument count).
    Arguments { dst: u8 },
    /// dst = a fresh object whose [[Prototype]] is ctor.prototype
    /// (the allocation half of `new`; the call half is CallThis).
    NewInstance { dst: u8, ctor: u8 },
    /// dst = a if a is an object else b (a ctor's explicit object
    /// return wins over the fresh instance, per spec).
    SelectObj { dst: u8, a: u8, b: u8 },
    /// dst = `a in b` — own-property membership.
    In { dst: u8, a: u8, b: u8 },
    /// dst = `delete obj[key]` — removes an own property (the object
    /// moves to a fresh shape, so inline caches self-invalidate).
    Delete { dst: u8, obj: u8, key: u8 },
    /// dst = `a instanceof b` — built-in ctors matched by identity;
    /// user functions yield false (the engine has no prototype chains).
    InstanceOf { dst: u8, a: u8, b: u8 },
    /// dst = obj.atom — `ic` indexes the VM's inline-cache table; a
    /// monomorphic hit turns the lookup into one shape check + slot read.
    GetProp { dst: u8, obj: u8, atom: u16, ic: u16 },
    /// obj.atom = src (adds the property via a shape transition if new)
    SetProp { obj: u8, atom: u16, src: u8, ic: u16 },
    NewArray { dst: u8 },
    /// arr.push(src) without the method dispatch (array literals)
    ArrayPush { arr: u8, src: u8 },
    /// dst = a fresh array of `obj`'s enumerable keys as strings, in
    /// insertion order (array indices first). Backs `for..in`.
    ForInKeys { dst: u8, obj: u8 },
    /// dst = obj[key] (integer keys on arrays)
    GetIndex { dst: u8, obj: u8, key: u8 },
    SetIndex { obj: u8, key: u8, src: u8 },
    /// obj.atom(args) — callee in `obj`, args right after, this = obj.
    /// Dispatches builtins (push/sort/toFixed/…) or a stored function.
    CallMethod { obj: u8, atom: u16, argc: u8 },
    LoadThis { dst: u8 },
    /// dst = typeof src (as an interned string)
    TypeOf { dst: u8, src: u8 },
    /// like GetGlobal but yields undefined instead of throwing
    /// (the `typeof x` special case)
    GetGlobalSafe { dst: u8, atom: u16 },
    /// Throw a ReferenceError if `src` holds the TDZ marker — emitted
    /// for reads/writes of a `let`/`const` that may run before its
    /// declaration. `atom` names the variable for the error message.
    TdzCheck { src: u8, atom: u16 },
    /// Arm an exception handler in the current frame: a throw while it
    /// is armed resumes at `catch_ip` with the thrown value in `exc`.
    PushHandler { catch_ip: u32, exc: u8 },
    /// Disarm the innermost handler (normal try-block completion).
    PopHandler,
    /// Throw the value in `src`.
    Throw { src: u8 },
    /// Like Call, but the callee gets `this` from register `recv` —
    /// computed member calls `o[k](...)` keep `o` as the receiver.
    CallThis { func: u8, recv: u8, argc: u8 },
}

/// Where a closure's captured variable comes from, resolved at compile
/// time against the *enclosing* frame at Closure-instruction time.
#[derive(Debug, Clone, Copy)]
pub enum CapSrc {
    /// A cell-wrapped local register of the enclosing function.
    LocalCell(u8),
    /// An upvalue of the enclosing function (transitive capture).
    Upval(u16),
}

pub struct FuncProto {
    pub name: String,
    pub nparams: u8,
    pub nregs: u8,
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
