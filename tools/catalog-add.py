#!/usr/bin/env python3
"""Adds (or updates) a plugin in Ferrocast's plugin catalog, then signs the catalog.

    python3 tools/catalog-add.py <catalog folder> <plugin.zip> [--key ~/.ferrocast/catalog.key]

The catalog folder is the ferrocast-plugins repo: catalog.json, its signature catalog.json.sig,
and plugins/<id>-<version>.zip. The plugin's details come from its own plugin.json; its size and
SHA-256 are worked out here. Signing runs `cargo tauri signer sign` with the catalog key (its
password is asked for, or read from TAURI_SIGNING_PRIVATE_KEY_PASSWORD). See docs/plan/CATALOG.md.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import zipfile

MAX_ZIP = 2 * 1024 * 1024
ID = re.compile(r"^[a-z][a-z0-9._-]{2,63}$")
KINDS = {"effect", "logic", "panel"}


def fail(why: str) -> None:
    print(f"catalog-add: {why}", file=sys.stderr)
    sys.exit(1)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("catalog", help="the ferrocast-plugins folder")
    ap.add_argument("zip", help="the plugin's .zip")
    ap.add_argument("--key", default=os.path.expanduser("~/.ferrocast/catalog.key"))
    ap.add_argument("--no-sign", action="store_true", help="update catalog.json only")
    a = ap.parse_args()

    data = open(a.zip, "rb").read()
    if not 0 < len(data) <= MAX_ZIP:
        fail(f"the plugin is {len(data)} bytes; plugins are 1 byte to {MAX_ZIP}")
    try:
        with zipfile.ZipFile(a.zip) as z:
            m = json.loads(z.read("plugin.json"))
    except (zipfile.BadZipFile, KeyError, json.JSONDecodeError) as e:
        fail(f"no readable plugin.json in it ({e})")
    pid, kind, version = m.get("id", ""), m.get("kind", ""), str(m.get("version", ""))
    if not ID.match(pid) or ".." in pid or pid == "catalog":
        fail(f"its id {pid!r} isn't a plugin id")
    if kind not in KINDS:
        fail(f"its kind {kind!r} isn't effect, logic or panel")
    # The app takes a catalog path only of letters, digits and ._-/ (plugin_catalog::file_url).
    if not re.match(r"^[0-9A-Za-z._-]{1,32}$", version):
        fail(f"its version {version!r} isn't one")

    os.makedirs(os.path.join(a.catalog, "plugins"), exist_ok=True)
    rel = f"plugins/{pid}-{version}.zip"
    shutil.copyfile(a.zip, os.path.join(a.catalog, rel))

    path = os.path.join(a.catalog, "catalog.json")
    cat = json.load(open(path)) if os.path.exists(path) else {"version": 1, "plugins": []}
    entry = {
        "id": pid,
        "name": m.get("name", pid),
        "version": version,
        "author": m.get("author", ""),
        "description": m.get("description", ""),
        "kind": kind,
        "actions": m.get("actions", []),
        "url": rel,
        "sha256": hashlib.sha256(data).hexdigest(),
        "size": len(data),
    }
    cat["plugins"] = [p for p in cat.get("plugins", []) if p.get("id") != pid] + [entry]
    cat["plugins"].sort(key=lambda p: p["name"].lower())
    with open(path, "w") as f:
        json.dump(cat, f, indent=2, ensure_ascii=False)
        f.write("\n")
    print(f"{entry['name']} {version}: {rel} ({len(data)} bytes)")

    if a.no_sign:
        return
    if not os.path.exists(a.key):
        fail(f"no catalog key at {a.key} (make one: cargo tauri signer generate -w {a.key})")
    sig = path + ".sig"
    if os.path.exists(sig):
        os.remove(sig)
    r = subprocess.run(["cargo", "tauri", "signer", "sign", "-f", a.key, path])
    if r.returncode != 0 or not os.path.exists(sig):
        fail("signing didn't work")
    print("signed: catalog.json.sig")


if __name__ == "__main__":
    main()
