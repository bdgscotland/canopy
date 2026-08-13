#!/bin/bash
# OQ4 round 2. Round 1 established that the AppleScript `command` field:
#   - does NOT honor the shell:/direct: prefixes (those are config-file only)
#   - does NOT go through /bin/sh -c (backslash-escaped spaces failed)
#   - runs a bare, space-free, argument-less path fine
#
# Two things the design depends on remain unknown:
#   A. Can the command carry ARGUMENTS?  (we need `<exe> __canopy_tree`)
#   B. Is there ANY way to express a path containing a space?
#
# If A fails, the subcommand must move into the environment.
# If B fails, Canopy cannot run from a spaced install path and must symlink out.

set -uo pipefail

W="${TMPDIR:-/tmp}/canopy-cmdform2"
PLAIN="$W/plainexe.sh"
SPACED_DIR="$W/dir with space"
SPACED="$SPACED_DIR/spaced exe.sh"
LINK="$W/linked-exe"

rm -rf "$W"; mkdir -p "$W" "$SPACED_DIR"
for p in "$PLAIN" "$SPACED"; do
  cat > "$p" <<'EOF'
#!/bin/bash
echo "argc=$# argv=$*" > "$MARKER"
sleep 1
EOF
  chmod +x "$p"
done
ln -sf "$SPACED" "$LINK"     # symlink from a space-free path INTO the spaced one

declare -a FORMS=(
  "args-one|$PLAIN __canopy_tree"
  "args-two|$PLAIN __canopy_tree --root /tmp"
  "dquote-spaced|\"$SPACED\""
  "squote-spaced|'$SPACED'"
  "symlink-to-spaced|$LINK"
)

spawn() {
  osascript -e '
    on run argv
      tell application "Ghostty"
        if not frontmost then return "ERR:not-frontmost"
        set t to focused terminal of selected tab of front window
        try
          set cfg to {initial working directory:(item 3 of argv), ¬
                      command:(item 1 of argv), ¬
                      wait after command:true, ¬
                      environment variables:{"MARKER=" & (item 2 of argv)}}
          set nt to split t direction right with configuration cfg
        on error e
          return "ERR:" & e
        end try
        set nid to id of nt
        focus t
        return nid
      end tell
    end run' "$1" "$2" "$W" 2>&1
}

close_pane() {
  case "${1:-}" in ""|ERR:*) return 0 ;; esac
  osascript -e '
    on run argv
      tell application "Ghostty"
        try
          close (first terminal of front window whose id is (item 1 of argv))
        end try
      end tell
    end run' "$1" >/dev/null 2>&1
}

printf '\n%-20s %-8s %-28s %s\n' "FORM" "RESULT" "ARGV SEEN" "COMMAND"
printf '%s\n' "--------------------------------------------------------------------------------"

for entry in "${FORMS[@]}"; do
  name="${entry%%|*}"; cmd="${entry#*|}"
  marker="$W/marker-$name"; rm -f "$marker"

  id=$(spawn "$cmd" "$marker")
  case "$id" in ERR:*) printf '%-20s %-8s %s\n' "$name" "SPAWNERR" "$id"; continue ;; esac

  waited=0
  while [ ! -f "$marker" ] && [ "$waited" -lt 25 ]; do sleep 0.1; waited=$((waited+1)); done

  if [ -f "$marker" ]; then
    printf '%-20s \033[32m%-8s\033[0m %-28s %s\n' "$name" "RAN" "$(cat "$marker")" "${cmd##*/}"
  else
    printf '%-20s \033[31m%-8s\033[0m %-28s %s\n' "$name" "NO-RUN" "-" "${cmd##*/}"
  fi
  close_pane "$id"
  sleep 0.3
done
echo
