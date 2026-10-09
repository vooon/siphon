"""Healthchecks stand-in for e2e/run.sh: logs every request as a JSON line.

Usage: python3 -I hc-mock.py <port> <logfile>
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlsplit

port, logfile = int(sys.argv[1]), sys.argv[2]


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        url = urlsplit(self.path)
        with open(logfile, "a") as f:
            f.write(json.dumps({
                "path": url.path,
                "rid": parse_qs(url.query).get("rid", [""])[0],
                "body": body.decode(),
            }) + "\n")
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"OK")

    def log_message(self, *args):
        pass


HTTPServer(("127.0.0.1", port), Handler).serve_forever()
