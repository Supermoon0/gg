# -*- coding: utf-8 -*-
"""Headless screenshot: drive the real native Shell with a fake window,
capture the presented RGB frame, and write it out as a PPM (P6) file.

Usage: python scripts/capture.py <url> <out.ppm> [width] [height] [settle_s]
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

os.environ.setdefault("GG_PROCESS_MODEL", "local")

from browser import shell as shell_mod  # noqa: E402
from browser import textengine          # noqa: E402


class FakeWindow:
    def __init__(self, w, h, scale=1.0):
        self._w, self._h, self._scale = w, h, scale
        self.last = None  # (pw, ph, buf)
        self.title = ""

    def size(self):
        return self._w, self._h

    def scale_factor(self):
        return self._scale

    def set_title(self, t):
        self.title = t

    def set_cursor_pointer(self, on):
        pass

    def present(self, pw, ph, buf):
        self.last = (pw, ph, bytes(buf))

    def pump(self, ms):
        return []


def main():
    url = sys.argv[1]
    out = sys.argv[2]
    w = int(sys.argv[3]) if len(sys.argv) > 3 else 1280
    h = int(sys.argv[4]) if len(sys.argv) > 4 else 2600
    settle_s = float(sys.argv[5]) if len(sys.argv) > 5 else 20.0

    if not textengine.available():
        raise SystemExit("native ggcore engine required")

    win = FakeWindow(w, h)
    shell = shell_mod.Shell(win, process_model="local")

    shell.load_url_string(url)
    # wait for the navigation fetch to complete
    t0 = time.monotonic()
    while time.monotonic() - t0 < 40:
        if shell.poll_navigation():
            break
        time.sleep(0.05)

    # advance the live loop so JS mounts, images load and layout settles
    deadline = time.monotonic() + settle_s
    while time.monotonic() < deadline:
        shell.tick_live()
        time.sleep(0.05)

    # one more relayout + paint at full window height, then render
    shell.relayout()
    shell.render_frame()

    pw, ph, buf = win.last
    with open(out, "wb") as f:
        f.write(b"P6\n%d %d\n255\n" % (pw, ph))
        f.write(buf)
    print(f"wrote {out} {pw}x{ph} title={win.title!r} "
          f"docH={getattr(shell, 'content_height', '?')}")


if __name__ == "__main__":
    main()
