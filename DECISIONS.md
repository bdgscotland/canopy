# DECISIONS

Only decisions that materially affect architecture or performance. Not a changelog.

Design doc: `~/.gstack/projects/canopy/duncan-main-design-20260813-093000.md`
Superseded: `~/.gstack/projects/canopy/duncan-main-design-20260813-081621.md` (see D9)

**Reading order note.** D1-D8 were decided against the terminal-automation design.
D9 pivoted to hosting the PTY. **D2, D4, and D6 are void**; D7's answers are accurate
but describe a design we are not building. D1, D3, D5, D8, and D9-D12 all apply.

---

## D1 — Raw kqueue, not fsnotify

**Decided.** `internal/fswatch/watch_darwin.go` uses `golang.org/x/sys/unix` kqueue
directly rather than `github.com/fsnotify/fsnotify`.

fsnotify's kqueue backend opens a descriptor per *entry* inside every watched
directory so it can report filenames, which reintroduces the fd-per-file cost that
directory-only watching exists to avoid. Raw kqueue also lets directory watches,
`.gitignore` watches, and the parent-PID `EVFILT_PROC` watch share one kqueue fd and
one blocking loop, which fsnotify cannot express.

fsnotify remains the right choice for a future Linux port — inotify has no equivalent
fd problem.

---

## D2 — exec + self-exiting tree, no supervisor

**Decided.** The launcher `syscall.Exec`s Claude. Cleanup is the tree process's job.

Verified empirically: `execve` preserves both PID and `p_starttime`;
`EVFILT_PROC | NOTE_EXIT` fires on a non-child, same-uid PID (`fflags=0x80000000`);
registration against a dead PID returns `ESRCH`. Ghostty's `wait-after-command`
defaults to false, so the pane is reclaimed when the tree process exits.

Consequence: Claude gets a real TTY, real SIGWINCH, real Ctrl-C, and its exit code
propagates for free, because Canopy is not a process by then.

Guard: the tree registers `EVFILT_PROC` as the first statement of `main()`, then
compares `unix.SysctlKinfoProc(pid).Proc.P_starttime` against `CANOPY_PARENT_START`.
Mismatch means the PID was recycled between exec and registration — exit immediately.

---

## D3 — go-git's gitignore, not sabhiram/go-gitignore

**Decided.** `github.com/go-git/go-git/v5/plumbing/format/gitignore`.

`github.com/sabhiram/go-gitignore` (unmaintained since 2021) was verified to:

- escape `?` into a literal, so `foo?.go` never matches `foo1.go`;
- pass `[…]` through untouched, so gitignore's `[!abc]` inverts into a regex class
  matching literal `!abc`;
- silently discard patterns whose generated regexp fails to compile;
- compile `build/` to `^(|.*/)build/(|.*)$`, which never matches a path lacking a
  trailing slash — meaning `echo 'build/' >> .gitignore` would not work.

go-git implements fnmatch semantics properly, and `Matcher.Match(path []string,
isDir bool)` carries the directory bit that makes the trailing-slash case correct.

---

## D4 — plain double quotes. NOT `shell:`, NOT `direct:`, NOT `quoted form`.

**Decided, empirically. This reverses the design doc's original position, which was
wrong in three separate ways.**

Ghostty's *config file* documentation describes `/bin/sh -c` execution and the
`shell:` / `direct:` prefixes. **None of that applies to the AppleScript
`surface configuration.command` field.** Measured behavior (`smoke/cmdform.sh`,
`smoke/cmdform2.sh`, `smoke/finish.sh`):

| Form | Result |
|------|--------|
| `<space-free path>` | RAN |
| `<space-free path> __canopy_tree --root /tmp` | RAN, `argc=3`, argv correct |
| `shell:<space-free path>` | **NO-RUN** — prefix not honored |
| `direct:<space-free path>` | **NO-RUN** — prefix not honored |
| `<path with spaces>` unquoted | NO-RUN |
| `<path with spaces>` backslash-escaped (`printf %q`) | **NO-RUN** — not a shell |
| `shell:/bin/sh -c '<escaped path>'` | NO-RUN |
| `"<path with spaces>"` | **RAN** |
| `'<path with spaces>'` | **RAN** |
| `"<path with an apostrophe>"` | RAN |
| `'<path with a double quote>'` | RAN |
| `"<path with a double quote>"` | NO-RUN (expected) |
| symlink from a space-free path into a spaced one | RAN |

So the field is a **quote-aware whitespace tokenizer feeding execvp** — not a shell.
AppleScript's `quoted form` is the wrong tool: it emits shell-style `'\''` escaping
that this tokenizer never interprets.

**Rule:**

1. Wrap the exe path in `"` by default.
2. If the path contains `"`, wrap in `'` instead.
3. If it contains both `"` and `'`, symlink it to a space-free path under
   `$TMPDIR` and use the symlink. Verified to work.

Arguments after the quoted path are tokenized normally, so
`"<exe>" __canopy_tree` is correct and the subcommand does not need to move into
the environment.

Root and PIDs still travel through `environment variables` — that decision stands on
its own merits (typed list, no tokenizer involvement at all).

---

## D5 — Bubble Tea v2 API notes

**Recorded.** Verified against `charm.land/bubbletea/v2` in the module cache, and
confirmed by a compiling probe in `smoke/fps/`:

- `tea.WithAltScreen()` does not exist. Altscreen is `tea.View.AltScreen`, set inside
  `View()`.
- `Model.Init()` returns `Cmd`, not `(Model, Cmd)`.
- `Msg` is an alias for `uv.Event`. Key messages are `tea.KeyPressMsg`.
- `p.startRenderer()` is called unguarded at `tea.go:1107` and `:1356`; it starts an
  fps ticker with no idle-stop path.
- `tea.WithoutRenderer()` does not guard the ticker, and `tty.go:25` returns early
  when it is set, so `initInput`/`term.MakeRaw` never run and key input dies.

---

## D6 — renderer fps

**OPEN.** Resolved by `smoke/fps/`. See its README for the decision rule. This is a
dependency fork: if no fps meets the 100 ms key-to-paint budget at an acceptable idle
wakeup rate, Bubble Tea is replaced by `x/term` + `ansi`.

| fps | worst key→View | View calls/sec idle | idle %CPU |
|-----|----------------|---------------------|-----------|
| 60  |                |                     |           |
| 10  |                |                     |           |
| 1   |                |                     |           |

---

## D7 — Step 0 smoke results

**CLOSED** (except OQ3, which is D6). Measured on Ghostty 1.3.1, macOS, 2026-08-13.

| OQ | Question | Answer |
|----|----------|--------|
| 1 | `surface configuration`: record literal or `new surface configuration`? | **Both work.** Use the record literal — one Apple Event instead of five, and no mutable object to leak. |
| 2 | `environment variables`: merge or replace? | **MERGE.** The pane inherits `TERM=xterm-ghostty`, `TERMINFO=/Applications/Ghostty.app/Contents/Resources/terminfo`, `PATH`, `HOME`, `LANG`, `USER`, `SHELL` — 31 vars — *and* the injected ones. **No need to pass terminal vars explicitly.** This was the failure that would have been invisible (pane closes before you can read the error). |
| 4 | Command field quoting | **See D4 — the original answer was wrong three ways.** Double quotes, not `shell:`/`direct:`/`quoted form`. |
| 5 | Does a split surface self-close on command exit? | **YES.** Terminal count 1 → 2 → 1 with `wait after command: false`. **P3's whole no-supervisor cleanup design is confirmed.** Sibling rebalance is a manual visual check. |
| 6 | Can `close` address a terminal by id? | **YES, directly.** `first terminal of front window whose id is X` resolves. No iteration needed. |
| P5 | Does the 200 ms focus poll verify reliably? | **YES**, on both config variants, every run. The bounded poll is cheap and it works. |

Harness: `smoke/run.sh` (main), `smoke/cmdform.sh` + `smoke/cmdform2.sh` (OQ4
isolation), `smoke/finish.sh` (OQ4c, OQ2, OQ5).

---

## D8 — watch cap derived from RLIMIT_NOFILE

**Decided.** `launchctl limit maxfiles` soft is 256 on this machine, while an
interactive shell reports 1048576. The tree pane is spawned by Ghostty — a
launchd-launched GUI app — as a direct command with no shell in between, so it
inherits the launchd limit, not the shell's.

A hard-coded 512-fd watch cap would hit `EMFILE` long before being reached. The tree
calls `unix.Getrlimit(RLIMIT_NOFILE)` at startup, raises soft toward hard where
permitted, and sets `watchCap = min(512, (soft-32)/2)`. The expanded-node budget is
clamped to `watchCap`.

---

## D9 — PIVOT: host the PTY, do not automate the terminal

**Decided 2026-08-13.** Supersedes the terminal-automation architecture. New design
doc: `~/.gstack/projects/canopy/duncan-main-design-20260813-093000.md`.

Canopy spawns Claude in a PTY and owns the screen, rather than asking a terminal
emulator for a companion pane. Triggered by three things:

1. Step 0 measured the coupling cost (see D4, D7) and found an unfixable flaw:
   Ghostty exports no surface id, so Canopy could never identify its own pane.
2. The requirement changed to "highlight files as Claude works on them," which needs
   an activity channel, not just a pane.
3. `github.com/jsleemaster/cltree` proved the PTY-host approach works — 3,355 LOC
   Rust, of which 2,011 is the host (`vterm.rs` 1,196 + `terminal.rs` 815).

**Consequences.** DELETED: the Companion interface, PaneID, Detect, Close, both
AppleScript files, TCC handling, focus verification, the parent-PID NOTE_EXIT watch
and its start-time guard, and the launcher package. Therefore **D2 and D4 are void**,
and D7's OQ1/OQ4/OQ5/OQ6 answers are now historical — accurate, but about a design we
are not building. D1, D3, D5, D8 all still apply.

GAINED: every terminal including Terminal.app and Alacritty, Linux support nearly
free, no permission model, and no pane-identification problem.

COST: ~2,000 lines of terminal emulation, and loss of native scrollback.

---

## D10 — Bubble Tea comes out; drive ultraviolet directly

**Decided.** Bubble Tea's `View()` returns a string. Compositing an embedded terminal
grid needs cell-level control, and flattening styled cells to ANSI and reparsing them
is lossy and wasteful on the hot path of every token Claude renders.

`github.com/charmbracelet/ultraviolet` — which Bubble Tea itself renders through —
exposes `Buffer`, `Cell`, `ScreenBuffer`, and a damage-tracked renderer
(`terminal_renderer_hashmap.go`, `terminal_renderer_hardscroll.go`) directly. It fills
the same role ratatui does for cltree.

Side effect: **D6 is void.** The unconditional-fps-ticker problem was a Bubble Tea
artifact and disappears with it. There is no ticker in this design.

---

## D11 — Go, not Rust. `vte` is only a tokenizer.

**Decided, from reading cltree's source.** The apparent Rust advantage was parser
maturity, and it does not survive inspection: `vte` supplies only the escape-sequence
state machine (`print`, `execute`, `csi_dispatch`, `osc_dispatch`, `esc_dispatch`),
while cltree hand-writes the entire ~1,200-line screen model on top of it.

Go's equivalent tokenizer is `github.com/charmbracelet/x/ansi` (`parser.go`,
`parser_csi`, `parser_dcs`, `parser_osc`, `parser_esc`, `parser_handler`), already a
transitive dependency here — v0.11.6 and v0.11.7 are in the module cache.

Performance was never the deciding factor. Peak throughput is a few MB/s; both
languages parse at hundreds of MB/s, and a preallocated cell grid means Go's GC has
nothing to collect on the hot path.

---

## D12 — the transcript is the activity channel

**Decided.** `~/.claude/projects/<mangled-cwd>/<sessionId>.jsonl`, verified live:

```json
{"tool":"Edit","file":"/Users/duncan/Developer/canopy/DECISIONS.md"}
```

Every tool use is appended with tool name, `file_path`, timestamp, `sessionId`, and
`cwd`. Zero configuration.

Rejected alternatives: **OSC 7** gives cwd only and cannot answer "which file" — it is
all cltree has, which is why cltree does not do this. **Hooks** are more robust but
require mutating `settings.json`. **Screen-scraping** Claude's TUI output is fragile
and was banned by the original spec for good reason; that ban still holds.

Mandatory: defensive parsing. The format is undocumented and can change between Claude
Code versions. Unknown shape ⇒ skip the line, degrade to a passive tree, never crash,
never render wrong data.
