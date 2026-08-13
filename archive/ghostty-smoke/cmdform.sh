#!/bin/bash
# OQ4 isolation: which `command` string forms actually execute in a Ghostty split?
#
# The full Step 0 run showed `shell:'<path with space>'` never executing. This
# narrows down whether the culprit is the `shell:` prefix, the quoting, the space,
# or the AppleScript command field itself.
#
# Each form writes a distinct marker file. A missing marker means that form does
# not work. Panes are closed explicitly, so `wait after command` is TRUE here —
# a pane that fails instantly would otherwise vanish before we could see it.

set -uo pipefail

W="${TMPDIR:-/tmp}/canopy-cmdform"
PLAIN="$W/plainexe.sh"
SPACED_DIR="$W/dir with space"
SPACED="$SPACED_DIR/spaced exe.sh"

rm -rf "$W"; mkdir -p "$W" "$SPACED_DIR"
for p in "$PLAIN" "$SPACED"; do
  cat > "$p" <<'EOF'
#!/bin/bash
touch "$MARKER"
sleep 1
EOF
  chmod +x "$p"
done

qplain=$(printf '%q' "$PLAIN")
qspaced=$(printf '%q' "$SPACED")

# form-name | command string | which exe it should run
declare -a FORMS=(
  "bare-plain|$PLAIN"
  "shell-plain|shell:$PLAIN"
  "direct-plain|direct:$PLAIN"
  "bare-spaced|$SPACED"
  "shellq-spaced|shell:$qspaced"
  "directraw-spaced|direct:$SPACED"
  "shell-sh-c-spaced|shell:/bin/sh -c $qspaced"
)

spawn() {
  # $1 = command string, $2 = marker path -> echoes the new terminal id or ERR:
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
  [ -n "${1:-}" ] || return 0
  case "$1" in ERR:*) return 0 ;; esac
  osascript -e '
    on run argv
      tell application "Ghostty"
        try
          close (first terminal of front window whose id is (item 1 of argv))
        end try
      end tell
    end run' "$1" >/dev/null 2>&1
}

printf '\n%-22s %-8s %s\n' "FORM" "RESULT" "COMMAND STRING"
printf '%s\n' "----------------------------------------------------------------------"

for entry in "${FORMS[@]}"; do
  name="${entry%%|*}"; cmd="${entry#*|}"
  marker="$W/marker-$name"
  rm -f "$marker"

  id=$(spawn "$cmd" "$marker")
  case "$id" in
    ERR:*) printf '%-22s %-8s %s\n' "$name" "SPAWNERR" "$id"; continue ;;
  esac

  waited=0
  while [ ! -f "$marker" ] && [ "$waited" -lt 25 ]; do sleep 0.1; waited=$((waited+1)); done

  if [ -f "$marker" ]; then
    printf '%-22s \033[32m%-8s\033[0m %s\n' "$name" "RAN" "$cmd"
  else
    printf '%-22s \033[31m%-8s\033[0m %s\n' "$name" "NO-RUN" "$cmd"
  fi
  close_pane "$id"
  sleep 0.3
done

printf '\n  Forms marked RAN are safe for the design. Record the winner in DECISIONS.md D4.\n\n'
