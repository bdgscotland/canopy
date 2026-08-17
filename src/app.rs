use anyhow::Result;
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::prelude::Rect;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// A tree rebuild walks the filesystem, so never do it more often than this
/// however many events arrive.
const REFRESH_THROTTLE: Duration = Duration::from_millis(250);

/// Give up waiting on a walk after this. spawn_blocking cannot be cancelled, so
/// the task is orphaned rather than stopped -- but the tree stops being held
/// hostage by it, and its late result is discarded by generation.
const WALK_TIMEOUT: Duration = Duration::from_secs(10);

use crate::activity::{ActivityKind, ActivityWatcher, Event, FileAction, GLYPH_EXPIRY};
use crate::tasks::{Task, TaskWatcher};
use crate::terminal::TerminalPane;
use crate::tree::FileNode;
use crate::tree::FileTree;

/// A selection anchored to the TEXT, not to the screen.
///
/// Screen rows move under the content whenever the view scrolls, so a
/// viewport-relative selection highlights different text after every wheel
/// tick and copies something other than what is highlighted. Absolute line
/// numbers (see `VirtualTerminal::absolute_line`) stay attached to the line
/// the user actually dragged over.
pub struct Selection {
    pub start: (u16, u64), // (col, absolute line)
    pub end: (u16, u64),
}

pub struct App {
    pub tree: FileTree,
    pub terminal: TerminalPane,
    pub tree_width_percent: u16,
    pub tree_loading: bool,
    pub tree_area: Option<Rect>,
    pub terminal_area: Option<Rect>,
    pub selection: Option<Selection>,
    /// Where the button went down. A selection only exists once the pointer
    /// actually moves — otherwise a plain click would leave a one-cell
    /// highlight that nothing clears.
    drag_anchor: Option<(u16, u64)>,
    /// Grab offset within the scrollbar thumb, so a drag moves the thumb
    /// relative to where it was grabbed rather than snapping its top to the
    /// pointer.
    scrollbar_grab: Option<usize>,
    /// Grab offset within the HORIZONTAL scrollbar thumb, same idea as
    /// scrollbar_grab one axis over.
    hscrollbar_grab: Option<usize>,
    /// Grab offset within the TERMINAL pane's scrollbar thumb.
    terminal_scrollbar_grab: Option<usize>,
    pub last_auto_scroll_cwd: Option<PathBuf>,
    /// Follows Claude's session transcript.
    pub activity: ActivityWatcher,
    /// The file Claude touched most recently, and how. Persists until the next
    /// event supersedes it — no fade timer, so nothing runs while idle.
    pub highlight: Option<(PathBuf, ActivityKind)>,
    /// Files Claude touched recently and how, pruned past GLYPH_EXPIRY.
    /// Feeds the per-file glyphs; `highlight` stays the single loud row.
    pub recent_activity: HashMap<PathBuf, (FileAction, Instant)>,
    /// The latest narratable action, display-ready ("⚒ Run the tests").
    pub now: Option<(String, Instant)>,
    pub tasks: Vec<Task>,
    task_watcher: TaskWatcher,
    /// Set when a revealed path was not in the tree, so the next tick rebuilds
    /// and retries. Claude creating a file is the common case.
    pending_reveal: Option<PathBuf>,
    last_refresh: Instant,
    refresh_queued: bool,
    reveal_attempts: u8,
    /// Completed walks arrive here from `spawn_blocking`. The walk is unbounded
    /// I/O and must never run on the event-loop thread.
    walk_rx: mpsc::UnboundedReceiver<WalkResult>,
    walk_tx: mpsc::UnboundedSender<WalkResult>,
    /// One walk at a time. Without this, a burst of events queues N walks and
    /// the last N-1 are wasted work against a tree that already changed.
    walk_in_flight: bool,
    /// The terminal feed died but the session did not. Surfaced in the UI.
    pub reader_failed_notice: bool,
    initial_walk_done: bool,
    /// When the in-flight walk started, so a hung one cannot latch the tree
    /// dead forever on an unresponsive mount.
    walk_started: Option<Instant>,
    /// Incremented per walk; results from an older generation are discarded.
    walk_generation: u64,
    /// A walk exceeded its timeout. Surfaced so a wedged mount is visible
    /// rather than looking like a tree that simply stopped updating.
    pub walk_stalled: bool,
}

/// A finished walk, tagged with the root it was for so a result that arrives
/// after the tree re-roots can be discarded instead of showing the wrong tree.
struct WalkResult {
    root: PathBuf,
    nodes: Vec<FileNode>,
    generation: u64,
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

        // Pin the child to a session id WE choose, by passing --session-id
        // to the claude we spawn. Discovery-by-newest is a race: several
        // sessions routinely share one project directory (other terminals,
        // cloud agents, path-mangling collisions), and whichever wrote last
        // won — so the tree followed a stranger while the session on screen
        // went unseen. An explicit CANOPY_SESSION_ID pins without injecting
        // (that session already exists); user args that pick the session
        // (--resume/--continue) and non-stock CANOPY_COMMANDs, which may not
        // accept the flag, keep the old discovery.
        let mut claude_args = claude_args;
        let pinned = std::env::var("CANOPY_SESSION_ID").ok().or_else(|| {
            let stock_claude = std::env::var("CANOPY_COMMAND")
                .map(|c| std::path::Path::new(&c).file_name() == Some("claude".as_ref()))
                .unwrap_or(true);
            if !stock_claude || crate::activity::steers_session(&claude_args) {
                return None;
            }
            let id = crate::activity::generate_session_id()?;
            claude_args.push("--session-id".to_string());
            claude_args.push(id.clone());
            Some(id)
        });

        // Open the PTY FIRST. Rust evaluates struct fields in source order, so
        // building the tree here meant Claude did not start until the walk
        // finished -- 443 ms on a 20k-node repo before the walk was rewritten,
        // and still non-zero on a cold cache. Claude's startup is the thing the
        // user is waiting for; the tree can arrive a few milliseconds later.
        let terminal = TerminalPane::new(&canonical_path, &claude_args, pty_tx)?;
        let tree = FileTree::new(&canonical_path, show_hidden, max_depth)?;
        let (walk_tx, walk_rx) = mpsc::unbounded_channel();

        Ok(Self {
            tree,
            terminal,
            tree_width_percent: tree_width.clamp(10, 50),
            // The tree starts EMPTY: FileTree::new no longer walks, so that
            // uncapped I/O cannot run in raw mode before any frame or signal
            // handler exists. The first tick spawns the real walk off-thread.
            tree_loading: true,
            tree_area: None,
            terminal_area: None,
            selection: None,
            drag_anchor: None,
            scrollbar_grab: None,
            hscrollbar_grab: None,
            terminal_scrollbar_grab: None,
            last_auto_scroll_cwd: None,
            activity: ActivityWatcher::new(&canonical_path, pinned),
            highlight: None,
            recent_activity: HashMap::new(),
            now: None,
            tasks: Vec::new(),
            task_watcher: TaskWatcher::new(),
            pending_reveal: None,
            last_refresh: Instant::now(),
            refresh_queued: false,
            reveal_attempts: 0,
            walk_rx,
            walk_tx,
            walk_in_flight: false,
            reader_failed_notice: false,
            initial_walk_done: false,
            walk_started: None,
            walk_generation: 0,
            walk_stalled: false,
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
            // set_root no longer walks; schedule it off-thread.
            self.request_refresh();
        }

        // Issue the initial walk on the first tick, once a frame has been
        // painted and the event loop is serving input and signals.
        if !self.initial_walk_done {
            self.initial_walk_done = true;
            self.request_refresh();
        }
        self.drain_walks();
        self.poll_activity();

        self.tasks = self
            .task_watcher
            .poll(self.activity.session_id().as_deref())
            .to_vec();

        // Process clipboard requests from vterm (OSC 52)
        {
            let requests = self.terminal.vterm_lock().take_clipboard_requests();
            for text in requests {
                copy_to_clipboard(&text);
            }
        }
        // ONLY a confirmed child exit ends Canopy. A dead reader thread
        // freezes the pane but leaves the session running, and quitting would
        // take that session down with us -- which is exactly what a vterm bug
        // used to do, silently, with exit code 0.
        if self.terminal.reader_failed() && !self.terminal.is_process_exited() {
            self.reader_failed_notice = true;
            return false;
        }
        self.terminal.is_process_exited()
    }

    /// The resume command for this session, so a user whose pane died can get
    /// straight back in. Recovery, not prevention: this survives failures no
    /// bound can catch, including SIGKILL.
    pub fn resume_hint(&self) -> Option<String> {
        let id = self.activity.session_id()?;
        Some(format!("claude --resume {id}"))
    }

    /// Rebuild the tree OFF the event-loop thread, at most once per
    /// REFRESH_THROTTLE, and never more than one at a time. Bursts coalesce.
    fn request_refresh(&mut self) {
        // A walk on a hung mount never returns, and spawn_blocking cannot be
        // cancelled. Without this the flag latches true and the tree is dead
        // for the rest of the session. After the timeout we orphan that task
        // and allow a new one; its result is discarded by generation.
        if self.walk_in_flight
            && self
                .walk_started
                .is_some_and(|t| t.elapsed() > WALK_TIMEOUT)
        {
            self.walk_in_flight = false;
            self.tree_loading = false;
            self.walk_stalled = true;
        }
        if self.walk_in_flight || self.last_refresh.elapsed() < REFRESH_THROTTLE {
            self.refresh_queued = true;
            return;
        }
        self.spawn_walk();
    }

    fn spawn_walk(&mut self) {
        let root = self.tree.root_path().to_path_buf();
        let show_hidden = self.tree.show_hidden();
        let max_depth = self.tree.max_depth();
        // Carry the fold state across, or every rescan silently re-opens
        // everything the user closed.
        let collapsed = self.tree.collapsed_set().clone();
        let tx = self.walk_tx.clone();

        self.walk_in_flight = true;
        self.refresh_queued = false;
        self.last_refresh = Instant::now();
        self.walk_started = Some(Instant::now());
        self.walk_generation += 1;
        let generation = self.walk_generation;
        self.tree_loading = true;

        // spawn_blocking, not spawn: this is blocking filesystem I/O. On the
        // current_thread runtime a blocking call in ANY task stalls every other
        // task, including the one reading the keyboard.
        tokio::task::spawn_blocking(move || {
            let nodes = crate::tree::walk(&root, show_hidden, max_depth, &collapsed);
            let _ = tx.send(WalkResult {
                root,
                nodes,
                generation,
            });
        });
    }

    /// Take any finished walk and start a queued one. Called from tick().
    fn drain_walks(&mut self) {
        while let Ok(result) = self.walk_rx.try_recv() {
            // Ignore a straggler from a walk we already gave up on, or one for
            // a root we have since moved away from.
            if result.generation != self.walk_generation {
                continue;
            }
            self.walk_in_flight = false;
            self.walk_started = None;
            self.walk_stalled = false;
            if result.root == self.tree.root_path() {
                self.tree.set_nodes(result.nodes);
                self.tree_loading = false;
            }
        }
        if self.refresh_queued
            && !self.walk_in_flight
            && self.last_refresh.elapsed() >= REFRESH_THROTTLE
        {
            self.spawn_walk();
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
            } else if self.reveal_attempts < 3 {
                // 3, not 10: drain_walks runs before poll_activity and the
                // 250 ms throttle outlasts the 200 ms tick, so attempt 2 is
                // routinely the one that succeeds for a genuinely new file.
                // Ten attempts only ever burned walks on paths that could
                // never appear.
                self.reveal_attempts += 1;
                self.request_refresh();
                self.pending_reveal = Some(path);
            }
        }

        let polled = self.activity.poll();

        // Narrate the newest event. A file touch also narrates, but a
        // command/search/agent event is more specific, so it wins the tie
        // within one poll.
        let now = Instant::now();
        if let Some(event) = polled.events.last() {
            let label = match event {
                Event::Command { label } => format!("⚒ {label}"),
                Event::Search { pattern } => format!("🔍 {pattern}"),
                Event::Agent { label } => format!("⧉ {label}"),
            };
            self.now = Some((label, now));
        } else if let Some(f) = polled.files.last() {
            let shown = f
                .path
                .strip_prefix(self.tree.root_path())
                .unwrap_or(&f.path);
            self.now = Some((format!("✎ {}", shown.display()), now));
        }

        // Classify BEFORE reveal_ancestors/request_refresh can add the file
        // to the tree: existence-now is what separates create from overwrite.
        for f in &polled.files {
            let existed = self.tree.nodes().iter().any(|n| n.path == f.path);
            self.recent_activity
                .insert(f.path.clone(), (FileAction::classify(f.kind, existed), now));
        }
        self.recent_activity
            .retain(|_, (_, t)| t.elapsed() < GLYPH_EXPIRY);

        let Some(latest) = polled.files.last() else {
            return;
        };
        self.highlight = Some((latest.path.clone(), latest.kind));

        // Open whatever is hiding it. Must happen BEFORE request_refresh, which
        // clones the fold set for the off-thread walk -- otherwise the walk runs
        // against the old set and the file stays hidden for another round.
        if self.tree.reveal_ancestors(&latest.path) {
            self.request_refresh();
            self.pending_reveal = Some(latest.path.clone());
            self.reveal_attempts = 0;
            return;
        }

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
                } else if !self.terminal.wheel(true, 3) {
                    self.terminal.scroll_up();
                }
            }
            MouseEventKind::ScrollDown => {
                if in_tree {
                    let visible_height = self.tree_area.map(|a| a.height as usize).unwrap_or(1);
                    let max_offset = self.tree.nodes().len().saturating_sub(visible_height);
                    let offset = (self.tree.offset() + 3).min(max_offset);
                    self.tree.set_offset(offset);
                } else if !self.terminal.wheel(false, 3) {
                    self.terminal.scroll_down();
                }
            }
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
            MouseEventKind::Down(MouseButton::Left) => {
                // Pressing anywhere clears the previous selection, as every
                // terminal does. The anchor is remembered, but no selection
                // exists until the pointer actually moves.
                self.selection = None;
                if in_terminal && self.terminal_scrollbar_down(event) {
                    self.drag_anchor = None;
                    return;
                }
                self.drag_anchor = if in_terminal {
                    self.terminal_point(event.column, event.row)
                } else {
                    None
                };

                // Click a directory in the tree to fold it. The mouse is the
                // only input Canopy owns -- every keystroke is forwarded to
                // Claude untouched -- so this costs the child nothing.
                if in_tree {
                    if let Some(area) = self.tree_area {
                        let row = event.row.saturating_sub(area.y) as usize;
                        let visible = area.height as usize;
                        let thumb = self.tree.scrollbar_thumb(visible);
                        let on_scrollbar =
                            area.width > 0 && event.column == area.x + area.width - 1;
                        let track_width = area.width.saturating_sub(1) as usize;
                        let hthumb = self.tree.hscrollbar_thumb(track_width);
                        let on_hscrollbar = area.height > 0
                            && event.row == area.y + area.height - 1
                            && event.column < area.x + track_width as u16
                            && hthumb.is_some();

                        match (on_scrollbar, thumb) {
                            // Grabbed the thumb: remember where within it, so
                            // it does not jump under the cursor on the drag.
                            (true, Some((pos, height))) if row >= pos && row < pos + height => {
                                self.scrollbar_grab = Some(row - pos);
                            }
                            // Clicked the track: page toward the click, which
                            // is what every scrollbar does.
                            (true, Some((pos, _))) => self.tree.page(row > pos, visible),
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
                        }
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Dragging the scrollbar thumb.
                if let Some(grab) = self.scrollbar_grab {
                    if let Some(area) = self.tree_area {
                        let row = event.row.saturating_sub(area.y) as usize;
                        self.tree
                            .scroll_to_thumb_row(row.saturating_sub(grab), area.height as usize);
                    }
                    return;
                }
                if let Some(grab) = self.hscrollbar_grab {
                    if let Some(area) = self.tree_area {
                        let col = event.column.saturating_sub(area.x) as usize;
                        let track_width = area.width.saturating_sub(1) as usize;
                        self.tree
                            .scroll_to_hthumb_col(col.saturating_sub(grab), track_width);
                    }
                    return;
                }
                if let Some(grab) = self.terminal_scrollbar_grab {
                    if let Some(area) = self.terminal_area {
                        let row = event.row.saturating_sub(area.y) as usize;
                        self.terminal
                            .vterm_lock()
                            .scroll_to_thumb_row(row.saturating_sub(grab), area.height as usize);
                    }
                    return;
                }
                if let Some(anchor) = self.drag_anchor {
                    if let Some(point) = self.terminal_point(event.column, event.row) {
                        if point != anchor {
                            self.selection = Some(Selection {
                                start: anchor,
                                end: point,
                            });
                        } else {
                            self.selection = None;
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag_anchor = None;
                self.scrollbar_grab = None;
                self.hscrollbar_grab = None;
                self.terminal_scrollbar_grab = None;
                if let Some(sel) = self.selection.as_ref() {
                    let text = self.terminal.extract_text(sel.start, sel.end);
                    if !text.is_empty() {
                        copy_to_clipboard(&text);
                    }
                }
            }
            _ => {}
        }
    }

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

    /// Map a mouse position to (column, absolute line), clamped to the pane.
    /// Returns None when the pointer is outside the terminal pane.
    fn terminal_point(&self, column: u16, row: u16) -> Option<(u16, u64)> {
        let area = self.terminal_area?;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let col = column
            .saturating_sub(area.x)
            .min(area.width.saturating_sub(1));
        let screen_row = row
            .saturating_sub(area.y)
            .min(area.height.saturating_sub(1));
        let vt = self.terminal.vterm_lock();
        let top = vt.top_view_index(area.height as usize);
        Some((col, vt.absolute_line(top + screen_row as usize)))
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

/// Queue a clipboard write on a dedicated worker thread.
///
/// `pbcopy` is a spawn plus a write plus a `wait()` with no timeout, measured at
/// 6.6 ms -- and it ran on the event-loop thread, on the path of every mouse
/// release AND once per OSC 52 inside a single tick(), so a burst serialised
/// into the UI. A clipboard is last-write-wins, so one long-lived worker with a
/// queue preserves ordering; thread-per-request would not, and would let an
/// unbounded number of threads pile up behind a wedged pbcopy.
pub(crate) fn copy_to_clipboard(text: &str) {
    use std::sync::mpsc::{channel, Sender};
    use std::sync::OnceLock;
    static TX: OnceLock<Sender<String>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = channel::<String>();
        std::thread::spawn(move || {
            while let Ok(mut text) = rx.recv() {
                // Coalesce: only the newest write can win anyway, so drain the
                // backlog rather than spawning pbcopy once per queued item.
                while let Ok(newer) = rx.try_recv() {
                    text = newer;
                }
                let _ = clipboard_write(&text);
            }
        });
        tx
    });
    let _ = tx.send(text.to_string());
}

fn clipboard_write(text: &str) -> bool {
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

#[cfg(test)]
mod loading_state_tests {
    /// `tree_loading` must match what the constructor actually did.
    ///
    /// This flag has now been wrong in BOTH directions, which is why it is
    /// pinned. It started true while FileTree::new walked synchronously, so
    /// "Scanning files..." stayed on screen forever over a populated tree.
    /// FileTree::new no longer walks -- that uncapped I/O ran in raw mode with
    /// no signal handler and no way to interrupt it -- so the tree really is
    /// empty at construction and the flag must start true again, with the first
    /// tick issuing the walk.
    ///
    /// Asserted against the source rather than by building an App, which needs
    /// a PTY and a live child.
    #[test]
    fn tree_loading_matches_what_the_constructor_actually_does() {
        let src = include_str!("app.rs");
        let ctor = src
            .split("Ok(Self {")
            .nth(1)
            .expect("App::new struct literal");
        assert!(
            ctor.contains("tree_loading: true"),
            "the tree is empty at construction, so loading must start true"
        );
        assert!(
            src.contains("initial_walk_done"),
            "something must issue the first walk once the loop is running"
        );
        let tree_src = include_str!("tree/mod.rs");
        assert!(
            !tree_src.contains("tree.rebuild_visible_nodes()?;"),
            "FileTree::new must NOT walk: it runs in raw mode with no signal \
             handler, where only SIGKILL can interrupt it"
        );
    }

    /// The tree must never be blanked for a rescan that usually takes tens of
    /// milliseconds. Loading annotates the title; only a genuinely empty tree
    /// shows the placeholder.
    #[test]
    fn a_rescan_annotates_the_tree_instead_of_replacing_it() {
        let ui = include_str!("ui/mod.rs");
        assert!(
            ui.contains("app.tree_loading && app.tree.nodes().is_empty()"),
            "the loading placeholder must be gated on an EMPTY tree"
        );
        assert!(
            ui.contains("rescanning"),
            "a rescan should be visible in the title, not by blanking the pane"
        );
    }
}
