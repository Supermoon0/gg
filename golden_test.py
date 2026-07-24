"""Golden rendering regression suite.

Renders self-contained fixture pages through the production path
(native Rust styling -> Python layout -> paint -> Rust raster) and
compares against checked-in goldens:

  - tests/golden/lists/<name>.txt   serialized display list (diffable)
  - tests/golden/golden.json        raster sha256 + page height

Run:    python golden_test.py            compare against goldens
        python golden_test.py --update   regenerate goldens (after an
                                         INTENDED rendering change —
                                         review the list diff first!)

Goldens are pinned to the Linux fallback fonts (DejaVu/WenQuanYi); on
other platforms the suite skips rather than mis-fails. Font ids in
the serialized lists are remapped to first-appearance order so adding
a fixture never shifts another fixture's golden.
"""

import difflib
import hashlib
import json
import os
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, ROOT)

from browser import native, textengine  # noqa: E402
from browser.layout import DocumentLayout, paint_tree  # noqa: E402

FIXTURES = os.path.join(ROOT, "tests", "golden", "fixtures")
LISTS = os.path.join(ROOT, "tests", "golden", "lists")
GOLDEN_JSON = os.path.join(ROOT, "tests", "golden", "golden.json")
FAILURES = os.path.join(ROOT, "tests", "golden", "failures")

VIEWPORT_W = 1000
VIEWPORT_H = 800
LINUX_FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"


def environment_ok():
    if not sys.platform.startswith("linux"):
        return False, "goldens are pinned to Linux fonts"
    if not os.path.exists(LINUX_FONT):
        return False, "DejaVu fonts not installed"
    if not native.available() or not textengine.available():
        return False, "native ggcore/textengine not available"
    return True, ""


def serialize(cmds):
    """Stable text form of a native command list. Font ids remap to
    first-appearance order so goldens don't depend on which fixtures
    rendered earlier in the process."""
    font_map = {}
    out = []
    for (kind, x1, y1, x2, y2, rgb, aux, font, text) in cmds:
        if font not in font_map:
            font_map[font] = len(font_map)
        out.append(
            f"{kind} {x1:.2f} {y1:.2f} {x2:.2f} {y2:.2f} "
            f"rgb{rgb} {aux:.2f} f{font_map[font]} {text!r}")
    return out


def render(html):
    nodes = native.parse_and_style(html, lambda hrefs: {})
    textengine.load_svgs(nodes)
    doc = DocumentLayout(nodes)
    doc.layout(VIEWPORT_W, VIEWPORT_H)
    cmds = [c.native(0) for c in paint_tree(doc, [])]
    raster = textengine.engine().render_raw(
        VIEWPORT_W, VIEWPORT_H, (255, 255, 255), cmds)
    return (serialize(cmds), hashlib.sha256(raster).hexdigest(),
            round(doc.height, 2), raster)


def main():
    ok, why = environment_ok()
    if not ok:
        print(f"[SKIP] golden suite: {why}")
        return 0
    update = "--update" in sys.argv
    names = sorted(f for f in os.listdir(FIXTURES)
                   if f.endswith(".html"))
    if not names:
        print("no fixtures found")
        return 1
    goldens = {}
    if os.path.exists(GOLDEN_JSON) and not update:
        with open(GOLDEN_JSON) as f:
            goldens = json.load(f)
    os.makedirs(LISTS, exist_ok=True)
    failed = []
    new_goldens = {}
    for name in names:
        with open(os.path.join(FIXTURES, name)) as f:
            html = f.read()
        lines, rhash, height, raster = render(html)
        # in-process determinism gate: the same input must render
        # identically twice before it may judge anything
        lines2, rhash2, _, _ = render(html)
        if lines != lines2 or rhash != rhash2:
            print(f"[FAIL] {name}: nondeterministic render")
            failed.append(name)
            continue
        key = name[:-5]
        list_path = os.path.join(LISTS, key + ".txt")
        if update:
            with open(list_path, "w") as f:
                f.write("\n".join(lines) + "\n")
            new_goldens[key] = {"raster": rhash, "height": height,
                                "cmds": len(lines)}
            print(f"[GOLD] {name}: {len(lines)} cmds, h={height}")
            continue
        want = goldens.get(key)
        if want is None or not os.path.exists(list_path):
            print(f"[FAIL] {name}: no golden (run --update)")
            failed.append(name)
            continue
        with open(list_path) as f:
            want_lines = f.read().splitlines()
        if lines != want_lines:
            diff = list(difflib.unified_diff(
                want_lines, lines, "golden", "current", lineterm=""))
            print(f"[FAIL] {name}: display list changed "
                  f"({len(want_lines)} -> {len(lines)} cmds)")
            for d in diff[:30]:
                print("   " + d)
            if len(diff) > 30:
                print(f"   ... {len(diff) - 30} more diff lines")
            failed.append(name)
        elif rhash != want["raster"]:
            print(f"[FAIL] {name}: raster changed "
                  "(same display list — rasterizer/font drift)")
            failed.append(name)
        elif abs(height - want["height"]) > 0.01:
            print(f"[FAIL] {name}: page height "
                  f"{want['height']} -> {height}")
            failed.append(name)
        else:
            print(f"[PASS] {name} ({len(lines)} cmds)")
        if failed and failed[-1] == name:
            os.makedirs(FAILURES, exist_ok=True)
            ppm = os.path.join(FAILURES, key + ".ppm")
            with open(ppm, "wb") as f:
                f.write(b"P6\n%d %d\n255\n"
                        % (VIEWPORT_W, VIEWPORT_H) + bytes(raster))
            print(f"   raster dumped: {ppm}")
    if update:
        with open(GOLDEN_JSON, "w") as f:
            json.dump(new_goldens, f, indent=1, sort_keys=True)
        print(f"\n{len(new_goldens)} goldens written")
        return 0
    print(f"\n{len(names) - len(failed)}/{len(names)} fixtures match")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
