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
        muted = ctrl.set_mute("Mic", True)
        self.assertTrue(muted)
        # No guessing a toggle: the API doesn't say whether an input is muted.
        with self.assertRaises(ValueError):
            ctrl.set_mute("Mic", None)  # type: ignore[arg-type]
        marker = ctrl.add_marker("Clutch")
        self.assertEqual(marker["label"], "Clutch")
        ctrl.queue_action("skip")
        ctrl.test_alert("follow")

        self.assertEqual(MockHandler.recorded[0], ("POST", "/api/v1/obs/scene", f"Bearer {key}", {"scene": "BRB"}))
        self.assertEqual(
            MockHandler.recorded[1],
            ("POST", "/api/v1/obs/input/mute", f"Bearer {key}", {"input": "Mic", "muted": True}),
        )
        self.assertEqual(
            MockHandler.recorded[2],
            ("POST", "/api/v1/moments/markers", f"Bearer {key}", {"label": "Clutch"}),
        )
        self.assertEqual(MockHandler.recorded[3][0:2], ("POST", "/api/v1/queue/skip"))
        self.assertEqual(
            MockHandler.recorded[4],
            ("POST", "/api/v1/test", f"Bearer {key}", {"kind": "follow", "platform": "twitch"}),
        )

    def test_403_maps_to_FerrocastApiError_and_never_leaks_key(self) -> None:
        key = "fc_key_" + "ef" * 32
        ctrl = controller.FerrocastController(self.base, key)
        with self.assertRaises(controller.FerrocastApiError) as ctx:
            ctrl.request("GET", "/accounts")
        self.assertEqual(ctx.exception.status, 403)
        self.assertNotIn(key, str(ctx.exception))

    def test_plain_http_is_for_this_machine_only(self) -> None:
        key = "fc_key_" + "12" * 32
        for ok in [
            "http://127.0.0.1:47820",
            "http://localhost:47820",
            "http://[::1]:47820",
            "http://127.0.0.5:1",
            "https://example.com",
            "https://192.168.1.20:47820/",
        ]:
            controller.FerrocastController(ok, key)
        for bad in [
            "http://192.168.1.20:47820",
            "http://example.com",
            "http://127.0.0.1.evil.example:47820",
            "http://localhost.evil.example",
            "ftp://127.0.0.1",
            "ws://127.0.0.1",
            "127.0.0.1:47820",
            "http://user:pw@127.0.0.1:1",
            "",
        ]:
            with self.assertRaises(ValueError, msg=bad):
                controller.FerrocastController(bad, key)
        # The error never carries the key.
        try:
            controller.FerrocastController("http://10.0.0.5", key)
        except ValueError as err:
            self.assertNotIn(key, str(err))

    def test_a_redirect_is_never_followed_with_the_key(self) -> None:
        key = "fc_key_" + "34" * 32
        seen: list[str | None] = []

        class Other(BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                seen.append(self.headers.get("Authorization"))
                self.send_response(200)
                self.end_headers()

            def log_message(self, format: str, *args: object) -> None:
                return

        other = HTTPServer(("127.0.0.1", 0), Other)
        threading.Thread(target=other.serve_forever, daemon=True).start()
        target = f"http://127.0.0.1:{other.server_port}/steal"

        class Redirecting(BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                self.send_response(302)
                self.send_header("Location", target)
                self.end_headers()

            def log_message(self, format: str, *args: object) -> None:
                return

        first = HTTPServer(("127.0.0.1", 0), Redirecting)
        threading.Thread(target=first.serve_forever, daemon=True).start()
        try:
            ctrl = controller.FerrocastController(f"http://127.0.0.1:{first.server_port}", key)
            with self.assertRaises(controller.FerrocastApiError) as ctx:
                ctrl.status()
            self.assertEqual(ctx.exception.status, 302)
            self.assertIn("redirect", str(ctx.exception).lower())
            self.assertNotIn(key, str(ctx.exception))
        finally:
            first.shutdown()
            first.server_close()
            other.shutdown()
            other.server_close()
        self.assertEqual(seen, [], "the second server was never asked, with or without the key")


if __name__ == "__main__":
    unittest.main()
