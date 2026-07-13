"""HTML tokenizer and tree builder producing a DOM tree."""

import html as html_entities

SELF_CLOSING_TAGS = {
    "area", "base", "br", "col", "embed", "hr", "img", "input",
    "link", "meta", "param", "source", "track", "wbr",
}

HEAD_TAGS = {
    "base", "basefont", "bgsound", "noscript",
    "link", "meta", "title", "style", "script",
}

RAW_TEXT_TAGS = {"script", "style"}

# Tags that implicitly close an open <p>
P_CLOSERS = {
    "p", "div", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "li",
    "table", "blockquote", "pre", "form", "hr", "section", "article",
    "header", "footer", "nav", "aside", "main", "figure",
}


class Text:
    def __init__(self, text, parent):
        self.text = text
        self.parent = parent
        self.children = []
        self.style = {}

    def __repr__(self):
        return repr(self.text)


class Element:
    def __init__(self, tag, attributes, parent):
        self.tag = tag
        self.attributes = attributes
        self.parent = parent
        self.children = []
        self.style = {}

    def __repr__(self):
        return "<" + self.tag + ">"


class HTMLParser:
    def __init__(self, body):
        self.body = body
        self.unfinished = []

    def parse(self):
        text = ""
        i = 0
        n = len(self.body)
        while i < n:
            c = self.body[i]
            if c == "<":
                # Comments and doctype
                if self.body.startswith("<!--", i):
                    end = self.body.find("-->", i + 4)
                    i = (end + 3) if end >= 0 else n
                    continue
                if self.body.startswith("<!", i):
                    end = self.body.find(">", i)
                    i = (end + 1) if end >= 0 else n
                    continue
                end = self._find_tag_end(i)
                if end < 0:
                    text += self.body[i:]
                    break
                if text:
                    self.add_text(text)
                    text = ""
                tag_content = self.body[i + 1:end]
                i = end + 1
                tag_name = self.add_tag(tag_content)
                # Raw text elements: consume until the closing tag
                if tag_name in RAW_TEXT_TAGS:
                    close = self.body.lower().find("</" + tag_name, i)
                    if close < 0:
                        close = n
                    raw = self.body[i:close]
                    if raw:
                        self.add_text(raw, raw_text=True)
                    gt = self.body.find(">", close)
                    i = (gt + 1) if gt >= 0 else n
                    self.add_tag("/" + tag_name)
            else:
                j = self.body.find("<", i)
                if j < 0:
                    j = n
                text += self.body[i:j]
                i = j
        if text:
            self.add_text(text)
        return self.finish()

    def _find_tag_end(self, start):
        """Find the '>' ending the tag at start, respecting quoted attrs."""
        i = start + 1
        n = len(self.body)
        quote = None
        while i < n:
            c = self.body[i]
            if quote:
                if c == quote:
                    quote = None
            elif c in ("'", '"'):
                quote = c
            elif c == ">":
                return i
            i += 1
        return -1

    def add_text(self, text, raw_text=False):
        if text.isspace():
            return
        self.implicit_tags(None)
        if not raw_text:
            text = html_entities.unescape(text)
        parent = self.unfinished[-1]
        node = Text(text, parent)
        parent.children.append(node)

    def add_tag(self, tag_content):
        tag, attributes = self.get_attributes(tag_content)
        if not tag or tag.startswith("?"):
            return tag
        self.implicit_tags(tag)
        if tag.startswith("/"):
            self._close_tag(tag[1:])
        elif (tag in SELF_CLOSING_TAGS
              or tag_content.rstrip().endswith("/")) and self.unfinished:
            # (an empty stack means a self-closed root like <html/> —
            # fall through and open it normally, as real browsers do)
            parent = self.unfinished[-1]
            node = Element(tag, attributes, parent)
            parent.children.append(node)
        else:
            # Auto-close <p> and <li> when a sibling opens
            if self.unfinished:
                open_tag = self.unfinished[-1].tag
                if open_tag == "p" and tag in P_CLOSERS:
                    self._close_tag("p")
                elif open_tag == "li" and tag == "li":
                    self._close_tag("li")
            parent = self.unfinished[-1] if self.unfinished else None
            node = Element(tag, attributes, parent)
            self.unfinished.append(node)
        return tag

    def _close_tag(self, tag):
        if len(self.unfinished) == 1:
            return
        # Find matching open tag; ignore stray close tags
        for idx in range(len(self.unfinished) - 1, 0, -1):
            if self.unfinished[idx].tag == tag:
                while len(self.unfinished) > idx:
                    node = self.unfinished.pop()
                    self.unfinished[-1].children.append(node)
                return

    def get_attributes(self, text):
        i = 0
        n = len(text)
        # Tag name: ends at whitespace or the self-closing slash
        # (a leading "/" belongs to a close tag's name).
        if i < n and text[i] == "/":
            i += 1
        while i < n and not text[i].isspace() and text[i] != "/":
            i += 1
        tag = text[:i].lower()
        attributes = {}
        while i < n:
            while i < n and text[i].isspace():
                i += 1
            if i >= n:
                break
            if text[i] == "/":
                i += 1  # stray solidus in a tag is ignored (spec)
                continue
            start = i
            while i < n and text[i] not in ("=", "/") \
                    and not text[i].isspace():
                i += 1
            name = text[start:i].lower()
            if not name:
                i += 1  # stray '=': skip it, keep later attributes
                continue
            while i < n and text[i].isspace():
                i += 1
            value = ""
            if i < n and text[i] == "=":
                i += 1
                while i < n and text[i].isspace():
                    i += 1
                if i < n and text[i] in ("'", '"'):
                    quote = text[i]
                    i += 1
                    start = i
                    while i < n and text[i] != quote:
                        i += 1
                    value = text[start:i]
                    i += 1
                else:
                    start = i
                    while i < n and not text[i].isspace():
                        i += 1
                    value = text[start:i]
            if name not in attributes:  # spec: first duplicate wins
                attributes[name] = html_entities.unescape(value)
        return tag, attributes

    def implicit_tags(self, tag):
        while True:
            open_tags = [node.tag for node in self.unfinished]
            if not open_tags and tag != "html":
                self.add_tag("html")
            elif open_tags == ["html"] and tag not in ("head", "body", "/html"):
                if tag in HEAD_TAGS:
                    self.add_tag("head")
                else:
                    self.add_tag("body")
            elif (open_tags == ["html", "head"]
                  and tag not in {"/head"} | HEAD_TAGS):
                self.add_tag("/head")
            else:
                break

    def finish(self):
        if not self.unfinished:
            self.implicit_tags(None)
        while len(self.unfinished) > 1:
            node = self.unfinished.pop()
            self.unfinished[-1].children.append(node)
        return self.unfinished.pop()


def tree_to_list(tree, out):
    out.append(tree)
    for child in tree.children:
        tree_to_list(child, out)
    return out


def print_tree(node, indent=0):
    print(" " * indent, node)
    for child in node.children:
        print_tree(child, indent + 2)
