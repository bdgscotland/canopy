use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::{prelude::*, widgets::StatefulWidget};

use super::FileTreeWidgetState;
use crate::activity::{ActivityKind, Fade, FileAction};
use crate::tree::FileTree;

pub struct FileTreeWidget<'a> {
    tree: &'a FileTree,
    cwd: Option<&'a Path>,
    /// The file Claude last touched, and how.
    highlight: Option<(&'a Path, ActivityKind)>,
    /// Files touched recently and how, for the trailing glyphs. Fade is
    /// precomputed by the caller so this widget stays clock-free.
    recent: Option<&'a HashMap<PathBuf, (FileAction, Fade)>>,
}

impl<'a> FileTreeWidget<'a> {
    pub fn new(tree: &'a FileTree, cwd: Option<&'a Path>) -> Self {
        Self {
            tree,
            cwd,
            highlight: None,
            recent: None,
        }
    }

    pub fn highlight(mut self, highlight: Option<(&'a Path, ActivityKind)>) -> Self {
        self.highlight = highlight;
        self
    }

    pub fn recent(mut self, recent: &'a HashMap<PathBuf, (FileAction, Fade)>) -> Self {
        self.recent = Some(recent);
        self
    }
}

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

impl<'a> StatefulWidget for FileTreeWidget<'a> {
    type State = FileTreeWidgetState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let nodes = self.tree.nodes();
        let visible_height = area.height as usize;

        // Calculate visible range
        let start = state.offset;
        let end = (start + visible_height).min(nodes.len());

        for (i, idx) in (start..end).enumerate() {
            if idx >= nodes.len() {
                break;
            }

            let node = &nodes[idx];
            let y = area.y + i as u16;

            if y >= area.y + area.height {
                break;
            }

            // Check if this node is the CWD
            let is_cwd = self.cwd.is_some_and(|cwd| node.is_dir && node.path == cwd);

            // The file Claude is working on right now. A write is loud; a read
            // is quiet, because Claude reads far more than it writes.
            let active = self
                .highlight
                .and_then(|(p, kind)| (p == node.path).then_some(kind));

            let active_bg = match active {
                Some(ActivityKind::Write) | Some(ActivityKind::Edit) => Some(Color::Rgb(72, 52, 20)),
                Some(ActivityKind::Read) => Some(Color::Rgb(34, 42, 56)),
                None => None,
            };

            // Clear background for CWD or active item
            if let Some(bg) = active_bg {
                for x in area.x..area.x + area.width {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_bg(bg);
                    }
                }
            } else if is_cwd {
                for x in area.x..area.x + area.width {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_bg(Color::Rgb(80, 70, 30));
                    }
                }
            }

            let tree_style = Style::default().fg(Color::DarkGray);
            let node_style = if let (Some(kind), Some(bg)) = (active, active_bg) {
                match kind {
                    ActivityKind::Write | ActivityKind::Edit => {
                        Style::default().bg(bg).fg(Color::Rgb(255, 214, 120)).bold()
                    }
                    ActivityKind::Read => Style::default().bg(bg).fg(Color::Rgb(150, 190, 240)),
                }
            } else if is_cwd {
                Style::default()
                    .bg(Color::Rgb(80, 70, 30))
                    .fg(Color::Rgb(255, 220, 100))
                    .bold()
            } else {
                let color = node.display_color();
                let mut s = Style::default().fg(color);
                if node.is_dir {
                    s = s.bold();
                }
                s
            };

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

            // Trailing action glyph for recently-touched files. One more
            // segment, so shifting and truncation treat it like content.
            if let Some((action, fade)) = self.recent.and_then(|m| m.get(&node.path)) {
                let glyph = match action {
                    FileAction::Create => "+",
                    FileAction::Edit | FileAction::Overwrite => "✎",
                    FileAction::Read => "·",
                };
                let color = match (fade, action) {
                    (Fade::Dim, _) => Color::DarkGray,
                    (Fade::Bright, FileAction::Create) => Color::Green,
                    (Fade::Bright, FileAction::Edit | FileAction::Overwrite) => {
                        Color::Rgb(255, 214, 120)
                    }
                    (Fade::Bright, FileAction::Read) => Color::Rgb(150, 190, 240),
                };
                segments.push((format!(" {glyph}"), Style::default().fg(color)));
            }

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
        }

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

        // Scrollbar. Geometry comes from FileTree so the thumb is drawn
        // exactly where the mouse handler thinks it is -- the old version
        // computed its own position, which would have made a drag land
        // somewhere other than where the user grabbed.
        if let Some((thumb_pos, thumb_height)) = self.tree.scrollbar_thumb(visible_height) {
            let scrollbar_x = area.x + area.width - 1;
            for y in 0..visible_height {
                let on_thumb = y >= thumb_pos && y < thumb_pos + thumb_height;
                let ch = if on_thumb { "█" } else { "░" };
                if let Some(cell) = buf.cell_mut((scrollbar_x, area.y + y as u16)) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::FileTree;
    use crate::ui::FileTreeWidgetState;
    use ratatui::{buffer::Buffer, widgets::StatefulWidget};

    #[test]
    fn render_does_not_panic_when_area_width_is_zero() {
        let temp = tempfile::tempdir().expect("failed to create tempdir");
        std::fs::write(temp.path().join("seed.txt"), "x").expect("failed to create seed file");
        let tree = FileTree::new(temp.path(), false, 10).expect("failed to build tree");

        let widget = FileTreeWidget::new(&tree, None);
        let area = Rect::new(0, 0, 0, 1);
        let mut buf = Buffer::empty(area);
        let mut state = FileTreeWidgetState { offset: 0 };

        widget.render(area, &mut buf, &mut state);
    }
}

#[cfg(test)]
mod width_tests {
    use super::*;
    use crate::tree::FileTree;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn render_at(width: u16) -> Buffer {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/tree")).unwrap();
        std::fs::write(d.path().join("src/tree/file_node.rs"), "x").unwrap();
        let mut tree = FileTree::new(d.path(), false, 10).unwrap();
        tree.set_nodes(crate::tree::walk(d.path(), false, 10, tree.collapsed_set()));

        let area = Rect::new(0, 0, width, 10);
        let mut buf = Buffer::empty(Rect::new(0, 0, width + 4, 10));
        let mut state = FileTreeWidgetState { offset: 0 };
        FileTreeWidget::new(&tree, None).render(area, &mut buf, &mut state);
        buf
    }

    /// Chrome per level was four columns, so a depth-3 file spent 15 columns
    /// before its name started -- inside the 22-column pane a default
    /// `--tree-width 30` gives on an 80-column terminal. Two columns per level
    /// matches nvim-tree's default and this repo's own README diagram.
    #[test]
    fn indentation_leaves_room_for_the_filename() {
        let buf = render_at(24);
        let deep = (0..10)
            .map(|y| {
                (0..24)
                    .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                    .collect::<String>()
            })
            .find(|line| line.contains("file_node"))
            .expect("the deep file should render");

        // Count COLUMNS, not bytes: the connectors are multi-byte, so
        // str::find would report 14 for a name starting in column 9.
        let name_starts = deep
            .char_indices()
            .position(|(i, _)| deep[i..].starts_with("file_node"))
            .expect("name present");
        assert!(
            name_starts <= 8,
            "chrome ate {name_starts} columns before the name: {deep:?}"
        );
        assert!(
            deep.contains("file_node.rs"),
            "the name was truncated in a 24-column pane: {deep:?}"
        );
    }

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

    /// set_string clips at the BUFFER edge, not the widget's, so a long name
    /// used to paint over the tree's own border column.
    #[test]
    fn a_long_name_never_paints_past_the_pane() {
        let width = 14u16;
        let buf = render_at(width);
        for y in 0..10 {
            for x in width..width + 4 {
                let sym = buf.cell((x, y)).map_or(" ", |c| c.symbol());
                assert_eq!(sym, " ", "painted outside the pane at ({x},{y}): {sym:?}");
            }
        }
    }
}

#[cfg(test)]
mod glyph_tests {
    use super::*;
    use crate::activity::{Fade, FileAction};
    use crate::tree::FileTree;
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn render_with(
        recent: &HashMap<PathBuf, (FileAction, Fade)>,
        root_file: &str,
        width: u16,
    ) -> Vec<String> {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(root_file), "x").unwrap();
        let mut tree = FileTree::new(d.path(), false, 10).unwrap();
        tree.set_nodes(crate::tree::walk(d.path(), false, 10, tree.collapsed_set()));

        // The tempdir path varies per run; key the map on the real path.
        let mut keyed = HashMap::new();
        for (_, v) in recent {
            keyed.insert(d.path().join(root_file), *v);
        }

        let area = Rect::new(0, 0, width, 5);
        let mut buf = Buffer::empty(area);
        let mut state = FileTreeWidgetState { offset: 0 };
        FileTreeWidget::new(&tree, None)
            .recent(&keyed)
            .render(area, &mut buf, &mut state);
        (0..5)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_recently_edited_file_gets_its_glyph() {
        let mut recent = HashMap::new();
        recent.insert(PathBuf::new(), (FileAction::Edit, Fade::Bright));
        let rows = render_with(&recent, "touched.rs", 30);
        let row = rows.iter().find(|r| r.contains("touched.rs")).unwrap();
        assert!(row.contains("touched.rs ✎"), "glyph after the name: {row:?}");
    }

    #[test]
    fn create_and_read_have_their_own_glyphs() {
        for (action, glyph) in [(FileAction::Create, "+"), (FileAction::Read, "·")] {
            let mut recent = HashMap::new();
            recent.insert(PathBuf::new(), (action, Fade::Bright));
            let rows = render_with(&recent, "f.rs", 30);
            let row = rows.iter().find(|r| r.contains("f.rs")).unwrap();
            assert!(
                row.contains(&format!("f.rs {glyph}")),
                "{action:?} should show {glyph}: {row:?}"
            );
        }
    }

    #[test]
    fn untouched_files_get_no_glyph() {
        let recent = HashMap::new();
        let rows = render_with(&recent, "quiet.rs", 30);
        let row = rows.iter().find(|r| r.contains("quiet.rs")).unwrap();
        assert!(!row.contains('✎') && !row.contains('+'), "{row:?}");
    }

    /// The glyph is one more segment: it must clip at the pane edge like
    /// everything else, not paint past it.
    #[test]
    fn glyphs_respect_the_pane_edge() {
        let width = 10u16;
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a_much_longer_name.rs"), "x").unwrap();
        let mut tree = FileTree::new(d.path(), false, 10).unwrap();
        tree.set_nodes(crate::tree::walk(d.path(), false, 10, tree.collapsed_set()));
        let mut keyed = HashMap::new();
        keyed.insert(
            d.path().join("a_much_longer_name.rs"),
            (FileAction::Edit, Fade::Bright),
        );
        let area = Rect::new(0, 0, width, 5);
        let mut buf = Buffer::empty(Rect::new(0, 0, width + 4, 5));
        let mut state = FileTreeWidgetState { offset: 0 };
        FileTreeWidget::new(&tree, None)
            .recent(&keyed)
            .render(area, &mut buf, &mut state);
        for y in 0..5 {
            for x in width..width + 4 {
                assert_eq!(buf.cell((x, y)).map_or(" ", |c| c.symbol()), " ");
            }
        }
    }
}
