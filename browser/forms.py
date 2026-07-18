"""Form interaction logic — shell-independent so it can be tested
headless. The tkinter/native shells own focus and key events; this
module owns what a submit means: walking to the owning <form>,
serializing its fields, and building the GET URL."""

from urllib.parse import quote_plus

from .html_parser import Element, tree_to_list

# input types that never contribute a field to the query
NON_SUBMITTING = {"submit", "button", "reset", "image", "file"}


def find_input(node):
    """The <input> at or above a clicked node (like find_link)."""
    while node is not None:
        if isinstance(node, Element) and node.tag == "input":
            return node
        node = node.parent
    return None


def find_form(node):
    """The <form> owning a node, or None."""
    while node is not None:
        if isinstance(node, Element) and node.tag == "form":
            return node
        node = node.parent
    return None


def form_pairs(form):
    """(name, value) pairs of the form's submittable fields, in
    document order. Checkboxes/radios count only when checked;
    <select>/<textarea> are out of scope for now."""
    pairs = []
    for node in tree_to_list(form, []):
        if not (isinstance(node, Element) and node.tag == "input"):
            continue
        name = node.attributes.get("name")
        if not name:
            continue
        itype = node.attributes.get("type", "text").lower()
        if itype in NON_SUBMITTING:
            continue
        if itype in ("checkbox", "radio") \
                and "checked" not in node.attributes:
            continue
        value = node.attributes.get("value", "")
        if itype in ("checkbox", "radio") and not value:
            value = "on"
        pairs.append((name, value))
    return pairs


def submit_href(form):
    """The href a GET submit navigates to (action + query), or None
    when the form can't be submitted this way (method=post)."""
    method = form.attributes.get("method", "get").lower()
    if method != "get":
        return None
    action = form.attributes.get("action", "")
    query = "&".join(
        f"{quote_plus(n)}={quote_plus(v)}" for n, v in form_pairs(form))
    if not query:
        return action or None
    # a GET submit replaces any query already on the action
    base = action.split("?", 1)[0]
    return f"{base}?{query}"
