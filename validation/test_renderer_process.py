import multiprocessing
import os
import unittest

from browser import native, net
from browser.driver import Page
from browser.ipc.blobs import BlobError, BlobStore
from browser.ipc.channel import JsonChannel
from browser.process.sandbox import apply_renderer_sandbox, renderer_limits
from browser.ipc.protocol import (
    MAX_CONTROL_BYTES,
    ProtocolError,
    decode_envelope,
    encode_envelope,
    make_envelope,
)
from browser.process.browser_host import RendererCrashed, RendererHung
from browser.renderer_session import RemoteRendererSession
from validation.test_process_seams import RecordingBackend


class TestIpcProtocol(unittest.TestCase):
    def test_json_round_trip_and_unknown_fields_rejected(self):
        message = make_envelope(
            "renderer.state", "1", renderer_id="r-test",
            generation=2, document_token="abc", payload={})
        self.assertEqual(decode_envelope(encode_envelope(message)), message)
        message["surprise"] = True
        with self.assertRaises(ProtocolError):
            encode_envelope(message)

    def test_malformed_oversized_url_and_headers_rejected(self):
        with self.assertRaises(ProtocolError):
            decode_envelope(b"{not-json")
        with self.assertRaises(ProtocolError):
            decode_envelope(b"x" * (MAX_CONTROL_BYTES + 1))
        with self.assertRaises(ProtocolError):
            make_envelope(
                "renderer.commit", "1", renderer_id="r-test",
                payload={"url": "https://x/" + "a" * (17 * 1024)})
        with self.assertRaises(ProtocolError):
            make_envelope(
                "renderer.commit", "1", renderer_id="r-test",
                payload={"headers": {"x": "a" * (65 * 1024)}})

    def test_duplicate_request_id_rejected(self):
        left, right = multiprocessing.Pipe(duplex=True)
        try:
            receiver = JsonChannel(left, "r-test")
            raw = encode_envelope(make_envelope(
                "renderer.state", "same", renderer_id="r-test"))
            right.send_bytes(raw)
            receiver.recv()
            right.send_bytes(raw)
            with self.assertRaises(ProtocolError):
                receiver.recv()
        finally:
            left.close()
            right.close()


@unittest.skipUnless(native.available(), "native ggcore wheel required")
class TestRendererProcess(unittest.TestCase):
    def test_local_and_isolated_page_results_match(self):
        body = """
<!doctype html><title>parity</title>
<h1 id="heading">Hello</h1><a id="link" href="/next">Next</a>
<script>document.getElementById('heading').textContent += ' world';</script>
"""
        local = Page(
            network_backend=RecordingBackend(body), process_model="local")
        isolated = Page(
            network_backend=RecordingBackend(body), process_model="isolated")
        try:
            local.goto("https://example.test/", settle=False)
            isolated.goto("https://example.test/", settle=False)
            self.assertEqual(isolated.title, local.title)
            self.assertEqual(isolated.text("h1"), local.text("h1"))
            self.assertEqual(isolated.attr("#link", "href"),
                             local.attr("#link", "href"))
            compact = lambda page: [
                (item["role"], item["tag"], item["name"], item["id"])
                for item in page.snapshot()]
            self.assertEqual(compact(isolated), compact(local))
        finally:
            local.close()
            isolated.close()

    def test_isolated_page_matches_local_contract_and_brokers_network(self):
        backend = RecordingBackend("""
<!doctype html><title>isolated</title>
<link rel="stylesheet" href="/site.css">
<button id="go" onclick="this.textContent='clicked'">before</button>
<p id="api">waiting</p>
<script src="/app.js"></script>
""")
        backend.resources.update({
            "https://example.test/site.css": "#go { color: red; }",
            "https://example.test/app.js": (
                "document.cookie='remote=yes; Path=/';"
                "document.getElementById('go').addEventListener('click',"
                "function(){this.textContent='clicked';});"
                "fetch('/api').then(function(r){return r.text();})"
                ".then(function(t){document.getElementById('api').textContent=t;});"),
            "https://example.test/api": "brokered",
        })
        page = Page(network_backend=backend, process_model="isolated")
        try:
            page.goto("https://example.test/")
            self.assertEqual(page.title, "isolated")
            self.assertEqual(page.text("#api"), "brokered")
            self.assertTrue(page.click("#go"))
            self.assertEqual(page.text("#go"), "clicked")
            self.assertTrue(page.snapshot())
            self.assertIn(
                ("script-fetch", "https://example.test/api"), backend.calls)
            self.assertTrue(any(
                value.startswith("remote=yes")
                for _url, value in backend.cookie_writes))
        finally:
            page.close()

    @unittest.skipUnless(native.available(), "native ggcore not built")
    def test_layout_and_scroll_seams_survive_the_json_boundary(self):
        """JSON decodes every row as a list, and the native bindings
        only extract real tuples — so both seams have to re-tuple in
        the worker or an isolated page load dies on its first layout."""
        body = ("<div id='sc' style='overflow:auto;height:100px'>"
                "<div style='height:500px'>x</div></div>"
                "<script>var e=document.getElementById('sc');"
                "e.scrollTo(0, 60); e.scrollBy({top: 25});"
                "window.scrollTo(0, 500); e.scrollIntoView();</script>")
        session = RemoteRendererSession(RecordingBackend(body))
        try:
            session.commit(net.URL("https://example.test/"), body)
            # the two host->renderer pushes must not raise
            session.set_layout_rects([(2, 0.0, 0.0, 200.0, 100.0)])
            session.set_scroll_state(
                [(2, 0.0, 0.0, 500.0, 200.0),
                 (0xFFFFFFFF, 500.0, 0.0, 3000.0, 800.0)])
            writes = [tuple(w) for w in session.take_scroll_writes()]
            self.assertEqual([(w[1], w[4]) for w in writes],
                             [(60.0, False), (25.0, True), (500.0, False)])
            self.assertEqual(int(writes[2][0]), 0xFFFFFFFF)
            self.assertEqual([w[3] for w in writes], sorted(w[3] for w in writes))
            self.assertEqual(
                [len(tuple(v)) for v in session.take_scroll_into_view()], [2])
            # and the page's own offset reaches window.scrollY
            self.assertEqual(session.run(["console.log(window.scrollY)"]),
                             ["500"])
        finally:
            session.close()

    def test_large_commit_uses_blob_and_stale_response_is_discarded(self):
        session = RemoteRendererSession(RecordingBackend())
        try:
            padding = "x" * (70 * 1024)
            session.commit(
                net.URL("https://example.test/"),
                f"<!doctype html><!--{padding}--><p id='ok'>yes</p>")
            self.assertEqual(session.query("#ok", first=True)[0][3], "yes")
            self.assertEqual(session.test_stale(), {"stale": False})
        finally:
            session.shutdown()

    def test_crash_and_hang_are_contained_and_next_commit_recovers(self):
        backend = RecordingBackend()
        session = RemoteRendererSession(backend)
        try:
            session.commit(
                net.URL("https://example.test/"), "<p id='v'>one</p>")
            first_pid = session.pid
            with self.assertRaises(RendererCrashed):
                session.test_crash()
            self.assertFalse(session.alive)
            self.assertEqual(backend.contexts, [])
            session.commit(
                net.URL("https://example.test/"), "<p id='v'>two</p>")
            self.assertNotEqual(session.pid, first_pid)
            self.assertEqual(session.query("#v", first=True)[0][3], "two")

            with self.assertRaises(RendererHung):
                session.test_hang(timeout=0.1)
            self.assertFalse(session.alive)
            self.assertEqual(backend.contexts, [])
            session.commit(
                net.URL("https://example.test/"), "<p id='v'>three</p>")
            self.assertEqual(session.query("#v", first=True)[0][3], "three")
        finally:
            session.shutdown()

    def test_malformed_peer_message_kills_only_renderer_and_recovers(self):
        session = RemoteRendererSession(RecordingBackend())
        try:
            first_pid = session.pid
            session.host.channel.connection.send_bytes(b"{malformed-json")
            session.host.process.join(timeout=2.0)
            self.assertFalse(session.alive)
            session.commit(
                net.URL("https://example.test/"), "<p id='ok'>recovered</p>")
            self.assertNotEqual(session.pid, first_pid)
            self.assertEqual(
                session.query("#ok", first=True)[0][3], "recovered")
        finally:
            session.shutdown()


def _sandbox_report_child(queue):
    """Spawn target: apply the sandbox, report the effective rlimits."""
    import resource

    report = apply_renderer_sandbox()
    queue.put({
        "report": report,
        "cpu": resource.getrlimit(resource.RLIMIT_CPU),
        "as": resource.getrlimit(resource.RLIMIT_AS),
        "core": resource.getrlimit(resource.RLIMIT_CORE),
    })


def _memory_bomb_child(queue):
    """Spawn target: a renderer-side allocation bomb must be contained."""
    apply_renderer_sandbox()
    try:
        block = bytearray(1024 * 1024 * 1024)  # 1 GiB against a 512MB cap
        block[-1] = 1
        queue.put("allocated")
    except MemoryError:
        queue.put("contained")


def _cpu_spin_child():
    """Spawn target: an infinite loop must die at the CPU quota."""
    apply_renderer_sandbox()
    while True:
        pass


class _EnvPatch:
    def __init__(self, **values):
        self.values = values
        self.saved = {}

    def __enter__(self):
        for key, value in self.values.items():
            self.saved[key] = os.environ.get(key)
            os.environ[key] = value

    def __exit__(self, *exc):
        for key, old in self.saved.items():
            if old is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = old


@unittest.skipUnless(os.name == "posix", "POSIX rlimit sandbox")
class TestRendererSandbox(unittest.TestCase):
    def test_limits_and_report_apply_in_a_child(self):
        import resource

        ctx = multiprocessing.get_context("spawn")
        queue = ctx.Queue()
        process = ctx.Process(target=_sandbox_report_child, args=(queue,))
        process.start()
        try:
            out = queue.get(timeout=30)
        finally:
            process.join(timeout=10)
        limits = renderer_limits()
        self.assertNotEqual(out["cpu"][0], resource.RLIM_INFINITY)
        self.assertLessEqual(out["cpu"][0], limits["cpu_seconds"])
        self.assertNotEqual(out["as"][0], resource.RLIM_INFINITY)
        self.assertLessEqual(
            out["as"][0], limits["memory_mb"] * 1024 * 1024)
        self.assertEqual(out["core"][0], 0)
        self.assertIn("umask=077", out["report"])

    def test_memory_quota_contains_an_allocation_bomb(self):
        with _EnvPatch(GG_RENDERER_MEMORY_MB="512"):
            ctx = multiprocessing.get_context("spawn")
            queue = ctx.Queue()
            process = ctx.Process(target=_memory_bomb_child, args=(queue,))
            process.start()
            try:
                outcome = queue.get(timeout=60)
            finally:
                process.join(timeout=10)
        self.assertEqual(outcome, "contained")

    def test_cpu_quota_kills_a_spinning_renderer(self):
        with _EnvPatch(GG_RENDERER_CPU_S="1"):
            ctx = multiprocessing.get_context("spawn")
            process = ctx.Process(target=_cpu_spin_child)
            process.start()
            process.join(timeout=90)
        self.assertIsNotNone(process.exitcode, "spinner outlived its quota")
        self.assertLess(process.exitcode, 0)  # killed by SIGXCPU/SIGKILL

    def test_sandbox_can_be_disabled_for_debugging(self):
        with _EnvPatch(GG_RENDERER_SANDBOX="0"):
            self.assertEqual(apply_renderer_sandbox(), ["disabled"])

    def test_renderer_handshake_reports_its_sandbox(self):
        session = RemoteRendererSession(RecordingBackend())
        try:
            report = session.host.sandbox_report
            self.assertTrue(report)
            self.assertTrue(any(item.startswith("cpu_s=")
                                for item in report), report)
            self.assertTrue(any(item.startswith("as_mb=")
                                for item in report), report)
        finally:
            session.shutdown()


class TestBlobQuota(unittest.TestCase):
    def test_store_total_quota_bounds_shared_memory(self):
        store = BlobStore(max_total=150 * 1024)
        big = b"x" * (100 * 1024)  # above INLINE_LIMIT -> shared memory
        try:
            store.pack(big)
            with self.assertRaises(BlobError):
                store.pack(big)  # 200KB total vs the 150KB store cap
        finally:
            store.release_all()
        # releasing the leases returns the quota
        try:
            store.pack(big)
        finally:
            store.release_all()

    def test_single_blob_limit_still_applies(self):
        from browser.ipc import blobs

        store = BlobStore()
        try:
            with self.assertRaises(BlobError):
                store.pack(b"y" * (blobs.MAX_BLOB_BYTES + 1))
        finally:
            store.release_all()


if __name__ == "__main__":
    unittest.main()
