"""Static ES-module linker v1.

`<script type=module>` sources are linked ahead of execution into one
classic script: static `import ... from` dependencies are fetched,
transformed, and inlined depth-first (deduplicated by resolved URL),
each wrapped in an IIFE that registers its exports on `__ggmod`;
import statements in the consumer become plain var reads.

v1 gates (out of scope, left for the engine to error on honestly):
dynamic import(), top-level await, `export ... from` re-exports,
live bindings (exports are snapshotted when the module finishes),
and import.meta.
"""

import re

_FROM_RE = re.compile(
    r"""^[ \t]*import[ \t]+(.+?)[ \t]+from[ \t]*["']([^"']+)["'][ \t]*;?[ \t]*$""",
    re.M)
_BARE_RE = re.compile(
    r"""^[ \t]*import[ \t]*["']([^"']+)["'][ \t]*;?[ \t]*$""", re.M)
_EXP_DEFAULT_FN = re.compile(
    r"^[ \t]*export[ \t]+default[ \t]+(function|class)([ \t]+([\w$]+))?",
    re.M)
_EXP_DEFAULT = re.compile(r"^[ \t]*export[ \t]+default[ \t]+", re.M)
_EXP_DECL = re.compile(
    r"^[ \t]*export[ \t]+(var|let|const|function|class)[ \t]+([\w$]+)",
    re.M)
_EXP_LIST = re.compile(r"^[ \t]*export[ \t]*\{([^}]*)\}[ \t]*;?[ \t]*$",
                       re.M)


def _mod_key(url_str):
    return url_str.replace("\\", "\\\\").replace("'", "\\'")


def _rewrite_imports(code, base_url, loader, chunks, seen):
    """Replace import statements with var reads off __ggmod, loading
    (and inlining) each dependency first."""

    def load_dep(spec):
        text, resolved = loader(spec, base_url)
        key = _mod_key(resolved)
        if resolved in seen:
            return key  # already inlined (or in progress: cycle)
        seen.add(resolved)
        if text is None:
            chunks.append(f"__ggmod['{key}'] = {{}}; "
                          f"/* module fetch failed: {spec} */")
            return key
        body = _rewrite_imports(text, resolved, loader, chunks, seen)
        body = _rewrite_exports(body)
        chunks.append(
            f"__ggmod['{key}'] = (function () {{ var __exp = {{}};\n"
            f"{body}\n"
            f"return __exp; }})();")
        return key

    def from_repl(m):
        clause, spec = m.group(1), m.group(2)
        key = load_dep(spec)
        decls = []
        rest = clause.strip()
        # `d, { a }` / `d, * as ns` — split the default part off
        while rest:
            if rest.startswith("{"):
                names, _, rest = rest[1:].partition("}")
                for item in names.split(","):
                    item = item.strip()
                    if not item:
                        continue
                    if " as " in item:
                        orig, alias = [
                            s.strip() for s in item.split(" as ", 1)]
                    else:
                        orig = alias = item
                    decls.append(
                        f"var {alias} = __ggmod['{key}'].{orig};")
                rest = rest.lstrip(", \t")
            elif rest.startswith("*"):
                alias = rest.split("as", 1)[1].strip().rstrip(",")
                decls.append(f"var {alias} = __ggmod['{key}'];")
                rest = ""
            else:
                default, _, rest = rest.partition(",")
                decls.append(
                    f"var {default.strip()} = "
                    f"__ggmod['{key}'].default;")
                rest = rest.strip()
        return " ".join(decls)

    def bare_repl(m):
        load_dep(m.group(1))
        return ""

    code = _FROM_RE.sub(from_repl, code)
    code = _BARE_RE.sub(bare_repl, code)
    return code


def _rewrite_exports(code):
    """Turn export statements into plain declarations plus __exp
    registrations (appended, so function/var reads resolve)."""
    tail = []

    def decl_repl(m):
        kw, name = m.group(1), m.group(2)
        tail.append(f"__exp.{name} = {name};")
        return m.group(0).replace("export", "", 1).lstrip()

    def default_fn_repl(m):
        kw, name = m.group(1), m.group(3)
        if name:
            tail.append(f"__exp.default = {name};")
            return f"{kw} {name}"
        return "__exp.default = " + kw

    def list_repl(m):
        for item in m.group(1).split(","):
            item = item.strip()
            if not item:
                continue
            if " as " in item:
                orig, alias = [s.strip() for s in item.split(" as ", 1)]
            else:
                orig = alias = item
            tail.append(f"__exp.{alias} = {orig};")
        return ""

    code = _EXP_DEFAULT_FN.sub(default_fn_repl, code)
    code = _EXP_DEFAULT.sub("__exp.default = ", code)
    code = _EXP_LIST.sub(list_repl, code)
    code = _EXP_DECL.sub(decl_repl, code)
    if tail:
        code += "\n" + "\n".join(tail)
    return code


def link(code, base_url, loader):
    """Link one module entry script into a classic script.

    loader(spec, base_url) -> (text or None, resolved_url_str) fetches
    a dependency; base_url is the entry's own URL (or the page URL for
    inline modules) and may be None for pure-inline graphs.
    """
    chunks = ["var __ggmod = typeof __ggmod !== 'undefined' "
              "? __ggmod : {};"]
    seen = set()
    body = _rewrite_imports(code, base_url, loader, chunks, seen)
    body = _strip_entry_exports(body)
    chunks.append(body)
    return "\n".join(chunks)


def _strip_entry_exports(code):
    """The entry module's exports go nowhere — keep the declarations,
    drop the export keywords/statements."""
    code = _EXP_DEFAULT_FN.sub(
        lambda m: (f"{m.group(1)} {m.group(3)}" if m.group(3)
                   else f"void {m.group(1)}"), code)
    code = _EXP_DEFAULT.sub("void ", code)
    code = _EXP_LIST.sub("", code)
    code = _EXP_DECL.sub(
        lambda m: m.group(0).replace("export", "", 1).lstrip(), code)
    return code


def is_module_source(code):
    """Cheap sniff used by callers that lack type= information."""
    return bool(_FROM_RE.search(code) or _BARE_RE.search(code)
                or _EXP_DECL.search(code) or _EXP_DEFAULT.search(code))
