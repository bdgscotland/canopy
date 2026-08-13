# OQ3 — renderer configuration probe

Answers: **what is the lowest Bubble Tea `fps` that keeps key-to-paint under 100 ms?**

If no fps satisfies both that budget and an acceptable idle wakeup rate, the design
drops Bubble Tea for direct `golang.org/x/term` + `github.com/charmbracelet/x/ansi`
(~150 lines for a tree with no text input). That is a dependency fork, which is why
this runs in Step 0 rather than Slice 1.

## Why this is a real question

Verified against `charm.land/bubbletea/v2` in the module cache:

- `p.startRenderer()` is called at `tea.go:1107` and `tea.go:1356` with no guard.
- `startRenderer` starts `time.NewTicker(time.Second / p.fps)`, default 60, which
  stops only at program exit. There is no idle-stop path.
- `tea.WithoutRenderer()` is **not** an escape: it does not guard `startRenderer`, so
  the ticker fires identically — and `tty.go:25` returns early when it is set, so
  `initInput()` and `term.MakeRaw` never run and key input stops working.
- `tea.WithAltScreen()` does not exist in v2. Altscreen is `tea.View.AltScreen`, set
  per-render inside `View()`.

The ticker is a **flush** timer, not a render timer. `Update` runs immediately on every
message; the frame reaches the terminal on the next tick. So low fps costs latency, not
correctness — and the size of that cost is the empirical question.

## Run

```bash
cd smoke/fps

for f in 60 10 1; do
  echo "=== fps=$f ==="
  go run . -fps $f
  # In the TUI: mash keys for ~10s, watch "worst so far".
  # Then leave it idle ~30s and in ANOTHER pane run the ps line it prints.
  # q to quit, then move to the next fps.
done
```

## Record for each fps

| fps | worst key→View | View calls/sec idle | idle %CPU |
|-----|----------------|---------------------|-----------|
| 60  |                |                     |           |
| 10  |                |                     |           |
| 1   |                |                     |           |

Wakeup count with sudo, if you want the authoritative number rather than the
`View() calls/sec` proxy:

```bash
sudo powermetrics --samplers tasks --show-process-wakeups -n 1 -i 5000 \
  | grep -i canopy
```

## Decision rule

1. Pick the **lowest** fps whose worst key→View stays under 100 ms.
2. If even fps=60 exceeds 100 ms, something else is wrong — investigate before
   blaming the ticker.
3. If fps=1 meets the budget, take it: 1 wakeup/sec is close enough to the design's
   "idle filesystem = idle UI" constraint that the hand-rolled path is not worth its
   150 lines.
4. If only fps=60 meets the budget **and** 60 wakeups/sec measures above 0.1% CPU,
   drop Bubble Tea and hand-roll.

Write the answer, and the numbers behind it, into `DECISIONS.md`.
