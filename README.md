# Canopy

A project tree that follows Claude Code as it works.

Run `canopy` instead of `claude`. You get Claude Code exactly as normal, with a
live file tree beside it that scrolls to and highlights each file as Claude
reads and edits it. Instead of reconstructing where the agent is working from
paths scrolling past in the transcript, you watch it move through your codebase.

```
┌────────────────────────────────────────┬────────────────────┐
│                                        │ ▾ src              │
│  ● Read src/vterm.rs                   │     activity.rs    │
│  ● Update src/vterm.rs                 │     app.rs         │
│                                        │   ● vterm.rs       │  ← just edited
│  I fixed the SGR parser — the arm      │     terminal.rs    │
│  wasn't checking intermediates.        │ ▾ tests            │
│                                        │     vterm_test.rs  │
│  > ▊                                   │   Cargo.toml       │
└────────────────────────────────────────┴────────────────────┘
```

Works in any terminal on any platform, because Canopy hosts Claude in a PTY
rather than asking your terminal emulator for a split. No configuration, no
permissions, no per-terminal setup.

## Install

```bash
cargo install --path .
```

Requires the `claude` CLI on your `PATH`.

## Usage

```bash
canopy                 # same as: claude
canopy --resume        # every argument passes through to Claude
canopy --model opus
canopy -p /my/project  # tree rooted elsewhere
```

`-p/--path`, `--help` and `--version` belong to Canopy. Everything else goes to
Claude Code untouched.

| Variable | Effect |
|---|---|
| `CANOPY_COMMAND` | run something other than `claude` |
| `CANOPY_SESSION_ID` | follow a specific session instead of the most recent |
| `CANOPY_TRANSCRIPT` | follow a specific transcript file |
| `CLAUDE_CONFIG_DIR` | honoured, as Claude Code honours it |

## How the tree follows Claude

Claude Code appends every tool use to
`~/.claude/projects/<mangled-cwd>/<session-id>.jsonl`, including the path for
file tools. Canopy tails that file from its current end and highlights what it
sees — writes bright, reads dim, since Claude reads far more than it writes.

No hooks, no `settings.json`, no MCP server, and no scraping of Claude's
rendered output.

The transcript format is undocumented and can change between Claude Code
releases, so every step fails soft: a malformed line is skipped, a missing
transcript just means no highlighting, and neither can take the tree down.

**Known gap:** only tools that name a file are visible — `Read`, `Write`,
`Edit`, `MultiEdit`, `NotebookEdit`. `Bash` records only its command string, so
a file changed by `sed -i` or a heredoc appears in the tree (the watcher sees
it) but is not highlighted.

## Fidelity

Canopy draws Claude's output itself, so an escape sequence modelled wrongly is
visible corruption. That risk is managed with tests rather than care.

`tests/fixtures/claude-startup.raw` is a real Claude session captured through a
PTY. `test_replay_real_claude_startup` replays it and fails if any cell picks up
styling it should not have — which is how the `ESC[>4;2m` bug below was caught.

`tmux` works as an oracle for new fixtures: feed it the same bytes and diff
`tmux capture-pane -p -e` against Canopy's grid. Any divergence is a bug with a
reproducible input.

## Performance

Measured on this machine, release builds.

| | |
|---|---|
| Idle CPU | ~0% — event-driven watching, no polling |
| Tree walk, 20,523 nodes | 41 ms |
| Transcript poll, idle | 1.2 µs, 8×/sec |
| Escape-sequence parse | ~480 MiB/s (Claude emits ~400 B/s) |
| Memory | ~8 MiB, almost all of it scrollback |

`examples/watchcost` and `examples/treecost` measure the watcher and the tree
walk directly.

## Credits

Canopy is a fork of [cltree](https://github.com/jsleemaster/cltree) by
jsleemaster, MIT licensed. cltree solved the hard part — hosting Claude in a PTY
and compositing a tree beside it — and Canopy builds on that work.

### Changes since the fork

**Transcript following.** The reason Canopy exists; cltree tracks only the
working directory, via OSC 7.

**Terminal fidelity.**
- `ESC[>4;2m` (xterm `modifyOtherKeys`) was dispatched to the SGR parser, which
  read it as underline + dim and applied both to every cell for the rest of the
  session — 520 wrongly styled cells on a plain startup screen. Only 3 of 24 CSI
  arms checked intermediates, and `esc_dispatch` discarded them entirely, so
  `ESC[<u` and `ESC[>1u` ran as `DECRC` and `ESC#8` restored the cursor. The
  precondition is now stated once per dispatcher.
- SGR sub-parameters were dropped: `4:0` (underline off) read as `4` (on).
- `SGR 37` mapped to bright white, making normal and bright white identical.
- `DA1`, `DA2` and `XTVERSION` went unanswered; Claude probes for all three.

**Crashes.** Four reproduced panics, each on the PTY reader thread, each of
which quit Canopy and took the Claude session with it: a `DECSTBM` clamp
off-by-one, `ED` at zero rows, `EL` at zero columns, and a `char`-boundary slice
in OSC 7 percent-decoding. Plus a main-thread panic when selecting in the
scrollback after widening the window.

**Input.** Ctrl+`<non-letter>` was re-encoded to bytes that are never valid
UTF-8 — Ctrl+Space became `0xC0`, Ctrl+/ became `0xD7` — so Claude's stdin
decoder dropped them. Alt+Enter submitted the prompt instead of inserting a
newline.

**CPU.** The file watcher polled, stat-ing every file under the root every
75 ms: 14.6% of a core idle on 1,353 files, pegging a core on larger trees. A
closed PTY channel spun the event loop at 41.9M iterations in 5 s and left
Canopy unquittable.

**Startup.** The tree walk ran before the PTY was opened, so Claude did not
start until it finished. The walk itself built one directory walker per
directory and re-`stat`ed entries the walker had already described.

## License

MIT. See [LICENSE](LICENSE) — it retains the original cltree copyright.
