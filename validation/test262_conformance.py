"""Run gg-js against Test262, the JavaScript conformance suite.

Test262 is to a JS engine what html5lib-tests is to a parser: the only
external scoreboard there is. gg-js has never had one, so "async works,
Proxy works" has meant a grep hit rather than a number.

The corpus is not vendored. Point ``--corpus`` at a checkout of
https://github.com/tc39/test262 (CI pins the revision) and this reads
``harness/`` and ``test/`` from it.

The gate is a baseline of exactly which tests passed, so a corpus that
grows does not silently move the score:

* a test in the baseline that now fails is a **regression** and fails
  the run, always;
* a test *not* in the baseline that fails is a **new failure**. Those
  are allowed up to a recorded budget and reported either way — a
  no-regression gate alone lets the pass rate drift downwards for
  free as the corpus grows;
* the corpus is fingerprinted by content, not by a revision string
  someone remembered to bump, so a checkout that does not match the
  baseline is an error rather than a surprise;
* ``--file``/``--limit`` run a subset, which cannot be compared against
  a whole-corpus baseline. They turn the comparison off and say so.
"""

from __future__ import annotations

import argparse
import concurrent.futures as futures
from dataclasses import dataclass
import datetime as _datetime
import fnmatch
import hashlib
import json
import os
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

DEFAULT_BASELINE = Path(__file__).with_name("baselines") / "test262.json"
DEFAULT_OUTPUT = Path(__file__).with_name("out") / "test262.json"

FRONTMATTER = re.compile(r"/\*---(.*?)---\*/", re.S)
UNCAUGHT = re.compile(r"uncaught ([A-Za-z]*Error)")
ASYNC_DONE = "Test262:AsyncTestComplete"


@dataclass(frozen=True)
class Case:
    """One test file, before the strict/sloppy split."""
    path: str
    source: str
    flags: frozenset
    includes: tuple
    negative_phase: str | None
    negative_type: str | None


def parse_frontmatter(text: str) -> dict:
    """The YAML subset Test262 actually uses in its ``/*--- ---*/`` block.

    Only ``flags``, ``includes`` and ``negative`` change how a test runs,
    and each appears either inline (``flags: [onlyStrict]``) or as an
    indented block. Pulling those three out by hand keeps the runner free
    of a YAML dependency it would otherwise need for nothing else.
    """
    m = FRONTMATTER.search(text)
    if not m:
        return {}
    body = m.group(1)
    out: dict = {}
    lines = body.split("\n")
    i = 0
    while i < len(lines):
        line = lines[i]
        stripped = line.strip()
        i += 1
        if not stripped or stripped.startswith("#"):
            continue
        if ":" not in line or line[:1].isspace():
            continue
        key, _, rest = line.partition(":")
        key = key.strip()
        rest = rest.strip()
        if key not in ("flags", "includes", "negative"):
            continue
        if key == "negative":
            neg: dict = {}
            while i < len(lines) and (
                    not lines[i].strip() or lines[i][:1].isspace()):
                sub = lines[i].strip()
                i += 1
                if ":" in sub:
                    k, _, v = sub.partition(":")
                    neg[k.strip()] = v.strip()
            out["negative"] = neg
            continue
        if rest.startswith("["):
            items = rest.strip("[]").split(",")
            out[key] = [x.strip() for x in items if x.strip()]
            continue
        items = []
        while i < len(lines) and lines[i].strip().startswith("-"):
            items.append(lines[i].strip()[1:].strip())
            i += 1
        out[key] = items
    return out


def load_case(path: Path, root: Path) -> Case:
    text = path.read_text(encoding="utf-8", errors="replace")
    meta = parse_frontmatter(text)
    neg = meta.get("negative") or {}
    return Case(
        path=path.relative_to(root).as_posix(),
        source=text,
        flags=frozenset(meta.get("flags") or ()),
        includes=tuple(meta.get("includes") or ()),
        negative_phase=neg.get("phase"),
        negative_type=neg.get("type"),
    )


def collect(corpus: Path, patterns: list[str] | None) -> list[Path]:
    test_root = corpus / "test"
    if not test_root.is_dir():
        raise ValueError(f"no test/ directory under {corpus}")
    paths = []
    for path in sorted(test_root.rglob("*.js")):
        name = path.as_posix()
        # _FIXTURE files are imported by module tests, never run alone
        if path.name.endswith("_FIXTURE.js"):
            continue
        if patterns and not any(
                fnmatch.fnmatchcase(name, f"*{p}*") for p in patterns):
            continue
        paths.append(path)
    return paths


_HARNESS: dict = {}


def harness(corpus: Path, name: str) -> str:
    if name not in _HARNESS:
        _HARNESS[name] = (corpus / "harness" / name).read_text(
            encoding="utf-8", errors="replace")
    return _HARNESS[name]


def build_source(corpus: Path, case: Case, strict: bool) -> str:
    if "raw" in case.flags:
        return case.source
    parts = [harness(corpus, "assert.js"), harness(corpus, "sta.js")]
    if "async" in case.flags:
        parts.append(harness(corpus, "doneprintHandle.js"))
    for inc in case.includes:
        parts.append(harness(corpus, inc))
    parts.append(case.source)
    body = "\n".join(parts)
    return '"use strict";\n' + body if strict else body


def error_type(kind: str, message: str) -> str:
    """The thrown error's constructor name.

    A native throw carries its kind directly; a user ``throw new
    RangeError(...)`` arrives as the generic "Error" with the real name
    in the message.
    """
    if kind and kind != "Error":
        return kind
    m = UNCAUGHT.search(message)
    return m.group(1) if m else kind


def judge(case: Case, ok: bool, kind: str, message: str,
          logs: list[str]) -> tuple[bool, str]:
    """(passed, why-not)."""
    if case.negative_phase:
        if ok:
            return False, "expected a throw, ran clean"
        want = case.negative_type
        got = error_type(kind, message)
        if case.negative_phase in ("parse", "resolve"):
            if kind != "SyntaxError":
                return False, f"expected a parse error, got {got}"
            return True, ""
        if want and got != want:
            return False, f"expected {want}, got {got or 'a throw'}"
        return True, ""
    if not ok:
        return False, f"{kind}: {message.splitlines()[0][:160]}"
    if "async" in case.flags and not any(
            ASYNC_DONE in line for line in logs):
        return False, "async test never signalled completion"
    return True, ""


def run_one(args) -> list[tuple[str, bool, str]]:
    """(id, passed, why-not) for each mode of one file."""
    corpus_str, path_str = args
    import ggcore

    corpus = Path(corpus_str)
    case = load_case(Path(path_str), corpus)
    if "module" in case.flags:
        return [(case.path + "#module", False, "modules not implemented")]

    modes = []
    if "raw" in case.flags:
        modes.append(("", False))
    else:
        if "onlyStrict" not in case.flags:
            modes.append(("", False))
        if "noStrict" not in case.flags:
            modes.append(("#strict", True))

    out = []
    for suffix, strict in modes:
        source = build_source(corpus, case, strict)
        try:
            ok, kind, message, logs = ggcore.jsvm_probe(source)
        except BaseException as exc:   # a panic must not stop the run
            ok, kind, message, logs = (
                False, type(exc).__name__, str(exc)[:200], [])
        passed, why = judge(case, ok, kind, message, logs)
        out.append((case.path + suffix, passed, why))
    return out


def _run_chunk(chunk):
    out = []
    for item in chunk:
        out.extend(run_one(item))
    return out


def _worker_main() -> int:
    """Run one chunk handed over on stdin; answer on stdout.

    Chunks run in their own process rather than in a pool worker
    because the engine can still abort outright — a bad register count
    asks for a 2^56-byte allocation, and Rust cannot recover a failed
    allocation. A pool that loses a worker refuses further work; a
    subprocess that dies is just a non-zero exit code, and the parent
    can bisect the chunk to name the offender.
    """
    payload = json.loads(sys.stdin.read())
    print(json.dumps(_run_chunk([tuple(x) for x in payload])))
    return 0


def load_partial(path: Path):
    """Results already recorded by an earlier, interrupted run."""
    done: dict = {}
    if not path or not path.is_file():
        return done
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue          # a run killed mid-write leaves one short line
        done[rec["file"]] = [tuple(r) for r in rec["results"]]
    return done


def run_all(work, jobs, chunk_size=64, timeout=600, partial=None):
    import subprocess
    import concurrent.futures as cf

    def run_chunk(chunk):
        proc = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), "--worker"],
            input=json.dumps(chunk), capture_output=True, text=True,
            timeout=timeout,
        )
        if proc.returncode != 0 or not proc.stdout.strip():
            raise RuntimeError(proc.stderr.strip()[-200:] or "no output")
        return json.loads(proc.stdout)

    # A run over fifty thousand files outlives this machine's patience,
    # so every chunk's results are appended as they land and a rerun
    # picks up where the last one stopped.
    done = load_partial(partial) if partial else {}
    results = [r for rs in done.values() for r in rs]
    work = [w for w in work
            if Path(w[1]).relative_to(Path(w[0])).as_posix() not in done]
    pending = [work[i:i + chunk_size]
               for i in range(0, len(work), chunk_size)]
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
                        path = Path(chunk[0][1]).relative_to(
                            Path(chunk[0][0])).as_posix()
                        crashed = [(path, False,
                                    "the engine aborted the process")]
                        results.extend(crashed)
                        if sink is not None:
                            record(sink, chunk, crashed)
                        crashes += 1
                    else:
                        half = len(chunk) // 2
                        nxt.append(chunk[:half])
                        nxt.append(chunk[half:])
        pending = nxt
    if sink is not None:
        sink.close()
    return results, crashes


def record(sink, chunk, got):
    """Append one chunk's results, keyed by the files it covered."""
    covered = [Path(c[1]).relative_to(Path(c[0])).as_posix() for c in chunk]
    by_file: dict = {name: [] for name in covered}
    for ident, ok, why in got:
        name = ident.split("#")[0]
        by_file.setdefault(name, []).append((ident, ok, why))
    for name, rows in by_file.items():
        sink.write(json.dumps({"file": name, "results": rows}) + "\n")
    sink.flush()


def fingerprint(paths: list[Path], corpus: Path) -> str:
    """Content hash of the corpus actually run.

    A revision string only records what someone typed; this records what
    was read.
    """
    h = hashlib.sha256()
    for path in paths:
        h.update(path.relative_to(corpus).as_posix().encode())
        h.update(b"\0")
        h.update(hashlib.sha256(path.read_bytes()).digest())
    return h.hexdigest()


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--corpus", type=Path,
                   default=os.environ.get("TEST262_DIR"),
                   help="a tc39/test262 checkout")
    p.add_argument("--file", action="append", dest="files",
                   help="only paths containing this (repeatable)")
    p.add_argument("--limit", type=int, help="stop after N files")
    p.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 2) - 1))
    p.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    p.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    p.add_argument("--update-baseline", action="store_true")
    p.add_argument("--new-failure-budget", type=int, default=None,
                   help="new failures tolerated; stored in the baseline")
    p.add_argument("--failure-samples", type=int, default=40)
    p.add_argument("--partial", type=Path,
                   help="append results here and resume from it")
    p.add_argument("--worker", action="store_true",
                   help=argparse.SUPPRESS)
    return p


def main(argv: list[str] | None = None) -> int:
    if argv is None and "--worker" in sys.argv[1:]:
        return _worker_main()
    args = build_parser().parse_args(argv)
    if args.corpus is None:
        print("--corpus or TEST262_DIR is required", file=sys.stderr)
        return 2
    corpus = args.corpus.resolve()
    subset = bool(args.files or args.limit)

    try:
        paths = collect(corpus, args.files)
    except (OSError, ValueError) as exc:
        print(f"configuration error: {exc}", file=sys.stderr)
        return 2
    if args.limit is not None:
        paths = paths[:max(args.limit, 0)]
    if not paths:
        print("no tests selected", file=sys.stderr)
        return 2

    work = [(str(corpus), str(p)) for p in paths]
    results, crashes = run_all(work, args.jobs, partial=args.partial)

    passed = sorted(i for i, ok, _ in results if ok)
    failures = [(i, why) for i, ok, why in results if not ok]
    current = set(passed)

    prints = [
        f"test262: {len(passed)}/{len(results)} pass "
        f"({len(passed) / len(results):.2%}) over {len(paths)} files"
    ]
    if crashes:
        prints.append(
            f"{crashes} case(s) took the whole process down with them")

    regressions: list[str] = []
    new_failures: list[str] = []
    budget = args.new_failure_budget
    exit_code = 0

    if subset:
        prints.append(
            "subset run: baseline comparison off "
            "(a partial run cannot be judged against a whole-corpus baseline)"
        )
    else:
        digest = fingerprint(paths, corpus)
        if args.update_baseline:
            # Recording and then comparing against what was just written
            # is a comparison that cannot fail. Say so instead of
            # printing a green line that means nothing.
            args.baseline.parent.mkdir(parents=True, exist_ok=True)
            args.baseline.write_text(json.dumps({
                "schema_version": 1,
                "corpus_sha256": digest,
                "new_failure_budget":
                    budget if budget is not None else 0,
                "passed": passed,
            }, indent=1) + "\n", encoding="utf-8")
            prints.append(
                f"baseline recorded at {args.baseline} "
                "(comparison skipped for this run)")
        elif not args.baseline.is_file():
            prints.append(f"no baseline at {args.baseline}; "
                          "run with --update-baseline to record one")
            exit_code = 2
        else:
            base = json.loads(args.baseline.read_text(encoding="utf-8"))
            if base.get("corpus_sha256") != digest:
                prints.append(
                    "corpus does not match the baseline (content hash "
                    f"{digest[:12]} vs {str(base.get('corpus_sha256'))[:12]})"
                )
                exit_code = 2
            else:
                known = set(base.get("passed", []))
                if budget is None:
                    budget = int(base.get("new_failure_budget", 0))
                regressions = sorted(known - current)
                seen = known | current
                new_failures = sorted(
                    i for i, ok, _ in results if not ok and i not in seen)
                prints.append(
                    f"baseline: regressions={len(regressions)}, "
                    f"new failures={len(new_failures)} (budget {budget}), "
                    f"improvements={len(current - known)}")
                if regressions:
                    exit_code = 1
                if len(new_failures) > budget:
                    exit_code = 1

    report = {
        "schema_version": 1,
        "generated_at": _datetime.datetime.now(
            _datetime.timezone.utc).isoformat(timespec="seconds"),
        "engine": "gg-js",
        "corpus": str(corpus),
        "subset": subset,
        "counts": {
            "files": len(paths),
            "cases": len(results),
            "passed": len(passed),
            "failed": len(failures),
            "process_crashes": crashes,
        },
        "regressions": regressions,
        "new_failures": new_failures,
        "failure_samples": [
            {"id": i, "why": w} for i, w in failures[:args.failure_samples]
        ],
        "passed": passed,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(report, indent=1) + "\n", encoding="utf-8")

    for line in prints:
        print(line)
    print(f"report: {args.output}")
    for test_id in regressions[:20]:
        print(f"  regressed: {test_id}", file=sys.stderr)
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
