"""Serve HTTP-01 challenges, renew the shared XMPP certificate, and forward
everything else to the hub's web page (onboarding and sign-up)."""

import functools
import http.server
import os
from pathlib import Path
import shutil
import subprocess
import threading
import time
import urllib.error
import urllib.request
import uuid

DOMAIN = os.environ.get("XMPP_DOMAIN", "agentchat.isagentproof.com")
WEBROOT = Path("/certs/webroot")
WEBROOT.mkdir(parents=True, exist_ok=True)
# The hub's HTTP port on Sliplane's private network (not published).
HUB_URL = os.environ.get("HUB_URL", "http://agentchat-xmpp.internal:5280").rstrip("/")
MAX_BODY = 64 * 1024
# File uploads (XEP-0363, under /upload/) stream through up to this size; the
# hub enforces its own, smaller limit.
MAX_UPLOAD = 110 * 1000 * 1000
REQUEST_HEADERS = ("Content-Type", "Accept", "Accept-Language", "User-Agent")
RESPONSE_HEADERS = (
    "Content-Type",
    "Cache-Control",
    "Content-Security-Policy",
    "Cross-Origin-Opener-Policy",
    "Cross-Origin-Resource-Policy",
    "Permissions-Policy",
    "Referrer-Policy",
    "X-Content-Type-Options",
    "X-Frame-Options",
    "Content-Disposition",
    "Access-Control-Allow-Origin",
    "Access-Control-Allow-Methods",
    "Access-Control-Allow-Headers",
)
UNAVAILABLE = b"agent-chat XMPP server\n\nThe web page is unavailable right now.\n"


class LimitedReader:
    """At most `remaining` bytes of a stream, for urllib to send as a body."""

    def __init__(self, stream, remaining):
        self.stream = stream
        self.remaining = remaining

    def read(self, size=-1):
        if self.remaining <= 0:
            return b""
        if size < 0 or size > self.remaining:
            size = self.remaining
        data = self.stream.read(min(size, 64 * 1024))
        self.remaining -= len(data)
        return data


class Handler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/healthz":
            self.reply(200, b"ok\n")
        elif self.path.startswith("/.well-known/acme-challenge/"):
            super().do_GET()
        else:
            self.forward()

    def do_HEAD(self):
        if self.path.startswith("/.well-known/acme-challenge/"):
            super().do_HEAD()
        else:
            self.forward()

    def do_POST(self):
        self.forward()

    def do_PUT(self):
        if self.path.startswith("/upload/"):
            self.forward()
        else:
            self.reply(405, b"method not allowed\n")

    def do_OPTIONS(self):
        if self.path.startswith("/upload/"):
            self.forward()
        else:
            self.reply(405, b"method not allowed\n")

    def reply(self, status, body, content_type="text/plain; charset=utf-8"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def client_address_for_hub(self):
        # Sliplane's HTTPS proxy appends the visitor's address; the hub limits
        # sign-ups per address, so pass on that one, not ours.
        forwarded = self.headers.get("X-Forwarded-For", "").rsplit(",", 1)[-1].strip()
        return forwarded or self.client_address[0]

    def forward(self):
        upload = self.path.startswith("/upload/")
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            return self.reply(400, b"bad request\n")
        if length < 0 or length > (MAX_UPLOAD if upload else MAX_BODY):
            return self.reply(413, b"request too large\n")
        if upload and length:
            # Stream the file through instead of holding it in memory.
            body = LimitedReader(self.rfile, length)
        else:
            body = self.rfile.read(length) if length else None
        request = urllib.request.Request(HUB_URL + self.path, data=body, method=self.command)
        if upload and length:
            request.add_header("Content-Length", str(length))
        for name in REQUEST_HEADERS:
            if self.headers.get(name):
                request.add_header(name, self.headers[name])
        request.add_header("X-Forwarded-For", self.client_address_for_hub())
        request.add_header("X-Forwarded-Proto", "https")
        try:
            response = urllib.request.urlopen(request, timeout=600 if upload else 30)
        except urllib.error.HTTPError as error:
            response = error
        except Exception as error:
            print(f"Hub web page unavailable: {error}", flush=True)
            return self.reply(502, UNAVAILABLE)
        with response:
            if upload:
                # Files stream back without being held in memory.
                self.send_response(response.status)
                for name in RESPONSE_HEADERS + ("Content-Length",):
                    value = response.headers.get(name)
                    if value:
                        self.send_header(name, value)
                self.end_headers()
                if self.command != "HEAD":
                    shutil.copyfileobj(response, self.wfile, 64 * 1024)
                return
            data = response.read()
            self.send_response(response.status)
            for name in RESPONSE_HEADERS:
                value = response.headers.get(name)
                if value:
                    self.send_header(name, value)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(data)

    def list_directory(self, path):
        self.send_error(404)
        return None


def renew():
    while True:
        # Wait for DNS and Sliplane's domain route before asking the CA.
        # Otherwise an unfinished DNS setup can exhaust ACME failure limits.
        token = uuid.uuid4().hex
        challenge = WEBROOT / ".well-known/acme-challenge" / token
        challenge.parent.mkdir(parents=True, exist_ok=True)
        challenge.write_text(token)
        try:
            with urllib.request.urlopen(
                f"http://{DOMAIN}/.well-known/acme-challenge/{token}", timeout=15
            ) as response:
                routed = response.read(256).decode() == token
        except Exception as error:
            print(f"Waiting for domain routing: {error}", flush=True)
            routed = False
        finally:
            challenge.unlink(missing_ok=True)
        if not routed:
            time.sleep(60)
            continue
        command = [
            "certbot", "certonly", "--non-interactive", "--agree-tos",
            "--register-unsafely-without-email", "--webroot", "-w", str(WEBROOT),
            "--config-dir", "/certs/letsencrypt", "--work-dir", "/certs/work",
            "--logs-dir", "/certs/logs", "--cert-name", DOMAIN,
            "--keep-until-expiring", "-d", DOMAIN,
        ]
        result = subprocess.run(command, check=False)
        if result.returncode == 0:
            destination = Path("/certs/xmpp")
            destination.mkdir(mode=0o700, exist_ok=True)
            for filename in ("privkey.pem", "fullchain.pem"):
                temporary = destination / (filename + ".new")
                shutil.copyfile(Path("/certs/letsencrypt/live") / DOMAIN / filename, temporary)
                temporary.chmod(0o600)
                temporary.replace(destination / filename)
            print("XMPP certificate is ready", flush=True)
        time.sleep(43200 if result.returncode == 0 else 3600)


server = http.server.ThreadingHTTPServer(
    ("0.0.0.0", int(os.environ.get("PORT", "8080"))),
    functools.partial(Handler, directory=str(WEBROOT)),
)
threading.Thread(target=renew, daemon=True).start()
server.serve_forever()
