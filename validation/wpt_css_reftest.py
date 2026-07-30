"""Run the reftest half of web-platform-tests' CSS suite.

`wpt_css_conformance.py` scores the 7293 files that judge themselves in
script. The other 16368 do not: a reftest renders a page, renders a
reference page written a different way, and passes when the two look
the same. That needs pixels, which is why they have been unscored — and
why every rendering fix so far has measured as zero. text-transform,
letter-spacing and link underlines all landed with the testharness
score unchanged, because almost nothing there looks at what was drawn.

The comparison is between two renders from *this* engine, never against
a stored image, so the harness needs no golden files and no font
matching with anyone else. If the engine draws both pages the same way,
it passes — which is exactly what the reftest is asserting.

    <link rel="match" href="...">      pass when identical
    <link rel="mismatch" href="...">   pass when they differ

A tolerance exists but is deliberately tiny (default: no differing
pixels). Reftests are written so that a correct engine produces an
exact match; anti-aliasing differences do not arise here because both
sides go through the same rasterizer.

The gate is the same shape as the other two: baseline of passing ids,
content fingerprint, resumable partials, one chunk per process.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

DEFAULT_BASELINE = (
    Path(__file__).with_name("baselines") / "wpt_css_reftest.json")
DEFAULT_OUTPUT = Path(__file__).with_name("out") / "wpt_css_reftest.json"

WIDTH, HEIGHT = 800, 600
JS_BUDGET = 1.0   # seconds per document; a reftest's scripts are setup

_REF_RE = re.compile(
    r"""<link\s[^>]*rel\s*=\s*["']?(match|mismatch)["']?[^>]*>""",
    re.I | re.S)
_HREF_RE = re.compile(r"""href\s*=\s*["']([^"']+)["']""", re.I)
_FUZZY_RE = re.compile(
    r"""<meta\s[^>]*name\s*=\s*["']?fuzzy["']?[^>]*>""", re.I | re.S)
_CONTENT_RE = re.compile(r"""content\s*=\s*["']([^"']*)["']""", re.I)
_RANGE_RE = re.compile(r"(\d+)\s*(?:-\s*(\d+))?")


def fuzzy(path: Path):
    """(max channel difference, differing pixels) a test declares it may
    still have, or (0, 0).

    A reftest that antialiases a curve, or rounds a subpixel edge, says
    so in the file: `<meta name=fuzzy content="maxDifference=0-1;
    totalPixels=0-6000">`. Ignoring that and demanding exact equality
    fails the test for the reason its author already excused, so 808
    files in this corpus were being scored against a stricter bar than
    the one they were written to.
    """
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return 0, 0
    diff = pixels = 0
    for m in _FUZZY_RE.finditer(text):
        content = _CONTENT_RE.search(m.group(0))
        if not content:
            continue
        # a per-reference form ("ref.html:0-1;0-600") narrows the meta to
        # one reference; the limits are the same either way, and taking
        # the loosest keeps this from being stricter than the file asks
        spec = content.group(1).rsplit(":", 1)[-1]
        for part in spec.split(";"):
            r = _RANGE_RE.search(part)
            if not r:
                continue
            hi = int(r.group(2) if r.group(2) is not None else r.group(1))
            if "totalpixels" in part.casefold():
                pixels = max(pixels, hi)
            elif "maxdifference" in part.casefold():
                diff = max(diff, hi)
            elif "maxdifference" not in spec.casefold():
                # bare "0-1;0-600": difference first, then pixel count
                if diff == 0:
                    diff = hi
                else:
                    pixels = max(pixels, hi)
    return diff, pixels


def references(path: Path):
    """[(kind, reference path)] declared by a reftest, in file order."""
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    out = []
    for m in _REF_RE.finditer(text):
        href = _HREF_RE.search(m.group(0))
        if not href:
            continue
        target = href.group(1).split("#")[0]
        if not target or ":" in target.split("/")[0]:
            continue  # http:, data:, about:blank — nothing local to read
        out.append((m.group(1).lower(), target))
    return out


def discover(corpus: Path, filters, limit):
    css = corpus / "css"
    if not css.is_dir():
        raise SystemExit(f"no css/ in {corpus}")
    out = []
    for path in sorted(css.rglob("*.html")):
        rel = path.relative_to(corpus).as_posix()
        if filters and not any(f in rel for f in filters):
            continue
        if not references(path):
            continue
        out.append(rel)
        if limit and len(out) >= limit:
            break
    return out


def resolve(href: str, base: Path, corpus: Path):
    """Local file a reftest's href names, or None if it names none.

    WPT is served with the corpus root as `/`, so a root-relative href
    is corpus-relative and everything else is relative to the page.
    Anything that escapes the corpus is refused rather than read: the
    corpus is full of `../../support/...` and one `../../../../etc`
    would have the harness reading the machine.
    """
    href = href.split("#")[0].split("?")[0].strip()
    if not href or ":" in href.split("/")[0]:
        return None
    try:
        path = ((corpus / href.lstrip("/")) if href.startswith("/")
                else (base / href)).resolve()
        path.relative_to(corpus.resolve())
    except (ValueError, OSError):
        return None
    return path if path.is_file() else None


def _text_fetcher(base: Path, corpus: Path):
    def fetch(hrefs):
        out = {}
        for href in hrefs:
            path = resolve(href, base, corpus)
            if path is None:
                continue
            try:
                out[href] = path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                pass
        return out
    return fetch


def _bytes_fetcher(base: Path, corpus: Path):
    def fetch(urls):
        out = {}
        for u in urls:
            path = resolve(u, base, corpus)
            if path is None:
                continue
            try:
                out[u] = path.read_bytes()
            except OSError:
                pass
        return out
    return fetch


_fonts_seen = set()


def _load_fonts(css_sources, base: Path, corpus: Path):
    """Register the @font-face fonts a page asks for.

    Ahem is the reason this exists: 3444 files in the CSS corpus set
    `font-family: Ahem`, whose every glyph is a solid em square, so a
    reftest written against it asserts an exact pixel layout instead
    of whatever the fallback font happens to measure. Without it the
    test and its reference both fall back and the comparison is
    testing the fallback.
    """
    from browser import textengine, webfonts
    from browser import layout as layout_mod

    loaded = 0
    for family, bold, italic, url, sheet in webfonts.parse_font_faces(
            css_sources):
        # a relative src belongs to the sheet that wrote it
        sheet_path = resolve(sheet, base, corpus) if sheet else None
        font = resolve(url, sheet_path.parent if sheet_path else base, corpus)
        if font is None:
            continue
        key = (family, bold, italic, str(font))
        if key in _fonts_seen:
            continue
        try:
            data = font.read_bytes()
        except OSError:
            continue
        if textengine.engine().load_font(family, bold, italic, data):
            _fonts_seen.add(key)
            loaded += 1
    if loaded:
        layout_mod._FONT_CACHE.clear()


def render(page: Path, corpus: Path):
    """Raw RGB bytes for one page, or None if it will not render."""
    from browser import native, textengine
    from browser.html_parser import Element, tree_to_list
    from browser.layout import DocumentLayout, paint_tree

    html = page.read_text(encoding="utf-8", errors="replace")
    base = page.parent

    # Every page starts from an empty image store. The background cache
    # is keyed by the raw CSS url, so `url(support/1x1.png)` from two
    # different directories would otherwise collide — and one worker
    # renders thousands of pages from all over the corpus.
    engine = textengine.engine()
    engine.clear_images()
    textengine.clear_image_caches()

    # Scripts run. A reftest that sets up its own case in JS — the whole
    # css-ui widget suite does, and it is 800 comparisons on its own —
    # renders as an empty page without them. The budget is per document
    # and there are two documents per comparison, so it is small: these
    # are setup scripts, not applications.
    nodes, doc, css_sources, _logs = native.load_document(
        html, _text_fetcher(base, corpus), _text_fetcher(base, corpus),
        js_budget=JS_BUDGET)

    fetch_bytes = _bytes_fetcher(base, corpus)
    _load_fonts(css_sources, base, corpus)
    img_nodes = [n for n in tree_to_list(nodes, [])
                 if isinstance(n, Element) and n.tag == "img"
                 and n.attributes.get("src")]
    if img_nodes:
        raw = fetch_bytes(sorted({n.attributes["src"] for n in img_nodes}))
        decoded = {}
        for src, data in raw.items():
            try:
                decoded[src] = engine.load_image(data)
            except Exception:
                decoded[src] = None
        for node in img_nodes:
            node._img = decoded.get(node.attributes["src"])
    textengine.load_svgs(nodes)
    textengine.load_background_images(nodes, fetch_bytes)

    document = DocumentLayout(nodes)
    document.layout(WIDTH, HEIGHT)
    cmds = [c.native(0) for c in paint_tree(document, [])]
    return bytes(engine.render_raw(
        WIDTH, HEIGHT, (255, 255, 255), cmds))


def differing_pixels(a: bytes, b: bytes, max_channel: int = 0) -> int:
    """Pixels differing by more than `max_channel` on any channel."""
    if len(a) != len(b):
        return max(len(a), len(b))
    n = 0
    for i in range(0, len(a), 3):
        if (abs(a[i] - b[i]) > max_channel
                or abs(a[i + 1] - b[i + 1]) > max_channel
                or abs(a[i + 2] - b[i + 2]) > max_channel):
            n += 1
    return n


def run_one(corpus: Path, rel: str, tolerance: int):
    path = corpus / rel
    refs = references(path)
    if not refs:
        return [(rel, False, "no reference")]
    try:
        got = render(path, corpus)
    except Exception as exc:
        return [(rel, False, f"test render failed: {type(exc).__name__}")]
    rows = []
    for kind, target in refs:
        ident = f"{rel}#{kind}:{target}"
        ref_path = resolve(target, path.parent, corpus)
        if ref_path is None:
            rows.append((ident, False, "reference is missing"))
            continue
        try:
            want = render(ref_path, corpus)
        except Exception as exc:
            rows.append((ident, False,
                         f"reference render failed: {type(exc).__name__}"))
            continue
        max_channel, allowed = fuzzy(path)
        diff = differing_pixels(got, want, max_channel)
        if kind == "match":
            ok = diff <= max(tolerance, allowed)
            why = "" if ok else f"{diff} pixels differ"
        else:
            ok = diff > max(tolerance, allowed)
            why = "" if ok else "renders identically to the mismatch ref"
        rows.append((ident, ok, why))
    return rows


def _run_chunk(corpus: str, rels, tolerance: int):
    out = []
    for rel in rels:
        out.extend(run_one(Path(corpus), rel, tolerance))
    return out


def _worker_main():
    payload = json.loads(sys.stdin.read())
    print(json.dumps(_run_chunk(
        payload["corpus"], payload["rels"], payload["tolerance"])))
    return 0


def load_partial(path: Path):
    done = {}
    if not path or not path.is_file():
        return done
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        done[rec["file"]] = [tuple(r) for r in rec["results"]]
    return done


def record(sink, rels, rows):
    by_file = {r: [] for r in rels}
    for ident, ok, why in rows:
        by_file.setdefault(ident.split("#")[0], []).append((ident, ok, why))
    for name, got in by_file.items():
        sink.write(json.dumps({"file": name, "results": got}) + "\n")
    sink.flush()


def run_all(corpus, rels, jobs, tolerance, chunk_size=32, partial=None):
    import concurrent.futures as cf

    def run_chunk(chunk):
        proc = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), "--worker"],
            input=json.dumps({"corpus": str(corpus), "rels": chunk,
                              "tolerance": tolerance}),
            capture_output=True, text=True,
            timeout=len(chunk) * 30 + 60)
        if proc.returncode != 0 or not proc.stdout.strip():
            raise RuntimeError(proc.stderr.strip()[-200:] or "no output")
        return json.loads(proc.stdout)

    done = load_partial(partial) if partial else {}
    results = [r for rs in done.values() for r in rs]
    todo = [r for r in rels if r not in done]
    pending = [todo[i:i + chunk_size]
               for i in range(0, len(todo), chunk_size)]
    crashes = 0
    sink = partial.open("a", encoding="utf-8") if partial else None
    while pending:
        nxt = []
        with cf.ThreadPoolExecutor(max_workers=jobs) as pool:
            futs = {pool.submit(run_chunk, c): c for c in pending}
            for fut in cf.as_completed(futs):
                chunk = futs[fut]
                try:
                    got = [(i, ok, why) for i, ok, why in fut.result()]
                    results.extend(got)
                    if sink is not None:
                        record(sink, chunk, got)
                except Exception:
                    if len(chunk) == 1:
                        got = [(chunk[0], False, "the engine died on this")]
                        results.extend(got)
                        if sink is not None:
                            record(sink, chunk, got)
                        crashes += 1
                    else:
                        half = len(chunk) // 2
                        nxt.append(chunk[:half])
                        nxt.append(chunk[half:])
        pending = nxt
    if sink is not None:
        sink.close()
    return results, crashes


def fingerprint(corpus: Path, rels) -> str:
    h = hashlib.sha256()
    for rel in rels:
        h.update(rel.encode())
        h.update(b"\0")
        try:
            h.update(hashlib.sha256((corpus / rel).read_bytes()).digest())
        except OSError:
            h.update(b"missing")
    return h.hexdigest()


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--corpus", type=Path, required=True)
    p.add_argument("--file", dest="filters", action="append", default=[])
    p.add_argument("--limit", type=int)
    p.add_argument("--jobs", type=int, default=4)
    p.add_argument("--tolerance", type=int, default=0,
                   help="differing pixels a match may still have")
    p.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    p.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    p.add_argument("--update-baseline", action="store_true")
    p.add_argument("--partial", type=Path)
    p.add_argument("--failure-samples", type=int, default=40)
    args = p.parse_args(argv)

    corpus = args.corpus.resolve()
    rels = discover(corpus, args.filters, args.limit)
    if not rels:
        print("no reftests selected", file=sys.stderr)
        return 2
    subset = bool(args.filters or args.limit)

    results, crashes = run_all(corpus, rels, args.jobs, args.tolerance,
                               partial=args.partial)
    passed = sorted(i for i, ok, _ in results if ok)
    failures = [(i, why) for i, ok, why in results if not ok]
    current = set(passed)

    lines = [f"wpt-css-reftest: {len(passed)}/{len(results)} pass "
             f"({len(passed) / len(results):.2%}) over {len(rels)} files"]
    if crashes:
        lines.append(f"{crashes} file(s) took the process down with them")

    exit_code = 0
    regressions = []
    if subset:
        lines.append("subset run: baseline comparison off")
    else:
        digest = fingerprint(corpus, rels)
        if args.update_baseline:
            args.baseline.parent.mkdir(parents=True, exist_ok=True)
            args.baseline.write_text(json.dumps({
                "schema_version": 1,
                "corpus_sha256": digest,
                "tolerance": args.tolerance,
                "passed": passed,
            }, indent=1) + "\n", encoding="utf-8")
            lines.append(f"baseline recorded at {args.baseline} "
                         "(comparison skipped for this run)")
        elif not args.baseline.is_file():
            lines.append(f"no baseline at {args.baseline}; run with "
                         "--update-baseline to record one")
            exit_code = 2
        else:
            base = json.loads(args.baseline.read_text(encoding="utf-8"))
            if base.get("corpus_sha256") != digest:
                lines.append(
                    "corpus does not match the baseline (content hash "
                    f"{digest[:12]} vs "
                    f"{str(base.get('corpus_sha256'))[:12]})")
                exit_code = 2
            else:
                known = set(base.get("passed", []))
                regressions = sorted(known - current)
                lines.append(
                    f"baseline: regressions={len(regressions)}, "
                    f"improvements={len(current - known)}, "
                    f"passing {len(current)}/{len(results)} "
                    f"({len(current) / len(results):.2%})")
                if regressions:
                    exit_code = 1

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps({
        "schema_version": 1,
        "engine": "gg",
        "corpus": str(corpus),
        "subset": subset,
        "tolerance": args.tolerance,
        "counts": {
            "files": len(rels), "comparisons": len(results),
            "passed": len(passed), "failed": len(failures),
            "process_crashes": crashes,
        },
        "regressions": regressions[:200],
        "failure_samples": [
            {"id": i, "why": w}
            for i, w in failures[:args.failure_samples]],
        "passed": passed,
    }, indent=1) + "\n", encoding="utf-8")

    for line in lines:
        print(line)
    for r in regressions[:20]:
        print(f"  regressed: {r}")
    print(f"report: {args.output}")
    return exit_code


if __name__ == "__main__":
    if "--worker" in sys.argv:
        raise SystemExit(_worker_main())
    raise SystemExit(main())
