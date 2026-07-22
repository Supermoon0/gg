"""Shared navigation jobs and session-history state for both GUI shells."""

from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
import threading

from . import net
from .html_parser import Element, tree_to_list


_STATE_ATTRS = {
    "input": ("value", "checked"),
    "textarea": ("value",),
    "select": (),
    "option": ("selected",),
}


@dataclass
class HistoryEntry:
    url: object
    body: str
    scroll: float = 0.0
    hscroll: float = 0.0
    form_state: tuple = field(default_factory=tuple)


@dataclass
class PendingNavigation:
    generation: int
    token: net.CancellationToken
    future: object
    context: object = None


class NavigationController:
    """Runs network work off the UI thread and discards cancelled jobs."""

    def __init__(self, workers=2):
        self._executor = ThreadPoolExecutor(
            max_workers=workers, thread_name_prefix="gg-navigation")
        self._lock = threading.Lock()
        self._generation = 0
        self._active = None

    @property
    def active(self):
        with self._lock:
            return self._active is not None

    def start(self, worker, context=None):
        self.cancel()
        token = net.CancellationToken()
        with self._lock:
            self._generation += 1
            generation = self._generation
            future = self._executor.submit(worker, token)
            pending = PendingNavigation(
                generation, token, future, context)
            self._active = pending
        return pending

    def cancel(self):
        with self._lock:
            pending = self._active
            self._active = None
            if pending is not None:
                self._generation += 1
        if pending is None:
            return False
        pending.token.cancel()
        pending.future.cancel()
        return True

    def take_ready(self):
        with self._lock:
            pending = self._active
            if pending is None or not pending.future.done():
                return None
            self._active = None
            return pending

    def shutdown(self):
        self.cancel()
        self._executor.shutdown(wait=False, cancel_futures=True)


def capture_form_state(root):
    """Snapshot live value/checked/selected state without retaining a DOM."""
    state = []
    ordinal = 0
    for node in tree_to_list(root, []) if root is not None else ():
        if not isinstance(node, Element) or node.tag not in _STATE_ATTRS:
            continue
        attrs = tuple(
            (name, (None if node.tag == "input" and name == "value"
                    and node.attributes.get("type", "").casefold() == "file"
                    else node.attributes[name]
                    if name in node.attributes else None))
            for name in _STATE_ATTRS[node.tag])
        state.append((ordinal, node.tag, attrs))
        ordinal += 1
    return tuple(state)


def restore_form_state(root, state, *, set_attr=None, remove_attr=None):
    """Restore a snapshot into a freshly parsed tree and optional native DOM."""
    controls = [node for node in tree_to_list(root, [])
                if isinstance(node, Element) and node.tag in _STATE_ATTRS]
    changed = False
    for ordinal, expected_tag, attrs in state or ():
        if ordinal >= len(controls):
            continue
        node = controls[ordinal]
        if node.tag != expected_tag:
            continue
        ridx = getattr(node, "_ridx", None)
        for name, value in attrs:
            old = node.attributes.get(name)
            old_present = name in node.attributes
            if value is None:
                node.attributes.pop(name, None)
                if ridx is not None and old_present and remove_attr is not None:
                    remove_attr(ridx, name)
                changed = changed or old_present
            else:
                node.attributes[name] = value
                if ridx is not None and (not old_present or old != value) \
                        and set_attr is not None:
                    set_attr(ridx, name, value)
                changed = changed or not old_present or old != value
    return changed
