# Horizontal scrollbar for the file tree

**Date:** 2026-08-17
**Status:** Approved (approach A)

## Problem

Long paths in the tree pane are truncated with a `…` marker. Once a name is
cut off there is no way to read the rest of it short of widening the pane.
The tree needs a horizontal scrollbar, matching the mouse-driven vertical
scrollbar added in `037762e`/`114268d`.

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
