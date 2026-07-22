"""Form interaction and serialization shared by every browser shell."""

from dataclasses import dataclass
import mimetypes
import os
import math
import re
import secrets
from urllib.parse import quote_plus, urlsplit

from .html_parser import Element, Text, tree_to_list


_FORM_CONTROL_TAGS = {"button", "input", "select", "textarea"}
_NO_VALIDATE_INPUT_TYPES = {"button", "hidden", "image", "reset", "submit"}


@dataclass(frozen=True)
class FormSubmission:
    method: str
    target: str
    body: bytes = b""
    content_type: str = ""

    @property
    def headers(self):
        return ({"Content-Type": self.content_type}
                if self.content_type else {})


@dataclass(frozen=True)
class FormActivation:
    kind: str
    submission: object = None
    logs: tuple = ()
    handled: bool = False
    prevented: bool = False
    invalid: tuple = ()
    changed: bool = False


def find_input(node):
    """The <input> at or above a clicked node (like find_link)."""
    while node is not None:
        if isinstance(node, Element) and node.tag == "input":
            return node
        node = node.parent
    return None


def find_submitter(node):
    """Return a clicked submit control, including text inside <button>."""
    while node is not None:
        if isinstance(node, Element):
            itype = node.attributes.get("type", "").lower()
            if node.tag == "button" and itype in ("", "submit"):
                return node
            if node.tag == "input" and itype in ("submit", "image"):
                return node
        node = node.parent
    return None


def find_resetter(node):
    """Return a clicked reset control, including text inside <button>."""
    while node is not None:
        if isinstance(node, Element):
            itype = node.attributes.get("type", "").lower()
            if node.tag == "button" and itype == "reset":
                return node
            if node.tag == "input" and itype == "reset":
                return node
        node = node.parent
    return None


def find_checkable(node):
    """Return a checkbox/radio input at or above an event target."""
    while node is not None:
        if (isinstance(node, Element) and node.tag == "input"
                and node.attributes.get("type", "text").lower()
                in ("checkbox", "radio")):
            return node
        node = node.parent
    return None


def _document_root(node):
    while node is not None and node.parent is not None:
        node = node.parent
    return node


def form_owner(control):
    """Resolve a form-associated element's owner, including ``form=id``."""
    if not isinstance(control, Element) \
            or control.tag not in _FORM_CONTROL_TAGS:
        return None
    if "form" in control.attributes:
        form_id = control.attributes.get("form", "")
        if not form_id:
            return None
        root = _document_root(control)
        return next((node for node in tree_to_list(root, [])
                     if isinstance(node, Element) and node.tag == "form"
                     and node.attributes.get("id") == form_id), None)
    node = control.parent
    while node is not None:
        if isinstance(node, Element) and node.tag == "form":
            return node
        node = node.parent
    return None


def find_form(node):
    """The owning form, honoring an associated control's ``form=id``."""
    while node is not None:
        if isinstance(node, Element):
            if node.tag == "form":
                return node
            if node.tag in _FORM_CONTROL_TAGS:
                return form_owner(node)
        node = node.parent
    return None


def attach_file(input_node, filename, data, content_type=None):
    """Attach a user-selected file to an input without trusting HTML value.

    Shells call this only after a file picker; markup cannot make the engine
    read an arbitrary local path. The bytes are retained until navigation.
    """
    if not (isinstance(input_node, Element)
            and input_node.tag == "input"
            and input_node.attributes.get("type", "").lower() == "file"):
        raise ValueError("attach_file requires <input type=file>")
    safe_name = os.path.basename(str(filename)).replace("\x00", "")
    mime = content_type or mimetypes.guess_type(safe_name)[0] \
        or "application/octet-stream"
    if not mime or any(ord(ch) < 0x20 or ord(ch) == 0x7f for ch in mime):
        raise ValueError("invalid file content type")
    input_node._selected_file = (safe_name, bytes(data), mime)
    input_node.attributes["value"] = safe_name


def _node_text(node):
    return "".join(
        child.text for child in tree_to_list(node, [])
        if isinstance(child, Text))


def _select_values(select):
    options = [node for node in tree_to_list(select, [])
               if isinstance(node, Element) and node.tag == "option"
               and "disabled" not in node.attributes]
    selected = [node for node in options
                if "selected" in node.attributes]
    if "multiple" not in select.attributes:
        selected = selected[:1] or options[:1]
    return [node.attributes.get("value", _node_text(node))
            for node in selected]


def _successful_controls(form, submitter=None):
    """Yield (name, value, selected_file) in document order."""
    root = _document_root(form)
    for node in tree_to_list(root, []):
        if not isinstance(node, Element) or "disabled" in node.attributes:
            continue
        if node.tag in _FORM_CONTROL_TAGS and form_owner(node) is not form:
            continue
        name = node.attributes.get("name")
        if not name:
            continue

        if node.tag == "textarea":
            value = node.attributes.get("value", _node_text(node))
            yield name, value, None
            continue

        if node.tag == "select":
            for value in _select_values(node):
                yield name, value, None
            continue

        if node.tag == "button":
            if node is submitter \
                    and node.attributes.get("type", "submit").lower() \
                    == "submit":
                yield name, node.attributes.get("value", ""), None
            continue

        if node.tag != "input":
            continue
        itype = node.attributes.get("type", "text").lower()
        if itype in ("button", "reset"):
            continue
        if itype in ("submit", "image"):
            if node is submitter:
                yield name, node.attributes.get("value", ""), None
            continue
        if itype in ("checkbox", "radio") \
                and "checked" not in node.attributes:
            continue
        if itype == "file":
            selected = getattr(node, "_selected_file", None)
            if selected is not None:
                yield name, selected[0], selected
            continue
        value = node.attributes.get("value", "")
        if itype in ("checkbox", "radio") and not value:
            value = "on"
        yield name, value, None


def form_pairs(form, submitter=None):
    """Successful (name, value) pairs in document order."""
    return [(name, value) for name, value, _file in
            _successful_controls(form, submitter)]


def _control_value(node):
    if node.tag == "textarea":
        return node.attributes.get("value", _node_text(node))
    if node.tag == "select":
        values = _select_values(node)
        return values[0] if values else ""
    return node.attributes.get("value", "")


def _validation_message(control, form):
    """Return an empty string when valid, otherwise a concise reason."""
    if not isinstance(control, Element) \
            or control.tag not in ("input", "select", "textarea"):
        return ""
    attrs = control.attributes
    if "disabled" in attrs or "readonly" in attrs:
        return ""
    itype = attrs.get("type", "text").lower() \
        if control.tag == "input" else control.tag
    if itype in _NO_VALIDATE_INPUT_TYPES:
        return ""
    value = _control_value(control)

    if "required" in attrs:
        if itype == "checkbox" and "checked" not in attrs:
            return "Please check this box."
        if itype == "radio":
            name = attrs.get("name", "")
            root = _document_root(form)
            group = [node for node in tree_to_list(root, [])
                     if isinstance(node, Element) and node.tag == "input"
                     and node.attributes.get("type", "text").lower()
                     == "radio" and node.attributes.get("name", "") == name
                     and form_owner(node) is form
                     and "disabled" not in node.attributes]
            if not any("checked" in node.attributes for node in group):
                return "Please select an option."
        elif itype == "file":
            if getattr(control, "_selected_file", None) is None:
                return "Please select a file."
        elif value == "":
            return "Please fill out this field."

    if not value:
        return ""
    if itype == "email":
        values = [part.strip() for part in value.split(",")] \
            if "multiple" in attrs else [value]
        email_re = re.compile(r"^[^\s@]+@[^\s@]+\.[^\s@]+$")
        if any(not email_re.fullmatch(part) for part in values):
            return "Please enter an email address."
    elif itype == "url":
        parsed = urlsplit(value)
        if not parsed.scheme or not parsed.netloc:
            return "Please enter a URL."

    pattern = attrs.get("pattern")
    if pattern:
        try:
            if re.fullmatch(pattern, value) is None:
                return "Please match the requested format."
        except re.error:
            pass  # browsers ignore an invalid pattern attribute

    try:
        min_length = int(attrs.get("minlength", ""))
    except ValueError:
        min_length = -1
    try:
        max_length = int(attrs.get("maxlength", ""))
    except ValueError:
        max_length = -1
    if min_length >= 0 and len(value) < min_length:
        return f"Please lengthen this text to {min_length} characters."
    if max_length >= 0 and len(value) > max_length:
        return f"Please shorten this text to {max_length} characters."

    if itype in ("number", "range"):
        try:
            number = float(value)
            if not math.isfinite(number):
                raise ValueError
        except ValueError:
            return "Please enter a number."
        for name, too_far in (
                ("min", lambda limit: number < limit),
                ("max", lambda limit: number > limit)):
            try:
                limit = float(attrs.get(name, ""))
            except ValueError:
                continue
            if math.isfinite(limit) and too_far(limit):
                return f"Value must be {name} {limit:g}."
    return ""


def invalid_controls(form):
    """Return invalid owned controls in document order."""
    root = _document_root(form)
    return [node for node in tree_to_list(root, [])
            if isinstance(node, Element)
            and node.tag in ("input", "select", "textarea")
            and form_owner(node) is form
            and _validation_message(node, form)]


def validation_message(control):
    form = form_owner(control)
    return _validation_message(control, form) if form is not None else ""


def should_validate(form, submitter=None):
    return "novalidate" not in form.attributes and not (
        submitter is not None
        and "formnovalidate" in submitter.attributes)


def capture_defaults(root):
    """Snapshot mutable control state once, before page/user edits."""
    defaults = {}
    for node in tree_to_list(root, []):
        if not isinstance(node, Element) \
                or node.tag not in _FORM_CONTROL_TAGS | {"option"}:
            continue
        key = getattr(node, "_ridx", None)
        key = ("ridx", key) if key is not None else ("py", id(node))
        defaults[key] = {
            "value_present": "value" in node.attributes,
            "value": (_node_text(node) if node.tag == "textarea"
                      and "value" not in node.attributes
                      else node.attributes.get("value", "")),
            "checked": "checked" in node.attributes,
            "selected": "selected" in node.attributes,
        }
    return defaults


def _default_for(node, defaults):
    ridx = getattr(node, "_ridx", None)
    return defaults.get(("ridx", ridx) if ridx is not None
                        else ("py", id(node)))


def _set_state_attr(node, name, present, value, set_attr, remove_attr):
    ridx = getattr(node, "_ridx", None)
    if present:
        node.attributes[name] = value
        if ridx is not None and set_attr is not None:
            set_attr(ridx, name, value)
    else:
        node.attributes.pop(name, None)
        if ridx is not None and remove_attr is not None:
            remove_attr(ridx, name)


def reset_form(form, defaults, set_attr=None, remove_attr=None):
    """Restore owned controls after a non-cancelled reset event."""
    root = _document_root(form)
    for control in tree_to_list(root, []):
        if not isinstance(control, Element) \
                or control.tag not in _FORM_CONTROL_TAGS \
                or form_owner(control) is not form:
            continue
        default = _default_for(control, defaults)
        if default is None:
            continue
        if control.tag == "input":
            _set_state_attr(
                control, "value", default["value_present"],
                default["value"], set_attr, remove_attr)
            _set_state_attr(
                control, "checked", default["checked"], "",
                set_attr, remove_attr)
            if hasattr(control, "_selected_file"):
                del control._selected_file
        elif control.tag == "textarea":
            # The engine stores the live textarea value in a value attr.
            _set_state_attr(
                control, "value", True, default["value"],
                set_attr, remove_attr)
        elif control.tag == "select":
            for option in tree_to_list(control, []):
                if not isinstance(option, Element) or option.tag != "option":
                    continue
                option_default = _default_for(option, defaults)
                if option_default is not None:
                    _set_state_attr(
                        option, "selected", option_default["selected"], "",
                        set_attr, remove_attr)


def _node_by_ridx(root, ridx):
    return next((node for node in tree_to_list(root, [])
                 if getattr(node, "_ridx", None) == ridx), None)


def activate_control(target, document_url, defaults, *,
                     dispatch_event=None, refresh_tree=None,
                     set_attr=None, remove_attr=None):
    """Run a clicked submit/reset control's browser default-action steps.

    ``dispatch_event`` receives (ridx, type, bubbles, cancelable,
    submitter_ridx) and returns (logs, handled, prevented). ``refresh_tree``
    re-exports the native DOM after handlers so submission sees mutations.
    """
    checkable = find_checkable(target)
    resetter = find_resetter(target)
    submitter = find_submitter(target)
    control = checkable or resetter or submitter
    if control is None:
        return None
    form = find_form(control)
    form_ridx = getattr(form, "_ridx", None) if form is not None else None
    control_ridx = getattr(control, "_ridx", None)
    logs = []
    handled = False

    def fire(node, event_type, bubbles=True, submitter_idx=None,
             cancelable=True):
        nonlocal handled
        ridx = getattr(node, "_ridx", None)
        if dispatch_event is None or ridx is None:
            return False
        event_logs, event_handled, prevented = dispatch_event(
            ridx, event_type, bubbles, cancelable, submitter_idx)
        logs.extend(event_logs)
        handled = handled or event_handled
        return prevented

    if checkable is not None:
        attrs = checkable.attributes
        if "disabled" in attrs:
            return None
        itype = attrs.get("type", "text").lower()
        if itype == "checkbox":
            _set_state_attr(
                checkable, "checked", "checked" not in attrs, "",
                set_attr, remove_attr)
        elif "checked" not in attrs:
            name = attrs.get("name", "")
            if name:
                root = _document_root(checkable)
                for candidate in tree_to_list(root, []):
                    if (candidate is not checkable
                            and isinstance(candidate, Element)
                            and candidate.tag == "input"
                            and candidate.attributes.get(
                                "type", "text").lower() == "radio"
                            and candidate.attributes.get("name", "") == name
                            and form_owner(candidate) is form):
                        _set_state_attr(
                            candidate, "checked", False, "",
                            set_attr, remove_attr)
            _set_state_attr(
                checkable, "checked", True, "", set_attr, remove_attr)
        else:
            return FormActivation("check")
        fire(checkable, "input", True, None, False)
        fire(checkable, "change", True, None, False)
        return FormActivation(
            "check", logs=tuple(logs), handled=handled, changed=True)

    if form is None:
        return None

    if resetter is not None:
        prevented = fire(form, "reset", True, None)
        if prevented:
            return FormActivation(
                "reset", logs=tuple(logs), handled=handled,
                prevented=True)
        if refresh_tree is not None and form_ridx is not None:
            root = refresh_tree()
            form = _node_by_ridx(root, form_ridx) or form
        reset_form(
            form, defaults, set_attr=set_attr, remove_attr=remove_attr)
        return FormActivation(
            "reset", logs=tuple(logs), handled=handled, changed=True)

    return activate_submission(
        form, document_url, submitter=submitter,
        dispatch_event=dispatch_event, refresh_tree=refresh_tree)


def activate_submission(form, document_url, submitter=None, *,
                        dispatch_event=None, refresh_tree=None):
    """Validate, dispatch submit/invalid, then prepare navigation."""
    form_ridx = getattr(form, "_ridx", None)
    submitter_ridx = getattr(submitter, "_ridx", None)
    logs = []
    handled = False

    def fire(node, event_type, bubbles=True, submitter_idx=None):
        nonlocal handled
        ridx = getattr(node, "_ridx", None)
        if dispatch_event is None or ridx is None:
            return False
        event_logs, event_handled, prevented = dispatch_event(
            ridx, event_type, bubbles, True, submitter_idx)
        logs.extend(event_logs)
        handled = handled or event_handled
        return prevented

    if should_validate(form, submitter):
        invalid = invalid_controls(form)
        if invalid:
            for control in invalid:
                fire(control, "invalid", False, None)
            return FormActivation(
                "submit", logs=tuple(logs), handled=handled,
                invalid=tuple(
                    getattr(node, "_ridx", None)
                    if getattr(node, "_ridx", None) is not None else node
                    for node in invalid))

    prevented = fire(form, "submit", True, submitter_ridx)
    if prevented:
        return FormActivation(
            "submit", logs=tuple(logs), handled=handled,
            prevented=True)
    if refresh_tree is not None and form_ridx is not None:
        root = refresh_tree()
        form = _node_by_ridx(root, form_ridx) or form
        if submitter_ridx is not None:
            submitter = _node_by_ridx(root, submitter_ridx) or submitter
    return FormActivation(
        "submit",
        submission=prepare_submission(
            form, document_url, submitter=submitter),
        logs=tuple(logs), handled=handled)


def _escape_disposition(value):
    return str(value).replace("\r", "").replace("\n", "") \
        .replace("\\", "\\\\").replace('"', '\\"')


def _multipart_body(controls, boundary):
    out = bytearray()
    marker = boundary.encode("ascii")
    for name, value, selected_file in controls:
        out.extend(b"--" + marker + b"\r\n")
        safe_name = _escape_disposition(name)
        if selected_file is None:
            out.extend((f'Content-Disposition: form-data; name="{safe_name}"'
                        "\r\n\r\n").encode("utf-8"))
            out.extend(str(value).encode("utf-8"))
        else:
            filename, data, mime = selected_file
            safe_filename = _escape_disposition(filename)
            out.extend((f'Content-Disposition: form-data; name="{safe_name}"; '
                        f'filename="{safe_filename}"\r\n'
                        f"Content-Type: {mime}\r\n\r\n").encode("utf-8"))
            out.extend(data)
        out.extend(b"\r\n")
    out.extend(b"--" + marker + b"--\r\n")
    return bytes(out)


def prepare_submission(form, document_url=None, submitter=None, boundary=None):
    """Build the navigation target, body, and Content-Type for a form."""
    raw_method = ((submitter.attributes.get("formmethod")
                   if submitter is not None else None)
                  or form.attributes.get("method", "get")).strip().lower()
    method = "POST" if raw_method == "post" else "GET"
    action = ((submitter.attributes.get("formaction")
               if submitter is not None else None)
              or form.attributes.get("action", "")).strip()
    if not action and document_url is not None:
        action = str(document_url)
    action = action.split("#", 1)[0]
    controls = list(_successful_controls(form, submitter))

    if method == "GET":
        query = "&".join(
            f"{quote_plus(str(name))}={quote_plus(str(value))}"
            for name, value, _file in controls)
        target = action.split("?", 1)[0]
        if query:
            target += "?" + query
        return FormSubmission("GET", target)

    enctype = ((submitter.attributes.get("formenctype")
                if submitter is not None else None)
               or form.attributes.get("enctype")
               or form.attributes.get("encoding")
               or "application/x-www-form-urlencoded").lower()
    if enctype == "multipart/form-data":
        boundary = boundary or ("----GGBrowser" + secrets.token_hex(12))
        boundary_chars = "'()+_,-./:=?"
        if not boundary.isascii() or not boundary or not all(
                ch.isalnum() or ch in boundary_chars for ch in boundary):
            raise ValueError("invalid multipart boundary")
        body = _multipart_body(controls, boundary)
        content_type = f"multipart/form-data; boundary={boundary}"
    elif enctype == "text/plain":
        body = "".join(
            f"{name}={value}\r\n" for name, value, _file in controls
        ).encode("utf-8")
        content_type = "text/plain; charset=UTF-8"
    else:
        body = "&".join(
            f"{quote_plus(str(name))}={quote_plus(str(value))}"
            for name, value, _file in controls
        ).encode("ascii")
        content_type = "application/x-www-form-urlencoded"
    return FormSubmission("POST", action, body, content_type)


def submit_href(form):
    """Compatibility helper returning only a GET submission target."""
    submission = prepare_submission(form)
    return submission.target if submission.method == "GET" else None
