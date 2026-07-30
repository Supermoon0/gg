"""Run the engine against web-platform-tests' CSS suite.

Test262 is the scoreboard for the JS engine; this is the one for CSS.
Until now "CSS works" has meant 23 hand-written gauntlet cases, which
say the things we thought to check and nothing about the rest. Three
times in one afternoon a quick coverage estimate came back 100%, then
44%, then something else again, because a home-made probe measures
whatever it happens to measure.

Only the testharness.js half of the suite is runnable here: those tests
score themselves in script, so the engine can report them. The other
16368 are reftests that compare rendering against a reference page and
need pixel comparison, which is a separate machine.

The corpus is not vendored. Point ``--corpus`` at a checkout of
https://github.com/web-platform-tests/wpt and this reads ``css/`` and
``resources/`` from it.

Scoring is per **subtest**, not per file: one parsing test carries
dozens of independent assertions, and a file-level pass/fail throws
away almost all of the signal.

The gate follows test262_conformance.py, for the same reasons:

* a subtest in the baseline that now fails is a **regression** and
  fails the run, always;
* the corpus is fingerprinted by content, so a checkout that does not
  match the baseline is an error rather than a surprise;
* ``--file``/``--limit`` run a subset, which cannot be judged against a
  whole-corpus baseline. They turn the comparison off and say so;
* every chunk's results are appended as they land (``--partial``), so a
  run that dies resumes instead of starting over.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

DEFAULT_BASELINE = (
    Path(__file__).with_name("baselines") / "wpt_css.json")
DEFAULT_OUTPUT = Path(__file__).with_name("out") / "wpt_css.json"


def discover(corpus: Path, filters, limit):
    """Every testharness.js-based test under css/, in a stable order."""
    css = corpus / "css"
    if not css.is_dir():
        raise SystemExit(f"no css/ in {corpus}")
    out = []
    for path in sorted(css.rglob("*.html")):
        rel = path.relative_to(corpus).as_posix()
        if filters and not any(f in rel for f in filters):
            continue
        try:
            head = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if "testharness.js" not in head:
            continue
        # a reftest scores by comparing pixels, which this cannot do
        if 'rel="match"' in head or 'rel="mismatch"' in head:
            continue
        out.append(rel)
        if limit and len(out) >= limit:
            break
    return out


def run_one(corpus: Path, rel: str, timeout: float):
    """(subtest id, passed, why) for one file."""
    import conformance as C

    path = corpus / rel
    try:
        html = C.inline_wpt_test(corpus, path)
    except Exception as exc:                       # unsupported shape
        return [(f"{rel}", False, f"skip: {exc}")]
    res = C._worker_call("wpt", {"html": html, "timeout": timeout},
                         timeout + 5)
    if not res.get("ok"):
        why = res.get("error", "worker failure")
        return [(f"{rel}", False, f"harness: {why}")]
    rows = []
    seen = set()
    for line in res.get("logs", []):
        if not line.startswith(C._WPT_RESULT):
            continue
        try:
            d = json.loads(line[len(C._WPT_RESULT):])
        except json.JSONDecodeError:
            continue
        name = d.get("name", "")
        # subtest names repeat across a file often enough to matter
        ident = f"{rel}#{name}"
        n = 2
        while ident in seen:
            ident = f"{rel}#{name}~{n}"
            n += 1
        seen.add(ident)
        rows.append((ident, d.get("status") == 0,
                     (d.get("message") or "")[:200]))
    if not rows:
        # the page never reported: an engine error, or it never loaded
        return [(f"{rel}", False, "no subtest results")]
    return rows


def _run_chunk(corpus: str, rels):
    out = []
    for rel in rels:
        out.extend(run_one(Path(corpus), rel, 20.0))
    return out


def _worker_main():
    """One chunk on stdin, its results on stdout.

    A chunk runs in its own process because a page can still take the
    interpreter down with it; the parent then bisects to name it.
    """
    payload = json.loads(sys.stdin.read())
    print(json.dumps(_run_chunk(payload["corpus"], payload["rels"])))
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
            continue                    # a run killed mid-write
        done[rec["file"]] = [tuple(r) for r in rec["results"]]
    return done


def record(sink, rels, rows):
    by_file = {r: [] for r in rels}
    for ident, ok, why in rows:
        by_file.setdefault(ident.split("#")[0], []).append((ident, ok, why))
    for name, got in by_file.items():
        sink.write(json.dumps({"file": name, "results": got}) + "\n")
    sink.flush()


def run_all(corpus: Path, rels, jobs, chunk_size=24, partial=None):
    import concurrent.futures as cf

    def run_chunk(chunk):
        proc = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), "--worker"],
            input=json.dumps({"corpus": str(corpus), "rels": chunk}),
            capture_output=True, text=True, timeout=len(chunk) * 40 + 60)
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
            h.update(hashlib.sha256(
                (corpus / rel).read_bytes()).digest())
        except OSError:
            h.update(b"missing")
    return h.hexdigest()


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--corpus", type=Path, required=True,
                   help="a web-platform-tests checkout")
    p.add_argument("--file", dest="filters", action="append", default=[],
                   help="only paths containing this (repeatable)")
    p.add_argument("--limit", type=int)
    p.add_argument("--jobs", type=int, default=4)
    p.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    p.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    p.add_argument("--update-baseline", action="store_true")
    p.add_argument("--partial", type=Path,
                   help="append results here and resume from it")
    p.add_argument("--failure-samples", type=int, default=40)
    args = p.parse_args(argv)

    corpus = args.corpus.resolve()
    rels = discover(corpus, args.filters, args.limit)
    if not rels:
        print("no tests selected", file=sys.stderr)
        return 2
    subset = bool(args.filters or args.limit)

    results, crashes = run_all(corpus, rels, args.jobs, partial=args.partial)
    passed = sorted(i for i, ok, _ in results if ok)
    failures = [(i, why) for i, ok, why in results if not ok]
    current = set(passed)

    lines = [f"wpt-css: {len(passed)}/{len(results)} subtests pass "
             f"({len(passed) / len(results):.2%}) over {len(rels)} files"]
    if crashes:
        lines.append(f"{crashes} file(s) took the process down with them")

    exit_code = 0
    regressions = []
    if subset:
        lines.append("subset run: baseline comparison off "
                     "(a partial run cannot be judged against a "
                     "whole-corpus baseline)")
    else:
        digest = fingerprint(corpus, rels)
        if args.update_baseline:
            args.baseline.parent.mkdir(parents=True, exist_ok=True)
            args.baseline.write_text(json.dumps({
                "schema_version": 1,
                "corpus_sha256": digest,
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
        "counts": {
            "files": len(rels), "subtests": len(results),
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
