# Scrollbars Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A horizontal scrollbar for the file-tree pane, and a mouse-usable vertical scrollbar for the terminal pane's scrollback, both driven by the same shared thumb geometry.

**Architecture:** All thumb arithmetic moves to one free module `src/scrollbar.rs`, used by `FileTree` (vertical + new horizontal) and `VirtualTerminal` (new). Widgets render bars from that geometry; `src/app.rs` mouse handlers query the same geometry, so render and click can never disagree. Bars overlay content (tree: last column + bottom row; terminal: last column, only while scrolled back on the primary screen) — no layout rows/columns are reserved.

**Tech Stack:** Rust, ratatui, crossterm, unicode-width (all already dependencies — add nothing to Cargo.toml).

**Spec:** `docs/superpowers/specs/2026-08-17-horizontal-scrollbar-design.md` — read it first.

## Global Constraints

- No new dependencies.
- Commit messages are descriptive sentences in this repo's style (e.g. "Wheel scrolls Claude; click folds directories") — NOT conventional-commit prefixes. End every commit message with a blank line then `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Run `cargo test` (full suite) before every commit; it must pass.
- Comments in this codebase explain *why*, often at length, and never narrate the next line. Match that.
- `src/tree/mod.rs` and `src/vterm.rs` are compiled into BOTH crate roots (`src/lib.rs` and `src/main.rs`), so any module they reference must be declared in both.

---

### Task 1: Shared scrollbar geometry module

**Files:**
- Create: `src/scrollbar.rs`
- Modify: `src/lib.rs` (2 lines), `src/main.rs:1-8` (module list), `src/tree/mod.rs:179-217` (delegate)

**Interfaces:**
- Produces: `crate::scrollbar::thumb(total: usize, visible: usize, offset: usize) -> Option<(usize, usize)>` and `crate::scrollbar::offset_for_thumb_pos(pos: usize, total: usize, visible: usize) -> usize`. Tasks 2 and 5 call both.
- Consumes: nothing.

- [ ] **Step 1: Write the failing tests**

Create `src/scrollbar.rs` with the tests only (no implementation yet, so write the functions as `todo!()` stubs to get a compile):

```rust
//! Scrollbar thumb arithmetic, shared by every scrollbar in the app: the
//! tree's vertical and horizontal bars and the terminal's scrollback bar.
//!
//! One copy on purpose. The renderer and the mouse handler must agree on
//! where the thumb is, and independent copies of this arithmetic drift --
//! the tree's vertical bar had exactly that bug before the math was
//! centralized on FileTree, and this module is the same idea one level up.

/// Where the thumb sits, as (position, length) in track cells, or None when
/// everything fits and no scrollbar should be drawn.
pub fn thumb(total: usize, visible: usize, offset: usize) -> Option<(usize, usize)> {
    todo!()
}

/// The content offset that puts the thumb's leading edge on `pos` -- the
/// inverse of [`thumb`], for drags. Returns 0 when no scrollbar exists.
pub fn offset_for_thumb_pos(pos: usize, total: usize, visible: usize) -> usize {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_thumb_when_everything_fits() {
        assert!(thumb(10, 10, 0).is_none(), "exact fit needs no bar");
        assert!(thumb(5, 10, 0).is_none(), "less than a screenful");
        assert!(thumb(10, 0, 0).is_none(), "zero-size track");
    }

    #[test]
    fn the_thumb_stays_inside_the_track_at_every_offset() {
        let (total, visible) = (100, 12);
        for offset in 0..=(total - visible) {
            let (pos, len) = thumb(total, visible, offset).expect("thumb");
            assert!(len >= 1, "thumb must be grabbable");
            assert!(pos + len <= visible, "escaped the track at offset {offset}");
        }
        assert_eq!(thumb(total, visible, 0).unwrap().0, 0, "top at offset 0");
        let (pos, len) = thumb(total, visible, total - visible).unwrap();
        assert_eq!(pos + len, visible, "full scroll parks the thumb at the end");
    }

    #[test]
    fn an_offset_past_the_end_is_clamped_not_wrapped() {
        let (pos, len) = thumb(100, 12, 10_000).expect("thumb");
        assert_eq!(pos + len, 12, "overshoot clamps to the end of the track");
    }

    /// Endpoints must be exact; interior positions may quantize by one cell
    /// (integer division), exactly as the tree's vertical drag always has.
    #[test]
    fn offset_for_thumb_pos_inverts_thumb() {
        let (total, visible) = (97, 12);
        let (_, len) = thumb(total, visible, 0).unwrap();
        let travel = visible - len;
        assert_eq!(offset_for_thumb_pos(0, total, visible), 0);
        assert_eq!(
            offset_for_thumb_pos(travel, total, visible),
            total - visible,
            "dragging to the end reaches the last line"
        );
        for pos in 0..=travel {
            let offset = offset_for_thumb_pos(pos, total, visible);
            let (round_trip, _) = thumb(total, visible, offset).unwrap();
            assert!(
                round_trip <= pos && pos - round_trip <= 1,
                "drag to {pos} drew the thumb at {round_trip}"
            );
        }
    }

    #[test]
    fn no_scrollbar_means_offset_zero() {
        assert_eq!(offset_for_thumb_pos(3, 5, 10), 0);
        assert_eq!(offset_for_thumb_pos(3, 10, 0), 0);
    }
}
```

Declare the module in BOTH crate roots. In `src/lib.rs`:

```rust
pub mod scrollbar;
pub mod tree;
pub mod vterm;
```

In `src/main.rs`, add to the module list (keep alphabetical):

```rust
mod scrollbar;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test scrollbar`
Expected: FAIL — panics with "not yet implemented" (the `todo!()`s).

- [ ] **Step 3: Write the implementation**

Replace the two `todo!()` bodies:

```rust
pub fn thumb(total: usize, visible: usize, offset: usize) -> Option<(usize, usize)> {
    if visible == 0 || total <= visible {
        return None;
    }
    let len = ((visible * visible) / total).max(1);
    let max_offset = total - visible;
    let travel = visible - len;
    let pos = if max_offset == 0 {
        0
    } else {
        (offset.min(max_offset) * travel) / max_offset
    };
    Some((pos.min(travel), len))
}

pub fn offset_for_thumb_pos(pos: usize, total: usize, visible: usize) -> usize {
    let Some((_, len)) = thumb(total, visible, 0) else {
        return 0;
    };
    let max_offset = total - visible;
    let travel = visible - len;
    if travel == 0 {
        0
    } else {
        (pos.min(travel) * max_offset) / travel
    }
}
```

This is the tree's existing arithmetic (`src/tree/mod.rs:185-217`) verbatim, generalized over its inputs.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test scrollbar`
Expected: PASS (5 tests).

- [ ] **Step 5: Delegate FileTree's vertical geometry to the shared module**

In `src/tree/mod.rs`, replace the bodies of `scrollbar_thumb` and `scroll_to_thumb_row` (keep their doc comments, and keep `page` as is):

```rust
    pub fn scrollbar_thumb(&self, visible_height: usize) -> Option<(usize, usize)> {
        crate::scrollbar::thumb(self.nodes.len(), visible_height, self.offset)
    }

    /// Scroll so the thumb's TOP lands on `row`, clamped. Used for a drag.
    pub fn scroll_to_thumb_row(&mut self, row: usize, visible_height: usize) {
        if self.scrollbar_thumb(visible_height).is_none() {
            return;
        }
        self.offset =
            crate::scrollbar::offset_for_thumb_pos(row, self.nodes.len(), visible_height);
    }
```

The existing `scrollbar_tests` module pins that this delegation changes nothing.

- [ ] **Step 6: Run the full suite**

Run: `cargo test`
Expected: PASS — in particular every test in `tree::scrollbar_tests` still passes unchanged. If one fails, the delegation is NOT behavior-preserving; fix `scrollbar.rs` to match the old arithmetic, never the test.

- [ ] **Step 7: Commit**

```bash
git add src/scrollbar.rs src/lib.rs src/main.rs src/tree/mod.rs
git commit -m "One copy of the scrollbar arithmetic, shared by render and mouse

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Horizontal scroll state and geometry on FileTree

**Files:**
- Modify: `src/tree/mod.rs` (struct at :23-39, `new` at :42, `set_nodes` at :258, new methods + new test module)

**Interfaces:**
- Consumes: `crate::scrollbar::{thumb, offset_for_thumb_pos}` from Task 1.
- Produces (Tasks 3 and 4 call these):
  - `FileTree::h_offset(&self) -> usize`
  - `FileTree::set_h_offset(&mut self, offset: usize)` — raw setter, call sites clamp (matching `set_offset`)
  - `FileTree::content_width(&self) -> usize`
  - `FileTree::hscrollbar_thumb(&self, visible_width: usize) -> Option<(usize, usize)>`
  - `FileTree::scroll_to_hthumb_col(&mut self, col: usize, visible_width: usize)`
  - `FileTree::hpage(&mut self, right: bool, visible_width: usize)`

- [ ] **Step 1: Write the failing tests**

Add at the bottom of `src/tree/mod.rs`, next to the existing `scrollbar_tests` module:

```rust
#[cfg(test)]
mod hscrollbar_tests {
    use super::*;

    /// A tree whose widest line is `deep/a_really_quite_long_file_name.rs`
    /// at depth 2: 2*2 connector columns + 2 icon columns + 32 name = 38.
    fn wide_tree() -> FileTree {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("deep")).unwrap();
        std::fs::write(d.path().join("deep/a_really_quite_long_file_name.rs"), "x").unwrap();
        let mut t = FileTree::new(d.path(), false, 10).unwrap();
        t.set_nodes(walk(d.path(), false, 10, t.collapsed_set()));
        t
    }

    #[test]
    fn content_width_is_the_widest_line_in_columns() {
        let t = wide_tree();
        assert_eq!(t.content_width(), 38, "2*depth + icon + name");
    }

    #[test]
    fn no_thumb_when_the_pane_is_wide_enough() {
        let t = wide_tree();
        assert!(t.hscrollbar_thumb(38).is_none());
        assert!(t.hscrollbar_thumb(80).is_none());
        assert!(t.hscrollbar_thumb(0).is_none());
    }

    #[test]
    fn the_thumb_stays_inside_the_track_at_every_offset() {
        let mut t = wide_tree();
        let visible = 20;
        for offset in 0..=(t.content_width() - visible) {
            t.set_h_offset(offset);
            let (pos, len) = t.hscrollbar_thumb(visible).expect("thumb");
            assert!(pos + len <= visible, "escaped the track at offset {offset}");
        }
    }

    #[test]
    fn a_drag_lands_where_the_thumb_is_drawn() {
        let mut t = wide_tree();
        let visible = 20;
        t.scroll_to_hthumb_col(visible, visible); // past the end: clamps
        assert_eq!(t.h_offset(), t.content_width() - visible);
        t.scroll_to_hthumb_col(0, visible);
        assert_eq!(t.h_offset(), 0);
    }

    #[test]
    fn hpage_moves_one_viewport_and_clamps() {
        let mut t = wide_tree();
        let visible = 20;
        t.hpage(true, visible);
        assert_eq!(t.h_offset(), t.content_width() - visible, "one page covers it");
        t.hpage(true, visible);
        assert_eq!(t.h_offset(), t.content_width() - visible, "clamped at the end");
        t.hpage(false, visible);
        assert_eq!(t.h_offset(), 0);
    }

    /// A fold that narrows the tree must not leave the view scrolled past
    /// the content -- the vertical axis clamps in set_nodes for the same
    /// reason.
    #[test]
    fn set_nodes_clamps_a_stale_h_offset() {
        let mut t = wide_tree();
        t.set_h_offset(30);
        t.set_nodes(Vec::new());
        assert_eq!(t.h_offset(), 0, "empty tree has nowhere to scroll");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test hscrollbar`
Expected: FAIL to compile — `content_width`, `set_h_offset` etc. don't exist yet.

- [ ] **Step 3: Implement the state and methods**

In the `FileTree` struct (`src/tree/mod.rs:23-39`), add two fields after `offset: usize,`:

```rust
    /// Columns the view is scrolled right. 0 = flush left.
    h_offset: usize,
    /// Widest line in the tree in display columns, cached by set_nodes so
    /// the horizontal scrollbar geometry never walks the node list.
    content_width: usize,
```

In `FileTree::new` (`:42`), initialize both after `offset: 0,`:

```rust
            h_offset: 0,
            content_width: 0,
```

Add a free function next to `walk` at the bottom of the impl-adjacent code:

```rust
/// Display width of a node's rendered line: indent, icon, name. The
/// connectors cost 2 columns per depth level and the icon is always 2
/// ("▾ ", "▸ ", "· "). The transient 2-column CWD marker is deliberately
/// excluded: it moves with the child's cwd, and the right-edge truncation
/// marker covers the rare frame where the CWD line is also the widest.
fn line_width(node: &FileNode) -> usize {
    node.depth * 2 + 2 + unicode_width::UnicodeWidthStr::width(node.name.as_str())
}
```

Replace `set_nodes` (`:258-262`):

```rust
    /// Replace the node list with one produced off-thread by [`walk`].
    pub fn set_nodes(&mut self, nodes: Vec<FileNode>) {
        self.nodes = nodes;
        let max_offset = self.nodes.len().saturating_sub(1);
        self.offset = self.offset.min(max_offset);
        self.content_width = self.nodes.iter().map(line_width).max().unwrap_or(0);
        // A fold that narrows the tree must not leave the view scrolled
        // past the content, same as the vertical clamp above.
        self.h_offset = self.h_offset.min(self.content_width);
    }
```

(Note: `set_nodes` clamps against `content_width` alone because it has no pane width; the per-frame geometry in `hscrollbar_thumb` clamps the drawn position regardless, and the mouse call sites clamp against `content_width - visible_width` — same division of labor as the vertical axis.)

Add the accessors and geometry methods next to the vertical ones (`scrollbar_thumb` etc.):

```rust
    pub fn h_offset(&self) -> usize {
        self.h_offset
    }

    pub fn set_h_offset(&mut self, offset: usize) {
        self.h_offset = offset;
    }

    pub fn content_width(&self) -> usize {
        self.content_width
    }

    /// Horizontal scrollbar thumb as (column, length) in track cells, or
    /// None when the tree fits. Shared by renderer and mouse handler for
    /// the same reason as the vertical one.
    pub fn hscrollbar_thumb(&self, visible_width: usize) -> Option<(usize, usize)> {
        crate::scrollbar::thumb(self.content_width, visible_width, self.h_offset)
    }

    /// Scroll so the thumb's LEFT edge lands on `col`, clamped. For drags.
    pub fn scroll_to_hthumb_col(&mut self, col: usize, visible_width: usize) {
        if self.hscrollbar_thumb(visible_width).is_none() {
            return;
        }
        self.h_offset =
            crate::scrollbar::offset_for_thumb_pos(col, self.content_width, visible_width);
    }

    /// Page left or right, for a click on the track either side of the thumb.
    pub fn hpage(&mut self, right: bool, visible_width: usize) {
        let max_offset = self.content_width.saturating_sub(visible_width);
        self.h_offset = if right {
            (self.h_offset + visible_width).min(max_offset)
        } else {
            self.h_offset.saturating_sub(visible_width)
        };
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test hscrollbar`
Expected: PASS (6 tests). If `content_width_is_the_widest_line_in_columns` fails, print the node list — the tempdir's root name is short (`.tmpXXXXXX` ≈ 12 columns) so the deep file must dominate at 38; a mismatch means `line_width` is wrong, not the test.

- [ ] **Step 5: Run the full suite and commit**

Run: `cargo test`
Expected: PASS.

```bash
git add src/tree/mod.rs
git commit -m "FileTree learns how wide it is: h_offset, content_width, thumb geometry

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: Render the tree shifted, with the horizontal bar

**Files:**
- Modify: `src/ui/file_tree_widget.rs` (the whole `render` body at :34-184, plus new helpers and tests)

**Interfaces:**
- Consumes: `FileTree::{h_offset, hscrollbar_thumb}` from Task 2.
- Produces: nothing new for later tasks (mouse geometry comes from FileTree, not the widget).

- [ ] **Step 1: Write the failing tests**

Add to the existing `width_tests` module in `src/ui/file_tree_widget.rs` (note `render_at` builds `src/tree/file_node.rs` in a tempdir; its deepest line is `│ └─· file_node.rs`, and the tempdir root name is short, so content width ≈ depth 3*2 + 2 + 12 = 20 columns):

```rust
    fn render_tree_at(width: u16, h_offset: usize) -> Buffer {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/tree")).unwrap();
        std::fs::write(d.path().join("src/tree/file_node.rs"), "x").unwrap();
        let mut tree = FileTree::new(d.path(), false, 10).unwrap();
        tree.set_nodes(crate::tree::walk(d.path(), false, 10, tree.collapsed_set()));
        tree.set_h_offset(h_offset);

        let area = Rect::new(0, 0, width, 10);
        let mut buf = Buffer::empty(Rect::new(0, 0, width + 4, 10));
        let mut state = FileTreeWidgetState { offset: 0 };
        FileTreeWidget::new(&tree, None).render(area, &mut buf, &mut state);
        buf
    }

    fn row_text(buf: &Buffer, width: u16, y: u16) -> String {
        (0..width)
            .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
            .collect()
    }

    /// Scrolling right by 8 removes the deep row's connectors (2 ancestor
    /// entries + branch = 6 columns) and icon (2), so the row starts
    /// directly at the name.
    #[test]
    fn a_shifted_row_starts_mid_line() {
        let buf = render_tree_at(12, 8);
        let found = (0..10).map(|y| row_text(&buf, 12, y)).any(|line| {
            line.starts_with("file_node")
        });
        assert!(found, "shift of 8 must land exactly on the deep file's name");
    }

    #[test]
    fn a_shifted_row_never_paints_past_the_pane() {
        let width = 12u16;
        let buf = render_tree_at(width, 8);
        for y in 0..10 {
            for x in width..width + 4 {
                let sym = buf.cell((x, y)).map_or(" ", |c| c.symbol());
                assert_eq!(sym, " ", "painted outside the pane at ({x},{y})");
            }
        }
    }

    /// Content is ~20 columns wide: a 12-column pane overflows (bar on the
    /// bottom row), a 40-column pane does not (no bar).
    #[test]
    fn the_bar_appears_only_on_overflow_and_spares_the_corner() {
        let narrow = render_tree_at(12, 0);
        let bottom = row_text(&narrow, 12, 9);
        assert!(
            bottom.contains('█') && bottom.contains('░'),
            "overflowing tree must draw the horizontal bar: {bottom:?}"
        );
        // The tree here is 5 nodes in 10 rows, so no vertical bar exists
        // and the corner cell (owned by the vertical bar) must stay empty:
        // the horizontal track stops one column short of it.
        let corner = narrow.cell((11, 9)).map_or(" ", |c| c.symbol());
        assert_eq!(
            corner, " ",
            "the corner cell belongs to the vertical bar, not the track"
        );

        let wide = render_tree_at(40, 0);
        let bottom = row_text(&wide, 40, 9);
        assert!(
            !bottom.contains('░'),
            "no horizontal bar when everything fits: {bottom:?}"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test width_tests`
Expected: the two new shifted tests FAIL (`set_h_offset` exists but the widget ignores it, so rows render unshifted), and the bar test FAILS (no bar is drawn yet). The two pre-existing tests still pass.

- [ ] **Step 3: Restructure render around shifted painting**

In `src/ui/file_tree_widget.rs`, add two helpers above the `impl StatefulWidget` block:

```rust
/// Drop the first `cols` display columns of `s`. Returns the remainder and
/// how far past `cols` the cut landed (1 when a wide character straddled
/// the boundary and was dropped whole rather than split).
fn skip_columns(s: &str, cols: usize) -> (&str, usize) {
    let mut seen = 0usize;
    for (i, ch) in s.char_indices() {
        if seen >= cols {
            return (&s[i..], seen - cols);
        }
        seen += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    ("", seen.saturating_sub(cols))
}

/// Paint `segments` on row `y` as one line shifted left by `skip` columns,
/// clipped to `room` columns starting at `x0`. Returns the line's total
/// UNSHIFTED width, so the caller can decide whether it was truncated.
fn paint_shifted(
    buf: &mut Buffer,
    x0: u16,
    y: u16,
    room: usize,
    skip: usize,
    segments: &[(String, Style)],
) -> usize {
    let mut col = 0usize;
    for (text, style) in segments {
        let w = unicode_width::UnicodeWidthStr::width(text.as_str());
        if col + w > skip {
            if col >= skip {
                let x = col - skip;
                if x < room {
                    buf.set_stringn(x0 + x as u16, y, text, room - x, *style);
                }
            } else {
                // This segment straddles the left edge: drop the hidden
                // prefix by display columns, never by bytes.
                let (rest, pad) = skip_columns(text, skip - col);
                if pad < room {
                    buf.set_stringn(x0 + pad as u16, y, rest, room - pad, *style);
                }
            }
        }
        col += w;
    }
    col
}
```

Then replace the per-node painting section of `render` (everything from `let mut x_offset = area.x;` at :110 through the truncation-marker block ending at :162) with:

```rust
            // Build the row as styled segments, then paint them shifted by
            // the horizontal scroll. Building first is what lets a shift
            // start mid-connector or mid-name without duplicating the
            // slicing logic per segment kind.
            let mut segments: Vec<(String, Style)> = Vec::new();
            if node.depth > 0 {
                let mut connectors = String::new();
                for &ancestor_is_last in &node.connector {
                    connectors.push_str(if ancestor_is_last { "  " } else { "│ " });
                }
                connectors.push_str(if node.is_last { "└─" } else { "├─" });
                segments.push((connectors, tree_style));
            }
            let icon = node.expanded_icon(!self.tree.is_collapsed(&node.path));
            let display = if is_cwd {
                format!("{}● {}", icon, node.name)
            } else {
                format!("{}{}", icon, node.name)
            };
            segments.push((display, node_style));

            // set_stringn clips at the BUFFER's right edge, not this
            // widget's, so an overlong name used to paint over the tree's
            // own border. Reserve the last column for the scrollbar too.
            let room = area.width.saturating_sub(1) as usize;
            let h_offset = self.tree.h_offset();
            let total_width = paint_shifted(buf, area.x, y, room, h_offset, &segments);

            // Mark truncation one column in from the edge: the last column
            // belongs to the scrollbar, which is painted afterwards, so a
            // marker written there was invisible exactly when the tree was
            // long enough to need one.
            if total_width.saturating_sub(h_offset) > room {
                if let Some(x) = area.x.checked_add(area.width.saturating_sub(2)) {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_symbol("…");
                    }
                }
            }
```

(The `tree_style` and `node_style` bindings above this block stay exactly as they are; only the painting moves. Delete the old root/non-root `if node.depth == 0` painting split — the segments handle both, since a depth-0 node simply contributes no connector segment.)

- [ ] **Step 4: Paint the horizontal bar**

After the per-node loop, immediately before the existing vertical-scrollbar block at :165, add:

```rust
        // Horizontal scrollbar along the bottom row, overlaying it -- the
        // same convention the vertical bar uses for the last column, and
        // for the same reason: reserving a layout row would mean every
        // height-based computation (thumb, paging, node_at_row, the
        // auto-scroll margins) had to agree on whether it exists. The
        // corner cell is left to the vertical bar.
        let track_width = area.width.saturating_sub(1) as usize;
        if let Some((thumb_pos, thumb_len)) = self.tree.hscrollbar_thumb(track_width) {
            let bar_y = area.y + area.height - 1;
            for x in 0..track_width {
                let on_thumb = x >= thumb_pos && x < thumb_pos + thumb_len;
                let ch = if on_thumb { "█" } else { "░" };
                if let Some(cell) = buf.cell_mut((area.x + x as u16, bar_y)) {
                    cell.set_symbol(ch);
                    cell.set_fg(if on_thumb {
                        Color::Gray
                    } else {
                        Color::DarkGray
                    });
                }
            }
        }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS, including the two pre-existing `width_tests` (`indentation_leaves_room_for_the_filename`, `a_long_name_never_paints_past_the_pane`) — they pin that unshifted rendering is unchanged.

- [ ] **Step 6: Commit**

```bash
git add src/ui/file_tree_widget.rs
git commit -m "The tree renders shifted, with a horizontal bar on overflow

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: Mouse drives the horizontal bar

**Files:**
- Modify: `src/app.rs` (struct fields near :45-49, ctor near :121-122, `handle_mouse` at :335-451)

**Interfaces:**
- Consumes: `FileTree::{h_offset, set_h_offset, content_width, hscrollbar_thumb, scroll_to_hthumb_col, hpage}` from Task 2.
- Produces: nothing for later tasks.

App needs a live PTY, so there are no App unit tests (see `loading_state_tests` for why); coverage is the FileTree tests from Task 2 plus the build. Keep each change minimal and mechanical.

- [ ] **Step 1: Add the grab field**

Next to `scrollbar_grab: Option<usize>,` at `src/app.rs:49`:

```rust
    /// Grab offset within the HORIZONTAL scrollbar thumb, same idea as
    /// scrollbar_grab one axis over.
    hscrollbar_grab: Option<usize>,
```

Initialize in the constructor next to `scrollbar_grab: None,`:

```rust
            hscrollbar_grab: None,
```

- [ ] **Step 2: Wheel events**

In `handle_mouse`'s `match event.kind`, after the `ScrollDown` arm, add:

```rust
            MouseEventKind::ScrollLeft => {
                if in_tree {
                    let h = self.tree.h_offset();
                    self.tree.set_h_offset(h.saturating_sub(3));
                }
            }
            MouseEventKind::ScrollRight => {
                if in_tree {
                    let visible = self
                        .tree_area
                        .map(|a| a.width.saturating_sub(1) as usize)
                        .unwrap_or(1);
                    let max = self.tree.content_width().saturating_sub(visible);
                    self.tree.set_h_offset((self.tree.h_offset() + 3).min(max));
                }
            }
```

(The visible width is `area.width - 1` everywhere in this task: the last column belongs to the vertical bar, so it is not part of the horizontal track or viewport.)

- [ ] **Step 3: Click routing on the bottom row**

In the `Down(Left)` arm's tree block (`:383-414`), extend the geometry probe and the match. After `let on_scrollbar = ...` add:

```rust
                        let track_width = area.width.saturating_sub(1) as usize;
                        let hthumb = self.tree.hscrollbar_thumb(track_width);
                        let on_hscrollbar = area.height > 0
                            && event.row == area.y + area.height - 1
                            && hthumb.is_some();
```

Then change the final `_ =>` arm of the existing `match (on_scrollbar, thumb)` so the bottom row is checked before the node click (the vertical arm stays first, which also settles the corner cell in its favor):

```rust
                            // Anywhere else: the horizontal bar if it is
                            // visible and this is its row, otherwise fold
                            // or unfold the row. A click on the bar must
                            // never fold the node hidden beneath it.
                            _ => {
                                if on_hscrollbar {
                                    let col = (event.column - area.x) as usize;
                                    if let Some((pos, len)) = hthumb {
                                        if col >= pos && col < pos + len {
                                            self.hscrollbar_grab = Some(col - pos);
                                        } else {
                                            self.tree.hpage(col > pos, track_width);
                                        }
                                    }
                                } else if let Some(path) =
                                    self.tree.node_at_row(row).map(|n| n.path.clone())
                                {
                                    if self.tree.toggle(&path) {
                                        // toggle() only records the fold; the
                                        // walk that applies it runs off-thread.
                                        self.request_refresh();
                                    }
                                }
                            }
```

- [ ] **Step 4: Drag and release**

In the `Drag(Left)` arm, after the existing vertical-grab block (`:418-425`) and before the selection handling, add:

```rust
                if let Some(grab) = self.hscrollbar_grab {
                    if let Some(area) = self.tree_area {
                        let col = event.column.saturating_sub(area.x) as usize;
                        let track_width = area.width.saturating_sub(1) as usize;
                        self.tree
                            .scroll_to_hthumb_col(col.saturating_sub(grab), track_width);
                    }
                    return;
                }
```

In the `Up(Left)` arm next to `self.scrollbar_grab = None;`:

```rust
                self.hscrollbar_grab = None;
```

- [ ] **Step 5: Build, test, try it**

Run: `cargo test`
Expected: PASS.

Run: `cargo build` then manually: `cargo run` in a directory with deep paths, narrow the tree, and confirm — wheel-right shifts the tree, the bottom bar drags, clicking its track pages, clicking the bar does NOT fold anything, and the `…` marker still appears on truncated rows.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs
git commit -m "Wheel and mouse drive the tree sideways

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Scrollback thumb geometry on VirtualTerminal

**Files:**
- Modify: `src/vterm.rs` (new methods near `set_scroll_offset` at :366, new tests in the `tests` module at :1336)

**Interfaces:**
- Consumes: `crate::scrollbar::{thumb, offset_for_thumb_pos}` from Task 1.
- Produces (Tasks 6 and 7 call these):
  - `VirtualTerminal::scrollbar_thumb(&self, visible_height: usize) -> Option<(usize, usize)>`
  - `VirtualTerminal::scroll_to_thumb_row(&mut self, row: usize, visible_height: usize)`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/vterm.rs` (the pattern for feeding content is `VirtualTerminal::new(cols, rows)` + `vt.feed(bytes)`; each `\r\n` past the bottom row pushes a line into scrollback; `\x1b[?1049h` enters the alt screen):

```rust
    fn scrolled_back_vterm() -> VirtualTerminal {
        let mut vt = VirtualTerminal::new(10, 5);
        for i in 0..50 {
            vt.feed(format!("line{i}\r\n").as_bytes());
        }
        assert!(vt.scrollback().len() >= 40, "history must have accumulated");
        vt
    }

    #[test]
    fn no_thumb_at_the_live_bottom() {
        let vt = scrolled_back_vterm();
        assert_eq!(vt.scroll_offset(), 0);
        assert!(
            vt.scrollbar_thumb(5).is_none(),
            "at the bottom the bar would permanently cover the child's UI"
        );
    }

    #[test]
    fn no_thumb_on_the_alt_screen() {
        let mut vt = scrolled_back_vterm();
        vt.set_scroll_offset(10);
        vt.feed(b"\x1b[?1049h");
        assert!(
            vt.scrollbar_thumb(5).is_none(),
            "alt screen has no scrollback by design"
        );
    }

    #[test]
    fn the_thumb_appears_when_scrolled_back_and_tracks_position() {
        let mut vt = scrolled_back_vterm();
        vt.set_scroll_offset(1);
        let (pos_low, len) = vt.scrollbar_thumb(5).expect("thumb while scrolled back");
        assert!(pos_low + len <= 5, "inside the track");

        vt.set_scroll_offset(usize::MAX); // clamps to the oldest line
        let (pos_high, _) = vt.scrollbar_thumb(5).expect("thumb at the top");
        assert_eq!(pos_high, 0, "oldest line puts the thumb at the top");
        assert!(pos_low > pos_high, "nearly-live sits below fully-scrolled");
    }

    #[test]
    fn a_drag_spans_the_whole_history() {
        let mut vt = scrolled_back_vterm();
        vt.set_scroll_offset(10);
        vt.scroll_to_thumb_row(0, 5);
        assert_eq!(
            vt.scroll_offset(),
            vt.scrollback().len(),
            "thumb at the track top shows the oldest line"
        );
        vt.scroll_to_thumb_row(5, 5);
        assert_eq!(vt.scroll_offset(), 0, "thumb at the bottom returns to live");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test vterm`
Expected: FAIL to compile — the two methods don't exist. (If `scrolled_back_vterm`'s history assertion fails instead, feed with explicit `\r\n` line endings as shown — a bare `\n` may only move the cursor.)

- [ ] **Step 3: Implement**

Add after `set_scroll_offset` at `src/vterm.rs:366-368`:

```rust
    /// Where the scrollback scrollbar thumb sits, or None when no bar
    /// should be drawn: on the alt screen (no scrollback by design -- the
    /// wheel is already translated to arrow keys there) and at the live
    /// bottom, where a permanent bar would cover the child's last column.
    /// Wheel up to enter scrollback and the bar appears.
    pub fn scrollbar_thumb(&self, visible_height: usize) -> Option<(usize, usize)> {
        if self.in_alternate_screen() || self.scroll_offset == 0 {
            return None;
        }
        let total = self.scrollback.len() + self.grid.len();
        // scroll_offset counts up from the bottom; the thumb from the top.
        let offset_from_top = total
            .saturating_sub(visible_height)
            .saturating_sub(self.scroll_offset);
        crate::scrollbar::thumb(total, visible_height, offset_from_top)
    }

    /// Scroll so the thumb's TOP lands on `row`, clamped. Used for a drag.
    ///
    /// Deliberately does NOT require the bar to be visible: a drag that
    /// reaches the live bottom sets scroll_offset to 0, which hides the
    /// bar, and the still-held drag must be able to pull back up.
    pub fn scroll_to_thumb_row(&mut self, row: usize, visible_height: usize) {
        if self.in_alternate_screen() {
            return;
        }
        let total = self.scrollback.len() + self.grid.len();
        if crate::scrollbar::thumb(total, visible_height, 0).is_none() {
            return;
        }
        let offset_from_top =
            crate::scrollbar::offset_for_thumb_pos(row, total, visible_height);
        let max_offset = total.saturating_sub(visible_height);
        self.set_scroll_offset(max_offset.saturating_sub(offset_from_top));
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test vterm`
Expected: PASS (the 4 new tests plus all pre-existing vterm tests).

- [ ] **Step 5: Full suite and commit**

Run: `cargo test`
Expected: PASS.

```bash
git add src/vterm.rs
git commit -m "The scrollback knows where its thumb is

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Render the terminal scrollbar

**Files:**
- Modify: `src/ui/terminal_widget.rs` (end of `render`)

**Interfaces:**
- Consumes: `VirtualTerminal::scrollbar_thumb` from Task 5.
- Produces: nothing.

- [ ] **Step 1: Paint the bar**

At the end of `TerminalWidget::render` in `src/ui/terminal_widget.rs`, while the `vterm` lock from :57 is still held, add:

```rust
        // Scrollback scrollbar, overlaying the last column -- but only
        // while scrolled back on the primary screen (the geometry method
        // returns None otherwise), so Claude Code's alt-screen UI is never
        // covered and the live view keeps its full width.
        if area.width > 0 {
            if let Some((thumb_pos, thumb_height)) =
                vterm.scrollbar_thumb(area.height as usize)
            {
                let x = area.x + area.width - 1;
                for y in 0..area.height as usize {
                    let on_thumb = y >= thumb_pos && y < thumb_pos + thumb_height;
                    let ch = if on_thumb { "█" } else { "░" };
                    if let Some(cell) = buf.cell_mut((x, area.y + y as u16)) {
                        cell.set_symbol(ch);
                        cell.set_fg(if on_thumb {
                            Color::Gray
                        } else {
                            Color::DarkGray
                        });
                    }
                }
            }
        }
```

(If `render` drops the `vterm` guard before its end, place this block before the drop. The glyphs and colors match the tree's bar exactly — `src/ui/file_tree_widget.rs:169-183`.)

Rendering `TerminalWidget` requires a live `TerminalPane` (a PTY and child process), so this task has no widget-level unit test; the visibility rule and geometry are pinned by Task 5's vterm tests, and Task 7 ends with a manual check of the drawn bar.

- [ ] **Step 2: Build, run the suite, commit**

Run: `cargo test`
Expected: PASS. Also run `cargo clippy` if the repo is clippy-clean; fix any new warnings in touched files.

```bash
git add src/ui/terminal_widget.rs
git commit -m "Draw the scrollback bar while scrolled back

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: Mouse drives the terminal scrollbar

**Files:**
- Modify: `src/app.rs` (struct fields, ctor, `handle_mouse`)

**Interfaces:**
- Consumes: `VirtualTerminal::{scrollbar_thumb, scroll_to_thumb_row, scroll_offset, set_scroll_offset}` from Task 5; `TerminalPane::vterm_lock` (`src/terminal.rs:685`).
- Produces: nothing.

- [ ] **Step 1: Add the grab field**

Next to the other grab fields in `App`:

```rust
    /// Grab offset within the TERMINAL pane's scrollbar thumb.
    terminal_scrollbar_grab: Option<usize>,
```

Initialize `terminal_scrollbar_grab: None,` in the constructor.

- [ ] **Step 2: Route the click**

Add a helper method to `App` (near `terminal_point`):

```rust
    /// A left-click on the terminal pane's scrollbar, if the bar is
    /// visible and the click is on its column. Returns true when handled,
    /// so the caller skips selection -- grabbing the bar must not start
    /// highlighting text underneath it.
    fn terminal_scrollbar_down(&mut self, event: MouseEvent) -> bool {
        let Some(area) = self.terminal_area else {
            return false;
        };
        if area.width == 0 || event.column != area.x + area.width - 1 {
            return false;
        }
        let visible = area.height as usize;
        let row = event.row.saturating_sub(area.y) as usize;
        let mut vt = self.terminal.vterm_lock();
        let Some((pos, height)) = vt.scrollbar_thumb(visible) else {
            return false;
        };
        if row >= pos && row < pos + height {
            drop(vt);
            self.terminal_scrollbar_grab = Some(row - pos);
        } else if row > pos {
            // Track below the thumb: page DOWN, toward live.
            let current = vt.scroll_offset();
            vt.set_scroll_offset(current.saturating_sub(visible));
        } else {
            // Track above the thumb: page UP, into history. set_scroll_offset
            // clamps to the history length.
            let current = vt.scroll_offset();
            vt.set_scroll_offset(current + visible);
        }
        true
    }
```

In the `Down(Left)` arm of `handle_mouse`, immediately after `self.selection = None;` at :373 and before the `drag_anchor` assignment:

```rust
                if in_terminal && self.terminal_scrollbar_down(event) {
                    self.drag_anchor = None;
                    return;
                }
```

- [ ] **Step 3: Drag and release**

In the `Drag(Left)` arm, next to the other two grab blocks (order among the three does not matter — at most one grab is ever set):

```rust
                if let Some(grab) = self.terminal_scrollbar_grab {
                    if let Some(area) = self.terminal_area {
                        let row = event.row.saturating_sub(area.y) as usize;
                        self.terminal
                            .vterm_lock()
                            .scroll_to_thumb_row(row.saturating_sub(grab), area.height as usize);
                    }
                    return;
                }
```

In the `Up(Left)` arm:

```rust
                self.terminal_scrollbar_grab = None;
```

- [ ] **Step 4: Full suite and manual check**

Run: `cargo test`
Expected: PASS.

Manual: `cargo run`, then in the child shell run something long (`ls -R /usr/share | head -2000`), quit any alt-screen app first so the primary screen shows. Wheel up: the bar appears on the pane's last column. Drag it: the view follows and no text selection appears. Drag to the bottom: the bar disappears and the view is live; drag back up without releasing: it returns. Click the track: it pages. Confirm the bar never appears while Claude Code (alt screen) is on screen.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "Grab the scrollback bar on the Claude pane

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```
