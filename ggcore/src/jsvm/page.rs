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
    self, call_value, call_value_this, exec, has_pending_work, host,
    make_native, new_plain_object, pump, pump_step, raw_set_prop,
    reject_fetch, resolve_fetch, Ids, ModStore, Native, St, DOC_NODE,
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
function Error(m, o) {
  if (m !== undefined) this.message = '' + m;
  if (o && typeof o === 'object' && 'cause' in o) this.cause = o.cause;
}
Error.prototype.name = 'Error';
Error.prototype.message = '';
Error.prototype.toString = function () {
  return this.message ? this.name + ': ' + this.message : this.name;
};
function TypeError(m, o) { Error.call(this, m, o); }
TypeError.prototype = new Error();
TypeError.prototype.name = 'TypeError';
function RangeError(m, o) { Error.call(this, m, o); }
RangeError.prototype = new Error();
RangeError.prototype.name = 'RangeError';
function SyntaxError(m, o) { Error.call(this, m, o); }
SyntaxError.prototype = new Error();
SyntaxError.prototype.name = 'SyntaxError';
function ReferenceError(m, o) { Error.call(this, m, o); }
ReferenceError.prototype = new Error();
ReferenceError.prototype.name = 'ReferenceError';
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
// path. Intentionally absent: Proxy and Reflect (their absence routes
// Babel/core-js to safer fallbacks than a half-stub would).
function MutationObserver(cb) { this._cb = cb; }
MutationObserver.prototype.observe = function () {};
MutationObserver.prototype.disconnect = function () {};
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
function ResizeObserver(cb) { this._cb = cb; }
ResizeObserver.prototype.observe = function () {};
ResizeObserver.prototype.unobserve = function () {};
ResizeObserver.prototype.disconnect = function () {};
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
function AbortController() {
  this.signal = { aborted: false,
    addEventListener: function () {},
    removeEventListener: function () {} };
}
AbortController.prototype.abort = function () {
  this.signal.aborted = true;
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
function TextEncoder() {}
TextEncoder.prototype.encode = function (s) {
  s = '' + s;
  var out = [];
  for (var i = 0; i < s.length; i++) {
    var c = s.charCodeAt(i);
    if (c < 128) { out.push(c); }
    else if (c < 2048) {
      out.push(192 | (c >> 6), 128 | (c & 63));
    } else {
      out.push(224 | (c >> 12), 128 | ((c >> 6) & 63),
               128 | (c & 63));
    }
  }
  return out;
};
function TextDecoder() {}
TextDecoder.prototype.decode = function () { return ''; };
function Worker() {}
Worker.prototype.postMessage = function () {};
Worker.prototype.terminate = function () {};
Worker.prototype.addEventListener = function () {};
function XMLHttpRequest() {
  this.readyState = 0;
  this.status = 0;
  this.responseText = '';
  this.response = '';
  this._headers = {};
}
XMLHttpRequest.prototype.open = function (method, url) {
  this._method = method;
  this._url = url;
  this.readyState = 1;
};
XMLHttpRequest.prototype.setRequestHeader = function (k, v) {
  this._headers[k] = v;
};
XMLHttpRequest.prototype.getResponseHeader = function () {
  return null;
};
XMLHttpRequest.prototype.abort = function () {};
XMLHttpRequest.prototype.addEventListener = function (ty, cb) {
  if (ty === 'load') this.onload = cb;
  if (ty === 'error') this.onerror = cb;
};
XMLHttpRequest.prototype.send = function () {
  var self = this;
  fetch(this._url).then(function (r) {
    return r.text();
  }).then(function (t) {
    self.readyState = 4;
    self.status = 200;
    self.responseText = t;
    self.response = t;
    if (self.onreadystatechange) self.onreadystatechange();
    if (self.onload) self.onload();
  }, function (e) {
    self.readyState = 4;
    self.status = 0;
    if (self.onreadystatechange) self.onreadystatechange();
    if (self.onerror) self.onerror(e);
  });
};
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
  if (arguments.length === 0) { this._t = __ggDateNow(); }
  else if (arguments.length === 1) {
    if (typeof a === 'number') { this._t = a; }
    else if (a instanceof Date) { this._t = a._t; }
    else { this._t = Date.parse('' + a); }
  } else {
    this._t = Date.UTC(a, b, c === undefined ? 1 : c,
                       d || 0, e || 0, f || 0, g || 0);
  }
}
Date.now = __ggDateNow;
Date.UTC = function (y, m, d, h, mi, s, ms) {
  return __gg_days(+y, (+m || 0) + 1, d === undefined ? 1 : +d)
    * 86400000 + (h || 0) * 3600000 + (mi || 0) * 60000
    + (s || 0) * 1000 + (ms || 0);
};
Date.parse = function (s) {
  s = ('' + s).replace('T', ' ');
  var m = /^(\d{4})-(\d{2})-(\d{2})(?:[ ](\d{2}):(\d{2})(?::(\d{2})(?:\.(\d{1,3}))?)?)?/.exec(s);
  if (!m) return NaN;
  return Date.UTC(+m[1], +m[2] - 1, +m[3], +(m[4] || 0),
                  +(m[5] || 0), +(m[6] || 0), +(m[7] || 0));
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
            ("clz32", Native::HostFn(host::M_CLZ32)),
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
        for m in ["postMessage", "scrollTo"] {
            let noop = make_native(&mut vm.st, Native::Noop);
            vm.set_object_prop(window, m, noop);
        }
        vm.st.known.window = window;
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
                ("userAgent",
                 "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                  AppleWebKit/537.36 (KHTML, like Gecko) \
                  GGBrowser/0.1"),
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
        let fetches = pump(&mut self.st, &self.mods, PUMP_BUDGET);
        (std::mem::take(&mut self.st.logs), fetches)
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
    }

    pub fn run_source(&mut self, src: &str) -> Result<Value, String> {
        let ast =
            parser::parse_program(src).map_err(|e| format!("{e:?}"))?;
        let module =
            compiler::compile(&ast).map_err(|e| format!("{e:?}"))?;
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
                // this = the node whose listener is running (the
                // currentTarget), matching dispatchEvent's behavior
                if let Err(e) = call_value_this(
                    &mut self.st,
                    &self.mods,
                    h,
                    Some(Value::dom_node(node)),
                    &[ev],
                ) {
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
    /// Fire the document lifecycle: readyState -> interactive,
    /// DOMContentLoaded (document + window), readyState -> complete,
    /// then window `load`. The loader calls this once after all
    /// scripts ran — app bundles bootstrap from these.
    pub fn fire_lifecycle(&mut self) -> Vec<String> {
        self.st.ready_state = "interactive";
        self.dispatch_simple(vm::DOC_NODE, "domcontentloaded");
        self.dispatch_simple(vm::WINDOW_NODE, "domcontentloaded");
        self.st.ready_state = "complete";
        self.dispatch_simple(vm::WINDOW_NODE, "load");
        self.dispatch_simple(vm::DOC_NODE, "load");
        std::mem::take(&mut self.st.logs)
    }

    fn dispatch_simple(&mut self, node: u32, ty: &str) {
        let cbs = self
            .st
            .listeners
            .get(&(node, ty.to_string()))
            .cloned()
            .unwrap_or_default();
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
            if let Err(e) = call_value_this(
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
                        .push(format!("[gg-js error] {}", e.msg));
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
        let fetches =
            vm::pump_bounded(&mut self.st, &self.mods, 10_000, dt_ms);
        (std::mem::take(&mut self.st.logs), fetches)
    }

    /// Fill location.* from the real page URL (loader calls this
    /// before scripts run).
    /// Seed `document.cookie` from the network jar (a `k=v; k2=v2`
    /// string) before page scripts run, so JS sees the server session.
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

    /// Current `document.cookie` value as `k=v; k2=v2` (JS writes flow
    /// back to the network jar through this).
    pub fn cookies_string(&self) -> String {
        self.st
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
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
        let mut vm = PageVm::new(None);
        vm.seed_cookies("sid=abc; theme=dark");
        assert_eq!(vm.cookies_string(), "sid=abc; theme=dark");
        // re-seeding replaces an existing name and appends new ones
        vm.seed_cookies("sid=xyz; lang=ko");
        let s = vm.cookies_string();
        assert!(s.contains("sid=xyz"), "{s}");
        assert!(s.contains("theme=dark"), "{s}");
        assert!(s.contains("lang=ko"), "{s}");
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
        let t = crate::js::find_tag(&doc.borrow(), "div").unwrap();
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
