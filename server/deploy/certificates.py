"""Serve HTTP-01 challenges and renew the shared XMPP certificate."""

import functools
import http.server
import os
from pathlib import Path
import shutil
import subprocess
import threading
import time
import urllib.request
import uuid

DOMAIN = os.environ.get("XMPP_DOMAIN", "agentchat.isagentproof.com")
WEBROOT = Path("/certs/webroot")
WEBROOT.mkdir(parents=True, exist_ok=True)


class Handler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path in ("/", "/healthz"):
            body = b'agent-chat XMPP server\n'
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif self.path.startswith("/.well-known/acme-challenge/"):
            super().do_GET()
        else:
            self.send_error(404)

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
