# CLAUDE.md: ferrocast

Read this first, every session.

- **On Windows:** start with `docs/plan/WINDOWS-HANDOFF.md`, all of it, then
  `docs/plan/WINDOWS-SESSION-10NOV.md` (what the 10-11 Nov session hit, fixed and left), then
  `docs/plan/WINDOWS-NEXT.md`. Run everything on the PC before calling it fixed.
- **Branch:** `own-engine-only` in this repo and in `ferrocast-web`, the dashboard, which sits
  beside this repo.
- **Docs:** API contract in `docs/API.md`; engine plan in `docs/plan/ENGINE.md`; plugins in
  `docs/PLUGINS.md`; lessons R-01… in `docs/LESSONS.md`.

## Rules (the owner's)

- **When to commit:** only when the owner says "commit" or "commit and push".
- **Commit style:** small focused commits, conventional subjects with a scope, at most 72
  characters after the colon (`tools/check-commit-msg.sh`); the body says why.
- **No AI credit:** author is the owner only. Never `Co-Authored-By`, "Generated with Claude
  Code" or any AI attribution.
- **Safety:** never commit secrets or `.env`, never force-push, push only the current branch.
- **Before committing:**
  - `cargo fmt --all`, `cargo clippy --all-targets`, the tests of the crates you touched;
  - `coderabbit review --agent --uncommitted` (free plan, 3 an hour, never `--use-credits`;
    `git add -N` new files first). If it can't run, do your own review at the same bar.
- **No agents:** no subagents or parallel agents without asking the owner.
- **Test data:**
  - run the app with `FERROCAST_HOME` set to a test folder, never the owner's data;
  - don't capture the owner's screen, camera, mic or keyboard from a test build unless they
    say so.
- **Talking to the owner:** plain, short English. In the UI, little text, details behind (i).
- **Releases:** only when the owner says (`docs/plan/WINDOWS-HANDOFF.md` §9 has the steps).
