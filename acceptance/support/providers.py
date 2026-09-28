"""Exact loopback provider replies and bounded network faults using only stdlib."""

import base64
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import socket
import threading


FIXTURES = Path(__file__).resolve().parents[1] / "fixtures"


@dataclass(frozen=True)
class Reply:
    body: bytes = b""
    status: int = 200
    headers: dict = field(default_factory=dict)
    delay_seconds: float = 0
    disconnect_after: int | None = None

    def __post_init__(self):
        if not isinstance(self.body, bytes) or len(self.body) > 64 * 1024 * 1024:
            raise ValueError("reply body must be bounded bytes")
        if not 100 <= self.status <= 599 or not 0 <= self.delay_seconds <= 60:
            raise ValueError("reply status or delay outside fixture bounds")
        if self.disconnect_after is not None and not 0 <= self.disconnect_after <= len(self.body):
            raise ValueError("disconnect offset outside reply body")
        if any("\r" in str(v) or "\n" in str(v) for pair in self.headers.items() for v in pair):
            raise ValueError("invalid reply header")


def provider_body(name):
    """Load one fixed provider fixture; base64 recordings retain original wire bytes."""
    if Path(name).name != name or name in (".", ".."):
        raise ValueError("provider fixture must be an exact leaf")
    path = FIXTURES / "providers" / name
    if path.is_symlink():
        raise ValueError("provider fixture cannot be a symlink")
    data = path.read_bytes()
    return base64.b64decode(data.strip(), validate=True) if name.endswith(".base64") else data


class ProviderServer:
    """An explicit (method, path-and-query) map; unmatched calls are recorded 501s.

    No proxy fallback, wildcard route, ambient network, or argument-based success.
    Call assert_no_unmatched() after the actual workflow to detect missing coverage.
    """

    def __init__(self, routes):
        self.routes = dict(routes)
        if any(method not in ("GET", "POST", "PUT", "DELETE", "PATCH", "HEAD")
               or not path.startswith("/") or not isinstance(reply, Reply)
               for (method, path), reply in self.routes.items()):
            raise ValueError("invalid exact provider route")
        self.requests = []
        self.unmatched = []
        self.lock = threading.Lock()
        self.stopping = threading.Event()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                key = (self.command, self.path)
                with owner.lock:
                    owner.requests.append(key)
                    reply = owner.routes.get(key)
                    if reply is None:
                        owner.unmatched.append(key)
                if reply is None:
                    reply = Reply(b"Unconfigured fixture request", status=501)
                if owner.stopping.wait(reply.delay_seconds):
                    return
                try:
                    self.send_response(reply.status)
                    if not any(k.lower() == "content-length" for k in reply.headers):
                        self.send_header("Content-Length", str(len(reply.body)))
                    for key, value in reply.headers.items():
                        self.send_header(key, value)
                    self.end_headers()
                    if self.command != "HEAD":
                        self.wfile.write(reply.body if reply.disconnect_after is None else reply.body[:reply.disconnect_after])
                        self.wfile.flush()
                    if reply.disconnect_after is not None:
                        self.connection.shutdown(socket.SHUT_RDWR)
                except (BrokenPipeError, ConnectionResetError, OSError):
                    pass  # The tested client may cancel or enforce its timeout.
                self.close_connection = True

            do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = do_GET

            def log_message(self, _format, *_args):
                pass  # Never print headers, request bodies, tokens, or URLs.

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    def __enter__(self):
        self.thread.start()
        return self

    def assert_no_unmatched(self):
        with self.lock:
            count = len(self.unmatched)
        if count:
            raise AssertionError(f"{count} unconfigured provider request(s)")

    def __exit__(self, *_exc):
        self.stopping.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)
        if self.thread.is_alive():
            raise AssertionError("provider fixture did not stop")
