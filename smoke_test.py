"""Headless smoke test for the GG engine pipeline."""

import os
import tkinter

from browser import keyboard, net
from browser.html_parser import Element, HTMLParser, Text, tree_to_list
from browser.css_parser import CSSParser
from browser.style import (RuleIndex, cascade_priority, default_rules,
                           style)
from browser.layout import (HSTEP, VSTEP, BlockLayout, DocumentLayout,
                            ImageLayout, layout_tree_to_list, paint_tree,
                            sticky_offset)
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

# --- sequential focus and keyboard default actions ---
focus_dom = HTMLParser(
    '<a id="natural" href="/next">link</a>'
    '<button id="two" tabindex="2">two</button>'
    '<input id="one" tabindex="1">'
    '<button id="zero" tabindex="0">zero</button>'
    '<button id="negative" tabindex="-1">negative</button>'
    '<input id="disabled" disabled><input id="hidden" type="hidden">'
    '<div inert><button id="inert-child">inert</button></div>').parse()
focus_ids = [node.attributes.get("id")
             for node in keyboard.focus_order(focus_dom)]
check("focus order: positive tabindex then document order",
      focus_ids == ["one", "two", "natural", "zero"],
      repr(focus_ids))
focus_order = keyboard.focus_order(focus_dom)
check("Tab focus wraps in both directions",
      keyboard.next_focus(focus_dom, focus_order[-1]) is focus_order[0]
      and keyboard.next_focus(
          focus_dom, focus_order[0], reverse=True) is focus_order[-1])
focus_text = focus_order[1].children[0]
check("click focus resolves from button text to its control",
      keyboard.focus_target(focus_text) is focus_order[1])
check("Enter and Space choose native element default actions",
      keyboard.key_action(focus_order[2], "Enter") == "activate"
      and keyboard.key_action(focus_order[1], "Space") == "activate"
      and keyboard.key_action(focus_order[0], "Enter") == "submit"
      and keyboard.key_action(focus_order[0], "Space") == "text")

# --- CSS parsing ---
rules = CSSParser(
    "h1, .big { color: red; } #x p { font-size: 2em; }"
    "@media (max-width: 5px) { p { color: blue; } }"
    "a:hover { color: green; } div { margin: 8px 16px; }"
).parse()
check("CSS rule count (non-matching @media excluded, :hover kept)",
      len(rules) == 5, f"got {len(rules)}")
ua = default_rules()
check("UA stylesheet parses", len(ua) > 20, f"{len(ua)} rules")

# :hover / :focus match only while the shell marks the node
hov_dom = HTMLParser(
    "<div class=m><p>x</p><span>s</span></div><input>").parse()
hov_rules = sorted(
    default_rules() + CSSParser(
        "p { color: black; } p:hover { color: red; }"
        ".m:hover span { color: green; }"
        "input:focus { color: blue; }").parse(),
    key=cascade_priority)
_hp = next(n for n in tree_to_list(hov_dom, [])
           if isinstance(n, Element) and n.tag == "p")
_hs = next(n for n in tree_to_list(hov_dom, [])
           if isinstance(n, Element) and n.tag == "span")
_hi = next(n for n in tree_to_list(hov_dom, [])
           if isinstance(n, Element) and n.tag == "input")
style(hov_dom, RuleIndex(hov_rules))
check(":hover rules stay inert without hover state",
      _hp.style["color"] == "black"
      and _hs.style.get("color") != "green")
# hover chain: the element and its ancestors
cur = _hp
while cur is not None:
    if isinstance(cur, Element):
        cur.is_hovered = True
    cur = cur.parent
_hi.is_focused = True
style(hov_dom, RuleIndex(hov_rules))
check(":hover matches the hovered element",
      _hp.style["color"] == "red", _hp.style.get("color"))
check("ancestor :hover fires descendant rules",
      _hs.style.get("color") == "green", _hs.style.get("color"))
check(":focus matches the focused input",
      _hi.style.get("color") == "blue", _hi.style.get("color"))

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

# --- @media evaluation against viewport (M1 sweep) ---
_media_css = ("p{color:black} "
              "@media (min-width:768px){p{color:red}} "
              "@media (max-width:600px){p{color:blue}}")
def _media_color(vw):
    d = HTMLParser("<p>x</p>").parse()
    style(d, sorted(ua + CSSParser(_media_css, viewport_width=vw).parse(),
                    key=cascade_priority))
    return _find(d, "p").style["color"]
check("@media min-width applies on desktop viewport",
      _media_color(1280) == "red")
check("@media max-width applies on narrow viewport",
      _media_color(500) == "blue")
_screen_rules = CSSParser(
    "@media screen and (min-width:100px){p{font-weight:bold}} "
    "@media (max-width:1px){a{color:red}} div{color:green}").parse()
check("@media screen-and matches, non-matching skipped, flow continues",
      len(_screen_rules) == 2)

# --- :not / :nth-child / structural selectors (M1 sweep) ---
sel_dom = _styled(
    "li:not(.skip){color:red} li:first-child{font-weight:bold} "
    "li:last-child{font-style:italic} li:nth-child(2){text-align:center} "
    "li:nth-child(odd){white-space:nowrap}",
    "<ul><li>a</li><li class=skip>b</li><li>c</li></ul>")
_lis = [n for n in tree_to_list(sel_dom, [])
        if isinstance(n, Element) and n.tag == "li"]
check(":not() excludes matching, applies elsewhere",
      _lis[0].style["color"] == "red"
      and _lis[1].style.get("color") != "red")
check(":first-child / :last-child structural",
      _lis[0].style["font-weight"] == "bold"
      and _lis[2].style["font-style"] == "italic")
check(":nth-child(N) exact + odd/even",
      _lis[1].style["text-align"] == "center"
      and _lis[0].style["white-space"] == "nowrap"
      and _lis[2].style["white-space"] == "nowrap"
      and _lis[1].style.get("white-space") != "nowrap")

# gradient background paints its first color stop as a solid fill
from browser.layout import gradient_color as _grad
check("gradient -> first stop color",
      _grad("linear-gradient(to right, #03c75a, #fff)") == "#03c75a"
      and _grad("radial-gradient(circle, red, blue)") == "red"
      and _grad("#abc") == "")
grad_dom = _styled(
    "", '<div style="width:80px; height:20px; '
    'background:linear-gradient(90deg, #03c75a, #ffffff)">x</div>')
_gdoc = DocumentLayout(grad_dom)
_gdoc.layout(400)
_gcmds = paint_tree(_gdoc, [])
check("gradient box paints a solid fill",
      any(getattr(c, "color", "") == "#03c75a" for c in _gcmds),
      str([getattr(c, "color", None) for c in _gcmds]))

# line-height, white-space:nowrap, text-overflow:ellipsis, box-shadow
from browser.layout import (line_height_factor as _lhf,
                            box_shadow as _bshadow)
class _N:  # minimal node stub for the pure helpers
    def __init__(self, **s): self.style = s
check("line-height: normal keeps 1.25",
      abs(_lhf(_N(), 16.0) - 1.25) < 1e-9)
check("line-height: unitless multiplier",
      abs(_lhf(_N(**{"line-height": "2"}), 16.0) - 2.0) < 1e-9)
check("line-height: px over font size",
      abs(_lhf(_N(**{"line-height": "24px"}), 16.0) - 1.5) < 1e-9)
check("box-shadow: offset + color parsed",
      _bshadow("2px 4px 8px #03c75a") == (2.0, 4.0, "#03c75a"))
check("box-shadow: inset / none ignored",
      _bshadow("inset 0 0 2px red") is None
      and _bshadow("none") is None)

nowrap_dom = _styled(
    "", '<div style="width:30px; white-space:nowrap">'
    'aaa bbb ccc ddd eee</div>')
_ndoc = DocumentLayout(nowrap_dom)
_ndoc.layout(400)
_nlist = layout_tree_to_list(_ndoc, [])
_nlines = [b for b in _nlist if type(b).__name__ == "LineLayout"]
check("white-space:nowrap keeps text on one line",
      len(_nlines) == 1, f"{len(_nlines)} lines")

ell_dom = _styled(
    "", '<div style="width:40px; white-space:nowrap; '
    'text-overflow:ellipsis">verylongtext</div>')
_edoc = DocumentLayout(ell_dom)
_edoc.layout(400)
_etexts = [c.text for c in paint_tree(_edoc, []) if hasattr(c, "text")]
check("text-overflow:ellipsis truncates with an ellipsis",
      any(t.endswith("…") and t != "verylongtext" for t in _etexts),
      str(_etexts))

shadow_dom = _styled(
    "", '<div style="width:40px; height:20px; background-color:#fff; '
    'box-shadow:3px 3px 4px #888888">x</div>')
_sdoc = DocumentLayout(shadow_dom)
_sdoc.layout(400)
_scmds = paint_tree(_sdoc, [])
check("box-shadow paints an offset rect behind the box",
      any(getattr(c, "color", "") == "#888888" for c in _scmds))

# @font-face descriptor extraction (loadable sources only)
from browser.webfonts import parse_font_faces
_faces = parse_font_faces(["""@font-face { font-family: 'My Face'; src: url('fonts/a.woff2') format('woff2'), url(fonts/a.ttf) format('truetype'); }@font-face { font-family: IconFont; font-weight: 700; src: url(icons.otf); }@font-face { font-family: WoffOnly; src: url(x.woff2); }@font-face { font-family: DataFont; src: url(data:font/ttf;base64,AAAA); }"""])
check("@font-face picks the loadable source",
      ("my face", False, False, "fonts/a.ttf") in _faces, str(_faces))
check("@font-face numeric weight maps to bold",
      ("iconfont", True, False, "icons.otf") in _faces, str(_faces))
check("woff2-only faces are skipped",
      not any(f[0] == "woffonly" for f in _faces), str(_faces))
check("data: font sources are accepted",
      any(f[0] == "datafont" and f[3].startswith("data:")
          for f in _faces), str(_faces))

# CSS width/height outrank HTML attributes on replaced elements
ri_dom = _styled("img.big{width:100px; height:50px} "
                 "img.half{width:48px}",
                 '<img class=big width=10 height=10>'
                 '<img class=half>'
                 '<img width=30 height=20>')
_ridoc = DocumentLayout(ri_dom)
_ridoc.layout(400)
_ims = [o for o in layout_tree_to_list(_ridoc, [])
        if isinstance(o, ImageLayout)]
check("CSS size wins over img attributes",
      abs(_ims[0].width - 100) < 1 and abs(_ims[0].height - 50) < 1,
      f"{_ims[0].width:.0f}x{_ims[0].height:.0f}")
check("CSS width alone keeps the intrinsic ratio",
      abs(_ims[1].width - 48) < 1 and abs(_ims[1].height - 48) < 1,
      f"{_ims[1].width:.0f}x{_ims[1].height:.0f}")
check("attrs still size an unstyled img",
      abs(_ims[2].width - 30) < 1 and abs(_ims[2].height - 20) < 1,
      f"{_ims[2].width:.0f}x{_ims[2].height:.0f}")

# transform: translate shifts the painted subtree, layout unaffected
tf_dom = _styled(
    "", '<div style="width:50px; height:20px; background-color:#c0ffee;'
    ' transform: translate(30px, 10px)">t</div>'
    '<div style="height:20px; background-color:#123123">after</div>')
_tdoc = DocumentLayout(tf_dom)
_tdoc.layout(400)
_tcmds = paint_tree(_tdoc, [])
_tbox = next(c for c in _tcmds if getattr(c, "color", "") == "#c0ffee")
_abox = next(c for c in _tcmds if getattr(c, "color", "") == "#123123")
# the untransformed sibling anchors the expected geometry: without
# the transform the first div would sit directly above it
check("transform:translate shifts the painted box",
      _tbox.left == _abox.left + 30 and _tbox.top == _abox.top - 10,
      f"box=({_tbox.left}, {_tbox.top}) anchor=({_abox.left}, "
      f"{_abox.top})")

# percentage translate resolves against the element's own border box
tp_dom = _styled(
    "", '<div style="width:60px; height:40px; background-color:#facade;'
    ' transform: translate(-50%, -50%)">p</div>'
    '<div style="height:20px; background-color:#123123">anchor</div>')
_tpdoc = DocumentLayout(tp_dom)
_tpdoc.layout(400)
_tpcmds = paint_tree(_tpdoc, [])
_tpbox = next(c for c in _tpcmds
              if getattr(c, "color", "") == "#facade")
_tpanchor = next(c for c in _tpcmds
                 if getattr(c, "color", "") == "#123123")
check("transform % translate uses own border box",
      _tpbox.left == _tpanchor.left - 30
      and _tpbox.top == _tpanchor.top - 40 - 20,
      f"box=({_tpbox.left}, {_tpbox.top}) anchor=({_tpanchor.left}, "
      f"{_tpanchor.top})")

# scale(0) hides the subtree
ts_dom = _styled(
    "", '<div style="height:20px; background-color:#dead00;'
    ' transform: scale(0)">gone</div>')
_tsdoc = DocumentLayout(ts_dom)
_tsdoc.layout(400)
_tscmds = paint_tree(_tsdoc, [])
check("transform scale(0) hides the subtree",
      not any(getattr(c, "color", "") == "#dead00" for c in _tscmds)
      and not any(getattr(c, "text", "") == "gone" for c in _tscmds))

# z-index reorders positioned siblings (lower z paints first = below)
z_dom = _styled(
    "", '<div>'
    '<div style="position:relative; z-index:2; '
    'background-color:#aaaaaa; height:10px">A</div>'
    '<div style="position:relative; z-index:1; '
    'background-color:#bbbbbb; height:10px">B</div></div>')
_zdoc = DocumentLayout(z_dom)
_zdoc.layout(400)
_zcmds = paint_tree(_zdoc, [])
_zorder = [c.color for c in _zcmds
           if getattr(c, "color", "") in ("#aaaaaa", "#bbbbbb")]
check("z-index paints lower value first (below higher)",
      _zorder == ["#bbbbbb", "#aaaaaa"], str(_zorder))
# without z-index, document order is preserved (stable)
z2_dom = _styled(
    "", '<div><div style="background-color:#111111; height:10px">A</div>'
    '<div style="background-color:#222222; height:10px">B</div></div>')
_z2doc = DocumentLayout(z2_dom)
_z2doc.layout(400)
_z2 = [c.color for c in paint_tree(_z2doc, [])
       if getattr(c, "color", "") in ("#111111", "#222222")]
check("no z-index keeps document paint order",
      _z2 == ["#111111", "#222222"], str(_z2))

# M4 live loop: tick fires timers in real-time slices; refresh
# re-styles the mutated DOM
from browser import native
_load_timings = {}
native.load_document(
    '<p>timed load</p>', lambda h: {}, None, timings=_load_timings)
_timing_keys = {
    "html_parse", "scripts", "stylesheet_wait", "style_compute",
    "dom_export", "load_document_total",
}
check("native load reports structured stage timings",
      _timing_keys.issubset(_load_timings)
      and all(_load_timings[key] >= 0 for key in _timing_keys)
      and _load_timings["load_document_total"]
      >= _load_timings["html_parse"], repr(_load_timings))
if native.async_available():
    _live_html = (
        '<div id="live">start</div><script>'
        'var n = 0;'
        'setInterval(function () { n += 1;'
        ' document.getElementById("live").textContent = "tick" + n; },'
        ' 30);</script>')
    _lnodes, _ldoc, _lcss, _llogs = native.load_document(
        _live_html, lambda h: {}, lambda s: {})
    _v0 = _ldoc.dom_version()
    _ldoc.tick(100.0)  # ~3 interval firings
    check("live tick mutates the DOM (version bumps)",
          _ldoc.dom_version() > _v0)
    _lnodes2 = native.refresh(_ldoc, _lcss)
    _ltexts = [t.text for t in tree_to_list(_lnodes2, [])
               if isinstance(t, Text) and t.text.strip()]
    check("refresh shows the timer-driven text",
          any(t.startswith("tick") for t in _ltexts), str(_ltexts))
    # bounded: a 1ms tick fires nothing further
    _v1 = _ldoc.dom_version()
    _ldoc.tick(1.0)
    check("a 1ms tick is quiet (real-time pacing, no fast-forward)",
          _ldoc.dom_version() == _v1)

    # settle_async must report a timer-only DOM mutation even when the
    # callback emits no console output and performs no fetch.
    _snodes, _sdoc, _scss, _slogs = native.load_document(
        '<div id="settled">before</div><script>'
        'setTimeout(function(){document.getElementById("settled")'
        '.textContent="after";}, 10);</script>',
        lambda h: {}, lambda s: {})
    _sv0 = _sdoc.dom_version()
    _settled_changed = native.settle_async(
        _sdoc, _scss, net.URL("about:blank"), max_rounds=20)
    check("settle detects timer-only DOM mutation",
          _settled_changed and _sdoc.dom_version() > _sv0)
    _snodes2 = native.refresh(_sdoc, _scss)
    _settled_text = [t.text for t in tree_to_list(_snodes2, [])
                     if isinstance(t, Text)]
    check("settle refresh exposes timer-only text",
          "after" in _settled_text, str(_settled_text))

    # P2 script scheduling: blocking scripts preserve parser order, async
    # scripts run when their fetch completes, and defer/modules finish before
    # DOMContentLoaded in document order.
    import time as _script_time

    _schedule_html = (
        '<script>console.log("setup");'
        'document.addEventListener("DOMContentLoaded",function(){'
        'console.log("dcl:"+document.readyState);});'
        'window.addEventListener("load",function(){'
        'console.log("load:"+document.readyState);});</script>'
        '<script async src="/slow.js"></script>'
        '<script async src="/fast.js"></script>'
        '<script src="/block.js"></script>'
        '<script>console.log("after")</script>'
        '<script defer src="/defer-1.js"></script>'
        '<script defer src="/defer-2.js"></script>')
    _schedule_sources = {
        "/slow.js": (0.20, 'console.log("slow")'),
        "/fast.js": (0.005, 'console.log("fast")'),
        "/block.js": (0.08, 'console.log("block")'),
        "/defer-1.js": (0.005, 'console.log("defer-1")'),
        "/defer-2.js": (0.0, 'console.log("defer-2")'),
    }

    def _fetch_scheduled(srcs):
        src = srcs[0]
        delay, code = _schedule_sources[src]
        _script_time.sleep(delay)
        return {src: code}

    _onodes, _odoc, _ocss, _order_logs = native.load_document(
        _schedule_html, lambda hrefs: {}, _fetch_scheduled,
        js_budget=2.0)
    check("script async/defer/blocking order follows lifecycle phases",
          _order_logs == [
              "setup", "fast", "block", "after", "defer-1",
              "defer-2", "dcl:interactive", "slow", "load:complete"],
          repr(_order_logs))

    _dynamic_sources = {
        "/dynamic.js": (0.03, 'console.log("dynamic")'),
        "/ordered-1.js": (0.04, 'console.log("ordered-1")'),
        "/ordered-2.js": (0.0, 'console.log("ordered-2")'),
    }

    def _fetch_dynamic(srcs):
        src = srcs[0]
        delay, code = _dynamic_sources[src]
        _script_time.sleep(delay)
        return {src: code}

    _dynamic_html = (
        '<html><body><script>'
        'document.addEventListener("DOMContentLoaded",function(){'
        'console.log("dynamic-dcl:"+document.readyState);});'
        'window.addEventListener("load",function(){'
        'console.log("dynamic-window-load:"+document.readyState);});'
        'var s=document.createElement("script");s.src="/dynamic.js";'
        's.onload=function(){console.log("dynamic-load:"+'
        'document.readyState);};document.body.appendChild(s);'
        'console.log("creator");</script></body></html>')
    _dnodes, _ddoc, _dcss, _dynamic_logs = native.load_document(
        _dynamic_html, lambda hrefs: {}, _fetch_dynamic,
        js_budget=2.0)
    check("dynamic script delays load but not DOMContentLoaded",
          _dynamic_logs == [
              "creator", "dynamic-dcl:interactive", "dynamic",
              "dynamic-load:interactive", "dynamic-window-load:complete"],
          repr(_dynamic_logs))

    _ordered_html = (
        '<html><body><script>document.addEventListener('
        '"DOMContentLoaded",function(){'
        'console.log("ordered-dcl");});'
        'var a=document.createElement("script");a.async=false;'
        'a.src="/ordered-1.js";document.body.appendChild(a);'
        'var b=document.createElement("script");b.async=false;'
        'b.src="/ordered-2.js";document.body.appendChild(b);'
        '</script></body></html>')
    _qnodes, _qdoc, _qcss, _ordered_logs = native.load_document(
        _ordered_html, lambda hrefs: {}, _fetch_dynamic,
        js_budget=2.0)
    check("dynamic async=false scripts keep insertion order",
          _ordered_logs == ["ordered-dcl", "ordered-1", "ordered-2"],
          repr(_ordered_logs))

    _event_html = (
        '<html><body><script>var bad=document.createElement("script");'
        'bad.src="/missing.js";'
        'bad.addEventListener("error",function(){'
        'console.log("script-error:"+document.readyState);});'
        'document.body.appendChild(bad);</script></body></html>')
    _enodes, _edoc, _ecss, _event_logs = native.load_document(
        _event_html, lambda hrefs: {}, lambda srcs: {}, js_budget=2.0)
    check("failed dynamic script dispatches error before load",
          _event_logs == ["script-error:loading"],
          repr(_event_logs))

    # ES modules: resolve imports against the importing module URL, evaluate
    # dependencies first, isolate top-level bindings, and cache by URL.
    _module_url = net.URL("https://modules.test/app/index.html")
    _module_sources = {
        "/app/main.js": (
            'import answer, {double as twice} from "./math.js";'
            'import "./side.js";'
            'export const result=twice(answer);'
            'console.log("module-main:"+result+":"+import.meta.url);'),
        "https://modules.test/app/math.js": (
            'export const base=21;'
            'export function double(x){return x*2}'
            'export default base;'),
        "https://modules.test/app/side.js": (
            'console.log("module-side");export const marker=1;'),
    }
    _module_fetches = []

    def _fetch_modules(srcs):
        src = srcs[0]
        _module_fetches.append(src)
        return ({src: _module_sources[src]}
                if src in _module_sources else {})

    _module_html = (
        '<html><body><script>'
        'document.addEventListener("DOMContentLoaded",function(){'
        'console.log("module-dcl:"+typeof result);});'
        'window.addEventListener("load",function(){'
        'console.log("module-load");});</script>'
        '<script type="module" src="/app/main.js"></script>'
        '</body></html>')
    _mnodes, _mdoc, _mcss, _module_logs = native.load_document(
        _module_html, lambda hrefs: {}, _fetch_modules,
        page_url=_module_url, js_budget=2.0)
    check("ES module graph resolves, evaluates dependencies, and isolates",
          _module_logs == [
              "module-side",
              "module-main:42:https://modules.test/app/main.js",
              "module-dcl:undefined", "module-load"],
          repr(_module_logs))
    check("ES module graph fetches each URL once",
          sorted(_module_fetches) == sorted([
              "/app/main.js",
              "https://modules.test/app/math.js",
              "https://modules.test/app/side.js"]),
          repr(_module_fetches))

    _shared_sources = {
        "/one.js": (
            'import {shared} from "./shared.js";'
            'console.log("one:"+shared);export const one=1;'),
        "/two.js": (
            'import {shared} from "./shared.js";'
            'console.log("two:"+shared);export const two=2;'),
        "https://modules.test/shared.js": (
            'console.log("shared-once");export const shared=7;'),
    }
    _shared_fetches = []

    def _fetch_shared(srcs):
        src = srcs[0]
        _shared_fetches.append(src)
        return {src: _shared_sources[src]}

    _shared_html = (
        '<html><body><script type="module" src="/one.js"></script>'
        '<script type="module" src="/two.js"></script></body></html>')
    _xnodes, _xdoc, _xcss, _shared_logs = native.load_document(
        _shared_html, lambda hrefs: {}, _fetch_shared,
        page_url=_module_url, js_budget=2.0)
    check("shared ES module dependency evaluates exactly once",
          _shared_logs == ["shared-once", "one:7", "two:7"],
          repr(_shared_logs))
    check("shared ES module dependency fetches exactly once",
          _shared_fetches.count("https://modules.test/shared.js") == 1,
          repr(_shared_fetches))

    _live_module_sources = {
        "/live-main.js": (
            'import * as state from "./live-state.js";'
            'setTimeout(function(){console.log("module-live:"+'
            'state.count);},10);'),
        "https://modules.test/live-state.js": (
            'export let count=1;'
            'setTimeout(function(){count=2;},5);'),
    }

    def _fetch_live_module(srcs):
        src = srcs[0]
        return {src: _live_module_sources[src]}

    _vnodes, _vdoc, _vcss, _live_module_logs = native.load_document(
        '<html><body><script type="module" src="/live-main.js">'
        '</script></body></html>', lambda hrefs: {}, _fetch_live_module,
        page_url=_module_url, js_budget=2.0)
    _live_tick_logs, _live_tick_fetches = _vdoc.tick(20.0)
    check("ES module namespace exposes an updated exported binding",
          _live_module_logs == []
          and list(_live_tick_logs) == ["module-live:2"]
          and not _live_tick_fetches,
          repr((_live_module_logs, _live_tick_logs,
                _live_tick_fetches)))

    _named_live_sources = {
        "/named-live-main.js": (
            'import {count} from "./named-live-state.js";'
            'setTimeout(function(){console.log("named-live:"+count);},10);'),
        "https://modules.test/named-live-state.js": (
            'export let count=1;'
            'setTimeout(function(){count=3;},5);'),
    }

    def _fetch_named_live(srcs):
        src = srcs[0]
        return {src: _named_live_sources[src]}

    _nv_nodes, _nv_doc, _nv_css, _named_live_logs = native.load_document(
        '<html><body><script type="module" '
        'src="/named-live-main.js"></script></body></html>',
        lambda hrefs: {}, _fetch_named_live,
        page_url=_module_url, js_budget=2.0)
    _named_live_tick_logs, _named_live_tick_fetches = _nv_doc.tick(20.0)
    check("named ES import reads remain live in later callbacks",
          _named_live_logs == []
          and list(_named_live_tick_logs) == ["named-live:3"]
          and not _named_live_tick_fetches,
          repr((_named_live_logs, list(_named_live_tick_logs))))

    _lexical_live_sources = {
        "/lexical-live-main.js": (
            'import {value} from "./lexical-live-state.js";'
            'function parameter(value){return value;}'
            'function destructured({value}){return value;}'
            'var arrow=(value)=>value+1;'
            'try{throw 6;}catch(value){'
            'console.log("lexical-catch:"+value);}'
            '{let value=4;console.log("lexical-block:"+value);}'
            '{function value(){return 10;}'
            'console.log("lexical-function:"+value());}'
            'for(let value of [7]){console.log("lexical-for:"+value);}'
            'var shorthand={value};var keyed={value:3};'
            'var methods={value(value){return value;}};'
            'class Box{value(value){return value;}}'
            'var pattern=/value/;'
            'console.log("lexical-values:"+parameter(2)+":"+'
            'destructured({value:3})+":"+arrow(4)+":"+'
            'shorthand.value+":"+keyed.value+":"+'
            'methods.value(11)+":"+(new Box()).value(12)+":"+'
            'value+":"+'
            'pattern.test("value"));'
            'setTimeout(function(){console.log(`lexical-template:${value}`);'
            '},10);'),
        "https://modules.test/lexical-live-state.js": (
            'export let value=1;'
            'setTimeout(function(){value=8;},5);'),
    }

    def _fetch_lexical_live(srcs):
        src = srcs[0]
        return {src: _lexical_live_sources[src]}

    _ll_nodes, _ll_doc, _ll_css, _lexical_live_logs = (
        native.load_document(
            '<html><body><script type="module" '
            'src="/lexical-live-main.js"></script></body></html>',
            lambda hrefs: {}, _fetch_lexical_live,
            page_url=_module_url, js_budget=2.0))
    check("import live reads respect parameter, catch, and block shadowing",
          _lexical_live_logs == [
              "lexical-catch:6", "lexical-block:4",
              "lexical-function:10", "lexical-for:7",
              "lexical-values:2:3:5:1:3:11:12:1:true"],
          repr(_lexical_live_logs))
    _lexical_tick_logs, _lexical_tick_fetches = _ll_doc.tick(20.0)
    check("object shorthand and template expressions keep lexical live reads",
          list(_lexical_tick_logs) == ["lexical-template:8"]
          and not _lexical_tick_fetches,
          repr((list(_lexical_tick_logs), _lexical_tick_fetches)))

    _write_import_sources = {
        "/write-import-main.js": (
            'import {value} from "./write-import-state.js";'
            'value=9;console.log("write-import-wrong");'),
        "https://modules.test/write-import-state.js": (
            'export const value=1;'),
    }

    def _fetch_write_import(srcs):
        src = srcs[0]
        return {src: _write_import_sources[src]}

    _write_import_html = (
        '<html><body><script>var target=document.getElementById("write-import");'
        'target.addEventListener("error",function(){'
        'console.log("write-import-error");});</script>'
        '<script id="write-import" type="module" '
        'src="/write-import-main.js"></script></body></html>')
    _wi_nodes, _wi_doc, _wi_css, _write_import_logs = native.load_document(
        _write_import_html, lambda hrefs: {}, _fetch_write_import,
        page_url=_module_url, js_budget=2.0)
    check("assigning to an imported binding fails module evaluation",
          len(_write_import_logs) == 2
          and _write_import_logs[0].startswith("[gg-js error]")
          and _write_import_logs[1] == "write-import-error",
          repr(_write_import_logs))

    _redeclare_import_sources = {
        "/redeclare-import-main.js": (
            'import {value} from "./redeclare-import-state.js";'
            'const value=2;console.log(value);'),
        "https://modules.test/redeclare-import-state.js": (
            'export const value=1;'),
    }

    def _fetch_redeclare_import(srcs):
        src = srcs[0]
        return {src: _redeclare_import_sources[src]}

    _redeclare_import_html = (
        '<html><body><script>var target=document.getElementById("redeclare");'
        'target.addEventListener("error",function(){'
        'console.log("redeclare-import-error");});</script>'
        '<script id="redeclare" type="module" '
        'src="/redeclare-import-main.js"></script></body></html>')
    _ri_nodes, _ri_doc, _ri_css, _redeclare_import_logs = (
        native.load_document(
            _redeclare_import_html, lambda hrefs: {},
            _fetch_redeclare_import,
            page_url=_module_url, js_budget=2.0))
    check("module-scope declarations cannot redeclare imports",
          len(_redeclare_import_logs) == 2
          and _redeclare_import_logs[0].startswith("[gg module error]")
          and _redeclare_import_logs[1] == "redeclare-import-error",
          repr(_redeclare_import_logs))

    _barrel_sources = {
        "/barrel-main.js": (
            'import * as api from "./barrel.js";'
            'console.log("barrel:"+api.value+":"+api.inc(4)+":"+'
            'api.tag);'),
        "https://modules.test/barrel.js": (
            'export {default as value, inc} from "./dep.js";'
            'export * from "./extra.js";'),
        "https://modules.test/dep.js": (
            'const base=4;export default base;'
            'export function inc(x){return x+1}'),
        "https://modules.test/extra.js": 'export const tag="ok";',
    }

    def _fetch_barrel(srcs):
        src = srcs[0]
        return {src: _barrel_sources[src]}

    _bnodes, _bdoc, _bcss, _barrel_logs = native.load_document(
        '<html><body><script type="module" src="/barrel-main.js">'
        '</script></body></html>', lambda hrefs: {}, _fetch_barrel,
        page_url=_module_url, js_budget=2.0)
    check("ES module namespace, default, named, and star re-exports work",
          _barrel_logs == ["barrel:4:5:ok"], repr(_barrel_logs))

    _module_error_html = (
        '<html><body><script>window.moduleFailed=false;'
        'var target=document.getElementById("bad-module");'
        'target.addEventListener("error",function(){'
        'console.log("module-error:"+document.readyState);});</script>'
        '<script id="bad-module" type="module" src="/bad.js"></script>'
        '</body></html>')
    _bad_sources = {"/bad.js": 'import x from "bare-package";'}
    _znodes, _zdoc, _zcss, _module_error_logs = native.load_document(
        _module_error_html, lambda hrefs: {},
        lambda srcs: {srcs[0]: _bad_sources[srcs[0]]},
        page_url=_module_url, js_budget=2.0)
    check("invalid module graph dispatches script error",
          len(_module_error_logs) == 2
          and _module_error_logs[0].startswith("[gg module error]")
          and _module_error_logs[1] == "module-error:loading",
          repr(_module_error_logs))

    _inline_sources = {
        "https://modules.test/app/inline-dep.js": (
            'console.log("inline-dep");export const value=9;')
    }
    _inline_html = (
        '<html><body><script type="module">'
        'import {value} from "./inline-dep.js";'
        'console.log("inline-module:"+value+":"+import.meta.url);'
        '</script><script>console.log("classic-after-inline")</script>'
        '<script>document.addEventListener("DOMContentLoaded",function(){'
        'console.log("inline-dcl");});</script></body></html>')
    _inodes, _idoc, _icss, _inline_logs = native.load_document(
        _inline_html, lambda hrefs: {},
        lambda srcs: {srcs[0]: _inline_sources[srcs[0]]},
        page_url=_module_url, js_budget=2.0)
    check("inline module is isolated and deferred before DOMContentLoaded",
          _inline_logs == [
              "classic-after-inline", "inline-dep",
              "inline-module:9:https://modules.test/app/index.html",
              "inline-dcl"], repr(_inline_logs))

    _cycle_sources = {
        "/cycle-a.js": (
            'import "./cycle-b.js";console.log("cycle-a");'
            'export const a=1;'),
        "https://modules.test/cycle-b.js": (
            'import "./cycle-a.js";console.log("cycle-b");'
            'export const b=2;'),
    }
    _cycle_fetches = []

    def _fetch_cycle(srcs):
        src = srcs[0]
        _cycle_fetches.append(src)
        return {src: _cycle_sources[src]}

    _cnodes, _cdoc, _ccss, _cycle_logs = native.load_document(
        '<html><body><script type="module" src="/cycle-a.js">'
        '</script></body></html>', lambda hrefs: {}, _fetch_cycle,
        page_url=_module_url, js_budget=2.0)
    check("cyclic module graph terminates and evaluates each module once",
          _cycle_logs == ["cycle-b", "cycle-a"]
          and _cycle_fetches.count("/cycle-a.js") == 1
          and _cycle_fetches.count("https://modules.test/cycle-b.js") == 1,
          repr((_cycle_logs, _cycle_fetches)))

    _dynamic_module_sources = {
        "/dynamic-module.js": (
            'console.log("dynamic-module");export const ready=true;')
    }

    def _fetch_dynamic_module(srcs):
        src = srcs[0]
        _script_time.sleep(0.02)
        return {src: _dynamic_module_sources[src]}

    _dynamic_module_html = (
        '<html><body><script>'
        'document.addEventListener("DOMContentLoaded",function(){'
        'console.log("dynamic-module-dcl");});'
        'window.addEventListener("load",function(){'
        'console.log("dynamic-module-window-load");});'
        'var dm=document.createElement("script");dm.type="module";'
        'dm.src="/dynamic-module.js";dm.onload=function(){'
        'console.log("dynamic-module-load");};document.body.appendChild(dm);'
        'console.log("dynamic-module-created");</script></body></html>')
    _ynodes, _ydoc, _ycss, _dynamic_module_logs = native.load_document(
        _dynamic_module_html, lambda hrefs: {}, _fetch_dynamic_module,
        page_url=net.URL("https://modules.test/index.html"),
        js_budget=2.0)
    check("dynamic module is async and delays load, not DOMContentLoaded",
          _dynamic_module_logs == [
              "dynamic-module-created", "dynamic-module-dcl",
              "dynamic-module", "dynamic-module-load",
              "dynamic-module-window-load"],
          repr(_dynamic_module_logs))

    _import_sources = {
        "/import-main.js": (
            'console.log("import-main");setTimeout(function(){'
            'console.log("import-before");import("./lazy.js").then('
            'function(m){console.log("import-value:"+m.value);});},10);'),
        "https://modules.test/lazy.js": (
            'console.log("lazy-evaluated");export const value=8;'),
    }
    _import_fetches = []

    def _fetch_import(srcs):
        src = srcs[0]
        _import_fetches.append(src)
        return {src: _import_sources[src]}

    _j_nodes, _j_doc, _j_css, _import_logs = native.load_document(
        '<html><body><script type="module" src="/import-main.js">'
        '</script></body></html>', lambda hrefs: {}, _fetch_import,
        page_url=net.URL("https://modules.test/index.html"),
        js_budget=2.0)
    _import_tick_logs, _import_tick_fetches = _j_doc.tick(10.0)
    check("dynamic import defers evaluation and resolves its namespace",
          _import_logs == ["import-main"]
          and list(_import_tick_logs) == [
              "import-before", "lazy-evaluated", "import-value:8"]
          and not _import_tick_fetches,
          repr((_import_logs, list(_import_tick_logs))))
    check("literal dynamic import participates in the URL fetch cache",
          _import_fetches.count("https://modules.test/lazy.js") == 1,
          repr(_import_fetches))

    _missing_import_sources = {
        "/missing-import-main.js": (
            'console.log("missing-import-main");setTimeout(function(){'
            'import("./not-found.js").then(function(){'
            'console.log("missing-import-wrong");},function(error){'
            'console.log("missing-import:"+error.name);});},1);')
    }

    def _fetch_missing_import(srcs):
        src = srcs[0]
        return ({src: _missing_import_sources[src]}
                if src in _missing_import_sources else {})

    _mi_nodes, _mi_doc, _mi_css, _missing_import_logs = (
        native.load_document(
            '<html><body><script type="module" '
            'src="/missing-import-main.js"></script></body></html>',
            lambda hrefs: {}, _fetch_missing_import,
            page_url=net.URL("https://modules.test/index.html"),
            js_budget=2.0))
    _missing_import_tick_logs, _missing_import_tick_fetches = (
        _mi_doc.tick(1.0))
    check("failed dynamic import rejects without failing its parent module",
          _missing_import_logs == ["missing-import-main"]
          and list(_missing_import_tick_logs) == ["missing-import:TypeError"]
          and not _missing_import_tick_fetches,
          repr((_missing_import_logs,
                list(_missing_import_tick_logs))))

    _computed_import_sources = {
        "/computed-import-main.js": (
            'var part="computed-lazy";console.log("computed-main");'
            'setTimeout(function(){console.log("computed-before");'
            'import("./"+part+".js").then(function(m){'
            'console.log("computed-value:"+m.value);});},10);'),
        "https://modules.test/computed-lazy.js": (
            'console.log("computed-evaluated");export const value=17;'),
    }
    _computed_import_fetches = []

    def _fetch_computed_import(srcs):
        src = srcs[0]
        _computed_import_fetches.append(src)
        return ({src: _computed_import_sources[src]}
                if src in _computed_import_sources else {})

    _ci_nodes, _ci_doc, _ci_css, _computed_import_logs = (
        native.load_document(
            '<html><body><script type="module" '
            'src="/computed-import-main.js"></script></body></html>',
            lambda hrefs: {}, _fetch_computed_import,
            page_url=net.URL("https://modules.test/index.html"),
            js_budget=2.0))
    _computed_tick_logs, _computed_requests = native.pump_script_requests(
        _ci_doc, 10.0)
    _computed_before_service = list(_computed_import_fetches)
    for _computed_request in _computed_requests:
        native.service_script_fetch(
            _ci_doc, net.URL("https://modules.test/index.html"),
            _computed_request)
    _computed_done_logs, _computed_done_requests = (
        native.pump_script_requests(_ci_doc, microtasks_only=True))
    check("computed dynamic import fetches only when its expression runs",
          _computed_import_logs == ["computed-main"]
          and _computed_tick_logs == ["computed-before"]
          and _computed_before_service == ["/computed-import-main.js"]
          and _computed_import_fetches == [
              "/computed-import-main.js",
              "https://modules.test/computed-lazy.js"],
          repr((_computed_import_logs, _computed_tick_logs,
                _computed_import_fetches)))
    check("computed dynamic import resolves and evaluates after loader exit",
          list(_computed_done_logs) == [
              "computed-evaluated", "computed-value:17"]
          and not _computed_done_requests,
          repr((list(_computed_done_logs), _computed_done_requests)))

    _computed_tla_sources = {
        "/computed-tla-main.js": (
            'var dependency="./computed-tla-dep.js";'
            'const loaded=await import(dependency);'
            'console.log("computed-tla:"+loaded.value);'),
        "https://modules.test/computed-tla-dep.js": (
            'console.log("computed-tla-dep");export const value=19;'),
    }

    def _fetch_computed_tla(srcs):
        src = srcs[0]
        return {src: _computed_tla_sources[src]}

    _computed_tla_html = (
        '<html><body><script>document.addEventListener('
        '"DOMContentLoaded",function(){console.log("computed-tla-dcl");});'
        '</script><script type="module" '
        'src="/computed-tla-main.js"></script></body></html>')
    _ct_nodes, _ct_doc, _ct_css, _computed_tla_logs = (
        native.load_document(
            _computed_tla_html, lambda hrefs: {}, _fetch_computed_tla,
            page_url=net.URL("https://modules.test/index.html"),
            js_budget=2.0))
    check("top-level await waits for a computed dynamic import",
          _computed_tla_logs == [
              "computed-tla-dep", "computed-tla:19",
              "computed-tla-dcl"],
          repr(_computed_tla_logs))

    _computed_cache_sources = {
        "/computed-cache-main.js": (
            'var target="./computed-cache-dep.js";setTimeout(function(){'
            'Promise.all([import(target),import(target)]).then(function(all){'
            'console.log("computed-cache:"+(all[0].value+all[1].value));'
            '});},1);'),
        "https://modules.test/computed-cache-dep.js": (
            'console.log("computed-cache-evaluated");'
            'export const value=5;'),
    }
    _computed_cache_fetches = []

    def _fetch_computed_cache(srcs):
        src = srcs[0]
        _computed_cache_fetches.append(src)
        return {src: _computed_cache_sources[src]}

    _cc_nodes, _cc_doc, _cc_css, _computed_cache_logs = (
        native.load_document(
            '<html><body><script type="module" '
            'src="/computed-cache-main.js"></script></body></html>',
            lambda hrefs: {}, _fetch_computed_cache,
            page_url=net.URL("https://modules.test/index.html"),
            js_budget=2.0))
    _cc_tick_logs, _cc_requests = native.pump_script_requests(
        _cc_doc, 1.0)
    for _cc_request in _cc_requests:
        native.service_script_fetch(
            _cc_doc, net.URL("https://modules.test/index.html"),
            _cc_request)
    _cc_done_logs, _cc_done_requests = native.pump_script_requests(
        _cc_doc, microtasks_only=True)
    check("concurrent computed imports share source and evaluation caches",
          _computed_cache_logs == [] and _cc_tick_logs == []
          and list(_cc_done_logs) == [
              "computed-cache-evaluated", "computed-cache:10"]
          and _computed_cache_fetches.count(
              "https://modules.test/computed-cache-dep.js") == 1
          and not _cc_done_requests,
          repr((list(_cc_done_logs), _computed_cache_fetches)))

    _computed_missing_sources = {
        "/computed-missing-main.js": (
            'var missing="not-here.js";setTimeout(function(){'
            'import("./"+missing).then(function(){console.log("wrong");},'
            'function(error){console.log("computed-missing:"+'
            'error.name);});},1);')
    }

    def _fetch_computed_missing(srcs):
        src = srcs[0]
        return ({src: _computed_missing_sources[src]}
                if src in _computed_missing_sources else {})

    _cm_nodes, _cm_doc, _cm_css, _computed_missing_logs = (
        native.load_document(
            '<html><body><script type="module" '
            'src="/computed-missing-main.js"></script></body></html>',
            lambda hrefs: {}, _fetch_computed_missing,
            page_url=net.URL("https://modules.test/index.html"),
            js_budget=2.0))
    _cm_tick_logs, _cm_requests = native.pump_script_requests(
        _cm_doc, 1.0)
    for _cm_request in _cm_requests:
        native.service_script_fetch(
            _cm_doc, net.URL("https://modules.test/index.html"),
            _cm_request)
    _cm_done_logs, _cm_done_requests = native.pump_script_requests(
        _cm_doc, microtasks_only=True)
    check("failed computed dynamic import rejects with TypeError",
          _computed_missing_logs == [] and _cm_tick_logs == []
          and list(_cm_done_logs) == ["computed-missing:TypeError"]
          and not _cm_done_requests,
          repr((_computed_missing_logs, _cm_tick_logs,
                list(_cm_done_logs))))

    _tla_sources = {
        "/tla-main.js": (
            'import {value} from "./tla-dep.js";'
            'console.log("tla-main:"+value);'),
        "https://modules.test/tla-dep.js": (
            'export const value=await Promise.resolve(9);'
            'console.log("tla-dep:"+value);'),
    }

    def _fetch_tla(srcs):
        src = srcs[0]
        return {src: _tla_sources[src]}

    _tla_html = (
        '<html><body><script>document.addEventListener('
        '"DOMContentLoaded",function(){console.log("tla-dcl");});'
        '</script><script type="module" src="/tla-main.js">'
        '</script></body></html>')
    _k_nodes, _k_doc, _k_css, _tla_logs = native.load_document(
        _tla_html, lambda hrefs: {}, _fetch_tla,
        page_url=net.URL("https://modules.test/index.html"),
        js_budget=2.0)
    check("top-level await settles dependencies before importer and DCL",
          _tla_logs == ["tla-dep:9", "tla-main:9", "tla-dcl"],
          repr(_tla_logs))

    _timer_tla_html = (
        '<html><body><script>document.addEventListener('
        '"DOMContentLoaded",function(){console.log("timer-tla-dcl");});'
        '</script><script type="module">'
        'await new Promise(function(resolve){setTimeout(resolve,5);});'
        'console.log("timer-tla-done");</script></body></html>')
    _l_nodes, _l_doc, _l_css, _timer_tla_logs = native.load_document(
        _timer_tla_html, lambda hrefs: {}, lambda srcs: {},
        page_url=net.URL("https://modules.test/index.html"),
        js_budget=2.0)
    check("top-level await can suspend on a timer before DCL",
          _timer_tla_logs == ["timer-tla-done", "timer-tla-dcl"],
          repr(_timer_tla_logs))

    _map_sources = {
        "/map-main.js": (
            'import {value} from "pkg";'
            'import {tool} from "lib/tool.js";'
            'console.log("import-map:"+value+":"+tool);'),
        "https://modules.test/app/mapped.js": 'export const value=4;',
        "https://modules.test/app/vendor/tool.js": 'export const tool=5;',
    }

    def _fetch_map(srcs):
        src = srcs[0]
        return {src: _map_sources[src]}

    _map_html = (
        '<html><head><script type="importmap">'
        '{"imports":{"pkg":"./mapped.js","lib/":"./vendor/"}}'
        '</script></head><body><script type="module" '
        'src="/map-main.js"></script></body></html>')
    _map_nodes, _map_doc, _map_css, _map_logs = native.load_document(
        _map_html, lambda hrefs: {}, _fetch_map,
        page_url=net.URL("https://modules.test/app/index.html"),
        js_budget=2.0)
    check("import maps resolve exact and prefix bare specifiers",
          _map_logs == ["import-map:4:5"], repr(_map_logs))

    _scoped_map_sources = {
        "/app/feature/main.js": (
            'import {value} from "pkg";'
            'console.log("scoped-map:"+value);'),
        "https://modules.test/app/scoped.js": 'export const value=7;',
    }

    def _fetch_scoped_map(srcs):
        src = srcs[0]
        return {src: _scoped_map_sources[src]}

    _scoped_map_html = (
        '<html><head><script type="importmap">'
        '{"imports":{"pkg":"./global.js"},"scopes":{'
        '"./feature/":{"pkg":"./scoped.js"}}}</script></head><body>'
        '<script type="module" src="/app/feature/main.js"></script>'
        '</body></html>')
    _sm_nodes, _sm_doc, _sm_css, _scoped_map_logs = (
        native.load_document(
            _scoped_map_html, lambda hrefs: {}, _fetch_scoped_map,
            page_url=net.URL("https://modules.test/app/index.html"),
            js_budget=2.0))
    check("import map scopes prefer the longest matching referrer prefix",
          _scoped_map_logs == ["scoped-map:7"],
          repr(_scoped_map_logs))

    _runtime_map_sources = {
        "/app/runtime-map-main.js": (
            'var packageName="runtime-pkg";import(packageName).then('
            'function(m){console.log("runtime-map:"+m.value);});'),
        "https://modules.test/app/runtime-mapped.js": (
            'export const value=12;'),
    }

    def _fetch_runtime_map(srcs):
        src = srcs[0]
        return {src: _runtime_map_sources[src]}

    _runtime_map_html = (
        '<html><head><script type="importmap">{"imports":{'
        '"runtime-pkg":"./runtime-mapped.js"}}</script></head><body>'
        '<script type="module" src="/app/runtime-map-main.js"></script>'
        '</body></html>')
    _rm_nodes, _rm_doc, _rm_css, _runtime_map_logs = (
        native.load_document(
            _runtime_map_html, lambda hrefs: {}, _fetch_runtime_map,
            page_url=net.URL("https://modules.test/app/index.html"),
            js_budget=2.0))
    _runtime_map_pump_logs, _runtime_map_requests = (
        native.pump_script_requests(_rm_doc, microtasks_only=True))
    for _runtime_map_request in _runtime_map_requests:
        native.service_script_fetch(
            _rm_doc, net.URL("https://modules.test/app/index.html"),
            _runtime_map_request)
    _runtime_map_done_logs, _runtime_map_done_requests = (
        native.pump_script_requests(_rm_doc, microtasks_only=True))
    check("computed dynamic import resolves through import maps",
          _runtime_map_logs == []
          and (list(_runtime_map_pump_logs)
               + list(_runtime_map_done_logs)) == ["runtime-map:12"]
          and not _runtime_map_done_requests,
          repr((_runtime_map_logs, _runtime_map_pump_logs,
                list(_runtime_map_done_logs))))

    _json_module_sources = {
        "/json-main.js": (
            'import config from "./config.json" with {type:"json"};'
            'import {config as forwarded} from "./json-forward.js";'
            'console.log("json-static:"+config.answer+":"+'
            'forwarded.name);'),
        "https://modules.test/config.json": (
            '{"answer":42,"name":"gg","__proto__":{"safe":true}}'),
        "https://modules.test/json-forward.js": (
            'export {default as config} from "./config.json" '
            'with {"type":"json"};'),
    }
    _json_module_fetches = []

    def _fetch_json_modules(srcs):
        src = srcs[0]
        _json_module_fetches.append(src)
        return ({src: _json_module_sources[src]}
                if src in _json_module_sources else {})

    _jm_nodes, _jm_doc, _jm_css, _json_module_logs = native.load_document(
        '<html><body><script type="module" src="/json-main.js">'
        '</script></body></html>', lambda hrefs: {}, _fetch_json_modules,
        page_url=net.URL("https://modules.test/index.html"),
        js_budget=2.0)
    check("static import attributes load JSON default exports and re-exports",
          _json_module_logs == ["json-static:42:gg"],
          repr(_json_module_logs))
    check("JSON modules share the URL fetch and evaluation cache",
          _json_module_fetches.count(
              "https://modules.test/config.json") == 1,
          repr(_json_module_fetches))

    _dynamic_json_sources = {
        "/dynamic-json-main.js": (
            'var target="./dynamic-data.json";setTimeout(function(){'
            'import(target,{with:{type:"json"}}).then(function(module){'
            'console.log("json-dynamic:"+module.default.value);});},1);'),
        "https://modules.test/dynamic-data.json": '{"value":17}',
    }
    _dynamic_json_fetches = []

    def _fetch_dynamic_json(srcs):
        src = srcs[0]
        _dynamic_json_fetches.append(src)
        return ({src: _dynamic_json_sources[src]}
                if src in _dynamic_json_sources else {})

    _dj_nodes, _dj_doc, _dj_css, _dynamic_json_logs = (
        native.load_document(
            '<html><body><script type="module" '
            'src="/dynamic-json-main.js"></script></body></html>',
            lambda hrefs: {}, _fetch_dynamic_json,
            page_url=net.URL("https://modules.test/index.html"),
            js_budget=2.0))
    check("dynamic import attributes defer JSON fetch until execution",
          _dynamic_json_logs == []
          and "https://modules.test/dynamic-data.json"
          not in _dynamic_json_fetches,
          repr((_dynamic_json_logs, _dynamic_json_fetches)))
    _dj_tick_logs, _dj_requests = native.pump_script_requests(
        _dj_doc, 1.0)
    for _dj_request in _dj_requests:
        native.service_script_fetch(
            _dj_doc, net.URL("https://modules.test/index.html"),
            _dj_request)
    _dj_done_logs, _dj_done_requests = native.pump_script_requests(
        _dj_doc, microtasks_only=True)
    check("dynamic import options resolve a JSON module namespace",
          _dj_tick_logs == []
          and list(_dj_done_logs) == ["json-dynamic:17"]
          and _dynamic_json_fetches.count(
              "https://modules.test/dynamic-data.json") == 1
          and not _dj_done_requests,
          repr((list(_dj_tick_logs), list(_dj_done_logs),
                _dynamic_json_fetches)))

    _bad_dynamic_attribute_sources = {
        "/bad-dynamic-attribute.js": (
            'setTimeout(function(){import("./never.css",'
            '{with:{type:"css"}}).then(function(){console.log("wrong");},'
            'function(error){console.log("bad-attribute:"+error.name);});'
            '},1);'),
    }
    _bad_dynamic_attribute_fetches = []

    def _fetch_bad_dynamic_attribute(srcs):
        src = srcs[0]
        _bad_dynamic_attribute_fetches.append(src)
        return ({src: _bad_dynamic_attribute_sources[src]}
                if src in _bad_dynamic_attribute_sources else {})

    _bda_nodes, _bda_doc, _bda_css, _bda_logs = native.load_document(
        '<html><body><script type="module" '
        'src="/bad-dynamic-attribute.js"></script></body></html>',
        lambda hrefs: {}, _fetch_bad_dynamic_attribute,
        page_url=net.URL("https://modules.test/index.html"),
        js_budget=2.0)
    _bda_tick_logs, _bda_requests = native.pump_script_requests(
        _bda_doc, 1.0)
    for _bda_request in _bda_requests:
        native.service_script_fetch(
            _bda_doc, net.URL("https://modules.test/index.html"),
            _bda_request)
    _bda_done_logs, _bda_done_requests = native.pump_script_requests(
        _bda_doc, microtasks_only=True)
    check("unsupported dynamic import attributes reject with TypeError",
          _bda_logs == [] and _bda_tick_logs == []
          and list(_bda_done_logs) == ["bad-attribute:TypeError"]
          and "https://modules.test/never.css"
          not in _bad_dynamic_attribute_fetches
          and not _bda_done_requests,
          repr((list(_bda_done_logs), _bad_dynamic_attribute_fetches)))

    _invalid_json_sources = {
        "/invalid-json-main.js": (
            'setTimeout(function(){import("./invalid.json",'
            '{with:{type:"json"}}).then(function(){console.log("wrong");},'
            'function(error){console.log("invalid-json:"+error.name);});'
            '},1);'),
        "https://modules.test/invalid.json": '{not valid JSON}',
    }

    def _fetch_invalid_json(srcs):
        src = srcs[0]
        return ({src: _invalid_json_sources[src]}
                if src in _invalid_json_sources else {})

    _ij_nodes, _ij_doc, _ij_css, _invalid_json_logs = native.load_document(
        '<html><body><script type="module" '
        'src="/invalid-json-main.js"></script></body></html>',
        lambda hrefs: {}, _fetch_invalid_json,
        page_url=net.URL("https://modules.test/index.html"),
        js_budget=2.0)
    _ij_tick_logs, _ij_requests = native.pump_script_requests(_ij_doc, 1.0)
    for _ij_request in _ij_requests:
        native.service_script_fetch(
            _ij_doc, net.URL("https://modules.test/index.html"),
            _ij_request)
    _ij_done_logs, _ij_done_requests = native.pump_script_requests(
        _ij_doc, microtasks_only=True)
    check("invalid JSON modules reject dynamic import with TypeError",
          _invalid_json_logs == [] and _ij_tick_logs == []
          and list(_ij_done_logs) == ["invalid-json:TypeError"]
          and not _ij_done_requests,
          repr((list(_ij_tick_logs), list(_ij_done_logs))))

    _bad_static_attribute_sources = {
        "/bad-static-attribute.js": (
            'import data from "./data.json" with {type:"css"};'
            'console.log(data);'),
    }

    def _fetch_bad_static_attribute(srcs):
        src = srcs[0]
        return ({src: _bad_static_attribute_sources[src]}
                if src in _bad_static_attribute_sources else {})

    _bad_static_attribute_html = (
        '<html><body><script>var target=document.getElementById("bad-attr");'
        'target.addEventListener("error",function(){'
        'console.log("bad-static-attribute-error");});</script>'
        '<script id="bad-attr" type="module" '
        'src="/bad-static-attribute.js"></script></body></html>')
    _bsa_nodes, _bsa_doc, _bsa_css, _bad_static_attribute_logs = (
        native.load_document(
            _bad_static_attribute_html, lambda hrefs: {},
            _fetch_bad_static_attribute,
            page_url=net.URL("https://modules.test/index.html"),
            js_budget=2.0))
    check("unsupported static import attributes fail module loading",
          len(_bad_static_attribute_logs) == 2
          and _bad_static_attribute_logs[0].startswith("[gg module error]")
          and _bad_static_attribute_logs[1]
          == "bad-static-attribute-error",
          repr(_bad_static_attribute_logs))

    _bad_map_html = (
        '<html><body><script>var badMap=document.getElementById("bad-map");'
        'badMap.addEventListener("error",function(){'
        'console.log("bad-map-error");});</script>'
        '<script type="importmap">{bad json</script>'
        '<script id="bad-map" type="module">console.log("wrong");'
        '</script></body></html>')
    _bm_nodes, _bm_doc, _bm_css, _bad_map_logs = native.load_document(
        _bad_map_html, lambda hrefs: {}, lambda srcs: {},
        page_url=net.URL("https://modules.test/app/index.html"),
        js_budget=2.0)
    check("invalid import map fails module loading and dispatches error",
          len(_bad_map_logs) == 2
          and _bad_map_logs[0].startswith("[gg module error]")
          and _bad_map_logs[1] == "bad-map-error",
          repr(_bad_map_logs))

# reader mode: EAGER-DATA JSON -> injected readable section
from browser import reader as _reader
_fixture = ('<html><body><div id=app></div><script>'
            'window["EAGER-DATA"] = {};'
            'window["EAGER-DATA"]["PC-NEWSSTAND-X"] = {"blocks": ['
            '{"title": "헤드라인 하나입니다", "url": "http://a.example/1"},'
            '{"title": "두번째 뉴스 제목", "url": "http://a.example/2"}]};'
            '</script></body></html>')
_sections = _reader.extract_sections(_fixture)
check("reader extracts title+url pairs from EAGER-DATA",
      len(_sections) == 1 and len(_sections[0][1]) == 2,
      str(_sections))
_injected = _reader.inject(_fixture)
check("reader injects a section before </body>",
      "gg-reader" in _injected
      and "헤드라인 하나입니다" in _injected
      and _injected.index("gg-reader") < _injected.index("</body>"))
check("reader is a no-op without EAGER-DATA",
      _reader.inject("<html><body><p>x</p></body></html>")
      == "<html><body><p>x</p></body></html>")
# ad-ish keys are skipped
_ad = ('<body><script>window["EAGER-DATA"]["PC-PREMIUM-AD"] = '
       '{"title": "광고제목입니다", "url": "http://ad.example"};'
       '</script></body>')
check("reader skips ad blocks", _reader.inject(_ad) == _ad)

# overflow:hidden emits a balanced clip push/pop around descendants
from browser.draw import DrawClipPush as _Push, DrawClipPop as _Pop
clip_dom = _styled(
    "", '<div style="width:50px; height:20px; overflow:hidden">'
    '<p>overflowing text that should be clipped</p></div>')
_cdoc = DocumentLayout(clip_dom)
_cdoc.layout(400)
_ccmds = paint_tree(_cdoc, [])
_pushes = [c for c in _ccmds if isinstance(c, _Push)]
_pops = [c for c in _ccmds if isinstance(c, _Pop)]
check("overflow:hidden emits one balanced clip push/pop",
      len(_pushes) == 1 and len(_pops) == 1)
# the push must precede the clipped text, the pop must follow it
_pi = _ccmds.index(_pushes[0])
_qi = _ccmds.index(_pops[0])
_ti = next((i for i, c in enumerate(_ccmds)
            if getattr(c, "text", "").startswith("overflowing")), None)
check("clip brackets the descendant text",
      _ti is not None and _pi < _ti < _qi)
# native tuples: push=kind 6, pop=kind 7, and clip survives scaling
_ntuple = _pushes[0].native(0.0)
check("clip push serializes as kind 6", _ntuple[0] == 6)
check("clip pop serializes as kind 7", _pops[0].native(0.0)[0] == 7)

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

# focused input paints a caret after the typed value; typed value
# replaces the placeholder
foc_dom = _styled(
    "", '<input value="gg" placeholder="search here">')
_finput = next(n for n in tree_to_list(foc_dom, [])
               if isinstance(n, Element) and n.tag == "input")
_finput.is_focused = True
_fdoc = DocumentLayout(foc_dom)
_fdoc.layout(400)
_fcmds = paint_tree(_fdoc, [])
from browser.draw import DrawLine as _DrawLine
_carets = [c for c in _fcmds
           if isinstance(c, _DrawLine) and c.color == "#333333"]
_ftexts = [c.text for c in _fcmds if hasattr(c, "text")]
check("focused input paints a caret", len(_carets) == 1,
      str([type(c).__name__ for c in _fcmds]))
check("typed value replaces placeholder",
      "gg" in _ftexts and "search here" not in _ftexts, str(_ftexts))
check("caret sits after the typed text",
      _carets and _carets[0].left > HSTEP, str(_carets and (
          _carets[0].left, _carets[0].top)))

# form GET submit assembles the query from named fields
from browser import forms as _forms
form_dom = HTMLParser(
    '<form action="https://search.example/search?old=1" method="get">'
    '<input name="query" value="한글 검색">'
    '<input type="hidden" name="where" value="nexearch">'
    '<input name="ignored-unnamed-type" type="submit" value="go">'
    '<input value="no-name-skipped">'
    '<input type="checkbox" name="unchecked" value="x">'
    '<input type="checkbox" name="checked" value="y" checked>'
    '</form>').parse()
_form = next(n for n in tree_to_list(form_dom, [])
             if isinstance(n, Element) and n.tag == "form")
_href = _forms.submit_href(_form)
check("form GET submit builds query (drops action's old query)",
      _href == "https://search.example/search?"
      "query=%ED%95%9C%EA%B8%80+%EA%B2%80%EC%83%89"
      "&where=nexearch&checked=y", str(_href))
_inp = next(n for n in tree_to_list(form_dom, [])
            if isinstance(n, Element)
            and n.attributes.get("name") == "query")
check("find_form walks up from an input",
      _forms.find_form(_inp) is _form)
_form.attributes["method"] = "post"
_post = _forms.prepare_submission(_form, net.URL(
    "https://origin.example/current?keep=no"))
check("POST form builds a URL-encoded body",
      _post.method == "POST"
      and _post.target == "https://search.example/search?old=1"
      and _post.content_type == "application/x-www-form-urlencoded"
      and _post.body == (
          "query=%ED%95%9C%EA%B8%80+%EA%B2%80%EC%83%89"
          "&where=nexearch&checked=y").encode("ascii"),
      repr(_post))
check("POST form has no GET-only href", _forms.submit_href(_form) is None)

_controls_dom = HTMLParser(
    '<form method="post" enctype="text/plain">'
    '<textarea name="memo">hello world</textarea>'
    '<select name="choice"><option value="a">A</option>'
    '<option value="b" selected>B</option></select>'
    '<input name="skip" value="x" disabled>'
    '<button name="go" value="yes">send</button>'
    '</form>').parse()
_controls_form = next(n for n in tree_to_list(_controls_dom, [])
                      if isinstance(n, Element) and n.tag == "form")
_submitter = next(n for n in tree_to_list(_controls_dom, [])
                  if isinstance(n, Element) and n.tag == "button")
_plain = _forms.prepare_submission(
    _controls_form, "https://origin.example/current",
    submitter=_submitter)
check("textarea/select/clicked submitter serialize in order",
      _plain.body == b"memo=hello world\r\nchoice=b\r\ngo=yes\r\n",
      repr(_plain.body))
check("find_submitter walks up from button text",
      _forms.find_submitter(_submitter.children[0]) is _submitter)

_upload_dom = HTMLParser(
    '<form action="/upload" method="post" enctype="multipart/form-data">'
    '<input name="title" value="report">'
    '<input type="file" name="attachment" value="C:/secret.txt">'
    '</form>').parse()
_upload_form = next(n for n in tree_to_list(_upload_dom, [])
                    if isinstance(n, Element) and n.tag == "form")
_file_input = next(n for n in tree_to_list(_upload_dom, [])
                   if isinstance(n, Element)
                   and n.attributes.get("type") == "file")
_forms.attach_file(
    _file_input, "notes.txt", b"hello\x00file", "text/plain")
_multipart = _forms.prepare_submission(
    _upload_form, "https://origin.example/form", boundary="GGBOUNDARY")
check("multipart form includes selected filename and exact bytes",
      _multipart.target == "/upload"
      and _multipart.content_type ==
      "multipart/form-data; boundary=GGBOUNDARY"
      and b'name="title"\r\n\r\nreport' in _multipart.body
      and b'filename="notes.txt"' in _multipart.body
      and b"hello\x00file" in _multipart.body
      and b"C:/secret.txt" not in _multipart.body,
      repr(_multipart.body))

_external_dom = HTMLParser(
    '<form id="main" action="/default" method="get">'
    '<input name="inside" value="1">'
    '<input name="other" value="skip" form="other-form">'
    '</form><form id="other-form"></form>'
    '<input name="outside" value="2" form="main">'
    '<button id="external-send" name="go" value="yes" form="main" '
    'formaction="/override" formmethod="post">send</button>').parse()
_external_form = next(n for n in tree_to_list(_external_dom, [])
                      if isinstance(n, Element)
                      and n.attributes.get("id") == "main")
_external_button = next(n for n in tree_to_list(_external_dom, [])
                        if isinstance(n, Element)
                        and n.attributes.get("id") == "external-send")
_external_post = _forms.prepare_submission(
    _external_form, "https://forms.test/start",
    submitter=_external_button)
check("external form= controls resolve their owner",
      _forms.find_form(_external_button.children[0]) is _external_form)
check("external controls serialize and submitter overrides apply",
      _external_post.method == "POST"
      and _external_post.target == "/override"
      and _external_post.body == b"inside=1&outside=2&go=yes",
      repr(_external_post))

_valid_dom = HTMLParser(
    '<form id="valid"><input id="mail" name="mail" type="email" required>'
    '<input id="code" name="code" pattern="[A-Z]{3}" value="ab">'
    '<input id="age" name="age" type="number" min="18" value="12">'
    '<input id="keep" name="keep" type="checkbox" checked>'
    '<button id="valid-send">send</button><button id="clear" type="reset">'
    'clear</button></form>').parse()
_valid_form = next(n for n in tree_to_list(_valid_dom, [])
                   if isinstance(n, Element) and n.tag == "form")
_valid_nodes = {n.attributes.get("id"): n
                for n in tree_to_list(_valid_dom, [])
                if isinstance(n, Element) and n.attributes.get("id")}
for _valid_ridx, _valid_node in enumerate(tree_to_list(_valid_dom, [])):
    if isinstance(_valid_node, Element):
        _valid_node._ridx = _valid_ridx
_defaults = _forms.capture_defaults(_valid_dom)
check("constraint validation reports invalid controls in order",
      [n.attributes.get("id") for n in
       _forms.invalid_controls(_valid_form)] == ["mail", "code", "age"])
_events = []
_blocked = _forms.activate_control(
    _valid_nodes["valid-send"], "https://forms.test/start", {},
    dispatch_event=lambda ridx, event, bubbles, cancelable, submitter:
        (_events.append(event) or [], False, False))
check("invalid controls block submit and fire invalid before submit",
      _blocked.submission is None and _events == ["invalid"] * 3,
      repr((_blocked, _events)))
_valid_nodes["mail"].attributes["value"] = "a@example.com"
_valid_nodes["code"].attributes["value"] = "ABC"
_valid_nodes["age"].attributes["value"] = "18"
check("valid required/type/pattern/min constraints allow submit",
      not _forms.invalid_controls(_valid_form))

_valid_nodes["mail"].attributes["value"] = "changed@example.com"
_valid_nodes["keep"].attributes.pop("checked", None)
_reset_events = []
_reset = _forms.activate_control(
    _valid_nodes["clear"], "https://forms.test/start", _defaults,
    dispatch_event=lambda ridx, event, bubbles, cancelable, submitter:
        (_reset_events.append(event) or [], True, False))
check("reset event restores captured control defaults",
      _reset.changed and _reset_events == ["reset"]
      and _valid_nodes["mail"].attributes.get("value", "") == ""
      and "checked" in _valid_nodes["keep"].attributes,
      repr((_reset, _valid_nodes["mail"].attributes)))
_check_events = []
_checked = _forms.activate_control(
    _valid_nodes["keep"], "https://forms.test/start", _defaults,
    dispatch_event=lambda ridx, event, bubbles, cancelable, submitter:
        (_check_events.append((event, cancelable)) or [], True, False))
check("checkbox default action toggles and emits input/change",
      _checked.changed
      and "checked" not in _valid_nodes["keep"].attributes
      and _check_events == [("input", False), ("change", False)],
      repr((_checked, _check_events)))

# Session-history snapshots restore live form state into a freshly parsed
# document without retaining the old DOM or file-input contents.
from browser import navigation as _navigation
_history_html = (
    '<form><input id="text" value="before">'
    '<input id="flag" type="checkbox" checked>'
    '<input id="file" type="file" value="secret.txt">'
    '<textarea id="memo">default</textarea>'
    '<select><option id="one" selected>one</option>'
    '<option id="two">two</option></select></form>')
_history_source = HTMLParser(_history_html).parse()
_history_nodes = {node.attributes.get("id"): node
                  for node in tree_to_list(_history_source, [])
                  if isinstance(node, Element) and node.attributes.get("id")}
_history_nodes["text"].attributes["value"] = "after"
_history_nodes["flag"].attributes.pop("checked", None)
_history_nodes["memo"].attributes["value"] = "draft"
_history_nodes["one"].attributes.pop("selected", None)
_history_nodes["two"].attributes["selected"] = ""
_form_state = _navigation.capture_form_state(_history_source)
_history_fresh = HTMLParser(_history_html).parse()
_fresh_controls = [node for node in tree_to_list(_history_fresh, [])
                   if isinstance(node, Element)
                   and node.tag in ("input", "textarea", "select", "option")]
for _ridx, _control in enumerate(_fresh_controls, 1):
    _control._ridx = _ridx
_state_sets, _state_removes = [], []
_navigation.restore_form_state(
    _history_fresh, _form_state,
    set_attr=lambda ridx, name, value:
        _state_sets.append((ridx, name, value)),
    remove_attr=lambda ridx, name: _state_removes.append((ridx, name)))
_restored_nodes = {node.attributes.get("id"): node
                   for node in tree_to_list(_history_fresh, [])
                   if isinstance(node, Element) and node.attributes.get("id")}
check("history restores form values, checks, and selection",
      _restored_nodes["text"].attributes.get("value") == "after"
      and "checked" not in _restored_nodes["flag"].attributes
      and "value" not in _restored_nodes["file"].attributes
      and _restored_nodes["memo"].attributes.get("value") == "draft"
      and "selected" not in _restored_nodes["one"].attributes
      and "selected" in _restored_nodes["two"].attributes
      and any(name == "checked" for _ridx, name in _state_removes),
      repr((_form_state, _state_sets, _state_removes)))

# Starting a newer navigation cancels the superseded worker and only the
# newest completion can be taken by the UI thread.
import threading as _threading
_nav_controller = _navigation.NavigationController()
_old_started = _threading.Event()

def _old_navigation(token):
    _old_started.set()
    while True:
        token.check()
        _threading.Event().wait(0.005)

_old_pending = _nav_controller.start(_old_navigation, "old")
_old_started.wait(1.0)
_new_pending = _nav_controller.start(lambda token: "new", "new")
_new_result = _new_pending.future.result(timeout=1.0)
_ready_navigation = _nav_controller.take_ready()
_old_cancelled = False
try:
    _old_pending.future.result(timeout=1.0)
except net.RequestCancelled:
    _old_cancelled = True
_nav_controller.shutdown()
_history_entry = _navigation.HistoryEntry(
    net.URL("https://history.test/a"), _history_html,
    scroll=321, hscroll=45, form_state=_form_state)
check("new navigation cancels stale work and history retains viewport",
      _old_cancelled and _new_result == "new"
      and _ready_navigation is _new_pending
      and _ready_navigation.context == "new"
      and _history_entry.scroll == 321 and _history_entry.hscroll == 45,
      repr((_old_cancelled, _new_result, _ready_navigation,
            _history_entry.scroll, _history_entry.hscroll)))

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
<div id=fxnav style="display: flex; width: 400px">
  <a id=lbl style="flex: 0 0 auto">Label</a>
  <div id=nav style="flex: 1 0 0">nav</div>
</div>
<div id=abs style="position: absolute; left: 50px; top: 300px;
     width: 80px">abs</div>
</body></html>"""
dom3 = HTMLParser(LAYOUT_PAGE).parse()
style(dom3, sorted(ua, key=cascade_priority))
doc3 = DocumentLayout(dom3)
doc3.layout(800)  # html fills the viewport: content width 800
boxes = {}
for b in layout_tree_to_list(doc3, []):
    if isinstance(b, BlockLayout) and isinstance(b.node, Element):
        node_id = b.node.attributes.get("id")
        if node_id:
            boxes[node_id] = b

center = boxes["center"]
# border-box 200 = content 176 + padding 20 + border 4, centered in 800:
# auto margin (800-200)/2 = 300, + border-left 2 + padding-left 10 = 12
check("margin auto centers", abs(center.x - (HSTEP + 300 + 12)) < 1,
      f"x={center.x}")
check("width is border-box", abs(center.width - 176) < 1,
      f"w={center.width}")
fa, fb, fc = boxes["fa"], boxes["fb"], boxes["fc"]
check("flex row placement", fa.y == fb.y == fc.y and fa.x < fb.x < fc.x,
      f"x: {fa.x:.0f},{fb.x:.0f},{fc.x:.0f}")
# the two auto items grow from their content basis and together consume
# all free space; each gets ~half, up to the b/c glyph-width difference
check("flex grow shares space",
      abs((fb.width + fc.width) - (800 - 100)) < 1
      and abs(fb.width - fc.width) < 4,
      f"fb.width={fb.width:.0f} fc.width={fc.width:.0f}")
# a `flex:1 0 0` pane fills the row while a `flex:0 0 auto` label keeps
# its content width (naver's nav tabs: the grow pane must not collapse)
lbl, nav = boxes["lbl"], boxes["nav"]
check("flex:1 0 0 grows past a content-sized sibling",
      nav.width > 300 and lbl.width < 100 and lbl.x < nav.x,
      f"lbl.width={lbl.width:.0f} nav.width={nav.width:.0f}")

# --- grid named lines, row spans, and collision-free auto placement ---
GRID_PAGE = """<html><body style="margin:0">
<div id=ng style="display:grid; width:400px; gap:10px;
     grid-template-columns:[side-start] 100px
                           [side-end content-start] 1fr [content-end];
     grid-template-rows:[top] 40px [middle] 30px [bottom]">
  <div id=side style="grid-column:side-start / side-end;
       grid-row:top / middle">side</div>
  <div id=main style="grid-column:content-start / content-end;
       grid-row:top / bottom">main</div>
  <div id=foot style="grid-column:side-start / content-end;
       grid-row:middle / bottom">foot</div>
</div>
<div id=cg style="display:grid; width:300px;
     grid-template-columns:repeat(3, [slot] 100px);
     grid-template-rows:20px 20px">
  <div id=auto-first>auto</div>
  <div id=reserved style="grid-column:1 / span 2;
       grid-row:1 / span 2">reserved</div>
  <div id=auto-second>auto2</div>
</div>
</body></html>"""
grid_dom = HTMLParser(GRID_PAGE).parse()
style(grid_dom, sorted(ua, key=cascade_priority))
grid_doc = DocumentLayout(grid_dom)
grid_doc.layout(800)
grid_boxes = {
    b.node.attributes.get("id"): b
    for b in layout_tree_to_list(grid_doc, [])
    if isinstance(b, BlockLayout) and isinstance(b.node, Element)
    and b.node.attributes.get("id")
}
ng = grid_boxes["ng"]
check("grid named column lines place sidebar and content",
      abs(grid_boxes["side"].x - ng.x) < 1
      and abs(grid_boxes["main"].x - (ng.x + 110)) < 1
      and abs(grid_boxes["main"].width - 290) < 1,
      f"side.x={grid_boxes['side'].x - ng.x:.0f} "
      f"main=({grid_boxes['main'].x - ng.x:.0f},"
      f"{grid_boxes['main'].width:.0f})")
check("grid named row lines and spans resolve fixed tracks",
      abs(grid_boxes["foot"].y - (ng.y + 50)) < 1
      and abs(grid_boxes["foot"].width - 400) < 1,
      f"foot=({grid_boxes['foot'].x - ng.x:.0f},"
      f"{grid_boxes['foot'].y - ng.y:.0f},"
      f"{grid_boxes['foot'].width:.0f})")
cg = grid_boxes["cg"]
check("grid auto placement reserves later explicit spans",
      abs(grid_boxes["reserved"].x - cg.x) < 1
      and abs(grid_boxes["auto-first"].x - (cg.x + 200)) < 1
      and abs(grid_boxes["auto-second"].x - (cg.x + 200)) < 1
      and grid_boxes["auto-second"].y > grid_boxes["auto-first"].y,
      f"auto1=({grid_boxes['auto-first'].x - cg.x:.0f},"
      f"{grid_boxes['auto-first'].y - cg.y:.0f}) "
      f"auto2=({grid_boxes['auto-second'].x - cg.x:.0f},"
      f"{grid_boxes['auto-second'].y - cg.y:.0f})")

STICKY_PAGE = """<html><body style="margin:0">
<div id=stick-wrap style="height:300px">
  <div id=stick style="position:sticky; top:10px; height:30px">
    <span id=stick-child>sticky</span>
  </div>
  <div style="height:500px">long content</div>
</div>
</body></html>"""
sticky_dom = HTMLParser(STICKY_PAGE).parse()
style(sticky_dom, sorted(ua, key=cascade_priority))
sticky_doc = DocumentLayout(sticky_dom)
sticky_doc.layout(800, 120)
sticky_boxes = {
    b.node.attributes.get("id"): b
    for b in layout_tree_to_list(sticky_doc, [])
    if isinstance(b, BlockLayout) and isinstance(b.node, Element)
    and b.node.attributes.get("id")
}
stick = sticky_boxes["stick"]
normal_top = stick.y - stick.pt - stick.bw - stick.margin_top
check("sticky top follows scroll and keeps its inset",
      abs(sticky_offset(stick, normal_top + 50) - 60) < 1,
      f"offset={sticky_offset(stick, normal_top + 50):.0f}")
check("sticky movement stops at the containing-block boundary",
      abs(sticky_offset(stick, normal_top + 500) - 270) < 1,
      f"offset={sticky_offset(stick, normal_top + 500):.0f}")
sticky_cmds = paint_tree(sticky_doc, [])
check("sticky paint emits native push/pop markers",
      sum(type(c).__name__ == "DrawStickyPush" for c in sticky_cmds) == 1
      and sum(type(c).__name__ == "DrawStickyPop" for c in sticky_cmds) == 1)

# --- vertical margin collapsing: parent edges, nesting, empty blocks ---
MARGIN_PAGE = """<html><body style="margin:0">
<div id=top-parent style="margin-top:10px">
  <div id=top-child style="margin-top:30px;height:20px"></div>
</div>
<div id=border-parent style="border-width:1px;margin-top:5px">
  <div id=border-child style="margin-top:25px;height:10px"></div>
</div>
<div id=bottom-parent style="margin-bottom:10px">
  <div id=bottom-child style="height:20px;margin-bottom:40px"></div>
</div>
<div id=bottom-next style="height:10px;margin-top:5px"></div>
<div id=before-empty style="height:10px;margin-bottom:10px"></div>
<div id=empty style="margin-top:30px;margin-bottom:20px"></div>
<div id=after-empty style="height:10px;margin-top:15px"></div>
<div id=neg-before style="height:10px;margin-bottom:20px"></div>
<div id=neg-empty style="margin-top:-10px;margin-bottom:30px"></div>
<div id=neg-after style="height:10px;margin-top:-5px"></div>
<div id=nested style="margin-top:5px">
  <div id=nested-mid style="margin-top:20px">
    <div id=nested-leaf style="margin-top:30px;height:10px"></div>
  </div>
</div>
</body></html>"""
margin_dom = HTMLParser(MARGIN_PAGE).parse()
style(margin_dom, sorted(ua, key=cascade_priority))
margin_doc = DocumentLayout(margin_dom)
margin_doc.layout(800)
margin_boxes = {
    b.node.attributes.get("id"): b
    for b in layout_tree_to_list(margin_doc, [])
    if isinstance(b, BlockLayout) and isinstance(b.node, Element)
    and b.node.attributes.get("id")
}
check("parent and first-child top margins collapse",
      abs(margin_boxes["top-parent"].margin_top - 30) < 1
      and abs(margin_boxes["top-child"].y
              - margin_boxes["top-parent"].y) < 1,
      f"parent.mt={margin_boxes['top-parent'].margin_top:.0f} "
      f"delta={margin_boxes['top-child'].y - margin_boxes['top-parent'].y:.0f}")
check("parent border prevents top margin collapse",
      abs(margin_boxes["border-child"].y
              - margin_boxes["border-parent"].y - 25) < 1,
      f"delta={margin_boxes['border-child'].y - margin_boxes['border-parent'].y:.0f}")
bp, bc, bn = (margin_boxes["bottom-parent"],
              margin_boxes["bottom-child"], margin_boxes["bottom-next"])
check("last-child bottom margin collapses through an auto-height parent",
      abs(bp.height - bc.height) < 1 and abs(bp.margin_bottom - 40) < 1
      and abs(bn.y - (bp.y + bp.height + 40)) < 1,
      f"parent.h={bp.height:.0f} mb={bp.margin_bottom:.0f} "
      f"next-gap={bn.y - bp.y - bp.height:.0f}")
be, ae = margin_boxes["before-empty"], margin_boxes["after-empty"]
check("empty block margins collapse across the zero-height box",
      abs(ae.y - (be.y + be.height) - 30) < 1,
      f"gap={ae.y - be.y - be.height:.0f}")
nb, na = margin_boxes["neg-before"], margin_boxes["neg-after"]
check("mixed positive/negative adjoining margins collapse as one set",
      abs(na.y - (nb.y + nb.height) - 20) < 1,
      f"gap={na.y - nb.y - nb.height:.0f}")
check("nested parent/first-child margins propagate as one set",
      abs(margin_boxes["nested"].margin_top - 30) < 1
      and abs(margin_boxes["nested"].y - margin_boxes["nested-mid"].y) < 1
      and abs(margin_boxes["nested-mid"].y
              - margin_boxes["nested-leaf"].y) < 1,
      f"ys={margin_boxes['nested'].y:.0f},"
      f"{margin_boxes['nested-mid'].y:.0f},"
      f"{margin_boxes['nested-leaf'].y:.0f}")

# --- float + clear (v1: width-bearing floats, block sidestep) ---
FLOAT_PAGE = """<html><body style="margin: 0">
<div id=wrap style="width:400px">
  <div id=fl style="float:left; width:100px; height:80px">img</div>
  <p id=txt style="margin:0">text beside the thumbnail</p>
  <div id=fr style="float:right; width:50px; height:30px">r</div>
  <p id=txt2 style="margin:0">second paragraph</p>
  <div id=cl style="clear:both; height:10px">below</div>
</div>
</body></html>"""
fl_dom = HTMLParser(FLOAT_PAGE).parse()
style(fl_dom, sorted(ua, key=cascade_priority))
fl_doc = DocumentLayout(fl_dom)
fl_doc.layout(800)
fb3 = {}
for b in layout_tree_to_list(fl_doc, []):
    if isinstance(b, BlockLayout) and isinstance(b.node, Element):
        node_id = b.node.attributes.get("id")
        if node_id:
            fb3[node_id] = b
_w = fb3["wrap"]
check("float:left sits at the container edge, top of flow",
      abs(fb3["fl"].x - _w.x) < 1 and abs(fb3["fl"].y - _w.y) < 1,
      f"({fb3['fl'].x:.0f},{fb3['fl'].y:.0f}) vs ({_w.x:.0f},{_w.y:.0f})")
check("in-flow text shifts right of the float and narrows",
      abs(fb3["txt"].x - (_w.x + 100)) < 1
      and abs(fb3["txt"].width - 300) < 1,
      f"x={fb3['txt'].x:.0f} w={fb3['txt'].width:.0f}")
check("in-flow text keeps the float's y (no push-down)",
      abs(fb3["txt"].y - _w.y) < 1, f"y={fb3['txt'].y:.0f}")
check("float:right hugs the right edge",
      abs((fb3["fr"].x + fb3["fr"].width) - (_w.x + 400)) < 1,
      f"right={fb3['fr'].x + fb3['fr'].width:.0f}")
check("clear:both drops below the tallest float",
      fb3["cl"].y >= _w.y + 80 - 1,
      f"cl.y={fb3['cl'].y:.0f} float bottom={_w.y + 80:.0f}")
check("container height contains its floats",
      _w.height >= 80 + 10 - 1, f"h={_w.height:.0f}")

# --- flex deep-dive: justify/align/shrink/basis/flex shorthand ---
FLEX2_PAGE = """<html><body style="margin: 0">
<div id=jc style="display:flex; justify-content:center; width:300px">
  <div id=jca style="width:50px; height:10px">a</div>
  <div id=jcb style="width:50px; height:10px">b</div>
</div>
<div id=sb style="display:flex; justify-content:space-between;
     width:300px">
  <div id=sba style="width:50px; height:10px">a</div>
  <div id=sbb style="width:50px; height:10px">b</div>
  <div id=sbc style="width:50px; height:10px">c</div>
</div>
<div id=ai style="display:flex; align-items:center; width:300px">
  <div id=aia style="width:50px; height:40px">tall</div>
  <div id=aib style="width:50px; height:10px">short</div>
</div>
<div id=sh style="display:flex; width:300px">
  <div id=sha style="width:400px; height:10px">a</div>
  <div id=shb style="width:200px; height:10px">b</div>
</div>
<div id=gr style="display:flex; width:300px">
  <div id=gra style="flex: 1; height:10px">a</div>
  <div id=grb style="flex: 2; height:10px">b</div>
  <div id=grc style="width:60px; height:10px">c</div>
</div>
</body></html>"""
fdom = HTMLParser(FLEX2_PAGE).parse()
style(fdom, sorted(ua, key=cascade_priority))
fdoc = DocumentLayout(fdom)
fdoc.layout(800)
fb2 = {}
for b in layout_tree_to_list(fdoc, []):
    if isinstance(b, BlockLayout) and isinstance(b.node, Element):
        node_id = b.node.attributes.get("id")
        if node_id:
            fb2[node_id] = b
_j = fb2["jc"]
check("justify-content:center leads with half the free space",
      abs(fb2["jca"].x - (_j.x + 100)) < 1
      and abs(fb2["jcb"].x - (_j.x + 150)) < 1,
      f"a={fb2['jca'].x - _j.x:.0f} b={fb2['jcb'].x - _j.x:.0f}")
_s = fb2["sb"]
check("justify-content:space-between pins ends, splits middle",
      abs(fb2["sba"].x - _s.x) < 1
      and abs(fb2["sbb"].x - (_s.x + 125)) < 1
      and abs(fb2["sbc"].x - (_s.x + 250)) < 1,
      f"{fb2['sba'].x - _s.x:.0f},{fb2['sbb'].x - _s.x:.0f},"
      f"{fb2['sbc'].x - _s.x:.0f}")
check("align-items:center centers the short item",
      abs(fb2["aib"].y - (fb2["aia"].y + 15)) < 1,
      f"tall.y={fb2['aia'].y:.0f} short.y={fb2['aib'].y:.0f}")
check("flex-shrink returns overflow proportionally",
      abs(fb2["sha"].width - 200) < 1
      and abs(fb2["shb"].width - 100) < 1,
      f"a={fb2['sha'].width:.0f} b={fb2['shb'].width:.0f}")
check("flex shorthand: grow factors split the remainder 1:2",
      abs(fb2["gra"].width - 80) < 1
      and abs(fb2["grb"].width - 160) < 1
      and abs(fb2["grc"].width - 60) < 1,
      f"a={fb2['gra'].width:.0f} b={fb2['grb'].width:.0f} "
      f"c={fb2['grc'].width:.0f}")
absbox = boxes["abs"]
check("absolute positioning", abs(absbox.x - 50) < 1
      and abs(absbox.y - 300) < 1,
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
doc4.layout(800)  # content width 800 (body{margin:0}), no viewport height
boxes4 = _boxes(doc4)

rel, after = boxes4["rel"], boxes4["after"]
check("relative offset shifts the box",
      abs(rel.y - 30) < 1 and abs(rel.x - 10) < 1,
      f"({rel.x:.0f}, {rel.y:.0f})")
check("relative offset is visual-only (siblings keep flow position)",
      abs(after.y - 40) < 1, f"after.y={after.y:.0f}")

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
    # ::before/::after synthesis (native engine) + icon paint path
    PSEUDO_PAGE = ('<html><head><style>'
                   '.ico::before{content:""; width:20px; height:20px; '
                   'background-image:url(i.png)} '
                   'p::after{content:"!"}</style></head>'
                   '<body><p class=ico>hi</p></body></html>')
    ps_dom = native.parse_and_style(PSEUDO_PAGE, lambda hrefs: {})
    ps_p = next(n for n in tree_to_list(ps_dom, [])
                if isinstance(n, Element) and n.tag == "p")
    kid_tags = [getattr(c, "tag", "#text") for c in ps_p.children]
    check("native synthesizes ::before/::after",
          kid_tags[0] == "::before" and kid_tags[-1] == "::after",
          str(kid_tags))
    ps_p.children[0]._bg = (9, 40, 40, {"url": "i.png", "position": [],
                                        "size": [], "repeat": "no-repeat"})
    import tkinter as _tk3
    _r3 = _tk3.Tk(); _r3.withdraw()
    # tk-fallback fonts died with the old root; force re-creation
    from browser import layout as _layout
    _layout._FONT_CACHE.clear()
    from browser.draw import DrawBgImage as _DrawBg2
    ps_doc = DocumentLayout(ps_dom)
    ps_doc.layout(400)
    ps_cmds = paint_tree(ps_doc, [])
    _r3.destroy()
    check("pseudo icon paints its background layer",
          any(isinstance(c, _DrawBg2) and c.image_id == 9
              for c in ps_cmds), str([type(c).__name__ for c in ps_cmds]))
    check("pseudo text content paints",
          any(getattr(c, "text", "") == "!" for c in ps_cmds))

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
        from browser import layout as _layout2
        _layout2._FONT_CACHE.clear()
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

# --- Cookie security/scoping regressions ---
from browser import psl as _psl
check("PSL exact rules produce the registrable domain",
      _psl.public_suffix("a.shop.example.co.uk") == "co.uk"
      and _psl.registrable_domain("a.shop.example.co.uk")
      == "example.co.uk", _psl.version())
check("PSL wildcard and exception rules are applied",
      _psl.public_suffix("a.b.ck") == "b.ck"
      and _psl.public_suffix("www.ck") == "ck"
      and _psl.registrable_domain("a.www.ck") == "www.ck")
_idn_host = _psl.canonical_host("食狮.公司.cn")
check("PSL canonicalizes Unicode hosts to IDNA",
      _idn_host == "xn--85x722f.xn--55qx5d.cn"
      and _psl.registrable_domain(_idn_host) == _idn_host,
      _idn_host)
check("schemeful site uses scheme plus eTLD+1",
      net._site_key(net.URL("https://a.example.co.uk/"))
      == ("https", "example.co.uk")
      and net._site_key(net.URL("https://b.example.co.uk/"))
      == ("https", "example.co.uk")
      and net._site_key(net.URL("http://b.example.co.uk/"))
      != ("https", "example.co.uk"))
check("PSL site domains preserve localhost and IP literals",
      _psl.site_domain("localhost") == "localhost"
      and _psl.site_domain("127.0.0.1") == "127.0.0.1"
      and _psl.site_domain("::1") == "::1")

net._COOKIE_JAR.clear()
_cu = net.URL("https://www.example.com/app/page")
_cu_http = net.URL("http://www.example.com/app/page")
check("document.cookie rejects header controls",
      not net.set_cookie_from_js(_cu, "sid=ok\r\nX-Injected: yes")
      and "X-Injected" not in net._cookie_header(_cu))
check("cookie names reject HTTP separators",
      not net.set_cookie_from_js(_cu, "bad:name=value"))
check("Domain cookies reject ICANN and private public suffixes",
      net._store_set_cookie(
          net.URL("https://shop.example.co.uk/"),
          ["bad=1; Domain=co.uk; Secure"]) == 0
      and net._store_set_cookie(
          net.URL("https://tenant.github.io/"),
          ["bad=1; Domain=github.io; Secure"]) == 0)

_stored = net._store_set_cookie(_cu, [
    "sid=secret; Path=/app; Secure; HttpOnly; SameSite=Strict",
    "domainwide=1; Domain=example.com; Path=/",
    "third=ok; Path=/; Secure; SameSite=None",
])
check("cookie attributes accepted", _stored == 3, str(net._COOKIE_JAR))
_doc_cookie = net.cookies_for(_cu)
check("HttpOnly hidden from document.cookie",
      "sid=" not in _doc_cookie and "domainwide=1" in _doc_cookie,
      _doc_cookie)
check("HttpOnly cookie still sent on matching HTTPS path",
      "sid=secret" in net._cookie_header(_cu))
check("Secure cookie withheld from HTTP",
      "sid=secret" not in net._cookie_header(_cu_http)
      and "third=ok" not in net._cookie_header(_cu_http))
check("cookie Path boundary enforced",
      "sid=secret" not in net._cookie_header(
          net.URL("https://www.example.com/application")))
check("Domain cookie reaches a subdomain",
      "domainwide=1" in net._cookie_header(
          net.URL("https://cdn.example.com/asset")))
_cross = net._cookie_header(
    _cu, site_for_cookies=net.URL("https://cross-site.test/"),
    top_level_navigation=False)
check("SameSite blocks cross-site subresource cookies",
      "sid=secret" not in _cross and "domainwide=1" not in _cross
      and "third=ok" in _cross, _cross)
_cross_navigation = net._cookie_header(
    _cu, site_for_cookies=net.URL("https://cross-site.test/"),
    top_level_navigation=True)
check("SameSite top-level GET allows Lax but withholds Strict",
      "sid=secret" not in _cross_navigation
      and "domainwide=1" in _cross_navigation
      and "third=ok" in _cross_navigation, _cross_navigation)
check("insecure origins cannot set Secure cookies",
      net._store_set_cookie(
          _cu_http, ["bad=1; Secure; Path=/"]) == 0)
check("document.cookie cannot overwrite HttpOnly",
      not net.set_cookie_from_js(_cu, "sid=evil; Path=/app")
      and "sid=secret" in net._cookie_header(_cu))
net._store_set_cookie(_cu, ["gone=1; Path=/; Max-Age=10"])
net._store_set_cookie(_cu, ["gone=; Path=/; Max-Age=0"])
check("Max-Age deletes a cookie", "gone=" not in net._cookie_header(_cu))

from concurrent.futures import ThreadPoolExecutor as _CookiePool
_cookie_errors = []
def _cookie_worker(i):
    try:
        for n in range(25):
            net.set_cookie_from_js(_cu, f"t{i}_{n}={n}; Path=/")
            net.cookies_for(_cu)
            net._cookie_header(_cu)
    except Exception as e:
        _cookie_errors.append(e)
with _CookiePool(max_workers=8) as _pool:
    list(_pool.map(_cookie_worker, range(8)))
check("cookie jar is safe under parallel reads/writes",
      not _cookie_errors, repr(_cookie_errors))
net._COOKIE_JAR.clear()

# --- Real network fetch (kept out of deterministic PR CI) ---
if os.environ.get("GG_SKIP_LIVE_NETWORK") == "1":
    print("[SKIP] live example.com/google.com checks")
else:
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
    # Both GUI shells commit completed navigation on their UI loop and keep
    # page body + viewport + form state in each history entry.
    from browser.browser import Browser as _TkBrowser
    _ui_page1 = net.URL(
        "data:text/html,<html><body><input id=remember value=before>"
        "<div style='width:2200px;height:2200px'>long</div></body></html>")
    _ui_page2 = net.URL(
        "data:text/html,<html><body><p>second</p></body></html>")
    _keyboard_url = net.URL("https://keyboard.test/start")
    _keyboard_html = (
        '<html><body><a id="k-link" tabindex="1" href="/target">link</a>'
        '<button id="k-button" tabindex="2" onclick="document.'
        "getElementById('k-out').setAttribute('data-hit','yes')\">"
        'button</button><form action="/submitted" method="post">'
        '<input id="k-text" name="q" value="ok"><button id="k-submit" '
        'name="go" value="yes">submit</button></form><input id="k-check" '
        'type="checkbox"><div id="k-out"></div></body></html>')

    def _finish_tk_navigation(browser, future):
        future.result(timeout=2.0)
        if browser._navigation_poll is not None:
            browser.window.after_cancel(browser._navigation_poll)
            browser._navigation_poll = None
        browser._poll_navigation()

    _tk_browser = _TkBrowser()
    _tk_browser.window.withdraw()
    _finish_tk_navigation(_tk_browser, _tk_browser.load(_ui_page1))
    _remember = next(node for node in tree_to_list(_tk_browser.nodes, [])
                     if isinstance(node, Element)
                     and node.attributes.get("id") == "remember")
    _remember.attributes["value"] = "restored"
    _tk_browser.sync_attr(_remember, "value", "restored")
    _tk_browser.scroll = 321
    _finish_tk_navigation(_tk_browser, _tk_browser.load(_ui_page2))
    _tk_browser.go_back()
    _remember_back = next(
        node for node in tree_to_list(_tk_browser.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "remember")
    check("tk shell back restores body, form value, and scroll",
          len(_tk_browser.history) == 2
          and _remember_back.attributes.get("value") == "restored"
          and _tk_browser.scroll == 321,
          repr((len(_tk_browser.history), _remember_back.attributes,
                _tk_browser.scroll)))
    _tk_browser.go_forward()
    check("tk shell forward restores the saved document without fetch",
          _tk_browser.history_index == 1
          and str(_tk_browser.url) == str(_ui_page2)
          and any(isinstance(node, Text) and node.text == "second"
                  for node in tree_to_list(_tk_browser.nodes, [])),
          repr((_tk_browser.history_index, _tk_browser.url)))

    class _TkKeyEvent:
        def __init__(self, keysym, char="", state=0):
            self.keysym = keysym
            self.char = char
            self.state = state

    _tk_browser.render_page(_keyboard_url, _keyboard_html)
    _tk_browser.on_key(_TkKeyEvent("Tab"))
    _tk_first = _tk_browser.focus_node.attributes.get("id")
    _tk_browser.on_key(_TkKeyEvent("Tab"))
    _tk_second = _tk_browser.focus_node.attributes.get("id")
    _tk_browser.on_key(_TkKeyEvent("ISO_Left_Tab", state=1))
    _tk_reverse = _tk_browser.focus_node.attributes.get("id")
    check("tk shell Tab and Shift+Tab follow sequential focus order",
          (_tk_first, _tk_second, _tk_reverse)
          == ("k-link", "k-button", "k-link"),
          repr((_tk_first, _tk_second, _tk_reverse)))
    _tk_button = next(
        node for node in tree_to_list(_tk_browser.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-button")
    _tk_browser.set_focus(_tk_button)
    _tk_browser.on_key(_TkKeyEvent("Return"))
    _tk_out = next(
        node for node in tree_to_list(_tk_browser.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-out")
    check("tk shell Enter activates the focused button",
          _tk_out.attributes.get("data-hit") == "yes",
          repr(_tk_out.attributes))
    _tk_check = next(
        node for node in tree_to_list(_tk_browser.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-check")
    _tk_browser.set_focus(_tk_check)
    _tk_browser.on_key(_TkKeyEvent("space", char=" "))
    _tk_check = next(
        node for node in tree_to_list(_tk_browser.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-check")
    check("tk shell Space toggles the focused checkbox",
          "checked" in _tk_check.attributes, repr(_tk_check.attributes))
    _tk_link = next(
        node for node in tree_to_list(_tk_browser.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-link")
    _tk_link_targets = []
    _tk_real_load = _tk_browser.load
    _tk_browser.load = lambda url, *args, **kwargs: \
        _tk_link_targets.append(str(url))
    try:
        _tk_browser.set_focus(_tk_link)
        _tk_browser.on_key(_TkKeyEvent("Return"))
    finally:
        _tk_browser.load = _tk_real_load
    check("tk shell Enter follows the focused link",
          _tk_link_targets == ["https://keyboard.test/target"],
          repr(_tk_link_targets))
    _tk_text = next(
        node for node in tree_to_list(_tk_browser.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-text")
    _tk_submit_calls = []
    _tk_browser.load = lambda url, *args, **kwargs: \
        _tk_submit_calls.append((str(url), kwargs))
    try:
        _tk_browser.set_focus(_tk_text)
        _tk_browser.on_key(_TkKeyEvent("Return"))
    finally:
        _tk_browser.load = _tk_real_load
    check("tk shell Enter uses the form's default submit button",
          _tk_submit_calls
          and _tk_submit_calls[0][0]
          == "https://keyboard.test/submitted"
          and _tk_submit_calls[0][1].get("method") == "POST"
          and _tk_submit_calls[0][1].get("body") == b"q=ok&go=yes",
          repr(_tk_submit_calls))
    _tk_browser._navigation.shutdown()
    _tk_browser.window.destroy()

    from browser.shell import Shell as _NativeShell

    class _FakeNativeWindow:
        def size(self):
            return 1100, 780

        def scale_factor(self):
            return 1.0

        def set_title(self, title):
            self.title = title

    # local renderer on purpose: this section tests shell UI logic, and
    # a spawn-based renderer child would re-import this top-level script
    # as __mp_main__ (process isolation has its own unittest suite)
    _native_shell = _NativeShell(_FakeNativeWindow(), process_model="local")
    _native_shell.load(_ui_page1).result(timeout=2.0)
    _native_shell.poll_navigation()
    _shell_remember = next(
        node for node in tree_to_list(_native_shell.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "remember")
    _shell_remember.attributes["value"] = "native-restored"
    _native_shell._doc.set_attr(
        _shell_remember._ridx, "value", "native-restored")
    _native_shell.scroll = 222
    _native_shell.hscroll = 111
    _native_shell.load(_ui_page2).result(timeout=2.0)
    _native_shell.poll_navigation()
    _native_shell.go_back()
    _shell_back = next(
        node for node in tree_to_list(_native_shell.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "remember")
    check("native shell history restores both viewport axes and forms",
          _shell_back.attributes.get("value") == "native-restored"
          and _native_shell.scroll == 222
          and _native_shell.hscroll == 111,
          repr((_shell_back.attributes, _native_shell.scroll,
                _native_shell.hscroll)))
    _native_shell.render_page(_keyboard_url, _keyboard_html)
    _native_shell.on_key("Tab")
    _native_first = _native_shell.focus_node.attributes.get("id")
    _native_shell.on_key("Tab")
    _native_second = _native_shell.focus_node.attributes.get("id")
    _native_shell.on_key("ShiftTab")
    _native_reverse = _native_shell.focus_node.attributes.get("id")
    check("native shell Tab and Shift+Tab follow sequential focus order",
          (_native_first, _native_second, _native_reverse)
          == ("k-link", "k-button", "k-link"),
          repr((_native_first, _native_second, _native_reverse)))
    _native_button = next(
        node for node in tree_to_list(_native_shell.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-button")
    _native_shell.set_focus(_native_button)
    _native_shell.on_key("Enter")
    _native_out = next(
        node for node in tree_to_list(_native_shell.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-out")
    check("native shell Enter activates the focused button",
          _native_out.attributes.get("data-hit") == "yes",
          repr(_native_out.attributes))
    _native_check = next(
        node for node in tree_to_list(_native_shell.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-check")
    _native_shell.set_focus(_native_check)
    _native_shell.on_text(" ")
    _native_check = next(
        node for node in tree_to_list(_native_shell.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-check")
    check("native shell Space toggles the focused checkbox",
          "checked" in _native_check.attributes,
          repr(_native_check.attributes))
    _native_link = next(
        node for node in tree_to_list(_native_shell.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-link")
    _native_link_targets = []
    _native_real_load = _native_shell.load
    _native_shell.load = lambda url, *args, **kwargs: \
        _native_link_targets.append(str(url))
    try:
        _native_shell.set_focus(_native_link)
        _native_shell.on_key("Enter")
    finally:
        _native_shell.load = _native_real_load
    check("native shell Enter follows the focused link",
          _native_link_targets == ["https://keyboard.test/target"],
          repr(_native_link_targets))
    _native_text = next(
        node for node in tree_to_list(_native_shell.nodes, [])
        if isinstance(node, Element)
        and node.attributes.get("id") == "k-text")
    _native_submit_calls = []
    _native_shell.load = lambda url, *args, **kwargs: \
        _native_submit_calls.append((str(url), kwargs))
    try:
        _native_shell.set_focus(_native_text)
        _native_shell.on_key("Enter")
    finally:
        _native_shell.load = _native_real_load
    check("native shell Enter uses the form's default submit button",
          _native_submit_calls
          and _native_submit_calls[0][0]
          == "https://keyboard.test/submitted"
          and _native_submit_calls[0][1].get("method") == "POST"
          and _native_submit_calls[0][1].get("body") == b"q=ok&go=yes",
          repr(_native_submit_calls))
    _native_shell._navigation.shutdown()

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

    _real_request_text = net.request_text
    _form_requests = []

    def _fake_form_request(url, *args, **kwargs):
        _form_requests.append((str(url), kwargs))
        if url.host == "form.test" and url.path == "/start":
            html = (
                '<html><body><form action="/result" method="post" '
                'onsubmit="document.getElementById(\'event-value\')'
                '.setAttribute(\'value\',\'after\')">'
                '<input name="q" value="hello world">'
                '<input id="event-value" name="event" value="before">'
                '<button id="send" name="go" value="yes">send</button>'
                '</form></body></html>')
        else:
            html = '<html><body><div id="result">posted</div></body></html>'
        return {}, html, url

    try:
        net.request_text = _fake_form_request
        dp3 = Page()
        dp3.goto("https://form.test/start")
        _submitted = dp3.click("#send")
    finally:
        net.request_text = _real_request_text
    _post_calls = [kwargs for url, kwargs in _form_requests
                   if url == "https://form.test/result"]
    check("driver: submit button performs POST navigation",
          _submitted and dp3.text("#result") == "posted"
          and len(_post_calls) == 1,
          repr(_form_requests))
    check("driver: POST form forwards body and Content-Type",
          _post_calls
          and _post_calls[0].get("method") == "POST"
          and _post_calls[0].get("body") ==
          b"q=hello+world&event=after&go=yes"
          and _post_calls[0].get("headers") == {
              "Content-Type": "application/x-www-form-urlencoded"},
          repr(_post_calls))

    _form_event_requests = []

    def _fake_form_event_page(url, *args, **kwargs):
        _form_event_requests.append(str(url))
        html = (
            '<html><body><form id="f" action="/sent" method="post">'
            '<input id="required" name="q" required></form>'
            '<button id="event-send" form="f">send</button>'
            '<button id="event-reset" type="reset" form="f">reset</button>'
            '<div id="event-out">idle</div><script>'
            "var q=document.getElementById('required');"
            "var f=document.getElementById('f');"
            "q.addEventListener('invalid',function(){"
            "document.getElementById('event-out').textContent='invalid';});"
            "f.addEventListener('submit',function(e){"
            "document.getElementById('event-out').textContent="
            "'submit:'+e.submitter.id;e.preventDefault();});"
            "f.addEventListener('reset',function(){"
            "document.getElementById('event-out').textContent='reset';});"
            '</script></body></html>')
        return {}, html, url

    try:
        net.request_text = _fake_form_event_page
        dp_events = Page()
        dp_events.goto("https://form-events.test/start")
        dp_events.click("#event-send")
        _invalid_text = dp_events.text("#event-out")
        dp_events.evaluate(
            "(document.getElementById('required').value = 'ok')")
        dp_events.click("#event-send")
        _submit_text = dp_events.text("#event-out")
        dp_events.click("#event-reset")
        _reset_text = dp_events.text("#event-out")
        _reset_value = dp_events.query("#required").attr("value")
    finally:
        net.request_text = _real_request_text
    check("driver: invalid event blocks form navigation",
          _invalid_text == "invalid" and len(_form_event_requests) == 1,
          repr((_invalid_text, _form_event_requests)))
    check("driver: submit event exposes submitter and can cancel",
          _submit_text == "submit:event-send"
          and len(_form_event_requests) == 1,
          repr((_submit_text, _form_event_requests)))
    check("driver: reset event restores initial control value",
          _reset_text == "reset" and _reset_value in (None, ""),
          repr((_reset_text, _reset_value)))

    # The gg-js host bridge must preserve fetch options all the way to the
    # Python security policy, then expose the real response status and URL.
    from browser import security as _security
    _real_policy_fetch = _security.perform_script_fetch
    _script_requests = []

    def _fake_script_policy(base_url, request, **_network_options):
        _script_requests.append((str(base_url), dict(request)))
        return _security.ScriptFetchResponse(
            201, {}, "cors-ok", net.URL("https://api.fetch.test/final"))

    def _fake_script_page(url, *args, **kwargs):
        html = (
            '<html><body><div id="out">pending</div><script>'
            "fetch('/api', {method:'POST', body:'x=1', "
            "credentials:'include', headers:{'X-Token':'yes'}})"
            ".then(function(r) { return r.text().then(function(t) {"
            "document.getElementById('out').textContent = "
            "r.status + '|' + r.url + '|' + t; }); });"
            '</script></body></html>')
        return {}, html, url

    try:
        net.request_text = _fake_script_page
        _security.perform_script_fetch = _fake_script_policy
        dp4 = Page()
        dp4.goto("https://app.fetch.test/start")
    finally:
        net.request_text = _real_request_text
        _security.perform_script_fetch = _real_policy_fetch
    _bridged = _script_requests[0][1] if _script_requests else {}
    check("driver: gg-js fetch bridge preserves request options",
          len(_script_requests) == 1
          and _bridged.get("method") == "POST"
          and _bridged.get("body") == "x=1"
          and _bridged.get("credentials") == "include"
          and ("X-Token", "yes") in _bridged.get("headers", []),
          repr(_script_requests))
    check("driver: fetch exposes response status, URL, and body",
          dp4.text("#out") ==
          "201|https://api.fetch.test/final|cors-ok",
          repr(dp4.text("#out")))
else:
    print("[SKIP] driver checks - native ggcore not built")

# --- CSS transitions & @keyframes animations (browser/animation.py) ---
from browser import animation as anim  # noqa: E402

check("anim: parse_time units",
      anim.parse_time("0.3s") == 0.3 and anim.parse_time("250ms") == 0.25
      and anim.parse_time("0s") == 0.0 and anim.parse_time("abc") is None)

_ts = anim.parse_transitions(
    {"transition": "opacity 0.2s ease-in 0.1s, width 1s"})
check("anim: transition shorthand normalizes",
      len(_ts) == 2 and _ts[0].prop == "opacity"
      and abs(_ts[0].duration - 0.2) < 1e-9
      and abs(_ts[0].delay - 0.1) < 1e-9
      and _ts[1].prop == "width" and _ts[1].duration == 1.0
      and _ts[1].delay == 0.0, repr(_ts))
_tl = anim.parse_transitions(
    {"transition-property": "color, width", "transition-duration": "1s",
     "transition-delay": "0s, 0.5s",
     "transition-timing-function": "linear"})
check("anim: transition longhand lists repeat per property",
      len(_tl) == 2 and _tl[0].prop == "color" and _tl[0].duration == 1.0
      and _tl[1].duration == 1.0 and _tl[1].delay == 0.5
      and _tl[0].timing(0.5) == 0.5, repr(_tl))
check("anim: transition-property none disables",
      anim.parse_transitions({"transition": "all 1s",
                              "transition-property": "none"}) == ())
_all = anim.parse_transitions({"transition": "all 2s, opacity 1s"})
check("anim: exact property match beats all",
      anim.transition_for(_all, "opacity").duration == 1.0
      and anim.transition_for(_all, "width").duration == 2.0)

_as = anim.parse_animations(
    {"animation": "spin 2s linear 0.5s infinite alternate paused"})
check("anim: animation shorthand normalizes",
      len(_as) == 1 and _as[0].name == "spin" and _as[0].duration == 2.0
      and _as[0].delay == 0.5 and _as[0].iterations == float("inf")
      and _as[0].direction == "alternate" and _as[0].play_state == "paused"
      and _as[0].timing(0.25) == 0.25, repr(_as))
_al = anim.parse_animations(
    {"animation-name": "a, b", "animation-duration": "1s, 2s",
     "animation-fill-mode": "both",
     "animation-iteration-count": "3"})
check("anim: animation longhands fan out over names",
      len(_al) == 2 and _al[0].name == "a" and _al[0].duration == 1.0
      and _al[1].name == "b" and _al[1].duration == 2.0
      and all(s.fill == "both" and s.iterations == 3.0 for s in _al),
      repr(_al))
check("anim: animation none yields nothing",
      anim.parse_animations({"animation": "none"}) == ())

_kf = anim.parse_keyframes([
    "@keyframes fade { from { opacity: 0 } 50% { opacity: 0.9 } "
    "to { opacity: 1 } }"
    "@-webkit-keyframes slide { 0%, 100% { margin: 4px } "
    "50% { margin-left: 8px } }"])
check("anim: @keyframes from/to/% parse",
      [o for o, _ in _kf["fade"]] == [0.0, 0.5, 1.0]
      and _kf["fade"][1][1]["opacity"] == "0.9", repr(_kf.get("fade")))
check("anim: prefixed keyframes + shorthand expansion in frames",
      _kf["slide"][0][1]["margin-left"] == "4px"
      and _kf["slide"][1][1]["margin-left"] == "8px"
      and _kf["slide"][2][1]["margin-top"] == "4px", repr(_kf.get("slide")))
_kf2 = anim.parse_keyframes(
    ["@keyframes x { to { opacity: 0 } }",
     "@keyframes x { to { opacity: 0.5 } }"])
check("anim: later same-name keyframes win",
      _kf2["x"][-1][1]["opacity"] == "0.5")

_ease_in = anim.parse_timing("ease-in")
_steps4 = anim.parse_timing("steps(4)")
check("anim: timing functions",
      anim.parse_timing("linear")(0.5) == 0.5
      and _ease_in(0.5) < 0.5
      and abs(anim.parse_timing("cubic-bezier(0,0,1,1)")(0.3) - 0.3) < 1e-3
      and _steps4(0.3) == 0.25 and _steps4(0.99) == 0.75
      and anim.parse_timing("steps(2, start)")(0.1) == 0.5)

check("anim: length/number interpolation",
      anim.interpolate("width", "10px", "20px", 0.5) == "15px"
      and anim.interpolate("opacity", "0", "1", 0.25) == "0.25")
check("anim: color interpolation",
      anim.interpolate("color", "#000000", "#ffffff", 0.5) == "#808080"
      and anim.interpolate("background-color", "rgb(0,0,0)",
                           "rgba(255,255,255,0)", 0.5)
      == "rgba(128,128,128,0.5)")
check("anim: transform interpolation",
      anim.interpolate("transform", "translate(0px, 0px)",
                       "translate(10px, 20px)", 0.5)
      == "translate(5px, 10px)"
      and anim.interpolate("transform", "none", "translatex(10px)", 0.5)
      == "translatex(5px)")
check("anim: non-interpolable values flip at 50%",
      anim.interpolate("display", "none", "block", 0.25) == "none"
      and anim.interpolate("display", "none", "block", 0.75) == "block")
check("anim: damage classification",
      anim.classify("opacity") == anim.DAMAGE_PAINT
      and anim.classify("background-color") == anim.DAMAGE_PAINT
      and anim.classify("transform") == anim.DAMAGE_PAINT
      and anim.classify("width") == anim.DAMAGE_LAYOUT)

# engine: transition lifecycle against an injected clock
_clock = [0.0]
_eng = anim.AnimationEngine(clock=lambda: _clock[0])
_eng.reset([])
_aroot = Element("html", {}, None)
_adiv = Element("div", {}, _aroot)
_aroot.children.append(_adiv)
_aroot.style = {}
_adiv.style = {"opacity": "0", "transition": "opacity 1s linear",
               "width": "100px"}
_r = _eng.on_frame(_aroot)
check("anim: first style never transitions",
      _r.damage == "none" and not _eng.active
      and _adiv.style["opacity"] == "0")
_adiv.style["opacity"] = "1"  # a restyle changed the base value
_r = _eng.on_frame(_aroot)
check("anim: transition starts from the old value",
      _r.damage == "paint" and _eng.active
      and _adiv.style["opacity"] == "0", repr(_adiv.style))
_clock[0] = 0.5
_eng.on_frame(_aroot)
check("anim: transition samples midway", _adiv.style["opacity"] == "0.5")
_clock[0] = 2.0
_r = _eng.on_frame(_aroot)
check("anim: transition completes to the base value",
      _adiv.style["opacity"] == "1" and not _eng.active
      and _r.damage == "paint")
_adiv.style["opacity"] = "0"  # reverse: new target mid-idle
_r = _eng.on_frame(_aroot)
_clock[0] = 2.5
_eng.on_frame(_aroot)
check("anim: transition retargets from current value",
      _adiv.style["opacity"] == "0.5", _adiv.style["opacity"])
_clock[0] = 4.0
_eng.on_frame(_aroot)
_adiv.style["width"] = "200px"  # width transitions are layout damage
_adiv.style["transition"] = "all 1s linear"
_r = _eng.on_frame(_aroot)
check("anim: layout property transition reports layout damage",
      _r.damage == "layout" and _adiv.style["width"] == "100px")
_clock[0] = 4.5
_eng.on_frame(_aroot)
check("anim: width transition midway", _adiv.style["width"] == "150px")
_clock[0] = 6.0
_eng.on_frame(_aroot)

# base change without a matching transition snaps instantly
_adiv.style["transition"] = "opacity 1s linear"
_eng.on_frame(_aroot)
_adiv.style["width"] = "300px"
_r = _eng.on_frame(_aroot)
check("anim: uncovered property snaps without animating",
      _adiv.style["width"] == "300px" and _r.damage == "none")

# engine: @keyframes lifecycle
_clock[0] = 0.0
_eng2 = anim.AnimationEngine(clock=lambda: _clock[0])
_eng2.reset(["@keyframes fade { from { opacity: 0 } to { opacity: 1 } }"])
_kroot = Element("html", {}, None)
_kdiv = Element("div", {}, _kroot)
_kroot.children.append(_kdiv)
_kroot.style = {}
_kdiv.style = {"animation": "fade 2s linear"}
_r = _eng2.on_frame(_kroot)
check("anim: keyframes start at from-frame",
      _kdiv.style.get("opacity") == "0" and _eng2.active
      and _r.damage == "paint")
_clock[0] = 1.0
_eng2.on_frame(_kroot)
check("anim: keyframes sample midway", _kdiv.style["opacity"] == "0.5")
_clock[0] = 3.0
_r = _eng2.on_frame(_kroot)
check("anim: fill:none reverts after the run",
      "opacity" not in _kdiv.style and not _eng2.active)

_kdiv.style = {"animation": "fade 1s linear 1s both"}
_clock[0] = 10.0
_eng2.on_frame(_kroot)
check("anim: backwards fill applies during the delay",
      _kdiv.style.get("opacity") == "0")
_clock[0] = 11.5
_eng2.on_frame(_kroot)
check("anim: delayed run samples after its delay",
      _kdiv.style["opacity"] == "0.5")
_clock[0] = 13.0
_r = _eng2.on_frame(_kroot)
check("anim: forwards fill holds the end value",
      _kdiv.style["opacity"] == "1" and not _eng2.active)

_kdiv.style = {"animation": "fade 1s linear infinite alternate"}
_clock[0] = 20.0
_eng2.on_frame(_kroot)
_clock[0] = 21.25  # second iteration runs in reverse
_eng2.on_frame(_kroot)
check("anim: alternate direction reverses odd iterations",
      _kdiv.style["opacity"] == "0.75" and _eng2.active,
      _kdiv.style.get("opacity"))
_kdiv.style = {"animation": "fade 10s linear paused"}
_clock[0] = 30.0
_eng2.on_frame(_kroot)
_clock[0] = 35.0
_r = _eng2.on_frame(_kroot)
check("anim: paused animation holds and wants no frames",
      _kdiv.style.get("opacity") == "0" and not _eng2.active)
_kdiv.style = {}
_r = _eng2.on_frame(_kroot)
check("anim: removing animation-name cancels and cleans up",
      "opacity" not in _kdiv.style and not _eng2._nodes)
_kdiv.style = {"animation": "fade 5s linear"}
_eng2.on_frame(_kroot)
_kroot.children.remove(_kdiv)
_eng2.on_frame(_kroot)
check("anim: removed DOM nodes drop their animation state",
      not _eng2._nodes)

# through the real pipeline: an animated height changes layout geometry
_anim_css = ("div { width: 100px; height: 10px; "
             "animation: grow 2s linear; } "
             "@keyframes grow { from { height: 10px } "
             "to { height: 110px } }")
_anim_dom = _styled(_anim_css, "<div>x</div>")
_clock[0] = 0.0
_eng3 = anim.AnimationEngine(clock=lambda: _clock[0])
_eng3.reset([_anim_css])
_eng3.on_frame(_anim_dom)   # first frame starts the animation's clock
_clock[0] = 1.0             # halfway through the 2s run
_eng3.on_frame(_anim_dom)
_anim_doc = DocumentLayout(_anim_dom)
_anim_doc.layout(800)
_anim_box = next(o for o in layout_tree_to_list(_anim_doc, [])
                 if isinstance(o, BlockLayout)
                 and getattr(o.node, "tag", None) == "div")
check("anim: sampled keyframe height feeds layout",
      abs(_anim_box.height - 60.0) < 0.01, _anim_box.height)

# --- iframe: isolated child browsing contexts (browser/frames.py) ---
from browser import frame_bridge  # noqa: E402
from browser import frames as gg_frames  # noqa: E402

check("iframe: schemeful origin model",
      gg_frames.same_origin(net.URL("https://a.test/x"),
                            net.URL("https://a.test/y"))
      and not gg_frames.same_origin(net.URL("https://a.test/"),
                                    net.URL("http://a.test/"))
      and not gg_frames.same_origin(net.URL("https://a.test/"),
                                    net.URL("https://b.test/"))
      and gg_frames.origin_of(net.URL("about:home")) is None)
check("iframe: sandbox attribute parses",
      gg_frames.parse_sandbox(None) is None
      and gg_frames.parse_sandbox("") == set()
      and gg_frames.parse_sandbox("allow-scripts ALLOW-SAME-ORIGIN")
      == {"allow-scripts", "allow-same-origin"})
check("iframe: X-Frame-Options gates embedding",
      gg_frames.frame_blocked_reason(
          {"X-Frame-Options": "DENY"}, net.URL("https://c.test/"),
          net.URL("https://p.test/")) is not None
      and gg_frames.frame_blocked_reason(
          {"x-frame-options": "SAMEORIGIN"}, net.URL("https://p.test/a"),
          net.URL("https://p.test/")) is None
      and gg_frames.frame_blocked_reason(
          {"x-frame-options": "SAMEORIGIN"}, net.URL("https://c.test/"),
          net.URL("https://p.test/")) is not None)
check("iframe: CSP frame-ancestors overrides XFO",
      gg_frames.frame_blocked_reason(
          {"content-security-policy": "frame-ancestors 'none'",
           "x-frame-options": "SAMEORIGIN"},
          net.URL("https://p.test/child"),
          net.URL("https://p.test/")) is not None
      and gg_frames.frame_blocked_reason(
          {"content-security-policy": "frame-ancestors 'self'"},
          net.URL("https://p.test/child"),
          net.URL("https://p.test/")) is None
      and gg_frames.frame_blocked_reason(
          {"content-security-policy":
           "default-src 'self'; frame-ancestors https://p.test"},
          net.URL("https://c.test/"), net.URL("https://p.test/")) is None
      and gg_frames.frame_blocked_reason(
          {"content-security-policy": "frame-ancestors *.p.test"},
          net.URL("https://c.test/"),
          net.URL("https://sub.p.test/")) is None)

# a tiny fake web the frame loader fetches from
_FRAME_SITES = {
    "https://frchild.test/": (
        {},
        "<style>body{margin:0} .red{background-color:#ff0000;"
        "width:50px;height:30px} p{margin:0}</style>"
        '<div class=red></div><a href="/next">go next</a>'
        '<div style="height:400px"></div>'),
    "https://frchild.test/next": (
        {}, "<p id=second>second page</p>"),
    "https://frdeny.test/": (
        {"x-frame-options": "DENY"}, "<p>secret</p>"),
}


def _fake_frame_request_text(url, *args, **kwargs):
    key = str(url)
    if key in _FRAME_SITES:
        headers, body = _FRAME_SITES[key]
        return dict(headers), body, url
    raise OSError(f"no fake page for {key}")


_real_rt = net.request_text
net.request_text = _fake_frame_request_text
try:
    _fr_parent_url = net.URL("https://frparent.test/")
    _fr_parent = _styled(
        "body { margin: 0 }",
        '<iframe id=fr src="https://frchild.test/" width=200 height=100>'
        "fallback content</iframe>")
    _fr_mgr = gg_frames.FrameManager(
        top_url=_fr_parent_url, use_native=False)
    _fr_mgr.sync(_fr_parent, _fr_parent_url)
    _fr_node = _find(_fr_parent, "iframe")
    _fr = _fr_node._frame
    check("iframe: fallback child document loads isolated",
          _fr is not None and _fr.status == "loaded"
          and str(_fr.url) == "https://frchild.test/"
          and _fr_node.children == []
          and _fr_node.style["width"] == "200.0px"
          and _fr_node.style["height"] == "100.0px",
          repr((_fr and _fr.status, _fr_node.style.get("width"))))

    _fr_doc = DocumentLayout(_fr_parent)
    _fr_doc.layout(800)
    _fr_box = next(o for o in layout_tree_to_list(_fr_doc, [])
                   if getattr(getattr(o, "node", None), "tag", None)
                   == "iframe" and getattr(o, "height", 0) > 0
                   and o.__class__.__name__ == "BlockLayout")
    _fr_cmds = paint_tree(_fr_doc, [])
    _fr_clips = [c for c in _fr_cmds
                 if c.__class__.__name__ == "DrawClipPush"
                 and abs(c.left - _fr_box.x) < 0.01
                 and abs(c.right - (_fr_box.x + _fr_box.width)) < 0.01]
    _fr_red = [c for c in _fr_cmds
               if getattr(c, "color", "") == "#ff0000"]
    check("iframe: child paints inside the clipped frame box",
          _fr_clips and _fr_red
          and abs(_fr_red[0].left - _fr_box.x) < 0.01
          and abs(_fr_red[0].top - _fr_box.y) < 0.01
          and _fr_red[0].right <= _fr_box.x + _fr_box.width + 0.01,
          repr((_fr_box.x, _fr_box.y,
                [(c.left, c.top) for c in _fr_red])))

    # nested hit test resolves the child link under parent coordinates
    def _in_anchor(o):
        node = getattr(o, "node", None)
        while node is not None:
            if getattr(node, "tag", None) == "a":
                return True
            node = getattr(node, "parent", None)
        return False

    _child_doc = _fr._document_layout(_fr_box.width, _fr_box.height)
    _child_a = next(o for o in layout_tree_to_list(_child_doc, [])
                    if _in_anchor(o) and getattr(o, "width", 0) > 0
                    and getattr(o, "height", 0) > 0)
    _hit = gg_frames.hit_frame(
        _fr_box, _fr_box.x + _child_a.x + 1, _fr_box.y + _child_a.y + 1)
    check("iframe: nested hit-test finds the child link",
          _hit is not None
          and _fr.find_link(_hit[1].node) == "/next",
          repr(_hit))

    # frame-internal scrolling: content shifts, page does not
    _scrolled = _fr.scroll_by(30, _fr_box.width, _fr_box.height)
    _fr_doc2 = DocumentLayout(_fr_parent)
    _fr_doc2.layout(800)
    _fr_red2 = [c for c in paint_tree(_fr_doc2, [])
                if getattr(c, "color", "") == "#ff0000"]
    check("iframe: wheel scroll moves only the frame content",
          _scrolled and _fr_red2
          and abs(_fr_red2[0].top - (_fr_box.y - 30)) < 0.01,
          repr([(c.top) for c in _fr_red2]))
    _fr.scroll = 0.0

    # in-frame navigation replaces the child document only
    _fr.navigate("/next")
    check("iframe: link navigation stays inside the frame",
          str(_fr.url) == "https://frchild.test/next"
          and any(isinstance(n, Element)
                  and n.attributes.get("id") == "second"
                  for n in tree_to_list(_fr.root, []))
          and _fr.status == "loaded")

    # X-Frame-Options refusal renders a placeholder, not the content
    _xfo_parent = _styled(
        "", '<iframe id=x src="https://frdeny.test/"></iframe>')
    _xfo_mgr = gg_frames.FrameManager(
        top_url=_fr_parent_url, use_native=False)
    _xfo_mgr.sync(_xfo_parent, _fr_parent_url)
    _xfo = _find(_xfo_parent, "iframe")._frame
    check("iframe: X-Frame-Options DENY blocks the document",
          _xfo.status == "blocked" and _xfo.root is None
          and "X-Frame-Options" in _xfo.blocked_reason
          and len(_xfo.paint_cmds(0, 0, 100, 50)) == 3)

    # srcdoc: inline markup, parent base URL (same-origin content)
    _sd_parent = _styled(
        "", "<iframe id=sd srcdoc=\"<p id=inner>hello</p>\"></iframe>")
    _sd_mgr = gg_frames.FrameManager(
        top_url=_fr_parent_url, use_native=False)
    _sd_mgr.sync(_sd_parent, _fr_parent_url)
    _sd = _find(_sd_parent, "iframe")._frame
    check("iframe: srcdoc renders with the parent base URL",
          _sd.status == "loaded" and _sd.url is _fr_parent_url
          and any(isinstance(n, Element)
                  and n.attributes.get("id") == "inner"
                  for n in tree_to_list(_sd.root, [])))

    # removing the element disposes its frame and returns the budget
    _budget_before = _fr_mgr.budget[0]
    for n in tree_to_list(_fr_parent, []):
        if isinstance(n, Element) and n.tag == "body":
            n.children = []
    _fr_mgr.sync(_fr_parent, _fr_parent_url)
    check("iframe: removed elements drop their frame documents",
          not _fr_mgr.frames and _fr_mgr.budget[0] == _budget_before + 1)
finally:
    net.request_text = _real_rt

if native.available():
    from browser.network_backend import default_network_backend \
        as _frame_backend_factory

    _FRAME_SITES.update({
        "https://frparent.test/": (
            {},
            '<div id=out>pending</div>'
            '<iframe id=f src="https://frchild2.test/"></iframe>'
            "<script>document.getElementById('f').addEventListener("
            "'load', function () { document.getElementById('out')"
            ".textContent = 'frame-loaded'; });</script>"),
        "https://frchild2.test/": (
            {},
            '<h1 id=t>child title</h1>'
            "<script>document.cookie = 'fc=1; path=/';"
            "document.getElementById('t').textContent = 'scripted';"
            "</script>"),
        "https://frsandbox.test/": (
            {},
            '<p id=s>static</p>'
            "<script>document.getElementById('s').textContent="
            "'scripted';</script>"),
        "https://frsbparent.test/": (
            {}, '<iframe id=sb sandbox src="https://frsandbox.test/">'
                "</iframe>"),
        "https://frdeep.test/1": (
            {}, '<p>d1</p><iframe id=d src="https://frdeep.test/2">'
                "</iframe>"),
        "https://frdeep.test/2": (
            {}, '<p>d2</p><iframe id=d src="https://frdeep.test/3">'
                "</iframe>"),
        "https://frdeep.test/3": (
            {}, '<p>d3</p><iframe id=d src="https://frdeep.test/4">'
                "</iframe>"),
        "https://frdeep.test/4": ({}, "<p>d4</p>"),
        "https://frdeepparent.test/": (
            {}, '<iframe id=d0 src="https://frdeep.test/1"></iframe>'),
        # postMessage: a same-origin child, a cross-origin one, and a
        # sandboxed same-site one (whose origin must read as opaque)
        "https://frmsg.test/": (
            {},
            '<p id=log>-</p>'
            '<iframe id=same src="https://frmsg.test/child"></iframe>'
            '<iframe id=cross src="https://frother.test/child"></iframe>'
            '<iframe id=sb sandbox="allow-scripts"'
            ' src="https://frmsg.test/child"></iframe>'
            "<script>var seen = [];"
            "window.addEventListener('message', function (e) {"
            "  seen.push(JSON.stringify(e.data) + '@' + e.origin"
            "    + (e.source === document.getElementById('same')"
            "       .contentWindow ? '#same' : '#other'));"
            "  document.getElementById('log').textContent ="
            "    seen.join(' | ');"
            "});</script>"),
        "https://frmsg.test/child": (
            {},
            '<p id=c>child</p>'
            "<script>"
            "window.addEventListener('message', function (e) {"
            "  document.getElementById('c').textContent ="
            "    'got ' + JSON.stringify(e.data) + ' @' + e.origin;"
            "  e.source.postMessage({pong: e.data.n + 1}, e.origin);"
            "});"
            "document.getElementById('c').textContent ="
            "  'framed=' + (window.top !== window);"
            "</script>"),
        "https://frother.test/child": ({}, '<p id=c>cross</p>'),
        # per-frame session history
        "https://frhist.test/": (
            {}, '<h1>top</h1>'
                '<iframe id=h src="https://frhist.test/a"></iframe>'),
        "https://frhist.test/a": ({}, '<p id=c>page-a</p>'),
        "https://frhist.test/b": ({}, '<p id=c>page-b</p>'),
        "https://frhist.test/c": ({}, '<p id=c>page-c</p>'),
    })
    net.request_text = _fake_frame_request_text
    try:
        _fpage = Page()
        _fpage.goto("https://frparent.test/", settle=True)
        _fp = _fpage.frame("#f")
        check("iframe: driver frame() opens the isolated child",
              _fp is not None and _fp.status == "loaded"
              and str(_fp.url) == "https://frchild2.test/"
              and _fp.text("#t") == "scripted",
              repr((_fp and _fp.status, _fp and _fp.text("#t"))))
        check("iframe: load event fires on the parent element",
              _fpage.text("#out") == "frame-loaded",
              repr(_fpage.text("#out")))
        _fbackend = _frame_backend_factory()
        _child_jar = _fbackend.cookies_for(net.URL("https://frchild2.test/"))
        _parent_jar = _fbackend.cookies_for(net.URL("https://frparent.test/"))
        check("iframe: child cookies stay in the child origin's jar",
              "fc=1" in _child_jar and "fc" not in _parent_jar,
              repr((_child_jar, _parent_jar)))
        check("iframe: frame document.title/DOM invisible to parent DOM",
              not any(isinstance(n, Element)
                      and n.attributes.get("id") == "t"
                      for n in tree_to_list(
                          native.build_tree(_fpage._export()), [])))
        _fpage.close()

        _spage = Page()
        _spage.goto("https://frsbparent.test/", settle=True)
        _sp = _spage.frame("#sb")
        check("iframe: sandbox without allow-scripts blocks child JS",
              _sp is not None and _sp.status == "loaded"
              and _sp.text("#s") == "static",
              repr(_sp and _sp.text("#s")))
        _spage.close()

        _dpage = Page()
        _dpage.goto("https://frdeepparent.test/", settle=True)
        _d0 = _dpage.frame("#d0")
        _fd1 = _d0._fd
        _fd2 = next(iter(_fd1.subframes.frames.values()))
        _fd3 = next(iter(_fd2.subframes.frames.values()))
        check("iframe: nesting stops at the depth limit",
              _fd1.status == "loaded" and _fd2.status == "loaded"
              and _fd3.status == "loaded"
              and (_fd3.subframes is None
                   or not _fd3.subframes.frames),
              repr((_fd1.status, _fd2.status, _fd3.status)))
        _dpage.close()

        # --- cross-document messaging (browser/frame_bridge.py) ---
        _mpage = Page()
        _mpage.goto("https://frmsg.test/", settle=True)
        _msame = _mpage.frame("#same")
        _mcross = _mpage.frame("#cross")
        _msb = _mpage.frame("#sb")
        check("iframe: a framed document sees window.top !== window",
              _msame is not None and _msame.text("#c") == "framed=true",
              repr(_msame and _msame.text("#c")))
        check("iframe: contentWindow resolves only for a published frame",
              _mpage.evaluate(
                  "typeof document.getElementById('same').contentWindow")
              == "object"
              and _mpage.evaluate(
                  "document.getElementById('same').contentWindow"
                  " === document.getElementById('same').contentWindow"),
              "the proxy must be stable, or e.source comparisons fail")
        # the isolation proof: a cross-origin frame hands out a proxy
        # carrying postMessage and nothing else
        check("iframe: a cross-origin frame exposes no document",
              _mpage.evaluate(
                  "document.getElementById('cross').contentDocument"
                  " === null")
              and _mpage.evaluate(
                  "typeof document.getElementById('cross').contentWindow"
                  ".document") == "undefined"
              and _mpage.evaluate(
                  "typeof document.getElementById('cross').contentWindow"
                  ".location") == "undefined",
              "postMessage is the only cross-origin channel")
        # parent -> same-origin child -> parent, in one pump
        _mpage.evaluate(
            "document.getElementById('same').contentWindow"
            ".postMessage({n: 41}, '*')")
        _mpage.pump_frames()
        check("iframe: postMessage reaches the child with the sender origin",
              _msame.text("#c") == 'got {"n":41} @https://frmsg.test',
              repr(_msame.text("#c")))
        check("iframe: e.source.postMessage replies to the right window",
              _mpage.text("#log") == '{"pong":42}@https://frmsg.test#same',
              repr(_mpage.text("#log")))
        # a targetOrigin the receiver does not match is dropped silently
        _mpage.evaluate(
            "document.getElementById('same').contentWindow"
            ".postMessage({n: 100}, 'https://wrong.test')")
        _mpage.pump_frames()
        check("iframe: a mismatched targetOrigin is dropped silently",
              _msame.text("#c") == 'got {"n":41} @https://frmsg.test',
              repr(_msame.text("#c")))
        # a sandboxed frame has an opaque origin even from its own site
        check("iframe: a sandboxed frame's origin is opaque, not same-site",
              frame_bridge.effective_origin(_msb._fd) is None
              and frame_bridge.origin_string(
                  frame_bridge.effective_origin(_msb._fd)) == "null"
              and not frame_bridge.scriptable(
                  _msb._fd, net.URL("https://frmsg.test/")),
              "allow-same-origin is what grants the real origin back")
        _mpage.close()

        # --- per-frame session history (browser/navigation.py) ---
        from browser import navigation as gg_nav  # noqa: E402

        _hpage = Page()
        _hpage.goto("https://frhist.test/", settle=True)
        _hframe = _hpage.frame("#h")
        _hstate0 = gg_nav.capture_frame_state(_hpage._frames)
        check("iframe: frame state is captured by positional path",
              _hstate0 == (((0,), "https://frhist.test/a", 0.0),),
              repr(_hstate0))
        # a navigation records a new entry against the same document
        _hnav = []
        _hpage._frames.on_navigate = lambda p, u: _hnav.append((p, u))
        for fd in _hpage._frames.ordered_frames():
            fd.on_navigate = _hpage._frames.on_navigate
        _hframe.navigate("/b")
        check("iframe: an in-frame navigation reports its path and target",
              _hnav == [((0,), "https://frhist.test/b")], repr(_hnav))
        _hstate1 = gg_nav.frame_state_with(
            _hstate0, _hnav[0][0], _hnav[0][1])
        check("iframe: the new entry repoints only that frame",
              _hstate1 == (((0,), "https://frhist.test/b", 0.0),),
              repr(_hstate1))
        check("iframe: frame_at resolves a path to the live frame",
              _hpage._frames.frame_at((0,)) is _hframe._fd
              and _hpage._frames.frame_at((5,)) is None
              and _hpage._frames.frame_at((0, 0)) is None)
        # replaying the older entry walks the frame back, content and all
        check("iframe: a navigation moved the frame forward",
              _hpage.frame("#h").text("#c") == "page-b",
              repr(_hpage.frame("#h").text("#c")))
        gg_nav.restore_frame_state(_hpage._frames, _hstate0)
        check("iframe: restoring an entry navigates the frame back",
              str(_hpage._frames.frame_at((0,)).url)
              == "https://frhist.test/a"
              and _hpage.frame("#h").text("#c") == "page-a",
              repr((str(_hpage._frames.frame_at((0,)).url),
                    _hpage.frame("#h").text("#c"))))
        # a parent-frame navigation invalidates rows for its children
        _hnested = (((0,), "https://frhist.test/a", 0.0),
                    ((0, 0), "https://frhist.test/b", 0.0),
                    ((1,), "https://frhist.test/c", 0.0))
        check("iframe: repointing a frame drops its childrens' rows",
              gg_nav.frame_state_with(_hnested, (0,), "https://frhist.test/c")
              == (((0,), "https://frhist.test/c", 0.0),
                  ((1,), "https://frhist.test/c", 0.0)),
              repr(gg_nav.frame_state_with(
                  _hnested, (0,), "https://frhist.test/c")))
        check("iframe: restore replays shallower paths first",
              [len(r[0]) for r in sorted(
                  _hnested, key=lambda r: (len(r[0]), r[0]))] == [1, 1, 2],
              "a parent navigation rebuilds the subframes a deeper row needs")
        _hpage.close()
    finally:
        net.request_text = _real_rt
else:
    print("[SKIP] iframe driver checks - native ggcore not built")

# --- accessibility tree (browser/accessibility.py) ---
from browser import accessibility as ax  # noqa: E402

_ax_dom = _styled("", """
<html><head><title>AX Test Page</title></head><body>
<header><h1>Site</h1></header>
<nav aria-label="주 메뉴"><a href="/a">첫 링크</a></nav>
<main>
  <h2 aria-level="3">Section</h2>
  <section aria-labelledby="sec-t"><span id="sec-t">Named region</span>
    <p>body text</p></section>
  <section><p>anonymous section is no landmark</p></section>
  <form aria-label="검색"><label for="q">검색어</label>
    <input id="q" placeholder="type here" value="hello">
    <input type="checkbox" id="c1" checked aria-describedby="c1help">
    <span id="c1help">동의 여부</span>
    <input type="submit" value="찾기">
  </form>
  <img src="x.png" alt="로고 이미지">
  <img src="y.png" alt="">
  <div role="presentation"><a href="/inner">through presentation</a></div>
  <div hidden><a href="/gone">hidden link</a></div>
  <div aria-hidden="true"><button>invisible</button></div>
  <button disabled aria-expanded="false" title="더보기">More</button>
  <table><caption>가격표</caption><tr><th>품목</th><td>값</td></tr></table>
  <ul><li>하나</li><li>둘</li></ul>
</main>
<article><header><p>article header is not a banner</p></header></article>
<footer>bottom</footer>
</body></html>""")
_ax_tree = ax.build_tree(_ax_dom)
_ax_flat = ax.flatten(_ax_tree)
_ax_roles = [n.role for n, _d in _ax_flat]


def _ax_find(role, name=None):
    for n, _d in _ax_flat:
        if n.role == role and (name is None or n.name == name):
            return n
    return None


check("ax: document root carries the page title",
      _ax_tree.role == "document" and _ax_tree.name == "AX Test Page")
check("ax: landmark roles with conditional header/footer",
      _ax_find("banner") is not None
      and _ax_find("contentinfo") is not None
      and _ax_find("navigation") is not None
      and _ax_find("main") is not None
      and _ax_roles.count("banner") == 1,  # article header excluded
      repr(_ax_roles))
check("ax: named form/section become landmarks, anonymous do not",
      _ax_find("form") is not None and _ax_find("form").name == "검색"
      and _ax_find("region") is not None
      and _ax_find("region").name == "Named region"
      and _ax_roles.count("region") == 1)
check("ax: aria-label names the navigation",
      _ax_find("navigation").name == "주 메뉴")
check("ax: label[for] names the textbox, value exposed",
      _ax_find("textbox") is not None
      and _ax_find("textbox").name == "검색어"
      and _ax_find("textbox").states.get("value") == "hello",
      repr((_ax_find("textbox").name, _ax_find("textbox").states)))
check("ax: submit button named from its value",
      _ax_find("button", "찾기") is not None)
check("ax: checkbox state + aria-describedby description",
      _ax_find("checkbox").states.get("checked") is True
      and _ax_find("checkbox").description == "동의 여부",
      repr((_ax_find("checkbox").states,
            _ax_find("checkbox").description)))
check("ax: img alt names, alt='' is decorative",
      _ax_find("img") is not None and _ax_find("img").name == "로고 이미지"
      and sum(1 for r in _ax_roles if r == "img") == 1)
check("ax: role=presentation drops the node, keeps the subtree",
      _ax_find("link", "through presentation") is not None)
check("ax: hidden/aria-hidden subtrees leave the tree",
      _ax_find("link", "hidden link") is None
      and _ax_find("button", "invisible") is None)
check("ax: disabled/expanded states and title fallback name",
      _ax_find("button", "More") is not None
      and _ax_find("button", "More").states.get("disabled") is True
      and _ax_find("button", "More").states.get("expanded") is False)
check("ax: table caption names the table, th scope splits headers",
      _ax_find("table").name == "가격표"
      and _ax_find("columnheader", "품목") is not None
      and _ax_find("cell", "값") is not None)
check("ax: list structure and item names",
      _ax_find("list") is not None
      and [n.name for n, _d in _ax_flat if n.role == "listitem"]
      == ["하나", "둘"])
check("ax: heading levels honor aria-level over the tag",
      ax.headings(_ax_tree) == [(1, "Site"), (3, "Section")],
      repr(ax.headings(_ax_tree)))
check("ax: landmark outline is ordered and typed",
      [(r, n) for r, n, _d in ax.landmarks(_ax_tree)]
      == [("banner", "Site"), ("navigation", "주 메뉴"),
          ("main", ""), ("region", "Named region"), ("form", "검색"),
          ("contentinfo", "bottom")]
      or [r for r, _n, _d in ax.landmarks(_ax_tree)]
      == ["banner", "navigation", "main", "region", "form",
          "contentinfo"],
      repr(ax.landmarks(_ax_tree)))

# keyboard focus and the accessibility tree must agree: everything in
# sequential focus order is either ax-focusable or sits in an
# aria-hidden subtree (aria-hidden hides from the tree but — like real
# browsers — does not remove keyboard focusability), and hidden/
# disabled controls appear in neither
def _in_aria_hidden(node):
    cur = node
    while cur is not None:
        attrs = getattr(cur, "attributes", None)
        if attrs and attrs.get("aria-hidden", "").casefold() == "true":
            return True
        cur = cur.parent
    return False


_ax_focusables = {id(n.node) for n, _d in _ax_flat if n.focusable}
_kb_order = keyboard.focus_order(_ax_dom)
check("ax: keyboard focus order matches ax-focusable nodes",
      _kb_order and all(
          id(n) in _ax_focusables or _in_aria_hidden(n)
          for n in _kb_order)
      and any(id(n) in _ax_focusables for n in _kb_order),
      repr([n.tag for n in _kb_order]))
_ax_disabled_btn = _ax_find("button", "More")
check("ax: disabled/hidden controls are not focusable",
      not _ax_disabled_btn.focusable
      and all("gone" not in (n.name or "") for n, _d in _ax_flat))
_kb_order[0].is_focused = True
_ax_tree2 = ax.build_tree(_ax_dom)
check("ax: focused element is marked in a fresh snapshot",
      any(n.focused and n.node is _kb_order[0]
          for n, _d in ax.flatten(_ax_tree2)))
_kb_order[0].is_focused = False

# activation consistency: toggling the checkbox flips the exposed state
_ax_cb_node = _ax_find("checkbox").node
del _ax_cb_node.attributes["checked"]
check("ax: state toggles flow into the next tree build",
      ax.build_tree(_ax_dom) is not None
      and next(n for n, _d in ax.flatten(ax.build_tree(_ax_dom))
               if n.role == "checkbox").states.get("checked") is False)

if native.available():
    _axp = Page()
    _axp.goto("data:text/html," + (
        "<title>drv-ax</title>"
        "<nav aria-label=menu><a href='/x'>go</a></nav>"
        "<label for=i>Name</label><input id=i>"
        "<div aria-hidden=true><a href='/no'>nope</a></div>"
        "<img src=z.png alt=Photo>"), settle=False)
    _axt = _axp.ax_tree()

    def _ax_walk(d, out):
        out.append(d)
        for c in d.get("children", []):
            _ax_walk(c, out)
        return out

    _axl = _ax_walk(_axt, [])
    check("ax: driver ax_tree mirrors roles/names/hidden pruning",
          _axt["role"] == "document" and _axt["name"] == "drv-ax"
          and any(d["role"] == "navigation" and d["name"] == "menu"
                  for d in _axl)
          and any(d["role"] == "textbox" and d["name"] == "Name"
                  for d in _axl)
          and any(d["role"] == "img" and d["name"] == "Photo"
                  for d in _axl)
          and not any(d.get("name") == "nope" for d in _axl),
          repr(_axl))
    _snap = _axp.snapshot()
    check("ax: native snapshot names use aria-label/label/alt",
          any(s["role"] == "navigation" and s["name"] == "menu"
              for s in _snap)
          and any(s["role"] == "textbox" and s["name"] == "Name"
                  for s in _snap)
          and any(s["role"] == "img" and s["name"] == "Photo"
                  for s in _snap)
          and not any(s["name"] == "nope" for s in _snap),
          repr(_snap))
    _axp.close()
else:
    print("[SKIP] ax driver checks - native ggcore not built")

# --- form controls: widget faces + select interaction ---
from browser import forms as gg_forms  # noqa: E402

_fc_dom = _styled("body { margin: 0 }", """<body>
<input type=checkbox id=cb0><input type=checkbox id=cb1 checked>
<input type=radio name=g id=rb0><input type=radio name=g id=rb1 checked>
<select id=sel><option>Apple<option selected>Banana<option>Cherry</select>
<textarea id=ta>hello
world</textarea>
<input type=submit value=Go id=sub>
<input type=text value=typed id=txt>
<span>tail</span></body>""")
_fc_doc = DocumentLayout(_fc_dom)
_fc_doc.layout(800, 600)
_fc_cmds = paint_tree(_fc_doc, [])
_fc_boxes = {}
for _o in layout_tree_to_list(_fc_doc, []):
    _nid = getattr(getattr(_o, "node", None), "attributes", {}).get("id")
    if _nid and isinstance(_o, BlockLayout):
        _fc_boxes.setdefault(_nid, _o)


def _fc_in(box, cmd, slack=3.0):
    """Is a painted command inside (or on) a control's box?"""
    return (cmd.left >= box.x - slack and cmd.top >= box.y - slack
            and getattr(cmd, "right", cmd.left) <= box.x + box.width + slack
            and cmd.bottom <= box.y + box.height + slack)


def _fc_kinds(box, kind):
    return [c for c in _fc_cmds
            if c.__class__.__name__ == kind and _fc_in(box, c)]


check("forms: options no longer leak into the page as inline text",
      not any(getattr(c, "text", "") in ("Apple", "Cherry")
              for c in _fc_cmds),
      repr([getattr(c, "text", "") for c in _fc_cmds
            if getattr(c, "text", "")]))
check("forms: a <select> gets its own box and paints the selected label",
      "sel" in _fc_boxes
      and any(getattr(c, "text", "") == "Banana"
              for c in _fc_kinds(_fc_boxes["sel"], "DrawText")),
      repr(sorted(_fc_boxes)))
check("forms: select paints a dropdown chevron inside its box",
      len(_fc_kinds(_fc_boxes["sel"], "DrawLine")) == 2)
check("forms: unchecked checkbox paints a visible empty box",
      len(_fc_kinds(_fc_boxes["cb0"], "DrawRect")) == 2
      and not _fc_kinds(_fc_boxes["cb0"], "DrawLine"),
      repr(_fc_kinds(_fc_boxes["cb0"], "DrawRect")))
check("forms: checked checkbox paints a checkmark",
      len(_fc_kinds(_fc_boxes["cb1"], "DrawLine")) == 2,
      repr(_fc_kinds(_fc_boxes["cb1"], "DrawLine")))
check("forms: radio paints circles, and only the checked one has a dot",
      len(_fc_kinds(_fc_boxes["rb0"], "DrawOval")) == 2
      and len(_fc_kinds(_fc_boxes["rb1"], "DrawOval")) == 3)
_fc_escapes = [
    (i, c.__class__.__name__, c.left, c.top)
    for i in ("cb0", "cb1", "rb0", "rb1", "sel", "ta", "sub", "txt")
    for c in _fc_boxes[i].paint()
    # clip markers carry sentinel top/bottom so the viewport cull can
    # never drop them — only real painted geometry is checked here
    if c.__class__.__name__ not in ("DrawClipPush", "DrawClipPop")
    and not _fc_in(_fc_boxes[i], c, slack=0.01)]
check("forms: control faces never paint outside their own box",
      not _fc_escapes, repr(_fc_escapes))
check("forms: textarea renders its lines, clipped to the control",
      [getattr(c, "text", "") for c in
       _fc_kinds(_fc_boxes["ta"], "DrawText")] == ["hello", "world"]
      and any(c.__class__.__name__ == "DrawClipPush" for c in _fc_cmds))
check("forms: submit button paints a face with a centred label",
      any(getattr(c, "text", "") == "Go" for c in _fc_cmds)
      and len(_fc_kinds(_fc_boxes["sub"], "DrawRect")) == 2)
check("forms: text input value is clipped to its box",
      any(getattr(c, "text", "") == "typed" for c in _fc_cmds))

# a styled control keeps the page's own look (the appearance:none idiom)
_fc_styled = _styled(
    "input { background-color: #222222; border-width: 2px }",
    "<input type=checkbox id=sc checked>")
_fc_sdoc = DocumentLayout(_fc_styled)
_fc_sdoc.layout(400)
_fc_scmds = paint_tree(_fc_sdoc, [])
check("forms: an author-styled control keeps its own face + state mark",
      not any(getattr(c, "color", "") == "#ffffff" for c in _fc_scmds)
      and sum(1 for c in _fc_scmds
              if c.__class__.__name__ == "DrawLine") == 2,
      repr([(c.__class__.__name__, getattr(c, "color", ""))
            for c in _fc_scmds]))

# <option> auto-closes so a select's label is only its own text
_fc_opts = HTMLParser(
    "<select><option>A<option selected>B<option>C</select>").parse()
_fc_sel = next(n for n in tree_to_list(_fc_opts, [])
               if isinstance(n, Element) and n.tag == "select")
check("forms: <option> auto-closes instead of nesting",
      [len([c for c in o.children if isinstance(c, Element)])
       for o in gg_forms.select_options(_fc_sel)] == [0, 0, 0],
      repr([o.children for o in gg_forms.select_options(_fc_sel)]))

# select activation: click/keyboard cycle the selection and submission
# follows it (gg paints a closed control, with no popup layer)
_fc_form = _styled(
    "", "<form><select id=s2><option>A<option selected>B<option>C"
        "</select></form>")
_fc_s2 = next(n for n in tree_to_list(_fc_form, [])
              if isinstance(n, Element) and n.tag == "select")
_fc_defaults = gg_forms.capture_defaults(_fc_form)
check("forms: select starts on its selected option",
      gg_forms.selected_index(_fc_s2) == 1
      and gg_forms._select_values(_fc_s2) == ["B"])
_fc_act = gg_forms.activate_control(_fc_s2, None, _fc_defaults)
check("forms: activating a select advances and reports a change",
      _fc_act is not None and _fc_act.kind == "select"
      and _fc_act.changed and gg_forms.selected_index(_fc_s2) == 2
      and gg_forms._select_values(_fc_s2) == ["C"],
      repr((_fc_act and _fc_act.kind, gg_forms.selected_index(_fc_s2))))
gg_forms.activate_control(_fc_s2, None, _fc_defaults)
check("forms: selection wraps at the end",
      gg_forms.selected_index(_fc_s2) == 0)
gg_forms.activate_control(_fc_s2, None, _fc_defaults, select_step=-1)
check("forms: a negative step walks backwards",
      gg_forms.selected_index(_fc_s2) == 2)
check("forms: exactly one option stays selected",
      sum(1 for o in gg_forms.select_options(_fc_s2)
          if "selected" in o.attributes) == 1)
check("forms: keyboard maps Enter/Space/arrows onto a select",
      keyboard.key_action(_fc_s2, "Enter") == "select-next"
      and keyboard.key_action(_fc_s2, "Space") == "select-next"
      and keyboard.key_action(_fc_s2, "ArrowDown") == "select-next"
      and keyboard.key_action(_fc_s2, "ArrowUp") == "select-prev")
_fc_disabled = _styled(
    "", "<select disabled id=d><option>A<option>B</select>")
_fc_d = next(n for n in tree_to_list(_fc_disabled, [])
             if isinstance(n, Element) and n.tag == "select")
check("forms: a disabled select never activates",
      gg_forms.activate_control(_fc_d, None, {}) is None
      and keyboard.key_action(_fc_d, "Enter") is None)
check("forms: reset restores the original selection",
      (lambda: (
          gg_forms.reset_form(gg_forms.find_form(_fc_s2), _fc_defaults),
          gg_forms.selected_index(_fc_s2))[1])() == 1)

# --- overflow:auto scroll containers ---
from browser.layout import (find_scrollable, hit_test_at,  # noqa: E402
                            scrolled_ancestor_offset,
                            is_scroll_container, scroll_axes,
                            scroll_container_by, scroll_position,
                            scroll_range)

_sc_dom = _styled("body { margin: 0 }", """<body>
<div id=fixed style="overflow:auto;height:100px;width:200px">
  <div id=tall style="height:400px;background-color:#ff0000">
    <p id=deep>deep</p></div></div>
<div id=grow style="overflow:auto;width:200px">
  <div style="height:300px">content-sized parent</div></div>
<div id=fits style="overflow:auto;height:300px;width:200px">
  <div style="height:50px">short</div></div>
<div id=hid style="overflow:hidden;height:60px;width:200px">
  <div style="height:400px">hidden</div></div>
<p id=tail>tail</p></body>""")
_sc_doc = DocumentLayout(_sc_dom)
_sc_doc.layout(600, 300)
_sc_list = layout_tree_to_list(_sc_doc, [])


def _sc_box(name):
    return next(o for o in _sc_list
                if isinstance(o, BlockLayout)
                and getattr(o.node, "attributes", {}).get("id") == name)


check("overflow: an axis scrolls only when it is definitely sized",
      scroll_axes(_sc_box("fixed").node) == (True, True)
      and scroll_axes(_sc_box("grow").node) == (False, True)
      and scroll_axes(_sc_box("hid").node) == (False, False)
      and is_scroll_container(_sc_box("fixed"))
      and not is_scroll_container(_sc_box("hid")),
      repr([(n, scroll_axes(_sc_box(n).node))
            for n in ("fixed", "grow", "hid")]))
check("overflow: a content-sized auto box cannot scroll that axis",
      scroll_range(_sc_box("grow"))[0] == 0.0
      and _sc_box("fixed")._clips())
check("overflow: scroll range is content minus viewport",
      abs(scroll_range(_sc_box("fixed"))[0] - 316.0) < 0.5
      and scroll_range(_sc_box("fits")) == (0.0, 0.0),
      repr(scroll_range(_sc_box("fixed"))))
check("overflow: a box whose content fits cannot scroll",
      not scroll_container_by(_sc_box("fits"), 100))

_sc_target = _sc_box("fixed")
check("overflow: scrolling moves and clamps at both ends",
      scroll_container_by(_sc_target, 50)
      and scroll_position(_sc_target)[0] == 50.0
      and scroll_container_by(_sc_target, 10 ** 6)
      and abs(scroll_position(_sc_target)[0] - 316.0) < 0.5
      and not scroll_container_by(_sc_target, 10)
      and scroll_container_by(_sc_target, -10 ** 6)
      and scroll_position(_sc_target)[0] == 0.0,
      repr(scroll_position(_sc_target)))

# paint: descendants shift, the container's own frame does not
_sc_before = [c for c in paint_tree(_sc_doc, [])
              if getattr(c, "color", "") == "#ff0000"]
scroll_container_by(_sc_target, 60)
_sc_after_cmds = paint_tree(_sc_doc, [])
_sc_after = [c for c in _sc_after_cmds
             if getattr(c, "color", "") == "#ff0000"]
check("overflow: scrolling offsets the content, not the container",
      _sc_before and _sc_after
      and abs((_sc_before[0].top - _sc_after[0].top) - 60.0) < 0.01,
      repr((_sc_before[0].top, _sc_after[0].top)))
_sc_clip = [c for c in _sc_after_cmds
            if c.__class__.__name__ == "DrawClipPush"
            and abs(c.left - _sc_target.x) < 0.01
            and abs(c.clip_bottom - (_sc_target.y + _sc_target.height))
            < 0.01]
check("overflow: the container clips its scrolled content", bool(_sc_clip),
      repr([(c.left, c.clip_top, c.right, c.clip_bottom)
            for c in _sc_after_cmds
            if c.__class__.__name__ == "DrawClipPush"]))
_sc_bar = [c for c in _sc_after_cmds
           if getattr(c, "color", "") == "#b0b0b0"]
check("overflow: an overflowing container paints a scrollbar inside it",
      len(_sc_bar) == 1
      and _sc_bar[0].right <= _sc_target.x + _sc_target.width + 0.01
      and _sc_bar[0].top >= _sc_target.y - 0.01
      and _sc_bar[0].bottom <= _sc_target.y + _sc_target.height + 0.01,
      repr([(c.left, c.top, c.right, c.bottom) for c in _sc_bar]))

# hit-testing composes the scroll offset the paint applied
_sc_deep = _sc_box("deep")
scroll_container_by(_sc_target, -10 ** 6)   # back to the top
_sc_hit_top = hit_test_at(_sc_list, 10, _sc_deep.y + 2, 0)
scroll_container_by(_sc_target, 40)
_sc_hit_scrolled = hit_test_at(_sc_list, 10, _sc_deep.y + 2 - 40, 0)
check("overflow: hit-testing follows the scrolled content",
      _sc_hit_top is not None and _sc_hit_scrolled is not None
      and _sc_hit_top is _sc_hit_scrolled,
      repr((_sc_hit_top, _sc_hit_scrolled)))
check("overflow: a point the content scrolled away from no longer hits it",
      hit_test_at(_sc_list, 10, _sc_deep.y + 2, 0) is not _sc_hit_top)

# scroll chaining: the innermost box that can still move wins
scroll_container_by(_sc_target, -10 ** 6)
check("overflow: a wheel inside the box targets the box",
      find_scrollable(_sc_deep, 90) is _sc_target)
check("overflow: upward at the top chains past it to the page",
      find_scrollable(_sc_deep, -90) is None)
scroll_container_by(_sc_target, 10 ** 6)
check("overflow: downward at the bottom chains past it to the page",
      find_scrollable(_sc_deep, 90) is None
      and find_scrollable(_sc_deep, -90) is _sc_target)
check("overflow: a box outside any scroller never targets one",
      find_scrollable(_sc_box("tail"), 90) is None)

# nested scrollers: the inner one takes the wheel first
_sc_nested = _styled("body { margin: 0 }", """<body>
<div id=outer style="overflow:auto;height:120px;width:200px">
  <div id=inner style="overflow:auto;height:60px;width:180px">
    <div id=innermost style="height:400px">x</div></div>
  <div style="height:300px">filler</div></div></body>""")
_sc_ndoc = DocumentLayout(_sc_nested)
_sc_ndoc.layout(600, 300)
_sc_nlist = layout_tree_to_list(_sc_ndoc, [])


def _sc_nbox(name):
    return next(o for o in _sc_nlist
                if isinstance(o, BlockLayout)
                and getattr(o.node, "attributes", {}).get("id") == name)


check("overflow: nested scrollers resolve innermost-first",
      find_scrollable(_sc_nbox("innermost"), 30) is _sc_nbox("inner"))
scroll_container_by(_sc_nbox("inner"), 10 ** 6)
check("overflow: a bottomed-out inner scroller chains to the outer one",
      find_scrollable(_sc_nbox("innermost"), 30) is _sc_nbox("outer"))
scroll_container_by(_sc_nbox("outer"), 25)
check("overflow: nested offsets compose in hit-testing",
      abs(scrolled_ancestor_offset(_sc_nbox("innermost"))[0]
          - (scroll_position(_sc_nbox("inner"))[0] + 25.0)) < 0.01,
      repr(scrolled_ancestor_offset(_sc_nbox("innermost"))))

# a clip bracket nested inside a scrolled subtree moves with it
_sc_tr = _styled("body { margin: 0 }", """<body>
<div id=s style="overflow:auto;height:80px;width:200px">
  <div style="overflow:hidden;height:40px;width:100px">
    <p style="height:200px">clipped</p></div>
  <div style="height:300px">filler</div></div></body>""")
_sc_trdoc = DocumentLayout(_sc_tr)
_sc_trdoc.layout(600, 300)
_sc_trbox = next(o for o in layout_tree_to_list(_sc_trdoc, [])
                 if isinstance(o, BlockLayout)
                 and getattr(o.node, "attributes", {}).get("id") == "s")


def _sc_inner_clip(doc):
    return [c for c in paint_tree(doc, [])
            if c.__class__.__name__ == "DrawClipPush"
            and abs(c.clip_bottom - c.clip_top - 40.0) < 0.5]


_sc_clip_before = _sc_inner_clip(_sc_trdoc)
scroll_container_by(_sc_trbox, 30)
_sc_clip_after = _sc_inner_clip(_sc_trdoc)
# the native path rebuilds the Python tree from the Rust arena on every
# DOM-changing tick; inner scroll offsets must ride across that
from browser.layout import (capture_scroll_state,  # noqa: E402
                            restore_scroll_state)

_sc_rebuild = _styled("body { margin: 0 }", """<body>
<div id=keep style="overflow:auto;height:80px;width:200px">
  <div style="height:400px">tall</div></div></body>""")
_sc_kdoc = DocumentLayout(_sc_rebuild)
_sc_kdoc.layout(600, 300)
_sc_keep = next(o for o in layout_tree_to_list(_sc_kdoc, [])
                if isinstance(o, BlockLayout)
                and getattr(o.node, "attributes", {}).get("id") == "keep")
_sc_keep.node._ridx = 42          # stands in for the Rust arena index
scroll_container_by(_sc_keep, 45)
_sc_state = capture_scroll_state(_sc_rebuild)
check("overflow: scroll state is captured by arena index",
      _sc_state == {42: (45.0, 0.0)}, repr(_sc_state))
# a rebuilt tree starts blank, then adopts the captured offsets
_sc_fresh = _styled("body { margin: 0 }", """<body>
<div id=keep style="overflow:auto;height:80px;width:200px">
  <div style="height:400px">tall</div></div></body>""")
_sc_fdoc = DocumentLayout(_sc_fresh)
_sc_fdoc.layout(600, 300)
_sc_fkeep = next(o for o in layout_tree_to_list(_sc_fdoc, [])
                 if isinstance(o, BlockLayout)
                 and getattr(o.node, "attributes", {}).get("id") == "keep")
_sc_fkeep.node._ridx = 42
check("overflow: a freshly rebuilt tree starts unscrolled",
      scroll_position(_sc_fkeep) == (0.0, 0.0))
restore_scroll_state(_sc_fresh, _sc_state)
check("overflow: restoring re-applies the offset after a tree rebuild",
      scroll_position(_sc_fkeep) == (45.0, 0.0),
      repr(scroll_position(_sc_fkeep)))

# JS observes and drives the engine's scrollers (scrollTop/scrollHeight)
from browser.layout import (PAGE_SCROLL_NODE,  # noqa: E402
                            apply_scroll_requests, collect_scroll_state)


def _sc_apply(layout_list, writes, into_view=(), viewport=300, page=0.0):
    """(element_moved, page_target) for one turn of scroll requests."""
    return apply_scroll_requests(layout_list, writes, into_view,
                                 viewport, page)

_sc_js = _styled("body { margin: 0 }", """<body>
<div id=j style="overflow:auto;height:100px;width:200px">
  <div style="height:500px">msg</div></div></body>""")
_sc_jdoc = DocumentLayout(_sc_js)
_sc_jdoc.layout(600, 300)
_sc_jlist = layout_tree_to_list(_sc_jdoc, [])
_sc_jbox = next(o for o in _sc_jlist
                if isinstance(o, BlockLayout)
                and getattr(o.node, "attributes", {}).get("id") == "j")
_sc_jbox.node._ridx = 7
_sc_report = collect_scroll_state(_sc_jlist)
check("overflow: scroll state reported to JS is (top, left, sh, sw)",
      _sc_report == [(7, 0.0, 0.0, 500.0, 200.0)], repr(_sc_report))
check("overflow: a JS scrollTop write moves the real scroller",
      _sc_apply(_sc_jlist, [(7, 120.0, 0.0)])[0]
      and scroll_position(_sc_jbox)[0] == 120.0,
      repr(scroll_position(_sc_jbox)))
check("overflow: an out-of-range JS write clamps like a browser",
      _sc_apply(_sc_jlist, [(7, 10 ** 6, 0.0)])[0]
      and scroll_position(_sc_jbox)[0] == 400.0
      and collect_scroll_state(_sc_jlist)[0][1] == 400.0,
      repr(scroll_position(_sc_jbox)))
check("overflow: a write to a non-scroller is ignored",
      _sc_apply(_sc_jlist, [(999, 50.0, 0.0)]) == (False, None))

# scrollTo / scrollBy / scrollIntoView / window.scrollTo: the VM queues
# them, the host replays them in the order the page asked
_sc_mk = _styled("body { margin: 0 }", """<body>
<div id=m style="overflow:auto;height:100px;width:200px">
  <div style="height:500px">
    <p id=far style="margin-top:300px">far</p></div></div>
<div style="height:2000px">pad</div>
<p id=below>below the fold</p></body>""")
_sc_mdoc = DocumentLayout(_sc_mk)
_sc_mdoc.layout(600, 300)
_sc_mlist = layout_tree_to_list(_sc_mdoc, [])
_sc_mbox = next(o for o in _sc_mlist
                if isinstance(o, BlockLayout)
                and getattr(o.node, "attributes", {}).get("id") == "m")
_sc_far = next(o for o in _sc_mlist
               if getattr(getattr(o, "node", None), "attributes", {})
               .get("id") == "far")
_sc_below = next(o for o in _sc_mlist
                 if getattr(getattr(o, "node", None), "attributes", {})
                 .get("id") == "below")
_sc_mbox.node._ridx = 11
_sc_far.node._ridx = 12
_sc_below.node._ridx = 13

check("overflow: window state is reported under the page sentinel",
      collect_scroll_state(_sc_mlist, (40.0, 0.0, 3000.0, 600.0))[0]
      == (PAGE_SCROLL_NODE, 40.0, 0.0, 3000.0, 600.0),
      repr(collect_scroll_state(_sc_mlist, (40.0, 0.0, 3000.0, 600.0))[0]))
check("overflow: an absolute scrollTo write lands at that position",
      apply_scroll_requests(
          _sc_mlist, [(11, 60.0, 0.0, 1, False)], [], 300, 0.0)[0]
      and scroll_position(_sc_mbox)[0] == 60.0,
      repr(scroll_position(_sc_mbox)))
check("overflow: a relative scrollBy write is applied as a delta",
      apply_scroll_requests(
          _sc_mlist, [(11, 25.0, 0.0, 2, True)], [], 300, 0.0)[0]
      and scroll_position(_sc_mbox)[0] == 85.0,
      repr(scroll_position(_sc_mbox)))
# scrollIntoView reveals through the ancestor scroller, not the page
_sc_iv = apply_scroll_requests(_sc_mlist, [], [(12, 3)], 300, 0.0)
check("overflow: scrollIntoView scrolls the element's own container",
      _sc_iv[0] and scroll_position(_sc_mbox)[0] > 200.0,
      repr((_sc_iv, scroll_position(_sc_mbox))))
# a window request rides the same queue and is handed back, not applied
check("overflow: a window write is reported to the caller, not applied",
      apply_scroll_requests(
          _sc_mlist, [(PAGE_SCROLL_NODE, 500.0, 0.0, 4, False)],
          [], 300, 0.0)[1] == (500.0, 0.0))
check("overflow: window.scrollBy is relative to the live page scroll",
      apply_scroll_requests(
          _sc_mlist, [(PAGE_SCROLL_NODE, 10.0, 0.0, 5, True)],
          [], 300, 500.0)[1] == (510.0, 0.0))
check("overflow: a window scroll never goes negative",
      apply_scroll_requests(
          _sc_mlist, [(PAGE_SCROLL_NODE, -900.0, 0.0, 6, True)],
          [], 300, 20.0)[1] == (0.0, 0.0))
# a below-the-fold reveal moves the page scroller the caller owns
_sc_rev = apply_scroll_requests(_sc_mlist, [], [(13, 9)], 300, 0.0)[1]
check("overflow: revealing a below-the-fold element moves the page",
      _sc_rev is not None and _sc_rev[0] > 1800.0, repr(_sc_rev))
# the sticky-header idiom: reveal, then nudge back up by the header
_sc_ord = apply_scroll_requests(
    _sc_mlist, [(PAGE_SCROLL_NODE, -80.0, 0.0, 8, True)], [(13, 7)],
    300, 0.0)
check("overflow: a scrollBy after scrollIntoView offsets the reveal",
      _sc_ord[1] is not None
      and abs(_sc_ord[1][0] - (_sc_rev[0] - 80.0)) < 0.01,
      repr((_sc_ord[1], _sc_rev)))
# ...and the reverse order: the reveal is later, so the reveal wins
_sc_rev2 = apply_scroll_requests(
    _sc_mlist, [(PAGE_SCROLL_NODE, 0.0, 0.0, 10, False)], [(13, 11)],
    300, 0.0)
check("overflow: an earlier window write does not override a later reveal",
      _sc_rev2[1] is not None and abs(_sc_rev2[1][0] - _sc_rev[0]) < 0.01,
      repr((_sc_rev2[1], _sc_rev)))

if native.available():
    _sjp = Page()
    _sjp.goto("data:text/html," + (
        "<div id=sc style='overflow:auto;height:100px;width:200px'>"
        "<div style='height:500px'>x</div></div>"), settle=False)
    check("overflow: scrollTop reads 0 and is writable from page JS",
          _sjp.evaluate("document.getElementById('sc').scrollTop") == 0
          and _sjp.evaluate(
              "(function(){var e=document.getElementById('sc');"
              "e.scrollTop = 42; return e.scrollTop;})()") == 42,
          "scrollTop must round-trip within a turn")
    check("overflow: scrollTop writes reach the host as pending work",
          any(int(w[0]) >= 0 and w[1] == 42
              for w in _sjp._renderer.take_scroll_writes()),
          "the host must see the write it has to apply")
    # scrollTo/scrollBy/scrollIntoView queue for the host and stay
    # readable within the turn; scrollBy travels as a delta
    _sjp.evaluate(
        "(function(){var e=document.getElementById('sc');"
        "e.scrollTo(0, 60); e.scrollBy({top: 25}); return 0;})()")
    _sj_w = [tuple(w) for w in _sjp._renderer.take_scroll_writes()]
    check("overflow: scrollTo is absolute and scrollBy is a delta",
          [(w[1], w[4]) for w in _sj_w] == [(60.0, False), (25.0, True)],
          repr(_sj_w))
    check("overflow: scrollBy still reads back as the summed position",
          _sjp.evaluate(
              "document.getElementById('sc').scrollTop") == 85,
          "the optimistic read must include the delta")
    check("overflow: queued writes carry a rising sequence",
          len(_sj_w) == 2 and _sj_w[1][3] > _sj_w[0][3], repr(_sj_w))
    _sjp.evaluate(
        "(function(){document.getElementById('sc').scrollIntoView();"
        "return 0;})()")
    check("overflow: scrollIntoView reaches the host as (node, seq)",
          [len(tuple(v)) for v in _sjp._renderer.take_scroll_into_view()]
          == [2],
          "resolving it needs layout, so it is a host request")
    _sjp.evaluate("window.scrollTo(0, 400)")
    check("overflow: window.scrollTo updates scrollY synchronously",
          _sjp.evaluate("window.scrollY") == 400
          and _sjp.evaluate("window.pageYOffset") == 400,
          "window scrolling is synchronous in a real browser")
    check("overflow: a window scroll queues under the page sentinel",
          [(int(w[0]), w[1]) for w in _sjp._renderer.take_scroll_writes()]
          == [(PAGE_SCROLL_NODE, 400.0)],
          "the shell owns the page scroller")
    _sjp.close()
else:
    print("[SKIP] scrollTop driver checks - native ggcore not built")

check("overflow: a nested clip rect scrolls with its content",
      _sc_clip_before and _sc_clip_after
      and abs((_sc_clip_before[0].clip_top - _sc_clip_after[0].clip_top)
              - 30.0) < 0.01,
      repr((_sc_clip_before[0].clip_top, _sc_clip_after[0].clip_top)))

print(f"\n{passed} checks passed - engine pipeline OK")
