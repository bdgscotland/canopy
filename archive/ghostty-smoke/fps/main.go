// OQ3 — renderer configuration.
//
// Bubble Tea v2 calls startRenderer() unconditionally (tea.go:1107), which starts
// time.NewTicker(time.Second / fps) with no idle-stop path. The ticker is a FLUSH
// timer, not a render timer: Update runs immediately on every message, but the
// resulting frame is not written to the terminal until the next tick.
//
// So fps trades idle wakeups against worst-case key-to-paint latency, and the
// design needs the lowest fps that keeps key-to-paint under 100ms.
//
// tea.WithoutRenderer() is NOT a way out: it does not guard startRenderer (the
// ticker still fires) and tty.go:24 returns early when it is set, so initInput
// and term.MakeRaw never run and key input stops working entirely.
//
// Run: go run . -fps 60   (then 10, then 1)
// Press keys and watch the "since keypress" figure. Leave it idle to measure CPU.
package main

import (
	"flag"
	"fmt"
	"os"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
)

type model struct {
	fps        int
	presses    int
	lastKey    time.Time
	lastUpdate time.Duration // keypress -> Update
	lastView   time.Duration // keypress -> View
	worstView  time.Duration
	views      int
	started    time.Time
}

func (m model) Init() tea.Cmd { return nil }

func (m model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.KeyPressMsg:
		if msg.String() == "q" || msg.String() == "ctrl+c" {
			return m, tea.Quit
		}
		m.lastKey = time.Now()
		m.presses++
		m.lastUpdate = 0
		m.lastView = 0
	case tea.WindowSizeMsg:
		// redraw only
	}
	if !m.lastKey.IsZero() && m.lastUpdate == 0 {
		m.lastUpdate = time.Since(m.lastKey)
	}
	return m, nil
}

func (m model) View() tea.View {
	// View is called by the renderer. The gap between the keypress and THIS call
	// is the number that decides whether a given fps is usable.
	if !m.lastKey.IsZero() && m.lastView == 0 {
		m.lastView = time.Since(m.lastKey)
		if m.lastView > m.worstView {
			m.worstView = m.lastView
		}
	}
	m.views++

	var b strings.Builder
	fmt.Fprintf(&b, "  CANOPY — OQ3 renderer probe\n\n")
	fmt.Fprintf(&b, "  fps            %d\n", m.fps)
	fmt.Fprintf(&b, "  uptime         %s\n", time.Since(m.started).Truncate(time.Second))
	fmt.Fprintf(&b, "  keypresses     %d\n", m.presses)
	fmt.Fprintf(&b, "  View() calls   %d   (~%.1f/sec — this is the idle wakeup rate)\n",
		m.views, float64(m.views)/time.Since(m.started).Seconds())
	fmt.Fprintf(&b, "\n")
	fmt.Fprintf(&b, "  key -> Update  %s\n", m.lastUpdate.Truncate(time.Microsecond))
	fmt.Fprintf(&b, "  key -> View    %s   <- THE NUMBER THAT MATTERS\n",
		m.lastView.Truncate(time.Microsecond))
	fmt.Fprintf(&b, "  worst so far   %s   (budget: 100ms)\n",
		m.worstView.Truncate(time.Microsecond))
	fmt.Fprintf(&b, "\n  press keys to sample; leave idle and run:\n")
	fmt.Fprintf(&b, "    ps -o %%cpu= -p %d\n", os.Getpid())
	fmt.Fprintf(&b, "  q to quit\n")

	v := tea.NewView(b.String())
	v.AltScreen = true // v2: altscreen is a view field, NOT tea.WithAltScreen()
	return v
}

func main() {
	fps := flag.Int("fps", 60, "renderer fps (1, 10, or 60)")
	flag.Parse()

	m := model{fps: *fps, started: time.Now()}
	p := tea.NewProgram(m, tea.WithFPS(*fps))
	if _, err := p.Run(); err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}
