// The boot chain a real app shell runs: the entry bundle injects its
// vendor and app bundles as <script src> with onload chaining. This
// is exactly the path naver's webpack runtime takes after preload —
// three injections deep, each dependent on the previous one's load.
(function () {
  function load(src, cb) {
    var s = document.createElement('script');
    s.src = src;
    s.onload = cb;
    s.onerror = function () {
      console.log('FIXTURE-ERR ' + src);
    };
    document.head.appendChild(s);
  }
  if (!window.__polyfillOk) {
    console.log('FIXTURE-ERR polyfill gate failed');
    return;
  }
  load('../js/react/react.production.min.js', function () {
    load('../js/react/react-dom.production.min.js', function () {
      load('app.js', function () {
        console.log('FIXTURE-CHAIN-DONE');
      });
    });
  });
})();
