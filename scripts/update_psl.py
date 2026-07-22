"""Refresh the vendored Public Suffix List from its canonical endpoint."""

from pathlib import Path
import os
import sys
import urllib.request


PSL_URL = "https://publicsuffix.org/list/public_suffix_list.dat"
ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "browser" / "data" / "public_suffix_list.dat"


def main():
    request = urllib.request.Request(
        PSL_URL, headers={"User-Agent": "GGBrowser-PSL-Updater/1.0"})
    with urllib.request.urlopen(request, timeout=30) as response:
        raw = response.read()
    text = raw.decode("utf-8")
    required = (
        "// ===BEGIN ICANN DOMAINS===",
        "// ===END ICANN DOMAINS===",
        "// ===BEGIN PRIVATE DOMAINS===",
        "// ===END PRIVATE DOMAINS===",
    )
    if not all(marker in text for marker in required):
        raise RuntimeError("download did not look like the official PSL")
    if len(raw) < 100_000:
        raise RuntimeError(f"PSL download was unexpectedly small: {len(raw)}")

    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    temporary = OUTPUT.with_suffix(".tmp")
    with temporary.open("w", encoding="utf-8", newline="\n") as output:
        output.write(text.replace("\r\n", "\n"))
    os.replace(temporary, OUTPUT)
    version = next((line for line in text.splitlines()
                    if line.startswith("// VERSION:")), "// VERSION: ?")
    print(f"updated {OUTPUT} ({len(raw)} bytes, {version[3:]})")


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        print(f"PSL update failed: {exc}", file=sys.stderr)
        raise SystemExit(1)
