use std::path::Path;

use ratatui::{prelude::*, widgets::StatefulWidget};

use super::FileTreeWidgetState;
use crate::activity::ActivityKind;
use crate::tree::FileTree;

pub struct FileTreeWidget<'a> {
    tree: &'a FileTree,
    cwd: Option<&'a Path>,
    /// The file Claude last touched, and how.
    highlight: Option<(&'a Path, ActivityKind)>,
}

impl<'a> FileTreeWidget<'a> {
    pub fn new(tree: &'a FileTree, cwd: Option<&'a Path>) -> Self {
        Self {
            tree,
            cwd,
            highlight: None,
        }
    }

    pub fn highlight(mut self, highlight: Option<(&'a Path, ActivityKind)>) -> Self {
        self.highlight = highlight;
        self
    }
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
                Some(ActivityKind::Write) => Some(Color::Rgb(72, 52, 20)),
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
                    ActivityKind::Write => {
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

            let mut x_offset = area.x;

            if node.depth == 0 {
                // Root node: icon + name, no tree prefix
                let icon = node.expanded_icon(!self.tree.is_collapsed(&node.path));
                let display = if is_cwd {
                    format!("{}● {}", icon, node.name)
                } else {
                    format!("{}{}", icon, node.name)
                };
                let room = (area.x + area.width).saturating_sub(x_offset + 1) as usize;
                buf.set_stringn(x_offset, y, &display, room, node_style);
                x_offset += unicode_width::UnicodeWidthStr::width(display.as_str()) as u16;
            } else {
                // Draw ancestor connectors
                for &ancestor_is_last in &node.connector {
                    let connector_str = if ancestor_is_last { "  " } else { "│ " };
                    buf.set_string(x_offset, y, connector_str, tree_style);
                    x_offset += 2;
                }

                // Draw this node's branch connector
                let branch = if node.is_last { "└─" } else { "├─" };
                buf.set_string(x_offset, y, branch, tree_style);
                x_offset += 2;

                // Draw icon + name
                let icon = node.expanded_icon(!self.tree.is_collapsed(&node.path));
                let display = if is_cwd {
                    format!("{}● {}", icon, node.name)
                } else {
                    format!("{}{}", icon, node.name)
                };
                // set_string clips at the BUFFER's right edge, not this
                // widget's, so an overlong name used to paint over the tree's
                // own border. Reserve the last column for the scrollbar too.
                let room = (area.x + area.width).saturating_sub(x_offset + 1) as usize;
                buf.set_stringn(x_offset, y, &display, room, node_style);
                x_offset += unicode_width::UnicodeWidthStr::width(display.as_str()) as u16;
            }

            // Mark truncation one column in from the edge: the last column
            // belongs to the scrollbar, which is painted afterwards, so a
            // marker written there was invisible exactly when the tree was long
            // enough to need one.
            let total_width = x_offset.saturating_sub(area.x);
            if total_width > area.width.saturating_sub(1) {
                if let Some(x) = area.x.checked_add(area.width.saturating_sub(2)) {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_symbol("…");
                    }
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
