# Horizontal scrollbar for the file tree; terminal-pane scrollbar

**Date:** 2026-08-17
**Status:** Approved (approach A, extended)

## Problem

Long paths in the tree pane are truncated with a `…` marker. Once a name is
cut off there is no way to read the rest of it short of widening the pane.
The tree needs a horizontal scrollbar, matching the mouse-driven vertical
scrollbar added in `037762e`/`114268d`.

Additionally (added during planning): scrollbars should be mouse-usable on
*either* pane. The terminal pane has wheel-driven scrollback but no drawn
scrollbar at all, so there is nothing to grab. It gets one — see
"Terminal-pane scrollbar" below. Horizontal scrolling does not apply to the
terminal pane: the PTY is resized to the pane width, so its content never
overflows horizontally.

## Approach

Whole-pane horizontal shift with an overlay scrollbar (approach A):

- The entire visible tree shifts left by `h_offset` columns; every row moves
  together.
- When the widest line exceeds the pane width, a scrollbar is painted across
  the bottom row of the pane, overlaying it — the same convention the
  vertical bar uses for the last column. No layout row is reserved, so none
  of the existing height-based geometry (vertical thumb, paging,
  `node_at_row`, auto-scroll margins) changes.
- All scrollbar geometry lives on `FileTree`, mirroring the vertical trio,
  so the renderer and the mouse handler share one source of truth.

Rejected alternatives: reserving the bottom row (approach B) reintroduces
the duplicated-geometry drift the vertical scrollbar work just eliminated;
per-line hover scrolling (approach C) is surprising and doesn't help read
the tree as a whole.

## State (`src/tree/mod.rs`)

New fields on `FileTree`:

- `h_offset: usize` — columns scrolled right, 0 = flush left.
- `content_width: usize` — widest line in columns, cached.

`content_width` is recomputed wherever the node list changes (`set_nodes`,
and the same clamp point `clamp_offset` uses). A line's width is
`depth * 2 + unicode_width(icon) + unicode_width(name)`. The transient
2-column `● ` CWD marker is deliberately excluded; if the CWD line is the
widest, the existing right-edge `…` marker covers the last 2 columns at
full scroll. `h_offset` is clamped against `content_width` at the same
recompute point, so a fold that narrows the tree can't leave the view
scrolled past the content.

New methods, mirroring the vertical set and parameterized by the visible
track width:

- `hscrollbar_thumb(visible_width) -> Option<(pos, len)>` — `None` when
  `content_width <= visible_width` (no bar drawn) or width is 0. The track
  is `pane_width - 1` columns: the bottom-right corner cell stays with the
  vertical bar.
- `scroll_to_hthumb_col(col, visible_width)` — set `h_offset` so the
  thumb's left edge lands on `col`, clamped. Used for drags.
- `hpage(down: bool, visible_width)` — page left/right by one viewport
  width, for clicks on the track either side of the thumb.
- `h_offset()` / `set_h_offset(n)` — like the vertical `set_offset`,
  clamping against `content_width.saturating_sub(visible_width)` happens at
  the call sites, which know the pane width; `set_nodes` additionally clamps
  against `content_width` so a stale offset can't outlive a shrink.

## Rendering (`src/ui/file_tree_widget.rs`)

Each row is built from the same segments as today (ancestor connectors,
branch, icon + name), but painting skips the first `h_offset` columns of
the logical line: each segment clips against the shifted origin instead of
starting at `area.x`. Concretely, track a logical column cursor; a segment
whose span ends before `h_offset` is skipped, a segment straddling it is
sliced by display width, and everything after paints as now.

- The right-edge `…` truncation marker keeps working unchanged (it keys off
  the painted width, which is now the post-shift width).
- No left-edge marker: shifted connectors make the scroll state obvious.
- After the rows, if `hscrollbar_thumb(track_width)` is `Some`, paint the
  bar across the bottom row (`area.height - 1`) from `area.x` to
  `area.x + area.width - 2`, `█` on the thumb and `░` on the track, same
  colors as the vertical bar. The corner cell at
  `(area.x + area.width - 1, bottom)` is left to the vertical bar.

## Mouse (`src/app.rs`)

Alongside `scrollbar_grab`, a new `hscrollbar_grab: Option<usize>` holding
the grab offset within the thumb.

- **Mouse down** on the tree pane's bottom row while the horizontal bar is
  visible: on the thumb → set `hscrollbar_grab` to the within-thumb offset;
  on the track → `hpage` toward the click. Either way it is *not* treated
  as a node click.
- **Drag** with `hscrollbar_grab` set → `scroll_to_hthumb_col(col - grab)`.
- **Mouse up** clears `hscrollbar_grab`.
- **`ScrollLeft` / `ScrollRight`** wheel events over the tree pane adjust
  `h_offset` by 3 columns, clamped — symmetric with the vertical wheel's
  3-row step.

## Shared geometry helper

The vertical tree scrollbar, the new horizontal tree scrollbar, and the new
terminal scrollbar all need the same thumb arithmetic. Rather than a third
copy, the math moves to a free module `src/scrollbar.rs` (declared in both
`lib.rs` and `main.rs`, since `tree` and `vterm` live in both crate roots):

- `thumb(total, visible, offset) -> Option<(pos, len)>` — `None` when
  `visible == 0` or `total <= visible`; thumb length
  `(visible² / total).max(1)`; position proportional to
  `offset / (total - visible)` over the travel, clamped.
- `offset_for_thumb_pos(pos, total, visible) -> usize` — the inverse, for
  drags.

`FileTree::scrollbar_thumb` / `scroll_to_thumb_row` delegate to these; the
existing `scrollbar_tests` pin that the delegation changes nothing.

## Terminal-pane scrollbar

A vertical scrollbar for the terminal pane's scrollback, mouse-usable like
the tree's.

**Visibility:** drawn only on the primary screen while scrolled back
(`!in_alternate_screen() && scroll_offset() > 0`). Claude Code runs on the
alt screen, where the wheel is already translated to arrow keys and there
is no scrollback to scroll — no bar there, ever. At the live bottom
(`scroll_offset == 0`) the bar disappears rather than permanently covering
the pane's last column; wheel up to enter scrollback and it appears.

**Geometry** lives on `VirtualTerminal`, next to the scroll state it reads:

- `scrollbar_thumb(visible_height) -> Option<(pos, len)>` — `None` when
  hidden per the rule above; otherwise `thumb(total, visible, offset_from_top)`
  where `total = scrollback.len() + grid.len()` and
  `offset_from_top = total - visible - scroll_offset` (saturating).
- `scroll_to_thumb_row(row, visible_height)` — inverts via
  `offset_for_thumb_pos` and stores the result back as a bottom-relative
  `scroll_offset`, clamped by the existing `set_scroll_offset`.

**Rendering** (`terminal_widget.rs`): same glyphs and colors as the tree's
bar (`█` thumb, `░` track), overlaying the last column of the pane.

**Mouse** (`app.rs`): a `terminal_scrollbar_grab: Option<usize>` mirroring
the tree's. Mouse-down on the last column while the bar is visible grabs
the thumb or pages toward the click, and does *not* set the selection
anchor; drag moves the thumb; mouse-up clears the grab. Everything else
(selection, wheel) is unchanged.

## Error handling

All arithmetic is saturating/clamped as in the vertical code; a pane of
width 0 or 1 draws no bar (`hscrollbar_thumb` returns `None`). Unicode
names slice by display width, never by byte index.

## Testing

- `hscrollbar_tests` in `src/tree/mod.rs`, mirroring `scrollbar_tests`:
  no thumb when everything fits or width is 0; thumb stays inside the
  track at every offset; `scroll_to_hthumb_col` round-trips a drag;
  clamping after a fold that narrows the tree.
- Render tests in `file_tree_widget.rs`: a shifted row starts mid-name
  (correct slice, counted in columns not bytes); nothing paints past the
  pane; the bar appears only on overflow and leaves the corner cell alone.
- Mouse test: a click on the bottom row with the bar visible does not
  select a node.
- `src/scrollbar.rs` unit tests: no thumb when content fits; thumb inside
  the track at every offset; `offset_for_thumb_pos` round-trips `thumb`.
- `vterm` tests: no thumb on the alt screen; no thumb at the live bottom;
  thumb appears when scrolled back; `scroll_to_thumb_row` at the track top
  reaches the oldest line and at the bottom returns to live.
