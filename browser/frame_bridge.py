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
        rows.append((int(ridx), int(handle), scriptable(fd, owner_url)))
    return rows
