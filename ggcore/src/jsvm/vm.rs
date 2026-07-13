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
}

impl std::fmt::Debug for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error: {}", self.msg)
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError { msg: msg.into(), value: None })
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

/// A compiled script plus its bindings into the shared VM namespace.
pub(super) struct LoadedModule {
    pub(super) module: Module,
    /// module atom -> VM-wide name id
    pub(super) global_map: Vec<u32>,
    /// this module's slice of the shared inline-cache table
    pub(super) ic_base: u32,
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
}

#[derive(Clone, Copy)]
pub(super) enum Native {
    ConsoleLog,
    DateNow,
    Alert,
    /// accepts anything, returns undefined (window.addEventListener
    /// and friends — enough for feature-detecting bundles to proceed)
    Noop,
    /// the `Function` constructor stub: returns a function that
    /// returns the global (bundles call `Function("return this")()`
    /// to find the global object; real eval is out of scope)
    FunctionCtor,
    /// an extracted builtin method (`''.slice` — the core-js
    /// uncurryThis pattern): remembers the method name id and
    /// dispatches when invoked with an explicit this via call/apply
    MethodRef(u32),
    /// callable `Object(x)`: coercion-ish (nullish -> {}, object -> x)
    ObjectCtor,
    /// callable `Array(n)` / `Array(a, b, ...)`
    ArrayCtor,
    /// what FunctionCtor's product does when called: yield `window`
    ReturnGlobal,
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
    pub const O_KEYS: u16 = 40;
    pub const O_VALUES: u16 = 41;
    pub const O_ENTRIES: u16 = 42;
    pub const O_ASSIGN: u16 = 43;
    pub const O_FREEZE: u16 = 44;
    pub const A_ISARRAY: u16 = 60;
    pub const A_FROM: u16 = 61;
    pub const N_ISNAN: u16 = 80;
    pub const N_ISFINITE: u16 = 81;
    pub const N_ISINTEGER: u16 = 82;
    pub const S_FROMCHARCODE: u16 = 100;
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
}

/// Sentinel: an Obj that is not a Promise.
pub(super) const PROMISE_NONE: u32 = u32::MAX;
/// Sentinel: an Obj that is not a RegExp.
pub(super) const REGEX_NONE: u32 = u32::MAX;

/// A compiled regular expression + JS flags.
pub(super) struct RegexRec {
    re: regex::Regex,
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
pub(super) const DEFAULT_FUEL: u64 = 80_000_000;

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
        Ok(re) => re,
        Err(_) => {
            return err(format!(
                "unsupported regex /{pattern}/{flags}"
            ))
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
    });
    Ok(Value::object((st.objects.len() - 1) as u32))
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
fn drain_microtasks(st: &mut St, mods: &[LoadedModule], budget: &mut usize) {
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
    mods: &[LoadedModule],
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
fn to_number(st: &mut St, v: Value) -> Result<f64, VmError> {
    if v.is_string() {
        let s = str_ref(st, v.index()).trim().to_string();
        return Ok(if s.is_empty() {
            0.0
        } else {
            s.parse::<f64>().unwrap_or(f64::NAN)
        });
    }
    num_of(v)
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

#[inline]
fn loose_eq(st: &mut St, x: Value, y: Value) -> Result<bool, VmError> {
    if x.is_nullish() || y.is_nullish() {
        return Ok(x.is_nullish() && y.is_nullish());
    }
    if x.is_string() && y.is_string() {
        return Ok(strict_eq(st, x, y));
    }
    // mixed types compare by ToNumber (string -> number), like real ==
    Ok(to_number(st, x)? == to_number(st, y)?)
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
    obj: Value,
    key: Value,
) -> Result<bool, VmError> {
    if !obj.is_object() {
        return err("'in' right-hand side is not an object");
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
            _ => {
                return err(format!(
                    "extracted array builtin .{name}() not yet"
                ))
            }
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
        "hasOwnProperty" => {
            let k = args.first().copied().unwrap_or(Value::UNDEFINED);
            Ok(Value::boolean(
                recv.is_object() && has_own_property(st, recv, k)?,
            ))
        }
        other => err(format!(
            "extracted builtin .{other}() is not supported yet"
        )),
    }
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

fn to_i32(st: &mut St, v: Value) -> Result<i32, VmError> {
    Ok(js_to_uint32(to_number(st, v)?) as i32)
}

fn to_u32(st: &mut St, v: Value) -> Result<u32, VmError> {
    Ok(js_to_uint32(to_number(st, v)?))
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
        return Ok(x.is_object());
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
    err("right-hand side of 'instanceof' is not callable")
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
    n: Native,
    args_base: usize,
    argc: u8,
) -> Result<Value, VmError> {
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
        Native::MethodRef(key) => {
            // called directly (no receiver): dispatch against undefined
            let args: Vec<Value> = (0..argc as usize)
                .map(|k| st.regs[args_base + k])
                .collect();
            method_ref_dispatch(st, Value::UNDEFINED, key, &args)
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
            let t = s.trim();
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
            let t = s.trim();
            // longest numeric prefix that parses
            let mut end = 0;
            for (i, _) in t.char_indices() {
                if t[..=i].parse::<f64>().is_ok() {
                    end = i + 1;
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
                to_number(st, st.regs[args_base + 1])?.max(0.0)
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
        Native::ClearTimeout => {
            if argc > 0 {
                let id = to_number(st, st.regs[args_base])? as u32;
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
        Native::HostFn(id) => host_fn(st, id, args_base, argc),
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
fn host_fn(
    st: &mut St,
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
            let x = to_number(st, a)?;
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
            let a = to_number(st, a0)?;
            let b = to_number(st, a1)?;
            Ok(Value::number(a.powf(b)))
        }
        M_ATAN2 => {
            let (a0, a1) = (argv!(0), argv!(1));
            let a = to_number(st, a0)?;
            let b = to_number(st, a1)?;
            Ok(Value::number(a.atan2(b)))
        }
        M_HYPOT => {
            let mut sum = 0.0;
            for k in 0..n {
                let vk = argv!(k);
                let v = to_number(st, vk)?;
                sum += v * v;
            }
            Ok(Value::number(sum.sqrt()))
        }
        M_MIN | M_MAX => {
            let mut acc = if id == M_MIN { f64::INFINITY } else { f64::NEG_INFINITY };
            for k in 0..n {
                let vk = argv!(k);
                let v = to_number(st, vk)?;
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
                let code = to_number(st, ck)? as u32;
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
    key: u32,
    node: u32,
    args_base: usize,
    argc: u8,
) -> Result<Value, VmError> {
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
        if key == ids.get_elements_by_tag_name {
            let tag = arg_string(st, args_base, argc, 0)?
                .to_ascii_lowercase();
            let d = doc.borrow();
            let mut out = Vec::new();
            let mut stack = vec![d.root];
            while let Some(i) = stack.pop() {
                if d.nodes[i].tag.as_deref() == Some(tag.as_str()) {
                    out.push(Value::dom_node(i as u32));
                }
                for &c in d.nodes[i].children.iter().rev() {
                    stack.push(c);
                }
            }
            drop(d);
            return Ok(new_array(st, out));
        }
    } else {
        let node_us = node as usize;
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
    err(format!(
        "unsupported DOM method (name id {key}) on node {node}"
    ))
}

fn dom_get_prop(st: &mut St, key: u32, node: u32) -> Result<Value, VmError> {
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
    Ok(Value::UNDEFINED)
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
        return err("cannot set that property on document (yet)");
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
    err("cannot set that DOM property (yet)")
}

/// Call a JS function value from native code (sort comparators, DOM
/// event handlers). Runs a nested `exec` to completion.
pub(super) fn call_value(
    st: &mut St,
    mods: &[LoadedModule],
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
    mods: &[LoadedModule],
    fv: Value,
    this_explicit: Option<Value>,
    args: &[Value],
) -> Result<Value, VmError> {
    if !fv.is_function() {
        return err(format!("{fv:?} is not a function"));
    }
    let idx = fv.index();
    match &st.closures[idx as usize] {
        ClosureRec::Native(n) => {
            let n = *n;
            if let Native::MethodRef(key) = n {
                let recv = this_explicit.unwrap_or(Value::UNDEFINED);
                return method_ref_dispatch(st, recv, key, args);
            }
            let top = st.regs.len();
            st.regs.extend_from_slice(args);
            let r = do_native(st, n, top, args.len() as u8);
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
            let callee = &mods[m as usize].module.protos[p as usize];
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
    mods: &[LoadedModule],
    v: &mut Vec<Value>,
    cmp: Option<Value>,
) -> Result<(), VmError> {
    fn less(
        st: &mut St,
        mods: &[LoadedModule],
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
    mods: &[LoadedModule],
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
/// Error-like `{name, message}` object for engine-raised errors.
fn exception_value(st: &mut St, e: VmError) -> Value {
    if let Some(v) = e.value {
        return v;
    }
    let obj = new_plain_object(st);
    let oi = obj.index() as usize;
    let name_id = st.intern_name("name");
    let msg_id = st.intern_name("message");
    let n = intern(st, "Error");
    raw_set_prop(st, oi, name_id, n);
    let m = push_str(st, e.msg);
    raw_set_prop(st, oi, msg_id, m);
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
    mods: &[LoadedModule],
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
    let mut code: &[Instr] =
        &mods[mi as usize].module.protos[pi as usize].code;

    macro_rules! reg {
        ($i:expr) => {
            st.regs[base + $i as usize]
        };
    }

    /// module atom -> VM-wide name id
    macro_rules! name {
        ($atom:expr) => {
            mods[mi as usize].global_map[$atom as usize]
        };
    }

    macro_rules! ic {
        ($ic:expr) => {
            (mods[mi as usize].ic_base + $ic as u32) as usize
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
                Value::number(to_number(st, x)? $op to_number(st, y)?)
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
                to_number(st, x)? $op to_number(st, y)?
            };
            reg!($dst) = Value::boolean(r);
        }};
    }

    loop {
        // execution fuel: abort a runaway loop instead of wedging the
        // worker. Shared across the whole call tree via St, so nested
        // calls and pump callbacks all draw from one turn's budget.
        if st.fuel == 0 {
            return err("script exceeded its instruction budget");
        }
        st.fuel -= 1;
        let instr = code[ip];
        ip += 1;
        match instr {
            Instr::LoadConst { dst, idx } => {
                reg!(dst) = mods[mi as usize].module.protos[pi as usize]
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
                    return err(format!(
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
                    Value::UNDEFINED
                };
            }
            Instr::TdzCheck { src, atom } => {
                if reg!(src).is_tdz() {
                    let key = name!(atom) as usize;
                    return err(format!(
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
                return Err(VmError { msg, value: Some(v) });
            }
            Instr::SetGlobal { atom, src } => {
                let key = name!(atom) as usize;
                st.globals[key] = reg!(src);
                st.gdef[key] = true;
            }
            Instr::Add { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
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
                    Value::number(to_number(st, x)? + to_number(st, y)?)
                };
            }
            Instr::Sub { dst, a, b } => arith!(dst, a, b, checked_sub, -),
            Instr::Mul { dst, a, b } => arith!(dst, a, b, checked_mul, *),
            Instr::Div { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                reg!(dst) = Value::number(to_number(st, x)? / to_number(st, y)?);
            }
            Instr::Mod { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                reg!(dst) = if x.is_int() && y.is_int() {
                    match x.as_i32().checked_rem(y.as_i32()) {
                        Some(v) => Value::int(v),
                        None => Value::number(f64::NAN),
                    }
                } else {
                    Value::number(to_number(st, x)? % to_number(st, y)?)
                };
            }
            Instr::Pow { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                reg!(dst) =
                    Value::number(to_number(st, x)?.powf(to_number(st, y)?));
            }
            Instr::Neg { dst, src } => {
                let v = reg!(src);
                reg!(dst) = if v.is_int() {
                    match 0i32.checked_sub(v.as_i32()) {
                        Some(n) if v.as_i32() != 0 => Value::int(n),
                        _ => Value::number(-(v.as_i32() as f64)),
                    }
                } else {
                    Value::number(-to_number(st, v)?)
                };
            }
            Instr::ToNum { dst, src } => {
                let v = reg!(src);
                reg!(dst) = Value::number(to_number(st, v)?);
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
                let r = loose_eq(st, x, y)?;
                reg!(dst) = Value::boolean(r);
            }
            Instr::LooseNotEq { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let r = loose_eq(st, x, y)?;
                reg!(dst) = Value::boolean(!r);
            }
            Instr::Shl { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let (xi, s) = (to_i32(st, x)?, to_u32(st, y)? & 31);
                reg!(dst) = Value::int(xi.wrapping_shl(s));
            }
            Instr::Shr { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let (xi, s) = (to_i32(st, x)?, to_u32(st, y)? & 31);
                reg!(dst) = Value::int(xi.wrapping_shr(s));
            }
            Instr::UShr { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let (xu, s) = (to_u32(st, x)?, to_u32(st, y)? & 31);
                reg!(dst) = Value::number((xu >> s) as f64);
            }
            Instr::BitAnd { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let r = to_i32(st, x)? & to_i32(st, y)?;
                reg!(dst) = Value::int(r);
            }
            Instr::BitOr { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let r = to_i32(st, x)? | to_i32(st, y)?;
                reg!(dst) = Value::int(r);
            }
            Instr::BitXor { dst, a, b } => {
                let (x, y) = (reg!(a), reg!(b));
                let r = to_i32(st, x)? ^ to_i32(st, y)?;
                reg!(dst) = Value::int(r);
            }
            Instr::BitNot { dst, src } => {
                let v = reg!(src);
                reg!(dst) = Value::int(!to_i32(st, v)?);
            }
            Instr::Arguments { dst } => {
                let vals: Vec<Value> = (0..cur_argc as usize)
                    .map(|k| st.regs[base + k])
                    .collect();
                reg!(dst) = new_array(st, vals);
            }
            Instr::NewInstance { dst, ctor } => {
                let cv = reg!(ctor);
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
                let r = has_own_property(st, obj, key)?;
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
                    return err(format!("{fv:?} is not a function"));
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
                };
                match kind {
                    Err(n) => {
                        let r =
                            do_native(st, n, base + func as usize + 1, argc)?;
                        reg!(func) = r;
                    }
                    Ok((cm, cp, this_cap)) => {
                        let callee = &mods[cm as usize].module.protos
                            [cp as usize];
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
                        code = &mods[mi as usize].module.protos[pi as usize]
                            .code;
                    }
                }
            }
            Instr::CallThis { func, recv, argc } => {
                let fv = reg!(func);
                if !fv.is_function() {
                    return err(format!("{fv:?} is not a function"));
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
                };
                match kind {
                    Err(n) => {
                        let r =
                            do_native(st, n, base + func as usize + 1, argc)?;
                        reg!(func) = r;
                    }
                    Ok((cm, cp, this_cap)) => {
                        let callee = &mods[cm as usize].module.protos
                            [cp as usize];
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
                        code = &mods[mi as usize].module.protos[pi as usize]
                            .code;
                    }
                }
            }
            Instr::CallMethod { obj, atom, argc } => {
                let ov = reg!(obj);
                let key = name!(atom);
                // Function.prototype.call / apply (backs spread calls)
                if ov.is_function() {
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
                    // (Object.keys, Array.isArray) and user statics
                    if let Some(&mv) =
                        st.fn_props.get(&(ov.index(), key))
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
                                let groups: Option<Vec<Option<String>>> =
                                    st.regexes[ri].re.captures(&subject)
                                        .map(|caps| caps.iter()
                                            .map(|m| m.map(|mm|
                                                mm.as_str().to_string()))
                                            .collect());
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
                    if is_array && key == st.ids.push {
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
                                _ => {
                                    done = false;
                                    Value::UNDEFINED
                                }
                            };
                            if done {
                                reg!(obj) = r;
                                continue;
                            }
                        }
                        let m = raw_get_prop(st, oi, key);
                        let Some(m) = m else {
                            return err(format!(
                                ".{}() is not a function",
                                st.names[key as usize]
                            ));
                        };
                        if !m.is_function() {
                            return err(format!(
                                ".{} is not a function",
                                st.names[key as usize]
                            ));
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
                        };
                        match kind {
                            Err(n) => {
                                let r = do_native(
                                    st, n, base + obj as usize + 1, argc,
                                )?;
                                reg!(obj) = r;
                            }
                            Ok((cm, cp, this_cap)) => {
                                let callee = &mods[cm as usize]
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
                                code = &mods[mi as usize].module.protos
                                    [pi as usize]
                                    .code;
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
                                let hits: Vec<String> = st.regexes[ri].re
                                    .find_iter(&s)
                                    .map(|m| m.as_str().to_string())
                                    .collect();
                                if hits.is_empty() {
                                    Value::NULL
                                } else {
                                    let vals: Vec<Value> = hits.into_iter()
                                        .map(|h| push_str(st, h)).collect();
                                    new_array(st, vals)
                                }
                            } else {
                                let groups: Option<Vec<Option<String>>> =
                                    st.regexes[ri].re.captures(&s).map(|c|
                                        c.iter().map(|m| m.map(|mm|
                                            mm.as_str().to_string()))
                                            .collect());
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
                                Some(ri) => match st.regexes[ri].re.find(&s) {
                                    Some(m) => Value::int(
                                        s[..m.start()].chars().count() as i32),
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
                                let parts: Vec<String> = st.regexes[ri].re
                                    .split(&s)
                                    .map(|p| p.to_string())
                                    .collect();
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
                                        .replace_all(&s, to.as_str())
                                        .into_owned()
                                } else {
                                    st.regexes[ri].re
                                        .replace(&s, to.as_str())
                                        .into_owned()
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
                        _ => {
                            return err(format!(
                                "cannot call .{}() on a string (yet)",
                                method
                            ))
                        }
                    };
                    reg!(obj) = r;
                } else if ov.is_dom_node() {
                    let r = dom_method(
                        st,
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
                    let r = method_ref_dispatch(st, ov, key, &args)?;
                    reg!(obj) = r;
                } else {
                    return err(format!(
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
                code = &mods[mi as usize].module.protos[pi as usize].code;
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
                code = &mods[mi as usize].module.protos[pi as usize].code;
            }
            Instr::Closure { dst, proto: p } => {
                let src_proto =
                    &mods[mi as usize].module.protos[p as usize];
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
                                ClosureRec::Native(_) => unreachable!(),
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
                    ClosureRec::Native(_) => unreachable!(),
                };
                reg!(dst) = st.cells[cell as usize];
            }
            Instr::SetUpval { idx, src } => {
                let cell = match &st.closures[cur_cl as usize] {
                    ClosureRec::User { upvals, .. } => upvals[idx as usize],
                    ClosureRec::Native(_) => unreachable!(),
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
                let proto = &mods[mi as usize].module.protos[pi as usize];
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
            Instr::GetIndex { dst, obj, key } => {
                let (ov, kv) = (reg!(obj), reg!(key));
                if ov.is_object() && kv.is_number() {
                    let o = &st.objects[ov.index() as usize];
                    let k = kv.to_number_raw();
                    reg!(dst) = if k >= 0.0 && (k as usize) < o.elems.len() {
                        o.elems[k as usize]
                    } else {
                        Value::UNDEFINED
                    };
                } else if ov.is_object() && kv.is_string() {
                    // dynamic property read: `o[key]`, `o[i]` from for-in
                    let text = str_ref(st, kv.index()).to_string();
                    let oi = ov.index() as usize;
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
                    reg!(dst) = raw_get_prop(st, oi, key_id)
                        .unwrap_or(Value::UNDEFINED);
                } else {
                    return err(format!(
                        "unsupported indexing {ov:?}[{kv:?}] (yet)"
                    ));
                }
            }
            Instr::SetIndex { obj, key, src } => {
                let (ov, kv, v) = (reg!(obj), reg!(key), reg!(src));
                if ov.is_object() && kv.is_number() {
                    let elems = &mut st.objects[ov.index() as usize].elems;
                    let k = kv.to_number_raw();
                    if k < 0.0 || k.fract() != 0.0 {
                        return err("negative/fractional array index (yet)");
                    }
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
                    raw_set_prop(st, oi, key_id, v);
                } else {
                    return err(format!(
                        "unsupported indexing {ov:?}[{kv:?}] (yet)"
                    ));
                }
            }
            Instr::GetProp { dst, obj, atom, ic } => {
                let ov = reg!(obj);
                let key = name!(atom);
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
                        Some(st.objects[oi].slots[e.slot as usize])
                    } else {
                        match st.shapes[shape as usize].props.get(&key) {
                            Some(&slot) => {
                                st.ics[slot_ic] = IcEntry { shape, slot };
                                Some(st.objects[oi].slots[slot as usize])
                            }
                            // own miss: walk the prototype chain
                            None => raw_get_prop(st, oi, key),
                        }
                    };
                    reg!(dst) = match hit {
                        Some(v) => v,
                        // arrays expose known builtins as extractable
                        // methods (`[].slice` — the core-js pattern)
                        None if is_arr
                            && matches!(
                                st.names[key as usize].as_str(),
                                "slice" | "concat" | "join" | "indexOf"
                                    | "push" | "pop" | "map" | "filter"
                                    | "forEach"
                            ) =>
                        {
                            make_native(st, Native::MethodRef(key))
                        }
                        None => Value::UNDEFINED,
                    };
                } else if ov.is_function() {
                    // functions expose a lazily-created .prototype and
                    // any static props (Object.keys, F.displayName)
                    reg!(dst) = if key == st.ids.prototype {
                        fn_prototype(st, ov)
                    } else if let Some(&v) =
                        st.fn_props.get(&(ov.index(), key))
                    {
                        v
                    } else {
                        Value::UNDEFINED
                    };
                } else if ov.is_string() && key == st.ids.length {
                    let n = str_ref(st, ov.index()).encode_utf16().count();
                    reg!(dst) = Value::int(n as i32);
                } else if ov.is_string() || ov.is_number()
                    || ov.is_boolean()
                {
                    // method extraction (`''.slice`, `(1).toString`):
                    // a callable that dispatches on its receiver
                    reg!(dst) = make_native(st, Native::MethodRef(key));
                } else if ov.is_dom_node() {
                    let r = dom_get_prop(st, key, ov.index())?;
                    reg!(dst) = r;
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
                if !ov.is_object() {
                    return err(format!(
                        "cannot set .{} on {ov:?} (yet)",
                        st.names[key as usize]
                    ));
                }
                let oi = ov.index() as usize;
                let v = reg!(src);
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
