"""Headless smoke test for the GG engine pipeline."""

import tkinter

from browser import net
from browser.html_parser import Element, HTMLParser, Text, tree_to_list
from browser.css_parser import CSSParser
from browser.style import cascade_priority, default_rules, style
from browser.layout import (HSTEP, VSTEP, BlockLayout, DocumentLayout,
                            layout_tree_to_list, paint_tree)
from browser.pages import DEMO_PAGE

passed = 0


def check(name, cond, detail=""):
    global passed
    status = "PASS" if cond else "FAIL"
    print(f"[{status}] {name}" + (f"  ({detail})" if detail else ""))
    if cond:
        passed += 1
    else:
        raise SystemExit(f"smoke test failed at: {name}")


# --- URL parsing ---
u = net.URL("https://example.com/a/b?q=1#frag")
check("URL parse", u.host == "example.com" and u.port == 443
      and u.path == "/a/b?q=1" and u.fragment == "frag")
check("URL resolve relative", str(u.resolve("c.html")).startswith(
    "https://example.com/a/c.html"))
check("URL resolve absolute path",
      str(u.resolve("/root")) == "https://example.com/root")
check("URL resolve other host",
      str(u.resolve("http://other.org/x")) == "http://other.org/x")

# --- HTML parsing ---
dom = HTMLParser("<p>Hello <b>world &amp; more</b><br>bye").parse()
nodes = tree_to_list(dom, [])
tags = [n.tag for n in nodes if isinstance(n, Element)]
texts = [n.text for n in nodes if isinstance(n, Text)]
check("implicit html/head/body", tags[:2] == ["html", "body"] or
      ("html" in tags and "body" in tags))
check("entity decoding", any("world & more" in t for t in texts), str(texts))
check("self-closing br", "br" in tags)

dom2 = HTMLParser("<ul><li>one<li>two</ul><p>a<p>b").parse()
lis = [n for n in tree_to_list(dom2, [])
       if isinstance(n, Element) and n.tag == "li"]
check("auto-close li", len(lis) == 2
      and all(len(li.children) == 1 for li in lis))

# --- parser edge cases (regression for fixed crash/garbage-tag bugs) ---
check("<html /> no crash",
      "html" in [n.tag for n in tree_to_list(HTMLParser("<html />").parse(),
                 []) if isinstance(n, Element)])
br_tags = [n.tag for n in tree_to_list(HTMLParser("<p>a<br/>b").parse(), [])
           if isinstance(n, Element)]
check("<br/> yields 'br' not 'br/'", "br" in br_tags and "br/" not in br_tags)
spaced = next(n for n in tree_to_list(
    HTMLParser('<a href = "x.html" id=y>go</a>').parse(), [])
    if isinstance(n, Element) and n.tag == "a")
check("spaces around '=' keep value+later attrs",
      spaced.attributes.get("href") == "x.html"
      and spaced.attributes.get("id") == "y", str(spaced.attributes))
dup = next(n for n in tree_to_list(
    HTMLParser('<div id="a" id="b">x</div>').parse(), [])
    if isinstance(n, Element) and n.tag == "div")
check("duplicate attr: first wins", dup.attributes["id"] == "a")

# --- CSS parsing ---
rules = CSSParser(
    "h1, .big { color: red; } #x p { font-size: 2em; }"
    "@media (max-width: 5px) { p { color: blue; } }"
    "a:hover { color: green; } div { margin: 8px 16px; }"
).parse()
check("CSS rule count (skips @media/:hover)", len(rules) == 4,
      f"got {len(rules)}")
ua = default_rules()
check("UA stylesheet parses", len(ua) > 20, f"{len(ua)} rules")

# --- Style + layout (needs a tk root for font metrics) ---
root = tkinter.Tk()
root.withdraw()

demo_dom = HTMLParser(DEMO_PAGE).parse()
all_rules = sorted(ua + CSSParser(
    "".join(c.text for n in tree_to_list(demo_dom, [])
            if isinstance(n, Element) and n.tag == "style"
            for c in n.children if isinstance(c, Text))
).parse(), key=cascade_priority)
style(demo_dom, all_rules)

h1 = next(n for n in tree_to_list(demo_dom, [])
          if isinstance(n, Element) and n.tag == "h1")
check("UA h1 font-size", h1.style["font-size"] == "32px",
      h1.style["font-size"])
check("page CSS overrides color", h1.style["color"] == "#2b5aa6",
      h1.style["color"])
a = next(n for n in tree_to_list(demo_dom, [])
         if isinstance(n, Element) and n.tag == "a")
check("link styled", a.style["color"] == "#1a0dab"
      and "underline" in a.style.get("text-decoration", ""))

# --- cascade regressions (rem, !important, origin, style-attr ws) ---
def _styled(css, html):
    d = HTMLParser(html).parse()
    style(d, sorted(ua + CSSParser(css).parse(), key=cascade_priority))
    return d


def _find(d, tag):
    return next(n for n in tree_to_list(d, [])
                if isinstance(n, Element) and n.tag == tag)


rem_p = _find(_styled("div{font-size:32px}p{font-size:2rem}",
                      "<div><p>x</p></div>"), "p")
check("rem is root-relative not parent", rem_p.style["font-size"] == "32px",
      rem_p.style["font-size"])
imp_p = _find(_styled("p{color:red !important}", "<p>x</p>"), "p")
check("!important stripped from value", imp_p.style["color"] == "red",
      imp_p.style["color"])
reset_body = _find(_styled("*{margin:0}", "<body><p>x</p></body>"), "body")
check("author reset outranks UA (origin)",
      reset_body.style.get("margin-top") == "0",
      reset_body.style.get("margin-top"))
ws_p = _find(_styled("", '<p style=" color: blue; font-weight: bold">x</p>'),
             "p")
check("leading-space style attr keeps first decl",
      ws_p.style["color"] == "blue")
# em math against a huge parent font-size once forced px_str(inf) to crash
crash_dom = _styled("div{font-size:1e308px}p{font-size:9e300em}",
                    "<div><p>x</p></div>")
crash_p = _find(crash_dom, "p")
check("non-finite font-size math does not crash px_str",
      crash_p.style["font-size"] == "0px", crash_p.style["font-size"])
# --- CSS custom properties (naver white-screen fix, 2026-07-11:
# colors like var(--color-neutral-background-base-legacy) leaked into
# computed styles unresolved, so nothing painted) ---
var_p = _find(_styled(":root{--c:#123456} p{color:var(--c)}", "<p>x</p>"),
              "p")
check("var() resolves from :root", var_p.style["color"] == "#123456",
      var_p.style["color"])
check("custom props stay out of computed style",
      not any(k.startswith("--") for k in var_p.style))
where_p = _find(_styled(
    ":where(:root,:host){--bg:#0f0} p{background-color:var(--bg)}",
    "<p>x</p>"), "p")
check(":where(:root) defines variables",
      where_p.style.get("background-color") == "#0f0",
      str(where_p.style.get("background-color")))
chain_p = _find(_styled("html{--a:var(--b)} :root{--b:red} "
                        "p{color:var(--a)}", "<p>x</p>"), "p")
check("chained var() resolves", chain_p.style["color"] == "red",
      chain_p.style["color"])
fb_p = _find(_styled("p{color:var(--nope, blue)}", "<p>x</p>"), "p")
check("var() fallback for missing variable", fb_p.style["color"] == "blue",
      fb_p.style["color"])
inh_p = _find(_styled("div{--t:right} p{text-align:var(--t)}",
                      "<div><p>x</p></div>"), "p")
check("custom properties inherit", inh_p.style["text-align"] == "right",
      inh_p.style["text-align"])
bad_inh = _find(_styled("body{color:#222} p{color:var(--gone)}",
                        "<body><p>x</p></body>"), "p")
check("unresolvable inherited prop falls back to parent",
      bad_inh.style["color"] == "#222", bad_inh.style["color"])
bad_bg = _find(_styled("p{background-color:var(--gone)}", "<p>x</p>"), "p")
check("unresolvable non-inherited prop dropped",
      "background-color" not in bad_bg.style)
cyc_p = _find(_styled(":root{--a:var(--b);--b:var(--a)} "
                      "p{color:var(--a,#345678)}", "<p>x</p>"), "p")
check("var() cycle terminates and uses fallback",
      cyc_p.style["color"] == "#345678", cyc_p.style["color"])
vfs_p = _find(_styled(":root{--fs:2em} p{font-size:var(--fs)}",
                      "<p>x</p>"), "p")
check("var() font-size resolves relative units",
      vfs_p.style["font-size"] == "32px", vfs_p.style["font-size"])
theme_p = _find(_styled(":where([data-theme=dark]){--x:#000} "
                        "p{color:var(--x,#eee)}", "<p>x</p>"), "p")
check("attr-gated :where stays inert without the attribute",
      theme_p.style["color"] == "#eee", theme_p.style["color"])

# --- attribute selectors (M1: naver uses 636 of them) ---
attr_hit = _find(_styled("[data-k=v]{color:red}",
                         '<p data-k="v">x</p>'), "p")
check("[attr=value] matches", attr_hit.style["color"] == "red")
attr_miss = _find(_styled("[data-k=v]{color:red}",
                          '<p data-k="w">x</p>'), "p")
check("[attr=value] mismatch ignored",
      attr_miss.style["color"] == "black")
attr_ops = _styled(
    '[data-a]{color:red} [href^="https:"]{font-weight:bold} '
    '[class~=big]{text-align:right}',
    '<p data-a href="https://x" class="a big">x</p>')
attr_p = _find(attr_ops, "p")
check("[attr] presence + ^= + ~= all apply",
      attr_p.style["color"] == "red"
      and attr_p.style["font-weight"] == "bold"
      and attr_p.style["text-align"] == "right", str(attr_p.style))

# --- M1 paint: border-radius, opacity gate, placeholder, bg-image ---
from browser.draw import DrawBgImage as _DrawBg, parse_background

round_dom = _styled("", '<div style="width:100px; height:40px; '
                    'background-color: #0f0; border: 2px solid #000; '
                    'border-radius: 12px">x</div>')
_rdoc = DocumentLayout(round_dom)
_rdoc.layout(400)
_rcmds = paint_tree(_rdoc, [])
check("border-radius paints rounded rects",
      any(getattr(c, "radius", 0) >= 12 for c in _rcmds),
      str([getattr(c, "radius", 0) for c in _rcmds]))

op_dom = _styled("", '<div style="opacity: 0"><p>hidden</p></div>'
                 '<p>shown</p>')
_odoc = DocumentLayout(op_dom)
_odoc.layout(400)
_otexts = [c.text for c in paint_tree(_odoc, []) if hasattr(c, "text")]
check("opacity:0 subtree is not painted",
      "shown" in " ".join(_otexts) and "hidden" not in " ".join(_otexts),
      str(_otexts))

ph_dom = _styled("", '<input placeholder="search here">')
_pdoc2 = DocumentLayout(ph_dom)
_pdoc2.layout(400)
_ptexts = [c for c in paint_tree(_pdoc2, []) if hasattr(c, "text")]
check("input placeholder is painted",
      any("search here" in c.text for c in _ptexts)
      and all(c.color == "#9e9e9e" for c in _ptexts
              if "search here" in c.text), str(_ptexts))

check("background shorthand parses url/pos/repeat",
      parse_background({"background":
                        "url('s.png') no-repeat -366px -282px #fff"})
      == {"url": "s.png", "position": ["-366px", "-282px"],
          "size": [], "repeat": "no-repeat"})
bg_dom = _styled("", '<div style="width:50px; height:20px">x</div>')
bg_div = _find(bg_dom, "div")
bg_div._bg = (7, 100, 50, {"url": "s.png", "position": ["-10px", "0"],
                           "size": [], "repeat": "no-repeat"})
_bdoc = DocumentLayout(bg_dom)
_bdoc.layout(400)
_bcmds = [c for c in paint_tree(_bdoc, []) if isinstance(c, _DrawBg)]
check("background-image paints a bg cmd with sprite offset",
      len(_bcmds) == 1 and _bcmds[0].image_id == 7
      and _bcmds[0].off_x == -10.0 and not _bcmds[0].rep_x,
      str([(c.image_id, c.off_x, c.rep_x) for c in _bcmds]))

# and background:none must not become a paint color
none_dom = _styled("p{background:none}", "<p>x</p>")
_pdoc = DocumentLayout(none_dom)
_pdoc.layout(400)
_cmds = paint_tree(_pdoc, [])
from browser.colors import NAMED as _NAMED
check("background:none is not painted",
      all(not getattr(c, "color", None)
          or c.color.startswith("#") or c.color in _NAMED for c in _cmds))

doc = DocumentLayout(demo_dom)
doc.layout(1000)
display_list = paint_tree(doc, [])
check("layout produces height", doc.height > 500, f"{doc.height:.0f}px")
check("display list nonempty", len(display_list) > 60,
      f"{len(display_list)} paint commands")
words = [c for c in display_list if hasattr(c, "text")]
check("Korean text laid out",
      any("데모" in c.text for c in words))
pre_text = [c for c in words if "print" in c.text]
check("pre block preserved", len(pre_text) == 1
      and 'print("Hello, GG Browser!")' in pre_text[0].text.strip())

# --- box model, flex, position ---
LAYOUT_PAGE = """<html><body style="margin: 0">
<div id=center style="width: 200px; margin: 0 auto;
     padding: 10px; border: 2px solid black">hi</div>
<div id=flexbox style="display: flex">
  <div id=fa style="width: 100px">a</div>
  <div id=fb>b</div>
  <div id=fc>c</div>
</div>
<div id=abs style="position: absolute; left: 50px; top: 300px;
     width: 80px">abs</div>
</body></html>"""
dom3 = HTMLParser(LAYOUT_PAGE).parse()
style(dom3, sorted(ua, key=cascade_priority))
doc3 = DocumentLayout(dom3)
doc3.layout(800)  # content width 800 - 2*HSTEP = 774
boxes = {}
for b in layout_tree_to_list(doc3, []):
    if isinstance(b, BlockLayout) and isinstance(b.node, Element):
        node_id = b.node.attributes.get("id")
        if node_id:
            boxes[node_id] = b

center = boxes["center"]
# border-box 200 = content 176 + padding 20 + border 4, centered in 774
check("margin auto centers", abs(center.x - (HSTEP + 287 + 12)) < 1,
      f"x={center.x}")
check("width is border-box", abs(center.width - 176) < 1,
      f"w={center.width}")
fa, fb, fc = boxes["fa"], boxes["fb"], boxes["fc"]
check("flex row placement", fa.y == fb.y == fc.y and fa.x < fb.x < fc.x,
      f"x: {fa.x:.0f},{fb.x:.0f},{fc.x:.0f}")
check("flex grow shares space", abs(fb.width - (774 - 100) / 2) < 1,
      f"fb.width={fb.width:.0f}")
absbox = boxes["abs"]
check("absolute positioning", abs(absbox.x - (HSTEP + 50)) < 1
      and abs(absbox.y - (VSTEP + 300)) < 1,
      f"({absbox.x:.0f}, {absbox.y:.0f})")
in_flow_h = boxes["flexbox"].y + boxes["flexbox"].height
check("absolute is out of flow", doc3.height < 300,
      f"doc height={doc3.height:.0f}")

# --- mis-render regressions: relative offset, %/vh height, flex auto ---
REGRESS_PAGE = """<html><body style="margin: 0">
<div id=rel style="position: relative; top: 30px; left: 10px;
     height: 40px">shifted</div>
<div id=after style="height: 20px">after</div>
<div id=parent style="height: 200px; padding: 10px">
  <div id=half style="height: 50%">half</div>
</div>
<div id=pct style="height: 50%">pct-no-base</div>
<div id=vh100 style="height: 100vh">vh</div>
<div id=flex2 style="display: flex">
  <div id=ga style="width: 100px">a</div>
  <div id=gb style="width: 150px; margin-left: auto">b</div>
</div>
</body></html>"""


def _boxes(doc):
    out = {}
    for b in layout_tree_to_list(doc, []):
        if isinstance(b, BlockLayout) and isinstance(b.node, Element):
            node_id = b.node.attributes.get("id")
            if node_id:
                out[node_id] = b
    return out


dom4 = HTMLParser(REGRESS_PAGE).parse()
style(dom4, sorted(ua, key=cascade_priority))
doc4 = DocumentLayout(dom4)
doc4.layout(800)  # content width 774, no viewport height
boxes4 = _boxes(doc4)

rel, after = boxes4["rel"], boxes4["after"]
check("relative offset shifts the box",
      abs(rel.y - (VSTEP + 30)) < 1 and abs(rel.x - (HSTEP + 10)) < 1,
      f"({rel.x:.0f}, {rel.y:.0f})")
check("relative offset is visual-only (siblings keep flow position)",
      abs(after.y - (VSTEP + 40)) < 1, f"after.y={after.y:.0f}")

# height:200px parent (padding 10 -> content 180); child 50% = 90
check("% height resolves against definite parent height",
      abs(boxes4["half"].height - 90) < 1,
      f"h={boxes4['half'].height:.0f}")
check("% height without a base stays auto (not 0)",
      boxes4["pct"].height > 10, f"h={boxes4['pct'].height:.0f}")
check("vh height without viewport stays auto (not 0)",
      boxes4["vh100"].height > 10, f"h={boxes4['vh100'].height:.0f}")

ga, gb = boxes4["ga"], boxes4["gb"]
check("flex margin-left:auto pushes item to the right edge",
      abs((gb.x + gb.width) - (doc4.x + doc4.width)) < 1,
      f"right={gb.x + gb.width:.0f} vs {doc4.x + doc4.width:.0f}")
check("flex auto margin keeps items on one row without overlap",
      ga.y == gb.y and gb.x >= ga.x + ga.width,
      f"ga=({ga.x:.0f},{ga.y:.0f}) gb=({gb.x:.0f},{gb.y:.0f})")

doc4.layout(800, 600)  # now with a 600px viewport
boxes4 = _boxes(doc4)
check("100vh resolves against the viewport height",
      abs(boxes4["vh100"].height - 600) < 1,
      f"h={boxes4['vh100'].height:.0f}")
check("% height unchanged by viewport pass",
      abs(boxes4["half"].height - 90) < 1,
      f"h={boxes4['half'].height:.0f}")

# --- transparent rgba() must not paint (naver search box was a black box:
# rgba(0,0,0,0) had its alpha discarded and rendered opaque #000000) ---
from browser.layout import safe_color as _sc
check("rgba alpha 0 is not painted", _sc("rgba(0,0,0,0)", default="") == "")
check("rgba alpha >0 still paints", _sc("rgba(255,0,0,1)") == "#ff0000")
check("rgb() unaffected", _sc("rgb(0,0,0)") == "#000000")

# --- nested absolute positioning must terminate (naver hang, 2026-07-09:
# laying out an out-of-flow box queued its absolute DESCENDANTS onto the
# same list being iterated -> infinite loop) ---
NESTED_ABS = """<html><body>
<div style="position: absolute; left: 10px; top: 10px">
  outer
  <div style="position: absolute; left: 5px; top: 5px">
    mid
    <div style="position: fixed; left: 2px; top: 2px">inner</div>
  </div>
</div>
<div style="position: absolute; right: 10px; top: 10px">
  <div style="position: absolute; right: 1px; bottom: 1px">deep</div>
</div>
<p>flow content</p>
</body></html>"""
dom5 = HTMLParser(NESTED_ABS).parse()
style(dom5, sorted(ua, key=cascade_priority))
doc5 = DocumentLayout(dom5)
_t0 = __import__("time").perf_counter()
doc5.layout(800, 600)
_dt = (__import__("time").perf_counter() - _t0) * 1000
check("nested absolute layout terminates fast", _dt < 1000,
      f"{_dt:.0f} ms")
_texts = [c.text for c in paint_tree(doc5, []) if hasattr(c, "text")]
check("nested absolutes all painted",
      all(any(w in t for t in _texts)
          for w in ("outer", "mid", "inner", "deep")), str(_texts))

# --- HiDPI: native cmd tuples scale from CSS px to device px ---
from browser.draw import scale_cmds

_cmds = [(0, 1, 2, 3, 4, (9, 9, 9), 0.0, 0, ""),          # rect
         (1, 10, 20, 0.0, 0.0, (0, 0, 0), 16.0, 3, "hi"),  # text
         (2, 0, 5, 100, 5, (1, 2, 3), 2.0, 0, ""),         # line
         (4, 8, 8, 24, 24, (0, 0, 0), 0.0, 7, "")]         # image
_scaled = scale_cmds(_cmds, 2.0)
check("HiDPI scaling maps coords, font size, and thickness",
      _scaled[0][1:5] == (2, 4, 6, 8)
      and _scaled[1][1:3] == (20, 40) and _scaled[1][6] == 32.0
      and _scaled[2][6] == 4.0
      and _scaled[3][1:5] == (16, 16, 48, 48) and _scaled[3][7] == 7,
      str(_scaled))
check("HiDPI scaling at 1.0 is the identity",
      scale_cmds(_cmds, 1.0) is _cmds)

root.destroy()

# --- Native (Rust) core, if built ---
from browser import native

if native.available():
    rs_dom = native.parse_and_style(DEMO_PAGE, lambda hrefs: {})
    rs_nodes = tree_to_list(rs_dom, [])
    check("native node count matches", len(rs_nodes) == len(
        tree_to_list(demo_dom, [])), f"{len(rs_nodes)} nodes")
    rs_h1 = next(n for n in rs_nodes
                 if isinstance(n, Element) and n.tag == "h1")
    check("native h1 style matches",
          rs_h1.style["font-size"] == h1.style["font-size"]
          and rs_h1.style["color"] == h1.style["color"])

    # var() parity: both engines must agree on custom-property output
    VAR_PAGE = ("<html><head><style>"
                ":where(:root,:host){--c:#123456;--bg:var(--base,#0f0)}"
                "p{color:var(--c);background-color:var(--bg);"
                "font-size:var(--fs,2em);border:1px solid var(--gone)}"
                "</style></head><body><p>x</p></body></html>")
    py_var_dom = HTMLParser(VAR_PAGE).parse()
    style(py_var_dom, sorted(ua + CSSParser(
        "".join(c.text for n in tree_to_list(py_var_dom, [])
                if isinstance(n, Element) and n.tag == "style"
                for c in n.children if isinstance(c, Text))
    ).parse(), key=cascade_priority))
    py_var_p = next(n for n in tree_to_list(py_var_dom, [])
                    if isinstance(n, Element) and n.tag == "p")
    rs_var_p = next(n for n in tree_to_list(
        native.parse_and_style(VAR_PAGE, lambda hrefs: {}), [])
        if isinstance(n, Element) and n.tag == "p")
    check("native var() output matches python",
          rs_var_p.style.get("color") == py_var_p.style.get("color")
          == "#123456"
          and rs_var_p.style.get("background-color")
          == py_var_p.style.get("background-color") == "#0f0"
          and rs_var_p.style.get("font-size")
          == py_var_p.style.get("font-size") == "32px"
          and "border-color" not in rs_var_p.style
          and "border-color" not in py_var_p.style,
          f"rs={rs_var_p.style} py={py_var_p.style}")
    # inline SVG rasterizes to an image handle (naver search icon)
    from browser import textengine
    if textengine.available():
        SVG_PAGE = ('<html><body><svg viewBox="0 0 50 50">'
                    '<path d="M5 5H45V45H5Z" fill="#03c75a"/>'
                    '</svg></body></html>')
        svg_dom = HTMLParser(SVG_PAGE).parse()
        style(svg_dom, sorted(ua, key=cascade_priority))
        textengine.load_svgs(svg_dom)
        svg_el = next(n for n in tree_to_list(svg_dom, [])
                      if isinstance(n, Element) and n.tag == "svg")
        img = getattr(svg_el, "_img", None)
        check("inline svg rasterizes", img is not None
              and img[1] == 50 and img[2] == 50, str(img))
        import tkinter as _tk
        _r2 = _tk.Tk(); _r2.withdraw()
        svg_doc = DocumentLayout(svg_dom)
        svg_doc.layout(400)
        svg_cmds = paint_tree(svg_doc, [])
        _r2.destroy()
        check("svg paints as an image command",
              any(getattr(c, "image_id", None) == img[0]
                  for c in svg_cmds))
else:
    print("[SKIP] native ggcore not built - pure Python fallback active")

# --- URL parsing regressions (query-no-path, userinfo, IPv6, injection) ---
u = net.URL("http://host.example?q=1")
check("query with no path", u.host == "host.example" and u.path == "/?q=1",
      f"{u.host} {u.path}")
u = net.URL("https://user:pass@host.example/x")
check("userinfo stripped", u.host == "host.example")
u = net.URL("http://[::1]:8080/x")
check("IPv6 literal + port", u.host == "::1" and u.port == 8080
      and str(u) == "http://[::1]:8080/x", str(u))
try:
    net.URL("http://evil\r\nX-Injected: 1/")
    check("CRLF in host rejected", False)
except ValueError:
    check("CRLF in host rejected", True)
check("request path sanitized",
      "\r" not in net._safe_path("/a\r\nX: 1")
      and " " not in net._safe_path("/a b"))

# --- Real network fetch ---
headers, body = net.request(net.URL("https://example.com"))
check("HTTPS fetch example.com", "<html" in body.lower()
      and "example" in body.lower(), f"{len(body)} bytes")

# redirect propagates the final URL (http://google.com -> www.google.com)
_h, _b, final = net.request_text(net.URL("http://google.com/"))
check("redirect returns final URL", final.host != "google.com"
      and "google" in final.host, str(final))

headers, body = net.request(net.URL("about:home"))
check("about:home renders", "GG Browser" in body)

# --- Headless automation driver (needs the native wheel + JS) ---
if native.available():
    from browser.driver import Page

    dp = Page()
    js_page = ("data:text/html,<html><head><title>D</title></head><body>"
               "<div id=app></div><a href='/n' id=go>next</a><script>"
               "document.getElementById('app').textContent = 'rendered';"
               "var d = document.createElement('div');"
               "d.setAttribute('class','item'); d.textContent='x';"
               "document.body.appendChild(d);</script></body></html>")
    dp.goto(js_page)
    check("driver: JS content materializes",
          dp.text("#app") == "rendered", repr(dp.text("#app")))
    check("driver: engine-selector query finds JS-created node",
          len(dp.query_all(".item")) == 1)
    check("driver: evaluate() structured read-back",
          dp.evaluate("6 * 7") == 42)
    check("driver: semantic snapshot yields link role",
          any(s["role"] == "link" and s["href"] == "/n"
              for s in dp.snapshot()))
    click_page = ("data:text/html,<html><body><button id=b onclick="
                  "\"document.getElementById('o').textContent='hit';\">"
                  "go</button><div id=o>idle</div></body></html>")
    dp.goto(click_page)
    dp.click("#b")
    check("driver: click runs the handler", dp.text("#o") == "hit",
          repr(dp.text("#o")))
else:
    print("[SKIP] driver checks - native ggcore not built")

print(f"\n{passed} checks passed - engine pipeline OK")
