import unittest

from browser import native, net
from browser.driver import Page
from browser.network_backend import (
    LocalNetworkBackend,
    NetworkBackend,
)
from browser.renderer_session import LocalRendererSession, RendererSession
from browser.security import ScriptFetchResponse


class RecordingBackend:
    """Deterministic backend used to prove callers do not reach net globals."""

    def __init__(self, page=""):
        self.page = page
        self.resources = {}
        self.calls = []
        self.cookie_writes = []
        self.contexts = []

    def bind_context(self, document_url, **kwargs):
        context = {"document_url": str(document_url)}
        self.contexts.append(context)
        return context

    def drop_context(self, context):
        self.contexts.remove(context)
        return True

    def new_cancel_token(self):
        return net.CancellationToken()

    def cancel(self, token):
        return token.cancel()

    def request_text(self, url, **kwargs):
        self.calls.append(("navigate", str(url)))
        return {}, self.page, url

    def request(self, url, **kwargs):
        self.calls.append(("text", str(url)))
        return {}, self.resources.get(str(url), "")

    def request_raw(self, url, **kwargs):
        self.calls.append(("binary", str(url)))
        return {}, self.resources.get(str(url), b"")

    def perform_script_fetch(self, base_url, request, **kwargs):
        target = base_url.resolve(request["url"])
        self.calls.append(("script-fetch", str(target)))
        return ScriptFetchResponse(
            200, {"content-type": "text/plain"},
            self.resources.get(str(target), ""), target)

    def cookies_for(self, url, **kwargs):
        self.calls.append(("cookies", str(url)))
        return "seed=one"

    def set_cookie_from_js(self, url, value, **kwargs):
        self.cookie_writes.append((str(url), value))
        return True


class TestProcessNeutralSeams(unittest.TestCase):
    def test_local_backend_satisfies_contract(self):
        self.assertIsInstance(LocalNetworkBackend(), NetworkBackend)

    @unittest.skipUnless(native.available(), "native ggcore wheel required")
    def test_page_routes_navigation_resources_and_cookies_through_backend(self):
        backend = RecordingBackend("""
<!doctype html>
<link rel="stylesheet" href="/site.css">
<p id="out">before</p>
<script src="/app.js"></script>
""")
        backend.resources.update({
            "https://example.test/site.css": "#out { color: red; }",
            "https://example.test/app.js": (
                "document.getElementById('out').textContent='after';"
                "document.cookie='theme=dark; Path=/';"),
        })

        page = Page(network_backend=backend)
        page.goto("https://example.test/", settle=False)

        self.assertEqual(page.text("#out"), "after")
        self.assertIn(("navigate", "https://example.test/"), backend.calls)
        self.assertIn(
            ("text", "https://example.test/site.css"), backend.calls)
        self.assertIn(
            ("text", "https://example.test/app.js"), backend.calls)
        self.assertTrue(any(
            value.startswith("theme=dark")
            for _url, value in backend.cookie_writes))
        self.assertEqual(len(backend.contexts), 1)

    @unittest.skipUnless(native.available(), "native ggcore wheel required")
    def test_renderer_session_owns_commit_tick_query_and_refresh(self):
        backend = RecordingBackend()
        session = LocalRendererSession(backend)
        self.assertIsInstance(session, RendererSession)
        url = net.URL("https://example.test/")
        root, doc, css, logs = session.commit(url, """
<!doctype html><p id="late">before</p>
<script>
setTimeout(function () {
  document.getElementById('late').textContent = 'after';
}, 10);
</script>
""")

        self.assertIs(session.doc, doc)
        self.assertIs(session.root, root)
        self.assertEqual(session.css_sources, css)
        update = session.tick(20)
        rows = session.query("#late", first=True)
        self.assertTrue(update.dom_changed)
        self.assertEqual(rows[0][3], "after")
        self.assertEqual(session.state().document_url,
                         "https://example.test/")
        self.assertIsNotNone(session.frame(900))
        session.close()
        self.assertIsNone(session.doc)

    @unittest.skipUnless(native.available(), "native ggcore wheel required")
    def test_script_fetch_is_backend_owned(self):
        backend = RecordingBackend()
        backend.resources["https://example.test/api"] = "payload"
        session = LocalRendererSession(backend)
        session.commit(net.URL("https://example.test/"), """
<!doctype html><p id="out">waiting</p>
<script>
fetch('/api').then(function (response) { return response.text(); })
  .then(function (text) {
    document.getElementById('out').textContent = text;
  });
</script>
""")
        session.settle()

        self.assertIn(
            ("script-fetch", "https://example.test/api"), backend.calls)
        self.assertEqual(session.query("#out", first=True)[0][3], "payload")


if __name__ == "__main__":
    unittest.main()
