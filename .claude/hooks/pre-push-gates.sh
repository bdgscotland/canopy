#!/usr/bin/env bash
# Blocks `git push` until the gates CI enforces actually pass locally.
#
# CI runs cargo fmt --check, cargo clippy -D warnings and cargo test. Pushing
# without them means finding out from a red build minutes later, which has
# already happened once.
#
# Reads the PreToolUse payload on stdin; emits a deny decision on failure.
set -uo pipefail

# Gate on the command ourselves rather than trusting the settings `if` filter,
# which was observed firing this hook for every Bash call.
payload=$(cat)
cmd=$(printf '%s' "$payload" | jq -r '.tool_input.command // ""' 2>/dev/null)
case "$cmd" in
  *"git push"*) ;;
  *) exit 0 ;;
esac

root=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0
cd "$root" || exit 0
# Only meaningful in a Rust workspace; anything else passes straight through.
[ -f Cargo.toml ] || exit 0

fails=()
out=""

run_gate() {
  local name="$1"; shift
  local log
  log=$("$@" 2>&1) || {
    fails+=("$name")
    out+="--- $name ---"$'\n'
    # Keep it short: the first few lines carry the actual error.
    out+=$(printf '%s\n' "$log" | tail -25)$'\n'
  }
}

run_gate "cargo fmt --check"                cargo fmt --check
run_gate "cargo clippy -D warnings"         cargo clippy --all-targets -- -D warnings
run_gate "cargo test"                       cargo test

if [ ${#fails[@]} -eq 0 ]; then
  exit 0
fi

joined=$(printf '%s, ' "${fails[@]}")
reason="Push blocked — these fail locally and would fail CI: ${joined%, }"$'\n\n'"$out"

if command -v jq >/dev/null 2>&1; then
  jq -nc --arg r "$reason" \
    '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:"deny",permissionDecisionReason:$r}}'
else
  printf '%s\n' "$reason" >&2
  exit 2
fi
