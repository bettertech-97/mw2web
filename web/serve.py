"""Local development server for the browser build.

Serves this folder with the cross-origin isolation headers SharedArrayBuffer
needs. With --games it also serves that folder at /games/ (byte ranges
supported) so a dev page can skip the folder picker. It binds to 127.0.0.1
only: the games folder is the developer's own install and never leaves the
machine.
"""

import argparse
import functools
import http.server
import json
import os
import re


class Handler(http.server.SimpleHTTPRequestHandler):
    games = None
    # Keep-alive: the engine issues thousands of small ranged reads.
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        if not self.path.startswith("/games/"):
            super().log_message(format, *args)

    def end_headers(self):
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        self.send_header("Cross-Origin-Resource-Policy", "same-origin")
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def translate_path(self, path):
        clean = path.split("?", 1)[0].split("#", 1)[0]
        if self.games and clean.startswith("/games/"):
            rel = http.server.urllib.parse.unquote(clean[len("/games/"):])
            full = os.path.normpath(os.path.join(self.games, rel))
            if os.path.commonpath([full, self.games]) == self.games:
                return full
            return os.path.join(self.games, "__outside__")
        return super().translate_path(path)

    def games_index(self):
        entries = []
        for dirpath, _, filenames in os.walk(self.games):
            for name in filenames:
                full = os.path.join(dirpath, name)
                stat = os.stat(full)
                rel = os.path.relpath(full, self.games).replace(os.sep, "/")
                entries.append({"path": rel, "size": stat.st_size, "modified": int(stat.st_mtime * 1000)})
        body = json.dumps(entries).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        # Dev only: the page mirrors its console here (web/dev.log).
        if self.path != "/log":
            self.send_error(404)
            return
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        log = os.path.join(os.path.dirname(os.path.abspath(__file__)), "dev.log")
        with open(log, "ab") as f:
            f.write(body.rstrip(b"\n") + b"\n")
        self.send_response(204)
        self.end_headers()

    def do_GET(self):
        if self.path.split("?", 1)[0] == "/games-index.json":
            if not self.games:
                self.send_error(404, "no --games folder")
                return
            return self.games_index()
        match = re.fullmatch(r"bytes=(\d+)-(\d*)", self.headers.get("Range", ""))
        path = self.translate_path(self.path)
        if not match or not os.path.isfile(path):
            return super().do_GET()
        size = os.path.getsize(path)
        start = int(match.group(1))
        end = min(int(match.group(2)) if match.group(2) else size - 1, size - 1)
        if start > end:
            self.send_error(416)
            return
        self.send_response(206)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.send_header("Content-Length", str(end - start + 1))
        self.end_headers()
        with open(path, "rb") as f:
            f.seek(start)
            remaining = end - start + 1
            while remaining:
                chunk = f.read(min(remaining, 1 << 20))
                if not chunk:
                    break
                self.wfile.write(chunk)
                remaining -= len(chunk)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8484)
    parser.add_argument("--games", help="games folder to serve at /games/ (dev only)")
    args = parser.parse_args()
    Handler.games = os.path.abspath(args.games) if args.games else None
    Handler.extensions_map[".wasm"] = "application/wasm"
    Handler.extensions_map[".js"] = "text/javascript"
    root = os.path.dirname(os.path.abspath(__file__))
    handler = functools.partial(Handler, directory=root)
    with http.server.ThreadingHTTPServer(("127.0.0.1", args.port), handler) as httpd:
        print(f"serving {root} on http://127.0.0.1:{args.port}/")
        httpd.serve_forever()


if __name__ == "__main__":
    main()
