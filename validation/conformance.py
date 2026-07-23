"""GG browser conformance scorecard.

The always-on ``builtin`` suites are small contract probes for gg-js and the
browser DOM integration.  Optional adapters run a pinned selection from local
Test262 and WPT checkouts without vendoring either upstream repository.

This is intentionally a scorecard, not a claim that the built-in probes are
WPT or Test262.  Official results are labelled separately in the JSON report.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as _datetime
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import urllib.parse


ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
DEFAULT_BASELINE = Path(__file__).with_name("baselines") / "conformance.json"
DEFAULT_OUTPUT = Path(__file__).with_name("out") / "conformance.json"
DEFAULT_TEST262_LIST = Path(__file__).with_name("conformance_subsets") / "test262.txt"
DEFAULT_WPT_LIST = Path(__file__).with_name("conformance_subsets") / "wpt.txt"

_ERROR_PREFIX = "[gg-js error] "
_WPT_RESULT = "__GG_WPT_RESULT__"
_WPT_COMPLETE = "__GG_WPT_COMPLETE__"


@dataclasses.dataclass
class Result:
    id: str
    suite: str
    status: str
    duration_ms: int = 0
    message: str = ""
    details: dict = dataclasses.field(default_factory=dict)

    def as_dict(self):
        value = dataclasses.asdict(self)
        if not self.message:
            value.pop("message")
        if not self.details:
            value.pop("details")
        return value


def _now_iso():
    return _datetime.datetime.now(_datetime.timezone.utc).isoformat(timespec="seconds")


def _engine_version():
    try:
        return importlib.metadata.version("ggcore")
    except importlib.metadata.PackageNotFoundError:
        return "unknown"


def _timed(call):
    import time

    started = time.perf_counter()
    try:
        return call(), int((time.perf_counter() - started) * 1000), None
    except Exception as exc:  # one probe must never hide the remaining score
        return None, int((time.perf_counter() - started) * 1000), exc


def _run_js(source):
    import ggcore

    return list(ggcore.jsvm_run(source))


def _data_url(html):
    return "data:text/html;charset=utf-8," + urllib.parse.quote(html, safe="")


def _run_page(html, check, click=None):
    from browser.driver import Page

    page = Page(timeout=5).goto(_data_url(html))
    if click:
        page.click(click)
    actual = check(page)
    return actual, page.console()


def run_builtin_js():
    cases = [
        (
            "closures/block-loop-binding",
            "const a=[]; for(let i=0;i<3;i++) a.push(()=>i); "
            "console.log(a.map(f=>f()).join(','));",
            ["0,1,2"],
        ),
        (
            "objects/proxy-reflect",
            "const t={x:2}; const p=new Proxy(t,{get(o,k,r){"
            "return Reflect.get(o,k,r)*3;}}); console.log(p.x);",
            ["6"],
        ),
        (
            "collections/map-set",
            "const m=new Map([['a',1],['b',2]]); "
            "const s=new Set([1,1,2]); "
            "console.log(m.size+':'+s.size+':'+m.get('b'));",
            ["2:2:2"],
        ),
        (
            "errors/type-hierarchy",
            "let x=''; try{null.x}catch(e){x=e.name+':'"
            "+(e instanceof TypeError)} console.log(x);",
            ["TypeError:true"],
        ),
        (
            "builtins/json-regexp",
            "const x=JSON.parse('{\"n\":3}'); "
            "console.log(x.n+':'+/a(b+)/.exec('abbb')[1]);",
            ["3:bbb"],
        ),
    ]
    results = []
    for name, source, expected in cases:
        actual, elapsed, error = _timed(lambda source=source: _run_js(source))
        if error is not None:
            results.append(Result(
                f"builtin-js/{name}", "builtin-js", "fail", elapsed,
                f"{type(error).__name__}: {error}", {"expected": expected}))
        elif actual != expected:
            results.append(Result(
                f"builtin-js/{name}", "builtin-js", "fail", elapsed,
                "console output differed",
                {"expected": expected, "actual": actual}))
        else:
            results.append(Result(
                f"builtin-js/{name}", "builtin-js", "pass", elapsed))
    return results


def run_builtin_web():
    cases = [
        (
            "dom/create-append-query",
            """<!doctype html><div id="root"><span class="item">A</span></div>
<script>const n=document.createElement("span"); n.className="item";
n.textContent="B"; document.querySelector("#root").appendChild(n);
document.querySelectorAll(".item")[0].setAttribute("data-ok","yes");</script>""",
            lambda page: [page.text("#root"), page.attr(".item", "data-ok")],
            ["AB", "yes"],
            None,
        ),
        (
            "events/click-listener",
            """<!doctype html><button id="b">0</button><script>
document.querySelector("#b").addEventListener("click",function(e){
this.textContent=String(Number(this.textContent)+1);
console.log("clicked:"+e.type);});</script>""",
            lambda page: [page.text("#b"), "clicked:click" in page.console()],
            ["1", True],
            "#b",
        ),
        (
            "event-loop/promise-timer",
            """<!doctype html><p id="v">start</p><script>
Promise.resolve(3).then(function(v){
document.querySelector("#v").textContent="p"+v;});
setTimeout(function(){document.querySelector("#v")
.setAttribute("data-timer","yes");},10);</script>""",
            lambda page: [page.text("#v"), page.attr("#v", "data-timer")],
            ["p3", "yes"],
            None,
        ),
        (
            "modules/inline-module",
            """<!doctype html><p id="v">start</p>
<script type="module">const x=4;
document.querySelector("#v").textContent="m"+x;</script>""",
            lambda page: page.text("#v"),
            "m4",
            None,
        ),
    ]
    results = []
    for name, html, check, expected, click in cases:
        value, elapsed, error = _timed(
            lambda html=html, check=check, click=click:
            _run_page(html, check, click))
        if error is not None:
            results.append(Result(
                f"builtin-web/{name}", "builtin-web", "fail", elapsed,
                f"{type(error).__name__}: {error}", {"expected": expected}))
            continue
        actual, console = value
        if actual != expected:
            results.append(Result(
                f"builtin-web/{name}", "builtin-web", "fail", elapsed,
                "page result differed", {
                    "expected": expected, "actual": actual,
                    "console": console[-20:],
                }))
        else:
            results.append(Result(
                f"builtin-web/{name}", "builtin-web", "pass", elapsed))
    return results


def parse_test262_metadata(source):
    """Parse the small metadata subset needed by the adapter.

    Test262 frontmatter is YAML.  Depending on PyYAML would make the release
    gate less portable, so this parser deliberately handles only the scalar,
    list, and ``negative`` forms consumed below.  Unknown keys remain harmless.
    """
    match = re.search(r"/\*---(.*?)---\*/", source, re.DOTALL)
    if not match:
        return {"flags": [], "includes": [], "features": [], "negative": None}
    block = match.group(1)

    def list_value(key):
        inline = re.search(
            rf"(?m)^\s*{re.escape(key)}\s*:\s*\[(.*?)\]\s*$", block)
        if inline:
            return [part.strip().strip("'\"")
                    for part in inline.group(1).split(",") if part.strip()]
        lines = block.splitlines()
        out = []
        active_indent = None
        for line in lines:
            head = re.match(rf"^(\s*){re.escape(key)}\s*:\s*$", line)
            if head:
                active_indent = len(head.group(1))
                continue
            if active_indent is None:
                continue
            item = re.match(r"^(\s*)-\s*(.*?)\s*$", line)
            if item and len(item.group(1)) > active_indent:
                out.append(item.group(2).strip("'\""))
                continue
            if line.strip() and len(line) - len(line.lstrip()) <= active_indent:
                break
        return out

    negative = None
    negative_match = re.search(
        r"(?ms)^\s*negative\s*:\s*\n"
        r"(?P<body>(?:\s+[^\n]+\n?)*)", block)
    if negative_match:
        body = negative_match.group("body")
        phase = re.search(r"(?m)^\s*phase\s*:\s*([^\s#]+)", body)
        kind = re.search(r"(?m)^\s*type\s*:\s*([^\s#]+)", body)
        negative = {
            "phase": phase.group(1).strip("'\"") if phase else "runtime",
            "type": kind.group(1).strip("'\"") if kind else "Error",
        }
    return {
        "flags": list_value("flags"),
        "includes": list_value("includes"),
        "features": list_value("features"),
        "negative": negative,
    }


def _read_selection(root, selection, max_tests=None):
    root = root.resolve()
    groups = []
    for raw in selection.read_text(encoding="utf-8").splitlines():
        entry = raw.strip()
        if not entry or entry.startswith("#"):
            continue
        matches = sorted(root.glob(entry)) if any(c in entry for c in "*?[") \
            else [root / entry]
        if not matches:
            raise ValueError(f"selection did not match: {entry}")
        group = []
        for path in matches:
            resolved = path.resolve()
            try:
                resolved.relative_to(root)
            except ValueError as exc:
                raise ValueError(f"selection escapes suite root: {entry}") from exc
            if not resolved.is_file():
                raise ValueError(f"selected test is not a file: {entry}")
            if resolved not in group:
                group.append(resolved)
        groups.append(group)
    paths = []
    if max_tests is None:
        candidates = (path for group in groups for path in group)
    else:
        # Round-robin keeps a global cap from silently consuming only the
        # first (often largest) feature directory in the selection file.
        candidates = (
            group[index]
            for index in range(max((len(group) for group in groups), default=0))
            for group in groups
            if index < len(group)
        )
    for path in candidates:
        if path not in paths:
            paths.append(path)
            if max_tests is not None and len(paths) >= max_tests:
                break
    if not paths:
        raise ValueError(f"selection is empty: {selection}")
    return paths


def _worker_call(mode, payload, timeout):
    command = [sys.executable, str(Path(__file__).resolve()), "--worker", mode]
    try:
        proc = subprocess.run(
            command, input=json.dumps(payload), capture_output=True, text=True,
            encoding="utf-8", timeout=timeout, check=False)
    except subprocess.TimeoutExpired:
        return {"ok": False, "timeout": True, "error": f"timeout after {timeout}s"}
    if proc.returncode != 0:
        return {
            "ok": False,
            "error": f"worker exited {proc.returncode}",
            "stderr": proc.stderr[-2000:],
        }
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError:
        return {
            "ok": False, "error": "worker returned invalid JSON",
            "stdout": proc.stdout[-2000:], "stderr": proc.stderr[-2000:],
        }


def _engine_error(logs):
    for line in reversed(logs):
        if line.startswith(_ERROR_PREFIX):
            return line[len(_ERROR_PREFIX):]
    return None


def _error_phase(message):
    if message is None:
        return None
    if message.startswith("line ") or message.startswith("compile error:"):
        return "parse"
    return "runtime"


def _error_type(message, phase):
    if phase == "parse":
        return "SyntaxError"
    typed = re.search(r"(?:uncaught\s+)?([A-Za-z]+Error)\b", message or "")
    if typed:
        return typed.group(1)
    if message and " is not defined" in message:
        return "ReferenceError"
    return "Error"


def _test262_variants(flags):
    if "raw" in flags:
        return [("raw", False)]
    if "onlyStrict" in flags:
        return [("strict", True)]
    if "noStrict" in flags:
        return [("sloppy", False)]
    return [("sloppy", False), ("strict", True)]


def _run_test262_variant(root, path, source, metadata, variant, strict, timeout):
    flags = metadata["flags"]
    if "module" in flags:
        return {"variant": variant, "status": "skip",
                "reason": "module resolution is not supported by this adapter"}
    blocking = sorted(set(flags) & {"CanBlockIsFalse", "CanBlockIsTrue"})
    if blocking:
        return {"variant": variant, "status": "skip",
                "reason": "agent blocking mode is not supported"}
    advanced_host = sorted(set(re.findall(r"\$262\.([A-Za-z_$][\w$]*)", source))
                           - {"global"})
    if advanced_host:
        return {"variant": variant, "status": "skip",
                "reason": "unsupported $262 host API: " + ", ".join(advanced_host)}

    scripts = []
    if "raw" not in flags:
        host = (
            "var print=function(value){console.log(String(value));};\n"
            "var $262={global:globalThis};\n"
            "function $DONOTEVALUATE(){throw new Error('$DONOTEVALUATE');}\n"
        )
        scripts.append(host)
        names = ["assert.js", "sta.js"]
        if "async" in flags:
            names.append("doneprintHandle.js")
        names.extend(metadata["includes"])
        for name in names:
            harness = (root / "harness" / name).resolve()
            try:
                harness.relative_to((root / "harness").resolve())
            except ValueError:
                return {"variant": variant, "status": "skip",
                        "reason": f"invalid harness include: {name}"}
            if not harness.is_file():
                return {"variant": variant, "status": "skip",
                        "reason": f"missing harness include: {name}"}
            scripts.append(harness.read_text(encoding="utf-8"))
    test_source = ('"use strict";\n' if strict else "") + source
    scripts.append(test_source)
    worker = _worker_call(
        "test262", {"scripts": scripts, "async": "async" in flags}, timeout)
    if worker.get("timeout"):
        return {"variant": variant, "status": "fail", "reason": worker["error"]}
    if not worker.get("ok"):
        return {"variant": variant, "status": "fail",
                "reason": worker.get("error", "worker failure")}
    logs = worker.get("logs", [])
    script_errors = worker.get("script_errors", [])
    harness_errors = [item for item in script_errors
                      if item["script_index"] < len(scripts) - 1]
    if harness_errors:
        return {
            "variant": variant, "status": "fail",
            "reason": "harness failed: " + harness_errors[0]["message"],
        }
    test_errors = [item for item in script_errors
                   if item["script_index"] == len(scripts) - 1]
    error = test_errors[-1]["message"] if test_errors else _engine_error(logs)
    phase = _error_phase(error)
    kind = _error_type(error, phase)
    negative = metadata["negative"]

    if negative:
        expected_phase = negative["phase"]
        expected_type = negative["type"]
        passed = phase == expected_phase and kind == expected_type
        return {
            "variant": variant, "status": "pass" if passed else "fail",
            "expected_error": negative,
            "actual_error": ({"phase": phase, "type": kind, "message": error}
                             if error else None),
        }
    if error:
        return {"variant": variant, "status": "fail", "reason": error}
    if "async" in flags:
        failures = [line for line in logs
                    if line.startswith("Test262:AsyncTestFailure:")]
        if failures:
            return {"variant": variant, "status": "fail", "reason": failures[-1]}
        if "Test262:AsyncTestComplete" not in logs:
            return {"variant": variant, "status": "fail",
                    "reason": "async completion marker was not printed"}
    return {"variant": variant, "status": "pass"}


def run_test262(root, selection, timeout=10, max_tests=None):
    root = root.resolve()
    if not (root / "harness" / "assert.js").is_file() \
            or not (root / "test").is_dir():
        raise ValueError(f"not a Test262 checkout: {root}")
    paths = _read_selection(root, selection, max_tests)
    results = []
    for path in paths:
        relative = path.relative_to(root).as_posix()
        if "_FIXTURE" in path.name:
            results.append(Result(
                f"test262/{relative}", "test262", "skip", message="fixture file"))
            continue
        source = path.read_text(encoding="utf-8")
        metadata = parse_test262_metadata(source)
        value, elapsed, error = _timed(lambda: [
            _run_test262_variant(
                root, path, source, metadata, variant, strict, timeout)
            for variant, strict in _test262_variants(metadata["flags"])
        ])
        if error is not None:
            results.append(Result(
                f"test262/{relative}", "test262", "fail", elapsed,
                f"{type(error).__name__}: {error}"))
            continue
        statuses = {item["status"] for item in value}
        status = "fail" if "fail" in statuses else (
            "pass" if "pass" in statuses else "skip")
        message = "; ".join(item.get("reason", "") for item in value
                            if item.get("reason"))
        results.append(Result(
            f"test262/{relative}", "test262", status, elapsed, message,
            {"features": metadata["features"], "flags": metadata["flags"],
             "variants": value}))
    return results


def _script_attr(attrs, name):
    match = re.search(
        rf"(?i)(?:^|\s){re.escape(name)}\s*=\s*(?:"
        r"\"([^\"]*)\"|'([^']*)'|([^\s>]+))", attrs)
    if not match:
        return None
    return next(group for group in match.groups() if group is not None)


def inline_wpt_test(root, path):
    """Inline static script dependencies and install a testharness reporter."""
    html = path.read_text(encoding="utf-8")
    if re.search(r"(?im)^\s*<meta\s+name=['\"]?variant", html):
        raise ValueError("WPT variants are not supported by the static adapter")
    saw_harness = False
    reporter = (
        "<script>\n"
        "add_result_callback(function(t){console.log('" + _WPT_RESULT + "'+"
        "JSON.stringify({name:t.name,status:t.status,message:t.message||''}));});\n"
        "add_completion_callback(function(tests,status){console.log('"
        + _WPT_COMPLETE + "'+JSON.stringify({status:status.status,"
        "message:status.message||''}));});\n"
        "</script>"
    )
    pattern = re.compile(
        r"(?is)<script(?P<attrs>[^>]*)>(?P<body>.*?)</script\s*>")

    def replace(match):
        nonlocal saw_harness
        attrs = match.group("attrs")
        src = _script_attr(attrs, "src")
        if not src:
            return match.group(0)
        parsed = urllib.parse.urlsplit(src)
        if parsed.scheme or parsed.netloc:
            raise ValueError(f"remote script is unsupported: {src}")
        resource = (root / parsed.path.lstrip("/")) if parsed.path.startswith("/") \
            else (path.parent / parsed.path)
        resource = resource.resolve()
        try:
            resource.relative_to(root.resolve())
        except ValueError as exc:
            raise ValueError(f"script escapes WPT root: {src}") from exc
        if not resource.is_file() or resource.suffix == ".py":
            raise ValueError(f"dynamic or missing WPT script: {src}")
        if resource.name == "testharnessreport.js":
            # Standalone WPT pages use this to paint human-readable results.
            # The adapter installs its own machine reporter after testharness.js.
            return ""
        code = resource.read_text(encoding="utf-8")
        clean_attrs = re.sub(
            r"(?i)\s+src\s*=\s*(?:\"[^\"]*\"|'[^']*'|[^\s>]+)",
            "", attrs)
        inlined = f"<script{clean_attrs}>\n{code}\n</script>"
        if resource.name == "testharness.js":
            saw_harness = True
            inlined += reporter
        return inlined

    html = pattern.sub(replace, html)
    if not saw_harness:
        raise ValueError("not a testharness.js WPT test")
    return html


def run_wpt(root, selection, timeout=15, max_tests=None):
    root = root.resolve()
    if not (root / "resources" / "testharness.js").is_file():
        raise ValueError(f"not a WPT checkout: {root}")
    paths = _read_selection(root, selection, max_tests)
    results = []
    for path in paths:
        relative = path.relative_to(root).as_posix()
        started_value, elapsed, inline_error = _timed(
            lambda path=path: inline_wpt_test(root, path))
        if inline_error is not None:
            results.append(Result(
                f"wpt/{relative}", "wpt", "skip", elapsed,
                str(inline_error)))
            continue
        worker, run_elapsed, run_error = _timed(lambda: _worker_call(
            "wpt", {"html": started_value, "timeout": timeout}, timeout + 2))
        elapsed += run_elapsed
        if run_error is not None or not worker.get("ok"):
            message = str(run_error) if run_error else worker.get("error", "worker failure")
            results.append(Result(
                f"wpt/{relative}", "wpt", "fail", elapsed, message))
            continue
        logs = worker.get("logs", [])
        engine_error = _engine_error(logs)
        subtests = []
        completion = None
        for line in logs:
            try:
                if line.startswith(_WPT_RESULT):
                    subtests.append(json.loads(line[len(_WPT_RESULT):]))
                elif line.startswith(_WPT_COMPLETE):
                    completion = json.loads(line[len(_WPT_COMPLETE):])
            except json.JSONDecodeError:
                pass
        passed = (engine_error is None and completion is not None
                  and completion.get("status") == 0 and subtests
                  and all(test.get("status") == 0 for test in subtests))
        message = ""
        if engine_error:
            message = engine_error
        elif completion is None:
            message = "testharness completion callback was not observed"
        elif not subtests:
            message = "testharness produced no subtests"
        results.append(Result(
            f"wpt/{relative}", "wpt", "pass" if passed else "fail",
            elapsed, message, {
                "completion": completion,
                "subtests": {
                    "total": len(subtests),
                    "pass": sum(test.get("status") == 0 for test in subtests),
                    "fail": sum(test.get("status") != 0 for test in subtests),
                },
                "failures": [test for test in subtests if test.get("status") != 0][:20],
            }))
    return results


def _counts(results):
    counts = {"total": len(results), "pass": 0, "fail": 0, "skip": 0}
    for result in results:
        counts[result.status] = counts.get(result.status, 0) + 1
    return counts


def _feature_counts(results):
    totals = {}
    for result in results:
        for feature in result.details.get("features", []):
            row = totals.setdefault(feature, {"total": 0, "pass": 0, "fail": 0,
                                               "skip": 0})
            row["total"] += 1
            row[result.status] += 1
    return dict(sorted(totals.items()))


def _checkout_revision(root):
    try:
        proc = subprocess.run(
            ["git", "-c", f"safe.directory={root.resolve()}",
             "-C", str(root), "rev-parse", "HEAD"],
            capture_output=True, text=True, encoding="utf-8", timeout=5,
            check=False)
    except (OSError, subprocess.SubprocessError):
        return None
    revision = proc.stdout.strip()
    return revision if proc.returncode == 0 and revision else None


def compare_baseline(results, baseline_path):
    baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
    expected = baseline.get("expected", {})
    actual = {result.id: result.status for result in results}
    regressions = []
    improvements = []
    for test_id, old_status in expected.items():
        new_status = actual.get(test_id, "missing")
        if old_status == "pass" and new_status != "pass":
            regressions.append({"id": test_id, "expected": old_status,
                                "actual": new_status})
        elif old_status != "pass" and new_status == "pass":
            improvements.append({"id": test_id, "previous": old_status,
                                 "actual": new_status})
    return regressions, improvements


def _selection_digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _baseline_sources(suites, args):
    sources = {}
    for suite in suites:
        if not suite["official"]:
            continue
        selection = args.test262_list if suite["name"] == "test262" else args.wpt_list
        sources[suite["name"]] = {
            "revision": suite.get("revision"),
            "selection_sha256": _selection_digest(selection),
            "max_tests": args.max_tests,
        }
    return sources


def _source_mismatches(baseline_path, actual_sources):
    baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
    expected = baseline.get("sources", {})
    mismatches = []
    for name, expected_value in expected.items():
        actual_value = actual_sources.get(name)
        if actual_value != expected_value:
            mismatches.append({
                "suite": name, "expected": expected_value, "actual": actual_value,
            })
    return mismatches


def write_baseline(results, path, sources=None):
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "schema_version": 1,
        "description": "Statuses accepted by the GG conformance regression gate.",
        "expected": {result.id: result.status for result in sorted(
            results, key=lambda item: item.id)},
    }
    if sources:
        payload["sources"] = sources
    path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n",
                    encoding="utf-8")


def _suite_payload(name, official, results):
    value = {
        "name": name,
        "official": official,
        "counts": _counts(results),
        "results": [result.as_dict() for result in results],
    }
    if name == "test262":
        value["features"] = _feature_counts(results)
    return value


def _print_summary(suites, regressions, improvements, output):
    print("GG conformance scorecard")
    for suite in suites:
        counts = suite["counts"]
        label = "official adapter" if suite["official"] else "project contract"
        print(f"  {suite['name']:<12} {counts['pass']:>4}/{counts['total']:<4} pass "
              f"({counts['fail']} fail, {counts['skip']} skip; {label})")
    if regressions:
        print(f"REGRESSION: {len(regressions)} baseline pass(es) were lost")
        for item in regressions:
            print(f"  {item['id']}: {item['expected']} -> {item['actual']}")
    elif improvements:
        print(f"No regressions; {len(improvements)} baseline improvement(s) found")
    else:
        print("No baseline regressions")
    print(f"JSON: {output}")


def _worker_main(mode):
    payload = json.loads(sys.stdin.read())
    try:
        if mode == "test262":
            import ggcore

            doc = ggcore.parse_html("<html><body></body></html>")
            logs = []
            script_errors = []
            for script_index, source in enumerate(payload["scripts"]):
                current = list(doc.run_scripts([source]))
                logs.extend(current)
                error = _engine_error(current)
                if error:
                    script_errors.append({
                        "script_index": script_index, "message": error,
                    })
            if payload.get("async"):
                for _ in range(2000):
                    more, fetches = doc.pump()
                    logs.extend(more)
                    for fetch_id, _url in fetches:
                        doc.reject_fetch(fetch_id, "network disabled in Test262")
                    if not fetches and not doc.has_pending_work():
                        break
            result = {"ok": True, "logs": logs,
                      "script_errors": script_errors}
        elif mode == "wpt":
            from browser.driver import Page

            page = Page(timeout=float(payload.get("timeout", 15)))
            page.goto(_data_url(payload["html"]))
            result = {"ok": True, "logs": page.console()}
        else:
            raise ValueError(f"unknown worker mode: {mode}")
    except Exception as exc:
        result = {"ok": False, "error": f"{type(exc).__name__}: {exc}"}
    sys.stdout.write(json.dumps(result, ensure_ascii=False))


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    parser.add_argument("--update-baseline", action="store_true")
    parser.add_argument("--strict", action="store_true",
                        help="also fail on every official-suite failure")
    parser.add_argument("--test262-root", type=Path)
    parser.add_argument("--test262-list", type=Path, default=DEFAULT_TEST262_LIST)
    parser.add_argument("--wpt-root", type=Path)
    parser.add_argument("--wpt-list", type=Path, default=DEFAULT_WPT_LIST)
    parser.add_argument("--case-timeout", type=float, default=15)
    parser.add_argument("--max-tests", type=int)
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    if args.max_tests is not None and args.max_tests < 1:
        print("--max-tests must be at least 1", file=sys.stderr)
        return 2
    all_results = []
    suites = []

    builtin_js = run_builtin_js()
    builtin_web = run_builtin_web()
    for name, official, results in (
            ("builtin-js", False, builtin_js),
            ("builtin-web", False, builtin_web)):
        suites.append(_suite_payload(name, official, results))
        all_results.extend(results)

    try:
        if args.test262_root:
            results = run_test262(
                args.test262_root, args.test262_list,
                timeout=args.case_timeout, max_tests=args.max_tests)
            suite = _suite_payload("test262", True, results)
            suite["revision"] = _checkout_revision(args.test262_root)
            suite["selection"] = str(args.test262_list)
            suites.append(suite)
            all_results.extend(results)
        if args.wpt_root:
            results = run_wpt(
                args.wpt_root, args.wpt_list,
                timeout=args.case_timeout, max_tests=args.max_tests)
            suite = _suite_payload("wpt", True, results)
            suite["revision"] = _checkout_revision(args.wpt_root)
            suite["selection"] = str(args.wpt_list)
            suites.append(suite)
            all_results.extend(results)
    except (OSError, ValueError) as exc:
        print(f"configuration error: {exc}", file=sys.stderr)
        return 2

    baseline_sources = _baseline_sources(suites, args)
    if args.update_baseline:
        write_baseline(all_results, args.baseline, baseline_sources)
    if not args.baseline.is_file():
        print(f"baseline does not exist: {args.baseline}", file=sys.stderr)
        return 2
    source_mismatches = _source_mismatches(args.baseline, baseline_sources)
    if source_mismatches:
        print("baseline source mismatch:", file=sys.stderr)
        for mismatch in source_mismatches:
            print(f"  {mismatch['suite']}: expected {mismatch['expected']}, "
                  f"actual {mismatch['actual']}", file=sys.stderr)
        return 2
    regressions, improvements = compare_baseline(all_results, args.baseline)
    total = _counts(all_results)
    report = {
        "schema_version": 1,
        "generated_at": _now_iso(),
        "engine": {"name": "gg-js", "package": "ggcore",
                   "version": _engine_version()},
        "counts": total,
        "baseline": {
            "path": os.path.relpath(args.baseline.resolve(), ROOT),
            "regressions": regressions,
            "improvements": improvements,
        },
        "suites": suites,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    _print_summary(suites, regressions, improvements, args.output)

    builtin_failure = any(
        result.status != "pass" for result in builtin_js + builtin_web)
    official_failure = args.strict and any(
        result.status == "fail" for result in all_results
        if result.suite in {"test262", "wpt"})
    return 1 if regressions or builtin_failure or official_failure else 0


if __name__ == "__main__":
    if len(sys.argv) >= 3 and sys.argv[1] == "--worker":
        _worker_main(sys.argv[2])
    else:
        raise SystemExit(main())
