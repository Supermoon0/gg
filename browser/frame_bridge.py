"""Cross-document messaging between a page and its iframes.

Every document in this engine is a separate gg-js VM with its own DOM
arena, so `postMessage` is the *only* channel between them — and it is
deliberately a channel for bytes, not for object graphs. A sender
serializes to JSON inside its own VM; the host routes the text; the
receiver parses it into its own heap. There is no code path from one
document's value to another's, which is what makes cross-origin
isolation structural here rather than a policy check.

The host's job is the routing table and the origin policy:

* which context handle a `FRAME_PARENT` / `FRAME_TOP` / explicit handle
  resolves to (and refusing to resolve one belonging to another tab),
* what origin to stamp on the message — the *sender's*, which is what
  a receiver checks before trusting `e.data`,
* whether the sender's `targetOrigin` matches the receiver, dropping
  the message silently when it does not, exactly as the spec requires.
"""

from . import frames

# One turn's messaging is bounded so a pair of documents ping-ponging
# in their own handlers cannot wedge the frame that drives them.
MAX_MESSAGE_ROUNDS = 4
MAX_MESSAGE_BYTES = 128 * 1024
MAX_MESSAGE_QUEUE = 256

# The VM's context sentinels (ggcore/src/jsvm/vm.rs).
CTX_SELF = 0xFFFFFFFF
CTX_PARENT = 0xFFFFFFFE
CTX_TOP = 0xFFFFFFFD

# A same-origin parent may read the child's DOM through a *mirror*: the
# host serializes the child's arena and the parent rebuilds its own copy
# (see Page::set_frame_document). Big documents are not worth copying
# into every embedder on every change, so the mirror is capped.
MAX_MIRROR_NODES = 4000

# Mutation ops a mirror write can carry (vm.rs `framedom`).
OP_SET_TEXT = 8
OP_SET_ID = 9
OP_SET_CLASS = 10
OP_SET_VALUE = 11
OP_SET_ATTR = 13
OP_REMOVE_ATTR = 14
OP_CLICK = 19

# ops whose (a, b) is already an attribute name/value pair
_ATTR_OPS = {OP_SET_ID: "id", OP_SET_CLASS: "class", OP_SET_VALUE: "value"}


def effective_origin(fd):
    """A frame's origin for scripting purposes, or None when opaque.

    A sandboxed frame without `allow-same-origin` has an opaque origin
    even when it was served from the embedder's own host: that is the
    point of the token. `FrameDocument.load` only uses that flag to
    pick a cookie-less backend, so the check has to happen here too or
    a same-site sandboxed frame would read as same-origin.
    """
    if fd is None or fd.url is None:
        return None
    sandbox = getattr(fd, "sandbox", None)
    if sandbox is not None and "allow-same-origin" not in sandbox:
        return None
    return frames.origin_of(fd.url)


def origin_string(origin):
    """A schemeful origin tuple as JS spells it; "null" when opaque."""
    if origin is None:
        return "null"
    scheme, host, port = origin
    default = {"http": 80, "https": 443}.get(scheme)
    if port and port != default:
        return f"{scheme}://{host}:{port}"
    return f"{scheme}://{host}"


def scriptable(fd, owner_url):
    """Whether `owner_url`'s document may script this frame directly.

    Same-origin DOM access is a much larger surface than messaging;
    this is the gate for it, and it is intentionally stricter than
    `same_origin` alone.
    """
    return (fd is not None
            and fd.status == "loaded"
            and fd.session is not None
            and effective_origin(fd) is not None
            and frames.same_origin(fd.url, owner_url))


def target_origin_allows(target, sender_origin, receiver_origin):
    """The sender's `targetOrigin` argument vs the receiving document.

    "*" reaches anyone; "/" means "only a document of my own origin";
    anything else must match the receiver's origin exactly. A mismatch
    is dropped silently — the spec is explicit that the sender must not
    learn where the message did not go.
    """
    target = (target or "").strip()
    if target == "*":
        return True
    if target == "/":
        return (sender_origin is not None
                and sender_origin == receiver_origin)
    if not target:
        return False
    return target.casefold() == origin_string(receiver_origin).casefold()


class Context:
    """One browsing context in this tab: a document plus its edges."""

    __slots__ = ("token", "session", "frame", "parent", "console",
                 "url_getter")

    def __init__(self, token, session, frame=None, parent=0):
        self.token = token
        self.session = session
        self.frame = frame        # the FrameDocument, or None for the page
        self.parent = parent      # embedder's token; 0 for the top document
        self.console = None       # where handler output goes
        # the top document's URL lives on the shell, not on a
        # FrameDocument, so it is read through a callback
        self.url_getter = None

    @property
    def url(self):
        if self.frame is not None:
            return self.frame.url
        getter = self.url_getter
        return getter() if callable(getter) else None


class ContextTable:
    """The tab's routing table. Handles are minted here and nowhere
    else, so a document can only name a context the host gave it."""

    def __init__(self):
        self.by_token = {}
        self._next = 1
        self.top = None

    def register(self, session, frame=None, parent=0):
        token = self._next
        self._next += 1
        ctx = Context(token, session, frame, parent)
        self.by_token[token] = ctx
        if self.top is None:
            self.top = token
        return ctx

    def register_top(self, session, url_getter=None):
        """The page itself. Re-registering keeps token 1 stable so
        handles already handed to child documents stay valid."""
        ctx = self.by_token.get(self.top) if self.top else None
        if ctx is None:
            ctx = self.register(session)
        else:
            ctx.session = session
        if url_getter is not None:
            ctx.url_getter = url_getter
        return ctx

    def drop(self, token):
        self.by_token.pop(token, None)

    def resolve(self, sender, target):
        """The sentinel or handle a sender named -> a Context, or None.

        An unknown handle resolves to nothing: a document cannot reach
        a context it was never handed, and cannot probe for one either.
        """
        if target == CTX_SELF:
            return sender
        if target == CTX_PARENT:
            return self.by_token.get(sender.parent) or sender
        if target == CTX_TOP:
            return self.by_token.get(self.top)
        return self.by_token.get(target)

    def handle_for(self, receiver, sender):
        """How `receiver` should name `sender` in `e.source`, so that
        `e.source.postMessage(...)` routes back correctly."""
        if sender is receiver:
            return 0
        return sender.token


def _top_url(ctx):
    getter = ctx.url_getter
    return getter() if callable(getter) else None


def context_origin(ctx):
    """Sender origin for a context, handling the top document (whose
    URL lives on the shell, not on a FrameDocument)."""
    if ctx.frame is not None:
        return effective_origin(ctx.frame)
    return frames.origin_of(_top_url(ctx))


def pump(table, log=None):
    """Route one turn's `postMessage` traffic. Returns True when any
    message was delivered (the caller repaints/settles on that)."""
    if table is None:
        return False
    delivered = False
    for _round in range(MAX_MESSAGE_ROUNDS):
        pending = []
        for ctx in list(table.by_token.values()):
            session = ctx.session
            if session is None:
                continue
            try:
                writes = session.take_frame_writes()
            except Exception:
                continue
            for write in writes or []:
                pending.append((write[3], ctx, write))
        if not pending:
            break
        pending.sort(key=lambda e: e[0])
        moved = False
        for _seq, sender, write in pending[:MAX_MESSAGE_QUEUE]:
            target, payload, target_origin = (
                int(write[0]), str(write[1]), str(write[2]))
            if len(payload) > MAX_MESSAGE_BYTES:
                if log is not None:
                    log(sender, "postMessage payload dropped (too large)")
                continue
            receiver = table.resolve(sender, target)
            if receiver is None or receiver.session is None:
                continue
            sender_origin = context_origin(sender)
            if not target_origin_allows(
                    target_origin, sender_origin, context_origin(receiver)):
                continue
            try:
                logs = receiver.session.deliver_message(
                    payload, origin_string(sender_origin),
                    table.handle_for(receiver, sender))
            except Exception:
                continue
            if log is not None:
                for line in logs or []:
                    log(receiver, line)
            moved = True
            delivered = True
        if not moved:
            break
    return delivered


def mirror_rows(session):
    """The child's arena as `set_frame_document` wants it.

    `export()` rows carry the computed style pairs too; those are the
    bulk of the payload and the parent's mirror has no use for them —
    it answers DOM queries, not layout.
    """
    try:
        rows = session.export()
    except Exception:
        return None
    if len(rows) > MAX_MIRROR_NODES:
        return None
    return [tuple(row[:5]) for row in rows]


def apply_dom_write(fd, node, op, a, b):
    """Replay one mirror mutation against the real child document.

    Returns a console line when the write could not be applied, so a
    divergence between what the parent sees and what the child holds is
    never silent."""
    session = getattr(fd, "session", None)
    if session is None:
        return "frame write dropped: child document is gone"
    try:
        if op == OP_SET_TEXT:
            session.set_text_content(node, a)
        elif op == OP_SET_ATTR:
            session.set_attr(node, a, b)
        elif op in _ATTR_OPS:
            session.set_attr(node, _ATTR_OPS[op], b or a)
        elif op == OP_REMOVE_ATTR:
            session.remove_attr(node, a)
        elif op == OP_CLICK:
            session.dispatch_click(node)
        else:
            return f"frame write dropped: unknown op {op}"
    except Exception as exc:
        return f"frame write failed: {type(exc).__name__}"
    return None


def sync_mirrors(table):
    """Push each same-origin child's DOM into its embedder.

    Re-pushed only when the child's DOM version moved, since rebuilding
    the mirror invalidates every element wrapper the parent is holding.
    """
    for ctx in list(table.by_token.values()):
        fd = ctx.frame
        if fd is None or not getattr(fd, "mirror_node", None):
            continue
        owner = table.by_token.get(ctx.parent)
        if owner is None or owner.session is None:
            continue
        version = None
        if getattr(fd, "mirror_ok", False) and fd.session is not None:
            try:
                version = fd.session.state().dom_version
            except Exception:
                version = None
            rows = (mirror_rows(fd.session)
                    if version != getattr(fd, "_mirror_version", object())
                    else None)
            if rows is None:
                continue
        else:
            # not scriptable (cross-origin, sandboxed, gone): an empty
            # push is what makes contentDocument read null again
            if getattr(fd, "_mirror_version", None) is None:
                continue
            rows = []
        try:
            owner.session.set_frame_document(
                int(fd.mirror_node), int(fd.handle),
                str(fd.url or ""), rows)
            fd._mirror_version = version
        except Exception:
            pass   # older wheel without the mirror seam


def pump_dom_writes(table, log=None):
    """Apply mirror mutations back to the documents they name."""
    moved = False
    by_handle = {c.frame.handle: c.frame
                 for c in table.by_token.values()
                 if c.frame is not None and c.frame.handle}
    for ctx in list(table.by_token.values()):
        if ctx.session is None:
            continue
        try:
            writes = ctx.session.take_frame_dom_writes()
        except Exception:
            continue
        for write in sorted(writes or [], key=lambda w: w[5]):
            fd = by_handle.get(int(write[0]))
            if fd is None:
                continue
            note = apply_dom_write(
                fd, int(write[1]), int(write[2]), str(write[3]), str(write[4]))
            if note is not None and log is not None:
                log(ctx, note)
            else:
                moved = True
    return moved


def frame_graph(manager, root, owner_url):
    """[(iframe node index, context handle, same_origin)] for the
    iframes a document embeds — what the VM needs to answer
    `contentWindow` and to route `postMessage` to a named child."""
    rows = []
    if manager is None or root is None:
        return rows
    for node in manager._iframe_nodes(root):
        ridx = getattr(node, "_ridx", None)
        fd = manager.frames.get(frames._frame_key(node))
        if ridx is None or fd is None:
            continue
        handle = getattr(fd, "handle", 0)
        if not handle:
            continue
        ok = scriptable(fd, owner_url)
        # remembered so the mirror sync (which runs every tick, without
        # the parent tree in hand) knows where to push and whether to
        fd.mirror_node = int(ridx)
        fd.mirror_ok = ok
        rows.append((int(ridx), int(handle), ok))
    return rows
