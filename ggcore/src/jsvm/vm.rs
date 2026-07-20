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
use std::collections::HashMap;
use std::rc::Rc;

use super::bytecode::{CapSrc, Instr, Module};
use super::value::Value;
use crate::dom;
use crate::html;
use crate::js::{find_tag, query, serialize_children};

pub struct VmError {
    pub msg: String,
    /// The JS value of an explicit `throw`; engine errors carry None
    /// and materialize as an Error-like object when a catch needs one.
    pub value: Option<Value>,
    /// Error class an engine-raised error materializes as ("Error",
    /// "TypeError", ...). Lets `catch (e)` see e.name/instanceof match
    /// what real JS would throw.
    pub kind: &'static str,
}

impl std::fmt::Debug for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error: {}", self.msg)
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError { msg: msg.into(), value: None, kind: "Error" })
}

/// An engine-raised error that real JS specifies as a TypeError
/// (member access on nullish, calling a non-function, ...).
fn type_err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError { msg: msg.into(), value: None, kind: "TypeError" })
}

/// As `type_err`, for ReferenceErrors (unresolved names, TDZ).
fn ref_err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError { msg: msg.into(), value: None, kind: "ReferenceError" })
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
}

#[derive(Clone, Copy)]
pub(super) enum Native {
    ConsoleLog,
    DateNow,
    Alert,
    /// accepts anything, returns undefined (window.addEventListener
    /// and friends — enough for feature-detecting bundles to proceed)
    Noop,
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
    PromiseReject,
    // --- host objects (P3b): Math/Object/Array/Number/String statics,
    // dispatched by id so one variant covers them all ---
    HostFn(u16),
    /// resolve/reject bound to a `new Promise(executor)` — settles the
    /// promise when called. Rides the ordinary Native call path (P3b).
    Resolve { pid: u32, reject: bool },
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
    pub const A_ISARRAY: u16 = 60;
    pub const A_FROM: u16 = 61;
    pub const N_ISNAN: u16 = 80;
    pub const N_ISFINITE: u16 = 81;
    pub const N_ISINTEGER: u16 = 82;
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
    /// Depth of nested native->JS re-entries (see MAX_NATIVE_DEPTH).
    native_depth: usize,
    /// Set by Native::PreventDefault during an event dispatch.
    pub(super) default_prevented: bool,
    pub(super) closures: Vec<ClosureRec>,
    cells: Vec<Value>,
    shapes: Vec<Shape>,
    pub(super) objects: Vec<Obj>,
    pub(super) strs: Vec<Str>,
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
    /// Web Storage backing maps (in-memory; not persisted to disk)
    pub(super) local_storage: HashMap<String, String>,
    pub(super) session_storage: HashMap<String, String>,
    /// document.cookie pairs in insertion order (in-memory; not yet
    /// wired to the network layer)
    pub(super) cookies: Vec<(String, String)>,
    /// node index -> (x, y, w, h) in document coordinates, pushed by
    /// the shell after each layout so getBoundingClientRect answers
    /// real geometry (document-origin approximation: scroll offset is
    /// not subtracted)
    pub(super) layout_rects: HashMap<u32, (f64, f64, f64, f64)>,
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
    /// FIFO microtask queue (promise reactions + queueMicrotask jobs).
    pub(super) microtasks: std::collections::VecDeque<Job>,
    /// pending timers, fired in (due_ms, seq) order by the pump.
    pub(super) timers: Vec<Timer>,
    next_timer_id: u32,
    timer_seq: u64,
    /// virtual clock (ms). Timers advance it; Date.now reads it — so a
    /// headless run is deterministic and never sleeps.
    pub(super) now_ms: f64,
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
}

/// Default per-turn instruction budget (~a few hundred ms of hot loop).
/// Generous enough for any real page script, small enough that a
/// runaway loop dies fast. The host can lower it per session.
// Per-script instruction budget. 80M stopped Naver's app mid-boot once
// the bundles started doing real work (07-20) — 400M keeps the runaway
// backstop while letting a genuine app initialize; the wall-clock
// js_budget in native.load_document still bounds the whole load.
pub(super) const DEFAULT_FUEL: u64 = 400_000_000;

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
            native_depth: 0,
            default_prevented: false,
            closures: Vec::new(),
            cells: Vec::new(),
            shapes: vec![Shape {
                props: HashMap::new(),
                transitions: HashMap::new(),
            }],
            objects: Vec::new(),
            strs: Vec::new(),
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
            local_storage: HashMap::new(),
            session_storage: HashMap::new(),
            cookies: Vec::new(),
            layout_rects: HashMap::new(),
            map_data: HashMap::new(),
            set_data: HashMap::new(),
            style_nodes: HashMap::new(),
            dataset_nodes: HashMap::new(),
            fn_proto_chain: HashMap::new(),
            ready_state: "loading",
            ty_names: [Value::UNDEFINED; 6],
            regexes: Vec::new(),
            promises: Vec::new(),
            microtasks: std::collections::VecDeque::new(),
            timers: Vec::new(),
            next_timer_id: 0,
            timer_seq: 0,
            now_ms: 0.0,
            pending_fetches: Vec::new(),
            awaiting: HashMap::new(),
            next_fetch_id: 0,
            fetch_body_atom: u32::MAX,
            text_atom: u32::MAX,
            json_atom: u32::MAX,
            rng_state: 0x2545_F491_4F6C_DD1D,
            fuel: DEFAULT_FUEL,
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
    if let Some(i) = st
        .strs
        .iter()
        .position(|x| matches!(x, Str::Flat(f) if f == s))
    {
        return Value::string(i as u32);
    }
    st.strs.push(Str::Flat(s.to_string()));
    Value::string((st.strs.len() - 1) as u32)
}

pub(super) fn push_str(st: &mut St, s: String) -> Value {
    st.strs.push(Str::Flat(s));
    Value::string((st.strs.len() - 1) as u32)
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

/// Build a RegExp value from a JS pattern + flags. JS flags map to the
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
    let full = if inline.is_empty() {
        pattern.to_string()
    } else {
        format!("(?{inline}){pattern}")
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
}

pub(super) struct PromiseRec {
    state: PromiseState,
    on_fulfill: Vec<Reaction>,
    on_reject: Vec<Reaction>,
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
    },
}

pub(super) struct Timer {
    id: u32,
    callback: Value,
    args: Vec<Value>,
    due_ms: f64,
    seq: u64,
    interval: Option<f64>,
}

pub(super) struct PendingFetch {
    pub(super) fetch_id: u32,
    promise: u32,
    pub(super) url: String,
}

fn new_promise(st: &mut St) -> (Value, u32) {
    let v = new_plain_object(st);
    st.promises.push(PromiseRec {
        state: PromiseState::Pending,
        on_fulfill: Vec::new(),
        on_reject: Vec::new(),
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
                is_reject: false,
            });
        }
        PromiseState::Rejected(v) if reject_side => {
            st.microtasks.push_back(Job::React {
                handler: rx.handler, value: v, derived: rx.derived,
                is_reject: true,
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
        add_reaction(st, inner, false, Reaction { handler: None, derived: pid });
        add_reaction(st, inner, true, Reaction { handler: None, derived: pid });
        return;
    }
    st.promises[pid as usize].state = if is_reject {
        PromiseState::Rejected(value)
    } else {
        PromiseState::Fulfilled(value)
    };
    // schedule the matching reaction set (the other set never runs)
    let reactions = if is_reject {
        std::mem::take(&mut st.promises[pid as usize].on_reject)
    } else {
        std::mem::take(&mut st.promises[pid as usize].on_fulfill)
    };
    for rx in reactions {
        st.microtasks.push_back(Job::React {
            handler: rx.handler, value, derived: rx.derived, is_reject,
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
    add_reaction(st, recv_pid, false, Reaction { handler: on_f, derived: dpid });
    add_reaction(st, recv_pid, true, Reaction { handler: on_r, derived: dpid });
    dval
}

fn make_string(st: &mut St, s: String) -> Value {
    push_str(st, s)
}

/// Build a fetch Response object: { ok, status, url, text(), json() }.
/// The body is stored under a hidden atom so text()/json() can find it.
fn new_response(st: &mut St, status: u16, url: &str, body: String) -> Value {
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
    v
}

/// Drain the microtask queue to empty, running each reaction/callback.
/// Callback errors reject the derived promise (never escape the pump).
fn drain_microtasks(st: &mut St, mods: &ModStore, budget: &mut usize) {
    while let Some(job) = st.microtasks.pop_front() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        st.fuel = DEFAULT_FUEL; // fresh instruction budget per reaction
        match job {
            Job::Call { callback, args } => {
                if let Err(e) = call_value(st, mods, callback, &args) {
                    st.logs.push(format!("[gg-js error] {}", e.msg));
                }
            }
            Job::React { handler, value, derived, is_reject } => match handler {
                None => promise_settle(st, derived, value, is_reject),
                Some(h) => match call_value(st, mods, h, &[value]) {
                    // a handler that returns normally FULFILLS the derived
                    // promise (even an onRejected handler — the rejection
                    // is considered handled)
                    Ok(ret) => promise_settle(st, derived, ret, false),
                    Err(e) => {
                        let reason = e.value.unwrap_or_else(|| {
                            let s = e.msg.clone();
                            make_string(st, s)
                        });
                        promise_settle(st, derived, reason, true);
                    }
                },
            },
        }
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
/// (advancing the virtual clock), repeat until both are empty or the
/// budget is hit. Returns the fetches issued this turn for the host to
/// service; they move to `awaiting` until resolve_fetch settles them.
pub(super) fn pump(
    st: &mut St,
    mods: &ModStore,
    budget_max: usize,
) -> Vec<(u32, String)> {
    let mut budget = budget_max;
    loop {
        drain_microtasks(st, mods, &mut budget);
        if budget == 0 {
            st.logs.push("[gg-js] event-loop budget exceeded".to_string());
            break;
        }
        match next_due_timer(st) {
            None => break,
            Some(i) => {
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
                    });
                }
                budget -= 1;
                st.fuel = DEFAULT_FUEL; // fresh budget per timer callback
                if let Err(e) = call_value(st, mods, t.callback, &t.args) {
                    st.logs.push(format!("[gg-js error] {}", e.msg));
                }
            }
        }
    }
    let issued = std::mem::take(&mut st.pending_fetches);
    for p in &issued {
        st.awaiting.insert(p.fetch_id, p.promise);
    }
    issued.into_iter().map(|p| (p.fetch_id, p.url)).collect()
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
) -> Vec<(u32, String)> {
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
                    });
                }
                budget -= 1;
                st.fuel = DEFAULT_FUEL;
                if let Err(e) = call_value(st, mods, t.callback, &t.args)
                {
                    st.logs.push(format!("[gg-js error] {}", e.msg));
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
    issued.into_iter().map(|p| (p.fetch_id, p.url)).collect()
}

/// Host (driver) settles a fetch: fulfill its promise with a Response.
pub(super) fn resolve_fetch(st: &mut St, fetch_id: u32, status: u16, body: String) {
    if let Some(pid) = st.awaiting.remove(&fetch_id) {
        // find the url of this fetch for the Response (best-effort)
        let resp = new_response(st, status, "", body);
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
    !st.microtasks.is_empty()
        || !st.timers.is_empty()
        || !st.pending_fetches.is_empty()
        || !st.awaiting.is_empty()
}

fn str_len(st: &St, i: u32) -> usize {
    match &st.strs[i as usize] {
        Str::Flat(s) => s.len(),
        Str::Cat { len, .. } => *len as usize,
    }
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
    st.strs[i as usize] = Str::Flat(out);
}

fn str_ref(st: &mut St, i: u32) -> &str {
    flatten(st, i);
    match &st.strs[i as usize] {
        Str::Flat(s) => s.as_str(),
        Str::Cat { .. } => unreachable!(),
    }
}

fn raw_get_prop(st: &St, oi: usize, key: u32) -> Option<Value> {
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
    if !v.is_object() {
        return Ok(v);
    }
    let order = if number_hint {
        ["valueOf", "toString"]
    } else {
        ["toString", "valueOf"]
    };
    for name in order {
        let key = st.intern_name(name);
        let m = match lookup_prop(st, v.index() as usize, key) {
            PropHit::Data(f) => f,
            PropHit::Getter(g) => {
                call_value_this(st, mods, g, Some(v), &[])?
            }
            PropHit::Missing => continue,
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
    if let Ok(i) = name.parse::<usize>() {
        if i < st.objects[oi].elems.len() {
            return Ok(true);
        }
    }
    // an un-interned name cannot be a property of anything
    let Some(&key_id) = st.name_ids.get(&name) else {
        return Ok(false);
    };
    let shape = st.objects[oi].shape as usize;
    Ok(st.shapes[shape].props.contains_key(&key_id))
}

/// Invoke an extracted builtin (`var f = ''.slice; f.call(s, 1)`).
/// Covers the methods polyfills actually extract; anything else names
/// itself in the error so the next gap is visible.
fn method_ref_dispatch(
    st: &mut St,
    mods: &ModStore,
    recv: Value,
    key: u32,
    args: &[Value],
) -> Result<Value, VmError> {
    let name = st.names[key as usize].clone();
    // array receivers: real element operations, not string ops
    if recv.is_object() && st.objects[recv.index() as usize].is_array {
        let elems = st.objects[recv.index() as usize].elems.clone();
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
                for (i, &e) in elems.iter().enumerate() {
                    let r = call_value(
                        st, mods, cb,
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
                let mut out: Vec<Value> = Vec::new();
                for (i, &el) in elems.iter().enumerate() {
                    let r = call_value(
                        st, mods, cb,
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
            let rep = args
                .get(1)
                .map(|&v| to_display(st, v))
                .unwrap_or_default();
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
                raw.into_iter()
                    .map(|p| make_string(st, p))
                    .collect()
            } else if pat.is_undefined() {
                vec![make_string(st, s.clone())]
            } else {
                let needle = to_display(st, pat);
                if needle.is_empty() {
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
            Ok(Value::boolean(
                recv.is_object() && has_own_property(st, mods, recv, k)?,
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
            Ok(Value::boolean(
                recv.is_object() && has_own_property(st, mods, recv, k)?,
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
                        .into_iter()
                        .map(|g| match g {
                            Some(s) => push_str(st, s),
                            None => Value::UNDEFINED,
                        })
                        .collect();
                    new_array(st, vals)
                }
            })
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
    let f = match lookup_prop(st, oi as usize, itk) {
        PropHit::Data(f) if f.is_function() => f,
        _ => return Ok(ov), // no protocol: existing behavior
    };
    let iter = call_value_this(st, mods, f, Some(ov), &[])?;
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
                | "toLowerCase" | "toUpperCase" | "match" | "search"
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
        return make_native(st, Native::MethodRef(key));
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

/// Object.prototype.toString brand of a value ("[object Array]" ...).
fn brand_string(st: &St, v: Value) -> String {
    let tag = if v.is_object() {
        let o = &st.objects[v.index() as usize];
        if o.is_array {
            "Array"
        } else if o.regex != REGEX_NONE {
            "RegExp"
        } else if o.promise != PROMISE_NONE {
            "Promise"
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
fn annotate_instr(
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
    if let Ok(i) = name.parse::<usize>() {
        if i < st.objects[oi].elems.len() {
            st.objects[oi].elems[i] = Value::UNDEFINED;
        }
        return true;
    }
    let Some(&key_id) = st.name_ids.get(&name) else {
        return true;
    };
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

/// `x instanceof Ctor` — built-ins matched by constructor identity.
/// User functions yield false: `new` is lowered to a plain object +
/// `Ctor.call`, so instances carry no link back to their constructor.
fn instance_of(st: &St, x: Value, ctor: Value) -> Result<bool, VmError> {
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
        "function".to_string()
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

fn to_str_idx(st: &mut St, v: Value) -> u32 {
    if v.is_string() {
        return v.index();
    }
    let s = to_display(st, v);
    st.strs.push(Str::Flat(s));
    (st.strs.len() - 1) as u32
}

fn concat(st: &mut St, x: Value, y: Value) -> Value {
    let a = to_str_idx(st, x);
    let b = to_str_idx(st, y);
    let len = str_len(st, a) + str_len(st, b);
    // small results stay flat: rope nodes only pay off on big strings
    if len <= 64 {
        let sa = str_ref(st, a).to_string();
        let sb = str_ref(st, b);
        let s = format!("{sa}{sb}");
        st.strs.push(Str::Flat(s));
    } else {
        st.strs.push(Str::Cat { a, b, len: len as u32 });
    }
    Value::string((st.strs.len() - 1) as u32)
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
            // optional iterable seed: array of pairs (Map) / values
            if argc > 0 {
                let seed = st.regs[args_base];
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
        Native::FunctionCtor => Ok(make_native(st, Native::ReturnGlobal)),
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
            let s = to_display(st, st.regs[args_base]);
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
        Native::ParseInt => {
            if argc == 0 {
                return Ok(Value::number(f64::NAN));
            }
            let s = to_display(st, st.regs[args_base]);
            let radix = if argc >= 2 {
                num_of(st.regs[args_base + 1])? as u32
            } else {
                10
            };
            let radix = if radix == 0 { 10 } else { radix };
            if !(2..=36).contains(&radix) {
                // spec: invalid radix -> NaN (and is_digit(r>36) panics)
                return Ok(Value::number(f64::NAN));
            }
            let t = s.trim_matches(|c: char| {
                c.is_whitespace() || c == '\u{feff}'
            });
            let (neg, digits) = match t.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, t.strip_prefix('+').unwrap_or(t)),
            };
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
            // `undefined`/function serialize to nothing at the top level.
            Ok(match json_stringify(st, v, &mut Vec::new())? {
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
                if handler.is_function() {
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
            });
            Ok(Value::int(id as i32))
        }
        Native::PerfNow => Ok(Value::number(st.now_ms)),
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
            let (pval, pid) = new_promise(st);
            st.next_fetch_id += 1;
            let fid = st.next_fetch_id;
            st.pending_fetches.push(PendingFetch {
                fetch_id: fid,
                promise: pid,
                url,
            });
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
                return err("Unexpected token in JSON");
            }
            Ok(v)
        }
    }
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
    let g = raw_get_prop(st, di, get_id).unwrap_or(Value::UNDEFINED);
    let s = raw_get_prop(st, di, set_id).unwrap_or(Value::UNDEFINED);
    if g.is_function() || s.is_function() {
        st.accessors.insert((oi as u32, key), (g, s));
        st.objects[oi].has_accessors = true;
    } else if let Some(v) = raw_get_prop(st, di, val_id) {
        // defineProperty(window, ...) must reach bare-name reads too
        // (core-js defineGlobalProperty installs polyfills this way)
        if obj == st.known.window {
            st.globals[key as usize] = v;
            st.gdef[key as usize] = true;
        }
        raw_set_prop(st, oi, key, v);
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
        M_SIGN => m1!(f64::signum),
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
            if !v.is_object() {
                return Ok(new_array(st, Vec::new()));
            }
            let oi = v.index() as usize;
            let (is_array, nelems, shape) = {
                let o = &st.objects[oi];
                (o.is_array, o.elems.len(), o.shape)
            };
            let mut pairs: Vec<(u16, u32)> = st.shapes[shape as usize]
                .props.iter().map(|(&a, &s)| (s, a)).collect();
            pairs.sort_by_key(|&(slot, _)| slot);
            let mut out = Vec::new();
            // array index keys first (like for-in)
            for k in 0..nelems {
                let key = intern(st, &k.to_string());
                let val = st.objects[oi].elems[k];
                out.push(host_entry(st, id, key, val));
            }
            for (slot, atom) in pairs {
                let name = st.names[atom as usize].clone();
                let key = intern(st, &name);
                let val = st.objects[oi].slots[slot as usize];
                out.push(host_entry(st, id, key, val));
            }
            let _ = is_array;
            Ok(new_array(st, out))
        }
        O_ASSIGN => {
            let target = argv!(0);
            if !target.is_object() {
                return Ok(target);
            }
            let ti = target.index() as usize;
            for k in 1..n {
                let src = argv!(k);
                if !src.is_object() {
                    continue;
                }
                let si = src.index() as usize;
                let shape = st.objects[si].shape;
                let mut pairs: Vec<(u16, u32)> = st.shapes[shape as usize]
                    .props.iter().map(|(&a, &s)| (s, a)).collect();
                pairs.sort_by_key(|&(slot, _)| slot);
                for (slot, atom) in pairs {
                    let val = st.objects[si].slots[slot as usize];
                    raw_set_prop(st, ti, atom, val);
                }
            }
            Ok(target)
        }
        O_FREEZE => Ok(argv!(0)), // no-op (we don't enforce immutability)
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
            define_one_prop(st, obj, key, argv!(2))?;
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
            let shape = st.objects[di].shape;
            let mut pairs: Vec<(u16, u32)> = st.shapes[shape as usize]
                .props.iter().map(|(&a, &s)| (s, a)).collect();
            pairs.sort_by_key(|&(slot, _)| slot);
            for (slot, atom) in pairs {
                let desc = st.objects[di].slots[slot as usize];
                define_one_prop(st, obj, atom, desc)?;
            }
            Ok(obj)
        }
        O_GET_OWN_NAMES => {
            // own names = index keys + shape props + accessor-only
            // keys (Object.keys skips the accessor side-table)
            let v = argv!(0);
            if v.is_function() {
                let fidx = v.index();
                let mut keys: Vec<u32> = st.fn_props.keys()
                    .filter(|&&(f, _)| f == fidx)
                    .map(|&(_, k)| k).collect();
                keys.sort_unstable();
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
            let (nelems, shape, has_acc) = {
                let o = &st.objects[oi];
                (o.elems.len(), o.shape, o.has_accessors)
            };
            let mut out = Vec::new();
            for k in 0..nelems {
                out.push(intern(st, &k.to_string()));
            }
            let mut pairs: Vec<(u16, u32)> = st.shapes[shape as usize]
                .props.iter().map(|(&a, &s)| (s, a)).collect();
            pairs.sort_by_key(|&(slot, _)| slot);
            let mut seen: Vec<u32> = Vec::new();
            for (_, atom) in pairs {
                seen.push(atom);
                let name = st.names[atom as usize].clone();
                out.push(intern(st, &name));
            }
            if has_acc {
                let mut acc: Vec<u32> = st.accessors.keys()
                    .filter(|&&(o, _)| o == oi as u32)
                    .map(|&(_, k)| k)
                    .filter(|k| !seen.contains(k))
                    .collect();
                acc.sort_unstable();
                for k in acc {
                    let name = st.names[k as usize].clone();
                    out.push(intern(st, &name));
                }
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
            if obj.is_function() {
                // builtin-constructor property copying (core-js):
                // answer for statics; prototype only on user functions
                let key_name = to_display(st, argv!(1));
                let Some(&key) = st.name_ids.get(&key_name) else {
                    return Ok(Value::UNDEFINED);
                };
                let v = if key == st.ids.prototype {
                    match st.closures[obj.index() as usize] {
                        ClosureRec::User { .. } => {
                            Some(fn_prototype(st, obj))
                        }
                        _ => None, // native bind etc: spec says none
                    }
                } else {
                    st.fn_props.get(&(obj.index(), key)).copied()
                };
                let Some(v) = v else {
                    return Ok(Value::UNDEFINED);
                };
                let out = new_plain_object(st);
                let pi = out.index() as usize;
                let t = Value::boolean(true);
                let vid = st.intern_name("value");
                let wid = st.intern_name("writable");
                let eid = st.intern_name("enumerable");
                let cid = st.intern_name("configurable");
                raw_set_prop(st, pi, vid, v);
                raw_set_prop(st, pi, wid, t);
                raw_set_prop(st, pi, eid, t);
                raw_set_prop(st, pi, cid, t);
                return Ok(out);
            }
            if !obj.is_object() {
                return Ok(Value::UNDEFINED);
            }
            let oi = obj.index() as usize;
            let key_name = to_display(st, argv!(1));
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
            let t = Value::boolean(true);
            if let Some((g, s)) = acc {
                let gid = st.intern_name("get");
                let sid = st.intern_name("set");
                raw_set_prop(st, pi, gid, g);
                raw_set_prop(st, pi, sid, s);
            } else if let Some(v) = own {
                let vid = st.intern_name("value");
                let wid = st.intern_name("writable");
                raw_set_prop(st, pi, vid, v);
                raw_set_prop(st, pi, wid, t);
            } else {
                return Ok(Value::UNDEFINED);
            }
            let eid = st.intern_name("enumerable");
            let cid = st.intern_name("configurable");
            raw_set_prop(st, pi, eid, t);
            raw_set_prop(st, pi, cid, t);
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
            if v.is_object() {
                st.objects[v.index() as usize].proto =
                    if p.is_object() { p } else { Value::UNDEFINED };
            } else if v.is_function() {
                // static inheritance (Babel: setPrototypeOf(Sub, Sup))
                st.fn_proto_chain.insert(v.index(), p);
            }
            Ok(v)
        }
        A_ISARRAY => {
            let v = argv!(0);
            Ok(Value::boolean(
                v.is_object() && st.objects[v.index() as usize].is_array,
            ))
        }
        A_FROM => {
            let v = argv!(0);
            let items: Vec<Value> = if v.is_object()
                && st.objects[v.index() as usize].is_array
            {
                st.objects[v.index() as usize].elems.clone()
            } else if v.is_string() {
                let s = str_ref(st, v.index()).to_string();
                s.chars().map(|c| push_str(st, c.to_string())).collect()
            } else {
                Vec::new()
            };
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
fn json_stringify(
    st: &mut St,
    v: Value,
    seen: &mut Vec<u32>,
) -> Result<Option<String>, VmError> {
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
        let out = if st.objects[oi].is_array {
            let elems = st.objects[oi].elems.clone();
            let mut parts = Vec::with_capacity(elems.len());
            for e in elems {
                parts.push(json_stringify(st, e, seen)?
                    .unwrap_or_else(|| "null".to_string()));
            }
            format!("[{}]", parts.join(","))
        } else {
            let shape = st.objects[oi].shape;
            let mut pairs: Vec<(u16, u32)> = st.shapes[shape as usize]
                .props.iter().map(|(&a, &s)| (s, a)).collect();
            pairs.sort_by_key(|&(slot, _)| slot);
            let mut parts = Vec::new();
            for (slot, atom) in pairs {
                let val = st.objects[oi].slots[slot as usize];
                if let Some(vs) = json_stringify(st, val, seen)? {
                    let key = st.names[atom as usize].clone();
                    parts.push(format!("{}:{}", json_quote(&key), vs));
                }
            }
            format!("{{{}}}", parts.join(","))
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
                return err("JSON input too deeply nested");
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
        _ => err("Unexpected token in JSON"),
    }
}

fn json_parse_lit(p: &mut JsonP, word: &str, v: Value) -> Result<Value, VmError> {
    for want in word.chars() {
        if p.bump() != Some(want) {
            return err("Unexpected token in JSON");
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
        Err(_) => err("Invalid number in JSON"),
    }
}

fn json_parse_string(p: &mut JsonP) -> Result<String, VmError> {
    p.bump(); // opening quote
    let mut s = String::new();
    loop {
        match p.bump() {
            None => return err("Unterminated string in JSON"),
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
                            None => return err("Bad \\u escape in JSON"),
                        }
                    }
                    s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                _ => return err("Bad escape in JSON"),
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
            _ => return err("Expected , or ] in JSON"),
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
            return err("Expected string key in JSON");
        }
        let key = json_parse_string(p)?;
        p.ws();
        if p.bump() != Some(':') {
            return err("Expected : in JSON");
        }
        let val = json_parse(st, p)?;
        let key_id = st.intern_name(&key);
        raw_set_prop(st, oi, key_id, val);
        p.ws();
        match p.bump() {
            Some(',') => continue,
            Some('}') => return Ok(obj),
            _ => return err("Expected , or } in JSON"),
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
        None => err("no document attached to this VM"),
    }
}

/// DOM method dispatch (`document.x(...)` and element methods).
fn dom_method(
    st: &mut St,
    mods: &ModStore,
    key: u32,
    node: u32,
    args_base: usize,
    argc: u8,
) -> Result<Value, VmError> {
    let _ = mods; // only event dispatch re-enters JS
    let ids = st.ids;
    let doc = need_doc(st)?;
    // addEventListener works on document too (listeners are keyed by
    // (node, type); DOC_NODE is just another key)
    if key == ids.add_event_listener {
        let ty = arg_string(st, args_base, argc, 0)?.to_lowercase();
        let handler = if argc >= 2 {
            st.regs[args_base + 1]
        } else {
            Value::UNDEFINED
        };
        if handler.is_function() {
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
    // shared by document and elements: descendant collection by tag
    // or class (jQuery fast paths)
    {
        let by_tag = st.names[key as usize] == "getElementsByTagName";
        if by_tag || st.names[key as usize] == "getElementsByClassName"
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
            let mut stack: Vec<usize> =
                d.nodes[start].children.iter().rev().copied().collect();
            while let Some(i) = stack.pop() {
                let hit = if by_tag {
                    (tag == "*" && d.nodes[i].is_element())
                        || d.nodes[i].tag.as_deref() == Some(tag.as_str())
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
            let idx =
                doc.borrow_mut().new_element(tag, Vec::new(), None);
            return Ok(Value::dom_node(idx as u32));
        }
        match st.names[key as usize].as_str() {
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
            return Ok(Value::UNDEFINED);
        }
        if key == ids.add_event_listener {
            let ty = arg_string(st, args_base, argc, 0)?.to_lowercase();
            let handler = if argc >= 2 {
                st.regs[args_base + 1]
            } else {
                Value::UNDEFINED
            };
            if handler.is_function() {
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
        "scrollIntoView" | "focus" | "blur" | "scrollTo" | "scrollBy"
        | "setAttributeNS" | "closest" => {
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
                    call_value_this(
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
            return Ok(make_native(st, Native::DomMethod { node, key }));
        }
        return Ok(Value::UNDEFINED);
    }
    let node_us = node as usize;
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
            return Ok(match doc.borrow().nodes[node_us].parent {
                Some(p) => Value::dom_node(p as u32),
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
        "firstChild" | "lastChild" => {
            let first = st.names[key as usize] == "firstChild";
            let d = doc.borrow();
            let kids = &d.nodes[node_us].children;
            let pick = if first { kids.first() } else { kids.last() };
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
            // NamedNodeMap snapshot: named + indexed access, each
            // entry an Attr-ish record (jQuery probes .expando)
            let attrs = doc.borrow().nodes[node_us].attrs.clone();
            let obj = new_plain_object(st);
            let oi = obj.index() as usize;
            let lenk = st.intern_name("length");
            raw_set_prop(st, oi, lenk,
                         Value::int(attrs.len() as i32));
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
                let kk = st.intern_name(an);
                raw_set_prop(st, oi, kk, item);
                st.objects[oi].elems.push(item);
            }
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
        _ => {}
    }
    if is_dom_method_name(st.names[key as usize].as_str())
        && !is_doc_only_method(st.names[key as usize].as_str())
    {
        return Ok(make_native(st, Native::DomMethod { node, key }));
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
            | "createTextNode"
            | "createComment"
            | "createDocumentFragment"
            | "getElementById"
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
            | "createTextNode"
            | "createComment"
            | "createDocumentFragment"
            | "querySelector"
            | "querySelectorAll"
            | "getElementsByTagName"
            | "getElementsByClassName"
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
            // "k=v; Path=/; ..." — the first pair is the cookie,
            // attributes are accepted and ignored (in-memory store)
            let text = to_display(st, v);
            let first = text.split(';').next().unwrap_or("");
            if let Some((k, val)) = first.split_once('=') {
                let (k, val) = (k.trim().to_string(),
                                val.trim().to_string());
                if !k.is_empty() {
                    if let Some(slot) = st
                        .cookies
                        .iter_mut()
                        .find(|(ck, _)| *ck == k)
                    {
                        slot.1 = val;
                    } else {
                        st.cookies.push((k, val));
                    }
                }
            }
            return Ok(());
        }
        st.dom_expando.insert((node, key), v);
        return Ok(());
    }
    let node_us = node as usize;
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
        n if n.starts_with("on") => Ok(()), // handler props: accepted
        "nodeValue" | "data" => {
            let value = to_display(st, v);
            let mut d = doc.borrow_mut();
            if d.nodes[node_us].tag.is_none() {
                d.nodes[node_us].text = value;
                d.version += 1;
            }
            Ok(())
        }
        "scrollTop" | "scrollLeft" | "selected" | "checked"
        | "disabled" | "hidden" | "draggable"
        | "contentEditable" | "async" | "defer" | "crossOrigin"
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
    match &st.closures[idx as usize] {
        ClosureRec::Bound { .. } => unreachable!("handled above"),
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
    let (mut mi, mut pi, mut ip) = (mi0, pi0, 0usize);
    let (mut base, mut cl, mut this_v) = (base0, cl0, this0);
    let mut cur_argc = argc0;
    loop {
        let r = exec_loop(
            st, mods, mi, pi, ip, base, cl, this_v, floor, cur_argc,
        );
        let e = match r {
            Ok(v) => {
                // Handlers armed by this activation must not outlive it
                // (a callback can return from inside `try` at `floor`,
                // where no Return cleanup runs).
                st.handlers.truncate(hfloor);
                return Ok(v);
            }
            Err(e) => e,
        };
        if st.handlers.len() <= hfloor {
            // Uncaught here. Unwind frames pushed by this activation: a
            // thrown error must not leak frames into the persistent VM
            // (each leak permanently shrinks headroom until every call
            // fails with "stack overflow").
            st.frames.truncate(floor);
            st.handlers.truncate(hfloor);
            return Err(e);
        }
        // Resume at the innermost armed catch with the thrown value.
        let h = st.handlers.pop().unwrap();
        st.frames.truncate(h.depth);
        let exc = exception_value(st, e);
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
fn exception_value(st: &mut St, e: VmError) -> Value {
    if let Some(v) = e.value {
        return v;
    }
    let obj = new_plain_object(st);
    let oi = obj.index() as usize;
    let name_id = st.intern_name("name");
    let msg_id = st.intern_name("message");
    let n = intern(st, e.kind);
    raw_set_prop(st, oi, name_id, n);
    let m = push_str(st, e.msg);
    raw_set_prop(st, oi, msg_id, m);
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
fn throw_msg(st: &mut St, v: Value) -> String {
    if v.is_object() {
        let oi = v.index() as usize;
        let name_id = st.intern_name("name");
        let msg_id = st.intern_name("message");
        if let (Some(n), Some(m)) =
            (raw_get_prop(st, oi, name_id), raw_get_prop(st, oi, msg_id))
        {
            let n = to_display(st, n);
            let m = to_display(st, m);
            return format!("uncaught {n}: {m}");
        }
    }
    let d = to_display(st, v);
    format!("uncaught {d}")
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
    argc0: u8,
) -> Result<Value, VmError> {
    let mut mi = mi0;
    let mut pi = pi0;
    let mut ip = ip0;
    let mut base = base0;
    let mut cur_cl = cl0;
    let mut this_v = this0;
    let mut cur_argc = argc0;
    let mut cmod: Rc<LoadedModule> = mods.rc(mi);

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
        st.fuel -= 1;
        let instr = cmod.module.protos[pi as usize].code[ip];
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
                });
            }
            Instr::PopHandler => {
                st.handlers.pop();
            }
            Instr::Throw { src } => {
                let v = reg!(src);
                let msg = throw_msg(st, v);
                return Err(VmError { msg, value: Some(v), kind: "Error" });
            }
            Instr::SetGlobal { atom, src } => {
                let key = name!(atom) as usize;
                st.globals[key] = reg!(src);
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
                    concat(st, x, y)
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
                let (x, y) = (reg!(a), reg!(b));
                reg!(dst) = if x.is_object() { x } else { y };
            }
            Instr::Delete { dst, obj, key } => {
                let (ov, kv) = (reg!(obj), reg!(key));
                let r = delete_property(st, ov, kv);
                reg!(dst) = Value::boolean(r);
            }
            Instr::In { dst, a, b } => {
                let (key, obj) = (reg!(a), reg!(b));
                let r = has_own_property(st, mods, obj, key)?;
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
                        "{fv:?} is not a function{hint}"
                    ));
                }
                if matches!(
                    st.closures[fv.index() as usize],
                    ClosureRec::Bound { .. }
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
                        let from = if callee.uses_arguments {
                            (argc as usize).max(callee.nparams as usize)
                        } else {
                            (argc as usize).min(callee.nparams as usize)
                        };
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
                        });
                        mi = cm;
                        pi = cp;
                        ip = 0;
                        base = new_base;
                        cur_cl = cl_idx;
                        cur_argc = argc;
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
                    return type_err(format!("{fv:?} is not a function"));
                }
                if matches!(
                    st.closures[fv.index() as usize],
                    ClosureRec::Bound { .. }
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
                        let from = if callee.uses_arguments {
                            (argc as usize).max(callee.nparams as usize)
                        } else {
                            (argc as usize).min(callee.nparams as usize)
                        };
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
                        });
                        mi = cm;
                        pi = cp;
                        ip = 0;
                        base = new_base;
                        cur_cl = cl_idx;
                        cur_argc = argc;
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
                                            .into_iter()
                                            .map(|g| match g {
                                                Some(s) => push_str(st, s),
                                                None => Value::UNDEFINED,
                                            })
                                            .collect();
                                        new_array(st, vals)
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
                                    let mut idx = -1i32;
                                    for (i, &e) in elems.iter().enumerate() {
                                        if strict_eq(st, e, arg0) {
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
                                    let mut found = false;
                                    for &e in elems.iter() {
                                        if strict_eq(st, e, arg0) {
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
                                        let v = call_value(
                                            st, mods, arg0,
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
                                        let v = call_value(
                                            st, mods, arg0,
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
                                        let v = call_value(
                                            st, mods, arg0,
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
                                        call_value(
                                            st, mods, arg0,
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
                                        let v = call_value(
                                            st, mods, arg0,
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
                                        let v = call_value(
                                            st, mods, arg0,
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
                                        let v = call_value(
                                            st, mods, arg0,
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
                                        let v = call_value(
                                            st, mods, arg0,
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
                        let mut m = raw_get_prop(st, oi, key);
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
                                let from = if callee.uses_arguments
                                {
                                    (argc as usize)
                                        .max(callee.nparams as usize)
                                } else {
                                    (argc as usize)
                                        .min(callee.nparams as usize)
                                };
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
                                });
                                mi = cm;
                                pi = cp;
                                ip = 0;
                                base = new_base;
                                cur_cl = cl_idx;
                                cur_argc = argc;
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
                        "trimStart" => push_str(st, s.trim_start().to_string()),
                        "trimEnd" => push_str(st, s.trim_end().to_string()),
                        "toUpperCase" => push_str(st, s.to_uppercase()),
                        "toLowerCase" => push_str(st, s.to_lowercase()),
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
                        "repeat" => {
                            let n = num_of(av0)?;
                            if n < 0.0 || !n.is_finite() {
                                return err("invalid repeat count");
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
                        "match" => {
                            let Some(ri) = regex_index(st, av0) else {
                                return err(
                                    "String.match needs a regex arg (yet)");
                            };
                            if st.regexes[ri].global {
                                let hits = st.regexes[ri].re.find_all(&s);
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
                                        let vals: Vec<Value> = gs.into_iter()
                                            .map(|g| match g {
                                                Some(x) => push_str(st, x),
                                                None => Value::UNDEFINED,
                                            }).collect();
                                        new_array(st, vals)
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
                            if let Some(ri) = regex_index(st, av0) {
                                let parts = st.regexes[ri].re.split_vec(&s);
                                let vals: Vec<Value> = parts.into_iter()
                                    .map(|p| push_str(st, p)).collect();
                                new_array(st, vals)
                            } else {
                                let parts: Vec<Value> = if argc == 0 {
                                    vec![push_str(st, s.clone())]
                                } else {
                                    let sep = to_display(st, av0);
                                    if sep.is_empty() {
                                        s.chars()
                                            .map(|c| push_str(st, c.to_string()))
                                            .collect()
                                    } else {
                                        s.split(&sep)
                                            .map(|p| push_str(st, p.to_string()))
                                            .collect::<Vec<_>>()
                                    }
                                };
                                new_array(st, parts)
                            }
                        }
                        "replace" => {
                            if let Some(ri) = regex_index(st, av0) {
                                // JS $& (whole match) -> regex crate ${0}
                                let to = to_display(st, av1)
                                    .replace("$&", "${0}");
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
                            let from = to_display(st, av0);
                            let to = to_display(st, av1);
                            push_str(st, s.replace(&from, &to))
                        }
                        "concat" => {
                            let other = to_display(st, av0);
                            push_str(st, format!("{s}{other}"))
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
                if st.frames.len() == floor {
                    return Ok(val);
                }
                let fr = st.frames.pop().unwrap();
                cur_argc = fr.argc;
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
                if st.frames.len() == floor {
                    return Ok(Value::UNDEFINED);
                }
                let fr = st.frames.pop().unwrap();
                cur_argc = fr.argc;
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
                                ClosureRec::Native(_) | ClosureRec::Bound { .. } => unreachable!(),
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
                    ClosureRec::Native(_) | ClosureRec::Bound { .. } => unreachable!(),
                };
                reg!(dst) = st.cells[cell as usize];
            }
            Instr::SetUpval { idx, src } => {
                let cell = match &st.closures[cur_cl as usize] {
                    ClosureRec::User { upvals, .. } => upvals[idx as usize],
                    ClosureRec::Native(_) | ClosureRec::Bound { .. } => unreachable!(),
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
                if ov.is_object() {
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
                    // named props in slot (= insertion) order
                    let mut pairs: Vec<(u16, u32)> = st.shapes
                        [shape as usize]
                        .props
                        .iter()
                        .map(|(&a, &s)| (s, a))
                        .collect();
                    pairs.sort_by_key(|&(slot, _)| slot);
                    for (_slot, atom) in pairs {
                        let name = st.names[atom as usize].clone();
                        let sv = intern(st, &name);
                        keys.push(sv);
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
                if ov.is_object() && kv.is_number() {
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
                    // integer-string keys hit dense elems on ANY object
                    // (numeric literal keys live there); gaps fall
                    // through to the named lookup below
                    if let Ok(n) = text.parse::<usize>() {
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
                    reg!(dst) = match raw_get_prop(st, oi, key_id) {
                        Some(v) => v,
                        // window doubles as the global namespace
                        None if ov == st.known.window
                            && st.gdef[key_id as usize] =>
                        {
                            st.globals[key_id as usize]
                        }
                        // a plain object's computed `toString` is the
                        // genuine Object.prototype.toString (brands)
                        None if !st.objects[oi].is_array
                            && text == "toString" =>
                        {
                            make_native(st, Native::BrandToString)
                        }
                        // Object.prototype staples — core-js getMethod
                        // reads V["valueOf"] as a computed access, so
                        // GetIndex must mirror GetProp's fallback or
                        // ordinaryToPrimitive finds nothing callable
                        None if matches!(
                            text.as_str(),
                            "hasOwnProperty" | "toString" | "valueOf"
                                | "propertyIsEnumerable"
                                | "isPrototypeOf"
                        ) =>
                        {
                            make_native(st, Native::MethodRef(key_id))
                        }
                        None if st.objects[oi].regex != REGEX_NONE
                            && matches!(text.as_str(), "exec" | "test") =>
                        {
                            make_native(st, Native::MethodRef(key_id))
                        }
                        None if st.objects[oi].is_array
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
                        None if st.objects[oi].is_array
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
                        None => Value::UNDEFINED,
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
                        Value::int(
                            sref.encode_utf16().count() as i32)
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
                if ov.is_object() && kv.is_number() {
                    let k = kv.to_number_raw();
                    if k < 0.0 || k.fract() != 0.0 {
                        // JS: not an element — a plain named property
                        let text = to_display(st, kv);
                        let key_id = st.intern_name(&text);
                        raw_set_prop(st, ov.index() as usize, key_id, v);
                        continue;
                    }
                    let elems = &mut st.objects[ov.index() as usize].elems;
                    let k = k as usize;
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
                    if st.objects[oi].is_array {
                        if let Ok(k) = text.parse::<usize>() {
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
                                        | "lastIndexOf"
                                ) =>
                        {
                            make_native(st, Native::MethodRef(key))
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
                            make_native(st, Native::MethodRef(key))
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
                            make_native(st, Native::MethodRef(key))
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
                    } else if matches!(
                        st.names[key as usize].as_str(),
                        "call" | "apply" | "bind" | "toString"
                            | "valueOf"
                    ) {
                        make_native(st, Native::MethodRef(key))
                    } else {
                        Value::UNDEFINED
                    };
                } else if ov.is_string() && key == st.ids.length {
                    let n = str_ref(st, ov.index()).encode_utf16().count();
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
                                mods.rc(f.module).module.protos
                                    [f.proto as usize]
                                    .name
                                    .clone()
                            })
                            .collect();
                        let here = cmod.module.protos[pi as usize]
                            .name
                            .clone();
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
