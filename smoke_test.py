"""Headless smoke test for the GG engine pipeline."""

import os
import tkinter

from browser import net
from browser.html_parser import Element, HTMLParser, Text, tree_to_list
from browser.css_parser import CSSParser
from browser.style import (RuleIndex, cascade_priority, default_rules,
                           style)
from browser.layout import (HSTEP, VSTEP, BlockLayout, DocumentLayout,
                            ImageLayout, layout_tree_to_list, paint_tree)
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
check("@font-face picks the first loadable source (woff2 now decodes)",
      ("my face", False, False, "fonts/a.woff2") in _faces, str(_faces))
check("@font-face numeric weight maps to bold",
      ("iconfont", True, False, "icons.otf") in _faces, str(_faces))
check("woff2-only faces load since the Rust decoder landed",
      any(f[0] == "woffonly" and f[3] == "x.woff2" for f in _faces),
      str(_faces))
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
import os
from browser import native
if native.async_available():
    os.environ["GGJS"] = "1"
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
check("POST form yields no GET href", _forms.submit_href(_form) is None)

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

# --- P2 sweep: margin collapse / table / grid / inline-block /
#     overflow:auto ---
P2_PAGE = """<html><body style="margin: 0">
<div id=mcw>
  <div id=mc1 style="margin:0; margin-bottom:30px; height:20px">a</div>
  <div id=mc2 style="margin:0; margin-top:10px; height:20px">b</div>
</div>
<table id=tb style="width:300px">
  <tr><td id=c11>a</td><td id=c12>b</td><td id=c13>c</td></tr>
  <tr><td id=c21 colspan=2>wide</td><td id=c22>d</td></tr>
</table>
<div id=gr style="display:grid; width:400px; gap:10px;
     grid-template-columns: 100px 1fr 1fr">
  <div id=g1 style="height:30px">1</div>
  <div id=g2 style="height:40px">2</div>
  <div id=g3 style="height:10px">3</div>
  <div id=g4 style="height:20px">4</div>
</div>
<div id=para style="width:400px"><span id=ib
  style="display:inline-block; width:120px; height:40px">box</span> tail</div>
<div id=ovf style="overflow:auto; height:30px">
  <div id=ovin style="height:200px">tall</div>
</div>
</body></html>"""
p2_dom = HTMLParser(P2_PAGE).parse()
style(p2_dom, sorted(ua, key=cascade_priority))
p2_doc = DocumentLayout(p2_dom)
p2_doc.layout(800)
p2 = {}
for b in layout_tree_to_list(p2_doc, []):
    if isinstance(b, BlockLayout) and isinstance(b.node, Element):
        node_id = b.node.attributes.get("id")
        if node_id:
            p2[node_id] = b
check("sibling margins collapse to the larger one",
      abs(p2["mc2"].y - (p2["mc1"].y + 20 + 30)) < 1,
      f"mc2.y={p2['mc2'].y:.0f} expected {p2['mc1'].y + 50:.0f}")
check("table: similar content gives near-equal columns summing to "
      "the table width",
      abs(p2["c12"].x - p2["c11"].x - 100) < 15
      and abs(p2["c13"].x - p2["c11"].x - 200) < 15
      and abs((p2["c13"].x + p2["c13"].outer_width())
              - (p2["c11"].x + 300)) < 2,
      f"dx12={p2['c12'].x - p2['c11'].x:.0f} "
      f"dx13={p2['c13'].x - p2['c11'].x:.0f}")
check("table: second row sits below the first",
      p2["c21"].y > p2["c11"].y,
      f"r1={p2['c11'].y:.0f} r2={p2['c21'].y:.0f}")
check("table: colspan=2 cell spans the first two columns exactly",
      abs((p2["c22"].x - p2["c21"].x)
          - (p2["c13"].x - p2["c11"].x)) < 1,
      f"span2={p2['c22'].x - p2['c21'].x:.0f} "
      f"col01={p2['c13'].x - p2['c11'].x:.0f}")
check("grid: px track then fr tracks share the rest minus gaps",
      abs(p2["g2"].x - p2["g1"].x - 110) < 1
      and abs(p2["g3"].x - p2["g1"].x - 260) < 1,
      f"g2dx={p2['g2'].x - p2['g1'].x:.0f} "
      f"g3dx={p2['g3'].x - p2['g1'].x:.0f}")
check("grid: second row drops by tallest item + row gap",
      abs(p2["g4"].y - (p2["g1"].y + 40 + 10)) < 1
      and abs(p2["g4"].x - p2["g1"].x) < 1,
      f"g4.y={p2['g4'].y:.0f} expected {p2['g1'].y + 50:.0f}")
check("inline-block: atomic box keeps its specified size",
      abs(p2["ib"].outer_width() - 120) < 1
      and abs(p2["ib"].height - 40) < 1,
      f"w={p2['ib'].outer_width():.0f} h={p2['ib'].height:.0f}")
check("inline-block: the line grows to the box height",
      p2["para"].height >= 40 - 1, f"h={p2['para'].height:.0f}")
check("overflow:auto clips like hidden (v1)",
      p2["ovf"]._clips() and abs(p2["ovf"].height - 30) < 8,
      f"clips={p2['ovf']._clips()} h={p2['ovf'].height:.0f}")

# --- layout v2: auto table columns / grid span / form controls ---
V2_PAGE = """<html><body style="margin: 0">
<table id=at style="width:400px">
  <tr><td id=wide>a much much longer cell with plenty of text</td>
      <td id=slim>x</td></tr>
  <tr><td>second row long-ish content here</td><td>y</td></tr>
</table>
<div id=g2 style="display:grid; width:330px; gap:10px;
     grid-template-columns: 100px 100px 100px">
  <div id=sp2 style="grid-column: span 2; height:10px">wide</div>
  <div id=sp1 style="height:10px">one</div>
  <div id=nx style="height:10px">next row</div>
</div>
<form>
  <input id=cb type=checkbox checked>
  <input id=rd type=radio name=g checked>
  <select id=sel><option>첫째</option>
    <option selected>둘째 옵션</option></select>
</form>
</body></html>"""
v2_dom = HTMLParser(V2_PAGE).parse()
style(v2_dom, sorted(ua, key=cascade_priority))
v2_doc = DocumentLayout(v2_dom)
v2_doc.layout(800)
v2 = {}
for b in layout_tree_to_list(v2_doc, []):
    if isinstance(b, BlockLayout) and isinstance(b.node, Element):
        node_id = b.node.attributes.get("id")
        if node_id:
            v2[node_id] = b
check("table v2: columns share width by content, not equally",
      v2["wide"].outer_width() > v2["slim"].outer_width() * 2
      and abs((v2["wide"].outer_width() + v2["slim"].outer_width())
              - 400) < 2,
      f"wide={v2['wide'].outer_width():.0f} "
      f"slim={v2['slim'].outer_width():.0f}")
check("grid v2: span 2 item covers two tracks plus the gap",
      abs(v2["sp2"].outer_width() - 210) < 1
      and abs(v2["sp1"].x - v2["sp2"].x - 220) < 1,
      f"w={v2['sp2'].outer_width():.0f} dx={v2['sp1'].x - v2['sp2'].x:.0f}")
check("grid v2: full row wraps the next item",
      v2["nx"].y > v2["sp2"].y and abs(v2["nx"].x - v2["sp2"].x) < 1,
      f"nx=({v2['nx'].x:.0f},{v2['nx'].y:.0f})")
check("form controls: checkbox/radio get their 14px UA box",
      abs(v2["cb"].width - 14) < 1 and abs(v2["rd"].width - 14) < 1,
      f"cb={v2['cb'].width:.0f}")
_cbc = v2["cb"].paint()
check("form controls: checked checkbox paints mark strokes",
      sum(1 for c in _cbc if type(c).__name__ == "DrawLine") >= 2
      and any(getattr(c, "color", "") == "#1a73e8" for c in _cbc),
      str([type(c).__name__ for c in _cbc]))
_rdc = v2["rd"].paint()
check("form controls: checked radio paints the inner dot",
      sum(1 for c in _rdc if type(c).__name__ == "DrawOval") >= 3,
      str([type(c).__name__ for c in _rdc]))
_selc = v2["sel"].paint()
_seltexts = [c.text for c in _selc if hasattr(c, "text")]
check("form controls: select shows the selected option + arrow",
      any("둘째" in t for t in _seltexts)
      and any("▾" in t for t in _seltexts), str(_seltexts))
check("form controls: option lists are display:none",
      all(n.style.get("display") == "none"
          for n in tree_to_list(v2_dom, [])
          if isinstance(n, Element) and n.tag == "option"))

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

# --- cookie jar (network layer, exact host) ---
net.store_cookie("a.example", "sid=abc; Path=/; HttpOnly")
net.store_cookie("a.example", "theme=dark")
net.store_cookie("b.example", "other=1")
check("cookie jar: attributes stripped, host-scoped",
      net.cookie_header("a.example") == "sid=abc; theme=dark"
      and net.cookie_header("b.example") == "other=1",
      net.cookie_header("a.example"))
net.store_cookie("a.example", "sid=xyz")
check("cookie jar: same name upserts",
      net.cookie_header("a.example") == "sid=xyz; theme=dark",
      net.cookie_header("a.example"))
net.store_cookie("a.example", "theme=; Max-Age=0")
check("cookie jar: Max-Age=0 deletes",
      net.cookie_header("a.example") == "sid=xyz",
      net.cookie_header("a.example"))
net.store_cookie("a.example", "sec=1; Secure", scheme="http")
check("cookie jar: Secure over http is dropped",
      "sec" not in net.cookie_header("a.example"),
      net.cookie_header("a.example"))
check("cookie jar: unknown host sends nothing",
      net.cookie_header("nowhere.example") == "")

# --- T3: attribute-complete cookies ---
net.store_cookie("example.com", "dom=1; Domain=example.com")
net.store_cookie("example.com", "hostonly=1")
check("cookie T3: Domain cookie reaches subdomains, host-only doesn't",
      "dom=1" in net.cookie_header("www.example.com")
      and "hostonly" not in net.cookie_header("www.example.com")
      and "hostonly=1" in net.cookie_header("example.com"),
      net.cookie_header("www.example.com"))
net.store_cookie("example.com", "scoped=1; Path=/app")
check("cookie T3: Path bounds where a cookie is sent",
      "scoped=1" in net.cookie_header("example.com", path="/app/x")
      and "scoped" not in net.cookie_header("example.com",
                                            path="/other"),
      net.cookie_header("example.com", path="/app/x"))
net.store_cookie("example.com", "sec=1; Secure")
check("cookie T3: Secure cookie stays off plain http",
      "sec=1" in net.cookie_header("example.com", scheme="https")
      and "sec" not in net.cookie_header("example.com", scheme="http"))
net.store_cookie("example.com",
                 "old=1; Expires=Wed, 01 Jan 2020 00:00:00 GMT")
check("cookie T3: expired cookie is purged",
      "old" not in net.cookie_header("example.com"))
net.store_cookie("example.com", "secret=1; HttpOnly")
check("cookie T3: HttpOnly sent on the wire but hidden from JS",
      "secret=1" in net.cookie_header("example.com")
      and "secret" not in net.cookies_for("example.com"))
net.store_cookie("example.com", "ss=1; SameSite=Strict")
check("cookie T3: SameSite=Strict blocks a cross-site initiator",
      "ss=1" in net.cookie_header("example.com",
                                  initiator="sub.example.com")
      and "ss" not in net.cookie_header("example.com",
                                        initiator="evil.org"),
      net.cookie_header("example.com", initiator="evil.org"))
check("cookie T3: a host cannot set cookies for another domain",
      (net.store_cookie("evil.org", "steal=1; Domain=example.com")
       or "steal" not in net.cookie_header("example.com")))

# --- T3: CORS response check (fetch/XHR channel) ---
_pg = net.URL("https://app.example/index.html")
check("CORS: same-origin always allowed",
      net.cors_allows(_pg, net.URL("https://app.example/api"), {}))
check("CORS: cross-origin without ACAO is blocked",
      not net.cors_allows(_pg, net.URL("https://api.other/v1"), {}))
check("CORS: ACAO * allows",
      net.cors_allows(_pg, net.URL("https://api.other/v1"),
                      {"access-control-allow-origin": "*"}))
check("CORS: ACAO exact origin allows, mismatch blocks",
      net.cors_allows(_pg, net.URL("https://api.other/v1"),
                      {"access-control-allow-origin":
                       "https://app.example"})
      and not net.cors_allows(_pg, net.URL("https://api.other/v1"),
                              {"access-control-allow-origin":
                               "https://elsewhere.example"}))
check("CORS: file: pages read file: fixtures (same scheme)",
      net.cors_allows(net.URL("file:///a/index.html"),
                      net.URL("file:///a/dep.js"), {}))

# --- T3: POST form serialization ---
from browser import forms as _f3
_post_dom = _styled("", '<form method=post action="/login">'
                    '<input name=user value="kim">'
                    '<input name=pw type=password value="s3cret">'
                    '<input type=submit value=go></form>')
_pform = _find(_post_dom, "form")
_ppost = _f3.submit_post(_pform)
check("POST: urlencoded body from named fields (submit excluded)",
      _ppost == ("/login", "user=kim&pw=s3cret"), str(_ppost))
_get_dom = _styled("", '<form action="/s"><input name=q value=x></form>')
check("POST: GET forms return None from submit_post",
      _f3.submit_post(_find(_get_dom, "form")) is None)

# --- charset sniffing (legacy Korean sites: EUC-KR via <meta>) ---
check("charset: Content-Type header wins",
      net.sniff_charset(b"<html>", "text/html; charset=euc-kr")
      == "euc-kr")
check("charset: <meta charset> sniffed from the head",
      net.sniff_charset(
          b'<html><head><meta charset="EUC-KR"></head>', "text/html")
      == "EUC-KR")
check("charset: http-equiv content sniffed",
      net.sniff_charset(
          b'<meta http-equiv="Content-Type" '
          b'content="text/html; charset=euc-kr">', "")
      == "euc-kr")
check("charset: euc-kr bytes decode",
      "한글 텍스트".encode("euc-kr").decode(
          net.sniff_charset(b"<meta charset=euc-kr>", ""))
      == "한글 텍스트")
check("charset: default stays utf-8", net.sniff_charset(b"<html>", "")
      == "utf-8")

# --- ES module linker v1 (static import/export -> classic script) ---
from browser import esmodules

_MODS = {
    "https://x.example/util.js":
        "export const answer = 42;\n"
        "export function double(n) { return n * 2; }\n"
        "export default function () { return 'dflt'; }\n",
    "https://x.example/side.js":
        "window.__side = (window.__side || 0) + 1;\n",
}


def _mod_loader(spec, base):
    url = "https://x.example/" + spec.lstrip("./")
    return _MODS.get(url), url


_ENTRY = (
    "import dflt, { answer, double as twice } from './util.js';\n"
    "import './side.js';\n"
    "import './side.js';\n"
    "export const local = 1;\n"
    "var out = answer + twice(4) + dflt().length;\n")
_linked = esmodules.link(_ENTRY, None, _mod_loader)
check("module linker: no import/export statements survive",
      "import " not in _linked and "\nexport " not in _linked
      and "export const" not in _linked, _linked[:120])
check("module linker: dedup - side-effect dep inlined once",
      _linked.count("side.js'] =") == 1,
      f"registrations={_linked.count(chr(39) + '] =')}")
check("module linker: named/renamed/default reads wired",
      "var twice = " in _linked and ".double;" in _linked
      and ".default;" in _linked)

# --- Real network fetch (GG_SKIP_NET=1 for egress-limited CI) ---
if os.environ.get("GG_SKIP_NET") == "1":
    print("[SKIP] real-network checks - GG_SKIP_NET=1")
else:
    headers, body = net.request(net.URL("https://example.com"))
    check("HTTPS fetch example.com", "<html" in body.lower()
          and "example" in body.lower(), f"{len(body)} bytes")

    # redirect propagates the final URL
    # (http://google.com -> www.google.com)
    _h, _b, final = net.request_text(net.URL("http://google.com/"))
    check("redirect returns final URL", final.host != "google.com"
          and "google" in final.host, str(final))

headers, body = net.request(net.URL("about:home"))
check("about:home renders", "GG Browser" in body)

# --- Headless automation driver (needs the native wheel + JS) ---
# --- N3: partial refresh splices mutated subtrees only ---
if native.available():
    import os as _os
    _prev_ggjs = _os.environ.get("GGJS")
    _os.environ["GGJS"] = "1"
    try:
        n3_root, n3_doc, n3_css, _lg = native.load_document(
            "<html><body><div id=a><p id=p1>one</p></div>"
            "<div id=b><p id=p2>two</p></div></body></html>",
            lambda h: {}, lambda s: {})
    finally:
        if _prev_ggjs is None:
            _os.environ.pop("GGJS", None)
        else:
            _os.environ["GGJS"] = _prev_ggjs
    if hasattr(n3_doc, "take_mutated"):
        def _n3_find(root, nid):
            return next(n for n in tree_to_list(root, [])
                        if isinstance(n, Element)
                        and n.attributes.get("id") == nid)

        def _n3_dump(root):
            out = []
            for n in tree_to_list(root, []):
                if isinstance(n, Element):
                    out.append((n.tag, tuple(sorted(
                        n.attributes.items()))))
                else:
                    out.append(("#text", n.text))
            return out

        b_before = _n3_find(n3_root, "b")
        n3_doc.run_scripts([
            "document.getElementById('p1').textContent = 'changed';"
            "var s = document.createElement('span');"
            "s.id = 'newnode'; s.textContent = 'fresh';"
            "document.getElementById('a').appendChild(s);"])
        n3_root2 = native.refresh_partial(n3_doc, n3_css, n3_root)
        check("partial refresh: root object survives",
              n3_root2 is n3_root)
        check("partial refresh: untouched sibling is NOT re-marshaled",
              _n3_find(n3_root2, "b") is b_before)
        check("partial refresh: mutated text and new node arrive",
              "changed" in "".join(
                  t.text for t in tree_to_list(
                      _n3_find(n3_root2, "p1"), [])
                  if isinstance(t, Text))
              and _n3_find(n3_root2, "newnode").tag == "span")
        check("partial refresh: tree equals a full rebuild (golden)",
              _n3_dump(n3_root2) == _n3_dump(
                  native.build_tree(n3_doc.export())))
    else:
        print("[SKIP] partial refresh - wheel predates take_mutated")

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
    # linked module output actually runs in gg-js
    check("driver: linked ES module executes",
          dp.evaluate("(function () { " + _linked
                      + "; return out; })()") == 54,
          repr(dp.evaluate("(function () { " + _linked
                           + "; return out; })()")))

    # --- synthetic naver fixture: the full boot chain under one roof
    #     (N1 injection chain -> React mount -> EAGER-DATA feed ->
    #     stateful click). Skipped if the React bundles are absent.
    _FX = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                       "bench", "naver-fixture", "index.html")
    _REACT = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                          "bench", "js", "react",
                          "react.production.min.js")
    if os.path.exists(_FX) and os.path.exists(_REACT):
        fp = Page(engine="ggjs")
        fp.goto("file://" + _FX)
        check("fixture: 3-deep injected chain boots React",
              fp.evaluate("typeof React") == "object"
              and fp.evaluate("typeof ReactDOM") == "object",
              repr(fp.evaluate("typeof React")))
        feed = fp.text("#feed") or ""
        check("fixture: React renders the EAGER-DATA feed",
              "헤드라인 첫째 기사" in feed and "뉴스 헤드라인" in feed,
              feed[:60])
        check("fixture: headline count matches the JSON",
              len(fp.query_all(".headline")) == 3)
        fp.click(".headline")
        check("fixture: click -> setState -> re-render",
              "선택: 1" in (fp.text("#picked") or ""),
              repr(fp.text("#picked")))
    else:
        print("[SKIP] naver fixture - react bundles not present")
else:
    print("[SKIP] driver checks - native ggcore not built")

print(f"\n{passed} checks passed - engine pipeline OK")
