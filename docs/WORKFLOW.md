# Workflow

## Commits

- Conventional subjects: `feat(core): …`, `fix(twitch): …`, `docs(plan): …`. One logical change per commit; each commit builds and passes `tools/check.sh`.
- The body says *why*.
- The author is the repo owner only. No `Co-Authored-By`, no "Generated with …" lines. The commit-msg hook enforces it (`git config core.hooksPath .githooks` once per clone).
- Contributors sign off with DCO (`git commit -s`).

## Gates

`tools/check.sh` runs rustfmt, clippy with `-D warnings`, the tests (nextest), and `cargo deny`. CI runs the same on macOS, Windows and Linux.

## Reviews

Every batch of changes gets a review before it is committed: CodeRabbit (`coderabbit review --agent --uncommitted`) or, when its free limit is reached, an equally strict manual review. Real findings are fixed; skipped ones are written down with the reason.

## Testing

- Pure logic in `core` gets unit tests for the happy path **and** for hostile input (empty, huge, unicode tricks, wrong types, replays, out-of-order).
- Adapters get fixture tests (raw payload → `Event`) and fake-server tests (disconnects, keepalive loss, reconnect handoff, 401 then refresh, 429 with Retry-After).
- Server routes get negative tests for every way a request can be wrong: no token, wrong token, wrong Origin, wrong Host, oversized body (413), wrong type (415), unknown ids (404), bad JSON (400).
