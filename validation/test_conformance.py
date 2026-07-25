import json
from pathlib import Path
import tempfile
import unittest

from validation.conformance import (
    Result,
    _read_selection,
    compare_baseline,
    parse_test262_metadata,
    run_test262,
    run_wpt,
    write_baseline,
)


class TestConformanceUtilities(unittest.TestCase):
    def test_test262_inline_metadata(self):
        metadata = parse_test262_metadata("""/*---
flags: [onlyStrict, async]
includes: [compareArray.js, propertyHelper.js]
features: [Proxy]
negative:
  phase: runtime
  type: TypeError
---*/""")
        self.assertEqual(metadata["flags"], ["onlyStrict", "async"])
        self.assertEqual(metadata["includes"],
                         ["compareArray.js", "propertyHelper.js"])
        self.assertEqual(metadata["features"], ["Proxy"])
        self.assertEqual(metadata["negative"],
                         {"phase": "runtime", "type": "TypeError"})

    def test_test262_block_lists(self):
        metadata = parse_test262_metadata("""/*---
flags:
  - noStrict
includes:
  - assert.js
features:
  - Symbol
---*/""")
        self.assertEqual(metadata["flags"], ["noStrict"])
        self.assertEqual(metadata["includes"], ["assert.js"])
        self.assertEqual(metadata["features"], ["Symbol"])

    def test_baseline_only_gates_previous_passes(self):
        with tempfile.TemporaryDirectory() as temp:
            baseline = Path(temp) / "baseline.json"
            write_baseline([
                Result("kept", "x", "pass"),
                Result("known-gap", "x", "fail"),
            ], baseline)
            regressions, improvements = compare_baseline([
                Result("kept", "x", "fail"),
                Result("known-gap", "x", "pass"),
            ], baseline)
            self.assertEqual(regressions[0]["id"], "kept")
            self.assertEqual(improvements[0]["id"], "known-gap")
            self.assertEqual(json.loads(baseline.read_text())["schema_version"], 1)

    def test_capped_selection_is_round_robin(self):
        with tempfile.TemporaryDirectory() as temp:
            # resolve() so Windows 8.3 short paths (RUNNER~1) from
            # tempfile match the long-form paths glob returns —
            # relative_to() raises across the two spellings
            root = Path(temp).resolve()
            for folder in ("a", "b"):
                (root / folder).mkdir()
                for number in range(3):
                    (root / folder / f"{number}.js").write_text("", encoding="utf-8")
            selection = root / "selection.txt"
            selection.write_text("a/*.js\nb/*.js\n", encoding="utf-8")
            paths = _read_selection(root, selection, max_tests=3)
            self.assertEqual(
                [path.relative_to(root).as_posix() for path in paths],
                ["a/0.js", "b/0.js", "a/1.js"],
            )

    def test_test262_adapter_can_pass_a_static_test(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "harness").mkdir()
            (root / "test").mkdir()
            (root / "harness" / "assert.js").write_text(
                "var assert={sameValue:function(a,b){"
                "if(a!==b)throw new Error('not same');}};", encoding="utf-8")
            (root / "harness" / "sta.js").write_text(
                "var Test262Error=Error;", encoding="utf-8")
            (root / "test" / "sum.js").write_text(
                "/*---\nflags: [noStrict]\nfeatures: [arithmetic]\n---*/\n"
                "assert.sameValue(1+2,3);", encoding="utf-8")
            selection = root / "selection.txt"
            selection.write_text("test/sum.js\n", encoding="utf-8")
            results = run_test262(root, selection, timeout=5)
            self.assertEqual(results[0].status, "pass")

    def test_wpt_adapter_reporter_can_pass_a_static_test(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "resources").mkdir()
            (root / "dom").mkdir()
            (root / "resources" / "testharness.js").write_text("""
var __results=[]; var __done=[];
function add_result_callback(fn){__results.push(fn);}
function add_completion_callback(fn){__done.push(fn);}
function test(fn,name){var t={name:name,status:0,message:''};
try{fn();}catch(e){t.status=1;t.message=String(e);}
for(var i=0;i<__results.length;i++){__results[i](t);}}
window.addEventListener('load',function(){
for(var i=0;i<__done.length;i++){__done[i]([],{status:0,message:''});}});
""", encoding="utf-8")
            (root / "resources" / "testharnessreport.js").write_text(
                "throw new Error('standalone reporter should be removed');",
                encoding="utf-8")
            (root / "dom" / "simple.html").write_text("""
<!doctype html><script src="/resources/testharness.js"></script>
<script src="/resources/testharnessreport.js"></script>
<script>test(function(){if(1+1!==2)throw new Error('bad');},'math');</script>
""", encoding="utf-8")
            selection = root / "selection.txt"
            selection.write_text("dom/simple.html\n", encoding="utf-8")
            results = run_wpt(root, selection, timeout=5)
            self.assertEqual(results[0].status, "pass", results[0].message)
            self.assertEqual(results[0].details["subtests"]["total"], 1)


if __name__ == "__main__":
    unittest.main()
