"""Sequential focus navigation and keyboard default-action rules."""

from .html_parser import Element, tree_to_list


_NATURALLY_FOCUSABLE = {"button", "input", "select", "textarea"}
_NON_TEXT_INPUT_TYPES = {
    "button", "checkbox", "color", "file", "hidden", "image", "radio",
    "range", "reset", "submit",
}
_SPACE_ACTIVATED_INPUT_TYPES = {
    "button", "checkbox", "file", "image", "radio", "reset", "submit",
}


def _tabindex(node):
    raw = node.attributes.get("tabindex")
    if raw is None:
        return None
    try:
        return int(raw.strip())
    except (AttributeError, ValueError):
        return 0


def _blocked(node):
    cur = node
    while cur is not None:
        if isinstance(cur, Element):
            if "hidden" in cur.attributes or "inert" in cur.attributes:
                return True
            style = getattr(cur, "style", {})
            if (style.get("display") == "none"
                    or style.get("visibility") == "hidden"):
                return True
        cur = cur.parent
    return False


def is_focusable(node, *, sequential=True):
    """Whether *node* can receive focus, optionally through Tab order."""
    if not isinstance(node, Element) or _blocked(node):
        return False
    attrs = node.attributes
    if "disabled" in attrs:
        return False
    if node.tag == "input" and attrs.get("type", "text").lower() == "hidden":
        return False

    index = _tabindex(node)
    if index is not None:
        return index >= 0 if sequential else True
    if node.tag == "a":
        return "href" in attrs
    if node.tag in _NATURALLY_FOCUSABLE:
        return True
    if node.tag == "summary":
        return True
    if "contenteditable" in attrs:
        return attrs.get("contenteditable", "").lower() != "false"
    return False


def focus_target(node):
    """Return the nearest click-focusable element at or above *node*."""
    while node is not None:
        if is_focusable(node, sequential=False):
            return node
        node = node.parent
    return None


def focus_order(root):
    """Return the HTML sequential focus order for a document tree."""
    positive = []
    normal = []
    for position, node in enumerate(tree_to_list(root, [])):
        if not is_focusable(node):
            continue
        index = _tabindex(node)
        if index is not None and index > 0:
            positive.append((index, position, node))
        else:
            normal.append(node)
    positive.sort(key=lambda item: (item[0], item[1]))
    return [item[2] for item in positive] + normal


def next_focus(root, current=None, *, reverse=False):
    """Move one step through sequential focus order, wrapping at an edge."""
    order = focus_order(root)
    if not order:
        return None
    try:
        position = next(i for i, node in enumerate(order)
                        if node is current
                        or (getattr(current, "_ridx", None) is not None
                            and getattr(node, "_ridx", None)
                            == getattr(current, "_ridx", None)))
    except StopIteration:
        return order[-1] if reverse else order[0]
    step = -1 if reverse else 1
    return order[(position + step) % len(order)]


def is_text_editable(node):
    if not isinstance(node, Element) or _blocked(node):
        return False
    if "disabled" in node.attributes or "readonly" in node.attributes:
        return False
    if node.tag == "textarea":
        return True
    if node.tag == "input":
        return node.attributes.get("type", "text").lower() \
            not in _NON_TEXT_INPUT_TYPES
    return False


def key_action(node, key):
    """Classify the browser default action for Enter, Space or an arrow.

    A focused <select> advances its selection: gg paints a closed
    control with no popup layer, so Enter/Space/Down move to the next
    option and Up to the previous one (see forms.cycle_selection)."""
    if not isinstance(node, Element) or "disabled" in node.attributes:
        return None
    if node.tag == "select":
        if key in ("Enter", "Space", "ArrowDown", "ArrowRight"):
            return "select-next"
        if key in ("ArrowUp", "ArrowLeft"):
            return "select-prev"
        return None
    if key == "Enter":
        if node.tag == "a" and "href" in node.attributes:
            return "activate"
        if node.tag == "button":
            return "activate"
        if node.tag == "input":
            itype = node.attributes.get("type", "text").lower()
            return "activate" if itype in _SPACE_ACTIVATED_INPUT_TYPES \
                else "submit"
        if node.tag == "textarea":
            return "newline"
    if key == "Space":
        if node.tag == "button":
            return "activate"
        if node.tag == "input" and \
                node.attributes.get("type", "text").lower() \
                in _SPACE_ACTIVATED_INPUT_TYPES:
            return "activate"
        if is_text_editable(node):
            return "text"
        return "scroll"
    return None
