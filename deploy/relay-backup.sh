#!/usr/bin/env bash
# The relay's nightly off-server backup to Cloudflare R2, encrypted before it leaves the server.
# Installed as /opt/ferrocast-relay/relay-backup.sh by deploy/relay-ci-setup.sh; runs as
# 'deploy' (in the docker group), so nothing is installed on the server: SQLite and rclone run
# in throwaway containers.
#
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/relay-backup.sh --setup'    keys + nightly job (once)
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/relay-backup.sh'            back up now
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/relay-backup.sh --restore-test'  the restore drill
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/relay-backup.sh --status'   last run
#
# Each night: a consistent copy of relay.db (SQLite's backup API, safe while the relay runs)
# and relay.env (it holds the vault key, without which nothing sealed can be opened) go to
# db/<date>/, kept 30 days; the blobs folder (cloud backups, history) is mirrored to blobs/,
# so a blob the relay deletes under its own 30-day rules is deleted there too. Everything goes
# through an rclone "crypt" remote: R2 only ever holds encrypted names and contents. The
# encryption password is shown once at --setup; keep it in a password manager, because a
# backup can't be opened without it. After a good run the time also goes into the relay's volume
# (/data/backup-last-ok), so the Control Center can show it.
set -euo pipefail

DIR="${RELAY_DIR:-/opt/ferrocast-relay}"
STATE="$DIR/state"
CONF="$STATE/rclone.conf"
LOG="$STATE/backup.log"
LAST="$STATE/backup-last-ok"
CONTAINER="${RELAY_CONTAINER:-ferrocast-relay}"
BUCKET="${RELAY_BACKUP_BUCKET:-ferrocast-relay-backups}"
KEEP_DAYS=30
# Pinned images (digest-free tags, updated by hand).
PY_IMAGE="python:3.13-alpine"
RCLONE_IMAGE="rclone/rclone:1.68"

die() {
  echo "$*" >&2
  exit 1
}

cd "$DIR" 2>/dev/null || die "No $DIR: install the relay first (deploy/deploy-relay.sh)."
command -v docker >/dev/null || die "No docker here: run this on the relay server."

rclone() { # rclone in a container, with the config and an optional host folder at /src
  local src_mount=()
  if [[ -n "${SRC:-}" ]]; then src_mount=(-v "$SRC:/src"); fi
  docker run --rm -v "$CONF:/config/rclone/rclone.conf:ro" ${src_mount[@]+"${src_mount[@]}"} "$RCLONE_IMAGE" "$@"
}

volume() { # the Docker volume behind the relay's /data
  docker inspect -f '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Name}}{{end}}{{end}}' "$CONTAINER" 2>/dev/null
}

setup() {
  [[ -t 0 ]] || die "Run it with ssh -t so it can ask for the keys."
  mkdir -p "$STATE"
  local account key secret pass
  echo "Cloudflare R2 → bucket '$BUCKET' → an API token with Object Read & Write on that bucket only."
  read -r -p "R2 account ID (32 hex characters): " account
  [[ "$account" =~ ^[0-9a-f]{32}$ ]] || die "That isn't an account ID (32 lowercase hex characters)."
  read -r -s -p "Access Key ID (hidden): " key
  echo
  read -r -s -p "Secret Access Key (hidden): " secret
  echo
  if [[ ${#key} -eq 64 && ${#secret} -eq 32 ]]; then
    die "Those look swapped: the Access Key ID is the shorter one (32 characters), the Secret the longer (64). Run --setup again."
  fi
  [[ ${#key} -eq 32 ]] || die "The Access Key ID should be 32 characters (got ${#key}). Run --setup again."
  [[ ${#secret} -eq 64 ]] || die "The Secret Access Key should be 64 characters (got ${#secret}). Run --setup again."
  pass="$(openssl rand -base64 33 | tr -d '/+=\n' | cut -c1-40)"
  local tmp
  tmp="$(mktemp "$STATE/rclone.conf.XXXXXX")"
  chmod 600 "$tmp"
  {
    printf '[r2]\ntype = s3\nprovider = Cloudflare\nendpoint = https://%s.r2.cloudflarestorage.com\n' "$account"
    printf 'access_key_id = %s\nsecret_access_key = %s\nno_check_bucket = true\n\n' "$key" "$secret"
    printf '[box]\ntype = crypt\nremote = r2:%s\nfilename_encryption = standard\n' "$BUCKET"
    # Through stdin: never on a command line, where any account could see it in `ps`.
    printf 'password = %s\n' "$(printf '%s' "$pass" | docker run -i --rm "$RCLONE_IMAGE" obscure -)"
  } >"$tmp"
  echo "Checking the bucket..."
  # Checked before it replaces anything, so a bad paste never leaves a broken config behind.
  if ! docker run --rm -v "$tmp:/config/rclone/rclone.conf:ro" "$RCLONE_IMAGE" lsd r2:"$BUCKET" >/dev/null; then
    rm -f "$tmp"
    die "R2 refused the keys or the bucket '$BUCKET' doesn't exist. Nothing was saved or scheduled."
  fi
  mv "$tmp" "$CONF"
  local line="30 21 * * * $DIR/relay-backup.sh >>$LOG 2>&1"
  { crontab -l 2>/dev/null | grep -v 'relay-backup.sh' || true; echo "$line"; } | crontab -
  echo "✓ Saved the R2 keys ($CONF, readable by deploy only)."
  echo "✓ Nightly backup at 03:00 IST (21:30 UTC)."
  echo
  echo "Backup password (shown once, needed to restore; save it in your password manager now):"
  echo "  $pass"
  echo
  echo "Now run it once and do the restore drill:"
  echo "  ssh -t <server> 'sudo -u deploy $DIR/relay-backup.sh && sudo -u deploy $DIR/relay-backup.sh --restore-test'"
}

backup() {
  [[ -f "$CONF" ]] || die "Not set up: run relay-backup.sh --setup first."
  local vol day work
  vol="$(volume)"
  [[ -n "$vol" ]] || die "The relay container ($CONTAINER) isn't there, so its data volume can't be found."
  day="$(date -u +%F)"
  work="$(mktemp -d "$STATE/backup.XXXXXX")"
  trap 'rm -rf "$work"' RETURN
  # SQLite's online backup: a consistent copy even while the relay writes (WAL).
  docker run --rm -v "$vol:/data" -v "$work:/out" "$PY_IMAGE" python -c '
import sqlite3
src = sqlite3.connect("file:/data/relay.db?mode=ro", uri=True)
dst = sqlite3.connect("/out/relay.db")
src.backup(dst)
ok = dst.execute("PRAGMA integrity_check").fetchone()[0]
dst.close()
assert ok == "ok", ok
'
  cp "$DIR/relay.env" "$work/relay.env"
  SRC="$work" rclone copy /src "box:db/$day"
  # Blobs mirror the relay's own deletions (no folder yet: nothing stored so far).
  if docker run --rm -v "$vol:/data:ro" "$PY_IMAGE" test -d /data/blobs; then
    docker run --rm -v "$vol:/data:ro" -v "$CONF:/config/rclone/rclone.conf:ro" "$RCLONE_IMAGE" \
      sync /data/blobs box:blobs --create-empty-src-dirs
  fi
  rclone delete box:db --min-age "${KEEP_DAYS}d"
  rclone rmdirs box:db --leave-root
  local ok_at
  ok_at="$(date -u +%FT%TZ)"
  printf '%s\n' "$ok_at" >"$LAST"
  # The same time next to relay.db, where the relay reads it for the Control Center
  # (GET /admin/v1/stats → backup.last_ok_at). Written whole, then renamed into place.
  docker run --rm -e "OK_AT=$ok_at" -v "$vol:/data" "$PY_IMAGE" python -c '
import os
tmp = "/data/backup-last-ok.tmp"
with open(tmp, "w") as f:
    f.write(os.environ["OK_AT"] + "\n")
os.chmod(tmp, 0o644)
os.replace(tmp, "/data/backup-last-ok")
' || echo "note: the backup is fine, but the relay couldn't be told when it ran (backup-last-ok)." >&2
  echo "$ok_at ✓ backed up relay.db, relay.env and blobs to R2 (db/$day)."
}

restore_test() {
  [[ -f "$CONF" ]] || die "Not set up: run relay-backup.sh --setup first."
  local latest work
  latest="$(rclone lsf box:db --dirs-only | sort | tail -n 1 | tr -d '/')"
  [[ -n "$latest" ]] || die "No backups in R2 yet: run relay-backup.sh first."
  work="$(mktemp -d "$STATE/restore.XXXXXX")"
  trap 'rm -rf "$work"' RETURN
  SRC="$work" rclone copy "box:db/$latest" /src
  # Opens the restored copy away from the live relay and counts what's in it.
  docker run --rm -v "$work:/r" "$PY_IMAGE" python -c '
import sqlite3
c = sqlite3.connect("/r/relay.db")
assert c.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
for t in ("installs", "pro_subs", "pro_installs"):
    n = c.execute("SELECT count(*) FROM " + t).fetchone()[0]
    print("  " + t + ": " + str(n) + " rows")
'
  grep -q '^RELAY_VAULT_KEY=' "$work/relay.env" || die "The restored relay.env has no RELAY_VAULT_KEY: sealed data couldn't be opened."
  local blobs
  blobs="$(rclone size box:blobs 2>/dev/null | head -n 2 | tr '\n' ' ')"
  echo "✓ Restore drill passed for $latest: the database opens and checks clean, relay.env has the vault key."
  echo "  Blobs in R2: ${blobs:-none yet}"
}

case "${1:-}" in
  --setup) setup ;;
  --restore-test) restore_test ;;
  --status)
    if [[ -f "$LAST" ]]; then echo "Last good backup: $(cat "$LAST")"; else echo "No backup has run yet."; fi
    [[ -f "$CONF" ]] && echo "Set up: yes" || echo "Set up: no (run --setup)"
    ;;
  "") backup ;;
  *) die "Use: relay-backup.sh [--setup | --restore-test | --status]" ;;
esac
