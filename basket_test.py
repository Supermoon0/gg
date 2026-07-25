# -*- coding: utf-8 -*-
"""Site basket: render representative pages headless and score how far
the engine gets (network, parse, style, layout, paint, JS errors).

Each site is measured in its own sandboxed child process (CPU/memory
rlimits + a wall-clock timeout), so one hostile or heavy page cannot
take down the whole run — the 07-25 CI run died with a runner shutdown
mid-namuwiki. Scoring happens after the async event loop settles:
naver mounts its React feed from fetch/timer work that only exists
after settle, and measuring straight after load_document graded the
pre-mount shell (218 elements) as a failure.
"""
import multiprocessing
import os
import sys
import time

REPO_ROOT = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, REPO_ROOT)
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

SITES = [
    ("네이버", "https://www.naver.com/"),
    ("위키백과", "https://ko.wikipedia.org/wiki/웹_브라우저"),
    ("나무위키", "https://namu.wiki/w/월드_와이드_웹"),
    ("연합뉴스", "https://www.yna.co.kr/"),
    ("티스토리", "https://www.tistory.com/"),
    ("HN", "https://news.ycombinator.com/"),
    ("MDN", "https://developer.mozilla.org/en-US/"),
    ("example", "https://example.com/"),
    ("정부24", "https://www.gov.kr/portal/main"),
]

SITE_TIMEOUT_S = 120     # wall clock per site (fetch + JS + settle + paint)
SETTLE_TIMEOUT_S = 20.0  # async event-loop budget per site


def _measure(name, url, out):
    """Child-process target: render one site, put a report row on `out`."""
    from concurrent.futures import ThreadPoolExecutor

    import tkinter
    from browser import net, native
    from browser.html_parser import Element, tree_to_list
    from browser.layout import DocumentLayout, paint_tree
    from browser.process.sandbox import apply_renderer_sandbox

    # CPU/address-space caps: a runaway page gets a MemoryError row,
    # not a dead runner VM
    apply_renderer_sandbox()
    root = tkinter.Tk()
    root.withdraw()
    t0 = time.perf_counter()
    try:
        base = net.URL(url)
        _h, body, final = net.request_text(base, no_cache=True)
        base = final

        def fetch_many(urls, binary=False):
            def one(u):
                try:
                    if binary:
                        return net.request_raw(
                            base.resolve(u), site_for_cookies=base,
                            top_level_navigation=False)[1]
                    return net.request_text(
                        base.resolve(u), site_for_cookies=base,
                        top_level_navigation=False)[1]
                except Exception:
                    return b"" if binary else ""
            with ThreadPoolExecutor(max_workers=6) as pool:
                return dict(zip(urls, pool.map(one, urls)))

        nodes, doc, css_sources, logs = native.load_document(
            body, fetch_many, fetch_many, page_url=base)
        # Settle the async event loop before scoring: SPA portals mount
        # their real content from fetch/timer work after the load, and
        # the shells show the settled DOM — the basket must measure the
        # same document (naver: 218 elements before, ~1,600 after).
        if native.async_available() and hasattr(doc, "step"):
            try:
                native.settle_lazy(
                    doc, css_sources, base, timeout=SETTLE_TIMEOUT_S)
                nodes = native.refresh(doc, css_sources)
            except Exception:
                pass  # score whatever the load produced

        flat = tree_to_list(nodes, [])
        elems = [n for n in flat if isinstance(n, Element)]
        styled = sum(1 for n in elems
                     if n.style.get("color") not in (None, "black")
                     or n.style.get("background-color"))
        d = DocumentLayout(nodes)
        d.layout(1280, 800)
        cmds = paint_tree(d, [])
        texts = [c for c in cmds if hasattr(c, "text")]
        imgs = [c for c in cmds if hasattr(c, "image_id")]
        js_err = sum(1 for l in logs if "error" in l.lower())
        dt = time.perf_counter() - t0
        verdict = ("읽을만함" if len(texts) > 40 and d.height > 300
                   else "빈약" if len(texts) > 3 else "실패")
        out.put(f"{name:10} {len(elems):>6} {styled:>6} {len(cmds):>6} "
                f"{len(texts):>6} {len(imgs):>5} {js_err:>6} "
                f"{dt:>5.1f}s  {verdict}")
    except Exception as e:
        dt = time.perf_counter() - t0
        out.put(f"{name:10} {'—':>6} {'—':>6} {'—':>6} {'—':>6} {'—':>5} "
                f"{'—':>6} {dt:>5.1f}s  예외: {type(e).__name__}: "
                f"{str(e)[:40]}")
    finally:
        try:
            root.destroy()
        except Exception:
            pass


def main():
    print(f"{'사이트':10} {'노드':>6} {'스타일':>6} {'페인트':>6} "
          f"{'텍스트':>6} {'이미지':>5} {'JS오류':>6} {'시간':>6}  결과")
    print("-" * 78)
    context = multiprocessing.get_context("spawn")
    for name, url in SITES:
        queue = context.Queue()
        process = context.Process(
            target=_measure, args=(name, url, queue), daemon=True)
        t0 = time.perf_counter()
        process.start()
        process.join(timeout=SITE_TIMEOUT_S)
        row = None
        if not queue.empty():
            row = queue.get()
        if process.is_alive():
            process.terminate()
            process.join(5)
            dt = time.perf_counter() - t0
            row = row or (f"{name:10} {'—':>6} {'—':>6} {'—':>6} {'—':>6} "
                          f"{'—':>5} {'—':>6} {dt:>5.1f}s  시간초과")
        elif row is None:
            dt = time.perf_counter() - t0
            row = (f"{name:10} {'—':>6} {'—':>6} {'—':>6} {'—':>6} "
                   f"{'—':>5} {'—':>6} {dt:>5.1f}s  "
                   f"프로세스 종료 code={process.exitcode}")
        print(row, flush=True)


if __name__ == "__main__":
    main()
