import multiprocessing
import unittest

from browser import native, net
from browser.driver import Page
from browser.ipc.channel import JsonChannel
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


if __name__ == "__main__":
    unittest.main()
