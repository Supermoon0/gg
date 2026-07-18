"""CSS parser: selectors, declarations, specificity.

Supports tag/class/id selectors, compound selectors (p.intro),
descendant combinators, comma-separated selector lists, and
skips at-rules and anything it can't parse (error recovery).
"""

from .html_parser import Element


class TagSelector:
    def __init__(self, tag):
        self.tag = tag
        self.specificity = (0, 0, 1)

    def matches(self, node):
        return isinstance(node, Element) and node.tag == self.tag

    def __repr__(self):
        return self.tag


def node_classes(node):
    """Cached class list for an element (parsed once per node)."""
    classes = getattr(node, "_classes", None)
    if classes is None:
        classes = node.attributes.get("class", "").split()
        node._classes = classes
    return classes


class ClassSelector:
    def __init__(self, cls):
        self.cls = cls
        self.specificity = (0, 1, 0)

    def matches(self, node):
        return isinstance(node, Element) and self.cls in node_classes(node)

    def __repr__(self):
        return "." + self.cls


class IdSelector:
    def __init__(self, id_):
        self.id = id_
        self.specificity = (1, 0, 0)

    def matches(self, node):
        return (isinstance(node, Element)
                and node.attributes.get("id", "") == self.id)

    def __repr__(self):
        return "#" + self.id


class UniversalSelector:
    specificity = (0, 0, 0)

    def matches(self, node):
        return isinstance(node, Element)

    def __repr__(self):
        return "*"


class AttributeSelector:
    """[attr], [attr=v], [attr~=v], [attr^=v], [attr$=v], [attr*=v],
    [attr|=v] — class-level specificity."""

    def __init__(self, name, op, value):
        self.name = name
        self.op = op
        self.value = value
        self.specificity = (0, 1, 0)

    def matches(self, node):
        if not isinstance(node, Element):
            return False
        actual = node.attributes.get(self.name)
        if actual is None:
            return False
        if self.op is None:
            return True
        v = self.value
        if self.op == "=":
            return actual == v
        if self.op == "~":
            return v in actual.split()
        if self.op == "^":
            return bool(v) and actual.startswith(v)
        if self.op == "$":
            return bool(v) and actual.endswith(v)
        if self.op == "*":
            return bool(v) and v in actual
        if self.op == "|":
            return actual == v or actual.startswith(v + "-")
        return False

    def __repr__(self):
        if self.op is None:
            return f"[{self.name}]"
        return f"[{self.name}{self.op}={self.value!r}]"


class RootSelector:
    """:root — the document root element (pseudo-class specificity)."""

    specificity = (0, 1, 0)

    def matches(self, node):
        return isinstance(node, Element) and node.tag == "html"

    def __repr__(self):
        return ":root"


class WhereSelector:
    """:where(a, b, ...) — matches if any alternative matches.
    Zero specificity per spec. Unsupported alternatives (attribute
    selectors, :host, ...) are dropped at parse time, so theme-override
    rules like :where([data-theme=dark]) simply never match."""

    def __init__(self, options):
        self.options = options
        self.specificity = (0, 0, 0)

    def matches(self, node):
        return any(o.matches(node) for o in self.options)

    def __repr__(self):
        return ":where(" + ",".join(repr(o) for o in self.options) + ")"


def _media_len(v):
    """A media-feature length in px (em/rem taken as 16px)."""
    v = v.strip()
    try:
        if v.endswith("px"):
            return float(v[:-2])
        if v.endswith(("em", "rem")):
            return float(v.rstrip("erm")) * 16.0
        return float(v)
    except ValueError:
        return None


def _element_siblings(node):
    parent = node.parent
    if parent is None:
        return []
    return [c for c in parent.children if isinstance(c, Element)]


class NotSelector:
    """:not(compound) — matches when the inner selector does not."""

    def __init__(self, inner):
        self.inner = inner
        self.specificity = (0, 1, 0)

    def matches(self, node):
        return isinstance(node, Element) and not self.inner.matches(node)

    def __repr__(self):
        return f":not({self.inner!r})"


class NthChildSelector:
    """:nth-child(odd|even|N) — structural position among element
    siblings (an+b is rejected at parse time)."""

    def __init__(self, kind):
        self.kind = kind  # "odd" | "even" | int (1-based)
        self.specificity = (0, 1, 0)

    def matches(self, node):
        if not isinstance(node, Element):
            return False
        sibs = _element_siblings(node)
        try:
            nth = sibs.index(node) + 1
        except ValueError:
            return False
        if self.kind == "odd":
            return nth % 2 == 1
        if self.kind == "even":
            return nth % 2 == 0
        return nth == self.kind

    def __repr__(self):
        return f":nth-child({self.kind})"


class StructuralSelector:
    """:first-child / :last-child."""

    def __init__(self, last):
        self.last = last
        self.specificity = (0, 1, 0)

    def matches(self, node):
        if not isinstance(node, Element):
            return False
        sibs = _element_siblings(node)
        if not sibs:
            return False
        return node is (sibs[-1] if self.last else sibs[0])

    def __repr__(self):
        return ":last-child" if self.last else ":first-child"


class CompoundSelector:
    """Several simple selectors that must all match one node: p.intro#x"""

    def __init__(self, parts):
        self.parts = parts
        self.specificity = tuple(
            sum(p.specificity[i] for p in parts) for i in range(3))

    def matches(self, node):
        return all(p.matches(node) for p in self.parts)

    def __repr__(self):
        return "".join(repr(p) for p in self.parts)


class DescendantSelector:
    """ancestor descendant"""

    def __init__(self, ancestor, descendant):
        self.ancestor = ancestor
        self.descendant = descendant
        self.specificity = tuple(
            ancestor.specificity[i] + descendant.specificity[i]
            for i in range(3))

    def matches(self, node):
        if not self.descendant.matches(node):
            return False
        parent = node.parent
        while parent:
            if self.ancestor.matches(parent):
                return True
            parent = parent.parent
        return False

    def __repr__(self):
        return f"{self.ancestor!r} {self.descendant!r}"


class CSSParser:
    def __init__(self, s, viewport_width=1280.0):
        self.s = s
        self.i = 0
        self.vw = viewport_width

    def whitespace(self):
        while self.i < len(self.s) and self.s[self.i].isspace():
            self.i += 1
        # Skip comments too
        while self.s.startswith("/*", self.i):
            end = self.s.find("*/", self.i + 2)
            self.i = (end + 2) if end >= 0 else len(self.s)
            while self.i < len(self.s) and self.s[self.i].isspace():
                self.i += 1

    def word(self):
        start = self.i
        while self.i < len(self.s):
            c = self.s[self.i]
            if c.isalnum() or c in "#-_.%!\"'()," or ord(c) > 127:
                self.i += 1
            else:
                break
        if self.i <= start:
            raise Exception(f"word expected at {self.i}")
        return self.s[start:self.i]

    def literal(self, literal):
        if self.i >= len(self.s) or self.s[self.i] != literal:
            raise Exception(f"expected {literal!r} at {self.i}")
        self.i += 1

    def until_chars(self, chars):
        start = self.i
        while self.i < len(self.s) and self.s[self.i] not in chars:
            self.i += 1
        return self.s[start:self.i]

    def pair(self):
        prop = self.word()
        self.whitespace()
        self.literal(":")
        self.whitespace()
        value = self.until_chars(";}").strip()
        # Strip !important so it can't poison the value ("red !important"
        # is not a color). Priority itself is approximated by source
        # order, which the cascade already preserves.
        low = value.casefold()
        if low.endswith("important"):
            head = value[:-len("important")].rstrip()
            if head.endswith("!"):
                value = head[:-1].rstrip()
        return prop.casefold(), value

    def ignore_until(self, chars):
        while self.i < len(self.s):
            if self.s[self.i] in chars:
                return self.s[self.i]
            self.i += 1
        return None

    def body(self):
        pairs = {}
        self.whitespace()  # tolerate a leading-space style attribute
        while self.i < len(self.s) and self.s[self.i] != "}":
            try:
                prop, value = self.pair()
                pairs[prop] = value
                self.whitespace()
                self.literal(";")
                self.whitespace()
            except Exception:
                why = self.ignore_until(";}")
                if why == ";":
                    self.literal(";")
                    self.whitespace()
                else:
                    break
        return pairs

    def simple_selector(self):
        """One compound selector like p, .cls, #id, p.cls#id, *"""
        parts = []
        while self.i < len(self.s):
            c = self.s[self.i]
            if c == "*":
                self.i += 1
                parts.append(UniversalSelector())
            elif c == ".":
                self.i += 1
                parts.append(ClassSelector(self._name()))
            elif c == "#":
                self.i += 1
                parts.append(IdSelector(self._name()))
            elif c.isalnum() or c in "-_":
                parts.append(TagSelector(self._name().casefold()))
            elif c == "[":
                self.i += 1
                parts.append(self._attribute())
            elif c == ":":
                low = self.s[self.i:self.i + 7].casefold()
                nxt = low[5:6]
                if low.startswith(":where("):
                    self.i += 7
                    parts.append(self._where())
                elif low.startswith(":root") and not (
                        nxt and (nxt.isalnum() or nxt in "-_")):
                    self.i += 5
                    parts.append(RootSelector())
                elif self.s[self.i:self.i + 5].casefold() == ":not(":
                    self.i += 5
                    inner = self.simple_selector()
                    self.whitespace()
                    self.literal(")")
                    parts.append(NotSelector(inner))
                elif self.s[self.i:self.i + 11].casefold() \
                        == ":nth-child(":
                    self.i += 11
                    arg = self.until_chars(")").strip().casefold()
                    self.literal(")")
                    if arg in ("odd", "even"):
                        parts.append(NthChildSelector(arg))
                    else:
                        try:
                            parts.append(NthChildSelector(int(arg)))
                        except ValueError:
                            raise Exception("an+b not supported")
                elif self.s[self.i:self.i + 12].casefold() \
                        == ":first-child":
                    self.i += 12
                    parts.append(StructuralSelector(False))
                elif self.s[self.i:self.i + 11].casefold() \
                        == ":last-child":
                    self.i += 11
                    parts.append(StructuralSelector(True))
                else:
                    break
            else:
                break
        if not parts:
            raise Exception(f"selector expected at {self.i}")
        if len(parts) == 1:
            return parts[0]
        return CompoundSelector(parts)

    def _attribute(self):
        """Parse an [attr...] selector ('[' already consumed)."""
        self.whitespace()
        name = self._name().casefold()
        self.whitespace()
        if self.i < len(self.s) and self.s[self.i] == "]":
            self.i += 1
            return AttributeSelector(name, None, "")
        op = "="
        if self.s[self.i] in "~^$*|":
            op = self.s[self.i]
            self.i += 1
        if self.i >= len(self.s) or self.s[self.i] != "=":
            raise Exception("bad attribute selector")
        self.i += 1
        self.whitespace()
        if self.i < len(self.s) and self.s[self.i] in "'\"":
            quote = self.s[self.i]
            self.i += 1
            start = self.i
            while self.i < len(self.s) and self.s[self.i] != quote:
                self.i += 1
            value = self.s[start:self.i]
            self.i += 1
        else:
            start = self.i
            while self.i < len(self.s) and self.s[self.i] not in "] \t\n":
                self.i += 1
            value = self.s[start:self.i]
        self.whitespace()
        # tolerate (and ignore) the case-sensitivity flag "[a=b i]"
        if self.i < len(self.s) and self.s[self.i] in "iIsS":
            self.i += 1
            self.whitespace()
        if self.i >= len(self.s) or self.s[self.i] != "]":
            raise Exception("bad attribute selector")
        self.i += 1
        return AttributeSelector(name, op, value)

    def _where(self):
        """Parse :where() alternatives (self.i is just past ':where(').
        Each alternative must be a compound selector we support and end
        cleanly at ',' or ')' — otherwise it is dropped, not the rule."""
        options = []
        while self.i < len(self.s):
            self.whitespace()
            if self.i < len(self.s) and self.s[self.i] == ")":
                self.i += 1
                break
            option = None
            try:
                option = self.simple_selector()
            except Exception:
                pass
            self.whitespace()
            clean = self.i < len(self.s) and self.s[self.i] in ",)"
            if option is not None and clean:
                options.append(option)
            depth = 0  # skip the rest of an unsupported alternative
            while self.i < len(self.s):
                ch = self.s[self.i]
                if ch == "(":
                    depth += 1
                elif ch == ")":
                    if depth == 0:
                        break
                    depth -= 1
                elif ch == "," and depth == 0:
                    break
                self.i += 1
            if self.i < len(self.s) and self.s[self.i] == ",":
                self.i += 1
                continue
            if self.i < len(self.s) and self.s[self.i] == ")":
                self.i += 1
            break
        return WhereSelector(options)

    def _name(self):
        start = self.i
        while self.i < len(self.s):
            c = self.s[self.i]
            if c.isalnum() or c in "-_" or ord(c) > 127:
                self.i += 1
            else:
                break
        if self.i <= start:
            raise Exception(f"name expected at {self.i}")
        return self.s[start:self.i]

    def selector(self):
        """One full selector (with descendant combinators)."""
        out = self.simple_selector()
        self.whitespace()
        while self.i < len(self.s) and self.s[self.i] not in "{,":
            # Unsupported combinators (>, +, ~) degrade to descendant
            if self.s[self.i] in ">+~":
                self.i += 1
                self.whitespace()
            # Unsupported pseudo suffixes like :hover -> bail on rule
            # (:root / :where( / [attr] are handled by simple_selector)
            if self.i < len(self.s) and self.s[self.i] == ":":
                low = self.s[self.i:self.i + 12].casefold()
                if not (low.startswith(":root")
                        or low.startswith(":where(")
                        or low.startswith(":not(")
                        or low.startswith(":nth-child(")
                        or low.startswith(":first-child")
                        or low.startswith(":last-child")):
                    raise Exception("unsupported selector feature")
            descendant = self.simple_selector()
            out = DescendantSelector(out, descendant)
            self.whitespace()
        return out

    def parse(self):
        """Returns a list of (selector, declarations) rules."""
        rules = []
        while self.i < len(self.s):
            try:
                self.whitespace()
                if self.i >= len(self.s):
                    break
                if self.s[self.i] == "@":
                    if self.s[self.i:self.i + 6].casefold() == "@media":
                        self._media(rules)
                    else:
                        self._skip_at_rule()
                    continue
                selectors = [self.selector()]
                self.whitespace()
                while self.i < len(self.s) and self.s[self.i] == ",":
                    self.literal(",")
                    self.whitespace()
                    selectors.append(self.selector())
                    self.whitespace()
                self.literal("{")
                self.whitespace()
                body = self.body()
                self.literal("}")
                for sel in selectors:
                    rules.append((sel, body))
            except Exception:
                why = self.ignore_until("};")
                if why in ("}", ";"):
                    self.i += 1
                else:
                    break
        return rules

    def _media(self, rules):
        """@media <cond> { rules }: when the condition matches the
        viewport, parse the inner rules at this level, else skip."""
        self.i += 6  # past "@media"
        start = self.i
        while self.i < len(self.s) and self.s[self.i] != "{":
            self.i += 1
        cond = self.s[start:self.i].casefold()
        if self.i >= len(self.s):
            return
        self.i += 1  # past "{"
        if self._media_matches(cond):
            while self.i < len(self.s):
                self.whitespace()
                if self.i >= len(self.s) or self.s[self.i] == "}":
                    break
                try:
                    selectors = [self.selector()]
                    self.whitespace()
                    while self.i < len(self.s) and self.s[self.i] == ",":
                        self.literal(",")
                        self.whitespace()
                        selectors.append(self.selector())
                        self.whitespace()
                    self.literal("{")
                    self.whitespace()
                    body = self.body()
                    self.literal("}")
                    for sel in selectors:
                        rules.append((sel, body))
                except Exception:
                    why = self.ignore_until("}")
                    if why == "}":
                        self.i += 1
                    else:
                        break
            if self.i < len(self.s) and self.s[self.i] == "}":
                self.i += 1
        else:
            depth = 1
            while self.i < len(self.s) and depth > 0:
                c = self.s[self.i]
                if c == "{":
                    depth += 1
                elif c == "}":
                    depth -= 1
                self.i += 1

    def _media_matches(self, cond):
        return any(self._media_query(q.strip())
                   for q in cond.split(","))

    def _media_query(self, q):
        ok = True
        for part in q.split(" and "):
            p = part.strip().replace("only ", "").strip()
            if not p or p in ("screen", "all"):
                continue
            if p in ("print", "speech"):
                return False
            if p.startswith("(") and p.endswith(")"):
                inner = p[1:-1]
                if ":" in inner:
                    feat, val = inner.split(":", 1)
                    px = _media_len(val.strip())
                    if px is not None:
                        if feat.strip() == "min-width":
                            ok = ok and self.vw >= px
                        elif feat.strip() == "max-width":
                            ok = ok and self.vw <= px
        return ok

    def _skip_at_rule(self):
        # @import ...; or non-media @rule { ... }
        while self.i < len(self.s) and self.s[self.i] not in ";{":
            self.i += 1
        if self.i >= len(self.s):
            return
        if self.s[self.i] == ";":
            self.i += 1
            return
        depth = 0
        while self.i < len(self.s):
            c = self.s[self.i]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    self.i += 1
                    return
            self.i += 1
