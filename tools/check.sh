#!/bin/sh
# Every local gate, in CI's order. Fails on the first problem.
# Never pipe this through `head` in a commit chain: it hides the exit code.
set -eu
cd "$(dirname "$0")/.."
# shellcheck source=/dev/null
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
echo "== rustfmt";  cargo fmt --all --check
echo "== clippy";   cargo clippy --workspace --all-targets --locked -q -- -D warnings
echo "== tests";    cargo nextest run --workspace --locked --no-fail-fast --status-level fail --final-status-level fail --no-tests=pass
echo "== deny";     cargo deny check -s 2>/dev/null || cargo deny check
echo "== shellcheck"; shellcheck tools/*.sh packaging/*.sh deploy/*.sh deploy/tests/*.sh
echo "== scripts";  packaging/tests.sh >/dev/null && deploy/tests/razorpay-keys.test.sh >/dev/null && deploy/tests/vault-key.test.sh >/dev/null && deploy/tests/deploy-relay-env.test.sh >/dev/null && deploy/tests/dashboard-swap.test.sh >/dev/null
echo "All checks passed."
