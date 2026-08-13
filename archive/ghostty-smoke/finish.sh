#!/bin/bash
# Step 0, completing run. Answers what the earlier runs left open:
#   OQ4c  paths containing a quote character — which wrapper survives?
#   OQ2   environment variables: MERGE with, or REPLACE, the inherited env?
#   OQ5   does a split surface self-close when its command exits?
#
# Established already: the AppleScript `command` field tokenizes on whitespace,
# honors "..." and '...' as quotes, does NOT honor shell:/direct: prefixes, and
# is NOT /bin/sh -c. So AppleScript's `quoted form` is the WRONG tool here — it
# emits shell escaping this tokenizer never sees.

set -uo pipefail
W="${TMPDIR:-/tmp}/canopy-finish"
rm -rf "$W"; mkdir -p "$W"

mk() {  # mk <dirname> -> echoes exe path
  local d="$W/$1"; mkdir -p "$d"
  cat > "$d/exe.sh" <<'EOF'
#!/bin/bash
{ echo "argc=$#"; echo "--- env ---"; env; } > "$MARKER"
sleep 2
EOF
  chmod +x "$d/exe.sh"; echo "$d/exe.sh"
}

APOS=$(mk "has ' apostrophe")
DQUO=$(mk 'has " dquote')
PLAIN=$(mk "plaindir")

spawn() {  # $1=command  $2=marker  $3=waitAfter -> id or ERR:
  osascript -e '
    on run argv
      tell application "Ghostty"
        if not frontmost then return "ERR:not-frontmost"
        set t to focused terminal of selected tab of front window
        set waitAfter to ((item 4 of argv) is "yes")
        try
          set cfg to {initial working directory:(item 3 of argv), ¬
                      command:(item 1 of argv), ¬
                      wait after command:waitAfter, ¬
                      environment variables:{"MARKER=" & (item 2 of argv), "CANOPY_PROBE=hello"}}
          set nt to split t direction right with configuration cfg
        on error e
          return "ERR:" & e
        end try
        set nid to id of nt
        focus t
        return nid
      end tell
    end run' "$1" "$2" "$W" "$3" 2>&1
}
closep() { case "${1:-}" in ""|ERR:*) return 0;; esac
  osascript -e 'on run argv
      tell application "Ghostty"
        try
          close (first terminal of front window whose id is (item 1 of argv))
        end try
      end tell
    end run' "$1" >/dev/null 2>&1; }
count() { osascript -e 'tell application "Ghostty" to return count of terminals of front window' 2>/dev/null || echo -1; }

echo
echo "=== OQ4c: quote wrappers for pathological paths ==="
printf '%-26s %-8s %s\n' "CASE" "RESULT" "WRAPPER"
for entry in "apostrophe-dquoted|\"$APOS\"|double quotes" \
             "dquote-squoted|'$DQUO'|single quotes" \
             "dquote-dquoted|\"$DQUO\"|double quotes (expected to fail)"; do
  name="${entry%%|*}"; rest="${entry#*|}"; cmd="${rest%%|*}"; desc="${rest#*|}"
  m="$W/m-$name"; rm -f "$m"
  id=$(spawn "$cmd" "$m" yes)
  case "$id" in ERR:*) printf '%-26s %-8s %s\n' "$name" "SPAWNERR" "$id"; continue;; esac
  w=0; while [ ! -f "$m" ] && [ $w -lt 25 ]; do sleep 0.1; w=$((w+1)); done
  if [ -f "$m" ]; then printf '%-26s \033[32m%-8s\033[0m %s\n' "$name" "RAN" "$desc"
  else printf '%-26s \033[31m%-8s\033[0m %s\n' "$name" "NO-RUN" "$desc"; fi
  closep "$id"; sleep 0.3
done

echo
echo "=== OQ2: environment merge or replace ==="
m="$W/m-env"; rm -f "$m"
id=$(spawn "\"$PLAIN\"" "$m" yes)
w=0; while [ ! -f "$m" ] && [ $w -lt 30 ]; do sleep 0.1; w=$((w+1)); done
if [ -f "$m" ]; then
  n=$(sed -n '/^--- env ---$/,$p' "$m" | grep -c '=')
  echo "  pane env var count: $n   (this shell: $(env | wc -l | tr -d ' '))"
  grep -q '^CANOPY_PROBE=hello' "$m" && echo "  injected var arrived: YES" || echo "  injected var arrived: NO"
  miss=""
  for v in TERM PATH HOME LANG TERMINFO USER SHELL; do grep -q "^${v}=" "$m" || miss="$miss $v"; done
  if [ -z "$miss" ]; then echo -e "  \033[32mMERGE\033[0m — TERM/PATH/HOME/LANG/TERMINFO/USER/SHELL all present"
  else echo -e "  \033[31mREPLACE (or partial)\033[0m — missing:$miss"; fi
  echo "  TERM=$(grep '^TERM=' "$m" | head -1 | cut -d= -f2-)"
  echo "  TERMINFO=$(grep '^TERMINFO=' "$m" | head -1 | cut -d= -f2- || echo '(unset)')"
else
  echo "  probe never ran — cannot answer OQ2"
fi
closep "$id"; sleep 0.3

echo
echo "=== OQ5: does a split self-close when its command exits? ==="
before=$(count); echo "  terminals before: $before"
m="$W/m-close"; rm -f "$m"
id=$(spawn "\"$PLAIN\"" "$m" no)      # wait after command FALSE
case "$id" in ERR:*) echo "  SPAWNERR $id";; *)
  sleep 5                              # probe sleeps 2, then exits
  after=$(count); echo "  terminals after command exit: $after"
  if [ "$after" -eq "$before" ]; then echo -e "  \033[32mSELF-CLOSED\033[0m — P3 cleanup mechanism confirmed"
  else echo -e "  \033[31mDID NOT SELF-CLOSE\033[0m — RELEASE BLOCKER, P3 needs rework"; closep "$id"; fi
  echo "  MANUAL: did the remaining pane rebalance to fill the space?"
;; esac
echo
