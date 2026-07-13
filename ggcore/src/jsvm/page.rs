//! PageVm: one persistent gg-js VM per page.
//!
//! Scripts, inline event handlers, and listeners all share globals,
//! heap, and the DOM — modules come and go, the VM state stays. This
//! is the engine the browser talks to (Doc.run_scripts routes here
//! when GGJS=1).

use std::cell::RefCell;
use std::rc::Rc;

use super::bytecode::Module;
use super::compiler;
use super::parser;
use super::value::Value;
use super::vm::{
    self, call_value, exec, has_pending_work, host, make_native,
    new_plain_object, pump, raw_set_prop, reject_fetch, resolve_fetch, Ids,
    LoadedModule, Native, St, DOC_NODE, IC_EMPTY,
};

/// Event-loop budget per settle turn (total microtasks + timers fired).
const PUMP_BUDGET: usize = 200_000;

/// Promise.all / Promise.race, built on the native new Promise + then.
const PROMISE_PRELUDE: &str = r#"
Promise.all = function (arr) {
  return new Promise(function (resolve, reject) {
    var results = []; var count = 0; var total = arr.length;
    if (total === 0) { resolve(results); return; }
    for (var i = 0; i < total; i++) {
      (function (idx) {
        Promise.resolve(arr[idx]).then(function (v) {
          results[idx] = v; count = count + 1;
          if (count === total) { resolve(results); }
        }, function (e) { reject(e); });
      })(i);
    }
  });
};
Promise.race = function (arr) {
  return new Promise(function (resolve, reject) {
    for (var i = 0; i < arr.length; i++) {
      Promise.resolve(arr[i]).then(function (v) { resolve(v); },
                                   function (e) { reject(e); });
    }
  });
};

// Error hierarchy in JS itself — real prototype chains (07-12) make
// `new TypeError(m) instanceof Error` just work.
function Error(m) { if (m !== undefined) this.message = '' + m; }
Error.prototype.name = 'Error';
Error.prototype.message = '';
Error.prototype.toString = function () {
  return this.message ? this.name + ': ' + this.message : this.name;
};
function TypeError(m) { if (m !== undefined) this.message = '' + m; }
TypeError.prototype = new Error();
TypeError.prototype.name = 'TypeError';
function RangeError(m) { if (m !== undefined) this.message = '' + m; }
RangeError.prototype = new Error();
RangeError.prototype.name = 'RangeError';
function SyntaxError(m) { if (m !== undefined) this.message = '' + m; }
SyntaxError.prototype = new Error();
SyntaxError.prototype.name = 'SyntaxError';
function ReferenceError(m) { if (m !== undefined) this.message = '' + m; }
ReferenceError.prototype = new Error();
ReferenceError.prototype.name = 'ReferenceError';
"#;
use crate::dom;

pub struct PageVm {
    st: St,
    mods: Vec<LoadedModule>,
}

impl PageVm {
    pub fn new(doc: Option<Rc<RefCell<dom::Document>>>) -> PageVm {
        let has_doc = doc.is_some();
        let mut vm = PageVm {
            st: St::new(doc),
            mods: Vec::new(),
        };
        vm.st.ids = Ids {
            push: vm.name_id("push"),
            sort: vm.name_id("sort"),
            to_fixed: vm.name_id("toFixed"),
            length: vm.name_id("length"),
            get_element_by_id: vm.name_id("getElementById"),
            create_element: vm.name_id("createElement"),
            query_selector: vm.name_id("querySelector"),
            query_selector_all: vm.name_id("querySelectorAll"),
            get_elements_by_tag_name: vm.name_id("getElementsByTagName"),
            append_child: vm.name_id("appendChild"),
            remove: vm.name_id("remove"),
            set_attribute: vm.name_id("setAttribute"),
            get_attribute: vm.name_id("getAttribute"),
            remove_attribute: vm.name_id("removeAttribute"),
            add_event_listener: vm.name_id("addEventListener"),
            text_content: vm.name_id("textContent"),
            inner_html: vm.name_id("innerHTML"),
            id: vm.name_id("id"),
            class_name: vm.name_id("className"),
            body: vm.name_id("body"),
            title: vm.name_id("title"),
            prototype: vm.name_id("prototype"),
        };
        // host globals
        vm.install_object("console", &[
            ("log", Native::ConsoleLog),
            ("error", Native::ConsoleLog),
            ("warn", Native::ConsoleLog),
            ("info", Native::ConsoleLog),
            ("debug", Native::ConsoleLog),
            ("trace", Native::ConsoleLog),
        ]);
        vm.install_object("Date", &[("now", Native::DateNow)]);
        vm.install_object(
            "JSON",
            &[
                ("stringify", Native::JsonStringify),
                ("parse", Native::JsonParse),
            ],
        );
        let alert = make_native(&mut vm.st, Native::Alert);
        vm.set_global("alert", alert);
        for (name, n) in [
            ("String", Native::String),
            ("Number", Native::Number),
            ("Boolean", Native::Boolean),
            ("parseInt", Native::ParseInt),
            ("parseFloat", Native::ParseFloat),
        ] {
            let fv = make_native(&mut vm.st, n);
            vm.set_global(name, fv);
            match name {
                "String" => vm.st.known.string = fv,
                "Number" => vm.st.known.number = fv,
                "Boolean" => vm.st.known.boolean = fv,
                _ => {}
            }
        }
        // async runtime (P3): timers, microtasks, fetch, Promise
        for (name, n) in [
            ("setTimeout", Native::SetTimeout),
            ("setInterval", Native::SetInterval),
            ("clearTimeout", Native::ClearTimeout),
            ("clearInterval", Native::ClearTimeout),
            ("queueMicrotask", Native::QueueMicrotask),
            ("fetch", Native::Fetch),
        ] {
            let fv = make_native(&mut vm.st, n);
            vm.set_global(name, fv);
        }
        let promise_ctor = vm.install_object(
            "Promise",
            &[
                ("resolve", Native::PromiseResolve),
                ("reject", Native::PromiseReject),
            ],
        );
        vm.st.known.promise = promise_ctor;

        // host objects (P3b): Math / Object / Array / Number / String
        let math = vm.install_object("Math", &[
            ("abs", Native::HostFn(host::M_ABS)),
            ("floor", Native::HostFn(host::M_FLOOR)),
            ("ceil", Native::HostFn(host::M_CEIL)),
            ("round", Native::HostFn(host::M_ROUND)),
            ("trunc", Native::HostFn(host::M_TRUNC)),
            ("sign", Native::HostFn(host::M_SIGN)),
            ("sqrt", Native::HostFn(host::M_SQRT)),
            ("cbrt", Native::HostFn(host::M_CBRT)),
            ("pow", Native::HostFn(host::M_POW)),
            ("exp", Native::HostFn(host::M_EXP)),
            ("log", Native::HostFn(host::M_LOG)),
            ("log2", Native::HostFn(host::M_LOG2)),
            ("log10", Native::HostFn(host::M_LOG10)),
            ("sin", Native::HostFn(host::M_SIN)),
            ("cos", Native::HostFn(host::M_COS)),
            ("tan", Native::HostFn(host::M_TAN)),
            ("atan", Native::HostFn(host::M_ATAN)),
            ("atan2", Native::HostFn(host::M_ATAN2)),
            ("min", Native::HostFn(host::M_MIN)),
            ("max", Native::HostFn(host::M_MAX)),
            ("random", Native::HostFn(host::M_RANDOM)),
            ("hypot", Native::HostFn(host::M_HYPOT)),
        ]);
        vm.set_object_prop(math, "PI", Value::number(std::f64::consts::PI));
        vm.set_object_prop(math, "E", Value::number(std::f64::consts::E));
        vm.set_object_prop(math, "LN2", Value::number(std::f64::consts::LN_2));
        vm.set_object_prop(math, "SQRT2",
                           Value::number(std::f64::consts::SQRT_2));

        let object_ctor = vm.install_callable(
            "Object",
            Native::ObjectCtor,
            &[
                ("keys", Native::HostFn(host::O_KEYS)),
                ("values", Native::HostFn(host::O_VALUES)),
                ("entries", Native::HostFn(host::O_ENTRIES)),
                ("assign", Native::HostFn(host::O_ASSIGN)),
                ("freeze", Native::HostFn(host::O_FREEZE)),
            ],
        );
        vm.st.known.object = object_ctor;
        let array_ctor = vm.install_callable(
            "Array",
            Native::ArrayCtor,
            &[
                ("isArray", Native::HostFn(host::A_ISARRAY)),
                ("from", Native::HostFn(host::A_FROM)),
            ],
        );
        vm.st.known.array = array_ctor;
        // Number/String stay callable (coercion: Number("3"), String(42)),
        // so isNaN/isFinite live as the *global* functions they also are
        // in JS. (Number.isInteger etc. are a later add — a function value
        // can't carry static properties in this model.)
        for (name, n) in [
            ("isNaN", Native::HostFn(host::N_ISNAN)),
            ("isFinite", Native::HostFn(host::N_ISFINITE)),
        ] {
            let fv = make_native(&mut vm.st, n);
            vm.set_global(name, fv);
        }

        let window = new_plain_object(&mut vm.st);
        vm.set_global("window", window);
        // webpack bundles address the global as self/globalThis
        // (`self.webpackChunk... = ...`); alias both to window
        vm.set_global("self", window);
        vm.set_global("globalThis", window);
        // bundles register load/resize/scroll handlers on window before
        // doing anything useful; accept and ignore them
        for m in ["addEventListener", "removeEventListener",
                  "postMessage", "scrollTo"] {
            let noop = make_native(&mut vm.st, Native::Noop);
            vm.set_object_prop(window, m, noop);
        }
        vm.st.known.window = window;
        let fctor = make_native(&mut vm.st, Native::FunctionCtor);
        vm.set_global("Function", fctor);
        if has_doc {
            vm.set_global("document", Value::dom_node(DOC_NODE));
        }
        // Promise.all/race defined in JS on top of new Promise + then
        vm.run_source(PROMISE_PRELUDE).ok();
        vm
    }

    // ---- async runtime driver surface (P3) ----

    /// Run the event loop to a fixed point: drain microtasks, fire
    /// virtual-clock timers. Returns (console output, fetches to service).
    /// The host (Python driver) performs the actual HTTP for each fetch
    /// and calls resolve_fetch/reject_fetch, then pumps again.
    pub fn pump(&mut self) -> (Vec<String>, Vec<(u32, String)>) {
        let fetches = pump(&mut self.st, &self.mods, PUMP_BUDGET);
        (std::mem::take(&mut self.st.logs), fetches)
    }

    pub fn resolve_fetch(&mut self, fetch_id: u32, status: u16, body: String) {
        resolve_fetch(&mut self.st, fetch_id, status, body);
    }

    pub fn reject_fetch(&mut self, fetch_id: u32, message: String) {
        reject_fetch(&mut self.st, fetch_id, message);
    }

    pub fn has_pending_work(&self) -> bool {
        has_pending_work(&self.st)
    }

    fn name_id(&mut self, name: &str) -> u32 {
        self.st.intern_name(name)
    }

    fn set_global(&mut self, name: &str, v: Value) {
        let i = self.name_id(name) as usize;
        self.st.globals[i] = v;
        self.st.gdef[i] = true;
    }

    /// A callable builtin (Object, Array): a native function whose
    /// static methods live in the fn_props side table.
    fn install_callable(
        &mut self,
        name: &str,
        ctor: Native,
        methods: &[(&str, Native)],
    ) -> Value {
        let fv = make_native(&mut self.st, ctor);
        for (m, native) in methods {
            let key = self.name_id(m);
            let mv = make_native(&mut self.st, *native);
            self.st.fn_props.insert((fv.index(), key), mv);
        }
        self.set_global(name, fv);
        fv
    }

    fn install_object(
        &mut self,
        name: &str,
        methods: &[(&str, Native)],
    ) -> Value {
        let obj = new_plain_object(&mut self.st);
        for (m, native) in methods {
            let key = self.name_id(m);
            let fv = make_native(&mut self.st, *native);
            raw_set_prop(&mut self.st, obj.index() as usize, key, fv);
        }
        self.set_global(name, obj);
        obj
    }

    fn set_object_prop(&mut self, obj: Value, prop: &str, val: Value) {
        let key = self.name_id(prop);
        raw_set_prop(&mut self.st, obj.index() as usize, key, val);
    }

    /// Compile a script and bind it into the shared namespace.
    fn load(&mut self, mut module: Module) -> u32 {
        // string constants move into the VM-wide rope arena
        let str_base = self.st.strs.len() as u32;
        for s in &module.strings {
            self.st.strs.push(vm::Str::Flat(s.clone()));
        }
        for proto in &mut module.protos {
            for c in &mut proto.consts {
                if c.is_string() {
                    *c = Value::string(str_base + c.index());
                }
            }
        }
        let global_map =
            module.atoms.iter().map(|n| self.name_id(n)).collect();
        let ic_base = self.st.ics.len() as u32;
        self.st
            .ics
            .extend(std::iter::repeat(IC_EMPTY).take(module.n_ics as usize));
        self.mods.push(LoadedModule { module, global_map, ic_base });
        (self.mods.len() - 1) as u32
    }

    /// Parse + compile + run one script; returns its last expression
    /// statement value.
    pub fn run_source(&mut self, src: &str) -> Result<Value, String> {
        let ast =
            parser::parse_program(src).map_err(|e| format!("{e:?}"))?;
        let module =
            compiler::compile(&ast).map_err(|e| format!("{e:?}"))?;
        let mi = self.load(module);
        let main = self.mods[mi as usize].module.main;
        let nregs = self.mods[mi as usize].module.protos[main as usize]
            .nregs as usize;
        let base = self.st.regs.len();
        self.st.regs.resize(base + nregs, Value::UNDEFINED);
        self.st.fuel = vm::DEFAULT_FUEL; // fresh budget per top-level script
        let out = exec(
            &mut self.st,
            &self.mods,
            mi,
            main,
            base,
            u32::MAX,
            Value::UNDEFINED,
            0,
        );
        self.st.regs.truncate(base);
        out.map_err(|e| e.msg)
    }

    /// Browser entry point: run page scripts in order, collect console
    /// output. Script errors log instead of aborting the page.
    pub fn run_scripts(&mut self, sources: &[String]) -> Vec<String> {
        for src in sources {
            if let Err(e) = self.run_source(src) {
                self.st.logs.push(format!("[gg-js error] {e}"));
            }
        }
        std::mem::take(&mut self.st.logs)
    }

    /// Bubble a click from a node to the root: run onclick attributes
    /// and addEventListener handlers. Returns (console output, any
    /// handler ran, default action prevented) — the same contract as
    /// the Boa path: navigation proceeds unless a handler called
    /// event.preventDefault() or an onclick returned false.
    pub fn dispatch_click(&mut self, idx: usize) -> (Vec<String>, bool, bool)
    {
        let Some(doc) = self.st.doc.clone() else {
            return (Vec::new(), false, false);
        };
        let chain: Vec<(u32, Option<String>)> = {
            let d = doc.borrow();
            if idx >= d.nodes.len() {
                return (Vec::new(), false, false);
            }
            let mut out = Vec::new();
            let mut cur = Some(idx);
            while let Some(i) = cur {
                let onclick =
                    d.nodes[i].attr("onclick").map(str::to_string);
                out.push((i as u32, onclick));
                cur = d.nodes[i].parent;
            }
            out
        };
        // one `event` object for the whole dispatch, exposed as a
        // global (like window.event) so onclick sources see it too
        self.st.default_prevented = false;
        let ev = new_plain_object(&mut self.st);
        let ev_idx = ev.index() as usize;
        let type_key = self.name_id("type");
        let target_key = self.name_id("target");
        let pd_key = self.name_id("preventDefault");
        let click_s = vm::intern(&mut self.st, "click");
        raw_set_prop(&mut self.st, ev_idx, type_key, click_s);
        raw_set_prop(
            &mut self.st,
            ev_idx,
            target_key,
            Value::dom_node(idx as u32),
        );
        let pd = make_native(&mut self.st, Native::PreventDefault);
        raw_set_prop(&mut self.st, ev_idx, pd_key, pd);
        self.set_global("event", ev);

        let mut handled = false;
        let mut prevented = false;
        for (node, onclick) in chain {
            if let Some(src) = onclick {
                handled = true;
                // run as a function body: `event` is the argument and
                // `return false` prevents the default action
                let wrapped =
                    format!("(function (event) {{ {src}\n }})(event)");
                match self.run_source(&wrapped) {
                    Ok(v) if v.is_boolean() && !v.as_bool() => {
                        prevented = true;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        self.st.logs.push(format!("[gg-js error] {e}"));
                    }
                }
            }
            let handlers = self
                .st
                .listeners
                .get(&(node, "click".to_string()))
                .cloned()
                .unwrap_or_default();
            for h in handlers {
                handled = true;
                self.st.fuel = vm::DEFAULT_FUEL; // fresh budget per handler
                if let Err(e) =
                    call_value(&mut self.st, &self.mods, h, &[ev])
                {
                    self.st.logs.push(format!("[gg-js error] {}", e.msg));
                }
            }
        }
        if self.st.default_prevented {
            prevented = true;
        }
        (std::mem::take(&mut self.st.logs), handled, prevented)
    }

    /// One-shot headless eval (tests, jsvm_eval/jsvm_run).
    pub fn eval(src: &str) -> Result<(Value, Vec<String>), String> {
        let mut vm = PageVm::new(None);
        let v = vm.run_source(src)?;
        Ok((v, std::mem::take(&mut vm.st.logs)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::eval;
    use super::PageVm;
    use crate::html;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn n(src: &str) -> f64 {
        let (v, _) = eval(src).unwrap();
        assert!(v.is_number(), "non-number result: {v:?} for {src}");
        v.to_number_raw()
    }

    #[test]
    fn spilled_locals_past_register_budget() {
        // 300 vars blow the u8 register file; overflow spills into the
        // hidden %spill object and keeps working
        let mut src = String::new();
        for i in 0..300 {
            src += &format!("var a{i} = {i}; ");
        }
        src += "a5 + a250 * 10 + a299";
        assert_eq!(n(&src), 5.0 + 2500.0 + 299.0);
        // spilled vars support compound ops and function reads
        let mut src2 = String::new();
        for i in 0..260 {
            src2 += &format!("var b{i} = 1; ");
        }
        src2 += "b259 += 41; b259";
        assert_eq!(n(&src2), 42.0);
        // captured overflow vars: closures reach them through the
        // shared spill object
        let mut src3 = String::new();
        for i in 0..260 {
            src3 += &format!("var c{i} = {i}; ");
        }
        src3 += "var f = function() { c255 += 1; return c250 + c5; }; \
                 f(); f() + c255";
        assert_eq!(n(&src3), 255.0 + 257.0);
    }

    #[test]
    fn labeled_statements() {
        // labeled break exits the outer loop
        assert_eq!(
            n("var c = 0; outer: for (var i = 0; i < 3; i++) { \
               for (var j = 0; j < 3; j++) { \
                 if (i + j === 2) break outer; c++; } } c"), 2.0);
        // labeled continue skips to the outer loop's next iteration
        assert_eq!(
            n("var c = 0; L: for (var i = 0; i < 3; i++) { \
               for (var j = 0; j < 3; j++) { \
                 if (j === 1) continue L; c++; } } c"), 3.0);
        // plain break/continue unaffected
        assert_eq!(
            n("var s = 0; loop: for (var i = 0; i < 5; i++) { \
               if (i === 3) break; s += i; } s"), 3.0);
    }

    #[test]
    fn no_in_stops_at_boundaries() {
        // `in` inside a function literal in a for-head is fine
        assert_eq!(
            n("var o = {a: 1}; var r = 0; \
               for (var f = function() { return 'a' in o ? 1 : 0 }; \
                    r < 1;) { r = f() + 1; } r"), 2.0);
        // parens lift the restriction too
        assert_eq!(
            n("var o = {a: 1}; \
               for (var t = ('a' in o); t;) { break; } t ? 1 : 0"), 1.0);
    }

    #[test]
    fn array_elision() {
        assert_eq!(n("var a = [,5]; a.length * 10 + (a[0] === undefined \
                      ? 1 : 0)"), 21.0);
        assert_eq!(n("var a = [1,,3]; a.length * 100 + a[2]"), 303.0);
        assert_eq!(n("[1,2,].length"), 2.0); // trailing comma: no hole
        assert_eq!(n("[,,].length"), 2.0);
    }

    #[test]
    fn labeled_blocks_and_switches() {
        assert_eq!(n("var x = 1; b: { x = 2; break b; x = 3; } x"), 2.0);
        assert_eq!(
            n("var r = 0; d: switch (1) { case 1: r = 5; break d; \
               default: r = 9; } r"), 5.0);
        // unlabeled break inside a labeled switch stays local
        assert_eq!(
            n("var r = 0; s: switch (1) { case 1: r = 7; break; \
               default: r = 9; } r"), 7.0);
        assert_eq!(
            n("var c = 0; o: for (var i = 0; i < 3; i++) \
               { inner: { if (i == 1) break o; c++; } } c"), 1.0);
    }

    #[test]
    fn arguments_object() {
        assert_eq!(n("function f() { return arguments.length; } \
                      f(1, 2, 3)"), 3.0);
        assert_eq!(n("function f() { return arguments[1]; } f(7, 8)"),
                   8.0);
        // extras beyond declared params survive
        assert_eq!(n("function f(a) { return a + arguments[2]; } \
                      f(1, 9, 5)"), 6.0);
        // the core-js staple: slicing arguments into a real array
        assert_eq!(
            n("function f() { \
               var a = [].slice.call(arguments, 1); \
               return a.length * 10 + a[0]; } f('x', 4, 5)"), 24.0);
        assert_eq!(n("function f() { return arguments.length; } f()"),
                   0.0);
    }

    #[test]
    fn callable_object_and_array() {
        assert_eq!(n("typeof Object === 'function' ? 1 : 0"), 1.0);
        assert_eq!(n("Object.keys({a:1, b:2}).length"), 2.0);
        assert_eq!(n("var o = Object(null); typeof o === 'object' \
                      ? 1 : 0"), 1.0);
        assert_eq!(n("var x = {k:1}; Object(x) === x ? 1 : 0"), 1.0);
        assert_eq!(n("Array(3).length + Array(1, 2).length * 10"), 23.0);
        assert_eq!(n("Array.isArray(Array(2)) ? 1 : 0"), 1.0);
        assert_eq!(n("([1] instanceof Array) ? 1 : 0"), 1.0);
        assert_eq!(n("function F() {} F.staticX = 5; F.staticX"), 5.0);
    }

    #[test]
    fn number_methods_via_extraction() {
        assert_eq!(
            n("(255).toString(16) === 'ff' ? 1 : 0"), 1.0);
        assert_eq!(
            n("(1.5).toString() === '1.5' ? 1 : 0"), 1.0);
        assert_eq!(n("(7).valueOf()"), 7.0);
        assert_eq!(
            n("true.toString() === 'true' ? 1 : 0"), 1.0);
    }

    #[test]
    fn error_hierarchy() {
        assert_eq!(
            n("var e = new TypeError('bad'); \
               (e instanceof TypeError ? 1 : 0) \
               + (e instanceof Error ? 10 : 0) \
               + (e.name === 'TypeError' ? 100 : 0) \
               + (e.message === 'bad' ? 1000 : 0)"), 1111.0);
        assert_eq!(
            n("var r = 0; try { throw new RangeError('x'); } \
               catch (e) { r = e instanceof RangeError ? 1 : 0; } r"),
            1.0);
    }

    #[test]
    fn method_extraction() {
        assert_eq!(
            n("var f = ''.slice; \
               f.call('hello', 1) === 'ello' ? 1 : 0"), 1.0);
        assert_eq!(n("(''.slice) ? 1 : 0"), 1.0); // feature detection
        assert_eq!(
            n("var c = ''.charCodeAt; c.call('A', 0)"), 65.0);
        assert_eq!(
            n("var i = ''.indexOf; i.call('abcabc', 'c')"), 2.0);
        assert_eq!(
            n("var sub = ''.substring; \
               sub.call('hello', 3, 1) === 'el' ? 1 : 0"), 1.0);
    }

    #[test]
    fn prototype_chain() {
        // methods on F.prototype are reachable from instances
        assert_eq!(
            n("function F(v) { this.v = v; } \
               F.prototype.get = function() { return this.v; }; \
               var a = new F(6); var b = new F(7); \
               a.get() * 10 + b.get()"), 67.0);
        // prototype replacement + instanceof through the chain
        assert_eq!(
            n("function F() {} F.prototype = {k: 42}; \
               var o = new F(); \
               (o instanceof F ? 100 : 0) + o.k"), 142.0);
        // constructor back-reference; own props shadow the proto
        assert_eq!(
            n("function F() {} var o = new F(); \
               (o.constructor === F ? 1 : 0) \
               + (new F().x === undefined ? 10 : 0)"), 11.0);
        // ctor returning an object wins over the fresh instance
        assert_eq!(
            n("function F() { return {v: 9}; } (new F()).v"), 9.0);
    }

    #[test]
    fn function_ctor_stub_yields_the_global() {
        assert_eq!(n("typeof Function === 'function' ? 1 : 0"), 1.0);
        assert_eq!(n("Function('return this')() === window ? 1 : 0"),
                   1.0);
    }

    #[test]
    fn optional_method_calls() {
        assert_eq!(
            n("var o = {m: function() { return this.v; }, v: 7}; \
               o.m?.()"), 7.0);
        assert_eq!(n("var o = {}; o.m?.() === undefined ? 1 : 0"), 1.0);
        assert_eq!(n("var o = null; o?.m?.() === undefined ? 1 : 0"), 1.0);
        assert_eq!(
            n("var o = {m: function(a, b) { return a * b + this.v; }, \
               v: 1}; o['m']?.(6, 7)"), 43.0);
    }

    #[test]
    fn bitwise_ops() {
        assert_eq!(n("(5 & 3) + (5 | 3) * 10 + (5 ^ 3) * 100"), 671.0);
        assert_eq!(n("1 << 5"), 32.0);
        assert_eq!(n("-8 >> 1"), -4.0);
        assert_eq!(n("-1 >>> 28"), 15.0);
        assert_eq!(n("~5"), -6.0);
        assert_eq!(n("'7' & 3"), 3.0); // ToInt32 coercion
        assert_eq!(n("4294967296 | 0"), 0.0); // 2^32 wraps to 0
        assert_eq!(n("var x = 6; x &= 3; x |= 8; x"), 10.0);
    }

    #[test]
    fn update_on_members() {
        assert_eq!(n("var o = {x: 5}; var a = o.x++; a * 10 + o.x"), 56.0);
        assert_eq!(n("var o = {x: 5}; var a = ++o.x; a * 10 + o.x"), 66.0);
        assert_eq!(n("var o = {x: 5}; o.x--; o.x"), 4.0);
        assert_eq!(n("var a = [7]; a[0]++; ++a[0]; a[0]"), 9.0);
        assert_eq!(n("var o = {n: '5'}; o.n++; o.n"), 6.0);
        assert_eq!(
            n("var i = 0; var a = [3, 3]; a[i++]++; a[0] * 10 + i"), 41.0);
    }

    #[test]
    fn delete_operator() {
        // removes the property (reads, in, keys all agree)
        assert_eq!(n("var o = {a: 1, b: 2}; delete o.a; \
                      (o.a === undefined ? 1 : 0) + ('a' in o ? 10 : 0) \
                      + Object.keys(o).length * 100"), 101.0);
        // computed member + array element + expression value
        assert_eq!(n("var o = {}; o['k'] = 1; delete o['k']; \
                      'k' in o ? 1 : 0"), 0.0);
        assert_eq!(n("var a = [1, 2, 3]; delete a[1]; \
                      (a[1] === undefined ? 1 : 0) + a.length"), 4.0);
        assert_eq!(n("var o = {x: 1}; (delete o.x) === true ? 1 : 0"),
                   1.0);
        // remaining properties keep their values after the shape move
        assert_eq!(n("var o = {a: 1, b: 2, c: 3}; delete o.b; \
                      o.a * 10 + o.c"), 13.0);
        // deleting a missing prop / bare ident is fine
        assert_eq!(n("var o = {}; delete o.nope; delete window; 1"), 1.0);
    }

    #[test]
    fn in_and_instanceof() {
        // `in`: own properties, array indices, length
        assert_eq!(n("var o = {a: 1}; ('a' in o) ? 1 : 0"), 1.0);
        assert_eq!(n("var o = {a: 1}; ('b' in o) ? 1 : 0"), 0.0);
        assert_eq!(n("var a = [7, 8]; (1 in a) ? 1 : 0"), 1.0);
        assert_eq!(n("var a = [7, 8]; (2 in a) ? 1 : 0"), 0.0);
        assert_eq!(n("('length' in [1]) ? 1 : 0"), 1.0);
        assert_eq!(n("var o = {}; o[3] = 'x'; ('3' in o) ? 1 : 0"), 1.0);
        // `in` on a non-object throws (catchably)
        assert_eq!(
            n("var r = 0; try { 'a' in 5; } catch (e) { r = 1; } r"),
            1.0
        );
        // instanceof: built-ins by identity
        assert_eq!(n("([] instanceof Array) ? 1 : 0"), 1.0);
        assert_eq!(n("({} instanceof Array) ? 1 : 0"), 0.0);
        assert_eq!(n("([] instanceof Object) ? 1 : 0"), 1.0);
        assert_eq!(n("({} instanceof Object) ? 1 : 0"), 1.0);
        assert_eq!(n("(5 instanceof Object) ? 1 : 0"), 0.0);
        assert_eq!(n("('s' instanceof String) ? 1 : 0"), 0.0);
        assert_eq!(
            n("(Promise.resolve(1) instanceof Promise) ? 1 : 0"),
            1.0
        );
        // user constructors: real prototype chains (07-11)
        assert_eq!(
            n("function F() {} (new F() instanceof F) ? 1 : 0"),
            1.0
        );
        // non-callable RHS throws (catchably)
        assert_eq!(
            n("var r = 0; try { [] instanceof 5; } catch (e) { r = 1; } r"),
            1.0
        );
    }

    #[test]
    fn arithmetic() {
        assert_eq!(n("1 + 2 * 3"), 7.0);
        assert_eq!(n("(1 + 2) * 3"), 9.0);
        assert_eq!(n("10 / 4"), 2.5);
        assert_eq!(n("2 ** 3 ** 2"), 512.0);
        assert_eq!(n("7 % 3"), 1.0);
        assert_eq!(n("-7 % 3"), -1.0);
        assert_eq!(n("-(3 + 4)"), -7.0);
        assert_eq!(n("2147483647 + 1"), 2147483648.0);
        assert_eq!(n("100000 * 100000"), 10000000000.0);
    }

    #[test]
    fn variables_and_control_flow() {
        assert_eq!(n("var x = 5; x = x + 1; x"), 6.0);
        assert_eq!(n("var a = 1, b = 2; a += b * 3; a"), 7.0);
        assert_eq!(n("var i = 5; var j = i++; j * 10 + i"), 56.0);
        assert_eq!(n("var i = 5; var j = ++i; j * 10 + i"), 66.0);
        assert_eq!(n("var x; if (1 < 2) x = 10; else x = 20; x"), 10.0);
        assert_eq!(
            n("var s = 0; var i = 0; while (i < 10) { s += i; i++; } s"),
            45.0
        );
        assert_eq!(
            n("var s = 0; for (var i = 0; i < 10; i++) s += i; s"),
            45.0
        );
        assert_eq!(n("var i = 0; do { i++; } while (i < 3); i"), 3.0);
        assert_eq!(
            n("var s = 0; \
               for (var i = 0; i < 10; i++) { \
                   if (i == 5) break; \
                   if (i % 2 == 1) continue; \
                   s += i; \
               } s"),
            6.0
        );
    }

    #[test]
    fn logic_equality_ternary() {
        assert_eq!(n("1 && 2"), 2.0);
        assert_eq!(n("var a = 0; a || 5"), 5.0);
        assert_eq!(n("1 < 2 ? 10 : 20"), 10.0);
        assert_eq!(n("true + true"), 2.0);
        assert_eq!(
            n("var c = 0; \
               function inc() { c = c + 1; return 1; } \
               0 && inc(); 1 || inc(); c"),
            0.0
        );
        assert_eq!(n("(1 === 1.0) ? 1 : 0"), 1.0);
        assert_eq!(n("(0 == false) ? 1 : 0"), 1.0);
        assert_eq!(n("(undefined == null) ? 1 : 0"), 1.0);
        assert_eq!(n("(undefined === null) ? 1 : 0"), 0.0);
        assert_eq!(n("(0 / 0 === 0 / 0) ? 1 : 0"), 0.0);
    }

    #[test]
    fn functions_and_closures() {
        assert_eq!(
            n("function fib(n) { return n < 2 ? n : fib(n-1) + fib(n-2); } \
               fib(20)"),
            6765.0
        );
        assert_eq!(
            n("function even(n) { return n == 0 ? true : odd(n - 1); } \
               function odd(n) { return n == 0 ? false : even(n - 1); } \
               even(10) ? 1 : 0"),
            1.0
        );
        assert_eq!(n("var g = (a, b) => a * b; g(6, 7)"), 42.0);
        assert_eq!(
            n("function counter() { var c = 0; \
                   return function () { c += 1; return c; }; } \
               var a = counter(); var b = counter(); \
               a(); a(); b(); a() * 10 + b()"),
            32.0
        );
        assert_eq!(
            n("function outer() { \
                   function fib(n) { \
                       return n < 2 ? n : fib(n - 1) + fib(n - 2); } \
                   return fib(10); } \
               outer()"),
            55.0
        );
    }

    #[test]
    fn objects_arrays_strings() {
        assert_eq!(n("var o = {x: 1, y: 2}; o.x + o.y"), 3.0);
        assert_eq!(n("var o = {}; o.a = 1; o.b = 2; o.a + o.b"), 3.0);
        assert_eq!(n("var o = {n: 10}; o.n += 5; o.n"), 15.0);
        assert_eq!(
            n("function get(p) { return p.x; } \
               var a = {x: 1}; var b = {y: 9, x: 2}; \
               get(a) + get(b) + get(a)"),
            4.0
        );
        assert_eq!(n("var a = [1, 2, 3]; a[0] + a[2]"), 4.0);
        assert_eq!(n("var a = []; a.push(5); a.push(7); a.length"), 2.0);
        assert_eq!(
            n("var a = [3, 1, 2]; \
               a.sort(function (x, y) { return x - y; }); \
               a[0] * 100 + a[1] * 10 + a[2]"),
            123.0
        );
        assert_eq!(n("'abc'.length"), 3.0);
        assert_eq!(
            n("var s = ''; for (var i = 0; i < 5000; i++) s += 'xy'; \
               s.length"),
            10000.0
        );
        assert_eq!(n("('a' + 'b' === 'ab') ? 1 : 0"), 1.0);
        assert_eq!(n("(1.5).toFixed(2) === '1.50' ? 1 : 0"), 1.0);
        assert_eq!(n("typeof nope_undeclared === 'undefined' ? 1 : 0"), 1.0);
        assert_eq!(
            n("var o = {v: 7, get() { return this.v; }}; o.get()"),
            7.0
        );
        let (_, logs) =
            eval("console.log('BENCH x ' + (42).toFixed(2)); 0").unwrap();
        assert_eq!(logs, vec!["BENCH x 42.00"]);
    }

    #[test]
    fn destructuring() {
        // object destructuring
        assert_eq!(n("var {a, b} = {a: 3, b: 4}; a * 10 + b"), 34.0);
        // rename + default
        assert_eq!(
            n("var {x: p, y: q = 9} = {x: 5}; p * 100 + q"), 509.0);
        // array destructuring + hole + rest
        assert_eq!(
            n("var [first, , third, ...rest] = [1, 2, 3, 4, 5]; \
               first * 1000 + third * 100 + rest.length * 10 + rest[0]"),
            1324.0);
        // array default
        assert_eq!(n("var [a = 7, b = 8] = [1]; a * 10 + b"), 18.0);
        // nested
        assert_eq!(
            n("var {p: [m, n]} = {p: [2, 3]}; m * 10 + n"), 23.0);
        // works with let/const too
        assert_eq!(n("let [u, v] = [6, 7]; u * 10 + v"), 67.0);
        // destructuring from a function result (temp is evaluated once)
        assert_eq!(
            n("var calls = 0; \
               function src() { calls++; return {a: 4, b: 5}; } \
               var {a, b} = src(); a * 100 + b * 10 + calls"),
            451.0);
    }

    #[test]
    fn param_defaults_and_patterns() {
        // default parameters
        assert_eq!(
            n("function f(a, b = 10) { return a + b; } f(5)"), 15.0);
        assert_eq!(
            n("function f(a, b = 10) { return a + b; } f(5, 2)"), 7.0);
        // default only applies to undefined, not other falsy values
        assert_eq!(
            n("function f(x = 3) { return x; } f(0)"), 0.0);
        // object-pattern parameter
        assert_eq!(
            n("function area({w, h}) { return w * h; } \
               area({w: 4, h: 5})"), 20.0);
        // array-pattern parameter with default inside
        assert_eq!(
            n("function f([a, b = 9]) { return a * 10 + b; } f([1])"),
            19.0);
        // pattern param on an arrow, plus default
        assert_eq!(
            n("var g = ({x}, k = 2) => x * k; g({x: 5})"), 10.0);
        // rest params are cleanly rejected (not silently wrong)
        assert!(eval("function f(...xs) { return xs.length; } f(1,2)")
            .is_err());
    }

    #[test]
    fn classes_and_new() {
        // function constructor + new
        assert_eq!(
            n("function A(x) { this.x = x; } new A(7).x"), 7.0);
        // class with constructor + method
        assert_eq!(
            n("class C { constructor(x) { this.x = x; } \
                        getX() { return this.x; } } \
               new C(5).getX()"), 5.0);
        // method using another method via this
        assert_eq!(
            n("class Box { constructor(w, h) { this.w = w; this.h = h; } \
                          area() { return this.w * this.h; } \
                          doubled() { return this.area() * 2; } } \
               new Box(3, 4).doubled()"), 24.0);
        // class expression + default param in constructor
        assert_eq!(
            n("var K = class { constructor(n = 10) { this.n = n; } }; \
               new K().n"), 10.0);
        // instance fields persist across method calls
        assert_eq!(
            n("class Counter { constructor() { this.c = 0; } \
                              inc() { this.c = this.c + 1; return this.c; } } \
               var c = new Counter(); c.inc(); c.inc(); c.inc()"), 3.0);
        // no-constructor class
        assert_eq!(
            n("class P { hi() { return 42; } } new P().hi()"), 42.0);
        // extends is a clean error (not silently wrong)
        assert!(eval("class B extends A {}").is_err());
    }

    #[test]
    fn regex_basics() {
        // literal + .test
        assert_eq!(n("/ab+/.test('xabbby') ? 1 : 0"), 1.0);
        assert_eq!(n("/ab+/.test('xyz') ? 1 : 0"), 0.0);
        // flags (case-insensitive)
        assert_eq!(n("/HELLO/i.test('hello world') ? 1 : 0"), 1.0);
        // String.search
        assert_eq!(n("'a1b2c3'.search(/\\d/)"), 1.0);
        assert_eq!(n("'abc'.search(/\\d/)"), -1.0);
        // String.replace with regex (first + global)
        let (_, l) = eval(
            "console.log('a1b2c3'.replace(/\\d/, '#'))").unwrap();
        assert_eq!(l, vec!["a#b2c3"]);
        let (_, l) = eval(
            "console.log('a1b2c3'.replace(/\\d/g, '#'))").unwrap();
        assert_eq!(l, vec!["a#b#c#"]);
        // $& backref
        let (_, l) = eval(
            "console.log('cat'.replace(/c/, '[$&]'))").unwrap();
        assert_eq!(l, vec!["[c]at"]);
        // String.match (non-global -> groups)
        let (_, l) = eval(
            "var m = 'a12b'.match(/(\\d)(\\d)/); \
             console.log(m[0] + ',' + m[1] + ',' + m[2])").unwrap();
        assert_eq!(l, vec!["12,1,2"]);
        // String.match (global -> all)
        assert_eq!(n("'a1b2c3'.match(/\\d/g).length"), 3.0);
        // no match -> null
        assert_eq!(n("'abc'.match(/\\d/) === null ? 1 : 0"), 1.0);
        // split by regex
        assert_eq!(n("'a1b2c'.split(/\\d/).length"), 3.0);
        // exec
        assert_eq!(n("/(\\d+)/.exec('abc42')[1] === '42' ? 1 : 0"), 1.0);
        // regex is an object
        assert_eq!(n("typeof /x/ === 'object' ? 1 : 0"), 1.0);
        // unsupported pattern errors cleanly (backreference)
        assert!(eval("/(a)\\1/.test('aa')").is_err());
    }

    #[test]
    fn spread_calls_and_apply() {
        // Function.prototype.apply / call
        assert_eq!(
            n("function add(a, b, c) { return a + b + c; } \
               add.apply(null, [1, 2, 3])"), 6.0);
        assert_eq!(
            n("function add(a, b) { return a + b; } add.call(null, 4, 5)"),
            9.0);
        // spread in a plain call
        assert_eq!(
            n("function add(a, b, c) { return a + b + c; } \
               var xs = [10, 20, 30]; add(...xs)"), 60.0);
        // spread mixed with fixed args
        assert_eq!(
            n("function f(a, b, c, d) { return a*1000+b*100+c*10+d; } \
               f(1, ...[2, 3], 4)"), 1234.0);
        // Math.max with spread (host fn via apply)
        assert_eq!(n("Math.max(...[3, 9, 5])"), 9.0);
        // spread in a method call keeps the receiver as `this`
        assert_eq!(
            n("var o = { base: 100, add: function (a, b) { \
                   return this.base + a + b; } }; \
               o.add(...[2, 3])"), 105.0);
    }

    #[test]
    fn array_spread() {
        assert_eq!(n("var a = [1, 2]; var b = [...a, 3]; b.length"), 3.0);
        assert_eq!(n("var a = [2, 3]; [1, ...a, 4][2]"), 3.0);
        assert_eq!(
            n("var a = [1], b = [2, 3]; [...a, ...b].length"), 3.0);
        assert_eq!(n("[...[9]][0]"), 9.0);
        // no spread -> still a plain array literal
        assert_eq!(n("[7, 8, 9][1]"), 8.0);
    }

    #[test]
    fn semantic_fixes() {
        // unary + is ToNumber, not identity
        assert_eq!(n("+'42'"), 42.0);
        assert_eq!(n("+'3.5' + 1"), 4.5);
        assert_eq!(n("+true"), 1.0);
        assert!(n("+'abc'").is_nan());
        // string coercion in arithmetic / relational / loose-eq / sort
        assert_eq!(n("'3' * 2"), 6.0);
        assert_eq!(n("'10' - '4'"), 6.0);
        assert_eq!(n("(2 < '10') ? 1 : 0"), 1.0);   // numeric compare
        assert_eq!(n("('10' < '9') ? 1 : 0"), 1.0);  // string compare
        assert_eq!(n("(1 == '1') ? 1 : 0"), 1.0);
        // default sort is lexicographic by string: [10,2,1] -> [1,10,2]
        let (_, logs) = eval(
            "var a = [10, 2, 1]; a.sort(); console.log(a[0]+','+a[1]+','+a[2])",
        )
        .unwrap();
        assert_eq!(logs, vec!["1,10,2"]);
        // function declarations are hoisted (callable before definition)
        assert_eq!(n("f(); function f() {} 1"), 1.0);
        assert_eq!(
            n("var r = fact(5); function fact(n){ \
               return n <= 1 ? 1 : n * fact(n-1); } r"),
            120.0
        );
        // evaluation-order: left operand read before right's side effect
        assert_eq!(n("var x = 5; x + (x = 10)"), 15.0);
        assert_eq!(n("var i = 3; var s = i + i++; s"), 6.0);
    }

    #[test]
    fn arrow_lexical_this() {
        // an arrow inside a method sees the method's `this`
        assert_eq!(
            n("var o = { v: 7, get: function () { \
                   var f = () => this.v; return f(); } }; o.get()"),
            7.0
        );
        // arrow passed as a callback keeps lexical this (not the callee's)
        assert_eq!(
            n("var o = { v: 9, run: function () { \
                   var a = [1]; var out = 0; \
                   a.forEach(() => { out = this.v; }); return out; } }; \
               o.run()"),
            9.0
        );
        // a regular function, by contrast, does NOT capture this
        let (v, _) = eval(
            "var o = { v: 5, get: function () { \
                 var f = function () { return this; }; return f(); } }; \
             o.get()",
        )
        .unwrap();
        assert!(v.is_undefined(), "plain call this should be undefined");
    }

    #[test]
    fn block_scoping() {
        // let binds to the innermost block and shadows correctly
        assert_eq!(n("let x = 1; { let x = 2; x = x + 10; } x"), 1.0);
        assert_eq!(
            n("var r = 0; let x = 1; { let x = 2; r = x; } r * 10 + x"),
            21.0
        );
        assert_eq!(
            n("function f() { let a = 1; { let a = 2; } return a; } f()"),
            1.0
        );
        // no shadow: an inner assignment hits the outer binding
        assert_eq!(
            n("function f() { let a = 1; { a = 7; } return a; } f()"),
            7.0
        );
        // sibling blocks are independent
        assert_eq!(
            n("var s = 0; { let a = 1; s += a; } { let a = 2; s += a; } s"),
            3.0
        );
        // a block let does not leak into the enclosing scope
        assert!(eval("{ let q = 1; } q").is_err());
        // let without initializer is undefined, and assignable
        assert_eq!(
            n("var r; { let u; r = (u === undefined) ? 1 : 0; u = 5; \
               r = r * 10 + u; } r"),
            15.0
        );
        // shadowing a function-scoped var inside a block
        assert_eq!(
            n("function f() { var v = 1; { let v = 2; } return v; } f()"),
            1.0
        );
    }

    #[test]
    fn let_per_iteration_capture() {
        // the classic: closures over a for-let see per-iteration values
        assert_eq!(
            n("var fns = []; \
               for (let i = 0; i < 3; i++) \
                   fns.push(function () { return i; }); \
               var a = fns[0]; var b = fns[1]; var c = fns[2]; \
               a() * 100 + b() * 10 + c()"),
            12.0
        );
        // same with arrows
        assert_eq!(
            n("var fns = []; \
               for (let i = 0; i < 3; i++) fns.push(() => i); \
               var a = fns[0]; var b = fns[1]; var c = fns[2]; \
               a() * 100 + b() * 10 + c()"),
            12.0
        );
        // var, by contrast, shares one binding across iterations
        assert_eq!(
            n("var fns = []; \
               for (var i = 0; i < 3; i++) fns.push(() => i); \
               var a = fns[0]; var b = fns[1]; var c = fns[2]; \
               a() * 100 + b() * 10 + c()"),
            333.0
        );
        // per-iteration binding also holds inside a function
        assert_eq!(
            n("function make() { var fs = []; \
                   for (let i = 0; i < 3; i++) fs.push(() => i); \
                   return fs; } \
               var fs = make(); \
               var a = fs[0]; var b = fs[1]; var c = fs[2]; \
               a() * 100 + b() * 10 + c()"),
            12.0
        );
        // continue still produces a fresh binding for the next pass
        assert_eq!(
            n("var fns = []; \
               for (let i = 0; i < 4; i++) { \
                   if (i == 1) continue; \
                   fns.push(() => i); \
               } \
               var a = fns[0]; var b = fns[1]; var c = fns[2]; \
               a() * 100 + b() * 10 + c()"),
            23.0
        );
        // mutating i in the body is visible to that iteration's
        // closure, but the update runs on the next iteration's copy
        assert_eq!(
            n("var fns = []; \
               for (let i = 0; i < 3; i++) { fns.push(() => i); i += 0; } \
               var a = fns[0]; var b = fns[1]; var c = fns[2]; \
               a() * 100 + b() * 10 + c()"),
            12.0
        );
        // a block-scoped let is fresh on every pass through the block
        assert_eq!(
            n("var out = []; var j = 0; \
               while (j < 3) { let v = j * 10; out.push(() => v); j++; } \
               var a = out[0]; var b = out[1]; var c = out[2]; \
               a() + b() + c()"),
            30.0
        );
        // for-of const: one binding per element
        assert_eq!(
            n("var fns = []; \
               for (const v of [1, 2, 3]) fns.push(() => v); \
               var a = fns[0]; var b = fns[1]; var c = fns[2]; \
               a() * 100 + b() * 10 + c()"),
            123.0
        );
        // nested for-let loops: both levels are per-iteration
        assert_eq!(
            n("var fns = []; \
               for (let i = 0; i < 2; i++) \
                   for (let j = 0; j < 2; j++) \
                       fns.push(() => i * 10 + j); \
               var a = fns[0]; var b = fns[1]; \
               var c = fns[2]; var d = fns[3]; \
               a() * 1000 + b() * 100 + c() * 10 + d()"),
            // captured values: 0, 1, 10, 11
            211.0
        );
    }

    #[test]
    fn script_level_let_is_shared_across_scripts() {
        // top-level let/const compile to globals so later <script>s on
        // the page see them, like the shared script lexical scope in
        // real browsers
        let mut vm = PageVm::new(None);
        let logs = vm.run_scripts(&[
            "let counter = 40; const step = 2;".to_string(),
            "counter += step; console.log(counter);".to_string(),
        ]);
        assert_eq!(logs, vec!["42"]);
    }

    #[test]
    fn const_reassignment_is_a_compile_error() {
        assert!(eval("const c = 1; c = 2;").is_err());
        assert!(eval("const c = 1; c += 1;").is_err());
        assert!(eval("const c = 1; c++;").is_err());
        assert!(eval("{ const b = 1; b = 2; }").is_err());
        assert!(eval("for (const v of [1, 2]) v = 9;").is_err());
        // rejected at compile time even if the function never runs
        assert!(eval("function f() { const k = 5; k = 6; } 1").is_err());
        assert!(eval("const c;").is_err(), "const needs an initializer");
        // reading and shadowing a const is fine
        assert_eq!(n("const c = 40; c + 2"), 42.0);
        assert_eq!(n("const c = 1; { const c = 2; } c"), 1.0);
        // only the binding is frozen, not the object it names
        assert_eq!(n("const o = {n: 1}; o.n = 5; o.n"), 5.0);
    }

    #[test]
    fn tdz_use_before_declaration_errors() {
        assert!(eval("{ x; let x = 1; }").is_err());
        assert!(eval("{ x = 5; let x; }").is_err());
        assert!(eval("{ x++; let x = 1; }").is_err());
        assert!(
            eval("function f() { return k; let k = 1; } f()").is_err()
        );
        // a closure calling into the TDZ throws...
        assert!(eval(
            "{ let f = function () { return y; }; f(); let y = 1; }"
        )
        .is_err());
        // ...but works once the binding is initialized
        assert_eq!(
            n("var r = 0; \
               { let f = function () { return y; }; let y = 7; r = f(); } \
               r"),
            7.0
        );
        assert_eq!(n("{ let x = 1; x }"), 1.0);
    }

    #[test]
    fn try_catch_basics() {
        assert_eq!(
            n("var r = 0; try { throw 5; r = 1; } catch (e) { r = e; } r"),
            5.0
        );
        // no throw: catch is skipped
        assert_eq!(
            n("var r = 0; try { r = 1; } catch (e) { r = 2; } r"),
            1.0
        );
        // engine errors are catchable and Error-shaped
        assert_eq!(
            n("var r = 0; try { nope(); } catch (e) { \
                   r = e.message.length > 0 ? 1 : 0; } r"),
            1.0
        );
        // the thrown value passes through unchanged (any type)
        assert_eq!(
            n("var r; try { throw {code: 42}; } catch (e) { r = e.code; } r"),
            42.0
        );
        // rethrow reaches the outer catch; inner binding shadows
        assert_eq!(
            n("var r = 0; \
               try { try { throw 1; } catch (a) { throw a + 1; } } \
               catch (b) { r = b * 10; } r"),
            20.0
        );
        // a throw deep in a call stack unwinds to the catch
        assert_eq!(
            n("function boom(k) { if (k == 0) throw 7; return boom(k-1); } \
               var r = 0; try { boom(5); } catch (e) { r = e; } r"),
            7.0
        );
        // catch binding is block-scoped and assignable
        assert_eq!(
            n("var e = 1; try { throw 5; } catch (e) { e = e + 1; } e"),
            1.0
        );
        // uncaught throws surface as errors
        assert!(eval("throw 'boom';").is_err());
        // ...and do not leak frames or handlers into the page VM
        assert_eq!(
            n("function f() { try { return g(); } finally { 0; } } \
               function g() { throw 1; } \
               var r = 0; \
               for (var i = 0; i < 100; i++) { \
                   try { f(); } catch (e) { r++; } \
               } r"),
            100.0
        );
    }

    #[test]
    fn try_finally_paths() {
        // finally runs on the normal path
        assert_eq!(
            n("var r = 0; try { r += 1; } finally { r += 10; } r"),
            11.0
        );
        // finally runs on the exception path, then rethrows
        assert_eq!(
            n("var r = 0; \
               try { try { throw 1; } finally { r += 10; } } \
               catch (e) { r += e; } r"),
            11.0
        );
        // try/catch/finally: all three regions run in order
        assert_eq!(
            n("var r = ''; \
               try { r += 't'; throw 0; } catch (e) { r += 'c'; } \
               finally { r += 'f'; } \
               r === 'tcf' ? 1 : 0"),
            1.0
        );
        // an exception in the catch body still runs the finally
        assert_eq!(
            n("var r = 0; \
               try { try { throw 1; } catch (e) { throw 2; } \
                     finally { r += 10; } } \
               catch (e) { r += e; } r"),
            12.0
        );
        // return through finally: value computed first, finally runs
        assert_eq!(
            n("var log = 0; \
               function f() { \
                   var x = 1; \
                   try { return x; } finally { log = 1; x = 99; } \
               } \
               f() * 10 + log"),
            11.0
        );
        // return inside finally overrides the pending return
        assert_eq!(
            n("function f() { try { return 1; } finally { return 2; } } \
               f()"),
            2.0
        );
        // break through finally (finally runs each time the loop exits)
        assert_eq!(
            n("var r = 0; \
               for (var i = 0; i < 5; i++) { \
                   try { if (i == 2) break; r += 1; } \
                   finally { r += 10; } \
               } r"),
            32.0
        );
        // continue through finally
        assert_eq!(
            n("var r = 0; \
               for (var i = 0; i < 3; i++) { \
                   try { if (i == 1) continue; r += 1; } \
                   finally { r += 10; } \
               } r"),
            32.0
        );
        // nested finallies unwind innermost-first on return
        assert_eq!(
            n("var r = ''; \
               function f() { \
                   try { try { return 'v'; } finally { r += 'a'; } } \
                   finally { r += 'b'; } \
               } \
               f(); r === 'ab' ? 1 : 0"),
            1.0
        );
    }

    #[test]
    fn catch_binding_captured_by_closure() {
        // the catch param can be closed over (it lives in a cell)
        assert_eq!(
            n("var f; try { throw 21; } catch (e) { f = () => e * 2; } \
               f()"),
            42.0
        );
        // one closure per catch entry sees that entry's exception
        assert_eq!(
            n("var fs = []; \
               for (var i = 0; i < 3; i++) { \
                   try { throw i; } \
                   catch (e) { fs.push(function () { return e; }) } \
               } \
               fs[0]() * 100 + fs[1]() * 10 + fs[2]()"),
            12.0
        );
    }

    #[test]
    fn computed_member_calls() {
        // a[i]() — the call works and `this` is the receiver
        assert_eq!(
            n("var fs = [function () { return 5; }]; fs[0]()"),
            5.0
        );
        assert_eq!(
            n("var o = {v: 7, get: function () { return this.v; }}; \
               var k = 'get'; o[k]()"),
            7.0
        );
        // computed callee with arguments
        assert_eq!(
            n("var ops = {add: function (a, b) { return a + b; }}; \
               ops['add'](19, 23)"),
            42.0
        );
        // per-iteration let capture, called back through the array
        assert_eq!(
            n("let fs = []; \
               for (let i = 0; i < 3; i++) { \
                   fs.push(function () { return i; }); \
               } \
               fs[0]() * 100 + fs[1]() * 10 + fs[2]()"),
            12.0
        );
    }

    #[test]
    fn builtin_conversions() {
        assert_eq!(n("String(42).length"), 2.0);
        assert_eq!(n("Number('3.5') + 1"), 4.5);
        assert_eq!(n("Number('') "), 0.0);
        assert_eq!(n("parseInt('42px')"), 42.0);
        assert_eq!(n("parseInt('ff', 16)"), 255.0);
        assert_eq!(n("parseFloat('3.14 is pi')"), 3.14);
        assert_eq!(n("Boolean(0) === false ? 1 : 0"), 1.0);
        assert_eq!(n("Boolean('x') === true ? 1 : 0"), 1.0);
        assert_eq!(n("(String(1) + String(2)) === '12' ? 1 : 0"), 1.0);
    }

    #[test]
    fn errors() {
        assert!(eval("nope()").is_err());
        assert!(eval("var x = 1; x()").is_err());
        assert!(
            eval("function f() { return f() + 1; } f()").is_err(),
            "must hit the frame limit, not crash"
        );
    }

    #[test]
    fn vm_stability_regressions() {
        // parseInt with an out-of-range radix: NaN, not a Rust panic
        assert_eq!(
            n("parseInt('10', 40) !== parseInt('10', 40) ? 1 : 0"),
            1.0
        );
        // cyclic array display terminates (cycle renders empty, like join)
        let (_, logs) =
            eval("var a = [1, 2]; a.push(a); console.log(a); 0").unwrap();
        assert_eq!(logs, vec!["1,2,"]);
        // cyclic JSON.stringify throws instead of blowing the stack
        assert!(eval("var o = {}; o.self = o; JSON.stringify(o)").is_err());
        assert!(
            eval("var a = []; a.push(a); JSON.stringify(a)").is_err()
        );
        // absurdly deep JSON input errors cleanly
        let deep = format!(
            "JSON.parse('{}1{}')",
            "[".repeat(600),
            "]".repeat(600)
        );
        assert!(eval(&deep).is_err());
        // re-entry through native callbacks (sort -> JS -> sort ...)
        // hits a depth limit instead of overflowing the native stack.
        // Each call sorts a *fresh* array (sort empties the one it is
        // sorting, so re-sorting the same array would just terminate).
        assert!(eval(
            "function boom() { \
                 var b = [2, 1]; \
                 b.sort(function (x, y) { boom(); return x - y; }); \
                 return 0; } \
             boom()"
        )
        .is_err());
    }

    #[test]
    fn thrown_errors_do_not_leak_frames() {
        // Each throw inside a called function used to leak one frame in
        // the persistent VM until every call died with "stack overflow".
        let mut vm = PageVm::new(None);
        for _ in 0..5000 {
            assert!(vm
                .run_source("function f() { nope(); } f()")
                .is_err());
        }
        let v = vm.run_source("6 * 7").unwrap();
        assert_eq!(v.to_number_raw(), 42.0);
    }

    #[test]
    fn dom_cycle_rejected() {
        let (mut vm, _doc) = dom_vm(
            "<html><body><div id=a><div id=b></div></div></body></html>",
        );
        let logs = vm.run_scripts(&["\
            var a = document.getElementById('a');\n\
            var b = document.getElementById('b');\n\
            b.appendChild(a);\n"
            .to_string()]);
        assert!(
            logs.first().is_some_and(|l| l.starts_with("[gg-js error]")),
            "cycle must be rejected: {logs:?}"
        );
        // the tree is still intact and walkable afterwards
        let logs = vm.run_scripts(&["\
            console.log(document.getElementById('a').innerHTML);\n"
            .to_string()]);
        assert_eq!(logs, vec!["<div id=\"b\"></div>"]);
    }

    #[test]
    fn state_persists_across_scripts() {
        let mut vm = PageVm::new(None);
        let logs = vm.run_scripts(&[
            "var total = 40; function add(n) { total += n; }".to_string(),
            "add(2); console.log('total ' + total);".to_string(),
        ]);
        assert_eq!(logs, vec!["total 42"]);
        // shapes/ICs survive too: same object accessed from a 3rd script
        let logs = vm.run_scripts(&[
            "var o = {x: 1};".to_string(),
            "o.x = 5; console.log(o.x);".to_string(),
        ]);
        assert_eq!(logs, vec!["5"]);
    }

    // ---- async runtime (P3) ----

    #[test]
    fn async_microtask_before_timer() {
        let mut vm = PageVm::new(None);
        let logs = vm.run_scripts(&["\
            console.log('sync');\n\
            setTimeout(function () { console.log('timer'); }, 0);\n\
            Promise.resolve().then(function () { console.log('micro'); });\n"
            .to_string()]);
        assert_eq!(logs, vec!["sync"]); // run-to-completion: only sync ran
        let (logs, fetches) = vm.pump();
        assert!(fetches.is_empty());
        // microtasks drain before any macrotask (timer)
        assert_eq!(logs, vec!["micro", "timer"]);
    }

    #[test]
    fn async_promise_chain_and_catch() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            Promise.resolve(3)\n\
              .then(function (x) { return x * 2; })\n\
              .then(function (x) { console.log('val ' + x); });\n\
            Promise.reject('boom')\n\
              .catch(function (e) { console.log('caught ' + e); });\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert!(logs.contains(&"val 6".to_string()), "{logs:?}");
        assert!(logs.contains(&"caught boom".to_string()), "{logs:?}");
    }

    #[test]
    fn async_timer_virtual_clock_order() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            setTimeout(function () { console.log('b 50'); }, 50);\n\
            setTimeout(function () { console.log('a 10'); }, 10);\n\
            setTimeout(function () { console.log('c 50'); }, 50);\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        // fire by due time; equal due (50) keeps insertion order (FIFO)
        assert_eq!(logs, vec!["a 10", "b 50", "c 50"]);
    }

    #[test]
    fn async_fetch_then_text_chain() {
        // fetch() defers to the host: pump reports it, we settle it, and
        // the .then(r => r.text()).then(t => ...) chain materializes.
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            fetch('http://example/data')\n\
              .then(function (r) { return r.text(); })\n\
              .then(function (t) { console.log('got:' + t); });\n"
            .to_string()]);
        let (_logs, fetches) = vm.pump();
        assert_eq!(fetches.len(), 1);
        let (fid, url) = fetches[0].clone();
        assert_eq!(url, "http://example/data");
        vm.resolve_fetch(fid, 200, "HELLO".to_string());
        let (logs, more) = vm.pump();
        assert!(more.is_empty());
        assert!(!vm.has_pending_work());
        assert_eq!(logs, vec!["got:HELLO"]);
    }

    #[test]
    fn async_fetch_json_and_status() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            fetch('/api')\n\
              .then(function (r) { console.log('ok ' + r.ok + ' ' + r.status);\n\
                 return r.json(); })\n\
              .then(function (o) { console.log('n=' + o.n); });\n"
            .to_string()]);
        let (_l, fetches) = vm.pump();
        let (fid, _u) = fetches[0].clone();
        vm.resolve_fetch(fid, 200, "{\"n\": 42}".to_string());
        let (logs, _) = vm.pump();
        assert_eq!(logs, vec!["ok true 200", "n=42"]);
    }

    #[test]
    fn async_dom_mutation_via_timer() {
        // the payoff: a timer callback mutates the DOM (the SPA pattern)
        let (mut vm, doc) = dom_vm(
            "<html><body><div id=root></div></body></html>");
        vm.run_scripts(&["\
            setTimeout(function () {\n\
              var d = document.createElement('div');\n\
              d.setAttribute('id', 'loaded');\n\
              d.textContent = 'hi';\n\
              document.getElementById('root').appendChild(d);\n\
            }, 30);\n"
            .to_string()]);
        assert!(doc.borrow().get_element_by_id("loaded").is_none());
        vm.pump();
        let d = doc.borrow();
        let loaded = d.get_element_by_id("loaded").expect("timer ran");
        assert_eq!(d.collect_text(loaded), "hi");
    }

    // ---- host objects + new Promise (P3b) ----

    #[test]
    fn host_math() {
        assert_eq!(n("Math.floor(3.7)"), 3.0);
        assert_eq!(n("Math.ceil(3.1)"), 4.0);
        assert_eq!(n("Math.round(2.5)"), 3.0);
        assert_eq!(n("Math.abs(-8)"), 8.0);
        assert_eq!(n("Math.max(1, 9, 4, 7)"), 9.0);
        assert_eq!(n("Math.min(5, 2, 8)"), 2.0);
        assert_eq!(n("Math.sqrt(144)"), 12.0);
        assert_eq!(n("Math.pow(2, 10)"), 1024.0);
        assert_eq!(n("(Math.PI > 3.14 && Math.PI < 3.15) ? 1 : 0"), 1.0);
        // Math.random is deterministic here but in [0,1)
        assert_eq!(n("var r = Math.random(); (r >= 0 && r < 1) ? 1 : 0"), 1.0);
    }

    #[test]
    fn host_object_and_array() {
        let mut vm = PageVm::new(None);
        let logs = vm.run_scripts(&["\
            var o = {a: 1, b: 2, c: 3};\n\
            console.log(Object.keys(o).join(','));\n\
            console.log(Object.values(o).join(','));\n\
            var m = Object.assign({}, o, {d: 4});\n\
            console.log(Object.keys(m).join(','));\n\
            console.log(Array.isArray([1,2]) + ',' + Array.isArray(3));\n\
            console.log(Array.from('abc').length);\n\
            console.log(isNaN(0/0) + ',' + isFinite(1/0));\n"
            .to_string()]);
        assert_eq!(logs, vec![
            "a,b,c", "1,2,3", "a,b,c,d", "true,false", "3", "true,false",
        ]);
    }

    #[test]
    fn new_promise_resolve_and_reject() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            new Promise(function (resolve) { resolve(42); })\n\
              .then(function (v) { console.log('got ' + v); });\n\
            new Promise(function (res, rej) { rej('bad'); })\n\
              .catch(function (e) { console.log('err ' + e); });\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert!(logs.contains(&"got 42".to_string()), "{logs:?}");
        assert!(logs.contains(&"err bad".to_string()), "{logs:?}");
    }

    #[test]
    fn new_promise_async_resolve_via_timer() {
        // the real pattern: resolve fires from a setTimeout callback
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            new Promise(function (resolve) {\n\
              setTimeout(function () { resolve('later'); }, 25);\n\
            }).then(function (v) { console.log(v); });\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert_eq!(logs, vec!["later"]);
    }

    #[test]
    fn new_promise_executor_throw_rejects() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            new Promise(function () { throw 'boom'; })\n\
              .catch(function (e) { console.log('caught ' + e); });\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert_eq!(logs, vec!["caught boom"]);
    }

    // ---- async/await syntax + Promise.all/race (P3c) ----

    #[test]
    fn async_function_no_await() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            async function f() { return 21 * 2; }\n\
            f().then(function (v) { console.log('r ' + v); });\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert_eq!(logs, vec!["r 42"]);
    }

    #[test]
    fn async_await_sequential() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            function delay(v) {\n\
              return new Promise(function (res) {\n\
                setTimeout(function () { res(v); }, 10);\n\
              });\n\
            }\n\
            async function run() {\n\
              var a = await delay(3);\n\
              var b = await delay(4);\n\
              return a * b;\n\
            }\n\
            run().then(function (v) { console.log('prod ' + v); });\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert_eq!(logs, vec!["prod 12"]);
    }

    #[test]
    fn async_arrow_and_await_expr() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            var g = async function () { return await Promise.resolve(9); };\n\
            g().then(function (v) { console.log('g ' + v); });\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert_eq!(logs, vec!["g 9"]);
    }

    #[test]
    fn async_await_error_propagates() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            async function bad() {\n\
              await Promise.resolve(1);\n\
              throw 'kaboom';\n\
            }\n\
            bad().catch(function (e) { console.log('caught ' + e); });\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert_eq!(logs, vec!["caught kaboom"]);
    }

    #[test]
    fn optional_chaining_and_nullish() {
        // ?? keeps the left unless null/undefined (not just falsy)
        assert_eq!(n("null ?? 7"), 7.0);
        assert_eq!(n("undefined ?? 7"), 7.0);
        assert_eq!(n("0 ?? 7"), 0.0); // 0 is not nullish
        assert_eq!(n("(('' ?? 7) === '') ? 1 : 0"), 1.0); // '' not nullish
        assert_eq!(n("var a = 5; a ?? 9"), 5.0);
        // optional member: nullish base short-circuits to undefined
        assert_eq!(n("var o = {a: {b: 3}}; o.a?.b"), 3.0);
        assert_eq!(n("var o = {}; (o.missing?.b === undefined) ? 1 : 0"), 1.0);
        // chain short-circuits the WHOLE rest, not just one link
        assert_eq!(
            n("var o = {}; (o.a?.b.c.d === undefined) ? 1 : 0"),
            1.0
        );
        // each optional link guards its own base
        assert_eq!(
            n("var o = {a: null}; (o.a?.b?.c === undefined) ? 1 : 0"),
            1.0
        );
        // computed optional + present chain
        assert_eq!(n("var o = {x: {y: 4}}; o?.['x']?.y"), 4.0);
        // optional method call `a?.b()` (receiver may be nullish)
        assert_eq!(
            n("var o = {g: function () { return 6; }}; o?.g()"),
            6.0
        );
        assert_eq!(
            n("var o = null; (o?.g() === undefined) ? 1 : 0"),
            1.0
        );
        // optional call of a plain callee `fn?.()`
        assert_eq!(
            n("var fn = function () { return 8; }; fn?.()"),
            8.0
        );
        assert_eq!(n("var fn = null; (fn?.() === undefined) ? 1 : 0"), 1.0);
        // ?? combined with a chain (common real pattern)
        assert_eq!(n("var o = {}; o.a?.b ?? 42"), 42.0);
    }

    #[test]
    fn computed_object_keys() {
        assert_eq!(n("var k = 'x'; var o = {[k]: 9}; o.x"), 9.0);
        assert_eq!(
            n("var i = 1; var o = {['p' + i]: 5}; o.p1"),
            5.0
        );
        assert_eq!(n("var o = {0: 'a', 1: 'b'}; o[0] === 'a' ? 1 : 0"), 1.0);
    }

    #[test]
    fn execution_fuel_kills_runaway_loops() {
        // a hostile infinite loop must not wedge the worker
        assert!(eval("while (true) {}").is_err());
        assert!(eval("for (;;) { var x = 1; }").is_err());
        assert!(eval("function f() { return f(); } f()").is_err());
        // the persistent VM survives a runaway script and keeps working
        let mut vm = PageVm::new(None);
        let logs = vm.run_scripts(&[
            "while (true) { }".to_string(),
            "console.log('alive ' + (6 * 7));".to_string(),
        ]);
        assert!(
            logs.iter().any(|l| l.contains("budget")),
            "runaway script should report the budget: {logs:?}"
        );
        assert!(logs.contains(&"alive 42".to_string()), "{logs:?}");
        // a legitimate heavy-but-finite loop still completes
        assert_eq!(
            {
                let (v, _) = eval(
                    "var s = 0; for (var i = 0; i < 100000; i++) s += i; s",
                )
                .unwrap();
                v.to_number_raw()
            },
            4999950000.0
        );
        // a runaway timer callback is killed but the pump continues
        let mut vm = PageVm::new(None);
        vm.run_scripts(&[
            "setTimeout(function () { while (true) {} }, 1);\n\
             setTimeout(function () { console.log('second ran'); }, 2);"
                .to_string(),
        ]);
        let (logs, _) = vm.pump();
        assert!(logs.contains(&"second ran".to_string()),
                "second timer must run after the first is fuel-killed: {logs:?}");
    }

    #[test]
    fn promise_all_and_race() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            Promise.all([Promise.resolve(1), Promise.resolve(2),\n\
                         Promise.resolve(3)])\n\
              .then(function (a) { console.log('all ' + a.join(',')); });\n\
            Promise.race([Promise.resolve('fast'), Promise.resolve('slow')])\n\
              .then(function (v) { console.log('race ' + v); });\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert!(logs.contains(&"all 1,2,3".to_string()), "{logs:?}");
        assert!(logs.contains(&"race fast".to_string()), "{logs:?}");
    }

    fn dom_vm(html_src: &str) -> (PageVm, Rc<RefCell<crate::dom::Document>>)
    {
        let doc = Rc::new(RefCell::new(html::parse(html_src)));
        (PageVm::new(Some(doc.clone())), doc)
    }

    #[test]
    fn dom_read_write() {
        let (mut vm, doc) = dom_vm(
            "<html><body><div id=a class=box>hi</div>\
             <p class=box>two</p></body></html>",
        );
        let logs = vm.run_scripts(&["\
            var el = document.getElementById('a');\n\
            console.log(el.textContent);\n\
            console.log(el === document.getElementById('a'));\n\
            el.textContent = 'bye';\n\
            console.log(document.getElementById('a').textContent);\n\
            console.log(document.querySelectorAll('.box').length);\n\
            console.log(el.getAttribute('class'));\n\
            el.setAttribute('data-x', '1');\n\
            console.log(el.getAttribute('data-x'));\n\
            el.removeAttribute('data-x');\n\
            console.log(el.getAttribute('data-x'));\n\
            el.removeAttribute('class');\n\
            console.log(document.querySelectorAll('.box').length);\n\
            console.log(document.getElementById('nope'));\n\
        "
        .to_string()]);
        assert_eq!(
            logs,
            vec!["hi", "true", "bye", "2", "box", "1", "null", "1",
                 "null"]
        );
        // the mutation is visible to the shared Rust DOM
        let d = doc.borrow();
        let a = d.get_element_by_id("a").unwrap();
        assert_eq!(d.collect_text(a), "bye");
    }

    #[test]
    fn dom_create_and_query() {
        let (mut vm, doc) = dom_vm("<html><body></body></html>");
        let logs = vm.run_scripts(&["\
            var body = document.body;\n\
            for (var i = 0; i < 3; i++) {\n\
                var d = document.createElement('div');\n\
                d.textContent = 'node ' + i;\n\
                d.setAttribute('class', 'item');\n\
                body.appendChild(d);\n\
            }\n\
            console.log(document.querySelectorAll('.item').length);\n\
            console.log(document.getElementsByTagName('div').length);\n\
            var first = document.querySelector('.item');\n\
            console.log(first.textContent);\n\
            first.innerHTML = '<b>bold</b>';\n\
            console.log(first.innerHTML);\n\
            document.title = 'gg';\n\
            console.log(document.title);\n\
        "
        .to_string()]);
        assert_eq!(
            logs,
            vec!["3", "3", "node 0", "<b>bold</b>", "gg"]
        );
        assert!(doc.borrow().nodes.len() > 5);
    }

    #[test]
    fn dom_click_dispatch() {
        let (mut vm, doc) = dom_vm(
            "<html><body><div id=outer>\
             <button id=btn onclick=\"clicks += 1;\">go</button>\
             </div></body></html>",
        );
        vm.run_scripts(&["\
            var clicks = 0;\n\
            document.getElementById('outer').addEventListener('click',\n\
                function () { clicks += 10; });\n\
            document.getElementById('btn').addEventListener('click',\n\
                function () { clicks += 100; });\n\
        "
        .to_string()]);
        let btn = doc.borrow().get_element_by_id("btn").unwrap();
        let (_, handled, prevented) = vm.dispatch_click(btn);
        assert!(handled);
        assert!(!prevented, "no handler prevented the default action");
        let logs = vm.run_scripts(&["console.log(clicks);".to_string()]);
        // onclick attr (1) + btn listener (100) + outer listener (10)
        assert_eq!(logs, vec!["111"]);
    }

    #[test]
    fn click_default_action_model() {
        // preventDefault() from a listener suppresses navigation
        let (mut vm, doc) = dom_vm(
            "<html><body><a id=x href=\"p\">go</a></body></html>",
        );
        vm.run_scripts(&["\
            document.getElementById('x').addEventListener('click',\n\
                function (e) { e.preventDefault(); });\n"
            .to_string()]);
        let a = doc.borrow().get_element_by_id("x").unwrap();
        let (_, handled, prevented) = vm.dispatch_click(a);
        assert!(handled && prevented);

        // a handler that does NOT prevent leaves navigation alone
        let (mut vm, doc) = dom_vm(
            "<html><body><a id=x href=\"p\">go</a></body></html>",
        );
        vm.run_scripts(&["\
            document.getElementById('x').addEventListener('click',\n\
                function (e) { var n = 1 + 1; });\n"
            .to_string()]);
        let a = doc.borrow().get_element_by_id("x").unwrap();
        let (_, handled, prevented) = vm.dispatch_click(a);
        assert!(handled && !prevented);

        // onclick="... return false" prevents (function-body semantics)
        let (mut vm, doc) = dom_vm(
            "<html><body><a id=x href=\"p\" \
             onclick=\"return false\">go</a></body></html>",
        );
        let a = doc.borrow().get_element_by_id("x").unwrap();
        let (_, handled, prevented) = vm.dispatch_click(a);
        assert!(handled && prevented);
    }
}
