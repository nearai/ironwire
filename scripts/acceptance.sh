#!/usr/bin/env bash
# Manual acceptance check (docs/PROTOCOL.md §7.5).
#
# The automated suite proves wire fidelity against a mock. This proves the only
# thing a mock cannot: that a real coding agent, talking to real providers
# through IronWire, behaves the same as it does talking to them directly.
#
# It costs real subscription quota. Run it before a release, not in CI.
#
#   scripts/acceptance.sh [task-directory]
#
# Requires: `claude` on PATH, a Claude Code login, and a built `ironwire`.

set -euo pipefail

TASK_DIR="${1:-$(mktemp -d)}"
PORT="${IRONWIRE_PORT:-8464}"
IRONWIRE="${IRONWIRE_BIN:-./target/release/ironwire}"
TASK='Read failing.rs, work out why the test fails, fix it, and run `cargo test` to confirm.'

command -v claude >/dev/null || { echo "claude is not on PATH"; exit 1; }
[ -x "$IRONWIRE" ] || { echo "build first: cargo build --release"; exit 1; }

echo "==> Task directory: $TASK_DIR"
mkdir -p "$TASK_DIR/src"
cat > "$TASK_DIR/Cargo.toml" <<'TOML'
[package]
name = "acceptance"
version = "0.1.0"
edition = "2021"
TOML
cat > "$TASK_DIR/src/lib.rs" <<'RS'
/// Sums 1..=n. Currently wrong: the upper bound is exclusive.
pub fn sum_to(n: u32) -> u32 {
    (1..n).sum()
}

#[cfg(test)]
mod tests {
    #[test]
    fn sums_inclusively() {
        assert_eq!(super::sum_to(5), 15);
    }
}
RS

echo "==> Starting IronWire on port $PORT"
"$IRONWIRE" serve --port "$PORT" > /tmp/ironwire-acceptance.log 2>&1 &
DAEMON=$!
trap 'kill $DAEMON 2>/dev/null || true' EXIT
sleep 2

"$IRONWIRE" doctor --port "$PORT"

echo
echo "==> Running the task THROUGH IronWire"
BEFORE=$(date +%s)
(
  cd "$TASK_DIR"
  ANTHROPIC_BASE_URL="http://127.0.0.1:$PORT/anthropic" claude -p "$TASK"
)
AFTER=$(date +%s)

echo
echo "==> Verifying the agent actually fixed it"
if (cd "$TASK_DIR" && cargo test --quiet); then
  echo "PASS — the task completed through IronWire in $((AFTER - BEFORE))s"
else
  echo "FAIL — the test still fails; the agent did not complete the task"
  exit 1
fi

echo
echo "==> What IronWire saw"
"$IRONWIRE" log --port "$PORT" --limit 40
echo
"$IRONWIRE" status --port "$PORT"

cat <<'NOTE'

Compare against a direct run (same task, fresh directory, no ANTHROPIC_BASE_URL).
What matters is not just that both pass, but that the turn counts are close: a
materially longer loop through IronWire means a field is being mis-mapped in a
way the mock-based tests do not catch.
NOTE
