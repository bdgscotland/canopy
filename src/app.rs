use anyhow::Result;
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::prelude::Rect;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// A tree rebuild walks the filesystem, so never do it more often than this
/// however many events arrive.
const REFRESH_THROTTLE: Duration = Duration::from_millis(250);

use crate::activity::{ActivityKind, ActivityWatcher};
use crate::terminal::TerminalPane;
use crate::tree::FileTree;

pub struct Selection {
    pub start: (u16, u16), // (col, row) terminal-local coordinates
    pub end: (u16, u16),
}

pub struct App {
    pub tree: FileTree,
    pub terminal: TerminalPane,
    pub tree_width_percent: u16,
    pub tree_loading: bool,
    pub tree_area: Option<Rect>,
    pub terminal_area: Option<Rect>,
    pub selection: Option<Selection>,
    pub last_auto_scroll_cwd: Option<PathBuf>,
    /// Follows Claude's session transcript.
    pub activity: ActivityWatcher,
    /// The file Claude touched most recently, and how. Persists until the next
    /// event supersedes it — no fade timer, so nothing runs while idle.
    pub highlight: Option<(PathBuf, ActivityKind)>,
    /// Set when a revealed path was not in the tree, so the next tick rebuilds
    /// and retries. Claude creating a file is the common case.
    pending_reveal: Option<PathBuf>,
    last_refresh: Instant,
    refresh_queued: bool,
    reveal_attempts: u8,
}

impl App {
    pub fn new(
        path: PathBuf,
        tree_width: u16,
        show_hidden: bool,
        max_depth: usize,
        claude_args: Vec<String>,
        pty_tx: mpsc::UnboundedSender<()>,
    ) -> Result<Self> {
        let canonical_path = path.canonicalize().unwrap_or(path);

        // Open the PTY FIRST. Rust evaluates struct fields in source order, so
        // building the tree here meant Claude did not start until the walk
        // finished -- 443 ms on a 20k-node repo before the walk was rewritten,
        // and still non-zero on a cold cache. Claude's startup is the thing the
        // user is waiting for; the tree can arrive a few milliseconds later.
        let terminal = TerminalPane::new(&canonical_path, &claude_args, pty_tx)?;
        let tree = FileTree::new(&canonical_path, show_hidden, max_depth)?;

        Ok(Self {
            tree,
            terminal,
            tree_width_percent: tree_width.clamp(10, 50),
            tree_loading: true,
            tree_area: None,
            terminal_area: None,
            selection: None,
            last_auto_scroll_cwd: None,
            activity: ActivityWatcher::new(&canonical_path, std::env::var("CANOPY_SESSION_ID").ok()),
            highlight: None,
            pending_reveal: None,
            last_refresh: Instant::now(),
            refresh_queued: false,
            reveal_attempts: 0,
        })
    }

    pub fn tick(&mut self) -> bool {
        self.terminal.tick();

        // If the CWD leaves the tree root, re-root the tree there.
        let cwd = self.terminal.cwd().to_path_buf();
        if !cwd.starts_with(self.tree.root_path()) {
            self.tree.set_root(cwd.clone());
            self.activity.set_root(cwd);
            self.last_auto_scroll_cwd = None;
        }

        self.flush_queued_refresh();
        self.poll_activity();

        // Process clipboard requests from vterm (OSC 52)
        {
            let requests = self.terminal.vterm_lock().take_clipboard_requests();
            for text in requests {
                copy_to_clipboard(&text);
            }
        }
        if self.tree_loading {
            self.tree_loading = false;
        }
        self.terminal.is_process_exited()
    }

    /// Rebuild the tree, but never more than once per REFRESH_THROTTLE. Bursts
    /// coalesce into a single walk on a later tick.
    fn request_refresh(&mut self) {
        if self.last_refresh.elapsed() >= REFRESH_THROTTLE {
            self.tree.refresh();
            self.last_refresh = Instant::now();
            self.refresh_queued = false;
        } else {
            self.refresh_queued = true;
        }
    }

    fn flush_queued_refresh(&mut self) {
        if self.refresh_queued && self.last_refresh.elapsed() >= REFRESH_THROTTLE {
            self.tree.refresh();
            self.last_refresh = Instant::now();
            self.refresh_queued = false;
        }
    }

    /// Pull anything Claude has done since the last tick and follow it.
    fn poll_activity(&mut self) {
        let visible = self.tree_area.map(|a| a.height as usize).unwrap_or(0);

        // A path we could not show last tick. The retry was previously
        // guaranteed to fail: pending_reveal is only set when the throttle
        // blocked the rebuild, and the retry ran before the rebuild happened.
        // Keep it pending, and keep asking for the rebuild, until it lands.
        if let Some(path) = self.pending_reveal.take() {
            if self.tree.reveal(&path, visible) {
                self.reveal_attempts = 0;
            } else if self.reveal_attempts < 10 {
                self.reveal_attempts += 1;
                self.request_refresh();
                self.pending_reveal = Some(path);
            }
        }

        let events = self.activity.poll();
        let Some(latest) = events.last() else {
            return;
        };

        self.highlight = Some((latest.path.clone(), latest.kind));

        if !self.tree.reveal(&latest.path, visible) {
            // A rebuild is only worth it if the path COULD appear. Without this
            // test, every tool-use on a hidden or ignored file cost a full walk.
            if self.tree.can_never_show(&latest.path) {
                return;
            }
            // Claude most likely just created it; rebuild and retry.
            self.request_refresh();
            if !self.tree.reveal(&latest.path, visible) {
                self.pending_reveal = Some(latest.path.clone());
                self.reveal_attempts = 0;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        self.selection = None;
        self.terminal.handle_key(key);
        false
    }

    pub fn handle_paste(&mut self, text: String) {
        self.selection = None;
        self.terminal.handle_paste(text);
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) {
        let in_tree = self.tree_area.is_some_and(|area| {
            event.column >= area.x
                && event.column < area.x + area.width
                && event.row >= area.y
                && event.row < area.y + area.height
        });

        let in_terminal = self.terminal_area.is_some_and(|area| {
            event.column >= area.x
                && event.column < area.x + area.width
                && event.row >= area.y
                && event.row < area.y + area.height
        });

        match event.kind {
            MouseEventKind::ScrollUp => {
                if in_tree {
                    let offset = self.tree.offset();
                    self.tree.set_offset(offset.saturating_sub(3));
                } else {
                    self.terminal.scroll_up();
                }
            }
            MouseEventKind::ScrollDown => {
                if in_tree {
                    let visible_height = self.tree_area.map(|a| a.height as usize).unwrap_or(1);
                    let max_offset = self.tree.nodes().len().saturating_sub(visible_height);
                    let offset = (self.tree.offset() + 3).min(max_offset);
                    self.tree.set_offset(offset);
                } else {
                    self.terminal.scroll_down();
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if in_terminal {
                    let area = self.terminal_area.unwrap();
                    let col = event.column.saturating_sub(area.x);
                    let row = event.row.saturating_sub(area.y);
                    self.selection = Some(Selection {
                        start: (col, row),
                        end: (col, row),
                    });
                } else {
                    self.selection = None;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(ref mut sel) = self.selection {
                    if let Some(area) = self.terminal_area {
                        let col = event
                            .column
                            .saturating_sub(area.x)
                            .min(area.width.saturating_sub(1));
                        let row = event
                            .row
                            .saturating_sub(area.y)
                            .min(area.height.saturating_sub(1));
                        sel.end = (col, row);
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = self.selection.as_ref() {
                    // Only copy if the selection spans more than a single point
                    if sel.start != sel.end {
                        let text = self.terminal.extract_text(sel.start, sel.end);
                        if !text.is_empty() {
                            copy_to_clipboard(&text);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub fn handle_file_change(&mut self, path: PathBuf) {
        if !path.starts_with(self.tree.root_path()) {
            return;
        }
        // Build output and other ignored paths are never displayed, so a
        // rebuild would be pure cost. `cargo build` alone emits thousands.
        if self.tree.is_noise(&path) {
            return;
        }
        self.request_refresh();
    }
}

pub(crate) fn copy_to_clipboard(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        try_clipboard_cmd("pbcopy", &[], text)
    }

    #[cfg(target_os = "linux")]
    {
        // Try xclip first, then xsel, then wl-copy (Wayland)
        try_clipboard_cmd("xclip", &["-selection", "clipboard"], text)
            || try_clipboard_cmd("xsel", &["--clipboard", "--input"], text)
            || try_clipboard_cmd("wl-copy", &[], text)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = text;
        false
    }
}

fn try_clipboard_cmd(program: &str, args: &[&str], text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(ref mut stdin) = child.stdin {
                let _ = stdin.write_all(text.as_bytes());
            }
            child.wait().map(|s| s.success()).unwrap_or(false)
        }
        Err(_) => false,
    }
}
