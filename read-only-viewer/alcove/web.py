"""HTTP layer: auth, static assets, and the JSON API.

Read-only by construction. The only POST is /login, and all it does is set a
cookie.
"""

from __future__ import annotations

import hmac
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import parse_qs

from . import config
from .collect import cached, public

_TYPES = {".html": "text/html; charset=utf-8", ".css": "text/css; charset=utf-8",
          ".js": "text/javascript; charset=utf-8"}


def token_ok(supplied: str) -> bool:
    """Constant-time compare so a wrong guess leaks no timing signal."""
    return bool(config.TOKEN) and hmac.compare_digest(supplied, config.TOKEN)


def _asset(name: str) -> bytes:
    # Resolve inside the static dir only: a request path must never escape it.
    target = (config.STATIC_DIR / name).resolve()
    if not str(target).startswith(str(config.STATIC_DIR.resolve())):
        raise FileNotFoundError(name)
    return target.read_bytes()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "alcove"

    def _send(self, body: bytes, ctype: str, status: int = 200,
              cookie: str | None = None) -> None:
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        # Nothing here loads a remote resource; say so rather than relying on it.
        self.send_header("Content-Security-Policy",
                         "default-src 'none'; style-src 'self'; script-src 'self'; "
                         "connect-src 'self'")
        if cookie:
            # HttpOnly so page scripts cannot read it back out; SameSite=Strict
            # so another origin cannot ride the cookie. No Secure flag: the
            # overlay is plain HTTP.
            self.send_header(
                "Set-Cookie",
                f"{config.COOKIE}={cookie}; Path=/; HttpOnly; SameSite=Strict; "
                "Max-Age=604800")
        self.end_headers()
        self.wfile.write(body)

    def _supplied_token(self) -> str:
        """Bearer header for scripts, cookie for browsers.

        Deliberately no `?token=` support: a secret in a URL lands in browser
        history, screenshots, referers, and shell history.
        """
        auth = self.headers.get("Authorization", "")
        if auth.startswith("Bearer "):
            return auth[len("Bearer "):].strip()
        for part in self.headers.get("Cookie", "").split(";"):
            name, _, value = part.strip().partition("=")
            if name == config.COOKIE:
                return value
        return ""

    def _login_page(self, error: str = "", status: int = 200) -> None:
        body = _asset("login.html").decode("utf-8").replace(
            "__ERR__", f'<p class="err">{error}</p>' if error else "")
        self._send(body.encode(), _TYPES[".html"], status=status)

    def do_POST(self) -> None:  # noqa: N802
        if self.path.split("?", 1)[0] != "/login":
            self.send_error(404)
            return
        length = min(int(self.headers.get("Content-Length") or 0), 4096)
        raw = self.rfile.read(length).decode("utf-8", errors="replace") if length else ""
        supplied = (parse_qs(raw).get("token") or [""])[0]
        if not token_ok(supplied):
            self._login_page("rejected", status=401)
            return
        # 303 so the browser re-requests with GET and the POST body is not
        # replayed on refresh.
        self.send_response(303)
        self.send_header("Location", "/")
        self.send_header(
            "Set-Cookie",
            f"{config.COOKIE}={supplied}; Path=/; HttpOnly; SameSite=Strict; "
            "Max-Age=604800")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self) -> None:  # noqa: N802
        route = self.path.split("?", 1)[0]
        # Loopback is trusted; anything wider requires the shared secret.
        if not config.is_local_bind() and not token_ok(self._supplied_token()):
            if route == "/api/sessions":
                self._send(b'{"error":"unauthorized"}', "application/json", status=401)
            else:
                self._login_page(status=401)
            return

        try:
            if route in ("/", "/index.html", "/login"):
                self._send(_asset("index.html"), _TYPES[".html"])
            elif route == "/api/sessions":
                self._send(json.dumps(public(cached())).encode(),
                           "application/json")
            elif route.startswith("/static/"):
                name = route[len("/static/"):]
                suffix = name[name.rfind("."):] if "." in name else ""
                if suffix not in _TYPES:
                    self.send_error(404)
                    return
                self._send(_asset(name), _TYPES[suffix])
            else:
                self.send_error(404)
        except (FileNotFoundError, OSError):
            self.send_error(404)

    def log_message(self, *args: Any) -> None:
        return  # a poll every 3s would drown the console


def serve() -> int:
    if not config.CLAUDE_ROOT.is_dir() and not config.CODEX_ROOT.is_dir():
        print(f"no transcripts found under {config.CLAUDE_ROOT} "
              f"or {config.CODEX_ROOT}")
        return 1
    # Fail closed: this page shows task prompts, so a non-loopback bind without
    # a token is a mistake, not a default worth honouring.
    if not config.is_local_bind() and not config.TOKEN:
        print(f"refusing to serve {config.BIND}:{config.PORT} without "
              f"ALCOVE_TOKEN.\n"
              "  generate one:  python3 -c 'import secrets;"
              "print(secrets.token_urlsafe(32))'\n"
              f"  then:          ALCOVE_TOKEN=<token> ALCOVE_BIND={config.BIND} "
              "python3 alcove.py\n"
              "  or bind loopback: ALCOVE_BIND=127.0.0.1 python3 alcove.py")
        return 2
    print(f"alcove: http://{config.BIND}:{config.PORT}")
    print(f"  claude: {config.CLAUDE_ROOT if config.CLAUDE_ROOT.is_dir() else '(absent)'}")
    print(f"  codex:  {config.CODEX_ROOT if config.CODEX_ROOT.is_dir() else '(absent)'}")
    print(f"  auth:   {'token required' if not config.is_local_bind() else 'loopback (none)'}")
    ThreadingHTTPServer((config.BIND, config.PORT), Handler).serve_forever()
    return 0
