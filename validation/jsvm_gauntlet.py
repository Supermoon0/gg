#!/usr/bin/env python3
"""Adversarial gauntlet for the gg-js VM (ggcore.jsvm_run + Doc/pump).

One claim per CLAIM-* case; each JS snippet console.logs canonical strings
compared exactly against an expected list of log lines. Expected values were
cross-checked against node v22 (ground truth), EXCEPT documented gg-js
deviations (Symbol typeof 'string'; lazy compile of 24+-token bodies).

GAP-* cases carry SPEC expectations and are EXPECTED TO FAIL on gg-js —
each failure is measured evidence of a boundary found while probing.
["<LOAD-ERROR>"] as expected means: the script must fail to load (raise).

Modes:
  sync  -> ggcore.jsvm_run(src)      (fresh VM per call, no event loop)
  async -> GGJS=1 Doc.run_scripts + pump loop (drains microtasks/timers)

Usage:
  python3 jsvm_gauntlet.py          # run against gg-js
  python3 jsvm_gauntlet.py --node   # run the same snippets under node
"""
import os
import sys
import subprocess

os.environ["GGJS"] = "1"

CASES = [
    # ==================================================================
    # CLAIM cases: expected == what the claim/docs promise
    # ==================================================================
    ("CLAIM prototype-chain/new/instanceof/inheritance", "sync", r"""
function Animal(name){ this.name = name; }
Animal.prototype.speak = function(){ return this.name + " generic"; };
function Dog(name){ Animal.call(this, name); }
Dog.prototype = Object.create(Animal.prototype);
Dog.prototype.constructor = Dog;
Dog.prototype.bark = function(){ return this.name + " woof"; };
var d = new Dog("rex");
console.log([d instanceof Dog, d instanceof Animal, d instanceof Object,
  d.speak(), d.bark(),
  Object.getPrototypeOf(d) === Dog.prototype,
  Object.getPrototypeOf(Dog.prototype) === Animal.prototype,
  d.constructor === Dog].map(String).join("|"));
""", ["true|true|true|rex generic|rex woof|true|true|true"]),
    # ------------------------------------------------------------------
    ("CLAIM Proxy/Reflect traps+call+construct+revocation", "sync", r"""
const target = {x: 2};
const seen = [];
const p = new Proxy(target, {
  get(t, k, r){ seen.push("g:" + k); return Reflect.get(t, k, r) + 1; },
  set(t, k, v, r){ seen.push("s:" + k); return Reflect.set(t, k, v * 2, r); },
  has(t, k){ seen.push("h:" + k); return k === "virtual" || Reflect.has(t, k); },
  deleteProperty(t, k){ seen.push("d:" + k); return Reflect.deleteProperty(t, k); }
});
p.x = 4;
const read = p.x;
const has = "virtual" in p;
delete p.x;
function add(a, b){ return a + b; }
const callable = new Proxy(add, {
  apply(t, th, args){ return Reflect.apply(t, th, args) + 1; }
});
function Box(v){ this.v = v; }
const Ctor = new Proxy(Box, {
  construct(t, args, nt){ return {v: args[0] + 2}; }
});
const rev = Proxy.revocable({ok: 1}, {});
const before = rev.proxy.ok;
rev.revoke();
let revoked = false;
try { rev.proxy.ok; } catch (e) { revoked = e instanceof TypeError; }
console.log([read, target.x, has, callable(20, 21), new Ctor(40).v,
  before, revoked, seen.join(",")].map(String).join("|"));
""", ["9|undefined|true|42|42|1|true|s:x,g:x,h:virtual,d:x"]),
    # ------------------------------------------------------------------
    ("CLAIM class-extends/super/static/fields/accessors/#private", "sync", r"""
class Base {
  static kind = "base";
  static make(v){ return new this(v); }
  #secret = 10;
  val = 1;
  constructor(v){ if (v !== undefined) this.val = v; }
  get double(){ return this.val * 2; }
  set double(x){ this.val = x / 2; }
  peek(){ return this.#secret; }
}
class Sub extends Base {
  constructor(v){ super(v); this.extra = this.val + 1; }
  peek(){ return super.peek() * 100; }
}
var s = new Sub(5);
s.double = 20;
console.log([s instanceof Sub, s instanceof Base, s.val, s.double, s.extra,
  s.peek(), Base.kind, Base.make(3).val].map(String).join("|"));
""", ["true|true|10|20|6|1000|base|3"]),
    # ------------------------------------------------------------------
    ("CLAIM generators-function*/yield/for-of-drain", "sync", r"""
function* g(){ yield 1; var x = yield 2; yield x * 3; return 9; }
var out = [];
for (const v of g()) out.push(v);
var it = g();
var a = it.next(), b = it.next(), c = it.next(5), d = it.next(), e = it.next();
console.log([out.join(","), a.value, a.done, b.value, b.done, c.value, c.done,
  d.value, d.done, e.value, e.done].map(String).join("|"));
""", ["1,2,NaN|1|false|2|false|15|false|9|true|undefined|true"]),
    # ------------------------------------------------------------------
    ("CLAIM async/await-supported-positions+try/catch", "async", r"""
async function f(x){ return x + 1; }
async function g(){
  const a = await f(1);
  var b = await Promise.resolve(20);
  const arr = [await f(10), await f(30)];
  const c = (await f(0)) ? "t" : "f";
  const d = 1 + await f(1);
  const e2 = Math.max(await f(4), 2);
  let caught = "";
  try {
    await Promise.reject(new TypeError("nope"));
    caught = "missed";
  } catch (err) { caught = (err instanceof TypeError) + ":" + err.message; }
  let s = 0;
  for (let i = 0; i < 2; i++) { s = s + await f(i); }
  if (a) { s = s + await f(100); }
  const res = [a, b, arr.join(","), c, d, e2, caught, s].join("|");
  return await Promise.resolve(res);
}
g().then(r => console.log("R:" + r)).catch(e => console.log("E:" + e));
console.log("sync-first");
""", ["sync-first", "R:2|20|11,31|t|3|5|true:nope|104"]),
    # ------------------------------------------------------------------
    ("CLAIM destructuring-decl/assign/for-of-head/defaults", "sync", r"""
let [a, , b = 5, ...rest] = [1, 2, undefined, 4, 5];
let {x, y: yy = 7, z: {w}} = {x: 10, z: {w: 20}};
let p = 0, q = 0;
[p, q] = [q + 1, p + 2];
({p} = {p: 42});
let acc = [];
for (const [k, v] of [[1, 2], [3, 4]]) acc.push(k + ":" + v);
function pd({m, n}){ return m + n; }
console.log([a, b, rest.join(","), x, yy, w, p, q, acc.join(";"),
  pd({m: 1, n: 2})].map(String).join("|"));
""", ["1|5|4,5|10|7|20|42|2|1:2;3:4|3"]),
    # ------------------------------------------------------------------
    ("CLAIM spread/rest-array/call/object-literal", "sync", r"""
function sum(...nums){ return nums.reduce((s, n) => s + n, 0); }
const arr = [1, ...[2, 3], 4];
const mx = Math.max(...arr);
const obj = {a: 1, ...{b: 2, c: 3}, ...{c: 4}};
console.log([arr.join(","), sum(...arr, 5), mx,
  obj.a, obj.b, obj.c].map(String).join("|"));
""", ["1,2,3,4|15|4|1|2|4"]),
    # ------------------------------------------------------------------
    ("CLAIM optional-chaining-incl-f?.()", "sync", r"""
const o = {a: {b: {c: 42}}, f(){ return "hit"; }};
const n = null;
console.log([o.a?.b?.c, n?.x, o.a?.zzz?.deep, o.f?.(), o.g?.(),
  n?.[0], o.a?.["b"]?.c].map(String).join("|"));
""", ["42|undefined|undefined|hit|undefined|undefined|42"]),
    # ------------------------------------------------------------------
    ("CLAIM nullish-coalescing-??", "sync", r"""
console.log([null ?? "d", undefined ?? "d", 0 ?? "d", "" ?? "d",
  false ?? "d", (null ?? undefined) ?? "e"].map(String).join("|"));
""", ["d|d|0||false|e"]),
    # ------------------------------------------------------------------
    ("CLAIM getset+defineProperty/descriptor/create/setPrototypeOf", "sync", r"""
const o = {_v: 1, get v(){ return this._v * 2; }, set v(x){ this._v = x + 100; }};
o.v = 1;
Object.defineProperty(o, "dp", {value: 7});
Object.defineProperty(o, "g2", {get(){ return 77; }});
const d = Object.getOwnPropertyDescriptor(o, "dp");
const d2 = Object.getOwnPropertyDescriptor(o, "v");
const proto = {greet(){ return "hi " + this.name; }};
const c = Object.create(proto, {name: {value: "bob", enumerable: true}});
const q = {};
Object.setPrototypeOf(q, proto);
q.name = "sue";
console.log([o.v, o._v, o.dp, o.g2, d.value, typeof d2.get, typeof d2.set,
  c.greet(), q.greet(),
  Object.getPrototypeOf(q) === proto].map(String).join("|"));
""", ["202|101|7|77|7|function|function|hi bob|hi sue|true"]),
    # ------------------------------------------------------------------
    ("CLAIM Map/Set/WeakMap/WeakSet-object-keys+size", "sync", r"""
const k1 = {}, k2 = {};
const m = new Map();
m.set(k1, "a").set(k2, "b").set("s", 1);
m.set(k1, "a2");
const st = new Set([1, 2, 2, 3]);
st.add(2); st.delete(3);
const wm = new WeakMap();
wm.set(k1, 99);
const ws = new WeakSet();
ws.add(k2);
console.log([m.size, m.get(k1), m.get(k2), m.has("s"), m.delete("s"), m.size,
  st.size, st.has(2), st.has(3), wm.get(k1), wm.has(k2),
  ws.has(k2), ws.has(k1)].map(String).join("|"));
""", ["3|a2|b|true|true|2|2|true|false|99|false|true|false"]),
    # ------------------------------------------------------------------
    # DOCUMENTED DEVIATION: Symbol is a string fake; typeof is 'string'
    # (node prints 'symbol' here — the gap is the claim being verified)
    ("CLAIM Symbol-fake-unique/well-knowns/typeof-string-gap", "sync", r"""
const s1 = Symbol("x"), s2 = Symbol("x");
const o = {}; o[s1] = 42;
const itobj = { [Symbol.iterator](){ let i = 0; return { next(){
  return i < 2 ? {value: i++, done: false} : {value: undefined, done: true};
} }; } };
let drained = [];
for (const v of itobj) drained.push(v);
console.log([s1 === s2, typeof s1, typeof Symbol.iterator,
  Symbol.iterator !== undefined, o[s1], String(s1).length > 0,
  drained.join(",")].map(String).join("|"));
""", ["false|string|string|true|42|true|0,1"]),
    # ------------------------------------------------------------------
    ("CLAIM regex-literal/constructor/exec/test/source-flags-global", "sync", r"""
const r = /a(b+)c/gi;
const m = r.exec("xxABBBCyy");
const t = /ab+c/;
const r2 = new RegExp("\\d+", "g");
console.log([r.source, r.flags, r.global, r.ignoreCase, t.global,
  t.test("xabbcx"), t.test("axc"), m[0], m[1], m.length,
  "a1b22c333".replace(r2, "#"), r2.source, r2.global,
  r2.test("zz9")].map(String).join("|"));
""", ["a(b+)c|gi|true|true|false|true|false|ABBBC|BBB|2|a#b#c#|\\d+|true|true"]),
    # ------------------------------------------------------------------
    ("CLAIM ToPrimitive-valueOf/toString-actually-called", "sync", r"""
const vcalls = [];
const vo = { valueOf(){ vcalls.push("v"); return 5; } };
const tcalls = [];
const to = { toString(){ tcalls.push("t"); return "S"; } };
console.log([vo + 1, vo == 5, to + "!", to == "S", `x${to}`,
  vcalls.join(""), tcalls.join("")].map(String).join("|"));
""", ["6|true|S!|true|xS|vv|ttt"]),
    # ------------------------------------------------------------------
    ("CLAIM labeled-statements+labeled-block-break", "sync", r"""
let log = [];
outer: for (let i = 0; i < 3; i++) {
  for (let j = 0; j < 3; j++) {
    if (j === 2) continue outer;
    if (i === 2) break outer;
    log.push(i + "" + j);
  }
}
blk: { log.push("in"); if (true) break blk; log.push("never"); }
console.log(log.join(","));
""", ["00,01,10,11,in"]),
    # ------------------------------------------------------------------
    ("CLAIM bitwise+ToInt32-wrapping", "sync", r"""
console.log([(1 << 31), (1 << 31) >> 0, (1 << 31) >>> 0, -1 >>> 0,
  5 & 3, 5 | 3, 5 ^ 3, ~5, 2147483648 | 0, 4294967296 | 0, 4294967297 | 0,
  "12" & 13, 0xFFFFFFFF | 0, 1 << 33, -9 >> 1].map(String).join("|"));
""", ["-2147483648|-2147483648|2147483648|4294967295|1|7|6|-6|-2147483648|0|1|12|-1|2|-5"]),
    # ------------------------------------------------------------------
    ("CLAIM arguments-object", "sync", r"""
function f(a, b){
  return [arguments.length, arguments[0], arguments[2],
    typeof arguments].map(String).join("|");
}
function g(){ return Array.prototype.slice.call(arguments).join(","); }
function h(){ return [].slice.call(arguments, 1).join(","); }
console.log(f(1, 2, 3) + "|" + g(4, 5, 6) + "|" + h(7, 8, 9));
""", ["3|1|3|object|4,5,6|8,9"]),
    # ------------------------------------------------------------------
    ("CLAIM bind/call/apply+uncurried-extraction", "sync", r"""
function greet(pre, post){ return pre + this.name + post; }
const o = {name: "bob"};
const bound = greet.bind(o, "<");
console.log([greet.call(o, "[", "]"), greet.apply(o, ["(", ")"]), bound(">"),
  [].slice.call({0: "a", 1: "b", length: 2}).join(","),
  "".slice.call("hello", 1, 3),
  (1).toString(2), (255).toString(16), (10).toString()].map(String).join("|"));
""", ["[bob]|(bob)|<bob>|a,b|el|1|ff|10"]),
    # ------------------------------------------------------------------
    ("CLAIM array-sort/splice/reduce/some/every/lastIndexOf", "sync", r"""
const a = [5, 1, 4, 2, 3];
a.sort((x, y) => x - y);
const lex = [10, 9, 1].sort().join(",");
const removed = a.splice(1, 2, 9);
console.log([a.join(","), lex, removed.join(","), a.reduce((s, x) => s + x, 0),
  a.some(x => x > 8), a.every(x => x > 0), a.every(x => x > 1),
  [1, 2, 1, 2].lastIndexOf(1), [1, 2, 1, 2].lastIndexOf(7)].map(String).join("|"));
""", ["1,9,4,5|1,10,9|2,3|19|true|true|false|2|-1"]),
    # ------------------------------------------------------------------
    ("CLAIM try/catch-error-hierarchy+engine-TypeErrors", "sync", r"""
let r = [];
try { null.x; } catch (e) { r.push((e instanceof TypeError) + ":" + (e instanceof Error)); }
try { (void 0)(); } catch (e) { r.push(e instanceof TypeError); }
try { ({}).nope(); } catch (e) { r.push(e instanceof TypeError); }
const te = new TypeError("msg");
r.push(te.name + ":" + te.message + ":" + (te instanceof TypeError) + ":" +
  (te instanceof Error) + ":" + (te instanceof RangeError));
try { throw new RangeError("rr"); }
catch (e) { r.push((e instanceof RangeError) + ":" + (e instanceof TypeError) + ":" + (e instanceof Error)); }
console.log(r.map(String).join("|"));
""", ["true:true|true|true|TypeError:msg:true:true:false|true:false:true"]),
    # ------------------------------------------------------------------
    # DOCUMENTED DEVIATION: lazy parse defers 24+-token pure-function bodies;
    # body syntax error must NOT block load, must be catchable SyntaxError at
    # first call. (node rejects this script at load.)
    ("CLAIM lazy-compile-24tok-body-error-at-first-call", "sync", r"""
function broken(){
  var a = 1; var b = 2; var c = 3; var d = 4; var e = 5; var f = 6; var g = 7; var h = 8;
  return 1 +* 2;
}
console.log("loaded");
let msg = "none";
try { broken(); }
catch (e) { msg = (e instanceof SyntaxError) + ":" + (e instanceof Error) + ":" + e.name; }
console.log("call:" + msg);
""", ["loaded", "call:true:true:SyntaxError"]),
    # ==================================================================
    # GAP cases: SPEC expectations — expected to FAIL on gg-js; each
    # failure documents a measured boundary of the engine.
    # ==================================================================
    ("GAP generators-yield-inside-loop (spec)", "sync", r"""
function* g(){ for (let i = 0; i < 3; i++) { yield i * 10; } }
var out = [];
for (const v of g()) out.push(v);
console.log(out.join(","));
""", ["0,10,20"]),
    # ------------------------------------------------------------------
    ("GAP await-in-for-of-body (spec)", "async", r"""
async function f(){
  let s = 0;
  for (const p of [Promise.resolve(1), Promise.resolve(2)]) { const v = await p; s = s + v; }
  return s;
}
f().then(r => console.log("R:" + r)).catch(e => console.log("E:" + e));
""", ["R:3"]),
    # ------------------------------------------------------------------
    ("GAP return-after-await-in-nested-block (spec)", "async", r"""
async function f(){
  while (true) { const v = await Promise.resolve(9); return v; }
}
f().then(r => console.log("R:" + r)).catch(e => console.log("E:" + e));
""", ["R:9"]),
    # ------------------------------------------------------------------
    ("GAP destructuring-nested-pattern-default (spec)", "sync", r"""
let {z: {w} = {w: 9}} = {};
console.log(w);
""", ["9"]),
    # ------------------------------------------------------------------
    ("GAP destructuring-param-object-defaults (spec)", "sync", r"""
function def({m = 3, n} = {}){ return m + (n || 0); }
console.log(def() + "|" + def({m: 1, n: 2}));
""", ["3|3"]),
    # ------------------------------------------------------------------
    ("GAP descriptor-attributes-enforced (spec)", "sync", r"""
const o = {};
Object.defineProperty(o, "dp", {value: 7, writable: false, enumerable: false});
o.dp = 99;
const d = Object.getOwnPropertyDescriptor(o, "dp");
console.log([o.dp, Object.keys(o).length, d.writable, d.enumerable].map(String).join("|"));
""", ["7|0|false|false"]),
    # ------------------------------------------------------------------
    ("GAP symbol-builtin-iterator-extraction (spec)", "sync", r"""
console.log(typeof [][Symbol.iterator]);
""", ["function"]),
    # ------------------------------------------------------------------
    ("GAP regex-match.index/input+lastIndex (spec)", "sync", r"""
const m = /a(b+)c/g.exec("xxabbcyy");
const r = /b/g;
r.exec("abcb");
console.log([m.index, m.input, r.lastIndex].map(String).join("|"));
""", ["2|xxabbcyy|2"]),
    # ------------------------------------------------------------------
    ("GAP ToPrimitive-string-hint-prefers-toString (spec)", "sync", r"""
const o = { valueOf(){ return 5; }, toString(){ return "S"; } };
console.log([String(o), `${o}`, [o].join("")].map(String).join("|"));
""", ["S|S|S"]),
    # ------------------------------------------------------------------
    # Documented boundary: bodies under 24 tokens are parsed eagerly, so a
    # short broken body kills the whole script at load.
    ("GAP lazy-parse-short-body-loads-eagerly (documented)", "sync", r"""
function broken(){ return 1 +* 2; }
console.log("loaded");
""", ["<LOAD-ERROR>"]),
]


def run_sync(src):
    import ggcore
    return list(ggcore.jsvm_run(src))


def run_async(src):
    import ggcore
    doc = ggcore.parse_html("<html><body></body></html>")
    logs = list(doc.run_scripts([src]))
    for _ in range(500):
        more, fetches = doc.pump()
        logs.extend(more)
        if fetches:
            for fid, _url in fetches:
                doc.reject_fetch(fid, "no network")
            continue
        if not doc.has_pending_work():
            break
    return logs


def run_node(src):
    p = subprocess.run(["node", "-e", src], capture_output=True, text=True,
                       timeout=20)
    out = p.stdout.splitlines()
    if p.returncode != 0:
        err = p.stderr.strip().splitlines()
        out.append("[node-error] " + (err[-1] if err else "?"))
    return out


def main():
    use_node = "--node" in sys.argv
    passed = failed = errored = 0
    for label, mode, src, expected in CASES:
        raised = None
        try:
            if use_node:
                actual = run_node(src)
            elif mode == "async":
                actual = run_async(src)
            else:
                actual = run_sync(src)
        except Exception as e:
            raised = f"{type(e).__name__}: {e}"
            actual = [f"<LOAD-ERROR> {raised}"]
        if expected == ["<LOAD-ERROR>"] and raised is not None:
            passed += 1
            print(f"PASS  {label}   (load rejected as documented)")
            print(f"      raised: {raised}")
        elif actual == expected:
            passed += 1
            print(f"PASS  {label}")
            print(f"      got: {actual}")
        elif raised is not None:
            errored += 1
            print(f"ERROR {label}")
            print(f"      expected: {expected}")
            print(f"      raised:   {raised}")
        else:
            failed += 1
            print(f"FAIL  {label}")
            print(f"      expected: {expected}")
            print(f"      actual:   {actual}")
    total = passed + failed + errored
    print(f"\nTALLY: {passed}/{total} pass, {failed} fail, {errored} error")


if __name__ == "__main__":
    main()
