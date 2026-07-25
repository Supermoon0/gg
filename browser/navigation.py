"""Shared navigation jobs and session-history state for both GUI shells."""

from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
import threading

from .html_parser import Element, tree_to_list
from .network_backend import default_network_backend


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
    # ((path, url, scroll), ...) for the frames this document embeds;
    # `path` is the frame's position in document order at each depth,
    # which survives the arena-index churn a DOM rebuild causes
    frame_state: tuple = field(default_factory=tuple)


@dataclass
class PendingNavigation:
    generation: int
    token: object
    future: object
    context: object = None


class NavigationController:
    """Runs network work off the UI thread and discards cancelled jobs."""

    def __init__(self, workers=2, network_backend=None):
        self._network = network_backend or default_network_backend()
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
        token = self._network.new_cancel_token()
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
        self._network.cancel(pending.token)
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


def capture_frame_state(manager, prefix=()):
    """Snapshot where every frame under `manager` is currently pointed.

    A frame navigation is a session-history entry of its own — pressing
    back after clicking a link *inside* a frame has to put that frame
    back, not reload the page — so the entry has to carry one row per
    frame, at every depth."""
    state = []
    if manager is None:
        return tuple(state)
    for ordinal, fd in enumerate(manager.ordered_frames()):
        path = prefix + (ordinal,)
        if fd.url is not None:
            state.append((path, str(fd.url), float(fd.scroll)))
        if fd.subframes is not None:
            state.extend(capture_frame_state(fd.subframes, path))
    return tuple(state)


def frame_state_with(state, path, url):
    """`state` with one frame's row repointed at `url`.

    Rows for frames *inside* the one that navigated are dropped: that
    document is being replaced, so its children no longer exist."""
    path = tuple(path)
    rows = [row for row in state or ()
            if row[0] != path and row[0][:len(path)] != path]
    rows.append((path, str(url), 0.0))
    return tuple(sorted(rows, key=lambda r: (len(r[0]), r[0])))


def restore_frame_state(manager, state):
    """Point every frame back where the entry says it was.

    Shallower paths replay first: navigating a parent frame rebuilds
    its subframes, so a deeper row would otherwise be applied to a
    frame that is about to be thrown away."""
    if manager is None or not state:
        return False
    moved = False
    for path, url, scroll in sorted(state, key=lambda r: (len(r[0]), r[0])):
        fd = manager.frame_at(path)
        if fd is None:
            continue
        if fd.url is None or str(fd.url) != url:
            fd.navigate(url, record=False)
            moved = True
        fd.scroll = float(scroll)
    return moved


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
