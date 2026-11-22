import io
import json
import threading
import unittest
from contextlib import redirect_stderr, redirect_stdout
from http.server import BaseHTTPRequestHandler, HTTPServer

import controller


class MockHandler(BaseHTTPRequestHandler):
    recorded: list[tuple[str, str, str | None, dict | None]] = []

    def do_GET(self) -> None:
        self._handle("GET")

    def do_POST(self) -> None:
        self._handle("POST")

    def _handle(self, method: str) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length).decode("utf-8") if length > 0 else ""
        body = json.loads(raw) if raw else None
        MockHandler.recorded.append((method, self.path, self.headers.get("Authorization"), body))

        if self.path == "/api/v1/obs/inputs":
            self._send(200, [{"name": "Mic", "muted": False}])
        elif self.path == "/api/v1/obs/scenes":
            self._send(200, {"current": "Main", "scenes": ["Main", "BRB"]})
        elif self.path == "/api/v1/moments/markers":
            self._send(201, {"id": "mk_1", "label": body.get("label") if body else ""})
        elif self.path == "/api/v1/accounts":
            self._send(403, {"error": {"code": "forbidden", "message": "Forbidden", "hint": None}})
        elif self.path == "/api/v1/status":
            self._send(200, {"version": "3.4.0", "connectors": {}})
        else:
            self._send(204, None)

    def _send(self, code: int, payload: object | None) -> None:
        self.send_response(code)
        if payload is not None:
            encoded = json.dumps(payload).encode("utf-8")
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
        else:
            self.end_headers()

    def log_message(self, format: str, *args: object) -> None:
        return


class ControllerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.server = HTTPServer(("127.0.0.1", 0), MockHandler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.base = f"http://127.0.0.1:{cls.server.server_port}"

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self) -> None:
        MockHandler.recorded.clear()

    def test_controller_repr_redacts_api_key(self) -> None:
        key = "fc_key_" + "ab" * 32
        ctrl = controller.FerrocastController(self.base, key)
        self.assertNotIn(key, repr(ctrl))

    def test_actions_send_bearer_header_and_expected_payloads(self) -> None:
        key = "fc_key_" + "cd" * 32
        ctrl = controller.FerrocastController(self.base, key)
        ctrl.switch_scene("BRB")
        muted = ctrl.set_mute("Mic")  # toggles False -> True
        self.assertTrue(muted)
        marker = ctrl.add_marker("Clutch")
        self.assertEqual(marker["label"], "Clutch")
        ctrl.queue_action("skip")
        ctrl.test_alert("follow")

        self.assertEqual(MockHandler.recorded[0], ("POST", "/api/v1/obs/scene", f"Bearer {key}", {"scene": "BRB"}))
        self.assertEqual(MockHandler.recorded[1][0:2], ("GET", "/api/v1/obs/inputs"))
        self.assertEqual(
            MockHandler.recorded[2],
            ("POST", "/api/v1/obs/input/mute", f"Bearer {key}", {"input": "Mic", "muted": True}),
        )
        self.assertEqual(
            MockHandler.recorded[3],
            ("POST", "/api/v1/moments/markers", f"Bearer {key}", {"label": "Clutch"}),
        )
        self.assertEqual(MockHandler.recorded[4][0:2], ("POST", "/api/v1/queue/skip"))
        self.assertEqual(
            MockHandler.recorded[5],
            ("POST", "/api/v1/test", f"Bearer {key}", {"kind": "follow", "platform": "twitch"}),
        )

    def test_403_maps_to_FerrocastApiError_and_never_leaks_key(self) -> None:
        key = "fc_key_" + "ef" * 32
        ctrl = controller.FerrocastController(self.base, key)
        with self.assertRaises(controller.FerrocastApiError) as ctx:
            ctrl.request("GET", "/accounts")
        self.assertEqual(ctx.exception.status, 403)
        self.assertNotIn(key, str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
