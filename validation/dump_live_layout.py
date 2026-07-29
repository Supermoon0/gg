"""Dump live GG layout boxes in a vertical slice for compatibility audits."""

import os
import re
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
os.environ.setdefault("GG_PROCESS_MODEL", "local")

from browser import shell as shell_mod  # noqa: E402
from scripts.capture import FakeWindow  # noqa: E402


def main():
    url = sys.argv[1]
    y0 = float(sys.argv[2]) if len(sys.argv) > 2 else 0.0
    y1 = float(sys.argv[3]) if len(sys.argv) > 3 else 720.0
    settle = float(sys.argv[4]) if len(sys.argv) > 4 else 8.0
    needle = sys.argv[5] if len(sys.argv) > 5 else ""
    shell = shell_mod.Shell(FakeWindow(1280, 720), process_model="local")
    try:
        shell.load_url_string(url)
        deadline = time.monotonic() + 40.0
        while time.monotonic() < deadline and not shell.poll_navigation():
            time.sleep(0.05)
        deadline = time.monotonic() + settle
        while time.monotonic() < deadline:
            shell.tick_live()
            time.sleep(0.05)
        shell.relayout()
        seen = set()
        for box in shell.layout_list:
            node = getattr(box, "node", None)
            tag = getattr(node, "tag", None)
            if not tag or not hasattr(box, "x") or not hasattr(box, "y"):
                continue
            key = (id(node), type(box).__name__)
            if key in seen or box.y > y1 or box.y + box.height < y0:
                continue
            seen.add(key)
            attrs = getattr(node, "attributes", {})
            cls = attrs.get("class", "")
            ident = attrs.get("id", "")
            if needle and not any(
                    part in f"{tag} {ident} {cls}"
                    for part in needle.split(",")):
                continue
            style = getattr(node, "style", {})
            print(
                f"{type(box).__name__:22} {tag:10} "
                f"x={box.x:7.1f} y={box.y:7.1f} "
                f"w={box.width:7.1f} h={box.height:7.1f} "
                f"display={style.get('display', ''):10} "
                f"id={ident!r} class={cls[:90]!r} "
                f"css-width={style.get('width', '')!r} "
                f"css-height={style.get('height', '')!r} "
                f"flex={style.get('flex', '')!r} "
                f"align-self={style.get('align-self', '')!r} "
                f"padding={style.get('padding', '')!r} "
                f"margin={style.get('margin', '')!r} "
                f"edges={{pt:{style.get('padding-top', '')!r},"
                f"pr:{style.get('padding-right', '')!r},"
                f"pb:{style.get('padding-bottom', '')!r},"
                f"pl:{style.get('padding-left', '')!r},"
                f"bt:{style.get('border-top-width', '')!r},"
                f"br:{style.get('border-right-width', '')!r},"
                f"bb:{style.get('border-bottom-width', '')!r},"
                f"bl:{style.get('border-left-width', '')!r}}} "
                f"border-style={style.get('border-style', '')!r} "
                f"background={style.get('background-color', '')!r} "
                f"appearance={style.get('appearance', '')!r}"
            )
        if needle:
            parts = needle.split(",")
            printed = set()
            for source in shell._css_sources:
                for match in re.finditer(r"([^{}]+)\{([^{}]*)\}", source):
                    selector, declarations = match.groups()
                    if not any(part in selector for part in parts):
                        continue
                    rule = (selector.strip(), declarations.strip())
                    if rule in printed:
                        continue
                    printed.add(rule)
                    print(f"[CSS] {rule[0][:260]} {{{rule[1][:700]}}}")
    finally:
        shell.renderer.close()


if __name__ == "__main__":
    main()
