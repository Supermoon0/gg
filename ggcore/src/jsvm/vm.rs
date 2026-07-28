//! The register VM dispatch loop.
//!
//! One flat register stack; a call's frame starts right after the
//! callee register, so arguments are already in place (Lua-style
//! overlapping windows) and Return writes to `base - 1`.
//!
//! The VM is *multi-module*: a page loads many scripts (and event
//! handlers) that share globals, heap, and DOM. Compile-time atoms are
//! per-module, so every module carries a `global_map` translating its
//! atoms into VM-wide name ids; globals and property keys share that
//! namespace.
//!
//! `exec` runs ONE activation to completion and is reentrant: natives
//! that take JS callbacks (sort comparators, DOM event handlers)
//! re-enter the interpreter through `call_value`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::bytecode::{CapSrc, Instr, Module};
use super::value::Value;
use crate::dom;
use crate::dom_api::{find_tag, query, query_within, serialize_children};
use crate::html;

pub struct VmError {
    pub msg: String,
    /// The JS value of an explicit `throw`; engine errors carry None
    /// and materialize as an Error-like object when a catch needs one.
    pub value: Option<Value>,
    /// Error class an engine-raised error materializes as ("Error",
    /// "TypeError", ...). Lets `catch (e)` see e.name/instanceof match
    /// what real JS would throw.
    pub kind: &'static str,
    /// Call frames captured the first time this error unwound past a
    /// function boundary. An engine-raised error is materialized at the
    /// catch site, long after the frames that threw are gone, so the
    /// trace has to be snapshotted on the way out -- and an error that
    /// is never caught still needs it, or the host reports a bare
    /// message with no way to tell which of a page's thousand scripts
    /// raised it.
    pub trace: Option<String>,
}

impl VmError {
    /// How the host reports this error. Plain message by default —
    /// the log vec is the engine's observable contract and callers
    /// match on it. `GG_JS_TRACE=1` appends the captured frames, which
    /// is the only way to tell *which* of a real page's thousand
    /// bundled functions raised a "cannot call .push() of undefined".
    pub fn report(&self) -> String {
        match &self.trace {
            Some(t) if trace_enabled() => format!("{}\n{t}", self.msg),
            _ => self.msg.clone(),
        }
    }
}

/// Per-instruction trace for one function by name (GG_TRACE_FN).
/// Diagnostic-only: the env read is cached, the name compare only
/// runs when the variable is set.
/// Whether GG_TRACE_FN is set at all. The interpreter checks this once
/// per instruction, so the answer has to cost a load and a branch —
/// not an Rc deref and a substring search.
fn fn_trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("GG_TRACE_FN").is_ok_and(|v| !v.is_empty())
    })
}

fn fn_trace_wanted(name: &str) -> bool {
    static PAT: std::sync::OnceLock<Option<String>> =
        std::sync::OnceLock::new();
    match PAT.get_or_init(|| std::env::var("GG_TRACE_FN").ok()) {
        Some(p) if !p.is_empty() => name.contains(p.as_str()),
        _ => false,
    }
}

fn trace_enabled() -> bool {
    thread_local! {
        static ON: bool = std::env::var("GG_JS_TRACE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
    }
    ON.with(|v| *v)
}

impl std::fmt::Debug for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error: {}", self.msg)
    }
}

/// First register of a fresh activation that must be blanked. The
/// caller wrote the real arguments at `new_base + 0 .. argc`;
/// everything above that is still the previous occupant of this
/// register window and has to be cleared or the callee reads another
/// frame's values as its own.
///
/// A function that reads `arguments` keeps the args past its parameter
/// list -- those registers are the only place the extra arguments
/// live. A *missing* parameter is a different case: `f(t, e)` called
/// as `f('a')` must see `e === undefined`. Blanking from
/// `max(argc, nparams)` conflated the two, so naver's event dispatcher
/// (`fire(t, e)` -> `e = e || {}`) found a leftover string in `e`,
/// skipped its own initializer, and threw on the next `e.<x>.push()`.
fn frame_blank_from(
    uses_arguments: bool, argc: usize, nparams: usize,
) -> usize {
    if uses_arguments { argc } else { argc.min(nparams) }
}

fn err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    let msg = msg.into();
    if trace_enabled() {
        eprintln!("[gg-raise] Error: {msg}");
    }
    Err(VmError { msg, value: None, kind: "Error", trace: None })
}

/// An engine-raised error that real JS specifies as a TypeError
/// (member access on nullish, calling a non-function, ...).
fn type_err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    let msg = msg.into();
    if trace_enabled() {
        eprintln!("[gg-raise] TypeError: {msg}");
    }
    Err(VmError { msg, value: None, kind: "TypeError", trace: None })
}

/// As `type_err`, for ReferenceErrors (unresolved names, TDZ).
fn ref_err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError {
        msg: msg.into(), value: None, kind: "ReferenceError", trace: None,
    })
}

/// As `type_err`, for RangeErrors (invalid lengths and counts).
fn range_err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError {
        msg: msg.into(), value: None, kind: "RangeError", trace: None,
    })
}

/// `JSON.parse` is specified to throw a SyntaxError, not a plain
/// Error — and code that branches on the error type sees the
/// difference.
fn syntax_err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError {
        msg: msg.into(), value: None, kind: "SyntaxError", trace: None,
    })
}

/// Longest string the VM will build, in bytes. Rope concatenation is
/// O(1), so a hostile `while(1) s += s` mints multi-gigabyte strings
/// in ~30 statements (far under the execution fuel) and the process
/// aborts on the first materialization — Rust cannot recover a failed
/// allocation. Real engines throw "Invalid string length" instead
/// (V8 caps around 2^29 code units); namuwiki's Cloudflare challenge
/// script killed the whole renderer this way.
const MAX_STR_BYTES: usize = 64 * 1024 * 1024;
/// Most elements one string-derived materialization (split,
/// Array.from(string), a global regex match) may produce — each
/// element is its own heap string, so counts beyond this are memory
/// bombs, not web content.
const MAX_MATERIALIZE: usize = 1024 * 1024;
/// Cap on dense array storage. gg stores elements in a flat Vec, so
/// `arr.length = 1e8` or `arr[1e8] = x` (real engines keep these
/// sparse) would force a multi-hundred-MB allocation and abort the
/// process. Past this, a length set or sparse index write throws a
/// catchable RangeError — gov.kr's bundle set a giant array length.
const MAX_ARRAY_ELEMS: usize = 16 * 1024 * 1024;
/// Cumulative string-heap bytes allowed within one script run. gg has
/// no GC, so every concat/method result lives until the run ends; an
/// obfuscation loop minting large strings per iteration (namuwiki's
/// Cloudflare challenge) stays under the fuel budget but exhausts RAM
/// and aborts on a failed allocation. The dispatch loop trips a
/// catchable RangeError past this. Far above any real page (naver
/// settles well under 256MB of live strings).
/// String-heap backstop. gg has no GC, so `st.strs` is append-only and
/// this really is the retained size, not a cumulative-allocation
/// proxy. Tunable so the ceiling can be measured against real pages
/// (`GG_JS_HEAP_MB`) instead of guessed at.
fn max_heap_bytes() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("GG_JS_HEAP_MB")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|mb| *mb > 0)
            .map(|mb| mb * 1024 * 1024)
            .unwrap_or(512 * 1024 * 1024)
    })
}

const MAX_FRAMES: usize = 4096;
/// Cap on native re-entry (JS -> sort/callback -> JS ...). Each level
/// nests several real Rust frames (merge_sort + less + call_value +
/// exec), which MAX_FRAMES does not count. Kept low so it trips before
/// the OS stack overflows — Windows' main thread has only ~1 MB, and
/// debug frames are large. Legitimate callback nesting is never deep.
const MAX_NATIVE_DEPTH: usize = 24;

/// `document` is a DOM-node value with this sentinel index.
pub(super) const DOC_NODE: u32 = u32::MAX;
/// Listener key for `window.addEventListener` (load/resize/...).
pub(super) const WINDOW_NODE: u32 = u32::MAX - 1;

/// Browsing-context handles used by `postMessage`. A document only
/// ever names a context it can already reach: itself, its embedder, or
/// the tab's root. Child frames get host-minted handles (1, 2, 3, ...)
/// pushed in by `set_frame_graph` — the VM never learns a handle it was
/// not told about, which is what keeps the graph unguessable.
pub(super) const FRAME_SELF: u32 = u32::MAX;
pub(super) const FRAME_PARENT: u32 = u32::MAX - 1;
pub(super) const FRAME_TOP: u32 = u32::MAX - 2;

/// A compiled script plus its bindings into the shared VM namespace.
pub(super) struct LoadedModule {
    pub(super) module: Module,
    /// module atom -> VM-wide name id
    pub(super) global_map: Vec<u32>,
    /// this module's slice of the shared inline-cache table
    pub(super) ic_base: u32,
}

/// The VM's module table. Interior-mutable because lazy compilation
/// loads new modules mid-execution, while active frames hold Rc's to
/// the modules they're running (a Vec re-allocation can't move them).
pub(super) struct ModStore {
    mods: RefCell<Vec<Rc<LoadedModule>>>,
    /// lazy stub (module, proto) -> its compiled (module, main)
    redirects: RefCell<HashMap<(u32, u32), (u32, u32)>>,
}

impl ModStore {
    pub(super) fn new() -> ModStore {
        ModStore {
            mods: RefCell::new(Vec::new()),
            redirects: RefCell::new(HashMap::new()),
        }
    }

    pub(super) fn rc(&self, mi: u32) -> Rc<LoadedModule> {
        self.mods.borrow()[mi as usize].clone()
    }

    pub(super) fn len(&self) -> usize {
        self.mods.borrow().len()
    }

    pub(super) fn push(&self, m: LoadedModule) -> u32 {
        let mut v = self.mods.borrow_mut();
        v.push(Rc::new(m));
        (v.len() - 1) as u32
    }
}

/// Move a compiled module into the VM: string constants into the
/// VM-wide rope arena, atoms mapped to name ids, IC slots allocated.
pub(super) fn load_module(
    st: &mut St,
    mods: &ModStore,
    mut module: Module,
) -> u32 {
    let str_base = st.strs.len() as u32;
    for s in &module.strings {
        st.strs.push(Str::Flat(s.clone()));
    }
    for proto in &mut module.protos {
        for c in &mut proto.consts {
            if c.is_string() {
                *c = Value::string(str_base + c.index());
            }
        }
    }
    let global_map =
        module.atoms.iter().map(|n| st.intern_name(n)).collect();
    let ic_base = st.ics.len() as u32;
    st.ics
        .extend(std::iter::repeat(IC_EMPTY).take(module.n_ics as usize));
    mods.push(LoadedModule { module, global_map, ic_base })
}

/// First call of a lazy stub: compile the deferred body, load it as a
/// fresh module, and redirect (cm, cp) to it. The fast path — proto is
/// not lazy — is one Rc clone and a field check. Compile errors
/// surface as a catchable SyntaxError at the call, not at page load
/// (so one broken cold function no longer kills the whole script).
fn ensure_compiled(
    st: &mut St,
    mods: &ModStore,
    cm: u32,
    cp: u32,
) -> Result<(u32, u32), VmError> {
    {
        let m = mods.rc(cm);
        if m.module.protos[cp as usize].lazy.is_none() {
            return Ok((cm, cp));
        }
    }
    if let Some(&t) = mods.redirects.borrow().get(&(cm, cp)) {
        return Ok(t);
    }
    let m = mods.rc(cm);
    let lazy = m.module.protos[cp as usize].lazy.as_deref().unwrap();
    let module = super::compiler::compile_lazy(lazy).map_err(|e| {
        VmError {
            msg: e.msg,
            value: None,
            kind: "SyntaxError",
            trace: None,
        }
    })?;
    let nmi = load_module(st, mods, module);
    let main = mods.rc(nmi).module.main;
    mods.redirects.borrow_mut().insert((cm, cp), (nmi, main));
    Ok((nmi, main))
}

struct Frame {
    module: u32,
    proto: u32,
    ip: usize,
    base: usize,
    closure: u32,
    this_val: Value,
    argc: u8,
    /// caller's with_stack base, so a callee never sees the caller's
    /// `with (obj)` scopes (with is lexical, not dynamic).
    with_base: usize,
}

/// An armed `try` handler: everything needed to resume the frame that
/// armed it at its catch label. `depth` is st.frames.len() at arm time
/// (the frame's own state lives in exec's locals, not in `frames`),
/// so unwinding truncates `frames` to `depth` and restarts from here.
struct Handler {
    depth: usize,
    module: u32,
    proto: u32,
    base: usize,
    closure: u32,
    this_val: Value,
    catch_ip: u32,
    exc_reg: u8,
    argc: u8,
    /// with_stack state to restore when this handler catches (drops any
    /// `with` scope entered inside the try but not yet exited).
    with_base: usize,
    with_len: usize,
}

pub(super) enum ClosureRec {
    User {
        module: u32,
        proto: u32,
        upvals: Vec<u32>,
        /// Some(this) for arrow functions (lexical `this` captured at
        /// creation); None for regular functions (this from call site).
        this_capture: Option<Value>,
    },
    Native(Native),
    /// Function.prototype.bind: fixed this + partially applied args.
    Bound {
        target: Value,
        this_val: Value,
        bound: Vec<Value>,
    },
    /// Callable Proxy wrapper.  Object Proxies use `St::object_proxies`;
    /// both point at the same ProxyRec arena so revocation is shared.
    Proxy(u32),
}

#[derive(Clone, Copy)]
pub(super) enum Native {
    ConsoleLog,
    DateNow,
    Alert,
    /// accepts anything, returns undefined (window.addEventListener
    /// and friends — enough for feature-detecting bundles to proceed)
    Noop,
    /// window.scrollTo / scrollBy / scroll: routed through the same
    /// host queue as element scrolling, with the DOC_NODE sentinel
    /// standing for the page's own scroller.
    WinScroll { relative: bool },
    /// Web Storage op on localStorage (session=false) or sessionStorage
    /// (session=true). op: 0=getItem 1=setItem 2=removeItem 3=clear
    /// 4=key
    Storage { session: bool, op: u8 },
    /// RegExp(pattern, flags) — builds the same object as a literal
    RegExpCtor,
    /// requestAnimationFrame: a ~16ms one-shot timer whose callback
    /// receives the (virtual) timestamp
    Raf,
    /// performance.now(): the virtual clock in ms
    PerfNow,
    /// element.classList.<op>; op: 0=add 1=remove 2=contains 3=toggle
    ClassList { node: u32, op: u8 },
    /// window.addEventListener / removeEventListener (listeners keyed
    /// on WINDOW_NODE so lifecycle events can find them)
    WinEvent { add: bool },
    /// window.dispatchEvent(ev): fire the WINDOW_NODE listeners for
    /// ev.type (React reports errors via window.dispatchEvent)
    WinDispatch,
    /// `otherWindow.postMessage(data, targetOrigin)`. The payload is
    /// serialized to JSON here and queued for the host, which decides
    /// whether the target may receive it. `ctx` is the browsing
    /// context this function was minted for.
    PostMessage { ctx: u32 },
    /// Internal: the macrotask that delivers one queued message. It
    /// fires the window's "message" listeners *and* `window.onmessage`,
    /// which plain `dispatchEvent` does not do.
    WinDeliver,
    /// One operation on a same-origin child document's mirror.
    /// `frame` is the context handle, `node` the child's arena index
    /// (0 for document-level ops), `op` a `framedom::*` constant.
    FrameDom { frame: u32, node: u32, op: u8 },
    /// encodeURI(Component)/decodeURI(Component)
    UriCoder { encode: bool, component: bool },
    /// Map()/WeakMap() constructor (weak = no difference: no GC)
    MapCtor,
    /// Set()/WeakSet() constructor
    SetCtor,
    /// Map method bound to its instance. op: 0=get 1=set 2=has
    /// 3=delete 4=clear 5=forEach 6=keys 7=values 8=entries
    MapOp { obj: u32, op: u8 },
    /// Set method bound to its instance. op: 0=add 1=has 2=delete
    /// 3=clear 4=forEach 5=values
    SetOp { obj: u32, op: u8 },
    /// the `Function` constructor stub: returns a function that
    /// returns the global (bundles call `Function("return this")()`
    /// to find the global object; real eval is out of scope)
    FunctionCtor,
    /// an extracted builtin method (`''.slice` — the core-js
    /// uncurryThis pattern): remembers the method name id and
    /// dispatches when invoked with an explicit this via call/apply
    MethodRef(u32),
    /// the genuine Object.prototype.toString: returns the receiver's
    /// brand ("[object Array]" etc. — core-js classof / jQuery type
    /// checks do `{}.toString.call(x)`)
    BrandToString,
    /// callable `Object(x)`: coercion-ish (nullish -> {}, object -> x)
    ObjectCtor,
    /// callable `Array(n)` / `Array(a, b, ...)`
    ArrayCtor,
    /// what FunctionCtor's product does when called: yield `window`
    ReturnGlobal,
    /// document.implementation.createHTMLDocument: yields the document
    /// (single-document model — created nodes stay detached anyway)
    ReturnDoc,
    /// an extracted DOM method (`document.addEventListener` read as a
    /// property — jQuery 1.x feature-detects this way): calling it
    /// routes back into dom_method with the captured node
    DomMethod { node: u32, key: u32 },
    String,
    Number,
    Boolean,
    ParseInt,
    ParseFloat,
    /// document.open/write/writeln/close on a script-created iframe's
    /// document (0 = open, 1 = write, 2 = writeln, 3 = close).
    DocWrite { node: u32, op: u8 },
    /// `__ggStack()`: the live call stack as V8-ish "    at name" lines.
    /// The prelude's Error constructor hangs it off `.stack`, which is
    /// what every minified bundle reports when something goes wrong.
    StackTrace,
    JsonStringify,
    JsonParse,
    /// event.preventDefault(): flags the current dispatch (page.rs).
    PreventDefault,
    // --- async runtime (P3) ---
    SetTimeout,
    SetInterval,
    ClearTimeout,
    QueueMicrotask,
    Fetch,
    PromiseResolve,
    /// The global `Promise` itself. A real constructor function --
    /// callable through `new t(executor)` where t arrived in a
    /// variable -- because core-js's PromiseCapability does exactly
    /// that, and `typeof Promise` must answer "function" or every
    /// library's feature detection installs its polyfill over us.
    PromiseCtor,
    PromiseReject,
    // --- host objects (P3b): Math/Object/Array/Number/String statics,
    // dispatched by id so one variant covers them all ---
    HostFn(u16),
    /// resolve/reject bound to a `new Promise(executor)` — settles the
    /// promise when called. Rides the ordinary Native call path (P3b).
    Resolve { pid: u32, reject: bool },
    /// The real Proxy constructor and Proxy.revocable helper.
    ProxyCtor,
    ProxyRevocable,
    ProxyRevoke { proxy: u32 },
    /// One of the Reflect.* internal-operation entry points.
    Reflect(u8),
}

pub(super) mod reflect {
    pub const APPLY: u8 = 0;
    pub const CONSTRUCT: u8 = 1;
    pub const DEFINE_PROPERTY: u8 = 2;
    pub const DELETE_PROPERTY: u8 = 3;
    pub const GET: u8 = 4;
    pub const GET_OWN_PROPERTY_DESCRIPTOR: u8 = 5;
    pub const GET_PROTOTYPE_OF: u8 = 6;
    pub const HAS: u8 = 7;
    pub const IS_EXTENSIBLE: u8 = 8;
    pub const OWN_KEYS: u8 = 9;
    pub const PREVENT_EXTENSIONS: u8 = 10;
    pub const SET: u8 = 11;
    pub const SET_PROTOTYPE_OF: u8 = 12;
}

/// host function ids (Native::HostFn payload)
pub(super) mod host {
    pub const M_ABS: u16 = 0;
    pub const M_FLOOR: u16 = 1;
    pub const M_CEIL: u16 = 2;
    pub const M_ROUND: u16 = 3;
    pub const M_TRUNC: u16 = 4;
    pub const M_SIGN: u16 = 5;
    pub const M_SQRT: u16 = 6;
    pub const M_CBRT: u16 = 7;
    pub const M_POW: u16 = 8;
    pub const M_EXP: u16 = 9;
    pub const M_LOG: u16 = 10;
    pub const M_LOG2: u16 = 11;
    pub const M_LOG10: u16 = 12;
    pub const M_SIN: u16 = 13;
    pub const M_COS: u16 = 14;
    pub const M_TAN: u16 = 15;
    pub const M_ATAN: u16 = 16;
    pub const M_ATAN2: u16 = 17;
    pub const M_MIN: u16 = 18;
    pub const M_MAX: u16 = 19;
    pub const M_RANDOM: u16 = 20;
    pub const M_HYPOT: u16 = 21;
    pub const M_CLZ32: u16 = 22;
    pub const O_KEYS: u16 = 40;
    pub const O_VALUES: u16 = 41;
    pub const O_ENTRIES: u16 = 42;
    pub const O_ASSIGN: u16 = 43;
    pub const O_FREEZE: u16 = 44;
    pub const O_DEFINE_PROP: u16 = 45;
    pub const O_GET_OWN_PD: u16 = 46;
    pub const O_CREATE: u16 = 47;
    pub const O_GET_PROTO: u16 = 48;
    pub const O_SET_PROTO: u16 = 49;
    pub const O_DEFINE_PROPS: u16 = 50;
    pub const O_GET_OWN_NAMES: u16 = 51;
    pub const O_IS: u16 = 52;
    pub const O_FROM_ENTRIES: u16 = 53;
    pub const O_GET_OWN_PDS: u16 = 54;
    pub const O_SEAL: u16 = 55;
    pub const O_IS_FROZEN: u16 = 56;
    pub const O_IS_SEALED: u16 = 57;
    pub const O_PREVENT_EXT: u16 = 58;
    pub const O_IS_EXTENSIBLE: u16 = 63;
    pub const A_ISARRAY: u16 = 60;
    pub const A_FROM: u16 = 61;
    pub const A_OF: u16 = 62;
    pub const N_ISNAN: u16 = 80;
    pub const N_ISFINITE: u16 = 81;
    pub const N_ISINTEGER: u16 = 82;
    pub const N_ISSAFEINT: u16 = 83;
    pub const O_HAS_OWN: u16 = 59;
    pub const S_FROMCHARCODE: u16 = 100;
    // canvas-2d stub context (crash prevention; no real rasterizing)
    pub const CV_MEASURE_TEXT: u16 = 120;
    pub const CV_IMAGE_DATA: u16 = 121;
    pub const CV_GRADIENT: u16 = 122;
}

/// Hidden class: property layout shared by every object that acquired
/// its properties in the same order. Keys are VM-wide name ids.
pub(super) struct Shape {
    props: HashMap<u32, u16>,
    transitions: HashMap<u32, u32>,
}

pub(super) struct Obj {
    shape: u32,
    slots: Vec<Value>,
    /// dense element storage; only arrays use it
    elems: Vec<Value>,
    is_array: bool,
    /// index into St.promises when this object is a Promise, else
    /// PROMISE_NONE. Keeps promise internal state out of shapes/for-in.
    promise: u32,
    /// index into St.regexes when this object is a RegExp, else REGEX_NONE.
    regex: u32,
    /// [[Prototype]]: an object value, or UNDEFINED for none. Property
    /// reads that miss the own shape walk this chain.
    proto: Value,
    /// this object has entries in St.accessors (checked before the
    /// side-table lookup so plain objects pay nothing)
    has_accessors: bool,
}

#[derive(Clone, Copy)]
struct ProxyRec {
    target: Value,
    handler: Value,
    revoked: bool,
}

/// Sentinel: an Obj that is not a Promise.
pub(super) const PROMISE_NONE: u32 = u32::MAX;
/// Sentinel: an Obj that is not a RegExp.
pub(super) const REGEX_NONE: u32 = u32::MAX;

/// A compiled regex: the fast `regex` crate when it can handle the
/// pattern, else `fancy-regex` for JS-only features (backreferences,
/// lookaround) that `regex` rejects — date-fns's tokenizer uses
/// `(\w)\1*`, and without backref support `.match` returns null and
/// the app's `for...of` over it throws. `Never` is the last resort
/// (truly uncompilable, e.g. lone surrogates that can't match UTF-8).
pub(super) enum CompiledRe {
    Std(regex::Regex),
    Fancy(Box<fancy_regex::Regex>),
    Never,
}

impl CompiledRe {
    fn is_match(&self, s: &str) -> bool {
        match self {
            CompiledRe::Std(r) => r.is_match(s),
            CompiledRe::Fancy(r) => r.is_match(s).unwrap_or(false),
            CompiledRe::Never => false,
        }
    }
    /// group strings (index 0 = whole match), None entry = unmatched
    fn captures_owned(&self, s: &str) -> Option<Vec<Option<String>>> {
        let grab = |caps: fancy_regex::Captures| -> Vec<Option<String>> {
            (0..caps.len())
                .map(|i| caps.get(i).map(|m| m.as_str().to_string()))
                .collect()
        };
        match self {
            CompiledRe::Std(r) => r.captures(s).map(|c| {
                (0..c.len())
                    .map(|i| c.get(i).map(|m| m.as_str().to_string()))
                    .collect()
            }),
            CompiledRe::Fancy(r) => {
                r.captures(s).ok().flatten().map(grab)
            }
            CompiledRe::Never => None,
        }
    }
    /// capture-group names by index (0 = whole match, always None);
    /// an unnamed group yields None. Empty when the regex never compiled.
    fn capture_names(&self) -> Vec<Option<String>> {
        match self {
            CompiledRe::Std(r) => r
                .capture_names()
                .map(|n| n.map(String::from))
                .collect(),
            CompiledRe::Fancy(r) => r
                .capture_names()
                .map(|n| n.map(String::from))
                .collect(),
            CompiledRe::Never => Vec::new(),
        }
    }
    /// byte offset of the first match, for String.search
    fn find_start(&self, s: &str) -> Option<usize> {
        match self {
            CompiledRe::Std(r) => r.find(s).map(|m| m.start()),
            CompiledRe::Fancy(r) => {
                r.find(s).ok().flatten().map(|m| m.start())
            }
            CompiledRe::Never => None,
        }
    }
    /// all whole-match substrings (global match / find_iter)
    fn find_all(&self, s: &str) -> Vec<String> {
        match self {
            CompiledRe::Std(r) => {
                r.find_iter(s).map(|m| m.as_str().to_string()).collect()
            }
            CompiledRe::Fancy(r) => r
                .find_iter(s)
                .filter_map(|m| m.ok())
                .map(|m| m.as_str().to_string())
                .collect(),
            CompiledRe::Never => Vec::new(),
        }
    }
    fn replace_all_str(&self, s: &str, rep: &str) -> String {
        match self {
            CompiledRe::Std(r) => r.replace_all(s, rep).into_owned(),
            CompiledRe::Fancy(r) => r.replace_all(s, rep).into_owned(),
            CompiledRe::Never => s.to_string(),
        }
    }
    fn replace_first_str(&self, s: &str, rep: &str) -> String {
        match self {
            CompiledRe::Std(r) => r.replace(s, rep).into_owned(),
            CompiledRe::Fancy(r) => r.replace(s, rep).into_owned(),
            CompiledRe::Never => s.to_string(),
        }
    }
    /// Matches with capture groups and byte start offset, for
    /// `String.replace(re, fn)`. Each entry: (whole match, [group or None
    /// per capture], byte start). `global=false` stops after the first.
    fn captures_all(
        &self,
        s: &str,
        global: bool,
    ) -> Vec<(String, Vec<Option<String>>, usize)> {
        let mut out = Vec::new();
        match self {
            CompiledRe::Std(r) => {
                for caps in r.captures_iter(s) {
                    let m0 = match caps.get(0) {
                        Some(m) => m,
                        None => continue,
                    };
                    let groups = (1..caps.len())
                        .map(|i| caps.get(i).map(|g| g.as_str().to_string()))
                        .collect();
                    out.push((m0.as_str().to_string(), groups, m0.start()));
                    if !global {
                        break;
                    }
                }
            }
            CompiledRe::Fancy(r) => {
                for caps in r.captures_iter(s).filter_map(|c| c.ok()) {
                    let m0 = match caps.get(0) {
                        Some(m) => m,
                        None => continue,
                    };
                    let groups = (1..caps.len())
                        .map(|i| caps.get(i).map(|g| g.as_str().to_string()))
                        .collect();
                    out.push((m0.as_str().to_string(), groups, m0.start()));
                    if !global {
                        break;
                    }
                }
            }
            CompiledRe::Never => {}
        }
        out
    }
    fn split_vec(&self, s: &str) -> Vec<String> {
        match self {
            CompiledRe::Std(r) => {
                r.split(s).map(|p| p.to_string()).collect()
            }
            CompiledRe::Fancy(r) => r
                .split(s)
                .filter_map(|p| p.ok())
                .map(|p| p.to_string())
                .collect(),
            CompiledRe::Never => vec![s.to_string()],
        }
    }
}

/// A compiled regular expression + JS flags.
pub(super) struct RegexRec {
    re: CompiledRe,
    global: bool,
    source: String,
    flags: String,
}

/// One per GetProp/SetProp site; a hit skips the hash lookup entirely.
#[derive(Clone, Copy)]
pub(super) struct IcEntry {
    shape: u32,
    slot: u16,
}

pub(super) const IC_EMPTY: IcEntry = IcEntry { shape: u32::MAX, slot: 0 };

/// Rope strings: concatenation is an O(1) Cat node; the byte content
/// materializes lazily (and memoizes) on first read.
pub(super) enum Str {
    Flat(String),
    Cat { a: u32, b: u32, len: u32 },
}

/// Well-known constructor values, recorded when PageVm installs the
/// globals. `instanceof` matches built-ins by identity — the engine
/// has no prototype chains to walk.
#[derive(Clone, Copy)]
pub(super) struct KnownCtors {
    pub(super) window: Value,
    pub(super) array: Value,
    pub(super) object: Value,
    pub(super) promise: Value,
    pub(super) string: Value,
    pub(super) number: Value,
    pub(super) boolean: Value,
    pub(super) function: Value,
}

impl Default for KnownCtors {
    fn default() -> KnownCtors {
        KnownCtors {
            window: Value::UNDEFINED,
            array: Value::UNDEFINED,
            object: Value::UNDEFINED,
            promise: Value::UNDEFINED,
            string: Value::UNDEFINED,
            number: Value::UNDEFINED,
            boolean: Value::UNDEFINED,
            function: Value::UNDEFINED,
        }
    }
}

/// Pre-resolved name ids for builtin dispatch (filled by PageVm).
#[derive(Clone, Copy)]
pub(super) struct Ids {
    pub(super) push: u32,
    pub(super) sort: u32,
    pub(super) to_fixed: u32,
    pub(super) length: u32,
    pub(super) get_element_by_id: u32,
    pub(super) create_element: u32,
    pub(super) query_selector: u32,
    pub(super) query_selector_all: u32,
    pub(super) get_elements_by_tag_name: u32,
    pub(super) append_child: u32,
    pub(super) remove: u32,
    pub(super) set_attribute: u32,
    pub(super) get_attribute: u32,
    pub(super) remove_attribute: u32,
    pub(super) add_event_listener: u32,
    pub(super) text_content: u32,
    pub(super) inner_html: u32,
    pub(super) id: u32,
    pub(super) class_name: u32,
    pub(super) body: u32,
    pub(super) title: u32,
    pub(super) prototype: u32,
}

impl Default for Ids {
    fn default() -> Ids {
        // u32::MAX never matches a real name id
        Ids {
            push: u32::MAX, sort: u32::MAX, to_fixed: u32::MAX,
            length: u32::MAX, get_element_by_id: u32::MAX,
            create_element: u32::MAX, query_selector: u32::MAX,
            query_selector_all: u32::MAX,
            get_elements_by_tag_name: u32::MAX, append_child: u32::MAX,
            remove: u32::MAX, set_attribute: u32::MAX,
            get_attribute: u32::MAX, remove_attribute: u32::MAX,
            add_event_listener: u32::MAX,
            text_content: u32::MAX, inner_html: u32::MAX, id: u32::MAX,
            class_name: u32::MAX, body: u32::MAX, title: u32::MAX,
            prototype: u32::MAX,
        }
    }
}

pub(super) struct St {
    pub(super) globals: Vec<Value>,
    pub(super) gdef: Vec<bool>,
    /// name id -> text (error messages, event keys)
    pub(super) names: Vec<String>,
    /// text -> name id (reverse of `names`; keeps compile-time and
    /// runtime property-name interning on one shared numbering)
    pub(super) name_ids: HashMap<String, u32>,
    frames: Vec<Frame>,
    /// Armed exception handlers, innermost last (see Handler).
    handlers: Vec<Handler>,
    /// Active `with (obj)` scopes, innermost last. A bare global read
    /// consults these (innermost first) before the real global.
    with_stack: Vec<Value>,
    /// Depth of nested native->JS re-entries (see MAX_NATIVE_DEPTH).
    native_depth: usize,
    /// Set by Native::PreventDefault during an event dispatch.
    pub(super) default_prevented: bool,
    pub(super) closures: Vec<ClosureRec>,
    cells: Vec<Value>,
    shapes: Vec<Shape>,
    pub(super) objects: Vec<Obj>,
    /// Proxy records are separate from object storage because callable
    /// targets must remain function-tagged. Object wrappers map their
    /// object index to the same arena used by ClosureRec::Proxy.
    proxies: Vec<ProxyRec>,
    object_proxies: HashMap<u32, u32>,
    non_extensible_objects: HashSet<u32>,
    non_extensible_functions: HashSet<u32>,
    pub(super) strs: Vec<Str>,
    /// Flat-string content -> its index in `strs`, so `intern` is O(1)
    /// instead of a linear scan of the whole arena. Only interned Flats
    /// are recorded (they are never mutated in place — rope flattening
    /// only ever rewrites Cat entries), so an entry here always points
    /// at a Flat whose text equals the key.
    pub(super) flat_index: HashMap<String, u32>,
    pub(super) ics: Vec<IcEntry>,
    pub(super) regs: Vec<Value>,
    pub(super) logs: Vec<String>,
    /// (node index, event type) -> JS handler values
    pub(super) listeners: HashMap<(u32, String), Vec<Value>>,
    pub(super) doc: Option<Rc<RefCell<dom::Document>>>,
    pub(super) ids: Ids,
    pub(super) known: KnownCtors,
    /// closure index -> its .prototype object (created lazily)
    pub(super) fn_protos: HashMap<u32, Value>,
    /// (closure index, name id) -> static properties on functions
    /// (Object.keys, Array.isArray, F.displayName = ...)
    pub(super) fn_props: HashMap<(u32, u32), Value>,
    /// (dom node, name id) -> expando properties scripts hang on
    /// nodes (jQuery's `elem[expando] = id` data-cache key)
    pub(super) dom_expando: HashMap<(u32, u32), Value>,
    /// (object index, name id) -> (getter, setter) accessor pair
    /// (Object.defineProperty with get/set; UNDEFINED = absent side)
    pub(super) accessors: HashMap<(u32, u32), (Value, Value)>,
    /// Property-attribute side-tables, populated only when scripts opt out
    /// of the defaults (defineProperty / freeze / seal). Hot paths gate on
    /// `is_empty()` so ordinary objects pay nothing.
    /// object indices frozen/sealed/preventExtensions'd (no new own props)
    /// (object index, name id) whose data slot is read-only (writable:false)
    pub(super) non_writable: std::collections::HashSet<(u32, u32)>,
    /// (object index, name id) hidden from keys/for-in/JSON (enumerable:false)
    pub(super) non_enum: std::collections::HashSet<(u32, u32)>,
    /// Web Storage backing maps (in-memory; not persisted to disk)
    pub(super) local_storage: HashMap<String, String>,
    pub(super) session_storage: HashMap<String, String>,
    /// document.cookie pairs in insertion order (the Python network jar
    /// seeds the visible values before scripts run).
    pub(super) cookies: Vec<(String, String)>,
    /// Original document.cookie setter strings awaiting host validation.
    /// Keeping the attributes is essential: Path/Domain/Secure/expiry
    /// cannot be reconstructed from the visible `k=v` string.
    pub(super) cookie_writes: Vec<String>,
    /// node index -> (x, y, w, h) in document coordinates, pushed by
    /// the shell after each layout so getBoundingClientRect answers
    /// real geometry (document-origin approximation: scroll offset is
    /// not subtracted)
    pub(super) layout_rects: HashMap<u32, (f64, f64, f64, f64)>,
    /// Per-node scroll state fed back by the host after layout:
    /// (scrollTop, scrollLeft, scrollHeight, scrollWidth). Mirrors
    /// `layout_rects` — the engine owns scrolling, JS only observes it.
    pub(super) scroll_state: HashMap<u32, (f64, f64, f64, f64)>,
    /// Scroll requests the host has not applied yet, as
    /// (node, top, left, seq, relative). The shell drains this each
    /// turn and moves the real scroller (page scripts scroll chat
    /// logs and carousels this way). A `relative` entry carries a
    /// delta, not a position — see `queue_scroll`.
    pub(super) scroll_writes: Vec<(u32, f64, f64, u64, bool)>,
    /// `el.scrollIntoView()` requests as (node, seq): resolving them
    /// needs layout, so the host drains this and scrolls the
    /// element's ancestors.
    pub(super) scroll_into_view: Vec<(u32, u64)>,
    /// Shared counter stamped onto both queues above. The host replays
    /// them in one merged order, so `el.scrollIntoView()` followed by
    /// `window.scrollBy(0, 10)` lands where the page asked instead of
    /// whichever queue the host happened to drain last.
    pub(super) scroll_seq: u64,
    /// The `<script>` element the host is executing right now, as an
    /// arena index. Loaders find their own tag with
    /// `document.currentScript.getAttribute('src')` / `.dataset`, so
    /// leaving this undefined makes them throw on their first line.
    pub(super) current_script: Option<u32>,
    /// This document's origin ("https://a.test", or "null" for an
    /// opaque one). Stamped onto every message this document sends so
    /// the receiver's `e.origin` is the sender's, not its own.
    pub(super) page_origin: String,
    /// `postMessage` calls the host has not routed yet, as
    /// (target context, JSON payload, targetOrigin, seq). Documents
    /// never touch each other's arenas — a message is serialized here
    /// and re-parsed in the receiver, which is the whole isolation
    /// story: there is no code path from one document's Value to
    /// another's.
    pub(super) message_writes: Vec<(u32, String, String, u64)>,
    /// Ordering counter for `message_writes` (see `scroll_seq`).
    pub(super) frame_seq: u64,
    /// context handle -> the one WindowProxy object handed to JS for
    /// it. Cached so `e.source === iframe.contentWindow` holds.
    pub(super) ctx_proxies: HashMap<u32, Value>,
    /// iframe element node index -> (context handle, same-origin).
    /// Pushed by the host after it decides what this document may
    /// reach; an entry missing here makes `contentWindow` read null.
    pub(super) frame_ctx: HashMap<u32, (u32, bool)>,
    /// context handle -> the same-origin child DOM this document may
    /// read. Only frames the host judged same-origin ever get one.
    pub(super) frame_mirrors: HashMap<u32, FrameMirror>,
    /// iframe element node index -> its `contentDocument` object
    pub(super) frame_docs: HashMap<u32, Value>,
    /// A script-created iframe has no document yet, but the canonical
    /// way to fill one is `f.contentDocument.write(html)`. Buffer the
    /// markup per iframe node; `close()` hands it to the host, which
    /// loads it into the real child document.
    pub(super) doc_write_buf: HashMap<u32, String>,
    /// While the HTML parser is running, document.write belongs to the
    /// input stream at the insertion point, not to the tree: what it
    /// writes has to be tokenized, or a written <script> never runs.
    /// Set by the parser; None restores the after-load behaviour of
    /// grafting parsed markup in beside the writing script.
    pub(super) parser_writes: Option<String>,
    /// (iframe node, markup) for documents whose `close()` has run.
    pub(super) doc_writes: Vec<(u32, String)>,
    /// Parser insertion point for `document.write` into the *running*
    /// document: (owning script node, parent node, next child index).
    /// A script that writes twice must see its second chunk land after
    /// the first, not between the script tag and the first -- so the
    /// point advances past everything already inserted, and resets
    /// when a different script takes over.
    pub(super) doc_write_at: Option<(u32, usize, usize)>,
    /// iframe nodes the host has spoken about (pushed *or* dropped a
    /// document for). Those are the host's to answer for -- a dropped
    /// mirror means "no document", not "write your own".
    pub(super) frame_host_owned: std::collections::HashSet<u32>,
    /// One function object per (native kind, a, b) identity triple.
    /// A builtin read must answer the same object every time --
    /// `a.push === a.push`, `el.focus === el.focus` -- or feature
    /// detection sees phantom "overrides" and removeEventListener
    /// can never match what addEventListener stored.
    pub(super) native_memo: HashMap<(u8, u32, u32), Value>,
    /// `el.attributes` object per node. A NamedNodeMap is *live* and
    /// identity-stable, and React 19's unmount loop banks on both:
    /// `for (e = n.attributes; e.length;) n.removeAttributeNode(e[0])`
    /// only terminates if removals shrink the map it captured.
    pub(super) attr_maps: HashMap<u32, Value>,
    /// mutations page JS made through a mirror, as
    /// (handle, child node index, op, arg a, arg b, seq). Applied to
    /// the mirror immediately and to the real child by the host.
    pub(super) frame_dom_writes: Vec<(u32, u32, u8, String, String, u64)>,
    /// Map/Set backing stores, keyed by the owning object's index
    /// (entries in insertion order; lookups are linear strict-eq)
    pub(super) map_data: HashMap<u32, Vec<(Value, Value)>>,
    pub(super) set_data: HashMap<u32, Vec<Value>>,
    /// el.style / el.dataset proxy objects -> their DOM node. Property
    /// reads/writes on these route to the style / data-* attributes.
    pub(super) style_nodes: HashMap<u32, u32>,
    pub(super) dataset_nodes: HashMap<u32, u32>,
    /// [[Prototype]] of function values (static inheritance:
    /// `Object.setPrototypeOf(Sub, Sup)`); static reads walk this.
    pub(super) fn_proto_chain: HashMap<u32, Value>,
    /// document.readyState ("loading" until the loader fires the
    /// lifecycle events, then "interactive"/"complete")
    pub(super) ready_state: &'static str,
    ty_names: [Value; 6],
    /// Compiled RegExp records; an Obj.regex indexes here.
    pub(super) regexes: Vec<RegexRec>,
    // --- async runtime (P3) ---
    /// Promise records; an Obj.promise indexes here.
    pub(super) promises: Vec<PromiseRec>,
    /// promises that rejected since the last drain, checked for a
    /// handler once the microtask queue empties
    pub(super) rejected: Vec<u32>,
    /// FIFO microtask queue (promise reactions + queueMicrotask jobs).
    pub(super) microtasks: std::collections::VecDeque<Job>,
    /// pending timers, fired in (due_ms, seq) order by the pump.
    pub(super) timers: Vec<Timer>,
    next_timer_id: u32,
    timer_seq: u64,
    /// virtual clock (ms). Timers advance it; Date.now reads it — so a
    /// headless run is deterministic and never sleeps.
    pub(super) now_ms: f64,
    /// The browser keeps two clocks and so do we. `Date.now` is the
    /// wall clock — virtual here, which is what makes a headless run
    /// reproducible. `performance.now` is the monotonic one a page
    /// measures itself with, and a frozen monotonic clock is a trap: a
    /// script that spins until it advances never finishes, and every
    /// benchmark reports zero. This one is real, from VM start.
    /// `GG_JS_VIRTUAL_TIME=1` pins it for determinism runs.
    pub(super) perf_origin: std::time::Instant,
    pub(super) perf_virtual: bool,
    /// fetches issued but not yet handed to the host (Python) driver.
    pub(super) pending_fetches: Vec<PendingFetch>,
    /// fetch_id -> promise id, handed to the driver, awaiting resolve.
    awaiting: HashMap<u32, u32>,
    next_fetch_id: u32,
    /// interned atoms for a Response's hidden body + the text/json method
    /// names (u32::MAX until St::new sets them).
    pub(super) fetch_body_atom: u32,
    text_atom: u32,
    json_atom: u32,
    /// deterministic PRNG state for Math.random (seedable, reproducible).
    rng_state: u64,
    /// Execution fuel: instructions the current turn may still run. The
    /// dispatch loop decrements it and aborts at 0, so a hostile
    /// `while(true){}` can never wedge the worker (fleet-safety, P4).
    pub(super) fuel: u64,
    /// Opt-in low-overhead instruction sampler.  Naver's initial React
    /// commit is a single long callback, so timer/microtask timings alone
    /// cannot identify the JavaScript function responsible.  Sampling is
    /// completely disabled unless GG_JS_PROFILE is set.
    profile_enabled: bool,
    /// (module, proto) of the function whose instruction is executing.
    /// The frame itself lives in `exec`'s locals rather than in
    /// `frames`, so without this the innermost -- and most useful --
    /// name is missing from every stack trace.
    cur_site: (u32, u32),
    profile_samples: HashMap<(u32, u32), u64>,
    /// Single-entry memo of a string's UTF-16 units, keyed by its `strs`
    /// index. Indexed char reads (charAt/charCodeAt) otherwise rebuild the
    /// whole unit vector per call — O(n) — so a char-by-char scanner over
    /// a big string is O(n^2). Strings are immutable, so the memo never
    /// goes stale; it holds only the most-recently-scanned string.
    pub(super) units_cache: Option<(u32, Vec<u16>)>,
    /// Memoized UTF-16 code-unit length per `strs` index. `s.length` is
    /// otherwise recounted O(n) per read, so a scanner's
    /// `while (i < s.length)` is O(n^2). Strings are immutable, so an
    /// entry never goes stale.
    pub(super) ulen_cache: HashMap<u32, u32>,
    /// Approx. live bytes in the string heap. gg has no GC and `strs`
    /// never shrinks, so this monotonically tracks the document's real
    /// string-memory footprint. A loop that mints large strings per
    /// iteration (namuwiki's Cloudflare challenge) stays under the fuel
    /// budget but exhausts RAM; the dispatch loop trips a catchable
    /// RangeError when this crosses `max_heap_bytes()`.
    pub(super) heap_bytes: usize,
}

/// Default per-turn instruction budget (~a few hundred ms of hot loop).
/// Generous enough for any real page script, small enough that a
/// runaway loop dies fast. The host can lower it per session.
// Per-script instruction budget. 80M stopped Naver's app mid-boot once
// the bundles started doing real work (07-20) — 400M keeps the runaway
// backstop while letting a genuine app initialize; the wall-clock
// js_budget in native.load_document still bounds the whole load.
pub(super) const DEFAULT_FUEL: u64 = 400_000_000;

// Layout viewport reported by documentElement.clientWidth/clientHeight and
// window.innerWidth/innerHeight. The height is deliberately tall so a
// headless full-page settle lets IntersectionObserver-gated ("lazy")
// content — naver's feed/news blocks — see itself as on-screen and render,
// instead of waiting for a scroll that never comes.
pub(super) const VIEWPORT_W: i32 = 1280;
pub(super) const VIEWPORT_H: i32 = 5000;

const TY_UNDEFINED: usize = 0;
const TY_BOOLEAN: usize = 1;
const TY_NUMBER: usize = 2;
const TY_STRING: usize = 3;
const TY_OBJECT: usize = 4;
const TY_FUNCTION: usize = 5;

impl St {
    pub(super) fn new(doc: Option<Rc<RefCell<dom::Document>>>) -> St {
        let mut st = St {
            globals: Vec::new(),
            gdef: Vec::new(),
            names: Vec::new(),
            name_ids: HashMap::new(),
            frames: Vec::new(),
            handlers: Vec::new(),
            with_stack: Vec::new(),
            native_depth: 0,
            default_prevented: false,
            closures: Vec::new(),
            cells: Vec::new(),
            shapes: vec![Shape {
                props: HashMap::new(),
                transitions: HashMap::new(),
            }],
            objects: Vec::new(),
            proxies: Vec::new(),
            object_proxies: HashMap::new(),
            non_extensible_objects: HashSet::new(),
            non_extensible_functions: HashSet::new(),
            strs: Vec::new(),
            flat_index: HashMap::new(),
            ics: Vec::new(),
            regs: Vec::new(),
            logs: Vec::new(),
            listeners: HashMap::new(),
            doc,
            ids: Ids::default(),
            known: KnownCtors::default(),
            fn_protos: HashMap::new(),
            fn_props: HashMap::new(),
            dom_expando: HashMap::new(),
            accessors: HashMap::new(),
            non_writable: std::collections::HashSet::new(),
            non_enum: std::collections::HashSet::new(),
            local_storage: HashMap::new(),
            session_storage: HashMap::new(),
            cookies: Vec::new(),
            cookie_writes: Vec::new(),
            layout_rects: HashMap::new(),
            scroll_state: HashMap::new(),
            scroll_writes: Vec::new(),
            scroll_into_view: Vec::new(),
            scroll_seq: 0,
            current_script: None,
            page_origin: String::new(),
            message_writes: Vec::new(),
            frame_seq: 0,
            ctx_proxies: HashMap::new(),
            frame_ctx: HashMap::new(),
            frame_mirrors: HashMap::new(),
            frame_docs: HashMap::new(),
            doc_write_buf: HashMap::new(),
            parser_writes: None,
            doc_writes: Vec::new(),
            doc_write_at: None,
            frame_host_owned: std::collections::HashSet::new(),
            attr_maps: HashMap::new(),
            native_memo: HashMap::new(),
            frame_dom_writes: Vec::new(),
            map_data: HashMap::new(),
            set_data: HashMap::new(),
            style_nodes: HashMap::new(),
            dataset_nodes: HashMap::new(),
            fn_proto_chain: HashMap::new(),
            ready_state: "loading",
            ty_names: [Value::UNDEFINED; 6],
            regexes: Vec::new(),
            promises: Vec::new(),
            rejected: Vec::new(),
            microtasks: std::collections::VecDeque::new(),
            timers: Vec::new(),
            next_timer_id: 0,
            timer_seq: 0,
            now_ms: 0.0,
            perf_origin: std::time::Instant::now(),
            perf_virtual: std::env::var("GG_JS_VIRTUAL_TIME")
                .is_ok_and(|v| v != "0"),
            pending_fetches: Vec::new(),
            awaiting: HashMap::new(),
            next_fetch_id: 0,
            fetch_body_atom: u32::MAX,
            text_atom: u32::MAX,
            json_atom: u32::MAX,
            rng_state: 0x2545_F491_4F6C_DD1D,
            fuel: DEFAULT_FUEL,
            profile_enabled: std::env::var_os("GG_JS_PROFILE").is_some(),
            cur_site: (0, 0),
            profile_samples: HashMap::new(),
            units_cache: None,
            ulen_cache: HashMap::new(),
            heap_bytes: 0,
        };
        for (i, name) in ["undefined", "boolean", "number", "string",
                          "object", "function"]
            .iter()
            .enumerate()
        {
            st.ty_names[i] = intern(&mut st, name);
        }
        // atoms for the async runtime's Response dispatch
        st.fetch_body_atom = st.intern_name("\u{1}__respbody");
        st.text_atom = st.intern_name("text");
        st.json_atom = st.intern_name("json");
        st
    }

    pub(super) fn take_profile_samples(
        &mut self,
    ) -> HashMap<(u32, u32), u64> {
        std::mem::take(&mut self.profile_samples)
    }

    /// Intern a property/global name into a stable id. Every name id is
    /// parallel to a globals slot (names double as the global namespace),
    /// so this also grows `globals`/`gdef`. Shared by the compiler-facing
    /// PageVm and runtime dynamic property access.
    pub(super) fn intern_name(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.name_ids.get(name) {
            return i;
        }
        let i = self.names.len() as u32;
        self.names.push(name.to_string());
        self.globals.push(Value::UNDEFINED);
        self.gdef.push(false);
        self.name_ids.insert(name.to_string(), i);
        i
    }
}

pub(super) fn intern(st: &mut St, s: &str) -> Value {
    if let Some(&i) = st.flat_index.get(s) {
        return Value::string(i);
    }
    let i = st.strs.len() as u32;
    st.strs.push(Str::Flat(s.to_string()));
    st.flat_index.insert(s.to_string(), i);
    Value::string(i)
}

pub(super) fn push_str(st: &mut St, s: String) -> Value {
    st.heap_bytes = st.heap_bytes.saturating_add(s.len() + 16);
    st.strs.push(Str::Flat(s));
    Value::string((st.strs.len() - 1) as u32)
}

/// make_native, but identity-stable: the same (tag, a, b) triple
/// always answers the same function object. Only for natives whose
/// behavior is fully determined by those fields.
pub(super) fn memo_native(
    st: &mut St, tag: u8, a: u32, b: u32, n: Native,
) -> Value {
    if let Some(&v) = st.native_memo.get(&(tag, a, b)) {
        return v;
    }
    let v = make_native(st, n);
    st.native_memo.insert((tag, a, b), v);
    v
}

pub(super) fn make_native(st: &mut St, n: Native) -> Value {
    st.closures.push(ClosureRec::Native(n));
    Value::function((st.closures.len() - 1) as u32)
}

pub(super) fn new_plain_object(st: &mut St) -> Value {
    st.objects.push(Obj {
        shape: 0,
        slots: Vec::new(),
        elems: Vec::new(),
        is_array: false,
        promise: PROMISE_NONE,
        regex: REGEX_NONE,
        proto: Value::UNDEFINED,
        has_accessors: false,
    });
    Value::object((st.objects.len() - 1) as u32)
}

fn new_array(st: &mut St, elems: Vec<Value>) -> Value {
    st.objects.push(Obj {
        shape: 0,
        slots: Vec::new(),
        elems,
        is_array: true,
        promise: PROMISE_NONE,
        regex: REGEX_NONE,
        proto: Value::UNDEFINED,
        has_accessors: false,
    });
    Value::object((st.objects.len() - 1) as u32)
}

/// If `v` is a RegExp object, its index into St.regexes.
fn regex_index(st: &St, v: Value) -> Option<usize> {
    if v.is_object() {
        let r = st.objects[v.index() as usize].regex;
        if r != REGEX_NONE {
            return Some(r as usize);
        }
    }
    None
}

/// Recursively flatten nested arrays up to `depth` levels — the shared
/// core of `Array.prototype.flat` and `.flatMap`. Non-array elements
/// (and arrays past the depth limit) are pushed as-is.
fn flatten_array(st: &St, elems: &[Value], depth: i64) -> Vec<Value> {
    let mut out = Vec::new();
    for &e in elems {
        if depth > 0
            && e.is_object()
            && st.objects[e.index() as usize].is_array
        {
            let inner = st.objects[e.index() as usize].elems.clone();
            out.extend(flatten_array(st, &inner, depth - 1));
        } else {
            out.push(e);
        }
    }
    out
}

/// Rewrite a JS regex source into one the Rust `regex` crate accepts.
/// JS and Rust's regex syntax diverge in a handful of common ways that
/// otherwise force whole patterns to compile as never-matching:
///   * `\uXXXX` (4 bare hex) -> `\u{XXXX}` (Rust requires the braces)
///   * a literal `[` inside a class -> `\[` (Rust reads it as a nested
///     class and errors; JS treats it as a literal). This is what makes
///     the ubiquitous regex-escape `/[\\^$.*+?()[\]{}|]/g` compile.
///   * `[^]` (JS "any char") -> `[\s\S]`, `[]` (JS "never") -> `[^\s\S]`
///     (Rust rejects an empty class outright)
///   * surrogate ranges like `[\uD800-\uDFFF]` -> the astral plane
///     `[\u{10000}-\u{10FFFF}]`; in our scalar (UTF-8) string world an
///     astral character is one scalar, not a surrogate pair, so this
///     preserves the author's intent ("match astral chars"). Lone
///     surrogate escapes become U+FFFD so the class still compiles.
/// Escapes are copied through verbatim so `\d`, `\w`, `\x5c`, `\b`, and
/// already-braced `\u{...}` are untouched.
fn translate_js_regex(src: &str) -> String {
    let cs: Vec<char> = src.chars().collect();
    let n = cs.len();
    let mut out = String::with_capacity(n + 8);
    let mut i = 0;
    let mut in_class = false;
    while i < n {
        let c = cs[i];
        if c == '\\' && i + 1 < n {
            let d = cs[i + 1];
            if d == 'u' && i + 2 < n && cs[i + 2] == '{' {
                // already braced: copy through to the closing '}'
                out.push('\\');
                out.push('u');
                i += 2;
                while i < n {
                    out.push(cs[i]);
                    let done = cs[i] == '}';
                    i += 1;
                    if done {
                        break;
                    }
                }
                continue;
            }
            if d == 'u'
                && i + 6 <= n
                && cs[i + 2..i + 6].iter().all(|c| c.is_ascii_hexdigit())
            {
                let hex: String =
                    cs[i + 2..i + 6].iter().collect::<String>().to_uppercase();
                out.push_str(&format!("\\u{{{hex}}}"));
                i += 6;
                continue;
            }
            if d == '0' && !(i + 2 < n && cs[i + 2].is_ascii_digit()) {
                // JS `\0` is NUL (only when not the head of an octal/backref
                // like `\0 1`). Rust's `regex` rejects a bare `\0`, which
                // degraded the whole pattern to never-matching — this is what
                // broke naver's URL-parser polyfill classes such as
                // `/[\0-~]/` and `/[\0\t\n\r #%/:<>?@[\\]^|]/`. Emit the
                // hex escape the `regex` crate accepts instead.
                out.push_str("\\x00");
                i += 2;
                continue;
            }
            // any other escape: copy the pair verbatim
            out.push('\\');
            out.push(d);
            i += 2;
            continue;
        }
        if !in_class {
            if c == '[' {
                // JS empty-class special cases close at the first ']'
                if i + 1 < n && cs[i + 1] == ']' {
                    out.push_str("[^\\s\\S]"); // [] matches nothing
                    i += 2;
                    continue;
                }
                if i + 2 < n && cs[i + 1] == '^' && cs[i + 2] == ']' {
                    out.push_str("[\\s\\S]"); // [^] matches anything
                    i += 3;
                    continue;
                }
                in_class = true;
            }
            out.push(c);
            i += 1;
            continue;
        }
        // inside a character class
        match c {
            ']' => {
                in_class = false;
                out.push(']');
            }
            '[' => out.push_str("\\["), // literal in JS, nested-class in Rust
            _ => out.push(c),
        }
        i += 1;
    }
    neutralize_surrogates(&out)
}

/// Map surrogate code points (never valid Rust scalars) to something
/// compilable: full/half surrogate ranges become the astral plane, and
/// any lone surrogate escape becomes U+FFFD. Operates on already
/// brace-normalized `\u{XXXX}` output from `translate_js_regex`.
fn neutralize_surrogates(s: &str) -> String {
    let mut t = s.to_string();
    for from in [
        "\\u{D800}-\\u{DFFF}",
        "\\u{D800}-\\u{DBFF}",
        "\\u{DC00}-\\u{DFFF}",
    ] {
        t = t.replace(from, "\\u{10000}-\\u{10FFFF}");
    }
    if !t.contains("\\u{D") && !t.contains("\\u{d") {
        return t;
    }
    let cs: Vec<char> = t.chars().collect();
    let n = cs.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        if i + 3 < n && cs[i] == '\\' && cs[i + 1] == 'u' && cs[i + 2] == '{' {
            let mut j = i + 3;
            let mut hex = String::new();
            while j < n && cs[j] != '}' {
                hex.push(cs[j]);
                j += 1;
            }
            if j < n && cs[j] == '}' {
                let is_surr = u32::from_str_radix(&hex, 16)
                    .map(|v| (0xD800..=0xDFFF).contains(&v))
                    .unwrap_or(false);
                if is_surr {
                    out.push_str("\\u{FFFD}");
                } else {
                    for k in i..=j {
                        out.push(cs[k]);
                    }
                }
                i = j + 1;
                continue;
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// Build a RegExp value from a JS pattern + flags. JS flags map to the
/// Translate a JS replacement string to the regex crate's syntax:
/// `$&` -> `${0}` (whole match) and `$<name>` -> `${name}` (named group).
/// Numbered `$1` / `${1}` already match the crate, so they pass through.
fn js_repl_to_rust(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            if chars[i + 1] == '&' {
                out.push_str("${0}");
                i += 2;
                continue;
            }
            if chars[i + 1] == '<' {
                if let Some(rel) =
                    chars[i + 2..].iter().position(|&c| c == '>')
                {
                    let name: String =
                        chars[i + 2..i + 2 + rel].iter().collect();
                    out.push_str("${");
                    out.push_str(&name);
                    out.push('}');
                    i = i + 2 + rel + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Build the `.groups` object for a match result: `{ name: capture }` for
/// each named group. Returns `undefined` when the pattern has no named
/// groups (matching the spec — `m.groups` is only an object when named
/// groups exist). `caps` is the positional capture list (index 0 = whole).
fn match_groups(st: &mut St, ri: usize, caps: &[Option<String>]) -> Value {
    let names = st.regexes[ri].re.capture_names();
    if !names.iter().any(|n| n.is_some()) {
        return Value::UNDEFINED;
    }
    let obj = new_plain_object(st);
    let oi = obj.index() as usize;
    for (i, name) in names.iter().enumerate() {
        if let Some(nm) = name {
            let atom = st.intern_name(nm);
            let v = match caps.get(i).and_then(|o| o.clone()) {
                Some(s) => make_string(st, s),
                None => Value::UNDEFINED,
            };
            raw_set_prop(st, oi, atom, v);
        }
    }
    obj
}

/// Attach `.groups` to a freshly built match-result array.
fn attach_groups(st: &mut St, arr: Value, ri: usize, caps: &[Option<String>]) {
    let groups = match_groups(st, ri, caps);
    let gatom = st.intern_name("groups");
    raw_set_prop(st, arr.index() as usize, gatom, groups);
}

/// `regex` crate's inline flags; `g` (global) is tracked separately.
fn new_regex(
    st: &mut St,
    pattern: &str,
    flags: &str,
) -> Result<Value, VmError> {
    let mut inline = String::new();
    for f in ['i', 'm', 's'] {
        if flags.contains(f) {
            inline.push(f);
        }
    }
    let translated = translate_js_regex(pattern);
    let full = if inline.is_empty() {
        translated
    } else {
        format!("(?{inline}){translated}")
    };
    let re = match regex::Regex::new(&full) {
        Ok(re) => CompiledRe::Std(re),
        Err(_) => {
            // `regex` rejected it — try fancy-regex (backreferences,
            // lookaround). date-fns's `(\w)\1*` tokenizer lives here.
            match fancy_regex::Regex::new(&full) {
                Ok(fr) => CompiledRe::Fancy(Box::new(fr)),
                Err(_) => {
                    // truly uncompilable (e.g. lone surrogates that
                    // can't match UTF-8 anyway) — never-matching
                    st.logs.push(format!(
                        "[gg] regex /{pattern}/{flags} unsupported - \
                         treated as never-matching"
                    ));
                    CompiledRe::Never
                }
            }
        }
    };
    st.regexes.push(RegexRec {
        re,
        global: flags.contains('g'),
        source: pattern.to_string(),
        flags: flags.to_string(),
    });
    let ri = (st.regexes.len() - 1) as u32;
    st.objects.push(Obj {
        shape: 0,
        slots: Vec::new(),
        elems: Vec::new(),
        is_array: false,
        promise: PROMISE_NONE,
        regex: ri,
        proto: Value::UNDEFINED,
        has_accessors: false,
    });
    let rv = Value::object((st.objects.len() - 1) as u32);
    // real instance properties (jQuery reads .source to rebuild
    // regexes: m.expr.match.bool.source.match(/\w+/g))
    let oi = rv.index() as usize;
    let sk = st.intern_name("source");
    let sv = push_str(st, pattern.to_string());
    raw_set_prop(st, oi, sk, sv);
    let fk = st.intern_name("flags");
    let fv = push_str(st, flags.to_string());
    raw_set_prop(st, oi, fk, fv);
    for (nm, on) in [
        ("global", flags.contains('g')),
        ("ignoreCase", flags.contains('i')),
        ("multiline", flags.contains('m')),
        ("sticky", flags.contains('y')),
        ("unicode", flags.contains('u')),
    ] {
        let k = st.intern_name(nm);
        raw_set_prop(st, oi, k, Value::boolean(on));
    }
    let lk = st.intern_name("lastIndex");
    raw_set_prop(st, oi, lk, Value::int(0));
    Ok(rv)
}

// ===================== async runtime (P3) =====================
// Event loop = a FIFO microtask queue + a virtual-clock timer list,
// drained by `pump`. Promises are plain objects with their state held
// in a side arena (St.promises). fetch() returns a promise and defers
// the real HTTP to the host (Python) driver via pending_fetches.
// No native async/await yet — Promise chains + timers are enough to
// materialize SPAs, and transpiled bundles lower await to .then.

#[derive(Clone, Copy)]
pub(super) enum PromiseState {
    Pending,
    Fulfilled(Value),
    Rejected(Value),
}

/// A queued reaction: run `handler(value)` (or pass the value through
/// when handler is None) and settle the derived promise with the result.
pub(super) struct Reaction {
    handler: Option<Value>,
    derived: u32, // promise id of the derived promise
    // `.finally(cb)`: run cb for its side effect, then forward the
    // ORIGINAL settlement to the derived promise (not cb's return value)
    finally: bool,
}

pub(super) struct PromiseRec {
    state: PromiseState,
    on_fulfill: Vec<Reaction>,
    on_reject: Vec<Reaction>,
    /// someone attached a rejection handler at some point. A rejected
    /// promise nobody ever asks about is reported once the microtask
    /// queue drains -- an error swallowed by a promise is otherwise
    /// completely silent, which is the worst way for a bundle to fail.
    handled: bool,
}

/// A microtask: either a bare callback (queueMicrotask) or a promise
/// reaction awaiting the pump.
pub(super) enum Job {
    Call { callback: Value, args: Vec<Value> },
    React {
        handler: Option<Value>,
        value: Value,
        derived: u32,
        is_reject: bool,
        finally: bool,
    },
    /// Promise Resolution Procedure step for a NON-native thenable
    /// (a foreign promise — core-js's polyfill, a userland library —
    /// or any `{then(res, rej)}` object): call `then` with the
    /// adopting promise's resolve/reject natives on a microtask.
    AdoptThen {
        thenable: Value,
        then: Value,
        resolve_fn: Value,
        reject_fn: Value,
        pid: u32,
    },
}

/// A same-origin child document, rebuilt inside the *parent's* VM.
///
/// This is a copy, not a handle: the host serializes the child's arena
/// and the parent reconstructs its own `dom::Document` from the bytes.
/// The engine's real selector engine and serializer then run against
/// it, so `contentDocument.querySelector(...)` is the same code path a
/// document uses on itself — while there is still no pointer from one
/// document's heap into another's.
pub(super) struct FrameMirror {
    pub(super) doc: dom::Document,
    /// child arena index -> mirror index, and back
    pub(super) idx_of: HashMap<u32, usize>,
    pub(super) ridx_of: Vec<u32>,
    /// child arena index -> the one element wrapper handed to JS, so
    /// `d.getElementById('x') === d.getElementById('x')` holds
    pub(super) wrappers: HashMap<u32, Value>,
    pub(super) url: String,
}

pub(super) struct Timer {
    id: u32,
    callback: Value,
    args: Vec<Value>,
    due_ms: f64,
    seq: u64,
    interval: Option<f64>,
    is_raf: bool,
}

pub(super) struct PendingFetch {
    pub(super) fetch_id: u32,
    promise: u32,
    pub(super) url: String,
    pub(super) method: String,
    pub(super) body: String,
    pub(super) headers: Vec<(String, String)>,
    pub(super) mode: String,
    pub(super) credentials: String,
}

fn new_promise(st: &mut St) -> (Value, u32) {
    let v = new_plain_object(st);
    st.promises.push(PromiseRec {
        state: PromiseState::Pending,
        on_fulfill: Vec::new(),
        on_reject: Vec::new(),
        handled: false,
    });
    let pid = (st.promises.len() - 1) as u32;
    st.objects[v.index() as usize].promise = pid;
    (v, pid)
}

pub(super) fn is_promise(st: &St, v: Value) -> bool {
    v.is_object() && st.objects[v.index() as usize].promise != PROMISE_NONE
}

fn promise_id_of(st: &St, v: Value) -> u32 {
    st.objects[v.index() as usize].promise
}

/// Register a reaction on promise `pid`. If already settled, the reaction
/// is scheduled as a microtask immediately (Promises/A+ ordering).
fn add_reaction(st: &mut St, pid: u32, reject_side: bool, rx: Reaction) {
    if reject_side {
        st.promises[pid as usize].handled = true;
    }
    match st.promises[pid as usize].state {
        PromiseState::Pending => {
            if reject_side {
                st.promises[pid as usize].on_reject.push(rx);
            } else {
                st.promises[pid as usize].on_fulfill.push(rx);
            }
        }
        PromiseState::Fulfilled(v) if !reject_side => {
            st.microtasks.push_back(Job::React {
                handler: rx.handler, value: v, derived: rx.derived,
                is_reject: false, finally: rx.finally,
            });
        }
        PromiseState::Rejected(v) if reject_side => {
            st.microtasks.push_back(Job::React {
                handler: rx.handler, value: v, derived: rx.derived,
                is_reject: true, finally: rx.finally,
            });
        }
        _ => {}
    }
}

/// Settle promise `pid`. Fulfilling with another promise ADOPTS it
/// (this promise follows the inner one). No-op if already settled.
pub(super) fn promise_settle(st: &mut St, pid: u32, value: Value, is_reject: bool) {
    if !matches!(st.promises[pid as usize].state, PromiseState::Pending) {
        return;
    }
    if !is_reject && is_promise(st, value) {
        let inner = promise_id_of(st, value);
        if inner == pid {
            // self-resolution: reject with a TypeError-ish string
            let msg = make_string(st, "Chaining cycle in promise".into());
            promise_settle(st, pid, msg, true);
            return;
        }
        // this promise follows `inner`: passthrough reactions on both sides
        add_reaction(st, inner, false,
            Reaction { handler: None, derived: pid, finally: false });
        add_reaction(st, inner, true,
            Reaction { handler: None, derived: pid, finally: false });
        return;
    }
    // Promise Resolution Procedure for foreign thenables: fulfilling
    // with a polyfilled promise (core-js) or any `{then}` object must
    // ADOPT its eventual value, not hand the thenable itself to
    // reactions. (naver: async/await over axios yields core-js
    // promises through this exact path.)
    if !is_reject && value.is_object() {
        let then_key = st.intern_name("then");
        if let PropHit::Data(thenv) =
            lookup_prop(st, value.index() as usize, then_key)
        {
            if thenv.is_function() {
                let resolve_fn =
                    make_native(st, Native::Resolve { pid, reject: false });
                let reject_fn =
                    make_native(st, Native::Resolve { pid, reject: true });
                st.microtasks.push_back(Job::AdoptThen {
                    thenable: value,
                    then: thenv,
                    resolve_fn,
                    reject_fn,
                    pid,
                });
                return; // stays pending until the thenable settles it
            }
        }
    }
    st.promises[pid as usize].state = if is_reject {
        PromiseState::Rejected(value)
    } else {
        PromiseState::Fulfilled(value)
    };
    if is_reject {
        st.rejected.push(pid);
    }
    // schedule the matching reaction set (the other set never runs)
    let reactions = if is_reject {
        std::mem::take(&mut st.promises[pid as usize].on_reject)
    } else {
        std::mem::take(&mut st.promises[pid as usize].on_fulfill)
    };
    for rx in reactions {
        st.microtasks.push_back(Job::React {
            handler: rx.handler, value, derived: rx.derived, is_reject,
            finally: rx.finally,
        });
    }
}

/// `p.then(onF, onR)` — returns a new derived promise.
fn promise_then(
    st: &mut St,
    recv_pid: u32,
    on_fulfilled: Value,
    on_rejected: Value,
) -> Value {
    let (dval, dpid) = new_promise(st);
    let on_f = if on_fulfilled.is_function() { Some(on_fulfilled) } else { None };
    let on_r = if on_rejected.is_function() { Some(on_rejected) } else { None };
    add_reaction(st, recv_pid, false,
        Reaction { handler: on_f, derived: dpid, finally: false });
    add_reaction(st, recv_pid, true,
        Reaction { handler: on_r, derived: dpid, finally: false });
    dval
}

/// `p.finally(cb)` — cb runs on both settlement paths for its side effect;
/// the derived promise mirrors p's original settlement (unless cb throws).
fn promise_finally(st: &mut St, recv_pid: u32, cb: Value) -> Value {
    let (dval, dpid) = new_promise(st);
    let h = if cb.is_function() { Some(cb) } else { None };
    add_reaction(st, recv_pid, false,
        Reaction { handler: h, derived: dpid, finally: true });
    add_reaction(st, recv_pid, true,
        Reaction { handler: h, derived: dpid, finally: true });
    dval
}

fn make_string(st: &mut St, s: String) -> Value {
    push_str(st, s)
}

/// Build a fetch Response object: { ok, status, url, text(), json() }.
/// The body is stored under a hidden atom so text()/json() can find it.
/// Response headers land in a plain `_h` map (lower-cased names); the JS
/// prelude wraps it into a Headers-like `headers` facade on first access.
fn new_response(
    st: &mut St,
    status: u16,
    url: &str,
    body: String,
    headers: &[(String, String)],
) -> Value {
    let v = new_plain_object(st);
    let oi = v.index() as usize;
    let ok_key = st.intern_name("ok");
    let status_key = st.intern_name("status");
    let url_key = st.intern_name("url");
    let ok = Value::boolean((200..300).contains(&status));
    raw_set_prop(st, oi, ok_key, ok);
    raw_set_prop(st, oi, status_key, Value::int(status as i32));
    let url_v = make_string(st, url.to_string());
    raw_set_prop(st, oi, url_key, url_v);
    let body_v = make_string(st, body);
    let body_atom = st.fetch_body_atom;
    raw_set_prop(st, oi, body_atom, body_v);
    let hmap = new_plain_object(st);
    let hi = hmap.index() as usize;
    for (name, value) in headers {
        let key = st.intern_name(&name.to_ascii_lowercase());
        let val = make_string(st, value.clone());
        raw_set_prop(st, hi, key, val);
    }
    let h_key = st.intern_name("_h");
    raw_set_prop(st, oi, h_key, hmap);
    v
}

/// Drain the microtask queue to empty, running each reaction/callback.
/// Callback errors reject the derived promise (never escape the pump).
/// Drain the microtask queue without collecting the fetches it
/// issued -- those stay queued for the next real pump. Used to run a
/// script's microtask checkpoint while it is still the current script.
pub(super) fn drain_microtasks_now(
    st: &mut St, mods: &ModStore, budget_max: usize,
) {
    let mut budget = budget_max;
    drain_microtasks(st, mods, &mut budget);
    report_unhandled_rejections(st, mods);
}

fn drain_microtasks(st: &mut St, mods: &ModStore, budget: &mut usize) {
    loop {
        // Budget check BEFORE the pop. Checking after meant the job
        // popped on the exhausted iteration was silently discarded --
        // one promise reaction eaten per bounded drain. A reaction
        // that vanishes neither runs nor rejects, so whatever awaited
        // it hangs forever with nothing pending: on naver, turbopack's
        // registerChunk gate lost its Promise.all continuation this
        // way whenever the load-time queue happened to be deep enough,
        // and the whole shopping module silently never hydrated.
        if *budget == 0 {
            return;
        }
        let Some(job) = st.microtasks.pop_front() else {
            return;
        };
        *budget -= 1;
        st.fuel = DEFAULT_FUEL; // fresh instruction budget per reaction
        match job {
            Job::Call { callback, args } => {
                if let Err(e) = call_value(st, mods, callback, &args) {
                    st.logs.push(format!("[gg-js error] {}", e.report()));
                }
            }
            Job::React { handler, value, derived, is_reject, finally } => {
                match handler {
                    None => promise_settle(st, derived, value, is_reject),
                    // .finally(cb): run cb, discard its result, then forward
                    // the ORIGINAL settlement. A throw in cb overrides it.
                    Some(h) if finally => {
                        match call_value(st, mods, h, &[]) {
                            Ok(_) => promise_settle(
                                st, derived, value, is_reject),
                            Err(e) => {
                                let reason = e.value.unwrap_or_else(|| {
                                    let s = e.msg.clone();
                                    make_string(st, s)
                                });
                                promise_settle(st, derived, reason, true);
                            }
                        }
                    }
                    Some(h) => match call_value(st, mods, h, &[value]) {
                        // a handler that returns normally FULFILLS the
                        // derived promise (even an onRejected handler — the
                        // rejection is considered handled)
                        Ok(ret) => promise_settle(st, derived, ret, false),
                        Err(e) => {
                            let reason = e.value.unwrap_or_else(|| {
                                let s = e.msg.clone();
                                make_string(st, s)
                            });
                            promise_settle(st, derived, reason, true);
                        }
                    },
                }
            }
            Job::AdoptThen { thenable, then, resolve_fn, reject_fn, pid } => {
                if let Err(e) = call_value_this(
                    st, mods, then, Some(thenable), &[resolve_fn, reject_fn],
                ) {
                    let reason = e.value.unwrap_or_else(|| {
                        let s = e.msg.clone();
                        make_string(st, s)
                    });
                    promise_settle(st, pid, reason, true);
                }
            }
        }
    }
}

/// Drain only the microtask checkpoint and hand newly-issued fetches to the
/// host. Module evaluation uses this before the full timer-aware pump so an
/// unrelated setTimeout cannot run ahead of DOMContentLoaded.
pub(super) fn pump_microtasks(
    st: &mut St,
    mods: &ModStore,
    budget_max: usize,
) -> Vec<PendingFetch> {
    let mut budget = budget_max;
    drain_microtasks(st, mods, &mut budget);
    if budget == 0 && !st.microtasks.is_empty() {
        st.logs.push("[gg-js] microtask budget exceeded".to_string());
    }
    report_unhandled_rejections(st, mods);
    let issued = std::mem::take(&mut st.pending_fetches);
    for request in &issued {
        st.awaiting.insert(request.fetch_id, request.promise);
    }
    issued
}

/// Report promises that rejected and were never asked about. Only
/// once the queue is empty: a handler attached later in the same drain
/// is not a swallowed error.
fn report_unhandled_rejections(st: &mut St, mods: &ModStore) {
    if st.rejected.is_empty() {
        return;
    }
    if !st.microtasks.is_empty() {
        return; // still settling; ask again next drain
    }
    let pending = std::mem::take(&mut st.rejected);
    for pid in pending {
        let rec = &st.promises[pid as usize];
        if rec.handled {
            continue;
        }
        let PromiseState::Rejected(v) = rec.state else { continue };
        st.promises[pid as usize].handled = true; // report once
        let msg = throw_msg(st, mods, v);
        let msg = msg.strip_prefix("uncaught ").unwrap_or(&msg).to_string();
        st.logs.push(format!("[gg-js error] unhandled rejection: {msg}"));
    }
}

fn next_due_timer(st: &St) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, t) in st.timers.iter().enumerate() {
        match best {
            None => best = Some(i),
            Some(b) => {
                let bt = &st.timers[b];
                if (t.due_ms, t.seq) < (bt.due_ms, bt.seq) {
                    best = Some(i);
                }
            }
        }
    }
    best
}

/// Event-loop pump: drain microtasks, then fire the earliest timer
/// Virtual-ms window that load-settling fast-forwards through. Timers due
/// past it are treated as animation/polling (fired later, at frame pace),
/// not initial content — big enough for real deferred init (React effects,
/// short setTimeouts) yet well under typical ticker/animation intervals.
pub(super) const SETTLE_HORIZON_MS: f64 = 250.0;

/// (advancing the virtual clock), repeat until both are empty or the
/// budget is hit. Returns the fetches issued this turn for the host to
/// service; they move to `awaiting` until resolve_fetch settles them.
pub(super) fn pump(
    st: &mut St,
    mods: &ModStore,
    budget_max: usize,
) -> Vec<PendingFetch> {
    let mut budget = budget_max;
    // Load-settle horizon: a self-rescheduling `setTimeout` (naver's
    // AutoRolling headline ticker re-arms a fresh timer every few seconds,
    // so the interval guard below can't catch it) would otherwise let the
    // fast-forward fire it up to the budget, re-rendering forever. Timers
    // due beyond this horizon of virtual time are animation/polling, not
    // initial content — leave them for later so settling captures the
    // first painted frame and terminates. Microtasks and fetches (the real
    // data path) are never horizon-gated.
    let horizon = st.now_ms + SETTLE_HORIZON_MS;
    // A setInterval never quiesces: re-arming it at `now + iv` and then
    // fast-forwarding to the next-due timer would fire it up to the whole
    // budget (200k) in one pump call — Naver's IntersectionObserver
    // polyfill polls on one, which cost ~50s per settle. During load
    // settling, fire each interval at most once per pump call (its effect
    // is steady-state polling), leave it registered so clearInterval still
    // finds it, and skip it for the rest of this call. has_pending_work
    // ignores intervals so the settle loop can reach quiescence.
    let mut fired_intervals: std::collections::HashSet<u32> =
        std::collections::HashSet::new();
    // A callback normally schedules the next rAF. Load settling captures
    // one initial animation frame instead of fast-forwarding the loop.
    let mut fired_raf = false;
    loop {
        drain_microtasks(st, mods, &mut budget);
        if budget == 0 {
            st.logs.push("[gg-js] event-loop budget exceeded".to_string());
            break;
        }
        // earliest timer that is not an already-fired interval
        let mut best: Option<usize> = None;
        for (i, t) in st.timers.iter().enumerate() {
            if t.due_ms > horizon {
                continue;
            }
            if t.interval.is_some() && fired_intervals.contains(&t.id) {
                continue;
            }
            if t.is_raf && fired_raf {
                continue;
            }
            match best {
                None => best = Some(i),
                Some(b) => {
                    let bt = &st.timers[b];
                    if (t.due_ms, t.seq) < (bt.due_ms, bt.seq) {
                        best = Some(i);
                    }
                }
            }
        }
        let Some(i) = best else { break };
        budget -= 1;
        st.fuel = DEFAULT_FUEL; // fresh budget per timer callback
        if let Some(iv) = st.timers[i].interval {
            // fire in place and re-arm for the next tick; mark it fired so
            // it can't drive the clock again this call
            let cb = st.timers[i].callback;
            let args = st.timers[i].args.clone();
            let id = st.timers[i].id;
            st.now_ms = st.now_ms.max(st.timers[i].due_ms);
            st.timer_seq += 1;
            st.timers[i].due_ms = st.now_ms + iv.max(0.0);
            st.timers[i].seq = st.timer_seq;
            fired_intervals.insert(id);
            if let Err(e) = call_value(st, mods, cb, &args) {
                st.logs.push(format!("[gg-js error] {}", e.report()));
            }
        } else {
            let t = st.timers.remove(i);
            fired_raf |= t.is_raf;
            st.now_ms = st.now_ms.max(t.due_ms);
            if let Err(e) = call_value(st, mods, t.callback, &t.args) {
                st.logs.push(format!("[gg-js error] {}", e.report()));
            }
        }
    }
    report_unhandled_rejections(st, mods);
    let issued = std::mem::take(&mut st.pending_fetches);
    for p in &issued {
        st.awaiting.insert(p.fetch_id, p.promise);
    }
    issued
}

/// One scheduler slice: drain microtasks, fire the SINGLE earliest timer
/// due within `horizon`, drain the microtasks it queued, and return.
/// `pump` fires every due timer in one call, so the host cannot interleave
/// layout between React's commit and its geometry-reading effects (both
/// run through separate `setTimeout(0)` scheduler slices). Stepping one
/// timer at a time lets the host refresh layout rects between slices, so an
/// effect that reads getBoundingClientRect sees real geometry — which is
/// what gates viewport-lazy sections (shopping/weather/stocks) from loading.
/// Returns (fetches issued, whether more load-time work remains).
pub(super) fn pump_step(
    st: &mut St,
    mods: &ModStore,
    budget_max: usize,
    horizon: f64,
) -> (Vec<(u32, String)>, bool) {
    let mut budget = budget_max;
    drain_microtasks(st, mods, &mut budget);
    // the single earliest timer due within the horizon (intervals included:
    // firing one advances the clock past its due, re-arming it beyond, so a
    // polling interval — the IntersectionObserver polyfill — steps forward
    // instead of spinning, and stops once the clock passes the horizon).
    let mut best: Option<usize> = None;
    for (i, t) in st.timers.iter().enumerate() {
        if t.due_ms > horizon {
            continue;
        }
        match best {
            None => best = Some(i),
            Some(b) => {
                let bt = &st.timers[b];
                if (t.due_ms, t.seq) < (bt.due_ms, bt.seq) {
                    best = Some(i);
                }
            }
        }
    }
    if let Some(i) = best {
        st.fuel = DEFAULT_FUEL;
        if let Some(iv) = st.timers[i].interval {
            let cb = st.timers[i].callback;
            let args = st.timers[i].args.clone();
            st.now_ms = st.now_ms.max(st.timers[i].due_ms);
            st.timer_seq += 1;
            st.timers[i].due_ms = st.now_ms + iv.max(0.0);
            st.timers[i].seq = st.timer_seq;
            if let Err(e) = call_value(st, mods, cb, &args) {
                st.logs.push(format!("[gg-js error] {}", e.report()));
            }
        } else {
            let t = st.timers.remove(i);
            st.now_ms = st.now_ms.max(t.due_ms);
            if let Err(e) = call_value(st, mods, t.callback, &t.args) {
                st.logs.push(format!("[gg-js error] {}", e.report()));
            }
        }
        drain_microtasks(st, mods, &mut budget);
    }
    report_unhandled_rejections(st, mods);
    let issued = std::mem::take(&mut st.pending_fetches);
    for p in &issued {
        st.awaiting.insert(p.fetch_id, p.promise);
    }
    let more = !st.microtasks.is_empty()
        || !st.awaiting.is_empty()
        || st.timers.iter().any(|t| t.due_ms <= horizon);
    (
        issued.into_iter().map(|p| (p.fetch_id, p.url)).collect(),
        more,
    )
}

/// Real-time slice of the event loop: fire only work due within the
/// next `dt_ms` of virtual time, then advance the clock to that point.
/// (Plain `pump` fast-forwards to quiescence — right for load-time
/// settling, wrong for a live page where a rAF loop must run at frame
/// pace, not spin to its budget.)
pub(super) fn pump_bounded(
    st: &mut St,
    mods: &ModStore,
    budget_max: usize,
    dt_ms: f64,
) -> Vec<PendingFetch> {
    let until = st.now_ms + dt_ms.max(0.0);
    let mut budget = budget_max;
    loop {
        drain_microtasks(st, mods, &mut budget);
        if budget == 0 {
            break;
        }
        match next_due_timer(st) {
            Some(i) if st.timers[i].due_ms <= until => {
                let t = st.timers.remove(i);
                st.now_ms = st.now_ms.max(t.due_ms);
                if let Some(iv) = t.interval {
                    st.timer_seq += 1;
                    st.timers.push(Timer {
                        id: t.id,
                        callback: t.callback,
                        args: t.args.clone(),
                        due_ms: st.now_ms + iv.max(0.0),
                        seq: st.timer_seq,
                        interval: Some(iv),
                        is_raf: false,
                    });
                }
                budget -= 1;
                st.fuel = DEFAULT_FUEL;
                if let Err(e) = call_value(st, mods, t.callback, &t.args)
                {
                    st.logs.push(format!("[gg-js error] {}", e.report()));
                }
            }
            _ => break,
        }
    }
    st.now_ms = st.now_ms.max(until);
    let issued = std::mem::take(&mut st.pending_fetches);
    for p in &issued {
        st.awaiting.insert(p.fetch_id, p.promise);
    }
    issued
}

/// Host (driver) settles a fetch: fulfill its promise with a Response.
pub(super) fn resolve_fetch(st: &mut St, fetch_id: u32, status: u16, body: String) {
    resolve_fetch_full(st, fetch_id, status, String::new(), body, Vec::new());
}

pub(super) fn resolve_fetch_full(
    st: &mut St,
    fetch_id: u32,
    status: u16,
    url: String,
    body: String,
    headers: Vec<(String, String)>,
) {
    if let Some(pid) = st.awaiting.remove(&fetch_id) {
        let resp = new_response(st, status, &url, body, &headers);
        promise_settle(st, pid, resp, false);
    }
}

pub(super) fn reject_fetch(st: &mut St, fetch_id: u32, message: String) {
    if let Some(pid) = st.awaiting.remove(&fetch_id) {
        let reason = make_string(st, message);
        promise_settle(st, pid, reason, true);
    }
}

pub(super) fn has_pending_work(st: &St) -> bool {
    // Intervals (setInterval) poll forever and never quiesce, so they do
    // not count as pending load-settle work — otherwise the settle loop
    // would spin on them until its wall-clock deadline. Likewise a one-shot
    // timer due beyond the settle horizon (an animation/ticker re-arm, not
    // initial content) is not pending load work. Near-due one-shots,
    // microtasks, and in-flight fetches are real pending work.
    !st.microtasks.is_empty()
        || st.timers.iter().any(|t| {
            t.interval.is_none() && !t.is_raf
                && t.due_ms <= st.now_ms + SETTLE_HORIZON_MS
        })
        || !st.pending_fetches.is_empty()
        || !st.awaiting.is_empty()
}

fn str_len(st: &St, i: u32) -> usize {
    match &st.strs[i as usize] {
        Str::Flat(s) => s.len(),
        Str::Cat { len, .. } => *len as usize,
    }
}

/// UTF-16 code-unit length of a string value, memoized. Strings are
/// immutable, so the cached count never goes stale — this turns a hot
/// `s.length` (recounted O(n) each read) into O(1) amortized.
fn str_u16_len(st: &mut St, i: u32) -> usize {
    if let Some(&n) = st.ulen_cache.get(&i) {
        return n as usize;
    }
    let n = str_ref(st, i).encode_utf16().count();
    st.ulen_cache.insert(i, n as u32);
    n
}

/// Indexed UTF-16 char read backing charAt (`want_code == false`) and
/// charCodeAt. Reuses the single-entry unit memo so a char-by-char scan
/// of one big string is O(n) amortized instead of rebuilding the whole
/// unit vector per call (O(n^2)). Strings are immutable, so the memo is
/// never stale.
fn str_char_read(st: &mut St, sidx: u32, i: f64, want_code: bool) -> Value {
    let hit = matches!(&st.units_cache, Some((c, _)) if *c == sidx);
    if !hit {
        let txt = str_ref(st, sidx).to_string();
        let u: Vec<u16> = txt.encode_utf16().collect();
        st.units_cache = Some((sidx, u));
    }
    let i = if i.is_nan() { 0.0 } else { i };
    let units = &st.units_cache.as_ref().unwrap().1;
    let ok = i >= 0.0 && (i as usize) < units.len();
    if want_code {
        return if ok {
            Value::int(units[i as usize] as i32)
        } else {
            Value::number(f64::NAN)
        };
    }
    let out = if ok {
        String::from_utf16_lossy(&units[i as usize..i as usize + 1])
    } else {
        String::new()
    };
    make_string(st, out)
}

/// Materialize a rope in place (iterative — chains can be 10k+ deep).
fn flatten(st: &mut St, i: u32) {
    if matches!(st.strs[i as usize], Str::Flat(_)) {
        return;
    }
    let mut out = String::with_capacity(str_len(st, i));
    let mut stack = vec![i];
    while let Some(k) = stack.pop() {
        match &st.strs[k as usize] {
            Str::Flat(s) => out.push_str(s),
            Str::Cat { a, b, .. } => {
                stack.push(*b);
                stack.push(*a);
            }
        }
    }
    // this buffer is the rope's bytes becoming real -- the node itself
    // was charged at concat, the text is charged exactly once, here
    st.heap_bytes = st.heap_bytes.saturating_add(out.len());
    st.strs[i as usize] = Str::Flat(out);
}

fn str_ref(st: &mut St, i: u32) -> &str {
    flatten(st, i);
    match &st.strs[i as usize] {
        Str::Flat(s) => s.as_str(),
        Str::Cat { .. } => unreachable!(),
    }
}

/// The element index a property name denotes, or None when the name is
/// an ordinary named property. Only the CANONICAL numeric form is an
/// index ("32" yes, "032"/"+1" no) — naver press ids are zero-padded
/// strings ("032"), and routing them through element storage made
/// o["032"] and o["32"] alias and then diverge across babel spreads.
fn elem_index(name: &str) -> Option<usize> {
    if name == "0" {
        return Some(0);
    }
    if name.is_empty() || name.starts_with('0') {
        return None;
    }
    if !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse::<usize>().ok()
}

pub(super) fn raw_get_prop(
    st: &St,
    oi: usize,
    key: u32,
) -> Option<Value> {
    let mut oi = oi;
    for _ in 0..16 {
        let o = &st.objects[oi];
        if let Some(&slot) = st.shapes[o.shape as usize].props.get(&key) {
            return Some(o.slots[slot as usize]);
        }
        if !o.proto.is_object() {
            return None;
        }
        oi = o.proto.index() as usize;
    }
    None
}

pub(super) fn raw_set_prop(st: &mut St, oi: usize, key: u32, v: Value) {
    let shape_id = st.objects[oi].shape;
    if let Some(&slot) = st.shapes[shape_id as usize].props.get(&key) {
        st.objects[oi].slots[slot as usize] = v;
        return;
    }
    let next = match st.shapes[shape_id as usize].transitions.get(&key) {
        Some(&n) => n,
        None => {
            let mut props = st.shapes[shape_id as usize].props.clone();
            props.insert(key, props.len() as u16);
            st.shapes.push(Shape { props, transitions: HashMap::new() });
            let n = (st.shapes.len() - 1) as u32;
            st.shapes[shape_id as usize].transitions.insert(key, n);
            n
        }
    };
    st.objects[oi].shape = next;
    st.objects[oi].slots.push(v);
}

#[inline]
fn num_of(v: Value) -> Result<f64, VmError> {
    if v.is_number() {
        Ok(v.to_number_raw())
    } else if v.is_boolean() {
        Ok(if v.as_bool() { 1.0 } else { 0.0 })
    } else if v.is_undefined() {
        Ok(f64::NAN)
    } else if v.is_null() {
        Ok(0.0)
    } else {
        err(format!("cannot convert {v:?} to a number (yet)"))
    }
}

/// ToNumber with string coercion (needs the string arena). Used by the
/// unary `+`, arithmetic, relational compares, and sort — so "3" * 2,
/// +"42", and "10" < "9" behave like real JS instead of throwing.
/// Objects go through ToPrimitive (valueOf/toString), which can call
/// back into JS — hence the mods parameter.
fn to_number(st: &mut St, mods: &ModStore, v: Value) -> Result<f64, VmError> {
    if v.is_string() {
        let s = str_ref(st, v.index()).trim().to_string();
        return Ok(if s.is_empty() {
            0.0
        } else {
            s.parse::<f64>().unwrap_or(f64::NAN)
        });
    }
    if v.is_object() {
        let p = to_primitive(st, mods, v, true)?;
        return to_number(st, mods, p);
    }
    num_of(v)
}

/// ToPrimitive for objects: valueOf/toString tried in hint order as real
/// calls, so user classes (the prelude Date, wrapped values in bundles)
/// convert with their own methods. A conversion attempt that fails or
/// returns another object falls through to the next; when nothing
/// produces a primitive we fall back to the display string instead of
/// throwing (plain objects coerce like "[object Object]").
pub(super) fn to_primitive(
    st: &mut St,
    mods: &ModStore,
    v: Value,
    number_hint: bool,
) -> Result<Value, VmError> {
    if !v.is_object() && proxy_id_of(st, v).is_none() {
        return Ok(v);
    }
    let order = if number_hint {
        ["valueOf", "toString"]
    } else {
        ["toString", "valueOf"]
    };
    for name in order {
        let key = st.intern_name(name);
        let m = if proxy_id_of(st, v).is_some() {
            internal_get(st, mods, v, key, v)?
        } else {
            match lookup_prop(st, v.index() as usize, key) {
                PropHit::Data(f) => f,
                PropHit::Getter(g) => {
                    call_value_this(st, mods, g, Some(v), &[])?
                }
                PropHit::Missing => continue,
            }
        };
        if !m.is_function() {
            continue;
        }
        if let Ok(r) = call_value_this(st, mods, m, Some(v), &[]) {
            if !r.is_object() {
                return Ok(r);
            }
        }
    }
    let s = to_display(st, v);
    Ok(push_str(st, s))
}

#[inline]
fn truthy(st: &St, v: Value) -> bool {
    if v.is_int() {
        v.as_i32() != 0
    } else if v.is_double() {
        let n = v.as_f64();
        n != 0.0 && !n.is_nan()
    } else if v.is_boolean() {
        v.as_bool()
    } else if v.is_string() {
        str_len(st, v.index()) != 0
    } else {
        !v.is_nullish()
    }
}

#[inline]
fn strict_eq(st: &mut St, x: Value, y: Value) -> bool {
    if x.is_number() && y.is_number() {
        // 1 === 1.0, NaN !== NaN, -0 === 0
        return x.to_number_raw() == y.to_number_raw();
    }
    if x.is_string() && y.is_string() {
        if x == y {
            return true;
        }
        if str_len(st, x.index()) != str_len(st, y.index()) {
            return false;
        }
        flatten(st, x.index());
        flatten(st, y.index());
        return match (
            &st.strs[x.index() as usize],
            &st.strs[y.index() as usize],
        ) {
            (Str::Flat(a), Str::Flat(b)) => a == b,
            _ => unreachable!(),
        };
    }
    x == y
}

fn loose_eq(
    st: &mut St,
    mods: &ModStore,
    x: Value,
    y: Value,
) -> Result<bool, VmError> {
    if x.is_nullish() || y.is_nullish() {
        return Ok(x.is_nullish() && y.is_nullish());
    }
    if x.is_string() && y.is_string() {
        return Ok(strict_eq(st, x, y));
    }
    // reference == reference is identity; object == primitive converts
    // the object side (ToPrimitive) and compares again
    let xr = x.is_object() || x.is_function() || x.is_dom_node();
    let yr = y.is_object() || y.is_function() || y.is_dom_node();
    if xr && yr {
        return Ok(x == y);
    }
    if x.is_object() {
        let xp = to_primitive(st, mods, x, true)?;
        return loose_eq(st, mods, xp, y);
    }
    if y.is_object() {
        let yp = to_primitive(st, mods, y, true)?;
        return loose_eq(st, mods, x, yp);
    }
    // mixed types compare by ToNumber (string -> number), like real ==
    Ok(to_number(st, mods, x)? == to_number(st, mods, y)?)
}

fn js_num_str(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        (if n > 0.0 { "Infinity" } else { "-Infinity" }).to_string()
    } else {
        format!("{n}")
    }
}

/// `key in obj` — own properties only (the engine has no prototype
/// chains); array element indices and `length` count as present.
fn has_own_property(
    st: &mut St,
    mods: &ModStore,
    obj: Value,
    key: Value,
) -> Result<bool, VmError> {
    // ToPropertyKey: object keys (polyfilled Symbol wrappers) go
    // through their toString, keeping tags distinct
    let key = if key.is_object() {
        to_primitive(st, mods, key, false)?
    } else {
        key
    };
    if obj.is_function() {
        // `'name' in fn` — statics, prototype, and callables
        let name = to_display(st, key);
        if matches!(name.as_str(),
                    "prototype" | "call" | "apply" | "bind"
                        | "length" | "name") {
            return Ok(true);
        }
        let key_id = st.intern_name(&name);
        return Ok(fn_static_lookup(st, obj.index(), key_id).is_some());
    }
    if obj.is_dom_node() {
        // feature detection ('ontouchstart' in document.documentElement)
        let name = to_display(st, key);
        let key_id = st.intern_name(&name);
        let v = dom_get_prop(st, key_id, obj.index())?;
        return Ok(!v.is_undefined());
    }
    if obj.is_object() && st.style_nodes.contains_key(&obj.index()) {
        // jQuery probes CSS support with `prop in div.style`; the
        // proxy accepts any property name, so membership is broad
        let name = to_display(st, key);
        return Ok(name
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic())
            .unwrap_or(false));
    }
    if obj.is_string() {
        // a string's own properties are its indices plus `length`
        let name = to_display(st, key);
        if name == "length" {
            return Ok(true);
        }
        let n = str_ref(st, obj.index()).chars().count();
        return Ok(elem_index(&name).is_some_and(|i| i < n));
    }
    if !obj.is_object() {
        return type_err("'in' right-hand side is not an object");
    }
    let oi = obj.index() as usize;
    let name = to_display(st, key);
    if st.objects[oi].is_array && name == "length" {
        return Ok(true);
    }
    // numeric keys live in dense element storage on every object
    // (SetIndex stores them there whether or not it is an array)
    if let Some(i) = elem_index(&name) {
        if i < st.objects[oi].elems.len()
            && !st.objects[oi].elems[i].is_undefined()
        {
            return Ok(true);
        }
    }
    // an un-interned name cannot be a property of anything
    let Some(&key_id) = st.name_ids.get(&name) else {
        return Ok(false);
    };
    let shape = st.objects[oi].shape as usize;
    Ok(st.shapes[shape].props.contains_key(&key_id)
        || (st.objects[oi].has_accessors
            && st.accessors.contains_key(&(oi as u32, key_id))))
}

/// Invoke an extracted builtin (`var f = ''.slice; f.call(s, 1)`).
/// Covers the methods polyfills actually extract; anything else names
/// itself in the error so the next gap is visible.
/// String.replace/replaceAll with a function replacement: for each match,
/// call `f(match, p1..pN, offset, whole_string)` and splice in its return
/// value. `matches` carries (whole, capture groups, byte start); the JS
/// offset is reported in UTF-16 code units to match String.length/.slice.
fn replace_with_fn(
    st: &mut St,
    mods: &ModStore,
    s: &str,
    matches: &[(String, Vec<Option<String>>, usize)],
    f: Value,
) -> Result<String, VmError> {
    let mut out = String::new();
    let mut last = 0usize;
    let full = make_string(st, s.to_string());
    for (whole, groups, start) in matches {
        if *start < last {
            continue; // never splice backwards (zero-width edge cases)
        }
        out.push_str(&s[last..*start]);
        let mut cargs: Vec<Value> = Vec::with_capacity(groups.len() + 3);
        cargs.push(make_string(st, whole.clone()));
        for g in groups {
            cargs.push(match g {
                Some(x) => make_string(st, x.clone()),
                None => Value::UNDEFINED,
            });
        }
        let off16 = s[..*start].encode_utf16().count() as i32;
        cargs.push(Value::int(off16));
        cargs.push(full);
        let r = call_value(st, mods, f, &cargs)?;
        out.push_str(&to_display(st, r));
        last = *start + whole.len();
    }
    out.push_str(&s[last..]);
    Ok(out)
}

/// `new Function(p1, ..., pN, body)`: compile the params + body into a real
/// callable closure in the *current* VM. Previously a stub that returned
/// `window`, which broke any library doing runtime code generation — most
/// visibly lodash `_.template`, whose final step is
/// `Function(importKeys, source).apply(undefined, importValues)` (naver's
/// search-autocomplete boot). Per spec the body is compiled in global scope,
/// so it captures no local variables — only its declared params and globals.
fn build_function(
    st: &mut St,
    mods: &ModStore,
    params: &str,
    body: &str,
) -> Result<Value, VmError> {
    let src = format!("(function anonymous({params}\n) {{\n{body}\n}})");
    let ast = super::parser::parse_program(&src).map_err(|e| VmError {
        msg: format!("{e:?}"),
        value: None,
        kind: "SyntaxError",
        trace: None,
    })?;
    let module = super::compiler::compile(&ast).map_err(|e| VmError {
        msg: format!("{e:?}"),
        value: None,
        kind: "SyntaxError",
        trace: None,
    })?;
    let mi = load_module(st, mods, module);
    let main = mods.rc(mi).module.main;
    let nregs = mods.rc(mi).module.protos[main as usize].nregs as usize;
    let base = st.regs.len();
    st.regs.resize(base + nregs, Value::UNDEFINED);
    let this_v = st.known.window;
    // current fuel budget applies (no reset) so a runaway page can't buy
    // more time by calling Function(); building the wrapper is cheap anyway
    let out = exec(st, mods, mi, main, base, u32::MAX, this_v, 0);
    st.regs.truncate(base);
    out
}

fn method_ref_dispatch(
    st: &mut St,
    mods: &ModStore,
    recv: Value,
    key: u32,
    args: &[Value],
) -> Result<Value, VmError> {
    let name = st.names[key as usize].clone();
    // promise receivers: `p.then` extracted as a value and invoked via
    // .call/.apply — core-js's thenable adoption does exactly
    // `then.call(promise, resolve, reject)` after READING p.then
    if recv.is_object()
        && st.objects[recv.index() as usize].promise != PROMISE_NONE
    {
        let pid = st.objects[recv.index() as usize].promise;
        let a0 = args.first().copied().unwrap_or(Value::UNDEFINED);
        let a1 = args.get(1).copied().unwrap_or(Value::UNDEFINED);
        match name.as_str() {
            "then" => return Ok(promise_then(st, pid, a0, a1)),
            "catch" => {
                return Ok(promise_then(st, pid, Value::UNDEFINED, a0))
            }
            _ => {}
        }
    }
    // array receivers: real element operations, not string ops
    if recv.is_object() && st.objects[recv.index() as usize].is_array {
        let oi = recv.index() as usize;
        // No-snapshot fast paths: mutate / scan the receiver in place so
        // a hot `push.apply(acc, chunk)` or `indexOf` loop over a GROWING
        // receiver does not clone the whole array on every call — that
        // eager clone was O(n) per call, i.e. O(n^2) over a build loop
        // (the dominant cost of Naver's React settle).
        match name.as_str() {
            "push" => {
                st.objects[oi].elems.extend_from_slice(args);
                return Ok(Value::int(
                    st.objects[oi].elems.len() as i32,
                ));
            }
            "pop" => {
                return Ok(st.objects[oi]
                    .elems
                    .pop()
                    .unwrap_or(Value::UNDEFINED));
            }
            "shift" => {
                return Ok(if st.objects[oi].elems.is_empty() {
                    Value::UNDEFINED
                } else {
                    st.objects[oi].elems.remove(0)
                });
            }
            "unshift" => {
                for &a in args.iter().rev() {
                    st.objects[oi].elems.insert(0, a);
                }
                return Ok(Value::int(
                    st.objects[oi].elems.len() as i32,
                ));
            }
            "reverse" => {
                st.objects[oi].elems.reverse();
                return Ok(recv);
            }
            "indexOf" | "lastIndexOf" => {
                let needle =
                    args.first().copied().unwrap_or(Value::UNDEFINED);
                let n = st.objects[oi].elems.len();
                let mut found = -1i32;
                if name == "indexOf" {
                    for i in 0..n {
                        let e = st.objects[oi].elems[i];
                        if strict_eq(st, e, needle) {
                            found = i as i32;
                            break;
                        }
                    }
                } else {
                    for i in (0..n).rev() {
                        let e = st.objects[oi].elems[i];
                        if strict_eq(st, e, needle) {
                            found = i as i32;
                            break;
                        }
                    }
                }
                return Ok(Value::int(found));
            }
            "at" => {
                let len = st.objects[oi].elems.len() as i64;
                let mut i = args
                    .first()
                    .map(|v| v.to_number_raw() as i64)
                    .unwrap_or(0);
                if i < 0 {
                    i += len;
                }
                return Ok(if i >= 0 && i < len {
                    st.objects[oi].elems[i as usize]
                } else {
                    Value::UNDEFINED
                });
            }
            "slice" => {
                // clone only the requested range, not the whole array
                let elen = st.objects[oi].elems.len() as f64;
                let clamp = |k: usize, default: f64| -> usize {
                    let v = args
                        .get(k)
                        .filter(|v| v.is_number())
                        .map(|v| v.to_number_raw())
                        .unwrap_or(default);
                    let v = if v < 0.0 {
                        (elen + v).max(0.0)
                    } else {
                        v.min(elen)
                    };
                    v as usize
                };
                let a = clamp(0, 0.0);
                let b = clamp(1, elen).max(a);
                let out = st.objects[oi].elems[a..b].to_vec();
                return Ok(new_array(st, out));
            }
            _ => {}
        }
        let elems = st.objects[oi].elems.clone();
        let elen = elems.len() as f64;
        let idx = |k: usize, default: f64| -> usize {
            let v = args
                .get(k)
                .filter(|v| v.is_number())
                .map(|v| v.to_number_raw())
                .unwrap_or(default);
            let v = if v < 0.0 { (elen + v).max(0.0) } else { v.min(elen) };
            v as usize
        };
        match name.as_str() {
            "slice" => {
                let a = idx(0, 0.0);
                let b = idx(1, elen).max(a);
                return Ok(new_array(st, elems[a..b].to_vec()));
            }
            "indexOf" => {
                let needle =
                    args.first().copied().unwrap_or(Value::UNDEFINED);
                let found = elems
                    .iter()
                    .position(|&e| strict_eq(st, e, needle))
                    .map(|p| p as i32)
                    .unwrap_or(-1);
                return Ok(Value::int(found));
            }
            "join" | "toString" => {
                let sep = if name == "join" {
                    match args.first() {
                        Some(v) if !v.is_undefined() => to_display(st, *v),
                        _ => ",".to_string(),
                    }
                } else {
                    ",".to_string()
                };
                let parts: Vec<String> = elems
                    .iter()
                    .map(|&e| {
                        if e.is_nullish() {
                            String::new()
                        } else {
                            to_display(st, e)
                        }
                    })
                    .collect();
                return Ok(make_string(st, parts.join(&sep)));
            }
            "concat" => {
                let mut out = elems.clone();
                for &a in args {
                    if a.is_object()
                        && st.objects[a.index() as usize].is_array
                    {
                        out.extend(
                            st.objects[a.index() as usize].elems.clone(),
                        );
                    } else {
                        out.push(a);
                    }
                }
                return Ok(new_array(st, out));
            }
            "push" => {
                let oi = recv.index() as usize;
                st.objects[oi].elems.extend_from_slice(args);
                return Ok(Value::int(
                    st.objects[oi].elems.len() as i32,
                ));
            }
            "pop" => {
                let oi = recv.index() as usize;
                return Ok(st.objects[oi]
                    .elems
                    .pop()
                    .unwrap_or(Value::UNDEFINED));
            }
            "shift" => {
                let oi = recv.index() as usize;
                return Ok(if st.objects[oi].elems.is_empty() {
                    Value::UNDEFINED
                } else {
                    st.objects[oi].elems.remove(0)
                });
            }
            "unshift" => {
                let oi = recv.index() as usize;
                for &a in args.iter().rev() {
                    st.objects[oi].elems.insert(0, a);
                }
                return Ok(Value::int(st.objects[oi].elems.len() as i32));
            }
            "reverse" => {
                st.objects[recv.index() as usize].elems.reverse();
                return Ok(recv);
            }
            "sort" => {
                let cmp = args.first().copied().filter(|v| v.is_function());
                let mut v = elems;
                merge_sort(st, mods, &mut v, cmp)?;
                st.objects[recv.index() as usize].elems = v;
                return Ok(recv);
            }
            "splice" => {
                let s = idx(0, 0.0);
                let dc = if args.len() > 1 {
                    match args.get(1).filter(|v| v.is_number()) {
                        Some(v) => (v.to_number_raw().max(0.0) as usize)
                            .min(elems.len() - s),
                        None => 0,
                    }
                } else {
                    elems.len() - s
                };
                let inserted: Vec<Value> =
                    args.iter().skip(2).copied().collect();
                let oi = recv.index() as usize;
                let removed: Vec<Value> = st.objects[oi]
                    .elems
                    .splice(s..s + dc, inserted)
                    .collect();
                return Ok(new_array(st, removed));
            }
            "lastIndexOf" => {
                let needle =
                    args.first().copied().unwrap_or(Value::UNDEFINED);
                let found = elems
                    .iter()
                    .rposition(|&e| strict_eq(st, e, needle))
                    .map(|p| p as i32)
                    .unwrap_or(-1);
                return Ok(Value::int(found));
            }
            "some" | "every" => {
                let want_all = name == "every";
                let cb = args
                    .first()
                    .copied()
                    .unwrap_or(Value::UNDEFINED);
                let cb_this = args.get(1).copied();
                for (i, &e) in elems.iter().enumerate() {
                    let r = call_value_this(
                        st, mods, cb, cb_this,
                        &[e, Value::int(i as i32), recv],
                    )?;
                    let t = truthy(st, r);
                    if t != want_all {
                        return Ok(Value::boolean(t));
                    }
                }
                return Ok(Value::boolean(want_all));
            }
            "reduce" => {
                let cb = args
                    .first()
                    .copied()
                    .unwrap_or(Value::UNDEFINED);
                let mut it = elems.iter().copied().enumerate();
                let mut acc = if args.len() > 1 {
                    args[1]
                } else {
                    match it.next() {
                        Some((_, v)) => v,
                        None => {
                            return type_err(
                                "Reduce of empty array with no \
                                 initial value",
                            )
                        }
                    }
                };
                for (i, e) in it {
                    acc = call_value(
                        st, mods, cb,
                        &[acc, e, Value::int(i as i32), recv],
                    )?;
                }
                return Ok(acc);
            }
            "values" | "keys" | "entries" | "@@iterator" => {
                // @@iterator: the fake-Symbol tag (prelude/core-js) —
                // behaves as values
                let arr = match name.as_str() {
                    "keys" => {
                        let ks: Vec<Value> = (0..elems.len())
                            .map(|i| Value::int(i as i32))
                            .collect();
                        new_array(st, ks)
                    }
                    "entries" => {
                        let ps: Vec<Value> = elems
                            .iter()
                            .enumerate()
                            .map(|(i, &v)| {
                                new_array(
                                    st,
                                    vec![Value::int(i as i32), v],
                                )
                            })
                            .collect();
                        new_array(st, ps)
                    }
                    _ => recv,
                };
                return Ok(make_array_iter(st, arr));
            }
            "forEach" | "map" | "filter" => {
                let cb = args
                    .first()
                    .copied()
                    .unwrap_or(Value::UNDEFINED);
                if !cb.is_function() {
                    return type_err(format!(
                        ".{name} callback is not a function"
                    ));
                }
                let cb_this = args.get(1).copied();
                let mut out: Vec<Value> = Vec::new();
                for (i, &el) in elems.iter().enumerate() {
                    let r = call_value_this(
                        st, mods, cb, cb_this,
                        &[el, Value::int(i as i32), recv],
                    )?;
                    match name.as_str() {
                        "map" => out.push(r),
                        "filter" => {
                            if truthy(st, r) {
                                out.push(el);
                            }
                        }
                        _ => {}
                    }
                }
                return Ok(if name == "forEach" {
                    Value::UNDEFINED
                } else {
                    new_array(st, out)
                });
            }
            "at" => {
                let len = elems.len() as i64;
                let mut i = args
                    .first()
                    .map(|v| v.to_number_raw() as i64)
                    .unwrap_or(0);
                if i < 0 {
                    i += len;
                }
                return Ok(if i >= 0 && i < len {
                    elems[i as usize]
                } else {
                    Value::UNDEFINED
                });
            }
            "flat" => {
                let depth = args
                    .first()
                    .map(|v| v.to_number_raw() as i64)
                    .unwrap_or(1);
                let out = flatten_array(st, &elems, depth);
                return Ok(new_array(st, out));
            }
            "flatMap" => {
                let cb = args.first().copied().unwrap_or(Value::UNDEFINED);
                let cb_this = args.get(1).copied();
                let mut mapped = Vec::with_capacity(elems.len());
                for (i, &e) in elems.iter().enumerate() {
                    let v = call_value_this(
                        st, mods, cb, cb_this,
                        &[e, Value::int(i as i32), recv],
                    )?;
                    mapped.push(v);
                }
                let out = flatten_array(st, &mapped, 1);
                return Ok(new_array(st, out));
            }
            "findLast" | "findLastIndex" => {
                let cb = args.first().copied().unwrap_or(Value::UNDEFINED);
                let cb_this = args.get(1).copied();
                let want_index = name == "findLastIndex";
                for i in (0..elems.len()).rev() {
                    let e = elems[i];
                    let v = call_value_this(
                        st, mods, cb, cb_this,
                        &[e, Value::int(i as i32), recv],
                    )?;
                    if truthy(st, v) {
                        return Ok(if want_index {
                            Value::int(i as i32)
                        } else {
                            e
                        });
                    }
                }
                return Ok(if want_index {
                    Value::int(-1)
                } else {
                    Value::UNDEFINED
                });
            }
            // Object.prototype staples reach arrays too — React calls
            // hasOwnProperty on prop arrays during reconciliation, and an
            // unhandled name here throws and aborts the work slice.
            "hasOwnProperty" => {
                let k = args.first().copied().unwrap_or(Value::UNDEFINED);
                return Ok(Value::boolean(
                    has_own_property(st, mods, recv, k)?,
                ));
            }
            "isPrototypeOf" => return Ok(Value::boolean(false)),
            "propertyIsEnumerable" => {
                let k = args
                    .first()
                    .map(|&v| to_display(st, v))
                    .unwrap_or_default();
                let is_idx = k
                    .parse::<usize>()
                    .map(|i| i < st.objects[oi].elems.len())
                    .unwrap_or(false);
                return Ok(Value::boolean(is_idx));
            }
            "valueOf" => return Ok(recv),
            _ => {
                return err(format!(
                    "extracted array builtin .{name}() not yet"
                ))
            }
        }
    }
    // array-like objects (jQuery instances: {0:.., 1:.., length:n}) —
    // a numeric length property routes Array.prototype methods to the
    // real array machinery before the display-string path below can
    // swallow them. Map/Set instances keep their own dispatch.
    if recv.is_object()
        && !st.map_data.contains_key(&recv.index())
        && !st.set_data.contains_key(&recv.index())
        && matches!(
            name.as_str(),
            "push" | "pop" | "slice" | "indexOf" | "lastIndexOf"
                | "join" | "concat" | "shift" | "unshift" | "splice"
                | "sort" | "reverse" | "forEach" | "map" | "filter"
                | "some" | "every" | "reduce"
        )
    {
        let lenk = st.intern_name("length");
        let has_len = raw_get_prop(st, recv.index() as usize, lenk)
            .map(|v| v.is_number())
            .unwrap_or(false);
        if has_len {
            return array_like_dispatch(st, mods, recv, &name, args);
        }
    }
    // Fast path: indexed single-char reads on a string receiver reuse a
    // memoized UTF-16 unit vector, so an uncurried charAt/charCodeAt
    // scanner over a big string is O(n) amortized, not O(n^2).
    if recv.is_string()
        && matches!(name.as_str(), "charAt" | "charCodeAt")
    {
        let iarg = args
            .first()
            .map(|v| v.to_number_raw())
            .unwrap_or(0.0);
        return Ok(str_char_read(
            st, recv.index(), iarg, name == "charCodeAt",
        ));
    }
    let s = to_display(st, recv);
    let units: Vec<u16> = s.encode_utf16().collect();
    let len = units.len() as f64;
    let nums: Vec<f64> = args
        .iter()
        .map(|v| if v.is_number() { v.to_number_raw() } else { f64::NAN })
        .collect();
    let idx_arg = |k: usize| -> f64 {
        nums.get(k).copied().unwrap_or(f64::NAN)
    };
    let clamp = |v: f64, hi: f64| -> usize {
        let v = if v.is_nan() { 0.0 } else { v };
        let v = if v < 0.0 { (hi + v).max(0.0) } else { v.min(hi) };
        v as usize
    };
    match name.as_str() {
        // The plain string transforms, reached through the extracted
        // form (`var f = "".toLowerCase; f.call(s)` -- core-js's
        // uncurryThis does this to every String.prototype method, and
        // bundles then call them everywhere).
        "toLowerCase" | "toLocaleLowerCase" => {
            Ok(make_string(st, s.to_lowercase()))
        }
        "toUpperCase" | "toLocaleUpperCase" => {
            Ok(make_string(st, s.to_uppercase()))
        }
        "trim" => Ok(make_string(st, s.trim().to_string())),
        "trimStart" | "trimLeft" => {
            Ok(make_string(st, s.trim_start().to_string()))
        }
        "trimEnd" | "trimRight" => {
            Ok(make_string(st, s.trim_end().to_string()))
        }
        "startsWith" => {
            let needle = args
                .first()
                .map(|&v| to_display(st, v))
                .unwrap_or_default();
            let from = clamp(idx_arg(1), len);
            let rest = String::from_utf16_lossy(&units[from.min(units.len())..]);
            Ok(Value::boolean(rest.starts_with(&needle)))
        }
        "endsWith" => {
            let needle = args
                .first()
                .map(|&v| to_display(st, v))
                .unwrap_or_default();
            let end = if args.len() > 1 {
                clamp(idx_arg(1), len)
            } else {
                units.len()
            };
            let head = String::from_utf16_lossy(&units[..end.min(units.len())]);
            Ok(Value::boolean(head.ends_with(&needle)))
        }
        "includes" => {
            let needle = args
                .first()
                .map(|&v| to_display(st, v))
                .unwrap_or_default();
            Ok(Value::boolean(s.contains(&needle)))
        }
        "lastIndexOf" => {
            let needle = args
                .first()
                .map(|&v| to_display(st, v))
                .unwrap_or_default();
            let nu: Vec<u16> = needle.encode_utf16().collect();
            let mut found: f64 = -1.0;
            if nu.len() <= units.len() {
                for i in 0..=(units.len() - nu.len()) {
                    if units[i..i + nu.len()] == nu[..] {
                        found = i as f64;
                    }
                }
            }
            Ok(Value::number(found))
        }
        "substring" | "substr" => {
            let a = clamp(idx_arg(0), len);
            let b = if args.len() > 1 {
                if name == "substr" {
                    (a + clamp(idx_arg(1), len)).min(units.len())
                } else {
                    clamp(idx_arg(1), len)
                }
            } else {
                units.len()
            };
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            Ok(make_string(
                st,
                String::from_utf16_lossy(&units[a.min(units.len())..b.min(units.len())]),
            ))
        }
        "padStart" | "padEnd" => {
            let want = idx_arg(0);
            let want = if want.is_nan() { 0.0 } else { want } as usize;
            let fill = if args.len() > 1 {
                to_display(st, args[1])
            } else {
                " ".to_string()
            };
            let mut out = s.clone();
            if fill.is_empty() || units.len() >= want {
                return Ok(make_string(st, out));
            }
            let mut pad = String::new();
            while pad.encode_utf16().count() + units.len() < want {
                pad.push_str(&fill);
            }
            let keep = want - units.len();
            let pu: Vec<u16> = pad.encode_utf16().collect();
            let pad = String::from_utf16_lossy(&pu[..keep.min(pu.len())]);
            if name == "padStart" {
                out = pad + &out;
            } else {
                out.push_str(&pad);
            }
            Ok(make_string(st, out))
        }
        "repeat" => {
            let n = idx_arg(0);
            let n = if n.is_nan() || n < 0.0 { 0.0 } else { n } as usize;
            Ok(make_string(st, s.repeat(n.min(10_000))))
        }
        "concat" => {
            // String.prototype.concat: recv then each arg coerced to
            // string. Reached via the extracted form (`"".concat.call(s,
            // ...)` — jindo/core-js string helpers), which otherwise
            // fell through to the unsupported-builtin error.
            let mut out = s.clone();
            for &a in args {
                // an object argument converts through its own toString,
                // so `"msg: ".concat(err)` carries the error's message
                let piece = to_js_string(st, mods, a)?;
                out.push_str(&piece);
            }
            Ok(make_string(st, out))
        }
        "slice" | "substring" => {
            let a = clamp(idx_arg(0), len);
            let b = if args.len() > 1 {
                clamp(idx_arg(1), len)
            } else {
                len as usize
            };
            let (a, b) = if name == "substring" && a > b {
                (b, a)
            } else {
                (a, b.max(a))
            };
            let out = String::from_utf16_lossy(&units[a..b]);
            Ok(make_string(st, out))
        }
        "charAt" => {
            let i = idx_arg(0);
            let i = if i.is_nan() { 0.0 } else { i };
            let out = if i >= 0.0 && (i as usize) < units.len() {
                String::from_utf16_lossy(&units[i as usize..i as usize + 1])
            } else {
                String::new()
            };
            Ok(make_string(st, out))
        }
        "charCodeAt" => {
            let i = idx_arg(0);
            let i = if i.is_nan() { 0.0 } else { i };
            Ok(if i >= 0.0 && (i as usize) < units.len() {
                Value::int(units[i as usize] as i32)
            } else {
                Value::number(f64::NAN)
            })
        }
        "indexOf" => {
            let needle = args
                .first()
                .map(|&v| to_display(st, v))
                .unwrap_or_default();
            Ok(Value::int(match s.find(&needle) {
                Some(byte) => s[..byte].encode_utf16().count() as i32,
                None => -1,
            }))
        }
        "toExponential" => {
            let x = recv.to_number_raw();
            if !x.is_finite() {
                let d = to_display(st, recv);
                return Ok(make_string(st, d));
            }
            let out = match nums.first().copied() {
                Some(d) if !d.is_nan() => {
                    let d = (d.max(0.0).min(100.0)) as usize;
                    format!("{x:.d$e}")
                }
                _ => format!("{x:e}"),
            };
            // JS writes a plus on non-negative exponents ("1e+2")
            let out = if let Some(pos) = out.find('e') {
                let (m, e) = out.split_at(pos);
                if e.as_bytes().get(1) == Some(&b'-') {
                    out.clone()
                } else {
                    format!("{m}e+{}", &e[1..])
                }
            } else {
                out
            };
            Ok(make_string(st, out))
        }
        "toPrecision" => {
            let x = recv.to_number_raw();
            let out = match nums.first().copied() {
                Some(d) if d.is_finite() && d >= 1.0 => {
                    let d = d as usize;
                    // width-precision significant digits
                    let s = format!("{x:.*}", d.saturating_sub(
                        (x.abs().max(1e-300).log10().floor() as i64
                            + 1).max(0) as usize,
                    ));
                    s
                }
                _ => {
                    let d = to_display(st, recv);
                    d
                }
            };
            Ok(make_string(st, out))
        }
        "toString" => {
            // number radix support: (255).toString(16) -> "ff"
            if recv.is_number() && !nums.is_empty() && !nums[0].is_nan() {
                let radix = nums[0] as u32;
                let v = recv.to_number_raw();
                if (2..=36).contains(&radix)
                    && radix != 10
                    && v.fract() == 0.0
                    && v.abs() < 9e15
                {
                    let neg = v < 0.0;
                    let mut n = v.abs() as u64;
                    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
                    let mut out = Vec::new();
                    loop {
                        out.push(digits[(n % radix as u64) as usize]);
                        n /= radix as u64;
                        if n == 0 {
                            break;
                        }
                    }
                    if neg {
                        out.push(b'-');
                    }
                    out.reverse();
                    let sr = String::from_utf8(out).unwrap();
                    return Ok(make_string(st, sr));
                }
            }
            Ok(make_string(st, s))
        }
        "valueOf" => Ok(if recv.is_string() {
            make_string(st, s)
        } else {
            recv
        }),
        "replace" => {
            let pat = args.first().copied().unwrap_or(Value::UNDEFINED);
            let rep_val = args.get(1).copied().unwrap_or(Value::UNDEFINED);
            // function replacement: call rep(match, ...groups, offset, str)
            // per match. Without this, a callback was coerced to the string
            // "function..." and spliced in literally, which broke lodash's
            // `_.template` (string.replace(reDelimiters, fn)) — the crash
            // that stalled naver's search-autocomplete boot.
            if rep_val.is_function() {
                let out = if let Some(ri) = regex_index(st, pat) {
                    let global = st.regexes[ri].global;
                    let matches = st.regexes[ri].re.captures_all(&s, global);
                    replace_with_fn(st, mods, &s, &matches, rep_val)?
                } else {
                    let needle = to_display(st, pat);
                    match s.find(&needle) {
                        Some(start) if !needle.is_empty() => {
                            let m = vec![(needle, Vec::new(), start)];
                            replace_with_fn(st, mods, &s, &m, rep_val)?
                        }
                        _ => s.clone(),
                    }
                };
                return Ok(make_string(st, out));
            }
            let rep = to_display(st, rep_val);
            let out = if let Some(ri) = regex_index(st, pat) {
                if st.regexes[ri].global {
                    st.regexes[ri].re.replace_all_str(&s, rep.as_str())
                } else {
                    st.regexes[ri].re.replace_first_str(&s, rep.as_str())
                }
            } else {
                let needle = to_display(st, pat);
                s.replacen(&needle, &rep, 1)
            };
            Ok(make_string(st, out))
        }
        "split" => {
            let pat = args.first().copied().unwrap_or(Value::UNDEFINED);
            let parts: Vec<Value> = if let Some(ri) = regex_index(st, pat)
            {
                let raw = st.regexes[ri].re.split_vec(&s);
                if raw.len() > MAX_MATERIALIZE {
                    return range_err("too many split results");
                }
                raw.into_iter()
                    .map(|p| make_string(st, p))
                    .collect()
            } else if pat.is_undefined() {
                vec![make_string(st, s.clone())]
            } else {
                let needle = to_display(st, pat);
                if needle.is_empty() {
                    if s.len() > MAX_MATERIALIZE {
                        return range_err("too many split results");
                    }
                    s.chars()
                        .map(|c| make_string(st, c.to_string()))
                        .collect()
                } else {
                    s.split(&needle)
                        .map(|p| make_string(st, p.to_string()))
                        .collect()
                }
            };
            Ok(new_array(st, parts))
        }
        "propertyIsEnumerable" => {
            let k = args.first().copied().unwrap_or(Value::UNDEFINED);
            if !is_js_object(recv) {
                return Ok(Value::boolean(false));
            }
            let (key, _) = property_key(st, mods, k)?;
            Ok(Value::boolean(
                !internal_get_own_descriptor(st, mods, recv, key)?
                    .is_undefined(),
            ))
        }
        "isPrototypeOf" => {
            let x = args.first().copied().unwrap_or(Value::UNDEFINED);
            if !recv.is_object() || !x.is_object() {
                return Ok(Value::boolean(false));
            }
            let mut p = st.objects[x.index() as usize].proto;
            for _ in 0..16 {
                if !p.is_object() {
                    return Ok(Value::boolean(false));
                }
                if p == recv {
                    return Ok(Value::boolean(true));
                }
                p = st.objects[p.index() as usize].proto;
            }
            Ok(Value::boolean(false))
        }
        "hasOwnProperty" => {
            let k = args.first().copied().unwrap_or(Value::UNDEFINED);
            // strings and functions both keep own properties outside
            // the shape, so the descriptor lookup below cannot see
            // them — `f.hasOwnProperty('name')` answered false
            if recv.is_string() || recv.is_function() {
                return Ok(Value::boolean(
                    has_own_property(st, mods, recv, k)?,
                ));
            }
            if !is_js_object(recv) {
                return Ok(Value::boolean(false));
            }
            let (key, _) = property_key(st, mods, k)?;
            Ok(Value::boolean(
                !internal_get_own_descriptor(st, mods, recv, key)?
                    .is_undefined(),
            ))
        }
        // array-iterator protocol (see make_array_iter)
        "next" if recv.is_object() => {
            let ii = recv.index() as usize;
            let ka = st.intern_name("__it_arr");
            let Some(arr) = raw_get_prop(st, ii, ka) else {
                return type_err(".next() on a non-iterator");
            };
            let ki = st.intern_name("__it_i");
            let i = raw_get_prop(st, ii, ki)
                .map(|v| v.to_number_raw() as usize)
                .unwrap_or(0);
            let elems = &st.objects[arr.index() as usize].elems;
            let (value, done) = if i < elems.len() {
                (elems[i], false)
            } else {
                (Value::UNDEFINED, true)
            };
            raw_set_prop(st, ii, ki, Value::int(i as i32 + 1));
            let out = new_plain_object(st);
            let oi = out.index() as usize;
            let kv = st.intern_name("value");
            raw_set_prop(st, oi, kv, value);
            let kd = st.intern_name("done");
            raw_set_prop(st, oi, kd, Value::boolean(done));
            Ok(out)
        }
        // @@iterator(): arrays/Sets/Maps hand out a fresh iterator;
        // iterator objects return themselves
        "@@iterator" if recv.is_object() => {
            let oi = recv.index();
            if st.objects[oi as usize].is_array {
                return Ok(make_array_iter(st, recv));
            }
            if let Some(vals) = st.set_data.get(&oi) {
                let vals = vals.clone();
                let a = new_array(st, vals);
                return Ok(make_array_iter(st, a));
            }
            if let Some(entries) = st.map_data.get(&oi) {
                let entries = entries.clone();
                let pairs: Vec<Value> = entries
                    .iter()
                    .map(|&(k, v)| new_array(st, vec![k, v]))
                    .collect();
                let a = new_array(st, pairs);
                return Ok(make_array_iter(st, a));
            }
            Ok(recv)
        }
        // Array.prototype.values/keys/entries/@@iterator: real
        // iterators (@@iterator = the fake-Symbol tag core-js and the
        // prelude install; behaves as values)
        "values" | "keys" | "entries" | "@@iterator"
            if recv.is_object()
                && st.objects[recv.index() as usize].is_array =>
        {
            let name = st.names[key as usize].clone();
            let elems =
                st.objects[recv.index() as usize].elems.clone();
            let arr = match name.as_str() {
                "keys" => {
                    let ks: Vec<Value> = (0..elems.len())
                        .map(|i| Value::int(i as i32))
                        .collect();
                    new_array(st, ks)
                }
                "entries" => {
                    let ps: Vec<Value> = elems
                        .iter()
                        .enumerate()
                        .map(|(i, &v)| {
                            new_array(
                                st,
                                vec![Value::int(i as i32), v],
                            )
                        })
                        .collect();
                    new_array(st, ps)
                }
                _ => recv,
            };
            Ok(make_array_iter(st, arr))
        }
        // extracted Set/Map prototype methods re-applied to instances
        _ if recv.is_object()
            && (st.set_data.contains_key(&recv.index())
                || st.map_data.contains_key(&recv.index())) =>
        {
            let name = st.names[key as usize].clone();
            let is_set = st.set_data.contains_key(&recv.index());
            let n = if is_set {
                let op = match name.as_str() {
                    "add" => 0u8,
                    "has" => 1,
                    "delete" => 2,
                    "clear" => 3,
                    "forEach" => 4,
                    "values" | "keys" | "entries" => 5,
                    _ => {
                        return type_err(format!(
                            "Set has no .{name}()"
                        ))
                    }
                };
                Native::SetOp { obj: recv.index(), op }
            } else {
                let op = match name.as_str() {
                    "get" => 0u8,
                    "set" => 1,
                    "has" => 2,
                    "delete" => 3,
                    "clear" => 4,
                    "forEach" => 5,
                    "keys" => 6,
                    "values" => 7,
                    "entries" => 8,
                    _ => {
                        return type_err(format!(
                            "Map has no .{name}()"
                        ))
                    }
                };
                Native::MapOp { obj: recv.index(), op }
            };
            let top = st.regs.len();
            st.regs.extend_from_slice(args);
            let r = do_native(st, mods, n, top, args.len() as u8);
            st.regs.truncate(top);
            r
        }
        // extracted RegExp.prototype.test/exec re-applied to a regex
        // (core-js regexp-exec calls them via functionCall)
        "test" if regex_index(st, recv).is_some() => {
            let ri = regex_index(st, recv).unwrap();
            let subject = args
                .first()
                .map(|&v| to_display(st, v))
                .unwrap_or_default();
            Ok(Value::boolean(st.regexes[ri].re.is_match(&subject)))
        }
        "exec" if regex_index(st, recv).is_some() => {
            let ri = regex_index(st, recv).unwrap();
            let subject = args
                .first()
                .map(|&v| to_display(st, v))
                .unwrap_or_default();
            let groups = st.regexes[ri].re.captures_owned(&subject);
            Ok(match groups {
                None => Value::NULL,
                Some(gs) => {
                    let vals: Vec<Value> = gs
                        .iter()
                        .map(|g| match g {
                            Some(s) => push_str(st, s.clone()),
                            None => Value::UNDEFINED,
                        })
                        .collect();
                    let arr = new_array(st, vals);
                    attach_groups(st, arr, ri, &gs);
                    arr
                }
            })
        }
        // no ICU on board: toLocaleString falls back to the default
        // string conversion (numbers, dates, arrays all coerce sanely)
        "toLocaleString" => {
            let s = to_display(st, recv);
            Ok(make_string(st, s))
        }
        // calling any method on nullish is a real TypeError (core-js
        // feature tests rely on catching it)
        other if recv.is_nullish() => type_err(format!(
            "cannot call .{other}() of {}",
            if recv.is_null() { "null" } else { "undefined" },
        )),
        other => err(format!(
            "extracted builtin .{other}() on {recv:?} is not \
             supported yet"
        )),
    }
}

/// Run an Array.prototype method against an array-like object: build a
/// scratch array from (elems, length), delegate to the array dispatch,
/// then write mutations and the new length back.
fn array_like_dispatch(
    st: &mut St,
    mods: &ModStore,
    recv: Value,
    name: &str,
    args: &[Value],
) -> Result<Value, VmError> {
    let oi = recv.index() as usize;
    let lenk = st.intern_name("length");
    let stored = st.objects[oi].elems.clone();
    let len = match raw_get_prop(st, oi, lenk) {
        Some(v) if v.is_number() => {
            (v.to_number_raw().max(0.0) as usize).min(1 << 20)
        }
        _ => stored.len(),
    };
    let view: Vec<Value> = (0..len)
        .map(|i| stored.get(i).copied().unwrap_or(Value::UNDEFINED))
        .collect();
    let tmp = new_array(st, view);
    let key = st.intern_name(name);
    let r = method_ref_dispatch(st, mods, tmp, key, args)?;
    if matches!(
        name,
        "push" | "pop" | "shift" | "unshift" | "splice" | "sort"
            | "reverse"
    ) {
        let out = st.objects[tmp.index() as usize].elems.clone();
        let n = out.len();
        st.objects[oi].elems = out;
        raw_set_prop(st, oi, lenk, Value::int(n as i32));
    }
    // methods that answer the receiver must answer the array-like,
    // not the scratch array (jQuery chains off .sort()/.reverse())
    Ok(if r == tmp { recv } else { r })
}

/// A property set on the window object (the global namespace's other
/// half): `window.X = v` must be readable as bare `X`.
fn window_prop(st: &St, key: u32) -> Option<Value> {
    let w = st.known.window;
    if !w.is_object() {
        return None;
    }
    match lookup_prop(st, w.index() as usize, key) {
        PropHit::Data(v) => Some(v),
        _ => None,
    }
}

/// A real JS iterator over an array's elements: a plain object with
/// receiver-dispatched `next` (extraction-safe — core-js pulls `next`
/// off one iterator and applies it to another).
pub(super) fn make_array_iter(st: &mut St, arr: Value) -> Value {
    let it = new_plain_object(st);
    let ii = it.index() as usize;
    let ka = st.intern_name("__it_arr");
    raw_set_prop(st, ii, ka, arr);
    let ki = st.intern_name("__it_i");
    raw_set_prop(st, ii, ki, Value::int(0));
    let kn = st.intern_name("next");
    let f = make_native(st, Native::MethodRef(kn));
    raw_set_prop(st, ii, kn, f);
    let kit = st.intern_name("@@iterator");
    let fit = make_native(st, Native::MethodRef(kit));
    raw_set_prop(st, ii, kit, fit);
    it
}

/// Static property lookup on a function, walking the function
/// [[Prototype]] chain (`Object.setPrototypeOf(Sub, Sup)` statics).
fn fn_static_lookup(st: &St, fidx: u32, key: u32) -> Option<Value> {
    let mut cur = fidx;
    for _ in 0..8 {
        if let Some(&v) = st.fn_props.get(&(cur, key)) {
            return Some(v);
        }
        match st.fn_proto_chain.get(&cur) {
            Some(p) if p.is_function() => cur = p.index(),
            Some(p) if p.is_object() => {
                return match lookup_prop(st, p.index() as usize, key) {
                    PropHit::Data(v) => Some(v),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
    None
}

/// A function's arity — its declared parameter count — for `func.length`.
/// Without this, `func.length` read as undefined, so lodash's `overRest`
/// computed `undefined - 1 = NaN` for its rest-arg start, emptied the
/// rest-args array, and crashed naver's search-autocomplete boot on
/// `.length of undefined`. Native/bound functions report 0 (unknown arity).
fn fn_arity(st: &St, mods: &ModStore, func: Value) -> i32 {
    if let ClosureRec::Proxy(id) = st.closures[func.index() as usize] {
        if let Some(rec) = st.proxies.get(id as usize) {
            if rec.target.is_function() {
                return fn_arity(st, mods, rec.target);
            }
        }
    }
    if let ClosureRec::User { module, proto, .. } =
        &st.closures[func.index() as usize]
    {
        mods.rc(*module).module.protos[*proto as usize].nparams as i32
    } else {
        0
    }
}

/// A function's `.name` (empty for native/bound functions).
/// The live call stack as V8-ish "    at name" lines, innermost first.
///
/// `top` optionally names a function to report ahead of `st.frames`.
/// Known gap: the frame currently executing lives in `exec`'s locals
/// rather than in `st.frames`, and a re-entrant `exec` (a construct,
/// `.call`, a native calling back into JS) keeps its caller's state
/// there too -- so the innermost name, and one name per re-entry, are
/// missing. Every name that *is* reported is a real ancestor, in
/// order, which is what makes a minified bundle's error locatable.
///
/// Bounded at 24 frames: gg has no GC, so an unbounded string per
/// constructed Error is real retained memory.
fn stack_string(
    st: &St, mods: &ModStore, top: Option<(u32, u32)>,
) -> String {
    let mut out = String::new();
    let mut n = 0usize;
    if let Some((m, p)) = top {
        let name = &mods.rc(m).module.protos[p as usize].name;
        out.push_str("    at ");
        out.push_str(if name.is_empty() { "<anonymous>" } else { name });
        out.push('\n');
        n += 1;
    }
    for f in st.frames.iter().rev() {
        if n >= 24 {
            out.push_str("    at ...\n");
            break;
        }
        let name = &mods.rc(f.module).module.protos[f.proto as usize].name;
        out.push_str("    at ");
        out.push_str(if name.is_empty() { "<anonymous>" } else { name });
        out.push('\n');
        n += 1;
    }
    out
}

fn fn_name(st: &St, mods: &ModStore, func: Value) -> String {
    if let ClosureRec::Proxy(id) = st.closures[func.index() as usize] {
        if let Some(rec) = st.proxies.get(id as usize) {
            if rec.target.is_function() {
                return fn_name(st, mods, rec.target);
            }
        }
    }
    if let ClosureRec::User { module, proto, .. } =
        &st.closures[func.index() as usize]
    {
        mods.rc(*module).module.protos[*proto as usize].name.clone()
    } else {
        String::new()
    }
}

/// camelCase -> kebab-case (fontSize -> font-size)
/// Resolve a possibly-relative URL against a base (page location).
fn resolve_url(raw: &str, base: &str) -> String {
    if raw.contains("://") || raw.is_empty() && base.is_empty() {
        return raw.to_string();
    }
    let (bscheme, brest) =
        base.split_once("://").unwrap_or(("https", "localhost/"));
    let (bhost, bpath) = match brest.find('/') {
        Some(i) => (&brest[..i], &brest[i..]),
        None => (brest, "/"),
    };
    if let Some(r) = raw.strip_prefix("//") {
        return format!("{bscheme}://{r}");
    }
    if raw.starts_with('/') {
        return format!("{bscheme}://{bhost}{raw}");
    }
    if raw.starts_with('#') || raw.starts_with('?') || raw.is_empty() {
        let bp = bpath.split(['#', '?']).next().unwrap_or("/");
        return format!("{bscheme}://{bhost}{bp}{raw}");
    }
    let dir = &bpath[..bpath.rfind('/').map(|i| i + 1).unwrap_or(1)];
    format!("{bscheme}://{bhost}{dir}{raw}")
}

/// One decomposed piece of an absolute URL (anchor-element getters).
fn url_part(url: &str, part: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let (hostport, pathq) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (pathq, hash) = match pathq.find('#') {
        Some(i) => (&pathq[..i], &pathq[i..]),
        None => (pathq, ""),
    };
    let (path, search) = match pathq.find('?') {
        Some(i) => (&pathq[..i], &pathq[i..]),
        None => (pathq, ""),
    };
    let (hostname, port) = match hostport.rsplit_once(':') {
        Some((h, p))
            if !p.is_empty()
                && p.chars().all(|c| c.is_ascii_digit()) =>
        {
            (h, p)
        }
        _ => (hostport, ""),
    };
    match part {
        "href" => url.to_string(),
        "protocol" => format!("{scheme}:"),
        "host" => hostport.to_string(),
        "hostname" => hostname.to_string(),
        "port" => port.to_string(),
        "pathname" => path.to_string(),
        "search" => search.to_string(),
        "hash" => hash.to_string(),
        "origin" => format!("{scheme}://{hostport}"),
        _ => String::new(),
    }
}

fn camel_to_kebab(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// One declaration's value out of an inline style string, or "".
fn style_attr_get(style: &str, prop: &str) -> String {
    for decl in style.split(';') {
        if let Some((k, v)) = decl.split_once(':') {
            if k.trim().eq_ignore_ascii_case(prop) {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// Upsert (or remove, when `value` is empty) one declaration in an
/// inline style string.
fn style_attr_set(style: &str, prop: &str, value: &str) -> String {
    let mut decls: Vec<(String, String)> = style
        .split(';')
        .filter_map(|d| {
            d.split_once(':').map(|(k, v)| {
                (k.trim().to_string(), v.trim().to_string())
            })
        })
        .filter(|(k, _)| !k.is_empty())
        .collect();
    decls.retain(|(k, _)| !k.eq_ignore_ascii_case(prop));
    if !value.is_empty() {
        decls.push((prop.to_string(), value.to_string()));
    }
    decls
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The array a `for-of` walks. Real arrays pass through; Map/Set
/// materialize from their backing store; anything with `@@iterator`
/// (generator objects, custom iterables) has the protocol drained
/// into a fresh array (budgeted — a non-terminating iterator errors
/// instead of hanging).
fn materialize_iterable(
    st: &mut St,
    mods: &ModStore,
    ov: Value,
) -> Result<Value, VmError> {
    if !ov.is_object() {
        return Ok(ov); // strings keep the existing indexed walk
    }
    let oi = ov.index();
    if st.objects[oi as usize].is_array {
        return Ok(ov);
    }
    if let Some(entries) = st.map_data.get(&oi) {
        let entries = entries.clone();
        let pairs: Vec<Value> = entries
            .iter()
            .map(|&(k, v)| new_array(st, vec![k, v]))
            .collect();
        return Ok(new_array(st, pairs));
    }
    if let Some(vals) = st.set_data.get(&oi) {
        let vals = vals.clone();
        return Ok(new_array(st, vals));
    }
    let itk = st.intern_name("@@iterator");
    let iter = match lookup_prop(st, oi as usize, itk) {
        PropHit::Data(f) if f.is_function() => {
            call_value_this(st, mods, f, Some(ov), &[])?
        }
        _ => {
            // the object may itself be an iterator (a generator object, or
            // a hand-written { next() } iterator): drive its own next().
            // @@iterator is often method-dispatched rather than a stored
            // property, so the data lookup above misses it.
            let nextk = st.intern_name("next");
            match lookup_prop(st, oi as usize, nextk) {
                PropHit::Data(f) if f.is_function() => ov,
                _ => return Ok(ov), // no protocol: existing behavior
            }
        }
    };
    if !iter.is_object() {
        return type_err("@@iterator did not return an object");
    }
    let nextk = st.intern_name("next");
    let donek = st.intern_name("done");
    let valuek = st.intern_name("value");
    let mut out: Vec<Value> = Vec::new();
    for _ in 0..100_000 {
        let next_f = raw_get_prop(st, iter.index() as usize, nextk)
            .unwrap_or(Value::UNDEFINED);
        if !next_f.is_function() {
            return type_err("iterator .next is not a function");
        }
        let r = call_value_this(st, mods, next_f, Some(iter), &[])?;
        if !r.is_object() {
            return type_err("iterator result is not an object");
        }
        let ri = r.index() as usize;
        let done = raw_get_prop(st, ri, donek)
            .unwrap_or(Value::UNDEFINED);
        if truthy(st, done) {
            return Ok(new_array(st, out));
        }
        out.push(
            raw_get_prop(st, ri, valuek).unwrap_or(Value::UNDEFINED),
        );
    }
    err("iterator did not terminate within the for-of budget")
}

/// Accessor-aware property lookup along the prototype chain.
pub(super) enum PropHit {
    Data(Value),
    Getter(Value),
    Missing,
}

/// Resolve `key` through the active `with (obj)` scopes (innermost first):
/// a with-object that owns the property shadows the real global. This is
/// what lets lodash `_.template`'s compiled `with (obj) { ... }` read its
/// interpolated variables from the data object. Data properties only —
/// getters/setters on a with-object fall through to the normal global.
fn with_lookup(st: &St, key: u32, base: usize) -> Option<Value> {
    for i in (base..st.with_stack.len()).rev() {
        let obj = st.with_stack[i];
        if obj.is_object() {
            if let PropHit::Data(v) =
                lookup_prop(st, obj.index() as usize, key)
            {
                return Some(v);
            }
        }
    }
    None
}

/// Innermost with-object (>= base) that owns `key`, for `with`-scoped
/// assignment. None if no active with-object has it.
fn with_target(st: &St, key: u32, base: usize) -> Option<Value> {
    for i in (base..st.with_stack.len()).rev() {
        let obj = st.with_stack[i];
        if obj.is_object()
            && !matches!(lookup_prop(st, obj.index() as usize, key),
                         PropHit::Missing)
        {
            return Some(obj);
        }
    }
    None
}

pub(super) fn lookup_prop(st: &St, oi: usize, key: u32) -> PropHit {
    let mut oi = oi;
    for _ in 0..16 {
        let o = &st.objects[oi];
        if o.has_accessors {
            if let Some(&(g, _s)) = st.accessors.get(&(oi as u32, key)) {
                return PropHit::Getter(g);
            }
        }
        if let Some(&slot) = st.shapes[o.shape as usize].props.get(&key) {
            return PropHit::Data(o.slots[slot as usize]);
        }
        if !o.proto.is_object() {
            return PropHit::Missing;
        }
        oi = o.proto.index() as usize;
    }
    PropHit::Missing
}

/// Own string-keyed properties of a non-array object in ECMAScript
/// enumeration order: array-index keys ascending, then other string keys
/// in insertion order. Includes accessor-only keys. When `skip_non_enum`
/// is set, keys marked `enumerable:false` are dropped (Object.keys/for-in/
/// JSON); getOwnPropertyNames passes false to see them all.
fn own_keys_ordered(st: &St, oi: usize, skip_non_enum: bool) -> Vec<u32> {
    let shape = st.objects[oi].shape;
    let mut data: Vec<(u16, u32)> = st.shapes[shape as usize]
        .props.iter().map(|(&a, &s)| (s, a)).collect();
    data.sort_by_key(|&(slot, _)| slot);
    let mut atoms: Vec<u32> = data.into_iter().map(|(_, a)| a).collect();
    if st.objects[oi].has_accessors {
        for (&(o, k), _) in st.accessors.iter() {
            if o == oi as u32 && !atoms.contains(&k) {
                atoms.push(k);
            }
        }
    }
    if skip_non_enum && !st.non_enum.is_empty() {
        atoms.retain(|&k| !st.non_enum.contains(&(oi as u32, k)));
    }
    // partition canonical array indices (ascending) ahead of string keys
    let mut ints: Vec<(u32, u32)> = Vec::new();
    let mut strs: Vec<u32> = Vec::new();
    for a in atoms {
        let nm = &st.names[a as usize];
        match nm.parse::<u32>() {
            Ok(iv) if iv != u32::MAX && iv.to_string() == *nm => {
                ints.push((iv, a))
            }
            _ => strs.push(a),
        }
    }
    ints.sort_by_key(|&(iv, _)| iv);
    let mut out: Vec<u32> = ints.into_iter().map(|(_, a)| a).collect();
    out.extend(strs);
    out
}

/// Expando lookup on the JS-visible Array.prototype object for array
/// instances — our arrays carry no [[Prototype]] link, but core-js
/// installs @@iterator/Symbol-keyed methods there and expects
/// instances to see them.
fn array_proto_hit(st: &St, key: u32) -> PropHit {
    if !st.known.array.is_function() {
        return PropHit::Missing;
    }
    match st.fn_protos.get(&st.known.array.index()) {
        Some(p) if p.is_object() => {
            lookup_prop(st, p.index() as usize, key)
        }
        _ => PropHit::Missing,
    }
}

/// Property read on a primitive receiver. Returns an extraction stub
/// only for names the builtin surface actually answers, consults
/// String/Number/Boolean.prototype expandos next (core-js installs
/// polyfills there), and yields undefined otherwise — a truthy stub
/// for arbitrary keys derails feature detection (jQuery reads its
/// expando off the string "ready" and skips wrapping the event).
fn primitive_prop_read(st: &mut St, recv: Value, key: u32) -> Value {
    let name = st.names[key as usize].clone();
    let ctor = if recv.is_string() {
        st.known.string
    } else if recv.is_number() {
        st.known.number
    } else {
        st.known.boolean
    };
    // identity type checks: "x".constructor === String
    if name == "constructor" && ctor.is_function() {
        return ctor;
    }
    let common = matches!(
        name.as_str(),
        "toString" | "valueOf" | "hasOwnProperty"
            | "propertyIsEnumerable" | "isPrototypeOf"
    );
    let per_type = if recv.is_string() {
        matches!(
            name.as_str(),
            "slice" | "substring" | "substr" | "charAt" | "charCodeAt"
                | "codePointAt" | "indexOf" | "lastIndexOf" | "includes"
                | "startsWith" | "endsWith" | "replace" | "replaceAll"
                | "split" | "concat" | "trim" | "trimStart" | "trimEnd"
                | "trimLeft" | "trimRight" | "toLowerCase" | "toUpperCase"
                | "toLocaleLowerCase" | "toLocaleUpperCase"
                | "match" | "matchAll" | "search" | "valueOf"
                | "repeat" | "padStart" | "padEnd" | "at"
                | "localeCompare" | "normalize"
        )
    } else if recv.is_number() {
        matches!(
            name.as_str(),
            "toFixed" | "toExponential" | "toPrecision"
                | "toLocaleString"
        )
    } else {
        false
    };
    if common || per_type {
        return memo_native(st, 1, key, 0, Native::MethodRef(key));
    }
    if ctor.is_function() {
        if let Some(p) = st.fn_protos.get(&ctor.index()).copied() {
            if p.is_object() {
                if let PropHit::Data(v) =
                    lookup_prop(st, p.index() as usize, key)
                {
                    return v;
                }
            }
        }
    }
    Value::UNDEFINED
}

/// A function's own enumerable property names, in a stable order.
///
/// Statics live in the `fn_props` side table, which `Object.keys` and
/// `for-in` used to skip entirely -- so a function carrying data
/// enumerated as empty. webpack's chunk gate is
/// `Object.keys(__webpack_require__.O).every(check)`, and an empty key
/// list makes `every` vacuously true: every entry module starts before
/// the chunks it needs are registered, and the page dies on
/// `__webpack_modules__[id].call` of undefined. `prototype` is left
/// out on purpose -- it is non-enumerable.
fn fn_own_enumerable_keys(st: &St, fidx: u32) -> Vec<u32> {
    let mut keys: Vec<u32> = st
        .fn_props
        .keys()
        .filter(|&&(f, _)| f == fidx)
        .map(|&(_, k)| k)
        .filter(|k| !st.non_enum.contains(&(fidx, *k)))
        .collect();
    keys.sort_unstable();
    keys
}

/// Object.prototype.toString brand of a value ("[object Array]" ...).
fn brand_string(st: &St, v: Value) -> String {
    let mut v = v;
    for _ in 0..16 {
        let Some(id) = proxy_id_of(st, v) else { break };
        let Some(rec) = st.proxies.get(id as usize) else { break };
        if rec.revoked {
            break;
        }
        v = rec.target;
    }
    let tag = if v.is_object() {
        let o = &st.objects[v.index() as usize];
        if o.is_array {
            "Array"
        } else if o.regex != REGEX_NONE {
            "RegExp"
        } else if o.promise != PROMISE_NONE {
            "Promise"
        } else if st.map_data.contains_key(&v.index()) {
            // core-js's classof keys its iterator registry off this
            // brand, so a Map that reports "Object" is not iterable
            "Map"
        } else if st.set_data.contains_key(&v.index()) {
            "Set"
        } else {
            "Object"
        }
    } else if v.is_function() {
        "Function"
    } else if v.is_string() {
        "String"
    } else if v.is_number() {
        "Number"
    } else if v.is_boolean() {
        "Boolean"
    } else if v.is_null() {
        "Null"
    } else if v.is_undefined() {
        "Undefined"
    } else if v.is_dom_node() {
        "HTMLElement"
    } else {
        "Object"
    };
    format!("[object {tag}]")
}

/// Debug disassembly: annotate an instruction with resolved names
/// (property atoms, globals) and constant strings so minified bytecode
/// reads. Diagnostic only (GG_JS_DUMP).
pub(super) fn annotate_instr(
    st: &St,
    gm: &[u32],
    consts: &[Value],
    instr: &Instr,
) -> String {
    let name = |atom: u16| -> String {
        let nid = gm.get(atom as usize).copied().unwrap_or(u32::MAX);
        st.names.get(nid as usize).cloned().unwrap_or_default()
    };
    let extra = match instr {
        Instr::GetProp { atom, .. }
        | Instr::SetProp { atom, .. }
        | Instr::CallMethod { atom, .. } => format!("  ; .{}", name(*atom)),
        Instr::GetGlobal { atom, .. }
        | Instr::GetGlobalSafe { atom, .. }
        | Instr::SetGlobal { atom, .. } => format!("  ; ${}", name(*atom)),
        Instr::LoadConst { idx, .. } => {
            let v = consts
                .get(*idx as usize)
                .copied()
                .unwrap_or(Value::UNDEFINED);
            if v.is_string() {
                // read-only string peek (str_ref needs &mut for
                // rope flattening; constants are already flat)
                match st.strs.get(v.index() as usize) {
                    Some(Str::Flat(s)) => format!("  ; \"{s}\""),
                    _ => "  ; <str>".to_string(),
                }
            } else {
                format!("  ; {v:?}")
            }
        }
        _ => String::new(),
    };
    format!("{instr:?}{extra}")
}

/// The setter for `key` visible from `oi` (own first, then the chain),
/// unless an own data property shadows it.
fn lookup_setter(st: &St, oi: usize, key: u32) -> Option<Value> {
    let mut oi = oi;
    for depth in 0..16 {
        let o = &st.objects[oi];
        if o.has_accessors {
            if let Some(&(_g, s)) = st.accessors.get(&(oi as u32, key)) {
                return if s.is_function() { Some(s) } else { None };
            }
        }
        // a data property at any level means plain assignment wins
        // (own level was already checked by the caller for depth 0)
        if depth > 0
            && st.shapes[o.shape as usize].props.contains_key(&key)
        {
            return None;
        }
        if !o.proto.is_object() {
            return None;
        }
        oi = o.proto.index() as usize;
    }
    None
}

/// A function's .prototype object, created on first touch with a
/// `constructor` back-reference (what `new` instances inherit from).
pub(super) fn fn_prototype(st: &mut St, func: Value) -> Value {
    let idx = func.index();
    if let Some(&p) = st.fn_protos.get(&idx) {
        return p;
    }
    let proto = new_plain_object(st);
    let ctor_key = st.intern_name("constructor");
    raw_set_prop(st, proto.index() as usize, ctor_key, func);
    st.fn_protos.insert(idx, proto);
    proto
}

/// JS ToInt32: modulo-2^32 wrap of the truncated number.
fn js_to_uint32(n: f64) -> u32 {
    if !n.is_finite() {
        return 0;
    }
    let m = n.trunc() % 4294967296.0;
    let m = if m < 0.0 { m + 4294967296.0 } else { m };
    m as u32
}

fn to_i32(st: &mut St, mods: &ModStore, v: Value) -> Result<i32, VmError> {
    Ok(js_to_uint32(to_number(st, mods, v)?) as i32)
}

fn to_u32(st: &mut St, mods: &ModStore, v: Value) -> Result<u32, VmError> {
    Ok(js_to_uint32(to_number(st, mods, v)?))
}

/// `delete obj[key]` — removes an own property by moving the object
/// to a fresh shape without it (ICs miss on the new shape). Numeric
/// keys clear the dense element slot. Always true (non-configurable
/// properties don't exist in this engine); false only for non-objects.
fn delete_property(st: &mut St, obj: Value, key: Value) -> bool {
    if !obj.is_object() {
        return false;
    }
    let oi = obj.index() as usize;
    let name = to_display(st, key);
    if let Some(i) = elem_index(&name) {
        if i < st.objects[oi].elems.len() {
            st.objects[oi].elems[i] = Value::UNDEFINED;
            return true;
        }
        // absent from elems: may still be a named prop below
    }
    let Some(&key_id) = st.name_ids.get(&name) else {
        return true;
    };
    st.accessors.remove(&(oi as u32, key_id));
    st.objects[oi].has_accessors = st.accessors.keys()
        .any(|&(owner, _)| owner == oi as u32);
    let shape = st.objects[oi].shape as usize;
    if !st.shapes[shape].props.contains_key(&key_id) {
        return true;
    }
    let mut remaining: Vec<(u32, u16)> = st.shapes[shape]
        .props
        .iter()
        .filter(|(k, _)| **k != key_id)
        .map(|(k, s)| (*k, *s))
        .collect();
    remaining.sort_by_key(|(_, s)| *s);
    let mut props = HashMap::with_capacity(remaining.len());
    let mut slots = Vec::with_capacity(remaining.len());
    for (new_slot, (k, old_slot)) in remaining.iter().enumerate() {
        props.insert(*k, new_slot as u16);
        slots.push(st.objects[oi].slots[*old_slot as usize]);
    }
    st.shapes.push(Shape { props, transitions: HashMap::new() });
    st.objects[oi].shape = (st.shapes.len() - 1) as u32;
    st.objects[oi].slots = slots;
    true
}

fn proxy_id_of(st: &St, value: Value) -> Option<u32> {
    if value.is_object() {
        return st.object_proxies.get(&value.index()).copied();
    }
    if value.is_function() {
        if let ClosureRec::Proxy(id) = st.closures[value.index() as usize] {
            return Some(id);
        }
    }
    None
}

fn proxy_rec(st: &St, id: u32) -> Result<ProxyRec, VmError> {
    let rec = st.proxies.get(id as usize).copied()
        .ok_or_else(|| VmError {
            msg: "invalid Proxy record".to_string(),
            value: None,
            kind: "TypeError",
            trace: None,
        })?;
    if rec.revoked {
        return type_err("Cannot perform operation on a revoked Proxy");
    }
    Ok(rec)
}

fn is_js_object(value: Value) -> bool {
    value.is_object() || value.is_function() || value.is_dom_node()
}

fn new_proxy(
    st: &mut St,
    target: Value,
    handler: Value,
) -> Result<Value, VmError> {
    if !is_js_object(target) {
        return type_err("Proxy target must be an object");
    }
    if !is_js_object(handler) {
        return type_err("Proxy handler must be an object");
    }
    let id = st.proxies.len() as u32;
    st.proxies.push(ProxyRec { target, handler, revoked: false });
    if target.is_function() {
        st.closures.push(ClosureRec::Proxy(id));
        Ok(Value::function((st.closures.len() - 1) as u32))
    } else {
        let out = new_plain_object(st);
        st.object_proxies.insert(out.index(), id);
        Ok(out)
    }
}

fn key_value(st: &mut St, key: u32) -> Value {
    let name = st.names[key as usize].clone();
    intern(st, &name)
}

fn property_key(
    st: &mut St,
    mods: &ModStore,
    key: Value,
) -> Result<(u32, Value), VmError> {
    let primitive = if key.is_object() {
        to_primitive(st, mods, key, false)?
    } else {
        key
    };
    let text = to_display(st, primitive);
    let id = st.intern_name(&text);
    Ok((id, intern(st, &text)))
}

/// Ordinary [[Get]] plus the Proxy dispatch shared by bytecode property
/// reads and Reflect.get. `receiver` is the value passed to accessors.
fn internal_get(
    st: &mut St,
    mods: &ModStore,
    target: Value,
    key: u32,
    receiver: Value,
) -> Result<Value, VmError> {
    if let Some(id) = proxy_id_of(st, target) {
        let rec = proxy_rec(st, id)?;
        let trap_key = st.intern_name("get");
        let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
        if trap.is_undefined() {
            return internal_get(st, mods, rec.target, key, receiver);
        }
        if !trap.is_function() {
            return type_err("Proxy get trap is not callable");
        }
        let prop = key_value(st, key);
        return call_value_this(
            st, mods, trap, Some(rec.handler),
            &[rec.target, prop, receiver],
        );
    }
    if target.is_object() {
        let oi = target.index() as usize;
        let name = st.names[key as usize].clone();
        if let Some(&node) = st.style_nodes.get(&target.index()) {
            let doc = need_doc(st)?;
            let cur = doc.borrow().nodes[node as usize]
                .attr("style").unwrap_or("").to_string();
            let out = if name == "cssText" {
                cur
            } else {
                style_attr_get(&cur, &camel_to_kebab(&name))
            };
            return Ok(push_str(st, out));
        }
        if let Some(&node) = st.dataset_nodes.get(&target.index()) {
            let attr = format!("data-{}", camel_to_kebab(&name));
            let doc = need_doc(st)?;
            let out = doc.borrow().nodes[node as usize]
                .attr(&attr).map(str::to_string);
            return Ok(match out {
                Some(s) => push_str(st, s),
                None => Value::UNDEFINED,
            });
        }
        if name == "length" && st.objects[oi].is_array {
            return Ok(Value::int(st.objects[oi].elems.len() as i32));
        }
        if let Some(index) = elem_index(&name) {
            if let Some(&value) = st.objects[oi].elems.get(index) {
                if !value.is_undefined() {
                    return Ok(value);
                }
            }
        }
        match lookup_prop(st, oi, key) {
            PropHit::Data(value) => return Ok(value),
            PropHit::Getter(getter) if getter.is_function() => {
                return call_value_this(
                    st, mods, getter, Some(receiver), &[],
                );
            }
            PropHit::Getter(_) => return Ok(Value::UNDEFINED),
            PropHit::Missing => {}
        }
        if st.objects[oi].is_array {
            if let PropHit::Data(value) = array_proto_hit(st, key) {
                return Ok(value);
            }
            if matches!(
                name.as_str(),
                "slice" | "concat" | "join" | "indexOf" | "push"
                    | "pop" | "map" | "filter" | "forEach" | "sort"
                    | "splice" | "shift" | "unshift" | "reverse"
                    | "some" | "every" | "reduce" | "lastIndexOf"
                    | "at" | "flat" | "flatMap" | "findLast"
                    | "findLastIndex" | "keys" | "values" | "entries"
            ) {
                return Ok(memo_native(st, 1, key, 0, Native::MethodRef(key)));
            }
        }
        if st.objects[oi].regex != REGEX_NONE
            && matches!(name.as_str(), "exec" | "test")
        {
            return Ok(memo_native(st, 1, key, 0, Native::MethodRef(key)));
        }
        if matches!(
            name.as_str(),
            "hasOwnProperty" | "valueOf" | "propertyIsEnumerable"
                | "isPrototypeOf"
        ) {
            return Ok(memo_native(st, 1, key, 0, Native::MethodRef(key)));
        }
        if name == "toString" {
            return Ok(if st.objects[oi].is_array {
                memo_native(st, 1, key, 0, Native::MethodRef(key))
            } else {
                make_native(st, Native::BrandToString)
            });
        }
        if target == st.known.window && st.gdef[key as usize] {
            return Ok(st.globals[key as usize]);
        }
        return Ok(Value::UNDEFINED);
    }
    if target.is_function() {
        let name = st.names[key as usize].clone();
        return Ok(if key == st.ids.prototype {
            fn_prototype(st, target)
        } else if let Some(value) = fn_static_lookup(st, target.index(), key) {
            value
        } else if key == st.ids.length {
            Value::int(fn_arity(st, mods, target))
        } else if name == "name" {
            let name = fn_name(st, mods, target);
            make_string(st, name)
        } else if matches!(
            name.as_str(), "call" | "apply" | "bind" | "toString" | "valueOf"
        ) {
            memo_native(st, 1, key, 0, Native::MethodRef(key))
        } else {
            Value::UNDEFINED
        });
    }
    if target.is_dom_node() {
        return dom_get_prop(st, key, target.index());
    }
    type_err("Reflect target must be an object")
}

fn internal_set(
    st: &mut St,
    mods: &ModStore,
    target: Value,
    key: u32,
    value: Value,
    receiver: Value,
) -> Result<bool, VmError> {
    if let Some(id) = proxy_id_of(st, target) {
        let rec = proxy_rec(st, id)?;
        let trap_key = st.intern_name("set");
        let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
        if trap.is_undefined() {
            return internal_set(st, mods, rec.target, key, value, receiver);
        }
        if !trap.is_function() {
            return type_err("Proxy set trap is not callable");
        }
        let prop = key_value(st, key);
        let result = call_value_this(
            st, mods, trap, Some(rec.handler),
            &[rec.target, prop, value, receiver],
        )?;
        return Ok(truthy(st, result));
    }
    if target.is_dom_node() {
        dom_set_prop(st, key, target.index(), value)?;
        return Ok(true);
    }
    if target.is_function() {
        if key == st.ids.prototype {
            st.fn_protos.insert(target.index(), value);
            return Ok(true);
        }
        let exists = st.fn_props.contains_key(&(target.index(), key));
        if !exists && st.non_extensible_functions.contains(&target.index()) {
            return Ok(false);
        }
        st.fn_props.insert((target.index(), key), value);
        return Ok(true);
    }
    if !target.is_object() {
        return type_err("Reflect target must be an object");
    }
    let oi = target.index() as usize;
    let name = st.names[key as usize].clone();
    if let Some(&node) = st.style_nodes.get(&target.index()) {
        let val = to_display(st, value);
        let doc = need_doc(st)?;
        if name == "cssText" {
            doc.borrow_mut().set_attr(node as usize, "style", &val);
        } else {
            let prop = camel_to_kebab(&name);
            let cur = doc.borrow().nodes[node as usize]
                .attr("style").unwrap_or("").to_string();
            let next = style_attr_set(&cur, &prop, &val);
            doc.borrow_mut().set_attr(node as usize, "style", &next);
        }
        return Ok(true);
    }
    if let Some(&node) = st.dataset_nodes.get(&target.index()) {
        let val = to_display(st, value);
        let attr = format!("data-{}", camel_to_kebab(&name));
        need_doc(st)?.borrow_mut().set_attr(node as usize, &attr, &val);
        return Ok(true);
    }
    if st.objects[oi].is_array && name == "length" {
        let len = to_number(st, mods, value)?;
        if len < 0.0 || len.fract() != 0.0 || len > u32::MAX as f64 {
            return range_err("Invalid array length");
        }
        if len as usize > MAX_ARRAY_ELEMS {
            return range_err("array length exceeds the engine limit");
        }
        st.objects[oi].elems.resize(len as usize, Value::UNDEFINED);
        return Ok(true);
    }
    if let Some(index) = elem_index(&name) {
        if index >= st.objects[oi].elems.len()
            && st.non_extensible_objects.contains(&target.index())
        {
            return Ok(false);
        }
        if index >= MAX_ARRAY_ELEMS {
            return range_err("array index exceeds the engine limit");
        }
        let elems = &mut st.objects[oi].elems;
        if index >= elems.len() {
            elems.resize(index, Value::UNDEFINED);
            elems.push(value);
        } else {
            elems[index] = value;
        }
        return Ok(true);
    }
    if let Some(setter) = lookup_setter(st, oi, key) {
        call_value_this(st, mods, setter, Some(receiver), &[value])?;
        return Ok(true);
    }
    let own = st.shapes[st.objects[oi].shape as usize]
        .props.contains_key(&key);
    if !own && st.non_extensible_objects.contains(&target.index()) {
        return Ok(false);
    }
    if target == st.known.window {
        st.globals[key as usize] = value;
        st.gdef[key as usize] = true;
    }
    raw_set_prop(st, oi, key, value);
    Ok(true)
}

fn internal_has(
    st: &mut St,
    mods: &ModStore,
    target: Value,
    key: u32,
) -> Result<bool, VmError> {
    if let Some(id) = proxy_id_of(st, target) {
        let rec = proxy_rec(st, id)?;
        let trap_key = st.intern_name("has");
        let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
        if trap.is_undefined() {
            return internal_has(st, mods, rec.target, key);
        }
        if !trap.is_function() {
            return type_err("Proxy has trap is not callable");
        }
        let prop = key_value(st, key);
        let result = call_value_this(
            st, mods, trap, Some(rec.handler), &[rec.target, prop],
        )?;
        let reported = truthy(st, result);
        if !reported {
            let actual = ordinary_get_own_descriptor(st, rec.target, key)?;
            if !actual.is_undefined()
                && (!internal_is_extensible(st, rec.target)
                    || !descriptor_flag(st, actual, "configurable"))
            {
                return type_err("Proxy has trap cannot hide a required target property");
            }
        }
        return Ok(reported);
    }
    if target.is_function() {
        let name = st.names[key as usize].as_str();
        return Ok(matches!(name, "prototype" | "call" | "apply" | "bind"
            | "length" | "name")
            || fn_static_lookup(st, target.index(), key).is_some());
    }
    if target.is_dom_node() {
        return Ok(!dom_get_prop(st, key, target.index())?.is_undefined());
    }
    if target.is_string() {
        // `Object('abc')` yields the primitive here (no wrapper
        // object), so `'0' in Object(s)` has to answer for the string
        let name = st.names[key as usize].clone();
        if matches!(name.as_str(), "length" | "constructor" | "toString"
                                   | "valueOf" | "hasOwnProperty") {
            return Ok(true);
        }
        let n = str_ref(st, target.index()).chars().count();
        return Ok(elem_index(&name).is_some_and(|i| i < n));
    }
    if !target.is_object() {
        return type_err("'in' right-hand side is not an object");
    }
    let oi = target.index() as usize;
    let name = st.names[key as usize].clone();
    if st.style_nodes.contains_key(&target.index()) {
        return Ok(name.chars().next()
            .is_some_and(|ch| ch.is_ascii_alphabetic()));
    }
    if st.objects[oi].is_array && name == "length" {
        return Ok(true);
    }
    if let Some(index) = elem_index(&name) {
        if st.objects[oi].elems.get(index)
            .is_some_and(|value| !value.is_undefined())
        {
            return Ok(true);
        }
        // non-arrays: a numeric name can still be a shape prop
        if st.objects[oi].is_array {
            return Ok(false);
        }
    }
    Ok(!matches!(lookup_prop(st, oi, key), PropHit::Missing))
}

fn internal_delete(
    st: &mut St,
    mods: &ModStore,
    target: Value,
    key: u32,
) -> Result<bool, VmError> {
    if let Some(id) = proxy_id_of(st, target) {
        let rec = proxy_rec(st, id)?;
        let trap_key = st.intern_name("deleteProperty");
        let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
        if trap.is_undefined() {
            return internal_delete(st, mods, rec.target, key);
        }
        if !trap.is_function() {
            return type_err("Proxy deleteProperty trap is not callable");
        }
        let prop = key_value(st, key);
        let result = call_value_this(
            st, mods, trap, Some(rec.handler), &[rec.target, prop],
        )?;
        let accepted = truthy(st, result);
        if accepted {
            let actual = ordinary_get_own_descriptor(st, rec.target, key)?;
            if !actual.is_undefined()
                && !descriptor_flag(st, actual, "configurable")
            {
                return type_err("Proxy cannot delete a non-configurable property");
            }
        }
        return Ok(accepted);
    }
    if target.is_function() {
        st.fn_props.remove(&(target.index(), key));
        return Ok(true);
    }
    if !target.is_object() {
        return Ok(false);
    }
    let prop = key_value(st, key);
    Ok(delete_property(st, target, prop))
}

fn ordinary_own_keys(st: &mut St, target: Value) -> Result<Vec<Value>, VmError> {
    if target.is_function() {
        let mut ids: Vec<u32> = st.fn_props.keys()
            .filter(|&&(owner, _)| owner == target.index())
            .map(|&(_, key)| key)
            .collect();
        if st.fn_protos.contains_key(&target.index()) {
            ids.push(st.ids.prototype);
        }
        ids.sort_unstable();
        ids.dedup();
        return Ok(ids.into_iter().map(|key| key_value(st, key)).collect());
    }
    if !target.is_object() {
        return type_err("Reflect target must be an object");
    }
    let oi = target.index() as usize;
    let mut out = Vec::new();
    for index in 0..st.objects[oi].elems.len() {
        if !st.objects[oi].elems[index].is_undefined() {
            out.push(intern(st, &index.to_string()));
        }
    }
    let shape = st.objects[oi].shape;
    let mut props: Vec<(u16, u32)> = st.shapes[shape as usize].props
        .iter().map(|(&key, &slot)| (slot, key)).collect();
    props.sort_by_key(|&(slot, _)| slot);
    let mut seen = HashSet::new();
    for (_, key) in props {
        seen.insert(key);
        out.push(key_value(st, key));
    }
    if st.objects[oi].has_accessors {
        let mut accessors: Vec<u32> = st.accessors.keys()
            .filter(|&&(owner, key)| owner == target.index() && !seen.contains(&key))
            .map(|&(_, key)| key).collect();
        accessors.sort_unstable();
        for key in accessors {
            out.push(key_value(st, key));
        }
    }
    if st.objects[oi].is_array {
        out.push(intern(st, "length"));
    }
    Ok(out)
}

fn internal_own_keys(
    st: &mut St,
    mods: &ModStore,
    target: Value,
) -> Result<Vec<Value>, VmError> {
    let Some(id) = proxy_id_of(st, target) else {
        return ordinary_own_keys(st, target);
    };
    let rec = proxy_rec(st, id)?;
    let trap_key = st.intern_name("ownKeys");
    let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
    if trap.is_undefined() {
        return internal_own_keys(st, mods, rec.target);
    }
    if !trap.is_function() {
        return type_err("Proxy ownKeys trap is not callable");
    }
    let result = call_value_this(
        st, mods, trap, Some(rec.handler), &[rec.target],
    )?;
    if !result.is_object() || !st.objects[result.index() as usize].is_array {
        return type_err("Proxy ownKeys trap must return an array");
    }
    let keys = st.objects[result.index() as usize].elems.clone();
    let mut names = HashSet::new();
    for &key in &keys {
        if !key.is_string() {
            return type_err("Proxy ownKeys result contains a non-string key");
        }
        let name = to_display(st, key);
        if !names.insert(name) {
            return type_err("Proxy ownKeys result contains duplicate keys");
        }
    }
    // This VM currently has one non-configurable ordinary key: array
    // length. It must be present even while the target is extensible.
    if rec.target.is_object()
        && st.objects[rec.target.index() as usize].is_array
        && !names.contains("length")
    {
        return type_err("Proxy ownKeys trap omitted non-configurable 'length'");
    }
    if !internal_is_extensible(st, rec.target) {
        let target_keys = ordinary_own_keys(st, rec.target)?;
        let target_names: HashSet<String> = target_keys.into_iter()
            .map(|key| to_display(st, key)).collect();
        if target_names != names {
            return type_err(
                "Proxy ownKeys trap omitted or added keys on a non-extensible target",
            );
        }
    }
    Ok(keys)
}

fn descriptor_object(
    st: &mut St,
    value: Value,
    writable: bool,
    enumerable: bool,
    configurable: bool,
) -> Value {
    let out = new_plain_object(st);
    let oi = out.index() as usize;
    for (name, val) in [
        ("value", value),
        ("writable", Value::boolean(writable)),
        ("enumerable", Value::boolean(enumerable)),
        ("configurable", Value::boolean(configurable)),
    ] {
        let key = st.intern_name(name);
        raw_set_prop(st, oi, key, val);
    }
    out
}

fn descriptor_flag(st: &St, descriptor: Value, name: &str) -> bool {
    if !descriptor.is_object() {
        return false;
    }
    let Some(&key) = st.name_ids.get(name) else {
        return false;
    };
    raw_get_prop(st, descriptor.index() as usize, key)
        .is_some_and(|value| truthy(st, value))
}

fn ordinary_get_own_descriptor(
    st: &mut St,
    target: Value,
    key: u32,
) -> Result<Value, VmError> {
    if target.is_function() {
        let value = if key == st.ids.prototype {
            st.fn_protos.get(&target.index()).copied()
        } else {
            st.fn_props.get(&(target.index(), key)).copied()
        };
        return Ok(value.map(|v| descriptor_object(st, v, true, true, true))
            .unwrap_or(Value::UNDEFINED));
    }
    if !target.is_object() {
        return type_err("Reflect target must be an object");
    }
    let oi = target.index() as usize;
    let name = st.names[key as usize].clone();
    if st.objects[oi].is_array && name == "length" {
        return Ok(descriptor_object(
            st, Value::int(st.objects[oi].elems.len() as i32),
            true, false, false,
        ));
    }
    if let Some(index) = elem_index(&name) {
        if let Some(&value) = st.objects[oi].elems.get(index) {
            if !value.is_undefined() {
                return Ok(descriptor_object(st, value, true, true, true));
            }
        }
        if st.objects[oi].is_array {
            return Ok(Value::UNDEFINED);
        }
        // plain object: a numeric name can also be a shape prop
        // (stored via a constant-key opcode) — fall through
    }
    if let Some(&(get, set)) = st.accessors.get(&(target.index(), key)) {
        let out = new_plain_object(st);
        let di = out.index() as usize;
        for (name, value) in [
            ("get", get), ("set", set),
            ("enumerable", Value::TRUE),
            ("configurable", Value::TRUE),
        ] {
            let id = st.intern_name(name);
            raw_set_prop(st, di, id, value);
        }
        return Ok(out);
    }
    let object = &st.objects[oi];
    let value = st.shapes[object.shape as usize].props.get(&key)
        .map(|&slot| object.slots[slot as usize]);
    Ok(value.map(|v| descriptor_object(st, v, true, true, true))
        .unwrap_or(Value::UNDEFINED))
}

fn internal_get_own_descriptor(
    st: &mut St,
    mods: &ModStore,
    target: Value,
    key: u32,
) -> Result<Value, VmError> {
    let Some(id) = proxy_id_of(st, target) else {
        return ordinary_get_own_descriptor(st, target, key);
    };
    let rec = proxy_rec(st, id)?;
    let trap_key = st.intern_name("getOwnPropertyDescriptor");
    let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
    if trap.is_undefined() {
        return internal_get_own_descriptor(st, mods, rec.target, key);
    }
    if !trap.is_function() {
        return type_err("Proxy getOwnPropertyDescriptor trap is not callable");
    }
    let prop = key_value(st, key);
    let result = call_value_this(
        st, mods, trap, Some(rec.handler), &[rec.target, prop],
    )?;
    if !result.is_undefined() && !result.is_object() {
        return type_err("Proxy descriptor trap must return an object or undefined");
    }
    if !internal_is_extensible(st, rec.target) {
        let actual = ordinary_get_own_descriptor(st, rec.target, key)?;
        if result.is_undefined() != actual.is_undefined() {
            return type_err("Proxy descriptor trap violated a non-extensible target");
        }
    }
    let actual = ordinary_get_own_descriptor(st, rec.target, key)?;
    if result.is_undefined() && !actual.is_undefined()
        && !descriptor_flag(st, actual, "configurable")
    {
        return type_err("Proxy descriptor trap hid a non-configurable property");
    }
    Ok(result)
}

fn ordinary_define_property(
    st: &mut St,
    target: Value,
    key: u32,
    desc: Value,
) -> Result<bool, VmError> {
    if !is_js_object(target) || target.is_dom_node() {
        return type_err("defineProperty needs an object");
    }
    let exists = !ordinary_get_own_descriptor(st, target, key)?.is_undefined();
    if !exists && !internal_is_extensible(st, target) {
        return Ok(false);
    }
    define_one_prop(st, target, key, desc)?;
    Ok(true)
}

fn internal_define_property(
    st: &mut St,
    mods: &ModStore,
    target: Value,
    key: u32,
    desc: Value,
) -> Result<bool, VmError> {
    let Some(id) = proxy_id_of(st, target) else {
        return ordinary_define_property(st, target, key, desc);
    };
    let rec = proxy_rec(st, id)?;
    let trap_key = st.intern_name("defineProperty");
    let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
    if trap.is_undefined() {
        return internal_define_property(st, mods, rec.target, key, desc);
    }
    if !trap.is_function() {
        return type_err("Proxy defineProperty trap is not callable");
    }
    let prop = key_value(st, key);
    let result = call_value_this(
        st, mods, trap, Some(rec.handler), &[rec.target, prop, desc],
    )?;
    let accepted = truthy(st, result);
    if accepted && !internal_is_extensible(st, rec.target)
        && ordinary_get_own_descriptor(st, rec.target, key)?.is_undefined()
    {
        return type_err("Proxy cannot define a new property on a non-extensible target");
    }
    Ok(accepted)
}

fn internal_get_proto(
    st: &mut St,
    mods: &ModStore,
    target: Value,
) -> Result<Value, VmError> {
    if let Some(id) = proxy_id_of(st, target) {
        let rec = proxy_rec(st, id)?;
        let trap_key = st.intern_name("getPrototypeOf");
        let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
        if trap.is_undefined() {
            return internal_get_proto(st, mods, rec.target);
        }
        if !trap.is_function() {
            return type_err("Proxy getPrototypeOf trap is not callable");
        }
        let result = call_value_this(
            st, mods, trap, Some(rec.handler), &[rec.target],
        )?;
        if !result.is_object() && !result.is_null() {
            return type_err("Proxy getPrototypeOf trap must return an object or null");
        }
        if !internal_is_extensible(st, rec.target) {
            let actual = internal_get_proto(st, mods, rec.target)?;
            if result != actual {
                return type_err("Proxy getPrototypeOf trap violated a non-extensible target");
            }
        }
        return Ok(result);
    }
    if target.is_object() {
        let proto = st.objects[target.index() as usize].proto;
        if proto.is_object() {
            return Ok(proto);
        }
        if st.known.object.is_function() {
            let object_proto = fn_prototype(st, st.known.object);
            return Ok(if target == object_proto { Value::NULL } else { object_proto });
        }
        return Ok(Value::NULL);
    }
    if target.is_function() {
        return Ok(st.fn_proto_chain.get(&target.index()).copied()
            .unwrap_or_else(|| fn_prototype(st, st.known.function)));
    }
    type_err("Reflect target must be an object")
}

fn internal_set_proto(
    st: &mut St,
    mods: &ModStore,
    target: Value,
    proto: Value,
) -> Result<bool, VmError> {
    if !proto.is_object() && !proto.is_null() {
        return type_err("prototype must be an object or null");
    }
    if let Some(id) = proxy_id_of(st, target) {
        let rec = proxy_rec(st, id)?;
        let trap_key = st.intern_name("setPrototypeOf");
        let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
        if trap.is_undefined() {
            return internal_set_proto(st, mods, rec.target, proto);
        }
        if !trap.is_function() {
            return type_err("Proxy setPrototypeOf trap is not callable");
        }
        let result = call_value_this(
            st, mods, trap, Some(rec.handler), &[rec.target, proto],
        )?;
        let accepted = truthy(st, result);
        if accepted && !internal_is_extensible(st, rec.target) {
            let actual = internal_get_proto(st, mods, rec.target)?;
            if actual != proto {
                return type_err("Proxy setPrototypeOf trap violated a non-extensible target");
            }
        }
        return Ok(accepted);
    }
    if !internal_is_extensible(st, target) {
        return Ok(internal_get_proto(st, mods, target)? == proto);
    }
    if target.is_object() {
        st.objects[target.index() as usize].proto =
            if proto.is_null() { Value::UNDEFINED } else { proto };
        return Ok(true);
    }
    if target.is_function() {
        st.fn_proto_chain.insert(target.index(), proto);
        return Ok(true);
    }
    type_err("Reflect target must be an object")
}

fn internal_is_extensible(st: &St, target: Value) -> bool {
    if let Some(id) = proxy_id_of(st, target) {
        return st.proxies.get(id as usize)
            .is_some_and(|rec| !rec.revoked && internal_is_extensible(st, rec.target));
    }
    if target.is_object() {
        !st.non_extensible_objects.contains(&target.index())
    } else if target.is_function() {
        !st.non_extensible_functions.contains(&target.index())
    } else {
        false
    }
}

fn internal_prevent_extensions(
    st: &mut St,
    mods: &ModStore,
    target: Value,
) -> Result<bool, VmError> {
    if let Some(id) = proxy_id_of(st, target) {
        let rec = proxy_rec(st, id)?;
        let trap_key = st.intern_name("preventExtensions");
        let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
        if trap.is_undefined() {
            return internal_prevent_extensions(st, mods, rec.target);
        }
        if !trap.is_function() {
            return type_err("Proxy preventExtensions trap is not callable");
        }
        let result = call_value_this(
            st, mods, trap, Some(rec.handler), &[rec.target],
        )?;
        let accepted = truthy(st, result);
        if accepted && internal_is_extensible(st, rec.target) {
            return type_err("Proxy preventExtensions trap returned true for an extensible target");
        }
        return Ok(accepted);
    }
    if target.is_object() {
        st.non_extensible_objects.insert(target.index());
        return Ok(true);
    }
    if target.is_function() {
        st.non_extensible_functions.insert(target.index());
        return Ok(true);
    }
    type_err("Reflect target must be an object")
}

/// `x instanceof Ctor` — built-ins matched by constructor identity.
/// User functions yield false: `new` is lowered to a plain object +
/// `Ctor.call`, so instances carry no link back to their constructor.
/// The IDL interfaces a node answers `instanceof` for, most specific
/// first. Not the whole platform -- the ones code actually tests.
fn dom_interface_chain(tag: &str) -> Vec<&'static str> {
    let specific = match tag {
        "script" => Some("HTMLScriptElement"),
        "div" => Some("HTMLDivElement"),
        "a" => Some("HTMLAnchorElement"),
        "img" => Some("HTMLImageElement"),
        "input" => Some("HTMLInputElement"),
        "form" => Some("HTMLFormElement"),
        "iframe" => Some("HTMLIFrameElement"),
        "link" => Some("HTMLLinkElement"),
        "style" => Some("HTMLStyleElement"),
        "canvas" => Some("HTMLCanvasElement"),
        "template" => Some("HTMLTemplateElement"),
        "textarea" => Some("HTMLTextAreaElement"),
        "select" => Some("HTMLSelectElement"),
        "option" => Some("HTMLOptionElement"),
        "button" => Some("HTMLButtonElement"),
        "video" => Some("HTMLVideoElement"),
        "audio" => Some("HTMLAudioElement"),
        _ => None,
    };
    let mut out = Vec::with_capacity(6);
    if tag == "#document" {
        out.extend_from_slice(&["Document", "Node", "EventTarget"]);
        return out;
    }
    if let Some(s) = specific {
        out.push(s);
    }
    out.extend_from_slice(&["HTMLElement", "Element", "Node", "EventTarget"]);
    out
}

fn instance_of(st: &St, x: Value, ctor: Value) -> Result<bool, VmError> {
    let mut ctor = ctor;
    for _ in 0..16 {
        let Some(id) = proxy_id_of(st, ctor) else { break };
        let rec = proxy_rec(st, id)?;
        ctor = rec.target;
    }
    let mut x = x;
    for _ in 0..16 {
        let Some(id) = proxy_id_of(st, x) else { break };
        let rec = proxy_rec(st, id)?;
        x = rec.target;
    }
    let k = &st.known;
    if ctor == k.array {
        return Ok(x.is_object()
            && st.objects[x.index() as usize].is_array);
    }
    if ctor == k.object {
        // functions are objects too (fn instanceof Object === true)
        return Ok(x.is_object() || x.is_function());
    }
    if ctor == k.function {
        return Ok(x.is_function());
    }
    if ctor == k.promise {
        return Ok(is_promise(st, x));
    }
    if ctor == k.string || ctor == k.number || ctor == k.boolean {
        return Ok(false); // primitives are never instances
    }
    if x.is_dom_node() && ctor.is_function() {
        // DOM nodes are not JS objects here, so they have no prototype
        // chain to walk -- but `el instanceof HTMLScriptElement` is a
        // real check real code makes (Next.js refuses to start without
        // it). Answer from the node's own interface chain.
        let tag = if x.index() == DOC_NODE {
            "#document".to_string()
        } else {
            st.doc
                .as_ref()
                .map(|d| {
                    d.borrow().nodes[x.index() as usize]
                        .tag
                        .clone()
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        };
        for iface in dom_interface_chain(&tag) {
            let Some(&id) = st.name_ids.get(iface) else { continue };
            if st.globals.get(id as usize) == Some(&ctor) {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if ctor.is_function() {
        // walk x's prototype chain looking for ctor.prototype
        let Some(&target) = st.fn_protos.get(&ctor.index()) else {
            return Ok(false); // prototype never touched: no instances
        };
        if !x.is_object() {
            return Ok(false);
        }
        let mut p = st.objects[x.index() as usize].proto;
        for _ in 0..16 {
            if !p.is_object() {
                return Ok(false);
            }
            if p == target {
                return Ok(true);
            }
            p = st.objects[p.index() as usize].proto;
        }
        return Ok(false);
    }
    // A non-callable RHS is a TypeError per spec, but real bundles do
    // `x instanceof MaybeMissingCtor` for feature detection (a DOM/SDK
    // constructor this engine doesn't expose resolves to undefined) —
    // throwing there aborts react's mount path. Tolerate with false;
    // the Babel _classCallCheck case is already handled by named-
    // function-expression self-binding making the ctor resolve.
    Ok(false)
}

fn to_display(st: &mut St, v: Value) -> String {
    to_display_rec(st, v, &mut Vec::new())
}

fn to_display_rec(st: &mut St, v: Value, seen: &mut Vec<u32>) -> String {
    if v.is_number() {
        js_num_str(v.to_number_raw())
    } else if v.is_string() {
        str_ref(st, v.index()).to_string()
    } else if v.is_boolean() {
        (if v.as_bool() { "true" } else { "false" }).to_string()
    } else if v.is_undefined() {
        "undefined".to_string()
    } else if v.is_null() {
        "null".to_string()
    } else if v.is_function() {
        // Must agree byte-for-byte with Function.prototype.toString:
        // core-js decides whether the host Promise is trustworthy by
        // comparing inspectSource(P) (the method) against String(P)
        // (this conversion) and testing /native code/. A mismatch --
        // or a string without "native code" -- forces its Promise
        // polyfill in, and the polyfill's scheduler never fires here,
        // so every async function on the page silently stops at its
        // first await. That was naver's shopping module.
        "function () { [native code] }".to_string()
    } else if v.is_dom_node() {
        "[object HTMLElement]".to_string()
    } else if v.is_object() {
        if st.objects[v.index() as usize].is_array {
            // Cyclic reference (or absurd nesting) renders as empty,
            // like Array.prototype.join's cycle handling — recursing
            // would blow the native stack.
            if seen.contains(&v.index()) || seen.len() >= 64 {
                return String::new();
            }
            seen.push(v.index());
            let elems = st.objects[v.index() as usize].elems.clone();
            let s = elems
                .iter()
                .map(|&e| to_display_rec(st, e, seen))
                .collect::<Vec<_>>()
                .join(",");
            seen.pop();
            s
        } else {
            "[object Object]".to_string()
        }
    } else {
        format!("{v:?}")
    }
}

/// ToString(v) as the spec means it: an object converts through
/// ToPrimitive first, so its own `toString` runs.
///
/// `to_display` cannot do this — it has no `mods` and so cannot call
/// back into JS — which is why `String(err)` and `"".concat(err)` used
/// to render every object as "[object Object]", swallowing the message
/// of every error a page logs. `+` was already correct because the Add
/// opcode primitivizes before concatenating.
pub(super) fn to_js_string(
    st: &mut St,
    mods: &ModStore,
    v: Value,
) -> Result<String, VmError> {
    let v = if v.is_object() {
        to_primitive(st, mods, v, false)?
    } else {
        v
    };
    Ok(to_display(st, v))
}

fn to_str_idx(st: &mut St, v: Value) -> u32 {
    if v.is_string() {
        return v.index();
    }
    let s = to_display(st, v);
    st.heap_bytes = st.heap_bytes.saturating_add(s.len() + 16);
    st.strs.push(Str::Flat(s));
    (st.strs.len() - 1) as u32
}

fn concat(st: &mut St, x: Value, y: Value) -> Result<Value, VmError> {
    let a = to_str_idx(st, x);
    let b = to_str_idx(st, y);
    let len = str_len(st, a) + str_len(st, b);
    if len > MAX_STR_BYTES {
        return range_err("Invalid string length");
    }
    // small results stay flat: rope nodes only pay off on big strings
    if len <= 64 {
        st.heap_bytes = st.heap_bytes.saturating_add(len + 16);
        let sa = str_ref(st, a).to_string();
        let sb = str_ref(st, b);
        let s = format!("{sa}{sb}");
        st.strs.push(Str::Flat(s));
    } else {
        // A Cat retains only its own node: the leaves were charged
        // when they were made, and the combined text is charged
        // if/when the rope flattens. Charging `len` per node here
        // billed a string built by appending as the *sum of every
        // prefix* -- O(n^2) phantom bytes -- so whether naver hit the
        // backstop depended on which ad creative's script happened to
        // build its payload that way. Retained size, not allocation
        // traffic, is what the backstop is documented to measure.
        st.heap_bytes = st
            .heap_bytes
            .saturating_add(std::mem::size_of::<Str>() + 8);
        st.strs.push(Str::Cat { a, b, len: len as u32 });
    }
    Ok(Value::string((st.strs.len() - 1) as u32))
}

fn proxy_call(
    st: &mut St,
    mods: &ModStore,
    id: u32,
    this_value: Value,
    args: &[Value],
) -> Result<Value, VmError> {
    let rec = proxy_rec(st, id)?;
    if !rec.target.is_function() {
        return type_err("Proxy target is not callable");
    }
    let trap_key = st.intern_name("apply");
    let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
    if trap.is_undefined() {
        return call_value_this(st, mods, rec.target, Some(this_value), args);
    }
    if !trap.is_function() {
        return type_err("Proxy apply trap is not callable");
    }
    let list = new_array(st, args.to_vec());
    call_value_this(
        st, mods, trap, Some(rec.handler),
        &[rec.target, this_value, list],
    )
}

fn construct_value(
    st: &mut St,
    mods: &ModStore,
    ctor: Value,
    args: &[Value],
    new_target: Value,
) -> Result<Value, VmError> {
    if !ctor.is_function() {
        return type_err("value is not a constructor");
    }
    let closure_index = ctor.index() as usize;
    if let ClosureRec::Bound { target, bound, .. } = &st.closures[closure_index] {
        let target = *target;
        let mut full = bound.clone();
        full.extend_from_slice(args);
        let effective_new_target = if new_target == ctor {
            target
        } else {
            new_target
        };
        return construct_value(
            st, mods, target, &full, effective_new_target,
        );
    }
    if let ClosureRec::Proxy(id) = st.closures[closure_index] {
        let rec = proxy_rec(st, id)?;
        if !rec.target.is_function() {
            return type_err("Proxy target is not a constructor");
        }
        let trap_key = st.intern_name("construct");
        let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
        if trap.is_undefined() {
            return construct_value(st, mods, rec.target, args, new_target);
        }
        if !trap.is_function() {
            return type_err("Proxy construct trap is not callable");
        }
        let list = new_array(st, args.to_vec());
        let result = call_value_this(
            st, mods, trap, Some(rec.handler),
            &[rec.target, list, new_target],
        )?;
        if !is_js_object(result) {
            return type_err("Proxy construct trap must return an object");
        }
        return Ok(result);
    }

    let instance = new_plain_object(st);
    let proto_ctor = if new_target.is_function() { new_target } else { ctor };
    let proto_key = st.ids.prototype;
    let proto = internal_get(st, mods, proto_ctor, proto_key, proto_ctor)?;
    if proto.is_object() {
        st.objects[instance.index() as usize].proto = proto;
    } else if st.known.object.is_function() {
        st.objects[instance.index() as usize].proto =
            fn_prototype(st, st.known.object);
    }
    let result = call_value_this(st, mods, ctor, Some(instance), args)?;
    Ok(if is_js_object(result) { result } else { instance })
}

fn reflect_op(
    st: &mut St,
    mods: &ModStore,
    op: u8,
    args_base: usize,
    argc: u8,
) -> Result<Value, VmError> {
    use reflect::*;
    let arg = |st: &St, index: usize| {
        if index < argc as usize {
            st.regs[args_base + index]
        } else {
            Value::UNDEFINED
        }
    };
    let target = arg(st, 0);
    match op {
        APPLY => {
            if !target.is_function() {
                return type_err("Reflect.apply target is not callable");
            }
            let this_value = arg(st, 1);
            let list = arg(st, 2);
            if !list.is_object() || !st.objects[list.index() as usize].is_array {
                return type_err("Reflect.apply argumentsList must be an array");
            }
            let args = st.objects[list.index() as usize].elems.clone();
            call_value_this(st, mods, target, Some(this_value), &args)
        }
        CONSTRUCT => {
            let list = arg(st, 1);
            if !list.is_object() || !st.objects[list.index() as usize].is_array {
                return type_err("Reflect.construct argumentsList must be an array");
            }
            let args = st.objects[list.index() as usize].elems.clone();
            let new_target = if argc > 2 { arg(st, 2) } else { target };
            if !new_target.is_function() {
                return type_err("Reflect.construct newTarget is not a constructor");
            }
            construct_value(st, mods, target, &args, new_target)
        }
        DEFINE_PROPERTY => {
            let key_arg = arg(st, 1);
            let (key, _) = property_key(st, mods, key_arg)?;
            let desc = arg(st, 2);
            Ok(Value::boolean(internal_define_property(
                st, mods, target, key, desc,
            )?))
        }
        DELETE_PROPERTY => {
            let key_arg = arg(st, 1);
            let (key, _) = property_key(st, mods, key_arg)?;
            Ok(Value::boolean(internal_delete(st, mods, target, key)?))
        }
        GET => {
            let key_arg = arg(st, 1);
            let receiver = if argc > 2 { arg(st, 2) } else { target };
            let (key, _) = property_key(st, mods, key_arg)?;
            internal_get(st, mods, target, key, receiver)
        }
        GET_OWN_PROPERTY_DESCRIPTOR => {
            let key_arg = arg(st, 1);
            let (key, _) = property_key(st, mods, key_arg)?;
            internal_get_own_descriptor(st, mods, target, key)
        }
        GET_PROTOTYPE_OF => internal_get_proto(st, mods, target),
        HAS => {
            let key_arg = arg(st, 1);
            let (key, _) = property_key(st, mods, key_arg)?;
            Ok(Value::boolean(internal_has(st, mods, target, key)?))
        }
        IS_EXTENSIBLE => {
            if let Some(id) = proxy_id_of(st, target) {
                let rec = proxy_rec(st, id)?;
                let trap_key = st.intern_name("isExtensible");
                let trap = internal_get(st, mods, rec.handler, trap_key, rec.handler)?;
                if !trap.is_undefined() {
                    if !trap.is_function() {
                        return type_err("Proxy isExtensible trap is not callable");
                    }
                    let result = call_value_this(
                        st, mods, trap, Some(rec.handler), &[rec.target],
                    )?;
                    let reported = truthy(st, result);
                    if reported != internal_is_extensible(st, rec.target) {
                        return type_err("Proxy isExtensible trap violated its target");
                    }
                    return Ok(Value::boolean(reported));
                }
            }
            if !is_js_object(target) {
                return type_err("Reflect target must be an object");
            }
            Ok(Value::boolean(internal_is_extensible(st, target)))
        }
        OWN_KEYS => {
            let keys = internal_own_keys(st, mods, target)?;
            Ok(new_array(st, keys))
        }
        PREVENT_EXTENSIONS => Ok(Value::boolean(
            internal_prevent_extensions(st, mods, target)?,
        )),
        SET => {
            let key_arg = arg(st, 1);
            let value = arg(st, 2);
            let receiver = if argc > 3 { arg(st, 3) } else { target };
            let (key, _) = property_key(st, mods, key_arg)?;
            Ok(Value::boolean(internal_set(
                st, mods, target, key, value, receiver,
            )?))
        }
        SET_PROTOTYPE_OF => Ok(Value::boolean(internal_set_proto(
            st, mods, target, arg(st, 1),
        )?)),
        _ => err("unknown Reflect operation"),
    }
}

fn do_native(
    st: &mut St,
    mods: &ModStore,
    n: Native,
    args_base: usize,
    argc: u8,
) -> Result<Value, VmError> {
    let _ = mods; // most natives don't call back into JS
    match n {
        Native::ConsoleLog => {
            let line = (0..argc as usize)
                .map(|k| to_display(st, st.regs[args_base + k]))
                .collect::<Vec<_>>()
                .join(" ");
            st.logs.push(line);
            Ok(Value::UNDEFINED)
        }
        Native::Noop => Ok(Value::UNDEFINED),
        Native::WinScroll { relative } => {
            // (x, y) or ({left, top, behavior})
            let a0 = if argc > 0 { st.regs[args_base] } else { Value::UNDEFINED };
            let a1 = if argc > 1 { st.regs[args_base + 1] } else { Value::UNDEFINED };
            let (left, top) = scroll_args(st, a0, a1);
            queue_scroll(st, DOC_NODE, left, top, relative);
            Ok(Value::UNDEFINED)
        }
        Native::ProxyCtor => {
            let target = if argc > 0 {
                st.regs[args_base]
            } else {
                Value::UNDEFINED
            };
            let handler = if argc > 1 {
                st.regs[args_base + 1]
            } else {
                Value::UNDEFINED
            };
            new_proxy(st, target, handler)
        }
        Native::ProxyRevocable => {
            let target = if argc > 0 {
                st.regs[args_base]
            } else {
                Value::UNDEFINED
            };
            let handler = if argc > 1 {
                st.regs[args_base + 1]
            } else {
                Value::UNDEFINED
            };
            let proxy = new_proxy(st, target, handler)?;
            let id = proxy_id_of(st, proxy).unwrap();
            let revoke = make_native(st, Native::ProxyRevoke { proxy: id });
            let out = new_plain_object(st);
            let oi = out.index() as usize;
            let proxy_key = st.intern_name("proxy");
            let revoke_key = st.intern_name("revoke");
            raw_set_prop(st, oi, proxy_key, proxy);
            raw_set_prop(st, oi, revoke_key, revoke);
            Ok(out)
        }
        Native::ProxyRevoke { proxy } => {
            if let Some(rec) = st.proxies.get_mut(proxy as usize) {
                rec.revoked = true;
                rec.target = Value::UNDEFINED;
                rec.handler = Value::UNDEFINED;
            }
            Ok(Value::UNDEFINED)
        }
        Native::Reflect(op) => reflect_op(st, mods, op, args_base, argc),
        Native::Storage { session, op } => {
            let arg0 = |st: &mut St| -> String {
                if argc > 0 {
                    to_display(st, st.regs[args_base])
                } else {
                    String::new()
                }
            };
            match op {
                0 => {
                    // getItem: present -> string, absent -> null
                    let k = arg0(st);
                    let store = if session {
                        &st.session_storage
                    } else {
                        &st.local_storage
                    };
                    match store.get(&k) {
                        Some(v) => {
                            let v = v.clone();
                            Ok(push_str(st, v))
                        }
                        None => Ok(Value::NULL),
                    }
                }
                1 => {
                    // setItem(key, value)
                    let k = arg0(st);
                    let v = if argc > 1 {
                        to_display(st, st.regs[args_base + 1])
                    } else {
                        String::new()
                    };
                    if session {
                        st.session_storage.insert(k, v);
                    } else {
                        st.local_storage.insert(k, v);
                    }
                    Ok(Value::UNDEFINED)
                }
                2 => {
                    let k = arg0(st);
                    if session {
                        st.session_storage.remove(&k);
                    } else {
                        st.local_storage.remove(&k);
                    }
                    Ok(Value::UNDEFINED)
                }
                3 => {
                    if session {
                        st.session_storage.clear();
                    } else {
                        st.local_storage.clear();
                    }
                    Ok(Value::UNDEFINED)
                }
                _ => {
                    // key(n): nth key in insertion order, or null
                    let n = if argc > 0 {
                        st.regs[args_base].to_number_raw() as usize
                    } else {
                        0
                    };
                    let store = if session {
                        &st.session_storage
                    } else {
                        &st.local_storage
                    };
                    match store.keys().nth(n) {
                        Some(k) => {
                            let k = k.clone();
                            Ok(push_str(st, k))
                        }
                        None => Ok(Value::NULL),
                    }
                }
            }
        }
        Native::MapCtor | Native::SetCtor => {
            let is_map = matches!(n, Native::MapCtor);
            let obj = new_plain_object(st);
            let oi = obj.index();
            if is_map {
                st.map_data.insert(oi, Vec::new());
                for (m, op) in [("get", 0u8), ("set", 1), ("has", 2),
                                ("delete", 3), ("clear", 4),
                                ("forEach", 5), ("keys", 6),
                                ("values", 7), ("entries", 8)] {
                    let k = st.intern_name(m);
                    let f = make_native(
                        st, Native::MapOp { obj: oi, op });
                    raw_set_prop(st, oi as usize, k, f);
                }
            } else {
                st.set_data.insert(oi, Vec::new());
                for (m, op) in [("add", 0u8), ("has", 1),
                                ("delete", 2), ("clear", 3),
                                ("forEach", 4), ("values", 5),
                                ("keys", 5), ("entries", 5)] {
                    let k = st.intern_name(m);
                    let f = make_native(
                        st, Native::SetOp { obj: oi, op });
                    raw_set_prop(st, oi as usize, k, f);
                }
            }
            let szk = st.intern_name("size");
            raw_set_prop(st, oi as usize, szk, Value::int(0));
            // A Map is an *object* to the page, not just something our
            // for-of understands natively: without @@iterator on the
            // instance, core-js's getIterator (`getMethod(o, @@iterator)
            // || getMethod(o, '@@iterator') || Iterators[classof(o)]`)
            // finds nothing and throws "[object Object] is not
            // iterable" -- which is what broke every ad slot on naver.
            let iter_op = if is_map { 8u8 } else { 5u8 };
            let itk = st.intern_name("@@iterator");
            let f = if is_map {
                make_native(st, Native::MapOp { obj: oi, op: iter_op })
            } else {
                make_native(st, Native::SetOp { obj: oi, op: iter_op })
            };
            raw_set_prop(st, oi as usize, itk, f);
            let tagk = st.intern_name("@@toStringTag");
            let tag = make_string(st, if is_map { "Map" } else { "Set" }
                .to_string());
            raw_set_prop(st, oi as usize, tagk, tag);
            let ctork = st.intern_name("constructor");
            let ctor = make_native(st, n);
            raw_set_prop(st, oi as usize, ctork, ctor);
            // ...and the methods live on the prototype in a real engine,
            // so none of this may show up in Object.keys/for-in/JSON.
            for k in own_keys_ordered(st, oi as usize, false) {
                st.non_enum.insert((oi, k));
            }
            // optional iterable seed: anything iterable, not only an
            // array literal -- `new Map(otherMap)` silently produced an
            // empty map before.
            if argc > 0 {
                let seed = materialize_iterable(st, mods, st.regs[args_base])?;
                if seed.is_object()
                    && st.objects[seed.index() as usize].is_array
                {
                    let items =
                        st.objects[seed.index() as usize].elems.clone();
                    if is_map {
                        let mut entries = Vec::new();
                        for it in items {
                            if it.is_object()
                                && st.objects[it.index() as usize]
                                    .is_array
                                && st.objects[it.index() as usize]
                                    .elems
                                    .len()
                                    >= 2
                            {
                                let e = &st.objects
                                    [it.index() as usize]
                                    .elems;
                                entries.push((e[0], e[1]));
                            }
                        }
                        let sz = entries.len() as i32;
                        st.map_data.insert(oi, entries);
                        raw_set_prop(
                            st, oi as usize, szk, Value::int(sz));
                    } else {
                        let mut vals: Vec<Value> = Vec::new();
                        for it in items {
                            if !vals
                                .iter()
                                .any(|&x| strict_eq(st, x, it))
                            {
                                vals.push(it);
                            }
                        }
                        let sz = vals.len() as i32;
                        st.set_data.insert(oi, vals);
                        raw_set_prop(
                            st, oi as usize, szk, Value::int(sz));
                    }
                }
            }
            Ok(obj)
        }
        Native::MapOp { obj, op } => {
            let a0 = if argc > 0 {
                st.regs[args_base]
            } else {
                Value::UNDEFINED
            };
            let a1 = if argc > 1 {
                st.regs[args_base + 1]
            } else {
                Value::UNDEFINED
            };
            let entries = st.map_data.get(&obj).cloned()
                .unwrap_or_default();
            let szk = st.intern_name("size");
            match op {
                0 => Ok(entries
                    .iter()
                    .find(|(k, _)| strict_eq(st, *k, a0))
                    .map(|&(_, v)| v)
                    .unwrap_or(Value::UNDEFINED)),
                1 => {
                    let mut e = entries;
                    if let Some(slot) =
                        e.iter_mut().find(|(k, _)| strict_eq(st, *k, a0))
                    {
                        slot.1 = a1;
                    } else {
                        e.push((a0, a1));
                    }
                    let sz = e.len() as i32;
                    st.map_data.insert(obj, e);
                    raw_set_prop(st, obj as usize, szk, Value::int(sz));
                    Ok(Value::object(obj))
                }
                2 => Ok(Value::boolean(
                    entries.iter().any(|(k, _)| strict_eq(st, *k, a0)),
                )),
                3 => {
                    let mut e = entries;
                    let before = e.len();
                    e.retain(|(k, _)| !strict_eq(st, *k, a0));
                    let removed = e.len() != before;
                    let sz = e.len() as i32;
                    st.map_data.insert(obj, e);
                    raw_set_prop(st, obj as usize, szk, Value::int(sz));
                    Ok(Value::boolean(removed))
                }
                4 => {
                    st.map_data.insert(obj, Vec::new());
                    raw_set_prop(st, obj as usize, szk, Value::int(0));
                    Ok(Value::UNDEFINED)
                }
                5 => {
                    // forEach(cb): cb(value, key, map)
                    for (k, v) in entries {
                        call_value(
                            st, mods, a0,
                            &[v, k, Value::object(obj)],
                        )?;
                    }
                    Ok(Value::UNDEFINED)
                }
                6 => {
                    let ks: Vec<Value> =
                        entries.iter().map(|&(k, _)| k).collect();
                    let a = new_array(st, ks);
                    Ok(make_array_iter(st, a))
                }
                7 => {
                    let vs: Vec<Value> =
                        entries.iter().map(|&(_, v)| v).collect();
                    let a = new_array(st, vs);
                    Ok(make_array_iter(st, a))
                }
                _ => {
                    let pairs: Vec<Value> = entries
                        .iter()
                        .map(|&(k, v)| new_array(st, vec![k, v]))
                        .collect();
                    let a = new_array(st, pairs);
                    Ok(make_array_iter(st, a))
                }
            }
        }
        Native::SetOp { obj, op } => {
            let a0 = if argc > 0 {
                st.regs[args_base]
            } else {
                Value::UNDEFINED
            };
            let vals = st.set_data.get(&obj).cloned().unwrap_or_default();
            let szk = st.intern_name("size");
            match op {
                0 => {
                    let mut v = vals;
                    if !v.iter().any(|&x| strict_eq(st, x, a0)) {
                        v.push(a0);
                    }
                    let sz = v.len() as i32;
                    st.set_data.insert(obj, v);
                    raw_set_prop(st, obj as usize, szk, Value::int(sz));
                    Ok(Value::object(obj))
                }
                1 => Ok(Value::boolean(
                    vals.iter().any(|&x| strict_eq(st, x, a0)),
                )),
                2 => {
                    let mut v = vals;
                    let before = v.len();
                    v.retain(|&x| !strict_eq(st, x, a0));
                    let removed = v.len() != before;
                    let sz = v.len() as i32;
                    st.set_data.insert(obj, v);
                    raw_set_prop(st, obj as usize, szk, Value::int(sz));
                    Ok(Value::boolean(removed))
                }
                3 => {
                    st.set_data.insert(obj, Vec::new());
                    raw_set_prop(st, obj as usize, szk, Value::int(0));
                    Ok(Value::UNDEFINED)
                }
                4 => {
                    for x in vals {
                        call_value(
                            st, mods, a0,
                            &[x, x, Value::object(obj)],
                        )?;
                    }
                    Ok(Value::UNDEFINED)
                }
                _ => {
                    let a = new_array(st, vals);
                    Ok(make_array_iter(st, a))
                }
            }
        }
        Native::RegExpCtor => {
            let pat = if argc > 0 {
                to_display(st, st.regs[args_base])
            } else {
                String::new()
            };
            let flags = if argc > 1 {
                to_display(st, st.regs[args_base + 1])
            } else {
                String::new()
            };
            new_regex(st, &pat, &flags)
        }
        Native::MethodRef(key) => {
            // called directly (no receiver): dispatch against undefined
            let args: Vec<Value> = (0..argc as usize)
                .map(|k| st.regs[args_base + k])
                .collect();
            method_ref_dispatch(st, mods, Value::UNDEFINED, key, &args)
        }
        Native::BrandToString => {
            let s = brand_string(st, Value::UNDEFINED);
            Ok(push_str(st, s))
        }
        Native::FunctionCtor => {
            // new Function(p1, ..., pN, body): compile a real closure. Falls
            // back to the historical window-returning stub if the source
            // won't parse (so `Function('return this')()` still works).
            let (params, body) = if argc == 0 {
                (String::new(), String::new())
            } else {
                let n = argc as usize;
                let body_v = st.regs[args_base + n - 1];
                let param_vs: Vec<Value> =
                    (0..n - 1).map(|k| st.regs[args_base + k]).collect();
                let body = to_display(st, body_v);
                let params = param_vs
                    .iter()
                    .map(|&v| to_display(st, v))
                    .collect::<Vec<_>>()
                    .join(",");
                (params, body)
            };
            match build_function(st, mods, &params, &body) {
                Ok(f) if f.is_function() => Ok(f),
                _ => Ok(make_native(st, Native::ReturnGlobal)),
            }
        }
        Native::ObjectCtor => {
            let v = if argc > 0 {
                st.regs[args_base]
            } else {
                Value::UNDEFINED
            };
            Ok(if v.is_nullish() {
                new_plain_object(st)
            } else {
                v // objects pass through; primitives too (no boxing)
            })
        }
        Native::ArrayCtor => {
            if argc == 1 && st.regs[args_base].is_number() {
                let len = st.regs[args_base].to_number_raw();
                let len = (len.max(0.0) as usize).min(100_000);
                Ok(new_array(st, vec![Value::UNDEFINED; len]))
            } else {
                let args: Vec<Value> = (0..argc as usize)
                    .map(|k| st.regs[args_base + k])
                    .collect();
                Ok(new_array(st, args))
            }
        }
        Native::ReturnGlobal => Ok(st.known.window),
        Native::ReturnDoc => Ok(Value::dom_node(DOC_NODE)),
        Native::DomMethod { node, key } => {
            dom_method(st, mods, key, node, args_base, argc)
        }
        Native::Alert => {
            let msg = if argc > 0 {
                to_display(st, st.regs[args_base])
            } else {
                String::new()
            };
            st.logs.push(format!("[alert] {msg}"));
            Ok(Value::UNDEFINED)
        }
        Native::DateNow => {
            // virtual clock: deterministic and never sleeps (see pump)
            Ok(Value::number(st.now_ms))
        }
        Native::String => {
            if argc == 0 {
                return Ok(intern(st, ""));
            }
            let s = to_js_string(st, mods, st.regs[args_base])?;
            Ok(push_str(st, s))
        }
        Native::Number => {
            if argc == 0 {
                return Ok(Value::int(0));
            }
            let v = st.regs[args_base];
            if v.is_number() {
                Ok(v)
            } else if v.is_string() {
                let s = str_ref(st, v.index()).trim().to_string();
                Ok(if s.is_empty() {
                    Value::int(0)
                } else {
                    Value::number(s.parse::<f64>().unwrap_or(f64::NAN))
                })
            } else {
                Ok(Value::number(num_of(v).unwrap_or(f64::NAN)))
            }
        }
        Native::Boolean => {
            if argc == 0 {
                return Ok(Value::FALSE);
            }
            Ok(Value::boolean(truthy(st, st.regs[args_base])))
        }
        Native::DocWrite { node, op } => {
            match op {
                0 => {
                    st.doc_write_buf.insert(node, String::new());
                }
                1 | 2 => {
                    let mut piece = (0..argc as usize)
                        .map(|k| to_display(st, st.regs[args_base + k]))
                        .collect::<Vec<_>>()
                        .join("");
                    if op == 2 {
                        piece.push('\n');
                    }
                    st.doc_write_buf.entry(node).or_default().push_str(&piece);
                }
                _ => {
                    let html =
                        st.doc_write_buf.remove(&node).unwrap_or_default();
                    st.doc_writes.push((node, html));
                }
            }
            Ok(Value::UNDEFINED)
        }
        Native::StackTrace => {
            let out = stack_string(st, mods, None);
            Ok(make_string(st, out))
        }
        Native::ParseInt => {
            if argc == 0 {
                return Ok(Value::number(f64::NAN));
            }
            let s = to_display(st, st.regs[args_base]);
            let radix_arg = if argc >= 2 && !st.regs[args_base + 1].is_undefined() {
                num_of(st.regs[args_base + 1])? as i64
            } else {
                0
            };
            let t = s.trim_matches(|c: char| {
                c.is_whitespace() || c == '\u{feff}'
            });
            let (neg, rest) = match t.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, t.strip_prefix('+').unwrap_or(t)),
            };
            // spec: with no/0 radix a `0x`/`0X` prefix forces base 16;
            // an explicit radix of 16 also tolerates the prefix.
            let has_hex_prefix =
                rest.starts_with("0x") || rest.starts_with("0X");
            let (radix, digits) = if radix_arg == 0 {
                if has_hex_prefix { (16u32, &rest[2..]) } else { (10u32, rest) }
            } else if radix_arg == 16 && has_hex_prefix {
                (16u32, &rest[2..])
            } else {
                (radix_arg as u32, rest)
            };
            if !(2..=36).contains(&radix) {
                // spec: invalid radix -> NaN (and is_digit(r>36) panics)
                return Ok(Value::number(f64::NAN));
            }
            let end = digits
                .find(|c: char| !c.is_digit(radix))
                .unwrap_or(digits.len());
            Ok(match i64::from_str_radix(&digits[..end], radix) {
                Ok(v) => Value::number(if neg { -v as f64 } else { v as f64 }),
                Err(_) => Value::number(f64::NAN),
            })
        }
        Native::ParseFloat => {
            if argc == 0 {
                return Ok(Value::number(f64::NAN));
            }
            let s = to_display(st, st.regs[args_base]);
            // JS whitespace includes the BOM (Rust's trim doesn't)
            let t = s.trim_matches(|c: char| {
                c.is_whitespace() || c == '\u{feff}'
            });
            // longest numeric prefix that parses (char-boundary safe)
            let mut end = 0;
            for (i, ch) in t.char_indices() {
                let e = i + ch.len_utf8();
                if t[..e].parse::<f64>().is_ok() {
                    end = e;
                }
            }
            Ok(if end == 0 {
                Value::number(f64::NAN)
            } else {
                Value::number(t[..end].parse::<f64>().unwrap_or(f64::NAN))
            })
        }
        Native::JsonStringify => {
            let v = if argc > 0 { st.regs[args_base] } else { Value::UNDEFINED };
            let replacer_v = if argc > 1 { st.regs[args_base + 1] } else { Value::UNDEFINED };
            let space_v = if argc > 2 { st.regs[args_base + 2] } else { Value::UNDEFINED };
            // space: a number gives that many (<=10) spaces, a string is
            // used verbatim (first 10 chars); anything else = compact.
            let gap = if space_v.is_number() {
                " ".repeat((space_v.to_number_raw() as i64).clamp(0, 10) as usize)
            } else if space_v.is_string() {
                str_ref(st, space_v.index()).chars().take(10).collect()
            } else {
                String::new()
            };
            // replacer: a function transforms each pair; an array is an
            // allow-list of property names to keep.
            let (replacer, allow) = if replacer_v.is_function() {
                (replacer_v, None)
            } else if replacer_v.is_object()
                && st.objects[replacer_v.index() as usize].is_array
            {
                let elems = st.objects[replacer_v.index() as usize].elems.clone();
                let mut a = Vec::new();
                for e in elems {
                    let ks = if e.is_string() {
                        str_ref(st, e.index()).to_string()
                    } else if e.is_number() {
                        js_num_str(e.to_number_raw())
                    } else {
                        continue;
                    };
                    a.push(st.intern_name(&ks));
                }
                (Value::UNDEFINED, Some(a))
            } else {
                (Value::UNDEFINED, None)
            };
            let ctx = JsonCtx { gap, replacer, allow };
            // top-level holder is the wrapper { "": value } the replacer
            // is invoked against (spec's SerializeJSONProperty).
            let holder = new_plain_object(st);
            let empty = st.intern_name("");
            raw_set_prop(st, holder.index() as usize, empty, v);
            Ok(match json_serialize(
                st, mods, holder, "", v, &mut Vec::new(), &ctx, "",
            )? {
                Some(s) => push_str(st, s),
                None => Value::UNDEFINED,
            })
        }
        Native::PreventDefault => {
            st.default_prevented = true;
            Ok(Value::UNDEFINED)
        }
        Native::SetTimeout | Native::SetInterval => {
            if argc == 0 || !st.regs[args_base].is_function() {
                return Ok(Value::int(0));
            }
            let cb = st.regs[args_base];
            let delay = if argc > 1 {
                to_number(st, mods, st.regs[args_base + 1])?.max(0.0)
            } else {
                0.0
            };
            let delay = if delay.is_finite() { delay } else { 0.0 };
            let extra: Vec<Value> =
                (2..argc as usize).map(|k| st.regs[args_base + k]).collect();
            st.next_timer_id += 1;
            st.timer_seq += 1;
            let id = st.next_timer_id;
            let interval = matches!(n, Native::SetInterval).then_some(delay);
            st.timers.push(Timer {
                id,
                callback: cb,
                args: extra,
                due_ms: st.now_ms + delay,
                seq: st.timer_seq,
                interval,
                is_raf: false,
            });
            Ok(Value::int(id as i32))
        }
        Native::WinEvent { add } => {
            let ty = if argc > 0 {
                to_display(st, st.regs[args_base]).to_lowercase()
            } else {
                return Ok(Value::UNDEFINED);
            };
            let handler = if argc > 1 {
                st.regs[args_base + 1]
            } else {
                Value::UNDEFINED
            };
            if add {
                if is_event_listener(handler) {
                    st.listeners
                        .entry((WINDOW_NODE, ty))
                        .or_default()
                        .push(handler);
                }
            } else if let Some(v) =
                st.listeners.get_mut(&(WINDOW_NODE, ty))
            {
                v.retain(|&h| h != handler);
            }
            Ok(Value::UNDEFINED)
        }
        Native::WinDispatch => {
            let ev = if argc > 0 {
                st.regs[args_base]
            } else {
                Value::UNDEFINED
            };
            let ty = if ev.is_object() {
                let tk = st.intern_name("type");
                match raw_get_prop(st, ev.index() as usize, tk) {
                    Some(t) => to_display(st, t).to_lowercase(),
                    None => String::new(),
                }
            } else {
                String::new()
            };
            let cbs = st
                .listeners
                .get(&(WINDOW_NODE, ty))
                .cloned()
                .unwrap_or_default();
            let win = st.known.window;
            for cb in cbs {
                // a throwing error-listener must not abort the caller
                let _ = call_listener(st, mods, cb, Some(win), &[ev]);
            }
            Ok(Value::boolean(true))
        }
        Native::PostMessage { ctx } => {
            // Serialize through the same path JSON.stringify uses (the
            // spec's structured clone is richer, but JSON covers the
            // plain data every handshake actually sends, and it is the
            // only representation that can cross a document boundary
            // here). A function or undefined clones as null; a cycle
            // throws, like DataCloneError.
            let v = if argc > 0 { st.regs[args_base] } else { Value::UNDEFINED };
            let target_origin = if argc > 1 {
                to_display(st, st.regs[args_base + 1])
            } else {
                "/".to_string()
            };
            let ctx_json = JsonCtx {
                gap: String::new(),
                replacer: Value::UNDEFINED,
                allow: None,
            };
            let holder = new_plain_object(st);
            let empty = st.intern_name("");
            raw_set_prop(st, holder.index() as usize, empty, v);
            let json = json_serialize(
                st, mods, holder, "", v, &mut Vec::new(), &ctx_json, "",
            )?
            .unwrap_or_else(|| "null".to_string());
            if json.len() > MAX_STR_BYTES {
                return range_err("postMessage payload too large");
            }
            st.frame_seq += 1;
            let seq = st.frame_seq;
            st.message_writes.push((ctx, json, target_origin, seq));
            Ok(Value::UNDEFINED)
        }
        Native::WinDeliver => {
            // one queued message, delivered as its own macrotask
            let ev = if argc > 0 { st.regs[args_base] } else { Value::UNDEFINED };
            let win = st.known.window;
            let mut cbs = st
                .listeners
                .get(&(WINDOW_NODE, "message".to_string()))
                .cloned()
                .unwrap_or_default();
            // `window.onmessage = fn` is a plain property, so
            // dispatchEvent never sees it — deliver it here too.
            let onk = st.intern_name("onmessage");
            if win.is_object() {
                if let Some(h) = raw_get_prop(st, win.index() as usize, onk) {
                    if h.is_function() {
                        cbs.push(h);
                    }
                }
            }
            for cb in cbs {
                // one handler must not be able to starve the next
                st.fuel = DEFAULT_FUEL;
                if let Err(e) =
                    call_listener(st, mods, cb, Some(win), &[ev])
                {
                    st.logs.push(format!("message handler: {}", e.msg));
                }
            }
            Ok(Value::UNDEFINED)
        }
        Native::FrameDom { frame, node, op } => {
            let a0 = if argc > 0 {
                to_display(st, st.regs[args_base])
            } else {
                String::new()
            };
            let a1 = if argc > 1 {
                to_display(st, st.regs[args_base + 1])
            } else {
                String::new()
            };
            // A mirror the host dropped (the frame navigated away, or
            // went cross-origin) makes every read empty and every
            // write a no-op — the wrapper is detached, not an error.
            let Some(m) = st.frame_mirrors.get(&frame) else {
                return Ok(Value::NULL);
            };
            let doc_op = op >= framedom::DOC_QUERY;
            let idx = if doc_op {
                m.doc.root
            } else {
                match m.idx_of.get(&node) {
                    Some(&i) if i < m.doc.nodes.len() => i,
                    _ => return Ok(Value::NULL),
                }
            };
            // reads first: they only borrow the mirror
            let text = match op {
                framedom::GET_TEXT => Some(m.doc.collect_text(idx)),
                framedom::GET_HTML => Some(serialize_children(&m.doc, idx)),
                framedom::GET_ID => Some(
                    m.doc.nodes[idx].attr("id").unwrap_or("").to_string()),
                framedom::GET_CLASS => Some(
                    m.doc.nodes[idx].attr("class").unwrap_or("").to_string()),
                framedom::GET_TAG => Some(
                    m.doc.nodes[idx].tag.clone().unwrap_or_default()
                        .to_uppercase()),
                framedom::GET_VALUE => Some(
                    m.doc.nodes[idx].attr("value").unwrap_or("").to_string()),
                framedom::DOC_TITLE => Some(
                    find_tag(&m.doc, "title")
                        .map(|t| m.doc.collect_text(t))
                        .unwrap_or_default()),
                framedom::DOC_URL => Some(m.url.clone()),
                framedom::GET_ATTR => match m.doc.nodes[idx].attr(&a0) {
                    Some(v) => Some(v.to_string()),
                    None => return Ok(Value::NULL),
                },
                framedom::HAS_ATTR => {
                    return Ok(Value::boolean(
                        m.doc.nodes[idx].attr(&a0).is_some()));
                }
                framedom::MATCHES => {
                    let hits = query(&m.doc, &a0, false);
                    return Ok(Value::boolean(hits.contains(&idx)));
                }
                _ => None,
            };
            if let Some(t) = text {
                return Ok(push_str(st, t));
            }
            // node-returning reads
            let picked: Option<Vec<usize>> = match op {
                framedom::QUERY | framedom::DOC_QUERY => {
                    Some(query_within(&m.doc, idx, &a0, true))
                }
                framedom::QUERY_ALL | framedom::DOC_QUERY_ALL => {
                    Some(query_within(&m.doc, idx, &a0, false))
                }
                framedom::DOC_BY_ID => Some(
                    m.doc.get_element_by_id(&a0).into_iter().take(1).collect()),
                framedom::DOC_BY_TAG => Some(
                    query_within(&m.doc, m.doc.root, &a0, false)),
                framedom::DOC_BODY => Some(
                    find_tag(&m.doc, "body").into_iter().collect()),
                framedom::DOC_ROOT => Some(
                    find_tag(&m.doc, "html").into_iter().collect()),
                framedom::GET_CHILDREN => Some(
                    m.doc.nodes[idx].children.iter().copied()
                        .filter(|&c| m.doc.nodes[c].is_element()).collect()),
                framedom::GET_PARENT => Some(
                    m.doc.nodes[idx].parent
                        .filter(|&p| m.doc.nodes[p].is_element())
                        .into_iter().collect()),
                _ => None,
            };
            if let Some(hits) = picked {
                let many = matches!(op, framedom::QUERY_ALL
                                    | framedom::DOC_QUERY_ALL
                                    | framedom::DOC_BY_TAG
                                    | framedom::GET_CHILDREN);
                let ridxs: Vec<u32> = hits.iter()
                    .filter_map(|&i| m.ridx_of.get(i).copied())
                    .collect();
                if many {
                    let elems: Vec<Value> = ridxs.iter()
                        .map(|&r| frame_element_value(st, frame, r))
                        .collect();
                    return Ok(new_array(st, elems));
                }
                return Ok(match ridxs.first() {
                    Some(&r) => frame_element_value(st, frame, r),
                    None => Value::NULL,
                });
            }
            // writes: the mirror moves now, the real child on the next
            // host pump (the queue_scroll pattern), so a read-after-
            // write inside one turn is coherent
            if !framedom::is_write(op) {
                return Ok(Value::UNDEFINED);
            }
            let m = st.frame_mirrors.get_mut(&frame).unwrap();
            let (a, b) = match op {
                framedom::SET_TEXT => {
                    m.doc.set_text_content(idx, &a0);
                    (a0.clone(), String::new())
                }
                framedom::SET_ID => {
                    m.doc.set_attr(idx, "id", &a0);
                    ("id".to_string(), a0.clone())
                }
                framedom::SET_CLASS => {
                    m.doc.set_attr(idx, "class", &a0);
                    ("class".to_string(), a0.clone())
                }
                framedom::SET_VALUE => {
                    m.doc.set_attr(idx, "value", &a0);
                    ("value".to_string(), a0.clone())
                }
                framedom::SET_ATTR => {
                    m.doc.set_attr(idx, &a0, &a1);
                    (a0.clone(), a1.clone())
                }
                framedom::REMOVE_ATTR => {
                    m.doc.remove_attr(idx, &a0);
                    (a0.clone(), String::new())
                }
                _ => (String::new(), String::new()),
            };
            queue_frame_write(st, frame, node, op, a, b);
            Ok(Value::UNDEFINED)
        }
        Native::UriCoder { encode, component } => {
            let s = if argc > 0 {
                to_display(st, st.regs[args_base])
            } else {
                return Ok(push_str(st, "undefined".to_string()));
            };
            let out = if encode {
                let keep = |c: u8| -> bool {
                    c.is_ascii_alphanumeric()
                        || matches!(c, b'-' | b'_' | b'.' | b'!' | b'~'
                            | b'*' | b'\'' | b'(' | b')')
                        || (!component
                            && matches!(c, b'#' | b'$' | b'&' | b'+'
                                | b',' | b'/' | b':' | b';' | b'='
                                | b'?' | b'@'))
                };
                let mut o = String::new();
                for &b in s.as_bytes() {
                    if keep(b) {
                        o.push(b as char);
                    } else {
                        o.push_str(&format!("%{b:02X}"));
                    }
                }
                o
            } else {
                let bytes = s.as_bytes();
                let mut o: Vec<u8> = Vec::with_capacity(bytes.len());
                let mut i = 0;
                while i < bytes.len() {
                    let decoded = if bytes[i] == b'%' {
                        bytes
                            .get(i + 1..i + 3)
                            .and_then(|h| std::str::from_utf8(h).ok())
                            .and_then(|h| u8::from_str_radix(h, 16).ok())
                    } else {
                        None
                    };
                    match decoded {
                        Some(b) => {
                            o.push(b);
                            i += 3;
                        }
                        None => {
                            o.push(bytes[i]);
                            i += 1;
                        }
                    }
                }
                String::from_utf8_lossy(&o).into_owned()
            };
            Ok(push_str(st, out))
        }
        Native::Raf => {
            if argc == 0 || !st.regs[args_base].is_function() {
                return Ok(Value::int(0));
            }
            let cb = st.regs[args_base];
            st.next_timer_id += 1;
            st.timer_seq += 1;
            let id = st.next_timer_id;
            let due = st.now_ms + 16.0;
            st.timers.push(Timer {
                id,
                callback: cb,
                args: vec![Value::number(due)],
                due_ms: due,
                seq: st.timer_seq,
                interval: None,
                is_raf: true,
            });
            Ok(Value::int(id as i32))
        }
        Native::PerfNow => Ok(Value::number(if st.perf_virtual {
            st.now_ms
        } else {
            st.perf_origin.elapsed().as_secs_f64() * 1000.0
        })),
        Native::ClassList { node, op } => {
            let doc = need_doc(st)?;
            let current = doc
                .borrow()
                .nodes[node as usize]
                .attr("class")
                .unwrap_or("")
                .to_string();
            let mut classes: Vec<String> = current
                .split_whitespace()
                .map(str::to_string)
                .collect();
            let args: Vec<String> = (0..argc as usize)
                .map(|k| to_display(st, st.regs[args_base + k]))
                .collect();
            let mut result = Value::UNDEFINED;
            match op {
                0 => {
                    for a in &args {
                        if !classes.iter().any(|c| c == a) {
                            classes.push(a.clone());
                        }
                    }
                }
                1 => classes.retain(|c| !args.iter().any(|a| a == c)),
                2 => {
                    let a = args.first().cloned().unwrap_or_default();
                    return Ok(Value::boolean(
                        classes.iter().any(|c| *c == a),
                    ));
                }
                _ => {
                    let a = args.first().cloned().unwrap_or_default();
                    if let Some(i) =
                        classes.iter().position(|c| *c == a)
                    {
                        classes.remove(i);
                        result = Value::boolean(false);
                    } else {
                        classes.push(a);
                        result = Value::boolean(true);
                    }
                }
            }
            let joined = classes.join(" ");
            doc.borrow_mut().set_attr(node as usize, "class", &joined);
            Ok(result)
        }
        Native::ClearTimeout => {
            if argc > 0 {
                let id = to_number(st, mods, st.regs[args_base])? as u32;
                st.timers.retain(|t| t.id != id);
            }
            Ok(Value::UNDEFINED)
        }
        Native::QueueMicrotask => {
            if argc > 0 && st.regs[args_base].is_function() {
                let cb = st.regs[args_base];
                st.microtasks.push_back(Job::Call { callback: cb, args: Vec::new() });
            }
            Ok(Value::UNDEFINED)
        }
        Native::Fetch => {
            let url = if argc > 0 {
                to_display(st, st.regs[args_base])
            } else {
                String::new()
            };
            let options = if argc > 1 {
                st.regs[args_base + 1]
            } else {
                Value::UNDEFINED
            };
            let option_value = |st: &mut St, name: &str| {
                if !options.is_object() {
                    return Value::UNDEFINED;
                }
                let key = st.intern_name(name);
                raw_get_prop(st, options.index() as usize, key)
                    .unwrap_or(Value::UNDEFINED)
            };
            let method_v = option_value(st, "method");
            let method = if method_v.is_undefined() {
                "GET".to_string()
            } else {
                to_display(st, method_v).to_uppercase()
            };
            let body_v = option_value(st, "body");
            let body = if body_v.is_undefined() || body_v.is_null() {
                String::new()
            } else {
                to_display(st, body_v)
            };
            let mode_v = option_value(st, "mode");
            let mode = if mode_v.is_undefined() {
                "cors".to_string()
            } else {
                to_display(st, mode_v).to_lowercase()
            };
            let credentials_v = option_value(st, "credentials");
            let credentials = if credentials_v.is_undefined() {
                "same-origin".to_string()
            } else {
                to_display(st, credentials_v).to_lowercase()
            };
            let headers_v = option_value(st, "headers");
            let mut headers = Vec::new();
            if headers_v.is_object() {
                let oi = headers_v.index() as usize;
                let shape = st.objects[oi].shape as usize;
                let mut props: Vec<(u32, u16)> = st.shapes[shape]
                    .props
                    .iter()
                    .map(|(&key, &slot)| (key, slot))
                    .collect();
                props.sort_by_key(|(_, slot)| *slot);
                for (key, slot) in props {
                    let name = st.names[key as usize].clone();
                    let value = st.objects[oi].slots[slot as usize];
                    headers.push((name, to_display(st, value)));
                }
            }
            let (pval, pid) = new_promise(st);
            st.next_fetch_id += 1;
            let fid = st.next_fetch_id;
            st.pending_fetches.push(PendingFetch {
                fetch_id: fid,
                promise: pid,
                url,
                method,
                body,
                headers,
                mode,
                credentials,
            });
            Ok(pval)
        }
        Native::PromiseCtor => {
            // `new Promise(executor)` reached dynamically (the
            // syntactic form compiles to Instr::NewPromise). Same
            // semantics: run the executor now, a throw rejects.
            let exec_fn = if argc > 0 {
                st.regs[args_base]
            } else {
                Value::UNDEFINED
            };
            if !exec_fn.is_function() {
                return type_err("Promise resolver is not a function");
            }
            let (pval, pid) = new_promise(st);
            let resolve =
                make_native(st, Native::Resolve { pid, reject: false });
            let reject =
                make_native(st, Native::Resolve { pid, reject: true });
            if let Err(e) =
                call_value(st, mods, exec_fn, &[resolve, reject])
            {
                let reason = e.value.unwrap_or_else(|| {
                    let s = e.msg.clone();
                    make_string(st, s)
                });
                promise_settle(st, pid, reason, true);
            }
            Ok(pval)
        }
        Native::PromiseResolve => {
            let v = if argc > 0 { st.regs[args_base] } else { Value::UNDEFINED };
            let (pval, pid) = new_promise(st);
            promise_settle(st, pid, v, false);
            Ok(pval)
        }
        Native::PromiseReject => {
            let v = if argc > 0 { st.regs[args_base] } else { Value::UNDEFINED };
            let (pval, pid) = new_promise(st);
            promise_settle(st, pid, v, true);
            Ok(pval)
        }
        Native::HostFn(id) => host_fn(st, mods, id, args_base, argc),
        Native::Resolve { pid, reject } => {
            let v = if argc > 0 { st.regs[args_base] } else { Value::UNDEFINED };
            promise_settle(st, pid, v, reject);
            Ok(Value::UNDEFINED)
        }
        Native::JsonParse => {
            let text = if argc > 0 {
                let v = st.regs[args_base];
                to_display(st, v)
            } else {
                "undefined".to_string()
            };
            let mut p = JsonP { b: text.chars().collect(), i: 0, depth: 0 };
            let v = json_parse(st, &mut p)?;
            p.ws();
            if p.i != p.b.len() {
                return err(p.bad());
            }
            let reviver =
                if argc > 1 { st.regs[args_base + 1] } else { Value::UNDEFINED };
            if !reviver.is_function() {
                return Ok(v);
            }
            // InternalizeJSONProperty: walk bottom-up through a root
            // holder whose single key is "". React's flight format is
            // built on this -- rows arrive as plain arrays and the
            // reviver turns the `"$"`-tagged ones into elements, so
            // ignoring it hands React raw objects and it refuses to
            // render them (its error #31).
            let root = new_plain_object(st);
            let empty = st.intern_name("");
            raw_set_prop(st, root.index() as usize, empty, v);
            json_revive(st, mods, root, "", v, reviver)
        }
    }
}

fn json_revive(
    st: &mut St,
    mods: &ModStore,
    holder: Value,
    key: &str,
    val: Value,
    reviver: Value,
) -> Result<Value, VmError> {
    if val.is_object() {
        let oi = val.index() as usize;
        let is_array = st.objects[oi].is_array;
        let nelems = st.objects[oi].elems.len();
        for i in 0..nelems {
            let child = st.objects[oi].elems[i];
            if !is_array && child.is_undefined() {
                continue;
            }
            let name = i.to_string();
            let out = json_revive(st, mods, val, &name, child, reviver)?;
            if i < st.objects[oi].elems.len() {
                st.objects[oi].elems[i] = out;
            }
        }
        if !is_array {
            for atom in own_keys_ordered(st, oi, true) {
                let name = st.names[atom as usize].clone();
                let child =
                    raw_get_prop(st, oi, atom).unwrap_or(Value::UNDEFINED);
                let out = json_revive(st, mods, val, &name, child, reviver)?;
                if out.is_undefined() {
                    let k = push_str(st, name);
                    delete_property(st, val, k);
                } else {
                    raw_set_prop(st, oi, atom, out);
                }
            }
        }
    }
    let k = push_str(st, key.to_string());
    call_value_this(st, mods, reviver, Some(holder), &[k, val])
}

/// Math/Object/Array/Number/String static methods (Native::HostFn).
/// One property-descriptor application, shared by Object.defineProperty
/// and Object.defineProperties. Function targets store value
/// descriptors in the static-props table ("prototype" swaps the
/// prototype); accessors on functions are accepted and ignored.
fn define_one_prop(
    st: &mut St,
    obj: Value,
    key: u32,
    desc: Value,
) -> Result<(), VmError> {
    if obj.is_function() {
        if desc.is_object() {
            let val_id = st.intern_name("value");
            if let Some(v) =
                raw_get_prop(st, desc.index() as usize, val_id)
            {
                if key == st.ids.prototype {
                    st.fn_protos.insert(obj.index(), v);
                } else {
                    st.fn_props.insert((obj.index(), key), v);
                }
            }
        }
        return Ok(());
    }
    let oi = obj.index() as usize;
    if !desc.is_object() {
        return err("defineProperty needs a descriptor object");
    }
    let di = desc.index() as usize;
    let get_id = st.intern_name("get");
    let set_id = st.intern_name("set");
    let val_id = st.intern_name("value");
    let writable_id = st.intern_name("writable");
    let enum_id = st.intern_name("enumerable");
    let get_p = raw_get_prop(st, di, get_id);
    let set_p = raw_get_prop(st, di, set_id);
    let val_p = raw_get_prop(st, di, val_id);
    // was this property already present (data slot or accessor)?
    let existed = st.shapes[st.objects[oi].shape as usize]
        .props.contains_key(&key)
        || st.accessors.contains_key(&(oi as u32, key));
    let is_accessor_desc = get_p.is_some() || set_p.is_some();
    if is_accessor_desc {
        // Redefining an accessor keeps the side the descriptor omits
        // (spec: absent get/set inherit the existing attribute).
        let (og, os) = st
            .accessors
            .get(&(oi as u32, key))
            .copied()
            .unwrap_or((Value::UNDEFINED, Value::UNDEFINED));
        let ng = if get_p.is_some() { get_p.unwrap() } else { og };
        let ns = if set_p.is_some() { set_p.unwrap() } else { os };
        st.accessors.insert((oi as u32, key), (ng, ns));
        st.objects[oi].has_accessors = true;
    } else if let Some(v) = val_p {
        // a data descriptor replaces any prior accessor of the same name
        st.accessors.remove(&(oi as u32, key));
        // defineProperty(window, ...) must reach bare-name reads too
        // (core-js defineGlobalProperty installs polyfills this way)
        if obj == st.known.window {
            st.globals[key as usize] = v;
            st.gdef[key as usize] = true;
        }
        // numeric keys go to element storage (where gets/keys look),
        // matching ordinary assignment — a shape prop named "907"
        // would be invisible to o["907"] reads
        if let Some(index) = elem_index(&st.names[key as usize].clone())
        {
            if index >= MAX_ARRAY_ELEMS {
                return range_err("array index exceeds the engine limit");
            }
            let elems = &mut st.objects[oi].elems;
            if index >= elems.len() {
                elems.resize(index, Value::UNDEFINED);
                elems.push(v);
            } else {
                elems[index] = v;
            }
        } else {
            raw_set_prop(st, oi, key, v);
        }
        // writable defaults to false for a newly defined data property;
        // for an existing one an omitted attribute is left unchanged.
        match raw_get_prop(st, di, writable_id).map(|w| truthy(st, w)) {
            Some(true) => {
                st.non_writable.remove(&(oi as u32, key));
            }
            Some(false) => {
                st.non_writable.insert((oi as u32, key));
            }
            None if !existed => {
                st.non_writable.insert((oi as u32, key));
            }
            None => {}
        }
    }
    // enumerable defaults to false for a newly defined property; omitted on
    // an existing one leaves the current setting alone.
    match raw_get_prop(st, di, enum_id).map(|e| truthy(st, e)) {
        Some(true) => {
            st.non_enum.remove(&(oi as u32, key));
        }
        Some(false) => {
            st.non_enum.insert((oi as u32, key));
        }
        None if !existed => {
            st.non_enum.insert((oi as u32, key));
        }
        None => {}
    }
    Ok(())
}

fn host_fn(
    st: &mut St,
    mods: &ModStore,
    id: u16,
    args_base: usize,
    argc: u8,
) -> Result<Value, VmError> {
    use host::*;
    let n = argc as usize;
    // read arg k as a Copy Value (no borrow held — avoids the
    // `f(&mut st, st.regs[..])` double-borrow)
    macro_rules! argv {
        ($k:expr) => {
            if ($k) < n { st.regs[args_base + $k] } else { Value::UNDEFINED }
        };
    }
    // unary math: ToNumber then apply f
    macro_rules! m1 {
        ($f:expr) => {{
            let a = argv!(0);
            let x = to_number(st, mods, a)?;
            Ok(Value::number($f(x)))
        }};
    }
    match id {
        M_ABS => m1!(f64::abs),
        M_FLOOR => m1!(f64::floor),
        M_CEIL => m1!(f64::ceil),
        M_ROUND => m1!(|x: f64| (x + 0.5).floor()), // JS rounds .5 up
        M_TRUNC => m1!(f64::trunc),
        // JS Math.sign: 0/-0/NaN pass through unchanged (f64::signum maps
        // 0 -> 1 and -0 -> -1, which is wrong)
        M_SIGN => m1!(|x: f64| if x.is_nan() || x == 0.0 {
            x
        } else if x > 0.0 {
            1.0
        } else {
            -1.0
        }),
        M_SQRT => m1!(f64::sqrt),
        M_CBRT => m1!(f64::cbrt),
        M_EXP => m1!(f64::exp),
        M_LOG => m1!(f64::ln),
        M_LOG2 => m1!(f64::log2),
        M_LOG10 => m1!(f64::log10),
        M_SIN => m1!(f64::sin),
        M_COS => m1!(f64::cos),
        M_TAN => m1!(f64::tan),
        M_ATAN => m1!(f64::atan),
        M_POW => {
            let (a0, a1) = (argv!(0), argv!(1));
            let a = to_number(st, mods, a0)?;
            let b = to_number(st, mods, a1)?;
            Ok(Value::number(a.powf(b)))
        }
        M_ATAN2 => {
            let (a0, a1) = (argv!(0), argv!(1));
            let a = to_number(st, mods, a0)?;
            let b = to_number(st, mods, a1)?;
            Ok(Value::number(a.atan2(b)))
        }
        M_HYPOT => {
            let mut sum = 0.0;
            for k in 0..n {
                let vk = argv!(k);
                let v = to_number(st, mods, vk)?;
                sum += v * v;
            }
            Ok(Value::number(sum.sqrt()))
        }
        M_CLZ32 => {
            // count leading zero bits of ToUint32(x); 32 for 0.
            // React's lane iteration (31 - clz32(lanes)) loops forever
            // without this — the highest set bit is never found.
            let v = argv!(0);
            let x = js_to_uint32(to_number(st, mods, v)?);
            Ok(Value::int(x.leading_zeros() as i32))
        }
        M_MIN | M_MAX => {
            let mut acc = if id == M_MIN { f64::INFINITY } else { f64::NEG_INFINITY };
            for k in 0..n {
                let vk = argv!(k);
                let v = to_number(st, mods, vk)?;
                if v.is_nan() {
                    acc = f64::NAN;
                    break;
                }
                acc = if id == M_MIN { acc.min(v) } else { acc.max(v) };
            }
            Ok(Value::number(acc))
        }
        M_RANDOM => {
            // xorshift64* — deterministic (seedable) but well-distributed
            let mut x = st.rng_state;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            st.rng_state = x;
            let r = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64
                / (1u64 << 53) as f64;
            Ok(Value::number(r))
        }
        O_KEYS | O_VALUES | O_ENTRIES => {
            let v = argv!(0);
            if proxy_id_of(st, v).is_some() {
                let keys = internal_own_keys(st, mods, v)?;
                let mut out = Vec::new();
                for key_value in keys {
                    let name = to_display(st, key_value);
                    if name == "length" {
                        continue;
                    }
                    let key = st.intern_name(&name);
                    let descriptor = internal_get_own_descriptor(
                        st, mods, v, key,
                    )?;
                    if descriptor.is_undefined()
                        || !descriptor_flag(st, descriptor, "enumerable")
                    {
                        continue;
                    }
                    let value = internal_get(st, mods, v, key, v)?;
                    out.push(host_entry(st, id, key_value, value));
                }
                return Ok(new_array(st, out));
            }
            if v.is_function() {
                let fidx = v.index();
                let mut out = Vec::new();
                for k in fn_own_enumerable_keys(st, fidx) {
                    let name = st.names[k as usize].clone();
                    let key = intern(st, &name);
                    let val = st
                        .fn_props
                        .get(&(fidx, k))
                        .copied()
                        .unwrap_or(Value::UNDEFINED);
                    out.push(host_entry(st, id, key, val));
                }
                return Ok(new_array(st, out));
            }
            if v.is_string() {
                // Object.keys('abc') is ['0','1','2'] -- a string's own
                // enumerable properties are its character indices
                let chars: Vec<String> = str_ref(st, v.index())
                    .chars()
                    .map(|c| c.to_string())
                    .collect();
                let mut out = Vec::with_capacity(chars.len());
                for (k, ch) in chars.into_iter().enumerate() {
                    let key = intern(st, &k.to_string());
                    let val = push_str(st, ch);
                    out.push(host_entry(st, id, key, val));
                }
                return Ok(new_array(st, out));
            }
            if !v.is_object() {
                return Ok(new_array(st, Vec::new()));
            }
            let oi = v.index() as usize;
            let nelems = st.objects[oi].elems.len();
            let v_is_array = st.objects[oi].is_array;
            let mut out = Vec::new();
            // array index keys first (like for-in). Plain objects store
            // numeric keys sparsely in elems (o["907"]=x pads 0..906
            // with holes) — enumerating the range would fabricate
            // hundreds of phantom undefined keys, so skip holes.
            for k in 0..nelems {
                let val = st.objects[oi].elems[k];
                if !v_is_array && val.is_undefined() {
                    continue;
                }
                let key = intern(st, &k.to_string());
                out.push(host_entry(st, id, key, val));
            }
            for atom in own_keys_ordered(st, oi, true) {
                let name = st.names[atom as usize].clone();
                let key = intern(st, &name);
                // read getter-aware so accessor props surface their value
                let val = match lookup_prop(st, oi, atom) {
                    PropHit::Data(d) => d,
                    PropHit::Getter(g) if g.is_function() => {
                        call_value_this(st, mods, g, Some(v), &[])?
                    }
                    _ => Value::UNDEFINED,
                };
                out.push(host_entry(st, id, key, val));
            }
            Ok(new_array(st, out))
        }
        O_ASSIGN => {
            let target = argv!(0);
            if !is_js_object(target) {
                return Ok(target);
            }
            for k in 1..n {
                let src = argv!(k);
                if !is_js_object(src) {
                    continue;
                }
                let keys = internal_own_keys(st, mods, src)?;
                for key_value in keys {
                    let name = to_display(st, key_value);
                    if name == "length" {
                        continue;
                    }
                    let key = st.intern_name(&name);
                    let value = internal_get(st, mods, src, key, src)?;
                    let _ = internal_set(
                        st, mods, target, key, value, target,
                    )?;
                }
            }
            Ok(target)
        }
        O_FROM_ENTRIES => {
            // Object.fromEntries(iterable): build an object from [k, v]
            // pairs (an array of arrays, or a Map). Data selectors use it
            // to reshape lists into keyed lookups.
            let src = argv!(0);
            let out = new_plain_object(st);
            let oi = out.index() as usize;
            let pairs: Vec<(Value, Value)> = if src.is_object() {
                let siu = src.index();
                let si = siu as usize;
                if let Some(entries) = st.map_data.get(&siu) {
                    entries.clone()
                } else if st.objects[si].is_array {
                    st.objects[si]
                        .elems
                        .clone()
                        .iter()
                        .map(|&e| {
                            if e.is_object()
                                && st.objects[e.index() as usize].is_array
                            {
                                let el =
                                    &st.objects[e.index() as usize].elems;
                                (
                                    el.first()
                                        .copied()
                                        .unwrap_or(Value::UNDEFINED),
                                    el.get(1)
                                        .copied()
                                        .unwrap_or(Value::UNDEFINED),
                                )
                            } else {
                                (Value::UNDEFINED, Value::UNDEFINED)
                            }
                        })
                        .collect()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            for (k, v) in pairs {
                let ks = to_display(st, k);
                let atom = st.intern_name(&ks);
                raw_set_prop(st, oi, atom, v);
            }
            Ok(out)
        }
        O_FREEZE => {
            // freeze = prevent extensions (proxy-aware, marks the
            // shared non_extensible_objects registry) + every current
            // own key becomes non-writable
            let o = argv!(0);
            if is_js_object(o) {
                let _ = internal_prevent_extensions(st, mods, o)?;
                if o.is_object() {
                    let oi = o.index();
                    for atom in own_keys_ordered(st, oi as usize, false) {
                        st.non_writable.insert((oi, atom));
                    }
                }
            }
            Ok(o)
        }
        O_SEAL | O_PREVENT_EXT => {
            let o = argv!(0);
            if is_js_object(o) {
                let _ = internal_prevent_extensions(st, mods, o)?;
            }
            Ok(o)
        }
        O_IS_EXTENSIBLE => {
            let o = argv!(0);
            Ok(Value::boolean(
                is_js_object(o) && internal_is_extensible(st, o),
            ))
        }
        O_IS_SEALED => {
            // primitives are sealed; objects are sealed once non-extensible
            // (seal/freeze/preventExtensions are the only routes to that)
            let o = argv!(0);
            Ok(Value::boolean(
                !is_js_object(o) || !internal_is_extensible(st, o),
            ))
        }
        O_IS_FROZEN => {
            let o = argv!(0);
            if !o.is_object() {
                return Ok(Value::boolean(!o.is_function()));
            }
            let oi = o.index();
            if internal_is_extensible(st, o)
                || !st.objects[oi as usize].elems.is_empty()
            {
                return Ok(Value::boolean(false));
            }
            let frozen = own_keys_ordered(st, oi as usize, false).iter().all(|&a| {
                st.non_writable.contains(&(oi, a))
                    || st.accessors.contains_key(&(oi, a))
            });
            Ok(Value::boolean(frozen))
        }
        O_DEFINE_PROP => {
            let obj = argv!(0);
            if !obj.is_object() && !obj.is_function() {
                return err("defineProperty needs an object");
            }
            // ToPropertyKey: object keys (polyfilled Symbols) keep
            // their distinct tags, matching the computed-read path
            let kv = argv!(1);
            let kvp = if kv.is_object() {
                to_primitive(st, mods, kv, false)?
            } else {
                kv
            };
            let key_name = to_display(st, kvp);
            let key = st.intern_name(&key_name);
            if !internal_define_property(st, mods, obj, key, argv!(2))? {
                return type_err("Object.defineProperty was rejected");
            }
            Ok(obj)
        }
        O_DEFINE_PROPS => {
            let obj = argv!(0);
            if !obj.is_object() && !obj.is_function() {
                return err("defineProperties needs an object");
            }
            let descs = argv!(1);
            if !descs.is_object() {
                return err("defineProperties needs a descriptor map");
            }
            let di = descs.index() as usize;
            // numeric keys of the descriptor map live in element
            // storage ({"907": {...}} — getOwnPropertyDescriptors
            // output); iterating only shape props silently defined
            // nothing for them
            for i in 0..st.objects[di].elems.len() {
                let desc = st.objects[di].elems[i];
                if desc.is_undefined() {
                    continue; // hole
                }
                let key = st.intern_name(&i.to_string());
                if !internal_define_property(st, mods, obj, key, desc)? {
                    return type_err("Object.defineProperties was rejected");
                }
            }
            let shape = st.objects[di].shape;
            let mut pairs: Vec<(u16, u32)> = st.shapes[shape as usize]
                .props.iter().map(|(&a, &s)| (s, a)).collect();
            pairs.sort_by_key(|&(slot, _)| slot);
            for (slot, atom) in pairs {
                let desc = st.objects[di].slots[slot as usize];
                if !internal_define_property(st, mods, obj, atom, desc)? {
                    return type_err("Object.defineProperties was rejected");
                }
            }
            Ok(obj)
        }
        O_GET_OWN_NAMES => {
            // own names = index keys + shape props + accessor-only
            // keys (Object.keys skips the accessor side-table)
            let v = argv!(0);
            if proxy_id_of(st, v).is_some() {
                let keys = internal_own_keys(st, mods, v)?;
                return Ok(new_array(st, keys));
            }
            if v.is_function() {
                let fidx = v.index();
                let mut keys: Vec<u32> = st.fn_props.keys()
                    .filter(|&&(f, _)| f == fidx)
                    .map(|&(_, k)| k).collect();
                keys.sort_unstable();
                // `length` and `name` are own properties of every
                // function even though this engine computes them
                // instead of storing them
                let name_id = st.intern_name("name");
                for k in [st.ids.length, name_id] {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
                if matches!(st.closures[fidx as usize],
                            ClosureRec::User { .. }) {
                    keys.push(st.ids.prototype);
                }
                let mut out = Vec::new();
                for k in keys {
                    let name = st.names[k as usize].clone();
                    out.push(intern(st, &name));
                }
                return Ok(new_array(st, out));
            }
            if !v.is_object() {
                return Ok(new_array(st, Vec::new()));
            }
            let oi = v.index() as usize;
            let (nelems, shape, has_acc, v_is_array) = {
                let o = &st.objects[oi];
                (o.elems.len(), o.shape, o.has_accessors, o.is_array)
            };
            let mut out = Vec::new();
            // skip plain-object element holes (sparse numeric keys)
            for k in 0..nelems {
                if !v_is_array && st.objects[oi].elems[k].is_undefined() {
                    continue;
                }
                out.push(intern(st, &k.to_string()));
            }
            // getOwnPropertyNames sees non-enumerable keys too (false)
            for atom in own_keys_ordered(st, oi, false) {
                let name = st.names[atom as usize].clone();
                out.push(intern(st, &name));
            }
            Ok(new_array(st, out))
        }
        CV_MEASURE_TEXT => {
            // canvas-2d stub: zero metrics (layout runs after JS)
            let m = new_plain_object(st);
            let mi = m.index() as usize;
            for f in ["width", "actualBoundingBoxAscent",
                      "actualBoundingBoxDescent",
                      "actualBoundingBoxLeft",
                      "actualBoundingBoxRight"] {
                let k = st.intern_name(f);
                raw_set_prop(st, mi, k, Value::int(0));
            }
            Ok(m)
        }
        CV_IMAGE_DATA => {
            let m = new_plain_object(st);
            let mi = m.index() as usize;
            let data = new_array(st, Vec::new());
            let dk = st.intern_name("data");
            raw_set_prop(st, mi, dk, data);
            for f in ["width", "height"] {
                let k = st.intern_name(f);
                raw_set_prop(st, mi, k, Value::int(0));
            }
            Ok(m)
        }
        CV_GRADIENT => {
            let g = new_plain_object(st);
            let gi = g.index() as usize;
            let noop = make_native(st, Native::Noop);
            let k = st.intern_name("addColorStop");
            raw_set_prop(st, gi, k, noop);
            Ok(g)
        }
        O_GET_OWN_PD => {
            let obj = argv!(0);
            if proxy_id_of(st, obj).is_some() {
                let key_value = argv!(1);
                let (key, _) = property_key(st, mods, key_value)?;
                return internal_get_own_descriptor(st, mods, obj, key);
            }
            if obj.is_function() {
                // builtin-constructor property copying (core-js):
                // answer for statics; prototype only on user functions
                let key_name = to_display(st, argv!(1));
                let Some(&key) = st.name_ids.get(&key_name) else {
                    return Ok(Value::UNDEFINED);
                };
                // (value, writable, enumerable, configurable). `name`
                // and `length` are computed rather than stored, so
                // without naming them here reflection could not see
                // them at all and they read as absent.
                let found = if key == st.ids.prototype {
                    match st.closures[obj.index() as usize] {
                        // a normal function's .prototype is writable
                        // but neither enumerable nor configurable
                        ClosureRec::User { .. } => {
                            Some((fn_prototype(st, obj), true, false, false))
                        }
                        _ => None, // native bind etc: spec says none
                    }
                } else if let Some(&v) =
                    st.fn_props.get(&(obj.index(), key))
                {
                    // an assigned static wins over the computed one,
                    // matching the order the read path uses
                    Some((v, true, true, true))
                } else if key == st.ids.length {
                    let n = Value::int(fn_arity(st, mods, obj));
                    Some((n, false, false, true))
                } else if key_name == "name" {
                    let n = fn_name(st, mods, obj);
                    Some((make_string(st, n), false, false, true))
                } else {
                    None
                };
                let Some((v, w, e, c)) = found else {
                    return Ok(Value::UNDEFINED);
                };
                let out = new_plain_object(st);
                let pi = out.index() as usize;
                let vid = st.intern_name("value");
                let wid = st.intern_name("writable");
                let eid = st.intern_name("enumerable");
                let cid = st.intern_name("configurable");
                raw_set_prop(st, pi, vid, v);
                raw_set_prop(st, pi, wid, Value::boolean(w));
                raw_set_prop(st, pi, eid, Value::boolean(e));
                raw_set_prop(st, pi, cid, Value::boolean(c));
                return Ok(out);
            }
            if !obj.is_object() {
                return Ok(Value::UNDEFINED);
            }
            let oi = obj.index() as usize;
            let key_name = to_display(st, argv!(1));
            // Arrays keep `length` and integer indices outside the shape,
            // so the slot lookup below misses them. core-js's array length
            // setter reads `getOwnPropertyDescriptor(arr, "length").writable`
            // before every mutation — an undefined descriptor makes it throw
            // "Cannot set read only .length", which aborts React's commit.
            if st.objects[oi].is_array && key_name == "length" {
                let n = st.objects[oi].elems.len() as i32;
                let out = new_plain_object(st);
                let pi = out.index() as usize;
                let f = Value::boolean(false);
                let vid = st.intern_name("value");
                let wid = st.intern_name("writable");
                let eid = st.intern_name("enumerable");
                let cid = st.intern_name("configurable");
                raw_set_prop(st, pi, vid, Value::int(n));
                raw_set_prop(st, pi, wid, Value::boolean(true));
                raw_set_prop(st, pi, eid, f);
                raw_set_prop(st, pi, cid, f);
                return Ok(out);
            }
            // numeric keys live in element storage on EVERY object, not
            // just arrays (o["907"]=x). Babel's object-spread copies one
            // property at a time through gOPD + defineProperty; missing
            // this made naver's pressInfo map lose all 246 entries.
            if let Some(i) = elem_index(&key_name) {
                if let Some(&v) = st.objects[oi].elems.get(i) {
                    if st.objects[oi].is_array || !v.is_undefined() {
                        return Ok(descriptor_object(st, v, true, true,
                                                    true));
                    }
                }
                if st.objects[oi].is_array {
                    return Ok(Value::UNDEFINED);
                }
                // non-array: a hole; fall through to the shape lookup
            }
            let Some(&key) = st.name_ids.get(&key_name) else {
                return Ok(Value::UNDEFINED);
            };
            let acc = if st.objects[oi].has_accessors {
                st.accessors.get(&(oi as u32, key)).copied()
            } else {
                None
            };
            let own = {
                let o = &st.objects[oi];
                st.shapes[o.shape as usize]
                    .props
                    .get(&key)
                    .map(|&slot| o.slots[slot as usize])
            };
            let out = new_plain_object(st);
            let pi = out.index() as usize;
            let enumerable =
                Value::boolean(!st.non_enum.contains(&(oi as u32, key)));
            if let Some((g, s)) = acc {
                let gid = st.intern_name("get");
                let sid = st.intern_name("set");
                raw_set_prop(st, pi, gid, g);
                raw_set_prop(st, pi, sid, s);
            } else if let Some(v) = own {
                let writable =
                    Value::boolean(!st.non_writable.contains(&(oi as u32, key)));
                let vid = st.intern_name("value");
                let wid = st.intern_name("writable");
                raw_set_prop(st, pi, vid, v);
                raw_set_prop(st, pi, wid, writable);
            } else {
                return Ok(Value::UNDEFINED);
            }
            let eid = st.intern_name("enumerable");
            let cid = st.intern_name("configurable");
            raw_set_prop(st, pi, eid, enumerable);
            raw_set_prop(st, pi, cid, Value::boolean(true));
            Ok(out)
        }
        O_GET_OWN_PDS => {
            // Object.getOwnPropertyDescriptors: {key: descriptor} for
            // every own property. Babel's _objectSpread2 prefers this
            // (with defineProperties) whenever it is truthy, so the
            // pair must round-trip element-stored numeric keys.
            let obj = argv!(0);
            let out = new_plain_object(st);
            if !obj.is_object() && !obj.is_function() {
                return Ok(out);
            }
            let keys = internal_own_keys(st, mods, obj)?;
            for key_value in keys {
                let name = to_display(st, key_value);
                if name == "length" && obj.is_object()
                    && st.objects[obj.index() as usize].is_array
                {
                    continue; // parity with Object.keys-based spreads
                }
                let key = st.intern_name(&name);
                let desc =
                    internal_get_own_descriptor(st, mods, obj, key)?;
                if desc.is_undefined() {
                    continue;
                }
                let ii = out.index() as usize;
                if let Some(index) = elem_index(&name) {
                    if index >= MAX_ARRAY_ELEMS {
                        return range_err(
                            "array index exceeds the engine limit");
                    }
                    let elems = &mut st.objects[ii].elems;
                    if index >= elems.len() {
                        elems.resize(index, Value::UNDEFINED);
                        elems.push(desc);
                    } else {
                        elems[index] = desc;
                    }
                } else {
                    raw_set_prop(st, ii, key, desc);
                }
            }
            Ok(out)
        }
        O_CREATE => {
            let proto = argv!(0);
            let out = new_plain_object(st);
            if proto.is_object() {
                st.objects[out.index() as usize].proto = proto;
            }
            // optional property-descriptor map (Babel _inherits):
            // plain {value} descriptors become data properties
            let descs = argv!(1);
            if descs.is_object() {
                let di = descs.index() as usize;
                let shape = st.objects[di].shape;
                let props: Vec<(u32, u16)> = st.shapes[shape as usize]
                    .props
                    .iter()
                    .map(|(&a, &s)| (a, s))
                    .collect();
                for (atom, slot) in props {
                    let desc = st.objects[di].slots[slot as usize];
                    if desc.is_object() {
                        let vk = st.intern_name("value");
                        if let Some(v) = raw_get_prop(
                            st, desc.index() as usize, vk)
                        {
                            raw_set_prop(
                                st, out.index() as usize, atom, v);
                        }
                    }
                }
            }
            Ok(out)
        }
        O_IS => {
            // Object.is: SameValue (=== but NaN==NaN and -0 !== +0).
            // React's bailout/shallowEqual lean on this heavily.
            let x = argv!(0);
            let y = argv!(1);
            let same = if x.is_number() && y.is_number() {
                let (a, b) = (x.to_number_raw(), y.to_number_raw());
                if a.is_nan() && b.is_nan() {
                    true
                } else if a == 0.0 && b == 0.0 {
                    // distinguish -0 from +0 by sign bit
                    a.is_sign_negative() == b.is_sign_negative()
                } else {
                    a == b
                }
            } else {
                strict_eq(st, x, y)
            };
            Ok(Value::boolean(same))
        }
        O_GET_PROTO => {
            let v = argv!(0);
            if proxy_id_of(st, v).is_some() {
                return internal_get_proto(st, mods, v);
            }
            Ok(if v.is_object() {
                let p = st.objects[v.index() as usize].proto;
                if !p.is_object() && st.known.object.is_function() {
                    // a plain object's [[Prototype]] is
                    // Object.prototype (ours store UNDEFINED); the
                    // canonical prototype itself ends the chain
                    let op = fn_prototype(st, st.known.object);
                    if v == op {
                        Value::NULL
                    } else {
                        op
                    }
                } else {
                    p
                }
            } else if v.is_function() {
                match st.fn_proto_chain.get(&v.index()).copied() {
                    Some(p) => p,
                    // an ordinary function's [[Prototype]] is
                    // Function.prototype (core-js-pure walks this to
                    // reach iterator prototypes — Naver search bundle)
                    None if st.known.function.is_function() => {
                        fn_prototype(st, st.known.function)
                    }
                    None => Value::UNDEFINED,
                }
            } else {
                Value::UNDEFINED
            })
        }
        O_SET_PROTO => {
            let v = argv!(0);
            let p = argv!(1);
            if proxy_id_of(st, v).is_some() {
                if !internal_set_proto(st, mods, v, p)? {
                    return type_err("Object.setPrototypeOf was rejected");
                }
                return Ok(v);
            }
            if v.is_object() {
                st.objects[v.index() as usize].proto =
                    if p.is_object() { p } else { Value::UNDEFINED };
            } else if v.is_function() {
                // static inheritance (Babel: setPrototypeOf(Sub, Sup))
                st.fn_proto_chain.insert(v.index(), p);
            }
            Ok(v)
        }
        A_OF => {
            // Array.of(...items): the args become the elements verbatim
            let items: Vec<Value> =
                (0..n).map(|k| argv!(k)).collect();
            Ok(new_array(st, items))
        }
        A_ISARRAY => {
            let mut v = argv!(0);
            for _ in 0..16 {
                let Some(id) = proxy_id_of(st, v) else { break };
                v = proxy_rec(st, id)?.target;
            }
            Ok(Value::boolean(v.is_object()
                && st.objects[v.index() as usize].is_array))
        }
        A_FROM => {
            let v = argv!(0);
            let mapfn = argv!(1);
            let mut items: Vec<Value> = if v.is_string() {
                let s = str_ref(st, v.index()).to_string();
                if s.len() > MAX_MATERIALIZE {
                    return range_err("string too long to materialize");
                }
                s.chars().map(|c| push_str(st, c.to_string())).collect()
            } else if v.is_object() {
                let oi = v.index();
                if st.objects[oi as usize].is_array {
                    st.objects[oi as usize].elems.clone()
                } else if let Some(m) = {
                    // iterables (Set, Map, generators, custom { next })
                    // drain through the shared iterator protocol
                    let m = materialize_iterable(st, mods, v)?;
                    if m.index() != oi
                        && m.is_object()
                        && st.objects[m.index() as usize].is_array
                    {
                        Some(st.objects[m.index() as usize].elems.clone())
                    } else {
                        None
                    }
                } {
                    m
                } else {
                    // array-like: consult .length, then index 0..length
                    let lenv = match lookup_prop(st, oi as usize, st.ids.length) {
                        PropHit::Data(d) => d,
                        PropHit::Getter(g) if g.is_function() => {
                            call_value_this(st, mods, g, Some(v), &[])?
                        }
                        _ => Value::UNDEFINED,
                    };
                    let len = {
                        let n = lenv.to_number_raw();
                        if n.is_finite() && n > 0.0 {
                            (n as usize).min(1 << 24)
                        } else {
                            0
                        }
                    };
                    let mut out = Vec::with_capacity(len.min(1 << 16));
                    for i in 0..len {
                        // numeric indices live in dense elems on any object
                        let val = if i < st.objects[oi as usize].elems.len() {
                            st.objects[oi as usize].elems[i]
                        } else {
                            let ka = st.intern_name(&i.to_string());
                            match lookup_prop(st, oi as usize, ka) {
                                PropHit::Data(d) => d,
                                PropHit::Getter(g) if g.is_function() => {
                                    call_value_this(st, mods, g, Some(v), &[])?
                                }
                                _ => Value::UNDEFINED,
                            }
                        };
                        out.push(val);
                    }
                    out
                }
            } else {
                Vec::new()
            };
            if mapfn.is_function() {
                for i in 0..items.len() {
                    let it = items[i];
                    items[i] = call_value_this(
                        st, mods, mapfn, None, &[it, Value::int(i as i32)],
                    )?;
                }
            }
            Ok(new_array(st, items))
        }
        N_ISNAN => {
            let v = argv!(0);
            Ok(Value::boolean(v.is_number() && v.to_number_raw().is_nan()))
        }
        N_ISFINITE => {
            let v = argv!(0);
            Ok(Value::boolean(v.is_number() && v.to_number_raw().is_finite()))
        }
        N_ISINTEGER => {
            let v = argv!(0);
            Ok(Value::boolean(
                v.is_number() && {
                    let x = v.to_number_raw();
                    x.is_finite() && x.fract() == 0.0
                },
            ))
        }
        N_ISSAFEINT => {
            let v = argv!(0);
            Ok(Value::boolean(v.is_number() && {
                let x = v.to_number_raw();
                x.is_finite()
                    && x.fract() == 0.0
                    && x.abs() <= 9_007_199_254_740_991.0
            }))
        }
        O_HAS_OWN => {
            // Object.hasOwn(o, k) — the static form of hasOwnProperty
            let o = argv!(0);
            let k = argv!(1);
            Ok(Value::boolean(
                o.is_object() && has_own_property(st, mods, o, k)?,
            ))
        }
        S_FROMCHARCODE => {
            let mut s = String::new();
            for k in 0..n {
                let ck = argv!(k);
                let code = to_number(st, mods, ck)? as u32;
                if let Some(c) = char::from_u32(code) {
                    s.push(c);
                }
            }
            Ok(push_str(st, s))
        }
        _ => err(format!("unknown host function id {id}")),
    }
}

/// Build one Object.keys/values/entries element.
fn host_entry(st: &mut St, id: u16, key: Value, val: Value) -> Value {
    match id {
        host::O_KEYS => key,
        host::O_VALUES => val,
        _ => new_array(st, vec![key, val]), // entries: [key, value]
    }
}

// ---- JSON ----

fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `None` means "omit" (undefined / function): dropped from objects,
/// becomes `null` in arrays, yields `undefined` at the top level.
/// Errs on cyclic structures like real JSON.stringify.
/// Options threaded through a JSON.stringify call: the indent unit
/// (`gap`, empty for compact output), an optional function replacer, and
/// an optional array replacer allow-list of atom keys.
struct JsonCtx {
    gap: String,
    replacer: Value,
    allow: Option<Vec<u32>>,
}

/// Own string-keyed properties of an object in enumeration order: data
/// slots by insertion, then accessor-only keys. Used by JSON.stringify so
/// getters are serialized (real engines call them) and defineProperty
/// accessors are not silently dropped.
fn json_own_keys(st: &St, oi: usize) -> Vec<u32> {
    own_keys_ordered(st, oi, true)
}

/// Full SerializeJSONProperty: applies toJSON, then a function replacer,
/// then serializes the (possibly replaced) value with `ctx.gap`
/// indentation. `holder`/`key` provide the receiver and property name the
/// replacer is invoked with.
fn json_serialize(
    st: &mut St,
    mods: &ModStore,
    holder: Value,
    key: &str,
    v0: Value,
    seen: &mut Vec<u32>,
    ctx: &JsonCtx,
    indent: &str,
) -> Result<Option<String>, VmError> {
    let mut v = v0;
    // 1. value.toJSON(key) if present
    if v.is_object() {
        let oi = v.index() as usize;
        let tj = st.intern_name("toJSON");
        let f = match lookup_prop(st, oi, tj) {
            PropHit::Data(f) => f,
            PropHit::Getter(g) if g.is_function() => {
                call_value_this(st, mods, g, Some(v), &[])?
            }
            _ => Value::UNDEFINED,
        };
        if f.is_function() {
            let ks = make_string(st, key.to_string());
            v = call_value_this(st, mods, f, Some(v), &[ks])?;
        }
    }
    // 2. function replacer(holder, key, value)
    if ctx.replacer.is_function() {
        let ks = make_string(st, key.to_string());
        v = call_value_this(st, mods, ctx.replacer, Some(holder), &[ks, v])?;
    }
    // 3. serialize
    if v.is_undefined() || v.is_function() {
        return Ok(None);
    }
    if v.is_null() {
        return Ok(Some("null".to_string()));
    }
    if v.is_boolean() {
        return Ok(Some(
            if v.as_bool() { "true" } else { "false" }.to_string(),
        ));
    }
    if v.is_number() {
        let n = v.to_number_raw();
        return Ok(Some(if n.is_finite() {
            js_num_str(n)
        } else {
            "null".to_string()
        }));
    }
    if v.is_string() {
        let s = str_ref(st, v.index()).to_string();
        return Ok(Some(json_quote(&s)));
    }
    if v.is_object() {
        let oi = v.index() as usize;
        if seen.contains(&(oi as u32)) {
            return err("Converting circular structure to JSON");
        }
        if seen.len() >= 200 {
            return err("structure too deep to JSON.stringify");
        }
        seen.push(oi as u32);
        let child = format!("{indent}{}", ctx.gap);
        let pretty = !ctx.gap.is_empty();
        let out = if st.objects[oi].is_array {
            let elems = st.objects[oi].elems.clone();
            if elems.is_empty() {
                "[]".to_string()
            } else {
                let mut parts = Vec::with_capacity(elems.len());
                for (i, e) in elems.into_iter().enumerate() {
                    let ks = i.to_string();
                    parts.push(
                        json_serialize(st, mods, v, &ks, e, seen, ctx, &child)?
                            .unwrap_or_else(|| "null".to_string()),
                    );
                }
                if pretty {
                    format!("[\n{child}{}\n{indent}]",
                        parts.join(&format!(",\n{child}")))
                } else {
                    format!("[{}]", parts.join(","))
                }
            }
        } else {
            let colon = if pretty { ": " } else { ":" };
            let keys = json_own_keys(st, oi);
            let mut parts = Vec::new();
            for atom in keys {
                if let Some(allow) = &ctx.allow {
                    if !allow.contains(&atom) {
                        continue;
                    }
                }
                let pv = match lookup_prop(st, oi, atom) {
                    PropHit::Data(d) => d,
                    PropHit::Getter(g) if g.is_function() => {
                        call_value_this(st, mods, g, Some(v), &[])?
                    }
                    _ => Value::UNDEFINED,
                };
                let kname = st.names[atom as usize].clone();
                if let Some(vs) =
                    json_serialize(st, mods, v, &kname, pv, seen, ctx, &child)?
                {
                    parts.push(format!("{}{colon}{vs}", json_quote(&kname)));
                }
            }
            if parts.is_empty() {
                "{}".to_string()
            } else if pretty {
                format!("{{\n{child}{}\n{indent}}}",
                    parts.join(&format!(",\n{child}")))
            } else {
                format!("{{{}}}", parts.join(","))
            }
        };
        seen.pop();
        return Ok(Some(out));
    }
    Ok(None)
}

struct JsonP {
    b: Vec<char>,
    i: usize,
    /// current [ / { nesting (recursion is native stack — cap it)
    depth: usize,
}

impl JsonP {
    /// "Unexpected token in JSON" on its own names nothing anybody can
    /// act on when the text came off a stream. Quote the neighbourhood.
    fn bad(&self) -> String {
        let from = self.i.saturating_sub(24);
        let to = (self.i + 24).min(self.b.len());
        let near: String = self.b[from..to].iter().collect();
        format!(
            "Unexpected token in JSON at {} of {} near {:?}",
            self.i,
            self.b.len(),
            near,
        )
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_whitespace() {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<char> {
        self.b.get(self.i).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.b.get(self.i).copied();
        if c.is_some() {
            self.i += 1;
        }
        c
    }
}

fn json_parse(st: &mut St, p: &mut JsonP) -> Result<Value, VmError> {
    p.ws();
    match p.peek() {
        Some(c @ ('{' | '[')) => {
            if p.depth >= 512 {
                return syntax_err("JSON input too deeply nested");
            }
            p.depth += 1;
            let r = if c == '{' {
                json_parse_object(st, p)
            } else {
                json_parse_array(st, p)
            };
            p.depth -= 1;
            r
        }
        Some('"') => {
            let s = json_parse_string(p)?;
            Ok(push_str(st, s))
        }
        Some('t') => json_parse_lit(p, "true", Value::TRUE),
        Some('f') => json_parse_lit(p, "false", Value::FALSE),
        Some('n') => json_parse_lit(p, "null", Value::NULL),
        Some(c) if c == '-' || c.is_ascii_digit() => json_parse_number(p),
        _ => syntax_err(p.bad()),
    }
}

fn json_parse_lit(p: &mut JsonP, word: &str, v: Value) -> Result<Value, VmError> {
    for want in word.chars() {
        if p.bump() != Some(want) {
            return syntax_err(p.bad());
        }
    }
    Ok(v)
}

fn json_parse_number(p: &mut JsonP) -> Result<Value, VmError> {
    let start = p.i;
    if p.peek() == Some('-') {
        p.bump();
    }
    while matches!(p.peek(), Some(c) if c.is_ascii_digit()) {
        p.bump();
    }
    if p.peek() == Some('.') {
        p.bump();
        while matches!(p.peek(), Some(c) if c.is_ascii_digit()) {
            p.bump();
        }
    }
    if matches!(p.peek(), Some('e') | Some('E')) {
        p.bump();
        if matches!(p.peek(), Some('+') | Some('-')) {
            p.bump();
        }
        while matches!(p.peek(), Some(c) if c.is_ascii_digit()) {
            p.bump();
        }
    }
    let numstr: String = p.b[start..p.i].iter().collect();
    if let Ok(i) = numstr.parse::<i32>() {
        return Ok(Value::int(i));
    }
    match numstr.parse::<f64>() {
        Ok(n) => Ok(Value::number(n)),
        Err(_) => syntax_err("Invalid number in JSON"),
    }
}

fn json_parse_string(p: &mut JsonP) -> Result<String, VmError> {
    p.bump(); // opening quote
    let mut s = String::new();
    loop {
        match p.bump() {
            None => return syntax_err("Unterminated string in JSON"),
            Some('"') => return Ok(s),
            Some('\\') => match p.bump() {
                Some('"') => s.push('"'),
                Some('\\') => s.push('\\'),
                Some('/') => s.push('/'),
                Some('n') => s.push('\n'),
                Some('t') => s.push('\t'),
                Some('r') => s.push('\r'),
                Some('b') => s.push('\u{8}'),
                Some('f') => s.push('\u{c}'),
                Some('u') => {
                    let mut code: u32 = 0;
                    for _ in 0..4 {
                        let d = p.bump().and_then(|c| c.to_digit(16));
                        match d {
                            Some(d) => code = code * 16 + d,
                            None => return syntax_err("Bad \\u escape in JSON"),
                        }
                    }
                    s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                _ => return syntax_err("Bad escape in JSON"),
            },
            Some(c) => s.push(c),
        }
    }
}

fn json_parse_array(st: &mut St, p: &mut JsonP) -> Result<Value, VmError> {
    p.bump(); // [
    let mut elems = Vec::new();
    p.ws();
    if p.peek() == Some(']') {
        p.bump();
        return Ok(new_array(st, elems));
    }
    loop {
        let v = json_parse(st, p)?;
        elems.push(v);
        p.ws();
        match p.bump() {
            Some(',') => continue,
            Some(']') => return Ok(new_array(st, elems)),
            _ => return syntax_err("Expected , or ] in JSON"),
        }
    }
}

fn json_parse_object(st: &mut St, p: &mut JsonP) -> Result<Value, VmError> {
    p.bump(); // {
    let obj = new_plain_object(st);
    let oi = obj.index() as usize;
    p.ws();
    if p.peek() == Some('}') {
        p.bump();
        return Ok(obj);
    }
    loop {
        p.ws();
        if p.peek() != Some('"') {
            return syntax_err("Expected string key in JSON");
        }
        let key = json_parse_string(p)?;
        p.ws();
        if p.bump() != Some(':') {
            return syntax_err("Expected : in JSON");
        }
        let val = json_parse(st, p)?;
        let key_id = st.intern_name(&key);
        raw_set_prop(st, oi, key_id, val);
        p.ws();
        match p.bump() {
            Some(',') => continue,
            Some('}') => return Ok(obj),
            _ => return syntax_err("Expected , or } in JSON"),
        }
    }
}

/// The argument as a flattened Rust string (for DOM natives).
fn arg_string(st: &mut St, args_base: usize, argc: u8, k: usize)
    -> Result<String, VmError>
{
    if k >= argc as usize {
        return Ok(String::new());
    }
    let v = st.regs[args_base + k];
    Ok(to_display(st, v))
}

fn need_doc(st: &St) -> Result<Rc<RefCell<dom::Document>>, VmError> {
    match &st.doc {
        Some(d) => Ok(d.clone()),
        None => syntax_err("no document attached to this VM"),
    }
}

/// DOM method dispatch (`document.x(...)` and element methods).
/// Decode scrollTo/scrollBy arguments: `(x, y)` or an options object
/// `{left, top, behavior}`. Returns (left, top), each None when the
/// caller did not specify that axis.
fn scroll_args(st: &mut St, a0: Value, a1: Value) -> (Option<f64>, Option<f64>) {
    if a0.is_object() {
        let oi = a0.index() as usize;
        let mut out = (None, None);
        for (field, is_left) in [("left", true), ("top", false)] {
            let fk = st.intern_name(field);
            if let Some(v) = raw_get_prop(st, oi, fk) {
                if !v.is_undefined() {
                    let n = Some(v.to_number_raw());
                    if is_left { out.0 = n } else { out.1 = n }
                }
            }
        }
        return out;
    }
    let left = if a0.is_undefined() { None } else { Some(a0.to_number_raw()) };
    let top = if a1.is_undefined() { None } else { Some(a1.to_number_raw()) };
    (left, top)
}

/// Operations available on a same-origin child document's mirror.
pub(super) mod framedom {
    // element reads
    pub const GET_TEXT: u8 = 0;
    pub const GET_HTML: u8 = 1;
    pub const GET_ID: u8 = 2;
    pub const GET_CLASS: u8 = 3;
    pub const GET_TAG: u8 = 4;
    pub const GET_VALUE: u8 = 5;
    pub const GET_CHILDREN: u8 = 6;
    pub const GET_PARENT: u8 = 7;
    // element writes (mirror first, then queued for the host)
    pub const SET_TEXT: u8 = 8;
    pub const SET_ID: u8 = 9;
    pub const SET_CLASS: u8 = 10;
    pub const SET_VALUE: u8 = 11;
    // element methods
    pub const GET_ATTR: u8 = 12;
    pub const SET_ATTR: u8 = 13;
    pub const REMOVE_ATTR: u8 = 14;
    pub const HAS_ATTR: u8 = 15;
    pub const QUERY: u8 = 16;
    pub const QUERY_ALL: u8 = 17;
    pub const MATCHES: u8 = 18;
    pub const CLICK: u8 = 19;
    // document-level (node is ignored)
    pub const DOC_QUERY: u8 = 20;
    pub const DOC_QUERY_ALL: u8 = 21;
    pub const DOC_BY_ID: u8 = 22;
    pub const DOC_BY_TAG: u8 = 23;
    pub const DOC_BODY: u8 = 24;
    pub const DOC_ROOT: u8 = 25;
    pub const DOC_TITLE: u8 = 26;
    pub const DOC_URL: u8 = 27;

    /// Writes the host has to replay against the real child document.
    pub fn is_write(op: u8) -> bool {
        matches!(op, SET_TEXT | SET_ID | SET_CLASS | SET_VALUE
                 | SET_ATTR | REMOVE_ATTR | CLICK)
    }
}

/// The `Window` object JS sees for another browsing context.
///
/// A proxy carries `postMessage` and the handful of properties that
/// are readable across origins, and nothing else: no `document`, no
/// `location`. That is not a stub -- it is the cross-origin surface,
/// and it is all a document ever gets for a context it does not own.
/// Proxies are cached per handle so `e.source === f.contentWindow`.
pub(super) fn window_proxy(st: &mut St, ctx: u32) -> Value {
    if ctx == 0 {
        return st.known.window;
    }
    if let Some(&v) = st.ctx_proxies.get(&ctx) {
        return v;
    }
    let w = new_plain_object(st);
    let oi = w.index() as usize;
    let post = make_native(st, Native::PostMessage { ctx });
    for (name, v) in [
        ("postMessage", post),
        ("closed", Value::boolean(false)),
        ("length", Value::int(0)),
        ("self", w),
        ("window", w),
    ] {
        let k = st.intern_name(name);
        raw_set_prop(st, oi, k, v);
    }
    let k = st.intern_name("name");
    let empty = push_str(st, String::new());
    raw_set_prop(st, oi, k, empty);
    st.ctx_proxies.insert(ctx, w);
    w
}

/// The object JS gets for one element of a mirrored child document.
///
/// Cached per node, so identity comparisons hold. Property reads that
/// look like fields (`textContent`, `id`, ...) are real accessors, not
/// snapshots: the mirror can be rewritten under them by a write or a
/// re-push, and a stale snapshot would silently lie.
pub(super) fn frame_element_value(
    st: &mut St,
    frame: u32,
    node: u32,
) -> Value {
    if let Some(m) = st.frame_mirrors.get(&frame) {
        if let Some(&v) = m.wrappers.get(&node) {
            return v;
        }
    }
    let w = new_plain_object(st);
    let oi = w.index() as usize;
    let one = st.intern_name("nodeType");
    raw_set_prop(st, oi, one, Value::int(1));
    // accessor pairs; a missing setter makes the property read-only
    for (name, get, set) in [
        ("textContent", framedom::GET_TEXT, Some(framedom::SET_TEXT)),
        ("innerText", framedom::GET_TEXT, Some(framedom::SET_TEXT)),
        ("innerHTML", framedom::GET_HTML, None),
        ("id", framedom::GET_ID, Some(framedom::SET_ID)),
        ("className", framedom::GET_CLASS, Some(framedom::SET_CLASS)),
        ("tagName", framedom::GET_TAG, None),
        ("nodeName", framedom::GET_TAG, None),
        ("value", framedom::GET_VALUE, Some(framedom::SET_VALUE)),
        ("children", framedom::GET_CHILDREN, None),
        ("parentElement", framedom::GET_PARENT, None),
    ] {
        let k = st.intern_name(name);
        let g = make_native(st, Native::FrameDom { frame, node, op: get });
        let sfn = match set {
            Some(op) => make_native(st, Native::FrameDom { frame, node, op }),
            None => Value::UNDEFINED,
        };
        st.accessors.insert((oi as u32, k), (g, sfn));
    }
    st.objects[oi].has_accessors = true;
    for (name, op) in [
        ("getAttribute", framedom::GET_ATTR),
        ("setAttribute", framedom::SET_ATTR),
        ("removeAttribute", framedom::REMOVE_ATTR),
        ("hasAttribute", framedom::HAS_ATTR),
        ("querySelector", framedom::QUERY),
        ("querySelectorAll", framedom::QUERY_ALL),
        ("matches", framedom::MATCHES),
        ("click", framedom::CLICK),
    ] {
        let k = st.intern_name(name);
        let f = make_native(st, Native::FrameDom { frame, node, op });
        raw_set_prop(st, oi, k, f);
    }
    if let Some(m) = st.frame_mirrors.get_mut(&frame) {
        m.wrappers.insert(node, w);
    }
    w
}

/// Install a read-only accessor on an object (page.rs cannot touch
/// `Obj.has_accessors` directly — the field is private to this module).
pub(super) fn define_getter(
    st: &mut St,
    oi: usize,
    name: &str,
    getter: Value,
) {
    let k = st.intern_name(name);
    st.accessors.insert((oi as u32, k), (getter, Value::UNDEFINED));
    st.objects[oi].has_accessors = true;
}

/// Record a mutation for the host to replay against the real child.
fn queue_frame_write(st: &mut St, frame: u32, node: u32, op: u8,
                     a: String, b: String) {
    st.frame_seq += 1;
    let seq = st.frame_seq;
    st.frame_dom_writes.push((frame, node, op, a, b, seq));
}

/// Parse a JSON payload back into a value in *this* document's heap.
/// Returns None on malformed input rather than throwing: the host
/// produced this text from a sibling document, so a parse failure is
/// an engine bug, not something page JS should observe as an
/// exception mid-delivery.
pub(super) fn parse_json(st: &mut St, text: &str) -> Option<Value> {
    let mut p = JsonP {
        b: text.chars().collect(),
        i: 0,
        depth: 0,
    };
    json_parse(st, &mut p).ok()
}

/// Build a MessageEvent and queue its delivery as a macrotask.
///
/// postMessage is always asynchronous, even to your own window, so the
/// sender's remaining statements run first. Reusing the timer queue
/// (rather than calling handlers inline) is what buys that ordering
/// for free.
pub(super) fn queue_message_task(
    st: &mut St,
    data_json: &str,
    origin: &str,
    source: Value,
) {
    let data = parse_json(st, data_json).unwrap_or(Value::NULL);
    let ev = new_plain_object(st);
    let oi = ev.index() as usize;
    let win = st.known.window;
    let ty = push_str(st, "message".to_string());
    let org = push_str(st, origin.to_string());
    let last = push_str(st, String::new());
    let ports = new_array(st, Vec::new());
    let noop = make_native(st, Native::Noop);
    for (name, v) in [
        ("type", ty),
        ("data", data),
        ("origin", org),
        ("source", source),
        ("lastEventId", last),
        ("ports", ports),
        ("target", win),
        ("currentTarget", win),
        ("preventDefault", noop),
        ("stopPropagation", noop),
        ("stopImmediatePropagation", noop),
    ] {
        let k = st.intern_name(name);
        raw_set_prop(st, oi, k, v);
    }
    let cb = make_native(st, Native::WinDeliver);
    st.next_timer_id += 1;
    st.timer_seq += 1;
    let id = st.next_timer_id;
    let seq = st.timer_seq;
    st.timers.push(Timer {
        id,
        callback: cb,
        args: vec![ev],
        due_ms: st.now_ms,
        seq,
        interval: None,
        is_raf: false,
    });
}

/// Queue one scroll request for the host and update the optimistic
/// scroll state page JS reads back within the same turn.
///
/// A relative request travels as a *delta* (kind 1) rather than a
/// resolved absolute position: the host replays it against wherever
/// the scroller actually sits, so `el.scrollIntoView(); window
/// .scrollBy(0, -80)` — the sticky-header idiom — still offsets the
/// reveal the engine performed, which the VM cannot see until layout.
fn queue_scroll(
    st: &mut St,
    node: u32,
    left: Option<f64>,
    top: Option<f64>,
    relative: bool,
) {
    let fin = |v: Option<f64>| v.filter(|n| n.is_finite());
    let (left, top) = (fin(left), fin(top));
    let cur = st.scroll_state.get(&node).copied()
        .unwrap_or((0.0, 0.0, 0.0, 0.0));
    st.scroll_seq += 1;
    let seq = st.scroll_seq;
    let (want_top, want_left) = if relative {
        let (dt, dl) = (top.unwrap_or(0.0), left.unwrap_or(0.0));
        st.scroll_writes.push((node, dt, dl, seq, true));
        ((cur.0 + dt).max(0.0), (cur.1 + dl).max(0.0))
    } else {
        let t = top.unwrap_or(cur.0).max(0.0);
        let l = left.unwrap_or(cur.1).max(0.0);
        st.scroll_writes.push((node, t, l, seq, false));
        (t, l)
    };
    let entry = st.scroll_state.entry(node)
        .or_insert((0.0, 0.0, 0.0, 0.0));
    entry.0 = want_top;
    entry.1 = want_left;
    if node == DOC_NODE {
        // window.scrollTo is synchronous in a real browser, so
        // window.scrollY has to answer the new position now — the
        // host's own push does not land until the next turn.
        let wi = st.known.window.index() as usize;
        for (f, v) in [("scrollY", want_top), ("pageYOffset", want_top),
                       ("scrollX", want_left), ("pageXOffset", want_left)] {
            let k = st.intern_name(f);
            raw_set_prop(st, wi, k, Value::number(v));
        }
    }
}

/// `document.write(markup)` into the document that is running: parse
/// the markup and splice it in at the parser insertion point.
///
/// A real parser inserts right where the `<script>` sits, because the
/// script is running mid-parse. We run scripts after the parse, but
/// `document.currentScript` still names the script whose turn it is,
/// and "immediately after that element" is the same position. Ad
/// SafeFrames are built entirely on this: the shell document is a
/// `<script>` inside a wrapper div, and it writes the creative markup
/// next to itself. Without it every ad slot laid out at its reserved
/// size and painted nothing.
fn doc_write_into(
    st: &mut St, doc: &Rc<RefCell<dom::Document>>, markup: &str,
) {
    if let Some(buf) = &mut st.parser_writes {
        buf.push_str(markup);
        return;
    }
    let owner = st.current_script.unwrap_or(u32::MAX);
    let mut d = doc.borrow_mut();
    let at = match st.doc_write_at {
        // same script writing again: continue where it left off
        Some((who, parent, next)) if who == owner
            && parent < d.nodes.len()
            && next <= d.nodes[parent].children.len() => Some((parent, next)),
        _ => st
            .current_script
            .map(|s| s as usize)
            .filter(|&s| s < d.nodes.len())
            .and_then(|s| d.nodes[s].parent.map(|p| (p, s)))
            .and_then(|(p, s)| {
                d.nodes[p].children.iter().position(|&c| c == s)
                    .map(|i| (p, i + 1))
            })
            .or_else(|| {
                find_tag(&d, "body").map(|b| (b, d.nodes[b].children.len()))
            }),
    };
    let Some((parent, at)) = at else { return };
    // whichever parser the page itself was built with
    let frag = crate::parse_document(markup);
    let before = d.nodes[parent].children.len();
    for section in ["head", "body"] {
        if let Some(s) = find_tag(&frag, section) {
            for child in frag.nodes[s].children.clone() {
                d.graft(&frag, child, parent);
            }
        }
    }
    // graft appends; rotate the new run back to the insertion point
    let kids = &mut d.nodes[parent].children;
    let added = kids.len() - before;
    kids[at..].rotate_right(added);
    st.doc_write_at = Some((owner, parent, at + added));
}

/// Rebuild a node's cached `attributes` map in place from the DOM's
/// current attribute list. Named keys that no longer exist are set to
/// undefined (shape slots cannot be removed); indexed entries and
/// `length` are replaced wholesale, which is what liveness means to
/// the loops that capture the map.
fn refresh_attr_map(
    st: &mut St,
    doc: &Rc<RefCell<dom::Document>>,
    node: u32,
    map: Value,
) {
    let oi = map.index() as usize;
    // names the map currently advertises, to blank the stale ones
    let nk = st.intern_name("name");
    let prev = st.objects[oi].elems.clone();
    let mut old_names = Vec::with_capacity(prev.len());
    for it in prev {
        if !it.is_object() {
            continue;
        }
        if let Some(v) = raw_get_prop(st, it.index() as usize, nk) {
            if v.is_string() {
                old_names.push(str_ref(st, v.index()).to_string());
            }
        }
    }
    let attrs = doc.borrow().nodes[node as usize].attrs.clone();
    for name in &old_names {
        if !attrs.iter().any(|(k, _)| k == name) {
            let kk = st.intern_name(name);
            raw_set_prop(st, oi, kk, Value::UNDEFINED);
        }
    }
    let lenk = st.intern_name("length");
    raw_set_prop(st, oi, lenk, Value::int(attrs.len() as i32));
    let mut items = Vec::with_capacity(attrs.len());
    for (an, av) in &attrs {
        let item = new_plain_object(st);
        let ii = item.index() as usize;
        let nk = st.intern_name("name");
        let nv = push_str(st, an.clone());
        raw_set_prop(st, ii, nk, nv);
        let vk = st.intern_name("value");
        let vv = push_str(st, av.clone());
        raw_set_prop(st, ii, vk, vv);
        let ek = st.intern_name("expando");
        raw_set_prop(st, ii, ek, Value::boolean(false));
        let sk = st.intern_name("specified");
        raw_set_prop(st, ii, sk, Value::boolean(true));
        let ok = st.intern_name("ownerElement");
        raw_set_prop(st, ii, ok, Value::dom_node(node));
        let kk = st.intern_name(an);
        raw_set_prop(st, oi, kk, item);
        items.push(item);
    }
    st.objects[oi].elems = items;
}

/// Can this value be registered as an event listener?
///
/// The EventListener interface is a *callback interface*: a plain
/// object with a `handleEvent` method is as valid as a function, and
/// class-based SDKs use it constantly (`window.addEventListener(
/// 'message', this)` with a `handleEvent(e)` method). Dropping those
/// silently cost naver its entire ad pipeline -- the SafeFrame host
/// listens exactly that way, so every `sf-loaded` and `sf-resized` the
/// child posted arrived at a window with no listener on it, and every
/// ad slot stayed at the zero height it was created with.
fn is_event_listener(v: Value) -> bool {
    v.is_function() || v.is_object()
}

/// Invoke a registered listener. Per spec the `handleEvent` lookup
/// happens at dispatch time, not registration, and the object itself
/// is the `this` for that call.
pub(super) fn call_listener(
    st: &mut St,
    mods: &ModStore,
    cb: Value,
    this: Option<Value>,
    args: &[Value],
) -> Result<Value, VmError> {
    if cb.is_function() {
        return call_value_this(st, mods, cb, this, args);
    }
    if cb.is_object() {
        let k = st.intern_name("handleEvent");
        if let Some(h) = raw_get_prop(st, cb.index() as usize, k) {
            if h.is_function() {
                return call_value_this(st, mods, h, Some(cb), args);
            }
        }
    }
    Ok(Value::UNDEFINED)
}

fn dom_method(
    st: &mut St,
    mods: &ModStore,
    key: u32,
    node: u32,
    args_base: usize,
    argc: u8,
) -> Result<Value, VmError> {
    let _ = mods; // only event dispatch re-enters JS
    // A script that stored its own function over a DOM method owns
    // that name from then on -- polyfills and wrappers
    // (`document.addEventListener = patched`) are built on it. The
    // read path already prefers expandos; the call path must agree
    // or the wrapper is read back but never invoked.
    if let Some(&f) = st.dom_expando.get(&(node, key)) {
        if f.is_function() {
            let args: Vec<Value> = (0..argc as usize)
                .map(|k| st.regs[args_base + k])
                .collect();
            return call_value_this(
                st, mods, f, Some(Value::dom_node(node)), &args,
            );
        }
    }
    let ids = st.ids;
    let doc = need_doc(st)?;
    if node == DOC_NODE {
        match st.names[key as usize].clone().as_str() {
            // A document still parsing is only "reopened" in the sense
            // that the write below continues it; wiping it here would
            // throw away the very script that called open(). After
            // load, open() really does clear the document.
            "open" => {
                if st.ready_state == "complete" {
                    if let Some(body) = find_tag(&doc.borrow(), "body") {
                        doc.borrow_mut().set_text_content(body, "");
                    }
                    st.doc_write_at = None;
                }
                return Ok(Value::dom_node(DOC_NODE));
            }
            "close" => return Ok(Value::UNDEFINED),
            m @ ("write" | "writeln") => {
                let mut markup = (0..argc as usize)
                    .map(|k| to_display(st, st.regs[args_base + k]))
                    .collect::<Vec<_>>()
                    .join("");
                if m == "writeln" {
                    markup.push('\n');
                }
                doc_write_into(st, &doc, &markup);
                return Ok(Value::UNDEFINED);
            }
            _ => {}
        }
    }
    // addEventListener works on document too (listeners are keyed by
    // (node, type); DOC_NODE is just another key)
    if key == ids.add_event_listener {
        let ty = arg_string(st, args_base, argc, 0)?.to_lowercase();
        let handler = if argc >= 2 {
            st.regs[args_base + 1]
        } else {
            Value::UNDEFINED
        };
        if is_event_listener(handler) {
            st.listeners.entry((node, ty)).or_default().push(handler);
        }
        return Ok(Value::UNDEFINED);
    }
    // IE-legacy pair: attachEvent("onload", fn) — the "on" prefix maps
    // onto the modern listener table so old jQuery paths survive
    {
        let nm = st.names[key as usize].as_str();
        if nm == "attachEvent" || nm == "detachEvent" {
            let add = nm == "attachEvent";
            let raw = arg_string(st, args_base, argc, 0)?.to_lowercase();
            let ty = raw.strip_prefix("on").unwrap_or(&raw).to_string();
            let handler = if argc >= 2 {
                st.regs[args_base + 1]
            } else {
                Value::UNDEFINED
            };
            if add {
                if handler.is_function() {
                    st.listeners
                        .entry((node, ty))
                        .or_default()
                        .push(handler);
                }
            } else if let Some(v) = st.listeners.get_mut(&(node, ty)) {
                v.retain(|&h| h != handler);
            }
            return Ok(Value::UNDEFINED);
        }
    }
    // primitive conversion (core-js ordinaryToPrimitive calls these)
    {
        let nm = st.names[key as usize].as_str();
        if nm == "toString" {
            let s = if node == DOC_NODE {
                "[object HTMLDocument]".to_string()
            } else if doc.borrow().nodes[node as usize].is_element() {
                "[object HTMLElement]".to_string()
            } else {
                "[object Text]".to_string()
            };
            return Ok(push_str(st, s));
        }
        if nm == "valueOf" {
            return Ok(Value::dom_node(node));
        }
    }
    // shared by document and elements: descendant collection by tag,
    // class (jQuery fast paths) or name (React 19 dedupes hoistable
    // resources through document.getElementsByName)
    {
        let by_tag = st.names[key as usize] == "getElementsByTagName";
        let by_name = st.names[key as usize] == "getElementsByName";
        if by_tag
            || by_name
            || st.names[key as usize] == "getElementsByClassName"
        {
            let needle = arg_string(st, args_base, argc, 0)?;
            let tag = needle.to_ascii_lowercase();
            let d = doc.borrow();
            let start = if node == DOC_NODE {
                d.root
            } else {
                node as usize
            };
            let mut out = Vec::new();
            // document.getElementsByTagName must include the document
            // element itself (Naver calls getElementsByTagName('html')
            // [0] to get <html>); the element-receiver form is
            // descendants-only per spec.
            let mut stack: Vec<usize> = if node == DOC_NODE {
                vec![start]
            } else {
                d.nodes[start].children.iter().rev().copied().collect()
            };
            while let Some(i) = stack.pop() {
                let hit = if by_tag {
                    (tag == "*" && d.nodes[i].is_element())
                        || d.nodes[i].tag.as_deref() == Some(tag.as_str())
                } else if by_name {
                    // name matches case-sensitively, per spec
                    d.nodes[i].attr("name") == Some(needle.as_str())
                } else {
                    d.nodes[i]
                        .attr("class")
                        .map(|c| {
                            c.split_whitespace().any(|w| w == needle)
                        })
                        .unwrap_or(false)
                };
                if hit {
                    out.push(Value::dom_node(i as u32));
                }
                for &c in d.nodes[i].children.iter().rev() {
                    stack.push(c);
                }
            }
            drop(d);
            return Ok(new_array(st, out));
        }
    }
    if node == DOC_NODE {
        if key == ids.get_element_by_id {
            let id = arg_string(st, args_base, argc, 0)?;
            return Ok(match doc.borrow().get_element_by_id(&id) {
                Some(i) => Value::dom_node(i as u32),
                None => Value::NULL,
            });
        }
        if key == ids.create_element {
            let tag = arg_string(st, args_base, argc, 0)?
                .to_ascii_lowercase();
            let mut d = doc.borrow_mut();
            let is_script = tag == "script";
            let idx = d.new_element(tag, Vec::new(), None);
            if is_script {
                d.script_created_dynamically.insert(idx);
            }
            return Ok(Value::dom_node(idx as u32));
        }
        // createElementNS(ns, tag): our DOM has no namespaces, so the
        // element is created by its local name (arg 1). React uses this
        // for every SVG node — without it the SVG stateNode is undefined
        // and the commit phase throws '.classList of undefined'.
        if st.names[key as usize] == "createElementNS" {
            let tag = arg_string(st, args_base, argc, 1)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let tag = if tag.is_empty() { "div".to_string() } else { tag };
            let mut d = doc.borrow_mut();
            let is_script = tag == "script";
            let idx = d.new_element(tag, Vec::new(), None);
            if is_script {
                d.script_created_dynamically.insert(idx);
            }
            return Ok(Value::dom_node(idx as u32));
        }
        match st.names[key as usize].as_str() {
            // a headless render is always the focused document as far
            // as scripts are concerned; ad SDKs gate their viewability
            // beacons on this and throw when it is missing
            "hasFocus" => return Ok(Value::TRUE),
            "createTextNode" | "createComment" => {
                let text = if argc > 0 {
                    arg_string(st, args_base, argc, 0)?
                } else {
                    String::new()
                };
                let idx = {
                    let mut d = doc.borrow_mut();
                    let root = d.root;
                    let i = d.new_text(text, root);
                    d.detach(i);
                    i
                };
                return Ok(Value::dom_node(idx as u32));
            }
            "createDocumentFragment" => {
                // a detached element works as a fragment in our model
                let idx = doc.borrow_mut().new_element(
                    "#fragment".to_string(),
                    Vec::new(),
                    None,
                );
                return Ok(Value::dom_node(idx as u32));
            }
            _ => {}
        }
        if key == ids.query_selector || key == ids.query_selector_all {
            let sel = arg_string(st, args_base, argc, 0)?;
            let first = key == ids.query_selector;
            let found = query(&doc.borrow(), &sel, first);
            if first {
                return Ok(match found.first() {
                    Some(&i) => Value::dom_node(i as u32),
                    None => Value::NULL,
                });
            }
            let elems =
                found.iter().map(|&i| Value::dom_node(i as u32)).collect();
            return Ok(new_array(st, elems));
        }
    } else {
        let node_us = node as usize;
        if key == ids.query_selector || key == ids.query_selector_all {
            // element-scoped: document-wide query filtered to this
            // subtree (jQuery's context.querySelectorAll path)
            let sel = arg_string(st, args_base, argc, 0)?;
            let first = key == ids.query_selector;
            let d = doc.borrow();
            let scoped: Vec<usize> = query(&d, &sel, false)
                .into_iter()
                .filter(|&i| {
                    let mut n = i;
                    loop {
                        match d.nodes[n].parent {
                            Some(p) if p == node_us => break true,
                            Some(p) => n = p,
                            None => break false,
                        }
                    }
                })
                .collect();
            drop(d);
            if first {
                return Ok(match scoped.first() {
                    Some(&i) => Value::dom_node(i as u32),
                    None => Value::NULL,
                });
            }
            let elems = scoped
                .iter()
                .map(|&i| Value::dom_node(i as u32))
                .collect();
            return Ok(new_array(st, elems));
        }
        if key == ids.append_child {
            if argc == 0 {
                return err("appendChild needs a DOM node");
            }
            let child = st.regs[args_base];
            if !child.is_dom_node() {
                return err("appendChild needs a DOM node");
            }
            let c = child.index() as usize;
            let mut d = doc.borrow_mut();
            // HierarchyRequestError: inserting a node into itself or a
            // descendant would create a cycle and hang every tree walk.
            let mut anc = Some(node_us);
            while let Some(a) = anc {
                if a == c {
                    return err(
                        "appendChild: new child is an ancestor of parent",
                    );
                }
                anc = d.nodes[a].parent;
            }
            d.detach(c);
            d.nodes[c].parent = Some(node_us);
            d.nodes[node_us].children.push(c);
            return Ok(child);
        }
        if key == ids.remove {
            doc.borrow_mut().detach(node_us);
            return Ok(Value::UNDEFINED);
        }
        if key == ids.set_attribute {
            let name = arg_string(st, args_base, argc, 0)?;
            let value = arg_string(st, args_base, argc, 1)?;
            doc.borrow_mut().set_attr(node_us, &name, &value);
            return Ok(Value::UNDEFINED);
        }
        if key == ids.get_attribute {
            let name = arg_string(st, args_base, argc, 0)?;
            let out = doc.borrow().nodes[node_us]
                .attr(&name)
                .map(str::to_string);
            return Ok(match out {
                Some(s) => push_str(st, s),
                None => Value::NULL,
            });
        }
        if key == ids.remove_attribute {
            let name = arg_string(st, args_base, argc, 0)?;
            doc.borrow_mut().remove_attr(node_us, &name);
            if let Some(&map) = st.attr_maps.get(&node) {
                refresh_attr_map(st, &doc, node, map);
            }
            return Ok(Value::UNDEFINED);
        }
        if key == ids.add_event_listener {
            let ty = arg_string(st, args_base, argc, 0)?.to_lowercase();
            let handler = if argc >= 2 {
                st.regs[args_base + 1]
            } else {
                Value::UNDEFINED
            };
            if is_event_listener(handler) {
                st.listeners.entry((node, ty)).or_default().push(handler);
            }
            return Ok(Value::UNDEFINED);
        }
    }
    // Layout-dependent and no-op-ish element methods. Layout runs in
    // Python *after* JS, so geometry reads return a zero-rect here
    // (enough for defensive scripts not to throw); focus/blur/scroll
    // are accepted and ignored.
    match st.names[key as usize].as_str() {
        "getBoundingClientRect" | "getClientRects" => {
            // real geometry when the shell has pushed layout rects
            // (document-origin; zero-rect before the first layout)
            let (x, y, w, h) = st
                .layout_rects
                .get(&node)
                .copied()
                .unwrap_or((0.0, 0.0, 0.0, 0.0));
            let rect = new_plain_object(st);
            let ri = rect.index() as usize;
            for (field, v) in [
                ("x", x),
                ("left", x),
                ("y", y),
                ("top", y),
                ("width", w),
                ("height", h),
                ("right", x + w),
                ("bottom", y + h),
            ] {
                let fk = st.intern_name(field);
                raw_set_prop(st, ri, fk, Value::number(v));
            }
            return Ok(rect);
        }
        "scrollTo" | "scrollBy" | "scroll" => {
            // scrollTo(x, y) | scrollTo({left, top, behavior}); scroll
            // is an alias of scrollTo, scrollBy is relative. The host
            // applies and clamps these against the real scroller.
            let name = st.names[key as usize].clone();
            let a0 = if argc > 0 { st.regs[args_base] } else { Value::UNDEFINED };
            let a1 = if argc > 1 { st.regs[args_base + 1] } else { Value::UNDEFINED };
            let (left, top) = scroll_args(st, a0, a1);
            queue_scroll(st, node, left, top, name == "scrollBy");
            return Ok(Value::UNDEFINED);
        }
        "scrollIntoView" => {
            // resolving this needs layout (which ancestors scroll, and
            // by how much), so it becomes a host request
            st.scroll_seq += 1;
            let seq = st.scroll_seq;
            st.scroll_into_view.push((node, seq));
            return Ok(Value::UNDEFINED);
        }
        "focus" | "blur" | "setAttributeNS" | "closest" => {
            return Ok(Value::UNDEFINED);
        }
        "getContext" => {
            // canvas-2d stub context: every drawing call is accepted
            // and ignored (no rasterizing); readbacks return zeros.
            // Non-2d kinds (webgl...) answer null so feature detection
            // takes its unsupported path honestly.
            let kind = arg_string(st, args_base, argc, 0)?
                .to_lowercase();
            if kind != "2d" {
                return Ok(Value::NULL);
            }
            let ctx = new_plain_object(st);
            let ci = ctx.index() as usize;
            for m in ["fillRect", "clearRect", "strokeRect",
                      "beginPath", "closePath", "moveTo", "lineTo",
                      "bezierCurveTo", "quadraticCurveTo", "arc",
                      "arcTo", "ellipse", "rect", "fill", "stroke",
                      "clip", "save", "restore", "translate", "scale",
                      "rotate", "transform", "setTransform",
                      "resetTransform", "drawImage", "fillText",
                      "strokeText", "putImageData", "setLineDash"] {
                let k = st.intern_name(m);
                let f = make_native(st, Native::Noop);
                raw_set_prop(st, ci, k, f);
            }
            for (m, id) in [
                ("measureText", host::CV_MEASURE_TEXT),
                ("getImageData", host::CV_IMAGE_DATA),
                ("createImageData", host::CV_IMAGE_DATA),
                ("createLinearGradient", host::CV_GRADIENT),
                ("createRadialGradient", host::CV_GRADIENT),
                ("createPattern", host::CV_GRADIENT),
            ] {
                let k = st.intern_name(m);
                let f = make_native(st, Native::HostFn(id));
                raw_set_prop(st, ci, k, f);
            }
            let ck = st.intern_name("canvas");
            raw_set_prop(st, ci, ck, Value::dom_node(node));
            for p in ["lineWidth", "globalAlpha"] {
                let k = st.intern_name(p);
                raw_set_prop(st, ci, k, Value::int(1));
            }
            for p in ["fillStyle", "strokeStyle", "font", "textAlign",
                      "textBaseline", "globalCompositeOperation"] {
                let k = st.intern_name(p);
                let empty = intern(st, "");
                raw_set_prop(st, ci, k, empty);
            }
            return Ok(ctx);
        }
        "toDataURL" => {
            // empty image, the smallest legal data URL
            return Ok(intern(st, "data:,"));
        }
        "removeEventListener" => {
            let ty = arg_string(st, args_base, argc, 0)?.to_lowercase();
            let handler = if argc >= 2 {
                st.regs[args_base + 1]
            } else {
                Value::UNDEFINED
            };
            if let Some(v) = st.listeners.get_mut(&(node, ty)) {
                v.retain(|&h| h != handler);
            }
            return Ok(Value::UNDEFINED);
        }
        "insertBefore" => {
            let newn = st.regs[args_base];
            let refn = if argc > 1 {
                st.regs[args_base + 1]
            } else {
                Value::NULL
            };
            if !newn.is_dom_node() {
                return type_err("insertBefore: not a node");
            }
            let ni = newn.index() as usize;
            let mut d = doc.borrow_mut();
            d.detach(ni);
            let pu = node as usize;
            let pos = if refn.is_dom_node() {
                d.nodes[pu]
                    .children
                    .iter()
                    .position(|&c| c == refn.index() as usize)
            } else {
                None
            };
            match pos {
                Some(i) => d.nodes[pu].children.insert(i, ni),
                None => d.nodes[pu].children.push(ni),
            }
            d.nodes[ni].parent = Some(pu);
            return Ok(newn);
        }
        "removeChild" => {
            let child = st.regs[args_base];
            if child.is_dom_node() {
                doc.borrow_mut().detach(child.index() as usize);
            }
            return Ok(child);
        }
        "replaceChild" => {
            let newn = st.regs[args_base];
            let oldn = if argc > 1 {
                st.regs[args_base + 1]
            } else {
                Value::UNDEFINED
            };
            if !newn.is_dom_node() || !oldn.is_dom_node() {
                return type_err("replaceChild: not a node");
            }
            let (ni, oi_) = (newn.index() as usize,
                             oldn.index() as usize);
            let mut d = doc.borrow_mut();
            d.detach(ni);
            let pu = node as usize;
            if let Some(i) =
                d.nodes[pu].children.iter().position(|&c| c == oi_)
            {
                d.nodes[pu].children[i] = ni;
                d.nodes[ni].parent = Some(pu);
                d.nodes[oi_].parent = None;
            }
            return Ok(oldn);
        }
        // No shadow DOM here, so a node's root is always the
        // document. React's float/resource layer asks the mount
        // container for its root -- and Next.js's app router mounts
        // *on* `document`, whose `ownerDocument` is null by spec, so
        // the missing method left it with no resource root at all
        // ("resourceRoot was expected to exist", its error #446).
        "getRootNode" => return Ok(Value::dom_node(DOC_NODE)),
        // React's hydration diff walks `element.attributes` and drops
        // the ones the server sent that the client did not, through
        // the Attr node -- not by name.
        "getAttributeNode" => {
            let name = arg_string(st, args_base, argc, 0)?.to_lowercase();
            let found = doc.borrow().nodes[node as usize]
                .attr(&name)
                .map(|v| v.to_string());
            return Ok(match found {
                Some(v) => {
                    let obj = new_plain_object(st);
                    let oi = obj.index() as usize;
                    let nk = st.intern_name("name");
                    let nv = push_str(st, name);
                    raw_set_prop(st, oi, nk, nv);
                    let vk = st.intern_name("value");
                    let vv = push_str(st, v);
                    raw_set_prop(st, oi, vk, vv);
                    let ok = st.intern_name("ownerElement");
                    raw_set_prop(st, oi, ok, Value::dom_node(node));
                    obj
                }
                None => Value::NULL,
            });
        }
        "removeAttributeNode" => {
            let attr = st.regs[args_base];
            let name = if attr.is_object() {
                let nk = st.intern_name("name");
                raw_get_prop(st, attr.index() as usize, nk)
                    .map(|v| to_display(st, v))
                    .unwrap_or_default()
            } else {
                to_display(st, attr)
            };
            doc.borrow_mut().nodes[node as usize]
                .attrs
                .retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
            // liveness: a captured `el.attributes` must see the removal
            if let Some(&map) = st.attr_maps.get(&node) {
                refresh_attr_map(st, &doc, node, map);
            }
            return Ok(attr);
        }
        "hasAttributes" => {
            let any = !doc.borrow().nodes[node as usize].attrs.is_empty();
            return Ok(Value::boolean(any));
        }
        // Shadow-less shadow DOM: the "root" is the host element
        // itself, so whatever the component mounts lands in the light
        // tree and renders. Style scoping is lost -- acceptable
        // degradation next to not rendering at all (recoshopping's
        // design-system web component died here mid-hydration).
        "attachShadow" => {
            let host = Value::dom_node(node);
            let hk = st.intern_name("host");
            st.dom_expando.insert((node, hk), host);
            let mk = st.intern_name("mode");
            let mv = push_str(st, "open".to_string());
            st.dom_expando.insert((node, mk), mv);
            let sk = st.intern_name("shadowRoot");
            st.dom_expando.insert((node, sk), host);
            return Ok(host);
        }
        "contains" => {
            let other = st.regs[args_base];
            if !other.is_dom_node() {
                return Ok(Value::boolean(false));
            }
            let d = doc.borrow();
            let mut cur = Some(other.index() as usize);
            while let Some(c) = cur {
                if c == node as usize {
                    return Ok(Value::boolean(true));
                }
                cur = d.nodes[c].parent;
            }
            return Ok(Value::boolean(false));
        }
        "cloneNode" => {
            let deep = argc > 0 && truthy(st, st.regs[args_base]);
            let mut d = doc.borrow_mut();
            fn clone_rec(
                d: &mut crate::dom::Document,
                src: usize,
                parent: Option<usize>,
                deep: bool,
            ) -> usize {
                let (tag, attrs, text, kids) = {
                    let n = &d.nodes[src];
                    (n.tag.clone(), n.attrs.clone(), n.text.clone(),
                     n.children.clone())
                };
                let idx = match tag {
                    Some(t) => d.new_element(t, attrs, parent),
                    None => {
                        let p = parent.unwrap_or(d.root);
                        d.new_text(text, p)
                    }
                };
                if deep {
                    for k in kids {
                        clone_rec(d, k, Some(idx), true);
                    }
                }
                idx
            }
            let idx = clone_rec(&mut d, node as usize, None, deep);
            // a fresh clone is detached
            d.detach(idx);
            return Ok(Value::dom_node(idx as u32));
        }
        "dispatchEvent" => {
            let evt = st.regs[args_base];
            if !evt.is_object() {
                return type_err("dispatchEvent: not an event");
            }
            let ei = evt.index() as usize;
            let tyk = st.intern_name("type");
            let ty = match raw_get_prop(st, ei, tyk) {
                Some(v) => to_display(st, v).to_lowercase(),
                None => return type_err("event has no type"),
            };
            let tgt = st.intern_name("target");
            let curk = st.intern_name("currentTarget");
            let node_v = Value::dom_node(node);
            raw_set_prop(st, ei, tgt, node_v);
            let bubk = st.intern_name("bubbles");
            let bubbles = raw_get_prop(st, ei, bubk)
                .map(|v| truthy(st, v))
                .unwrap_or(false);
            let stopk = st.intern_name("__stopped");
            let mut cur = Some(node as usize);
            while let Some(c) = cur {
                raw_set_prop(
                    st, ei, curk, Value::dom_node(c as u32));
                let cbs = st
                    .listeners
                    .get(&(c as u32, ty.clone()))
                    .cloned()
                    .unwrap_or_default();
                for cb in cbs {
                    call_listener(
                        st, mods, cb,
                        Some(Value::dom_node(c as u32)), &[evt],
                    )?;
                    if raw_get_prop(st, ei, stopk)
                        .map(|v| truthy(st, v))
                        .unwrap_or(false)
                    {
                        cur = None;
                        break;
                    }
                }
                if !bubbles || cur.is_none() {
                    break;
                }
                cur = doc.borrow().nodes[c].parent;
            }
            let dpk = st.intern_name("defaultPrevented");
            let dp = raw_get_prop(st, ei, dpk)
                .map(|v| truthy(st, v))
                .unwrap_or(false);
            return Ok(Value::boolean(!dp));
        }
        _ => {}
    }
    err(format!(
        "unsupported DOM method .{}() on node {node}",
        st.names[key as usize]
    ))
}

fn dom_get_prop(st: &mut St, key: u32, node: u32) -> Result<Value, VmError> {
    // expandos (own properties scripts stored) win over everything
    if let Some(&v) = st.dom_expando.get(&(node, key)) {
        return Ok(v);
    }
    let ids = st.ids;
    let doc = need_doc(st)?;
    if node == DOC_NODE {
        if key == ids.body {
            return Ok(match find_tag(&doc.borrow(), "body") {
                Some(i) => Value::dom_node(i as u32),
                None => Value::NULL,
            });
        }
        if key == ids.title {
            let text = {
                let d = doc.borrow();
                find_tag(&d, "title")
                    .map(|t| d.collect_text(t))
                    .unwrap_or_default()
            };
            return Ok(push_str(st, text));
        }
        if st.names[key as usize] == "cookie" {
            let joined = st
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Ok(push_str(st, joined));
        }
        if st.names[key as usize] == "readyState" {
            let rs = st.ready_state;
            return Ok(push_str(st, rs.to_string()));
        }
        match st.names[key as usize].as_str() {
            // null (not undefined) when nothing is executing, which
            // is what a page's `currentScript || fallback` relies on
            "currentScript" => {
                return Ok(match st.current_script {
                    Some(i) => Value::dom_node(i),
                    None => Value::NULL,
                });
            }
            "documentElement" | "head" => {
                let tag = if st.names[key as usize] == "head" {
                    "head"
                } else {
                    "html"
                };
                return Ok(match find_tag(&doc.borrow(), tag) {
                    Some(i) => Value::dom_node(i as u32),
                    None => Value::NULL,
                });
            }
            // React reads document.activeElement before every commit
            // (getActiveElementDeep). We have no real focus model, so
            // report body — a non-null Element, which is what React's
            // guard `activeElement !== body` needs to short-circuit.
            "activeElement" => {
                return Ok(match find_tag(&doc.borrow(), "body") {
                    Some(i) => Value::dom_node(i as u32),
                    None => Value::NULL,
                });
            }
            "implementation" => {
                // jQuery's parseHTML support probe; created nodes are
                // detached until appended, so the real document serves
                let obj = new_plain_object(st);
                let k = st.intern_name("createHTMLDocument");
                let f = make_native(st, Native::ReturnDoc);
                raw_set_prop(st, obj.index() as usize, k, f);
                return Ok(obj);
            }
            "ownerDocument" => return Ok(Value::NULL),
            "defaultView" | "parentWindow" => return Ok(st.known.window),
            "nodeType" => return Ok(Value::int(9)),
            _ => {}
        }
        if is_dom_method_name(st.names[key as usize].as_str()) {
            return Ok(memo_native(st, 2, node, key, Native::DomMethod { node, key }));
        }
        return Ok(Value::UNDEFINED);
    }
    let node_us = node as usize;
    match st.names[key as usize].as_str() {
        "async" => {
            let d = doc.borrow();
            let value = d.script_async_overrides.get(&node_us).copied()
                .unwrap_or_else(|| {
                    d.script_created_dynamically.contains(&node_us)
                        || d.nodes[node_us].attr("async").is_some()
                });
            return Ok(Value::boolean(value));
        }
        "defer" => {
            return Ok(Value::boolean(
                doc.borrow().nodes[node_us].attr("defer").is_some()));
        }
        "text" if doc.borrow().nodes[node_us].tag.as_deref()
            == Some("script") =>
        {
            let text = doc.borrow().collect_text(node_us);
            return Ok(push_str(st, text));
        }
        _ => {}
    }
    if key == ids.text_content {
        let text = doc.borrow().collect_text(node_us);
        return Ok(push_str(st, text));
    }
    if key == ids.inner_html {
        let html = serialize_children(&doc.borrow(), node_us);
        return Ok(push_str(st, html));
    }
    if key == ids.id || key == ids.class_name {
        let attr = if key == ids.id { "id" } else { "class" };
        let out = doc.borrow().nodes[node_us]
            .attr(attr)
            .unwrap_or("")
            .to_string();
        return Ok(push_str(st, out));
    }
    match st.names[key as usize].as_str() {
        "contentWindow" => {
            // Only a context the host published is reachable, and even
            // then JS gets a proxy, never the child's real window.
            return Ok(match st.frame_ctx.get(&node) {
                Some(&(handle, _)) => window_proxy(st, handle),
                None => Value::NULL,
            });
        }
        "contentDocument" | "contentWindowDocument" => {
            // Only frames the host judged same-origin ever get a
            // mirror pushed, so this is null for everything else.
            if let Some(&d) = st.frame_docs.get(&node) {
                return Ok(d);
            }
            // ...but a *script-created* iframe has no child document
            // at all yet, and `contentDocument.write(html)` is how ad
            // SDKs (and every "render into a frame" helper) fill one.
            // Hand back something writable; the host turns a closed
            // buffer into the real child document.
            // ...but only for a frame that has no source of its own.
            // A frame with a `src` is the host's to judge: if it were
            // same-origin a mirror would already be here, so handing
            // back a writable document would be a cross-origin leak.
            if st.frame_host_owned.contains(&node) {
                return Ok(Value::NULL);
            }
            let writable = {
                let d = doc.borrow();
                let n = &d.nodes[node as usize];
                let is_frame = n
                    .tag
                    .as_deref()
                    .is_some_and(|t| t == "iframe" || t == "frame");
                let src = n.attr("src").unwrap_or("").trim().to_lowercase();
                is_frame
                    && (src.is_empty() || src == "about:blank")
                    && n.attr("srcdoc").is_none()
            };
            if !writable {
                return Ok(Value::NULL);
            }
            let obj = new_plain_object(st);
            let oi = obj.index() as usize;
            for (m, op) in [("open", 0u8), ("write", 1), ("writeln", 2),
                            ("close", 3)] {
                let k = st.intern_name(m);
                let f = make_native(st, Native::DocWrite { node, op });
                raw_set_prop(st, oi, k, f);
            }
            let k = st.intern_name("nodeType");
            raw_set_prop(st, oi, k, Value::int(9));
            for name in ["documentElement", "body", "head", "defaultView"] {
                let k = st.intern_name(name);
                raw_set_prop(st, oi, k, Value::NULL);
            }
            return Ok(obj);
        }
        "classList" => {
            // a fresh object whose methods carry the node id
            let obj = new_plain_object(st);
            let oi = obj.index() as usize;
            for (m, op) in [("add", 0u8), ("remove", 1),
                            ("contains", 2), ("toggle", 3)] {
                let mk = st.intern_name(m);
                let f = make_native(st, Native::ClassList { node, op });
                raw_set_prop(st, oi, mk, f);
            }
            return Ok(obj);
        }
        "style" => {
            // proxy: property access routes to the style attribute
            let obj = new_plain_object(st);
            st.style_nodes.insert(obj.index(), node);
            return Ok(obj);
        }
        "dataset" => {
            let obj = new_plain_object(st);
            st.dataset_nodes.insert(obj.index(), node);
            return Ok(obj);
        }
        "parentNode" | "parentElement" => {
            let is_parent_node = st.names[key as usize] == "parentNode";
            let d = doc.borrow();
            return Ok(match d.nodes[node_us].parent {
                Some(p) => Value::dom_node(p as u32),
                // documentElement.parentNode is the document node (real
                // DOM); parentElement stays null. containsDeep walks
                // parentNode up to the document, so without this the
                // IntersectionObserver polyfill's _rootContainsTarget
                // returns false and lazy content (naver's feed) never
                // sees itself on-screen.
                None if is_parent_node
                    && d.nodes[node_us].tag.as_deref() == Some("html") =>
                {
                    Value::dom_node(DOC_NODE)
                }
                None => Value::NULL,
            });
        }
        "ownerDocument" => {
            return Ok(Value::dom_node(DOC_NODE));
        }
        "tagName" | "nodeName" => {
            let tag = doc.borrow().nodes[node_us]
                .tag
                .clone()
                .unwrap_or_default()
                .to_uppercase();
            return Ok(push_str(st, tag));
        }
        "firstChild" | "lastChild"
        | "firstElementChild" | "lastElementChild" => {
            let nm = st.names[key as usize].as_str();
            let first = nm.starts_with("first");
            // the *Element* pair skips text nodes, and like every other
            // DOM traversal answers null -- not undefined -- when there
            // is nothing there
            let el_only = nm.ends_with("ElementChild");
            let d = doc.borrow();
            let kids = &d.nodes[node_us].children;
            let mut it = kids.iter().filter(|&&c| {
                !el_only || d.nodes[c].is_element()
            });
            let pick = if first { it.next() } else { it.last() };
            return Ok(match pick {
                Some(&c) => Value::dom_node(c as u32),
                None => Value::NULL,
            });
        }
        "nextSibling" | "previousSibling"
        | "nextElementSibling" | "previousElementSibling" => {
            let nm = st.names[key as usize].clone();
            let fwd = nm.starts_with("next");
            let el_only = nm.ends_with("ElementSibling");
            let d = doc.borrow();
            let sib = d.nodes[node_us].parent.and_then(|p| {
                let kids = &d.nodes[p].children;
                let at = kids.iter().position(|&c| c == node_us)?;
                let mut i = at;
                loop {
                    i = if fwd {
                        if i + 1 >= kids.len() {
                            return None;
                        }
                        i + 1
                    } else {
                        if i == 0 {
                            return None;
                        }
                        i - 1
                    };
                    let c = kids[i];
                    if !el_only || d.nodes[c].is_element() {
                        return Some(c);
                    }
                }
            });
            return Ok(match sib {
                Some(c) => Value::dom_node(c as u32),
                None => Value::NULL,
            });
        }
        "defaultValue" => {
            // the initial value attribute (jQuery's clone support test)
            let out = doc.borrow().nodes[node_us]
                .attr("value")
                .unwrap_or("")
                .to_string();
            return Ok(push_str(st, out));
        }
        "href" | "protocol" | "host" | "hostname" | "port"
        | "pathname" | "search" | "hash" | "origin"
            if doc.borrow().nodes[node_us].attr("href").is_some()
                || matches!(
                    doc.borrow().nodes[node_us].tag.as_deref(),
                    Some("a") | Some("area")
                ) =>
        {
            // anchor URL decomposition (jQuery parses URLs by
            // reading a.pathname off a created <a>)
            let raw = doc.borrow().nodes[node_us]
                .attr("href")
                .unwrap_or("")
                .to_string();
            let base = {
                let lockey = st.intern_name("location");
                let loc = st.globals[lockey as usize];
                if loc.is_object() {
                    let hk = st.intern_name("href");
                    raw_get_prop(st, loc.index() as usize, hk)
                        .filter(|v| v.is_string())
                        .map(|v| str_ref(st, v.index()).to_string())
                } else {
                    None
                }
            }
            .unwrap_or_default();
            let abs = resolve_url(&raw, &base);
            let part =
                url_part(&abs, st.names[key as usize].as_str());
            return Ok(push_str(st, part));
        }
        "src" | "value" | "type" | "title" | "alt" | "name" | "rel"
        | "target" | "placeholder" | "content" | "lang" | "dir"
        | "role" | "href" => {
            // attribute-mirror getters (the setter side already
            // routes these to attributes)
            let nm = st.names[key as usize].clone();
            let out = doc.borrow().nodes[node_us]
                .attr(&nm)
                .unwrap_or("")
                .to_string();
            return Ok(push_str(st, out));
        }
        "attributes" => {
            // NamedNodeMap: named + indexed access, each entry an
            // Attr-ish record (jQuery probes .expando). One object per
            // node for its whole life, refreshed in place on every
            // read and every attribute removal -- a real NamedNodeMap
            // is live and identity-stable, and React 19's unmount loop
            // (`for (e = n.attributes; e.length;) n.removeAttribute
            // Node(e[0])`) spins forever on a snapshot whose length
            // can never reach zero. That one loop burned recoshopping's
            // entire 400M-instruction budget mid-hydration.
            let obj = match st.attr_maps.get(&node) {
                Some(&o) => o,
                None => {
                    let o = new_plain_object(st);
                    st.attr_maps.insert(node, o);
                    o
                }
            };
            refresh_attr_map(st, &doc, node, obj);
            return Ok(obj);
        }
        "children" => {
            let kids: Vec<Value> = {
                let d = doc.borrow();
                d.nodes[node_us]
                    .children
                    .iter()
                    .filter(|&&c| d.nodes[c].is_element())
                    .map(|&c| Value::dom_node(c as u32))
                    .collect()
            };
            return Ok(new_array(st, kids));
        }
        "childNodes" => {
            let kids: Vec<Value> = doc.borrow().nodes[node_us]
                .children
                .iter()
                .map(|&c| Value::dom_node(c as u32))
                .collect();
            return Ok(new_array(st, kids));
        }
        "nodeType" => {
            let is_el = doc.borrow().nodes[node_us].is_element();
            return Ok(Value::int(if is_el { 1 } else { 3 }));
        }
        "scrollTop" | "scrollLeft" => {
            // the host feeds real scroll state back after layout, the
            // same way layout_rects feeds getBoundingClientRect
            let top = st.names[key as usize] == "scrollTop";
            let v = st
                .scroll_state
                .get(&node)
                .map(|&(t, l, _, _)| if top { t } else { l })
                .unwrap_or(0.0);
            return Ok(Value::number(v));
        }
        "clientWidth" | "clientHeight" | "offsetWidth" | "offsetHeight"
        | "scrollWidth" | "scrollHeight" | "clientTop" | "clientLeft"
        | "offsetTop" | "offsetLeft" => {
            let name = st.names[key as usize].as_str();
            if name == "clientTop" || name == "clientLeft" {
                return Ok(Value::int(0));
            }
            // scrollWidth/scrollHeight are the CONTENT size, which for a
            // scroll container exceeds its box — the host reports both
            if matches!(name, "scrollWidth" | "scrollHeight") {
                if let Some(&(_, _, sh, sw)) = st.scroll_state.get(&node) {
                    return Ok(Value::int(
                        if name == "scrollHeight" { sh } else { sw } as i32,
                    ));
                }
            }
            let want_h = name.ends_with("Height");
            let want_pos = name == "offsetTop" || name == "offsetLeft";
            // The documentElement (and body) report the layout viewport:
            // the IntersectionObserver polyfill builds its root rect from
            // `documentElement.clientWidth/clientHeight`, and an undefined
            // value collapsed the viewport so nothing ever intersected and
            // lazy content (naver's feed) never rendered.
            let tag = doc.borrow().nodes[node_us].tag.clone();
            // ...except offsetWidth/offsetHeight, which are the border
            // box even on the body. A SafeFrame ad measures itself with
            // `document.body.offsetHeight` and posts that out as the
            // height the host should give the <iframe>; answering the
            // viewport there asks for a 5000px ad slot.
            let is_offset_size =
                !want_pos && matches!(name, "offsetWidth" | "offsetHeight");
            if !want_pos
                && matches!(tag.as_deref(), Some("html") | Some("body"))
                && !(is_offset_size
                    && tag.as_deref() == Some("body")
                    && st.layout_rects.contains_key(&node))
            {
                return Ok(Value::int(if want_h {
                    VIEWPORT_H
                } else {
                    VIEWPORT_W
                }));
            }
            // other elements: their laid-out box, once layout geometry has
            // been fed back via set_layout_rects; zero before first layout
            if let Some(&(x, y, w, h)) = st.layout_rects.get(&node) {
                let v = if want_pos {
                    if name == "offsetTop" { y } else { x }
                } else if want_h {
                    h
                } else {
                    w
                };
                return Ok(Value::int(v as i32));
            }
            return Ok(Value::int(0));
        }
        _ => {}
    }
    if is_dom_method_name(st.names[key as usize].as_str())
        && !is_doc_only_method(st.names[key as usize].as_str())
    {
        return Ok(memo_native(st, 2, node, key, Native::DomMethod { node, key }));
    }
    Ok(Value::UNDEFINED)
}

/// Factory methods only the document owns. Elements must NOT advertise
/// these: jQuery 1.x sniffs `fragment.createElement` truthiness to
/// detect IE8 and takes shim paths when it sees a function.
fn is_doc_only_method(name: &str) -> bool {
    matches!(
        name,
        "createElement"
            | "createElementNS"
            | "createTextNode"
            | "createComment"
            | "createDocumentFragment"
            | "getElementById"
            | "getElementsByName"
    )
}

/// Names dom_method can dispatch. dom_get_prop resolves these to
/// extractable natives, so feature detection (`if (document
/// .addEventListener)`) and uncurrying see real functions instead of
/// undefined. attachEvent/detachEvent stay out: old jQuery must pick
/// the modern branch of its either/or probe.
fn is_dom_method_name(name: &str) -> bool {
    matches!(
        name,
        "addEventListener"
            | "removeEventListener"
            | "dispatchEvent"
            | "getElementById"
            | "createElement"
            | "createElementNS"
            | "createTextNode"
            | "createComment"
            | "createDocumentFragment"
            | "querySelector"
            | "querySelectorAll"
            | "getElementsByTagName"
            | "getElementsByClassName"
            | "getElementsByName"
            | "getAttribute"
            | "setAttribute"
            | "setAttributeNS"
            | "removeAttribute"
            | "appendChild"
            | "insertBefore"
            | "removeChild"
            | "replaceChild"
            | "cloneNode"
            | "contains"
            | "getRootNode"
            | "attachShadow"
            | "getAttributeNode"
            | "removeAttributeNode"
            | "hasAttributes"
            | "closest"
            | "focus"
            | "blur"
            | "scrollIntoView"
            | "scrollTo"
            | "scrollBy"
            | "getBoundingClientRect"
            | "getClientRects"
            | "getContext"
            | "toString"
            | "valueOf"
    )
}

fn dom_set_prop(
    st: &mut St,
    key: u32,
    node: u32,
    v: Value,
) -> Result<(), VmError> {
    let ids = st.ids;
    let doc = need_doc(st)?;
    if node == DOC_NODE {
        if key == ids.title {
            let text = to_display(st, v);
            let mut d = doc.borrow_mut();
            let t = match find_tag(&d, "title") {
                Some(t) => t,
                None => {
                    let head = find_tag(&d, "head").unwrap_or(d.root);
                    d.new_element("title".to_string(), Vec::new(),
                                  Some(head))
                }
            };
            d.nodes[t].children.clear();
            d.new_text(text, t);
            return Ok(());
        }
        if st.names[key as usize] == "cookie" {
            // Keep a same-turn visible copy, but let the Python cookie jar
            // validate and apply Path/Domain/Secure/expiry before the value
            // reaches the network. Invalid control characters and cookie
            // names are ignored here too, matching a browser setter.
            let text = to_display(st, v);
            let first = text.split(';').next().unwrap_or("");
            if let Some((k, val)) = first.split_once('=') {
                let (k, val) = (k.trim().to_string(),
                                val.trim().to_string());
                let bad_name = |ch: char| {
                    ch <= '\u{20}' || ch >= '\u{7f}'
                        || "()<>@,;:\\\"/[]?={}".contains(ch)
                };
                let bad_value = |ch: char| ch < '\u{20}' || ch == '\u{7f}';
                if !k.is_empty()
                    && !k.chars().any(bad_name)
                    && !text.chars().any(bad_value)
                {
                    if let Some(slot) = st
                        .cookies
                        .iter_mut()
                        .find(|(ck, _)| *ck == k)
                    {
                        slot.1 = val;
                    } else {
                        st.cookies.push((k, val));
                    }
                    st.cookie_writes.push(text);
                }
            }
            return Ok(());
        }
        st.dom_expando.insert((node, key), v);
        return Ok(());
    }
    let node_us = node as usize;
    let prop_name = st.names[key as usize].clone();
    if prop_name == "text"
        && doc.borrow().nodes[node_us].tag.as_deref() == Some("script")
    {
        let text = to_display(st, v);
        let mut d = doc.borrow_mut();
        d.nodes[node_us].children.clear();
        d.new_text(text, node_us);
        return Ok(());
    }
    if prop_name == "async" {
        let enabled = truthy(st, v);
        let mut d = doc.borrow_mut();
        d.script_async_overrides.insert(node_us, enabled);
        if enabled {
            d.set_attr(node_us, "async", "");
        } else {
            d.remove_attr(node_us, "async");
        }
        return Ok(());
    }
    if prop_name == "defer" {
        let enabled = truthy(st, v);
        let mut d = doc.borrow_mut();
        if enabled {
            d.set_attr(node_us, "defer", "");
        } else {
            d.remove_attr(node_us, "defer");
        }
        return Ok(());
    }
    if key == ids.text_content {
        let text = to_display(st, v);
        let mut d = doc.borrow_mut();
        d.nodes[node_us].children.clear();
        d.new_text(text, node_us);
        return Ok(());
    }
    if key == ids.inner_html {
        let markup = to_display(st, v);
        let mut d = doc.borrow_mut();
        let frag = html::parse(&markup);
        d.nodes[node_us].children.clear();
        // fragment parser wraps content in implicit html/body
        if let Some(body) = find_tag(&frag, "body") {
            let kids = frag.nodes[body].children.clone();
            for child in kids {
                d.graft(&frag, child, node_us);
            }
        }
        return Ok(());
    }
    if key == ids.id || key == ids.class_name {
        let attr = if key == ids.id { "id" } else { "class" };
        let value = to_display(st, v);
        doc.borrow_mut().set_attr(node_us, attr, &value);
        return Ok(());
    }
    // common element properties map to attributes; on* handlers and
    // unknown-but-plausible props are accepted (expando-style writes
    // land as attributes so re-reads via getAttribute see them)
    let name = st.names[key as usize].clone();
    match name.as_str() {
        "src" | "href" | "value" | "type" | "title" | "alt" | "name"
        | "rel" | "target" | "placeholder" | "content" | "lang"
        | "dir" | "role" | "width" | "height" | "tabIndex" => {
            let value = to_display(st, v);
            let attr = if name == "tabIndex" {
                "tabindex".to_string()
            } else {
                name
            };
            doc.borrow_mut().set_attr(node_us, &attr, &value);
            Ok(())
        }
        n if n.starts_with("on") => {
            st.dom_expando.insert((node, key), v);
            Ok(())
        }
        "nodeValue" | "data" => {
            let value = to_display(st, v);
            let mut d = doc.borrow_mut();
            if !d.nodes[node_us].is_element() {
                d.nodes[node_us].text = value;
                d.version += 1;
            }
            Ok(())
        }
        "scrollTop" | "scrollLeft" => {
            // record the request; the host applies it to the real
            // scroller and feeds the clamped result back. Updating the
            // observable value now keeps a read-after-write consistent
            // within the same turn (`el.scrollTop = el.scrollHeight`
            // then reading it back is a common auto-scroll idiom).
            let want = if v.is_string() {
                str_ref(st, v.index()).trim().parse::<f64>().unwrap_or(0.0)
            } else {
                v.to_number_raw()
            };
            let want = if want.is_finite() { want } else { 0.0 };
            let top = st.names[key as usize] == "scrollTop";
            let entry = st.scroll_state.entry(node).or_insert((
                0.0, 0.0, 0.0, 0.0,
            ));
            if top {
                entry.0 = want.max(0.0);
            } else {
                entry.1 = want.max(0.0);
            }
            let (t, l) = (entry.0, entry.1);
            st.scroll_seq += 1;
            let seq = st.scroll_seq;
            st.scroll_writes.push((node, t, l, seq, false));
            Ok(())
        }
        "selected" | "checked"
        | "disabled" | "hidden" | "draggable"
        | "contentEditable" | "crossOrigin"
        | "charset" | "referrerPolicy" | "integrity"
        | "loading" | "decoding" => Ok(()),
        _ => {
            // real DOM nodes accept arbitrary expandos (jQuery's
            // data cache hangs its id key on the element)
            st.dom_expando.insert((node, key), v);
            Ok(())
        }
    }
}

/// Call a JS function value from native code (sort comparators, DOM
/// event handlers). Runs a nested `exec` to completion.
pub(super) fn call_value(
    st: &mut St,
    mods: &ModStore,
    fv: Value,
    args: &[Value],
) -> Result<Value, VmError> {
    call_value_this(st, mods, fv, None, args)
}

/// Like call_value but with an explicit `this` for regular (non-arrow)
/// functions — backs Function.prototype.call/apply. Arrows ignore it
/// (they keep their captured lexical this).
pub(super) fn call_value_this(
    st: &mut St,
    mods: &ModStore,
    fv: Value,
    this_explicit: Option<Value>,
    args: &[Value],
) -> Result<Value, VmError> {
    if !fv.is_function() {
        return type_err(format!("{fv:?} is not a function"));
    }
    let idx = fv.index();
    if let ClosureRec::Bound { target, this_val, bound } =
        &st.closures[idx as usize]
    {
        let (t, tv) = (*target, *this_val);
        let mut full = bound.clone();
        full.extend_from_slice(args);
        // a nullish bound this yields to the call-site receiver —
        // this is what makes `new (C.bind(null, ...args))` construct
        // real instances (Babel _construct)
        let eff = if tv.is_nullish() {
            this_explicit.unwrap_or(tv)
        } else {
            tv
        };
        return call_value_this(st, mods, t, Some(eff), &full);
    }
    if let ClosureRec::Proxy(id) = st.closures[idx as usize] {
        return proxy_call(
            st,
            mods,
            id,
            this_explicit.unwrap_or(Value::UNDEFINED),
            args,
        );
    }
    match &st.closures[idx as usize] {
        ClosureRec::Bound { .. } => unreachable!("handled above"),
        ClosureRec::Proxy(_) => unreachable!("handled above"),
        ClosureRec::Native(n) => {
            let n = *n;
            if let Native::BrandToString = n {
                // brands the explicit receiver: {}.toString.call(x)
                let recv = this_explicit.unwrap_or(Value::UNDEFINED);
                let s = brand_string(st, recv);
                return Ok(push_str(st, s));
            }
            if let Native::MethodRef(key) = n {
                let recv = this_explicit.unwrap_or(Value::UNDEFINED);
                // extracted call/apply/bind invoked ON a function
                // (core-js uncurry: `FP.apply.call(fn, this, args)`)
                if recv.is_function() {
                    let name = st.names[key as usize].clone();
                    match name.as_str() {
                        "apply" => {
                            let t = args
                                .first()
                                .copied()
                                .unwrap_or(Value::UNDEFINED);
                            let list: Vec<Value> = match args.get(1) {
                                Some(a)
                                    if a.is_object()
                                        && st.objects
                                            [a.index() as usize]
                                            .is_array =>
                                {
                                    st.objects[a.index() as usize]
                                        .elems
                                        .clone()
                                }
                                _ => Vec::new(),
                            };
                            return call_value_this(
                                st, mods, recv, Some(t), &list,
                            );
                        }
                        "call" => {
                            let t = args
                                .first()
                                .copied()
                                .unwrap_or(Value::UNDEFINED);
                            let rest =
                                if args.len() > 1 { &args[1..] } else { &[] };
                            return call_value_this(
                                st, mods, recv, Some(t), rest,
                            );
                        }
                        "bind" => {
                            let t = args
                                .first()
                                .copied()
                                .unwrap_or(Value::UNDEFINED);
                            let bound = if args.len() > 1 {
                                args[1..].to_vec()
                            } else {
                                Vec::new()
                            };
                            st.closures.push(ClosureRec::Bound {
                                target: recv,
                                this_val: t,
                                bound,
                            });
                            return Ok(Value::function(
                                (st.closures.len() - 1) as u32,
                            ));
                        }
                        _ => {}
                    }
                }
                // uncurried tolerance: call/apply/bind reached with a
                // non-function receiver but a function first argument
                // (`c(fn, t, ...)` through any route) — treat args[0]
                // as the target. The strict form would TypeError anyway.
                if !recv.is_function() {
                    let name = st.names[key as usize].clone();
                    if matches!(name.as_str(), "call" | "apply" | "bind")
                    {
                        if let Some(&f0) = args.first() {
                            if f0.is_function() {
                                let t = args
                                    .get(1)
                                    .copied()
                                    .unwrap_or(Value::UNDEFINED);
                                match name.as_str() {
                                    "call" => {
                                        let rest = if args.len() > 2 {
                                            &args[2..]
                                        } else {
                                            &[]
                                        };
                                        return call_value_this(
                                            st, mods, f0, Some(t), rest,
                                        );
                                    }
                                    "apply" => {
                                        let list: Vec<Value> =
                                            match args.get(2) {
                                                Some(a)
                                                    if a.is_object()
                                                        && st.objects[a
                                                            .index()
                                                            as usize]
                                                            .is_array =>
                                                {
                                                    st.objects[a.index()
                                                        as usize]
                                                        .elems
                                                        .clone()
                                                }
                                                _ => Vec::new(),
                                            };
                                        return call_value_this(
                                            st, mods, f0, Some(t), &list,
                                        );
                                    }
                                    _ => {
                                        let bound = if args.len() > 2 {
                                            args[2..].to_vec()
                                        } else {
                                            Vec::new()
                                        };
                                        st.closures.push(
                                            ClosureRec::Bound {
                                                target: f0,
                                                this_val: t,
                                                bound,
                                            },
                                        );
                                        return Ok(Value::function(
                                            (st.closures.len() - 1)
                                                as u32,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                return method_ref_dispatch(st, mods, recv, key, args);
            }
            let top = st.regs.len();
            st.regs.extend_from_slice(args);
            let r = do_native(st, mods, n, top, args.len() as u8);
            st.regs.truncate(top);
            r
        }
        ClosureRec::User { module, proto, this_capture, .. } => {
            // Each native->JS re-entry nests a real Rust `exec` frame;
            // MAX_FRAMES doesn't see those, so cap them separately.
            if st.native_depth >= MAX_NATIVE_DEPTH {
                if trace_enabled() {
                    eprintln!(
                        "[gg-trace] native re-entry cap hit ({} deep)",
                        st.native_depth
                    );
                }
                return err("stack overflow");
            }
            let (m, p) = (*module, *proto);
            // arrows keep lexical this; else the explicit this (call/apply)
            let this0 = this_capture
                .or(this_explicit)
                .unwrap_or(Value::UNDEFINED);
            let (m, p) = ensure_compiled(st, mods, m, p)?;
            if let ClosureRec::User { module, proto, .. } =
                &mut st.closures[idx as usize]
            {
                (*module, *proto) = (m, p);
            }
            let callee_rc = mods.rc(m);
            let callee = &callee_rc.module.protos[p as usize];
            let top = st.regs.len();
            let new_base = top + 1;
            let keep = if callee.uses_arguments {
                args.len().max(callee.nparams as usize)
            } else {
                callee.nparams as usize
            };
            st.regs.resize(
                new_base + (callee.nregs as usize).max(keep),
                Value::UNDEFINED,
            );
            st.regs[top] = fv;
            for (k, a) in args.iter().take(keep).enumerate() {
                st.regs[new_base + k] = *a;
            }
            st.native_depth += 1;
            let val = exec(st, mods, m, p, new_base, idx, this0,
                           args.len().min(255) as u8);
            st.native_depth -= 1;
            st.regs.truncate(top);
            val
        }
    }
}

/// Fallible merge sort (the comparator is JS and may throw).
fn merge_sort(
    st: &mut St,
    mods: &ModStore,
    v: &mut Vec<Value>,
    cmp: Option<Value>,
) -> Result<(), VmError> {
    fn less(
        st: &mut St,
        mods: &ModStore,
        cmp: Option<Value>,
        a: Value,
        b: Value,
    ) -> Result<bool, VmError> {
        match cmp {
            Some(f) => {
                let r = call_value(st, mods, f, &[a, b])?;
                Ok(num_of(r)? <= 0.0)
            }
            None => {
                // default sort() compares by string, like real JS
                let sa = to_display(st, a);
                let sb = to_display(st, b);
                Ok(sa <= sb)
            }
        }
    }
    let n = v.len();
    if n <= 1 {
        return Ok(());
    }
    let mid = n / 2;
    let mut right = v.split_off(mid);
    merge_sort(st, mods, v, cmp)?;
    merge_sort(st, mods, &mut right, cmp)?;
    let left = std::mem::take(v);
    let mut out = Vec::with_capacity(n);
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        if less(st, mods, cmp, left[i], right[j])? {
            out.push(left[i]);
            i += 1;
        } else {
            out.push(right[j]);
            j += 1;
        }
    }
    out.extend_from_slice(&left[i..]);
    out.extend_from_slice(&right[j..]);
    *v = out;
    Ok(())
}

/// Run one activation (function invocation) to completion.
pub(super) fn exec(
    st: &mut St,
    mods: &ModStore,
    mi0: u32,
    pi0: u32,
    base0: usize,
    cl0: u32,
    this0: Value,
    argc0: u8,
) -> Result<Value, VmError> {
    let floor = st.frames.len();
    let hfloor = st.handlers.len();
    let with_floor = st.with_stack.len();
    let (mut mi, mut pi, mut ip) = (mi0, pi0, 0usize);
    let (mut base, mut cl, mut this_v) = (base0, cl0, this0);
    let mut cur_argc = argc0;
    let mut with_base = with_floor;
    loop {
        let r = exec_loop(
            st, mods, mi, pi, ip, base, cl, this_v, floor, with_base,
            cur_argc,
        );
        let mut e = match r {
            Ok(v) => {
                // Handlers armed by this activation must not outlive it
                // (a callback can return from inside `try` at `floor`,
                // where no Return cleanup runs).
                st.handlers.truncate(hfloor);
                st.with_stack.truncate(with_floor);
                return Ok(v);
            }
            Err(e) => e,
        };
        // Snapshot before unwinding: an engine-raised TypeError is
        // materialized at the catch site, by which point the frames
        // that threw are gone. `e.value` set means the page threw its
        // own object, which already carries whatever stack it wants.
        let trace = if e.value.is_none() {
            Some(
                e.trace.clone().unwrap_or_else(|| {
                    stack_string(st, mods, Some(st.cur_site))
                }),
            )
        } else {
            None
        };
        if st.handlers.len() <= hfloor {
            // Carry the frames out with the error. This activation is
            // about to be truncated away, so a caller that reports the
            // error later -- the host printing "[gg-js error] ..." --
            // has no other way to say where it came from.
            if e.trace.is_none() {
                e.trace = trace;
            }
            // Uncaught here. Unwind frames pushed by this activation: a
            // thrown error must not leak frames into the persistent VM
            // (each leak permanently shrinks headroom until every call
            // fails with "stack overflow").
            st.frames.truncate(floor);
            st.handlers.truncate(hfloor);
            st.with_stack.truncate(with_floor);
            return Err(e);
        }
        // Resume at the innermost armed catch with the thrown value.
        let h = st.handlers.pop().unwrap();
        st.frames.truncate(h.depth);
        st.with_stack.truncate(h.with_len);
        with_base = h.with_base;
        let exc = exception_value(st, e, trace);
        st.regs[h.base + h.exc_reg as usize] = exc;
        mi = h.module;
        pi = h.proto;
        ip = h.catch_ip as usize;
        base = h.base;
        cl = h.closure;
        this_v = h.this_val;
        cur_argc = h.argc;
    }
}

/// The JS value a `catch` binds: the thrown value itself, or an
/// Error-like `{name, message}` object for engine-raised errors. The
/// object's [[Prototype]] is wired to the global constructor of the
/// error's kind (TypeError etc.) so `instanceof` and inherited methods
/// behave like a real thrown error.
fn exception_value(
    st: &mut St, e: VmError, trace: Option<String>,
) -> Value {
    if let Some(v) = e.value {
        return v;
    }
    let obj = new_plain_object(st);
    let oi = obj.index() as usize;
    let name_id = st.intern_name("name");
    let msg_id = st.intern_name("message");
    let n = intern(st, e.kind);
    raw_set_prop(st, oi, name_id, n);
    let m = push_str(st, e.msg.clone());
    raw_set_prop(st, oi, msg_id, m);
    if let Some(trace) = trace {
        let stack_id = st.intern_name("stack");
        let s = push_str(st, format!("{}: {}\n{trace}", e.kind, e.msg));
        raw_set_prop(st, oi, stack_id, s);
    }
    let ctor_id = st.intern_name(e.kind);
    if let Some(&ctor) = st.globals.get(ctor_id as usize) {
        if ctor.is_function() {
            let proto = fn_prototype(st, ctor);
            st.objects[oi].proto = proto;
        }
    }
    obj
}

/// Message shown if a thrown value escapes uncaught. Error-like
/// objects print as "name: message", everything else as its display.
fn throw_msg(st: &mut St, mods: &ModStore, v: Value) -> String {
    if v.is_object() {
        let oi = v.index() as usize;
        let name_id = st.intern_name("name");
        let msg_id = st.intern_name("message");
        let name = raw_get_prop(st, oi, name_id);
        let msg = raw_get_prop(st, oi, msg_id);
        // Either half of the pair is enough to call it error-like.
        // Requiring both meant a class that sets only `message` and
        // leaves the label to its prototype's toString reported as
        // "[object Object]" — which is exactly what Test262's own
        // Test262Error does, so a quarter of that corpus came back
        // with no diagnosis at all.
        if name.is_some() || msg.is_some() {
            let n = match name {
                Some(n) => to_display(st, n),
                None => ctor_name(st, mods, oi)
                    .unwrap_or_else(|| "Error".to_string()),
            };
            let m = msg.map(|m| to_display(st, m)).unwrap_or_default();
            // An uncaught error in a third-party bundle is the one
            // report anyone gets; without the frames it names nothing
            // anybody can act on.
            let stack_id = st.intern_name("stack");
            let frames = match raw_get_prop(st, oi, stack_id) {
                Some(sv) if sv.is_string() => {
                    let text = to_display(st, sv);
                    match text.split_once('\n') {
                        Some((_, rest)) if !rest.trim().is_empty() => {
                            format!("\n{rest}")
                        }
                        _ => String::new(),
                    }
                }
                _ => String::new(),
            };
            if m.is_empty() {
                return format!("uncaught {n}{frames}");
            }
            return format!("uncaught {n}: {m}{frames}");
        }
    }
    let d = to_display(st, v);
    format!("uncaught {d}")
}

/// The name of the constructor that made `oi`, for labelling a thrown
/// object that carries a message but no `name`. Property reads only —
/// the throw path must not run user code to report a throw.
fn ctor_name(st: &mut St, mods: &ModStore, oi: usize) -> Option<String> {
    let ctor_id = st.intern_name("constructor");
    let ctor = raw_get_prop(st, oi, ctor_id)?;
    // A function's `name` is computed, not stored in a slot, so the
    // raw property read that found the constructor cannot be used
    // again to read its name.
    if !ctor.is_function() {
        return None;
    }
    let n = fn_name(st, mods, ctor);
    if n.is_empty() { None } else { Some(n) }
}

#[allow(clippy::too_many_arguments)]
fn exec_loop(
    st: &mut St,
    mods: &ModStore,
    mi0: u32,
    pi0: u32,
    ip0: usize,
    base0: usize,
    cl0: u32,
    this0: Value,
    floor: usize,
    with_base0: usize,
    argc0: u8,
) -> Result<Value, VmError> {
    let mut mi = mi0;
    let mut pi = pi0;
    let mut ip = ip0;
    let mut cur_with_base = with_base0;
    let mut base = base0;
    let mut cur_cl = cl0;
    let mut this_v = this0;
    let mut cur_argc = argc0;
    let mut cmod: Rc<LoadedModule> = mods.rc(mi);
    let heap_cap = max_heap_bytes();
    let trace_fn = fn_trace_on();

    macro_rules! reg {
        ($i:expr) => {
            st.regs[base + $i as usize]
        };
    }

    /// module atom -> VM-wide name id
    macro_rules! name {
        ($atom:expr) => {
            cmod.global_map[$atom as usize]
        };
    }

    macro_rules! ic {
        ($ic:expr) => {
            (cmod.ic_base + $ic as u32) as usize
        };
    }

    macro_rules! arith {
        ($dst:expr, $a:expr, $b:expr, $checked:ident, $op:tt) => {{
            let (x, y) = (reg!($a), reg!($b));
            reg!($dst) = if x.is_int() && y.is_int() {
                match x.as_i32().$checked(y.as_i32()) {
                    Some(v) => Value::int(v),
                    None => Value::number(
                        x.as_i32() as f64 $op y.as_i32() as f64,
                    ),
                }
            } else {
                Value::number(to_number(st, mods, x)? $op to_number(st, mods, y)?)
            };
        }};
    }

    macro_rules! cmp {
        ($dst:expr, $a:expr, $b:expr, $op:tt) => {{
            let (x, y) = (reg!($a), reg!($b));
            let r = if x.is_int() && y.is_int() {
                x.as_i32() $op y.as_i32()
            } else if x.is_string() && y.is_string() {
                // both strings: lexicographic, like real JS relational
                let a = str_ref(st, x.index()).to_string();
                let b = str_ref(st, y.index());
                a.as_str() $op b
            } else {
                to_number(st, mods, x)? $op to_number(st, mods, y)?
            };
            reg!($dst) = Value::boolean(r);
        }};
    }

    loop {
        // execution fuel: abort a runaway loop instead of wedging the
        // worker. Shared across the whole call tree via St, so nested
        // calls and pump callbacks all draw from one turn's budget.
        if st.fuel == 0 {
            if std::env::var("GG_JS_TRACE").is_ok() {
                let names: Vec<String> = std::iter::once(
                    cmod.module.protos[pi as usize].name.clone(),
                )
                .chain(st.frames.iter().rev().take(6).map(|f| {
                    mods.rc(f.module).module.protos[f.proto as usize]
                        .name
                        .clone()
                }))
                .collect();
                let code = &cmod.module.protos[pi as usize].code;
                let lo = ip.saturating_sub(30);
                let hi = (ip + 4).min(code.len());
                let dis: Vec<String> = code[lo..hi]
                    .iter()
                    .enumerate()
                    .map(|(k, i)| format!("{}:{:?}", lo + k, i))
                    .collect();
                if let Ok(path) = std::env::var("GG_JS_DUMP") {
                    let _ = std::fs::write(
                        &path,
                        code.iter()
                            .enumerate()
                            .map(|(k, i)| format!("{k}:{i:?}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                return err(format!(
                    "script exceeded its instruction budget \
                     [in {} | mi={} pi={} ip={} nparams={} len={}]\n{}",
                    names.join(" <- "),
                    mi, pi, ip,
                    cmod.module.protos[pi as usize].nparams,
                    code.len(),
                    dis.join("\n")
                ));
            }
            return err("script exceeded its instruction budget");
        }
        // heap backstop: gg has no GC, so string-heap entries live for
        // the whole script run. A challenge/obfuscation loop that mints
        // large strings per iteration (namuwiki's Cloudflare
        // orchestrator) stays under the instruction budget but exhausts
        // RAM and aborts the process on a failed allocation. Trip a
        // catchable RangeError first.
        if st.heap_bytes > heap_cap {
            return range_err("string heap exhausted");
        }
        st.fuel -= 1;
        st.cur_site = (mi, pi);
        // One sample per 16K bytecode instructions keeps the profiler cheap
        // enough to leave compiled in while still producing hundreds of
        // samples for a multi-second application callback.
        if st.profile_enabled && (st.fuel & 0x3fff) == 0 {
            *st.profile_samples.entry((mi, pi)).or_insert(0) += 1;
        }
        let instr = cmod.module.protos[pi as usize].code[ip];
        if trace_fn
            && fn_trace_wanted(&cmod.module.protos[pi as usize].name)
        {
            eprintln!(
                "[gg-fn] m{mi} p{pi} {} ip{ip}: {instr:?}",
                cmod.module.protos[pi as usize].name,
            );
        }
        ip += 1;
        match instr {
            Instr::LoadConst { dst, idx } => {
                reg!(dst) = cmod.module.protos[pi as usize]
                    .consts[idx as usize];
            }
            Instr::LoadInt { dst, val } => reg!(dst) = Value::int(val),
            Instr::LoadUndef { dst } => reg!(dst) = Value::UNDEFINED,
            Instr::LoadBool { dst, val } => {
                reg!(dst) = Value::boolean(val);
            }
            Instr::Move { dst, src } => reg!(dst) = reg!(src),
            Instr::GetGlobal { dst, atom } => {
                let key = name!(atom) as usize;
                if st.with_stack.len() > cur_with_base {
                    if let Some(v) =
                        with_lookup(st, key as u32, cur_with_base)
                    {
                        reg!(dst) = v;
                        continue;
                    }
                }
                if !st.gdef[key] {
                    // `window.X = v; X` — the window object doubles as
                    // the global namespace (core-js installs polyfills
                    // through it)
                    if let Some(v) = window_prop(st, key as u32) {
                        reg!(dst) = v;
                        continue;
                    }
                    return ref_err(format!(
                        "{} is not defined",
                        st.names[key]
                    ));
                }
                reg!(dst) = st.globals[key];
            }
            Instr::GetGlobalSafe { dst, atom } => {
                let key = name!(atom) as usize;
                if st.with_stack.len() > cur_with_base {
                    if let Some(v) =
                        with_lookup(st, key as u32, cur_with_base)
                    {
                        reg!(dst) = v;
                        continue;
                    }
                }
                reg!(dst) = if st.gdef[key] {
                    st.globals[key]
                } else {
                    window_prop(st, key as u32)
                        .unwrap_or(Value::UNDEFINED)
                };
            }
            Instr::TdzCheck { src, atom } => {
                if reg!(src).is_tdz() {
                    let key = name!(atom) as usize;
                    return ref_err(format!(
                        "cannot access '{}' before initialization",
                        st.names[key]
                    ));
                }
            }
            Instr::PushHandler { catch_ip, exc } => {
                st.handlers.push(Handler {
                    depth: st.frames.len(),
                    module: mi,
                    proto: pi,
                    base,
                    closure: cur_cl,
                    this_val: this_v,
                    catch_ip,
                    exc_reg: exc,
                    argc: cur_argc,
                    with_base: cur_with_base,
                    with_len: st.with_stack.len(),
                });
            }
            Instr::PopHandler => {
                st.handlers.pop();
            }
            Instr::WithEnter { obj } => {
                let o = reg!(obj);
                st.with_stack.push(o);
            }
            Instr::WithExit => {
                if st.with_stack.len() > cur_with_base {
                    st.with_stack.pop();
                }
            }
            Instr::Throw { src } => {
                let v = reg!(src);
                let msg = throw_msg(st, mods, v);
                return Err(VmError {
                    msg, value: Some(v), kind: "Error", trace: None,
                });
            }
            Instr::SetGlobal { atom, src } => {
                let key = name!(atom) as usize;
                let v = reg!(src);
                // inside `with (obj)`, a write to a name the object owns
                // updates that object, not the global
                if st.with_stack.len() > cur_with_base {
                    if let Some(obj) =
                        with_target(st, key as u32, cur_with_base)
                    {
                        let oi = obj.index() as usize;
                        raw_set_prop(st, oi, key as u32, v);
                        continue;
                    }
                }
                st.globals[key] = v;
                st.gdef[key] = true;
            }
            Instr::DeclGlobal { atom } => {
                let key = name!(atom) as usize;
                if !st.gdef[key] {
                    st.globals[key] = Value::UNDEFINED;
                    st.gdef[key] = true;
                }
            }
            Instr::Add { dst, a, b } => {
                let (mut x, mut y) = (reg!(a), reg!(b));
                // `+` sees primitives: objects convert first (valueOf/
                // toString), then the string-vs-number split below
                if x.is_object() {
                    x = to_primitive(st, mods, x, true)?;
                }
                if y.is_object() {
                    y = to_primitive(st, mods, y, true)?;
                }
                reg!(dst) = if x.is_int() && y.is_int() {
                    match x.as_i32().checked_add(y.as_i32()) {
                        Some(v) => Value::int(v),
                        None => Value::number(
                            x.as_i32() as f64 + y.as_i32() as f64,
                        ),
                    }
                } else if x.is_string() || y.is_string() {
                    concat(st, x, y)?
                } else {
                    Value::number(to_number(st, mods, x)? + to_number(st, mods, y)?)
                };
            }
            Instr::Sub { dst, a, b } => arith!(dst, a, b, checked_sub, -),
            Instr::Mul { dst, a, b } => arith!(dst, a, b, checked_mul, *),
            Instr::Div { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                reg!(dst) = Value::number(to_number(st, mods, x)? / to_number(st, mods, y)?);
            }
            Instr::Mod { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                reg!(dst) = if x.is_int() && y.is_int() {
                    match x.as_i32().checked_rem(y.as_i32()) {
                        Some(v) => Value::int(v),
                        None => Value::number(f64::NAN),
                    }
                } else {
                    Value::number(to_number(st, mods, x)? % to_number(st, mods, y)?)
                };
            }
            Instr::Pow { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                reg!(dst) =
                    Value::number(to_number(st, mods, x)?.powf(to_number(st, mods, y)?));
            }
            Instr::Neg { dst, src } => {
                let v = reg!(src);
                reg!(dst) = if v.is_int() {
                    match 0i32.checked_sub(v.as_i32()) {
                        Some(n) if v.as_i32() != 0 => Value::int(n),
                        _ => Value::number(-(v.as_i32() as f64)),
                    }
                } else {
                    Value::number(-to_number(st, mods, v)?)
                };
            }
            Instr::ToNum { dst, src } => {
                let v = reg!(src);
                reg!(dst) = Value::number(to_number(st, mods, v)?);
            }
            Instr::Not { dst, src } => {
                reg!(dst) = Value::boolean(!truthy(st, reg!(src)));
            }
            Instr::IncBy { dst, src, delta } => {
                let x = reg!(src);
                reg!(dst) = if x.is_int() {
                    match x.as_i32().checked_add(delta) {
                        Some(v) => Value::int(v),
                        None => Value::number(
                            x.as_i32() as f64 + delta as f64),
                    }
                } else {
                    Value::number(to_number(st, mods, x)? + delta as f64)
                };
            }
            Instr::LtImm { dst, a, imm } => {
                let x = reg!(a);
                let r = if x.is_int() {
                    x.as_i32() < imm
                } else {
                    to_number(st, mods, x)? < imm as f64
                };
                reg!(dst) = Value::boolean(r);
            }
            Instr::Lt { dst, a, b } => cmp!(dst, a, b, <),
            Instr::LtEq { dst, a, b } => cmp!(dst, a, b, <=),
            Instr::Gt { dst, a, b } => cmp!(dst, a, b, >),
            Instr::GtEq { dst, a, b } => cmp!(dst, a, b, >=),
            Instr::StrictEq { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let r = strict_eq(st, x, y);
                reg!(dst) = Value::boolean(r);
            }
            Instr::StrictNotEq { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let r = strict_eq(st, x, y);
                reg!(dst) = Value::boolean(!r);
            }
            Instr::LooseEq { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let r = loose_eq(st, mods, x, y)?;
                reg!(dst) = Value::boolean(r);
            }
            Instr::LooseNotEq { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let r = loose_eq(st, mods, x, y)?;
                reg!(dst) = Value::boolean(!r);
            }
            Instr::Shl { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let (xi, s) = (to_i32(st, mods, x)?, to_u32(st, mods, y)? & 31);
                reg!(dst) = Value::int(xi.wrapping_shl(s));
            }
            Instr::Shr { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let (xi, s) = (to_i32(st, mods, x)?, to_u32(st, mods, y)? & 31);
                reg!(dst) = Value::int(xi.wrapping_shr(s));
            }
            Instr::UShr { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let (xu, s) = (to_u32(st, mods, x)?, to_u32(st, mods, y)? & 31);
                reg!(dst) = Value::number((xu >> s) as f64);
            }
            Instr::BitAnd { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let r = to_i32(st, mods, x)? & to_i32(st, mods, y)?;
                reg!(dst) = Value::int(r);
            }
            Instr::BitOr { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let r = to_i32(st, mods, x)? | to_i32(st, mods, y)?;
                reg!(dst) = Value::int(r);
            }
            Instr::BitXor { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let r = to_i32(st, mods, x)? ^ to_i32(st, mods, y)?;
                reg!(dst) = Value::int(r);
            }
            Instr::BitNot { dst, src } => {
                let v = reg!(src);
                reg!(dst) = Value::int(!to_i32(st, mods, v)?);
            }
            Instr::Arguments { dst } => {
                let vals: Vec<Value> = (0..cur_argc as usize)
                    .map(|k| st.regs[base + k])
                    .collect();
                let arr = new_array(st, vals);
                // sloppy-mode arguments.callee (jindo's Component
                // .extend saves it for re-invocation on subclasses).
                // Deviation: enumerable here, non-enumerable in spec
                let k = st.intern_name("callee");
                raw_set_prop(
                    st, arr.index() as usize, k, Value::function(cur_cl),
                );
                reg!(dst) = arr;
            }
            Instr::LoadSelf { dst } => {
                reg!(dst) = Value::function(cur_cl);
            }
            Instr::NewInstance { dst, ctor } => {
                let mut cv = reg!(ctor);
                // a bound constructor builds instances of its TARGET
                // (Babel _construct: `new (Function.bind.apply(C,...))`)
                for _ in 0..8 {
                    if !cv.is_function() {
                        break;
                    }
                    match &st.closures[cv.index() as usize] {
                        ClosureRec::Bound { target, .. } => cv = *target,
                        _ => break,
                    }
                }
                let obj = new_plain_object(st);
                if cv.is_function() {
                    let proto = fn_prototype(st, cv);
                    st.objects[obj.index() as usize].proto = proto;
                }
                reg!(dst) = obj;
            }
            Instr::SelectObj { dst, a, b } => {
                // `new` yields the constructor's return value when it is an
                // object, else the freshly allocated `this`. DOM nodes are
                // objects too, so a factory constructor like
                // `function Image(){ return document.createElement('img'); }`
                // must return the node, not the empty `this`. Functions are
                // objects as well — `new Function(...)` (and any ctor that
                // returns a closure) must yield the function, not empty this.
                let (x, y) = (reg!(a), reg!(b));
                reg!(dst) = if x.is_object() || x.is_dom_node()
                    || x.is_function()
                {
                    x
                } else {
                    y
                };
            }
            Instr::Construct { ctor, argc } => {
                let constructor = reg!(ctor);
                let args: Vec<Value> = (0..argc as usize)
                    .map(|index| st.regs[base + ctor as usize + 1 + index])
                    .collect();
                reg!(ctor) = construct_value(
                    st, mods, constructor, &args, constructor,
                )?;
            }
            Instr::Delete { dst, obj, key } => {
                let (ov, kv) = (reg!(obj), reg!(key));
                let (key_id, _) = property_key(st, mods, kv)?;
                let r = internal_delete(st, mods, ov, key_id)?;
                reg!(dst) = Value::boolean(r);
            }
            Instr::In { dst, a, b } => {
                let (key_value, obj) = (reg!(a), reg!(b));
                let (key, _) = property_key(st, mods, key_value)?;
                let r = internal_has(st, mods, obj, key)?;
                reg!(dst) = Value::boolean(r);
            }
            Instr::InstanceOf { dst, a, b } => {
                let (x, ctor) = (reg!(a), reg!(b));
                let r = instance_of(st, x, ctor)?;
                reg!(dst) = Value::boolean(r);
            }
            Instr::Jump { target } => ip = target as usize,
            Instr::JumpIfFalse { cond, target } => {
                if !truthy(st, reg!(cond)) {
                    ip = target as usize;
                }
            }
            Instr::JumpIfTrue { cond, target } => {
                if truthy(st, reg!(cond)) {
                    ip = target as usize;
                }
            }
            Instr::JumpIfNullish { cond, target } => {
                if reg!(cond).is_nullish() {
                    ip = target as usize;
                }
            }
            Instr::JumpIfNotNullish { cond, target } => {
                if !reg!(cond).is_nullish() {
                    ip = target as usize;
                }
            }
            Instr::Call { func, argc } => {
                let fv = reg!(func);
                if !fv.is_function() {
                    // name a few own props so the log identifies WHICH
                    // object was called (module namespace, stub, ...)
                    let hint = if fv.is_object() {
                        let oi = fv.index() as usize;
                        let shape = st.objects[oi].shape as usize;
                        let mut ks: Vec<&str> = st.shapes[shape]
                            .props
                            .keys()
                            .map(|&k| st.names[k as usize].as_str())
                            .take(5)
                            .collect();
                        ks.sort_unstable();
                        format!(" (props: {})", ks.join(","))
                    } else {
                        String::new()
                    };
                    return type_err(format!(
                        "{fv:?} is not a function{hint} \
                         [at m{mi} p{pi} ip{ip} in {}]",
                        cmod.module.protos[pi as usize].name,
                    ));
                }
                if matches!(
                    st.closures[fv.index() as usize],
                    ClosureRec::Bound { .. } | ClosureRec::Proxy(_)
                ) {
                    let args: Vec<Value> = (0..argc as usize)
                        .map(|k| st.regs[base + func as usize + 1 + k])
                        .collect();
                    let r = call_value_this(st, mods, fv, None, &args)?;
                    reg!(func) = r;
                    continue;
                }
                // extracted call/apply/bind invoked directly:
                // `c(fn, t, ...)` behaves as the uncurried form
                // (core-js: `var call = FP.call; call(fn, ...)`)
                if let ClosureRec::Native(Native::MethodRef(k)) =
                    st.closures[fv.index() as usize]
                {
                    if matches!(
                        st.names[k as usize].as_str(),
                        "call" | "apply" | "bind"
                    ) {
                        let args: Vec<Value> = (0..argc as usize)
                            .map(|j| {
                                st.regs[base + func as usize + 1 + j]
                            })
                            .collect();
                        let t = args
                            .first()
                            .copied()
                            .unwrap_or(Value::UNDEFINED);
                        let rest =
                            if args.len() > 1 { &args[1..] } else { &[] };
                        let r = call_value_this(
                            st, mods, fv, Some(t), rest,
                        )?;
                        reg!(func) = r;
                        continue;
                    }
                }
                if st.frames.len() >= MAX_FRAMES {
                    return err("stack overflow");
                }
                let cl_idx = fv.index();
                let kind = match &st.closures[cl_idx as usize] {
                    ClosureRec::User {
                        module, proto, this_capture, ..
                    } => Ok((*module, *proto, *this_capture)),
                    ClosureRec::Native(n) => Err(*n),
                    ClosureRec::Bound { .. } => {
                        unreachable!("bound pre-checked")
                    }
                    ClosureRec::Proxy(_) => unreachable!("proxy pre-checked"),
                };
                match kind {
                    Err(n) => {
                        let r =
                            do_native(st, mods, n, base + func as usize + 1, argc)?;
                        reg!(func) = r;
                    }
                    Ok((cm, cp, this_cap)) => {
                        let (cm, cp) =
                            ensure_compiled(st, mods, cm, cp)?;
                        if let ClosureRec::User { module, proto, .. } =
                            &mut st.closures[cl_idx as usize]
                        {
                            (*module, *proto) = (cm, cp);
                        }
                        let callee_rc = mods.rc(cm);
                        let callee =
                            &callee_rc.module.protos[cp as usize];
                        let new_base = base + func as usize + 1;
                        let need = new_base + callee.nregs as usize;
                        if st.regs.len() < need {
                            st.regs.resize(need, Value::UNDEFINED);
                        }
                        let from = frame_blank_from(
                            callee.uses_arguments,
                            argc as usize,
                            callee.nparams as usize,
                        );
                        for r in from..callee.nregs as usize {
                            st.regs[new_base + r] = Value::UNDEFINED;
                        }
                        st.frames.push(Frame {
                            module: mi,
                            proto: pi,
                            ip,
                            base,
                            closure: cur_cl,
                            this_val: this_v,
                            argc: cur_argc,
                            with_base: cur_with_base,
                        });
                        mi = cm;
                        pi = cp;
                        ip = 0;
                        base = new_base;
                        cur_cl = cl_idx;
                        cur_argc = argc;
                        cur_with_base = st.with_stack.len();
                        // arrows keep their lexical this; plain calls get
                        // undefined (no receiver)
                        this_v = this_cap.unwrap_or(Value::UNDEFINED);
                        cmod = mods.rc(mi);
                    }
                }
            }
            Instr::CallThis { func, recv, argc } => {
                let fv = reg!(func);
                if !fv.is_function() {
                    return type_err(format!(
                        "{fv:?} is not a function \
                         [at m{mi} p{pi} ip{ip} in {}]",
                        cmod.module.protos[pi as usize].name,
                    ));
                }
                if matches!(
                    st.closures[fv.index() as usize],
                    ClosureRec::Bound { .. }
                        | ClosureRec::Proxy(_)
                        | ClosureRec::Native(Native::MethodRef(_))
                ) {
                    let receiver = reg!(recv);
                    let args: Vec<Value> = (0..argc as usize)
                        .map(|k| st.regs[base + func as usize + 1 + k])
                        .collect();
                    let r = call_value_this(
                        st, mods, fv, Some(receiver), &args,
                    )?;
                    reg!(func) = r;
                    continue;
                }
                if st.frames.len() >= MAX_FRAMES {
                    return err("stack overflow");
                }
                let receiver = reg!(recv);
                let cl_idx = fv.index();
                let kind = match &st.closures[cl_idx as usize] {
                    ClosureRec::User {
                        module, proto, this_capture, ..
                    } => Ok((*module, *proto, *this_capture)),
                    ClosureRec::Native(n) => Err(*n),
                    ClosureRec::Bound { .. } => {
                        unreachable!("bound pre-checked")
                    }
                    ClosureRec::Proxy(_) => unreachable!("proxy pre-checked"),
                };
                match kind {
                    Err(n) => {
                        let r =
                            do_native(st, mods, n, base + func as usize + 1, argc)?;
                        reg!(func) = r;
                    }
                    Ok((cm, cp, this_cap)) => {
                        let (cm, cp) =
                            ensure_compiled(st, mods, cm, cp)?;
                        if let ClosureRec::User { module, proto, .. } =
                            &mut st.closures[cl_idx as usize]
                        {
                            (*module, *proto) = (cm, cp);
                        }
                        let callee_rc = mods.rc(cm);
                        let callee =
                            &callee_rc.module.protos[cp as usize];
                        let new_base = base + func as usize + 1;
                        let need = new_base + callee.nregs as usize;
                        if st.regs.len() < need {
                            st.regs.resize(need, Value::UNDEFINED);
                        }
                        let from = frame_blank_from(
                            callee.uses_arguments,
                            argc as usize,
                            callee.nparams as usize,
                        );
                        for r in from..callee.nregs as usize {
                            st.regs[new_base + r] = Value::UNDEFINED;
                        }
                        st.frames.push(Frame {
                            module: mi,
                            proto: pi,
                            ip,
                            base,
                            closure: cur_cl,
                            this_val: this_v,
                            argc: cur_argc,
                            with_base: cur_with_base,
                        });
                        mi = cm;
                        pi = cp;
                        ip = 0;
                        base = new_base;
                        cur_cl = cl_idx;
                        cur_argc = argc;
                        cur_with_base = st.with_stack.len();
                        // arrows keep their lexical this; everything
                        // else gets the receiver the callee was read off
                        this_v = this_cap.unwrap_or(receiver);
                        cmod = mods.rc(mi);
                    }
                }
            }
            Instr::CallMethod { obj, atom, argc } => {
                let ov = reg!(obj);
                let key = name!(atom);
                if proxy_id_of(st, ov).is_some() {
                    let method = internal_get(st, mods, ov, key, ov)?;
                    if !method.is_function() {
                        return type_err(format!(
                            ".{} is not a function on Proxy",
                            st.names[key as usize],
                        ));
                    }
                    let args: Vec<Value> = (0..argc as usize)
                        .map(|index| {
                            st.regs[base + obj as usize + 1 + index]
                        })
                        .collect();
                    reg!(obj) = call_value_this(
                        st, mods, method, Some(ov), &args,
                    )?;
                    continue;
                }
                // Function.prototype.call / apply (backs spread calls)
                if ov.is_function() {
                    // Function.prototype.bind: package this + partials
                    if st.names[key as usize] == "bind" {
                        let a0 = base + obj as usize + 1;
                        let this_val = if argc > 0 {
                            st.regs[a0]
                        } else {
                            Value::UNDEFINED
                        };
                        let bound: Vec<Value> = (1..argc as usize)
                            .map(|k| st.regs[a0 + k])
                            .collect();
                        st.closures.push(ClosureRec::Bound {
                            target: ov,
                            this_val,
                            bound,
                        });
                        reg!(obj) = Value::function(
                            (st.closures.len() - 1) as u32,
                        );
                        continue;
                    }
                    let is_apply = st.names[key as usize] == "apply";
                    let is_call = st.names[key as usize] == "call";
                    if is_apply || is_call {
                        let a0 = base + obj as usize + 1;
                        let this_arg = if argc > 0 {
                            st.regs[a0]
                        } else {
                            Value::UNDEFINED
                        };
                        let args: Vec<Value> = if is_apply {
                            let av = if argc > 1 {
                                st.regs[a0 + 1]
                            } else {
                                Value::UNDEFINED
                            };
                            if av.is_object()
                                && st.objects[av.index() as usize].is_array
                            {
                                st.objects[av.index() as usize].elems.clone()
                            } else {
                                Vec::new()
                            }
                        } else {
                            (1..argc as usize)
                                .map(|k| st.regs[a0 + k])
                                .collect()
                        };
                        // Hot path for transpiled bundles:
                        // `fn.apply(this, arguments)` appears hundreds of
                        // thousands of times in Naver. Re-entering exec()
                        // through call_value_this used to allocate a second
                        // register segment and a native Rust stack frame for
                        // every wrapper. A plain user function can instead
                        // use the VM's normal in-loop call-frame transition.
                        let direct_user = match &st.closures
                            [ov.index() as usize]
                        {
                            ClosureRec::User {
                                module, proto, this_capture, ..
                            } => Some((*module, *proto, *this_capture)),
                            _ => None,
                        };
                        if let Some((cm0, cp0, this_cap)) = direct_user {
                            if st.frames.len() >= MAX_FRAMES {
                                return err("stack overflow");
                            }
                            let cl_idx = ov.index();
                            let (cm, cp) =
                                ensure_compiled(st, mods, cm0, cp0)?;
                            if let ClosureRec::User {
                                module, proto, ..
                            } = &mut st.closures[cl_idx as usize]
                            {
                                (*module, *proto) = (cm, cp);
                            }
                            let callee_rc = mods.rc(cm);
                            let callee =
                                &callee_rc.module.protos[cp as usize];
                            let actual = args.len().min(u8::MAX as usize);
                            let copied = frame_blank_from(
                                callee.uses_arguments,
                                actual,
                                callee.nparams as usize,
                            );
                            let new_base = base + obj as usize + 1;
                            let need = new_base + (callee.nregs as usize)
                                .max(copied);
                            if st.regs.len() < need {
                                st.regs.resize(need, Value::UNDEFINED);
                            }
                            for (index, value) in
                                args.iter().take(copied).enumerate()
                            {
                                st.regs[new_base + index] = *value;
                            }
                            for register in copied
                                ..callee.nregs as usize
                            {
                                st.regs[new_base + register] =
                                    Value::UNDEFINED;
                            }
                            st.frames.push(Frame {
                                module: mi,
                                proto: pi,
                                ip,
                                base,
                                closure: cur_cl,
                                this_val: this_v,
                                argc: cur_argc,
                                with_base: cur_with_base,
                            });
                            mi = cm;
                            pi = cp;
                            ip = 0;
                            base = new_base;
                            cur_cl = cl_idx;
                            cur_argc = actual as u8;
                            cur_with_base = st.with_stack.len();
                            this_v = this_cap.unwrap_or(this_arg);
                            cmod = mods.rc(mi);
                            continue;
                        }
                        let r = call_value_this(
                            st, mods, ov, Some(this_arg), &args,
                        )?;
                        reg!(obj) = r;
                        continue;
                    }
                    // static methods on callable builtins
                    // (Object.keys, Array.isArray) and user statics —
                    // including inherited ones (fn [[Prototype]] chain)
                    if let Some(mv) =
                        fn_static_lookup(st, ov.index(), key)
                    {
                        let args: Vec<Value> = (0..argc as usize)
                            .map(|k| st.regs[base + obj as usize + 1 + k])
                            .collect();
                        let r = call_value_this(
                            st, mods, mv, Some(ov), &args,
                        )?;
                        reg!(obj) = r;
                        continue;
                    }
                    // Function.prototype.toString/valueOf and the
                    // Object.prototype staples on a function receiver
                    // (bundles hash/feature-detect via fn.toString())
                    match st.names[key as usize].as_str() {
                        "toString" => {
                            reg!(obj) = push_str(
                                st,
                                "function () { [native code] }"
                                    .to_string(),
                            );
                            continue;
                        }
                        "valueOf" => {
                            reg!(obj) = ov;
                            continue;
                        }
                        "hasOwnProperty" => {
                            let a0 = base + obj as usize + 1;
                            let k = if argc > 0 {
                                to_display(st, st.regs[a0])
                            } else {
                                String::new()
                            };
                            let kid = st.intern_name(&k);
                            // `length` and `name` are own properties of
                            // every function; this engine computes them
                            // rather than storing them, so a check
                            // against stored statics alone says no
                            let has = k == "prototype"
                                || k == "length"
                                || k == "name"
                                || st.fn_props
                                    .contains_key(&(ov.index(), kid));
                            reg!(obj) = Value::boolean(has);
                            continue;
                        }
                        "isPrototypeOf"
                        | "propertyIsEnumerable" => {
                            reg!(obj) = Value::boolean(false);
                            continue;
                        }
                        _ => {}
                    }
                }
                if ov.is_object() {
                    let oi = ov.index() as usize;
                    // RegExp .test(str) / .exec(str)
                    if st.objects[oi].regex != REGEX_NONE {
                        let ri = st.objects[oi].regex as usize;
                        let method = st.names[key as usize].clone();
                        let a0 = base + obj as usize + 1;
                        let subject = if argc > 0 {
                            to_display(st, st.regs[a0])
                        } else {
                            String::new()
                        };
                        let r = match method.as_str() {
                            "test" => {
                                Value::boolean(
                                    st.regexes[ri].re.is_match(&subject),
                                )
                            }
                            "exec" => {
                                // collect owned strings first to release
                                // the regex borrow before push_str mutates st
                                let groups = st.regexes[ri]
                                    .re
                                    .captures_owned(&subject);
                                match groups {
                                    None => Value::NULL,
                                    Some(gs) => {
                                        let vals: Vec<Value> = gs
                                            .iter()
                                            .map(|g| match g {
                                                Some(s) => {
                                                    push_str(st, s.clone())
                                                }
                                                None => Value::UNDEFINED,
                                            })
                                            .collect();
                                        let arr = new_array(st, vals);
                                        attach_groups(st, arr, ri, &gs);
                                        arr
                                    }
                                }
                            }
                            other => {
                                return err(format!(
                                    "RegExp has no .{other}() yet"
                                ))
                            }
                        };
                        reg!(obj) = r;
                        continue;
                    }
                    // Promise .then/.catch (async runtime, P3).
                    if st.objects[oi].promise != PROMISE_NONE {
                        let pid = st.objects[oi].promise;
                        let method = st.names[key as usize].clone();
                        let a0 = base + obj as usize + 1;
                        let arg0 = if argc > 0 { st.regs[a0] }
                            else { Value::UNDEFINED };
                        let arg1 = if argc > 1 { st.regs[a0 + 1] }
                            else { Value::UNDEFINED };
                        let res = match method.as_str() {
                            "then" => promise_then(st, pid, arg0, arg1),
                            "catch" => {
                                promise_then(st, pid, Value::UNDEFINED, arg0)
                            }
                            "finally" => promise_finally(st, pid, arg0),
                            other => {
                                return err(format!(
                                    "Promise has no method .{other}() yet"
                                ))
                            }
                        };
                        reg!(obj) = res;
                        continue;
                    }
                    // fetch Response .text()/.json() (async runtime, P3).
                    if st.fetch_body_atom != u32::MAX
                        && (key == st.text_atom || key == st.json_atom)
                    {
                        if let Some(body_v) =
                            raw_get_prop(st, oi, st.fetch_body_atom)
                        {
                            let body = to_display(st, body_v);
                            let (pv, pid) = new_promise(st);
                            let resolved = if key == st.json_atom {
                                let mut p = JsonP {
                                    b: body.chars().collect(),
                                    i: 0,
                                    depth: 0,
                                };
                                json_parse(st, &mut p)
                                    .unwrap_or(Value::NULL)
                            } else {
                                make_string(st, body)
                            };
                            promise_settle(st, pid, resolved, false);
                            reg!(obj) = pv;
                            continue;
                        }
                    }
                    let is_array = st.objects[oi].is_array;
                    if is_array
                        && key == st.ids.push
                        && raw_get_prop(st, oi, key).is_none()
                    {
                        // fast path — unless push was replaced
                        // (webpack's chunk-loading global does that)
                        for k in 0..argc as usize {
                            let v = st.regs[base + obj as usize + 1 + k];
                            st.objects[oi].elems.push(v);
                        }
                        reg!(obj) =
                            Value::int(st.objects[oi].elems.len() as i32);
                    } else if is_array && key == st.ids.sort {
                        let cmp = if argc > 0 {
                            Some(st.regs[base + obj as usize + 1])
                        } else {
                            None
                        };
                        let mut elems =
                            std::mem::take(&mut st.objects[oi].elems);
                        let r = merge_sort(st, mods, &mut elems, cmp);
                        st.objects[oi].elems = elems;
                        r?;
                        reg!(obj) = ov;
                    } else {
                        if is_array {
                            // An own function property SHADOWS the
                            // builtin (webpack replaces chunk-array
                            // .push with its runtime loader — that
                            // override must win)
                            if let Some(ov_fn) = raw_get_prop(
                                st, oi, key)
                            {
                                if ov_fn.is_function() {
                                    let args: Vec<Value> = (0..argc
                                        as usize)
                                        .map(|k| {
                                            st.regs[base
                                                + obj as usize
                                                + 1
                                                + k]
                                        })
                                        .collect();
                                    let r = call_value_this(
                                        st, mods, ov_fn, Some(ov),
                                        &args,
                                    )?;
                                    reg!(obj) = r;
                                    continue;
                                }
                            }
                            // Array builtins dispatched by name. Callback
                            // forms reuse call_value (as sort does). An
                            // unrecognized name falls through to a stored
                            // function property.
                            let method = st.names[key as usize].clone();
                            let a0 = base + obj as usize + 1;
                            let arg0 = if argc > 0 {
                                st.regs[a0]
                            } else {
                                Value::UNDEFINED
                            };
                            let arg1 = if argc > 1 {
                                st.regs[a0 + 1]
                            } else {
                                Value::UNDEFINED
                            };
                            // forEach/map/filter/some/every/find/findIndex
                            // take a `thisArg` (2nd arg) that becomes the
                            // callback's `this`; the IntersectionObserver
                            // polyfill relies on `.forEach(cb, this)`.
                            let cb_this =
                                if argc > 1 { Some(arg1) } else { None };
                            let mut done = true;
                            let r: Value = match method.as_str() {
                                "pop" => st.objects[oi].elems.pop()
                                    .unwrap_or(Value::UNDEFINED),
                                "shift" => {
                                    let e = &mut st.objects[oi].elems;
                                    if e.is_empty() {
                                        Value::UNDEFINED
                                    } else {
                                        e.remove(0)
                                    }
                                }
                                "unshift" => {
                                    for k in (0..argc as usize).rev() {
                                        let v = st.regs[a0 + k];
                                        st.objects[oi].elems.insert(0, v);
                                    }
                                    Value::int(
                                        st.objects[oi].elems.len() as i32,
                                    )
                                }
                                "splice" => {
                                    let len =
                                        st.objects[oi].elems.len() as f64;
                                    let s = if argc > 0 {
                                        let v = arg0.to_number_raw();
                                        if v < 0.0 {
                                            (len + v).max(0.0)
                                        } else {
                                            v.min(len)
                                        }
                                    } else {
                                        0.0
                                    } as usize;
                                    let dc = if argc > 1 {
                                        arg1.to_number_raw()
                                            .max(0.0)
                                            .min(len - s as f64)
                                            as usize
                                    } else {
                                        len as usize - s
                                    };
                                    let inserted: Vec<Value> = (2..argc
                                        as usize)
                                        .map(|k| st.regs[a0 + k])
                                        .collect();
                                    let removed: Vec<Value> = st.objects
                                        [oi]
                                        .elems
                                        .splice(s..s + dc, inserted)
                                        .collect();
                                    new_array(st, removed)
                                }
                                "reverse" => {
                                    st.objects[oi].elems.reverse();
                                    ov
                                }
                                "join" => {
                                    let sep = if argc > 0 {
                                        to_display(st, arg0)
                                    } else {
                                        ",".to_string()
                                    };
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let mut out = String::new();
                                    for (i, &e) in elems.iter().enumerate() {
                                        if i > 0 {
                                            out.push_str(&sep);
                                        }
                                        if !(e.is_null()
                                            || e.is_undefined())
                                        {
                                            let s = to_display(st, e);
                                            out.push_str(&s);
                                        }
                                    }
                                    push_str(st, out)
                                }
                                "indexOf" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    // optional fromIndex (negative counts
                                    // back from the end)
                                    let start = if argc > 1 {
                                        let n = arg1.to_number_raw();
                                        let len = elems.len() as i64;
                                        let s = if n < 0.0 {
                                            (len + n as i64).max(0)
                                        } else {
                                            n as i64
                                        };
                                        s.max(0) as usize
                                    } else {
                                        0
                                    };
                                    let mut idx = -1i32;
                                    for i in start..elems.len() {
                                        if strict_eq(st, elems[i], arg0) {
                                            idx = i as i32;
                                            break;
                                        }
                                    }
                                    Value::int(idx)
                                }
                                "lastIndexOf" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let mut idx = -1i32;
                                    for (i, &e) in elems.iter().enumerate() {
                                        if strict_eq(st, e, arg0) {
                                            idx = i as i32;
                                        }
                                    }
                                    Value::int(idx)
                                }
                                "includes" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    // SameValueZero: unlike indexOf, NaN
                                    // matches NaN
                                    let want_nan = arg0.is_number()
                                        && arg0.to_number_raw().is_nan();
                                    let mut found = false;
                                    for &e in elems.iter() {
                                        if strict_eq(st, e, arg0)
                                            || (want_nan
                                                && e.is_number()
                                                && e.to_number_raw().is_nan())
                                        {
                                            found = true;
                                            break;
                                        }
                                    }
                                    Value::boolean(found)
                                }
                                "slice" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let len = elems.len() as i64;
                                    let ra = if argc > 0 {
                                        num_of(arg0)? as i64
                                    } else {
                                        0
                                    };
                                    let rb = if argc > 1 {
                                        num_of(arg1)? as i64
                                    } else {
                                        len
                                    };
                                    let a = if ra < 0 {
                                        (len + ra).max(0)
                                    } else {
                                        ra.min(len)
                                    };
                                    let b = if rb < 0 {
                                        (len + rb).max(0)
                                    } else {
                                        rb.min(len)
                                    };
                                    let out: Vec<Value> = if a < b {
                                        elems[a as usize..b as usize]
                                            .to_vec()
                                    } else {
                                        Vec::new()
                                    };
                                    new_array(st, out)
                                }
                                "concat" => {
                                    let mut out =
                                        st.objects[oi].elems.clone();
                                    for k in 0..argc as usize {
                                        let a = st.regs[a0 + k];
                                        if a.is_object()
                                            && st.objects[a.index()
                                                as usize]
                                                .is_array
                                        {
                                            let other = st.objects
                                                [a.index() as usize]
                                                .elems
                                                .clone();
                                            out.extend(other);
                                        } else {
                                            out.push(a);
                                        }
                                    }
                                    new_array(st, out)
                                }
                                "map" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let mut out =
                                        Vec::with_capacity(elems.len());
                                    for (i, &e) in elems.iter().enumerate()
                                    {
                                        let v = call_value_this(
                                            st, mods, arg0, cb_this,
                                            &[e, Value::int(i as i32), ov],
                                        )?;
                                        out.push(v);
                                    }
                                    new_array(st, out)
                                }
                                "filter" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let mut out = Vec::new();
                                    for (i, &e) in elems.iter().enumerate()
                                    {
                                        let v = call_value_this(
                                            st, mods, arg0, cb_this,
                                            &[e, Value::int(i as i32), ov],
                                        )?;
                                        if truthy(st, v) {
                                            out.push(e);
                                        }
                                    }
                                    new_array(st, out)
                                }
                                "some" | "every" => {
                                    let want_all = method == "every";
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let mut result = want_all;
                                    for (i, &e) in
                                        elems.iter().enumerate()
                                    {
                                        let v = call_value_this(
                                            st, mods, arg0, cb_this,
                                            &[e, Value::int(i as i32), ov],
                                        )?;
                                        let tv = truthy(st, v);
                                        if want_all && !tv {
                                            result = false;
                                            break;
                                        }
                                        if !want_all && tv {
                                            result = true;
                                            break;
                                        }
                                    }
                                    Value::boolean(result)
                                }
                                "forEach" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    for (i, &e) in elems.iter().enumerate()
                                    {
                                        call_value_this(
                                            st, mods, arg0, cb_this,
                                            &[e, Value::int(i as i32), ov],
                                        )?;
                                    }
                                    Value::UNDEFINED
                                }
                                "find" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let mut res = Value::UNDEFINED;
                                    for (i, &e) in elems.iter().enumerate()
                                    {
                                        let v = call_value_this(
                                            st, mods, arg0, cb_this,
                                            &[e, Value::int(i as i32), ov],
                                        )?;
                                        if truthy(st, v) {
                                            res = e;
                                            break;
                                        }
                                    }
                                    res
                                }
                                "findIndex" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let mut res = -1i32;
                                    for (i, &e) in elems.iter().enumerate()
                                    {
                                        let v = call_value_this(
                                            st, mods, arg0, cb_this,
                                            &[e, Value::int(i as i32), ov],
                                        )?;
                                        if truthy(st, v) {
                                            res = i as i32;
                                            break;
                                        }
                                    }
                                    Value::int(res)
                                }
                                "some" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let mut res = false;
                                    for (i, &e) in elems.iter().enumerate()
                                    {
                                        let v = call_value_this(
                                            st, mods, arg0, cb_this,
                                            &[e, Value::int(i as i32), ov],
                                        )?;
                                        if truthy(st, v) {
                                            res = true;
                                            break;
                                        }
                                    }
                                    Value::boolean(res)
                                }
                                "every" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let mut res = true;
                                    for (i, &e) in elems.iter().enumerate()
                                    {
                                        let v = call_value_this(
                                            st, mods, arg0, cb_this,
                                            &[e, Value::int(i as i32), ov],
                                        )?;
                                        if !truthy(st, v) {
                                            res = false;
                                            break;
                                        }
                                    }
                                    Value::boolean(res)
                                }
                                "reduce" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let (mut acc, start) = if argc >= 2 {
                                        (arg1, 0)
                                    } else if !elems.is_empty() {
                                        (elems[0], 1)
                                    } else {
                                        return err(
                                            "Reduce of empty array with \
                                             no initial value",
                                        );
                                    };
                                    for i in start..elems.len() {
                                        acc = call_value(
                                            st, mods, arg0,
                                            &[
                                                acc,
                                                elems[i],
                                                Value::int(i as i32),
                                                ov,
                                            ],
                                        )?;
                                    }
                                    acc
                                }
                                "reduceRight" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let n = elems.len() as i64;
                                    let (mut acc, mut i) = if argc >= 2 {
                                        (arg1, n - 1)
                                    } else if !elems.is_empty() {
                                        (elems[elems.len() - 1], n - 2)
                                    } else {
                                        return err(
                                            "Reduce of empty array with \
                                             no initial value",
                                        );
                                    };
                                    while i >= 0 {
                                        acc = call_value(
                                            st, mods, arg0,
                                            &[
                                                acc,
                                                elems[i as usize],
                                                Value::int(i as i32),
                                                ov,
                                            ],
                                        )?;
                                        i -= 1;
                                    }
                                    acc
                                }
                                "fill" => {
                                    let len =
                                        st.objects[oi].elems.len() as i64;
                                    let clamp = |x: i64| {
                                        if x < 0 {
                                            (len + x).max(0)
                                        } else {
                                            x.min(len)
                                        }
                                    };
                                    let s = clamp(if argc > 1 {
                                        num_of(arg1)? as i64
                                    } else {
                                        0
                                    });
                                    let e_arg = if argc > 2 {
                                        st.regs[a0 + 2]
                                    } else {
                                        Value::UNDEFINED
                                    };
                                    let e = if e_arg.is_undefined() {
                                        len
                                    } else {
                                        clamp(num_of(e_arg)? as i64)
                                    };
                                    for k in s..e {
                                        st.objects[oi].elems[k as usize] =
                                            arg0;
                                    }
                                    ov
                                }
                                "copyWithin" => {
                                    let len =
                                        st.objects[oi].elems.len() as i64;
                                    let clamp = |x: i64| {
                                        if x < 0 {
                                            (len + x).max(0)
                                        } else {
                                            x.min(len)
                                        }
                                    };
                                    let target = clamp(if argc > 0 {
                                        num_of(arg0)? as i64
                                    } else {
                                        0
                                    });
                                    let start = clamp(if argc > 1 {
                                        num_of(arg1)? as i64
                                    } else {
                                        0
                                    });
                                    let e_arg = if argc > 2 {
                                        st.regs[a0 + 2]
                                    } else {
                                        Value::UNDEFINED
                                    };
                                    let end = if e_arg.is_undefined() {
                                        len
                                    } else {
                                        clamp(num_of(e_arg)? as i64)
                                    };
                                    let count =
                                        (end - start).min(len - target).max(0);
                                    let src: Vec<Value> = st.objects[oi]
                                        .elems[start as usize
                                            ..(start + count) as usize]
                                        .to_vec();
                                    for k in 0..count as usize {
                                        st.objects[oi].elems
                                            [target as usize + k] = src[k];
                                    }
                                    ov
                                }
                                // ES2023 immutable variants return a copy
                                "toReversed" => {
                                    let mut e =
                                        st.objects[oi].elems.clone();
                                    e.reverse();
                                    new_array(st, e)
                                }
                                "toSorted" => {
                                    let mut e =
                                        st.objects[oi].elems.clone();
                                    let cmp = if argc > 0
                                        && arg0.is_function()
                                    {
                                        Some(arg0)
                                    } else {
                                        None
                                    };
                                    merge_sort(st, mods, &mut e, cmp)?;
                                    new_array(st, e)
                                }
                                "with" => {
                                    let mut e =
                                        st.objects[oi].elems.clone();
                                    let len = e.len() as i64;
                                    let mut i = num_of(arg0)? as i64;
                                    if i < 0 {
                                        i += len;
                                    }
                                    if i < 0 || i >= len {
                                        return err(
                                            "Array.with: index out of range",
                                        );
                                    }
                                    e[i as usize] = arg1;
                                    new_array(st, e)
                                }
                                "at" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let len = elems.len() as i64;
                                    let mut i = if argc > 0 {
                                        num_of(arg0)? as i64
                                    } else {
                                        0
                                    };
                                    if i < 0 {
                                        i += len;
                                    }
                                    if i >= 0 && i < len {
                                        elems[i as usize]
                                    } else {
                                        Value::UNDEFINED
                                    }
                                }
                                "flat" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let depth = if argc > 0 {
                                        num_of(arg0)? as i64
                                    } else {
                                        1
                                    };
                                    let out =
                                        flatten_array(st, &elems, depth);
                                    new_array(st, out)
                                }
                                "flatMap" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let mut mapped =
                                        Vec::with_capacity(elems.len());
                                    for (i, &e) in elems.iter().enumerate()
                                    {
                                        let v = call_value_this(
                                            st, mods, arg0, cb_this,
                                            &[e, Value::int(i as i32), ov],
                                        )?;
                                        mapped.push(v);
                                    }
                                    let out =
                                        flatten_array(st, &mapped, 1);
                                    new_array(st, out)
                                }
                                "findLast" | "findLastIndex" => {
                                    let elems =
                                        st.objects[oi].elems.clone();
                                    let want_index =
                                        method == "findLastIndex";
                                    let mut res = if want_index {
                                        Value::int(-1)
                                    } else {
                                        Value::UNDEFINED
                                    };
                                    for i in (0..elems.len()).rev() {
                                        let e = elems[i];
                                        let v = call_value_this(
                                            st, mods, arg0, cb_this,
                                            &[e, Value::int(i as i32), ov],
                                        )?;
                                        if truthy(st, v) {
                                            res = if want_index {
                                                Value::int(i as i32)
                                            } else {
                                                e
                                            };
                                            break;
                                        }
                                    }
                                    res
                                }
                                // real array iterators (core-js
                                // es.array.iterator calls [].keys())
                                "keys" | "values" | "entries" => {
                                    let args: Vec<Value> = (0..argc
                                        as usize)
                                        .map(|k| st.regs[a0 + k])
                                        .collect();
                                    method_ref_dispatch(
                                        st, mods, ov, key, &args,
                                    )?
                                }
                                _ => {
                                    done = false;
                                    Value::UNDEFINED
                                }
                            };
                            if done {
                                reg!(obj) = r;
                                continue;
                            }
                            // core-js expandos on Array.prototype
                            // (methods installed via defineProperty)
                            if let PropHit::Data(f) =
                                array_proto_hit(st, key)
                            {
                                if f.is_function() {
                                    let args: Vec<Value> = (0..argc
                                        as usize)
                                        .map(|k| st.regs[a0 + k])
                                        .collect();
                                    let r = call_value_this(
                                        st, mods, f, Some(ov), &args,
                                    )?;
                                    reg!(obj) = r;
                                    continue;
                                }
                            }
                        }
                        let mut m = match lookup_prop(st, oi, key) {
                            PropHit::Data(value) => Some(value),
                            PropHit::Getter(getter)
                                if getter.is_function() =>
                            {
                                Some(call_value_this(
                                    st, mods, getter, Some(ov), &[],
                                )?)
                            }
                            _ => None,
                        };
                        // window.parseInt(...) — global fns are
                        // reachable as window methods
                        if m.is_none()
                            && ov == st.known.window
                            && st.gdef[key as usize]
                            && st.globals[key as usize].is_function()
                        {
                            m = Some(st.globals[key as usize]);
                        }
                        let Some(m) = m else {
                            // universal Object.prototype methods
                            // (hasOwnProperty, propertyIsEnumerable,
                            // isPrototypeOf, ...) share the extraction
                            // dispatcher
                            if matches!(
                                st.names[key as usize].as_str(),
                                "hasOwnProperty"
                                    | "propertyIsEnumerable"
                                    | "isPrototypeOf"
                                    | "valueOf"
                                    | "toString"
                            ) {
                                let args: Vec<Value> = (0..argc as usize)
                                    .map(|k| {
                                        st.regs
                                            [base + obj as usize + 1 + k]
                                    })
                                    .collect();
                                let r = method_ref_dispatch(
                                    st, mods, ov, key, &args,
                                )?;
                                reg!(obj) = r;
                                continue;
                            }
                            let b = brand_string(st, ov);
                            return type_err(format!(
                                ".{}() is not a function (receiver {b})",
                                st.names[key as usize]
                            ));
                        };
                        if !m.is_function() {
                            let b = brand_string(st, ov);
                            return type_err(format!(
                                ".{} is not a function (receiver {b}, \
                                 value {m:?})",
                                st.names[key as usize]
                            ));
                        }
                        if matches!(
                            st.closures[m.index() as usize],
                            ClosureRec::Bound { .. }
                                | ClosureRec::Proxy(_)
                                | ClosureRec::Native(
                                    Native::MethodRef(_),
                                )
                        ) {
                            let args: Vec<Value> = (0..argc as usize)
                                .map(|k| {
                                    st.regs[base + obj as usize + 1 + k]
                                })
                                .collect();
                            let r = call_value_this(
                                st, mods, m, Some(ov), &args,
                            )?;
                            reg!(obj) = r;
                            continue;
                        }
                        if st.frames.len() >= MAX_FRAMES {
                            return err("stack overflow");
                        }
                        let cl_idx = m.index();
                        let kind = match &st.closures[cl_idx as usize] {
                            ClosureRec::User {
                                module, proto, this_capture, ..
                            } => Ok((*module, *proto, *this_capture)),
                            ClosureRec::Native(n) => Err(*n),
                            ClosureRec::Bound { .. } => {
                                unreachable!("bound pre-checked")
                            }
                            ClosureRec::Proxy(_) => {
                                unreachable!("proxy pre-checked")
                            }
                        };
                        match kind {
                            Err(n) => {
                                let r = do_native(
                                    st, mods, n,
                                    base + obj as usize + 1, argc,
                                )?;
                                reg!(obj) = r;
                            }
                            Ok((cm, cp, this_cap)) => {
                                let (cm, cp) = ensure_compiled(
                                    st, mods, cm, cp,
                                )?;
                                if let ClosureRec::User {
                                    module, proto, ..
                                } = &mut st.closures[cl_idx as usize]
                                {
                                    (*module, *proto) = (cm, cp);
                                }
                                let callee_rc = mods.rc(cm);
                                let callee = &callee_rc
                                    .module
                                    .protos[cp as usize];
                                let new_base = base + obj as usize + 1;
                                let need =
                                    new_base + callee.nregs as usize;
                                if st.regs.len() < need {
                                    st.regs.resize(need, Value::UNDEFINED);
                                }
                                let from = frame_blank_from(
                                    callee.uses_arguments,
                                    argc as usize,
                                    callee.nparams as usize,
                                );
                                for r in from..callee.nregs as usize {
                                    st.regs[new_base + r] =
                                        Value::UNDEFINED;
                                }
                                st.frames.push(Frame {
                                    module: mi,
                                    proto: pi,
                                    ip,
                                    base,
                                    closure: cur_cl,
                                    this_val: this_v,
                                    argc: cur_argc,
                                    with_base: cur_with_base,
                                });
                                mi = cm;
                                pi = cp;
                                ip = 0;
                                base = new_base;
                                cur_cl = cl_idx;
                                cur_argc = argc;
                                cur_with_base = st.with_stack.len();
                                // arrow method keeps its lexical this
                                this_v = this_cap.unwrap_or(ov);
                                cmod = mods.rc(mi);
                            }
                        }
                    }
                } else if ov.is_number() && key == st.ids.to_fixed {
                    let digits = if argc > 0 {
                        num_of(st.regs[base + obj as usize + 1])? as usize
                    } else {
                        0
                    };
                    let s = format!("{:.*}", digits, ov.to_number_raw());
                    reg!(obj) = push_str(st, s);
                } else if ov.is_string() {
                    // Fast path: indexed single-char reads reuse a
                    // memoized UTF-16 unit vector, so a char-by-char scan
                    // (`for (i;i<s.length;i++) s.charCodeAt(i)`) is O(n)
                    // amortized, not O(n^2) from rebuilding the string
                    // every call.
                    if matches!(
                        st.names[key as usize].as_str(),
                        "charAt" | "charCodeAt"
                    ) {
                        let a0 = base + obj as usize + 1;
                        let iv = if argc > 0 {
                            num_of(st.regs[a0])?
                        } else {
                            0.0
                        };
                        let code =
                            st.names[key as usize] == "charCodeAt";
                        reg!(obj) =
                            str_char_read(st, ov.index(), iv, code);
                        continue;
                    }
                    // String methods. Index semantics use Unicode scalars
                    // (matches ASCII/BMP; astral chars differ from JS's
                    // UTF-16 units — acceptable for now).
                    let method = st.names[key as usize].clone();
                    let s = str_ref(st, ov.index()).to_string();
                    let a0 = base + obj as usize + 1;
                    let av0 = if argc > 0 { st.regs[a0] } else { Value::UNDEFINED };
                    let av1 = if argc > 1 { st.regs[a0 + 1] } else { Value::UNDEFINED };
                    let r = match method.as_str() {
                        "trim" => push_str(st, s.trim().to_string()),
                        "trimStart" | "trimLeft" => push_str(st, s.trim_start().to_string()),
                        "trimEnd" | "trimRight" => push_str(st, s.trim_end().to_string()),
                        "toUpperCase" | "toLocaleUpperCase" => push_str(st, s.to_uppercase()),
                        "toLowerCase" | "toLocaleLowerCase" => push_str(st, s.to_lowercase()),
                        "toString" => push_str(st, s.clone()),
                        "includes" => {
                            let sub = to_display(st, av0);
                            Value::boolean(s.contains(&sub))
                        }
                        "startsWith" => {
                            let sub = to_display(st, av0);
                            Value::boolean(s.starts_with(&sub))
                        }
                        "endsWith" => {
                            let sub = to_display(st, av0);
                            Value::boolean(s.ends_with(&sub))
                        }
                        "indexOf" => {
                            let sub = to_display(st, av0);
                            let idx = match s.find(&sub) {
                                Some(b) => s[..b].chars().count() as i32,
                                None => -1,
                            };
                            Value::int(idx)
                        }
                        "lastIndexOf" => {
                            let sub = to_display(st, av0);
                            let idx = match s.rfind(&sub) {
                                Some(b) => s[..b].chars().count() as i32,
                                None => -1,
                            };
                            Value::int(idx)
                        }
                        // the primitive is its own value; jQuery's
                        // isFunction probe calls it on hot paths
                        "valueOf" => push_str(st, s.clone()),
                        "repeat" => {
                            let n = num_of(av0)?;
                            if n < 0.0 || !n.is_finite() {
                                return err("invalid repeat count");
                            }
                            if s.len().saturating_mul(n as usize)
                                > MAX_STR_BYTES
                            {
                                return range_err("Invalid string length");
                            }
                            push_str(st, s.repeat(n as usize))
                        }
                        "charAt" => {
                            let i = num_of(av0)? as i64;
                            let ch = if i >= 0 {
                                s.chars().nth(i as usize)
                            } else {
                                None
                            };
                            push_str(st, ch.map(|c| c.to_string())
                                .unwrap_or_default())
                        }
                        "charCodeAt" => {
                            let i = num_of(av0)? as i64;
                            let ch = if i >= 0 {
                                s.chars().nth(i as usize)
                            } else {
                                None
                            };
                            match ch {
                                Some(c) => Value::int(c as i32),
                                None => Value::number(f64::NAN),
                            }
                        }
                        "at" => {
                            let chars: Vec<char> = s.chars().collect();
                            let len = chars.len() as i64;
                            let mut i = num_of(av0)? as i64;
                            if i < 0 {
                                i += len;
                            }
                            if i >= 0 && i < len {
                                push_str(st, chars[i as usize].to_string())
                            } else {
                                Value::UNDEFINED
                            }
                        }
                        "slice" | "substring" => {
                            let chars: Vec<char> = s.chars().collect();
                            let len = chars.len() as i64;
                            let ra = if argc > 0 { num_of(av0)? as i64 } else { 0 };
                            let rb = if argc > 1 { num_of(av1)? as i64 } else { len };
                            let (mut a, mut b);
                            if method == "slice" {
                                a = if ra < 0 { (len + ra).max(0) } else { ra.min(len) };
                                b = if rb < 0 { (len + rb).max(0) } else { rb.min(len) };
                            } else {
                                a = ra.clamp(0, len);
                                b = rb.clamp(0, len);
                                if a > b {
                                    std::mem::swap(&mut a, &mut b);
                                }
                            }
                            let out: String = if a < b {
                                chars[a as usize..b as usize].iter().collect()
                            } else {
                                String::new()
                            };
                            push_str(st, out)
                        }
                        "matchAll" => {
                            let Some(ri) = regex_index(st, av0) else {
                                return err(
                                    "String.matchAll needs a regex arg");
                            };
                            let all = st.regexes[ri].re
                                .captures_all(&s, true);
                            let mut results = Vec::with_capacity(all.len());
                            let idx_atom = st.intern_name("index");
                            let inp_atom = st.intern_name("input");
                            for (whole, groups, start) in all {
                                // char offset of the byte position `start`
                                let cidx = s[..start].chars().count() as i32;
                                let full: Vec<Option<String>> =
                                    std::iter::once(Some(whole.clone()))
                                        .chain(groups.into_iter())
                                        .collect();
                                let vals: Vec<Value> = full.iter()
                                    .map(|g| match g {
                                        Some(x) => push_str(st, x.clone()),
                                        None => Value::UNDEFINED,
                                    })
                                    .collect();
                                let arr = new_array(st, vals);
                                attach_groups(st, arr, ri, &full);
                                let ai = arr.index() as usize;
                                raw_set_prop(st, ai, idx_atom, Value::int(cidx));
                                let inp = push_str(st, s.clone());
                                raw_set_prop(st, ai, inp_atom, inp);
                                results.push(arr);
                            }
                            // an array is iterable — satisfies for-of/spread
                            new_array(st, results)
                        }
                        "match" => {
                            let Some(ri) = regex_index(st, av0) else {
                                return err(
                                    "String.match needs a regex arg (yet)");
                            };
                            if st.regexes[ri].global {
                                let hits = st.regexes[ri].re.find_all(&s);
                                if hits.len() > MAX_MATERIALIZE {
                                    return range_err(
                                        "too many regex matches");
                                }
                                if hits.is_empty() {
                                    Value::NULL
                                } else {
                                    let vals: Vec<Value> = hits.into_iter()
                                        .map(|h| push_str(st, h)).collect();
                                    new_array(st, vals)
                                }
                            } else {
                                let groups =
                                    st.regexes[ri].re.captures_owned(&s);
                                match groups {
                                    None => Value::NULL,
                                    Some(gs) => {
                                        let vals: Vec<Value> = gs.iter()
                                            .map(|g| match g {
                                                Some(x) => {
                                                    push_str(st, x.clone())
                                                }
                                                None => Value::UNDEFINED,
                                            }).collect();
                                        let arr = new_array(st, vals);
                                        attach_groups(st, arr, ri, &gs);
                                        arr
                                    }
                                }
                            }
                        }
                        "search" => {
                            match regex_index(st, av0) {
                                Some(ri) => match st.regexes[ri].re.find_start(&s) {
                                    Some(b) => Value::int(
                                        s[..b].chars().count() as i32),
                                    None => Value::int(-1),
                                },
                                None => {
                                    let sub = to_display(st, av0);
                                    match s.find(&sub) {
                                        Some(b) => Value::int(
                                            s[..b].chars().count() as i32),
                                        None => Value::int(-1),
                                    }
                                }
                            }
                        }
                        "split" => {
                            let mut vals: Vec<Value> =
                                if let Some(ri) = regex_index(st, av0) {
                                    let parts =
                                        st.regexes[ri].re.split_vec(&s);
                                    if parts.len() > MAX_MATERIALIZE {
                                        return range_err(
                                            "too many split results");
                                    }
                                    parts.into_iter()
                                        .map(|p| push_str(st, p)).collect()
                                } else if argc == 0 {
                                    vec![push_str(st, s.clone())]
                                } else {
                                    let sep = to_display(st, av0);
                                    if sep.is_empty() {
                                        if s.len() > MAX_MATERIALIZE {
                                            return range_err(
                                                "too many split results");
                                        }
                                        s.chars()
                                            .map(|c| push_str(st, c.to_string()))
                                            .collect()
                                    } else {
                                        let parts: Vec<&str> = s
                                            .split(&sep)
                                            .take(MAX_MATERIALIZE + 1)
                                            .collect();
                                        if parts.len() > MAX_MATERIALIZE {
                                            return range_err(
                                                "too many split results");
                                        }
                                        parts.into_iter()
                                            .map(|p| push_str(st, p.to_string()))
                                            .collect::<Vec<_>>()
                                    }
                                };
                            // optional limit argument caps the piece count
                            if argc > 1 && !av1.is_undefined() {
                                let lim = av1.to_number_raw();
                                if lim.is_finite() && lim >= 0.0 {
                                    vals.truncate(lim as usize);
                                }
                            }
                            new_array(st, vals)
                        }
                        "replace" => {
                            if av1.is_function() {
                                // callback replacement (lodash _.template etc.)
                                let out = if let Some(ri) = regex_index(st, av0)
                                {
                                    let g = st.regexes[ri].global;
                                    let m = st.regexes[ri].re
                                        .captures_all(&s, g);
                                    replace_with_fn(st, mods, &s, &m, av1)?
                                } else {
                                    let needle = to_display(st, av0);
                                    match s.find(&needle) {
                                        Some(p) if !needle.is_empty() => {
                                            let m = vec![
                                                (needle, Vec::new(), p)];
                                            replace_with_fn(
                                                st, mods, &s, &m, av1)?
                                        }
                                        _ => s.clone(),
                                    }
                                };
                                push_str(st, out)
                            } else if let Some(ri) = regex_index(st, av0) {
                                // JS $&/$<name> -> regex crate syntax
                                let to =
                                    js_repl_to_rust(&to_display(st, av1));
                                let out = if st.regexes[ri].global {
                                    st.regexes[ri].re
                                        .replace_all_str(&s, to.as_str())
                                } else {
                                    st.regexes[ri].re
                                        .replace_first_str(&s, to.as_str())
                                };
                                push_str(st, out)
                            } else {
                                let from = to_display(st, av0);
                                let to = to_display(st, av1);
                                push_str(st, s.replacen(&from, &to, 1))
                            }
                        }
                        "replaceAll" => {
                            if av1.is_function() {
                                let out = if let Some(ri) = regex_index(st, av0)
                                {
                                    let m = st.regexes[ri].re
                                        .captures_all(&s, true);
                                    replace_with_fn(st, mods, &s, &m, av1)?
                                } else {
                                    let needle = to_display(st, av0);
                                    let mut m = Vec::new();
                                    if !needle.is_empty() {
                                        let mut from = 0;
                                        while let Some(rel) =
                                            s[from..].find(&needle)
                                        {
                                            let p = from + rel;
                                            m.push((
                                                needle.clone(),
                                                Vec::new(),
                                                p,
                                            ));
                                            from = p + needle.len();
                                        }
                                    }
                                    replace_with_fn(st, mods, &s, &m, av1)?
                                };
                                push_str(st, out)
                            } else if let Some(ri) = regex_index(st, av0) {
                                let to =
                                    js_repl_to_rust(&to_display(st, av1));
                                let out = st.regexes[ri].re
                                    .replace_all_str(&s, to.as_str());
                                push_str(st, out)
                            } else {
                                let from = to_display(st, av0);
                                let to = to_display(st, av1);
                                push_str(st, s.replace(&from, &to))
                            }
                        }
                        "concat" => {
                            // variadic: recv then EVERY arg coerced to
                            // string (was dropping all but the first)
                            let mut out = s.clone();
                            for k in 0..argc as usize {
                                let v = st.regs[a0 + k];
                                // ToString, not a raw dump: an object
                                // with a toString() must get to run it
                                let piece = to_js_string(st, mods, v)?;
                                out.push_str(&piece);
                            }
                            push_str(st, out)
                        }
                        "substr" => {
                            let units: Vec<u16> =
                                s.encode_utf16().collect();
                            let len = units.len() as f64;
                            let start = av0.to_number_raw();
                            let start = if start < 0.0 {
                                (len + start).max(0.0)
                            } else {
                                start.min(len)
                            } as usize;
                            let count = if av1.is_undefined() {
                                units.len() - start
                            } else {
                                (av1.to_number_raw().max(0.0) as usize)
                                    .min(units.len() - start)
                            };
                            let out = String::from_utf16_lossy(
                                &units[start..start + count],
                            );
                            push_str(st, out)
                        }
                        "propertyIsEnumerable" => {
                            let k = to_display(st, av0);
                            Value::boolean(
                                k.parse::<usize>()
                                    .map(|i| i < s.chars().count())
                                    .unwrap_or(false),
                            )
                        }
                        "hasOwnProperty" => {
                            let k = to_display(st, av0);
                            Value::boolean(
                                k == "length"
                                    || k.parse::<usize>()
                                        .map(|i| i < s.chars().count())
                                        .unwrap_or(false),
                            )
                        }
                        "padStart" | "padEnd" => {
                            let target = num_of(av0)? as i64;
                            if target > MAX_STR_BYTES as i64 {
                                return range_err("Invalid string length");
                            }
                            let pad = if av1.is_undefined() {
                                " ".to_string()
                            } else {
                                to_display(st, av1)
                            };
                            let curlen = s.chars().count() as i64;
                            if target <= curlen || pad.is_empty() {
                                push_str(st, s.clone())
                            } else {
                                let need = (target - curlen) as usize;
                                let pc: Vec<char> = pad.chars().collect();
                                let fill: String =
                                    (0..need).map(|i| pc[i % pc.len()]).collect();
                                let out = if method == "padStart" {
                                    format!("{fill}{s}")
                                } else {
                                    format!("{s}{fill}")
                                };
                                push_str(st, out)
                            }
                        }
                        "codePointAt" => {
                            let i = num_of(av0)? as i64;
                            let ch = if i >= 0 {
                                s.chars().nth(i as usize)
                            } else {
                                None
                            };
                            match ch {
                                Some(c) => Value::int(c as i32),
                                None => Value::UNDEFINED,
                            }
                        }
                        // no ICU on board: NFC-normalize is a best-effort
                        // identity (BMP text is already single-form here)
                        "normalize" => push_str(st, s.clone()),
                        "localeCompare" => {
                            let other = to_display(st, av0);
                            Value::int(match s.cmp(&other) {
                                std::cmp::Ordering::Less => -1,
                                std::cmp::Ordering::Equal => 0,
                                std::cmp::Ordering::Greater => 1,
                            })
                        }
                        _ => {
                            return type_err(format!(
                                "cannot call .{}() on a string (yet)",
                                method
                            ))
                        }
                    };
                    reg!(obj) = r;
                } else if ov.is_dom_node() {
                    let r = dom_method(
                        st,
                        mods,
                        key,
                        ov.index(),
                        base + obj as usize + 1,
                        argc,
                    )?;
                    reg!(obj) = r;
                } else if ov.is_number() || ov.is_boolean() {
                    // primitive methods share the extraction dispatcher
                    let args: Vec<Value> = (0..argc as usize)
                        .map(|k| st.regs[base + obj as usize + 1 + k])
                        .collect();
                    let r = method_ref_dispatch(st, mods, ov, key, &args)?;
                    reg!(obj) = r;
                } else if ov.is_nullish() {
                    return type_err(format!(
                        "cannot call .{}() of {}",
                        st.names[key as usize],
                        if ov.is_null() { "null" } else { "undefined" },
                    ));
                } else {
                    return type_err(format!(
                        "cannot call .{}() on {ov:?} (yet)",
                        st.names[key as usize]
                    ));
                }
            }
            Instr::Return { src } => {
                let val = reg!(src);
                // drop any `with` scope this frame left open (early return
                // from inside `with (obj) { return ... }`)
                st.with_stack.truncate(cur_with_base);
                if st.frames.len() == floor {
                    return Ok(val);
                }
                let fr = st.frames.pop().unwrap();
                cur_argc = fr.argc;
                cur_with_base = fr.with_base;
                // a return from inside `try` leaves its handlers armed;
                // they die with the frame
                while st.handlers.last().is_some_and(
                    |h| h.depth > st.frames.len(),
                ) {
                    st.handlers.pop();
                }
                st.regs[base - 1] = val;
                mi = fr.module;
                pi = fr.proto;
                ip = fr.ip;
                base = fr.base;
                cur_cl = fr.closure;
                this_v = fr.this_val;
                cmod = mods.rc(mi);
            }
            Instr::ReturnUndef => {
                st.with_stack.truncate(cur_with_base);
                if st.frames.len() == floor {
                    return Ok(Value::UNDEFINED);
                }
                let fr = st.frames.pop().unwrap();
                cur_argc = fr.argc;
                cur_with_base = fr.with_base;
                while st.handlers.last().is_some_and(
                    |h| h.depth > st.frames.len(),
                ) {
                    st.handlers.pop();
                }
                st.regs[base - 1] = Value::UNDEFINED;
                mi = fr.module;
                pi = fr.proto;
                ip = fr.ip;
                base = fr.base;
                cur_cl = fr.closure;
                this_v = fr.this_val;
                cmod = mods.rc(mi);
            }
            Instr::Closure { dst, proto: p } => {
                let src_proto =
                    &cmod.module.protos[p as usize];
                let mut upvals =
                    Vec::with_capacity(src_proto.captures.len());
                for cap in &src_proto.captures {
                    upvals.push(match *cap {
                        CapSrc::LocalCell(r) => {
                            let c = reg!(r);
                            debug_assert!(c.is_cell());
                            c.index()
                        }
                        CapSrc::Upval(i) => {
                            match &st.closures[cur_cl as usize] {
                                ClosureRec::User { upvals, .. } => {
                                    upvals[i as usize]
                                }
                                ClosureRec::Native(_) | ClosureRec::Bound { .. }
                                | ClosureRec::Proxy(_) => unreachable!(),
                            }
                        }
                    });
                }
                let this_capture = if src_proto.is_arrow {
                    Some(this_v)
                } else {
                    None
                };
                st.closures.push(ClosureRec::User {
                    module: mi,
                    proto: p as u32,
                    upvals,
                    this_capture,
                });
                reg!(dst) =
                    Value::function((st.closures.len() - 1) as u32);
            }
            Instr::CellWrap { reg } => {
                st.cells.push(reg!(reg));
                reg!(reg) = Value::cell((st.cells.len() - 1) as u32);
            }
            Instr::LoadCell { dst, src } => {
                let c = reg!(src);
                reg!(dst) = st.cells[c.index() as usize];
            }
            Instr::StoreCell { dst, src } => {
                let c = reg!(dst);
                st.cells[c.index() as usize] = reg!(src);
            }
            Instr::GetUpval { dst, idx } => {
                let cell = match &st.closures[cur_cl as usize] {
                    ClosureRec::User { upvals, .. } => upvals[idx as usize],
                    ClosureRec::Native(_) | ClosureRec::Bound { .. }
                    | ClosureRec::Proxy(_) => unreachable!(),
                };
                reg!(dst) = st.cells[cell as usize];
            }
            Instr::SetUpval { idx, src } => {
                let cell = match &st.closures[cur_cl as usize] {
                    ClosureRec::User { upvals, .. } => upvals[idx as usize],
                    ClosureRec::Native(_) | ClosureRec::Bound { .. }
                    | ClosureRec::Proxy(_) => unreachable!(),
                };
                st.cells[cell as usize] = reg!(src);
            }
            Instr::NewObject { dst } => {
                reg!(dst) = new_plain_object(st);
            }
            Instr::NewPromise { executor } => {
                let exec_fn = reg!(executor);
                let (pval, pid) = new_promise(st);
                let resolve = make_native(st, Native::Resolve { pid, reject: false });
                let reject = make_native(st, Native::Resolve { pid, reject: true });
                if exec_fn.is_function() {
                    // executor(resolve, reject); a throw rejects the promise
                    if let Err(e) =
                        call_value(st, mods, exec_fn, &[resolve, reject])
                    {
                        let reason = e.value.unwrap_or_else(|| {
                            let s = e.msg.clone();
                            make_string(st, s)
                        });
                        promise_settle(st, pid, reason, true);
                    }
                }
                reg!(executor) = pval;
            }
            Instr::NewRegex { dst, pat, flags } => {
                let proto = &cmod.module.protos[pi as usize];
                let pv = proto.consts[pat as usize];
                let fv = proto.consts[flags as usize];
                let pattern = str_ref(st, pv.index()).to_string();
                let fl = str_ref(st, fv.index()).to_string();
                reg!(dst) = new_regex(st, &pattern, &fl)?;
            }
            Instr::NewArray { dst } => {
                reg!(dst) = new_array(st, Vec::new());
            }
            Instr::ArrayPush { arr, src } => {
                let v = reg!(src);
                let av = reg!(arr);
                st.objects[av.index() as usize].elems.push(v);
            }
            Instr::ForInKeys { dst, obj } => {
                let ov = reg!(obj);
                let mut keys: Vec<Value> = Vec::new();
                if proxy_id_of(st, ov).is_some() {
                    keys = internal_own_keys(st, mods, ov)?;
                    keys.retain(|&key| to_display(st, key) != "length");
                } else if ov.is_object() {
                    let oi = ov.index() as usize;
                    let (is_array, nelems, shape) = {
                        let o = &st.objects[oi];
                        (o.is_array, o.elems.len(), o.shape)
                    };
                    if is_array {
                        for k in 0..nelems {
                            let s = intern(st, &k.to_string());
                            keys.push(s);
                        }
                    } else {
                        // numeric keys on plain objects also live in
                        // elems (sparse — skip the undefined gaps).
                        // Webpack module maps ({90805: fn, ...}) are
                        // exactly this shape.
                        for k in 0..nelems {
                            if !st.objects[oi].elems[k].is_undefined() {
                                let s = intern(st, &k.to_string());
                                keys.push(s);
                            }
                        }
                    }
                    let _ = shape;
                    // own enumerable named keys, array-index keys ahead of
                    // string keys (skips enumerable:false props)
                    for atom in own_keys_ordered(st, oi, true) {
                        let name = st.names[atom as usize].clone();
                        let sv = intern(st, &name);
                        keys.push(sv);
                    }
                } else if ov.is_function() {
                    for k in fn_own_enumerable_keys(st, ov.index()) {
                        let name = st.names[k as usize].clone();
                        let sv = intern(st, &name);
                        keys.push(sv);
                    }
                } else if ov.is_string() {
                    // A string's own enumerable properties are its
                    // character indices. Emptiness checks in the wild
                    // are written as `for (k in v) return false`, and
                    // reporting a string as empty silently dropped
                    // every string parameter of an ad request.
                    let n = str_ref(st, ov.index()).chars().count();
                    for k in 0..n {
                        let s = intern(st, &k.to_string());
                        keys.push(s);
                    }
                }
                reg!(dst) = new_array(st, keys);
            }
            Instr::IterMaterialize { dst, obj } => {
                let ov = reg!(obj);
                reg!(dst) = materialize_iterable(st, mods, ov)?;
            }
            Instr::GetIndex { dst, obj, key } => {
                let (ov, kv) = (reg!(obj), reg!(key));
                if proxy_id_of(st, ov).is_some() {
                    let (key_id, _) = property_key(st, mods, kv)?;
                    reg!(dst) = internal_get(st, mods, ov, key_id, ov)?;
                } else if ov.is_object() && kv.is_number() {
                    let oi = ov.index() as usize;
                    let k = kv.to_number_raw();
                    let mut hit = Value::UNDEFINED;
                    let in_elems = k >= 0.0
                        && k.fract() == 0.0
                        && (k as usize) < st.objects[oi].elems.len();
                    if in_elems {
                        hit = st.objects[oi].elems[k as usize];
                    }
                    if hit.is_undefined() {
                        // gap or out-of-range: the value may live as a
                        // named (possibly accessor) property — string
                        // and number keys are one namespace in JS
                        let text = to_display(st, kv);
                        let key_id = st.intern_name(&text);
                        hit = match lookup_prop(st, oi, key_id) {
                            PropHit::Data(v) => v,
                            PropHit::Getter(g) if g.is_function() => {
                                call_value_this(
                                    st, mods, g, Some(ov), &[],
                                )?
                            }
                            _ => Value::UNDEFINED,
                        };
                    }
                    reg!(dst) = hit;
                } else if ov.is_object() && kv.is_string() {
                    // dynamic property read: `o[key]`, `o[i]` from for-in
                    let text = str_ref(st, kv.index()).to_string();
                    let oi = ov.index() as usize;
                    // el.style[name] — computed-key style reads mirror
                    // the SetIndex write path
                    if let Some(&node) = st.style_nodes.get(&ov.index())
                    {
                        let doc = need_doc(st)?;
                        let cur = doc.borrow().nodes[node as usize]
                            .attr("style")
                            .unwrap_or("")
                            .to_string();
                        let out = if text == "cssText" {
                            cur
                        } else {
                            style_attr_get(&cur, &camel_to_kebab(&text))
                        };
                        reg!(dst) = push_str(st, out);
                        continue;
                    }
                    // integer-string keys hit dense elems on ANY object
                    // (numeric literal keys live there); gaps fall
                    // through to the named lookup below
                    if let Some(n) = elem_index(&text) {
                        if let Some(&v) = st.objects[oi].elems.get(n) {
                            if !v.is_undefined() {
                                reg!(dst) = v;
                                continue;
                            }
                        }
                        if st.objects[oi].is_array {
                            reg!(dst) = Value::UNDEFINED;
                            continue;
                        }
                    }
                    if st.objects[oi].is_array {
                        if let Ok(n) = text.parse::<usize>() {
                            let o = &st.objects[oi];
                            reg!(dst) = o.elems.get(n).copied()
                                .unwrap_or(Value::UNDEFINED);
                            continue;
                        }
                        if text == "length" {
                            let n = st.objects[oi].elems.len() as i32;
                            reg!(dst) = Value::int(n);
                            continue;
                        }
                    }
                    let key_id = st.intern_name(&text);
                    reg!(dst) = match lookup_prop(st, oi, key_id) {
                        PropHit::Data(v) => v,
                        PropHit::Getter(g) if g.is_function() => {
                            call_value_this(st, mods, g, Some(ov), &[])?
                        }
                        PropHit::Getter(_) => Value::UNDEFINED,
                        // window doubles as the global namespace
                        PropHit::Missing if ov == st.known.window
                            && st.gdef[key_id as usize] =>
                        {
                            st.globals[key_id as usize]
                        }
                        // a plain object's computed `toString` is the
                        // genuine Object.prototype.toString (brands)
                        PropHit::Missing if !st.objects[oi].is_array
                            && text == "toString" =>
                        {
                            make_native(st, Native::BrandToString)
                        }
                        // Object.prototype staples — core-js getMethod
                        // reads V["valueOf"] as a computed access, so
                        // GetIndex must mirror GetProp's fallback or
                        // ordinaryToPrimitive finds nothing callable
                        PropHit::Missing if matches!(
                            text.as_str(),
                            "hasOwnProperty" | "toString" | "valueOf"
                                | "propertyIsEnumerable"
                                | "isPrototypeOf"
                        ) =>
                        {
                            make_native(st, Native::MethodRef(key_id))
                        }
                        PropHit::Missing if st.objects[oi].regex != REGEX_NONE
                            && matches!(text.as_str(), "exec" | "test") =>
                        {
                            make_native(st, Native::MethodRef(key_id))
                        }
                        PropHit::Missing if st.objects[oi].is_array
                            && matches!(
                                text.as_str(),
                                "slice" | "concat" | "join" | "indexOf"
                                    | "push" | "pop" | "map" | "filter"
                                    | "forEach" | "sort" | "splice"
                                    | "shift" | "unshift" | "reverse"
                                    | "some" | "every" | "reduce"
                                    | "lastIndexOf"
                            ) =>
                        {
                            make_native(st, Native::MethodRef(key_id))
                        }
                        // core-js expandos on Array.prototype
                        PropHit::Missing if st.objects[oi].is_array
                            && !matches!(
                                array_proto_hit(st, key_id),
                                PropHit::Missing
                            ) =>
                        {
                            match array_proto_hit(st, key_id) {
                                PropHit::Data(v) => v,
                                PropHit::Getter(g) if g.is_function() => {
                                    call_value_this(
                                        st, mods, g, Some(ov), &[],
                                    )?
                                }
                                _ => Value::UNDEFINED,
                            }
                        }
                        PropHit::Missing => Value::UNDEFINED,
                    };
                } else if ov.is_function() {
                    // fn["prop"]: same surface as static GetProp
                    let text = to_display(st, kv);
                    let key_id = st.intern_name(&text);
                    reg!(dst) = if key_id == st.ids.prototype {
                        fn_prototype(st, ov)
                    } else if let Some(v) =
                        fn_static_lookup(st, ov.index(), key_id)
                    {
                        v
                    } else if key_id == st.ids.length {
                        Value::int(fn_arity(st, mods, ov))
                    } else if text == "name" {
                        let nm = fn_name(st, mods, ov);
                        make_string(st, nm)
                    } else if matches!(
                        text.as_str(),
                        "call" | "apply" | "bind" | "toString" | "valueOf"
                    ) {
                        make_native(st, Native::MethodRef(key_id))
                    } else {
                        Value::UNDEFINED
                    };
                } else if ov.is_object() {
                    // odd key type (null/undefined/bool/object): JS
                    // ToPropertyKey = ToPrimitive(string) then
                    // stringify — object keys with custom toString
                    // (polyfilled Symbol wrappers) must keep their
                    // distinct tags, not collapse to [object Object].
                    // Accessor/chain-aware, with the Array.prototype
                    // expando fallback (t[Symbol.iterator] on arrays)
                    let kvp = to_primitive(st, mods, kv, false)?;
                    let text = to_display(st, kvp);
                    let oi = ov.index() as usize;
                    let key_id = st.intern_name(&text);
                    let mut hit = lookup_prop(st, oi, key_id);
                    if matches!(hit, PropHit::Missing)
                        && st.objects[oi].is_array
                    {
                        hit = array_proto_hit(st, key_id);
                    }
                    reg!(dst) = match hit {
                        PropHit::Data(v) => v,
                        PropHit::Getter(g) if g.is_function() => {
                            call_value_this(st, mods, g, Some(ov), &[])?
                        }
                        _ => Value::UNDEFINED,
                    };
                } else if ov.is_dom_node() {
                    // document[key] / el[key]: same surface as GetProp
                    let text = to_display(st, kv);
                    let key_id = st.intern_name(&text);
                    let r = dom_get_prop(st, key_id, ov.index())?;
                    reg!(dst) = r;
                } else if ov.is_string() {
                    // s[i] / s["length"] / s["slice"] (extraction)
                    let text = to_display(st, kv);
                    let sref = str_ref(st, ov.index()).to_string();
                    reg!(dst) = if let Ok(i) = text.parse::<usize>() {
                        match sref.encode_utf16().nth(i) {
                            Some(u) => {
                                let ch = String::from_utf16_lossy(&[u]);
                                push_str(st, ch)
                            }
                            None => Value::UNDEFINED,
                        }
                    } else if text == "length" {
                        Value::int(str_u16_len(st, ov.index()) as i32)
                    } else {
                        let key_id = st.intern_name(&text);
                        primitive_prop_read(st, ov, key_id)
                    };
                } else if ov.is_nullish() {
                    let text = to_display(st, kv);
                    let tr = if std::env::var("GG_JS_TRACE").is_ok() {
                        let names: Vec<String> = std::iter::once(format!(
                            "{}@{}:{}",
                            cmod.module.protos[pi as usize].name, mi, pi
                        ))
                        .chain(st.frames.iter().rev().take(6).map(|f| {
                            format!(
                                "{}@{}:{}",
                                mods.rc(f.module).module.protos
                                    [f.proto as usize]
                                    .name,
                                f.module, f.proto
                            )
                        }))
                        .collect();
                        format!(
                            " [in {} | mi={} pi={} ip={}]",
                            names.join(" <- "), mi, pi, ip
                        )
                    } else {
                        String::new()
                    };
                    if let Ok(dir) = std::env::var("GG_JS_DUMP") {
                        let dump = |lbl: &str, m: u32, p: u32| {
                            let lm = mods.rc(m);
                            let proto = &lm.module.protos[p as usize];
                            let out = proto
                                .code
                                .iter()
                                .enumerate()
                                .map(|(k, i)| {
                                    format!(
                                        "{k}:{}",
                                        annotate_instr(
                                            st,
                                            &lm.global_map,
                                            &proto.consts,
                                            i,
                                        )
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            let _ = std::fs::write(
                                format!("{dir}/{lbl}_{m}_{p}.txt"),
                                out,
                            );
                        };
                        dump("cur", mi, pi);
                        for (n, f) in
                            st.frames.iter().rev().take(4).enumerate()
                        {
                            dump(&format!("c{n}"), f.module, f.proto);
                        }
                    }
                    return type_err(format!(
                        "cannot read [{text}] of {}{tr}",
                        if ov.is_null() { "null" } else { "undefined" },
                    ));
                } else if ov.is_number() || ov.is_boolean() {
                    // primitives have no own indexed props in our model
                    reg!(dst) = Value::UNDEFINED;
                } else {
                    return err(format!(
                        "unsupported indexing {ov:?}[{kv:?}] (yet)"
                    ));
                }
            }
            Instr::SetIndex { obj, key, src } => {
                let (ov, kv, v) = (reg!(obj), reg!(key), reg!(src));
                if proxy_id_of(st, ov).is_some() {
                    let (key_id, _) = property_key(st, mods, kv)?;
                    let _ = internal_set(st, mods, ov, key_id, v, ov)?;
                } else if ov.is_object() && kv.is_number() {
                    let k = kv.to_number_raw();
                    if k < 0.0 || k.fract() != 0.0 {
                        // JS: not an element — a plain named property
                        let text = to_display(st, kv);
                        let key_id = st.intern_name(&text);
                        raw_set_prop(st, ov.index() as usize, key_id, v);
                        continue;
                    }
                    let k = k as usize;
                    if k >= MAX_ARRAY_ELEMS
                        && k >= st.objects[ov.index() as usize].elems.len()
                    {
                        return range_err(
                            "array index exceeds the engine limit");
                    }
                    let elems = &mut st.objects[ov.index() as usize].elems;
                    if k < elems.len() {
                        elems[k] = v;
                    } else {
                        elems.resize(k, Value::UNDEFINED);
                        elems.push(v);
                    }
                } else if ov.is_object() && kv.is_string() {
                    // dynamic property write: `o[key] = v`
                    let text = str_ref(st, kv.index()).to_string();
                    let oi = ov.index() as usize;
                    // el.style[name] = v — React's setValueForStyles
                    // writes styles with a COMPUTED key; dropping this
                    // lost naver's inline overflow:hidden on the
                    // widget-board carousel viewport
                    if let Some(&node) = st.style_nodes.get(&ov.index())
                    {
                        let val = to_display(st, v);
                        let doc = need_doc(st)?;
                        if text == "cssText" {
                            doc.borrow_mut().set_attr(
                                node as usize, "style", &val);
                        } else {
                            let prop = camel_to_kebab(&text);
                            let cur = doc.borrow().nodes[node as usize]
                                .attr("style")
                                .unwrap_or("")
                                .to_string();
                            let next =
                                style_attr_set(&cur, &prop, &val);
                            doc.borrow_mut().set_attr(
                                node as usize, "style", &next);
                        }
                        continue;
                    }
                    if let Some(&node) =
                        st.dataset_nodes.get(&ov.index())
                    {
                        let val = to_display(st, v);
                        let attr =
                            format!("data-{}", camel_to_kebab(&text));
                        let doc = need_doc(st)?;
                        doc.borrow_mut().set_attr(
                            node as usize, &attr, &val);
                        continue;
                    }
                    if st.objects[oi].is_array {
                        if let Some(k) = elem_index(&text) {
                            // the numeric-string write path had no cap,
                            // so an index near 2^53 asked for a 72 PB
                            // Vec and Rust aborts the process on a
                            // failed allocation — a page could take the
                            // whole browser down with it
                            if k >= MAX_ARRAY_ELEMS {
                                return range_err(
                                    "array index exceeds the engine limit");
                            }
                            let elems = &mut st.objects[oi].elems;
                            if k < elems.len() {
                                elems[k] = v;
                            } else {
                                elems.resize(k, Value::UNDEFINED);
                                elems.push(v);
                            }
                            continue;
                        }
                    }
                    let key_id = st.intern_name(&text);
                    if ov == st.known.window {
                        st.globals[key_id as usize] = v;
                        st.gdef[key_id as usize] = true;
                    }
                    raw_set_prop(st, oi, key_id, v);
                } else if ov.is_function() {
                    let text = to_display(st, kv);
                    let key_id = st.intern_name(&text);
                    if key_id == st.ids.prototype {
                        st.fn_protos.insert(ov.index(), v);
                    } else {
                        st.fn_props.insert((ov.index(), key_id), v);
                    }
                } else if ov.is_dom_node() {
                    // el[expando] = v — same surface as SetProp
                    let text = to_display(st, kv);
                    let key_id = st.intern_name(&text);
                    dom_set_prop(st, key_id, ov.index(), v)?;
                } else if ov.is_object() {
                    // odd key type: ToPropertyKey (see GetIndex twin)
                    let kvp = to_primitive(st, mods, kv, false)?;
                    let text = to_display(st, kvp);
                    let key_id = st.intern_name(&text);
                    raw_set_prop(st, ov.index() as usize, key_id, v);
                } else if ov.is_nullish() {
                    let text = to_display(st, kv);
                    return type_err(format!(
                        "cannot set [{text}] of {}",
                        if ov.is_null() { "null" } else { "undefined" },
                    ));
                }
                // sloppy mode: computed writes to other primitives
                // are silently dropped
            }
            Instr::GetProp { dst, obj, atom, ic } => {
                let ov = reg!(obj);
                let key = name!(atom);
                if proxy_id_of(st, ov).is_some() {
                    reg!(dst) = internal_get(st, mods, ov, key, ov)?;
                    continue;
                }
                if ov.is_object() && !st.style_nodes.is_empty()
                    || ov.is_object() && !st.dataset_nodes.is_empty()
                {
                    // el.style.prop / el.dataset.k proxy reads
                    if let Some(&node) = st.style_nodes.get(&ov.index())
                    {
                        let name = st.names[key as usize].clone();
                        let doc = need_doc(st)?;
                        let cur = doc.borrow().nodes[node as usize]
                            .attr("style")
                            .unwrap_or("")
                            .to_string();
                        let out = if name == "cssText" {
                            cur
                        } else {
                            style_attr_get(
                                &cur, &camel_to_kebab(&name))
                        };
                        reg!(dst) = push_str(st, out);
                        continue;
                    }
                    if let Some(&node) =
                        st.dataset_nodes.get(&ov.index())
                    {
                        let name = st.names[key as usize].clone();
                        let attr =
                            format!("data-{}", camel_to_kebab(&name));
                        let doc = need_doc(st)?;
                        let out = doc.borrow().nodes[node as usize]
                            .attr(&attr)
                            .map(str::to_string);
                        reg!(dst) = match out {
                            Some(s) => push_str(st, s),
                            None => Value::UNDEFINED,
                        };
                        continue;
                    }
                }
                if ov.is_object() {
                    let oi = ov.index() as usize;
                    let (is_arr, shape, elen) = {
                        let o = &st.objects[oi];
                        (o.is_array, o.shape, o.elems.len())
                    };
                    if is_arr && key == st.ids.length {
                        reg!(dst) = Value::int(elen as i32);
                        continue;
                    }
                    // An own accessor (Object.defineProperty get/set) wins
                    // over any data slot of the same name — including one
                    // the shape still carries because the property used to
                    // be a plain value before it was redefined. The IC/slot
                    // fast path below reads the raw slot and cannot see the
                    // getter, so intercept here (cheap: gated on the flag).
                    if st.objects[oi].has_accessors {
                        if let Some(&(g, _s)) =
                            st.accessors.get(&(oi as u32, key))
                        {
                            reg!(dst) = if g.is_function() {
                                call_value_this(
                                    st, mods, g, Some(ov), &[],
                                )?
                            } else {
                                Value::UNDEFINED
                            };
                            continue;
                        }
                    }
                    let slot_ic = ic!(ic);
                    let e = st.ics[slot_ic];
                    let hit = if e.shape == shape {
                        PropHit::Data(st.objects[oi].slots[e.slot as usize])
                    } else {
                        match st.shapes[shape as usize].props.get(&key) {
                            Some(&slot) => {
                                st.ics[slot_ic] = IcEntry { shape, slot };
                                PropHit::Data(
                                    st.objects[oi].slots[slot as usize],
                                )
                            }
                            // own miss: accessor-aware chain walk
                            None => lookup_prop(st, oi, key),
                        }
                    };
                    reg!(dst) = match hit {
                        PropHit::Data(v) => v,
                        PropHit::Getter(g) => {
                            if g.is_function() {
                                call_value_this(
                                    st, mods, g, Some(ov), &[],
                                )?
                            } else {
                                Value::UNDEFINED
                            }
                        }
                        // arrays expose known builtins as extractable
                        // methods (`[].slice` — the core-js pattern)
                        PropHit::Missing
                            if is_arr
                                && matches!(
                                    st.names[key as usize].as_str(),
                                    "slice" | "concat" | "join"
                                        | "indexOf" | "push" | "pop"
                                        | "map" | "filter" | "forEach"
                                        | "sort" | "splice" | "shift"
                                        | "unshift" | "reverse" | "some"
                                        | "every" | "reduce"
                                        | "lastIndexOf" | "at" | "flat"
                                        | "flatMap" | "findLast"
                                        | "findLastIndex"
                                ) =>
                        {
                            memo_native(st, 1, key, 0, Native::MethodRef(key))
                        }
                        // core-js expandos on the JS-visible
                        // Array.prototype (@@iterator and friends)
                        PropHit::Missing
                            if is_arr
                                && !matches!(
                                    array_proto_hit(st, key),
                                    PropHit::Missing
                                ) =>
                        {
                            match array_proto_hit(st, key) {
                                PropHit::Data(v) => v,
                                PropHit::Getter(g) if g.is_function() => {
                                    call_value_this(
                                        st, mods, g, Some(ov), &[],
                                    )?
                                }
                                _ => Value::UNDEFINED,
                            }
                        }
                        // regex literals expose extractable exec/test
                        // (core-js: uncurryThis(/^0x/i.exec))
                        PropHit::Missing
                            if st.objects[oi].regex != REGEX_NONE
                                && matches!(
                                    st.names[key as usize].as_str(),
                                    "exec" | "test"
                                ) =>
                        {
                            memo_native(st, 1, key, 0, Native::MethodRef(key))
                        }
                        // promises expose then/catch as READABLE values:
                        // core-js's isThenable check is `var f = x.then`
                        // followed by `f.call(x, res, rej)` — a magic
                        // call-only method reads as undefined and makes
                        // every native promise look like a plain value,
                        // so adopted promises leak through unwrapped
                        PropHit::Missing
                            if st.objects[oi].promise != PROMISE_NONE
                                && matches!(
                                    st.names[key as usize].as_str(),
                                    "then" | "catch"
                                ) =>
                        {
                            memo_native(st, 1, key, 0, Native::MethodRef(key))
                        }
                        // a plain object's `toString` is the genuine
                        // Object.prototype.toString (brands); an
                        // array's stays MethodRef (join semantics)
                        PropHit::Missing
                            if !is_arr
                                && st.names[key as usize].as_str()
                                    == "toString" =>
                        {
                            make_native(st, Native::BrandToString)
                        }
                        // Object.prototype staples on any object
                        // (`{}.hasOwnProperty` — core-js hasOwn)
                        PropHit::Missing
                            if matches!(
                                st.names[key as usize].as_str(),
                                "hasOwnProperty" | "toString" | "valueOf"
                                    | "propertyIsEnumerable"
                                    | "isPrototypeOf"
                            ) =>
                        {
                            memo_native(st, 1, key, 0, Native::MethodRef(key))
                        }
                        // the window object doubles as the global
                        // namespace: `window.Number`/`window.parseInt`
                        PropHit::Missing if ov == st.known.window => {
                            let k = key as usize;
                            if st.gdef[k] {
                                st.globals[k]
                            } else {
                                Value::UNDEFINED
                            }
                        }
                        PropHit::Missing => Value::UNDEFINED,
                    };
                } else if ov.is_function() {
                    // functions expose a lazily-created .prototype,
                    // static props, and extractable call/apply/bind
                    reg!(dst) = if key == st.ids.prototype {
                        fn_prototype(st, ov)
                    } else if let Some(v) =
                        fn_static_lookup(st, ov.index(), key)
                    {
                        v
                    } else if key == st.ids.length {
                        Value::int(fn_arity(st, mods, ov))
                    } else if st.names[key as usize] == "name" {
                        let nm = fn_name(st, mods, ov);
                        make_string(st, nm)
                    } else if matches!(
                        st.names[key as usize].as_str(),
                        "call" | "apply" | "bind" | "toString"
                            | "valueOf"
                    ) {
                        memo_native(st, 1, key, 0, Native::MethodRef(key))
                    } else {
                        Value::UNDEFINED
                    };
                } else if ov.is_string() && key == st.ids.length {
                    let n = str_u16_len(st, ov.index());
                    reg!(dst) = Value::int(n as i32);
                } else if ov.is_string() || ov.is_number()
                    || ov.is_boolean()
                {
                    // method extraction (`''.slice`, `(1).toString`)
                    // for supported names; undefined for the rest
                    reg!(dst) = primitive_prop_read(st, ov, key);
                } else if ov.is_dom_node() {
                    let r = dom_get_prop(st, key, ov.index())?;
                    reg!(dst) = r;
                } else if ov.is_nullish() {
                    let trace = if std::env::var("GG_JS_TRACE").is_ok() {
                        let mut names: Vec<String> = st
                            .frames
                            .iter()
                            .rev()
                            .take(8)
                            .map(|f| {
                                format!(
                                    "{}@{}:{}",
                                    mods.rc(f.module).module.protos
                                        [f.proto as usize]
                                        .name,
                                    f.module,
                                    f.proto,
                                )
                            })
                            .collect();
                        let here = format!(
                            "{}@{}:{}",
                            cmod.module.protos[pi as usize].name, mi, pi,
                        );
                        names.insert(0, here);
                        // dump a window of consts around ip so the
                        // failing site can be found in the source: the
                        // last string const loaded names the object
                        let recent: Vec<String> = cmod.module.protos
                            [pi as usize]
                            .code[ip.saturating_sub(6)..ip]
                            .iter()
                            .filter_map(|instr| match instr {
                                Instr::GetProp { atom, .. }
                                | Instr::CallMethod { atom, .. } => Some(
                                    st.names[name!(*atom) as usize]
                                        .clone(),
                                ),
                                _ => None,
                            })
                            .collect();
                        // full instruction window so the undefined's
                        // origin (arg reg / global / upval) is visible
                        let lo = ip.saturating_sub(5);
                        let hi = (ip + 1)
                            .min(cmod.module.protos[pi as usize].code.len());
                        let dis: Vec<String> = cmod.module.protos
                            [pi as usize]
                            .code[lo..hi]
                            .iter()
                            .enumerate()
                            .map(|(k, i)| format!("{}:{:?}", lo + k, i))
                            .collect();
                        format!(
                            " [in {} | mi={} pi={} ip={} nparams={} \
                             | props: {} | {}]",
                            names.join(" <- "),
                            mi, pi, ip,
                            cmod.module.protos[pi as usize].nparams,
                            recent.join("."),
                            dis.join("  ")
                        )
                    } else {
                        String::new()
                    };
                    if let Ok(dir) = std::env::var("GG_JS_DUMP") {
                        let dump = |lbl: &str, m: u32, p: u32| {
                            let lm = mods.rc(m);
                            let proto = &lm.module.protos[p as usize];
                            let out = proto
                                .code
                                .iter()
                                .enumerate()
                                .map(|(k, i)| {
                                    format!(
                                        "{k}:{}",
                                        annotate_instr(
                                            st,
                                            &lm.global_map,
                                            &proto.consts,
                                            i,
                                        )
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            let _ = std::fs::write(
                                format!("{dir}/{lbl}_{m}_{p}.txt"),
                                out,
                            );
                        };
                        dump("cur", mi, pi);
                        for (n, f) in
                            st.frames.iter().rev().take(4).enumerate()
                        {
                            dump(&format!("c{n}"), f.module, f.proto);
                        }
                    }
                    return type_err(format!(
                        "cannot read .{} of {}{trace}",
                        st.names[key as usize],
                        if ov.is_null() { "null" } else { "undefined" },
                    ));
                } else {
                    return err(format!(
                        "cannot read .{} of {ov:?} (yet)",
                        st.names[key as usize]
                    ));
                }
            }
            Instr::SetProp { obj, atom, src, ic } => {
                let ov = reg!(obj);
                let key = name!(atom);
                if proxy_id_of(st, ov).is_some() {
                    let value = reg!(src);
                    let _ = internal_set(st, mods, ov, key, value, ov)?;
                    continue;
                }
                if ov.is_dom_node() {
                    let v = reg!(src);
                    dom_set_prop(st, key, ov.index(), v)?;
                    continue;
                }
                if ov.is_object() {
                    // el.style.prop = / el.dataset.k = proxies
                    if let Some(&node) = st.style_nodes.get(&ov.index())
                    {
                        let v = reg!(src);
                        let name = st.names[key as usize].clone();
                        let val = to_display(st, v);
                        let doc = need_doc(st)?;
                        if name == "cssText" {
                            doc.borrow_mut().set_attr(
                                node as usize, "style", &val);
                        } else {
                            let prop = camel_to_kebab(&name);
                            let cur = doc.borrow().nodes[node as usize]
                                .attr("style")
                                .unwrap_or("")
                                .to_string();
                            let next =
                                style_attr_set(&cur, &prop, &val);
                            doc.borrow_mut().set_attr(
                                node as usize, "style", &next);
                        }
                        continue;
                    }
                    if let Some(&node) =
                        st.dataset_nodes.get(&ov.index())
                    {
                        let v = reg!(src);
                        let name = st.names[key as usize].clone();
                        let val = to_display(st, v);
                        let attr =
                            format!("data-{}", camel_to_kebab(&name));
                        let doc = need_doc(st)?;
                        doc.borrow_mut().set_attr(
                            node as usize, &attr, &val);
                        continue;
                    }
                    // arr.length = n truncates (or grows with holes) the
                    // dense element storage — length is not a real slot.
                    let oi0 = ov.index() as usize;
                    if st.objects[oi0].is_array && key == st.ids.length {
                        let n = reg!(src).to_number_raw();
                        if n < 0.0 || n.fract() != 0.0 || n > u32::MAX as f64 {
                            return range_err("Invalid array length");
                        }
                        let newlen = n as usize;
                        if newlen > MAX_ARRAY_ELEMS
                            && newlen > st.objects[oi0].elems.len()
                        {
                            return range_err(
                                "array length exceeds the engine limit");
                        }
                        let elems = &mut st.objects[oi0].elems;
                        if newlen < elems.len() {
                            elems.truncate(newlen);
                        } else {
                            elems.resize(newlen, Value::UNDEFINED);
                        }
                        continue;
                    }
                }
                if ov.is_function() {
                    // F.prototype = {...} replaces the lazy prototype;
                    // anything else lands in the static-props table
                    let v = reg!(src);
                    if key == st.ids.prototype {
                        st.fn_protos.insert(ov.index(), v);
                    } else {
                        st.fn_props.insert((ov.index(), key), v);
                    }
                    continue;
                }
                if ov.is_nullish() {
                    // polyfills patch natives we do not have
                    // (`NativeProto.constructor = C`); ignore the write
                    continue;
                }
                if !ov.is_object() {
                    // sloppy mode: writes to primitive receivers are
                    // silently dropped ("str".x = 1 — jQuery trigger
                    // stamps .isTrigger on whatever it was passed)
                    continue;
                }
                let oi = ov.index() as usize;
                let v = reg!(src);
                // window doubles as the global namespace — a write
                // must be visible to bare-name reads too, or polyfills
                // that replace `window.Symbol` leave the old global
                // behind (split-brain Symbol broke core-js isSymbol)
                if ov == st.known.window {
                    let k = key as usize;
                    st.globals[k] = v;
                    st.gdef[k] = true;
                }
                // Property-attribute enforcement (sloppy mode: a blocked
                // write is a silent no-op). Gated on the side-tables being
                // non-empty so unfrozen objects skip the checks entirely.
                if !st.non_writable.is_empty()
                    && st.non_writable.contains(&(oi as u32, key))
                {
                    continue; // writable:false / frozen
                }
                if !st.non_extensible_objects.is_empty()
                    && st.non_extensible_objects.contains(&(oi as u32))
                {
                    let exists = st.shapes[st.objects[oi].shape as usize]
                        .props.contains_key(&key)
                        || (st.objects[oi].has_accessors
                            && st.accessors.contains_key(&(oi as u32, key)));
                    if !exists {
                        continue; // can't add new props to a sealed object
                    }
                }
                // An own accessor takes precedence over the data fast path:
                // invoke its setter, or drop the write (sloppy mode) when
                // the property is getter-only. Without this, assigning to a
                // getter-only property clobbers it into a plain data slot.
                if st.objects[oi].has_accessors {
                    if let Some(&(_g, s)) =
                        st.accessors.get(&(oi as u32, key))
                    {
                        if s.is_function() {
                            call_value_this(
                                st, mods, s, Some(ov), &[v],
                            )?;
                        }
                        continue;
                    }
                }
                let slot_ic = ic!(ic);
                let e = st.ics[slot_ic];
                if e.shape == st.objects[oi].shape {
                    st.objects[oi].slots[e.slot as usize] = v;
                } else {
                    let shape_id = st.objects[oi].shape;
                    if let Some(&slot) =
                        st.shapes[shape_id as usize].props.get(&key)
                    {
                        st.objects[oi].slots[slot as usize] = v;
                        st.ics[slot_ic] = IcEntry { shape: shape_id, slot };
                    } else if let Some(setter) =
                        lookup_setter(st, oi, key)
                    {
                        call_value_this(st, mods, setter, Some(ov), &[v])?;
                    } else {
                        raw_set_prop(st, oi, key, v);
                    }
                }
            }
            Instr::LoadThis { dst } => reg!(dst) = this_v,
            Instr::TypeOf { dst, src } => {
                let v = reg!(src);
                let k = if v.is_undefined() {
                    TY_UNDEFINED
                } else if v.is_boolean() {
                    TY_BOOLEAN
                } else if v.is_number() {
                    TY_NUMBER
                } else if v.is_string() {
                    TY_STRING
                } else if v.is_function() {
                    TY_FUNCTION
                } else {
                    TY_OBJECT // objects, arrays, null, DOM nodes
                };
                reg!(dst) = st.ty_names[k];
            }
        }
    }
}
