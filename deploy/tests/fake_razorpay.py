"""A tiny fake Razorpay API for deploy/tests/razorpay-keys.test.sh (stdlib only).

Accepts one test key pair over HTTP basic auth, lists plans, creates plans, and can be
told to refuse plan creation (FAIL_PLANS=1). Prints the port it listens on, then serves.
"""

import base64
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

KEY = "rzp_test_FakeKey000001:FakeSecretForTestsOnly00"
plans = []


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def authed(self):
        want = "Basic " + base64.b64encode(KEY.encode()).decode()
        if self.headers.get("Authorization") != want:
            self.reply(401, {"error": {"code": "BAD_REQUEST_ERROR", "description": "The api key provided is invalid"}})
            return False
        return True

    def do_GET(self):
        if self.authed() and self.path.startswith("/v1/plans"):
            self.reply(200, {"entity": "collection", "count": len(plans), "items": plans})

    def do_POST(self):
        if not self.authed():
            return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"{}")
        if os.environ.get("FAIL_PLANS") == "1":
            self.reply(400, {"error": {"code": "BAD_REQUEST_ERROR", "description": "refused on purpose"}})
            return
        plan = {"id": "plan_Fake%06d" % (len(plans) + 1), "entity": "plan", **body}
        plans.append(plan)
        self.reply(200, plan)


server = HTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
sys.stdout.close()
server.serve_forever()
