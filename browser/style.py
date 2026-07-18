"""Style computation: cascade, inheritance, and the UA stylesheet.

Rule matching uses hash-bucket indexing (the same technique real
engines use): rules are bucketed by their rightmost id/class/tag,
so each element only tests a handful of candidate rules instead
of every rule on the page.
"""

import math
import re

from .css_parser import (ClassSelector, CompoundSelector, CSSParser,
                         DescendantSelector, IdSelector, TagSelector,
                         node_classes)
from .html_parser import Element

INHERITED_PROPERTIES = {
    "font-size": "16px",
    "font-style": "normal",
    "font-weight": "normal",
    "font-family": "default",
    "color": "black",
    "text-align": "left",
    "white-space": "normal",
}

# The browser's built-in default styles.
DEFAULT_STYLE_SHEET = """
head, script, style, title, meta, link, template, noscript { display: none; }
input[type=hidden] { display: none; }
input { height: 1.5em; }

html { display: block; }
body { display: block; margin: 8px; }
div, section, article, header, footer, nav, aside, main, figure,
form, fieldset, table, address, dl, dd, dt, hr { display: block; }

h1 { display: block; font-size: 32px; font-weight: bold;
     margin-top: 21px; margin-bottom: 21px; }
h2 { display: block; font-size: 24px; font-weight: bold;
     margin-top: 20px; margin-bottom: 20px; }
h3 { display: block; font-size: 19px; font-weight: bold;
     margin-top: 18px; margin-bottom: 18px; }
h4, h5, h6 { display: block; font-weight: bold;
     margin-top: 16px; margin-bottom: 16px; }
p  { display: block; margin-top: 16px; margin-bottom: 16px; }

ul, ol { display: block; margin-top: 16px; margin-bottom: 16px;
         margin-left: 24px; }
li { display: block; margin-left: 16px; }

blockquote { display: block; margin-top: 16px; margin-bottom: 16px;
             margin-left: 40px; margin-right: 40px; color: #444444; }

pre { display: block; font-family: monospace; white-space: pre;
      background-color: #f2f2f2; padding: 8px;
      margin-top: 16px; margin-bottom: 16px; }
code { font-family: monospace; background-color: #f2f2f2; }

a { color: #1a0dab; text-decoration: underline; }
b, strong { font-weight: bold; }
i, em, cite, var { font-style: italic; }
small { font-size: 13px; }
big { font-size: 20px; }
h1 a, h2 a, h3 a { }

center { display: block; text-align: center; }
button { font-weight: bold; }
"""


def parse_px(value, default=0.0):
    try:
        out = float(value[:-2]) if value.endswith("px") else float(value)
    except (ValueError, AttributeError):
        return default
    # inf/NaN (e.g. font-size: 1e400px) would crash int() downstream
    return out if math.isfinite(out) else default


def px_str(value):
    """Canonical px string, identical to the native (Rust) core's output."""
    if not math.isfinite(value):
        return "0px"
    if value == int(value):
        return f"{int(value)}px"
    return f"{value!r}px"


def parse_size(value, percent_base=0.0, em_base=16.0):
    """Resolve a CSS length to px. Returns None for auto/unsupported."""
    if not value:
        return None
    v = value.strip().casefold()
    if v in ("auto", "none", "inherit", "initial", "unset",
             "min-content", "max-content", "fit-content"):
        return None
    try:
        if v.endswith("px"):
            out = float(v[:-2])
        elif v.endswith("rem"):
            out = float(v[:-3]) * 16.0
        elif v.endswith("em"):
            out = float(v[:-2]) * em_base
        elif v.endswith("%"):
            out = float(v[:-1]) / 100.0 * percent_base
        elif v.endswith("vw") or v.endswith("vh"):
            out = float(v[:-2]) / 100.0 * percent_base
        else:
            out = float(v)
    except ValueError:
        return None
    return out if math.isfinite(out) else None


# --- CSS custom properties (var) -------------------------------------

_VAR_RE = re.compile(r"(?<![\w-])var\(", re.IGNORECASE)
_VAR_MAX_DEPTH = 16


def _find_var(value, start):
    """Next var(...) in value: (open_idx, end_idx_past_paren, inner)."""
    m = _VAR_RE.search(value, start)
    if not m:
        return None
    depth = 1
    j = m.end()
    while j < len(value) and depth:
        if value[j] == "(":
            depth += 1
        elif value[j] == ")":
            depth -= 1
        j += 1
    if depth:
        return None  # unbalanced — treat the rest as plain text
    return m.start(), j, value[m.end():j - 1]


def _split_fallback(inner):
    """Split 'name' / 'name, fallback' at the first top-level comma."""
    depth = 0
    for k, ch in enumerate(inner):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        elif ch == "," and depth == 0:
            return inner[:k], inner[k + 1:]
    return inner, None


def resolve_var_refs(value, vars_map, depth=0):
    """Substitute every var() in value using vars_map. Returns None when
    the value is invalid at computed-value time (undefined variable with
    no fallback, or a reference cycle)."""
    if depth > _VAR_MAX_DEPTH:
        return None
    out = []
    i = 0
    while True:
        hit = _find_var(value, i)
        if hit is None:
            out.append(value[i:])
            break
        start, end, inner = hit
        out.append(value[i:start])
        name, fallback = _split_fallback(inner)
        sub = vars_map.get(name.strip().casefold())
        resolved = None
        if sub is not None and sub.strip():
            resolved = resolve_var_refs(sub, vars_map, depth + 1)
        if (resolved is None or not resolved.strip()) \
                and fallback is not None:
            resolved = resolve_var_refs(fallback.strip(), vars_map,
                                        depth + 1)
        if resolved is None or not resolved.strip():
            return None
        out.append(resolved)
        i = end
    return "".join(out)


def _bucket_key(selector):
    """Bucket a rule by the rightmost, most selective simple selector."""
    target = selector
    while isinstance(target, DescendantSelector):
        target = target.descendant
    parts = target.parts if isinstance(target, CompoundSelector) else [target]
    for p in parts:
        if isinstance(p, IdSelector):
            return "id", p.id
    for p in parts:
        if isinstance(p, ClassSelector):
            return "class", p.cls
    for p in parts:
        if isinstance(p, TagSelector):
            return "tag", p.tag
    return "universal", None


class RuleIndex:
    """Hash-bucket rule index. Rules must be pre-sorted by cascade
    priority; bucket entries keep (specificity, order) so candidates
    from different buckets re-merge in correct cascade order."""

    def __init__(self, rules):
        self.by_id = {}
        self.by_class = {}
        self.by_tag = {}
        self.universal = []
        for order, (sel, body) in enumerate(rules):
            kind, key = _bucket_key(sel)
            # entry key mirrors cascade_priority (origin, specificity)
            # so re-merged candidates keep correct cascade order
            entry = ((getattr(sel, "origin", 1), sel.specificity),
                     order, sel, body)
            if kind == "id":
                self.by_id.setdefault(key, []).append(entry)
            elif kind == "class":
                self.by_class.setdefault(key, []).append(entry)
            elif kind == "tag":
                self.by_tag.setdefault(key, []).append(entry)
            else:
                self.universal.append(entry)

    def matching(self, node):
        candidates = list(self.by_tag.get(node.tag, ()))
        for cls in node_classes(node):
            candidates += self.by_class.get(cls, ())
        node_id = node.attributes.get("id")
        if node_id:
            candidates += self.by_id.get(node_id, ())
        candidates += self.universal
        matched = [e for e in candidates if e[2].matches(node)]
        matched.sort(key=lambda e: (e[0], e[1]))
        return matched


def style(node, rules, parent_style=None):
    """Compute node.style for node and all descendants.

    `rules` is a cascade-sorted rule list or a prebuilt RuleIndex.
    """
    if not isinstance(rules, RuleIndex):
        rules = RuleIndex(rules)
    _style(node, rules, parent_style or INHERITED_PROPERTIES, {})


def _style(node, index, parent_style, parent_vars):
    node.style = {}
    node._font = None  # invalidate the per-node font cache (see layout)

    # 1. Inherited defaults
    for prop, default in INHERITED_PROPERTIES.items():
        node.style[prop] = parent_style.get(prop, default)

    # 2. Cascade via the rule index
    if isinstance(node, Element):
        for _spec, _order, _sel, body in index.matching(node):
            for prop, value in body.items():
                _apply(node.style, prop, value)
        # 3. Inline style attribute wins
        if "style" in node.attributes:
            pairs = CSSParser(node.attributes["style"]).body()
            for prop, value in pairs.items():
                _apply(node.style, prop, value)

    # 3.5 Custom properties: collect --* declarations into the inherited
    # variable scope (copy-on-write — nodes that define none share the
    # parent's map), then substitute var() references. A failed
    # substitution is "invalid at computed-value time": inherited
    # properties fall back to the parent's value, others are dropped.
    own = [p for p in node.style if p.startswith("--")]
    if own:
        vars_map = dict(parent_vars)
        for prop in own:
            vars_map[prop] = node.style.pop(prop)
    else:
        vars_map = parent_vars
    for prop in [p for p, v in node.style.items() if "var(" in v.lower()]:
        resolved = resolve_var_refs(node.style[prop], vars_map)
        if resolved is not None and resolved.strip():
            node.style[prop] = resolved.strip()
        elif prop in INHERITED_PROPERTIES:
            node.style[prop] = parent_style.get(
                prop, INHERITED_PROPERTIES[prop])
        else:
            del node.style[prop]

    # 4. Resolve relative font sizes against the parent
    fs = node.style["font-size"]
    parent_px = parse_px(parent_style.get("font-size", "16px"), 16.0)
    if fs.endswith("rem"):
        # must be checked before "em" (which is its suffix); rem is
        # relative to the root font-size (16px)
        try:
            node.style["font-size"] = px_str(16.0 * float(fs[:-3]))
        except ValueError:
            node.style["font-size"] = px_str(parent_px)
    elif fs.endswith("%"):
        try:
            node.style["font-size"] = px_str(
                parent_px * float(fs[:-1]) / 100)
        except ValueError:
            node.style["font-size"] = px_str(parent_px)
    elif fs.endswith("em"):
        try:
            node.style["font-size"] = px_str(parent_px * float(fs[:-2]))
        except ValueError:
            node.style["font-size"] = px_str(parent_px)
    elif not fs.endswith("px"):
        keywords = {
            "xx-small": 9, "x-small": 10, "small": 13, "medium": 16,
            "large": 18, "x-large": 24, "xx-large": 32,
        }
        node.style["font-size"] = px_str(keywords.get(fs, parent_px))

    for child in node.children:
        _style(child, index, node.style, vars_map)


def _expand_box(styles, prefix, value):
    parts = value.split()
    if len(parts) == 1:
        parts = parts * 4
    elif len(parts) == 2:
        parts = [parts[0], parts[1], parts[0], parts[1]]
    elif len(parts) == 3:
        parts = [parts[0], parts[1], parts[2], parts[1]]
    if len(parts) >= 4:
        styles[f"{prefix}-top"] = parts[0]
        styles[f"{prefix}-right"] = parts[1]
        styles[f"{prefix}-bottom"] = parts[2]
        styles[f"{prefix}-left"] = parts[3]


def _apply(styles, prop, value):
    """Apply one declaration, expanding simple shorthands."""
    value = value.strip()
    if prop == "margin":
        _expand_box(styles, "margin", value)
    elif prop == "padding":
        _expand_box(styles, "padding", value)
    elif prop == "border":
        # "1px solid #ccc" in any order; "none"/"0" disables
        width = None
        color = None
        for part in value.split():
            p = part.casefold()
            if p in ("none", "hidden"):
                width = 0.0
            elif p in ("solid", "dashed", "dotted", "double",
                       "groove", "ridge", "inset", "outset"):
                continue
            elif parse_size(p) is not None:
                width = parse_size(p)
            else:
                color = part
        if width is not None:
            styles["border-width"] = px_str(width)
        elif "border-width" not in styles:
            styles["border-width"] = "1px"
        if color:
            styles["border-color"] = color
    elif prop == "flex":
        # flex: none | <grow> <shrink>? <basis>?
        v = value.casefold()
        if v == "none":
            styles["flex-grow"] = "0"
            styles["flex-shrink"] = "0"
            styles["flex-basis"] = "auto"
        else:
            nums = []
            basis = None
            for part in value.split():
                try:
                    nums.append(float(part))
                except ValueError:
                    basis = part
            if nums:
                styles["flex-grow"] = str(nums[0])
            if len(nums) > 1:
                styles["flex-shrink"] = str(nums[1])
            if basis is not None:
                styles["flex-basis"] = basis
            elif nums:
                # "flex: 1" means basis 0 per spec
                styles["flex-basis"] = "0"
    elif prop == "font":
        # Too complex to fully parse; ignore rather than misrender.
        pass
    else:
        styles[prop] = value


def cascade_priority(rule):
    selector, body = rule
    # Origin outranks specificity (CSS cascade): any author rule beats
    # any UA rule, so `* { margin: 0 }` resets can override UA styles.
    return (getattr(selector, "origin", 1), selector.specificity)


def default_rules():
    rules = CSSParser(DEFAULT_STYLE_SHEET).parse()
    for sel, _ in rules:
        sel.origin = 0  # user-agent origin
    return rules
