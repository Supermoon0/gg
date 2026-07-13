"""GG Browser entry point.

Usage:
    python main.py                     # tkinter shell, home page
    python main.py <url>               # tkinter shell, given URL
    python main.py --native [<url>]    # Rust (winit) window shell
"""

import sys

# Crisp text on high-DPI Windows displays
try:
    import ctypes
    ctypes.windll.shcore.SetProcessDpiAwareness(1)
except Exception:
    pass


def main():
    args = [a for a in sys.argv[1:] if a != "--native"]
    url = args[0] if args else None
    if "--native" in sys.argv[1:]:
        from browser.shell import run
        run(url)
    else:
        from browser.browser import Browser
        Browser().start(url)


if __name__ == "__main__":
    main()
