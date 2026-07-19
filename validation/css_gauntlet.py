# -*- coding: utf-8 -*-
"""Independent CSS/layout gauntlet for the gg engine.

Builds tiny HTML fixtures, runs the native pipeline
(native.load_document -> DocumentLayout.layout(1280,800) -> paint_tree)
and asserts on computed styles, layout box geometry, and display-list
commands. Prints one RESULT| line per claim.
"""
import os, sys, subprocess, traceback

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.environ["GGJS"] = "1"
os.environ["GG_REPO_ROOT"] = REPO_ROOT
sys.path.insert(0, REPO_ROOT)
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

import tkinter
_tk = tkinter.Tk(); _tk.withdraw()

from browser import native
from browser.html_parser import Element, Text, tree_to_list
from browser.layout import (DocumentLayout, paint_tree,
                            layout_tree_to_list, HSTEP, VSTEP)
from browser.draw import (DrawRect, DrawText, DrawClipPush, DrawClipPop,
                          DrawImage)

assert native.available(), "ggcore wheel not importable"


def pipeline(html, w=1280, h=800):
    nodes, doc, css, logs = native.load_document(html, lambda hrefs: {}, None)
    d = DocumentLayout(nodes)
    d.layout(w, h)
    cmds = paint_tree(d, [])
    return nodes, d, cmds


def by_id(nodes, ident):
    for n in tree_to_list(nodes, []):
        if isinstance(n, Element) and n.attributes.get("id") == ident:
            return n
    return None


def box_for(d, node):
    for obj in layout_tree_to_list(d, []):
        if getattr(obj, "node", None) is node and hasattr(obj, "outer_width"):
            return obj
    return None


def rect_of(cmds, color):
    """(index, cmd) of the first DrawRect with this fill color."""
    color = color.lower()
    for i, c in enumerate(cmds):
        if isinstance(c, DrawRect) and str(c.color).lower() == color:
            return i, c
    return None, None


def texts(cmds):
    return [(i, c) for i, c in enumerate(cmds) if isinstance(c, DrawText)]


RESULTS = []


def report(name, ok, evidence):
    RESULTS.append((name, "PASS" if ok else "FAIL", evidence))
    print(f"RESULT|{name}|{'PASS' if ok else 'FAIL'}|{evidence}")


def run(name, fn):
    try:
        fn()
    except Exception:
        tb = traceback.format_exc().strip().splitlines()[-1]
        RESULTS.append((name, "ERROR", tb))
        print(f"RESULT|{name}|ERROR|{tb}")


# ---------------------------------------------------------------- selectors
def t_attr_selectors():
    html = """
    <style>
    body { margin: 0 }
    [data-x]        { color: #101010 }
    [data-y="v"]    { color: #202020 }
    [data-z~="b"]   { color: #303030 }
    [data-l|="en"]  { color: #404040 }
    [data-p^="pre"] { color: #505050 }
    [data-s$="fix"] { color: #606060 }
    [data-c*="mid"] { color: #707070 }
    </style>
    <div id="t1" data-x="anything">a</div>
    <div id="t2" data-y="v">a</div>
    <div id="t3" data-z="a b c">a</div>
    <div id="t4" data-l="en-US">a</div>
    <div id="t5" data-p="prefix">a</div>
    <div id="t6" data-s="suffix">a</div>
    <div id="t7" data-c="xxmidyy">a</div>
    <div id="n2" data-y="other">a</div>
    <div id="n3" data-z="ab c">a</div>
    <div id="n5" data-p="xprefix">a</div>
    """
    nodes, d, cmds = pipeline(html)
    got = {i: (by_id(nodes, i).style.get("color") or "").lower()
           for i in ("t1", "t2", "t3", "t4", "t5", "t6", "t7",
                     "n2", "n3", "n5")}
    want = {"t1": "#101010", "t2": "#202020", "t3": "#303030",
            "t4": "#404040", "t5": "#505050", "t6": "#606060",
            "t7": "#707070"}
    ok = all(got[k] == v for k, v in want.items())
    neg_ok = all(got[k] not in want.values() for k in ("n2", "n3", "n5"))
    report("attr-selectors-7-forms", ok and neg_ok,
           f"matched={got} negatives_clean={neg_ok}")


def t_pseudo_class_selectors():
    html = """
    <style>
    body { margin: 0 }
    p:not(.skip) { color: #111111 }
    li:nth-child(odd)  { color: #0000aa }
    li:nth-child(even) { color: #00aa00 }
    li:nth-child(3)    { background-color: #abcdef }
    li:first-child { font-style: italic }
    li:last-child  { font-weight: bold }
    :root { --main: #123123 }
    #rv { color: var(--main) }
    </style>
    <p id="pn">plain</p>
    <p id="ps" class="skip">skip</p>
    <ul>
      <li id="l1">1</li><li id="l2">2</li><li id="l3">3</li>
      <li id="l4">4</li><li id="l5">5</li>
    </ul>
    <div id="rv">rootvar</div>
    """
    nodes, d, cmds = pipeline(html)
    g = lambda i, p="color": (by_id(nodes, i).style.get(p) or "").lower()
    checks = {
        ":not applies": g("pn") == "#111111",
        ":not excludes": g("ps") != "#111111",
        "odd": all(g(i) == "#0000aa" for i in ("l1", "l3", "l5")),
        "even": all(g(i) == "#00aa00" for i in ("l2", "l4")),
        "nth(3)": g("l3", "background-color") == "#abcdef"
                  and g("l2", "background-color") != "#abcdef",
        "first": g("l1", "font-style") == "italic"
                 and g("l2", "font-style") != "italic",
        "last": g("l5", "font-weight") == "bold"
                and g("l4", "font-weight") != "bold",
        ":root var": g("rv") == "#123123",
    }
    report("pseudo-class-selectors", all(checks.values()),
           f"checks={checks} pn={g('pn')} l1={g('l1')} l2={g('l2')} "
           f"l3bg={g('l3','background-color')} rv={g('rv')}")


def t_specificity_cascade():
    html = """
    <style>
    body { margin: 0 }
    .cls { color: #002000 }
    p { color: #100000 }
    .later { color: #111111 }
    .later { color: #222222 }
    </style>
    <p id="c1">tag</p>
    <p id="c2" class="cls">class beats later tag rule</p>
    <p id="c3" class="cls" style="color: #000030">inline</p>
    <p id="c4" class="later">later wins</p>
    """
    nodes, d, cmds = pipeline(html)
    g = lambda i: (by_id(nodes, i).style.get("color") or "").lower()
    checks = {
        "tag": g("c1") == "#100000",
        "class>tag despite order": g("c2") == "#002000",
        "inline>class": g("c3") == "#000030",
        "later wins same spec": g("c4") == "#222222",
    }
    report("specificity-and-cascade", all(checks.values()),
           f"c1={g('c1')} c2={g('c2')} c3={g('c3')} c4={g('c4')} {checks}")


def t_media_queries():
    html = """
    <style>
    body { margin: 0 }
    @media (max-width: 800px)  { #m1 { color: #440000 } }
    @media (min-width: 1000px) { #m2 { color: #004400 } }
    @media (max-width: 1500px) { #m3 { color: #000044 } }
    @media (min-width: 1300px) { #m4 { color: #444400 } }
    </style>
    <p id="m1">a</p><p id="m2">a</p><p id="m3">a</p><p id="m4">a</p>
    """
    nodes, d, cmds = pipeline(html)  # viewport 1280
    g = lambda i: (by_id(nodes, i).style.get("color") or "").lower()
    checks = {
        "max800 not applied": g("m1") != "#440000",
        "min1000 applied": g("m2") == "#004400",
        "max1500 applied": g("m3") == "#000044",
        "min1300 not applied": g("m4") != "#444400",
    }
    report("media-queries-1280", all(checks.values()),
           f"m1={g('m1')} m2={g('m2')} m3={g('m3')} m4={g('m4')} {checks}")


def t_var_chain_fallback():
    html = """
    <style>
    body { margin: 0 }
    :root { --a: #123456; --b: var(--a) }
    #v1 { color: var(--b) }
    #v2 { color: var(--nope, #654321) }
    </style>
    <p id="v1">chain</p><p id="v2">fallback</p>
    """
    nodes, d, cmds = pipeline(html)
    g = lambda i: (by_id(nodes, i).style.get("color") or "").lower()
    report("var-chain-and-fallback",
           g("v1") == "#123456" and g("v2") == "#654321",
           f"v1={g('v1')} (want #123456) v2={g('v2')} (want #654321)")


def t_var_cycle_guard():
    # run in a subprocess so a hang cannot take down the gauntlet
    code = r'''
import os, sys
os.environ["GGJS"] = "1"
sys.path.insert(0, os.environ["GG_REPO_ROOT"])
from browser import native
from browser.html_parser import Element, tree_to_list
html = ("<style>:root { --x: var(--y); --y: var(--x) } "
        "#v3 { color: var(--x, #445566) }</style><p id=\"v3\">c</p>")
nodes, doc, css, logs = native.load_document(html, lambda h: {}, None)
for n in tree_to_list(nodes, []):
    if isinstance(n, Element) and n.attributes.get("id") == "v3":
        print("CYCLE_OK color=%r" % n.style.get("color"))
        break
'''
    try:
        p = subprocess.run([sys.executable, "-c", code], capture_output=True,
                           text=True, timeout=20)
        out = (p.stdout + p.stderr).strip().replace("\n", " ")
        report("var-cycle-guard", "CYCLE_OK" in out and p.returncode == 0,
               f"rc={p.returncode} out={out[:200]}")
    except subprocess.TimeoutExpired:
        report("var-cycle-guard", False, "HANG: subprocess killed at 20s")


# ------------------------------------------------------------------- flex
def t_flex_space_between():
    html = """
    <style>body { margin: 0 }</style>
    <div id="fc" style="display:flex; width:600px;
         justify-content: space-between">
      <div id="f1" style="width:100px;height:30px"></div>
      <div id="f2" style="width:100px;height:30px"></div>
      <div id="f3" style="width:100px;height:30px"></div>
    </div>
    """
    nodes, d, cmds = pipeline(html)
    fc = box_for(d, by_id(nodes, "fc"))
    xs = [box_for(d, by_id(nodes, f"f{i}")).x for i in (1, 2, 3)]
    want = [fc.x, fc.x + (600 - 300) / 2 + 100, fc.x + 600 - 100]
    ok = all(abs(a - b) < 1.0 for a, b in zip(xs, want))
    report("flex-space-between", ok,
           f"container_x={fc.x} w=600 child_x={xs} expected={want}")


def t_flex_align_center():
    html = """
    <style>body { margin: 0 }</style>
    <div style="display:flex; width:600px; align-items:center">
      <div id="tall" style="width:100px;height:100px"></div>
      <div id="short" style="width:100px;height:20px"></div>
    </div>
    """
    nodes, d, cmds = pipeline(html)
    tall = box_for(d, by_id(nodes, "tall"))
    short = box_for(d, by_id(nodes, "short"))
    dy = short.y - tall.y
    report("flex-align-items-center", abs(dy - 40.0) < 1.0,
           f"tall.y={tall.y} short.y={short.y} dy={dy} expected=40 "
           f"(=(100-20)/2)")


def t_flex_grow():
    html = """
    <style>body { margin: 0 }</style>
    <div style="display:flex; width:600px">
      <div id="ga" style="width:200px;height:20px"></div>
      <div id="gb" style="flex:1;height:20px"></div>
    </div>
    <div style="display:flex; width:600px">
      <div id="ha" style="width:200px;height:20px"></div>
      <div id="hb" style="flex-grow:1;height:20px"></div>
    </div>
    """
    nodes, d, cmds = pipeline(html)
    gb = box_for(d, by_id(nodes, "gb"))
    hb = box_for(d, by_id(nodes, "hb"))
    short_ok = abs(gb.outer_width() - 400) < 1.0
    long_ok = abs(hb.outer_width() - 400) < 1.0
    report("flex-1-fills-remaining", short_ok and long_ok,
           f"flex:1 child outer_width={gb.outer_width()} "
           f"flex-grow:1 child outer_width={hb.outer_width()} expected=400 "
           f"(600 container - 200 fixed sibling)")


# ------------------------------------------------------------------ float
def t_float():
    html = """
    <style>body { margin: 0 } div { margin: 0 }</style>
    <div id="w1">
      <div id="flt" style="float:left;width:200px;height:50px"></div>
      <div id="aft" style="height:20px"></div>
    </div>
    <div id="w2">
      <div style="float:left;width:200px;height:50px"></div>
      <div id="clr" style="clear:both;height:10px"></div>
    </div>
    <div id="w3">
      <div style="float:left;width:150px;height:70px"></div>
    </div>
    """
    nodes, d, cmds = pipeline(html)
    w1 = box_for(d, by_id(nodes, "w1"))
    flt = box_for(d, by_id(nodes, "flt"))
    aft = box_for(d, by_id(nodes, "aft"))
    w2 = box_for(d, by_id(nodes, "w2"))
    clr = box_for(d, by_id(nodes, "clr"))
    w3 = box_for(d, by_id(nodes, "w3"))
    shift_ok = abs(aft.x - (w1.x + 200)) < 1.0
    narrow_ok = abs(aft.width - (w1.width - 200)) < 1.0
    float_bottom = w2.y + 50
    clear_ok = clr.y >= float_bottom - 0.5
    contain_ok = w3.height >= 70 - 0.5
    report("float-left-shifts-flow", shift_ok and narrow_ok,
           f"wrap.x={w1.x} float w=200 next.x={aft.x} (want {w1.x+200}) "
           f"next.width={aft.width} (want {w1.width-200})")
    report("clear-both-drops-below", clear_ok,
           f"float_bottom={float_bottom} cleared.y={clr.y}")
    report("container-height-contains-float", contain_ok,
           f"container height={w3.height} float height=70")


# --------------------------------------------------------------- position
def t_position_abs_fixed():
    html = """
    <style>body { margin: 0 }</style>
    <p>filler</p>
    <div id="abs" style="position:absolute;left:100px;top:50px;
         width:120px;height:40px;background-color:#00fefe"></div>
    <div id="fix" style="position:fixed;left:0;top:0;
         width:50px;height:10px;background-color:#00fdfd"></div>
    """
    nodes, d, cmds = pipeline(html)
    ia, ra = rect_of(cmds, "#00fefe")
    if_, rf = rect_of(cmds, "#00fdfd")
    ax, ay = HSTEP + 100, VSTEP + 50
    abs_ok = (ra is not None and abs(ra.left - ax) < 1 and
              abs(ra.top - ay) < 1 and abs(ra.right - (ax + 120)) < 1 and
              abs(ra.bottom - (ay + 40)) < 1)
    fix_ok = (rf is not None and abs(rf.left - HSTEP) < 1 and
              abs(rf.top - VSTEP) < 1)
    report("position-absolute-offsets", abs_ok,
           f"rect=({getattr(ra,'left',None)},{getattr(ra,'top',None)},"
           f"{getattr(ra,'right',None)},{getattr(ra,'bottom',None)}) "
           f"expected=({ax},{ay},{ax+120},{ay+40})")
    report("position-fixed-offsets", fix_ok,
           f"rect=({getattr(rf,'left',None)},{getattr(rf,'top',None)}) "
           f"expected=({HSTEP},{VSTEP})")


def t_position_relative():
    base = """
    <style>body {{ margin: 0 }}</style>
    <p id="r1">first</p>
    <p id="r2" style="{style}">second</p>
    """
    n0, d0, _ = pipeline(base.format(style=""))
    n1, d1, _ = pipeline(base.format(
        style="position:relative;left:30px;top:10px"))
    b0 = box_for(d0, by_id(n0, "r2"))
    b1 = box_for(d1, by_id(n1, "r2"))
    dx, dy = b1.x - b0.x, b1.y - b0.y
    report("position-relative-offsets",
           abs(dx - 30) < 0.5 and abs(dy - 10) < 0.5,
           f"unpositioned=({b0.x},{b0.y}) relative=({b1.x},{b1.y}) "
           f"delta=({dx},{dy}) expected=(30,10)")


def t_z_index():
    html = """
    <style>body { margin: 0 }</style>
    <div id="za" style="position:relative;z-index:5;width:100px;
         height:50px;background-color:#aa0001"></div>
    <div id="zb" style="position:relative;z-index:1;top:-30px;width:100px;
         height:50px;background-color:#00aa02"></div>
    """
    nodes, d, cmds = pipeline(html)
    ia, ra = rect_of(cmds, "#aa0001")
    ib, rb = rect_of(cmds, "#00aa02")
    overlap = (ra.left < rb.right and rb.left < ra.right and
               ra.top < rb.bottom and rb.top < ra.bottom)
    # za is FIRST in the document but has the HIGHER z-index: it must
    # paint LATER (larger display-list index) than zb.
    report("z-index-paint-order", overlap and ia > ib,
           f"z5_first_in_doc rect@idx={ia} z1_second rect@idx={ib} "
           f"overlap={overlap} rects a=({ra.left},{ra.top},{ra.right},"
           f"{ra.bottom}) b=({rb.left},{rb.top},{rb.right},{rb.bottom})")


# ------------------------------------------------------------------ text
def t_ellipsis():
    word = "Supercalifragilisticexpialidocious"
    html = f"""
    <style>body {{ margin: 0 }}</style>
    <div style="width:100px;white-space:nowrap;overflow:hidden;
         text-overflow:ellipsis">{word}</div>
    """
    nodes, d, cmds = pipeline(html)
    painted = [c.text for _, c in texts(cmds)]
    ell = [t for t in painted if t.endswith("…")]
    ok = bool(ell) and all(len(t) < len(word) for t in ell) \
        and word not in painted
    report("text-overflow-ellipsis", ok,
           f"painted={painted!r} original_len={len(word)} "
           f"ellipsized_len={[len(t) for t in ell]}")


def t_line_height():
    html = """
    <style>body { margin: 0 }</style>
    <p id="lh1" style="line-height:40px">word</p>
    <p id="lh2">word</p>
    """
    nodes, d, cmds = pipeline(html)
    b1 = box_for(d, by_id(nodes, "lh1"))
    b2 = box_for(d, by_id(nodes, "lh2"))
    ratio = b1.height / b2.height if b2.height else 0
    # 40px on a 16px font = factor 2.5 vs default 1.25 -> ratio 2.0
    report("line-height-px", abs(ratio - 2.0) < 0.05 and b1.height > b2.height,
           f"lh40_height={b1.height} default_height={b2.height} "
           f"ratio={ratio:.3f} expected_ratio=2.0")


def t_before_after():
    html = """
    <style>
    body { margin: 0 }
    #pb::before { content: "BEFTOK " }
    #pb::after  { content: " AFTTOK" }
    </style>
    <p id="pb">MIDTOK</p>
    """
    nodes, d, cmds = pipeline(html)
    pseudo_tags = [n.tag for n in tree_to_list(nodes, [])
                   if isinstance(n, Element)
                   and n.tag in ("::before", "::after")]
    painted = [c.text for _, c in texts(cmds)]
    idx = {t: i for i, t in enumerate(painted)}
    ok = ("BEFTOK" in idx and "MIDTOK" in idx and "AFTTOK" in idx
          and idx["BEFTOK"] < idx["MIDTOK"] < idx["AFTTOK"])
    report("pseudo-element-content", ok,
           f"pseudo_nodes={pseudo_tags} painted_texts={painted!r}")


def t_overflow_clip():
    html = """
    <style>body { margin: 0 }</style>
    <div style="overflow:hidden;width:200px;height:30px;
         background-color:#010101">CLIPME</div>
    """
    nodes, d, cmds = pipeline(html)
    pushes = [i for i, c in enumerate(cmds) if isinstance(c, DrawClipPush)]
    pops = [i for i, c in enumerate(cmds) if isinstance(c, DrawClipPop)]
    ti = [i for i, c in texts(cmds) if c.text == "CLIPME"]
    kinds_ok = (pushes and pops and
                cmds[pushes[0]].native(0)[0] == 6 and
                cmds[pops[0]].native(0)[0] == 7)
    push = cmds[pushes[0]] if pushes else None
    bracket_ok = bool(pushes and pops and ti
                      and pushes[0] < ti[0] < pops[-1])
    geom = (f"clip_rect=({push.left},{push.clip_top},{push.right},"
            f"{push.clip_bottom})") if push else "no push"
    report("overflow-hidden-clip-kinds-6-7", bool(kinds_ok and bracket_ok),
           f"push@{pushes} text@{ti} pop@{pops} native_kinds="
           f"{[cmds[i].native(0)[0] for i in pushes+pops]} {geom}")


def t_transform():
    html = """
    <style>body { margin: 0 } div { margin: 0 }</style>
    <div id="a" style="width:100px;height:20px;
         background-color:#0c0c01"></div>
    <div id="b" style="width:100px;height:20px;background-color:#0c0c02;
         transform:translate(20px,10px)"></div>
    <div id="c" style="width:100px;height:20px;background-color:#0c0c03;
         transform:scale(0)">GONETXT</div>
    """
    nodes, d, cmds = pipeline(html)
    _, ra = rect_of(cmds, "#0c0c01")
    _, rb = rect_of(cmds, "#0c0c02")
    ic, rc = rect_of(cmds, "#0c0c03")
    gone_text = any(c.text == "GONETXT" for _, c in texts(cmds))
    # b sits directly below a (heights 20, margin 0): untransformed top
    # would be ra.top+20; transform adds (20,10)
    tr_ok = (rb is not None and abs(rb.left - (ra.left + 20)) < 0.5
             and abs(rb.top - (ra.top + 20 + 10)) < 0.5)
    hide_ok = rc is None and not gone_text
    report("transform-translate-20-10", tr_ok,
           f"base_rect=({ra.left},{ra.top}) translated_rect="
           f"({getattr(rb,'left',None)},{getattr(rb,'top',None)}) "
           f"expected=({ra.left+20},{ra.top+30})")
    report("transform-scale0-hides", hide_ok,
           f"scale0_rect_found={rc is not None} scale0_text_found={gone_text}")


def t_nowrap():
    html = """
    <style>body { margin: 0 }</style>
    <div style="width:120px">wrapa wrapb wrapc wrapd wrape wrapf</div>
    <div style="width:120px;white-space:nowrap">
      nowrapa nowrapb nowrapc nowrapd nowrape nowrapf</div>
    """
    nodes, d, cmds = pipeline(html)
    wrap_y = {c.top for _, c in texts(cmds) if c.text.startswith("wrap")}
    nowrap_y = {c.top for _, c in texts(cmds) if c.text.startswith("nowrap")}
    wrap_n = len([1 for _, c in texts(cmds) if c.text.startswith("wrap")])
    nowrap_n = len([1 for _, c in texts(cmds) if c.text.startswith("nowrap")])
    report("white-space-nowrap", len(wrap_y) > 1 and len(nowrap_y) == 1,
           f"normal: {wrap_n} words on {len(wrap_y)} lines (y={sorted(wrap_y)}); "
           f"nowrap: {nowrap_n} words on {len(nowrap_y)} line(s) "
           f"(y={sorted(nowrap_y)})")


TESTS = [
    ("attr-selectors", t_attr_selectors),
    ("pseudo-classes", t_pseudo_class_selectors),
    ("specificity", t_specificity_cascade),
    ("media", t_media_queries),
    ("var-chain", t_var_chain_fallback),
    ("var-cycle", t_var_cycle_guard),
    ("flex-sb", t_flex_space_between),
    ("flex-align", t_flex_align_center),
    ("flex-grow", t_flex_grow),
    ("float", t_float),
    ("pos-abs-fixed", t_position_abs_fixed),
    ("pos-rel", t_position_relative),
    ("z-index", t_z_index),
    ("ellipsis", t_ellipsis),
    ("line-height", t_line_height),
    ("before-after", t_before_after),
    ("clip", t_overflow_clip),
    ("transform", t_transform),
    ("nowrap", t_nowrap),
]

for name, fn in TESTS:
    run(name, fn)

npass = sum(1 for _, v, _ in RESULTS if v == "PASS")
print(f"SUMMARY|{npass}/{len(RESULTS)} PASS")
_tk.destroy()
