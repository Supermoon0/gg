"""Accessibility tree: roles, accessible names, states, landmarks.

Builds a hierarchical accessibility projection of a styled DOM tree —
the structure a screen reader (or an agent) consumes. Works on the
Python node tree from either style engine, so the native and fallback
paths produce the same projection.

- Roles: explicit role attribute wins; otherwise the HTML-AAM implicit
  role (link, button, textbox, heading, landmarks, list/table
  structure, img, ...). role=presentation/none removes the node but
  keeps its subtree.
- Accessible name (WAI-ARIA AccName subset, in priority order):
  aria-labelledby -> aria-label -> native markup (alt, <label for>/
  wrapping label, value for button inputs, <caption>, <legend>,
  placeholder) -> title -> content text for name-from-content roles.
- Hidden state: display:none / visibility:hidden / hidden attribute /
  aria-hidden=true prune the subtree (like the platform trees do).
- States: checked/selected/expanded/disabled/required/readonly/
  pressed/value/level, plus focusable/focused so keyboard behavior
  (browser/keyboard.py) can be validated against the tree.

The Rust snapshot() (ggcore/src/lib.rs) mirrors the role map and the
name priority for its flat fast path; this module is the full tree.
"""

import re

from . import keyboard
from .html_parser import Element, Text, tree_to_list

_HEADINGS = {"h1": 1, "h2": 2, "h3": 3, "h4": 4, "h5": 5, "h6": 6}

LANDMARK_ROLES = {
    "banner", "complementary", "contentinfo", "form", "main",
    "navigation", "region", "search",
}

# roles whose accessible name may come from their contents
_NAME_FROM_CONTENT = {
    "button", "link", "heading", "cell", "columnheader", "rowheader",
    "option", "listitem", "checkbox", "radio", "menuitem", "tab",
    "caption", "switch",
}

_SECTIONING = {"article", "aside", "main", "nav", "section"}

NAME_MAX = 120


# ---------------------------------------------------------------------
# per-node predicates
# ---------------------------------------------------------------------


def is_hidden(node):
    """Whether this node (not its ancestors) is accessibility-hidden."""
    if not isinstance(node, Element):
        return False
    style = getattr(node, "style", {}) or {}
    if style.get("display") == "none":
        return True
    if style.get("visibility", "").strip().casefold() in (
            "hidden", "collapse"):
        return True
    attrs = node.attributes
    if "hidden" in attrs:
        return True
    if attrs.get("aria-hidden", "").strip().casefold() == "true":
        return True
    if node.tag == "input" \
            and attrs.get("type", "").strip().casefold() == "hidden":
        return True
    return False


def _named(node):
    attrs = node.attributes
    return bool(attrs.get("aria-label", "").strip()
                or attrs.get("aria-labelledby", "").strip()
                or attrs.get("title", "").strip())


def _in_sectioning_content(node):
    cur = node.parent
    while cur is not None:
        if isinstance(cur, Element) and cur.tag in _SECTIONING:
            return True
        cur = cur.parent
    return False


def role_of(node):
    """Explicit role attribute (first token) or the implicit HTML-AAM
    role. None => no accessibility node of its own."""
    if not isinstance(node, Element):
        return None
    attrs = node.attributes
    explicit = attrs.get("role", "").split()
    if explicit:
        return explicit[0].casefold()
    tag = node.tag
    if tag == "a":
        return "link" if "href" in attrs else None
    if tag == "button":
        return "button"
    if tag == "input":
        itype = attrs.get("type", "text").strip().casefold()
        return {
            "checkbox": "checkbox", "radio": "radio",
            "submit": "button", "button": "button", "reset": "button",
            "image": "button", "range": "slider",
            "number": "spinbutton", "search": "searchbox",
            "hidden": None,
        }.get(itype, "textbox")
    if tag == "select":
        return "listbox" if "multiple" in attrs else "combobox"
    if tag == "option":
        return "option"
    if tag == "textarea":
        return "textbox"
    if tag in _HEADINGS:
        return "heading"
    if tag == "nav":
        return "navigation"
    if tag == "main":
        return "main"
    if tag == "aside":
        return "complementary"
    if tag == "header":
        return None if _in_sectioning_content(node) else "banner"
    if tag == "footer":
        return None if _in_sectioning_content(node) else "contentinfo"
    if tag == "form":
        # a form is a landmark only when it has an accessible name
        return "form" if _named(node) else None
    if tag == "section":
        return "region" if _named(node) else None
    if tag in ("ul", "ol"):
        return "list"
    if tag == "li":
        return "listitem"
    if tag == "table":
        return "table"
    if tag == "tr":
        return "row"
    if tag == "td":
        return "cell"
    if tag == "th":
        scope = attrs.get("scope", "").strip().casefold()
        return "rowheader" if scope == "row" else "columnheader"
    if tag == "caption":
        return "caption"
    if tag == "img":
        alt = attrs.get("alt")
        if alt is not None and not alt.strip():
            return None  # alt="" is decorative
        return "img"
    if tag == "article":
        return "article"
    if tag == "dialog":
        return "dialog"
    if tag == "summary":
        return "button"
    if tag == "fieldset":
        return "group"
    if tag == "progress":
        return "progressbar"
    return None


def heading_level(node):
    """1-6 for headings (aria-level wins over the tag), else None."""
    if not isinstance(node, Element):
        return None
    raw = node.attributes.get("aria-level")
    if raw:
        try:
            level = int(raw.strip())
            if level >= 1:
                return level
        except ValueError:
            pass
    return _HEADINGS.get(node.tag)


# ---------------------------------------------------------------------
# accessible name computation
# ---------------------------------------------------------------------


def visible_text(node):
    """Concatenated descendant text, skipping hidden subtrees and
    substituting alt text for images."""
    out = []
    stack = [node]
    while stack:
        cur = stack.pop()
        if isinstance(cur, Text):
            out.append(cur.text)
            continue
        if is_hidden(cur):
            continue
        if isinstance(cur, Element) and cur.tag == "img":
            alt = cur.attributes.get("alt", "")
            if alt.strip():
                out.append(alt)
            continue
        stack.extend(reversed(cur.children))
    return _collapse(" ".join(out))


def _collapse(text):
    return re.sub(r"\s+", " ", text or "").strip()


class AXContext:
    """Per-document lookup tables the name computation needs."""

    def __init__(self, root):
        self.root = root
        self.by_id = {}
        self.label_for = {}
        for node in tree_to_list(root, []):
            if not isinstance(node, Element):
                continue
            node_id = node.attributes.get("id")
            if node_id and node_id not in self.by_id:
                self.by_id[node_id] = node
            if node.tag == "label":
                target = node.attributes.get("for")
                if target and target not in self.label_for:
                    self.label_for[target] = node

    def _wrapping_label(self, node):
        cur = node.parent
        while cur is not None:
            if isinstance(cur, Element) and cur.tag == "label":
                return cur
            cur = cur.parent
        return None

    def label_of(self, node):
        """The <label> associated with a form control, if any."""
        node_id = node.attributes.get("id")
        if node_id and node_id in self.label_for:
            return self.label_for[node_id]
        return self._wrapping_label(node)


def accessible_name(node, ctx, role=None):
    """WAI-ARIA accessible-name subset (see the module docstring for
    the priority order). Returns a whitespace-collapsed string."""
    if not isinstance(node, Element):
        return ""
    attrs = node.attributes
    role = role if role is not None else role_of(node)

    labelledby = attrs.get("aria-labelledby", "").split()
    if labelledby:
        parts = []
        for ref in labelledby:
            target = ctx.by_id.get(ref)
            if target is not None:
                parts.append(attrs_label_text(target))
        name = _collapse(" ".join(p for p in parts if p))
        if name:
            return name[:NAME_MAX]

    aria_label = _collapse(attrs.get("aria-label", ""))
    if aria_label:
        return aria_label[:NAME_MAX]

    # native markup
    tag = node.tag
    if tag in ("img", "area"):
        alt = _collapse(attrs.get("alt", ""))
        if alt:
            return alt[:NAME_MAX]
    if tag in ("input", "select", "textarea"):
        label = ctx.label_of(node)
        if label is not None:
            name = visible_text(label)
            if name:
                return name[:NAME_MAX]
        itype = attrs.get("type", "text").strip().casefold()
        if tag == "input" and itype in (
                "submit", "button", "reset", "image"):
            value = _collapse(attrs.get("value", ""))
            if value:
                return value[:NAME_MAX]
        placeholder = _collapse(attrs.get("placeholder", ""))
        if placeholder:
            return placeholder[:NAME_MAX]
    if tag == "table":
        caption = next(
            (c for c in node.children
             if isinstance(c, Element) and c.tag == "caption"), None)
        if caption is not None:
            name = visible_text(caption)
            if name:
                return name[:NAME_MAX]
    if tag == "fieldset":
        legend = next(
            (c for c in node.children
             if isinstance(c, Element) and c.tag == "legend"), None)
        if legend is not None:
            name = visible_text(legend)
            if name:
                return name[:NAME_MAX]

    if role in _NAME_FROM_CONTENT:
        name = visible_text(node)
        if name:
            return name[:NAME_MAX]

    return _collapse(attrs.get("title", ""))[:NAME_MAX]


def attrs_label_text(node):
    """Text alternative of an aria-labelledby target: its own
    aria-label, else its visible text (even when the target itself is
    hidden — labelledby may reference hidden nodes per spec, so only
    the reference's *descendant* hidden rules apply)."""
    label = _collapse(node.attributes.get("aria-label", ""))
    if label:
        return label
    if is_hidden(node):
        # referenced-but-hidden: take raw descendant text
        return _collapse("".join(
            n.text for n in tree_to_list(node, [])
            if isinstance(n, Text)))
    return visible_text(node)


def description_of(node, ctx):
    """aria-describedby -> title (when the title was not the name)."""
    attrs = node.attributes
    refs = attrs.get("aria-describedby", "").split()
    parts = []
    for ref in refs:
        target = ctx.by_id.get(ref)
        if target is not None:
            parts.append(attrs_label_text(target))
    return _collapse(" ".join(p for p in parts if p))[:NAME_MAX]


# ---------------------------------------------------------------------
# states
# ---------------------------------------------------------------------


def _tristate(value):
    v = (value or "").strip().casefold()
    if v == "true":
        return True
    if v == "false":
        return False
    if v == "mixed":
        return "mixed"
    return None


def states_of(node, role):
    """Exposed state/property map. Only meaningful keys are present."""
    attrs = node.attributes
    out = {}
    if role in ("checkbox", "radio", "switch", "menuitemcheckbox"):
        aria = _tristate(attrs.get("aria-checked"))
        out["checked"] = ("checked" in attrs) if aria is None else aria
    if role == "option":
        aria = _tristate(attrs.get("aria-selected"))
        out["selected"] = ("selected" in attrs) if aria is None else aria
    elif "aria-selected" in attrs:
        out["selected"] = _tristate(attrs.get("aria-selected"))
    if "aria-expanded" in attrs:
        out["expanded"] = _tristate(attrs.get("aria-expanded"))
    elif node.tag == "details":
        out["expanded"] = "open" in attrs
    if "aria-pressed" in attrs:
        out["pressed"] = _tristate(attrs.get("aria-pressed"))
    if "disabled" in attrs \
            or _tristate(attrs.get("aria-disabled")) is True:
        out["disabled"] = True
    if "required" in attrs \
            or _tristate(attrs.get("aria-required")) is True:
        out["required"] = True
    if "readonly" in attrs \
            or _tristate(attrs.get("aria-readonly")) is True:
        out["readonly"] = True
    if role in ("textbox", "searchbox", "spinbutton", "combobox"):
        value = attrs.get("value")
        if value:
            out["value"] = value[:NAME_MAX]
    if role in ("slider", "progressbar", "spinbutton"):
        for aria_attr, key in (("aria-valuenow", "value"),
                               ("aria-valuemin", "valuemin"),
                               ("aria-valuemax", "valuemax")):
            raw = attrs.get(aria_attr)
            if raw is None and aria_attr == "aria-valuenow":
                raw = attrs.get("value")
            if raw is not None:
                out[key] = raw
    level = heading_level(node) if role == "heading" else None
    if level is not None:
        out["level"] = level
    return out


# ---------------------------------------------------------------------
# the tree
# ---------------------------------------------------------------------


class AXNode:
    __slots__ = ("role", "name", "description", "states", "tag",
                 "ridx", "focusable", "focused", "children", "node")

    def __init__(self, role, name, description, states, tag, ridx,
                 focusable, focused, node):
        self.role = role
        self.name = name
        self.description = description
        self.states = states
        self.tag = tag
        self.ridx = ridx
        self.focusable = focusable
        self.focused = focused
        self.children = []
        self.node = node

    def to_dict(self):
        out = {"role": self.role, "name": self.name}
        if self.description:
            out["description"] = self.description
        if self.states:
            out["states"] = dict(self.states)
        if self.ridx is not None:
            out["ridx"] = self.ridx
        if self.focusable:
            out["focusable"] = True
        if self.focused:
            out["focused"] = True
        if self.children:
            out["children"] = [c.to_dict() for c in self.children]
        return out

    def __repr__(self):
        return f"<ax {self.role} {self.name!r} ({len(self.children)})>"


def build_tree(root):
    """The document's accessibility tree. The returned root has role
    'document' and its name is the page title."""
    ctx = AXContext(root)
    title = ""
    for node in tree_to_list(root, []):
        if isinstance(node, Element) and node.tag == "title":
            title = _collapse("".join(
                c.text for c in node.children if isinstance(c, Text)))
            break
    doc = AXNode("document", title[:NAME_MAX], "", {}, "html",
                 getattr(root, "_ridx", None), False, False, root)
    _build_children(root, doc, ctx)
    return doc


def _build_children(node, ax_parent, ctx):
    for child in node.children:
        if not isinstance(child, Element):
            continue
        if is_hidden(child):
            continue
        explicit = child.attributes.get("role", "").split()
        if explicit and explicit[0].casefold() in ("presentation", "none"):
            _build_children(child, ax_parent, ctx)
            continue
        role = role_of(child)
        if role is None:
            _build_children(child, ax_parent, ctx)
            continue
        ax = AXNode(
            role,
            accessible_name(child, ctx, role),
            description_of(child, ctx),
            states_of(child, role),
            child.tag,
            getattr(child, "_ridx", None),
            keyboard.is_focusable(child, sequential=False),
            bool(getattr(child, "is_focused", False)),
            child)
        ax_parent.children.append(ax)
        _build_children(child, ax, ctx)


def flatten(ax, out=None, depth=0):
    """Depth-first (node, depth) list over an AX tree."""
    if out is None:
        out = []
    out.append((ax, depth))
    for child in ax.children:
        flatten(child, out, depth + 1)
    return out


def landmarks(ax):
    """The page's landmark skeleton: (role, name, depth) rows in
    document order — what rotor/landmark navigation exposes."""
    return [(node.role, node.name, depth)
            for node, depth in flatten(ax)
            if node.role in LANDMARK_ROLES]


def headings(ax):
    """(level, name) outline rows in document order."""
    out = []
    for node, _depth in flatten(ax):
        if node.role == "heading":
            out.append((node.states.get("level", 2), node.name))
    return out
