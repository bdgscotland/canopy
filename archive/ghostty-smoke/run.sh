#!/bin/bash
# Canopy Step 0 — smoke checklist.
#
# Resolves the six empirical unknowns (OQ1-OQ6) from the design doc BEFORE any
# production Go is written. Each unknown, if answered wrong, changes the design:
#
#   OQ1  surface configuration syntax: record literal vs `new surface configuration`
#   OQ2  environment variables: does the list MERGE with or REPLACE the inherited env?
#   OQ3  renderer configuration                       (separate: see smoke/fps/)
#   OQ4  does `shell:` + `quoted form` survive a binary path containing a space?
#   OQ5  does a split surface self-close when its command exits, and does the
#        sibling rebalance to fill the space?
#   OQ6  can `close` address a terminal by id, or must the implementation iterate?
#
# This script CREATES AND DESTROYS SPLIT PANES in your frontmost Ghostty window.
# Ghostty must be frontmost and focused on the pane you want split.

set -uo pipefail

SMOKE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="${TMPDIR:-/tmp}/canopy-smoke"
# Deliberately contains a space and an apostrophe — this IS the OQ4 test.
SPACED_DIR="$WORK/dir with space and ' quote"
PROBE="$SPACED_DIR/probe exe.sh"

PASS=0; FAIL=0; UNKNOWN=0
declare -a RESULTS=()

say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); RESULTS+=("PASS  $1"); printf '  \033[32mPASS\033[0m  %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); RESULTS+=("FAIL  $1"); printf '  \033[31mFAIL\033[0m  %s\n' "$1"; }
huh()  { UNKNOWN=$((UNKNOWN+1)); RESULTS+=("?     $1"); printf '  \033[33m?\033[0m     %s\n' "$1"; }

count_terminals() {
  osascript -e 'tell application "Ghostty" to return count of terminals of front window' 2>/dev/null || echo "-1"
}

preflight() {
  say "=== preflight ==="
  if [ "${TERM_PROGRAM:-}" != "ghostty" ]; then
    say "  FATAL: not running inside Ghostty (TERM_PROGRAM=${TERM_PROGRAM:-unset})"; exit 1
  fi
  if [ -n "${TMUX:-}" ]; then
    say "  FATAL: running inside tmux. Detach first — the design rejects this case."; exit 1
  fi
  local fm
  fm=$(osascript -e 'tell application "Ghostty" to return frontmost' 2>&1)
  if [ "$fm" != "true" ]; then
    say "  FATAL: Ghostty is not frontmost (got: $fm)."
    say "         Click into the Ghostty window and re-run. Do not switch away during the run."
    exit 1
  fi
  say "  Ghostty $(osascript -e 'tell application "Ghostty" to return version' 2>/dev/null)  frontmost=true  terminals=$(count_terminals)"
  say "  RLIMIT_NOFILE (this shell): $(ulimit -n)   launchd soft: $(launchctl limit maxfiles | awk '{print $2}')"
  say ""
}

setup() {
  rm -rf "$WORK"
  mkdir -p "$SPACED_DIR"
  # Probe writes its argv and full environment, then exits. Exiting is the point:
  # OQ5 depends on the pane closing when the command finishes.
  cat > "$PROBE" <<'PROBE_EOF'
#!/bin/bash
OUT="${CANOPY_SMOKE_OUT:-/tmp/canopy-smoke/probe-out.txt}"
mkdir -p "$(dirname "$OUT")"
{
  echo "ARGV0=$0"
  echo "ARGC=$#"
  echo "--- env ---"
  env
} > "$OUT" 2>&1
sleep 2
PROBE_EOF
  chmod +x "$PROBE"
}

# ---------------------------------------------------------------------------
# OQ1 + OQ4 + OQ2 + OQ5 : one split per config variant
# ---------------------------------------------------------------------------
run_split_variant() {
  local variant="$1" script="$2"
  local out="$WORK/probe-$variant.txt"
  rm -f "$out"

  local before after result treeid focusok
  before=$(count_terminals)

  result=$(osascript "$script" "$PROBE" "$PWD" \
      "CANOPY_SMOKE_OUT=$out" "CANOPY_SMOKE_MARKER=marker-$variant" 2>&1)

  case "$result" in
    ERR:record-literal-rejected:*|ERR:newcfg-rejected:*)
      bad "OQ1/$variant: config syntax rejected — ${result#ERR:}"
      return 1 ;;
    ERR:*)
      bad "OQ1/$variant: ${result#ERR:}"
      return 1 ;;
    OK\|*) ;;
    *)
      huh "OQ1/$variant: unrecognized result: $result"
      return 1 ;;
  esac

  ok "OQ1/$variant: config syntax accepted, split returned a terminal"
  treeid="$(printf '%s' "$result" | cut -d'|' -f2)"
  focusok="$(printf '%s' "$result" | cut -d'|' -f3)"

  after=$(count_terminals)
  if [ "$after" -gt "$before" ]; then
    ok "OQ1/$variant: terminal count $before -> $after (pane really appeared)"
  else
    bad "OQ1/$variant: terminal count did not increase ($before -> $after)"
  fi

  if [ "$focusok" = "true" ]; then
    ok "P5/$variant: focus verified back on the original pane within 200ms"
  else
    bad "P5/$variant: focus did NOT verify within 200ms — focus poll is load-bearing"
  fi

  # --- OQ4: did the spaced/quoted path actually execute? ---
  local waited=0
  while [ ! -f "$out" ] && [ "$waited" -lt 30 ]; do sleep 0.1; waited=$((waited+1)); done
  if [ -f "$out" ]; then
    ok "OQ4/$variant: shell: + quoted form executed a path with space and apostrophe"
    local argc; argc=$(grep '^ARGC=' "$out" | cut -d= -f2)
    say "        ARGV0=$(grep '^ARGV0=' "$out" | cut -d= -f2-)  ARGC=$argc"
  else
    bad "OQ4/$variant: probe never ran — quoting is broken for spaced paths"
    printf '%s' "$treeid" > "$WORK/lastid-$variant"
    return 1
  fi

  # --- OQ2: merge or replace? ---
  if grep -q '^CANOPY_SMOKE_MARKER=' "$out"; then
    ok "OQ2/$variant: injected env vars arrived in the pane"
  else
    bad "OQ2/$variant: injected env vars did NOT arrive"
  fi
  local missing=""
  for v in TERM PATH HOME LANG TERMINFO; do
    grep -q "^${v}=" "$out" || missing="$missing $v"
  done
  if [ -z "$missing" ]; then
    ok "OQ2/$variant: env MERGES — TERM/PATH/HOME/LANG/TERMINFO all inherited"
  else
    bad "OQ2/$variant: env REPLACES — missing:$missing (design must pass these explicitly)"
  fi
  say "        pane env var count: $(sed -n '/^--- env ---$/,$p' "$out" | grep -c '=')"

  # --- OQ5: does the pane self-close when the command exits? ---
  say "        waiting for probe to exit (sleep 2) to test self-close..."
  sleep 4
  local final; final=$(count_terminals)
  if [ "$final" -eq "$before" ]; then
    ok "OQ5/$variant: pane self-closed on command exit (count back to $before)"
    printf '' > "$WORK/lastid-$variant"
  else
    bad "OQ5/$variant: pane did NOT self-close (count $final, expected $before) — RELEASE BLOCKER"
    printf '%s' "$treeid" > "$WORK/lastid-$variant"
  fi
  say "        NOTE (manual): did the remaining pane rebalance to fill the space? Watch the window."
  return 0
}

# ---------------------------------------------------------------------------
# OQ6 : close by id
# ---------------------------------------------------------------------------
test_close_by_id() {
  say ""
  say "=== OQ6: close a terminal by id ==="
  local before after result treeid
  before=$(count_terminals)
  # Spawn a long-lived pane specifically so we have something to close.
  result=$(osascript -e '
    on run argv
      tell application "Ghostty"
        set t to focused terminal of selected tab of front window
        set cfg to {initial working directory:(item 1 of argv), command:"shell:sleep 600", wait after command:false}
        set nt to split t direction right with configuration cfg
        focus t
        return id of nt
      end tell
    end run' "$PWD" 2>&1)

  case "$result" in
    *error*|"") huh "OQ6: could not create a pane to close ($result)"; return ;;
  esac
  treeid="$result"
  sleep 0.5

  local closeres; closeres=$(osascript "$SMOKE_DIR/close-by-id.applescript" "$treeid" 2>&1)
  sleep 0.5
  after=$(count_terminals)

  case "$closeres" in
    OK:direct)
      ok "OQ6: 'first terminal of front window whose id is X' resolves — use the direct specifier" ;;
    OK:iterate)
      ok "OQ6: direct id specifier failed; iteration works — Close() must iterate terminals" ;;
    *)
      bad "OQ6: could not close by id at all ($closeres)" ;;
  esac
  if [ "$after" -eq "$before" ]; then
    ok "OQ6: terminal count returned to $before (the right pane was closed)"
  else
    bad "OQ6: terminal count is $after, expected $before"
  fi
}

cleanup() {
  say ""
  say "=== cleanup ==="
  for f in "$WORK"/lastid-*; do
    [ -f "$f" ] || continue
    local id; id=$(cat "$f")
    [ -n "$id" ] || continue
    say "  closing leftover pane $id"
    osascript "$SMOKE_DIR/close-by-id.applescript" "$id" >/dev/null 2>&1
  done
  say "  probe output kept in $WORK for inspection"
}

main() {
  preflight
  setup

  say "=== OQ1 variant A: record literal (+ OQ2, OQ4, OQ5, P5) ==="
  run_split_variant "record" "$SMOKE_DIR/split-record.applescript"
  say ""
  say "=== OQ1 variant B: new surface configuration (+ OQ2, OQ4, OQ5, P5) ==="
  run_split_variant "newcfg" "$SMOKE_DIR/split-newcfg.applescript"

  test_close_by_id
  cleanup

  say ""
  say "════════════════════════════════════════════════"
  printf '  %d pass  %d fail  %d unknown\n' "$PASS" "$FAIL" "$UNKNOWN"
  say "════════════════════════════════════════════════"
  for r in "${RESULTS[@]}"; do say "  $r"; done
  say ""
  say "  OQ3 (renderer) is separate: see smoke/fps/README.md"
  say "  Record every answer in DECISIONS.md before writing internal/companion/ghostty."
  [ "$FAIL" -eq 0 ]
}

main "$@"
