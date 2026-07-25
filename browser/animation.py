"""CSS transitions and @keyframes animations.

One pure-Python animation engine shared by the tkinter shell and the
native (winit) shell — both call ``AnimationEngine.on_frame`` from their
frame tick, so the two shells run the same scheduler and produce the
same sampled values.

How it plugs into the pipeline:

- Style computation (Rust or Python) does not understand ``transition-*``
  / ``animation-*``; it carries them through as plain computed-style
  strings. This module normalizes shorthand + longhand declarations
  from a node's computed style dict.
- Both CSS parsers skip at-rules, so ``@keyframes`` blocks never reach
  the rule cascade. The shells keep the raw stylesheet texts
  (``css_sources``); ``parse_keyframes`` extracts the frames from them.
- Each frame, ``on_frame`` walks the DOM tree, detects base-style
  changes (starting/retargeting transitions), syncs running animations
  with the current ``animation-*`` values, samples every active effect
  against a monotonic clock, and patches the sampled values into
  ``node.style`` in place, before layout/paint read it.
- The engine distinguishes paint-only damage (opacity, colors,
  transform...) from layout damage so the shells can skip relayout for
  the common compositor-style animations.

The clock is injectable (``AnimationEngine(clock=...)``) so tests can
drive sampling deterministically without sleeping.
"""

import math
import re
import time

from .colors import NAMED
from .css_parser import CSSParser
from .html_parser import Element
from .style import _apply as _apply_declaration

# ---------------------------------------------------------------------
# times and timing functions
# ---------------------------------------------------------------------


def parse_time(value):
    """CSS <time> -> seconds, or None ("0.3s", "300ms", "0s")."""
    if value is None:
        return None
    v = value.strip().casefold()
    try:
        if v.endswith("ms"):
            return float(v[:-2]) / 1000.0
        if v.endswith("s"):
            return float(v[:-1])
    except ValueError:
        return None
    return None


def _cubic_bezier(x1, y1, x2, y2):
    """The CSS cubic-bezier easing: y as a function of progress x."""
    # clamp control-point x into [0, 1] per spec so x(t) is invertible
    x1 = min(max(x1, 0.0), 1.0)
    x2 = min(max(x2, 0.0), 1.0)

    def bezier(t, a, b):
        # one coordinate of the curve at parameter t (P0=0, P3=1)
        mt = 1.0 - t
        return 3 * mt * mt * t * a + 3 * mt * t * t * b + t * t * t

    def ease(x):
        if x <= 0.0:
            return 0.0
        if x >= 1.0:
            return 1.0
        # invert x(t) = x by bisection (monotonic in [0, 1])
        lo, hi = 0.0, 1.0
        for _ in range(24):
            mid = (lo + hi) / 2.0
            if bezier(mid, x1, x2) < x:
                lo = mid
            else:
                hi = mid
        return bezier((lo + hi) / 2.0, y1, y2)

    return ease


def _steps(count, position):
    count = max(1, int(count))
    start = position in ("start", "jump-start")

    def ease(x):
        if x <= 0.0:
            return 0.0
        if x >= 1.0:
            return 1.0
        step = math.floor(x * count) + (1 if start else 0)
        return min(max(step / count, 0.0), 1.0)

    return ease


_PRESET_TIMINGS = {
    "linear": lambda x: min(max(x, 0.0), 1.0),
    "ease": _cubic_bezier(0.25, 0.1, 0.25, 1.0),
    "ease-in": _cubic_bezier(0.42, 0.0, 1.0, 1.0),
    "ease-out": _cubic_bezier(0.0, 0.0, 0.58, 1.0),
    "ease-in-out": _cubic_bezier(0.42, 0.0, 0.58, 1.0),
    "step-start": _steps(1, "start"),
    "step-end": _steps(1, "end"),
}


def parse_timing(value):
    """A <easing-function> token -> callable [0,1]->[0,1], or None."""
    if not value:
        return None
    v = value.strip().casefold()
    if v in _PRESET_TIMINGS:
        return _PRESET_TIMINGS[v]
    m = re.fullmatch(
        r"cubic-bezier\(\s*([-\d.]+)\s*,\s*([-\d.]+)\s*,"
        r"\s*([-\d.]+)\s*,\s*([-\d.]+)\s*\)", v)
    if m:
        try:
            return _cubic_bezier(*(float(g) for g in m.groups()))
        except ValueError:
            return None
    m = re.fullmatch(r"steps\(\s*(\d+)\s*(?:,\s*([-\w]+)\s*)?\)", v)
    if m:
        return _steps(int(m.group(1)), (m.group(2) or "end"))
    return None


def _is_timing_token(tok):
    return parse_timing(tok) is not None


def _split_commas(value):
    """Split a CSS list on top-level commas (not inside parens)."""
    out, depth, start = [], 0, 0
    for i, ch in enumerate(value):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth = max(depth - 1, 0)
        elif ch == "," and depth == 0:
            out.append(value[start:i].strip())
            start = i + 1
    out.append(value[start:].strip())
    return out


def _split_tokens(item):
    """Split one comma-item into tokens, keeping function parens whole."""
    out, depth, start = [], 0, None
    for i, ch in enumerate(item):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth = max(depth - 1, 0)
        if ch.isspace() and depth == 0:
            if start is not None:
                out.append(item[start:i])
                start = None
        elif start is None:
            start = i
    if start is not None:
        out.append(item[start:])
    return out


# ---------------------------------------------------------------------
# transition-* normalization
# ---------------------------------------------------------------------


class TransitionSpec:
    __slots__ = ("prop", "duration", "delay", "timing")

    def __init__(self, prop, duration, delay, timing):
        self.prop = prop
        self.duration = duration
        self.delay = delay
        self.timing = timing

    def __repr__(self):
        return (f"TransitionSpec({self.prop!r}, dur={self.duration}, "
                f"delay={self.delay})")


_TRANSITION_CACHE = {}


def parse_transitions(style):
    """Normalize the transition shorthand + longhands from a computed
    style dict into a list of TransitionSpec (document order; a later
    spec for the same property wins)."""
    key = (style.get("transition"), style.get("transition-property"),
           style.get("transition-duration"), style.get("transition-delay"),
           style.get("transition-timing-function"))
    if all(v is None for v in key):
        return ()
    hit = _TRANSITION_CACHE.get(key)
    if hit is not None:
        return hit

    specs = []
    shorthand = style.get("transition")
    if shorthand and shorthand.strip().casefold() != "none":
        for item in _split_commas(shorthand):
            if not item:
                continue
            prop = None
            times = []
            timing = None
            for tok in _split_tokens(item):
                t = parse_time(tok)
                if t is not None:
                    times.append(t)
                elif _is_timing_token(tok):
                    timing = parse_timing(tok)
                elif prop is None:
                    prop = tok.casefold()
            if prop == "none":
                continue
            specs.append(TransitionSpec(
                prop or "all",
                times[0] if times else 0.0,
                times[1] if len(times) > 1 else 0.0,
                timing or _PRESET_TIMINGS["ease"]))

    # longhands override the shorthand's components; the property list
    # defines how many transitions there are, other lists repeat
    props_raw = style.get("transition-property")
    if props_raw is not None:
        p = props_raw.strip().casefold()
        props = [] if p == "none" else \
            [x.casefold() for x in _split_commas(props_raw) if x]
        specs = [TransitionSpec(prop, 0.0, 0.0, _PRESET_TIMINGS["ease"])
                 for prop in props]

    def _longhand(name, parse, attr, default):
        raw = style.get(name)
        if raw is None:
            return
        values = [parse(x) for x in _split_commas(raw)]
        values = [v if v is not None else default for v in values]
        if not values:
            return
        if not specs and props_raw is None and shorthand is None:
            # duration/delay/timing without a property list applies to all
            specs.append(TransitionSpec(
                "all", 0.0, 0.0, _PRESET_TIMINGS["ease"]))
        for i, spec in enumerate(specs):
            setattr(spec, attr, values[i % len(values)])

    _longhand("transition-duration", parse_time, "duration", 0.0)
    _longhand("transition-delay", parse_time, "delay", 0.0)
    _longhand("transition-timing-function", parse_timing, "timing",
              _PRESET_TIMINGS["ease"])

    specs = tuple(specs)
    if len(_TRANSITION_CACHE) < 4096:
        _TRANSITION_CACHE[key] = specs
    return specs


def transition_for(specs, prop):
    """The spec covering `prop` (exact name beats `all`; later wins)."""
    match = None
    for spec in specs:
        if spec.prop == prop:
            match = spec
        elif spec.prop == "all" and match is None:
            match = spec
    # approximation: an exact property match beats `all`; within each
    # class the later spec wins (real CSS matches positionally)
    return match


# ---------------------------------------------------------------------
# animation-* normalization
# ---------------------------------------------------------------------

_DIRECTIONS = ("normal", "reverse", "alternate", "alternate-reverse")
_FILL_MODES = ("none", "forwards", "backwards", "both")
_PLAY_STATES = ("running", "paused")


class AnimationSpec:
    __slots__ = ("name", "duration", "delay", "timing", "iterations",
                 "direction", "fill", "play_state")

    def __init__(self):
        self.name = "none"
        self.duration = 0.0
        self.delay = 0.0
        self.timing = _PRESET_TIMINGS["ease"]
        self.iterations = 1.0
        self.direction = "normal"
        self.fill = "none"
        self.play_state = "running"

    def __repr__(self):
        return (f"AnimationSpec({self.name!r}, dur={self.duration}, "
                f"delay={self.delay}, n={self.iterations}, "
                f"{self.direction}, fill={self.fill}, {self.play_state})")


_ANIMATION_CACHE = {}
_ANIMATION_LONGHANDS = (
    "animation-name", "animation-duration", "animation-delay",
    "animation-timing-function", "animation-iteration-count",
    "animation-direction", "animation-fill-mode", "animation-play-state")


def _parse_iterations(tok):
    t = tok.strip().casefold()
    if t == "infinite":
        return float("inf")
    try:
        n = float(t)
        return n if n >= 0 else None
    except ValueError:
        return None


def parse_animations(style):
    """Normalize the animation shorthand + longhands into a list of
    AnimationSpec (one per comma item of animation-name)."""
    key = tuple(style.get(k) for k in ("animation",) + _ANIMATION_LONGHANDS)
    if all(v is None for v in key):
        return ()
    hit = _ANIMATION_CACHE.get(key)
    if hit is not None:
        return hit

    specs = []
    shorthand = style.get("animation")
    if shorthand and shorthand.strip().casefold() != "none":
        for item in _split_commas(shorthand):
            if not item:
                continue
            spec = AnimationSpec()
            times = []
            have = set()
            for tok in _split_tokens(item):
                t = parse_time(tok)
                low = tok.casefold()
                if t is not None:
                    times.append(t)
                elif _is_timing_token(tok) and "timing" not in have:
                    spec.timing = parse_timing(tok)
                    have.add("timing")
                elif low == "infinite" or _is_number(low):
                    if "iterations" not in have:
                        parsed = _parse_iterations(low)
                        if parsed is not None:
                            spec.iterations = parsed
                        have.add("iterations")
                elif low in _DIRECTIONS and "direction" not in have:
                    spec.direction = low
                    have.add("direction")
                elif low in _FILL_MODES and "fill" not in have:
                    spec.fill = low
                    have.add("fill")
                elif low in _PLAY_STATES and "play" not in have:
                    spec.play_state = low
                    have.add("play")
                elif spec.name == "none":
                    spec.name = low
            spec.duration = times[0] if times else 0.0
            spec.delay = times[1] if len(times) > 1 else 0.0
            specs.append(spec)

    names_raw = style.get("animation-name")
    if names_raw is not None:
        names = [x.casefold() for x in _split_commas(names_raw) if x]
        specs = []
        for name in names:
            spec = AnimationSpec()
            spec.name = name
            specs.append(spec)

    def _longhand(prop, parse, attr, default):
        raw = style.get(prop)
        if raw is None:
            return
        values = [parse(x) for x in _split_commas(raw)]
        values = [v if v is not None else default for v in values]
        if not values or not specs:
            return
        for i, spec in enumerate(specs):
            setattr(spec, attr, values[i % len(values)])

    _longhand("animation-duration", parse_time, "duration", 0.0)
    _longhand("animation-delay", parse_time, "delay", 0.0)
    _longhand("animation-timing-function", parse_timing, "timing",
              _PRESET_TIMINGS["ease"])
    _longhand("animation-iteration-count", _parse_iterations,
              "iterations", 1.0)
    _longhand("animation-direction",
              lambda t: t.strip().casefold()
              if t.strip().casefold() in _DIRECTIONS else None,
              "direction", "normal")
    _longhand("animation-fill-mode",
              lambda t: t.strip().casefold()
              if t.strip().casefold() in _FILL_MODES else None,
              "fill", "none")
    _longhand("animation-play-state",
              lambda t: t.strip().casefold()
              if t.strip().casefold() in _PLAY_STATES else None,
              "play_state", "running")

    specs = tuple(s for s in specs if s.name not in ("none", ""))
    if len(_ANIMATION_CACHE) < 4096:
        _ANIMATION_CACHE[key] = specs
    return specs


def _is_number(tok):
    try:
        float(tok)
        return True
    except ValueError:
        return False


# ---------------------------------------------------------------------
# @keyframes parsing (from raw stylesheet text — both CSS parsers skip
# at-rules, so the cascade never sees these blocks)
# ---------------------------------------------------------------------

_KEYFRAMES_RE = re.compile(
    r"@(?:-webkit-|-moz-|-o-)?keyframes\s+([-\w]+)\s*\{",
    re.IGNORECASE)


def _strip_comments(css):
    return re.sub(r"/\*.*?\*/", " ", css, flags=re.DOTALL)


def _match_brace(s, open_idx):
    """Index just past the '}' matching the '{' at open_idx, or len(s)."""
    depth = 0
    for i in range(open_idx, len(s)):
        if s[i] == "{":
            depth += 1
        elif s[i] == "}":
            depth -= 1
            if depth == 0:
                return i + 1
    return len(s)


def _frame_offsets(selector):
    """'from', 'to', '50%' (comma list) -> offsets in [0,1]."""
    offsets = []
    for part in selector.split(","):
        p = part.strip().casefold()
        if p == "from":
            offsets.append(0.0)
        elif p == "to":
            offsets.append(1.0)
        elif p.endswith("%"):
            try:
                v = float(p[:-1]) / 100.0
            except ValueError:
                continue
            if 0.0 <= v <= 1.0:
                offsets.append(v)
    return offsets


def parse_keyframes(css_sources):
    """Extract every @keyframes block from raw stylesheet texts.

    Returns {name: [(offset, {prop: value}), ...]} with offsets sorted
    ascending. A later block with the same name replaces the earlier
    one (CSS cascade for @keyframes)."""
    out = {}
    for src in css_sources or ():
        if not src or "keyframes" not in src.casefold():
            continue
        text = _strip_comments(src)
        pos = 0
        while True:
            m = _KEYFRAMES_RE.search(text, pos)
            if m is None:
                break
            name = m.group(1).casefold()
            open_idx = m.end() - 1
            end = _match_brace(text, open_idx)
            body = text[m.end():end - 1]
            pos = end
            frames = {}
            i = 0
            while i < len(body):
                brace = body.find("{", i)
                if brace < 0:
                    break
                selector = body[i:brace]
                fend = _match_brace(body, brace)
                decls = CSSParser(body[brace + 1:fend - 1]).body()
                props = {}
                for prop, value in decls.items():
                    _apply_declaration(props, prop, value)
                for off in _frame_offsets(selector):
                    frames.setdefault(off, {}).update(props)
                i = fend
            out[name] = sorted(frames.items())
    return out


# ---------------------------------------------------------------------
# value interpolation
# ---------------------------------------------------------------------

_HEX_RE = re.compile(r"#[0-9a-fA-F]{3,8}$")
_RGB_RE = re.compile(
    r"rgba?\(\s*([\d.]+)\s*,\s*([\d.]+)\s*,\s*([\d.]+)"
    r"\s*(?:[,/]\s*([\d.]+%?)\s*)?\)$")


def parse_color(value):
    """CSS color -> (r, g, b, a) floats, or None if unsupported."""
    if not value:
        return None
    v = value.strip().casefold()
    if v == "transparent":
        return (0.0, 0.0, 0.0, 0.0)
    if v in NAMED:
        r, g, b = NAMED[v]
        return (float(r), float(g), float(b), 1.0)
    if _HEX_RE.fullmatch(v):
        h = v[1:]
        try:
            if len(h) == 3:
                return (*(float(int(c * 2, 16)) for c in h), 1.0)
            if len(h) == 4:
                return (*(float(int(c * 2, 16)) for c in h[:3]),
                        int(h[3] * 2, 16) / 255.0)
            if len(h) == 6:
                return (*(float(int(h[i:i + 2], 16)) for i in (0, 2, 4)),
                        1.0)
            if len(h) == 8:
                return (*(float(int(h[i:i + 2], 16)) for i in (0, 2, 4)),
                        int(h[6:8], 16) / 255.0)
        except ValueError:
            return None
        return None
    m = _RGB_RE.fullmatch(v)
    if m:
        try:
            r, g, b = (float(m.group(i)) for i in (1, 2, 3))
            a = m.group(4)
            if a is None:
                alpha = 1.0
            elif a.endswith("%"):
                alpha = float(a[:-1]) / 100.0
            else:
                alpha = float(a)
            return (r, g, b, min(max(alpha, 0.0), 1.0))
        except ValueError:
            return None
    return None


def format_color(rgba):
    r, g, b, a = rgba
    r = min(max(int(round(r)), 0), 255)
    g = min(max(int(round(g)), 0), 255)
    b = min(max(int(round(b)), 0), 255)
    if a >= 0.999:
        return f"#{r:02x}{g:02x}{b:02x}"
    return f"rgba({r},{g},{b},{round(a, 4)})"


_NUM_UNIT_RE = re.compile(r"([-+]?[\d.]+(?:e[-+]?\d+)?)([a-z%]*)$",
                          re.IGNORECASE)


def _num_unit(value):
    m = _NUM_UNIT_RE.fullmatch(value.strip())
    if not m:
        return None
    try:
        return float(m.group(1)), m.group(2).casefold()
    except ValueError:
        return None


def _fmt_number(v):
    r = round(v, 4)
    if r == int(r):
        return str(int(r))
    return f"{r:g}"


_TRANSFORM_FN_RE = re.compile(r"([a-zA-Z0-9]+)\s*\(([^)]*)\)")


def _parse_transform_list(value):
    """transform value -> [(fn, [(num, unit), ...])], or None."""
    v = (value or "").strip()
    if not v or v.casefold() == "none":
        return []
    out = []
    matched = 0
    for m in _TRANSFORM_FN_RE.finditer(v):
        matched += 1
        args = []
        for a in m.group(2).split(","):
            if not a.strip():
                continue
            nu = _num_unit(a)
            if nu is None:
                return None
            args.append(nu)
        out.append((m.group(1).casefold(), args))
    return out if matched else None


def _identity_transform(fns):
    """A neutral counterpart for interpolating against `none`."""
    out = []
    for fn, args in fns:
        neutral = 1.0 if fn.startswith("scale") or fn == "matrix" else 0.0
        if fn == "matrix" and len(args) == 6:
            out.append((fn, [(1.0, ""), (0.0, ""), (0.0, ""),
                             (1.0, ""), (0.0, ""), (0.0, "")]))
        else:
            out.append((fn, [(neutral, u) for _n, u in args]))
    return out


def _lerp(a, b, t):
    return a + (b - a) * t


def interpolate(prop, a, b, t):
    """Interpolate CSS values a->b at progress t; falls back to a
    discrete 50% flip for value forms we cannot blend."""
    if a == b:
        return a
    if t <= 0.0:
        return a
    if t >= 1.0:
        return b
    # colors
    if prop == "color" or prop.endswith("-color"):
        ca, cb = parse_color(a), parse_color(b)
        if ca is not None and cb is not None:
            return format_color(tuple(_lerp(x, y, t)
                                      for x, y in zip(ca, cb)))
        return a if t < 0.5 else b
    # transforms
    if prop == "transform":
        fa, fb = _parse_transform_list(a), _parse_transform_list(b)
        if fa is not None and fb is not None:
            if not fa and fb:
                fa = _identity_transform(fb)
            elif fa and not fb:
                fb = _identity_transform(fa)
            if len(fa) == len(fb) and all(
                    x[0] == y[0] and len(x[1]) == len(y[1])
                    and all(xu == yu for (_xn, xu), (_yn, yu)
                            in zip(x[1], y[1]))
                    for x, y in zip(fa, fb)):
                parts = []
                for (fn, aa), (_fn, bb) in zip(fa, fb):
                    args = ", ".join(
                        _fmt_number(_lerp(x, y, t)) + u
                        for (x, u), (y, _u) in zip(aa, bb))
                    parts.append(f"{fn}({args})")
                return " ".join(parts) if parts else "none"
        return a if t < 0.5 else b
    # numbers and same-unit lengths
    na, nb = _num_unit(a), _num_unit(b)
    if na is not None and nb is not None:
        (va, ua), (vb, ub) = na, nb
        if ua == ub or (va == 0 and not ua) or (vb == 0 and not ub):
            unit = ua or ub
            return _fmt_number(_lerp(va, vb, t)) + unit
    return a if t < 0.5 else b


# ---------------------------------------------------------------------
# damage classification: which properties need layout vs paint only
# ---------------------------------------------------------------------

PAINT_ONLY_PROPS = {
    "opacity", "transform", "visibility", "box-shadow",
    "background-image", "background-position", "border-radius",
    "text-decoration", "z-index", "cursor",
}

DAMAGE_NONE, DAMAGE_PAINT, DAMAGE_LAYOUT = 0, 1, 2

_FONT_PROPS = {"font-size", "font-weight", "font-style", "font-family"}

# properties the engine itself consumes — never animated
_CONTROL_PREFIXES = ("transition", "animation")


def classify(prop):
    if prop in PAINT_ONLY_PROPS or prop == "color" \
            or prop.endswith("-color"):
        return DAMAGE_PAINT
    return DAMAGE_LAYOUT


def _is_control_prop(prop):
    return prop.startswith(_CONTROL_PREFIXES)


# ---------------------------------------------------------------------
# the engine
# ---------------------------------------------------------------------


class _Transition:
    __slots__ = ("begin", "end", "start", "duration", "delay", "timing")

    def __init__(self, begin, end, start, duration, delay, timing):
        self.begin = begin
        self.end = end
        self.start = start
        self.duration = duration
        self.delay = delay
        self.timing = timing


class _Animation:
    __slots__ = ("spec", "start", "paused_at", "pause_accum", "finished")

    def __init__(self, spec, start):
        self.spec = spec
        self.start = start
        self.paused_at = None
        self.pause_accum = 0.0
        self.finished = False


class _NodeState:
    __slots__ = ("base", "written", "transitions", "animations")

    def __init__(self):
        self.base = {}
        self.written = {}
        self.transitions = {}
        self.animations = {}


class FrameResult:
    __slots__ = ("damage", "active")

    def __init__(self, damage, active):
        self.damage = damage  # "none" | "paint" | "layout"
        self.active = active  # more frames wanted

    def __repr__(self):
        return f"FrameResult(damage={self.damage!r}, active={self.active})"


_DAMAGE_NAMES = {DAMAGE_NONE: "none", DAMAGE_PAINT: "paint",
                 DAMAGE_LAYOUT: "layout"}


class AnimationEngine:
    """Samples transitions/@keyframes for one page's node tree.

    Call ``reset(css_sources)`` on navigation and ``on_frame(root)``
    once per frame (and after any restyle). Nodes are keyed by their
    Rust arena index when present (stable across native tree rebuilds)
    or by object identity on the pure-Python path."""

    def __init__(self, clock=time.monotonic):
        self.clock = clock
        self.keyframes = {}
        self._nodes = {}
        self.active = False

    def reset(self, css_sources=None):
        self.keyframes = parse_keyframes(css_sources or [])
        self._nodes = {}
        self.active = False

    # -- helpers -------------------------------------------------------

    @staticmethod
    def _key(node):
        ridx = getattr(node, "_ridx", None)
        if ridx is not None:
            return ("r", ridx)
        return ("p", id(node))

    def _sync_animations(self, state, base, now):
        keep = {}
        for i, spec in enumerate(parse_animations(base)):
            frames = self.keyframes.get(spec.name)
            if not frames or spec.duration <= 0 or spec.iterations <= 0:
                continue
            akey = (i, spec.name)
            anim = state.animations.get(akey)
            if anim is None:
                anim = _Animation(spec, now)
                if spec.play_state == "paused":
                    anim.paused_at = now
            else:
                if spec.play_state == "paused" and anim.paused_at is None:
                    anim.paused_at = now
                elif spec.play_state == "running" \
                        and anim.paused_at is not None:
                    anim.pause_accum += now - anim.paused_at
                    anim.paused_at = None
                anim.spec = spec
            keep[akey] = anim
        state.animations = keep

    def _sample_animation(self, anim, now, base):
        """-> ({prop: value} or None, still_active). Marks
        anim.finished when a finite run has completed."""
        spec = anim.spec
        frames = self.keyframes.get(spec.name)
        if not frames:
            return None, False
        t_now = anim.paused_at if anim.paused_at is not None else now
        elapsed = t_now - anim.start - anim.pause_accum - spec.delay
        total = spec.iterations * spec.duration

        if elapsed < 0:
            if spec.fill in ("backwards", "both"):
                progress = self._directed(spec, 0, 0.0)
                return self._values_at(spec, frames, progress, base), True
            return None, True
        if elapsed >= total:
            anim.finished = True
            if spec.fill in ("forwards", "both"):
                last_iter = max(int(math.ceil(spec.iterations)) - 1, 0)
                local = min(max(spec.iterations - last_iter, 0.0), 1.0)
                progress = self._directed(spec, last_iter, local)
                return (self._values_at(spec, frames, progress, base),
                        False)
            return None, False
        iter_idx = int(elapsed // spec.duration)
        local = (elapsed % spec.duration) / spec.duration
        progress = self._directed(spec, iter_idx, local)
        paused = anim.paused_at is not None
        return self._values_at(spec, frames, progress, base), not paused

    @staticmethod
    def _directed(spec, iter_idx, local):
        d = spec.direction
        if d == "reverse":
            reverse = True
        elif d == "alternate":
            reverse = iter_idx % 2 == 1
        elif d == "alternate-reverse":
            reverse = iter_idx % 2 == 0
        else:
            reverse = False
        return 1.0 - local if reverse else local

    @staticmethod
    def _values_at(spec, frames, progress, base):
        """Interpolated {prop: value} at directed progress in [0,1].
        Timing applies within each keyframe segment; missing 0%/100%
        frames fall back to the property's base value."""
        props = set()
        for _off, decls in frames:
            props.update(decls)
        out = {}
        for prop in props:
            if _is_control_prop(prop):
                continue
            points = [(off, decls[prop]) for off, decls in frames
                      if prop in decls]
            if not points:
                continue
            if points[0][0] > 0.0:
                fallback = base.get(prop, points[0][1])
                points.insert(0, (0.0, fallback))
            if points[-1][0] < 1.0:
                fallback = base.get(prop, points[-1][1])
                points.append((1.0, fallback))
            value = points[-1][1]
            for (o1, v1), (o2, v2) in zip(points, points[1:]):
                if progress <= o1:
                    value = v1
                    break
                if progress < o2 or (o2 >= 1.0 and progress <= o2):
                    span = o2 - o1
                    u = (progress - o1) / span if span > 0 else 1.0
                    value = interpolate(prop, v1, v2, spec.timing(u))
                    break
            out[prop] = value
        return out

    # -- the per-frame pass ---------------------------------------------

    def on_frame(self, root, now=None):
        """Sample all effects and patch node.style in place. Returns a
        FrameResult telling the caller how much work the frame needs
        ("none"/"paint"/"layout") and whether more frames are wanted."""
        if root is None:
            self.active = False
            return FrameResult("none", False)
        if now is None:
            now = self.clock()
        damage = DAMAGE_NONE
        active = False
        seen = {}

        stack = [root]
        while stack:
            node = stack.pop()
            stack.extend(getattr(node, "children", ()))
            if not isinstance(node, Element):
                continue
            style = getattr(node, "style", None)
            if style is None:
                continue
            key = self._key(node)
            state = self._nodes.get(key)

            # cheap gate: nothing animatable on this node, no history
            if state is None and not any(
                    p.startswith(_CONTROL_PREFIXES) for p in style):
                continue
            fresh = state is None
            if fresh:
                state = _NodeState()
            seen[key] = state

            # 1. derive the base style: values we wrote ourselves are
            # not page changes — recover the underlying base for them
            old_base = state.base
            base = {}
            for prop, val in style.items():
                if prop in state.written and state.written[prop] == val:
                    if prop in old_base:
                        base[prop] = old_base[prop]
                    # else: purely-animated prop — not part of the base
                else:
                    base[prop] = val

            # 2. transitions: react to base changes (never on the first
            # sighting of a node — insertion does not transition)
            if not fresh:
                t_specs = parse_transitions(base)
                changed = [p for p in set(old_base) | set(base)
                           if old_base.get(p) != base.get(p)
                           and not _is_control_prop(p)]
                for prop in changed:
                    spec = transition_for(t_specs, prop) if t_specs \
                        else None
                    begin = state.written.get(prop, old_base.get(prop))
                    end = base.get(prop)
                    if (spec is None or spec.duration <= 0
                            or begin is None or end is None):
                        state.transitions.pop(prop, None)
                        continue
                    running = state.transitions.get(prop)
                    if running is not None and running.end == end:
                        continue  # already heading there
                    if begin == end:
                        state.transitions.pop(prop, None)
                        continue
                    state.transitions[prop] = _Transition(
                        begin, end, now, spec.duration, spec.delay,
                        spec.timing)
            state.base = base

            # 3. animations: sync with the current animation-* values
            self._sync_animations(state, base, now)

            # 4. sample
            out = {}
            for prop, tr in list(state.transitions.items()):
                t = (now - tr.start - tr.delay)
                if t >= tr.duration:
                    state.transitions.pop(prop)
                    continue  # override gone -> base shows through
                if t <= 0:
                    value = tr.begin
                else:
                    value = interpolate(
                        prop, tr.begin, tr.end,
                        tr.timing(t / tr.duration))
                out[prop] = value
                active = True
            for anim in list(state.animations.values()):
                values, running = self._sample_animation(anim, now, base)
                if values:
                    out.update(values)
                if running:
                    active = True
                elif anim.finished and anim.spec.fill not in (
                        "forwards", "both"):
                    akey = next(k for k, v in state.animations.items()
                                if v is anim)
                    state.animations.pop(akey)

            # 5. patch node.style; restore base under retired overrides
            for prop in state.written:
                if prop in out:
                    continue
                if style.get(prop) == state.written[prop]:
                    if prop in base:
                        style[prop] = base[prop]
                    else:
                        style.pop(prop, None)
                    damage = max(damage, classify(prop))
                    if prop in _FONT_PROPS:
                        node._font = None
            for prop, value in out.items():
                if style.get(prop) != value:
                    style[prop] = value
                    damage = max(damage, classify(prop))
                    if prop in _FONT_PROPS:
                        node._font = None
            state.written = out

            # keep the base snapshot for any node that *could* start a
            # transition later (its style declares transition-*) — the
            # next base change must see the previous value to animate
            # from. Everything else with no live effects is dropped.
            if not state.transitions and not state.animations \
                    and not state.written and not any(
                        p.startswith("transition") for p in style):
                seen.pop(key, None)

        self._nodes = seen  # removed DOM nodes drop their state here
        self.active = active
        return FrameResult(_DAMAGE_NAMES[damage], active)
