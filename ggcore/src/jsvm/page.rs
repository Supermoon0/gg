//! PageVm: one persistent gg-js VM per page.
//!
//! Scripts, inline event handlers, and listeners all share globals,
//! heap, and the DOM — modules come and go, the VM state stays. This
//! is the engine the browser talks to (Doc.run_scripts routes here
//! through the native document bridge.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::bytecode::Module;
use super::compiler;
use super::parser;
use super::value::Value;
use super::vm::{
    self, call_listener, call_value, call_value_this, exec,
    has_pending_work, host,
    make_native, new_plain_object, pump, pump_microtasks, pump_step,
    raw_get_prop, raw_set_prop,
    reject_fetch, resolve_fetch, resolve_fetch_full, Ids, ModStore, Native,
    PendingFetch, St, DOC_NODE,
};

type HostFetch = (
    u32,
    String,
    String,
    String,
    Vec<(String, String)>,
    String,
    String,
);

fn host_fetch(request: PendingFetch) -> HostFetch {
    (
        request.fetch_id,
        request.url,
        request.method,
        request.body,
        request.headers,
        request.mode,
        request.credentials,
    )
}

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
Promise.allSettled = function (arr) {
  return Promise.all(arr.map(function (p) {
    return Promise.resolve(p).then(
      function (v) { return { status: 'fulfilled', value: v }; },
      function (e) { return { status: 'rejected', reason: e }; });
  }));
};
// Instances dispatch then/catch/finally at the engine level and never
// consult this prototype; it exists because feature detection reads
// Promise.prototype.catch / .finally before trusting the host Promise,
// and because odd code `.call`s these with a promise as `this`.
Promise.prototype.then = function (a, b) {
  return Promise.resolve(this).then(a, b);
};
Promise.prototype.catch = function (f) {
  return Promise.resolve(this).then(undefined, f);
};
Promise.prototype.finally = function (f) {
  return Promise.resolve(this).finally(f);
};
// core-js's inspectSource is Function.prototype.toString uncurried and
// .call()ed on the candidate. Left unset, that name fell through the
// bare prototype object to the Object brand toString and answered
// "[object Function]" -- which can never contain "native code", so
// the host Promise could never be trusted. The extracted string
// method stringifies whatever `this` it is handed.
Function.prototype.toString = (function () {}).toString;

// Error hierarchy in JS itself — real prototype chains (07-12) make
// `new TypeError(m) instanceof Error` just work.
function Error(m, o) {
  // `Error(m)` without `new` is legal and webpack's chunk loader does
  // exactly that (`var u = Error()`), with no receiver at all. It has
  // to build an error rather than write to nothing.
  if (this === undefined || this === null || typeof this !== 'object') {
    return new Error(m, o);
  }
  if (m !== undefined) this.message = '' + m;
  if (o && typeof o === 'object' && 'cause' in o) this.cause = o.cause;
  // Every minified bundle reports `e.stack` and nothing else when it
  // swallows an error; undefined there turns a one-line diagnosis into
  // an afternoon of bisecting someone else's dist file.
  this.stack = (this.name || 'Error')
    + (m !== undefined && m !== '' ? ': ' + m : '')
    + '\n' + __ggStack();
}
Error.captureStackTrace = function (target) {
  if (target) target.stack = '\n' + __ggStack();
};
Error.prototype.name = 'Error';
Error.prototype.message = '';
Error.prototype.toString = function () {
  return this.message ? this.name + ': ' + this.message : this.name;
};
function TypeError(m, o) {
  if (this === undefined || this === null
      || typeof this !== 'object') return new TypeError(m, o);
  Error.call(this, m, o);
}
function RangeError(m, o) {
  if (this === undefined || this === null
      || typeof this !== 'object') return new RangeError(m, o);
  Error.call(this, m, o);
}
function SyntaxError(m, o) {
  if (this === undefined || this === null
      || typeof this !== 'object') return new SyntaxError(m, o);
  Error.call(this, m, o);
}
function ReferenceError(m, o) {
  if (this === undefined || this === null
      || typeof this !== 'object') return new ReferenceError(m, o);
  Error.call(this, m, o);
}
function EvalError(m, o) {
  if (this === undefined || this === null
      || typeof this !== 'object') return new EvalError(m, o);
  Error.call(this, m, o);
}
function URIError(m, o) {
  if (this === undefined || this === null
      || typeof this !== 'object') return new URIError(m, o);
  Error.call(this, m, o);
}
// Assigning a fresh object to .prototype drops the constructor
// back-pointer that comes with it. Almost nothing reads it, so this
// went unnoticed — until it turned out that `assert.throws` compares
// `thrown.constructor !== expected` rather than using instanceof, so
// every error subclass answered `Error` and every such assertion
// failed with "Expected a TypeError but got a Error". Restore it
// non-enumerably, the way the default one was.
(function () {
  var kinds = [
    [TypeError, 'TypeError'], [RangeError, 'RangeError'],
    [SyntaxError, 'SyntaxError'], [ReferenceError, 'ReferenceError'],
    [EvalError, 'EvalError'], [URIError, 'URIError']
  ];
  for (var i = 0; i < kinds.length; i++) {
    var ctor = kinds[i][0];
    ctor.prototype = new Error();
    ctor.prototype.name = kinds[i][1];
    Object.defineProperty(ctor.prototype, 'constructor', {
      value: ctor, writable: true, enumerable: false, configurable: true
    });
  }
})();
// Promise.any rejects with one of these, so it is not optional once
// Promise.any exists.
function AggregateError(errs, m, o) {
  if (this === undefined || this === null
      || typeof this !== 'object') return new AggregateError(errs, m, o);
  Error.call(this, m, o);
  var list = [];
  if (errs !== undefined && errs !== null) {
    var it = errs[Symbol.iterator];
    if (typeof it === 'function') {
      var g = it.call(errs), s;
      while (!(s = g.next()).done) list.push(s.value);
    } else {
      for (var i = 0; i < errs.length; i++) list.push(errs[i]);
    }
  }
  this.errors = list;
}
AggregateError.prototype = new Error();
AggregateError.prototype.name = 'AggregateError';
Object.defineProperty(AggregateError.prototype, 'constructor', {
  value: AggregateError, writable: true, enumerable: false,
  configurable: true
});
// Symbol: a string-based stand-in. Unique enough for property keys and
// the well-known-symbol protocol; `typeof` reports 'string' (known gap).
var __ggSymN = 0;
function Symbol(d) {
  __ggSymN += 1;
  return '@@Sym(' + (d === undefined ? '' : '' + d) + ')' + __ggSymN;
}
Symbol.iterator = '@@iterator';
Symbol.asyncIterator = '@@asyncIterator';
Symbol.toStringTag = '@@toStringTag';
Symbol.toPrimitive = '@@toPrimitive';
Symbol.hasInstance = '@@hasInstance';
Symbol.unscopables = '@@unscopables';
Symbol.for = function (k) { return '@@SymFor:' + k; };
Symbol.keyFor = function (s) {
  return ('' + s).indexOf('@@SymFor:') === 0
    ? ('' + s).slice(9) : undefined;
};
// DOM events (dispatchEvent walks the tree natively; these are the
// value shapes it reads/writes)
function Event(type, opts) {
  this.type = '' + type;
  opts = opts || {};
  this.bubbles = !!opts.bubbles;
  this.cancelable = !!opts.cancelable;
  this.defaultPrevented = false;
  this.target = null;
  this.currentTarget = null;
}
Event.prototype.preventDefault = function () {
  if (this.cancelable) this.defaultPrevented = true;
};
Event.prototype.stopPropagation = function () {
  this.__stopped = true;
};
Event.prototype.stopImmediatePropagation = function () {
  this.__stopped = true;
};
function CustomEvent(type, opts) {
  Event.call(this, type, opts);
  this.detail = (opts || {}).detail;
}
CustomEvent.prototype = new Event('');
function ErrorEvent(type, opts) {
  Event.call(this, type, opts);
  opts = opts || {};
  this.message = '' + (opts.message || '');
  this.filename = '' + (opts.filename || '');
  this.lineno = opts.lineno || 0;
  this.colno = opts.colno || 0;
  this.error = opts.error !== undefined ? opts.error : null;
}
ErrorEvent.prototype = new Event('');
function PromiseRejectionEvent(type, opts) {
  Event.call(this, type, opts);
  opts = opts || {};
  this.promise = opts.promise !== undefined ? opts.promise : null;
  this.reason = opts.reason;
}
PromiseRejectionEvent.prototype = new Event('');
function MessageEvent(type, opts) {
  Event.call(this, type, opts);
  opts = opts || {};
  this.data = opts.data;
  this.origin = '' + (opts.origin || '');
}
MessageEvent.prototype = new Event('');
// --- platform stub layer -------------------------------------------
// Enough surface for feature-detecting bundles to take their happy
// path. Proxy and Reflect are installed natively below: advertising
// them here is now safe because reads/writes/calls/construction all
// route through their actual internal operations.
function MutationObserver(cb) { this._cb = cb; }
// core-js's microtask fallback (used when it can't find an own
// queueMicrotask descriptor on the global) flushes its job queue on a
// characterData mutation of a bare text node. Make that one idiom
// actually fire — a silent stub here leaves every core-js Promise
// reaction queued forever, which reads as "promises never resolve".
MutationObserver.prototype.observe = function (target, opts) {
  if (!(opts && opts.characterData && target
        && typeof target === 'object')) return;
  var self = this;
  try {
    var value = target.data;
    Object.defineProperty(target, 'data', {
      configurable: true,
      get: function () { return value; },
      set: function (v) {
        value = v;
        queueMicrotask(function () {
          if (self._cb) {
            self._cb([{ type: 'characterData', target: target }], self);
          }
        });
      },
    });
  } catch (e) {}
};
MutationObserver.prototype.disconnect = function () { this._cb = null; };
MutationObserver.prototype.takeRecords = function () { return []; };
function IntersectionObserver(cb, opts) { this._cb = cb; }
IntersectionObserver.prototype.observe = function (t) {
  // everything is "visible": lazy content loads eagerly
  var self = this;
  setTimeout(function () {
    self._cb([{ isIntersecting: true, intersectionRatio: 1,
                target: t }], self);
  }, 0);
};
IntersectionObserver.prototype.unobserve = function () {};
IntersectionObserver.prototype.disconnect = function () {};
// Driven by the host: every time real layout rects are pushed in, the
// engine calls __ggFlushResizeObservers and anything whose box moved
// gets its callback. A stub that never fired left SafeFrame ads at
// height 0 forever -- the creative markup is written into the child
// document, and the *only* thing that gives the frame a height is the
// child observing its own body and posting the measurement out.
var __ggResizeObservers = [];
function ResizeObserver(cb) {
  this._cb = cb; this._t = []; this._last = [];
  __ggResizeObservers.push(this);
}
ResizeObserver.prototype.observe = function (el) {
  if (el && this._t.indexOf(el) < 0) { this._t.push(el); this._last.push(null); }
};
ResizeObserver.prototype.unobserve = function (el) {
  var i = this._t.indexOf(el);
  if (i >= 0) { this._t.splice(i, 1); this._last.splice(i, 1); }
};
ResizeObserver.prototype.disconnect = function () {
  this._t = []; this._last = [];
};
function __ggFlushResizeObservers() {
  for (var k = 0; k < __ggResizeObservers.length; k++) {
    var o = __ggResizeObservers[k];
    if (!o._cb || !o._t.length) continue;
    var entries = [];
    for (var i = 0; i < o._t.length; i++) {
      var r;
      try { r = o._t[i].getBoundingClientRect(); } catch (e) { continue; }
      var key = r.width + 'x' + r.height;
      // the spec delivers one notification when observation starts,
      // whatever the size is -- _last === null covers that
      if (o._last[i] === key) continue;
      o._last[i] = key;
      var box = [{ inlineSize: r.width, blockSize: r.height }];
      entries.push({ target: o._t[i], contentRect: r,
                     borderBoxSize: box, contentBoxSize: box,
                     devicePixelContentBoxSize: box });
    }
    if (entries.length) { try { o._cb(entries, o); } catch (e) {} }
  }
}
// Enough WebCrypto for the id-minting libraries (uuid v4 checks for
// getRandomValues and refuses to run without it). Not cryptographic
// -- nothing rendered depends on that, only on the call existing.
var crypto = {
  getRandomValues: function (arr) {
    if (!arr || typeof arr.length !== 'number') {
      throw new TypeError('crypto.getRandomValues: not an array view');
    }
    for (var i = 0; i < arr.length; i++) {
      arr[i] = (Math.random() * 256) | 0;
    }
    return arr;
  },
  randomUUID: function () {
    var h = '0123456789abcdef', out = '';
    for (var i = 0; i < 36; i++) {
      if (i === 8 || i === 13 || i === 18 || i === 23) { out += '-'; }
      else if (i === 14) { out += '4'; }
      else if (i === 19) { out += h[(Math.random() * 4 | 0) + 8]; }
      else { out += h[Math.random() * 16 | 0]; }
    }
    return out;
  },
  subtle: undefined
};
function matchMedia(q) {
  return { matches: false, media: '' + q,
    onchange: null,
    addListener: function () {}, removeListener: function () {},
    addEventListener: function () {},
    removeEventListener: function () {},
    dispatchEvent: function () { return false; } };
}
function getComputedStyle(el) {
  var s = (el && el.style) || {};
  return s;
}
var customElements = {
  define: function () {}, get: function () {},
  whenDefined: function () { return Promise.resolve(); }
};
// gg has no GC, so a weak reference that never clears is not a
// shortcut -- it is the whole truth. The SDK on naver.com keeps its
// picked ad adapter in a WeakRef and reads it back through .deref()
// on every call, so a missing global left `pickedAdapter` undefined.
function WeakRef(target) { this._t = target; }
WeakRef.prototype.deref = function () { return this._t; };
function FinalizationRegistry(cb) { this._cb = cb; }
FinalizationRegistry.prototype.register = function () {};
FinalizationRegistry.prototype.unregister = function () { return false; };
// A real AbortSignal, not a bag with `aborted: false`. Ad SDKs and
// fetch wrappers reference the *global* to build their request
// timeouts (`AbortSignal.timeout(n)`, `AbortSignal.any([...])`), so a
// missing constructor took down every ad slot on naver.com with
// "AbortSignal is not defined" long before any request was made.
function AbortSignal() {
  this.aborted = false;
  this.reason = undefined;
  this.onabort = null;
  this._ls = [];
}
AbortSignal.prototype.addEventListener = function (type, fn) {
  if (type === 'abort' && fn) this._ls.push(fn);
};
AbortSignal.prototype.removeEventListener = function (type, fn) {
  for (var i = 0; i < this._ls.length; i++) {
    if (this._ls[i] === fn) { this._ls.splice(i, 1); return; }
  }
};
AbortSignal.prototype.dispatchEvent = function (ev) {
  this._fire(ev);
  return true;
};
AbortSignal.prototype.throwIfAborted = function () {
  if (this.aborted) throw this.reason;
};
AbortSignal.prototype._fire = function (ev) {
  var ls = this._ls.slice();
  if (typeof this.onabort === 'function') ls.unshift(this.onabort);
  for (var i = 0; i < ls.length; i++) {
    try { ls[i].call(this, ev); } catch (e) {}
  }
};
AbortSignal.prototype._abort = function (reason) {
  if (this.aborted) return;
  this.aborted = true;
  this.reason = reason === undefined
    ? new DOMException('signal is aborted without reason', 'AbortError')
    : reason;
  this._fire({ type: 'abort', target: this, currentTarget: this });
};
AbortSignal.abort = function (reason) {
  var s = new AbortSignal();
  s._abort(reason);
  return s;
};
AbortSignal.timeout = function (ms) {
  var s = new AbortSignal();
  setTimeout(function () {
    s._abort(new DOMException('signal timed out', 'TimeoutError'));
  }, ms);
  return s;
};
AbortSignal.any = function (list) {
  var s = new AbortSignal();
  var arr = list || [];
  for (var i = 0; i < arr.length; i++) {
    var one = arr[i];
    if (!one) continue;
    if (one.aborted) { s._abort(one.reason); return s; }
    (function (o) {
      o.addEventListener('abort', function () { s._abort(o.reason); });
    })(one);
  }
  return s;
};
function AbortController() {
  this.signal = new AbortSignal();
}
AbortController.prototype.abort = function (reason) {
  this.signal._abort(reason);
};
function URLSearchParams(init) {
  this._p = [];
  if (typeof init === 'string') {
    var s = init.charAt(0) === '?' ? init.slice(1) : init;
    if (s) {
      var parts = s.split('&');
      for (var i = 0; i < parts.length; i++) {
        var kv = parts[i].split('=');
        this._p.push([decodeURIComponent(kv[0]),
                      decodeURIComponent(kv[1] || '')]);
      }
    }
  }
}
URLSearchParams.prototype.get = function (k) {
  for (var i = 0; i < this._p.length; i++) {
    if (this._p[i][0] === k) return this._p[i][1];
  }
  return null;
};
URLSearchParams.prototype.has = function (k) {
  return this.get(k) !== null;
};
URLSearchParams.prototype.set = function (k, v) {
  for (var i = 0; i < this._p.length; i++) {
    if (this._p[i][0] === k) { this._p[i][1] = '' + v; return; }
  }
  this._p.push([k, '' + v]);
};
URLSearchParams.prototype.append = function (k, v) {
  this._p.push([k, '' + v]);
};
URLSearchParams.prototype.toString = function () {
  var out = [];
  for (var i = 0; i < this._p.length; i++) {
    out.push(encodeURIComponent(this._p[i][0]) + '=' +
             encodeURIComponent(this._p[i][1]));
  }
  return out.join('&');
};
URLSearchParams.prototype['delete'] = function (k, v) {
  var out = [];
  for (var i = 0; i < this._p.length; i++) {
    var e = this._p[i];
    if (e[0] === k && (v === undefined || '' + e[1] === '' + v)) {
      continue;
    }
    out.push(e);
  }
  this._p = out;
};
URLSearchParams.prototype.forEach = function (cb, thisArg) {
  for (var i = 0; i < this._p.length; i++) {
    cb.call(thisArg, this._p[i][1], this._p[i][0], this);
  }
};
URLSearchParams.prototype.getAll = function (k) {
  var out = [];
  for (var i = 0; i < this._p.length; i++) {
    if (this._p[i][0] === k) out.push(this._p[i][1]);
  }
  return out;
};
URLSearchParams.prototype.keys = function () {
  var out = [];
  for (var i = 0; i < this._p.length; i++) out.push(this._p[i][0]);
  return out;
};
URLSearchParams.prototype.values = function () {
  var out = [];
  for (var i = 0; i < this._p.length; i++) out.push(this._p[i][1]);
  return out;
};
URLSearchParams.prototype.entries = function () {
  var out = [];
  for (var i = 0; i < this._p.length; i++) {
    out.push([this._p[i][0], this._p[i][1]]);
  }
  return out;
};
URLSearchParams.prototype.sort = function () {
  this._p.sort(function (a, b) {
    return a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0;
  });
};
function URL(u, base) {
  u = '' + u;
  if (base && u.indexOf('://') < 0) {
    base = '' + base;
    var origin = base.split('/').slice(0, 3).join('/');
    u = u.charAt(0) === '/' ? origin + u
      : base.replace(/[^\/]*$/, '') + u;
  }
  this.href = u;
  var m = u.split('://');
  this.protocol = (m.length > 1 ? m[0] : 'https') + ':';
  var rest = m.length > 1 ? m[1] : u;
  var slash = rest.indexOf('/');
  this.host = slash < 0 ? rest : rest.slice(0, slash);
  this.hostname = this.host.split(':')[0];
  this.port = this.host.indexOf(':') > 0
    ? this.host.split(':')[1] : '';
  var pathq = slash < 0 ? '/' : rest.slice(slash);
  var hi = pathq.indexOf('#');
  this.hash = hi < 0 ? '' : pathq.slice(hi);
  if (hi >= 0) pathq = pathq.slice(0, hi);
  var qi = pathq.indexOf('?');
  this.search = qi < 0 ? '' : pathq.slice(qi);
  this.pathname = qi < 0 ? pathq : pathq.slice(0, qi);
  this.origin = this.protocol + '//' + this.host;
  this.searchParams = new URLSearchParams(this.search);
}
URL.prototype.toString = function () { return this.href; };
// Byte buffers. Bytes live in a plain array shared with the backing
// ArrayBuffer, and the view carries its own index properties -- a real
// engine aliases them, so a write *through a view* is not seen by
// another view of the same buffer unless it goes through `set()`,
// which does write through. Byte-stream code (React's flight parser,
// TextDecoder) builds with `set` and reads through views, which this
// serves exactly; it is not a substitute for shared mutable memory.
function ArrayBuffer(len) {
  len = len > 0 ? Math.floor(len) : 0;
  this.byteLength = len;
  this._b = [];
  for (var i = 0; i < len; i++) this._b.push(0);
}
ArrayBuffer.prototype.slice = function (a, b) {
  var out = new ArrayBuffer(0);
  out._b = this._b.slice(a, b === undefined ? this.byteLength : b);
  out.byteLength = out._b.length;
  return out;
};
ArrayBuffer.isView = function (v) { return !!(v && v.__typed); };
function __ggBytes(src) {
  var out = [], i;
  if (src === undefined || src === null) return out;
  if (typeof src === 'number') {
    for (i = 0; i < Math.floor(src); i++) out.push(0);
    return out;
  }
  if (typeof src.length === 'number') {
    for (i = 0; i < src.length; i++) out.push(src[i] & 255);
    return out;
  }
  return out;
}
function Uint8Array(src, off, len) {
  var bytes;
  if (src instanceof ArrayBuffer) {
    off = off === undefined ? 0 : Math.floor(off);
    len = len === undefined ? src.byteLength - off : Math.floor(len);
    if (len < 0) len = 0;
    bytes = src._b.slice(off, off + len);
    this.buffer = src;
    this.byteOffset = off;
  } else {
    bytes = __ggBytes(src);
    var buf = new ArrayBuffer(0);
    buf._b = bytes;
    buf.byteLength = bytes.length;
    this.buffer = buf;
    this.byteOffset = 0;
  }
  for (var i = 0; i < bytes.length; i++) this[i] = bytes[i] & 255;
  this.length = bytes.length;
  this.byteLength = bytes.length;
  this.BYTES_PER_ELEMENT = 1;
  this.__typed = true;
}
Uint8Array.prototype.set = function (src, offset) {
  offset = offset === undefined ? 0 : Math.floor(offset);
  var b = __ggBytes(src);
  for (var i = 0; i < b.length; i++) {
    this[offset + i] = b[i];
    this.buffer._b[this.byteOffset + offset + i] = b[i];
  }
};
Uint8Array.prototype.subarray = function (a, b) {
  a = a === undefined ? 0 : (a < 0 ? this.length + a : a);
  b = b === undefined ? this.length : (b < 0 ? this.length + b : b);
  var out = [];
  for (var i = a; i < b && i < this.length; i++) out.push(this[i]);
  var view = new Uint8Array(out);
  view.buffer = this.buffer;
  view.byteOffset = this.byteOffset + a;
  return view;
};
Uint8Array.prototype.slice = function (a, b) {
  return new Uint8Array(this.subarray(a, b));
};
Uint8Array.prototype.fill = function (v, a, b) {
  a = a === undefined ? 0 : a;
  b = b === undefined ? this.length : b;
  for (var i = a; i < b; i++) this[i] = v & 255;
  return this;
};
Uint8Array.prototype.indexOf = function (v, from) {
  // the fromIndex is not optional in practice: React's flight parser
  // scans for the next row terminator with `chunk.indexOf(10, at)`,
  // and ignoring it re-finds the first one forever
  var i = from === undefined ? 0 : Math.floor(from);
  if (i < 0) i = Math.max(this.length + i, 0);
  for (; i < this.length; i++) if (this[i] === v) return i;
  return -1;
};
Uint8Array.prototype.lastIndexOf = function (v, from) {
  var i = from === undefined ? this.length - 1 : Math.floor(from);
  if (i < 0) i = this.length + i;
  if (i > this.length - 1) i = this.length - 1;
  for (; i >= 0; i--) if (this[i] === v) return i;
  return -1;
};
Uint8Array.prototype.includes = function (v, from) {
  return this.indexOf(v, from) !== -1;
};
Uint8Array.prototype.join = function (sep) {
  var out = [];
  for (var i = 0; i < this.length; i++) out.push(this[i]);
  return out.join(sep === undefined ? ',' : sep);
};
Uint8Array.prototype.forEach = function (fn, thisArg) {
  for (var i = 0; i < this.length; i++) fn.call(thisArg, this[i], i, this);
};
Uint8Array.prototype.toString = function () { return this.join(','); };
Uint8Array.prototype[Symbol.iterator] = function () {
  var i = 0, self = this;
  return { next: function () {
    return i < self.length
      ? { value: self[i++], done: false }
      : { value: undefined, done: true };
  } };
};
Uint8Array.from = function (src) { return new Uint8Array(src); };
Uint8Array.of = function () { return new Uint8Array(arguments); };
Uint8Array.BYTES_PER_ELEMENT = 1;
var Uint8ClampedArray = Uint8Array;
var Int8Array = Uint8Array;
function TextEncoder() { this.encoding = 'utf-8'; }
TextEncoder.prototype.encode = function (s) {
  s = s === undefined ? '' : '' + s;
  var out = [];
  for (var i = 0; i < s.length; i++) {
    var c = s.charCodeAt(i);
    if (c >= 0xd800 && c <= 0xdbff && i + 1 < s.length) {
      var lo = s.charCodeAt(i + 1);
      if (lo >= 0xdc00 && lo <= 0xdfff) {
        c = 0x10000 + ((c - 0xd800) << 10) + (lo - 0xdc00);
        i++;
      }
    }
    if (c < 128) { out.push(c); }
    else if (c < 2048) {
      out.push(192 | (c >> 6), 128 | (c & 63));
    } else if (c < 65536) {
      out.push(224 | (c >> 12), 128 | ((c >> 6) & 63), 128 | (c & 63));
    } else {
      out.push(240 | (c >> 18), 128 | ((c >> 12) & 63),
               128 | ((c >> 6) & 63), 128 | (c & 63));
    }
  }
  return new Uint8Array(out);
};
// A real decoder, not a stub returning ''. `{stream: true}` holds a
// truncated multi-byte sequence back for the next chunk, which is the
// whole point when the bytes arrive from a stream.
function TextDecoder(label) {
  this.encoding = label ? ('' + label).toLowerCase() : 'utf-8';
  this._tail = [];
}
TextDecoder.prototype.decode = function (input, opts) {
  var b = this._tail.concat(__ggBytes(input));
  this._tail = [];
  var stream = !!(opts && opts.stream);
  var out = '', i = 0;
  while (i < b.length) {
    var c = b[i], need = 0, cp = 0;
    if (c < 128) { out += String.fromCharCode(c); i++; continue; }
    else if ((c & 224) === 192) { need = 1; cp = c & 31; }
    else if ((c & 240) === 224) { need = 2; cp = c & 15; }
    else if ((c & 248) === 240) { need = 3; cp = c & 7; }
    else { out += '�'; i++; continue; }
    if (i + need >= b.length + (stream ? 0 : 1) && i + need >= b.length) {
      if (stream) { this._tail = b.slice(i); return out; }
      out += '�';
      i++;
      continue;
    }
    for (var k = 1; k <= need; k++) cp = (cp << 6) | (b[i + k] & 63);
    i += need + 1;
    if (cp >= 65536) {
      cp -= 65536;
      out += String.fromCharCode(0xd800 + (cp >> 10),
                                 0xdc00 + (cp & 1023));
    } else {
      out += String.fromCharCode(cp);
    }
  }
  return out;
};
// A ReadableStream the way byte-stream consumers use one: a source
// with `start(controller)` that enqueues, and a reader whose `read()`
// resolves as chunks arrive. React's RSC client hands its flight data
// in exactly this shape.
function ReadableStream(source, strategy) {
  var self = this;
  this.locked = false;
  this._chunks = [];
  this._done = false;
  this._err = null;
  this._waiting = [];
  this._source = source || {};
  var controller = {
    enqueue: function (chunk) {
      if (self._done) return;
      self._chunks.push(chunk);
      self._wake();
    },
    close: function () { self._done = true; self._wake(); },
    error: function (e) { self._err = e; self._done = true; self._wake(); },
    get desiredSize() { return 1; }
  };
  this._controller = controller;
  if (typeof this._source.start === 'function') {
    try { this._source.start(controller); }
    catch (e) { controller.error(e); }
  }
}
ReadableStream.prototype._wake = function () {
  var waiting = this._waiting;
  this._waiting = [];
  for (var i = 0; i < waiting.length; i++) waiting[i]();
};
ReadableStream.prototype._pull = function () {
  var self = this;
  return new Promise(function (resolve, reject) {
    function step() {
      if (self._chunks.length) {
        resolve({ value: self._chunks.shift(), done: false });
        return;
      }
      if (self._err) { reject(self._err); return; }
      if (self._done) { resolve({ value: undefined, done: true }); return; }
      if (typeof self._source.pull === 'function') {
        try { self._source.pull(self._controller); }
        catch (e) { reject(e); return; }
        if (self._chunks.length || self._done) { step(); return; }
      }
      self._waiting.push(step);
    }
    step();
  });
};
ReadableStream.prototype.getReader = function () {
  var self = this;
  this.locked = true;
  return {
    read: function () { return self._pull(); },
    cancel: function (reason) {
      self._done = true;
      self._chunks = [];
      self._wake();
      if (typeof self._source.cancel === 'function') {
        try { self._source.cancel(reason); } catch (e) {}
      }
      return Promise.resolve();
    },
    releaseLock: function () { self.locked = false; },
    closed: new Promise(function () {})
  };
};
ReadableStream.prototype.cancel = function (reason) {
  return this.getReader().cancel(reason);
};
ReadableStream.prototype[Symbol.iterator] = function () {
  var self = this;
  return { next: function () { return self._pull(); } };
};
function Worker() {}
Worker.prototype.postMessage = function () {};
Worker.prototype.terminate = function () {};
Worker.prototype.addEventListener = function () {};
// Headers-like facade over a plain lower-cased {name: value} map. Fetch
// Responses carry the raw map as `_h` (set by the engine); XHR reuses it.
function __ggMakeHeaders(map) {
  map = map || {};
  return {
    get: function (n) {
      var v = map[('' + n).toLowerCase()];
      return v === undefined ? null : v;
    },
    has: function (n) { return map[('' + n).toLowerCase()] !== undefined; },
    forEach: function (cb, thisArg) {
      for (var k in map) cb.call(thisArg, map[k], k, this);
    },
    entries: function () {
      var out = [];
      for (var k in map) out.push([k, map[k]]);
      return out[Symbol.iterator] ? out[Symbol.iterator]() : out;
    },
    keys: function () {
      var out = [];
      for (var k in map) out.push(k);
      return out[Symbol.iterator] ? out[Symbol.iterator]() : out;
    },
  };
}
function XMLHttpRequest() {
  this.readyState = 0;
  this.status = 0;
  this.statusText = '';
  this.responseText = '';
  this.response = '';
  this.responseURL = '';
  this.responseType = '';
  this.timeout = 0;
  this._headers = {};
  this._resH = {};
  this._lis = {};
  this.withCredentials = false;
  // handler slots must EXIST so `'onloadend' in xhr` feature checks
  // (axios and friends) take the modern path
  this.onreadystatechange = null;
  this.onload = null;
  this.onloadend = null;
  this.onloadstart = null;
  this.onerror = null;
  this.onabort = null;
  this.ontimeout = null;
  this.onprogress = null;
}
XMLHttpRequest.UNSENT = 0; XMLHttpRequest.OPENED = 1;
XMLHttpRequest.HEADERS_RECEIVED = 2; XMLHttpRequest.LOADING = 3;
XMLHttpRequest.DONE = 4;
XMLHttpRequest.prototype.UNSENT = 0;
XMLHttpRequest.prototype.OPENED = 1;
XMLHttpRequest.prototype.HEADERS_RECEIVED = 2;
XMLHttpRequest.prototype.LOADING = 3;
XMLHttpRequest.prototype.DONE = 4;
XMLHttpRequest.prototype.open = function (method, url) {
  this._method = method;
  this._url = url;
  this.readyState = 1;
  this._fire('readystatechange');
};
XMLHttpRequest.prototype.setRequestHeader = function (k, v) {
  this._headers[k] = v;
};
XMLHttpRequest.prototype.getResponseHeader = function (n) {
  var v = this._resH[('' + n).toLowerCase()];
  return v === undefined ? null : v;
};
XMLHttpRequest.prototype.getAllResponseHeaders = function () {
  var out = '';
  for (var k in this._resH) out += k + ': ' + this._resH[k] + '\r\n';
  return out;
};
XMLHttpRequest.prototype.abort = function () {
  this._aborted = true;
  this.readyState = 4;
  this.status = 0;
  this._fire('readystatechange');
  this._fire('abort');
  this._fire('loadend');
};
XMLHttpRequest.prototype.overrideMimeType = function () {};
XMLHttpRequest.prototype.addEventListener = function (ty, cb) {
  if (typeof cb !== 'function') return;
  if (!this._lis[ty]) this._lis[ty] = [];
  this._lis[ty].push(cb);
};
XMLHttpRequest.prototype.removeEventListener = function (ty, cb) {
  var l = this._lis[ty];
  if (!l) return;
  var i = l.indexOf(cb);
  if (i >= 0) l.splice(i, 1);
};
XMLHttpRequest.prototype._fire = function (ty, extra) {
  var ev = { type: ty, target: this, currentTarget: this };
  if (extra) for (var k in extra) ev[k] = extra[k];
  var h = this['on' + ty];
  if (typeof h === 'function') {
    try { h.call(this, ev); } catch (e) { console.log('[gg-js error] xhr on' + ty + ': ' + e); }
  }
  var l = this._lis[ty];
  if (l) for (var i = 0; i < l.length; i++) {
    try { l[i].call(this, ev); } catch (e2) { console.log('[gg-js error] xhr ' + ty + ' listener: ' + e2); }
  }
};
XMLHttpRequest.prototype._finish = function () {
  var t = this.responseText;
  if (this.responseType === 'json') {
    try { this.response = JSON.parse(t); }
    catch (e) { this.response = null; }
  } else {
    this.response = t;
  }
  this.readyState = 4;
  this._fire('readystatechange');
  this._fire('load');
  this._fire('loadend');
};
XMLHttpRequest.prototype.send = function (body) {
  var self = this;
  this._fire('loadstart');
  fetch(this._url, {
    method: this._method || 'GET',
    headers: this._headers,
    body: body,
    mode: 'cors',
    credentials: this.withCredentials ? 'include' : 'same-origin'
  }).then(function (r) {
    if (self._aborted) return '';
    self.status = r.status;
    self.statusText = r.ok ? 'OK' : '';
    self.responseURL = r.url || self._url;
    self._resH = r._h || {};
    return r.text();
  }).then(function (t) {
    if (self._aborted) return;
    self.responseText = t;
    self._finish();
  }, function (e) {
    if (self._aborted) return;
    self.readyState = 4;
    self.status = 0;
    self._fire('readystatechange');
    self._fire('error', { error: e });
    self._fire('loadend');
  });
};
// Dress the engine's bare fetch Response ({ok,status,url,text,json,_h})
// with the standard surface page code expects: headers facade,
// statusText, clone(), arrayBuffer()/blob() shims.
var __ggNativeFetch = fetch;
fetch = function (input, init) {
  return __ggNativeFetch(input, init).then(function (r) {
    if (r && typeof r === 'object' && !r.headers) {
      r.headers = __ggMakeHeaders(r._h);
      if (r.statusText === undefined) r.statusText = r.ok ? 'OK' : '';
      if (!r.clone) r.clone = function () { return r; };
      if (!r.arrayBuffer) r.arrayBuffer = function () {
        return r.text().then(function (t) {
          return new TextEncoder().encode(t);
        });
      };
      if (!r.blob) r.blob = function () { return r.text(); };
    }
    return r;
  });
};
window.fetch = fetch;
// Globals live outside the window object in this engine; property READS
// fall through the window alias, but getOwnPropertyDescriptor(window, x)
// does not. core-js's microtask module resolves queueMicrotask through
// exactly that descriptor probe — when it comes back empty it falls back
// to MutationObserver-based flushing. Expose the scheduling natives as
// real own properties so polyfills take the native path.
window.queueMicrotask = queueMicrotask;
window.setTimeout = setTimeout;
window.clearTimeout = clearTimeout;
window.Promise = Promise;
// DOM interface constructors: patch surfaces for polyfills
// (real DOM nodes are engine values, not instances of these)
function EventTarget() {}
EventTarget.prototype.addEventListener = function (t, cb) {
  if (typeof cb !== 'function') return;
  if (!this.__lis) this.__lis = {};
  if (!this.__lis[t]) this.__lis[t] = [];
  this.__lis[t].push(cb);
};
EventTarget.prototype.removeEventListener = function (t, cb) {
  var a = this.__lis && this.__lis[t];
  if (!a) return;
  for (var i = 0; i < a.length; i++) {
    if (a[i] === cb) { a.splice(i, 1); break; }
  }
};
EventTarget.prototype.dispatchEvent = function (e) {
  var a = this.__lis && e && this.__lis[e.type];
  if (a) {
    if (e.target == null) e.target = this;
    e.currentTarget = this;
    var copy = a.slice();
    for (var i = 0; i < copy.length; i++) copy[i].call(this, e);
  }
  return !(e && e.defaultPrevented);
};
function Node() {}
function Element() {}
function HTMLElement() {}
function HTMLDivElement() {}
function HTMLAnchorElement() {}
function HTMLScriptElement() {}
function HTMLImageElement() {}
function HTMLInputElement() {}
// `new Image(w, h)` builds a real <img> DOM node, so feed/thumbnail
// components (naver's news cards, many SPAs) that preload or insert an
// image during render don't hit `Image is not defined` — a ReferenceError
// there makes React discard the whole component subtree mid-render.
function Image(w, h) {
  var el = document.createElement('img');
  if (w != null) el.width = w;
  if (h != null) el.height = h;
  return el;
}
// structuredClone: a deep copy of plain data (arrays, objects, dates).
// Data selectors/stores use it to snapshot state before mutating.
function structuredClone(v) {
  if (v === null || typeof v !== 'object') return v;
  if (Array.isArray(v)) {
    var a = [];
    for (var i = 0; i < v.length; i++) a[i] = structuredClone(v[i]);
    return a;
  }
  if (v instanceof Date) return new Date(v.getTime());
  var o = {};
  for (var k in v) {
    if (Object.prototype.hasOwnProperty.call(v, k)) {
      o[k] = structuredClone(v[k]);
    }
  }
  return o;
}
function SVGElement() {}
function Document() {}
function HTMLDocument() {}
function CharacterData() {}
function Text() {}
function Comment() {}
function DocumentFragment() {}
function HTMLCollection() {}
function NodeList() {}
function DOMTokenList() {}
function ShadowRoot() {}
function DocumentType() {}
function ProcessingInstruction() {}
function CDATASection() {}
function Attr() {}
function DOMException(m, n) {
  this.message = '' + (m || '');
  this.name = '' + (n || 'Error');
}
// A working MessageChannel: port.postMessage delivers to the OTHER
// port's onmessage on a macrotask (setTimeout 0). react-dom 18's
// scheduler drives its entire render work loop through this, so a
// no-op stub silently prevents React from ever committing.
function MessageChannel() {
  function mkport() {
    return { onmessage: null, _peer: null,
      addEventListener: function (t, f) {
        if (t === 'message') this.onmessage = f;
      },
      removeEventListener: function () {},
      postMessage: function (d) {
        var peer = this._peer;
        setTimeout(function () {
          if (peer.onmessage) peer.onmessage({ data: d });
        }, 0);
      },
      start: function () {}, close: function () {} };
  }
  var p1 = mkport(), p2 = mkport();
  p1._peer = p2; p2._peer = p1;
  this.port1 = p1; this.port2 = p2;
}
function Blob() {}
function File() {}
function FormData() {}
FormData.prototype.append = function () {};
FormData.prototype.get = function () { return null; };
// --- Date: a real class over the virtual clock (UTC == local, tz 0).
// Calendar math is Howard Hinnant's civil algorithm.
function __gg_civil(z) {
  z += 719468;
  var era = Math.floor(z / 146097);
  var doe = z - era * 146097;
  var yoe = Math.floor((doe - Math.floor(doe / 1460)
    + Math.floor(doe / 36524) - Math.floor(doe / 146096)) / 365);
  var y = yoe + era * 400;
  var doy = doe - (365 * yoe + Math.floor(yoe / 4)
    - Math.floor(yoe / 100));
  var mp = Math.floor((5 * doy + 2) / 153);
  var d = doy - Math.floor((153 * mp + 2) / 5) + 1;
  var m = mp + (mp < 10 ? 3 : -9);
  return [y + (m <= 2 ? 1 : 0), m, d];
}
function __gg_days(y, m, d) {
  y -= m <= 2 ? 1 : 0;
  var era = Math.floor(y / 400);
  var yoe = y - era * 400;
  var mp = m + (m > 2 ? -3 : 9);
  var doy = Math.floor((153 * mp + 2) / 5) + d - 1;
  var doe = yoe * 365 + Math.floor(yoe / 4)
    - Math.floor(yoe / 100) + doy;
  return era * 146097 + doe - 719468;
}
function Date(a, b, c, d, e, f, g) {
  var t;
  if (arguments.length === 0) { t = __ggDateNow(); }
  else if (arguments.length === 1) {
    if (typeof a === 'number') { t = a; }
    else if (a instanceof Date) { t = a._t; }
    else { t = Date.parse('' + a); }
  } else {
    t = Date.UTC(a, b, c === undefined ? 1 : c,
                 d || 0, e || 0, f || 0, g || 0);
  }
  // The timestamp is this engine's storage, not a property of a Date.
  // As a plain assignment it was enumerable, so `_t` turned up in
  // Object.keys(new Date()), in for-in, in Object.assign, and in
  // JSON.stringify of anything holding a date.
  Object.defineProperty(this, '_t', {
    value: t, writable: true, enumerable: false, configurable: true
  });
}
Date.now = __ggDateNow;
Date.UTC = function (y, m, d, h, mi, s, ms) {
  return __gg_days(+y, (+m || 0) + 1, d === undefined ? 1 : +d)
    * 86400000 + (h || 0) * 3600000 + (mi || 0) * 60000
    + (s || 0) * 1000 + (ms || 0);
};
// ISO first, then the two formats that actually turn up in markup and
// in hand-written dates. Only ISO was accepted, so `new Date("1/1/2000")`
// — the commonest way a date is written in the wild — was NaN.
var __ggMonNames = ['jan', 'feb', 'mar', 'apr', 'may', 'jun',
                    'jul', 'aug', 'sep', 'oct', 'nov', 'dec'];
var __ggISO =
  /^(\d{4})-(\d{2})(?:-(\d{2}))?(?:[ ](\d{2}):(\d{2})(?::(\d{2})(?:\.(\d{1,3}))?)?)?/;
// M/D/YYYY, the US ordering, optionally with a time
var __ggSlash =
  /^(\d{1,2})\/(\d{1,2})\/(\d{1,4})(?:[ ](\d{1,2}):(\d{2})(?::(\d{2}))?)?/;
// A written month can sit in any of the three positions, and the day
// and year in the other two: "Jan 1 2000", "1 Jan 2000", "1 2000 Jan",
// "Jan 2000 1", "2000 Jan 1", "2000 1 Jan" are all the same date.
// Rather than write six patterns, pull the month name out and read
// whatever numbers are left.
var __ggMonRe = /(^|[^A-Za-z])(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*/i;
var __ggTimeRe = /(\d{1,2}):(\d{2})(?::(\d{2}))?/;
function __ggParseText(s) {
  var mm = __ggMonRe.exec(s);
  if (!mm) return null;
  var mon = __ggMonNames.indexOf(mm[2].toLowerCase());
  var rest = s.slice(0, mm.index) + ' '
    + s.slice(mm.index + mm[0].length);
  var h = 0, mi = 0, sec = 0;
  var tm = __ggTimeRe.exec(rest);
  if (tm) {
    h = +tm[1]; mi = +tm[2]; sec = +(tm[3] || 0);
    rest = rest.slice(0, tm.index) + ' '
      + rest.slice(tm.index + tm[0].length);
  }
  var nums = rest.match(/\d+/g);
  if (!nums || nums.length !== 2) return null;
  var a = +nums[0], b = +nums[1];
  // A number above 31 can only be the year. If both are, there is no
  // date to be had. If neither is, the day is written first — that is
  // the ordering every "1 Jan 99" style string uses.
  var day, yr;
  if (a > 31 && b > 31) return null;
  else if (a > 31) { yr = a; day = b; }
  else { day = a; yr = b; }
  return [yr, mon, day, h, mi, sec];
}
Date.parse = function (s) {
  // Strip a leading weekday by name, not by shape: "Jan 2, 2000" also
  // starts with three letters and a space.
  s = ('' + s).replace('T', ' ')
    .replace(/^(?:sun|mon|tue|wed|thu|fri|sat)[a-z]*,?[ ]/i, '');
  s = s.replace(/[ ]*(?:GMT|UTC|Z)$/, '');
  var m = __ggISO.exec(s);
  if (m) {
    return Date.UTC(+m[1], +m[2] - 1, +(m[3] || 1), +(m[4] || 0),
                    +(m[5] || 0), +(m[6] || 0), +(m[7] || 0));
  }
  var mon, day, yr, h, mi, sec;
  if ((m = __ggSlash.exec(s))) {
    // M/D/Y, unless the first field cannot be a month. Above 31 it can
    // only be a year, so "99/1/2" is 1999-01-02. Between 13 and 31 it
    // could be either a year or a day and there is no way to tell, so
    // the whole string is rejected rather than guessed at.
    if (+m[1] > 31) {
      yr = +m[1]; mon = +m[2] - 1; day = +m[3];
    } else if (+m[1] > 12) {
      return NaN;
    } else {
      mon = +m[1] - 1; day = +m[2]; yr = +m[3];
    }
    h = m[4]; mi = m[5]; sec = m[6];
  } else if ((m = __ggParseText(s))) {
    yr = m[0]; mon = m[1]; day = m[2];
    h = m[3]; mi = m[4]; sec = m[5];
  } else {
    return NaN;
  }
  if (mon < 0 || mon > 11) return NaN;
  // A two-digit year is 19xx from 50 up and 20xx below it, the same
  // split every other engine uses.
  if (yr < 50) yr += 2000; else if (yr < 100) yr += 1900;
  // The day has to exist. Date.UTC rolls an out-of-range day into the
  // following month, so without this "99/1/99" came back as
  // 1999-04-09 — a confidently wrong date, which is worse than NaN.
  var leap = (yr % 4 === 0 && yr % 100 !== 0) || yr % 400 === 0;
  var mlen = [31, leap ? 29 : 28, 31, 30, 31, 30,
              31, 31, 30, 31, 30, 31];
  if (day < 1 || day > mlen[mon]) return NaN;
  if (+(h || 0) > 24 || +(mi || 0) > 59 || +(sec || 0) > 59) return NaN;
  return Date.UTC(yr, mon, day, +(h || 0), +(mi || 0), +(sec || 0), 0);
};
(function () {
  var P = Date.prototype;
  function day(t) { return Math.floor(t / 86400000); }
  function mod(t, n) { var r = t % n; return r < 0 ? r + n : r; }
  P.getTime = function () { return this._t; };
  P.valueOf = function () { return this._t; };
  P.setTime = function (t) { this._t = +t; return this._t; };
  P.getFullYear = function () { return __gg_civil(day(this._t))[0]; };
  P.getMonth = function () { return __gg_civil(day(this._t))[1] - 1; };
  P.getDate = function () { return __gg_civil(day(this._t))[2]; };
  P.getDay = function () { return mod(day(this._t) + 4, 7); };
  P.getHours = function () {
    return Math.floor(mod(this._t, 86400000) / 3600000);
  };
  P.getMinutes = function () {
    return Math.floor(mod(this._t, 3600000) / 60000);
  };
  P.getSeconds = function () {
    return Math.floor(mod(this._t, 60000) / 1000);
  };
  P.getMilliseconds = function () { return mod(this._t, 1000); };
  P.getYear = function () { return this.getFullYear() - 1900; };
  P.getTimezoneOffset = function () { return 0; };
  P.getUTCFullYear = P.getFullYear;
  P.getUTCMonth = P.getMonth;
  P.getUTCDate = P.getDate;
  P.getUTCDay = P.getDay;
  P.getUTCHours = P.getHours;
  P.getUTCMinutes = P.getMinutes;
  P.getUTCSeconds = P.getSeconds;
  P.getUTCMilliseconds = P.getMilliseconds;
  // setters — keep the untouched half (date or time) and recompute
  // _t from civil days. UTC == local (tz 0). date-fns builds every
  // date through setUTCFullYear/Month/Date/Hours.
  P.setFullYear = function (y, m, d) {
    var c = __gg_civil(day(this._t));
    if (m === undefined) m = c[1] - 1;
    if (d === undefined) d = c[2];
    this._t = __gg_days(+y, (+m) + 1, +d) * 86400000
      + mod(this._t, 86400000);
    return this._t;
  };
  P.setMonth = function (m, d) {
    var c = __gg_civil(day(this._t));
    if (d === undefined) d = c[2];
    this._t = __gg_days(c[0], (+m) + 1, +d) * 86400000
      + mod(this._t, 86400000);
    return this._t;
  };
  P.setDate = function (d) {
    var c = __gg_civil(day(this._t));
    this._t = __gg_days(c[0], c[1], +d) * 86400000
      + mod(this._t, 86400000);
    return this._t;
  };
  P.setHours = function (h, mi, s, ms) {
    var H = h !== undefined ? +h : this.getHours();
    var M = mi !== undefined ? +mi : this.getMinutes();
    var S = s !== undefined ? +s : this.getSeconds();
    var MS = ms !== undefined ? +ms : this.getMilliseconds();
    this._t = day(this._t) * 86400000
      + H * 3600000 + M * 60000 + S * 1000 + MS;
    return this._t;
  };
  P.setMinutes = function (mi, s, ms) {
    return P.setHours.call(this, this.getHours(), mi, s, ms);
  };
  P.setSeconds = function (s, ms) {
    return P.setHours.call(this, this.getHours(),
      this.getMinutes(), s, ms);
  };
  P.setMilliseconds = function (ms) {
    return P.setHours.call(this, this.getHours(),
      this.getMinutes(), this.getSeconds(), ms);
  };
  P.setYear = function (y) {
    return P.setFullYear.call(this, y < 100 ? 1900 + (+y) : +y);
  };
  P.setUTCFullYear = P.setFullYear;
  P.setUTCMonth = P.setMonth;
  P.setUTCDate = P.setDate;
  P.setUTCHours = P.setHours;
  P.setUTCMinutes = P.setMinutes;
  P.setUTCSeconds = P.setSeconds;
  P.setUTCMilliseconds = P.setMilliseconds;
  function pad(n, w) {
    n = '' + n;
    while (n.length < (w || 2)) n = '0' + n;
    return n;
  }
  P.toISOString = function () {
    var c = __gg_civil(day(this._t));
    return pad(c[0], 4) + '-' + pad(c[1]) + '-' + pad(c[2])
      + 'T' + pad(this.getHours()) + ':' + pad(this.getMinutes())
      + ':' + pad(this.getSeconds()) + '.'
      + pad(this.getMilliseconds(), 3) + 'Z';
  };
  P.toJSON = function () { return this.toISOString(); };
  P.toString = function () { return this.toISOString(); };
  P.toLocaleDateString = P.toString;
  P.toLocaleTimeString = P.toString;
  P.toLocaleString = P.toString;
  P.toUTCString = P.toString;
  P.toGMTString = P.toString;
})();

// Legacy escape/unescape (still used by older bundles; distinct from
// encodeURIComponent — %XX / %uXXXX, no UTF-8 transform).
function unescape(s) {
  s = '' + s;
  var out = '', i = 0;
  while (i < s.length) {
    var c = s.charAt(i);
    if (c === '%' && s.charAt(i + 1) === 'u') {
      out += String.fromCharCode(parseInt(s.substr(i + 2, 4), 16));
      i += 6;
    } else if (c === '%') {
      out += String.fromCharCode(parseInt(s.substr(i + 1, 2), 16));
      i += 3;
    } else { out += c; i += 1; }
  }
  return out;
}
function escape(s) {
  s = '' + s;
  var ok = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz' +
           '0123456789@*_+-./';
  var out = '';
  for (var i = 0; i < s.length; i++) {
    var ch = s.charAt(i), code = s.charCodeAt(i);
    if (ok.indexOf(ch) >= 0) { out += ch; }
    else if (code < 256) {
      out += '%' + ('0' + code.toString(16).toUpperCase()).slice(-2);
    } else {
      out += '%u' + ('000' + code.toString(16).toUpperCase()).slice(-4);
    }
  }
  return out;
}
String.raw = function (strings) {
  var raw = (strings && strings.raw) || strings || [];
  var out = '';
  for (var i = 0; i < raw.length; i++) {
    out += raw[i];
    if (i + 1 < arguments.length) out += arguments[i + 1];
  }
  return out;
};
Object.getOwnPropertyDescriptors = function (o) {
  var out = {};
  var names = Object.getOwnPropertyNames(o);
  for (var i = 0; i < names.length; i++) {
    out[names[i]] = Object.getOwnPropertyDescriptor(o, names[i]);
  }
  return out;
};
var __b64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
function btoa(input) {
  input = '' + input;
  var out = '', bits = 0, n = 0;
  for (var i = 0; i < input.length; i++) {
    bits = (bits << 8) | (input.charCodeAt(i) & 0xFF);
    n += 8;
    while (n >= 6) { n -= 6; out += __b64.charAt((bits >> n) & 63); }
  }
  if (n > 0) out += __b64.charAt((bits << (6 - n)) & 63);
  while (out.length % 4) out += '=';
  return out;
}
function atob(input) {
  input = ('' + input).replace(/=+$/, '');
  var out = '', bits = 0, n = 0;
  for (var i = 0; i < input.length; i++) {
    var idx = __b64.indexOf(input.charAt(i));
    if (idx < 0) continue;
    bits = (bits << 6) | idx;
    n += 6;
    if (n >= 8) { n -= 8; out += String.fromCharCode((bits >> n) & 0xFF); }
  }
  return out;
}
"#;
use crate::dom;

pub struct PageVm {
    st: St,
    mods: ModStore,
}

impl PageVm {
    pub fn new(doc: Option<Rc<RefCell<dom::Document>>>) -> PageVm {
        let has_doc = doc.is_some();
        let mut vm = PageVm {
            st: St::new(doc),
            mods: ModStore::new(),
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
        // the prelude builds a real Date class over this native tick
        let dn = make_native(&mut vm.st, Native::DateNow);
        vm.set_global("__ggDateNow", dn);
        let stk = make_native(&mut vm.st, Native::StackTrace);
        vm.set_global("__ggStack", stk);
        vm.set_global("NaN", Value::number(f64::NAN));
        vm.set_global("Infinity", Value::number(f64::INFINITY));
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
        // String statics ride the fn_props table like Object.keys does
        // (the ctor itself stays a callable coercion function)
        for (m, id) in [
            ("fromCharCode", host::S_FROMCHARCODE),
            ("fromCodePoint", host::S_FROMCHARCODE),
        ] {
            let key = vm.name_id(m);
            let mv = make_native(&mut vm.st, Native::HostFn(id));
            let sidx = vm.st.known.string.index();
            vm.st.fn_props.insert((sidx, key), mv);
        }
        // Number statics ride fn_props on the Number ctor (same as
        // String.fromCharCode): method values plus numeric constants.
        for (m, n) in [
            ("parseInt", Native::ParseInt),
            ("parseFloat", Native::ParseFloat),
            ("isNaN", Native::HostFn(host::N_ISNAN)),
            ("isFinite", Native::HostFn(host::N_ISFINITE)),
            ("isInteger", Native::HostFn(host::N_ISINTEGER)),
            ("isSafeInteger", Native::HostFn(host::N_ISSAFEINT)),
        ] {
            let key = vm.name_id(m);
            let mv = make_native(&mut vm.st, n);
            let nidx = vm.st.known.number.index();
            vm.st.fn_props.insert((nidx, key), mv);
        }
        for (m, val) in [
            ("MAX_SAFE_INTEGER", 9_007_199_254_740_991.0_f64),
            ("MIN_SAFE_INTEGER", -9_007_199_254_740_991.0),
            ("MAX_VALUE", f64::MAX),
            ("MIN_VALUE", f64::MIN_POSITIVE),
            ("EPSILON", f64::EPSILON),
            ("POSITIVE_INFINITY", f64::INFINITY),
            ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
            ("NaN", f64::NAN),
        ] {
            let key = vm.name_id(m);
            let nidx = vm.st.known.number.index();
            vm.st.fn_props.insert((nidx, key), Value::number(val));
        }
        // async runtime (P3): timers, microtasks, fetch, Promise
        for (name, n) in [
            ("setTimeout", Native::SetTimeout),
            ("setInterval", Native::SetInterval),
            ("clearTimeout", Native::ClearTimeout),
            ("clearInterval", Native::ClearTimeout),
            ("queueMicrotask", Native::QueueMicrotask),
            ("fetch", Native::Fetch),
            ("requestAnimationFrame", Native::Raf),
            ("cancelAnimationFrame", Native::ClearTimeout),
            ("requestIdleCallback", Native::Raf),
            ("cancelIdleCallback", Native::ClearTimeout),
            ("encodeURIComponent",
             Native::UriCoder { encode: true, component: true }),
            ("decodeURIComponent",
             Native::UriCoder { encode: false, component: true }),
            ("encodeURI",
             Native::UriCoder { encode: true, component: false }),
            ("decodeURI",
             Native::UriCoder { encode: false, component: false }),
        ] {
            let fv = make_native(&mut vm.st, n);
            vm.set_global(name, fv);
        }
        // Promise is a real constructor FUNCTION. It was a plain
        // object of statics for a long time, and that one wrong
        // `typeof` failed core-js's trustworthiness probe on every
        // page that ships it: the polyfill replaced the global, every
        // async function's machinery then ran through the polyfill's
        // scheduler (which never fires here), and whole apps stopped
        // silently at their first await. naver's shopping module was
        // the visible casualty.
        let promise_ctor =
            make_native(&mut vm.st, Native::PromiseCtor);
        vm.set_global("Promise", promise_ctor);
        for (m, n) in [
            ("resolve", Native::PromiseResolve),
            ("reject", Native::PromiseReject),
        ] {
            let key = vm.name_id(m);
            let fv = make_native(&mut vm.st, n);
            vm.st.fn_props.insert((promise_ctor.index(), key), fv);
        }
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
            ("clz32", Native::HostFn(host::M_CLZ32)),
        ]);
        vm.set_builtin_prop(math, "PI", Value::number(std::f64::consts::PI));
        vm.set_builtin_prop(math, "E", Value::number(std::f64::consts::E));
        vm.set_builtin_prop(math, "LN2", Value::number(std::f64::consts::LN_2));
        vm.set_builtin_prop(math, "SQRT2",
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
                ("seal", Native::HostFn(host::O_SEAL)),
                ("isFrozen", Native::HostFn(host::O_IS_FROZEN)),
                ("isSealed", Native::HostFn(host::O_IS_SEALED)),
                ("preventExtensions", Native::HostFn(host::O_PREVENT_EXT)),
                ("isExtensible", Native::HostFn(host::O_IS_EXTENSIBLE)),
                ("defineProperty", Native::HostFn(host::O_DEFINE_PROP)),
                ("defineProperties",
                 Native::HostFn(host::O_DEFINE_PROPS)),
                ("getOwnPropertyDescriptor",
                 Native::HostFn(host::O_GET_OWN_PD)),
                ("getOwnPropertyDescriptors",
                 Native::HostFn(host::O_GET_OWN_PDS)),
                ("getOwnPropertyNames",
                 Native::HostFn(host::O_GET_OWN_NAMES)),
                ("create", Native::HostFn(host::O_CREATE)),
                ("getPrototypeOf", Native::HostFn(host::O_GET_PROTO)),
                ("setPrototypeOf", Native::HostFn(host::O_SET_PROTO)),
                ("is", Native::HostFn(host::O_IS)),
                ("fromEntries", Native::HostFn(host::O_FROM_ENTRIES)),
                ("hasOwn", Native::HostFn(host::O_HAS_OWN)),
            ],
        );
        vm.st.known.object = object_ctor;
        let array_ctor = vm.install_callable(
            "Array",
            Native::ArrayCtor,
            &[
                ("isArray", Native::HostFn(host::A_ISARRAY)),
                ("from", Native::HostFn(host::A_FROM)),
                ("of", Native::HostFn(host::A_OF)),
            ],
        );
        vm.st.known.array = array_ctor;
        vm.install_callable(
            "Proxy",
            Native::ProxyCtor,
            &[("revocable", Native::ProxyRevocable)],
        );
        vm.install_object("Reflect", &[
            ("apply", Native::Reflect(vm::reflect::APPLY)),
            ("construct", Native::Reflect(vm::reflect::CONSTRUCT)),
            ("defineProperty",
             Native::Reflect(vm::reflect::DEFINE_PROPERTY)),
            ("deleteProperty",
             Native::Reflect(vm::reflect::DELETE_PROPERTY)),
            ("get", Native::Reflect(vm::reflect::GET)),
            ("getOwnPropertyDescriptor",
             Native::Reflect(vm::reflect::GET_OWN_PROPERTY_DESCRIPTOR)),
            ("getPrototypeOf",
             Native::Reflect(vm::reflect::GET_PROTOTYPE_OF)),
            ("has", Native::Reflect(vm::reflect::HAS)),
            ("isExtensible",
             Native::Reflect(vm::reflect::IS_EXTENSIBLE)),
            ("ownKeys", Native::Reflect(vm::reflect::OWN_KEYS)),
            ("preventExtensions",
             Native::Reflect(vm::reflect::PREVENT_EXTENSIONS)),
            ("set", Native::Reflect(vm::reflect::SET)),
            ("setPrototypeOf",
             Native::Reflect(vm::reflect::SET_PROTOTYPE_OF)),
        ]);
        // Object.prototype staples as extractable values (webpack's
        // runtime does Object.prototype.hasOwnProperty.call(...))
        let oproto = vm::fn_prototype(&mut vm.st, object_ctor);
        for m in ["hasOwnProperty", "valueOf",
                  "propertyIsEnumerable", "isPrototypeOf"] {
            let k = vm.name_id(m);
            let f = make_native(&mut vm.st, Native::MethodRef(k));
            raw_set_prop(&mut vm.st, oproto.index() as usize, k, f);
        }
        // the genuine Object.prototype.toString brands its receiver
        // (core-js classof: `{}.toString.call([]) == '[object Array]'`)
        let k = vm.name_id("toString");
        let f = make_native(&mut vm.st, Native::BrandToString);
        raw_set_prop(&mut vm.st, oproto.index() as usize, k, f);
        // Array.prototype.values/keys/entries as extractables
        // (iterator-helper polyfills read them off the prototype)
        let aproto = vm::fn_prototype(&mut vm.st, array_ctor);
        for m in ["values", "keys", "entries", "slice", "concat",
                  "join", "indexOf", "push", "pop", "forEach", "map",
                  "filter", "@@iterator", "sort", "splice", "shift",
                  "unshift", "reverse", "some", "every", "reduce",
                  "lastIndexOf", "toString"] {
            let k = vm.name_id(m);
            let f = make_native(&mut vm.st, Native::MethodRef(k));
            raw_set_prop(&mut vm.st, aproto.index() as usize, k, f);
        }
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
        // typed-array/binary globals are deliberately ABSENT: half-real
        // stubs made core-js take its native path and crash inside our
        // empty shells (naver polyfill, module 34697). With them gone,
        // core-js builds its own pure-JS ArrayBuffer/DataView/typed
        // arrays on WeakMap-backed internal state — which we support.
        // window listeners are real (lifecycle events find them by the
        // WINDOW_NODE key); the rest stay accepted-and-ignored
        for (m, add) in [("addEventListener", true),
                         ("removeEventListener", false)] {
            let f = make_native(&mut vm.st, Native::WinEvent { add });
            vm.set_object_prop(window, m, f);
        }
        let disp = make_native(&mut vm.st, Native::WinDispatch);
        vm.set_object_prop(window, "dispatchEvent", disp);
        let post = make_native(
            &mut vm.st, Native::PostMessage { ctx: vm::FRAME_SELF });
        vm.set_object_prop(window, "postMessage", post);
        // window.scrollTo/scroll are absolute, scrollBy is relative;
        // all three reach the host through the shared scroll queue
        for (m, relative) in
            [("scrollTo", false), ("scroll", false), ("scrollBy", true)]
        {
            let f = make_native(&mut vm.st, Native::WinScroll { relative });
            vm.set_object_prop(window, m, f);
        }
        vm.st.known.window = window;
        // Top-level defaults. Pages framebust on `window.top !==
        // window`, so these must be the real window until the host
        // says otherwise (see `set_framed`).
        for f in ["parent", "top", "self"] {
            vm.set_object_prop(window, f, window);
        }
        vm.set_object_prop(window, "frameElement", Value::NULL);
        // `window.name` survives navigation and is the classic channel
        // for handing parameters into a frame -- SafeFrame ad creatives
        // read their whole init blob out of it.
        let empty = vm::intern(&mut vm.st, "");
        vm.set_object_prop(window, "name", empty);
        let fctor = make_native(&mut vm.st, Native::FunctionCtor);
        vm.set_global("Function", fctor);
        vm.st.known.function = fctor;
        // Function.prototype.call/apply/bind as extractable values
        // (core-js uncurryThis reads them off the prototype object)
        let fproto = vm::fn_prototype(&mut vm.st, fctor);
        for m in ["call", "apply", "bind"] {
            let k = vm.name_id(m);
            let f = make_native(&mut vm.st, Native::MethodRef(k));
            raw_set_prop(&mut vm.st, fproto.index() as usize, k, f);
        }
        let rctor = make_native(&mut vm.st, Native::RegExpCtor);
        vm.set_global("RegExp", rctor);
        // RegExp.prototype.exec/test as extractable methods (core-js
        // regexp-exec reads then re-applies them via .call)
        let rproto = vm::fn_prototype(&mut vm.st, rctor);
        for m in ["exec", "test"] {
            let k = vm.name_id(m);
            let f = make_native(&mut vm.st, Native::MethodRef(k));
            raw_set_prop(&mut vm.st, rproto.index() as usize, k, f);
        }
        // Map/Set (Weak variants share the impl — no GC either way).
        // Their prototypes carry extractable methods (core-js pulls
        // Set.prototype.forEach/keys off and re-applies them).
        for (name, ctor) in [("Map", Native::MapCtor),
                             ("WeakMap", Native::MapCtor),
                             ("Set", Native::SetCtor),
                             ("WeakSet", Native::SetCtor)] {
            let f = make_native(&mut vm.st, ctor);
            vm.set_global(name, f);
            let proto = vm::fn_prototype(&mut vm.st, f);
            for m in ["get", "set", "add", "has", "delete", "clear",
                      "forEach", "keys", "values", "entries",
                      "@@iterator"] {
                let k = vm.name_id(m);
                let mf = make_native(&mut vm.st, Native::MethodRef(k));
                raw_set_prop(
                    &mut vm.st, proto.index() as usize, k, mf);
            }
        }
        // browser objects: location / navigator / performance /
        // history / screen (location gets real values via
        // set_page_url once the loader knows the URL)
        {
            let loc = new_plain_object(&mut vm.st);
            let li = loc.index() as usize;
            for (f, dv) in [("href", ""), ("protocol", "https:"),
                            ("host", ""), ("hostname", ""),
                            ("pathname", "/"), ("search", ""),
                            ("hash", ""), ("origin", ""),
                            ("port", "")] {
                let k = vm.name_id(f);
                let sv = vm::intern(&mut vm.st, dv);
                raw_set_prop(&mut vm.st, li, k, sv);
            }
            for m in ["assign", "replace", "reload"] {
                let k = vm.name_id(m);
                let f = make_native(&mut vm.st, Native::Noop);
                raw_set_prop(&mut vm.st, li, k, f);
            }
            vm.set_global("location", loc);
            vm.set_object_prop(window, "location", loc);

            let nav = new_plain_object(&mut vm.st);
            let ni = nav.index() as usize;
            for (f, dv) in [
                // The Chrome token is what core-js's V8_VERSION
                // sniff reads; >= 51 skips the Promise subclassing
                // probe our engine cannot pass, which otherwise
                // forces the polyfill in (see to_display on
                // functions). GGBrowser stays as the real identity.
                ("userAgent",
                 "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                  AppleWebKit/537.36 (KHTML, like Gecko) \
                  Chrome/122.0.0.0 Safari/537.36 GGBrowser/0.1"),
                ("platform", "Win32"),
                ("language", "ko-KR"),
                ("vendor", ""),
                ("appName", "Netscape"),
            ] {
                let k = vm.name_id(f);
                let sv = vm::intern(&mut vm.st, dv);
                raw_set_prop(&mut vm.st, ni, k, sv);
            }
            let k = vm.name_id("cookieEnabled");
            raw_set_prop(&mut vm.st, ni, k, Value::boolean(true));
            let k = vm.name_id("sendBeacon");
            let f = make_native(&mut vm.st, Native::Noop);
            raw_set_prop(&mut vm.st, ni, k, f);
            vm.set_global("navigator", nav);
            vm.set_object_prop(window, "navigator", nav);

            let perf = new_plain_object(&mut vm.st);
            let pi = perf.index() as usize;
            let k = vm.name_id("now");
            let f = make_native(&mut vm.st, Native::PerfNow);
            raw_set_prop(&mut vm.st, pi, k, f);
            for m in ["mark", "measure", "clearMarks",
                      "clearMeasures", "getEntriesByName"] {
                let k = vm.name_id(m);
                let f = make_native(&mut vm.st, Native::Noop);
                raw_set_prop(&mut vm.st, pi, k, f);
            }
            let k = vm.name_id("timeOrigin");
            raw_set_prop(&mut vm.st, pi, k, Value::number(0.0));
            vm.set_global("performance", perf);
            vm.set_object_prop(window, "performance", perf);

            let hist = new_plain_object(&mut vm.st);
            let hi = hist.index() as usize;
            let k = vm.name_id("length");
            raw_set_prop(&mut vm.st, hi, k, Value::int(1));
            let k = vm.name_id("state");
            raw_set_prop(&mut vm.st, hi, k, Value::NULL);
            for m in ["pushState", "replaceState", "back",
                      "forward", "go"] {
                let k = vm.name_id(m);
                let f = make_native(&mut vm.st, Native::Noop);
                raw_set_prop(&mut vm.st, hi, k, f);
            }
            vm.set_global("history", hist);
            vm.set_object_prop(window, "history", hist);

            let scr = new_plain_object(&mut vm.st);
            let si = scr.index() as usize;
            for (f, n) in [("width", 1280), ("height", 800),
                           ("availWidth", 1280),
                           ("availHeight", 760),
                           ("colorDepth", 24), ("pixelDepth", 24)] {
                let k = vm.name_id(f);
                raw_set_prop(&mut vm.st, si, k, Value::int(n));
            }
            vm.set_global("screen", scr);
            vm.set_object_prop(window, "screen", scr);

            // viewport metrics: some observers/lazy loaders read these
            // instead of documentElement.clientWidth/clientHeight.
            for (f, n) in [
                ("innerWidth", vm::VIEWPORT_W),
                ("innerHeight", vm::VIEWPORT_H),
                ("outerWidth", vm::VIEWPORT_W),
                ("outerHeight", vm::VIEWPORT_H),
                ("scrollX", 0), ("scrollY", 0),
                ("pageXOffset", 0), ("pageYOffset", 0),
                ("devicePixelRatio", 1),
            ] {
                vm.set_object_prop(window, f, Value::int(n));
            }
        }
        // localStorage / sessionStorage (in-memory key-value)
        for (name, session) in [("localStorage", false),
                                ("sessionStorage", true)] {
            let store = new_plain_object(&mut vm.st);
            for (m, op) in [("getItem", 0u8), ("setItem", 1),
                            ("removeItem", 2), ("clear", 3), ("key", 4)] {
                let key = vm.name_id(m);
                let fv = make_native(
                    &mut vm.st, Native::Storage { session, op });
                raw_set_prop(&mut vm.st, store.index() as usize, key, fv);
            }
            vm.set_global(name, store);
            vm.set_object_prop(window, name, store);
        }
        // core-js reads Function.prototype.apply/call/bind as values
        // (uncurryThis); seed them as extraction dispatchers
        let fproto = vm::fn_prototype(&mut vm.st, fctor);
        for m in ["apply", "call", "bind"] {
            let key = vm.name_id(m);
            let mv = make_native(&mut vm.st, Native::MethodRef(key));
            raw_set_prop(&mut vm.st, fproto.index() as usize, key, mv);
        }
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
        let (logs, requests) = self.pump_requests();
        let fetches = requests
            .into_iter()
            .map(|request| (request.0, request.1))
            .collect();
        (logs, fetches)
    }

    pub fn pump_requests(&mut self) -> (Vec<String>, Vec<HostFetch>) {
        let fetches = pump(&mut self.st, &self.mods, PUMP_BUDGET)
            .into_iter()
            .map(host_fetch)
            .collect();
        (std::mem::take(&mut self.st.logs), fetches)
    }

    pub fn pump_microtasks_requests(
        &mut self,
    ) -> (Vec<String>, Vec<HostFetch>) {
        let fetches = pump_microtasks(&mut self.st, &self.mods, PUMP_BUDGET)
            .into_iter()
            .map(host_fetch)
            .collect();
        (std::mem::take(&mut self.st.logs), fetches)
    }

    /// Return and reset the opt-in GG_JS_PROFILE instruction samples.
    /// The final number is an estimated bytecode-instruction count.
    pub fn take_profile(&mut self) -> Vec<(String, u32, u32, u64)> {
        let mut rows: Vec<_> = self.st.take_profile_samples()
            .into_iter()
            .map(|((module, proto), samples)| {
                let loaded = self.mods.rc(module);
                let name = loaded.module.protos
                    .get(proto as usize)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "<unknown>".to_string());
                (name, module, proto, samples.saturating_mul(16_384))
            })
            .collect();
        rows.sort_by(|a, b| b.3.cmp(&a.3));
        rows
    }

    pub fn global_number(&mut self, name: &str) -> Option<f64> {
        let id = self.st.intern_name(name) as usize;
        let value = self.st.globals[id];
        value.is_number().then(|| value.to_number_raw())
    }

    /// Current virtual-clock time (ms). The host reads this to fix a
    /// settle horizon before stepping.
    pub fn now_ms(&self) -> f64 {
        self.st.now_ms
    }

    /// Fire ONE scheduler slice (see vm::pump_step). Returns
    /// (console output, fetches to service, more-work-remains). The host
    /// refreshes layout rects (set_layout_rects) between steps so
    /// geometry-reading effects see real rects.
    pub fn step(&mut self, horizon_ms: f64) -> (Vec<String>, Vec<(u32, String)>, bool) {
        let (fetches, more) =
            pump_step(&mut self.st, &self.mods, PUMP_BUDGET, horizon_ms);
        (std::mem::take(&mut self.st.logs), fetches, more)
    }

    pub fn resolve_fetch(&mut self, fetch_id: u32, status: u16, body: String) {
        resolve_fetch(&mut self.st, fetch_id, status, body);
    }

    pub fn resolve_fetch_full(
        &mut self,
        fetch_id: u32,
        status: u16,
        url: String,
        body: String,
        headers: Vec<(String, String)>,
    ) {
        resolve_fetch_full(&mut self.st, fetch_id, status, url, body, headers);
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
            self.st.non_enum.insert((obj.index(), key));
        }
        self.set_global(name, obj);
        obj
    }

    fn set_object_prop(&mut self, obj: Value, prop: &str, val: Value) {
        let key = self.name_id(prop);
        raw_set_prop(&mut self.st, obj.index() as usize, key, val);
    }

    /// Like set_object_prop, but for a builtin's own constants, which
    /// are not enumerable. `Object.keys(Math)` is specified to be
    /// empty; it used to list all 27 of Math's members, and
    /// `Object.create({}, Math)` — legal precisely *because* they are
    /// not enumerable — tried to read Math.PI as a descriptor.
    fn set_builtin_prop(&mut self, obj: Value, prop: &str, val: Value) {
        let key = self.name_id(prop);
        raw_set_prop(&mut self.st, obj.index() as usize, key, val);
        self.st.non_enum.insert((obj.index(), key));
    }

    /// Compile a script and bind it into the shared namespace.
    fn load(&mut self, module: Module) -> u32 {
        vm::load_module(&mut self.st, &self.mods, module)
    }

    /// Parse + compile + run one script; returns its last expression
    /// statement value.
    /// Shell push after layout: real getBoundingClientRect geometry.
    pub fn set_layout_rects(
        &mut self,
        rects: Vec<(u32, f64, f64, f64, f64)>,
    ) {
        self.st.layout_rects.clear();
        for (idx, x, y, w, h) in rects {
            self.st.layout_rects.insert(idx, (x, y, w, h));
        }
        // Real layout landing is the only moment a size can be said to
        // have changed, so this is where ResizeObserver fires. The JS
        // side early-outs when nothing is observed.
        let id = self.st.intern_name("__ggFlushResizeObservers");
        if let Some(&f) = self.st.globals.get(id as usize) {
            if f.is_function() {
                let _ = call_value(&mut self.st, &self.mods, f, &[]);
            }
        }
    }

    /// Tell the document it is embedded, so `window.parent` and
    /// `window.top` stop being itself.
    ///
    /// This has to be callable *before* the page's own scripts run:
    /// the `if (window.top !== window)` framed-check and the
    /// `window.parent.postMessage(...)` handshake are both parser-time
    /// inline scripts in the widgets that use them.
    /// Seed `window.name`. The embedder sets it from the frame
    /// element's `name` before the child's scripts run, which is the
    /// only moment it is observable to them.
    pub fn set_window_name(&mut self, name: &str) {
        let window = self.st.known.window;
        if !window.is_object() {
            return;
        }
        let v = vm::intern(&mut self.st, name);
        self.set_object_prop(window, "name", v);
    }

    pub fn set_framed(&mut self, framed: bool) {
        let window = self.st.known.window;
        if !window.is_object() {
            return;
        }
        let (parent, top) = if framed {
            (
                vm::window_proxy(&mut self.st, vm::FRAME_PARENT),
                vm::window_proxy(&mut self.st, vm::FRAME_TOP),
            )
        } else {
            (window, window)
        };
        self.set_object_prop(window, "parent", parent);
        self.set_object_prop(window, "top", top);
    }

    /// Publish which `<iframe>` elements map to which browsing
    /// context: [(iframe node index, context handle, same_origin)].
    ///
    /// The host decides this — the VM has no way to discover a context
    /// it was not handed, which is what makes the frame graph
    /// unguessable from page JS.
    pub fn set_frame_graph(&mut self, frames: Vec<(u32, u32, bool)>) {
        // rebuilt wholesale: a removed <iframe> must stop resolving
        self.st.frame_ctx.clear();
        for (node, handle, same_origin) in frames {
            vm::window_proxy(&mut self.st, handle);
            if !same_origin {
                // a frame that navigated cross-origin must lose the
                // DOM the parent could read a moment ago
                self.st.frame_mirrors.remove(&handle);
                self.st.frame_docs.remove(&node);
            }
            self.st.frame_ctx.insert(node, (handle, same_origin));
        }
    }

    /// Publish a same-origin child's DOM into this document, so
    /// `iframe.contentDocument` can read it.
    ///
    /// `rows` are the child's exported arena as
    /// (parent, index, tag, text, attrs) — the host strips the style
    /// pairs `export()` also carries. The mirror is a *rebuild*, not a
    /// handle: the engine's own selector engine then runs against a
    /// `dom::Document` that lives in this VM. Empty rows drop the
    /// mirror, which is how a frame that went away reads null again.
    pub fn set_frame_document(
        &mut self,
        node: u32,
        handle: u32,
        url: String,
        rows: Vec<(i64, u64, Option<String>, Option<String>,
                   Vec<(String, String)>)>,
    ) {
        // Either way the host has taken responsibility for this
        // frame's document, so the script-created-iframe fallback
        // stops applying to it.
        self.st.frame_host_owned.insert(node);
        if rows.is_empty() {
            self.st.frame_mirrors.remove(&handle);
            self.st.frame_docs.remove(&node);
            return;
        }
        // `with_capacity` starts with an empty arena, so the first
        // row has to *create* the root rather than be mapped onto a
        // node that does not exist yet.
        let mut doc = dom::Document::with_capacity(rows.len());
        let mut idx_of: HashMap<u32, usize> = HashMap::new();
        let mut ridx_of: Vec<u32> = Vec::with_capacity(rows.len());
        // `export` names a row's parent by its position in the dump, not
        // by the child's arena index. The two coincide only while the
        // arena happens to be in document order — which a spec parse
        // does not produce, and which any script that moves a node
        // destroys. Keep the row mapping separate from the arena one.
        let mut by_row: Vec<usize> = Vec::with_capacity(rows.len());
        for (parent, ridx, tag, text, attrs) in rows {
            let ridx = ridx as u32;
            let p = if parent < 0 {
                None
            } else {
                by_row
                    .get(parent as usize)
                    .copied()
                    .filter(|&i| i != usize::MAX)
            };
            // a row whose parent was pruned is pruned with it
            let placed = if parent >= 0 && p.is_none() {
                None
            } else {
                match (tag, p) {
                    (Some(t), _) => Some(doc.new_element(t, attrs, p)),
                    // a text node with no parent cannot exist
                    (None, Some(p)) => {
                        Some(doc.new_text(text.unwrap_or_default(), p))
                    }
                    (None, None) => None,
                }
            };
            if let Some(new) = placed {
                if idx_of.is_empty() {
                    doc.root = new;
                }
                idx_of.insert(ridx, new);
                while ridx_of.len() <= new {
                    ridx_of.push(u32::MAX);
                }
                ridx_of[new] = ridx;
            }
            by_row.push(placed.unwrap_or(usize::MAX));
        }
        // a rebuilt DOM invalidates every wrapper handed out before:
        // the nodes they named may not exist any more
        self.st.frame_mirrors.insert(handle, vm::FrameMirror {
            doc,
            idx_of,
            ridx_of,
            wrappers: HashMap::new(),
            url: url.clone(),
        });
        let d = self.build_frame_document(handle, url);
        self.st.frame_docs.insert(node, d);
    }

    fn build_frame_document(&mut self, frame: u32, url: String) -> Value {
        let d = new_plain_object(&mut self.st);
        let oi = d.index() as usize;
        for (name, op) in [
            ("body", vm::framedom::DOC_BODY),
            ("documentElement", vm::framedom::DOC_ROOT),
            ("title", vm::framedom::DOC_TITLE),
            ("URL", vm::framedom::DOC_URL),
        ] {
            let g = make_native(
                &mut self.st, Native::FrameDom { frame, node: 0, op });
            vm::define_getter(&mut self.st, oi, name, g);
        }
        for (name, op) in [
            ("querySelector", vm::framedom::DOC_QUERY),
            ("querySelectorAll", vm::framedom::DOC_QUERY_ALL),
            ("getElementById", vm::framedom::DOC_BY_ID),
            ("getElementsByTagName", vm::framedom::DOC_BY_TAG),
        ] {
            let f = make_native(
                &mut self.st, Native::FrameDom { frame, node: 0, op });
            self.set_object_prop(d, name, f);
        }
        let loc = new_plain_object(&mut self.st);
        let href = vm::push_str(&mut self.st, url);
        self.set_object_prop(loc, "href", href);
        self.set_object_prop(d, "location", loc);
        let ready = vm::push_str(&mut self.st, "complete".to_string());
        self.set_object_prop(d, "readyState", ready);
        d
    }

    /// Drain the mutations page JS made through a child's mirror, as
    /// (handle, child node index, op, a, b, seq).
    /// Drain the markup scripts wrote into script-created iframes
    /// via `contentDocument.open()/write()/close()`, as
    /// (iframe node index, markup). The host loads each into the real
    /// child document -- the VM has no child arena of its own.
    /// Send document.write to the parser's input stream instead of the
    /// tree, for as long as a parse is in progress.
    pub fn set_parser_writes(&mut self, on: bool) {
        self.st.parser_writes = if on { Some(String::new()) } else { None };
    }

    /// Markup written since the last call, to be tokenized at the
    /// insertion point.
    pub fn take_parser_writes(&mut self) -> String {
        match &mut self.st.parser_writes {
            Some(b) => std::mem::take(b),
            None => String::new(),
        }
    }

    pub fn take_document_writes(&mut self) -> Vec<(u32, String)> {
        std::mem::take(&mut self.st.doc_writes)
    }

    pub fn take_frame_dom_writes(
        &mut self,
    ) -> Vec<(u32, u32, u8, String, String, u64)> {
        std::mem::take(&mut self.st.frame_dom_writes)
    }

    /// Drain the `postMessage` calls page scripts made since the last
    /// call, as (target context, JSON payload, targetOrigin, seq).
    pub fn take_frame_writes(&mut self) -> Vec<(u32, String, String, u64)> {
        std::mem::take(&mut self.st.message_writes)
    }

    /// Queue one message for delivery into this document.
    ///
    /// `source_handle` is the sender's context as *this* document
    /// names it (0 = this window), so `e.source.postMessage(...)`
    /// replies to the right place. Delivery is a macrotask, so the
    /// handlers run on the next pump, not here — the returned logs are
    /// only whatever was already pending.
    pub fn deliver_message(
        &mut self,
        data_json: &str,
        origin: &str,
        source_handle: u32,
    ) -> Vec<String> {
        let source = vm::window_proxy(&mut self.st, source_handle);
        vm::queue_message_task(&mut self.st, data_json, origin, source);
        std::mem::take(&mut self.st.logs)
    }

    /// Retained string-heap bytes (see the backstop in vm.rs).
    pub fn heap_bytes(&self) -> usize {
        self.st.heap_bytes
    }

    /// Tell the VM which `<script>` element is executing, so
    /// `document.currentScript` answers it. `None` clears it (nothing
    /// is running, or the code came from a timer/event).
    pub fn set_current_script(&mut self, node: Option<u32>) {
        self.st.current_script = node;
    }

    /// This document's origin, as stamped onto messages it sends.
    pub fn page_origin(&self) -> String {
        self.st.page_origin.clone()
    }

    /// Feed real scroll state back so `el.scrollTop`/`scrollHeight`
    /// read the engine's actual scrollers (the counterpart of
    /// `set_layout_rects` for scrolling).
    pub fn set_scroll_state(
        &mut self,
        state: Vec<(u32, f64, f64, f64, f64)>,
    ) {
        // A node the page has scrolled but the host has not applied
        // yet is reported at its *old* position: the host's report was
        // taken before it drained the write. Keep the optimistic
        // offset for those and adopt only the fresh content size, so a
        // synchronous `window.scrollTo(0, 400); window.scrollY` read
        // does not snap back to 0 mid-turn.
        let pending: std::collections::HashSet<u32> =
            self.st.scroll_writes.iter().map(|w| w.0).collect();
        let held: Vec<(u32, (f64, f64))> = pending.iter()
            .filter_map(|n| self.st.scroll_state.get(n)
                .map(|s| (*n, (s.0, s.1))))
            .collect();
        self.st.scroll_state.clear();
        for (idx, top, left, sh, sw) in state {
            self.st.scroll_state.insert(idx, (top, left, sh, sw));
        }
        for (idx, (top, left)) in held {
            let e = self.st.scroll_state.entry(idx)
                .or_insert((0.0, 0.0, 0.0, 0.0));
            e.0 = top;
            e.1 = left;
        }
        // The page's own scroller arrives under the DOC_NODE sentinel.
        // window.scrollY and friends are plain data properties (the VM
        // has no accessors), so refresh them here — otherwise a
        // "scroll down one screen" button reads 0 forever and
        // window.scrollBy stacks relative to the wrong origin.
        if let Some(&(top, left, _, _)) =
            self.st.scroll_state.get(&DOC_NODE)
        {
            let window = self.st.known.window;
            for (f, v) in [("scrollY", top), ("pageYOffset", top),
                           ("scrollX", left), ("pageXOffset", left)] {
                self.set_object_prop(window, f, Value::number(v));
            }
        }
    }

    /// Drain the scroll writes page scripts have made since the last
    /// call as (node, top, left, seq, relative), so the host can move
    /// the real scroller. `seq` orders them against
    /// `take_scroll_into_view`; a relative entry carries a delta.
    pub fn take_scroll_writes(
        &mut self,
    ) -> Vec<(u32, f64, f64, u64, bool)> {
        std::mem::take(&mut self.st.scroll_writes)
    }

    /// Drain pending `el.scrollIntoView()` requests as (node, seq).
    pub fn take_scroll_into_view(&mut self) -> Vec<(u32, u64)> {
        std::mem::take(&mut self.st.scroll_into_view)
    }

    pub fn run_source(&mut self, src: &str) -> Result<Value, String> {
        self.run_source_detail(src).map_err(|(_, msg)| msg)
    }

    /// Console output so far, drained.
    pub fn take_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.st.logs)
    }

    /// As `run_source`, but keeping the error's *kind* — "SyntaxError"
    /// for anything that failed to parse or compile, otherwise the
    /// VmError's own kind. A conformance runner has to tell a syntax
    /// error from a TypeError to judge a negative test, and the
    /// flattened message cannot always say which it was.
    pub fn run_source_detail(
        &mut self, src: &str,
    ) -> Result<Value, (String, String)> {
        let syn = |e: String| ("SyntaxError".to_string(), e);
        let ast = parser::parse_program(src)
            .map_err(|e| syn(format!("{e:?}")))?;
        let module =
            compiler::compile(&ast).map_err(|e| syn(format!("{e:?}")))?;
        let mi = self.load(module);
        let m = self.mods.rc(mi);
        let main = m.module.main;
        let nregs = m.module.protos[main as usize].nregs as usize;
        drop(m);
        let base = self.st.regs.len();
        self.st.regs.resize(base + nregs, Value::UNDEFINED);
        self.st.fuel = vm::DEFAULT_FUEL; // fresh budget per top-level script
        // sloppy-script semantics: top-level `this` is the window
        // (webpack UMD wrappers pass it around as the global)
        let this_v = self.st.known.window;
        let out = exec(
            &mut self.st,
            &self.mods,
            mi,
            main,
            base,
            u32::MAX,
            this_v,
            0,
        );
        self.st.regs.truncate(base);
        out.map_err(|e| (e.kind.to_string(), e.report()))
    }

    /// Browser entry point: run page scripts in order, collect console
    /// output. Script errors log instead of aborting the page.
    pub fn run_scripts(&mut self, sources: &[String]) -> Vec<String> {
        for src in sources {
            if let Err(e) = self.run_source(src) {
                self.st.logs.push(format!("[gg-js error] {e}"));
            }
        }
        // HTML runs the microtask checkpoint inside "clean up after
        // running script", *before* `document.currentScript` is
        // restored -- so a promise continuation queued by a script
        // still sees it. Next.js's asset-prefix lookup depends on
        // exactly that, and draining only in the later pump made
        // currentScript null there.
        //
        // What those continuations log stays queued for the pump, so
        // the caller still sees script output and scheduled output as
        // two separate batches.
        let out = std::mem::take(&mut self.st.logs);
        vm::drain_microtasks_now(&mut self.st, &self.mods, PUMP_BUDGET);
        out
    }

    /// Bubble a click from a node to the root: run onclick attributes
    /// and addEventListener handlers. Returns (console output, any
    /// handler ran, default action prevented). Navigation proceeds unless
    /// a handler called
    /// event.preventDefault() or an onclick returned false.
    pub fn dispatch_click(&mut self, idx: usize) -> (Vec<String>, bool, bool)
    {
        self.dispatch_event(idx, "click", true, true, None)
    }

    /// Dispatch a host-initiated DOM event. Form submit/reset/invalid use
    /// this same path as clicks so inline handlers, bubbling listeners and
    /// preventDefault() all share one cancellation model.
    pub fn dispatch_event(
        &mut self,
        idx: usize,
        event_type: &str,
        bubbles: bool,
        cancelable: bool,
        submitter: Option<usize>,
    ) -> (Vec<String>, bool, bool) {
        let Some(doc) = self.st.doc.clone() else {
            return (Vec::new(), false, false);
        };
        let event_type = event_type.to_ascii_lowercase();
        let inline_name = format!("on{event_type}");
        let handler_key = self.name_id(&inline_name);
        let chain: Vec<(u32, Option<String>, Option<Value>)> = {
            let d = doc.borrow();
            if idx >= d.nodes.len() {
                return (Vec::new(), false, false);
            }
            let mut out = Vec::new();
            let mut cur = Some(idx);
            while let Some(i) = cur {
                let inline = d.nodes[i]
                    .attr(&inline_name)
                    .map(str::to_string);
                let property = self.st.dom_expando
                    .get(&(i as u32, handler_key)).copied()
                    .filter(|handler| handler.is_function());
                out.push((i as u32, inline, property));
                if !bubbles {
                    break;
                }
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
        let current_key = self.name_id("currentTarget");
        let bubbles_key = self.name_id("bubbles");
        let cancelable_key = self.name_id("cancelable");
        let prevented_key = self.name_id("defaultPrevented");
        let submitter_key = self.name_id("submitter");
        let pd_key = self.name_id("preventDefault");
        let stop_key = self.name_id("stopPropagation");
        let stop_now_key = self.name_id("stopImmediatePropagation");
        let type_value = vm::intern(&mut self.st, &event_type);
        raw_set_prop(&mut self.st, ev_idx, type_key, type_value);
        raw_set_prop(
            &mut self.st,
            ev_idx,
            target_key,
            Value::dom_node(idx as u32),
        );
        raw_set_prop(
            &mut self.st,
            ev_idx,
            bubbles_key,
            Value::boolean(bubbles),
        );
        raw_set_prop(
            &mut self.st,
            ev_idx,
            cancelable_key,
            Value::boolean(cancelable),
        );
        raw_set_prop(
            &mut self.st,
            ev_idx,
            prevented_key,
            Value::boolean(false),
        );
        let submitter_value = submitter
            .filter(|&node| node < doc.borrow().nodes.len())
            .map(|node| Value::dom_node(node as u32))
            .unwrap_or(Value::NULL);
        raw_set_prop(
            &mut self.st,
            ev_idx,
            submitter_key,
            submitter_value,
        );
        let pd = make_native(
            &mut self.st,
            if cancelable {
                Native::PreventDefault
            } else {
                Native::Noop
            },
        );
        raw_set_prop(&mut self.st, ev_idx, pd_key, pd);
        let noop = make_native(&mut self.st, Native::Noop);
        raw_set_prop(&mut self.st, ev_idx, stop_key, noop);
        raw_set_prop(&mut self.st, ev_idx, stop_now_key, noop);
        self.set_global("event", ev);

        let mut handled = false;
        let mut prevented = false;
        for (node, inline, property) in chain {
            raw_set_prop(
                &mut self.st,
                ev_idx,
                current_key,
                Value::dom_node(node),
            );
            if let Some(src) = inline {
                handled = true;
                // run as a function body: `event` is the argument and
                // `return false` prevents the default action
                let wrapped =
                    format!("(function (event) {{ {src}\n }})(event)");
                match self.run_source(&wrapped) {
                    Ok(v) if cancelable && v.is_boolean() && !v.as_bool() => {
                        prevented = true;
                        self.st.default_prevented = true;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        self.st.logs.push(format!("[gg-js error] {e}"));
                    }
                }
            }
            if let Some(handler) = property {
                handled = true;
                self.st.fuel = vm::DEFAULT_FUEL;
                if let Err(e) = call_value_this(
                    &mut self.st,
                    &self.mods,
                    handler,
                    Some(Value::dom_node(node)),
                    &[ev],
                ) {
                    self.st.logs.push(format!("[gg-js error] {}", e.report()));
                }
            }
            let handlers = self
                .st
                .listeners
                .get(&(node, event_type.clone()))
                .cloned()
                .unwrap_or_default();
            for h in handlers {
                handled = true;
                self.st.fuel = vm::DEFAULT_FUEL; // fresh budget per handler
                // this = the node whose listener is running (the
                // currentTarget), matching dispatchEvent's behavior
                if let Err(e) = call_listener(
                    &mut self.st,
                    &self.mods,
                    h,
                    Some(Value::dom_node(node)),
                    &[ev],
                ) {
                    self.st.logs.push(format!("[gg-js error] {}", e.report()));
                }
            }
            if self.st.default_prevented {
                raw_set_prop(
                    &mut self.st,
                    ev_idx,
                    prevented_key,
                    Value::boolean(true),
                );
            }
        }
        if cancelable && self.st.default_prevented {
            prevented = true;
        }
        (std::mem::take(&mut self.st.logs), handled, prevented)
    }

    /// One-shot headless eval (tests, jsvm_eval/jsvm_run).
    /// Fire the document lifecycle: readyState -> interactive,
    /// DOMContentLoaded (document + window), readyState -> complete,
    /// then window `load`. The loader calls this once after all
    /// scripts ran — app bundles bootstrap from these.
    pub fn fire_lifecycle(&mut self) -> Vec<String> {
        let mut logs = self.fire_dom_content_loaded();
        logs.extend(self.fire_load());
        logs
    }

    /// Parser completion waits for defer/module scripts, but not async or
    /// dynamically inserted scripts. Keep this phase separate from load so
    /// the host loader can preserve that ordering.
    pub fn fire_dom_content_loaded(&mut self) -> Vec<String> {
        if self.st.ready_state != "loading" {
            return Vec::new();
        }
        self.st.ready_state = "interactive";
        self.dispatch_simple(vm::DOC_NODE, "readystatechange");
        self.dispatch_simple(vm::DOC_NODE, "domcontentloaded");
        self.dispatch_simple(vm::WINDOW_NODE, "domcontentloaded");
        std::mem::take(&mut self.st.logs)
    }

    /// Window load follows all initial external scripts, including async
    /// scripts and scripts inserted during initial execution.
    pub fn fire_load(&mut self) -> Vec<String> {
        if self.st.ready_state == "complete" {
            return Vec::new();
        }
        if self.st.ready_state == "loading" {
            let mut logs = self.fire_dom_content_loaded();
            self.st.ready_state = "complete";
            self.dispatch_simple(vm::DOC_NODE, "readystatechange");
            self.dispatch_simple(vm::WINDOW_NODE, "load");
            self.dispatch_simple(vm::DOC_NODE, "load");
            logs.extend(std::mem::take(&mut self.st.logs));
            return logs;
        }
        self.st.ready_state = "complete";
        self.dispatch_simple(vm::DOC_NODE, "readystatechange");
        self.dispatch_simple(vm::WINDOW_NODE, "load");
        self.dispatch_simple(vm::DOC_NODE, "load");
        std::mem::take(&mut self.st.logs)
    }

    fn dispatch_simple(&mut self, node: u32, ty: &str) {
        let property_key = self.name_id(&format!("on{ty}"));
        let property = if node == vm::WINDOW_NODE {
            raw_get_prop(
                &self.st,
                self.st.known.window.index() as usize,
                property_key,
            )
        } else {
            self.st.dom_expando.get(&(node, property_key)).copied()
        };
        let mut cbs = Vec::new();
        if let Some(handler) = property.filter(|value| value.is_function()) {
            cbs.push(handler);
        }
        cbs.extend(self
            .st
            .listeners
            .get(&(node, ty.to_string()))
            .cloned()
            .unwrap_or_default());
        if cbs.is_empty() {
            return;
        }
        // this = the registration target. The window "node" is a
        // sentinel index with no arena entry — hand those handlers the
        // real JS window object instead (a fake dom node would panic
        // on the first property access).
        let this_v = if node == vm::WINDOW_NODE {
            self.st.known.window
        } else {
            Value::dom_node(node)
        };
        // a realistic Event: handlers read target/currentTarget and
        // call preventDefault/stopPropagation (a bare {type} object
        // makes `event.target.X` throw deep in app code)
        let evt = new_plain_object(&mut self.st);
        let ei = evt.index() as usize;
        let tv = vm::intern(&mut self.st, ty);
        let tgt = if node == vm::WINDOW_NODE {
            self.st.known.window
        } else {
            Value::dom_node(node)
        };
        let noop = make_native(&mut self.st, Native::Noop);
        for (k, v) in [
            ("type", tv),
            ("target", tgt),
            ("currentTarget", tgt),
            ("srcElement", tgt),
            ("bubbles", Value::boolean(false)),
            ("cancelable", Value::boolean(false)),
            ("defaultPrevented", Value::boolean(false)),
            ("eventPhase", Value::int(2)),
            ("timeStamp", Value::number(self.st.now_ms)),
            ("preventDefault", noop),
            ("stopPropagation", noop),
            ("stopImmediatePropagation", noop),
        ] {
            let kk = self.name_id(k);
            raw_set_prop(&mut self.st, ei, kk, v);
        }
        let trace = std::env::var("GG_JS_TRACE").is_ok();
        let n = cbs.len();
        for (i, cb) in cbs.into_iter().enumerate() {
            if let Err(e) = call_listener(
                &mut self.st,
                &self.mods,
                cb,
                Some(this_v),
                &[evt],
            ) {
                if trace {
                    self.st.logs.push(format!(
                        "[gg-js error] {} (listener {}/{} of \
                         node {} '{}')",
                        e.msg, i + 1, n, node, ty
                    ));
                } else {
                    self.st
                        .logs
                        .push(format!("[gg-js error] {}", e.report()));
                }
            }
        }
    }

    /// (listeners, timers, microtasks) — boot diagnosis: did the page
    /// register anything to wake up for?
    pub fn pending_counts(&self) -> (usize, usize, usize) {
        (
            self.st.listeners.values().map(|v| v.len()).sum(),
            self.st.timers.len(),
            self.st.microtasks.len(),
        )
    }

    /// One real-time slice of the event loop (see vm::pump_bounded).
    /// Returns (console output, fetches to service).
    pub fn tick(
        &mut self,
        dt_ms: f64,
    ) -> (Vec<String>, Vec<(u32, String)>) {
        let (logs, requests) = self.tick_requests(dt_ms);
        let fetches = requests
            .into_iter()
            .map(|request| (request.0, request.1))
            .collect();
        (logs, fetches)
    }

    pub fn tick_requests(
        &mut self,
        dt_ms: f64,
    ) -> (Vec<String>, Vec<HostFetch>) {
        let fetches = vm::pump_bounded(
            &mut self.st,
            &self.mods,
            10_000,
            dt_ms,
        )
        .into_iter()
        .map(host_fetch)
        .collect();
        (std::mem::take(&mut self.st.logs), fetches)
    }

    /// Fill location.* from the real page URL (loader calls this
    /// before scripts run).
    /// Seed `document.cookie` from the network jar (a `k=v; k2=v2`
    /// string) before page scripts run, so JS sees the server session.
    /// Replace the `document.cookie` replica with the host's filtered
    /// snapshot.
    ///
    /// Distinct from `seed_cookies`, which merges: a snapshot has to be
    /// able to *remove*, or a cookie the server expired would linger in
    /// the page's view forever.
    pub fn set_cookie_snapshot(&mut self, s: &str) {
        self.st.cookies.clear();
        self.seed_cookies(s);
    }

    pub fn seed_cookies(&mut self, s: &str) {
        for pair in s.split(';') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=') {
                let k = k.trim().to_string();
                if k.is_empty() {
                    continue;
                }
                let v = v.trim().to_string();
                match self.st.cookies.iter_mut().find(|(ck, _)| *ck == k) {
                    Some(e) => e.1 = v,
                    None => self.st.cookies.push((k, v)),
                }
            }
        }
    }

    /// Current document.cookie-visible value as `k=v; k2=v2`.
    pub fn cookies_string(&self) -> String {
        self.st
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Drain the original document.cookie setter strings for the host.
    /// Seeded network cookies never enter this queue.
    pub fn take_cookie_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.st.cookie_writes)
    }

    pub fn set_page_url(&mut self, url: &str) {
        let (scheme, rest) =
            url.split_once("://").unwrap_or(("https", url));
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
        let origin = format!("{scheme}://{hostport}");
        // every message this document sends is stamped with this
        self.st.page_origin = origin.clone();
        let window = self.st.known.window;
        if window.is_object() {
            let ov = vm::push_str(&mut self.st, origin.clone());
            self.set_object_prop(window, "origin", ov);
        }
        let sets = [
            ("href", url.to_string()),
            ("protocol", format!("{scheme}:")),
            ("host", hostport.to_string()),
            ("hostname", hostname.to_string()),
            ("pathname", path.to_string()),
            ("search", search.to_string()),
            ("hash", hash.to_string()),
            ("origin", origin),
            ("port", port.to_string()),
        ];
        let lockey = self.name_id("location");
        let loc = self.st.globals[lockey as usize];
        if !loc.is_object() {
            return;
        }
        let li = loc.index() as usize;
        for (f, v) in sets {
            let k = self.name_id(f);
            let sv = vm::push_str(&mut self.st, v);
            raw_set_prop(&mut self.st, li, k, sv);
        }
    }

    /// One script under a profiler, no Python in the way:
    /// `GG_JS_PROF='<code>' valgrind --tool=callgrind cargo test
    /// --release prof_js -- --nocapture`.
    pub fn prof_entry() {
        let Ok(src) = std::env::var("GG_JS_PROF") else { return };
        let mut vm = PageVm::new(None);
        let out = vm.run_source(&src);
        std::hint::black_box(out.is_ok());
    }

    pub fn eval(src: &str) -> Result<(Value, Vec<String>), String> {
        let mut vm = PageVm::new(None);
        let v = vm.run_source(src)?;
        Ok((v, std::mem::take(&mut vm.st.logs)))
    }
}

#[cfg(test)]
mod prof {
    #[test]
    fn prof_js() {
        super::PageVm::prof_entry();
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
    fn proxy_and_reflect_internal_operations() {
        assert_eq!(
            n("(typeof Proxy === 'function' && typeof Reflect === 'object') ? 1 : 0"),
            1.0,
        );
        assert_eq!(
            n("var target={x:2}; var p=new Proxy(target,{\
               get:function(t,k,r){return k==='x'?Reflect.get(t,k,r)+3:Reflect.get(t,k,r);},\
               set:function(t,k,v,r){return Reflect.set(t,k,v*2,r);}});\
               p.x=4; p.x*10+target.x"),
            118.0,
        );
        assert_eq!(
            n("var log=''; var t={a:1}; var p=new Proxy(t,{\
               has:function(t,k){log+='h';return k==='virtual'||Reflect.has(t,k);},\
               deleteProperty:function(t,k){log+='d';return Reflect.deleteProperty(t,k);}});\
               var h=('virtual' in p)&&('a' in p); delete p.a;\
               (h?100:0)+(t.a===undefined?10:0)+log.length"),
            113.0,
        );
        assert_eq!(
            n("var p=new Proxy({a:1,b:2},{ownKeys:function(){return ['b','a','z'];}});\
               Reflect.ownKeys(p).join(',')==='b,a,z'?1:0"),
            1.0,
        );
        assert_eq!(
            n("var p=new Proxy({a:1,b:2},{ownKeys:function(){return ['b','a','z'];}});\
               Object.keys(p).join(',')==='b,a'?1:0"),
            1.0,
        );
        assert_eq!(
            n("function add(a,b){return a+b;}\
               var p=new Proxy(add,{apply:function(t,th,args){\
                 return Reflect.apply(t,th,args)+1;}}); p(20,21)"),
            42.0,
        );
        assert_eq!(
            n("function F(x){this.x=x;}\
               var P=new Proxy(F,{construct:function(t,args,n){\
                 return {x:args[0]+2};}}); (new P(40)).x"),
            42.0,
        );
        assert_eq!(
            n("function F(x){this.x=x;} var P=new Proxy(F,{});\
               var v=Reflect.construct(P,[42]); v.x"),
            42.0,
        );
        assert_eq!(
            n("var r=Proxy.revocable({x:1},{}); var p=r.proxy;\
               var before=p.x; r.revoke(); var caught=0;\
               try{p.x;}catch(e){caught=1;} before+41*caught"),
            42.0,
        );
        assert_eq!(
            n("var t={x:1}; var p=new Proxy(t,{});\
               var d=Reflect.getOwnPropertyDescriptor(p,'x');\
               var a=Reflect.defineProperty(p,'y',{value:40});\
               var b=Reflect.preventExtensions(t);\
               var c=Reflect.set(t,'z',9);\
               d.value+(a?1:0)+(b?0:10)+(c?100:0)+p.y"),
            42.0,
        );
        assert_eq!(
            n("var score=0;\
               var a=[1]; var pa=new Proxy(a,{ownKeys:function(){return [];}});\
               try{Reflect.ownKeys(pa);}catch(e){if(e instanceof TypeError)score+=1;}\
               var t={x:1}; Reflect.preventExtensions(t);\
               var ph=new Proxy(t,{has:function(){return false;}});\
               try{'x' in ph;}catch(e){if(e instanceof TypeError)score+=10;}\
               var pd=new Proxy(t,{getOwnPropertyDescriptor:function(){return undefined;}});\
               try{Reflect.getOwnPropertyDescriptor(pd,'x');}\
               catch(e){if(e instanceof TypeError)score+=100;}\
               var pe=new Proxy({},{preventExtensions:function(){return true;}});\
               try{Reflect.preventExtensions(pe);}\
               catch(e){if(e instanceof TypeError)score+=1000;}\
               function F(){} var pc=new Proxy(F,{construct:function(){return 1;}});\
               try{new pc();}catch(e){if(e instanceof TypeError)score+=10000;} score"),
            11111.0,
        );
    }

    #[test]
    fn regex_js_syntax_translates_to_rust() {
        // \uXXXX (4 bare hex, JS form) compiles and matches; Rust needs
        // \u{XXXX} so without translation the whole pattern never matched
        assert_eq!(n(r"/A/.test('A') ? 1 : 0"), 1.0);
        assert_eq!(n(r"/A/.test('B') ? 1 : 0"), 0.0);
        // the ubiquitous regex-escape class has a literal '[' inside it,
        // which Rust reads as a nested class and rejects unless escaped
        assert_eq!(
            n(r"'a.b*c'.replace(/[\\^$.*+?()[\]{}|]/g, '_') === 'a_b_c' ? 1 : 0"),
            1.0,
        );
        // JS empty-class semantics: [^] is any char, [] is never
        assert_eq!(n(r"/[^]/.test('x') ? 1 : 0"), 1.0);
        assert_eq!(n(r"/[]/.test('x') ? 1 : 0"), 0.0);
        // a surrogate range is remapped to the astral plane, so an emoji
        // (one scalar here) matches while a BMP char does not
        assert_eq!(n("/[\\uD800-\\uDFFF]/.test('😀') ? 1 : 0"), 1.0);
        assert_eq!(n("/[\\uD800-\\uDFFF]/.test('a') ? 1 : 0"), 0.0);
        // already-braced \u{...} is left untouched
        assert_eq!(n(r"/\u{1F600}/u.test('😀') ? 1 : 0"), 1.0);
        // JS `\0` (NUL escape) must translate to `\x00`; a bare `\0` made
        // Rust reject the class, degrading naver's URL-parser polyfill
        // patterns like `/[\0-~]/` and `/[\0\t\n\r #%/:<>?@[\]^|]/` to
        // never-matching (every URL char then read as "forbidden"/"non-ASCII").
        assert_eq!(n(r"/[\0-~]/.test('A') ? 1 : 0"), 1.0);
        assert_eq!(n(r"/[^\0-~]/.test('A') ? 1 : 0"), 0.0);
        assert_eq!(n("/[^\\0-~]/.test('\u{AC00}') ? 1 : 0"), 1.0);
        assert_eq!(n(r"/[\0#%:@]/.test('#') ? 1 : 0"), 1.0);
        // .source still reports the original JS pattern
        assert_eq!(n(r"/A/.source === 'A' ? 1 : 0"), 1.0);
    }

    #[test]
    fn string_replace_with_function() {
        // a function replacement must be CALLED per match, not coerced to
        // the string "function..." and spliced in literally. lodash's
        // `_.template` is `string.replace(reDelimiters, fn)`; the broken
        // form stalled naver's search-autocomplete boot.
        assert_eq!(
            n(r"'a1b2c'.replace(/\d/g,function(m){return '<'+m+'>';})==='a<1>b<2>c'?1:0"),
            1.0,
        );
        // capture groups arrive as args after the match
        assert_eq!(
            n(r"'x=5'.replace(/(\w)=(\d)/,function(m,a,b){return a+':'+b;})==='x:5'?1:0"),
            1.0,
        );
        // no-capture regex: 2nd arg is the offset
        assert_eq!(
            n(r"'abc'.replace(/b/,function(m,off){return '['+off+']';})==='a[1]c'?1:0"),
            1.0,
        );
        // string pattern + function replaces the first occurrence only
        assert_eq!(
            n(r"'hello'.replace('l',function(){return 'L';})==='heLlo'?1:0"),
            1.0,
        );
        // replaceAll + function hits every occurrence
        assert_eq!(
            n(r"'a.b.c'.replaceAll('.',function(){return '-';})==='a-b-c'?1:0"),
            1.0,
        );
        // lodash-style delimiter scan yields the callback output
        assert_eq!(
            n(r"'p<%=x%>q'.replace(/<%=([\s\S]+?)%>/g,function(m,c){return '{'+c+'}';})==='p{x}q'?1:0"),
            1.0,
        );
        // plain string replacement still works (no regression)
        assert_eq!(n(r"'aaa'.replace(/a/g,'b')==='bbb'?1:0"), 1.0);
    }

    #[test]
    fn function_constructor_compiles_body() {
        // `Function(...)` must compile a real callable, not return a stub —
        // lodash `_.template` ends with `Function(keys, source).apply(...)`
        // (called WITHOUT `new`, which is the shape that matters here).
        assert_eq!(n("Function('a','b','return a+b')(2,3)"), 5.0);
        assert_eq!(n("Function('x','return x*x')(4)"), 16.0);
        // params supplied via apply (the exact lodash template call shape)
        assert_eq!(
            n("Function('a','b','return a*b').apply(null,[6,7])"),
            42.0,
        );
        // multi-statement body with control flow
        assert_eq!(
            n("Function('a','var s=0;for(var i=0;i<a;i++)s+=i;return s;')(5)"),
            10.0,
        );
        // zero params
        assert_eq!(n("Function('return 42')()"), 42.0);
        // `new Function` must work too (uses the constructor's return value)
        assert_eq!(n("(new Function('a','b','return a+b'))(2,3)"), 5.0);
    }

    #[test]
    fn function_length_is_arity() {
        // func.length = declared parameter count. Missing this read as
        // undefined, so lodash's overRest computed `undefined - 1 = NaN`,
        // emptied its rest-args array, and crashed naver's search
        // autocomplete on `.length of undefined`.
        assert_eq!(n("(function(a,b){}).length"), 2.0);
        assert_eq!(n("(function(){}).length"), 0.0);
        assert_eq!(n("(function(a,b,c){}).length"), 3.0);
        // arrow functions carry arity too
        assert_eq!(n("((a,b)=>a+b).length"), 2.0);
        // a returned closure keeps its arity (the exact overRest case)
        assert_eq!(
            n("(function(){ return function(x,y){}; })().length"),
            2.0,
        );
        // and .name resolves for a named function
        assert_eq!(n("(function foo(){}).name === 'foo' ? 1 : 0"), 1.0);
        // the lodash overRest pattern now works end-to-end
        assert_eq!(
            n("function oR(f,s){s=Math.max(s===undefined?f.length-1:s,0);\
               return function(){var a=arguments,n=Math.max(a.length-s,0),\
               r=Array(n),i=-1;while(++i<n)r[i]=a[s+i];return f(a[0],r);};}\
               var g=oR(function(o,src){return src.length;});g({},1,2,3)"),
            3.0,
        );
    }

    #[test]
    fn with_statement_scopes_reads() {
        // `with (obj)` resolves bare reads against obj first — lodash
        // `_.template`'s compiled body depends on this.
        assert_eq!(n("var o={a:10,b:20},r=0; with(o){ r=a+b; } r"), 30.0);
        // a local still wins when obj lacks the name
        assert_eq!(n("var o={a:1},x=5; with(o){ x=x+a; } x"), 6.0);
        // inner with-object shadows the outer
        assert_eq!(n("var r=0; with({a:1}){ with({a:2}){ r=a; } } r"), 2.0);
        // with-scope does NOT leak past the block
        assert_eq!(n("with({q:9}){} typeof q === 'undefined' ? 1 : 0"), 1.0);
        // the lodash-template shape: with inside a Function-compiled body
        assert_eq!(
            n("var f=new Function('o','var s=0; with(o){ s=x+y; } return s;');\
               f({x:3,y:4})"),
            7.0,
        );
        // a with-object does NOT leak into a function called from the body
        // (with is lexical, not dynamic)
        assert_eq!(
            n("var o={x:1}; function f(){ return typeof x; } var r;\
               with(o){ r=f(); } r==='undefined'?1:0"),
            1.0,
        );
        // a write to a name the object owns updates the object
        assert_eq!(n("var o={a:1}; with(o){ a=5; } o.a"), 5.0);
        // an early return from inside `with` still closes the scope
        assert_eq!(
            n("function g(){ with({z:7}){ return z; } } g()"),
            7.0,
        );
        // a throw inside `with` unwinds the scope (q not visible after)
        assert_eq!(
            n("var seen=1; try{ with({q:9}){ throw 0; } }catch(e){}\
               typeof q==='undefined'?seen:0"),
            1.0,
        );
    }

    #[test]
    fn cookie_seed_and_read_roundtrip() {
        // network jar <-> document.cookie bridge: seed before scripts,
        // read back JS writes.
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<p>cookie bridge</p>"),
        ))));
        vm.seed_cookies("sid=abc; theme=dark");
        assert_eq!(vm.cookies_string(), "sid=abc; theme=dark");
        assert!(vm.take_cookie_writes().is_empty());
        // re-seeding replaces an existing name and appends new ones
        vm.seed_cookies("sid=xyz; lang=ko");
        let s = vm.cookies_string();
        assert!(s.contains("sid=xyz"), "{s}");
        assert!(s.contains("theme=dark"), "{s}");
        assert!(s.contains("lang=ko"), "{s}");

        vm.run_scripts(&[
            "document.cookie = 'fresh=1; Path=/app; SameSite=Strict';"
                .to_string(),
        ]);
        assert_eq!(
            vm.take_cookie_writes(),
            vec!["fresh=1; Path=/app; SameSite=Strict"]
        );
        assert!(vm.take_cookie_writes().is_empty());

        vm.run_scripts(&[
            "document.cookie = 'bad=x\\r\\nX-Injected: yes';".to_string(),
        ]);
        assert!(!vm.cookies_string().contains("bad="));
        assert!(vm.take_cookie_writes().is_empty());
    }

    #[test]
    fn array_and_object_modern_methods() {
        // Array.prototype.at (negative indexes)
        assert_eq!(n("[1,2,3].at(-1)"), 3.0);
        assert_eq!(n("[5,6,7].at(1)"), 6.0);
        // flat / flatMap
        assert_eq!(n("[[1],[2,3]].flat().length"), 3.0);
        assert_eq!(
            n("[1,2].flatMap(function(x){return [x,x*10];})\
               .join(',') === '1,10,2,20' ? 1 : 0"),
            1.0,
        );
        // findLast / findLastIndex
        assert_eq!(n("[1,2,3,4].findLast(function(x){return x<3;})"), 2.0);
        assert_eq!(
            n("[1,2,3,4].findLastIndex(function(x){return x<3;})"),
            1.0,
        );
        // Object.fromEntries
        assert_eq!(
            n("var o=Object.fromEntries([['a',1],['b',2]]); o.a*10+o.b"),
            12.0,
        );
        // String.prototype.at
        assert_eq!(n("'abc'.at(-1) === 'c' ? 1 : 0"), 1.0);
        // structuredClone is a deep, independent copy
        assert_eq!(n("structuredClone({a:[1,2]}).a[1]"), 2.0);
        assert_eq!(
            n("var s={x:{y:1}}; var c=structuredClone(s); \
               c.x.y=9; s.x.y"),
            1.0,
        );
        // extracted (uncurried) forms route through the same core
        assert_eq!(
            n("var f=[].flatMap; \
               f.call([1,2],function(x){return [x];}).length"),
            2.0,
        );
        assert_eq!(n("var a=[].at; a.call([9,8,7],-1)"), 7.0);
    }

    #[test]
    fn image_constructor_builds_img_node() {
        // new Image() must yield a usable <img> element (naver's thumbnail
        // components construct one during render); needs a real document
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<html><body></body></html>"),
        ))));
        let logs = vm.run_scripts(&["\
            var i = new Image();\n\
            console.log('tag ' + String(i.tagName).toLowerCase());\n\
            i.src = 'http://x/y.png';\n\
            i.onload = function () {};\n\
            console.log('src ' + (String(i.src).indexOf('y.png') >= 0 ? 1 : 0));\n\
            console.log('ok ' + (new Image() ? 1 : 0));\n"
            .to_string()]);
        assert!(logs.contains(&"tag img".to_string()), "{logs:?}");
        assert!(logs.contains(&"src 1".to_string()), "{logs:?}");
        assert!(logs.contains(&"ok 1".to_string()), "{logs:?}");
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
    fn to_primitive_object_coercion() {
        // valueOf drives arithmetic
        assert_eq!(
            n("var o = { valueOf: function() { return 7; } }; o * 3"),
            21.0);
        assert_eq!(
            n("var o = { valueOf: function() { return 5; } }; o + 1"),
            6.0);
        // toString drives concat when valueOf yields no primitive
        assert_eq!(
            n("var o = { toString: function() { return '4'; } }; \
               +(o + '2')"),
            42.0);
        // == converts the object side; object == object is identity
        assert_eq!(
            n("var o = { valueOf: function() { return 3; } }; \
               (o == 3 ? 1 : 0) + ({} == {} ? 10 : 0) + \
               (function(a) { return a == a ? 100 : 0; })({})"),
            101.0);
        // prototype methods convert instances too
        assert_eq!(
            n("function C() {} C.prototype.valueOf = \
               function() { return 9; }; new C() - 4"),
            5.0);
        // plain objects and arrays fall back to display strings
        assert_eq!(n("+[] + [3] * 2"), 6.0);
        assert_eq!(n("('' + {}).length"), 15.0);
        // computed access finds the staples too (core-js getMethod
        // does V["valueOf"] — GetIndex, not GetProp)
        assert_eq!(
            n("var o = { a: 1 }; var f = o['valueOf']; \
               (typeof f === 'function' ? 1 : 0) + \
               (o['toString']().length > 0 ? 10 : 0) + \
               ([]['sort'] ? 100 : 0)"),
            111.0);
    }

    #[test]
    fn function_to_string_and_dom_siblings() {
        // String(fn) — bundle feature-sniffing must not throw
        assert_eq!(
            n("function f() {} (('' + f).length > 0 ? 1 : 0) + \
               (typeof f.toString === 'function' ? 10 : 0) + \
               (typeof f['valueOf'] === 'function' ? 100 : 0)"),
            111.0);
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse(
                "<html><body><div id=a>t1<p id=b>x</p>t2\
                 <p id=c>y</p></div></body></html>",
            ),
        ))));
        let logs = vm.run_scripts(&["\
            var a = document.getElementById('a');\n\
            var b = document.getElementById('b');\n\
            console.log('last ' + (a.lastChild === a.firstChild ? 0 : 1));\n\
            console.log('nes ' + b.nextElementSibling.id);\n\
            console.log('pes ' + \
                (b.previousElementSibling === null ? 'null' : 'el'));\n\
            console.log('ns ' + b.nextSibling.nodeType);\n"
            .to_string()]);
        assert!(logs.contains(&"last 1".to_string()), "{logs:?}");
        assert!(logs.contains(&"nes c".to_string()), "{logs:?}");
        assert!(logs.contains(&"pes null".to_string()), "{logs:?}");
        assert!(logs.contains(&"ns 3".to_string()), "{logs:?}");
    }

    #[test]
    fn anchor_url_decomposition() {
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<html><body></body></html>"),
        ))));
        vm.set_page_url("https://www.naver.com/dir/page?q=1#top");
        let logs = vm.run_scripts(&["\
            var a = document.createElement('a');\n\
            a.setAttribute('href', '/search?x=2#frag');\n\
            console.log('pn ' + a.pathname);\n\
            console.log('hn ' + a.hostname);\n\
            console.log('pr ' + a.protocol);\n\
            console.log('se ' + a.search);\n\
            console.log('ha ' + a.hash);\n\
            var b = document.createElement('a');\n\
            b.setAttribute('href', 'rel.html');\n\
            console.log('rel ' + b.pathname);\n\
            var s = document.createElement('div').style;\n\
            console.log('in ' + ('position' in s) + ('9' in s));\n"
            .to_string()]);
        assert!(logs.contains(&"pn /search".to_string()), "{logs:?}");
        assert!(
            logs.contains(&"hn www.naver.com".to_string()),
            "{logs:?}"
        );
        assert!(logs.contains(&"pr https:".to_string()), "{logs:?}");
        assert!(logs.contains(&"se ?x=2".to_string()), "{logs:?}");
        assert!(logs.contains(&"ha #frag".to_string()), "{logs:?}");
        assert!(logs.contains(&"rel /dir/rel.html".to_string()),
                "{logs:?}");
        assert!(logs.contains(&"in truefalse".to_string()), "{logs:?}");
    }

    #[test]
    fn object_keys_use_to_property_key() {
        // polyfilled Symbol wrappers carry unique toString tags; as
        // computed keys they must stay distinct (not all collapse to
        // "[object Object]")
        assert_eq!(
            n("var k1 = { toString: function(){ return '@@a'; } }; \
               var k2 = { toString: function(){ return '@@b'; } }; \
               var o = {}; o[k1] = 1; o[k2] = 2; \
               (o[k1] === 1 ? 1 : 0) + (o[k2] === 2 ? 10 : 0) + \
               (k1 in o ? 100 : 0) + ('@@a' in o ? 1000 : 0)"),
            1111.0);
    }

    #[test]
    fn window_writes_reach_bare_globals() {
        // polyfills replace prelude globals via the window face; the
        // bare name must see the replacement (split-brain Symbol bug)
        assert_eq!(
            n("window.Symbol = function(){ return { poly: 1 }; }; \
               (Symbol === window.Symbol ? 1 : 0) + \
               (typeof Symbol() === 'object' ? 10 : 0)"),
            11.0);
        assert_eq!(
            n("window['parseInt'] = function(){ return 777; }; \
               parseInt('42') === 777 ? 1 : 0"),
            1.0);
        assert_eq!(
            n("Object.defineProperty(window, 'isNaN', \
               { value: function(){ return 'patched'; } }); \
               isNaN(5) === 'patched' ? 1 : 0"),
            1.0);
    }

    #[test]
    fn regex_instance_properties() {
        assert_eq!(
            n("var r = /ab+c/gi; \
               (r.source === 'ab+c' ? 1 : 0) + \
               (r.global ? 10 : 0) + \
               (r.ignoreCase ? 100 : 0) + \
               (r.multiline ? 1000 : 0) + \
               (r.flags === 'gi' ? 10000 : 0) + \
               (r.source.match(/\\w+/) ? 100000 : 0)"),
            110111.0);
        // extracted exec/test survive uncurrying (core-js parseInt)
        assert_eq!(
            n("var h = /^[+-]?0x/i; var p = h.exec; \
               (typeof p === 'function' ? 1 : 0) + \
               (p.call(h, '0x1F') ? 10 : 0) + \
               (p.call(h, 'zzz') === null ? 100 : 0) + \
               (h['test'].call(h, '0xAB') ? 1000 : 0)"),
            1111.0);
    }

    #[test]
    fn string_static_from_char_code() {
        assert_eq!(n("String.fromCharCode(72, 105).length"), 2.0);
        assert_eq!(n("String.fromCharCode(65) === 'A' ? 1 : 0"), 1.0);
        // extraction survives (bundles do var f = String.fromCharCode)
        assert_eq!(
            n("var f = String.fromCharCode; f(66) === 'B' ? 1 : 0"),
            1.0);
    }

    #[test]
    fn descriptors_and_accessors() {
        // getter invoked on read, with the right this
        assert_eq!(
            n("var o = {v: 6}; Object.defineProperty(o, 'x', \
               {get: function() { return this.v * 7; }}); o.x"), 42.0);
        // Computed reads must use the same accessor-aware lookup. Module
        // namespace imports use ns["export"] and exposed this gap.
        assert_eq!(
            n("var o = {v: 6}; Object.defineProperty(o, 'x', \
               {get: function() { return this.v * 7; }}); o['x']"), 42.0);
        // Method-call lookup also invokes an accessor before checking that
        // the resulting value is callable (module namespaces expose
        // exported functions this way).
        assert_eq!(
            n("var o = {}; Object.defineProperty(o, 'f', \
               {get: function() { return function(x) { return x + 1; }; }}); \
               o.f(41)"), 42.0);
        // setter intercepts writes
        assert_eq!(
            n("var o = {}; Object.defineProperty(o, 'y', \
               {set: function(v) { this.stored = v * 2; }}); \
               o.y = 21; o.stored"), 42.0);
        // data descriptor + read-back
        assert_eq!(
            n("var o = {}; Object.defineProperty(o, 'z', {value: 5}); \
               o.z * 10 + Object.getOwnPropertyDescriptor(o, 'z').value"),
            55.0);
        // accessor on a prototype: instances hit it with their own this
        assert_eq!(
            n("function F(v) { this.v = v; } \
               Object.defineProperty(F.prototype, 'twice', \
               {get: function() { return this.v * 2; }}); \
               new F(4).twice * 10 + new F(9).twice"), 98.0);
        // Object.create / getPrototypeOf / setPrototypeOf
        assert_eq!(
            n("var p = {a: 7}; var o = Object.create(p); \
               (Object.getPrototypeOf(o) === p ? 100 : 0) + o.a"), 107.0);
        assert_eq!(
            n("var o = {}; Object.setPrototypeOf(o, {b: 3}); o.b"), 3.0);
        // missing descriptor -> undefined
        assert_eq!(
            n("Object.getOwnPropertyDescriptor({}, 'nope') === \
               undefined ? 1 : 0"), 1.0);
        // defineProperties: batch of data + accessor descriptors
        assert_eq!(
            n("var o = {}; Object.defineProperties(o, { \
                 a: {value: 4}, \
                 b: {get: function() { return this.a * 10; }} }); \
               o.a + o.b"), 44.0);
        // an accessor redefined OVER an existing data property wins on read
        // (the shape still carries the old data slot; the getter must be
        // consulted first — regression: GetProp fast path read the slot)
        assert_eq!(
            n("var o = {y: 10}; Object.defineProperty(o, 'y', \
               {get: function() { return 99; }}); o.y"), 99.0);
        // assignment to a getter-only accessor is a sloppy no-op, NOT a
        // clobber back into a data slot (regression: SetProp created a
        // stray data property that shadowed nothing but leaked the value)
        assert_eq!(
            n("var o = {}; Object.defineProperty(o, 'z', \
               {get: function() { return 7; }}); o.z = 123; o.z"), 7.0);
        // a setter still intercepts writes when the property already had a
        // data slot before being redefined as an accessor
        assert_eq!(
            n("var o = {w: 1}; Object.defineProperty(o, 'w', \
               {get: function() { return this._w || 0; }, \
                set: function(v) { this._w = v * 3; }}); \
               o.w = 14; o.w"), 42.0);
        // getOwnPropertyNames sees data props AND accessor-only keys
        // (Object.keys skips the accessor side-table)
        assert_eq!(
            n("var o = {x: 1}; Object.defineProperty(o, 'y', \
               {get: function() { return 2; }}); \
               var names = Object.getOwnPropertyNames(o); \
               (names.indexOf('x') >= 0 ? 1 : 0) + \
               (names.indexOf('y') >= 0 ? 2 : 0) + \
               (Object.keys(o).indexOf('y') < 0 ? 4 : 0)"), 7.0);
        // array: index keys come first
        assert_eq!(
            n("Object.getOwnPropertyNames([7, 8]).join(',') === '0,1' \
               ? 1 : 0"), 1.0);
        // array `length` descriptor: writable data prop, not undefined.
        // core-js's length setter reads `.writable` before mutating and
        // throws "Cannot set read only .length" on an undefined descriptor,
        // which aborts React's commit on Naver.
        assert_eq!(
            n("var d = Object.getOwnPropertyDescriptor([1, 2, 3], 'length'); \
               d.value * 100 + (d.writable ? 10 : 0) + \
               (d.enumerable ? 0 : 1)"), 311.0);
        // array index descriptor: value + writable/enumerable/configurable
        assert_eq!(
            n("var d = Object.getOwnPropertyDescriptor(['a', 'b'], '1'); \
               (d.value === 'b' ? 1 : 0) + (d.writable ? 2 : 0) + \
               (d.enumerable ? 4 : 0)"), 7.0);
        // out-of-range index -> undefined
        assert_eq!(
            n("Object.getOwnPropertyDescriptor([1], '5') === undefined \
               ? 1 : 0"), 1.0);
    }

    #[test]
    fn numeric_string_keys_survive_descriptor_round_trips() {
        // Plain objects store numeric keys in element storage; the
        // descriptor APIs must see them there. Babel's _objectSpread2
        // copies {"907": press} one property at a time through
        // getOwnPropertyDescriptor(s) + defineProperty/ies — any gap
        // silently emptied naver's newsstand pressInfo map.
        assert_eq!(
            n("var o = {}; o['907'] = 'x'; \
               var d = Object.getOwnPropertyDescriptor(o, '907'); \
               d && d.value === 'x' && d.enumerable ? 1 : 0"), 1.0);
        // defineProperty with a numeric key is visible to reads + keys
        assert_eq!(
            n("var o = {}; Object.defineProperty(o, '32', \
               {value: 'p', enumerable: true, writable: true, \
                configurable: true}); \
               (o['32'] === 'p' ? 1 : 0) + \
               (Object.keys(o).indexOf('32') >= 0 ? 2 : 0)"), 3.0);
        // no phantom keys from the sparse element range
        assert_eq!(
            n("var o = {}; o['200'] = 1; o['abc'] = 2; \
               Object.keys(o).length"), 2.0);
        assert_eq!(
            n("var o = {}; o['200'] = 1; \
               Object.getOwnPropertyNames(o).join(',') === '200' \
               ? 1 : 0"), 1.0);
        // getOwnPropertyDescriptors + defineProperties round-trip
        assert_eq!(
            n("var src = {}; src['907'] = {pid: '907'}; src.name = 'n'; \
               var ds = Object.getOwnPropertyDescriptors(src); \
               var dst = Object.defineProperties({}, ds); \
               (dst['907'].pid === '907' ? 1 : 0) + \
               (dst.name === 'n' ? 2 : 0) + \
               (Object.keys(dst).length === 2 ? 4 : 0)"), 7.0);
        // the full babel spread chain: reduce + per-key copy
        assert_eq!(
            n("var blocks = [{pid:'907'},{pid:'032'},{pid:'315'}]; \
               var b = blocks.reduce(function(acc, t){ \
                 var e = Object.defineProperties({}, \
                   Object.getOwnPropertyDescriptors(acc)); \
                 e[t.pid] = t; return e; }, {}); \
               (Object.keys(b).length === 3 ? 1 : 0) + \
               (b['907'] && b['032'] && b['315'] ? 2 : 0)"), 3.0);
        // only CANONICAL numeric strings are element indices: "032"
        // stays a distinct named property (naver's zero-padded press
        // ids) and survives keys/descriptor round-trips verbatim
        assert_eq!(
            n("var o = {}; o['032'] = 'a'; o['32'] = 'b'; \
               (o['032'] === 'a' ? 1 : 0) + (o['32'] === 'b' ? 2 : 0) + \
               (Object.keys(o).indexOf('032') >= 0 ? 4 : 0) + \
               (Object.getOwnPropertyDescriptor(o, '032').value === 'a' \
                ? 8 : 0)"), 15.0);
        assert_eq!(
            n("var src = {}; src['032'] = 'p'; \
               var dst = Object.defineProperties({}, \
                 Object.getOwnPropertyDescriptors(src)); \
               (dst['032'] === 'p' ? 1 : 0) + \
               (Object.keys(dst).join(',') === '032' ? 2 : 0)"), 3.0);
        // delete works on both storages
        assert_eq!(
            n("var o = {}; o['907'] = 1; o['032'] = 2; \
               delete o['907']; delete o['032']; \
               Object.keys(o).length"), 0.0);
    }

    #[test]
    fn extracted_call_apply_bind() {
        // the core-js uncurryThis pattern end to end
        assert_eq!(
            n("var ap = Function.prototype.apply; \
               function f(a, b) { return this.v + a * 10 + b; } \
               ap.call(f, {v: 100}, [2, 3])"), 123.0);
        assert_eq!(
            n("function f(a) { return this.v + a; } \
               var c = f.call; c.call(f, {v: 7}, 5)"), 12.0);
        assert_eq!(
            n("function f(a) { return a * 2; } \
               var b = f.bind; var g = b.call(f, null, 21); g()"), 42.0);
    }

    #[test]
    fn string_concat_is_variadic() {
        // direct String.prototype.concat must fold ALL args, not just the
        // first (Naver's bundles concat multi-part strings)
        assert_eq!(
            n("'n='.concat(4, '!', '?') === 'n=4!?' ? 1 : 0"), 1.0);
        assert_eq!(n("'a'.concat('b', 'c', 'd') === 'abcd' ? 1 : 0"), 1.0);
        // extracted form folds all args too
        assert_eq!(
            n("'x'.concat.call('p', 'q', 'r') === 'pqr' ? 1 : 0"), 1.0);
    }

    #[test]
    fn bound_constructor_new() {
        // Babel _construct: new (Function.bind.apply(C, [null, args]))
        assert_eq!(
            n("function C(v) { \
               if (!(this instanceof C)) throw new TypeError('nope'); \
               this.v = v; } \
               var B = Function.prototype.bind.apply(C, [null, 7]); \
               var i = new B(); \
               (i instanceof C ? 100 : 0) + i.v"), 107.0);
        assert_eq!(
            n("function C(a, b) { this.s = a + b; } \
               var B = C.bind(null, 40); (new B(2)).s"), 42.0);
    }

    #[test]
    fn function_bind() {
        assert_eq!(
            n("function f(a, b) { return this.v + a * 10 + b; } \
               var g = f.bind({v: 100}, 2); g(3)"), 123.0);
        // bind of bind stacks partials, this stays from the first bind
        assert_eq!(
            n("function f(a, b) { return this.v + a * 10 + b; } \
               var g = f.bind({v: 5}).bind({v: 9}, 4); g(6)"), 51.0);
        // webpack loader staple: r.bind(null, 0)
        assert_eq!(
            n("function r(x, y) { return x * 10 + y; } \
               var p = r.bind(null, 7); p(3) + p(1) * 100"), 7173.0);
    }

    #[test]
    #[ignore] // diagnostic: run with -- --ignored (needs the saved file)
    fn polyfill_diag() {
        let path = r"C:\Users\ehdgu\AppData\Local\Temp\claude\E--gg\1e9603b2-4741-42c9-807a-54c1120767e2\scratchpad\polyfill_full.js";
        let Ok(src) = std::fs::read_to_string(path) else { return };
        match eval(&src) {
            Ok(_) => println!("polyfill: OK"),
            Err(e) => println!("polyfill: {e}"),
        }
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
    fn string_pad_codepoint_and_parse_int() {
        assert_eq!(n("'5'.padStart(3, '0') === '005' ? 1 : 0"), 1.0);
        assert_eq!(n("'5'.padEnd(3, '.') === '5..' ? 1 : 0"), 1.0);
        assert_eq!(n("'ab'.padStart(1) === 'ab' ? 1 : 0"), 1.0);
        assert_eq!(
            n("String.fromCodePoint(0x1F600).codePointAt(0)"), 0x1F600 as f64);
        // parseInt honours a 0x prefix when no radix (or radix 16) is given
        assert_eq!(n("parseInt('0x1f')"), 31.0);
        assert_eq!(n("parseInt('0xff', 16)"), 255.0);
        assert_eq!(n("parseInt('ff', 16)"), 255.0);
        assert_eq!(n("parseInt('42')"), 42.0);
    }

    #[test]
    fn json_stringify_space_replacer_tojson() {
        // array replacer allow-list
        assert_eq!(
            n("JSON.stringify({a:1,b:2}, ['a']) === '{\"a\":1}' ? 1 : 0"), 1.0);
        // function replacer
        assert_eq!(
            n("JSON.stringify({a:1,b:2}, function(k,v){ \
                 return k==='b' ? undefined : v; }) === '{\"a\":1}' ? 1 : 0"),
            1.0);
        // indentation
        assert_eq!(
            n("JSON.stringify({a:1}, null, 2) === '{\\n  \"a\": 1\\n}' ? 1 : 0"),
            1.0);
        // toJSON hook
        assert_eq!(
            n("JSON.stringify({toJSON:function(){return 'X';}}) === '\"X\"' \
               ? 1 : 0"), 1.0);
        // getters are serialized
        assert_eq!(
            n("JSON.stringify({get x(){return 7;}}) === '{\"x\":7}' ? 1 : 0"),
            1.0);
        // nested indentation + arrays
        assert_eq!(
            n("JSON.stringify([1,2], null, 1) === '[\\n 1,\\n 2\\n]' ? 1 : 0"),
            1.0);
    }

    #[test]
    fn property_attributes_and_extensibility() {
        // freeze blocks writes and reports frozen; seal blocks new props
        assert_eq!(
            n("var o={a:1}; Object.freeze(o); o.a=2; \
               (o.a===1 && Object.isFrozen(o)) ? 1 : 0"), 1.0);
        assert_eq!(
            n("var o={a:1}; Object.seal(o); o.b=2; o.a=5; \
               (o.b===undefined && o.a===5 && Object.isSealed(o)) ? 1 : 0"),
            1.0);
        assert_eq!(
            n("var o={}; Object.defineProperty(o,'a',{value:1,writable:false}); \
               o.a=2; o.a"), 1.0);
        // non-enumerable defineProperty hidden from keys; enumerable stays
        assert_eq!(
            n("var o={a:1}; Object.defineProperty(o,'b',{value:2,enumerable:false}); \
               Object.keys(o).join()==='a' ? 1 : 0"), 1.0);
        // array-index keys enumerate ahead of string keys, ascending
        assert_eq!(
            n("var o={}; o.b=1; o['2']=1; o.a=1; o['1']=1; \
               Object.keys(o).join()==='1,2,b,a' ? 1 : 0"), 1.0);
        // hasOwnProperty sees accessor-only keys
        assert_eq!(
            n("var o={get x(){return 1;}}; \
               (o.hasOwnProperty('x') && !o.hasOwnProperty('y')) ? 1 : 0"), 1.0);
        // object-literal accessors are enumerable (unlike defineProperty)
        assert_eq!(
            n("Object.keys({get x(){return 1;}}).join()==='x' ? 1 : 0"), 1.0);
        // Object.assign reads source getters
        assert_eq!(
            n("Object.assign({}, {get x(){return 7;}}).x"), 7.0);
    }

    #[test]
    fn array_length_from_and_own_property() {
        // setting length shorter drops the tail
        assert_eq!(
            n("var a=[1,2,3,4]; a.length=2; \
               (a.join()==='1,2' && a[2]===undefined) ? 1 : 0"), 1.0);
        // Array.from over an array-like, a Set, and with a map fn
        assert_eq!(
            n("Array.from({length:3,0:'x',1:'y',2:'z'}).join()==='x,y,z' ? 1 : 0"),
            1.0);
        assert_eq!(n("Array.from(new Set([1,1,2,3,3])).length"), 3.0);
        assert_eq!(
            n("Array.from([1,2,3], function(x){return x*2;}).join()==='2,4,6' \
               ? 1 : 0"), 1.0);
        // extracted-builtin hasOwnProperty works on arrays (React uses this)
        assert_eq!(
            n("var a=[9]; (a.hasOwnProperty(0) && !a.hasOwnProperty(5)) ? 1 : 0"),
            1.0);
    }

    #[test]
    fn number_math_object_statics() {
        assert_eq!(n("Number.parseInt('42px')"), 42.0);
        assert_eq!(n("Number.parseFloat('3.14x')"), 3.14);
        assert_eq!(n("Number.MAX_SAFE_INTEGER"), 9_007_199_254_740_991.0);
        assert_eq!(n("Number.isSafeInteger(5) && !Number.isSafeInteger(1.5) \
                      ? 1 : 0"), 1.0);
        assert_eq!(n("Number.EPSILON > 0 ? 1 : 0"), 1.0);
        // Math.sign: 0 stays 0 (not 1)
        assert_eq!(n("(Math.sign(-5)===-1 && Math.sign(0)===0 \
                      && Math.sign(3)===1) ? 1 : 0"), 1.0);
        assert_eq!(n("Object.hasOwn({a:1},'a') && !Object.hasOwn({},'a') \
                      ? 1 : 0"), 1.0);
        // Array.indexOf with a fromIndex
        assert_eq!(n("[1,2,1].indexOf(1,1)"), 2.0);
        // Error cause option
        assert_eq!(n("new Error('x',{cause:5}).cause"), 5.0);
    }

    #[test]
    fn logical_assignment_and_immutable_arrays() {
        assert_eq!(n("var x=0; x||=5; x"), 5.0);
        assert_eq!(n("var x=3; x||=9; x"), 3.0); // truthy: no assign
        assert_eq!(n("var x=1; x&&=7; x"), 7.0);
        assert_eq!(n("var x=null; x??=3; x"), 3.0);
        assert_eq!(n("var y=0; y??=9; y"), 0.0); // 0 is not nullish
        // ES2023 immutable array methods
        assert_eq!(n("var a=[3,1,2]; var b=a.toSorted(); \
                      (b.join()==='1,2,3' && a[0]===3) ? 1 : 0"), 1.0);
        assert_eq!(n("[1,2,3].toReversed().join()==='3,2,1' ? 1 : 0"), 1.0);
        assert_eq!(n("[1,2,3].with(1,9).join()==='1,9,3' ? 1 : 0"), 1.0);
        // btoa/atob round-trip
        assert_eq!(n("atob(btoa('hi'))==='hi' ? 1 : 0"), 1.0);
    }

    #[test]
    fn string_match_all() {
        // matchAll yields one array per match, iterable via spread
        assert_eq!(
            n("var a=[...'a1b2'.matchAll(/(\\w)(\\d)/g)]; \
               (a.length===2 && a[0][1]==='a' && a[1][2]==='2') ? 1 : 0"),
            1.0);
        // each result carries .index and named .groups
        assert_eq!(
            n("var a=[...'x9'.matchAll(/(?<c>\\w)(?<d>\\d)/g)]; \
               (a[0].index===0 && a[0].groups.c==='x' && a[0].groups.d==='9') \
               ? 1 : 0"), 1.0);
        // for-of also works
        assert_eq!(
            n("var n=0; for (var m of 'aaa'.matchAll(/a/g)) n++; n"), 3.0);
    }

    #[test]
    fn regex_named_replacement() {
        // $<name> in a replacement string refers to a named group
        assert_eq!(
            n("'2020'.replace(/(?<y>\\d{4})/,'$<y>!')==='2020!' ? 1 : 0"), 1.0);
        // $& (whole match) still works alongside
        assert_eq!(
            n("'abc'.replace(/b/,'[$&]')==='a[b]c' ? 1 : 0"), 1.0);
        // named replacement over a global regex
        assert_eq!(
            n("'a1b2'.replace(/(?<c>[a-z])(?<n>\\d)/g,'$<n>$<c>')==='1a2b' \
               ? 1 : 0"), 1.0);
    }

    #[test]
    fn regex_named_groups() {
        assert_eq!(
            n("var m='2021-05'.match(/(?<y>\\d+)-(?<mo>\\d+)/); \
               (m.groups.y==='2021' && m.groups.mo==='05') ? 1 : 0"), 1.0);
        // exec exposes .groups too
        assert_eq!(
            n("var m=/(?<a>\\w)(?<b>\\w)/.exec('xy'); \
               (m.groups.a==='x' && m.groups.b==='y') ? 1 : 0"), 1.0);
        // no named groups -> groups is undefined
        assert_eq!(
            n("'ab'.match(/(\\w)/).groups === undefined ? 1 : 0"), 1.0);
    }

    #[test]
    fn promise_finally_forwards() {
        // .finally() is callable and chains (value-forwarding is verified
        // end-to-end through the page event loop)
        assert_eq!(
            n("Promise.resolve(1).finally(function(){}) \
                 .finally(function(){}); 5"), 5.0);
        assert_eq!(
            n("Promise.reject('e').finally(function(){}).catch(function(){}); \
               5"), 5.0);
    }

    #[test]
    fn computed_destructuring() {
        assert_eq!(
            n("var k='x'; var {[k]:v}={x:9}; v"), 9.0);
        assert_eq!(
            n("var k='a'; var {[k]:v=7}={}; v"), 7.0);
        assert_eq!(
            n("var {['a'+'b']:v}={ab:3}; v"), 3.0);
    }

    #[test]
    fn more_array_and_string_builtins() {
        // Array statics/methods
        assert_eq!(n("Array.of(1,2,3).join()==='1,2,3' ? 1 : 0"), 1.0);
        assert_eq!(n("[1,2,3].fill(0,1).join()==='1,0,0' ? 1 : 0"), 1.0);
        assert_eq!(
            n("['a','b','c'].reduceRight(function(a,b){return a+b;}) === 'cba' \
               ? 1 : 0"), 1.0);
        assert_eq!(
            n("[1,2,3,4,5].copyWithin(0,3).join()==='4,5,3,4,5' ? 1 : 0"), 1.0);
        assert_eq!(n("[NaN].includes(NaN) ? 1 : 0"), 1.0);
        // string split with a limit; replaceAll with a global regex
        assert_eq!(n("'a,b,c,d'.split(',',2).join()==='a,b' ? 1 : 0"), 1.0);
        assert_eq!(n("'a1b2'.replaceAll(/\\d/g,'X')==='aXbX' ? 1 : 0"), 1.0);
        // numeric separators
        assert_eq!(n("1_000_000"), 1000000.0);
        assert_eq!(n("0xFF_FF"), 65535.0);
        // toLocaleString returns a string
        assert_eq!(n("typeof (1234).toLocaleString()==='string' ? 1 : 0"), 1.0);
        // getOwnPropertyDescriptors
        assert_eq!(
            n("var d=Object.getOwnPropertyDescriptors({a:1}); \
               (d.a.value===1 && d.a.enumerable===true) ? 1 : 0"), 1.0);
    }

    #[test]
    fn tagged_templates() {
        // tag receives (cooked-strings, ...substitutions)
        assert_eq!(
            n("function t(s, v){ return s[0] + v + s[1]; } \
               t`a${9}b` === 'a9b' ? 1 : 0"), 1.0);
        // multiple substitutions and the strings array length
        assert_eq!(
            n("function t(s){ return s.length; } t`${1}${2}${3}`"), 4.0);
        // the strings array carries a `.raw`
        assert_eq!(
            n("function t(s){ return s.raw[0]; } t`hi${1}` === 'hi' ? 1 : 0"),
            1.0);
        // String.raw builtin pattern (cooked === raw here)
        assert_eq!(
            n("function t(s,a){ return s[0]+a+s[1]; } t`x${5}y` === 'x5y' \
               ? 1 : 0"), 1.0);
    }

    #[test]
    fn iterable_spread_and_from() {
        // spreading a generator expands element-wise (not [object Object])
        assert_eq!(
            n("function* g(){yield 1;yield 2;} \
               [...g()].join()==='1,2' ? 1 : 0"), 1.0);
        // spreading a Set and a string
        assert_eq!(
            n("[...new Set([1,1,2])].join()==='1,2' ? 1 : 0"), 1.0);
        assert_eq!(n("[...'abc'].length"), 3.0);
        // Array.from over a generator drains the iterator
        assert_eq!(
            n("function* g(){yield 5;yield 6;} \
               Array.from(g()).join()==='5,6' ? 1 : 0"), 1.0);
        // spread into a call argument list
        assert_eq!(
            n("function* g(){yield 1;yield 2;yield 3;} \
               Math.max(...g())"), 3.0);
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
    fn rest_params_and_object_spread() {
        // rest parameters (desugared to arguments.slice(n))
        assert_eq!(
            n("function f(a, ...r) { return a + r.length } f(1, 2, 3, 4)"),
            4.0);
        assert_eq!(n("function f(...r) { return r[0] + r[2] } f(5, 6, 7)"),
                   12.0);
        assert_eq!(n("var f = (...xs) => xs.length; f()"), 0.0);
        // object literal spread (desugared to Object.assign)
        assert_eq!(
            n("var o = {a: 1, ...{b: 2, c: 3}, d: 4}; o.a + o.b + o.c + o.d"),
            10.0);
        assert_eq!(n("var s = {a: 5}; var o = {a: 1, ...s}; o.a"), 5.0);
        assert_eq!(n("var o = {...{x: 2}}; o.x"), 2.0);
        // object rest pattern: rest gets the unbound keys only
        assert_eq!(
            n("var {a, ...rest} = {a: 1, b: 2, c: 3}; \
               a + rest.b + rest.c + (rest.a === undefined ? 100 : 0)"),
            106.0);
        // RegExp constructor builds a working regex (new and plain call)
        assert_eq!(n("new RegExp('a+', 'i').test('bAAb') ? 1 : 0"), 1.0);
        assert_eq!(n("RegExp('\\\\d{2}').test('x42') ? 1 : 0"), 1.0);
    }

    #[test]
    fn engine_errors_are_catchable_typeerrors() {
        // unresolved global: a catchable ReferenceError
        assert_eq!(
            n("var r = 0; try { totallyMissing(); } catch (e) { \
               r = (e instanceof ReferenceError ? 1 : 0) \
                 + (e instanceof Error ? 2 : 0); } r"),
            3.0);
        // `var u;` (no init) declares the global: reads are undefined
        assert_eq!(n("var u; u === undefined ? 1 : 0"), 1.0);
        // member read on undefined: caught, instanceof TypeError+Error
        assert_eq!(
            n("var r = 0; try { var u; u.x; } catch (e) { \
               r = (e instanceof TypeError ? 1 : 0) \
                 + (e instanceof Error ? 2 : 0) \
                 + (e.name === 'TypeError' ? 4 : 0); } r"),
            7.0);
        // calling a missing method
        assert_eq!(
            n("var r = 0; try { ({}).nope(); } \
               catch (e) { r = e instanceof TypeError ? 1 : 0; } r"),
            1.0);
        // method call on null
        assert_eq!(
            n("var r = 0; try { null.foo(); } \
               catch (e) { r = e.name === 'TypeError' ? 1 : 0; } r"),
            1.0);
        // calling a non-function value
        assert_eq!(
            n("var r = 0; try { var x = 5; x(); } \
               catch (e) { r = e instanceof TypeError ? 1 : 0; } r"),
            1.0);
        // toString inherited from the Error prelude chain
        assert_eq!(
            n("var r = ''; try { var u; u.x; } \
               catch (e) { r = e.toString(); } \
               r.indexOf('TypeError') === 0 ? 1 : 0"),
            1.0);
    }

    #[test]
    fn js_semantics_instead_of_bails() {
        // null/bool keys stringify like real JS
        assert_eq!(n("var o = {}; o[null] = 5; o['null']"), 5.0);
        assert_eq!(n("var o = {}; o[true] = 3; o[true]"), 3.0);
        // negative / fractional indices are named properties
        assert_eq!(n("var a = [1]; a[-1] = 7; a[-1] + a.length"), 8.0);
        assert_eq!(n("var a = []; a[0.5] = 3; a[0.5]"), 3.0);
        assert_eq!(
            n("var a = [1, 2]; a[-1] === undefined ? 1 : 0"), 1.0);
        // string propertyIsEnumerable / hasOwnProperty
        assert_eq!(n("'ab'.propertyIsEnumerable(0) ? 1 : 0"), 1.0);
        assert_eq!(n("'ab'.propertyIsEnumerable(5) ? 1 : 0"), 0.0);
        assert_eq!(n("'ab'.hasOwnProperty('length') ? 1 : 0"), 1.0);
        // object propertyIsEnumerable via the universal fallback
        assert_eq!(
            n("var o = {a: 1}; (o.propertyIsEnumerable('a') ? 1 : 0) \
               + (o.propertyIsEnumerable('b') ? 10 : 0)"),
            1.0);
        // primitive indexing yields undefined, not an engine bail
        assert_eq!(n("var n = 0; (n[1] === undefined) ? 1 : 0"), 1.0);
        // new with spread args (Babel _construct shape)
        assert_eq!(
            n("function P(a, b) { this.s = a + b; } \
               var args = [3, 4]; new P(...args).s"),
            7.0);
        // string/number/computed-keyed method shorthand
        assert_eq!(n("var o = { 'hi'() { return 5; } }; o.hi()"), 5.0);
        assert_eq!(n("var o = { 7() { return 3; } }; o[7]()"), 3.0);
        assert_eq!(
            n("var k = 'm'; var o = { [k]() { return 9; } }; o.m()"),
            9.0);
    }

    #[test]
    fn small_syntax_gaps() {
        // object literal getter/setter shorthand
        assert_eq!(
            n("var o = { _v: 3, get v() { return this._v * 2; }, \
               set v(x) { this._v = x; } }; o.v = 5; o.v"),
            10.0);
        // for-of / for-in with a destructuring head
        assert_eq!(
            n("var s = 0; \
               for (var [a, b] of [[1, 2], [3, 4]]) { s += a * b; } s"),
            14.0);
        assert_eq!(
            n("var ks = []; var o = {x: 1, y: 2}; \
               for (const [k, v] of Object.entries ? [] : []) {} \
               ks.length"),
            0.0);
        // destructuring assignment expressions
        assert_eq!(
            n("var a, b; [a, b] = [7, 8]; a * 10 + b"), 78.0);
        assert_eq!(
            n("var x, y; ({x, y} = {x: 2, y: 3}); x * y"), 6.0);
        assert_eq!(
            n("var p, q; [p, q = 9] = [4]; p + q"), 13.0);
        assert_eq!(
            n("var o = {}; [o.v] = [5]; o.v"), 5.0);
        // Array.prototype.splice: removal, insertion, return value
        assert_eq!(
            n("var a = [1, 2, 3, 4, 5]; var r = a.splice(1, 2, 9); \
               a.join('-') + '|' + r.join('-') === '1-9-4-5|2-3' ? 1 : 0"),
            1.0);
        assert_eq!(
            n("var a = [1, 2, 3]; a.splice(1); a.length"), 1.0);
    }

    #[test]
    fn class_extends_super_static() {
        // super() in the constructor + method inheritance
        assert_eq!(
            n("class A { constructor(x) { this.x = x; } \
                 getX() { return this.x; } } \
               class B extends A { constructor(x) { super(x * 2); } } \
               new B(21).getX()"),
            42.0);
        // super.method() delegation
        assert_eq!(
            n("class A { hi() { return 10; } } \
               class B extends A { hi() { return super.hi() + 5; } } \
               new B().hi()"),
            15.0);
        // default ctor forwards arguments to the parent
        assert_eq!(
            n("class A { constructor(a, b) { this.s = a + b; } } \
               class B extends A {} new B(4, 5).s"),
            9.0);
        // instanceof sees the chain
        assert_eq!(
            n("class A {} class B extends A {} \
               var b = new B(); \
               (b instanceof B ? 1 : 0) + (b instanceof A ? 2 : 0)"),
            3.0);
        // static methods and class fields
        assert_eq!(
            n("class C { static make(v) { return new C(v); } \
                 constructor(v) { this.v = v; } } \
               C.make(7).v"),
            7.0);
        assert_eq!(
            n("class P { count = 3; bump() { return ++this.count; } } \
               new P().bump()"),
            4.0);
        // getter via class accessor syntax
        assert_eq!(
            n("class G { constructor() { this._v = 6; } \
                 get v() { return this._v * 2; } } \
               new G().v"),
            12.0);
        // plain classes keep working (legacy path)
        assert_eq!(
            n("class K { constructor() { this.n = 1; } m() { return 2; } } \
               var k = new K(); k.n + k.m()"),
            3.0);
    }

    #[test]
    fn class_async_methods_parse_and_run() {
        // async instance method with await + a following method (the
        // shape that broke naver's gfp ad SDK: `async m(){...await...}`
        // followed by another method left the class parser desynced)
        assert_eq!(
            n("var log = 0; \
               class C { \
                 async load(v) { var x = await v; return x + 1; } \
                 mark() { log = 5; return log; } } \
               var c = new C(); c.mark()"),
            5.0);
        // the async method actually resolves its awaited value
        assert_eq!(
            n("class C { async load(v) { var x = await v; return x * 2; } } \
               var out = 0; \
               new C().load(21).then(function (r) { out = r; }); out"),
            0.0); // resolves on a later microtask; sync read is still 0
        // static async + a member literally named `async` must not be
        // mistaken for the async-method prefix
        assert_eq!(
            n("class C { static async make() { return 9; } \
                 async() { return 3; } } \
               new C().async()"),
            3.0);
        // async method wedged between plain methods, minified (no spaces)
        assert_eq!(
            n("class C{a(){return 1}async b(t){const{x:e}=await t;return e}c(){return 7}}\
               new C().c()"),
            7.0);
    }

    #[test]
    fn class_computed_and_literal_member_names() {
        // computed method name, evaluated at class-definition time
        assert_eq!(
            n("var m = 'go'; \
               class C { [m]() { return 7; } } \
               new C().go()"),
            7.0);
        // computed field name
        assert_eq!(
            n("var k = 'x' + 1; \
               class C { [k] = 5; } \
               new C().x1"),
            5.0);
        // string- and number-literal member names
        assert_eq!(
            n("class C { 'a b'() { return 2; } 3() { return 4; } } \
               var c = new C(); c['a b']() + c[3]()"),
            6.0);
        // computed static + computed getter
        assert_eq!(
            n("var s = 'make'; var g = 'v'; \
               class C { static [s]() { return 9; } \
                 constructor() { this._v = 6; } \
                 get [g]() { return this._v; } } \
               C.make() + new C().v"),
            15.0);
        // Symbol.iterator as a computed method makes the class iterable
        assert_eq!(
            n("class R { \
                 constructor(n) { this.n = n; } \
                 [Symbol.iterator]() { \
                   var i = 0, n = this.n; \
                   return { next() { \
                     return i < n ? { value: i++, done: false } \
                                  : { value: undefined, done: true }; } }; \
                 } } \
               var sum = 0; for (var x of new R(4)) sum += x; sum"),
            6.0);
    }

    #[test]
    fn generators_state_machine() {
        // basic yields + done protocol
        assert_eq!(
            n("function* g() { yield 1; yield 2; return 3; } \
               var it = g(); \
               var a = it.next(), b = it.next(), c = it.next(), \
                   d = it.next(); \
               (a.value === 1 && !a.done ? 1 : 0) \
               + (b.value === 2 && !b.done ? 10 : 0) \
               + (c.value === 3 && c.done ? 100 : 0) \
               + (d.value === undefined && d.done ? 1000 : 0)"),
            1111.0);
        // locals persist across next() calls; sent values arrive
        assert_eq!(
            n("function* acc(start) { \
                 var total = start; \
                 var x = yield total; \
                 total += x; \
                 var y = yield total; \
                 total += y; \
                 return total; \
               } \
               var it = acc(10); \
               it.next(); it.next(5); it.next(7).value"),
            22.0);
        // generator methods in object literals and classes
        assert_eq!(
            n("var o = { *pair() { yield 'a'; yield 'b'; } }; \
               var it = o.pair(); \
               it.next().value + it.next().value === 'ab' ? 1 : 0"),
            1.0);
        assert_eq!(
            n("class C { *nums() { yield 4; yield 2; } } \
               var it = new C().nums(); \
               it.next().value * 10 + it.next().value"),
            42.0);
        // return() closes the iterator early
        assert_eq!(
            n("function* g() { yield 1; yield 2; } var it = g(); \
               it.next(); it['return'](9); \
               var r = it.next(); \
               (r.done ? 1 : 0) + (r.value === undefined ? 2 : 0)"),
            3.0);
        // yield inside control flow: a clear error, not wrong code
        assert!(eval(
            "function* bad() { while (true) { yield 1; } } bad()")
            .is_err());
    }

    #[test]
    fn for_of_iterator_protocol() {
        // for-of drives a generator
        assert_eq!(
            n("function* g() { yield 1; yield 2; yield 3; } \
               var s = 0; for (var v of g()) { s += v; } s"),
            6.0);
        // for-of over Set (insertion order, deduped)
        assert_eq!(
            n("var s = 0; for (var v of new Set([5, 5, 7])) { s += v; } \
               s"),
            12.0);
        // for-of over Map with a destructuring head
        assert_eq!(
            n("var m = new Map([['a', 1], ['b', 2]]); var s = 0; \
               for (var [k, v] of m) { s += v; } s"),
            3.0);
        // custom iterable via Symbol.iterator
        assert_eq!(
            n("var obj = {}; \
               obj[Symbol.iterator] = function () { \
                 var i = 0; \
                 return { next: function () { \
                   i += 1; \
                   return i <= 3 ? { value: i * 10, done: false } \
                                 : { value: undefined, done: true }; \
                 } }; \
               }; \
               var s = 0; for (var v of obj) { s += v; } s"),
            60.0);
        // arrays and strings keep the fast path
        assert_eq!(
            n("var s = 0; for (var v of [1, 2]) { s += v; } s"), 3.0);
    }

    #[test]
    fn symbol_map_set() {
        // Symbol: unique values, stable well-knowns, for/keyFor
        assert_eq!(n("Symbol('a') === Symbol('a') ? 0 : 1"), 1.0);
        assert_eq!(n("Symbol.iterator === Symbol.iterator ? 1 : 0"), 1.0);
        assert_eq!(
            n("Symbol.keyFor(Symbol.for('x')) === 'x' ? 1 : 0"), 1.0);
        // Map: set-chaining, get/has/size, seed from pairs, delete
        assert_eq!(
            n("var m = new Map(); m.set('a', 1).set('b', 2); \
               m.get('a') + m.size + (m.has('c') ? 0 : 10)"),
            13.0);
        assert_eq!(
            n("var m = new Map([['k', 5]]); m.delete('k'); \
               m.size + (m.get('k') === undefined ? 1 : 0)"),
            1.0);
        // object keys compare by identity
        assert_eq!(
            n("var o1 = {}, o2 = {}; var m = new Map(); \
               m.set(o1, 'x'); (m.get(o1) === 'x' ? 1 : 0) \
               + (m.get(o2) === undefined ? 2 : 0)"),
            3.0);
        // forEach and for-of over entries()/values() (arrays)
        assert_eq!(
            n("var m = new Map([['a', 1], ['b', 2]]); var s = 0; \
               m.forEach(function (v, k) { s += v; }); s"),
            3.0);
        assert_eq!(
            n("var m = new Map([['a', 1], ['b', 2]]); var s = 0; \
               for (var p of m.entries()) { s += p[1]; } s"),
            3.0);
        // Set: dedup on seed and add, chaining, has/delete/clear
        assert_eq!(
            n("var s = new Set([1, 2, 2, 3]); s.add(3).add(4); \
               s.size + (s.has(2) ? 10 : 0)"),
            14.0);
        assert_eq!(
            n("var s = new Set(); s.add(1); s.clear(); s.size"), 0.0);
        // WeakMap shares the impl
        assert_eq!(
            n("var o = {}; var wm = new WeakMap(); wm.set(o, 7); \
               wm.get(o)"),
            7.0);
    }

    #[test]
    fn m3_browser_objects() {
        // location fields fill from set_page_url
        let mut vm = PageVm::new(None);
        vm.set_page_url(
            "https://www.naver.com:8080/path/x?q=1#frag");
        let logs = vm.run_scripts(&["\
            console.log(location.hostname);\n\
            console.log(location.pathname);\n\
            console.log(location.search);\n\
            console.log(location.hash);\n\
            console.log(location.port);\n\
            console.log(window.location.origin);\n"
            .to_string()]);
        assert_eq!(logs, vec![
            "www.naver.com", "/path/x", "?q=1", "#frag", "8080",
            "https://www.naver.com:8080",
        ]);
        // navigator / performance / history / screen exist
        assert_eq!(
            n("(navigator.userAgent.indexOf('GGBrowser') >= 0 ? 1 : 0) \
               + (typeof performance.now() === 'number' ? 2 : 0) \
               + (history.length === 1 ? 4 : 0) \
               + (screen.width === 1280 ? 8 : 0)"),
            15.0);
        // document.cookie round-trip (needs a document)
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<p>x</p>"),
        ))));
        let logs = vm.run_scripts(&["\
            document.cookie = 'a=1';\n\
            document.cookie = 'b=2; Path=/';\n\
            document.cookie = 'a=3';\n\
            console.log(document.cookie);\n"
            .to_string()]);
        assert_eq!(logs, vec!["a=3; b=2"]);
        // requestAnimationFrame fires via the pump with a timestamp
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            requestAnimationFrame(function (ts) { \
              console.log('raf ' + (typeof ts)); });\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert!(logs.contains(&"raf number".to_string()), "{logs:?}");
    }

    #[test]
    fn async_switch_lowers_to_if_chain() {
        // awaits inside switch cases (naver's gfp SDK adapter dispatch)
        // compile via the switch->if-chain lowering. Verify VALUES, not
        // just the absence of logs — an unhandled rejection is silent.
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            var out = [];\n\
            function P(v){ return Promise.resolve(v); }\n\
            (async function(){ var r=0; switch(2){\n\
               case 1: { r = await P(10); break; }\n\
               case 2: { r = await P(20); break; }\n\
               default: { r = await P(30); } } return r; })()\n\
              .then(function(v){ out.push('basic=' + v); },\n\
                    function(e){ out.push('basic-REJ ' + e); });\n\
            (async function(){ var r=0; switch(9){\n\
               case 1: { r = 1; break; }\n\
               default: { r = await P(42); } } return r; })()\n\
              .then(function(v){ out.push('dflt=' + v); },\n\
                    function(e){ out.push('dflt-REJ ' + e); });\n\
            (async function(){ var r=0; switch(2){\n\
               case 1: case 2: { r = await P(7); break; }\n\
               case 3: { r = 3; break; } } return r; })()\n\
              .then(function(v){ out.push('share=' + v); },\n\
                    function(e){ out.push('share-REJ ' + e); });\n"
            .to_string()]);
        let (_l1, _) = vm.pump();
        let (_l2, _) = vm.pump();
        let logs = vm.run_scripts(&[
            "console.log(out.sort().join('|'));\n".to_string(),
        ]);
        let joined = format!("{logs:?}");
        assert!(
            joined.contains("basic=20")
                && joined.contains("dflt=42")
                && joined.contains("share=7"),
            "async switch results wrong: {joined}"
        );
    }

    #[test]
    fn promise_adopts_foreign_thenables() {
        // Fulfilling with a `{then}` object must adopt its eventual
        // value (Promise Resolution Procedure), not pass the thenable
        // itself through to reactions. Polyfilled promises (core-js on
        // naver) and async-over-axios flows depend on this: handing the
        // foreign promise object onward reads as `response === undefined`.
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            var out = 'none';\n\
            Promise.resolve({ then: function (res) { res({ z: 9 }); } })\n\
              .then(function (v) { out = 'z=' + (v && v.z); });\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert!(logs.is_empty(), "{logs:?}");
        let mut vm2 = PageVm::new(None);
        let logs2 = vm2.run_scripts(&["\
            var seen = [];\n\
            // sync-resolving thenable: object value must survive\n\
            Promise.resolve({ then: function (res) { res({ z: 9 }); } })\n\
              .then(function (v) { seen.push('sync:' + v.z); });\n\
            // async-resolving thenable via timer\n\
            Promise.resolve({ then: function (res) {\n\
              setTimeout(function () { res({ w: 7 }); }, 0); } })\n\
              .then(function (v) { seen.push('async:' + v.w); });\n\
            // rejecting thenable routes to onRejected\n\
            Promise.resolve({ then: function (_res, rej) { rej('nope'); } })\n\
              .then(function () { seen.push('BAD'); },\n\
                    function (e) { seen.push('rej:' + e); });\n"
            .to_string()]);
        assert!(logs2.is_empty(), "{logs2:?}");
        let (_logs, _) = vm2.pump();
        let (_logs, _) = vm2.pump();
        vm2.run_scripts(&[
            "console.log(seen.sort().join('|'));\n".to_string(),
        ]);
        let (logs3, _) = vm2.pump();
        let all: Vec<String> = vm2.run_scripts(&[
            "console.log('FINAL ' + seen.sort().join('|'));\n".to_string(),
        ]);
        let joined = format!("{logs3:?} {all:?}");
        assert!(
            joined.contains("async:7") && joined.contains("sync:9")
                && joined.contains("rej:nope"),
            "thenable adoption results missing: {joined}"
        );
    }

    #[test]
    fn await_of_async_function_expression() {
        // `await async function(){...}()` — an async IIFE in operand
        // position. The async-fn EXPRESSION form must parse anywhere an
        // expression can, not just at assignment level (naver's gfp SDK
        // does `const e = await async function(){ ... await ... }()`).
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            var out = 0;\n\
            (async function(){\n\
              var e = await async function(){\n\
                var t = await Promise.resolve(3); return t * 2; }();\n\
              return e; })().then(function(v){ out = v; });\n"
            .to_string()]);
        let (_l, _) = vm.pump();
        let (_l, _) = vm.pump();
        let logs = vm.run_scripts(&[
            "console.log('out=' + out);\n".to_string(),
        ]);
        assert!(
            format!("{logs:?}").contains("out=6"),
            "await of async fn expr wrong: {logs:?}"
        );
        // typeof of an async fn expression (feature-detection idiom)
        assert_eq!(
            n("typeof async function(){} === 'function' ? 1 : 0"),
            1.0,
        );
    }

    #[test]
    fn babel_es5_class_inheritance() {
        // the exact shapes @babel/preset-env emits for `class B
        // extends A` — naver's bundles boot through this
        assert_eq!(
            n(r#"
function _classCallCheck(i, C) {
  if (!(i instanceof C)) {
    throw new TypeError('Cannot call a class as a function');
  }
}
function _inherits(sub, sup) {
  sub.prototype = Object.create(sup && sup.prototype, {
    constructor: { value: sub, writable: true, configurable: true }
  });
  if (sup) Object.setPrototypeOf(sub, sup);
}
function _getPrototypeOf(o) {
  return Object.getPrototypeOf ? Object.getPrototypeOf(o) : o.__proto__;
}
function _possibleConstructorReturn(self, call) {
  if (call && (typeof call === 'object' || typeof call === 'function')) {
    return call;
  }
  return self;
}
function _createSuper(D) {
  return function () {
    var Super = _getPrototypeOf(D);
    var result = Super.apply(this, arguments);
    return _possibleConstructorReturn(this, result);
  };
}
var A = function A(v) {
  _classCallCheck(this, A);
  this.v = v;
};
A.prototype.get = function () { return this.v; };
A.mk = function () { return 'static'; };
var B = (function (_A) {
  _inherits(B, _A);
  var _super = _createSuper(B);
  function B(v) {
    _classCallCheck(this, B);
    return _super.call(this, v * 2);
  }
  return B;
})(A);
var b = new B(21);
(b.get() === 42 ? 1 : 0)
  + (b instanceof B ? 2 : 0)
  + (b instanceof A ? 4 : 0)
  + (B.mk() === 'static' ? 8 : 0)
"#),
            15.0);
    }

    #[test]
    fn zz_uncurry_probe() {
        // core-js uncurryThis, NATIVE_BIND path
        eprintln!("A: {:?}", eval(r#"
var i = Function.prototype, o = i.call;
var s = i.bind.bind(o, o);
var c = s(Set.prototype.keys);
var it = c(new Set([1,2]));
console.log('A typeof it: ' + typeof it);
console.log('A next: ' + typeof it.next);
"#).map(|(_, l)| l));
        // fallback path
        eprintln!("B: {:?}", eval(r#"
var o = Function.prototype.call;
var c = function (t) {
  return function () { return o.apply(t, arguments); };
}(Set.prototype.keys);
var it = c(new Set([1,2]));
console.log('B typeof it: ' + typeof it);
"#).map(|(_, l)| l));
    }

    #[test]
    fn date_class() {
        // 16e11 ms = 2020-09-13 (the exact probe naver's polyfill runs)
        assert_eq!(
            n("var d = new Date(16e11); \
               (d.getFullYear() === 2020 ? 1 : 0) \
               + (d.getYear() === 120 ? 2 : 0) \
               + (d.getMonth() === 8 ? 4 : 0) \
               + (d.getDate() === 13 ? 8 : 0)"),
            15.0);
        assert_eq!(
            n("Date.parse('2026-07-17 12:30:05') \
               === Date.UTC(2026, 6, 17, 12, 30, 5) ? 1 : 0"),
            1.0);
        assert_eq!(
            n("new Date(0).toISOString() \
               === '1970-01-01T00:00:00.000Z' ? 1 : 0"),
            1.0);
        assert_eq!(n("new Date(86400000 * 3).getDay()"), 0.0); // Sun
        assert_eq!(n("typeof Date.now() === 'number' ? 1 : 0"), 1.0);
        assert_eq!(
            n("new Date(2026, 0, 2).getFullYear() === 2026 ? 1 : 0"),
            1.0);
    }

    #[test]
    fn platform_stub_layer() {
        // observers register; IntersectionObserver reports visible
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            new MutationObserver(function () {}).observe();\n\
            new IntersectionObserver(function (es) {\n\
              console.log('io ' + es[0].isIntersecting);\n\
            }).observe({});\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert!(logs.contains(&"io true".to_string()), "{logs:?}");
        // URL/URLSearchParams parse
        assert_eq!(
            n("var u = new URL('https://a.b:8000/p/q?x=1&y=2#f'); \
               (u.hostname === 'a.b' ? 1 : 0) \
               + (u.pathname === '/p/q' ? 2 : 0) \
               + (u.searchParams.get('y') === '2' ? 4 : 0) \
               + (u.hash === '#f' ? 8 : 0)"),
            15.0);
        assert_eq!(
            n("new URL('/z', 'https://a.b/c/d').pathname === '/z' \
               ? 1 : 0"),
            1.0);
        // matchMedia and getComputedStyle don't throw
        assert_eq!(
            n("matchMedia('(min-width: 0px)').matches === false \
               ? 1 : 0"),
            1.0);
    }

    #[test]
    #[ignore] // profiling harness, not a correctness test — run with:
              // cargo test --release -- --ignored profile_phases --nocapture
    fn profile_phases() {
        use super::super::{compiler, lexer, parser, vm};
        use super::super::value::Value;
        use std::time::Instant;
        // webpack-shaped synthetic bundle: many module functions, few
        // ever called (naver: 4 bundles, 1.3MB, most modules cold)
        let mut src = String::new();
        src.push_str("var mods = {};\n");
        for i in 0..1500 {
            src.push_str(&format!(
                "mods[{i}] = function (exports) {{\n\
                   var state = {{n: {i}, list: []}};\n\
                   function step(k) {{\n\
                     var acc = 0;\n\
                     for (var j = 0; j < k; j++) \
                       {{ acc += j * state.n; }}\n\
                     return acc;\n\
                   }}\n\
                   function push(v) {{ state.list.push(v); \
                     return state.list.length; }}\n\
                   var helper = function (a, b) \
                     {{ return a < b ? a : b; }};\n\
                   exports.run = function (k) {{\n\
                     var s = step(k) + helper(k, {i});\n\
                     push(s);\n\
                     return s;\n\
                   }};\n\
                   exports.tag = 'm{i}';\n\
                   return exports;\n\
                 }};\n"));
        }
        src.push_str(
            "var total = 0;\n\
             for (var i = 0; i < 1500; i += 20) {\n\
               var e = mods[i]({});\n\
               total += e.run(50);\n\
             }\n\
             total");
        println!("source:  {} KB", src.len() / 1024);
        let t0 = Instant::now();
        let toks = lexer::tokenize(&src).unwrap();
        println!("lex:     {:>7.2} ms ({} tokens)",
                 t0.elapsed().as_secs_f64() * 1e3, toks.len());
        let t1 = Instant::now();
        let ast = parser::parse_program(&src).unwrap();
        println!("parse:   {:>7.2} ms (incl. its own lex)",
                 t1.elapsed().as_secs_f64() * 1e3);
        let t2 = Instant::now();
        let module = compiler::compile(&ast).unwrap();
        println!("compile: {:>7.2} ms ({} protos)",
                 t2.elapsed().as_secs_f64() * 1e3,
                 module.protos.len());
        let mut pvm = PageVm::new(None);
        let mi = pvm.load(module);
        let m = pvm.mods.rc(mi);
        let main = m.module.main;
        let nregs = m.module.protos[main as usize].nregs as usize;
        drop(m);
        let base = pvm.st.regs.len();
        pvm.st.regs.resize(base + nregs, Value::UNDEFINED);
        pvm.st.fuel = vm::DEFAULT_FUEL;
        let t3 = Instant::now();
        let v = vm::exec(&mut pvm.st, &pvm.mods, mi, main, base,
                         u32::MAX, Value::UNDEFINED, 0)
            .unwrap();
        println!("exec:    {:>7.2} ms (cold: includes lazy compiles)",
                 t3.elapsed().as_secs_f64() * 1e3);
        assert!(v.is_number());
        // warm pass: same script, every called body already compiled —
        // isolates pure interpreter time from lazy-compile time
        pvm.st.fuel = vm::DEFAULT_FUEL;
        let t4 = Instant::now();
        let v2 = vm::exec(&mut pvm.st, &pvm.mods, mi, main, base,
                          u32::MAX, Value::UNDEFINED, 0)
            .unwrap();
        println!("exec:    {:>7.2} ms (warm)",
                 t4.elapsed().as_secs_f64() * 1e3);
        assert!(v2.is_number());
    }

    #[test]
    fn canvas_2d_stub_context() {
        // drawing calls are swallowed, readbacks return zeros, and
        // unsupported context kinds answer null (feature detection)
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<canvas id=c width=300></canvas>"),
        ))));
        let logs = vm.run_scripts(&["\
            var el = document.getElementById('c');\n\
            var ctx = el.getContext('2d');\n\
            ctx.fillStyle = '#fff';\n\
            ctx.fillRect(0, 0, 10, 10);\n\
            ctx.beginPath(); ctx.moveTo(0, 0); ctx.lineTo(5, 5);\n\
            ctx.stroke();\n\
            var g = ctx.createLinearGradient(0, 0, 1, 1);\n\
            g.addColorStop(0, 'red');\n\
            console.log('w ' + ctx.measureText('hi').width);\n\
            console.log('img ' + ctx.getImageData(0, 0, 1, 1).data.length);\n\
            console.log('gl ' + (el.getContext('webgl') === null));\n\
            console.log('cv ' + (ctx.canvas === el));\n\
            console.log('url ' + el.toDataURL());\n"
            .to_string()]);
        assert!(logs.contains(&"w 0".to_string()), "{logs:?}");
        assert!(logs.contains(&"img 0".to_string()), "{logs:?}");
        assert!(logs.contains(&"gl true".to_string()), "{logs:?}");
        assert!(logs.contains(&"cv true".to_string()), "{logs:?}");
        assert!(logs.contains(&"url data:,".to_string()), "{logs:?}");
    }

    #[test]
    fn document_surface_for_jquery() {
        // implementation/documentElement/ownerDocument: the probes
        // jQuery+Sizzle run before touching the selector engine
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<html><body><p id=t>x</p></body></html>"),
        ))));
        let logs = vm.run_scripts(&["\
            var d = document.implementation.createHTMLDocument('');\n\
            var el = d.createElement('div');\n\
            console.log('ce ' + el.tagName);\n\
            console.log('de ' + document.documentElement.nodeType);\n\
            var p = document.getElementById('t');\n\
            console.log('od ' + (p.ownerDocument === document));\n\
            console.log('odn ' + (document.ownerDocument === null));\n\
            console.log('nt ' + document.nodeType);\n\
            console.log('dv ' + (document.defaultView === window));\n"
            .to_string()]);
        assert!(logs.contains(&"ce DIV".to_string()), "{logs:?}");
        assert!(logs.contains(&"de 1".to_string()), "{logs:?}");
        assert!(logs.contains(&"od true".to_string()), "{logs:?}");
        assert!(logs.contains(&"odn true".to_string()), "{logs:?}");
        assert!(logs.contains(&"nt 9".to_string()), "{logs:?}");
        assert!(logs.contains(&"dv true".to_string()), "{logs:?}");
    }

    #[test]
    fn dom_method_extraction_and_legacy_events() {
        // jQuery 1.x probes `if (document.addEventListener)` as a
        // property read, and falls back to attachEvent("onX") on IE
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<html><body><p id=t>x</p></body></html>"),
        ))));
        let logs = vm.run_scripts(&["\
            console.log('ael ' + (document.addEventListener ? 1 : 0));\n\
            var probe = document.addEventListener ? 'modern' : 'legacy';\n\
            console.log('branch ' + probe);\n\
            var hits = 0;\n\
            document.attachEvent('onclick', function() { hits++; });\n\
            var ce = document.createElement;\n\
            var el = ce.call(document, 'div');\n\
            console.log('ext ' + el.tagName);\n\
            var p = document.getElementById('t');\n\
            var ga = p.getAttribute;\n\
            console.log('ga ' + ga.call(p, 'id'));\n"
            .to_string()]);
        assert!(logs.contains(&"ael 1".to_string()), "{logs:?}");
        assert!(logs.contains(&"branch modern".to_string()), "{logs:?}");
        assert!(logs.contains(&"ext DIV".to_string()), "{logs:?}");
        assert!(logs.contains(&"ga t".to_string()), "{logs:?}");
        assert!(
            !logs.iter().any(|l| l.contains("error")),
            "{logs:?}"
        );
    }

    #[test]
    fn array_like_uncurried_methods() {
        // jQuery merge: push.call(arrayLike, el) grows elems + length
        assert_eq!(
            n("var push = [].push; var jq = { length: 0 }; \
               push.call(jq, 'a'); push.call(jq, 'b'); \
               jq.length * 10 + ([].indexOf.call(jq, 'b'))"),
            21.0);
        // slice materializes a real array; sort answers the receiver
        assert_eq!(
            n("var jq = { 0: 3, 1: 1, 2: 2, length: 3 }; \
               var arr = [].slice.call(jq); \
               var back = [].sort.call(jq); \
               arr.length * 100 + jq[0] * 10 + (back === jq ? 1 : 0)"),
            311.0);
    }

    #[test]
    fn scoped_element_collections() {
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse(
                "<html><body><div id=a><p class='x y'>1</p><p>2</p>\
                 <span class=x>3</span></div><p class=x>out</p>\
                 </body></html>",
            ),
        ))));
        let logs = vm.run_scripts(&["\
            var a = document.getElementById('a');\n\
            console.log('tag ' + a.getElementsByTagName('p').length);\n\
            console.log('all ' + a.getElementsByTagName('*').length);\n\
            console.log('cls ' + a.getElementsByClassName('x').length);\n\
            console.log('doc ' + \
                document.getElementsByClassName('x').length);\n\
            console.log('qsa ' + a.querySelectorAll('.x').length);\n\
            var q = a.querySelector('span');\n\
            console.log('qs ' + (q ? q.textContent : 'none'));\n"
            .to_string()]);
        assert!(logs.contains(&"tag 2".to_string()), "{logs:?}");
        assert!(logs.contains(&"all 3".to_string()), "{logs:?}");
        assert!(logs.contains(&"cls 2".to_string()), "{logs:?}");
        assert!(logs.contains(&"doc 3".to_string()), "{logs:?}");
        assert!(logs.contains(&"qsa 2".to_string()), "{logs:?}");
        assert!(logs.contains(&"qs 3".to_string()), "{logs:?}");
    }

    #[test]
    fn dom_expando_properties() {
        // jQuery's data cache: elem[expando] = id, read back later
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<html><body><p id=t>x</p></body></html>"),
        ))));
        let logs = vm.run_scripts(&["\
            var el = document.getElementById('t');\n\
            var key = 'jQuery360' + Math.floor(Math.random() * 10);\n\
            el[key] = 42;\n\
            console.log('rt ' + el[key]);\n\
            el._cache = { n: 7 };\n\
            console.log('obj ' + el._cache.n);\n\
            document.customFlag = 'y';\n\
            console.log('doc ' + document.customFlag);\n\
            console.log('tc ' + el.textContent);\n"
            .to_string()]);
        assert!(logs.contains(&"rt 42".to_string()), "{logs:?}");
        assert!(logs.contains(&"obj 7".to_string()), "{logs:?}");
        assert!(logs.contains(&"doc y".to_string()), "{logs:?}");
        assert!(logs.contains(&"tc x".to_string()), "{logs:?}");
    }

    #[test]
    fn attributes_named_node_map() {
        // jQuery's event support probe: setAttribute then
        // n.attributes[name].expando === false
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<html><body></body></html>"),
        ))));
        let logs = vm.run_scripts(&["\
            var n = document.createElement('div');\n\
            n.setAttribute('onsubmit', 't');\n\
            var a = n.attributes['onsubmit'];\n\
            console.log('exp ' + (false === a.expando));\n\
            console.log('val ' + a.value);\n\
            console.log('len ' + n.attributes.length);\n\
            console.log('idx ' + n.attributes[0].name);\n\
            console.log('ts ' + n.toString());\n\
            console.log('vo ' + (n.valueOf() === n));\n\
            console.log('frag ' + (document.createDocumentFragment()\
.createElement ? 'ie' : 'modern'));\n\
            's'.isTrigger = 1; console.log('sloppy ok');\n"
            .to_string()]);
        assert!(logs.contains(&"exp true".to_string()), "{logs:?}");
        assert!(logs.contains(&"val t".to_string()), "{logs:?}");
        assert!(logs.contains(&"len 1".to_string()), "{logs:?}");
        assert!(logs.contains(&"idx onsubmit".to_string()), "{logs:?}");
        assert!(
            logs.contains(&"ts [object HTMLElement]".to_string()),
            "{logs:?}"
        );
        assert!(logs.contains(&"vo true".to_string()), "{logs:?}");
        assert!(logs.contains(&"frag modern".to_string()), "{logs:?}");
        assert!(logs.contains(&"sloppy ok".to_string()), "{logs:?}");
    }

    #[test]
    fn in_operator_on_dom_nodes() {
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<html><body><p id=t>x</p></body></html>"),
        ))));
        let logs = vm.run_scripts(&["\
            var de = document.documentElement;\n\
            console.log('touch ' + ('ontouchstart' in de ? 1 : 0));\n\
            console.log('tag ' + ('tagName' in de ? 1 : 0));\n\
            console.log('ael ' + ('addEventListener' in document ? 1 : 0));\n"
            .to_string()]);
        assert!(logs.contains(&"touch 0".to_string()), "{logs:?}");
        assert!(logs.contains(&"tag 1".to_string()), "{logs:?}");
        assert!(logs.contains(&"ael 1".to_string()), "{logs:?}");
    }

    #[test]
    fn document_element_parent_is_document() {
        // documentElement.parentNode is the document (parentElement null).
        // containsDeep walks parentNode to the document — the
        // IntersectionObserver polyfill's _rootContainsTarget needs it, or
        // lazy content never registers as on-screen.
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<html><body><p id=t>x</p></body></html>"),
        ))));
        let logs = vm.run_scripts(&["\
            var de = document.documentElement;\n\
            console.log('pn ' + (de.parentNode === document ? 1 : 0));\n\
            console.log('pe ' + (de.parentElement === null ? 1 : 0));\n\
            var t = document.getElementById('t');\n\
            function contains(root, n){ while(n){ if(n===root) return 1; \
              n = n.parentNode; } return 0; }\n\
            console.log('cd ' + contains(document, t));\n\
            console.log('cw ' + (document.documentElement.clientWidth > 0 \
              ? 1 : 0));\n"
            .to_string()]);
        assert!(logs.contains(&"pn 1".to_string()), "{logs:?}");
        assert!(logs.contains(&"pe 1".to_string()), "{logs:?}");
        assert!(logs.contains(&"cd 1".to_string()), "{logs:?}");
        assert!(logs.contains(&"cw 1".to_string()), "{logs:?}");
    }

    #[test]
    fn array_method_extraction() {
        // uncurried prototype methods (core-js habit) keep working
        assert_eq!(
            n("var s = [].sort; var a = [3, 1, 2]; \
               s.call(a); a[0] * 100 + a[1] * 10 + a[2]"),
            123.0);
        assert_eq!(
            n("var sp = Array.prototype.splice; var a = [1, 2, 3, 4]; \
               var r = sp.call(a, 1, 2, 9); \
               a.length * 100 + a[1] * 10 + r[0]"),
            392.0);
        assert_eq!(
            n("var sh = [].shift, un = [].unshift; var a = [5, 6]; \
               un.call(a, 4); sh.call(a) * 10 + a.length"),
            42.0);
        assert_eq!(
            n("var r = [].reduce; \
               r.call([1, 2, 3], function(x, y) { return x + y; }, 10)"),
            16.0);
        assert_eq!(
            n("var ev = [].every, so = [].some; \
               (ev.call([1, 2], function(x) { return x > 0; }) ? 10 : 0) \
               + (so.call([0, 3], function(x) { return x > 2; }) ? 1 : 0)"),
            11.0);
        assert_eq!(
            n("var rv = [].reverse; var a = [1, 2, 3]; rv.call(a); a[0]"),
            3.0);
    }

    #[test]
    fn array_iterators_honor_this_arg() {
        // forEach/map/filter/some/every/find/findIndex accept a `thisArg`
        // 2nd argument that becomes the callback's `this`. The
        // IntersectionObserver polyfill (shipped by Naver) calls
        // `this._observationTargets.forEach(function(){ this._x() }, this)`
        // — without honoring thisArg the callback's `this` is undefined and
        // it throws, aborting React's async work.
        assert_eq!(
            n("var o = {v: 10, sum: 0}; \
               [1, 2, 3].forEach(function(x) { this.sum += x * this.v; }, o); \
               o.sum"), 60.0);
        assert_eq!(
            n("var o = {m: 3}; \
               [1, 2, 3].map(function(x) { return x * this.m; }, o) \
               .reduce(function(a, b) { return a + b; }, 0)"), 18.0);
        assert_eq!(
            n("var o = {lo: 1, hi: 3}; \
               [0, 2, 5].filter(function(x) { \
                 return x >= this.lo && x <= this.hi; }, o).length"), 1.0);
        assert_eq!(
            n("var o = {t: 2}; \
               ([1, 2, 3].some(function(x) { return x === this.t; }, o) \
                ? 1 : 0) + \
               ([1, 2, 3].every(function(x) { return x <= this.t; }, o) \
                ? 0 : 10)"), 11.0);
        assert_eq!(
            n("var o = {want: 7}; \
               [3, 7, 9].find(function(x) { return x === this.want; }, o) \
               * 100 + \
               [3, 7, 9].findIndex(function(x) { \
                 return x === this.want; }, o)"), 701.0);
        // uncurried form (core-js habit) must honor thisArg too
        assert_eq!(
            n("var fe = [].forEach; var o = {n: 0}; \
               fe.call([1, 1, 1], function() { this.n++; }, o); o.n"), 3.0);
    }

    #[test]
    fn lifecycle_events_fire() {
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<p>x</p>"),
        ))));
        let logs = vm.run_scripts(&["\
            console.log('rs:' + document.readyState);\n\
            document.addEventListener('DOMContentLoaded', function (e) {\n\
              console.log('dcl ' + e.type + ' ' + document.readyState);\n\
            });\n\
            window.addEventListener('load', function () {\n\
              console.log('load ' + document.readyState);\n\
            });\n"
            .to_string()]);
        assert_eq!(logs, vec!["rs:loading"]);
        let logs = vm.fire_lifecycle();
        assert_eq!(logs, vec![
            "dcl domcontentloaded interactive",
            "load complete",
        ]);
    }

    #[test]
    fn lifecycle_phases_are_separate_and_idempotent() {
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<p>x</p>"),
        ))));
        vm.run_scripts(&["\
            document.addEventListener('DOMContentLoaded', function () {\n\
              console.log('dcl:' + document.readyState);\n\
            });\n\
            document.onreadystatechange = function () {\n\
              console.log('state:' + document.readyState);\n\
            };\n\
            window.onload = function () {\n\
              console.log('property-load:' + document.readyState);\n\
            };\n\
            window.addEventListener('load', function () {\n\
              console.log('load:' + document.readyState);\n\
            });\n"
            .to_string()]);
        assert_eq!(
            vm.fire_dom_content_loaded(),
            vec!["state:interactive", "dcl:interactive"]
        );
        assert!(vm.fire_dom_content_loaded().is_empty());
        assert_eq!(
            vm.fire_load(),
            vec!["state:complete", "property-load:complete", "load:complete"]
        );
        assert!(vm.fire_load().is_empty());
    }

    #[test]
    fn script_properties_and_property_load_handler_work() {
        let (mut vm, doc) = dom_vm("<html><body></body></html>");
        let logs = vm.run_scripts(&["\
            var s = document.createElement('script');\n\
            console.log('default:' + s.async);\n\
            s.id = 'dynamic-script';\n\
            s.async = false;\n\
            s.text = 'console.log(42)';\n\
            s.onload = function () { console.log('property-load'); };\n\
            s.addEventListener('load', function () {\n\
              console.log('listener-load');\n\
            });\n\
            document.body.appendChild(s);\n\
            console.log('final:' + s.async + ':' + s.text);\n"
            .to_string()]);
        assert_eq!(
            logs,
            vec!["default:true", "final:false:console.log(42)"]
        );
        let script = doc
            .borrow()
            .get_element_by_id("dynamic-script")
            .unwrap();
        assert_eq!(
            vm.dispatch_event(script, "load", false, false, None).0,
            vec!["property-load", "listener-load"]
        );
    }

    #[test]
    fn live_tick_paces_raf() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            var n = 0;\n\
            function frame() { n += 1; console.log('f' + n); \
              if (n < 3) requestAnimationFrame(frame); }\n\
            requestAnimationFrame(frame);\n"
            .to_string()]);
        // 20ms slices fire one 16ms frame each — no fast-forward spin
        let (logs, _) = vm.tick(20.0);
        assert_eq!(logs, vec!["f1"]);
        let (logs, _) = vm.tick(20.0);
        assert_eq!(logs, vec!["f2"]);
        let (logs, _) = vm.tick(1.0); // nothing due in 1ms
        assert!(logs.is_empty(), "{logs:?}");
        let (logs, _) = vm.tick(30.0);
        assert_eq!(logs, vec!["f3"]);
        // generator done: further ticks stay quiet
        let (logs, _) = vm.tick(100.0);
        assert!(logs.is_empty(), "{logs:?}");
    }

    #[test]
    fn settle_pump_limits_self_rescheduling_raf_to_one_frame() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            function frame() {\n\
                console.log('frame');\n\
                requestAnimationFrame(frame);\n\
            }\n\
            requestAnimationFrame(frame);\n"
            .to_string()]);

        let (logs, fetches) = vm.pump();
        assert_eq!(logs, vec!["frame"]);
        assert!(fetches.is_empty());
        assert!(!vm.has_pending_work());

        let (logs, fetches) = vm.pump();
        assert_eq!(logs, vec!["frame"]);
        assert!(fetches.is_empty());
        assert!(!vm.has_pending_work());
    }

    #[test]
    fn live_tick_dom_mutation_and_version() {
        let doc = Rc::new(RefCell::new(crate::html::parse(
            "<div id=t>old</div>",
        )));
        let v0 = doc.borrow().version;
        let mut vm = PageVm::new(Some(doc.clone()));
        vm.run_scripts(&["\
            setTimeout(function () { \
              document.getElementById('t').textContent = 'hi'; \
            }, 50);\n"
            .to_string()]);
        vm.tick(20.0); // 20ms: timer not yet due
        let t = crate::dom_api::find_tag(&doc.borrow(), "div").unwrap();
        assert_eq!(doc.borrow().collect_text(t), "old");
        vm.tick(40.0); // 60ms total: fires
        assert_eq!(doc.borrow().collect_text(t), "hi");
        assert!(doc.borrow().version > v0, "version must bump");
    }

    #[test]
    fn m3_style_dataset_events_tree() {
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse(
                "<div id=a data-user-id='7' style='color: red'>\
                 <p id=b>x</p><p id=c>y</p></div>",
            ),
        ))));
        let logs = vm.run_scripts(&["\
            var a = document.getElementById('a');\n\
            var b = document.getElementById('b');\n\
            var c = document.getElementById('c');\n\
            // style proxy: read, camelCase write, cssText\n\
            console.log(a.style.color);\n\
            a.style.backgroundColor = 'blue';\n\
            console.log(a.getAttribute('style'));\n\
            // dataset\n\
            console.log(a.dataset.userId);\n\
            a.dataset.mode = 'dark';\n\
            console.log(a.getAttribute('data-mode'));\n\
            // tree getters + manipulation\n\
            console.log(a.children.length);\n\
            console.log(b.parentNode === a);\n\
            a.insertBefore(c, b);\n\
            console.log(a.children[0] === c);\n\
            a.removeChild(c);\n\
            console.log(a.children.length);\n\
            var d = a.cloneNode(true);\n\
            console.log(d.children.length);\n\
            console.log(a.contains(b));\n\
            console.log(a.tagName);\n\
            // events: bubble + detail + preventDefault\n\
            var got = '';\n\
            a.addEventListener('ping', function (e) {\n\
              got += 'a:' + e.detail; e.preventDefault();\n\
            });\n\
            b.addEventListener('ping', function (e) {\n\
              got += 'b:' + e.detail + ' ';\n\
            });\n\
            var ok = b.dispatchEvent(new CustomEvent('ping',\n\
              { bubbles: true, cancelable: true, detail: 5 }));\n\
            console.log(got);\n\
            console.log(ok);\n"
            .to_string()]);
        assert_eq!(logs, vec![
            "red",
            "color: red; background-color: blue",
            "7",
            "dark",
            "2", "true", "true", "1", "1", "true", "DIV",
            "b:5 a:5", "false",
        ]);
    }

    #[test]
    fn style_computed_key_writes_reach_the_attribute() {
        // React's setValueForStyles writes styles with a COMPUTED key
        // (style[name] = value); it must land in the style attribute
        // exactly like the constant-key path — naver's widget-board
        // carousel viewport gets its overflow:hidden this way.
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<div id=w></div>"),
        ))));
        let logs = vm.run_scripts(&["\
            var w = document.getElementById('w');\n\
            var name = 'overflow';\n\
            w.style[name] = 'hidden';\n\
            var camel = 'backgroundColor';\n\
            w.style[camel] = 'blue';\n\
            console.log(w.getAttribute('style'));\n\
            console.log(w.style[name]);\n\
            console.log(w.style['background-color'.replace(/-c/, 'C')]);\n\
            var ct = 'cssText';\n\
            w.style[ct] = 'color: red';\n\
            console.log(w.getAttribute('style'));\n"
            .to_string()]);
        assert_eq!(logs, vec![
            "overflow: hidden; background-color: blue",
            "hidden",
            "blue",
            "color: red",
        ]);
    }

    #[test]
    fn classlist_ops() {
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<div id=t class='a b'></div>"),
        ))));
        let logs = vm.run_scripts(&["\
            var el = document.getElementById('t');\n\
            el.classList.add('c');\n\
            el.classList.remove('a');\n\
            console.log(el.className);\n\
            console.log(el.classList.contains('b'));\n\
            console.log(el.classList.toggle('b'));\n\
            console.log(el.classList.toggle('z'));\n\
            console.log(el.className);\n"
            .to_string()]);
        assert_eq!(logs, vec!["b c", "true", "false", "true", "c z"]);
    }

    #[test]
    fn web_storage() {
        assert_eq!(
            n("localStorage.setItem('k', 42); \
               Number(localStorage.getItem('k'))"), 42.0);
        assert_eq!(
            n("localStorage.getItem('nope') === null ? 1 : 0"), 1.0);
        assert_eq!(
            n("localStorage.setItem('a', 'x'); localStorage.removeItem('a'); \
               localStorage.getItem('a') === null ? 1 : 0"), 1.0);
        // local and session are independent stores
        assert_eq!(
            n("localStorage.setItem('s', '1'); \
               sessionStorage.getItem('s') === null ? 1 : 0"), 1.0);
        assert_eq!(
            n("window.localStorage === localStorage ? 1 : 0"), 1.0);
    }

    #[test]
    fn function_ctor_compiles_real_functions() {
        // Function is a real constructor that compiles its body (was a stub
        // returning the global). `Function('return this')` is now a genuine
        // function; core-js/lodash reach the global via window/globalThis
        // first, so this idiom returning undefined under strict `this` is
        // fine — what matters is that Function actually compiles code.
        assert_eq!(n("typeof Function === 'function' ? 1 : 0"), 1.0);
        assert_eq!(n("Function('a','return a+1')(41)"), 42.0);
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
        // non-callable RHS tolerates to false: bundles do
        // `x instanceof MaybeMissingCtor` for feature detection, and
        // throwing there aborts React's mount (07-20). The Babel
        // _classCallCheck case is handled by NFE self-binding instead.
        assert_eq!(n("([] instanceof 5) ? 1 : 0"), 0.0);
        assert_eq!(n("([] instanceof undefined) ? 1 : 0"), 0.0);
    }

    #[test]
    fn naver_boot_gates() {
        // gate 1: a function's [[Prototype]] is Function.prototype,
        // terminating at Object.prototype -> null
        assert_eq!(
            n("var p = Object.getPrototypeOf(function () {}); \
               (p === Function.prototype) ? 1 : 0"),
            1.0
        );
        assert_eq!(
            n("var p = Object.getPrototypeOf(function () {}); \
               var q = Object.getPrototypeOf(p); \
               (Object.getPrototypeOf(q) === null) ? 1 : 0"),
            1.0
        );
        assert_eq!(n("(function(){} instanceof Object) ? 1 : 0"), 1.0);
        assert_eq!(n("(function(){} instanceof Function) ? 1 : 0"), 1.0);
        // gate 2a: instances see Array.prototype expandos
        assert_eq!(
            n("Array.prototype.__xy = 42; var v = [].__xy; \
               delete Array.prototype.__xy; v"),
            42.0
        );
        // gate 2b: defineProperty with an object key (polyfilled
        // Symbol) matches the computed-read path
        assert_eq!(
            n("var sym = { toString: function () { return '@@t'; } }; \
               var o = {}; \
               Object.defineProperty(o, sym, { value: 9 }); \
               o[sym]"),
            9.0
        );
        // gate 2c: the genuine Object.prototype.toString brands
        assert_eq!(
            n("(Object.prototype.toString.call([]) \
                 === '[object Array]') ? 1 : 0"),
            1.0
        );
        assert_eq!(
            n("(({}).toString.call([]) === '[object Array]') ? 1 : 0"),
            1.0
        );
        assert_eq!(
            n("(({}).toString.call(function(){}) \
                 === '[object Function]') ? 1 : 0"),
            1.0
        );
        // ...while a real array's own toString keeps join semantics
        assert_eq!(n("([1,2].toString() === '1,2') ? 1 : 0"), 1.0);
        // gate 3: arbitrary keys on primitives read undefined (jQuery
        // expando feature detection), builtins still extract, and
        // prototype expandos are visible
        assert_eq!(
            n("('ready'['jQuery361001'] === undefined) ? 1 : 0"),
            1.0
        );
        assert_eq!(n("(typeof ''.slice === 'function') ? 1 : 0"), 1.0);
        assert_eq!(n("('x'.constructor === String) ? 1 : 0"), 1.0);
        assert_eq!(n("Number.prototype.__nn = 3; (5).__nn"), 3.0);
        // gate 4: top-level `var X = X || {}` self-reference
        assert_eq!(
            n("var __NBP = __NBP || { ok: 1 }; __NBP.ok"),
            1.0
        );
        // round 2 (07-20): top-level `this` is the window (webpack
        // UMD wrappers pass it as the global)
        assert_eq!(n("(this === window) ? 1 : 0"), 1.0);
        // round 2: [].keys()/values()/entries()/@@iterator as METHOD
        // CALLS (core-js es.array.iterator boots through these)
        assert_eq!(n("[7,8].keys().next().value"), 0.0);
        assert_eq!(n("[7,8].values().next().value"), 7.0);
        assert_eq!(n("[7,8].entries().next().value[1]"), 7.0);
        assert_eq!(n("[7,8]['@@iterator']().next().value"), 7.0);
        // round 2: methods installed on Array.prototype via
        // defineProperty are callable on instances
        assert_eq!(
            n("Object.defineProperty(Array.prototype, '__m', \
               { value: function () { return this.length * 10; } }); \
               var v = [1,2,3].__m(); \
               delete Array.prototype.__m; v"),
            30.0
        );
        // gate 5: named function expressions bind their own name
        // (Babel _classCallCheck pattern boots)
        assert_eq!(
            n("var mk = function t(v) { \
                 if (!(this instanceof t)) { \
                   throw new TypeError('no new'); } \
                 this.v = v; }; \
               (new mk(7)).v"),
            7.0
        );
        assert_eq!(
            n("var r; var f = function t() { return typeof t; }; \
               (f() === 'function') ? 1 : 0"),
            1.0
        );
        // round 2: sloppy-mode arguments.callee (jindo Component.extend
        // in Naver's search bundle saves it for re-invocation)
        assert_eq!(
            n("function f() { return arguments.callee === f ? 1 : 0; } \
               f()"),
            1.0
        );
        // round 3: Object.is SameValue (react bailout/shallowEqual)
        assert_eq!(n("Object.is(3, 3) ? 1 : 0"), 1.0);
        assert_eq!(n("Object.is(NaN, NaN) ? 1 : 0"), 1.0);
        assert_eq!(n("Object.is(-0, 0) ? 1 : 0"), 0.0);
        assert_eq!(n("Object.is(0, 0) ? 1 : 0"), 1.0);
        assert_eq!(n("var o = {}; Object.is(o, o) ? 1 : 0"), 1.0);
        assert_eq!(n("Object.is({}, {}) ? 1 : 0"), 0.0);
        assert_eq!(n("(typeof Object.is === 'function') ? 1 : 0"), 1.0);
        // round 3: Math.clz32 — react's lane iteration
        // (31 - Math.clz32(lanes)) infinite-loops without it
        assert_eq!(n("Math.clz32(1)"), 31.0);
        assert_eq!(n("Math.clz32(2)"), 30.0);
        assert_eq!(n("Math.clz32(0)"), 32.0);
        assert_eq!(n("Math.clz32(1024)"), 21.0);
        assert_eq!(n("31 - Math.clz32(1)"), 0.0);
        assert_eq!(n("(typeof Math.clz32 === 'function') ? 1 : 0"), 1.0);
        // the exact react lane loop terminates now
        assert_eq!(
            n("var lanes = 42, count = 0; \
               while (lanes > 0 && count < 100) { \
                 var i = 31 - Math.clz32(lanes); \
                 lanes = lanes & ~(1 << i); count++; } \
               count"),
            3.0
        );
        // round 4: backreferences (fancy-regex second-chance engine).
        // date-fns's tokenizer /(\w)\1*|./g returned null without this,
        // making the app's `for...of` over the tokens throw and abort
        // React's render.
        // 'yyyy-MM' tokenizes to ["yyyy", "-", "MM"] (3 tokens)
        assert_eq!(
            n("var m = 'yyyy-MM'.match(/(\\w)\\1*|./g); \
               m === null ? -1 : m.length"),
            3.0
        );
        assert_eq!(
            n("/(\\w)\\1+/.test('aabb') ? 1 : 0"),
            1.0
        );
        // lookahead also compiles now (was never-matching)
        assert_eq!(
            n("var r = 'foobar'.match(/foo(?=bar)/); \
               r === null ? -1 : r[0].length"),
            3.0
        );
        // and a plain regex still uses the fast std engine
        assert_eq!(n("'aabb'.match(/\\w/g).length"), 4.0);
        // round 4: Date setters (date-fns builds dates via
        // setUTCFullYear/Month/Date/Hours; missing setters threw
        // '.setUTCFullYear is not a function' and aborted React)
        assert_eq!(
            n("var d = new Date(0); d.setUTCFullYear(2026); \
               d.getUTCFullYear()"),
            2026.0
        );
        assert_eq!(
            n("var d = new Date(Date.UTC(2026, 0, 15)); \
               d.setUTCMonth(6); d.getUTCMonth()"),
            6.0
        );
        assert_eq!(
            n("var d = new Date(0); d.setUTCHours(13, 30); \
               d.getUTCHours() * 100 + d.getUTCMinutes()"),
            1330.0
        );
        assert_eq!(
            n("var d = new Date(Date.UTC(2026, 5, 20)); \
               d.setUTCDate(25); d.getUTCDate()"),
            25.0
        );
        // round 4: fn.toString()/valueOf() (bundles hash / feature-
        // detect functions; missing threw '.toString() on Func')
        assert_eq!(
            n("function f() {} (typeof f.toString() === 'string' \
               && f.toString().indexOf('function') === 0) ? 1 : 0"),
            1.0
        );
        assert_eq!(n("function f() {} f.valueOf() === f ? 1 : 0"), 1.0);
        assert_eq!(
            n("function f() {} f.hasOwnProperty('prototype') ? 1 : 0"),
            1.0
        );
        // round 4: ErrorEvent/PromiseRejectionEvent/MessageEvent globals
        // (error-reporting paths reference them; 'ErrorEvent is not
        // defined' aborted React)
        assert_eq!(
            n("var e = new ErrorEvent('error', { message: 'x', lineno: 3 }); \
               (e.message === 'x' && e.lineno === 3 \
                && e instanceof Event) ? 1 : 0"),
            1.0
        );
        assert_eq!(n("typeof PromiseRejectionEvent === 'function' ? 1 : 0"), 1.0);
        assert_eq!(n("typeof MessageEvent === 'function' ? 1 : 0"), 1.0);
        // round 4: window.dispatchEvent (React reports errors this way)
        assert_eq!(
            n("var hit = 0; \
               window.addEventListener('error', function () { hit = 1; }); \
               window.dispatchEvent(new ErrorEvent('error', {})); hit"),
            1.0
        );
        // EventTarget instances get a working event system
        assert_eq!(
            n("var t = new EventTarget(); var got = 0; \
               t.addEventListener('x', function (e) { got = e.detail; }); \
               t.dispatchEvent(new CustomEvent('x', { detail: 9 })); got"),
            9.0
        );
    }

    #[test]
    fn create_element_ns_for_svg() {
        // React creates every SVG node via createElementNS; a missing
        // impl left the stateNode undefined and the commit phase threw
        // '.classList of undefined'. Namespace is ignored (arg 1 = tag).
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<html><body></body></html>"),
        ))));
        let logs = vm.run_scripts(&["\
            var e = document.createElementNS(\
              'http://www.w3.org/2000/svg', 'svg');\n\
            e.classList.add('ic');\n\
            console.log(e.tagName + ' ' + e.className);\n"
            .to_string()]);
        assert!(
            logs.iter().any(|s| {
                let l = s.to_lowercase();
                l.contains("svg") && l.contains("ic")
            }),
            "createElementNS logs: {logs:?}"
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

    /// A nested pattern may carry its own default, and the `=` shows up
    /// only after the pattern closes — the shape behind the largest
    /// single block of Test262 parse failures.
    #[test]
    fn nested_patterns_take_defaults() {
        // array element that is an array pattern with a default
        assert_eq!(
            n("var [[x, y] = [4, 5]] = []; x * 10 + y"), 45.0);
        assert_eq!(
            n("var [[x, y] = [4, 5]] = [[1, 2]]; x * 10 + y"), 12.0);
        // object property whose value is a pattern with a default
        assert_eq!(
            n("var {p: {q} = {q: 8}} = {}; q"), 8.0);
        assert_eq!(
            n("var {p: {q} = {q: 8}} = {p: {q: 3}}; q"), 3.0);
        // computed key with a nested defaulted pattern
        assert_eq!(
            n("var k = 'p'; var {[k]: [a] = [6]} = {}; a"), 6.0);
        // three deep, defaults at every level (property-list-with-
        // property-list.js in Test262 is exactly this shape)
        assert_eq!(
            n("var {x: {y: {z} = {z: 2}} = {}} = {}; z"), 2.0);
        // a present-but-undefined value still takes the default
        assert_eq!(
            n("var [[a] = [7]] = [undefined]; a"), 7.0);
        // the default is evaluated once, and only when it is needed
        assert_eq!(
            n("var calls = 0; \
               function d() { calls++; return [1, 2]; } \
               var [[a, b] = d()] = []; a * 100 + b * 10 + calls"),
            121.0);
        assert_eq!(
            n("var calls = 0; \
               function d() { calls++; return [1, 2]; } \
               var [[a, b] = d()] = [[3, 4]]; a * 100 + b * 10 + calls"),
            340.0);
    }

    /// Parameters get the same treatment: a destructuring parameter can
    /// default as a whole (`([x] = it) => {}`), and its elements can
    /// default individually.
    #[test]
    fn destructuring_params_take_defaults() {
        assert_eq!(n("(([x] = [3]) => x)()"), 3.0);
        assert_eq!(n("(([x] = [3]) => x)([9])"), 9.0);
        assert_eq!(n("(({a} = {a: 4}) => a)()"), 4.0);
        // nested pattern with a default, inside a parameter
        assert_eq!(n("(([[x, y] = [4, 5]]) => x * 10 + y)([])"), 45.0);
        assert_eq!(
            n("(([[x, y] = [4, 5]]) => x * 10 + y)([[1, 2]])"), 12.0);
        // and on a function declaration, not just an arrow
        assert_eq!(
            n("function f({p: {q} = {q: 8}}) { return q } f({})"), 8.0);
        // the whole-pattern default is applied before the unpack reads
        assert_eq!(
            n("function g([a, b] = [1, 2]) { return a * 10 + b } g()"),
            12.0);
    }

    /// Destructuring *assignment* is a separate desugaring path, and it
    /// takes defaults only on plain targets. A nested pattern with a
    /// default (`[[a] = [1]] = x`) is still unsupported there — see the
    /// note in parser.rs destr_target.
    #[test]
    fn destructuring_assignment_takes_defaults_on_plain_targets() {
        assert_eq!(n("var a, b; [a = 4, b = 5] = []; a * 10 + b"), 45.0);
        assert_eq!(
            n("var a, b; [a = 4, b = 5] = [1, 2]; a * 10 + b"), 12.0);
        assert_eq!(n("var q; ({q = 8} = {}); q"), 8.0);
        // nested patterns work, just not nested patterns with defaults
        assert_eq!(
            n("var x, y; [[x, y]] = [[1, 2]]; x * 10 + y"), 12.0);
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
        // rest params collect the extra arguments
        assert_eq!(
            n("function f(...xs) { return xs.length; } f(1,2)"), 2.0);
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
        // backreferences now work via the fancy-regex second-chance
        // engine (07-20; previously degraded to never-matching)
        assert_eq!(n("/(a)\\1/.test('aa') ? 1 : 0"), 1.0);
        assert_eq!(n("/(a)\\1/.test('ab') ? 1 : 0"), 0.0);
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
        // Hot transpiler wrapper: preserve receiver and the complete
        // arguments object while taking the in-loop apply fast path.
        assert_eq!(
            n("function target(a,b){return this.base+a+b+arguments.length;} \
               function wrap(){return target.apply(this,arguments);} \
               var o={base:10,wrap:wrap}; o.wrap(2,3)"), 17.0);
        assert_eq!(
            n("function target(a){return a+arguments[2];} \
               function wrap(){return target.apply(null,arguments);} \
               var total=0; for(var i=0;i<1000;i++){total+=wrap(1,2,3);} \
               total"), 4000.0);
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
        // lazy compilation: a function body is checked at first call,
        // so a never-called offender no longer fails the whole script
        // (this matches real JS, where const-assignment is a call-time
        // error, not an early error)
        assert_eq!(n("function f() { const k = 5; k = 6; } 1"), 1.0);
        assert!(
            eval("function f() { const k = 5; k = 6; } f()").is_err());
        assert!(eval("const c;").is_err(), "const needs an initializer");
        // reading and shadowing a const is fine
        assert_eq!(n("const c = 40; c + 2"), 42.0);
        assert_eq!(n("const c = 1; { const c = 2; } c"), 1.0);
        // only the binding is frozen, not the object it names
        assert_eq!(n("const o = {n: 1}; o.n = 5; o.n"), 5.0);
    }

    #[test]
    fn lazy_compilation_semantics() {
        // a broken cold function must not kill the script...
        assert_eq!(
            n("function bad() { const k = 1; k = 2; } 40 + 2"), 42.0);
        // ...and calling it throws a catchable error instead
        assert_eq!(
            n("function bad() { const k = 1; k = 2; } \
               var r = 0; try { bad(); } catch (e) { r = 1; } r"), 1.0);
        // captures resolved at deferral time behave identically:
        // transitive capture through an intermediate function
        assert_eq!(
            n("function outer() { var x = 40; \
               function mid() { function inner() { return x + 2; } \
                 return inner(); } \
               return mid(); } outer()"), 42.0);
        // mutation through a captured cell round-trips
        assert_eq!(
            n("function box() { var v = 0; \
               return { set: function (x) { v = x; }, \
                        get: function () { return v; } }; } \
               var b = box(); b.set(21); b.get() * 2"), 42.0);
        // a lazy fn called twice compiles once and stays correct
        assert_eq!(
            n("function fib(n) { \
               return n < 2 ? n : fib(n - 1) + fib(n - 2); } \
               fib(10)"), 55.0);
    }

    #[test]
    fn event_handler_this_binding() {
        // lifecycle handlers: this = registration target; the window
        // sentinel must map to the real window object (a fake dom
        // node would panic on any property access)
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<div id=t>x</div>"),
        ))));
        let logs = vm.run_scripts(&["\
            window.addEventListener('load', function () {\n\
              console.log('w ' + (this === window));\n\
            });\n\
            document.addEventListener('DOMContentLoaded', \
            function () {\n\
              console.log('d ' + (this === document));\n\
            });\n\
            document.getElementById('t').addEventListener('click', \
            function () {\n\
              console.log('n ' + this.id);\n\
            });\n"
            .to_string()]);
        assert!(logs.is_empty(), "{logs:?}");
        let logs = vm.fire_lifecycle();
        assert!(logs.contains(&"w true".to_string()), "{logs:?}");
        assert!(logs.contains(&"d true".to_string()), "{logs:?}");
        // click dispatch: this = the node whose listener runs
        let t = {
            let d = vm.st.doc.as_ref().unwrap().borrow();
            (0..d.nodes.len())
                .find(|&i| d.nodes[i].attr("id") == Some("t"))
                .unwrap()
        };
        let (logs, handled, _) = vm.dispatch_click(t);
        assert!(handled);
        assert!(logs.contains(&"n t".to_string()), "{logs:?}");
    }

    #[test]
    fn private_class_fields() {
        // #x fields/methods work through the class desugar
        assert_eq!(
            n("class Counter { \
                 #n = 0; \
                 inc() { this.#n += 1; return this.#n; } \
                 #twice() { return this.#n * 2; } \
                 read() { return this.#twice(); } } \
               var c = new Counter(); c.inc(); c.inc(); \
               c.read() * 10 + c.inc()"), 43.0);
        // two instances keep separate private state
        assert_eq!(
            n("class B { #v = 0; set(x) { this.#v = x; } \
                 get() { return this.#v; } } \
               var a = new B(), b = new B(); \
               a.set(4); b.set(2); a.get() * 10 + b.get()"), 42.0);
    }

    #[test]
    fn layout_rects_feed_gbcr() {
        let mut vm = PageVm::new(Some(Rc::new(RefCell::new(
            crate::html::parse("<div id=t>x</div>"),
        ))));
        // before any push: zero-rect (crash prevention as before)
        let logs = vm.run_scripts(&["\
            var r = document.getElementById('t')\
                .getBoundingClientRect();\n\
            console.log('pre ' + r.width + ' ' + r.height);\n"
            .to_string()]);
        assert!(logs.contains(&"pre 0 0".to_string()), "{logs:?}");
        // find the div's node index and push a rect for it
        let div = {
            let d = vm.st.doc.as_ref().unwrap().borrow();
            (0..d.nodes.len())
                .find(|&i| d.nodes[i].tag.as_deref() == Some("div"))
                .unwrap() as u32
        };
        vm.set_layout_rects(vec![(div, 13.0, 26.0, 774.0, 20.0)]);
        let logs = vm.run_scripts(&["\
            var r = document.getElementById('t')\
                .getBoundingClientRect();\n\
            console.log('post ' + r.x + ' ' + r.top + ' ' + r.width \
                + ' ' + r.bottom);\n"
            .to_string()]);
        assert!(logs.contains(&"post 13 26 774 46".to_string()),
                "{logs:?}");
    }

    #[test]
    fn lazy_parse_semantics() {
        // bodies >= 24 tokens skip AST building at load; the token
        // range parses at first call. Captures must still work.
        assert_eq!(
            n("var a = 30, b = 12; \
               function big() { \
                 var x = 0; var y = 0; var z = 0; \
                 x = a; y = b; z = x + y; \
                 return z + 0 + 0 + 0 + 0 + 0; } \
               big()"), 42.0);
        // template ${holes} in a skipped body still capture
        assert_eq!(
            n("var w = 'world'; \
               function greet() { \
                 var a = 1; var b = 2; var c = 3; var d = 4; \
                 var s = `hi ${w}`; \
                 return s.length + a + b + c + d; } \
               greet()"), 18.0);
        // a parse error inside a lazy body surfaces at call, catchable
        assert_eq!(
            n("function broken() { \
                 var a = 1; var b = 2; var c = 3; var d = 4; \
                 var e = 5; var f = 6; var g = 7; \
                 return a + ; } \
               var r = 0; try { broken(); } catch (e) { r = 1; } r"),
            1.0);
        // mutation of a captured var from a lazy-parsed body sticks
        assert_eq!(
            n("var n0 = 0; \
               function bump() { \
                 var a = 1; var b = 2; var c = 3; var d = 4; \
                 var e = 5; var f = 6; \
                 n0 = n0 + a + b + c + d + e + f; } \
               bump(); bump(); n0"), 42.0);
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

    /// What an uncaught throw is *called*. A thrown object only needs
    /// one half of the name/message pair to be reported as an error
    /// rather than as "[object Object]" — the shape Test262's own
    /// Test262Error uses, and the reason a quarter of that corpus
    /// used to fail with nothing to go on.
    #[test]
    fn uncaught_throws_name_themselves() {
        let msg = |src: &str| match eval(src) {
            Err(e) => e,
            Ok(_) => panic!("expected a throw from {src}"),
        };
        // message only: the label comes from the constructor
        assert!(
            msg("function Test262Error(m) { this.message = m; } \
                 throw new Test262Error('bad value');")
                .contains("Test262Error: bad value"),
            "got {}",
            msg("function Test262Error(m) { this.message = m; } \
                 throw new Test262Error('bad value');")
        );
        // an explicit name still wins over the constructor
        assert!(msg(
            "var e = new Error('x'); e.name = 'Custom'; throw e;"
        )
        .contains("Custom: x"));
        // name only, no message: no dangling colon
        let m = msg("throw {name: 'Whoops'};");
        assert!(m.contains("Whoops"), "got {m}");
        assert!(!m.contains("Whoops:"), "dangling colon in {m}");
        // a thrown built-in Error still reads off its own name
        assert!(msg("throw new TypeError('nope');").contains("TypeError: nope"));
        assert!(msg("throw new RangeError('oob');").contains("RangeError: oob"));
        // VM-raised errors do not go through here at all — they carry
        // their name as the error kind, not in the message
        assert_eq!(msg("null.x;"), "cannot read .x of null");
        // a thrown object with neither half is still not error-like
        assert!(msg("throw {a: 1};").contains("[object Object]"));
    }

    /// `name` and `length` are own properties of every function. This
    /// engine computes them instead of storing them, which made them
    /// invisible to every reflection path — `hasOwnProperty` said no,
    /// `getOwnPropertyDescriptor` said undefined, `getOwnPropertyNames`
    /// left them out.
    #[test]
    fn function_name_and_length_are_own_properties() {
        assert_eq!(n("function f(a,b){} f.hasOwnProperty('name')?1:0"), 1.0);
        assert_eq!(n("function f(a,b){} f.hasOwnProperty('length')?1:0"), 1.0);
        assert_eq!(
            n("function f(a,b){} \
               Object.getOwnPropertyNames(f).indexOf('name') >= 0 && \
               Object.getOwnPropertyNames(f).indexOf('length') >= 0 ? 1 : 0"),
            1.0);
        // the spec attributes: not writable, not enumerable, but
        // configurable — that combination is what verifyProperty checks
        assert_eq!(
            n("function f(a,b){} \
               var d = Object.getOwnPropertyDescriptor(f, 'length'); \
               d.value === 2 && d.writable === false && \
               d.enumerable === false && d.configurable === true ? 1 : 0"),
            1.0);
        assert_eq!(
            n("function foo(){} \
               var d = Object.getOwnPropertyDescriptor(foo, 'name'); \
               d.value === 'foo' && d.writable === false && \
               d.enumerable === false && d.configurable === true ? 1 : 0"),
            1.0);
        // .prototype is writable but neither enumerable nor configurable
        assert_eq!(
            n("function f(){} \
               var d = Object.getOwnPropertyDescriptor(f, 'prototype'); \
               d.writable === true && d.enumerable === false && \
               d.configurable === false ? 1 : 0"),
            1.0);
        // an assigned static still reports as a plain data property
        assert_eq!(
            n("function f(){} f.x = 7; \
               var d = Object.getOwnPropertyDescriptor(f, 'x'); \
               d.value === 7 && d.writable && d.enumerable && \
               d.configurable ? 1 : 0"),
            1.0);
        // neither shows up in Object.keys — they are not enumerable
        assert_eq!(n("function f(a){} Object.keys(f).length"), 0.0);
    }

    /// A page must not be able to abort the process. Rust cannot
    /// recover a failed allocation, so any element-storage growth that
    /// takes its size from script has to be capped *before* the resize
    /// — one uncapped path asked for 72 PB and took the whole
    /// interpreter down with it.
    #[test]
    fn huge_array_indices_throw_instead_of_aborting() {
        // the numeric-string write path, which had no cap
        assert_eq!(
            n("var a = []; \
               try { a['9007199254740990'] = 1; 0 } catch (e) { 1 }"),
            1.0);
        // and the same index as a number, which did
        assert_eq!(
            n("var a = []; \
               try { a[9007199254740990] = 1; 0 } catch (e) { 1 }"),
            1.0);
        // an ordinary index is untouched by the cap
        assert_eq!(n("var a = []; a['1000'] = 7; a.length"), 1001.0);
    }

    /// An error subclass has to point back at its own constructor.
    /// `assert.throws` — and plenty of library code — compares
    /// `thrown.constructor` rather than using instanceof.
    #[test]
    fn error_subclasses_point_at_their_own_constructor() {
        for (ctor, thrower) in [
            ("TypeError", "null.x"),
            ("RangeError", "[].length = -1"),
            ("ReferenceError", "xyzzy"),
        ] {
            assert_eq!(
                n(&format!(
                    "try {{ {thrower} }} catch (e) {{ \
                       e.constructor === {ctor} && e.name === '{ctor}' \
                       && e instanceof {ctor} && e instanceof Error \
                       ? 1 : 0 }}"
                )),
                1.0,
                "{ctor} raised by the VM",
            );
        }
        for ctor in ["TypeError", "RangeError", "SyntaxError",
                     "ReferenceError", "EvalError", "URIError"] {
            assert_eq!(
                n(&format!(
                    "var e = new {ctor}('m'); \
                     e.constructor === {ctor} && \
                     {ctor}.prototype.constructor === {ctor} && \
                     e.message === 'm' && e.name === '{ctor}' ? 1 : 0"
                )),
                1.0,
                "new {ctor}",
            );
        }
        // constructor stays non-enumerable, as the default one was
        assert_eq!(
            n("var k = 0; for (var p in new TypeError('m')) \
               { if (p === 'constructor') k = 1; } k"),
            0.0);
        // JSON.parse is specified to throw a SyntaxError
        assert_eq!(
            n("try { JSON.parse('{'); 0 } catch (e) { \
               e.constructor === SyntaxError ? 1 : 0 }"),
            1.0);
        assert_eq!(
            n("try { JSON.parse('[1,]'); 0 } catch (e) { \
               e instanceof SyntaxError ? 1 : 0 }"),
            1.0);
        // AggregateError collects its errors
        assert_eq!(
            n("var e = new AggregateError([1, 2], 'boom'); \
               e.errors.length === 2 && e.errors[1] === 2 && \
               e.message === 'boom' && e.name === 'AggregateError' && \
               e.constructor === AggregateError ? 1 : 0"),
            1.0);
    }

    /// `\uXXXX` names a character in an identifier just as in a string,
    /// so `A` and `A` are the same name.
    #[test]
    fn identifiers_accept_unicode_escapes() {
        // escape declares, plain reads
        assert_eq!(n(r"var \u0041 = 4; A"), 4.0);
        // plain declares, escape reads
        assert_eq!(n(r"var A = 5; \u0041"), 5.0);
        // an escape in the middle, and in a member name
        assert_eq!(n(r"var a\u0042c = 6; aBc"), 6.0);
        assert_eq!(n(r"var o = {}; o.\u0062 = 7; o.b"), 7.0);
        // the braced form, and a non-ASCII letter
        assert_eq!(n(r"var \u{48}i = 8; Hi"), 8.0);
        assert_eq!(n("var \\u{d55c} = 9; \u{d55c}"), 9.0);
        // a digit is fine after the first character, not before it
        assert_eq!(n(r"var a\u0031 = 3; a1"), 3.0);
        assert!(eval(r"var \u0031a = 1;").is_err());
        // an escape that names punctuation is not an identifier
        assert!(eval(r"var \u002B = 1;").is_err());
        // an escape may spell an identifier, never a reserved word,
        // where a name is expected
        assert!(eval(r"var x = 1; x + \u0069\u0066").is_err());
        assert!(eval(r"(\u0074his)").is_err());
        assert!(eval(r"var x = \u0074ypeof;").is_err());
        // ...but a property name may be a reserved word, escaped or
        // not, so the check cannot live in the lexer
        assert_eq!(n(r"var o = { i\u0066: 5 }; o['if']"), 5.0);
        assert_eq!(n(r"var o = { if: 6 }; o.i\u0066"), 6.0);
        assert_eq!(n(r"var o = {}; o.\u0066or = 7; o.for"), 7.0);
        // shorthand makes the name a reference too, so there the
        // reserved word is rejected after all
        assert!(eval(r"var x = { bre\u0061k } = { break: 1 };").is_err());
        assert!(eval(r"var o = { i\u0066 };").is_err());
        // ...but only in shorthand: as a key or a method it is fine
        assert_eq!(n(r"var o = { i\u0066: 8 }; o['if']"), 8.0);
        assert_eq!(n(r"var o = { i\u0066() { return 9 } }; o.if()"), 9.0);
        // a word that merely contains an escape is still fine
        assert_eq!(n(r"var \u0069fy = 2; ify"), 2.0);
        // and a non-\u escape is not one either
        assert!(eval(r"var \x41 = 1;").is_err());
    }

    /// A regex literal builds a fresh object every time it is reached,
    /// but not a fresh automaton. Compiling dominates the cost by three
    /// orders of magnitude, so a literal in a loop body used to spend
    /// all its time rebuilding a pattern it already had.
    #[test]
    fn regex_literals_do_not_recompile() {
        // same pattern, many evaluations: one compiled record
        assert_eq!(
            n("var n = 0; \
               for (var i = 0; i < 50; i++) { \
                 if (/^(\\d{4})-(\\d{2})-(\\d{2})$/.test('2026-07-28')) n++; \
               } n"),
            50.0);
        // distinct objects, as the spec requires — the sharing is
        // behind them, not in front
        assert_eq!(
            n("function mk() { return /a(b)c/g; } \
               var x = mk(), y = mk(); x === y ? 1 : 0"),
            0.0);
        // ...so lastIndex stays per-object even when the pattern is
        // shared, which is what makes sharing safe at all
        assert_eq!(
            n("function mk() { return /a/g; } \
               var x = mk(), y = mk(); \
               x.exec('aaa'); \
               x.lastIndex === 1 && y.lastIndex === 0 ? 1 : 0"),
            1.0);
        // flags are part of the identity: same source, different flags
        assert_eq!(
            n("var a = /x/i, b = /x/; \
               a.ignoreCase === true && b.ignoreCase === false && \
               a.test('X') && !b.test('X') ? 1 : 0"),
            1.0);
        // and a cached record still reports its own source and flags
        assert_eq!(
            n("function mk() { return /a(b)c/gi; } \
               mk(); var r = mk(); \
               r.source === 'a(b)c' && r.flags === 'gi' && \
               r.global === true ? 1 : 0"),
            1.0);
    }

    /// A global or sticky regex resumes from `lastIndex` and writes
    /// back where it stopped. Without that, the idiom every reference
    /// for `exec` shows — `while ((m = re.exec(s)) !== null)` — matches
    /// position zero forever and never terminates.
    #[test]
    fn global_exec_advances_last_index() {
        assert_eq!(
            n("var re = /a/g, m, n = 0; \
               while ((m = re.exec('aXaXa')) !== null) { \
                 n++; if (n > 20) break; \
               } n"),
            3.0);
        // the positions it walks through, and the reset at the end
        assert_eq!(
            n("var re = /a/g; re.exec('aXa'); var p = re.lastIndex; \
               re.exec('aXa'); var q = re.lastIndex; \
               re.exec('aXa'); \
               p === 1 && q === 3 && re.lastIndex === 0 ? 1 : 0"),
            1.0);
        // a non-global regex ignores lastIndex and never sets it
        assert_eq!(
            n("var re = /a/; re.lastIndex = 2; \
               var m = re.exec('aXa'); \
               m.index === 0 && re.lastIndex === 2 ? 1 : 0"),
            1.0);
        // .index and .input come with the result
        assert_eq!(
            n("var m = /b(c)/.exec('abcd'); \
               m.index === 1 && m.input === 'abcd' && \
               m[0] === 'bc' && m[1] === 'c' ? 1 : 0"),
            1.0);
        // positions are UTF-16 units, matching String.prototype.length
        assert_eq!(
            n("var m = /b/.exec('\u{1F600}b'); \
               m.index === 2 && '\u{1F600}b'.length === 3 ? 1 : 0"),
            1.0);
        // lastIndex past the end fails the match and resets
        assert_eq!(
            n("var re = /a/g; re.lastIndex = 99; \
               re.exec('aaa') === null && re.lastIndex === 0 ? 1 : 0"),
            1.0);
    }

    /// Date.parse only accepted the ISO form, so the commonest way a
    /// date is actually written was NaN.
    #[test]
    fn date_parse_accepts_the_usual_formats() {
        let iso = "Date.UTC(2000, 0, 2)";
        for s in ["2000-01-02", "2000-01-02T00:00:00", "1/2/2000",
                  "Jan 2, 2000", "January 2, 2000", "2 Jan 2000",
                  "Wed, 2 Jan 2000"] {
            assert_eq!(
                n(&format!("Date.parse('{s}') === {iso} ? 1 : 0")),
                1.0,
                "Date.parse({s:?})",
            );
        }
        // times come through
        assert_eq!(
            n("Date.parse('1/2/2000 03:04:05') === \
               Date.UTC(2000, 0, 2, 3, 4, 5) ? 1 : 0"),
            1.0);
        // two-digit years split at 50, as everywhere else
        assert_eq!(
            n("Date.parse('1/1/49') === Date.UTC(2049, 0, 1) && \
               Date.parse('1/1/50') === Date.UTC(1950, 0, 1) ? 1 : 0"),
            1.0);
        // and the constructor goes through the same path
        assert_eq!(
            n("new Date('1/2/2000').getFullYear() * 100 + \
               new Date('1/2/2000').getDate()"),
            200002.0);
        // above 31 the first field can only be a year
        assert_eq!(
            n("Date.parse('99/1/2') === Date.UTC(1999, 0, 2) && \
               Date.parse('50/1/2') === Date.UTC(1950, 0, 2) && \
               Date.parse('32/1/2') === Date.UTC(2032, 0, 2) ? 1 : 0"),
            1.0);
        // 13..31 could be a year or a day; rather than guess, reject
        assert_eq!(
            n("isNaN(Date.parse('13/1/2')) && \
               isNaN(Date.parse('31/1/2')) ? 1 : 0"),
            1.0);
        // 12 and below is a month
        assert_eq!(
            n("Date.parse('12/1/2') === Date.UTC(2002, 11, 1) ? 1 : 0"),
            1.0);
        // a written month may sit in any of the three positions
        for s in ["may 1 2000", "1 may 2000", "1 2000 may",
                  "may 2000 1", "2000 may 1", "2000 1 may",
                  "May 1, 2000"] {
            assert_eq!(
                n(&format!(
                    "Date.parse('{s}') === Date.UTC(2000, 4, 1) ? 1 : 0"
                )),
                1.0,
                "Date.parse({s:?})",
            );
        }
        // two numbers that can both only be years is not a date
        assert_eq!(
            n("isNaN(Date.parse('may 1999 1999')) && \
               isNaN(Date.parse('may 0 0')) ? 1 : 0"),
            1.0);
        // an out-of-range day is rejected, not rolled into the next
        // month: "99/1/99" was coming back as 1999-04-09
        assert_eq!(
            n("isNaN(Date.parse('99/1/99')) && \
               isNaN(Date.parse('2/30/2000')) && \
               isNaN(Date.parse('4/31/2000')) ? 1 : 0"),
            1.0);
        // ...but a real leap day is fine
        assert_eq!(
            n("Date.parse('2/29/2000') === Date.UTC(2000, 1, 29) && \
               isNaN(Date.parse('2/29/1900')) ? 1 : 0"),
            1.0);
        // nonsense is still NaN, not a wrong date
        assert_eq!(
            n("isNaN(Date.parse('not a date')) && \
               isNaN(Date.parse('13/40/2000')) ? 1 : 0"),
            1.0);
    }

    /// `for...in` order is insertion order, and it must not depend on
    /// which process you are in. Object.create's descriptor-map loop
    /// walked a HashMap unsorted, so the order changed run to run —
    /// which showed up as a Test262 case that passed or failed at
    /// random.
    #[test]
    fn object_create_defines_properties_in_order() {
        assert_eq!(
            n("var p = {}; \
               p.a = {value: 1, enumerable: true}; \
               p.b = {value: 2, enumerable: true}; \
               p.c = {value: 3, enumerable: true}; \
               var r = []; \
               for (var k in Object.create({}, p)) r.push(k); \
               r.join('') === 'abc' ? 1 : 0"),
            1.0);
        // Object.create's descriptor map is defineProperties, so it
        // honours the whole descriptor, not just `value`
        assert_eq!(
            n("var o = Object.create({}, {x: {value: 5}}); \
               o.x === 5 && Object.keys(o).length === 0 ? 1 : 0"),
            1.0);
        assert_eq!(
            n("var o = Object.create({}, \
                 {x: {get: function () { return 7 }, enumerable: true}}); \
               o.x === 7 && Object.keys(o).join('') === 'x' ? 1 : 0"),
            1.0);
        // a builtin namespace has no enumerable own properties, so
        // it is legal as a descriptor map: only what someone assigned
        // to it is read
        assert_eq!(n("Object.keys(Math).length"), 0.0);
        assert_eq!(
            n("Math.prop = {value: 12, enumerable: true}; \
               var o = Object.create({}, Math); \
               o.hasOwnProperty('prop') && o.prop === 12 ? 1 : 0"),
            1.0);
        // ...while still being reachable by name
        assert_eq!(n("Math.floor(Math.PI * 100)"), 314.0);
        // and integer keys still come first, ascending
        assert_eq!(
            n("var o = {}; o.z = 1; o[2] = 1; o.a = 1; o[1] = 1; \
               var r = []; for (var k in o) r.push(k); r.join(',')  \
                 === '1,2,z,a' ? 1 : 0"),
            1.0);
    }

    /// Engine bookkeeping must not be visible as enumerable own
    /// properties. Each of these leaked into for-in, Object.keys,
    /// Object.assign and JSON.stringify.
    #[test]
    fn engine_internals_are_not_enumerable() {
        // Date kept its timestamp as a plain `_t` assignment
        assert_eq!(n("Object.keys(new Date(0)).length"), 0.0);
        assert_eq!(
            n("JSON.stringify(Object.assign({}, new Date(0))) === '{}' \
               ? 1 : 0"),
            1.0);
        // ...and still works
        assert_eq!(n("new Date(0).getTime()"), 0.0);
        assert_eq!(
            n("new Date(0).toISOString() === '1970-01-01T00:00:00.000Z' \
               ? 1 : 0"),
            1.0);
        // a RegExp instance kept eight
        assert_eq!(n("Object.keys(/a/g).length"), 0.0);
        assert_eq!(
            n("JSON.stringify({...(/a/g)}) === '{}' ? 1 : 0"), 1.0);
        assert_eq!(
            n("var r = /a(b)/gi; \
               r.source === 'a(b)' && r.flags === 'gi' && \
               r.global === true && r.lastIndex === 0 ? 1 : 0"),
            1.0);
        // arguments.callee is non-enumerable, the indices are not
        assert_eq!(
            n("function f() { return Object.keys(arguments).join(',') } \
               f(1, 2) === '0,1' ? 1 : 0"),
            1.0);
        assert_eq!(
            n("function f() { return arguments.callee === f } \
               f() ? 1 : 0"),
            1.0);
        // Object.assign copies own *enumerable* only
        assert_eq!(
            n("JSON.stringify(Object.assign({a: 1}, {b: 2})) \
                 === '{\"a\":1,\"b\":2}' ? 1 : 0"),
            1.0);
    }

    /// An integer-like key on a plain object lives in element storage.
    /// Every reflection path compensated for that except JSON, which
    /// dropped such keys without a word.
    #[test]
    fn json_stringify_keeps_integer_keys() {
        assert_eq!(
            n("JSON.stringify({1: 'a', 2: 'b'}) \
                 === '{\"1\":\"a\",\"2\":\"b\"}' ? 1 : 0"),
            1.0);
        // mixed: integer keys first, ascending, then insertion order
        assert_eq!(
            n("var o = {}; o.z = 1; o[2] = 2; o.a = 3; o[1] = 4; \
               JSON.stringify(o) \
                 === '{\"1\":4,\"2\":2,\"z\":1,\"a\":3}' ? 1 : 0"),
            1.0);
        // and it round-trips
        assert_eq!(
            n("var o = {}; o[1001] = 'v'; \
               JSON.parse(JSON.stringify(o))[1001] === 'v' ? 1 : 0"),
            1.0);
        // arrays are unchanged
        assert_eq!(
            n("JSON.stringify([1, 2]) === '[1,2]' ? 1 : 0"), 1.0);
        assert_eq!(
            n("JSON.stringify({a: [1, {b: 2}]}) \
                 === '{\"a\":[1,{\"b\":2}]}' ? 1 : 0"),
            1.0);
    }

    /// Property order is insertion order, and it must be the same in
    /// every process. Accessor-only keys were enumerated straight out
    /// of a HashMap, so `Object.keys` over an object of getters gave a
    /// different answer each run — and a Test262 case that checks
    /// enumeration stops at the *first* throwing getter passed or
    /// failed at random.
    #[test]
    fn accessor_keys_enumerate_in_insertion_order() {
        assert_eq!(
            n("var o = {get a() { return 1 }, get b() { return 2 }, \
                       get c() { return 3 }}; \
               Object.keys(o).join('') === 'abc' ? 1 : 0"),
            1.0);
        // enumeration reaches `a` before `b`, so `a` throws first
        assert_eq!(
            n("var o = {get a() { throw new RangeError('r') }, \
                       get b() { throw new Error('b') }}; \
               try { Object.entries(o); 0 } \
               catch (e) { e.constructor === RangeError ? 1 : 0 }"),
            1.0);
        // defineProperty accessors keep their order too
        assert_eq!(
            n("var o = {}; \
               Object.defineProperty(o, 'p', \
                 {get: function () { return 1 }, enumerable: true}); \
               Object.defineProperty(o, 'q', \
                 {get: function () { return 2 }, enumerable: true}); \
               Object.keys(o).join('') === 'pq' ? 1 : 0"),
            1.0);
    }

    /// VT and FF are WhiteSpace in the grammar, same as space and tab.
    #[test]
    fn vertical_tab_and_form_feed_are_whitespace() {
        assert_eq!(n("var\u{0b}a\u{0b}=\u{0b}4; a"), 4.0);
        assert_eq!(n("var\u{0c}b\u{0c}=\u{0c}5; b"), 5.0);
        // and they do not count as line terminators for ASI
        assert_eq!(n("var c = 1\u{0b}+ 2; c"), 3.0);
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
    fn await_in_expression_position() {
        let mut vm = PageVm::new(None);
        let logs = vm.run_scripts(&["\
            async function one() { return 5; }\n\
            async function sum() { return await one() + await one(); }\n\
            async function branch(v) {\n\
              if (await one() === v) { return 'eq'; }\n\
              return 'ne';\n\
            }\n\
            async function cond() {\n\
              var out = [];\n\
              var i = 0;\n\
              while (await one() > i) { out.push(i); i += 3; }\n\
              return out.join(',');\n\
            }\n\
            async function ifbody(flag) {\n\
              var x = 1;\n\
              if (flag) { x = await one(); }\n\
              return x + 10;\n\
            }\n\
            sum().then(function (v) { console.log('sum ' + v); });\n\
            branch(5).then(function (v) { console.log('br ' + v); });\n\
            cond().then(function (v) { console.log('loop ' + v); });\n\
            ifbody(true).then(function (v) { console.log('ift ' + v); });\n\
            ifbody(false).then(function (v) { console.log('iff ' + v); });\n"
            .to_string()]);
        assert!(logs.iter().all(|l| !l.contains("error")), "{logs:?}");
        let (logs, _) = vm.pump();
        assert!(logs.contains(&"sum 10".to_string()), "{logs:?}");
        assert!(logs.contains(&"br eq".to_string()), "{logs:?}");
        assert!(logs.contains(&"loop 0,3".to_string()), "{logs:?}");
        assert!(logs.contains(&"ift 15".to_string()), "{logs:?}");
        assert!(logs.contains(&"iff 11".to_string()), "{logs:?}");
    }

    #[test]
    fn await_inside_try_catch() {
        let mut vm = PageVm::new(None);
        let logs = vm.run_scripts(&["\
            async function boom() { throw 'bad'; }\n\
            async function five() { return 5; }\n\
            async function guarded() {\n\
              try { var v = await five(); return v + 1; }\n\
              catch (e) { return 'caught ' + e; }\n\
            }\n\
            async function rescued() {\n\
              var r = 0;\n\
              try { r = await boom(); } catch (e) { r = 'c:' + e; }\n\
              return r;\n\
            }\n\
            guarded().then(function (v) { console.log('g ' + v); });\n\
            rescued().then(function (v) { console.log('r ' + v); });\n"
            .to_string()]);
        assert!(logs.iter().all(|l| !l.contains("error")), "{logs:?}");
        let (logs, _) = vm.pump();
        assert!(logs.contains(&"g 6".to_string()), "{logs:?}");
        assert!(logs.contains(&"r c:bad".to_string()), "{logs:?}");
    }

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
    fn interval_fires_once_per_pump_and_settles() {
        // A setInterval must not fast-forward to the event-loop budget
        // during load settling (Naver's IntersectionObserver polyfill polls
        // on one — the unbounded fast-forward cost ~50s per settle). Each
        // pump fires it exactly once and the settle loop can terminate.
        let mut vm = PageVm::new(None);
        vm.run_scripts(
            &["setInterval(function () { console.log('tick'); }, 100);"
                .to_string()],
        );
        let (logs, _) = vm.pump();
        assert_eq!(logs, vec!["tick"], "interval must fire once, not spin");
        // an interval alone is steady-state polling, not pending load work
        assert!(!vm.has_pending_work());
        let (logs2, _) = vm.pump();
        assert_eq!(logs2, vec!["tick"]);
    }

    #[test]
    fn settle_horizon_fires_near_defers_far() {
        // load-settling fires timers due within the horizon but leaves
        // far-future ones (animations/tickers) for frame-pace playback
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            setTimeout(function(){ console.log('near'); }, 50);\n\
            setTimeout(function(){ console.log('far'); }, 3000);\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert_eq!(logs, vec!["near"], "near fires, far deferred: {logs:?}");
        // the far one-shot is past the horizon: not pending load work
        assert!(!vm.has_pending_work());
    }

    #[test]
    fn self_rescheduling_timer_does_not_spin() {
        // naver's AutoRolling ticker re-arms a fresh setTimeout every few
        // seconds; the fast-forward must not chase it forever. Its first
        // re-arm (1000ms) is past the 250ms horizon, so settling quiesces.
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            var n = 0;\n\
            (function tick(){ n++; setTimeout(tick, 1000); })();\n\
            setTimeout(function(){ console.log('n=' + n); }, 100);\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        assert_eq!(logs, vec!["n=1"], "ticker must not fast-forward: {logs:?}");
        assert!(!vm.has_pending_work());
    }

    #[test]
    fn clear_interval_during_pump_stops_it() {
        // in-place interval firing must keep clearInterval working: a
        // one-shot that clears the interval removes it for good.
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            var id = setInterval(function () { console.log('x'); }, 50);\n\
            setTimeout(function () { clearInterval(id); }, 10);\n"
            .to_string()]);
        let (logs, _) = vm.pump();
        // the t=10 clear runs before the t=50 tick, so no 'x' ever fires
        assert!(!logs.iter().any(|l| l == "x"), "cleared: {logs:?}");
        assert!(!vm.has_pending_work());
        let (logs2, _) = vm.pump();
        assert!(logs2.is_empty(), "interval stays cleared: {logs2:?}");
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
    fn fetch_options_and_response_url_reach_the_host_bridge() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            fetch('/api', {
              method: 'POST', body: 'x=1', mode: 'cors',
              credentials: 'include',
              headers: {'Content-Type': 'text/plain', 'X-Token': 'abc'}
            }).then(function (r) {
              console.log(r.status + ' ' + r.url);
            });\n"
            .to_string()]);
        let (_logs, requests) = vm.pump_requests();
        assert_eq!(requests.len(), 1);
        let (fid, url, method, body, headers, mode, credentials) =
            requests[0].clone();
        assert_eq!(url, "/api");
        assert_eq!(method, "POST");
        assert_eq!(body, "x=1");
        assert_eq!(mode, "cors");
        assert_eq!(credentials, "include");
        assert!(headers.contains(&(
            "Content-Type".to_string(),
            "text/plain".to_string()
        )));
        assert!(headers.contains(&(
            "X-Token".to_string(),
            "abc".to_string()
        )));
        vm.resolve_fetch_full(
            fid,
            201,
            "https://api.example/final".to_string(),
            "ok".to_string(),
            vec![("Content-Type".to_string(), "text/plain".to_string())],
        );
        let (logs, more) = vm.pump();
        assert!(more.is_empty());
        assert_eq!(logs, vec!["201 https://api.example/final"]);
    }

    #[test]
    fn xhr_method_headers_body_and_credentials_use_fetch_bridge() {
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["\
            var xhr = new XMLHttpRequest();
            xhr.open('POST', '/xhr');
            xhr.setRequestHeader('Content-Type', 'text/plain');
            xhr.withCredentials = true;
            xhr.send('payload');\n"
            .to_string()]);
        let (_logs, requests) = vm.pump_requests();
        assert_eq!(requests.len(), 1);
        let (_fid, url, method, body, headers, mode, credentials) =
            requests[0].clone();
        assert_eq!(url, "/xhr");
        assert_eq!(method, "POST");
        assert_eq!(body, "payload");
        assert_eq!(mode, "cors");
        assert_eq!(credentials, "include");
        assert!(headers.contains(&(
            "Content-Type".to_string(),
            "text/plain".to_string()
        )));
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
    fn message_channel_delivers_to_peer_port() {
        // react-dom 18's scheduler drives its work loop through
        // MessageChannel: port2.postMessage must reach port1.onmessage
        // on a later macrotask (not synchronously, not never)
        let mut vm = PageVm::new(None);
        let sync = vm.run_scripts(&["\
            var c = new MessageChannel();\n\
            c.port1.onmessage = function (e) { console.log('got ' + e.data); };\n\
            console.log('before');\n\
            c.port2.postMessage('ping');\n"
            .to_string()]);
        // synchronous run does NOT deliver the message
        assert_eq!(sync, vec!["before"]);
        // it arrives on the next macrotask, drained by the pump
        let (logs, _) = vm.pump();
        assert_eq!(logs, vec!["got ping"]);
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
    fn a_long_declarator_list_does_not_exhaust_the_register_file() {
        // Registers are u8, so a function has 250 of them. An
        // initializer is dead once its value is bound, but the temps
        // used to be kept until the whole statement ended -- and
        // minifiers emit `var a={},b={},...` with a hundred-plus
        // declarators (core-js ships one), which made the bundle fail
        // to compile at all rather than merely run slowly.
        for kw in ["var", "let", "const"] {
            let decls = (0..400)
                .map(|i| format!("a{i}={{}}"))
                .collect::<Vec<_>>()
                .join(",");
            let src = format!("function f(){{ {kw} {decls}; return a399; }}\
                               console.log(typeof f());");
            let mut vm = PageVm::new(None);
            let out = vm.run_scripts(&[src]);
            assert_eq!(out, vec!["object".to_string()], "{kw} declarators");
        }
        // ...and the values still chain through the list correctly
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["var a=1,b=a+1,c=b+1,d={v:c};\
                             console.log(a,b,c,d.v);".to_string()]),
            vec!["1 2 3 3".to_string()]
        );
    }

    #[test]
    fn an_optional_chain_off_a_call_passes_its_arguments() {
        // Call reads its arguments at func+1. Inside an optional
        // chain the callee was compiled through chain_into, which
        // leaves its scratch register allocated, so the first argument
        // landed one slot too high and the callee itself was passed as
        // argument one: `f(x)?.y` called `f(f)`.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["function id(v) { return { got: v }; }\
                var e = 'ELEM';\
                console.log(id(e).got);\
                console.log(id(e)?.got);\
                console.log(id(e)?.got ?? 'none');\
                console.log(JSON.stringify({ a: id(e)?.got,\
                                             b: id(e)?.got }));\
                function two(a, b) { return { got: a + '/' + b }; }\
                console.log(two('x', 'y')?.got);\
                var maybe = null;\
                console.log(String(maybe?.(e)));\
                var f = id;\
                console.log(f?.(e).got);"
                .to_string()]),
            vec![
                "ELEM".to_string(),
                "ELEM".to_string(),
                "ELEM".to_string(),
                r#"{"a":"ELEM","b":"ELEM"}"#.to_string(),
                "x/y".to_string(),
                "undefined".to_string(),
                "ELEM".to_string(),
            ],
        );
    }

    #[test]
    fn try_catch_finally_survive_an_await() {
        // A `try` holding an await is wrapped in an IIFE, and a
        // `return` cannot escape one -- so a try that returned, with
        // anything after it, refused to compile ("await only supported
        // at statement level"). gfp-display-sdk's adapter picker is
        // exactly that shape, and every ad slot on naver hung on it.
        let mut vm = PageVm::new(None);
        let out = vm.run_scripts(&[
            "function p(v) { return Promise.resolve(v); }\
             function say(v) { console.log(v); }\
             async function ret() {\
                 try { return await p('ok'); } catch (e) { return 'C'; }\
                 throw new Error('unreachable');\
             }\
             async function caught() {\
                 try { throw 1; } catch (e) { return await p('caught'); }\
                 throw new Error('unreachable');\
             }\
             async function fall() {\
                 var s = '';\
                 try { s = await p('a'); } catch (e) {}\
                 return s + 'b';\
             }\
             async function fin() {\
                 var log = [];\
                 try { log.push(await p('t')); } finally { log.push('f'); }\
                 return log.join(',');\
             }\
             async function rethrow() {\
                 try { await p(1); throw 'BOOM'; } finally { var z = 1; }\
             }\
             ret().then(say); caught().then(say); fall().then(say);\
             fin().then(say);\
             rethrow().then(function () { say('NOT REACHED'); },\
                            function (e) { say('rejected ' + e); });"
                .to_string(),
        ]);
        assert!(out.is_empty(), "{out:?}");
        let (logs, _) = vm.pump();
        let mut logs = logs;
        logs.sort();
        assert_eq!(
            logs,
            vec![
                "ab".to_string(),
                "caught".to_string(),
                "ok".to_string(),
                // finally runs, and does not swallow the value...
                "t,f".to_string(),
                // ...nor turn a rejection into a resolution
                "rejected BOOM".to_string(),
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn extracted_string_builtins_cover_the_plain_transforms() {
        // core-js uncurries every String.prototype method
        // (`var f = "".toLowerCase; f.call(s)`) and bundles then call
        // them everywhere; anything missing from this path threw
        // "extracted builtin ... is not supported yet" mid-render.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["var u = function (m) { return m.call; };\
                var s = '  MiXeD  ';\
                var lower = ''.toLowerCase, upper = ''.toUpperCase;\
                var trim = ''.trim;\
                console.log(lower.call(s) + '|' + upper.call(s));\
                console.log('[' + trim.call(s) + ']');\
                console.log(''.trimStart.call(s) + '|' + ''.trimEnd.call(s));\
                console.log(''.startsWith.call('abcdef', 'cd', 2),\
                            ''.endsWith.call('abcdef', 'cd', 4),\
                            ''.includes.call('abcdef', 'cde'));\
                console.log(''.lastIndexOf.call('abcabc', 'b'));\
                console.log(''.substring.call('abcdef', 4, 1),\
                            ''.substr.call('abcdef', 1, 2));\
                console.log(''.padStart.call('7', 3, '0'),\
                            ''.padEnd.call('7', 3, '.'),\
                            ''.repeat.call('ab', 3));"
                .to_string()]),
            vec![
                "  mixed  |  MIXED  ".to_string(),
                "[MiXeD]".to_string(),
                "MiXeD  |  MiXeD".to_string(),
                "true true true".to_string(),
                "4".to_string(),
                "bcd bc".to_string(),
                "007 7.. ababab".to_string(),
            ],
        );
    }

    #[test]
    fn document_write_lands_next_to_the_script_that_wrote_it() {
        // The whole SafeFrame ad mechanism is this: a shell document
        // whose only content is a <script>, which writes the creative
        // markup next to itself. document.write on the running
        // document was unsupported, so every ad slot laid out at its
        // reserved size and painted nothing.
        let doc = Rc::new(RefCell::new(crate::html::parse(
            "<div id=box><script id=s></script></div><div id=tail></div>",
        )));
        let mut vm = PageVm::new(Some(doc.clone()));
        let script =
            crate::dom_api::query(&doc.borrow(), "#s", true)[0] as u32;
        vm.set_current_script(Some(script));
        vm.run_scripts(&["document.write('<b id=one>1</b>');\
             document.write('<i id=two>2</i>');"
            .to_string()]);
        vm.set_current_script(None);
        // both chunks inside the script's parent, in write order, after
        // the script and before anything that followed it
        assert_eq!(
            vm.run_scripts(&["var box = document.getElementById('box');\
                var out = [];\
                for (var i = 0; i < box.childNodes.length; i++) {\
                  var c = box.childNodes[i];\
                  if (c.tagName) out.push(c.tagName + '#' + c.id);\
                }\
                console.log(out.join(',') + '|'\
                  + document.getElementById('tail').id);"
                .to_string()]),
            vec!["SCRIPT#s,B#one,I#two|tail".to_string()],
        );
    }

    #[test]
    fn an_object_with_handle_event_is_a_listener() {
        // EventListener is a callback interface: an object with a
        // handleEvent method is as valid as a function, and every
        // class-based SDK registers `this`. Dropping those silently
        // cost naver's ad host every message its frames sent it.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["var seen = [];\
                var sink = {\
                  tag: 'sink',\
                  handleEvent: function (e) {\
                    seen.push(this.tag + ':' + e.type);\
                  }\
                };\
                window.addEventListener('ping', sink);\
                window.addEventListener('ping', function (e) {\
                  seen.push('fn:' + e.type);\
                });\
                window.addEventListener('ping', { nope: 1 });\
                window.dispatchEvent({ type: 'ping' });\
                console.log(seen.join('|'));"
                .to_string()]),
            vec!["sink:ping|fn:ping".to_string()],
        );
    }

    #[test]
    fn a_resize_observer_reports_when_layout_lands() {
        // Driven by the host pushing real geometry, which is the only
        // moment a size can be said to have changed. The spec also
        // delivers one notification when observation starts.
        let doc = Rc::new(RefCell::new(crate::html::parse(
            "<div id=a></div><div id=b></div>",
        )));
        let mut vm = PageVm::new(Some(doc.clone()));
        let a = crate::dom_api::query(&doc.borrow(), "#a", true)[0] as u32;
        vm.run_scripts(&["var log = [];\
            var ro = new ResizeObserver(function (es) {\
              for (var i = 0; i < es.length; i++) {\
                log.push(es[i].contentRect.width + 'x'\
                         + es[i].contentRect.height);\
              }\
            });\
            ro.observe(document.getElementById('a'));"
            .to_string()]);
        vm.set_layout_rects(vec![(a, 0.0, 0.0, 300.0, 40.0)]);
        vm.set_layout_rects(vec![(a, 0.0, 0.0, 300.0, 40.0)]); // no change
        vm.set_layout_rects(vec![(a, 0.0, 0.0, 300.0, 90.0)]);
        assert_eq!(
            vm.run_scripts(&["console.log(log.join('|'));".to_string()]),
            vec!["300x40|300x90".to_string()],
        );
    }

    /// Not a test: a bytecode microscope. Set GG_DUMP_SRC to a JS
    /// file and GG_DUMP_PAT to a function-name substring, run with
    /// --nocapture, and every matching proto's annotated disassembly
    /// prints. Built for the shopsquare stall (docs/shopsquare-
    /// hydration-stall.md): a branch the interpreter demonstrably
    /// skips can only be read at this level.
    #[test]
    fn zz_dump_bytecode() {
        let Ok(path) = std::env::var("GG_DUMP_SRC") else { return };
        let pat = std::env::var("GG_DUMP_PAT")
            .unwrap_or_else(|_| "registerChunk".to_string());
        let src = std::fs::read_to_string(&path).unwrap();
        let mut vm = PageVm::new(None);
        // Drive the real runtime deterministically: register the
        // runtime chunk (one pending dependency), then register that
        // dependency afterwards so the gate await resolves across a
        // real turn boundary, like on the page. If the branch runs,
        // the runtime must complain that module 21479's factory is
        // missing -- silence instead means the stall reproduced.
        let prefix = "https://spastatic.naver.com/v1/shopad/static/\
                      shopad-spblock-node/v260720-153933/_next/";
        let prelude = format!(
            "var __stub = function (p) {{\
               return {{ getAttribute: function () {{\
                 return '{prefix}' + p; }} }};\
             }};\
             globalThis.TURBOPACK = [\
               [__stub('static/chunks/turbopack-x.js'),\
                {{ otherChunks: ['static/chunks/dep.js'],\
                   runtimeModuleIds: [21479] }}]\
             ];",
        );
        let kick =
            "console.log('[kick] registering dep');\
             TURBOPACK.push([__stub('static/chunks/dep.js'), {}]);\
             console.log('[kick] done');"
                .to_string();
        for l in vm.run_scripts(&[prelude, src]) {
            eprintln!("log: {l}");
        }
        for l in vm.run_scripts(&[kick]) {
            eprintln!("log: {l}");
        }
        for _ in 0..4 {
            let (logs, _reqs) = vm.pump();
            for l in logs {
                eprintln!("pump: {l}");
            }
            if !vm.has_pending_work() {
                break;
            }
        }
        for mi in 0..vm.mods.len() as u32 {
            let m = vm.mods.rc(mi);
            for (pi, p) in m.module.protos.iter().enumerate() {
                if !p.name.contains(&pat) {
                    continue;
                }
                eprintln!(
                    "=== m{mi} p{pi} name={} nparams={} nregs={} \
                     lazy={} arrow={} len={}",
                    p.name, p.nparams, p.nregs, p.lazy.is_some(),
                    p.is_arrow, p.code.len(),
                );
                for (k, ins) in p.code.iter().enumerate() {
                    eprintln!(
                        "{k:4}: {:?}{}",
                        ins,
                        crate::jsvm::vm::annotate_instr(
                            &vm.st, &m.global_map, &p.consts, ins,
                        ),
                    );
                }
            }
        }
    }

    #[test]
    fn a_bounded_drain_never_eats_a_microtask() {
        // The exhausted iteration used to pop-then-return, silently
        // discarding one queued reaction per bounded drain. A reaction
        // that vanishes neither runs nor rejects -- the awaiting chain
        // hangs forever with nothing pending, which is how turbopack's
        // chunk gate on naver lost its Promise.all continuation and
        // the shopping module never hydrated (nondeterministically:
        // it needed the queue to be exactly deep enough at the time).
        let mut vm = PageVm::new(None);
        vm.run_scripts(&["var ran = [];".to_string()]);
        vm.run_source(
            "for (var i = 0; i < 10; i++) {\
               (function (k) {\
                 Promise.resolve().then(function () { ran.push(k); });\
               })(i);\
             }",
        )
        .unwrap();
        // four bounded drains of 3: with the off-by-one, the fourth
        // job of each exhausted drain was eaten and order broke
        for _ in 0..4 {
            crate::jsvm::vm::drain_microtasks_now(
                &mut vm.st, &vm.mods, 3,
            );
        }
        assert_eq!(
            vm.run_scripts(&["console.log(ran.join(','));".to_string()]),
            vec!["0,1,2,3,4,5,6,7,8,9".to_string()],
        );
    }

    #[test]
    fn builtin_method_reads_are_identity_stable() {
        // Every read of a builtin used to mint a fresh function
        // object, so `a.push === a.push` was false and every
        // "has someone overridden this?" probe saw a phantom override
        // -- which is exactly how the shopsquare diagnosis went wrong.
        let doc = Rc::new(RefCell::new(crate::html::parse(
            "<div id=d></div>",
        )));
        let mut vm = PageVm::new(Some(doc));
        assert_eq!(
            vm.run_scripts(&["var a = [1];\
                var el = document.getElementById('d');\
                console.log(a.push === a.push,\
                            [].map === [].map,\
                            el.focus === el.focus,\
                            ''.hasOwnProperty === ''.hasOwnProperty);\
                var mine = function () { return 7; };\
                a.push = mine;\
                console.log(a.push === mine, a.push(9), a.length);"
                .to_string()]),
            vec![
                "true true true true".to_string(),
                "true 7 1".to_string(),
            ],
        );
    }

    #[test]
    fn a_stored_function_overrides_a_dom_method_for_calls_too() {
        // The read path preferred expandos all along; the call path
        // dispatched straight to the builtin, so a polyfill's wrapper
        // was read back but never invoked.
        let doc = Rc::new(RefCell::new(crate::html::parse(
            "<div id=d>x</div>",
        )));
        let mut vm = PageVm::new(Some(doc));
        assert_eq!(
            vm.run_scripts(&["var seen = [];\
                var real = document.addEventListener;\
                document.addEventListener = function (t, f) {\
                  seen.push('doc:' + t);\
                };\
                document.addEventListener('ping', function () {});\
                var el = document.getElementById('d');\
                el.getAttribute = function (n) { return 'wrapped:' + n; };\
                console.log(seen.join('|'), el.getAttribute('id'),\
                            typeof real);"
                .to_string()]),
            vec!["doc:ping wrapped:id function".to_string()],
        );
    }

    #[test]
    fn direct_string_calls_cover_the_same_surface_as_extraction() {
        // s.lastIndexOf('/') threw "cannot call .lastIndexOf() on a
        // string (yet)" -- the direct-call arm lagged behind the
        // extracted-builtin dispatcher. Same for the locale aliases,
        // the legacy trim pair, and valueOf.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["var u = 'a/b/c.js';\
                console.log(u.lastIndexOf('/'), u.lastIndexOf('zz'));\
                console.log('MiX'.toLocaleLowerCase(),\
                            'MiX'.toLocaleUpperCase());\
                console.log('[' + '  x  '.trimLeft() + '|'\
                            + '  x  '.trimRight() + ']');\
                console.log('abc'.valueOf() === 'abc');"
                .to_string()]),
            vec![
                "3 -1".to_string(),
                "mix MIX".to_string(),
                "[x  |  x]".to_string(),
                "true".to_string(),
            ],
        );
    }

    #[test]
    fn get_elements_by_name_answers_on_the_document() {
        // React 19 dedupes hoistable resources through
        // document.getElementsByName; recoshopping's hydration died on
        // the missing method before it could commit anything.
        let doc = Rc::new(RefCell::new(crate::html::parse(
            "<form name=f1></form><input name=q><meta name=q>",
        )));
        let mut vm = PageVm::new(Some(doc));
        assert_eq!(
            vm.run_scripts(&["var q = document.getElementsByName('q');\
                console.log(q.length, q[0].tagName, q[1].tagName,\
                            document.getElementsByName('f1').length,\
                            document.getElementsByName('zz').length,\
                            typeof document.body.getElementsByName);"
                .to_string()]),
            vec!["2 INPUT META 1 0 undefined".to_string()],
        );
    }

    #[test]
    fn attach_shadow_degrades_to_the_light_tree() {
        // No shadow DOM here: the returned "root" is the host itself,
        // so a web component's content mounts where it can render.
        let doc = Rc::new(RefCell::new(crate::html::parse(
            "<div id=host></div>",
        )));
        let mut vm = PageVm::new(Some(doc));
        assert_eq!(
            vm.run_scripts(&["var el = document.getElementById('host');\
                var root = el.attachShadow({ mode: 'open' });\
                var p = document.createElement('p');\
                p.id = 'inner';\
                root.appendChild(p);\
                console.log(root === el, el.shadowRoot === root,\
                            root.host === el,\
                            document.getElementById('inner') !== null,\
                            el.children.length);"
                .to_string()]),
            vec!["true true true true 1".to_string()],
        );
    }

    #[test]
    fn crypto_mints_random_bytes_and_uuids() {
        // uuid v4 refuses to run without crypto.getRandomValues; the
        // shopping modules mint ids through it during hydration.
        let mut vm = PageVm::new(None);
        let out = vm.run_scripts(&["var a = new Uint8Array(16);\
            var r = crypto.getRandomValues(a);\
            var u = crypto.randomUUID();\
            console.log(r === a, a.length,\
                        /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab]\
[0-9a-f]{3}-[0-9a-f]{12}$/.test(u));"
            .to_string()]);
        assert_eq!(out, vec!["true 16 true".to_string()]);
    }

    #[test]
    fn the_attributes_map_is_live_and_identity_stable() {
        // React 19's HostSingleton unmount is
        //   for (e = n.attributes; e.length;) n.removeAttributeNode(e[0])
        // -- it never re-reads `.attributes`, so the captured map must
        // shrink as attributes are removed or the loop never exits.
        // On recoshopping that one loop ate the whole 400M-instruction
        // budget and hydration died mid-flight.
        let doc = Rc::new(RefCell::new(crate::html::parse(
            "<div id=x class=c data-a=1 data-b=2></div>",
        )));
        let mut vm = PageVm::new(Some(doc));
        assert_eq!(
            vm.run_scripts(&["var el = document.getElementById('x');\
                var m = el.attributes;\
                var start = m.length;\
                var rounds = 0;\
                while (m.length) {\
                  el.removeAttributeNode(m[0]);\
                  if (++rounds > 50) break;\
                }\
                console.log(start, rounds, m.length,\
                            el.attributes === m,\
                            el.hasAttributes());"
                .to_string()]),
            vec!["4 4 0 true false".to_string()],
        );
    }

    #[test]
    fn building_a_string_by_appending_is_charged_once_not_squared() {
        // The heap backstop measures retained bytes. A rope Cat node
        // retains ~32 bytes, but concat used to charge the combined
        // length per node, billing an appended string as the sum of
        // every prefix -- O(n^2) phantom bytes. 200 x 4KB appends is
        // ~800KB of real text; the old accounting called it ~82MB and
        // pages died of "string heap exhausted" depending on which ad
        // script built a payload this way.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["var chunk = 'x'.repeat(4096);\
                var s = '';\
                for (var i = 0; i < 200; i++) s = s + chunk;\
                console.log(s.length, s.indexOf('y'));"
                .to_string()]),
            vec!["819200 -1".to_string()],
        );
        let used = vm.heap_bytes();
        assert!(
            used < 8 * 1024 * 1024,
            "heap accounting ballooned: {used} bytes for ~800KB of text",
        );
    }

    #[test]
    fn a_missing_argument_is_undefined_even_with_arguments() {
        // A function that reads `arguments` keeps the args past its
        // parameter list -- but blanking the activation from
        // max(argc, nparams) also spared the *missing* parameters, so
        // they read whatever the previous call had left in that
        // register window. naver's event dispatcher is exactly this
        // shape: `fire(t, e) { e = e || {}; ... arguments.length }`
        // called as `fire('x')` found a leftover string in `e`, skipped
        // its own initializer, and threw on the next `e.q.push()`.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["function fire(t, e) {\
                    e = e || { q: [] };\
                    e.q.push(t);\
                    var n = arguments.length;\
                    for (var i = 2; i < n; i++) e.q.push(arguments[i]);\
                    return e.q.join(',') + '/' + n;\
                };\
                function seed(a, b) { return a + b; };\
                seed('leftover', 'junk');\
                console.log(fire('one'));\
                console.log(fire('two', { q: ['pre'] }));\
                console.log(fire('three', null, 'x', 'y'));"
                .to_string()]),
            vec![
                "one/1".to_string(),
                "pre,two/2".to_string(),
                "three,x,y/4".to_string(),
            ],
        );
    }

    #[test]
    fn an_async_body_runs_up_to_its_first_await() {
        // The whole body used to be deferred behind
        // `Promise.resolve().then(...)`, putting everything before the
        // first await one tick later than the spec does. Observable:
        // turbopack registers a chunk in that prologue, and its module
        // factories read `document.currentScript`.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["var order = [];\
                async function f() { order.push('body'); await 0;\
                                     order.push('after'); }\
                order.push('before');\
                f();\
                order.push('caller');\
                console.log(order.join(','));\
                async function boom() { throw new Error('sync'); }\
                boom().then(function () { console.log('NOT REACHED'); },\
                            function (e) { console.log('rejected ' + e.message); });\
                async function val() { return 7; }\
                val().then(function (v) { console.log('value ' + v); });\
                async function plain() { return await 42; }\
                plain().then(function (v) { console.log('awaited ' + v); });"
                .to_string()]),
            vec!["before,body,caller".to_string()],
        );
        let (logs, _) = vm.pump();
        let mut logs = logs;
        logs.sort();
        assert_eq!(
            logs,
            vec![
                // `await 42` must not call .then on a number
                "awaited 42".to_string(),
                "rejected sync".to_string(),
                "value 7".to_string(),
            ],
        );
    }

    #[test]
    fn an_if_branch_can_return_across_an_await() {
        // `if (c) return void await f();` followed by anything -- the
        // shape of turbopack's chunk loader -- did not compile at all.
        let mut vm = PageVm::new(None);
        let out = vm.run_scripts(&[
            "function p(v) { return Promise.resolve(v); }\
             async function loader(hit) {\
                 if (hit) return void await p('early');\
                 var tail = await p('tail');\
                 return tail;\
             }\
             loader(true).then(function (v) { console.log('hit=' + v); });\
             loader(false).then(function (v) { console.log('miss=' + v); });\
             async function shadow() { let shadow = 'inner'; await 0;\
                                       return shadow; }\
             shadow().then(function (v) { console.log('shadow=' + v); });"
                .to_string(),
        ]);
        assert!(out.is_empty(), "{out:?}");
        let (logs, _) = vm.pump();
        let mut logs = logs;
        logs.sort();
        assert_eq!(
            logs,
            vec![
                "hit=undefined".to_string(),
                "miss=tail".to_string(),
                // a function body may shadow the function's own name
                "shadow=inner".to_string(),
            ],
        );
    }

    #[test]
    fn a_rejection_nobody_handles_is_reported() {
        // An error swallowed by a promise was completely silent, which
        // is the worst way for a bundle to fail: the turbopack loader
        // failed to compile and nothing said so.
        let mut vm = PageVm::new(None);
        assert!(vm
            .run_scripts(&["Promise.reject(new Error('nobody catches me'));\
                Promise.reject(new Error('but this one is')).catch(\
                    function () {});"
                .to_string()])
            .is_empty());
        let (logs, _) = vm.pump();
        assert_eq!(
            logs,
            vec!["[gg-js error] unhandled rejection: \
                  Error: nobody catches me"
                .to_string()],
        );
    }

    #[test]
    fn byte_streams_round_trip_through_typed_arrays() {
        // React's flight client decodes its payload with
        // TextDecoder over Uint8Array views of a ReadableStream, and
        // scans for row terminators with `chunk.indexOf(10, at)`. None
        // of that existed: no Uint8Array at all, TextEncoder returned
        // a plain array, and TextDecoder returned the empty string.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["var enc = new TextEncoder();\
                var u = enc.encode('{\"k\":\"\\uc548\\ub155\"}');\
                console.log(new TextDecoder().decode(u));\
                console.log(u.length, u.byteLength, u.byteOffset);\
                var v = new Uint8Array(u.buffer, 1, 3);\
                console.log(v.length, new TextDecoder().decode(v));\
                console.log(u.indexOf(107), u.indexOf(107, 3));\
                var parts = enc.encode('ab\\ncd');\
                console.log(parts.indexOf(10), parts.indexOf(10, 3));\
                var joined = new Uint8Array(4);\
                joined.set(enc.encode('ab'), 0);\
                joined.set(enc.encode('cd'), 2);\
                console.log(new TextDecoder().decode(joined));\
                var d = new TextDecoder();\
                var k = enc.encode('\\uac00\\ub098');\
                console.log(d.decode(k.subarray(0, 4), { stream: true })\
                            + d.decode(k.subarray(4), { stream: true }));"
                .to_string()]),
            vec![
                "{\"k\":\"안녕\"}".to_string(),
                // {"k":"안녕"} -- 8 ASCII bytes plus 3 each for 안녕
                "14 14 0".to_string(),
                "3 \"k\"".to_string(),
                // indexOf honours its fromIndex
                "2 -1".to_string(),
                "2 -1".to_string(),
                "abcd".to_string(),
                // a multi-byte char split across chunks survives
                "가나".to_string(),
            ],
        );
    }

    #[test]
    fn a_readable_stream_delivers_its_chunks() {
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["var s = new ReadableStream({ start: function (c) {\
                    c.enqueue('one'); c.enqueue('two'); c.close(); } });\
                var r = s.getReader(), got = [];\
                function step(x) {\
                    if (x.done) { console.log(got.join(',')); return; }\
                    got.push(x.value);\
                    r.read().then(step);\
                }\
                r.read().then(step);\
                var late = new ReadableStream({ start: function (c) {\
                    setTimeout(function () { c.enqueue('late'); c.close(); }, 0);\
                } });\
                late.getReader().read().then(function (x) {\
                    console.log('late=' + x.value); });"
                .to_string()]),
            Vec::<String>::new(),
        );
        let (logs, _) = vm.pump();
        let mut logs = logs;
        logs.sort();
        assert_eq!(
            logs,
            vec!["late=late".to_string(), "one,two".to_string()],
        );
    }

    #[test]
    fn json_parse_runs_its_reviver() {
        // React's flight rows arrive as plain arrays and the reviver
        // turns the `"$"`-tagged ones into elements. Ignoring it hands
        // React raw objects, which it refuses to render (error #31).
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["var seen = [];\
                var out = JSON.parse('{\"a\":1,\"b\":[2,3],\"c\":{\"d\":\"x\"}}',\
                    function (k, v) { seen.push(k); return v; });\
                console.log(seen.join('|'));\
                console.log(JSON.stringify(out));\
                console.log(JSON.stringify(JSON.parse('{\"n\":1}',\
                    function (k, v) {\
                        return typeof v === 'number' ? v * 10 : v; })));\
                console.log(JSON.stringify(JSON.parse('{\"k\":1,\"d\":2}',\
                    function (k, v) { return k === 'd' ? undefined : v; })));\
                console.log(JSON.stringify(JSON.parse(\
                    '[\"$\",\"link\",0,{\"rel\":\"x\"}]',\
                    function (k, v) {\
                        return Array.isArray(v) && v[0] === '$'\
                            ? { tag: v[1] } : v; })));"
                .to_string()]),
            vec![
                // depth-first, children before their holder, root last
                "a|0|1|b|d|c|".to_string(),
                r#"{"a":1,"b":[2,3],"c":{"d":"x"}}"#.to_string(),
                r#"{"n":10}"#.to_string(),
                // returning undefined deletes the property
                r#"{"k":1}"#.to_string(),
                r#"{"tag":"link"}"#.to_string(),
            ],
        );
    }

    #[test]
    fn a_function_enumerates_its_own_properties() {
        // Statics live in a side table that Object.keys and for-in
        // skipped, so a function carrying data enumerated as empty.
        // webpack gates every entry module on
        // `Object.keys(__webpack_require__.O).every(check)`, and an
        // empty key list makes `every` vacuously true -- entry modules
        // start before their chunks are registered and the page dies
        // on `__webpack_modules__[id].call` of undefined. That is what
        // left naver's shopping boxes empty.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["function O() {}\
                O.j = function (id) { return id === 'ready'; };\
                O.k = 7;\
                console.log(Object.keys(O).sort().join(','));\
                console.log(Object.values(O).length,\
                            Object.entries(O).length);\
                var ks = []; for (var k in O) ks.push(k);\
                console.log(ks.sort().join(','));\
                console.log(Object.keys(O).every(function (n) {\
                    return n === 'j' || n === 'k'; }));\
                function bare() {}\
                console.log(Object.keys(bare).length,\
                            'prototype' in bare);\
                var gate = function () {};\
                gate.a = function (id) { return id === 1; };\
                console.log(Object.keys(gate).every(\
                    function (n) { return gate[n](2); }));"
                .to_string()]),
            vec![
                "j,k".to_string(),
                "2 2".to_string(),
                "j,k".to_string(),
                "true".to_string(),
                // `prototype` exists but is not enumerable
                "0 true".to_string(),
                // the gate now actually runs its check
                "false".to_string(),
            ],
        );
    }

    #[test]
    fn error_called_without_new_still_builds_an_error() {
        // `Error(m)` as a plain call is legal, and webpack's chunk
        // loader does exactly that (`var u = Error()`), with no
        // receiver -- so the constructor must not assume `this`.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["var e = Error('boom');\
                console.log(e instanceof Error, e.message);\
                console.log(typeof Error().stack);\
                var f = Error;\
                console.log(f('x').message);\
                console.log(TypeError('t').message);"
                .to_string()]),
            vec![
                "true boom".to_string(),
                "string".to_string(),
                "x".to_string(),
                "t".to_string(),
            ],
        );
    }

    #[test]
    fn a_string_owns_its_character_indices() {
        // `for (k in v) return false` is how bundles write "is this
        // empty?", so a string that enumerates nothing reads as empty.
        // gfp-display-sdk filters its ad request that way and dropped
        // every string parameter -- including the ad unit id, which
        // the ad server then rejected with `invalid inventory(adUnit)`.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["var ks = [];\
                for (var k in 'abc') ks.push(k);\
                console.log(ks.join(','));\
                console.log(Object.keys('abc').join(','));\
                console.log(Object.values('ab').join(','));\
                console.log(JSON.stringify(Object.entries('ab')));\
                var h = Object.prototype.hasOwnProperty;\
                console.log(h.call('abc', '0'), h.call('abc', '9'),\
                            h.call('abc', 'length'), h.call('', '0'));\
                console.log('0' in Object('abc'));\
                var empty = [];\
                for (var e in '') empty.push(e);\
                console.log(empty.length);"
                .to_string()]),
            vec![
                "0,1,2".to_string(),
                "0,1,2".to_string(),
                "a,b".to_string(),
                r#"[["0","a"],["1","b"]]"#.to_string(),
                "true false true false".to_string(),
                "true".to_string(),
                "0".to_string(),
            ],
        );
    }

    #[test]
    fn an_await_does_not_jump_ahead_of_its_siblings() {
        // `await` is desugared by hoisting it into a preceding
        // statement, which lifted it *above* the operands to its left.
        // Minifiers emit the whole body as one comma sequence, so
        // `this.x = new T(), await this.x.load()` ran the await first
        // and called .load() on undefined -- that is what broke every
        // ad slot on naver.com.
        let mut vm = PageVm::new(None);
        let out = vm.run_scripts(&[
            "var log = [];\
             function a() { log.push('a'); return 1; }\
             function b() { log.push('b'); return Promise.resolve(2); }\
             async function seq() { return (a(), await b()); }\
             async function assign(o) {\
                 return o.m = { load: function () {\
                     return Promise.resolve('LOADED'); } },\
                     await o.m.load(); }\
             async function args(o) {\
                 return o.tag = 'T',\
                     o.join('-', await Promise.resolve(2)); }\
             seq().then(function (v) { console.log(log.join(',') + '|' + v); });\
             assign({}).then(function (v) { console.log(v); });\
             args({ join: function (s, n) { return this.tag + s + n; } })\
                 .then(function (v) { console.log(v); });"
                .to_string(),
        ]);
        // nothing resolves until the microtask queue is drained
        assert!(out.is_empty(), "{out:?}");
        let (logs, _) = vm.pump();
        assert_eq!(
            logs,
            vec![
                "a,b|2".to_string(),  // left-to-right, not b,a
                "LOADED".to_string(), // the assignment happened first
                // an awaited argument must not unbind the receiver
                "T-2".to_string(),
            ],
        );
    }

    #[test]
    fn errors_carry_a_call_stack() {
        // Bundles report `e.stack` and nothing else when they swallow
        // an error; undefined there makes a shipped minified failure
        // undiagnosable. Both a constructed Error and an engine-raised
        // TypeError have to carry one.
        let mut vm = PageVm::new(None);
        let out = vm.run_scripts(&[
            "function inner() { return new Error('boom'); }\
             function middle() { return inner(); }\
             function outer() { return middle(); }\
             console.log(outer().stack);\
             function bad() { return null.x; }\
             function wrapper() { return bad(); }\
             try { wrapper(); } catch (e) { console.log(e.stack); }"
                .to_string(),
        ]);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].starts_with("Error: boom\n    at "), "{:?}", out[0]);
        // the chain above the throw is reported, in order
        assert!(out[0].contains("at middle\n    at outer"), "{:?}", out[0]);
        assert!(
            out[1].starts_with("TypeError: cannot read .x of null\n"),
            "{:?}", out[1],
        );
        assert!(out[1].contains("at wrapper"), "{:?}", out[1]);
    }

    #[test]
    fn abort_signal_is_a_global_with_working_listeners() {
        // gfp-display-sdk builds every ad request's timeout out of the
        // AbortSignal *global*; without it each slot failed with
        // "AbortSignal is not defined" and naver removed the container.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["console.log(typeof AbortSignal);\
                var c = new AbortController();\
                console.log(c.signal instanceof AbortSignal, c.signal.aborted);\
                var seen = [];\
                c.signal.addEventListener('abort', function (e) {\
                    seen.push(e.type); });\
                c.signal.onabort = function () { seen.push('onabort'); };\
                c.abort('stop');\
                console.log(c.signal.aborted, c.signal.reason, seen.join(','));\
                c.abort('again');\
                console.log(seen.length);\
                console.log(AbortSignal.abort().aborted,\
                            AbortSignal.abort().reason.name);\
                var any = AbortSignal.any([new AbortController().signal,\
                                           c.signal]);\
                console.log(any.aborted, any.reason);\
                try { c.signal.throwIfAborted(); }\
                catch (e) { console.log('threw ' + e); }"
                .to_string()]),
            vec![
                "function".to_string(),
                "true false".to_string(),
                "true stop onabort,abort".to_string(),
                "2".to_string(),
                "true AbortError".to_string(),
                "true stop".to_string(),
                "threw stop".to_string(),
            ],
        );
    }

    #[test]
    fn string_coercion_of_an_object_runs_its_tostring() {
        // `String(x)` and `''.concat(x)` both dumped the raw object
        // instead of performing ToString, so a page that logged
        // `'[Ad] '.concat(err)` printed `[object Object]` and threw
        // away the only description of what had failed.
        let mut vm = PageVm::new(None);
        assert_eq!(
            vm.run_scripts(&["console.log(String(new Error('boom')));\
                              console.log('X: '.concat(new Error('boom')));\
                              console.log(''.concat({toString:function(){\
                                  return 'CUSTOM';}}));\
                              console.log(''.concat({valueOf:function(){\
                                  return 42;}}));\
                              console.log('a'.concat(1, [2,3], null));\
                              try { null.x } catch (e) {\
                                  console.log(''.concat(e)); }"
                .to_string()]),
            vec![
                "Error: boom".to_string(),
                "X: Error: boom".to_string(),
                "CUSTOM".to_string(),
                // no toString, so ToPrimitive falls through to valueOf
                "42".to_string(),
                "a12,3null".to_string(),
                "TypeError: cannot read .x of null".to_string(),
            ],
        );
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
    fn memory_bombs_throw_range_errors_instead_of_aborting() {
        // rope doubling: ~30 statements reach gigabytes well under the
        // fuel budget; the engine must throw a catchable RangeError,
        // never abort the process on a failed allocation (namuwiki's
        // Cloudflare challenge script killed the renderer this way)
        let mut vm = PageVm::new(None);
        let logs = vm.run_scripts(&[
            "var s = 'a';\n\
             try { while (true) { s += s; } }\n\
             catch (e) { console.log('caught ' + e.name); }\n\
             console.log('len ' + (s.length <= 67108864));"
                .to_string(),
        ]);
        assert!(logs.contains(&"caught RangeError".to_string()), "{logs:?}");
        assert!(logs.contains(&"len true".to_string()), "{logs:?}");

        // repeat / padStart with hostile counts fail the same way
        for src in [
            "try { 'ab'.repeat(1e9); console.log('no'); } \
             catch (e) { console.log('caught ' + e.name); }",
            "try { 'x'.padStart(1e9); console.log('no'); } \
             catch (e) { console.log('caught ' + e.name); }",
        ] {
            let mut vm = PageVm::new(None);
            let logs = vm.run_scripts(&[src.to_string()]);
            assert!(
                logs.contains(&"caught RangeError".to_string()),
                "{src}: {logs:?}"
            );
        }

        // split('') of a huge string materializes per-char strings —
        // capped instead of exploding
        let mut vm = PageVm::new(None);
        let logs = vm.run_scripts(&[
            "var s = 'abcdefgh';\n\
             for (var i = 0; i < 21; i++) s += s; // 16M chars\n\
             try { s.split(''); console.log('no'); }\n\
             catch (e) { console.log('caught ' + e.name); }"
                .to_string(),
        ]);
        assert!(logs.contains(&"caught RangeError".to_string()), "{logs:?}");

        // giant array length / sparse index (gov.kr) throw RangeError
        // instead of forcing a hundred-MB dense allocation
        for src in [
            "try { var a = []; a.length = 1e8; console.log('no'); } \
             catch (e) { console.log('caught ' + e.name); }",
            "try { var a = []; a[1e8] = 1; console.log('no'); } \
             catch (e) { console.log('caught ' + e.name); }",
            "try { var a = []; a.length = -1; console.log('no'); } \
             catch (e) { console.log('caught ' + e.name); }",
        ] {
            let mut vm = PageVm::new(None);
            let logs = vm.run_scripts(&[src.to_string()]);
            assert!(
                logs.contains(&"caught RangeError".to_string()),
                "{src}: {logs:?}"
            );
        }
        // a modest length set still works
        let (v, _) = eval("var a = []; a.length = 5; a.length").unwrap();
        assert_eq!(v.to_number_raw(), 5.0);

        // the VM survives and keeps executing after every bomb
        let mut vm = PageVm::new(None);
        let logs = vm.run_scripts(&[
            "try { var q = 'q'; while (true) q += q; } catch (e) {}"
                .to_string(),
            "console.log('alive ' + (6 * 7));".to_string(),
        ]);
        assert!(logs.contains(&"alive 42".to_string()), "{logs:?}");
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

    #[test]
    fn host_form_events_bubble_and_are_cancelable() {
        let (mut vm, doc) = dom_vm(
            "<html><body><div id=outer><form id=f>\
             <button id=s>send</button></form></div></body></html>",
        );
        vm.run_scripts(&["\
            var f = document.getElementById('f');\n\
            f.addEventListener('submit', function (e) {\n\
              console.log(e.type + '|' + e.target.id + '|' +\n\
                e.currentTarget.id + '|' + e.submitter.id + '|' +\n\
                e.bubbles + '|' + e.cancelable);\n\
              e.preventDefault();\n\
            });\n\
            document.getElementById('outer').addEventListener(\n\
              'submit', function () { console.log('bubbled'); });\n"
            .to_string()]);
        let form = doc.borrow().get_element_by_id("f").unwrap();
        let submitter = doc.borrow().get_element_by_id("s").unwrap();
        let (logs, handled, prevented) = vm.dispatch_event(
            form,
            "submit",
            true,
            true,
            Some(submitter),
        );
        assert!(handled && prevented);
        assert_eq!(
            logs,
            vec!["submit|f|f|s|true|true", "bubbled"],
        );

        vm.run_scripts(&["\
            document.getElementById('f').addEventListener(\n\
              'invalid', function () { console.log('bad bubble'); });\n"
            .to_string()]);
        let (logs, handled, prevented) = vm.dispatch_event(
            submitter,
            "invalid",
            false,
            true,
            None,
        );
        assert!(!handled && !prevented);
        assert!(logs.is_empty());
    }
}
