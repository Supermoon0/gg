"""Small ES-module front end for the native page loader.

The gg-js parser intentionally consumes classic JavaScript.  This module
handles the browser-facing module layer: static import/export declarations,
literal and computed dynamic imports, URL/import-map resolution, dependency
metadata, namespace wiring, and import.meta.url.
The transformed body runs in an IIFE so top-level declarations do not leak
into the classic-script global scope.
"""

from dataclasses import dataclass
import ast
import json
import re
import urllib.parse


class ModuleSyntaxError(ValueError):
    pass


@dataclass(frozen=True)
class ModuleTransform:
    code: str
    dependencies: tuple
    dependency_types: tuple
    dynamic_dependencies: tuple
    dynamic_dependency_types: tuple
    exports: tuple
    has_top_level_await: bool


class ImportMap:
    """Resolved import-map tables used by static and dynamic imports."""

    def __init__(self, imports=None, scopes=None):
        self.imports = imports or {}
        self.scopes = scopes or {}

    @staticmethod
    def _match(table, normalized):
        if normalized in table:
            target = table[normalized]
            return False if target is None else target
        prefixes = [key for key in table
                    if key.endswith("/") and normalized.startswith(key)]
        if not prefixes:
            return None
        key = max(prefixes, key=len)
        target = table[key]
        if target is None:
            return False
        return target + normalized[len(key):]

    def resolve(self, base_url, specifier):
        normalized = _normalize_map_key(base_url, specifier)
        scope_keys = [scope for scope in self.scopes
                      if str(base_url).startswith(scope)]
        for scope in sorted(scope_keys, key=len, reverse=True):
            match = self._match(self.scopes[scope], normalized)
            if match is False:
                raise ModuleSyntaxError(
                    f"import map blocked specifier: {specifier!r}")
            if match is not None:
                return match
        match = self._match(self.imports, normalized)
        if match is False:
            raise ModuleSyntaxError(
                f"import map blocked specifier: {specifier!r}")
        if match is not None:
            return match
        return _resolve_url_specifier(base_url, specifier)


def _is_url_like(specifier):
    parsed = urllib.parse.urlsplit(specifier)
    return bool(parsed.scheme) or specifier.startswith((
        "./", "../", "/", "//"))


def _resolve_url_specifier(base_url, specifier):
    parsed = urllib.parse.urlsplit(specifier)
    if not parsed.scheme and not specifier.startswith(("./", "../", "/", "//")):
        raise ModuleSyntaxError(
            f"bare module specifier is unsupported: {specifier!r}")
    resolved = urllib.parse.urljoin(str(base_url), specifier)
    scheme = urllib.parse.urlsplit(resolved).scheme.lower()
    if scheme not in ("http", "https", "file", "data"):
        raise ModuleSyntaxError(
            f"unsupported module URL scheme: {scheme or '(relative)'}")
    return urllib.parse.urldefrag(resolved)[0]


def _normalize_map_key(base_url, key):
    return (_resolve_url_specifier(base_url, key)
            if _is_url_like(key) else key)


def _parse_map_table(raw, base_url, label):
    if raw is None:
        return {}
    if not isinstance(raw, dict):
        raise ModuleSyntaxError(f"import map {label} must be an object")
    out = {}
    for raw_key, raw_target in raw.items():
        if not isinstance(raw_key, str):
            continue
        key = _normalize_map_key(base_url, raw_key)
        if raw_target is None:
            out[key] = None
            continue
        if not isinstance(raw_target, str) or not _is_url_like(raw_target):
            raise ModuleSyntaxError(
                f"invalid import map address for {raw_key!r}")
        target = _resolve_url_specifier(base_url, raw_target)
        if key.endswith("/") and not target.endswith("/"):
            raise ModuleSyntaxError(
                f"import map prefix target must end in '/': {raw_key!r}")
        out[key] = target
    return out


def parse_import_maps(sources, base_url):
    """Parse and merge import-map script bodies in document order."""
    imports = {}
    scopes = {}
    for source in sources:
        try:
            raw = json.loads(source)
        except (TypeError, ValueError) as exc:
            raise ModuleSyntaxError(f"invalid import map JSON: {exc}") from exc
        if not isinstance(raw, dict):
            raise ModuleSyntaxError("import map root must be an object")
        imports.update(_parse_map_table(raw.get("imports"), base_url,
                                        "imports"))
        raw_scopes = raw.get("scopes", {})
        if not isinstance(raw_scopes, dict):
            raise ModuleSyntaxError("import map scopes must be an object")
        for raw_scope, table in raw_scopes.items():
            if not isinstance(raw_scope, str) or not _is_url_like(raw_scope):
                raise ModuleSyntaxError(
                    f"invalid import map scope: {raw_scope!r}")
            scope = _resolve_url_specifier(base_url, raw_scope)
            scopes.setdefault(scope, {}).update(
                _parse_map_table(table, base_url, f"scope {raw_scope!r}"))
    return ImportMap(imports, scopes)


def resolve_module_specifier(base_url, specifier, import_map=None):
    """Resolve one browser module specifier and remove its fragment."""
    specifier = specifier.strip()
    if not specifier:
        raise ModuleSyntaxError("empty module specifier")
    if import_map is not None:
        return import_map.resolve(base_url, specifier)
    return _resolve_url_specifier(base_url, specifier)


def module_script_url(page_url, src, node_idx, inline=False):
    """Canonical cache key/base URL for a module script element."""
    base = str(page_url) if page_url else "https://gg.invalid/"
    if inline:
        clean = urllib.parse.urldefrag(base)[0]
        return f"{clean}#gg-inline-module-{node_idx}"
    return resolve_module_specifier(base, src)


def _skip_quoted(source, pos):
    quote = source[pos]
    if quote == "`":
        return _skip_template(source, pos)
    pos += 1
    while pos < len(source):
        ch = source[pos]
        if ch == "\\":
            pos += 2
            continue
        pos += 1
        if ch == quote:
            return pos
    raise ModuleSyntaxError("unterminated string or template literal")


def _skip_template(source, pos):
    """Skip a template literal, descending into ${...} holes. A hole can
    contain strings, comments, and NESTED templates whose backticks must
    not terminate the outer scan — a naive next-backtick skip flips the
    lexer's string/code parity and poisons everything after it."""
    pos += 1  # opening backtick
    while pos < len(source):
        ch = source[pos]
        if ch == "\\":
            pos += 2
            continue
        if ch == "`":
            return pos + 1
        if ch == "$" and source.startswith("${", pos):
            pos = _skip_template_hole(source, pos + 2)
            continue
        pos += 1
    raise ModuleSyntaxError("unterminated string or template literal")


def _skip_template_hole(source, pos):
    """Skip a ${...} hole body; `pos` is just past the opening brace."""
    depth = 1
    while pos < len(source):
        ch = source[pos]
        if ch in "'\"`":
            pos = _skip_quoted(source, pos)
            continue
        if source.startswith(("//", "/*"), pos):
            pos = _skip_space_comments(source, pos)
            continue
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return pos + 1
        pos += 1
    raise ModuleSyntaxError("unterminated template literal hole")


def _skip_space_comments(source, pos):
    while pos < len(source):
        if source[pos].isspace():
            pos += 1
        elif source.startswith("//", pos):
            end = source.find("\n", pos + 2)
            pos = len(source) if end < 0 else end
        elif source.startswith("/*", pos):
            end = source.find("*/", pos + 2)
            if end < 0:
                raise ModuleSyntaxError("unterminated block comment")
            pos = end + 2
        else:
            break
    return pos


def _word_at(source, pos, word):
    if not source.startswith(word, pos):
        return False
    before = source[pos - 1] if pos else ""
    after_pos = pos + len(word)
    after = source[after_pos] if after_pos < len(source) else ""
    ident = lambda ch: bool(ch) and (ch.isalnum() or ch in "_$")
    return not ident(before) and not ident(after)


def _declaration_end(source, pos):
    """End of an exported function/class declaration."""
    pos = _skip_space_comments(source, pos)
    while pos < len(source) and source[pos] != "{":
        if source[pos] in "'\"`":
            pos = _skip_quoted(source, pos)
        elif source.startswith(("//", "/*"), pos):
            pos = _skip_space_comments(source, pos)
        else:
            pos += 1
    if pos >= len(source):
        raise ModuleSyntaxError("exported declaration has no body")
    depth = 0
    while pos < len(source):
        if source[pos] in "'\"`":
            pos = _skip_quoted(source, pos)
            continue
        if source.startswith(("//", "/*"), pos):
            pos = _skip_space_comments(source, pos)
            continue
        ch = source[pos]
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                pos += 1
                pos = _skip_space_comments(source, pos)
                if pos < len(source) and source[pos] == ";":
                    pos += 1
                return pos
        pos += 1
    raise ModuleSyntaxError("unterminated exported declaration")


def _simple_statement_end(source, pos):
    paren = bracket = brace = 0
    last_sig = ""
    while pos < len(source):
        ch = source[pos]
        if ch in "'\"`":
            pos = _skip_quoted(source, pos)
            last_sig = "string"
            continue
        if source.startswith(("//", "/*"), pos):
            before = pos
            pos = _skip_space_comments(source, pos)
            if "\n" in source[before:pos] and not (paren or bracket or brace):
                return pos
            continue
        if ch == "(":
            paren += 1
        elif ch == ")":
            paren -= 1
        elif ch == "[":
            bracket += 1
        elif ch == "]":
            bracket -= 1
        elif ch == "{":
            brace += 1
        elif ch == "}":
            if brace == 0:
                return pos
            brace -= 1
        elif ch == ";" and not (paren or bracket or brace):
            return pos + 1
        elif ch == "\n" and not (paren or bracket or brace):
            if last_sig and last_sig not in (
                    "=", ",", ".", ":", "?", "+", "-", "*", "/",
                    "%", "&", "|", "^", "!", "<", ">", "(", "[", "{"):
                return pos + 1
        if not ch.isspace():
            last_sig = ch
        pos += 1
    return pos


def _module_statement_spans(source):
    spans = []
    pos = 0
    paren = bracket = brace = 0
    while pos < len(source):
        ch = source[pos]
        if ch in "'\"`":
            pos = _skip_quoted(source, pos)
            continue
        if source.startswith(("//", "/*"), pos):
            pos = _skip_space_comments(source, pos)
            continue
        if ch == "/" and _regex_may_start(source, pos):
            # a regex literal may hold quotes (/["']/) or unbalanced
            # brackets/braces (/[{]/) that would otherwise desync the
            # string-skip and depth counters and hide a later export
            pos = _skip_regex(source, pos)
            continue
        if ch == "(":
            paren += 1
        elif ch == ")":
            paren -= 1
        elif ch == "[":
            bracket += 1
        elif ch == "]":
            bracket -= 1
        elif ch == "{":
            brace += 1
        elif ch == "}":
            brace -= 1
        elif not (paren or bracket or brace):
            keyword = None
            if _word_at(source, pos, "import"):
                after = _skip_space_comments(source, pos + 6)
                if after < len(source) and source[after] not in "(.":
                    keyword = "import"
            elif _word_at(source, pos, "export"):
                keyword = "export"
            if keyword:
                after = _skip_space_comments(source, pos + len(keyword))
                tail = source[after:]
                declaration = keyword == "export" and re.match(
                    r"(?:(?:default\s+)?(?:async\s+)?function\b|"
                    r"(?:default\s+)?class\b)", tail)
                end = (_declaration_end(source, after)
                       if declaration else _simple_statement_end(source, after))
                spans.append((pos, end, keyword))
                pos = end
                continue
        pos += 1
    return spans


def _unquote(text):
    try:
        value = ast.literal_eval(text)
    except (SyntaxError, ValueError) as exc:
        raise ModuleSyntaxError(f"invalid module specifier: {text}") from exc
    if not isinstance(value, str):
        raise ModuleSyntaxError("module specifier must be a string")
    return value


def _parse_attribute_object(text):
    """Parse the restricted object literal used by static import attributes."""
    pos = _skip_space_comments(text, 0)
    if pos >= len(text) or text[pos] != "{":
        raise ModuleSyntaxError("import attributes must be an object literal")
    pos += 1
    attributes = {}
    while True:
        pos = _skip_space_comments(text, pos)
        if pos >= len(text):
            raise ModuleSyntaxError("unterminated import attributes")
        if text[pos] == "}":
            pos = _skip_space_comments(text, pos + 1)
            if pos != len(text):
                raise ModuleSyntaxError("invalid text after import attributes")
            return attributes

        if text[pos] in "'\"":
            end = _skip_quoted(text, pos)
            key = _unquote(text[pos:end])
            pos = end
        else:
            match = re.match(r"[A-Za-z_$][\w$]*", text[pos:])
            if match is None:
                raise ModuleSyntaxError("invalid import attribute key")
            key = match.group(0)
            pos += len(key)
        pos = _skip_space_comments(text, pos)
        if pos >= len(text) or text[pos] != ":":
            raise ModuleSyntaxError("import attribute requires ':'")
        pos = _skip_space_comments(text, pos + 1)
        if pos >= len(text) or text[pos] not in "'\"":
            raise ModuleSyntaxError("import attribute values must be strings")
        end = _skip_quoted(text, pos)
        value = _unquote(text[pos:end])
        if key in attributes:
            raise ModuleSyntaxError(f"duplicate import attribute: {key!r}")
        attributes[key] = value
        pos = _skip_space_comments(text, end)
        if pos >= len(text):
            raise ModuleSyntaxError("unterminated import attributes")
        if text[pos] == "}":
            continue
        if text[pos] != ",":
            raise ModuleSyntaxError("import attributes must be comma-separated")
        pos += 1


def _parse_static_import_attributes(tail):
    pos = _skip_space_comments(tail, 0)
    if pos == len(tail):
        return {}
    if not _word_at(tail, pos, "with"):
        raise ModuleSyntaxError("expected 'with' before import attributes")
    return _parse_attribute_object(tail[pos + len("with"):])


def module_type_from_import_attributes(attributes):
    """Validate import attributes and return the requested module type."""
    if attributes is None:
        attributes = {}
    if not isinstance(attributes, dict):
        raise ModuleSyntaxError("import attributes must be an object")
    for key, value in attributes.items():
        if not isinstance(key, str) or not isinstance(value, str):
            raise ModuleSyntaxError("import attribute keys and values must be strings")
        if key != "type":
            raise ModuleSyntaxError(f"unsupported import attribute: {key!r}")
    if "type" not in attributes:
        return "javascript"
    module_type = attributes["type"]
    if module_type != "json":
        raise ModuleSyntaxError(
            f"unsupported module type in import attributes: {module_type!r}")
    return "json"


def _split_commas(text):
    out = []
    start = 0
    paren = bracket = brace = 0
    pos = 0
    while pos < len(text):
        ch = text[pos]
        if ch in "'\"`":
            pos = _skip_quoted(text, pos)
            continue
        if ch == "(":
            paren += 1
        elif ch == ")":
            paren -= 1
        elif ch == "[":
            bracket += 1
        elif ch == "]":
            bracket -= 1
        elif ch == "{":
            brace += 1
        elif ch == "}":
            brace -= 1
        elif ch == "," and not (paren or bracket or brace):
            out.append(text[start:pos].strip())
            start = pos + 1
        pos += 1
    tail = text[start:].strip()
    if tail:
        out.append(tail)
    return out


def _parse_named_list(text):
    pairs = []
    for item in _split_commas(text):
        parts = re.split(r"\s+as\s+", item.strip())
        if len(parts) == 1:
            original = exported = parts[0].strip()
        elif len(parts) == 2:
            original, exported = (part.strip() for part in parts)
        else:
            raise ModuleSyntaxError(f"invalid import/export item: {item}")
        if not original or not exported:
            raise ModuleSyntaxError(f"invalid import/export item: {item}")
        pairs.append((original, exported))
    return pairs


def _source_clause(statement):
    matches = list(re.finditer(
        r"\bfrom\s*((?:'(?:\\.|[^'])*')|(?:\"(?:\\.|[^\"])*\"))",
        statement, re.S))
    if not matches:
        return None, None, None
    match = matches[-1]
    attributes = _parse_static_import_attributes(statement[match.end():])
    return (statement[:match.start()].strip(), _unquote(match.group(1)),
            attributes)


def _declared_names(declaration):
    names = []
    for part in _split_commas(declaration):
        lhs = part.split("=", 1)[0]
        match = re.match(r"\s*([A-Za-z_$][\w$]*)", lhs)
        if match:
            names.append(match.group(1))
    return names


def _regex_may_start(source, pos):
    """Division/regex disambiguation: a `/` starts a regex literal when
    the previous significant token cannot end an expression. Heuristic
    (prev non-space char + keyword check) — the cases minified bundles
    actually produce."""
    j = pos - 1
    while j >= 0 and source[j] in " \t\r\n":
        j -= 1
    if j < 0:
        return True
    prev = source[j]
    if prev in "(,=:[!&|?{};+-*%~^<>":
        return True
    # identifier tail: regex after return/typeof/case/in/of/new/do/else...
    if prev.isalnum() or prev in "_$":
        k = j
        while k >= 0 and (source[k].isalnum() or source[k] in "_$"):
            k -= 1
        word = source[k + 1:j + 1]
        return word in (
            "return", "typeof", "instanceof", "in", "of", "new", "do",
            "else", "void", "delete", "throw", "case", "yield", "await")
    return False


def _skip_regex(source, pos):
    """Skip a regex literal starting at `pos` (the `/`). Handles escapes
    and [...] classes; returns the offset past the flags."""
    pos += 1
    in_class = False
    while pos < len(source):
        ch = source[pos]
        if ch == "\\":
            pos += 2
            continue
        if ch == "\n":
            break  # not a regex after all; treat conservatively
        if in_class:
            if ch == "]":
                in_class = False
        elif ch == "[":
            in_class = True
        elif ch == "/":
            pos += 1
            while pos < len(source) and (source[pos].isalpha()):
                pos += 1
            return pos
        pos += 1
    return pos


def _code_mask(source):
    chars = list(source)
    pos = 0
    while pos < len(source):
        ch = source[pos]
        if ch in "'\"`":
            end = _skip_quoted(source, pos)
            chars[pos:end] = " " * (end - pos)
            pos = end
        elif source.startswith(("//", "/*"), pos):
            end = _skip_space_comments(source, pos)
            chars[pos:end] = " " * (end - pos)
            pos = end
        elif ch == "/" and _regex_may_start(source, pos):
            # regex literals may contain quotes (/["']/) that would
            # otherwise open a bogus string and poison the whole mask
            end = _skip_regex(source, pos)
            chars[pos:end] = " " * (end - pos)
            pos = end
        else:
            pos += 1
    return "".join(chars)


def _replace_module_expressions(source, module_url, import_map=None):
    """Lower import.meta.url and dynamic import() expressions."""
    mask = _code_mask(source)
    replacements = []
    dynamic_dependencies = []
    search_pos = 0
    while True:
        match = re.search(r"\bimport\s*\(", mask[search_pos:])
        if match is None:
            break
        start = search_pos + match.start()
        pos = search_pos + match.end()
        arg_start = pos
        paren = 1
        while pos < len(source) and paren:
            if source[pos] in "'\"`":
                pos = _skip_quoted(source, pos)
                continue
            if source.startswith(("//", "/*"), pos):
                pos = _skip_space_comments(source, pos)
                continue
            if source[pos] == "(":
                paren += 1
            elif source[pos] == ")":
                paren -= 1
                if paren == 0:
                    break
            pos += 1
        if paren:
            raise ModuleSyntaxError("unterminated dynamic import expression")
        close = pos
        arguments = _split_commas(source[arg_start:close])
        if not arguments:
            raise ModuleSyntaxError("dynamic import requires a specifier")
        if len(arguments) > 2:
            raise ModuleSyntaxError("dynamic import accepts at most two arguments")
        expression = arguments[0].strip()
        options = arguments[1].strip() if len(arguments) == 2 else None
        if options == "":
            raise ModuleSyntaxError("dynamic import options cannot be empty")
        literal = False
        if expression and expression[0] in "'\"":
            try:
                literal = _skip_quoted(expression, 0) == len(expression)
            except ModuleSyntaxError:
                literal = False
        if literal and options is None:
            specifier = _unquote(expression)
            resolved = resolve_module_specifier(
                module_url, specifier, import_map=import_map)
            if resolved not in dynamic_dependencies:
                dynamic_dependencies.append(resolved)
            replacement = (
                "globalThis.__ggDynamicImport__("
                + json.dumps(resolved) + ")")
        else:
            replacement = (
                "globalThis.__ggDynamicImportExpression__(("
                + expression + "), " + json.dumps(module_url)
                + (", (" + options + ")" if options is not None else "")
                + ")")
        replacements.append((start, close + 1, replacement))
        search_pos = close + 1
    for start, end, replacement in reversed(replacements):
        source = source[:start] + replacement + source[end:]

    mask = _code_mask(source)
    matches = list(re.finditer(
        r"\bimport\s*\.\s*meta\s*\.\s*url\b", mask))
    for match in reversed(matches):
        source = (source[:match.start()] + json.dumps(module_url)
                  + source[match.end():])
    return source, tuple(dynamic_dependencies)


def _unique_internal_name(source, base, reserved=()):
    """Choose a lowering-only identifier that cannot capture module code."""
    serial = 0
    candidate = base
    reserved = set(reserved)
    while candidate in reserved or re.search(
            r"(?<![\w$])" + re.escape(candidate) + r"(?![\w$])",
            source):
        serial += 1
        candidate = f"{base}{serial}"
    return candidate


@dataclass(frozen=True)
class _JSToken:
    value: str
    start: int
    end: int
    kind: str


_JS_PUNCTUATORS = tuple(sorted((
    ">>>=", "===", "!==", ">>>", "**=", "&&=", "||=", "??=", "...",
    "=>", "==", "!=", "<=", ">=", "++", "--", "&&", "||", "??",
    "?.", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "**",
    "<<", ">>", "::",
), key=len, reverse=True))


def _regex_can_start(previous):
    if previous is None:
        return True
    return previous.value in {
        "(", "[", "{", ",", ";", ":", "?", "=", "=>", "!", "~",
        "+", "-", "*", "%", "&", "|", "^", "&&", "||", "??",
        "return", "throw", "case", "delete", "void", "typeof", "new",
        "in", "of", "yield", "await",
    }


def _skip_regex_literal(source, pos):
    pos += 1
    in_class = False
    while pos < len(source):
        ch = source[pos]
        if ch == "\\":
            pos += 2
            continue
        if ch in "\r\n":
            return pos
        if ch == "[":
            in_class = True
        elif ch == "]":
            in_class = False
        elif ch == "/" and not in_class:
            pos += 1
            while pos < len(source) and (
                    source[pos].isalpha() or source[pos].isdigit()):
                pos += 1
            return pos
        pos += 1
    return pos


def _tokenize_js(source):
    """Tokenize the syntax needed to distinguish identifier references."""
    tokens = []

    def scan(pos, stop_on_template_brace=False):
        brace_depth = 0
        while pos < len(source):
            ch = source[pos]
            if ch.isspace():
                pos += 1
                continue
            if source.startswith("//", pos):
                end = source.find("\n", pos + 2)
                pos = len(source) if end < 0 else end + 1
                continue
            if source.startswith("/*", pos):
                end = source.find("*/", pos + 2)
                if end < 0:
                    raise ModuleSyntaxError("unterminated block comment")
                pos = end + 2
                continue
            if ch in "'\"":
                end = _skip_quoted(source, pos)
                tokens.append(_JSToken(source[pos:end], pos, end, "literal"))
                pos = end
                continue
            if ch == "`":
                template_start = pos
                pos += 1
                while pos < len(source):
                    if source[pos] == "\\":
                        pos += 2
                    elif source[pos] == "`":
                        pos += 1
                        break
                    elif source.startswith("${", pos):
                        pos = scan(pos + 2, True)
                        if pos >= len(source) or source[pos] != "}":
                            raise ModuleSyntaxError(
                                "unterminated template expression")
                        pos += 1
                    else:
                        pos += 1
                else:
                    raise ModuleSyntaxError("unterminated template literal")
                # A template behaves like one literal for slash disambiguation;
                # its expression tokens were appended at their original spans.
                tokens.append(_JSToken(
                    source[template_start:pos], template_start, pos,
                    "template"))
                continue
            if stop_on_template_brace and ch == "}" and brace_depth == 0:
                return pos
            if ch.isalpha() or ch in "_$" or ord(ch) >= 128:
                end = pos + 1
                while end < len(source):
                    nxt = source[end]
                    if not (nxt.isalnum() or nxt in "_$" or ord(nxt) >= 128):
                        break
                    end += 1
                tokens.append(_JSToken(source[pos:end], pos, end, "identifier"))
                pos = end
                continue
            if ch.isdigit():
                end = pos + 1
                while end < len(source) and (
                        source[end].isalnum() or source[end] in "._"):
                    end += 1
                tokens.append(_JSToken(source[pos:end], pos, end, "number"))
                pos = end
                continue
            previous = tokens[-1] if tokens else None
            if ch == "/" and not source.startswith(("//", "/*"), pos) \
                    and _regex_can_start(previous):
                end = _skip_regex_literal(source, pos)
                tokens.append(_JSToken(source[pos:end], pos, end, "regex"))
                pos = end
                continue
            punctuator = next((item for item in _JS_PUNCTUATORS
                               if source.startswith(item, pos)), ch)
            end = pos + len(punctuator)
            tokens.append(_JSToken(punctuator, pos, end, "punctuator"))
            if punctuator == "{":
                brace_depth += 1
            elif punctuator == "}" and brace_depth:
                brace_depth -= 1
            pos = end
        if stop_on_template_brace:
            raise ModuleSyntaxError("unterminated template expression")
        return pos

    scan(0)
    tokens.sort(key=lambda token: (token.start, token.end))
    return tokens


def _delimiter_pairs(tokens):
    pairs = {}
    reverse = {}
    stacks = {"(": [], "[": [], "{": []}
    closing = {")": "(", "]": "[", "}": "{"}
    for idx, token in enumerate(tokens):
        if token.value in stacks:
            stacks[token.value].append(idx)
        elif token.value in closing:
            stack = stacks[closing[token.value]]
            if stack:
                start = stack.pop()
                pairs[start] = idx
                reverse[idx] = start
    return pairs, reverse


def _split_token_ranges(tokens, start, end, separator=","):
    ranges = []
    item_start = start
    paren = bracket = brace = 0
    for idx in range(start, end):
        value = tokens[idx].value
        if value == "(":
            paren += 1
        elif value == ")":
            paren -= 1
        elif value == "[":
            bracket += 1
        elif value == "]":
            bracket -= 1
        elif value == "{":
            brace += 1
        elif value == "}":
            brace -= 1
        elif value == separator and not (paren or bracket or brace):
            ranges.append((item_start, idx))
            item_start = idx + 1
    ranges.append((item_start, end))
    return ranges


def _top_level_token(tokens, start, end, values):
    paren = bracket = brace = 0
    for idx in range(start, end):
        value = tokens[idx].value
        if value == "(":
            paren += 1
        elif value == ")":
            paren -= 1
        elif value == "[":
            bracket += 1
        elif value == "]":
            bracket -= 1
        elif value == "{":
            brace += 1
        elif value == "}":
            brace -= 1
        elif value in values and not (paren or bracket or brace):
            return idx
    return None


def _binding_token_indices(tokens, start, end):
    """Return binding identifiers from one parameter/declarator pattern."""
    while start < end and tokens[start].value == "...":
        start += 1
    assignment = _top_level_token(tokens, start, end, {"="})
    if assignment is not None:
        end = assignment
    if start >= end:
        return []
    if tokens[start].value == "{" and tokens[end - 1].value == "}":
        out = []
        for item_start, item_end in _split_token_ranges(
                tokens, start + 1, end - 1):
            if item_start >= item_end:
                continue
            if tokens[item_start].value == "...":
                out.extend(_binding_token_indices(
                    tokens, item_start + 1, item_end))
                continue
            colon = _top_level_token(
                tokens, item_start, item_end, {":"})
            if colon is None:
                out.extend(_binding_token_indices(
                    tokens, item_start, item_end))
            else:
                out.extend(_binding_token_indices(
                    tokens, colon + 1, item_end))
        return out
    if tokens[start].value == "[" and tokens[end - 1].value == "]":
        out = []
        for item_start, item_end in _split_token_ranges(
                tokens, start + 1, end - 1):
            out.extend(_binding_token_indices(tokens, item_start, item_end))
        return out
    for idx in range(start, end):
        if tokens[idx].kind == "identifier":
            return [idx]
    return []


def _collect_import_shadowing(tokens, imported_names):
    """Collect declaration tokens and source ranges shadowing imports."""
    pairs, reverse = _delimiter_pairs(tokens)
    shadows = {name: [] for name in imported_names}
    binding_indices = set()
    function_bodies = []
    special_block_opens = set()
    parameter_parens = set()
    pending_declarations = []

    def add_bindings(indices, range_start, range_end):
        for binding_idx in indices:
            binding_indices.add(binding_idx)
            name = tokens[binding_idx].value
            if name in shadows:
                shadows[name].append((range_start, range_end))

    for idx, token in enumerate(tokens):
        if token.value == "function":
            paren = next((pos for pos in range(idx + 1, len(tokens))
                          if tokens[pos].value == "("), None)
            if paren is None or paren not in pairs:
                continue
            close = pairs[paren]
            body = close + 1
            if body >= len(tokens) or tokens[body].value != "{" \
                    or body not in pairs:
                continue
            body_close = pairs[body]
            function_bodies.append((body, body_close))
            special_block_opens.add(body)
            parameter_parens.add(paren)
            params = []
            for part_start, part_end in _split_token_ranges(
                    tokens, paren + 1, close):
                params.extend(_binding_token_indices(
                    tokens, part_start, part_end))
            add_bindings(params, tokens[paren].start,
                         tokens[body_close].end)
            name_idx = next((pos for pos in range(idx + 1, paren)
                             if tokens[pos].kind == "identifier"), None)
            if name_idx is not None:
                binding_indices.add(name_idx)
                previous_idx = idx - 1
                if previous_idx >= 0 and tokens[previous_idx].value == "async":
                    previous_idx -= 1
                previous = (tokens[previous_idx].value
                            if previous_idx >= 0 else "")
                expression = previous in {
                    "=", "(", "[", ",", ":", "return", "=>", "?",
                }
                if expression:
                    add_bindings([name_idx], tokens[body].end,
                                 tokens[body_close].start)
                else:
                    pending_declarations.append((name_idx, idx, body_close))
        elif token.value == "catch" and idx + 1 < len(tokens) \
                and tokens[idx + 1].value == "(":
            paren = idx + 1
            if paren not in pairs:
                continue
            close = pairs[paren]
            body = close + 1
            if body >= len(tokens) or tokens[body].value != "{" \
                    or body not in pairs:
                continue
            body_close = pairs[body]
            special_block_opens.add(body)
            parameter_parens.add(paren)
            bindings = _binding_token_indices(tokens, paren + 1, close)
            add_bindings(bindings, tokens[paren].start,
                         tokens[body_close].end)
        elif token.value == "=>":
            if idx == 0:
                continue
            if tokens[idx - 1].value == ")" and idx - 1 in reverse:
                paren = reverse[idx - 1]
                params = []
                for part_start, part_end in _split_token_ranges(
                        tokens, paren + 1, idx - 1):
                    params.extend(_binding_token_indices(
                        tokens, part_start, part_end))
                range_start = tokens[paren].start
            else:
                params = [idx - 1] if tokens[idx - 1].kind == "identifier" else []
                range_start = tokens[idx - 1].start
            body = idx + 1
            if body < len(tokens) and tokens[body].value == "{" \
                    and body in pairs:
                body_close = pairs[body]
                special_block_opens.add(body)
                function_bodies.append((body, body_close))
                add_bindings(params, range_start, tokens[body_close].end)
            else:
                base_containers = {
                    start for start, close in pairs.items()
                    if start < idx < close
                }
                end = body
                while end < len(tokens):
                    containers = {
                        start for start, close in pairs.items()
                        if start < end < close
                    }
                    if tokens[end].value in (",", ";") \
                            and containers == base_containers:
                        break
                    if end in reverse and reverse[end] in base_containers:
                        break
                    end += 1
                range_end = (tokens[end].start if end < len(tokens)
                             else (tokens[-1].end if tokens else 0))
                add_bindings(params, range_start, range_end)
        elif token.value == "class":
            body = next((pos for pos in range(idx + 1, len(tokens))
                         if tokens[pos].value == "{"), None)
            if body is None or body not in pairs:
                continue
            body_close = pairs[body]
            special_block_opens.add(body)
            name_idx = (idx + 1 if idx + 1 < body
                        and tokens[idx + 1].kind == "identifier"
                        and tokens[idx + 1].value != "extends" else None)
            if name_idx is None:
                continue
            binding_indices.add(name_idx)
            previous = tokens[idx - 1].value if idx else ""
            expression = previous in {
                "=", "(", "[", ",", ":", "return", "=>", "?",
            }
            if expression:
                add_bindings([name_idx], tokens[body].end,
                             tokens[body_close].start)
            else:
                pending_declarations.append((name_idx, idx, body_close))

    # Object/class methods have parameter environments even without the
    # ``function`` keyword. Mark their property name as non-reference syntax.
    for paren, close in list(pairs.items()):
        if tokens[paren].value != "(" or paren in parameter_parens:
            continue
        body = close + 1
        if body >= len(tokens) or tokens[body].value != "{" \
                or body not in pairs or paren == 0:
            continue
        previous = tokens[paren - 1].value
        if previous in {"if", "for", "while", "switch", "with", "catch"}:
            continue
        body_close = pairs[body]
        function_bodies.append((body, body_close))
        special_block_opens.add(body)
        parameter_parens.add(paren)
        if tokens[paren - 1].kind == "identifier":
            binding_indices.add(paren - 1)
        params = []
        for part_start, part_end in _split_token_ranges(
                tokens, paren + 1, close):
            params.extend(_binding_token_indices(tokens, part_start, part_end))
        add_bindings(params, tokens[paren].start, tokens[body_close].end)

    def nearest_brace(idx):
        candidates = [(start, close) for start, close in pairs.items()
                      if tokens[start].value == "{" and start < idx < close]
        return max(candidates, default=None, key=lambda pair: pair[0])

    def nearest_function(idx):
        candidates = [(start, close) for start, close in function_bodies
                      if start < idx < close]
        return max(candidates, default=None, key=lambda pair: pair[0])

    def enclosing_for_scope(idx):
        candidates = []
        for start, close in pairs.items():
            if tokens[start].value != "(" or not (start < idx < close):
                continue
            before = start - 1
            if before >= 0 and tokens[before].value == "await":
                before -= 1
            if before < 0 or tokens[before].value != "for":
                continue
            body = close + 1
            if body < len(tokens) and tokens[body].value == "{" \
                    and body in pairs:
                candidates.append((start, pairs[body]))
        return max(candidates, default=None, key=lambda pair: pair[0])

    for binding_idx, declaration_idx, body_close in pending_declarations:
        name = tokens[binding_idx].value
        scope = nearest_brace(declaration_idx)
        if scope is None:
            if name in imported_names:
                raise ModuleSyntaxError(
                    f"import binding {name!r} is redeclared in module scope")
        else:
            add_bindings([binding_idx], tokens[scope[0]].end,
                         tokens[scope[1]].start)

    for idx, token in enumerate(tokens):
        if token.value not in ("let", "const", "var"):
            continue
        if idx and tokens[idx - 1].value in (".", "?."):
            continue
        if idx + 1 < len(tokens) and tokens[idx + 1].value == ":":
            continue
        end = idx + 1
        paren = bracket = brace = 0
        while end < len(tokens):
            value = tokens[end].value
            if value == "(":
                paren += 1
            elif value == ")":
                if paren == 0:
                    break
                paren -= 1
            elif value == "[":
                bracket += 1
            elif value == "]":
                bracket -= 1
            elif value == "{":
                brace += 1
            elif value == "}":
                if brace == 0:
                    break
                brace -= 1
            elif not (paren or bracket or brace) and value in (";", "in", "of"):
                break
            end += 1
        bindings = []
        for part_start, part_end in _split_token_ranges(tokens, idx + 1, end):
            bindings.extend(_binding_token_indices(
                tokens, part_start, part_end))
        if token.value == "var":
            scope = nearest_function(idx)
        else:
            scope = enclosing_for_scope(idx) or nearest_brace(idx)
        if scope is None:
            for binding_idx in bindings:
                binding_indices.add(binding_idx)
                name = tokens[binding_idx].value
                if name in imported_names:
                    raise ModuleSyntaxError(
                        f"import binding {name!r} is redeclared in module scope")
            continue
        add_bindings(bindings, tokens[scope[0]].end,
                     tokens[scope[1]].start)

    return shadows, binding_indices, pairs, special_block_opens


def _is_object_shorthand(tokens, idx, pairs, special_block_opens):
    previous = tokens[idx - 1].value if idx else ""
    following = tokens[idx + 1].value if idx + 1 < len(tokens) else ""
    if previous not in ("{", ",") or following not in ("}", ","):
        return False
    braces = [(start, close) for start, close in pairs.items()
              if tokens[start].value == "{" and start < idx < close]
    if not braces:
        return False
    opening, _closing = max(braces, key=lambda pair: pair[0])
    if opening in special_block_opens:
        return False
    before = tokens[opening - 1].value if opening else ""
    if before in (")", "else", "try", "finally", "do", "=>"):
        return False
    return True


def _replace_lexical_import_reads(source, bindings):
    """Rewrite only unshadowed imported identifier references."""
    if not bindings:
        return source
    tokens = _tokenize_js(source)
    shadows, binding_indices, pairs, block_opens = (
        _collect_import_shadowing(tokens, set(bindings)))
    replacements = []
    for idx, token in enumerate(tokens):
        name = token.value
        if token.kind != "identifier" or name not in bindings:
            continue
        if idx in binding_indices or any(
                start <= token.start < end for start, end in shadows[name]):
            continue
        previous = tokens[idx - 1].value if idx else ""
        following = tokens[idx + 1].value if idx + 1 < len(tokens) else ""
        if previous in (".", "?.", "break", "continue") or following == ":":
            continue
        expression = bindings[name]
        replacement = (f"{name}:({expression})"
                       if _is_object_shorthand(
                           tokens, idx, pairs, block_opens)
                       else f"({expression})")
        replacements.append((token.start, token.end, replacement))
    for start, end, replacement in reversed(replacements):
        source = source[:start] + replacement + source[end:]
    return source


def transform_module(source, module_url, module_key=None, import_map=None):
    """Lower static ESM syntax to one isolated gg-js program."""
    module_key = module_url if module_key is None else module_key
    replacements = []
    dependencies = []
    dependency_types = []
    imports = []
    local_exports = []
    reexports = []
    star_exports = []
    default_serial = 0

    def resolve(specifier, attributes=None):
        url = resolve_module_specifier(
            module_url, specifier, import_map=import_map)
        module_type = module_type_from_import_attributes(attributes)
        if url in dependencies:
            previous = dependency_types[dependencies.index(url)]
            if previous != module_type:
                raise ModuleSyntaxError(
                    f"module {url!r} requested as both {previous} and "
                    f"{module_type}")
        else:
            dependencies.append(url)
            dependency_types.append(module_type)
        return url

    for start, end, keyword in _module_statement_spans(source):
        statement = source[start:end].strip()
        statement = statement[:-1].rstrip() \
            if statement.endswith(";") else statement
        if keyword == "import":
            side_effect = re.match(
                r"import\s*((?:'(?:\\.|[^'])*')|"
                r"(?:\"(?:\\.|[^\"])*\"))", statement, re.S)
            if side_effect:
                attributes = _parse_static_import_attributes(
                    statement[side_effect.end():])
                resolve(_unquote(side_effect.group(1)), attributes)
                replacements.append((start, end, ""))
                continue
            clause, specifier, attributes = _source_clause(statement)
            if specifier is None:
                raise ModuleSyntaxError(f"invalid import declaration: {statement}")
            dep = resolve(specifier, attributes)
            clause = clause[len("import"):].strip()
            pieces = _split_commas(clause)
            if pieces and not pieces[0].startswith(("{", "*")):
                imports.append((pieces.pop(0), dep, "default"))
            rest = ",".join(pieces).strip()
            if rest.startswith("{") and rest.endswith("}"):
                for imported, local in _parse_named_list(rest[1:-1]):
                    imports.append((local, dep, imported))
            elif rest.startswith("*"):
                match = re.fullmatch(
                    r"\*\s+as\s+([A-Za-z_$][\w$]*)", rest)
                if not match:
                    raise ModuleSyntaxError(f"invalid namespace import: {rest}")
                imports.append((match.group(1), dep, None))
            elif rest:
                raise ModuleSyntaxError(f"invalid import clause: {rest}")
            replacements.append((start, end, ""))
            continue

        body = statement[len("export"):].strip()
        star = re.fullmatch(
            r"\*\s*(?:as\s+([A-Za-z_$][\w$]*)\s+)?from\s+"
            r"((?:'(?:\\.|[^'])*')|(?:\"(?:\\.|[^\"])*\"))(.*)",
            body, re.S)
        if star:
            attributes = _parse_static_import_attributes(star.group(3))
            dep = resolve(_unquote(star.group(2)), attributes)
            if star.group(1):
                reexports.append((star.group(1), dep, None))
            else:
                star_exports.append(dep)
            replacements.append((start, end, ""))
            continue
        if body.startswith("{"):
            close = body.find("}")
            if close < 0:
                raise ModuleSyntaxError("unterminated export list")
            pairs = _parse_named_list(body[1:close])
            rest = body[close + 1:].strip()
            if rest:
                prefix, specifier, attributes = _source_clause(rest)
                if specifier is None or prefix:
                    raise ModuleSyntaxError(f"invalid re-export: {statement}")
                dep = resolve(specifier, attributes)
                for imported, exported in pairs:
                    reexports.append((exported, dep, imported))
            else:
                for local, exported in pairs:
                    local_exports.append((exported, local))
            replacements.append((start, end, ""))
            continue
        decl = re.match(r"(const|let|var)\s+(.+)$", body, re.S)
        if decl:
            for name in _declared_names(decl.group(2)):
                local_exports.append((name, name))
            replacements.append((start, start + len("export"), ""))
            continue
        named_decl = re.match(
            r"(?:(async)\s+)?(function|class)\s+"
            r"([A-Za-z_$][\w$]*)\b", body, re.S)
        if named_decl:
            name = named_decl.group(3)
            local_exports.append((name, name))
            replacements.append((start, start + len("export"), ""))
            continue
        if body.startswith("default"):
            value = body[len("default"):].strip()
            named = re.match(
                r"(?:(?:async\s+)?function|class)\s+"
                r"([A-Za-z_$][\w$]*)\b", value, re.S)
            if named:
                local = named.group(1)
                replacement = value
            else:
                default_serial += 1
                local = _unique_internal_name(
                    source, f"__ggDefaultExport{default_serial}__")
                replacement = f"var {local} = {value};"
            local_exports.append(("default", local))
            replacements.append((start, end, replacement))
            continue
        raise ModuleSyntaxError(f"unsupported export declaration: {statement}")

    transformed = source
    for start, end, replacement in reversed(replacements):
        transformed = transformed[:start] + replacement + transformed[end:]
    transformed, dynamic_dependencies = _replace_module_expressions(
        transformed, module_url, import_map=import_map)
    internal_names = []
    module_var = _unique_internal_name(
        source, "__ggModule__", internal_names)
    internal_names.append(module_var)
    imports_var = _unique_internal_name(
        source, "__ggModuleImports__", internal_names)
    internal_names.append(imports_var)
    object_var = _unique_internal_name(
        source, "__ggModuleObject__", internal_names)
    internal_names.append(object_var)
    getter_var = _unique_internal_name(
        source, "__ggModuleImportGetters__", internal_names)
    raw_imports = {}
    for local, dep, imported in imports:
        if local in ("eval", "arguments"):
            raise ModuleSyntaxError(
                f"invalid import binding in strict module code: {local!r}")
        if local in raw_imports:
            raise ModuleSyntaxError(f"duplicate import binding: {local!r}")
        dep_key = json.dumps(dep)
        if imported is None:
            raw_imports[local] = f"{imports_var}[{dep_key}]"
        else:
            raw_imports[local] = (
                f"{imports_var}[{dep_key}][{json.dumps(imported)}]")
    prelude = [f"var {getter_var} = {{}};"]
    for local, value in raw_imports.items():
        prelude.append(
            f"{getter_var}[{json.dumps(local)}] = function(){{return "
            f"{value};}};")
    live_imports = {
        local: f"{getter_var}[{json.dumps(local)}]()"
        for local in raw_imports
    }
    transformed = _replace_lexical_import_reads(
        transformed, live_imports)
    # The gg-js compiler accepts await inside async functions. Wrapping the
    # isolated module body in an async IIFE gives module-level await the same
    # continuation machinery without leaking bindings into classic scripts.
    has_top_level_await = bool(re.search(
        r"\bawait\b", _code_mask(transformed)))

    epilogue = []
    export_names = []
    for exported, local in local_exports:
        export_names.append(exported)
        value = live_imports.get(local, local)
        epilogue.append(
            f"{object_var}.defineProperty({module_var}, "
            + json.dumps(exported)
            + ", {enumerable:true, get:function(){return "
            + value + ";}});")
    for exported, dep, imported in reexports:
        export_names.append(exported)
        dep_key = json.dumps(dep)
        if imported is None:
            value = f"{imports_var}[{dep_key}]"
        else:
            value = (f"{imports_var}[{dep_key}]"
                     f"[{json.dumps(imported)}]")
        epilogue.append(
            f"{object_var}.defineProperty({module_var}, "
            + json.dumps(exported)
            + ", {enumerable:true, get:function(){return "
            + value + ";}});")
    for dep in star_exports:
        epilogue.append(
            f"{object_var}.getOwnPropertyNames("
            f"{imports_var}[{json.dumps(dep)}])"
            ".forEach("
            "function(__ggExportKey__){if(__ggExportKey__!==\"default\")"
            f"{{{module_var}[__ggExportKey__]="
            f"{imports_var}[{json.dumps(dep)}][__ggExportKey__];"
            "}});")

    wrapper = "async function" if has_top_level_await else "function"
    code = "\n".join([
        f"({wrapper}({module_var}, {imports_var}, {object_var}){{",
        '"use strict";',
        *prelude,
        transformed,
        *epilogue,
        "})(globalThis.__ggModuleRegistry__["
        + json.dumps(module_key)
        + "], globalThis.__ggModuleRegistry__, globalThis.Object);",
    ])
    return ModuleTransform(
        code=code,
        dependencies=tuple(dependencies),
        dependency_types=tuple(dependency_types),
        dynamic_dependencies=dynamic_dependencies,
        dynamic_dependency_types=("javascript",) * len(dynamic_dependencies),
        exports=tuple(dict.fromkeys(export_names)),
        has_top_level_await=has_top_level_await,
    )


def transform_json_module(source, module_url, module_key=None):
    """Create a single-default-export module from a fetched JSON resource."""
    try:
        value = json.loads(source)
    except (TypeError, ValueError) as exc:
        raise ModuleSyntaxError(f"invalid JSON module {module_url}: {exc}") from exc
    canonical = json.dumps(value, ensure_ascii=True, separators=(",", ":"))
    javascript = "export default JSON.parse(" + json.dumps(canonical) + ");"
    return transform_module(javascript, module_url, module_key=module_key)
