"""Optional native (Rust) fast path: HTML parse + CSS + style + gg-js.

If the compiled ggcore module is importable, the browser routes the
front half of the pipeline through Rust, including running scripts in
the native gg-js engine. Otherwise everything falls back to the
pure-Python implementation transparently (without JS).
"""

try:
    import ggcore
except ImportError:
    ggcore = None
else:
    # 휠이 설치되지 않은 채 프로젝트 루트에서 실행하면 러스트 소스 디렉토리
    # ggcore/ 가 네임스페이스 패키지로 잡힌다 — 진짜 확장 모듈인지 확인.
    if not hasattr(ggcore, "parse_html"):
        ggcore = None

import os
import json
import urllib.parse
import weakref

from .html_parser import Element, Text, tree_to_list
from . import modules as _modules
from .style import DEFAULT_STYLE_SHEET


_DYNAMIC_MODULE_PREFIX = "gg-module-import:"
_MODULE_RUNTIMES = weakref.WeakKeyDictionary()
_LEGACY_MODULE_RUNTIMES = {}


def _resolve_network_backend(network_backend):
    if network_backend is not None:
        return network_backend
    from .network_backend import default_network_backend
    return default_network_backend()


def _register_module_runtime(doc, graph):
    try:
        _MODULE_RUNTIMES[doc] = graph
    except TypeError:
        # Compatibility with an older wheel built before Doc gained a
        # weak-reference slot. Entries are bounded by explicit replacement
        # in the normal one-document-per-shell path.
        _LEGACY_MODULE_RUNTIMES[hash(doc)] = graph


def _module_runtime(doc):
    try:
        runtime = _MODULE_RUNTIMES.get(doc)
    except TypeError:
        runtime = None
    return runtime or _LEGACY_MODULE_RUNTIMES.get(hash(doc))


def available():
    return ggcore is not None


def build_tree(flat):
    """Rebuild a Python Element/Text tree from a Rust export dump."""
    root = None
    nodes = []
    for parent_idx, ridx, tag, text, attrs, style_pairs in flat:
        parent = nodes[parent_idx] if parent_idx >= 0 else None
        if tag is None:
            node = Text(text, parent)
        else:
            node = Element(tag, dict(attrs), parent)
        node.style = dict(style_pairs)
        node._font = None
        node._ridx = ridx
        nodes.append(node)
        if parent is not None:
            parent.children.append(node)
        else:
            root = node
    return root


class _ModuleGraph:
    """Per-document URL cache and dependency evaluator for ES modules."""

    def __init__(self, loader, page_url):
        self.loader = loader
        self.doc = loader.doc
        self.fetch_js = loader.fetch_js
        self.page_url = (str(page_url) if page_url
                         else "https://gg.invalid/")
        self.sources = {}
        self.base_urls = {}
        self.transforms = {}
        self.module_types = {}
        self.futures = {}
        self.prepare_state = {}
        self.errors = {}
        self.dynamic_errors = {}
        self.eval_serial = 0
        self.import_map = None
        self.import_map_error = None
        if hasattr(self.doc, "import_map_sources"):
            try:
                self.import_map = _modules.parse_import_maps(
                    list(self.doc.import_map_sources()), self.page_url)
            except _modules.ModuleSyntaxError as exc:
                self.import_map_error = exc

    @staticmethod
    def is_module(record):
        return record[3] in ("module", "async-module", "ordered-module")

    def root_key(self, record):
        node, kind, value, _mode = record
        return _modules.module_script_url(
            self.page_url, value, node, inline=(kind == "inline"))

    def submit(self, record):
        key = self.root_key(record)
        request_url = record[2]
        return self._source_future(key, request_url)

    def _source_future(self, key, request_url=None):
        from concurrent.futures import Future

        if key in self.sources:
            future = Future()
            future.set_result((True, self.sources[key]))
            return future
        if key not in self.futures:
            if self.loader is not None:
                self.futures[key] = self.loader.pool.submit(
                    self.loader._fetch_one, request_url or key)
            else:
                future = Future()
                try:
                    src = request_url or key
                    fetched = self.fetch_js([src])
                    future.set_result((src in fetched, fetched.get(src, "")))
                except Exception as exc:
                    future.set_exception(exc)
                self.futures[key] = future
        return self.futures[key]

    def register_root(self, record, source):
        key = self.root_key(record)
        self.sources.setdefault(key, source)
        if record[1] == "inline":
            self.base_urls.setdefault(key, self.page_url)
        else:
            self.base_urls.setdefault(key, key)
        return key

    def _prepare(self, key, module_type="javascript"):
        if self.import_map_error is not None:
            raise self.import_map_error
        previous_type = self.module_types.get(key)
        if previous_type is not None and previous_type != module_type:
            raise _modules.ModuleSyntaxError(
                f"module {key!r} requested as both {previous_type} and "
                f"{module_type}")
        self.module_types[key] = module_type
        state = self.prepare_state.get(key)
        if state == "prepared" or state == "preparing":
            return
        if state == "errored":
            raise self.errors[key]
        self.prepare_state[key] = "preparing"
        try:
            if key not in self.sources:
                found, source = self._source_future(key).result()
                if not found:
                    raise _modules.ModuleSyntaxError(
                        f"failed to fetch module {key}")
                self.sources[key] = source
                self.base_urls[key] = key
            base_url = self.base_urls.get(key, key)
            if module_type == "json":
                transformed = _modules.transform_json_module(
                    self.sources[key], base_url, module_key=key)
            else:
                transformed = _modules.transform_module(
                    self.sources[key], base_url, module_key=key,
                    import_map=self.import_map)
            self.transforms[key] = transformed
            # Start sibling fetches together before descending into the graph.
            all_dependencies = (
                transformed.dependencies + transformed.dynamic_dependencies)
            for dependency in all_dependencies:
                self._source_future(dependency)
            for dependency, dependency_type in zip(
                    transformed.dependencies, transformed.dependency_types):
                self._prepare(dependency, dependency_type)
            # Literal dynamic imports are fetched and compiled up front so
            # their factories remain usable after the Python loader returns,
            # but they are not evaluated until import() is called.
            for dependency, dependency_type in zip(
                    transformed.dynamic_dependencies,
                    transformed.dynamic_dependency_types):
                try:
                    self._prepare(dependency, dependency_type)
                except Exception as exc:
                    self.dynamic_errors[dependency] = exc
            self.prepare_state[key] = "prepared"
        except Exception as exc:
            self.prepare_state[key] = "errored"
            self.errors[key] = exc
            raise

    def _initialize_runtime(self, doc=None):
        doc = doc or self.doc
        lines = [
            "globalThis.__ggModuleRegistry__ = "
            "globalThis.__ggModuleRegistry__ || {};",
            "globalThis.__ggModuleFactories__ = "
            "globalThis.__ggModuleFactories__ || {};",
            "globalThis.__ggModuleStates__ = "
            "globalThis.__ggModuleStates__ || {};",
            "globalThis.__ggModuleErrors__ = "
            "globalThis.__ggModuleErrors__ || {};",
            "globalThis.__ggEvaluateModule__ = "
            "globalThis.__ggEvaluateModule__ || function(url){"
            "var states=globalThis.__ggModuleStates__;"
            "var modules=globalThis.__ggModuleRegistry__;"
            "if(states[url]===2){return Promise.resolve(modules[url]);}"
            "if(states[url]===1){return Promise.resolve(modules[url]);}"
            "if(states[url]===3){return Promise.reject("
            "globalThis.__ggModuleErrors__[url]);}"
            "var factory=globalThis.__ggModuleFactories__[url];"
            "if(typeof factory!==\"function\"){return Promise.reject("
            "new TypeError(\"module is not available: \"+url));}"
            "states[url]=1;"
            "return Promise.resolve().then(factory).then(function(){"
            "states[url]=2;return modules[url];},function(error){"
            "states[url]=3;globalThis.__ggModuleErrors__[url]=error;"
            "throw error;});};",
            "globalThis.__ggDynamicImport__ = "
            "globalThis.__ggDynamicImport__ || function(url){"
            "return globalThis.__ggEvaluateModule__(url);};",
            "globalThis.__ggDynamicImportExpression__ = "
            "globalThis.__ggDynamicImportExpression__ || "
            "function(specifier, referrer, options){"
            "return Promise.resolve().then(function(){"
            "var attributes={};"
            "if(options!==undefined){"
            "if(options===null||(typeof options!==\"object\"&&"
            "typeof options!==\"function\")){throw new TypeError("
            "\"dynamic import options must be an object\");}"
            "attributes=options.with;"
            "if(attributes===undefined){attributes={};}"
            "if(attributes===null||(typeof attributes!==\"object\"&&"
            "typeof attributes!==\"function\")){throw new TypeError("
            "\"dynamic import attributes must be an object\");}}"
            "var serialized=JSON.stringify(attributes);"
            "if(typeof serialized!==\"string\"){throw new TypeError("
            "\"dynamic import attributes are not serializable\");}"
            "var request=" + json.dumps(_DYNAMIC_MODULE_PREFIX) + "+"
            "encodeURIComponent(String(referrer))+\":\"+"
            "encodeURIComponent(String(specifier))+\":\"+"
            "encodeURIComponent(serialized);"
            "return fetch(request);}).then(function(response){"
            "if(!response.ok){throw new TypeError("
            "\"dynamic module request failed\");}"
            "return response.text();},function(error){"
            "throw new TypeError(String(error));}).then(function(url){"
            "return globalThis.__ggEvaluateModule__(url);});};",
        ]
        for key in self.transforms:
            encoded = json.dumps(key)
            lines.append(
                f"globalThis.__ggModuleRegistry__[{encoded}] = "
                f"globalThis.__ggModuleRegistry__[{encoded}] || {{}};")
        for key, transformed in self.transforms.items():
            encoded = json.dumps(key)
            expression = transformed.code.rstrip().removesuffix(";")
            body = ["var ready=Promise.resolve();"]
            for dependency in transformed.dependencies:
                dep = json.dumps(dependency)
                body.append(
                    "ready=ready.then(function(){return "
                    f"globalThis.__ggEvaluateModule__({dep});}});")
            body.append(
                "return ready.then(function(){return "
                + expression + ";});")
            lines.append(
                f"globalThis.__ggModuleFactories__[{encoded}] = "
                f"globalThis.__ggModuleFactories__[{encoded}] || "
                "function(){" + "".join(body) + "};")
        for key, error in self.dynamic_errors.items():
            encoded = json.dumps(key)
            message = json.dumps(str(error))
            lines.append(
                f"globalThis.__ggModuleRegistry__[{encoded}] = "
                f"globalThis.__ggModuleRegistry__[{encoded}] || {{}};")
            lines.append(
                f"globalThis.__ggModuleFactories__[{encoded}] = "
                f"globalThis.__ggModuleFactories__[{encoded}] || "
                f"function(){{throw new TypeError({message});}};")
        return doc.run_scripts(["\n".join(lines)])

    @staticmethod
    def _has_js_error(logs):
        return any(str(line).startswith("[gg-js error]") for line in logs)

    def _evaluate(self, key, logs):
        import time

        self.eval_serial += 1
        marker = f"__ggModuleWait{self.eval_serial}__"
        encoded = json.dumps(key)
        source = (
            f"var {marker}=0;"
            f"globalThis.__ggEvaluateModule__({encoded}).then("
            f"function(){{{marker}=1;}},function(error){{"
            f"{marker}=-1;console.log("
            f"\"[gg-js error] module evaluation failed: \"+error);}});")
        current = list(self.doc.run_scripts([source]))
        logs.extend(current)
        if self._has_js_error(current):
            return False

        # One module evaluation gets a SLICE of the load's JS budget,
        # never the whole thing: a module whose top-level await depends
        # on something we don't provide (naver's gfp-display-sdk ad
        # module retries on timers forever) would otherwise spin this
        # pump until loader.deadline and starve every later script —
        # main.js skipped == a blank page that mounts nothing.
        slice_deadline = min(
            self.loader.deadline,
            time.perf_counter() + max(0.5, self.loader.js_budget / 3.0))
        while time.perf_counter() <= slice_deadline:
            state = (self.doc.global_number(marker)
                     if hasattr(self.doc, "global_number") else None)
            if state in (1.0, -1.0):
                return state == 1.0

            current, requests = pump_script_requests(
                self.doc, microtasks_only=True)
            logs.extend(current)
            if self._has_js_error(current):
                return False
            for request in requests:
                self.loader._service_runtime_fetch(request)
            state = (self.doc.global_number(marker)
                     if hasattr(self.doc, "global_number") else None)
            if state in (1.0, -1.0):
                return state == 1.0
            if requests:
                continue
            if not self.doc.has_pending_work():
                break

            # A still-pending top-level await may depend on a short timer.
            current, requests = pump_script_requests(self.doc)
            logs.extend(current)
            if self._has_js_error(current):
                return False
            for request in requests:
                self.loader._service_runtime_fetch(request)

        logs.append(f"[gg-js error] module evaluation timed out: {key}")
        return False

    def evaluate(self, record, source):
        key = self.register_root(record, source)
        self._prepare(key)
        logs = list(self._initialize_runtime())
        if self._has_js_error(logs):
            return logs, False
        success = self._evaluate(key, logs)
        _register_module_runtime(self.doc, self)
        return logs, success

    def prepare_runtime_import(self, doc, referrer, specifier,
                               attributes=None):
        """Resolve, fetch, transform, and register a computed import URL."""
        module_type = _modules.module_type_from_import_attributes(attributes)
        resolved = _modules.resolve_module_specifier(
            referrer, specifier, import_map=self.import_map)
        self._prepare(resolved, module_type)
        logs = list(self._initialize_runtime(doc))
        if self._has_js_error(logs):
            raise _modules.ModuleSyntaxError(
                f"failed to initialize dynamically imported module {resolved}")
        return resolved

    def close(self):
        """Break Future/callback cycles while Doc is on its owner thread."""
        self.futures.clear()
        # Keep source/transform/evaluation metadata alive through the weak
        # per-document runtime registry. It is needed by import(expression)
        # after the parser-time loader and thread pool are gone.
        self.doc = None
        self.loader = None


class _ScriptLoader:
    """Schedule classic/module scripts around parser and page lifecycle.

    Parsing itself is currently whole-document, but execution follows the
    browser-visible guarantees: blocking scripts preserve parser order,
    defer/modules preserve document order before DOMContentLoaded, async
    scripts run in fetch-completion order, and dynamically inserted external
    scripts delay load (but not DOMContentLoaded).
    """

    def __init__(self, doc, fetch_js, js_budget, page_url=None,
                 network_backend=None, network_timeout=15.0,
                 cancel_token=None, network_context=None):
        import threading
        import time
        from concurrent.futures import ThreadPoolExecutor

        self.doc = doc
        self.fetch_js = fetch_js
        self.deadline = time.perf_counter() + js_budget
        self.js_budget = js_budget
        self.pool = ThreadPoolExecutor(max_workers=6)
        self.lock = threading.Lock()
        self.completion_serial = 0
        self.completed = {}
        self.known = set()
        self.async_pending = {}
        self.ordered_dynamic = []
        self.dynamic_inline = []
        self.dynamic_modules = []
        self.logs = []
        self.budget_reported = False
        self.page_url = page_url
        self.network_backend = _resolve_network_backend(network_backend)
        self.network_timeout = network_timeout
        self.cancel_token = cancel_token
        self.network_context = network_context
        self.module_graph = _ModuleGraph(self, page_url)

    def _service_runtime_fetch(self, request):
        if self.page_url is None:
            self.doc.reject_fetch(
                request["fetch_id"], "fetch has no document base URL")
            return False
        return service_script_fetch(
            self.doc, self.page_url, request,
            network_timeout=self.network_timeout,
            cancel_token=self.cancel_token,
            module_graph=self.module_graph,
            network_backend=self.network_backend,
            network_context=self.network_context)

    @staticmethod
    def _normalize(raw):
        node, kind, value, mode = raw
        return (int(node), str(kind), str(value), str(mode))

    def _fetch_one(self, src):
        fetched = self.fetch_js([src])
        return src in fetched, fetched.get(src, "")

    def _submit(self, record):
        node, _kind, src, _mode = record
        future = (self.module_graph.submit(record)
                  if self.module_graph.is_module(record)
                  else self.pool.submit(self._fetch_one, src))

        def completed(_future, node_idx=node):
            with self.lock:
                self.completion_serial += 1
                self.completed[node_idx] = self.completion_serial

        future.add_done_callback(completed)
        return future

    def _dispatch(self, node, event_type):
        if not hasattr(self.doc, "dispatch_event"):
            return
        try:
            logs, _handled, _prevented = self.doc.dispatch_event(
                node, event_type, False, False, None)
            self.logs.extend(logs)
        except Exception as exc:
            self.logs.append(
                f"[gg] script {event_type} handler failed: {exc}")

    def _run_code(self, record, code, *, external=False):
        import time

        node = record[0]
        if time.perf_counter() > self.deadline:
            if not self.budget_reported:
                self.logs.append(
                    f"[gg] JS budget ({self.js_budget:.0f}s) exceeded - "
                    "remaining script execution skipped")
                self.budget_reported = True
        elif self.module_graph.is_module(record):
            try:
                module_logs, succeeded = self.module_graph.evaluate(
                    record, code)
                self.logs.extend(module_logs)
                if not succeeded:
                    self._dispatch(node, "error")
                    external = False
            except _modules.ModuleSyntaxError as exc:
                self.logs.append(
                    f"[gg module error] {self.module_graph.root_key(record)}: "
                    f"{exc}")
                self._dispatch(node, "error")
                external = False
        elif code:
            self.logs.extend(self.doc.run_scripts([code]))
        if external:
            self._dispatch(node, "load")
        self._discover_dynamic()
        self._run_dynamic_inline()

    def _settle_fetch(self, record, future):
        found, code = future.result()
        if not found:
            self._dispatch(record[0], "error")
            self._discover_dynamic()
            self._run_dynamic_inline()
            return
        self._run_code(record, code, external=True)

    def _run_dynamic_inline(self):
        while self.dynamic_inline:
            record = self.dynamic_inline.pop(0)
            self._run_code(record, record[2])

    def _discover_dynamic(self):
        for raw in self.doc.script_records():
            record = self._normalize(raw)
            node, kind, _value, mode = record
            if node in self.known:
                continue
            self.known.add(node)
            if kind == "inline":
                if self.module_graph.is_module(record):
                    self.dynamic_modules.append(record)
                else:
                    self.dynamic_inline.append(record)
            elif mode in ("ordered", "ordered-module"):
                self.ordered_dynamic.append((record, self._submit(record)))
            else:
                # DOM-created external scripts carry force-async by default.
                self.async_pending[node] = (record, self._submit(record))

    def _ready_async(self):
        ready = []
        with self.lock:
            for node, (record, future) in self.async_pending.items():
                if future.done():
                    ready.append((self.completed.get(node, 0), node,
                                  record, future))
        ready.sort(key=lambda item: (item[0], item[1]))
        return ready

    def _drain_ready_async(self):
        while True:
            ready = self._ready_async()
            if not ready:
                return
            for _serial, node, record, future in ready:
                if node not in self.async_pending:
                    continue
                self.async_pending.pop(node, None)
                self._settle_fetch(record, future)

    def _wait_for(self, target=None):
        from concurrent.futures import FIRST_COMPLETED, wait

        futures = [future for _record, future
                   in self.async_pending.values()]
        if target is not None:
            futures.append(target)
        if futures:
            wait(futures, return_when=FIRST_COMPLETED)
        self._drain_ready_async()

    def _finish_pending(self):
        while (self.async_pending or self.ordered_dynamic
               or self.dynamic_inline or self.dynamic_modules):
            self._run_dynamic_inline()
            self._drain_ready_async()
            if self.ordered_dynamic:
                record, future = self.ordered_dynamic.pop(0)
                while not future.done():
                    self._wait_for(future)
                self._drain_ready_async()
                self._settle_fetch(record, future)
            elif self.async_pending:
                self._wait_for()
            elif self.dynamic_modules:
                record = self.dynamic_modules.pop(0)
                self._run_code(record, record[2])

    def run(self, initial_records):
        records = [self._normalize(raw) for raw in initial_records]
        self.known.update(record[0] for record in records)
        deferred = []
        try:
            # Parser phase. External async/defer work begins when its element
            # is encountered; blocking fetches pause later parser scripts.
            for record in records:
                _node, kind, value, mode = record
                if mode in ("defer", "module"):
                    future = self._submit(record) if kind == "src" else None
                    deferred.append((record, future))
                elif mode in ("async", "async-module"):
                    self.async_pending[record[0]] = (
                        record, self._submit(record))
                elif kind == "src":
                    future = self._submit(record)
                    future.result()  # parser-blocking fetch
                    self._drain_ready_async()
                    self._settle_fetch(record, future)
                else:
                    self._run_code(record, value)
                self._drain_ready_async()

            # Defer and module scripts wait for their own fetch but execute in
            # document order. Completed async scripts may interleave.
            for record, future in deferred:
                if future is not None:
                    while not future.done():
                        self._wait_for(future)
                    self._drain_ready_async()
                    self._settle_fetch(record, future)
                else:
                    self._run_code(record, record[2])
                self._drain_ready_async()

            self.logs.extend(self.doc.fire_dom_content_loaded())
            self._discover_dynamic()
            self._run_dynamic_inline()

            # DOMContentLoaded does not wait for these, but window.load does.
            self._finish_pending()

            self.logs.extend(self.doc.fire_load())
            # load handlers can insert more scripts. They no longer block the
            # already-fired load event, but still execute normally.
            self._discover_dynamic()
            self._finish_pending()
            return self.logs
        finally:
            self.pool.shutdown(wait=True)
            self.module_graph.close()


def _mark_framed(doc, framed):
    """`window.top !== window` has to be true before the page's own
    inline scripts run — that check and the parent handshake are both
    parser-time idioms in the widgets that use them."""
    if not framed:
        return
    try:
        doc.set_framed(True)
    except Exception:
        pass  # older wheel without the frame seam


def load_document(html, fetch_css, fetch_js=None, js_budget=8.0,
                  page_url=None, viewport_width=1280.0, timings=None,
                  network_backend=None, network_timeout=15.0,
                  cancel_token=None, network_context=None, framed=False):
    """Full native front half: parse -> scripts -> styles -> tree.

    fetch_css(hrefs) / fetch_js(srcs) -> {url: text} keep networking
    in Python. Returns (root, doc_handle, css_sources, js_console).

    js_budget: wall-clock seconds allowed for page scripts, checked
    between scripts. Heavy sites (naver, news portals) ship hundreds
    of KB of JS; without a budget the load stalls for minutes inside
    the JS engine. When exceeded, remaining scripts are skipped and
    the page renders with whatever ran — degraded, never frozen.
    """
    import threading
    import time

    network_backend = _resolve_network_backend(network_backend)
    load_started = time.perf_counter()

    def record(name, started):
        if timings is not None:
            timings[name] = (time.perf_counter() - started) * 1000.0

    # reader mode: app-shell pages carry their content as inline JSON;
    # surface it as ordinary markup before parsing (no-op elsewhere).
    # The JS engine now renders naver's feed natively (real card grid with
    # thumbnails), and the injected reader section pushes the feed's lazy
    # observers off-screen so they never mount — so reader mode is now an
    # opt-in fallback (GG_READER=1), not the default path.
    if os.environ.get("GG_READER") == "1":
        from . import reader
        html = reader.inject(html)

    stage_started = time.perf_counter()
    doc = ggcore.parse_html(html)
    record("html_parse", stage_started)

    # overlap: stylesheets download while scripts fetch and run.
    # fetch_css must be UI-silent (it runs off the main thread); results
    # land in prefetched and net's cache, so the post-JS pass is free.
    pre_hrefs = [v for k, v in doc.stylesheet_entries() if k == "link"]
    prefetched = {}
    warm = None
    if pre_hrefs:
        def _warm():
            try:
                prefetched.update(fetch_css(pre_hrefs))
            except Exception:
                pass
        warm = threading.Thread(target=_warm, daemon=True)
        warm.start()

    logs = []
    stage_started = time.perf_counter()
    if (fetch_js is not None and hasattr(doc, "script_records")
            and hasattr(doc, "fire_dom_content_loaded")
            and hasattr(doc, "fire_load")):
        if page_url:
            try:
                doc.set_page_url(str(page_url))
            except Exception:
                pass
            _mark_framed(doc, framed)
            try:
                host = getattr(page_url, "host", None)
                if host and hasattr(doc, "seed_cookies"):
                    jar = network_backend.cookies_for(
                        page_url, context=network_context)
                    if jar:
                        doc.seed_cookies(jar)
            except Exception:
                pass
        loader = _ScriptLoader(
            doc, fetch_js, js_budget, page_url,
            network_backend=network_backend,
            network_timeout=network_timeout,
            cancel_token=cancel_token,
            network_context=network_context)
        logs.extend(loader.run(doc.script_records()))
        sync_cookie_writes(
            doc, page_url, network_backend=network_backend,
            network_context=network_context)
    elif fetch_js is not None:
        import time
        entries = doc.script_entries()
        srcs = [value for kind, value in entries if kind == "src"]
        fetched = fetch_js(srcs) if srcs else {}
        sources = []
        for kind, value in entries:
            code = value if kind == "inline" else fetched.get(value, "")
            if code:
                sources.append(code)
        if page_url:
            try:
                doc.set_page_url(str(page_url))
            except Exception:
                pass  # older wheels have no set_page_url
            _mark_framed(doc, framed)
            # seed document.cookie from the network jar so page scripts
            # see the server session before they run
            try:
                host = getattr(page_url, "host", None)
                if host and hasattr(doc, "seed_cookies"):
                    jar = network_backend.cookies_for(
                        page_url, context=network_context)
                    if jar:
                        doc.seed_cookies(jar)
            except Exception:
                pass
        deadline = time.perf_counter() + js_budget
        for i, code in enumerate(sources):
            logs.extend(doc.run_scripts([code]))
            if time.perf_counter() > deadline:
                skipped = len(sources) - i - 1
                if skipped:
                    logs.append(
                        f"[gg] JS budget ({js_budget:.0f}s) exceeded - "
                        f"skipped {skipped} remaining script(s)")
                break
        # all scripts ran: fire DOMContentLoaded / load — app bundles
        # bootstrap from these
        if hasattr(doc, "fire_lifecycle"):
            try:
                logs.extend(doc.fire_lifecycle())
            except Exception:
                pass
        # Fold only actual JS setter writes back into the network jar. The
        # native bridge preserves their attributes; seeded HttpOnly-filtered
        # values are not mistaken for new writes.
        sync_cookie_writes(
            doc, page_url, network_backend=network_backend,
            network_context=network_context)
    record("scripts", stage_started)

    entries = doc.stylesheet_entries()
    hrefs = [value for kind, value in entries if kind == "link"]
    stage_started = time.perf_counter()
    if warm is not None:
        warm.join()
    fetched = dict(prefetched)
    missing = [h for h in hrefs if h not in fetched]
    if missing:
        fetched.update(fetch_css(missing))
    record("stylesheet_wait", stage_started)
    css_sources = [DEFAULT_STYLE_SHEET]
    for kind, value in entries:
        css = value if kind == "inline" else fetched.get(value, "")
        if css:
            css_sources.append(css)

    stage_started = time.perf_counter()
    doc.compute_styles(css_sources, viewport_width)
    record("style_compute", stage_started)
    stage_started = time.perf_counter()
    root = build_tree(doc.export())
    record("dom_export", stage_started)
    record("load_document_total", load_started)
    return root, doc, css_sources, logs


def refresh(doc, css_sources, viewport_width=1280.0):
    """Re-style and re-export after JS mutated the DOM."""
    doc.compute_styles(css_sources, viewport_width)
    return build_tree(doc.export())


def restyle_patch(doc, css_sources, root):
    """Partial invalidation: recompute styles in Rust and, when only
    paint-affecting properties changed, patch them into the existing
    Python tree in place. Returns the least work the caller must
    redo: 'none' | 'paint' | 'layout' | 'tree'. For 'layout'/'tree'
    the Rust styles are already fresh — re-export with
    build_tree(doc.export()), do not compute_styles again."""
    if not hasattr(doc, "restyle_diff"):
        doc.compute_styles(css_sources)
        return "tree"
    kind, patches = doc.restyle_diff(css_sources)
    if kind == 0:
        return "none"
    if kind == 2:
        return "layout"
    if kind == 3:
        return "tree"
    by_ridx = {}
    for n in tree_to_list(root, []):
        r = getattr(n, "_ridx", None)
        if r is not None:
            by_ridx[r] = n
    for ridx, style_pairs in patches:
        n = by_ridx.get(ridx)
        if n is None:
            return "tree"  # tree out of sync: rebuild
        n.style = dict(style_pairs)
        n._font = None  # style changed: invalidate the font cache
    return "paint"


def async_available():
    """The gg-js event loop (pump/fetch) is present in this wheel."""
    return ggcore is not None and hasattr(ggcore.Doc, "pump")


def dispatch_dom_event(doc, node_idx, event_type, bubbles=True,
                       cancelable=True, submitter_idx=None):
    """Dispatch a browser-triggered DOM event on new and old wheels."""
    if doc is None or not hasattr(doc, "dispatch_event"):
        return [], False, False
    return doc.dispatch_event(
        node_idx, event_type, bubbles, cancelable, submitter_idx)


def pump_script_requests(doc, dt_ms=None, *, microtasks_only=False):
    """Pump gg-js and normalize old/new wheel fetch request records."""
    if microtasks_only and hasattr(doc, "pump_microtasks_requests"):
        logs, raw_requests = doc.pump_microtasks_requests()
    elif dt_ms is None:
        method = (doc.pump_requests if hasattr(doc, "pump_requests")
                  else doc.pump)
        logs, raw_requests = method()
    else:
        method = (doc.tick_requests if hasattr(doc, "tick_requests")
                  else doc.tick)
        logs, raw_requests = method(dt_ms)
    requests = []
    for raw in raw_requests:
        if len(raw) >= 7:
            fetch_id, url, http_method, body, headers, mode, credentials = raw
        else:
            fetch_id, url = raw
            http_method, body, headers = "GET", "", []
            mode, credentials = "cors", "same-origin"
        requests.append({
            "fetch_id": fetch_id,
            "url": url,
            "method": http_method,
            "body": body,
            "headers": list(headers),
            "mode": mode,
            "credentials": credentials,
        })
    return list(logs), requests


def _service_dynamic_module_fetch(doc, request, graph=None):
    fetch_id = request["fetch_id"]
    try:
        payload = request["url"][len(_DYNAMIC_MODULE_PREFIX):]
        parts = payload.split(":", 2)
        if len(parts) not in (2, 3):
            raise _modules.ModuleSyntaxError("invalid dynamic module request")
        encoded_referrer, encoded_specifier = parts[:2]
        referrer = urllib.parse.unquote(encoded_referrer)
        specifier = urllib.parse.unquote(encoded_specifier)
        attributes = {}
        if len(parts) == 3:
            try:
                attributes = json.loads(urllib.parse.unquote(parts[2]))
            except (TypeError, ValueError) as exc:
                raise _modules.ModuleSyntaxError(
                    "invalid dynamic import attributes") from exc
        graph = graph or _module_runtime(doc)
        if graph is None:
            raise _modules.ModuleSyntaxError(
                "dynamic module runtime is no longer available")
        resolved = graph.prepare_runtime_import(
            doc, referrer, specifier, attributes)
        if hasattr(doc, "resolve_fetch_full"):
            doc.resolve_fetch_full(fetch_id, 200, resolved, resolved)
        else:
            doc.resolve_fetch(fetch_id, 200, resolved)
        return True
    except Exception as exc:
        doc.reject_fetch(fetch_id, f"{type(exc).__name__}: {exc}")
        return False


def service_script_fetch(doc, base_url, request, *, network_timeout=15.0,
                         cancel_token=None, module_graph=None,
                         network_backend=None, network_context=None):
    """Apply origin/CORS/credentials policy and settle one JS request."""
    if request["url"].startswith(_DYNAMIC_MODULE_PREFIX):
        return _service_dynamic_module_fetch(
            doc, request, graph=module_graph)
    network_backend = _resolve_network_backend(network_backend)
    fetch_id = request["fetch_id"]
    try:
        response = network_backend.perform_script_fetch(
            base_url, request, timeout=network_timeout,
            cancel_token=cancel_token, context=network_context)
        if hasattr(doc, "resolve_fetch_full"):
            header_items = [(str(k), str(v)) for k, v in
                            dict(response.headers or {}).items()]
            try:
                doc.resolve_fetch_full(
                    fetch_id, response.status, str(response.final_url),
                    response.body, header_items)
            except TypeError:  # older wheel: 4-arg signature
                doc.resolve_fetch_full(
                    fetch_id, response.status, str(response.final_url),
                    response.body)
        else:
            doc.resolve_fetch(fetch_id, response.status, response.body)
        return True
    except Exception as exc:
        doc.reject_fetch(fetch_id, f"{type(exc).__name__}: {exc}")
        from . import net
        if isinstance(exc, net.RequestCancelled):
            raise
        return False


def refresh_cookie_replica(doc, page_url, *, network_backend=None,
                           network_context=None):
    """Re-point `document.cookie` at the jar's current visible state.

    `document.cookie` is a synchronous read, so the VM keeps a replica
    seeded at load. Nothing refreshed it afterwards, which meant a
    `fetch()` that logged the user in set a session cookie the page
    could not see until it navigated. The jar filters HttpOnly, so the
    snapshot is exactly what script is allowed to know.
    """
    if doc is None or page_url is None or not getattr(page_url, "host", None):
        return False
    setter = getattr(doc, "set_cookie_snapshot", None)
    if setter is None:
        return False          # older wheel: only the merging seed exists
    try:
        network_backend = _resolve_network_backend(network_backend)
        setter(network_backend.cookies_for(
            page_url, context=network_context) or "")
        return True
    except Exception:
        return False


def sync_cookie_writes(doc, page_url, *, network_backend=None,
                       network_context=None):
    """Drain document.cookie setter strings into the scoped network jar."""
    if doc is None or page_url is None or not getattr(page_url, "host", None):
        return 0
    try:
        if hasattr(doc, "take_cookie_writes"):
            writes = list(doc.take_cookie_writes())
        elif hasattr(doc, "read_cookies"):
            # Compatibility with older installed wheels. New wheels use the
            # lossless setter queue above, including cookie attributes.
            value = doc.read_cookies()
            writes = [p.strip() for p in value.split(";") if p.strip()]
        else:
            return 0
        network_backend = _resolve_network_backend(network_backend)
        return sum(
            network_backend.set_cookie_from_js(
                page_url, value, context=network_context)
            for value in writes)
    except Exception:
        return 0


def settle_async(doc, css_sources, base_url, timeout=8.0, max_rounds=2000,
                 *, network_timeout=15.0, cancel_token=None,
                 network_backend=None, network_context=None):
    """Drive the gg-js event loop to quiescence: drain microtasks, fire
    virtual-clock timers, and service fetch() over the real network,
    looping until no work remains or the timeout. This is what makes
    fetch/setTimeout-driven SPA content materialize. Returns True if the
    DOM was mutated (caller should re-style/re-layout). No-op unless the
    async runtime is present."""
    if not async_available() or not hasattr(doc, "pump"):
        return False
    import time
    deadline = time.monotonic() + timeout
    version_before = doc.dom_version() if hasattr(doc, "dom_version") else None
    activity = False
    for _ in range(max_rounds):
        logs, fetches = pump_script_requests(doc)
        sync_cookie_writes(
            doc, base_url, network_backend=network_backend,
            network_context=network_context)
        if logs:
            for line in logs:
                print(f"[js console] {line}")
            activity = True
        if fetches:
            activity = True
            for request in fetches:
                service_script_fetch(
                    doc, base_url, request,
                    network_timeout=network_timeout,
                    cancel_token=cancel_token,
                    network_backend=network_backend,
                    network_context=network_context)
            continue  # resolving fetches queues more microtasks
        if not doc.has_pending_work():
            break
        if time.monotonic() > deadline:
            print("[js] event loop settle timed out")
            break
    sync_cookie_writes(
        doc, base_url, network_backend=network_backend,
        network_context=network_context)
    if version_before is not None:
        return doc.dom_version() != version_before
    return activity


def settle_lazy(doc, css_sources, base_url, viewport_w=1280.0,
                viewport_h=3000.0, horizon_ms=8000.0, max_steps=8000,
                timeout=25.0, eager_steps=40, layout_interval=12):
    """Step-driven settle with layout interleave — unlocks viewport-lazy
    content (Naver's shopping/stocks/widgets, batched behind one
    `/nvhaproxy/v2/pc/lazy` request).

    Plain `settle_async` fires every due timer in one `pump()`, so the host
    can never interleave layout between React's commit and its
    geometry-reading effects. Those effects call getBoundingClientRect to
    decide "is this section on screen?" — and with no layout they read 0,
    conclude "off-screen", and never load. Here we fire the event loop one
    scheduler slice at a time (`doc.step`) and refresh real layout rects
    (`set_layout_rects`) whenever the DOM changes, so the next slice's
    effects see true geometry and trigger their loads.

    Requires the `step` primitive (newer wheel); falls back to
    `settle_async` otherwise. Slower than settle_async (a layout per DOM
    mutation) — meant for full-page headless capture, not the live loop.
    Returns True if the DOM mutated."""
    if not (async_available() and hasattr(doc, "step")):
        return settle_async(doc, css_sources, base_url, timeout=timeout)
    from . import net
    from .layout import DocumentLayout
    import time

    def _push_rects():
        nodes = refresh(doc, css_sources, viewport_w)
        d = DocumentLayout(nodes)
        d.layout(viewport_w, viewport_h)
        rects, stack = [], [d]
        while stack:
            b = stack.pop()
            nd = getattr(b, "node", None)
            r = getattr(nd, "_ridx", None) if nd is not None else None
            if r is not None:
                rects.append((int(r), float(getattr(b, "x", 0)),
                              float(getattr(b, "y", 0)),
                              float(getattr(b, "width", 0)),
                              float(getattr(b, "height", 0))))
            stack.extend(getattr(b, "children", []) or [])
        doc.set_layout_rects(rects)

    deadline = time.monotonic() + timeout
    horizon = doc.now_ms() + horizon_ms
    last_ver = -1
    stable = 0
    got_lazy = False
    after_lazy = 0
    mutated = False
    last_layout = -(10 ** 9)
    for step_i in range(max_steps):
        logs, fetches, more = doc.step(horizon)
        force_layout = False
        if logs:
            for line in logs:
                print(f"[js console] {line}")
            mutated = True
        if fetches:
            mutated = True
            for fetch_id, url in fetches:
                try:
                    _h, body, _f = net.request_text(base_url.resolve(url))
                    doc.resolve_fetch(fetch_id, 200, body)
                    if "lazy" in url:
                        got_lazy = True
                        force_layout = True  # re-layout for the batch's render
                except Exception as e:
                    doc.reject_fetch(fetch_id, f"{type(e).__name__}: {e}")
            stable = 0
        ver = doc.dom_version()
        changed = ver != last_ver
        if changed:
            last_ver = ver
            stable = 0
            mutated = True
        else:
            stable += 1
        # Coalesce layouts (a full restyle+layout per mutation is ~90% of
        # the cost). React commits a little every slice, so lay out eagerly
        # while sections are first mounting + reading geometry, then throttle
        # to roughly frame granularity — same content, ~4x faster.
        interval = 1 if step_i < eager_steps else layout_interval
        if (changed and step_i - last_layout >= interval) or force_layout:
            _push_rects()
            last_layout = step_i
        # terminate once the DOM has settled and (if a lazy batch loaded)
        # its re-render has drained
        if not more and stable > 8 and (not got_lazy or after_lazy > 150):
            break
        if got_lazy:
            after_lazy += 1
        if time.monotonic() > deadline:
            print("[js] lazy settle timed out")
            break
    _push_rects()  # final geometry for the caller's paint
    return mutated


def parse_and_style(html, fetch_stylesheets):
    """Compatibility helper: parse + style without running JS."""
    root, _doc, _css, _logs = load_document(html, fetch_stylesheets)
    return root
