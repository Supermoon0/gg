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

_REF_RE = re.compile(
    r"""<link\s[^>]*rel\s*=\s*["']?(match|mismatch)["']?[^>]*>""",
    re.I | re.S)
_HREF_RE = re.compile(r"""href\s*=\s*["']([^"']+)["']""", re.I)


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
        if not target or target.startswith(("http:", "https:", "data:")):
            continue
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


def render(page: Path, corpus: Path):
    """Raw RGB bytes for one page, or None if it will not render."""
    from browser import native, textengine
    from browser.layout import DocumentLayout, paint_tree

    html = page.read_text(encoding="utf-8", errors="replace")

    def fetch(_hrefs):
        return {}

    nodes, doc, css_sources, _logs = native.load_document(
        html, fetch, None)
    document = DocumentLayout(nodes)
    document.layout(WIDTH, HEIGHT)
    cmds = [c.native(0) for c in paint_tree(document, [])]
    return bytes(textengine.engine().render_raw(
        WIDTH, HEIGHT, (255, 255, 255), cmds))


def differing_pixels(a: bytes, b: bytes) -> int:
    if len(a) != len(b):
        return max(len(a), len(b))
    n = 0
    for i in range(0, len(a), 3):
        if a[i] != b[i] or a[i + 1] != b[i + 1] or a[i + 2] != b[i + 2]:
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
        ref_path = (path.parent / target).resolve()
        try:
            ref_path.relative_to(corpus.resolve())
        except ValueError:
            rows.append((ident, False, "reference escapes the corpus"))
            continue
        if not ref_path.is_file():
            rows.append((ident, False, "reference is missing"))
            continue
        try:
            want = render(ref_path, corpus)
        except Exception as exc:
            rows.append((ident, False,
                         f"reference render failed: {type(exc).__name__}"))
            continue
        diff = differing_pixels(got, want)
        if kind == "match":
            ok = diff <= tolerance
            why = "" if ok else f"{diff} pixels differ"
        else:
            ok = diff > tolerance
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
