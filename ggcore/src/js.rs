//! JavaScript support: embeds the Boa engine and binds it to our DOM.
//!
//! The active document is exposed through a thread_local while JS runs
//! (everything is single-threaded through PyO3). Element wrappers are
//! plain JS objects holding a node index (`__idx`) plus accessors and
//! methods that read/write the Rust DOM.

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{
        builtins::{JsArray, JsFunction},
        FunctionObjectBuilder, ObjectInitializer,
    },
    property::Attribute,
    Context, JsNativeError, JsObject, JsResult, JsString, JsValue,
    NativeFunction, Source,
};

use crate::dom::Document;
use crate::html;

/// JS-side plumbing for addEventListener (functions must be owned by
/// the JS heap, so the registry lives inside the context itself).
const PRELUDE: &str = r#"
var window = globalThis;
var __wrappers = {};
var __listeners = {};
function __addListener(idx, type, fn) {
    var key = idx + ":" + type;
    if (!__listeners[key]) __listeners[key] = [];
    __listeners[key].push(fn);
}
function __dispatch(idx, type) {
    var key = idx + ":" + type;
    var l = __listeners[key];
    if (!l) return 0;
    window.event.currentTarget = __wrappers[idx] || null;
    for (var i = 0; i < l.length; i++) {
        l[i].call(__wrappers[idx] || null, window.event);
    }
    return l.length;
}
function __mkevent(type, target, bubbles, cancelable, submitter) {
    window.event = {
        type: type,
        target: target,
        currentTarget: target,
        bubbles: bubbles,
        cancelable: cancelable,
        submitter: submitter,
        defaultPrevented: false,
        preventDefault: function () {
            if (this.cancelable) this.defaultPrevented = true;
        },
        stopPropagation: function () {},
        stopImmediatePropagation: function () {},
    };
    return window.event;
}
function __runinline(code, current) {
    // onclick attributes run as a function body: `event` is in scope
    // and `return false` prevents the default action (per browsers).
    var f = new Function("event", code);
    return f.call(current, window.event) === false;
}
"#;

thread_local! {
    static ACTIVE: RefCell<Option<Rc<RefCell<Document>>>> =
        const { RefCell::new(None) };
    static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn log(line: String) {
    LOG.with(|l| l.borrow_mut().push(line));
}

fn with_doc<R>(
    f: impl FnOnce(&mut Document) -> R,
) -> JsResult<R> {
    ACTIVE.with(|a| {
        let guard = a.borrow();
        match guard.as_ref() {
            Some(rc) => Ok(f(&mut rc.borrow_mut())),
            None => Err(JsNativeError::error()
                .with_message("no active document")
                .into()),
        }
    })
}

fn this_idx(this: &JsValue, ctx: &mut Context) -> JsResult<usize> {
    let obj = this.as_object().ok_or_else(|| {
        JsNativeError::typ().with_message("not a DOM element")
    })?;
    let v = obj.get(js_string!("__idx"), ctx)?;
    let n = v.to_number(ctx)?;
    let idx = n as usize;
    // Page JS can fabricate {__idx: ...}: validate before indexing the
    // arena, or an out-of-range value panics Rust and kills the browser.
    let valid =
        n >= 0.0 && n.fract() == 0.0 && with_doc(|d| idx < d.nodes.len())?;
    if !valid {
        return Err(JsNativeError::typ()
            .with_message("not a DOM element")
            .into());
    }
    Ok(idx)
}

fn arg_str(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<String> {
    Ok(args
        .get(i)
        .cloned()
        .unwrap_or(JsValue::undefined())
        .to_string(ctx)?
        .to_std_string_escaped())
}

/// The accessor/method set is identical for every element, so build it
/// once per context and share it as the wrappers' prototype.
fn element_proto(ctx: &mut Context) -> JsObject {
    if let Ok(v) = ctx.global_object().get(js_string!("__elproto"), ctx) {
        if let Some(obj) = v.as_object() {
            return obj.clone();
        }
    }

    fn accessor(
        ctx: &mut Context,
        get: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
        set: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
    ) -> (JsFunction, JsFunction) {
        let g = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_fn_ptr(get),
        )
        .build();
        let s = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_fn_ptr(set),
        )
        .build();
        (g, s)
    }

    let (text_get, text_set) = accessor(ctx, el_text_get, el_text_set);
    let (html_get, html_set) = accessor(ctx, el_html_get, el_html_set);
    let (id_get, _) = accessor(ctx, el_id_get, el_id_get);
    let (class_get, class_set) = accessor(ctx, el_class_get, el_class_set);

    let proto = ObjectInitializer::new(ctx)
        .function(
            NativeFunction::from_fn_ptr(el_add_event_listener),
            js_string!("addEventListener"),
            2,
        )
        .accessor(js_string!("id"), Some(id_get), None, Attribute::all())
        .accessor(
            js_string!("className"),
            Some(class_get),
            Some(class_set),
            Attribute::all(),
        )
        .function(
            NativeFunction::from_fn_ptr(el_get_attribute),
            js_string!("getAttribute"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_set_attribute),
            js_string!("setAttribute"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(el_remove_attribute),
            js_string!("removeAttribute"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_append_child),
            js_string!("appendChild"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_remove),
            js_string!("remove"),
            0,
        )
        .accessor(
            js_string!("textContent"),
            Some(text_get),
            Some(text_set),
            Attribute::all(),
        )
        .accessor(
            js_string!("innerHTML"),
            Some(html_get),
            Some(html_set),
            Attribute::all(),
        )
        .build();

    let _ = ctx.global_object().set(
        js_string!("__elproto"),
        proto.clone(),
        false,
        ctx,
    );
    proto
}

fn wrap_element(idx: usize, ctx: &mut Context) -> JsObject {
    // One JS object per node, cached in the context (arena indices are
    // never reused, so no invalidation). The wrapper itself is just
    // { __idx } ??everything else lives on the shared prototype.
    let cache = ctx
        .global_object()
        .get(js_string!("__wrappers"), ctx)
        .ok()
        .and_then(|v| v.as_object().cloned());
    if let Some(cache) = &cache {
        if let Ok(hit) = cache.get(idx as u32, ctx) {
            if let Some(obj) = hit.as_object() {
                return obj.clone();
            }
        }
    }

    let proto = element_proto(ctx);
    let wrapper = ObjectInitializer::new(ctx)
        .property(js_string!("__idx"), idx as f64, Attribute::all())
        .build();
    wrapper.set_prototype(Some(proto));

    if let Some(cache) = &cache {
        let _ = cache.set(idx as u32, wrapper.clone(), false, ctx);
    }
    wrapper
}

// ---- element natives ----

fn el_text_get(
    this: &JsValue,
    _args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    let text = with_doc(|d| d.collect_text(idx))?;
    Ok(js_string!(text).into())
}

fn el_text_set(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    let text = arg_str(args, 0, ctx)?;
    with_doc(|d| {
        d.nodes[idx].children.clear();
        d.new_text(text, idx);
    })?;
    Ok(JsValue::undefined())
}

fn el_html_get(
    this: &JsValue,
    _args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    let html = with_doc(|d| serialize_children(d, idx))?;
    Ok(js_string!(html).into())
}

fn el_html_set(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    let markup = arg_str(args, 0, ctx)?;
    with_doc(|d| {
        let frag = html::parse(&markup);
        d.nodes[idx].children.clear();
        // fragment parser wraps content in implicit html/body
        if let Some(body) = find_tag(&frag, "body") {
            let kids = frag.nodes[body].children.clone();
            for child in kids {
                d.graft(&frag, child, idx);
            }
        }
    })?;
    Ok(JsValue::undefined())
}

fn el_get_attribute(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    let name = arg_str(args, 0, ctx)?.to_ascii_lowercase();
    let value = with_doc(|d| d.nodes[idx].attr(&name).map(str::to_string))?;
    Ok(match value {
        Some(v) => js_string!(v).into(),
        None => JsValue::null(),
    })
}

fn el_set_attribute(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    let name = arg_str(args, 0, ctx)?.to_ascii_lowercase();
    let value = arg_str(args, 1, ctx)?;
    with_doc(|d| d.set_attr(idx, &name, &value))?;
    Ok(JsValue::undefined())
}

fn el_remove_attribute(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    let name = arg_str(args, 0, ctx)?.to_ascii_lowercase();
    with_doc(|d| d.remove_attr(idx, &name))?;
    Ok(JsValue::undefined())
}

fn el_append_child(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let parent = this_idx(this, ctx)?;
    let child_val = args.get(0).cloned().unwrap_or(JsValue::undefined());
    let child = this_idx(&child_val, ctx)?;
    let ok = with_doc(|d| {
        // HierarchyRequestError: a cycle would hang every tree walk.
        let mut anc = Some(parent);
        while let Some(a) = anc {
            if a == child {
                return false;
            }
            anc = d.nodes[a].parent;
        }
        d.detach(child);
        d.nodes[child].parent = Some(parent);
        d.nodes[parent].children.push(child);
        true
    })?;
    if !ok {
        return Err(JsNativeError::error()
            .with_message("appendChild: new child is an ancestor of parent")
            .into());
    }
    Ok(child_val)
}

fn el_remove(
    this: &JsValue,
    _args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    with_doc(|d| d.detach(idx))?;
    Ok(JsValue::undefined())
}

fn el_id_get(
    this: &JsValue,
    _args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    let value = with_doc(|d| {
        d.nodes[idx].attr("id").unwrap_or("").to_string()
    })?;
    Ok(JsString::from(value).into())
}

fn el_class_get(
    this: &JsValue,
    _args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    let value = with_doc(|d| {
        d.nodes[idx].attr("class").unwrap_or("").to_string()
    })?;
    Ok(JsString::from(value).into())
}

fn el_class_set(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    let value = arg_str(args, 0, ctx)?;
    with_doc(|d| d.set_attr(idx, "class", &value))?;
    Ok(JsValue::undefined())
}

fn el_add_event_listener(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = this_idx(this, ctx)?;
    let etype = arg_str(args, 0, ctx)?;
    let handler = args.get(1).cloned().unwrap_or_default();
    let global = ctx.global_object();
    let add = global.get(js_string!("__addListener"), ctx)?;
    if let Some(func) = add.as_callable() {
        func.call(
            &JsValue::undefined(),
            &[
                JsValue::from(idx as f64),
                JsString::from(etype).into(),
                handler,
            ],
            ctx,
        )?;
    }
    Ok(JsValue::undefined())
}

// ---- document natives ----

pub(crate) fn query(doc: &Document, selector_text: &str, first_only: bool) -> Vec<usize> {
    let selectors = crate::css::parse_selector_list(selector_text);
    if selectors.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut stack = vec![doc.root];
    while let Some(idx) = stack.pop() {
        let node = &doc.nodes[idx];
        if node.is_element()
            && selectors.iter().any(|s| s.matches(doc, idx))
        {
            out.push(idx);
            if first_only {
                return out;
            }
        }
        for &c in node.children.iter().rev() {
            stack.push(c);
        }
    }
    out
}

fn doc_query_selector(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let sel = arg_str(args, 0, ctx)?;
    let found = with_doc(|d| query(d, &sel, true))?;
    Ok(match found.first() {
        Some(&idx) => wrap_element(idx, ctx).into(),
        None => JsValue::null(),
    })
}

fn doc_query_selector_all(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let sel = arg_str(args, 0, ctx)?;
    let found = with_doc(|d| query(d, &sel, false))?;
    let array = JsArray::new(ctx);
    for idx in found {
        let el: JsValue = wrap_element(idx, ctx).into();
        array.push(el, ctx)?;
    }
    Ok(array.into())
}

fn doc_get_elements_by_tag_name(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let tag = arg_str(args, 0, ctx)?.to_ascii_lowercase();
    let found = with_doc(|d| {
        let mut out = Vec::new();
        let mut stack = vec![d.root];
        while let Some(idx) = stack.pop() {
            if d.nodes[idx].tag.as_deref() == Some(tag.as_str()) {
                out.push(idx);
            }
            for &c in d.nodes[idx].children.iter().rev() {
                stack.push(c);
            }
        }
        out
    })?;
    let array = JsArray::new(ctx);
    for idx in found {
        let el: JsValue = wrap_element(idx, ctx).into();
        array.push(el, ctx)?;
    }
    Ok(array.into())
}

fn doc_get_element_by_id(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let id = arg_str(args, 0, ctx)?;
    let found = with_doc(|d| d.get_element_by_id(id.as_str()))?;
    Ok(match found {
        Some(idx) => wrap_element(idx, ctx).into(),
        None => JsValue::null(),
    })
}

fn doc_create_element(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let tag = arg_str(args, 0, ctx)?.to_ascii_lowercase();
    let idx = with_doc(|d| d.new_element(tag, Vec::new(), None))?;
    Ok(wrap_element(idx, ctx).into())
}

fn doc_body_get(
    _this: &JsValue,
    _args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let found = with_doc(|d| find_tag(d, "body"))?;
    Ok(match found {
        Some(idx) => wrap_element(idx, ctx).into(),
        None => JsValue::null(),
    })
}

fn doc_title_get(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let title = with_doc(|d| {
        find_tag(d, "title")
            .map(|t| d.collect_text(t))
            .unwrap_or_default()
    })?;
    Ok(js_string!(title).into())
}

fn doc_title_set(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let text = arg_str(args, 0, ctx)?;
    with_doc(|d| {
        let title = match find_tag(d, "title") {
            Some(t) => t,
            None => match find_tag(d, "head") {
                Some(head) => {
                    d.new_element("title".to_string(), Vec::new(), Some(head))
                }
                None => return,
            },
        };
        d.nodes[title].children.clear();
        d.new_text(text, title);
    })?;
    Ok(JsValue::undefined())
}

fn console_log(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let parts: Vec<String> = args
        .iter()
        .map(|a| {
            a.to_string(ctx)
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_else(|_| "<?>".to_string())
        })
        .collect();
    log(parts.join(" "));
    Ok(JsValue::undefined())
}

fn alert(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let msg = arg_str(args, 0, ctx)?;
    log(format!("[alert] {msg}"));
    Ok(JsValue::undefined())
}

// ---- helpers ----

pub(crate) fn find_tag(doc: &Document, tag: &str) -> Option<usize> {
    doc.nodes
        .iter()
        .position(|n| n.tag.as_deref() == Some(tag))
}

pub(crate) fn serialize_children(doc: &Document, idx: usize) -> String {
    let mut out = String::new();
    for &c in &doc.nodes[idx].children {
        serialize(doc, c, &mut out);
    }
    out
}

fn serialize(doc: &Document, idx: usize, out: &mut String) {
    let node = &doc.nodes[idx];
    match &node.tag {
        None => out.push_str(
            &node
                .text
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;"),
        ),
        Some(tag) => {
            out.push('<');
            out.push_str(tag);
            for (k, v) in &node.attrs {
                out.push(' ');
                out.push_str(k);
                out.push_str("=\"");
                out.push_str(&v.replace('"', "&quot;"));
                out.push('"');
            }
            out.push('>');
            for &c in &node.children {
                serialize(doc, c, out);
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
}

// ---- public API ----

pub fn new_context() -> Context {
    let mut ctx = Context::default();

    let console = ObjectInitializer::new(&mut ctx)
        .function(NativeFunction::from_fn_ptr(console_log), js_string!("log"), 1)
        .function(
            NativeFunction::from_fn_ptr(console_log),
            js_string!("warn"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(console_log),
            js_string!("error"),
            1,
        )
        .build();
    ctx.register_global_property(
        js_string!("console"),
        console,
        Attribute::all(),
    )
    .ok();

    let title_get = FunctionObjectBuilder::new(
        ctx.realm(),
        NativeFunction::from_fn_ptr(doc_title_get),
    )
    .build();
    let title_set = FunctionObjectBuilder::new(
        ctx.realm(),
        NativeFunction::from_fn_ptr(doc_title_set),
    )
    .build();
    let body_get = FunctionObjectBuilder::new(
        ctx.realm(),
        NativeFunction::from_fn_ptr(doc_body_get),
    )
    .build();

    let document = ObjectInitializer::new(&mut ctx)
        .function(
            NativeFunction::from_fn_ptr(doc_get_element_by_id),
            js_string!("getElementById"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_create_element),
            js_string!("createElement"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_query_selector),
            js_string!("querySelector"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_query_selector_all),
            js_string!("querySelectorAll"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_get_elements_by_tag_name),
            js_string!("getElementsByTagName"),
            1,
        )
        .accessor(
            js_string!("title"),
            Some(title_get),
            Some(title_set),
            Attribute::all(),
        )
        .accessor(js_string!("body"), Some(body_get), None, Attribute::all())
        .build();
    ctx.register_global_property(
        js_string!("document"),
        document,
        Attribute::all(),
    )
    .ok();

    ctx.register_global_callable(
        js_string!("alert"),
        1,
        NativeFunction::from_fn_ptr(alert),
    )
    .ok();

    ctx.eval(Source::from_bytes(PRELUDE)).ok();

    ctx
}

/// Bubble a click from a node to the root: run onclick attributes and
/// addEventListener handlers. Returns (console output, any handler ran,
/// default action prevented) — navigation proceeds unless a handler
/// called event.preventDefault() or an onclick returned false.
pub fn dispatch_click(
    ctx: &mut Context,
    doc: Rc<RefCell<Document>>,
    idx: usize,
) -> (Vec<String>, bool, bool) {
    dispatch_event(ctx, doc, idx, "click", true, true, None)
}

pub fn dispatch_event(
    ctx: &mut Context,
    doc: Rc<RefCell<Document>>,
    idx: usize,
    event_type: &str,
    bubbles: bool,
    cancelable: bool,
    submitter: Option<usize>,
) -> (Vec<String>, bool, bool) {
    let event_type = event_type.to_ascii_lowercase();
    let inline_name = format!("on{event_type}");
    let chain: Vec<(usize, Option<String>)> = {
        let d = doc.borrow();
        if idx >= d.nodes.len() {
            return (Vec::new(), false, false);
        }
        let mut chain = Vec::new();
        let mut cur = Some(idx);
        while let Some(i) = cur {
            let node = &d.nodes[i];
            if node.is_element() {
                chain.push((
                    i,
                    node.attr(&inline_name).map(str::to_string),
                ));
            }
            if !bubbles {
                break;
            }
            cur = node.parent;
        }
        chain
    };

    ACTIVE.with(|a| *a.borrow_mut() = Some(doc));
    LOG.with(|l| l.borrow_mut().clear());
    let global = ctx.global_object();
    if let Ok(make_event) = global.get(js_string!("__mkevent"), ctx) {
        if let Some(func) = make_event.as_callable() {
            let target: JsValue = wrap_element(idx, ctx).into();
            let submitter_value = submitter
                .map(|node| wrap_element(node, ctx).into())
                .unwrap_or(JsValue::null());
            let _ = func.call(
                &JsValue::undefined(),
                &[
                    js_string!(event_type.as_str()).into(),
                    target,
                    JsValue::new(bubbles),
                    JsValue::new(cancelable),
                    submitter_value,
                ],
                ctx,
            );
        }
    }
    let mut handled = false;
    let mut prevented = false;
    for (i, inline) in chain {
        if let Some(code) = inline {
            handled = true;
            let global = ctx.global_object();
            match global.get(js_string!("__runinline"), ctx) {
                Ok(f) => {
                    if let Some(func) = f.as_callable() {
                        match func.call(
                            &JsValue::undefined(),
                            &[
                                JsString::from(code.as_str()).into(),
                                wrap_element(i, ctx).into(),
                            ],
                            ctx,
                        ) {
                            Ok(v) => {
                                if cancelable && v.to_boolean() {
                                    prevented = true; // returned false
                                }
                            }
                            Err(e) => log(format!("Uncaught {e}")),
                        }
                    }
                }
                Err(e) => log(format!("Uncaught {e}")),
            }
        }
        let global = ctx.global_object();
        if let Ok(dispatch) = global.get(js_string!("__dispatch"), ctx) {
            if let Some(func) = dispatch.as_callable() {
                let result = func.call(
                    &JsValue::undefined(),
                    &[
                        JsValue::from(i as f64),
                        js_string!(event_type.as_str()).into(),
                    ],
                    ctx,
                );
                match result {
                    Ok(v) => {
                        if v.to_number(ctx).unwrap_or(0.0) > 0.0 {
                            handled = true;
                        }
                    }
                    Err(e) => log(format!("Uncaught {e}")),
                }
            }
        }
    }
    // any handler may have called event.preventDefault()
    if cancelable {
        if let Ok(prevented_v) = ctx.eval(Source::from_bytes(
            b"window.event && window.event.defaultPrevented === true",
        )) {
            if prevented_v.to_boolean() {
                prevented = true;
            }
        }
    }
    ACTIVE.with(|a| *a.borrow_mut() = None);
    (
        LOG.with(|l| std::mem::take(&mut *l.borrow_mut())),
        handled,
        prevented,
    )
}

/// Run scripts against a document; returns console output + errors.
pub fn run(
    ctx: &mut Context,
    doc: Rc<RefCell<Document>>,
    sources: &[String],
) -> Vec<String> {
    ACTIVE.with(|a| *a.borrow_mut() = Some(doc));
    LOG.with(|l| l.borrow_mut().clear());
    for src in sources {
        if let Err(e) = ctx.eval(Source::from_bytes(src.as_bytes())) {
            log(format!("Uncaught {e}"));
        }
    }
    ACTIVE.with(|a| *a.borrow_mut() = None);
    LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
}
