#!/usr/bin/env python3
"""Serve the built documentation site.

    python3 serve.py                 # build, then serve on http://127.0.0.1:8000
    python3 serve.py --port 9000
    python3 serve.py --watch         # rebuild whenever content/ or assets/ change
    python3 serve.py --no-build      # serve site/ exactly as it is

Standard library only. It serves `site/` and nothing above it.
"""

from __future__ import annotations

import argparse
import functools
import http.server
import socketserver
import sys
import threading
import time
from pathlib import Path

import build as builder

ROOT = Path(__file__).resolve().parent
OUT = ROOT / "site"
WATCHED = (ROOT / "content", ROOT / "assets", ROOT / "build.py")


class Handler(http.server.SimpleHTTPRequestHandler):
    """A static handler with `/` → `index.html`, no caching, and quiet logs."""

    extensions_map = {
        **http.server.SimpleHTTPRequestHandler.extensions_map,
        ".json": "application/json",
        ".svg": "image/svg+xml",
        ".js": "text/javascript",
    }

    def end_headers(self) -> None:
        # A docs preview that caches is a docs preview that lies after an edit.
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def log_message(self, format: str, *args) -> None:  # noqa: A002 - stdlib signature
        if self.path.endswith((".css", ".js", ".svg", ".json")):
            return
        print(f"  {self.command} {self.path}")


def fingerprint() -> tuple:
    """Every watched file's mtime — cheap enough to poll, exact enough to trust."""
    stamps = []
    for target in WATCHED:
        if target.is_file():
            stamps.append((str(target), target.stat().st_mtime_ns))
        elif target.is_dir():
            for path in sorted(target.rglob("*")):
                if path.is_file():
                    stamps.append((str(path), path.stat().st_mtime_ns))
    return tuple(stamps)


def watch() -> None:
    previous = fingerprint()
    while True:
        time.sleep(0.6)
        current = fingerprint()
        if current != previous:
            previous = current
            print("  change detected — rebuilding")
            try:
                builder.build()
            except Exception as error:  # a bad edit must not kill the server
                print(f"  build failed: {error}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--host", default="127.0.0.1", help="use 0.0.0.0 to reach it from another machine")
    parser.add_argument("--no-build", action="store_true", help="serve site/ without rebuilding first")
    parser.add_argument("--watch", action="store_true", help="rebuild on every change under content/ or assets/")
    args = parser.parse_args()

    # Line-buffered, because a preview server whose output only appears when it exits
    # is a preview server that looks hung.
    sys.stdout.reconfigure(line_buffering=True)

    if not args.no_build:
        if builder.build() != 0:
            return 1
    if not OUT.exists():
        print("nothing to serve: run `python3 build.py` first")
        return 1

    if args.watch:
        threading.Thread(target=watch, daemon=True).start()

    handler = functools.partial(Handler, directory=str(OUT))
    socketserver.ThreadingTCPServer.allow_reuse_address = True
    with socketserver.ThreadingTCPServer((args.host, args.port), handler) as server:
        print(f"\n  Fjord docs → http://{args.host}:{args.port}/  (Ctrl-C to stop)\n")
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            print("\n  stopped")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
