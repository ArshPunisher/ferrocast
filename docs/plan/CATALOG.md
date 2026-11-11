# Plugin catalog (built 11 Nov 2026, closed until the owner's key is in)

Settings › Plugins › Catalog lists plugins from a signed list in a GitHub repo and installs or
updates one in a click (owner's pick: "GitHub list, inside the app"). Code:
`crates/engine/src/plugin_catalog.rs`, routes `GET /plugins/catalog`,
`POST /plugins/catalog/install` (PC only, like every plugin route), dashboard
`plugins-settings.tsx` (`CatalogGroup`).

## What's checked

1. The list (`catalog.json`) against its signature (`catalog.json.sig`) and the catalog key
   built into the app (`CATALOG_KEY`): minisign, made with `cargo tauri signer`. Its own key,
   not the app updater's.
2. Each entry: a good plugin id, a kind (effect, logic, panel), a SHA-256, a size up to 2 MB, and
   a file either beside the list (a plain path, no `..`) or https on GitHub. Others are left out.
3. Each file before it's installed: its size and SHA-256 against the signed entry, and the id
   in its own plugin.json against the entry's. Then the usual install checks.
4. Redirects only to GitHub's hosts over https, at most 5.

Until `CATALOG_KEY` holds a key, the catalog says "opens soon" (503 `catalog_not_open`).

## Owner: opening it (once)

1. Make the catalog key (pick a password; keep the key file and password safe, never in git):
   `cargo tauri signer generate -w ~/.ferrocast/catalog.key`
   It prints the public key: put it in `CATALOG_KEY` (or send it to Claude to put in).
2. Make a public GitHub repo `ArshPunisher/ferrocast-plugins` (the address is `CATALOG_URL`).

## Owner: adding or updating a plugin

```bash
python3 tools/catalog-add.py ~/path/to/ferrocast-plugins ~/Downloads/some-plugin.zip
cd ~/path/to/ferrocast-plugins && git add -A && git commit -m "Add some-plugin 1.0.0" && git push
```

The script reads the plugin's plugin.json, copies the zip to `plugins/<id>-<version>.zip`,
writes its entry (size, SHA-256) into `catalog.json` and signs it. Look at a plugin before you
add it: the catalog says Ferrocast vouches for it.
