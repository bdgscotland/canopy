mod activity_pane;
mod file_tree_widget;
mod terminal_widget;

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};
use std::collections::HashMap;

use crate::activity::{Fade, GLYPH_BRIGHT, GLYPH_EXPIRY, NOW_STALE};
use crate::app::App;
use crate::tasks::TaskStatus;
use activity_pane::{content_height, ActivityPaneWidget};
use file_tree_widget::FileTreeWidget;
use terminal_widget::TerminalWidget;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let size = frame.area();

    // Main layout: tree on right, terminal on left
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(100 - app.tree_width_percent),
            Constraint::Percentage(app.tree_width_percent),
        ])
        .split(size);

    // Terminal pane (left/main area)
    let terminal_area = chunks[0];
    // The breaker's voice. If input is queued and not draining, say so —
    // otherwise the user types, nothing happens, and there is no explanation.
    // Also surfaces a refused write (an oversized paste, or a full queue).
    let stalled = app
        .terminal
        .write_stalled_for()
        .filter(|d| *d >= crate::ptywrite::STALL_NOTICE);
    let (terminal_title, terminal_colour) = if app.reader_failed_notice {
        (
            " Claude Code — terminal feed lost, session still running ".to_string(),
            Color::Red,
        )
    } else if let Some(d) = stalled {
        (
            format!(
                " Claude Code — not reading input, {:.1} KiB queued ({}s) ",
                app.terminal.queued_input_bytes() as f64 / 1024.0,
                d.as_secs()
            ),
            Color::Yellow,
        )
    } else if let Some(ref msg) = app.terminal.last_refusal {
        (format!(" Claude Code — {msg} "), Color::Yellow)
    } else {
        (" Claude Code ".to_string(), Color::Cyan)
    };

    let terminal_block = Block::default()
        .title(terminal_title)
        .title_style(Style::default().fg(terminal_colour).bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(terminal_colour));

    let terminal_inner = terminal_block.inner(terminal_area);
    frame.render_widget(terminal_block, terminal_area);

    // Resize PTY to match terminal area
    app.terminal
        .resize(terminal_inner.width, terminal_inner.height);

    // Store terminal area for mouse drag routing
    app.terminal_area = Some(terminal_inner);

    let terminal_widget = TerminalWidget::new(&app.terminal, app.selection.as_ref());
    frame.render_widget(terminal_widget, terminal_inner);

    // Set hardware blinking cursor position (terminal always focused)
    {
        let vterm = app.terminal.vterm_lock();
        let cursor = vterm.cursor();
        if cursor.visible {
            let cx =
                terminal_inner.x + (cursor.x as u16).min(terminal_inner.width.saturating_sub(1));
            let cy =
                terminal_inner.y + (cursor.y as u16).min(terminal_inner.height.saturating_sub(1));
            if cx < terminal_inner.x + terminal_inner.width
                && cy < terminal_inner.y + terminal_inner.height
            {
                frame.set_cursor_position((cx, cy));
            }
        }
    }

    // Right column: tree above, activity pane below. The pane exists only
    // when it has something to say -- an empty bordered box under the tree
    // would be pure noise -- so an idle session looks exactly like today.
    let right = chunks[1];

    let now_line: Option<String> = app
        .now
        .as_ref()
        .filter(|(_, t)| t.elapsed() < NOW_STALE)
        .map(|(label, _)| label.clone())
        .or_else(|| {
            // Tools have gone quiet; the in-progress task is the best
            // description of what Claude is doing.
            app.tasks
                .iter()
                .find(|t| t.status == TaskStatus::InProgress)
                .map(|t| format!("◐ {}", t.active_form))
        });

    let cap = (right.height * 2) / 5; // spec: at most 40% of the column
    let pane_rows = content_height(now_line.is_some(), app.tasks.len(), cap);
    let (tree_area, pane_area) = if pane_rows == 0 {
        (right, None)
    } else {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(pane_rows + 2)])
            .split(right);
        (split[0], Some(split[1]))
    };

    let tree_name = app
        .tree
        .root_path()
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| app.tree.root_path().to_string_lossy().to_string());
    // A rescan in progress is a hint in the title, not a blank pane.
    let tree_title = if app.tree_loading && !app.tree.nodes().is_empty() {
        format!(" {tree_name} · rescanning ")
    } else {
        format!(" {tree_name} ")
    };

    let tree_block = Block::default()
        .title(tree_title)
        .title_style(Style::default().fg(Color::Yellow).bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let tree_inner = tree_block.inner(tree_area);
    frame.render_widget(tree_block, tree_area);

    // Store tree area for mouse scroll routing
    app.tree_area = Some(tree_inner);

    // A rescan ANNOTATES the tree, it never replaces it. Blanking the pane to
    // say "scanning" throws away the perfectly good tree the user was reading,
    // for work that usually finishes in tens of milliseconds.
    if app.tree_loading && app.tree.nodes().is_empty() {
        let loading =
            Paragraph::new("  Scanning files...").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(loading, tree_inner);
    } else {
        // A widened pane invalidates the horizontal scroll: re-clamp before
        // anything reads it, so the painted shift, the thumb, and the mouse
        // all agree on the same offset.
        app.tree
            .clamp_h_offset(tree_inner.width.saturating_sub(1) as usize);

        // Auto-scroll to keep CWD visible — only when CWD actually changes
        let visible_height = tree_inner.height as usize;
        let cwd = app.terminal.cwd();
        let cwd_changed = app
            .last_auto_scroll_cwd
            .as_ref()
            .is_none_or(|last| last.as_path() != cwd);

        if cwd_changed {
            let cwd_index = app
                .tree
                .nodes()
                .iter()
                .position(|n| n.is_dir && n.path == cwd);

            if let Some(idx) = cwd_index {
                let mut offset = app.tree.offset();
                if idx >= offset + visible_height {
                    offset = idx - visible_height + 1;
                } else if idx < offset {
                    offset = idx;
                }
                app.tree.set_offset(offset);
            }
            app.last_auto_scroll_cwd = Some(cwd.to_path_buf());
        }

        // Fade is computed here, once per frame, so the widget itself
        // never consults a clock and stays deterministic under test.
        let recent: HashMap<std::path::PathBuf, (crate::activity::FileAction, Fade)> = app
            .recent_activity
            .iter()
            .filter(|(_, (_, t))| t.elapsed() < GLYPH_EXPIRY)
            .map(|(p, (action, t))| {
                let fade = if t.elapsed() < GLYPH_BRIGHT {
                    Fade::Bright
                } else {
                    Fade::Dim
                };
                (p.clone(), (*action, fade))
            })
            .collect();

        // Render file tree
        let file_tree_widget = FileTreeWidget::new(&app.tree, Some(app.terminal.cwd()))
            .highlight(app.highlight.as_ref().map(|(p, k)| (p.as_path(), *k)))
            .recent(&recent);
        frame.render_stateful_widget(
            file_tree_widget,
            tree_inner,
            &mut FileTreeWidgetState {
                offset: app.tree.offset(),
            },
        );
    }

    if let Some(pane_area) = pane_area {
        let pane_block = Block::default()
            .title(" claude ")
            .title_style(Style::default().fg(Color::Cyan))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let pane_inner = pane_block.inner(pane_area);
        frame.render_widget(pane_block, pane_area);
        frame.render_widget(
            ActivityPaneWidget::new(now_line.as_deref(), &app.tasks),
            pane_inner,
        );
    }
}

pub struct FileTreeWidgetState {
    pub offset: usize,
}
