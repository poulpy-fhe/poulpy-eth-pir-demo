#!/usr/bin/env python3
"""Local-only static portal plus /v1 reverse proxy for the demo."""

from __future__ import annotations

import argparse
import functools
import http.client
import http.server
import socketserver
from pathlib import Path
from urllib.parse import urlsplit


HOP_BY_HOP = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}


def parse_addr(value: str) -> tuple[str, int]:
    host, _, port = value.rpartition(":")
    if not host or not port:
        raise argparse.ArgumentTypeError("expected HOST:PORT")
    return host, int(port)


class Portal(http.server.SimpleHTTPRequestHandler):
    backend: str

    def do_GET(self) -> None:
        if self.path.startswith("/v1/"):
            self.proxy()
            return
        super().do_GET()

    def do_POST(self) -> None:
        if self.path.startswith("/v1/"):
            self.proxy()
            return
        self.send_error(404)

    def proxy(self) -> None:
        target = urlsplit(self.backend)
        body = self.read_body()
        connection = (
            http.client.HTTPSConnection
            if target.scheme == "https"
            else http.client.HTTPConnection
        )
        conn = connection(target.hostname, target.port, timeout=300)
        try:
            conn.request(self.command, self.path, body=body, headers=self.forward_headers())
            response = conn.getresponse()
            self.send_response(response.status, response.reason)
            for key, value in response.getheaders():
                if key.lower() not in HOP_BY_HOP:
                    self.send_header(key, value)
            self.end_headers()
            self.wfile.write(response.read())
        finally:
            conn.close()

    def read_body(self) -> bytes | None:
        length = int(self.headers.get("content-length", "0"))
        if length == 0:
            return None
        return self.rfile.read(length)

    def forward_headers(self) -> dict[str, str]:
        headers = {}
        for key in ("content-type", "accept"):
            value = self.headers.get(key)
            if value:
                headers[key] = value
        return headers


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen", type=parse_addr, default=("127.0.0.1", 8080))
    parser.add_argument(
        "--backend",
        default="http://127.0.0.1:8787",
        help="PIR API origin (http:// or https://; the port is optional)",
    )
    parser.add_argument("--web", type=Path, default=Path("client/web"))
    args = parser.parse_args()

    try:
        backend = urlsplit(args.backend)
        backend.port
    except ValueError as error:
        raise SystemExit(f"invalid --backend URL: {error}") from error
    if backend.scheme not in {"http", "https"} or backend.hostname is None:
        raise SystemExit("--backend must be an http:// or https:// URL")

    Portal.backend = args.backend.rstrip("/")
    handler = functools.partial(Portal, directory=str(args.web))
    with socketserver.ThreadingTCPServer(args.listen, handler) as server:
        server.daemon_threads = True
        print(f"local portal listening on http://{args.listen[0]}:{args.listen[1]}")
        server.serve_forever()


if __name__ == "__main__":
    main()
