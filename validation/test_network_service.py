"""The network service owns the profile's credentials, so these tests
are about two things: that it behaves exactly like the local backend,
and that the browser process no longer holds what it moved out."""

import http.server
import socketserver
import threading
import time
import unittest

from browser import net
from browser.network_backend import (NetworkBackend, create_network_backend,
                                     default_network_backend)
from browser.ipc.wire import from_wire, to_wire


class _Server(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True

    def handle_error(self, request, client_address):
        # the cancel test closes a socket mid-response on purpose; a
        # traceback in a passing run reads like a failure
        pass


class _Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _send(self, body, headers=()):
        try:
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            for name, value in headers:
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass          # the client cancelled; nothing to report

    def do_GET(self):
        if self.path.startswith("/slow"):
            time.sleep(1.5)
            self._send(b"slow")
        elif self.path == "/cookie/set":
            self.send_response(200)
            self.send_header("Set-Cookie", "sid=abc; Path=/")
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")
        elif self.path == "/cookie/echo":
            self._send(self.headers.get("Cookie", "").encode())
        else:
            self._send(b"hello" + self.path.encode())


class TestWireKwargs(unittest.TestCase):
    def test_url_arguments_survive_and_unknown_context_is_refused(self):
        wire = to_wire({
            "site_for_cookies": net.URL("https://top.test/"),
            "top_level_navigation": False,
            "cancel_token": object(),
        })
        self.assertNotIn("cancel_token", wire)
        self.assertEqual(wire["url_kwargs"],
                         {"site_for_cookies": "https://top.test/"})
        rebuilt = from_wire(wire)
        self.assertEqual(str(rebuilt["site_for_cookies"]),
                         "https://top.test/")
        self.assertEqual(rebuilt["site_for_cookies"].scheme, "https")
        # a named context the far side does not hold is a protocol
        # error, never a silent downgrade to unbound rights
        from browser.ipc.protocol import ProtocolError

        with self.assertRaises(ProtocolError):
            from_wire({"context_id": "nope"}, contexts={})


class TestNetworkService(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = _Server(("127.0.0.1", 0), _Handler)
        cls.thread = threading.Thread(target=cls.server.serve_forever,
                                      daemon=True)
        cls.thread.start()
        cls.base = f"http://127.0.0.1:{cls.server.server_address[1]}"
        cls.backend = create_network_backend("service")

    @classmethod
    def tearDownClass(cls):
        cls.backend.close()
        cls.server.shutdown()

    def test_backend_satisfies_the_contract_and_runs_out_of_process(self):
        self.assertIsInstance(self.backend, NetworkBackend)
        self.assertNotEqual(self.backend.host.pid, None)
        self.assertTrue(self.backend.host.alive)

    def test_fetch_shapes_match_the_local_backend(self):
        url = net.URL(self.base + "/a")
        local = default_network_backend()
        _h, remote_text, remote_final = self.backend.request_text(url)
        _h, local_text, local_final = local.request_text(url)
        self.assertEqual(remote_text, local_text)
        self.assertEqual(str(remote_final), str(local_final))
        self.assertEqual(self.backend.request_raw(net.URL(self.base + "/b"))[1],
                         b"hello/b")

    def test_cookies_stay_in_the_service_not_the_browser_process(self):
        """The point of the split: a Set-Cookie the service applied must
        not appear in this process's jar."""
        backend = create_network_backend("service")
        try:
            backend.request_text(net.URL(self.base + "/cookie/set"))
            _h, echoed = backend.request(net.URL(self.base + "/cookie/echo"))
            self.assertIn("sid=abc", echoed)
            self.assertIn("sid=abc", backend.cookies_for(
                net.URL(self.base + "/")))
            # the browser process never saw it
            self.assertNotIn("sid=abc",
                             net.cookies_for(net.URL(self.base + "/")))
        finally:
            backend.close()

    def test_concurrent_fetches_overlap_rather_than_queue(self):
        """One pipe must not serialize a page's subresource fan-out, so
        the browser side routes replies by id instead of lock-stepping."""
        from concurrent.futures import ThreadPoolExecutor

        started = time.monotonic()
        with ThreadPoolExecutor(max_workers=4) as pool:
            bodies = list(pool.map(
                lambda i: self.backend.request(
                    net.URL(f"{self.base}/slow?{i}"))[1],
                range(4)))
        elapsed = time.monotonic() - started
        self.assertEqual(bodies, ["slow"] * 4)
        # serialized would be ~6s; overlapped is ~1.5s
        self.assertLess(elapsed, 4.0, f"fetches serialized: {elapsed:.1f}s")

    def test_cancel_reaches_an_in_flight_request_with_its_own_type(self):
        """A cancel that queued behind the request it is stopping would
        be useless, and callers tell a cancelled navigation from a
        failed one by exception type — so both have to survive."""
        token = self.backend.new_cancel_token()
        outcome = {}

        def fetch():
            try:
                self.backend.request(net.URL(self.base + "/slow"),
                                     cancel_token=token)
                outcome["result"] = "completed"
            except net.RequestCancelled:
                outcome["result"] = "cancelled"
            except Exception as exc:
                outcome["result"] = type(exc).__name__

        worker = threading.Thread(target=fetch)
        worker.start()
        time.sleep(0.3)
        self.backend.cancel(token)
        worker.join(timeout=8)
        self.assertEqual(outcome.get("result"), "cancelled")

    def test_context_lifetime_is_owned_by_the_service(self):
        context = self.backend.bind_context(self.base + "/")
        self.assertTrue(context["context_id"])
        self.assertEqual(context["document_url"], self.base + "/")
        self.assertTrue(self.backend.drop_context(context))
        self.assertFalse(self.backend.drop_context(context))

    def test_a_crashed_service_is_restarted_and_keeps_its_grants(self):
        """A dead service must not make the browser permanently
        unusable. The in-memory jar is gone — that is what a crash
        costs — but the capabilities are the browser's to re-grant, so
        a caller holding a context id keeps working."""
        from browser.process.network_host import NetworkCrashed

        backend = create_network_backend("service")
        try:
            context = backend.bind_context(self.base + "/")
            first_pid = backend.host.pid
            backend.request_text(net.URL(self.base + "/cookie/set"),
                                 context=context)
            self.assertIn("sid=abc",
                          backend.cookies_for(net.URL(self.base + "/")))

            backend.host.test_crash()
            self.assertFalse(backend.host.alive)

            # the very next call brings it back and is served
            _h, body, _f = backend.request_text(
                net.URL(self.base + "/after"), context=context)
            self.assertEqual(body, "hello/after")
            self.assertNotEqual(backend.host.pid, first_pid)
            self.assertEqual(backend.host.restarts, 1)

            # the context the caller still holds resolves in the new
            # service; the jar legitimately did not survive
            self.assertTrue(backend.drop_context(context))
            self.assertNotIn(
                "sid=abc", backend.cookies_for(net.URL(self.base + "/")))

            # ...and a request in flight when it died fails honestly
            backend.host.test_crash()
            backend.host._closing = True
            with self.assertRaises(NetworkCrashed):
                backend.request_text(net.URL(self.base + "/x"))
        finally:
            backend.close()

    def test_unknown_model_is_refused(self):
        with self.assertRaises(ValueError):
            create_network_backend("telepathy")


if __name__ == "__main__":
    unittest.main()
