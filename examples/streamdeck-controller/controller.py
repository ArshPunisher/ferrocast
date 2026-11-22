#!/usr/bin/env python3
"""Zero-dependency Stream Deck style CLI/interactive controller for Ferrocast."""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any, Mapping

DEFAULT_URL = "http://127.0.0.1:47820"


class FerrocastApiError(RuntimeError):
    """Structured HTTP or transport error raised by the Ferrocast API client."""

    def __init__(self, status: int, code: str, message: str, hint: str | None = None) -> None:
        super().__init__(message)
        self.status = status
        self.code = code
        self.hint = hint


@dataclass
class FerrocastController:
    """Thin Stream Deck controller client authenticating with a scoped API key."""

    base_url: str
    api_key: str

    def __post_init__(self) -> None:
        self.base_url = self.base_url.rstrip("/")
        if not self.api_key.startswith("fc_key_"):
            raise ValueError("API key must start with 'fc_key_'.")

    def __repr__(self) -> str:
        return f"FerrocastController(base_url={self.base_url!r}, api_key='[redacted]')"

    def request(self, method: str, rel_path: str, payload: Any | None = None) -> tuple[int, Any]:
        url = f"{self.base_url}/api/v1{rel_path}"
        headers = {
            "Authorization": f"Bearer {self.api_key}",
            "Accept": "application/json",
        }
        data: bytes | None = None
        if payload is not None:
            data = json.dumps(payload).encode("utf-8")
            headers["Content-Type"] = "application/json"

        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                raw = resp.read().decode("utf-8")
                return resp.status, (json.loads(raw) if raw else None)
        except urllib.error.HTTPError as err:
            raw = err.read().decode("utf-8", errors="replace")
            code = "http_error"
            msg = f"{method} {rel_path} failed with HTTP {err.code}"
            hint = None
            try:
                parsed = json.loads(raw)
                if isinstance(parsed, dict) and isinstance(parsed.get("error"), dict):
                    code = str(parsed["error"].get("code") or code)
                    msg = str(parsed["error"].get("message") or msg)
                    hint = parsed["error"].get("hint")
            except json.JSONDecodeError:
                pass
            if err.code == 429:
                retry = err.headers.get("Retry-After")
                if retry:
                    hint = f"Rate limit reached. Wait {retry}s before retrying."
            raise FerrocastApiError(err.code, code, msg, hint) from None
        except urllib.error.URLError as err:
            raise FerrocastApiError(0, "offline", f"Could not reach Ferrocast at {self.base_url}: {err.reason}") from None

    def status(self) -> Any:
        return self.request("GET", "/status")[1]

    def scenes(self) -> Any:
        return self.request("GET", "/obs/scenes")[1]

    def switch_scene(self, scene: str) -> None:
        self.request("POST", "/obs/scene", {"scene": scene})

    def set_mute(self, input_name: str, muted: bool | None = None) -> bool:
        if muted is None:
            _, inputs = self.request("GET", "/obs/inputs")
            current = next((row for row in inputs if row.get("name") == input_name), None)
            if current is None:
                raise FerrocastApiError(404, "not_found", f"OBS input {input_name!r} was not found.")
            muted = not bool(current.get("muted"))
        self.request("POST", "/obs/input/mute", {"input": input_name, "muted": muted})
        return muted

    def add_marker(self, label: str) -> Any:
        return self.request("POST", "/moments/markers", {"label": label})[1]

    def save_replay(self) -> None:
        self.request("POST", "/obs/replay/save", {})

    def queue_action(self, action: str) -> Any:
        if action not in {"pause", "resume", "skip", "mute", "unmute", "clear"}:
            raise ValueError(f"Unsupported queue action: {action}")
        return self.request("POST", f"/queue/{action}", {})[1]

    def test_alert(self, kind: str = "follow", platform: str = "twitch") -> Any:
        return self.request("POST", "/test", {"kind": kind, "platform": platform})[1]


def load_config(environ: Mapping[str, str] = os.environ) -> FerrocastController:
    api_key = environ.get("FERROCAST_API_KEY", "").strip()
    if not api_key:
        raise ValueError("Set FERROCAST_API_KEY to a local Ferrocast API key (fc_key_...).")
    base_url = environ.get("FERROCAST_URL", DEFAULT_URL).strip() or DEFAULT_URL
    return FerrocastController(base_url=base_url, api_key=api_key)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Ferrocast Stream Deck controller")
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("status", help="Show Ferrocast status and connectors")
    sub.add_parser("scenes", help="List OBS scenes")
    p_scene = sub.add_parser("scene", help="Switch active OBS scene")
    p_scene.add_argument("name", help="Target OBS scene name")

    p_mute = sub.add_parser("mute", help="Set or toggle OBS audio input mute")
    p_mute.add_argument("input", help="Input name, e.g. Mic")
    grp = p_mute.add_mutually_exclusive_group()
    grp.add_argument("--on", action="store_true", help="Mute the input")
    grp.add_argument("--off", action="store_true", help="Unmute the input")

    p_marker = sub.add_parser("marker", help="Drop a manual stream marker")
    p_marker.add_argument("label", nargs="?", default="Stream Deck Marker")

    sub.add_parser("replay", help="Save OBS replay buffer clip")

    p_alerts = sub.add_parser("alerts", help="Alert queue buttons")
    p_alerts.add_argument("action", choices=["pause", "resume", "skip", "mute", "unmute", "clear", "test"])
    p_alerts.add_argument("kind", nargs="?", default="follow", help="Test alert kind when action=test")

    sub.add_parser("deck", help="Interactive numbered keypad loop")
    return parser


def interactive_deck(ctrl: FerrocastController) -> None:
    print("Ferrocast Keypad Deck: [1] Scenes  [2] Skip alert  [3] Pause alerts  [4] Resume alerts  [5] Marker  [q] Quit")
    while True:
        try:
            choice = input("deck> ").strip()
        except EOFError:
            break
        if choice in {"q", "quit", "exit"}:
            break
        try:
            if choice == "1":
                print(json.dumps(ctrl.scenes(), indent=2))
            elif choice.startswith("1 "):
                scene = choice[2:].strip()
                ctrl.switch_scene(scene)
                print(f"Switched scene -> {scene}")
            elif choice == "2":
                ctrl.queue_action("skip")
                print("Skipped current alert.")
            elif choice == "3":
                ctrl.queue_action("pause")
                print("Paused alert queue.")
            elif choice == "4":
                ctrl.queue_action("resume")
                print("Resumed alert queue.")
            elif choice == "5":
                m = ctrl.add_marker("Keypad marker")
                print(f"Dropped marker: {m.get('id', 'ok')}")
            else:
                print("Commands: 1 (list), '1 <scene>' (switch), 2 (skip), 3 (pause), 4 (resume), 5 (marker), q (quit)")
        except FerrocastApiError as err:
            print(f"Error ({err.status} {err.code}): {err}", file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        ctrl = load_config()
        if args.cmd == "status":
            print(json.dumps(ctrl.status(), indent=2))
        elif args.cmd == "scenes":
            print(json.dumps(ctrl.scenes(), indent=2))
        elif args.cmd == "scene":
            ctrl.switch_scene(args.name)
            print(f"Switched OBS scene to {args.name}")
        elif args.cmd == "mute":
            want = True if args.on else (False if args.off else None)
            state = ctrl.set_mute(args.input, want)
            print(f"{args.input}: {'muted' if state else 'unmuted'}")
        elif args.cmd == "marker":
            print(json.dumps(ctrl.add_marker(args.label), indent=2))
        elif args.cmd == "replay":
            ctrl.save_replay()
            print("Saved OBS replay buffer.")
        elif args.cmd == "alerts":
            if args.action == "test":
                print(json.dumps(ctrl.test_alert(args.kind), indent=2))
            else:
                ctrl.queue_action(args.action)
                print(f"Alert queue: {args.action}")
        elif args.cmd == "deck":
            interactive_deck(ctrl)
        return 0
    except FerrocastApiError as err:
        print(f"Error ({err.status} {err.code}): {err}", file=sys.stderr)
        if err.hint:
            print(f"Hint: {err.hint}", file=sys.stderr)
        return 2
    except ValueError as err:
        print(f"Error: {err}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
