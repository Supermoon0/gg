"""JS 엔진 벤치마크 하니스: 우리 엔진(ggcore) vs Edge(V8 + Blink).

사용법:
    python bench/run_bench.py            # 둘 다 실행, 비교표 출력
    python bench/run_bench.py --gg       # 우리 엔진만
    python bench/run_bench.py --edge     # Edge만

같은 JS 파일(bench/js/*.js)을 양쪽에서 그대로 실행하고,
스크립트가 찍는 "BENCH <이름> <ms>" 콘솔 라인을 수집해 비교한다.
"""

import os
import re
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

BENCH_DIR = os.path.join(ROOT, "bench", "js")
EDGE = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
SEED_HTML = "<html><body><div id=seed></div></body></html>"

BENCH_LINE = re.compile(r"BENCH (\S+) ([0-9.]+)")


def parse_lines(lines):
    out = {}
    for line in lines:
        m = BENCH_LINE.search(line)
        if m:
            out[m.group(1)] = float(m.group(2))
    return out


def run_ggjs(js_code):
    """손수 만든 gg-js VM에서 실행. (결과 dict, 총 실행 wall ms)"""
    import ggcore

    t0 = time.perf_counter()
    logs = ggcore.jsvm_run(js_code)
    wall = (time.perf_counter() - t0) * 1000
    for line in logs:
        if line.startswith("ERROR"):
            print("  [gg-js 오류]", line)
    return parse_lines(logs), wall


def run_ggjs_dom(js_code):
    """gg-js + 우리 DOM (GGJS=1로 Doc.run_scripts가 gg-js에 라우팅)."""
    import ggcore

    os.environ["GGJS"] = "1"
    try:
        doc = ggcore.parse_html(SEED_HTML)
        t0 = time.perf_counter()
        logs = doc.run_scripts([js_code])
        wall = (time.perf_counter() - t0) * 1000
    finally:
        os.environ.pop("GGJS", None)
    for line in logs:
        if line.startswith("[gg-js error]"):
            print("  [gg-js 오류]", line)
    return parse_lines(logs), wall


def run_ggcore(js_code):
    """Boa 경로에서 실행. (결과 dict, 컨텍스트 생성+총 실행 wall ms)"""
    import ggcore

    doc = ggcore.parse_html(SEED_HTML)
    t0 = time.perf_counter()
    logs = doc.run_scripts([js_code])
    wall = (time.perf_counter() - t0) * 1000
    for line in logs:
        if line.startswith("ERROR"):
            print("  [gg 오류]", line)
    return parse_lines(logs), wall


def gg_startup_ms(runs=20):
    """새 JS 컨텍스트 생성 + 한 줄 실행까지의 시간(ms, 최솟값)."""
    import ggcore

    best = float("inf")
    for _ in range(runs):
        doc = ggcore.parse_html(SEED_HTML)
        t0 = time.perf_counter()
        doc.run_scripts(["console.log(1+1)"])
        best = min(best, (time.perf_counter() - t0) * 1000)
    return best


def run_edge(js_code, v8_flags=None):
    """Edge 헤드리스에서 같은 코드를 실행. (결과 dict, 프로세스 전체 wall ms)

    v8_flags: V8에 넘길 플래그 (예: ["--jitless"] — JIT 없이 인터프리터만).
    """
    html = (
        "<!doctype html><html><body><div id=seed></div>\n<script>\n"
        "var __lines = [];\n"
        "console.log = function (s) { __lines.push(String(s)); };\n"
        "try {\n" + js_code + "\n} catch (e) { __lines.push('ERROR ' + e); }\n"
        "var pre = document.createElement('pre');\n"
        "pre.textContent = '@@BENCH@@\\n' + __lines.join('\\n') + '\\n@@END@@';\n"
        "document.body.appendChild(pre);\n"
        "</scr" + "ipt></body></html>"
    )
    tmpdir = tempfile.mkdtemp(prefix="ggbench_")
    page = os.path.join(tmpdir, "bench.html")
    with open(page, "w", encoding="utf-8") as f:
        f.write(html)
    cmd = [
        EDGE, "--headless", "--disable-gpu", "--no-first-run",
        "--no-default-browser-check",
        "--user-data-dir=" + os.path.join(tmpdir, "profile"),
        "--dump-dom", "file:///" + page.replace("\\", "/"),
    ]
    if v8_flags:
        cmd.insert(1, "--js-flags=" + " ".join(v8_flags))
    t0 = time.perf_counter()
    proc = subprocess.run(
        cmd, capture_output=True, text=True, encoding="utf-8", timeout=180,
    )
    wall = (time.perf_counter() - t0) * 1000
    m = re.search(r"@@BENCH@@\n(.*?)\n@@END@@", proc.stdout, re.S)
    if not m:
        print("  [edge 오류] 벤치 출력을 찾지 못함 (exit %d)" % proc.returncode)
        return {}, wall
    lines = m.group(1).splitlines()
    for line in lines:
        if line.startswith("ERROR"):
            print("  [edge 오류]", line)
    return parse_lines(lines), wall


def load(name):
    with open(os.path.join(BENCH_DIR, name), encoding="utf-8") as f:
        return f.read()


def table(title, cols):
    """cols: [(엔진 라벨, {벤치 이름: ms})]"""
    print("\n== %s (ms) ==" % title)
    names = []
    for _, d in cols:
        for k in d:
            if k not in names:
                names.append(k)
    if not names:
        print("  (측정 없음)")
        return
    w = max(len(n) for n in names) + 2
    print("%-*s" % (w, "벤치") + "".join("%11s" % lab for lab, _ in cols))
    for n in names:
        row = "%-*s" % (w, n)
        for _, d in cols:
            v = d.get(n)
            row += "%11s" % ("%.1f" % v if v is not None else "-")
        print(row)


def main():
    args = sys.argv[1:]
    do_gg = "--edge" not in args
    do_edge = "--gg" not in args
    v8_flags = ["--jitless"] if "--jitless" in args else None

    gg_ok = ggjs_ok = False
    if do_gg:
        try:
            import ggcore
            gg_ok = hasattr(ggcore, "parse_html")
            ggjs_ok = hasattr(ggcore, "jsvm_run")
        except ImportError:
            pass
        if not gg_ok:
            print("ggcore 휠이 설치돼 있지 않아 우리 엔진 측정은 건너뜀")

    for fname, title, ggjs_runner in [
        ("engine_bench.js", "순수 연산", run_ggjs),
        ("dom_bench.js", "DOM 조작", run_ggjs_dom),
    ]:
        code = load(fname)
        cols = []
        if do_gg and ggjs_ok:
            try:
                r, _ = ggjs_runner(code)
                cols.append(("gg-js", r))
            except Exception as e:
                print("  [gg-js 실행 불가] %s" % e)
        if do_gg and gg_ok:
            r, _ = run_ggcore(code)
            cols.append(("Boa", r))
        if do_edge and os.path.exists(EDGE):
            r, _ = run_edge(code, v8_flags)
            cols.append(("V8", r))
        table(title, cols)

    if do_gg and gg_ok:
        print("\n엔진 시동(컨텍스트 생성+1줄 실행): %.2f ms" % gg_startup_ms())


if __name__ == "__main__":
    main()
