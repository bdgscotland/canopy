# Contributing to Canopy

## Build and run

```bash
cargo build --release
./target/release/canopy          # runs Claude with the tree beside it
CANOPY_COMMAND=bash ./target/release/canopy   # host something else while hacking
```

## The gates

CI runs these, and so should you before pushing:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## What this project is careful about

Canopy hosts Claude Code in a PTY and redraws its output. Three requirements
follow from that, and most review attention goes to them.

**1. Claude must render exactly as it does without Canopy.** An escape sequence
modelled wrongly is visible corruption. Two such bugs have already shipped, both
in the same class: a CSI sequence carrying a private marker (`ESC[>4;2m`,
`ESC[<u`) was executed as the standard sequence sharing its final byte.

If you touch `src/vterm.rs`, assume you have introduced a fidelity bug and go
looking for it.

**2. Idle must cost nothing.** No polling, no periodic scanning, no timers that
run when nothing is happening. The watcher is event-driven and the transcript
poll is a `stat` against a stored offset. A regression here is easy to introduce
and invisible without measurement — `examples/watchcost` and `examples/treecost`
exist for that.

**3. Canopy must not degrade Claude.** Input is forwarded uninterpreted. Work
that blocks the event loop delays keystrokes and repaints; the tree walk used to
run before the PTY was even opened.

## Testing the terminal emulator

`src/vterm.rs` is a hand-written terminal emulator. Reasoning about it is not
enough — every bug found in it so far was found by measurement or by looking at
a screenshot, never by reading the code.

**Replay real captures.** `tests/fixtures/claude-startup.raw` is a real Claude
session captured through a PTY. Prefer adding a capture over hand-writing an
escape sequence you believe Claude emits.

**Assert the invariant, not the instance.** The four panic fixes are pinned by
tests that sweep every scroll region and every erase operation at degenerate
geometries, because each was one instance of "grid indices were validated
against `rows`/`cols` while nothing guaranteed the grid had those dimensions".

**Use an oracle.** `tmux` has been an emulator for fifteen years and
`tmux capture-pane -p -e` dumps its rendered grid with styling. Feed it the same
bytes and diff. Both sides need normalising first — tmux emits `ESC[0m` where
the source sent `ESC[22m` — so compare parsed cells, not text.

**A regression test must fail before the fix.** Verify that it does.

## Commit messages

Say what was wrong and how you know. A measurement, a quoted line, a spec
citation, or a reproduction — not an assertion. `git log` is the record of why
this code is shaped the way it is, and `DECISIONS.md` carries the decisions that
outlived their commits.

## Relationship to cltree

Canopy is a fork of [cltree](https://github.com/jsleemaster/cltree) (MIT).
Fixes to code inherited from cltree are worth offering upstream — the
`ESC[>4;2m` fix applies there unchanged.
