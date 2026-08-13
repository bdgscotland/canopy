# Canopy

A project tree that follows Claude Code as it works.

Run `canopy` instead of `claude`. You get Claude Code exactly as normal, with a
live file tree beside it that expands, scrolls, and highlights as Claude reads
and edits files. Instead of inferring where the agent is working from paths
scrolling past in the transcript, you see it.

```
┌──────────────────────────────────────┬──────────────────┐
│                                      │ ▾ internal       │
│                                      │   ● tree.go      │  ← Claude just edited
│           Claude Code                │     watch.go     │
│                                      │ ▾ cmd            │
│                                      │     main.go      │
└──────────────────────────────────────┴──────────────────┘
```

Works in any terminal on any platform, because Canopy hosts Claude in a PTY
rather than asking your terminal emulator for a split. No configuration, no
permissions, no per-terminal setup.

## Install

```bash
cargo install --path .
```

## Usage

```bash
canopy                    # same as: claude
canopy --resume           # every argument passes through to Claude
canopy --model opus
canopy -p /my/project     # tree rooted elsewhere
```

Canopy-specific flags are `-p/--path`, `--help`, and `--version`. Everything
else goes to Claude Code untouched.

## Status

Early. The PTY host, tree, scrollback, mouse selection, and clipboard all work.
Transcript-driven highlighting — the feature the whole thing exists for — is in
progress.

## Fidelity

Canopy draws Claude's output itself, so any escape sequence it models wrongly
shows up as visible corruption. That risk is managed with a golden-master test
suite: real Claude Code output is captured through a PTY, replayed through the
terminal emulator, and asserted against.

`tests/fixtures/claude-startup.raw` is one such capture. The
`test_replay_real_claude_startup` test replays it and fails if any cell picks up
styling it should not have — which is how the `ESC[>4;2m` bug below was caught
and pinned.

`tmux` can serve as an oracle for new fixtures: feed it the same bytes and diff
`tmux capture-pane -p -e` against Canopy's grid. Any divergence is a bug with a
reproducible input.

## Credits

Canopy is a fork of [cltree](https://github.com/jsleemaster/cltree) by
jsleemaster, MIT licensed. cltree solved the hard part — hosting Claude in a PTY
and compositing a tree beside it — and Canopy builds on that work.

Changes so far:

- **Fixed `ESC[>4;2m` being parsed as SGR.** That sequence is xterm's
  `modifyOtherKeys`; it shares the final byte `m` with SGR but carries a `>`
  intermediate. Claude Code emits it in the first 100 bytes of every session, so
  parsing it as SGR set underline(4) and dim(2) permanently — 520 wrongly styled
  cells on a plain startup screen. Only a bare `CSI ... m` is SGR.
- **Fixed SGR sub-parameter handling.** `4:0` (underline off) was read as `4`
  (underline on), and the colon form of extended colour (`38:2::R:G:B`) consumed
  following parameters that belonged to other attributes.
- Added golden-master replay testing against captured Claude output.

## License

MIT. See [LICENSE](LICENSE) — it retains the original cltree copyright.
