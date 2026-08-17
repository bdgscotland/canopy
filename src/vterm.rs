use ratatui::prelude::*;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use vte::{Params, Perform};

#[derive(Clone, Debug)]
pub struct Cell {
    pub ch: String,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: " ".to_string(),
            style: Style::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CursorState {
    pub x: usize,
    pub y: usize,
    pub visible: bool,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            visible: true,
        }
    }
}

pub struct VirtualTerminal {
    grid: Vec<Vec<Cell>>,
    cols: usize,
    rows: usize,
    cursor: CursorState,
    current_style: Style,
    scrollback: VecDeque<Vec<Cell>>,
    scroll_offset: usize,
    saved_cursor: Option<CursorState>,
    // Alternate screen buffer (used by full-screen apps like vim, less, etc.)
    saved_grid: Option<Vec<Vec<Cell>>>,
    saved_scrollback: Option<VecDeque<Vec<Cell>>>,
    /// Set by every Perform callback. A slice that produces none means vte is
    /// accumulating bytes internally with no way for us to see them -- which is
    /// exactly what a never-terminated OSC looks like.
    saw_callback: bool,
    /// Bytes accumulated inside the OSC currently being parsed. vte's own
    /// `MAX_OSC_RAW` guard is `#[cfg(not(feature = "std"))]` and compiled out
    /// of this build, so its buffer grows without limit -- measured 1:1 with
    /// input, 64 MiB in gave 70 MB RSS, and `Vec::clear()` keeps the capacity.
    osc_bytes: usize,
    /// How many times an escape flood or oversized escape was discarded.
    escape_floods: u32,
    /// One-shot message for the UI.
    truncation_notice: Option<String>,
    /// Lines that have fallen out of the front of `scrollback`, ever.
    ///
    /// Selections anchor to ABSOLUTE line numbers rather than screen rows.
    /// Screen coordinates move under the text whenever the view scrolls, and
    /// indices into `scrollback` shift whenever a line is evicted; this counter
    /// makes `lines_evicted + index` monotonic, so a highlight stays on the
    /// text the user actually selected.
    lines_evicted: u64,
    saved_main_cursor: Option<CursorState>,
    parser: Option<vte::Parser>,
    // Scroll region (DECSTBM): top..bottom (0-indexed, bottom is exclusive)
    scroll_top: usize,
    scroll_bottom: usize,
    // Response queue for DSR/CPR etc. — caller must flush these to PTY
    response_queue: Vec<Vec<u8>>,
    // CWD reported via OSC 7
    reported_cwd: Option<PathBuf>,
    // Clipboard requests from OSC 52
    clipboard_requests: Vec<String>,
    // Whether the child process has enabled focus event tracking (DECSET 1004)
    focus_tracking: bool,
}

const MAX_SCROLLBACK: usize = 1000;

/// Cap on a single OSC payload. Deliberately generous: Claude emits clipboard
/// writes through OSC 52 with no length limit of its own, and truncating a real
/// clipboard payload would be a silent data bug. This exists to catch an OSC
/// that never terminates, not to limit legitimate ones.
const MAX_OSC_BYTES: usize = 8 * 1024 * 1024;

impl VirtualTerminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            grid: Self::make_grid(cols, rows),
            cols,
            rows,
            cursor: CursorState::default(),
            current_style: Style::default(),
            scrollback: VecDeque::new(),
            scroll_offset: 0,
            saved_cursor: None,
            saved_grid: None,
            saved_scrollback: None,
            saw_callback: false,
            osc_bytes: 0,
            escape_floods: 0,
            truncation_notice: None,
            lines_evicted: 0,
            saved_main_cursor: None,
            parser: Some(vte::Parser::new()),
            scroll_top: 0,
            scroll_bottom: rows,
            response_queue: Vec::new(),
            reported_cwd: None,
            clipboard_requests: Vec::new(),
            focus_tracking: false,
        }
    }

    /// Take pending responses (e.g. DSR/CPR replies) to send back to the PTY
    pub fn take_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.response_queue)
    }

    /// Take pending clipboard requests (from OSC 52) for the app to process
    pub fn take_clipboard_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.clipboard_requests)
    }

    /// Get the CWD reported via OSC 7
    pub fn reported_cwd(&self) -> Option<&Path> {
        self.reported_cwd.as_deref()
    }

    /// Clear the cached OSC 7 CWD (called when fallback scan overrides it)
    pub fn clear_reported_cwd(&mut self) {
        self.reported_cwd = None;
    }

    /// Whether the child process has enabled focus event tracking (DECSET 1004)
    pub fn focus_tracking_enabled(&self) -> bool {
        self.focus_tracking
    }

    fn make_grid(cols: usize, rows: usize) -> Vec<Vec<Cell>> {
        vec![vec![Cell::default(); cols]; rows]
    }

    fn make_row(&self) -> Vec<Cell> {
        vec![Cell::default(); self.cols]
    }

    /// Feed raw PTY bytes through the vte parser
    /// Feed bytes from the child, with a wall-clock budget.
    ///
    /// This runs on the PTY reader thread while HOLDING the vterm mutex, which
    /// the main thread needs to render or handle a mouse event. Clamping the
    /// individual operations is necessary but not sufficient: the already-
    /// clamped IL/DL/SU/SD still cost 38-41 ms per 4 KiB chunk under an escape
    /// flood, and a 1 MiB file of them rendered 20 frames in 62 seconds with a
    /// peak lock wait of 16.8 s. A user does not wait that out; they SIGKILL,
    /// and the session goes with it.
    ///
    /// So the budget is the class-level backstop: process in slices, and once
    /// over budget, discard the rest of the buffer and say so. Losing some
    /// output is strictly better than losing the session.
    pub fn feed(&mut self, bytes: &[u8]) {
        const BUDGET: Duration = Duration::from_millis(20);
        const SLICE: usize = 512;

        let start = Instant::now();
        let mut parser = self.parser.take().unwrap_or_default();

        let mut consumed = 0;
        while consumed < bytes.len() {
            let end = (consumed + SLICE).min(bytes.len());
            let slice_len = end - consumed;
            self.saw_callback = false;
            parser.advance(self, &bytes[consumed..end]);
            consumed = end;

            // No callback at all means every byte in that slice disappeared
            // into vte's internal accumulator. vte's own MAX_OSC_RAW guard is
            // cfg(not(std)) and compiled out of this build, so it has no
            // ceiling: measured 1:1 growth, 64 MiB in gave 70 MB RSS. Worse
            // than the memory, the pane silently stops updating and never
            // recovers, which is indistinguishable from a hang.
            if self.saw_callback {
                self.osc_bytes = 0;
            } else {
                self.osc_bytes += slice_len;
            }

            if consumed < bytes.len() && start.elapsed() > BUDGET {
                self.escape_floods += 1;
                self.parser = Some(vte::Parser::new());
                self.note_truncation(bytes.len() - consumed);
                return;
            }

            // An OSC with no terminator swallows every following byte, so the
            // pane silently stops updating and never recovers. Replacing the
            // parser frees the buffer AND returns to Ground, so the remaining
            // bytes render as text instead of vanishing.
            if self.osc_bytes > MAX_OSC_BYTES {
                self.osc_bytes = 0;
                self.escape_floods += 1;
                parser = vte::Parser::new();
                self.note_truncation(0);
            }
        }

        self.parser = Some(parser);
    }

    /// Tell the user we dropped output rather than doing it silently.
    fn note_truncation(&mut self, dropped: usize) {
        self.truncation_notice = Some(if dropped > 0 {
            format!("output truncated: escape flood, {dropped} bytes dropped")
        } else {
            "oversized escape sequence discarded".to_string()
        });
    }

    /// A one-shot notice for the UI, cleared when read.
    pub fn take_truncation_notice(&mut self) -> Option<String> {
        self.truncation_notice.take()
    }

    pub fn escape_floods(&self) -> u32 {
        self.escape_floods
    }

    /// Clamp a cursor to the current grid. Saved cursors outlive the geometry
    /// they were captured in, so every restore must go through this.
    fn clamp_cursor(&self, mut c: CursorState) -> CursorState {
        c.x = c.x.min(self.cols.saturating_sub(1));
        c.y = c.y.min(self.rows.saturating_sub(1));
        c
    }

    /// Return to a known-good state after the parser panicked mid-feed.
    /// Everything derived from the byte stream is suspect, so drop it all
    /// rather than carry corrupt state forward. The child is untouched.
    pub fn reset_after_panic(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.grid = Self::make_grid(cols, rows);
        self.lines_evicted += (self.scrollback.len() + rows) as u64;
        self.scrollback.clear();
        self.scroll_offset = 0;
        self.scroll_top = 0;
        self.scroll_bottom = rows;
        self.cursor = CursorState::default();
        self.saved_cursor = None;
        self.saved_main_cursor = None;
        self.saved_grid = None;
        self.saved_scrollback = None;
        self.current_style = Style::default();
        self.response_queue.clear();
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }

        let mut new_grid = Self::make_grid(cols, rows);

        // Copy existing content
        let copy_rows = rows.min(self.rows);
        let copy_cols = cols.min(self.cols);
        for (r, new_row) in new_grid.iter_mut().enumerate().take(copy_rows) {
            for (c, new_cell) in new_row.iter_mut().enumerate().take(copy_cols) {
                *new_cell = self.grid[r][c].clone();
            }
        }

        self.grid = new_grid;
        self.cols = cols;
        self.rows = rows;

        // Everything else holding rows must be normalised too. Only `grid` was
        // rebuilt, so after a widen the scrollback kept the OLD width while
        // callers indexed it with the NEW one -- extract_text then sliced
        // row[100..40] and panicked on the MAIN thread from a mouse release.
        // Reachable from three ordinary actions: widen the window, wheel into
        // scrollback, drag past the old width.
        for row in &mut self.scrollback {
            row.resize(cols, Cell::default());
        }
        if let Some(saved) = self.saved_grid.as_mut() {
            saved.resize(rows, vec![Cell::default(); cols]);
            for row in saved.iter_mut() {
                row.resize(cols, Cell::default());
            }
        }
        if let Some(saved) = self.saved_scrollback.as_mut() {
            for row in saved.iter_mut() {
                row.resize(cols, Cell::default());
            }
        }

        // Reset scroll region to full screen
        self.scroll_top = 0;
        self.scroll_bottom = rows;

        // Clamp cursor
        self.cursor.x = self.cursor.x.min(cols.saturating_sub(1));
        self.cursor.y = self.cursor.y.min(rows.saturating_sub(1));
    }

    pub fn grid(&self) -> &Vec<Vec<Cell>> {
        &self.grid
    }

    pub fn cursor(&self) -> &CursorState {
        &self.cursor
    }

    /// Absolute number of the first line still held in `scrollback`.
    pub fn lines_evicted(&self) -> u64 {
        self.lines_evicted
    }

    /// Absolute line number for an index into the virtual `scrollback ++ grid`
    /// buffer, which is how the widget walks lines when rendering.
    pub fn absolute_line(&self, view_index: usize) -> u64 {
        self.lines_evicted + view_index as u64
    }

    /// Index into `scrollback ++ grid` of the topmost line currently on screen.
    pub fn top_view_index(&self, visible_height: usize) -> usize {
        if self.scroll_offset == 0 {
            self.scrollback.len()
        } else {
            let total = self.scrollback.len() + self.grid.len();
            total
                .saturating_sub(self.scroll_offset)
                .saturating_sub(visible_height)
        }
    }

    /// True while the child is on the alternate screen.
    ///
    /// Alt screen has no scrollback by design — that is the point of it — so a
    /// wheel event must be translated into arrow keys for the application to
    /// handle, exactly as xterm, Ghostty and iTerm2 do. Claude Code lives on
    /// the alt screen and manages its own conversation history.
    pub fn in_alternate_screen(&self) -> bool {
        self.saved_grid.is_some()
    }

    pub fn scrollback(&self) -> &VecDeque<Vec<Cell>> {
        &self.scrollback
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn set_scroll_offset(&mut self, offset: usize) {
        self.scroll_offset = offset.min(self.scrollback.len());
    }

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

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Get the text content of a specific grid row (0-indexed)
    pub fn row_text(&self, row: usize) -> String {
        if row >= self.rows {
            return String::new();
        }
        self.grid[row]
            .iter()
            .map(|c| {
                if c.ch.is_empty() || c.ch == " " {
                    " ".to_string()
                } else {
                    c.ch.clone()
                }
            })
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Scroll within the scroll region up by one line
    fn scroll_up(&mut self) {
        if self.rows == 0 || self.scroll_top >= self.scroll_bottom {
            return;
        }
        let removed = self.grid.remove(self.scroll_top);
        // Scrollback accrues only from the MAIN screen, and only when scrolling
        // from the very top. Lines leaving the alternate screen are discarded,
        // as in every real terminal: alt screen has no history, which is why a
        // pager or editor never pollutes what you scroll back to.
        if self.scroll_top == 0 && self.saved_grid.is_none() {
            self.scrollback.push_back(removed);
            if self.scrollback.len() > MAX_SCROLLBACK {
                self.scrollback.pop_front();
                self.lines_evicted += 1;
            }
        }
        // Insert blank row at the bottom of the scroll region
        let insert_pos = (self.scroll_bottom - 1).min(self.grid.len());
        self.grid.insert(insert_pos, self.make_row());
    }

    /// Scroll within the scroll region down by one line (reverse index)
    fn scroll_down(&mut self) {
        if self.rows == 0 || self.scroll_top >= self.scroll_bottom {
            return;
        }
        // Remove the bottom line of the scroll region
        let remove_pos = (self.scroll_bottom - 1).min(self.grid.len().saturating_sub(1));
        self.grid.remove(remove_pos);
        // Insert blank row at the top of the scroll region
        self.grid.insert(self.scroll_top, self.make_row());
    }

    fn put_char(&mut self, ch: char) {
        if self.cols == 0 || self.rows == 0 {
            return;
        }

        // Combining/zero-width characters merge into previous cell
        let char_width = unicode_width::UnicodeWidthChar::width(ch);
        if char_width == Some(0) || char_width.is_none() {
            if self.cursor.x > 0 && self.cursor.y < self.rows {
                let prev_x = self.cursor.x - 1;
                // If previous cell is a continuation cell (empty string from wide char),
                // merge into the cell before it instead
                if self.grid[self.cursor.y][prev_x].ch.is_empty() && prev_x > 0 {
                    self.grid[self.cursor.y][prev_x - 1].ch.push(ch);
                } else {
                    self.grid[self.cursor.y][prev_x].ch.push(ch);
                }
            }
            return; // No cursor advance for zero-width characters
        }

        if self.cursor.x >= self.cols {
            // Line wrap
            self.cursor.x = 0;
            self.cursor.y += 1;
            if self.cursor.y >= self.rows {
                self.scroll_up();
                self.cursor.y = self.rows - 1;
            }
        }

        // Wide char boundary check: if a 2-cell char can't fit, pad and wrap
        let w = char_width.unwrap_or(1);
        if w == 2 && self.cursor.x + 1 >= self.cols {
            if self.cursor.y < self.rows && self.cursor.x < self.cols {
                self.grid[self.cursor.y][self.cursor.x] = Cell {
                    ch: " ".to_string(),
                    style: self.current_style,
                };
            }
            self.cursor.x = 0;
            self.cursor.y += 1;
            if self.cursor.y >= self.rows {
                self.scroll_up();
                self.cursor.y = self.rows - 1;
            }
        }

        if self.cursor.y < self.rows && self.cursor.x < self.cols {
            self.grid[self.cursor.y][self.cursor.x] = Cell {
                ch: ch.to_string(),
                style: self.current_style,
            };
        }

        self.cursor.x += 1;

        // Handle wide characters.
        //
        // This checked only `cursor.x < cols`. `cursor.y` was left over from
        // whatever the main write above had guarded against, so a restored
        // off-grid cursor plus one wide character (emoji, CJK) panicked here —
        // on the PTY READER thread, which ends the session with exit code 0.
        // A narrow character never panicked, which is why it survived.
        if w == 2 && self.cursor.y < self.rows && self.cursor.x < self.cols {
            // Mark next cell as continuation (empty string)
            self.grid[self.cursor.y][self.cursor.x] = Cell {
                ch: String::new(),
                style: self.current_style,
            };
            self.cursor.x += 1;
        }
    }

    fn parse_sgr(&mut self, params: &Params) {
        let mut iter = params.iter();

        while let Some(param) = iter.next() {
            let code = param[0];

            match code {
                0 => self.current_style = Style::default(),
                1 => self.current_style = self.current_style.bold(),
                2 => self.current_style = self.current_style.dim(),
                3 => self.current_style = self.current_style.italic(),
                4 => {
                    // Sub-parameter form (ECMA-48 / kitty): 4:0 disables the
                    // underline, 4:1..4:5 select a style. Reading only param[0]
                    // makes 4:0 turn underline ON and it can never be cleared.
                    if param.len() > 1 && param[1] == 0 {
                        self.current_style = self.current_style.not_underlined();
                    } else {
                        self.current_style = self.current_style.underlined();
                    }
                }
                21 => self.current_style = self.current_style.underlined(),
                7 => self.current_style = self.current_style.reversed(),
                8 => {
                    // Hidden - approximate with dim
                }
                9 => self.current_style = self.current_style.crossed_out(),
                22 => self.current_style = self.current_style.not_bold().not_dim(),
                23 => self.current_style = self.current_style.not_italic(),
                24 => self.current_style = self.current_style.not_underlined(),
                27 => self.current_style = self.current_style.not_reversed(),
                29 => self.current_style = self.current_style.not_crossed_out(),

                // Foreground colors
                30 => self.current_style = self.current_style.fg(Color::Black),
                31 => self.current_style = self.current_style.fg(Color::Red),
                32 => self.current_style = self.current_style.fg(Color::Green),
                33 => self.current_style = self.current_style.fg(Color::Yellow),
                34 => self.current_style = self.current_style.fg(Color::Blue),
                35 => self.current_style = self.current_style.fg(Color::Magenta),
                36 => self.current_style = self.current_style.fg(Color::Cyan),
                37 => self.current_style = self.current_style.fg(Color::Gray),
                38 => {
                    // Two wire forms. Semicolon: 38;5;N / 38;2;R;G;B -> the
                    // selector and components arrive as SEPARATE parameters.
                    // Colon: 38:5:N / 38:2::R:G:B -> they are SUB-parameters of
                    // this one. Consuming iter.next() in the colon case eats an
                    // unrelated parameter and desynchronises the whole sequence.
                    if param.len() > 1 {
                        match param[1] {
                            5 => {
                                if param.len() > 2 {
                                    self.current_style =
                                        self.current_style.fg(Color::Indexed(param[2] as u8));
                                }
                            }
                            2 => {
                                // 38:2::R:G:B carries an empty colour-space id,
                                // 38:2:R:G:B omits it.
                                let off = if param.len() >= 6 { 3 } else { 2 };
                                if param.len() >= off + 3 {
                                    self.current_style = self.current_style.fg(Color::Rgb(
                                        param[off] as u8,
                                        param[off + 1] as u8,
                                        param[off + 2] as u8,
                                    ));
                                }
                            }
                            _ => {}
                        }
                    } else if let Some(sub) = iter.next() {
                        match sub[0] {
                            5 => {
                                if let Some(idx) = iter.next() {
                                    self.current_style =
                                        self.current_style.fg(Color::Indexed(idx[0] as u8));
                                }
                            }
                            2 => {
                                let r = iter.next().map(|p| p[0] as u8).unwrap_or(0);
                                let g = iter.next().map(|p| p[0] as u8).unwrap_or(0);
                                let b = iter.next().map(|p| p[0] as u8).unwrap_or(0);
                                self.current_style = self.current_style.fg(Color::Rgb(r, g, b));
                            }
                            _ => {}
                        }
                    }
                }
                39 => self.current_style = self.current_style.fg(Color::Reset),
                58 => {
                    // Underline colour. We do not render it, but the semicolon
                    // form's components must still be consumed.
                    if param.len() == 1 {
                        if let Some(sub) = iter.next() {
                            match sub[0] {
                                5 => {
                                    iter.next();
                                }
                                2 => {
                                    iter.next();
                                    iter.next();
                                    iter.next();
                                }
                                _ => {}
                            }
                        }
                    }
                }
                59 => {}

                // Bright foreground colors
                90 => self.current_style = self.current_style.fg(Color::DarkGray),
                91 => self.current_style = self.current_style.fg(Color::LightRed),
                92 => self.current_style = self.current_style.fg(Color::LightGreen),
                93 => self.current_style = self.current_style.fg(Color::LightYellow),
                94 => self.current_style = self.current_style.fg(Color::LightBlue),
                95 => self.current_style = self.current_style.fg(Color::LightMagenta),
                96 => self.current_style = self.current_style.fg(Color::LightCyan),
                97 => self.current_style = self.current_style.fg(Color::White),

                // Background colors
                40 => self.current_style = self.current_style.bg(Color::Black),
                41 => self.current_style = self.current_style.bg(Color::Red),
                42 => self.current_style = self.current_style.bg(Color::Green),
                43 => self.current_style = self.current_style.bg(Color::Yellow),
                44 => self.current_style = self.current_style.bg(Color::Blue),
                45 => self.current_style = self.current_style.bg(Color::Magenta),
                46 => self.current_style = self.current_style.bg(Color::Cyan),
                47 => self.current_style = self.current_style.bg(Color::Gray),
                48 => {
                    // Two wire forms. Semicolon: 48;5;N / 48;2;R;G;B -> the
                    // selector and components arrive as SEPARATE parameters.
                    // Colon: 48:5:N / 48:2::R:G:B -> they are SUB-parameters of
                    // this one. Consuming iter.next() in the colon case eats an
                    // unrelated parameter and desynchronises the whole sequence.
                    if param.len() > 1 {
                        match param[1] {
                            5 => {
                                if param.len() > 2 {
                                    self.current_style =
                                        self.current_style.bg(Color::Indexed(param[2] as u8));
                                }
                            }
                            2 => {
                                // 38:2::R:G:B carries an empty colour-space id,
                                // 38:2:R:G:B omits it.
                                let off = if param.len() >= 6 { 3 } else { 2 };
                                if param.len() >= off + 3 {
                                    self.current_style = self.current_style.bg(Color::Rgb(
                                        param[off] as u8,
                                        param[off + 1] as u8,
                                        param[off + 2] as u8,
                                    ));
                                }
                            }
                            _ => {}
                        }
                    } else if let Some(sub) = iter.next() {
                        match sub[0] {
                            5 => {
                                if let Some(idx) = iter.next() {
                                    self.current_style =
                                        self.current_style.bg(Color::Indexed(idx[0] as u8));
                                }
                            }
                            2 => {
                                let r = iter.next().map(|p| p[0] as u8).unwrap_or(0);
                                let g = iter.next().map(|p| p[0] as u8).unwrap_or(0);
                                let b = iter.next().map(|p| p[0] as u8).unwrap_or(0);
                                self.current_style = self.current_style.bg(Color::Rgb(r, g, b));
                            }
                            _ => {}
                        }
                    }
                }
                49 => self.current_style = self.current_style.bg(Color::Reset),

                // Bright background colors
                100 => self.current_style = self.current_style.bg(Color::DarkGray),
                101 => self.current_style = self.current_style.bg(Color::LightRed),
                102 => self.current_style = self.current_style.bg(Color::LightGreen),
                103 => self.current_style = self.current_style.bg(Color::LightYellow),
                104 => self.current_style = self.current_style.bg(Color::LightBlue),
                105 => self.current_style = self.current_style.bg(Color::LightMagenta),
                106 => self.current_style = self.current_style.bg(Color::LightCyan),
                107 => self.current_style = self.current_style.bg(Color::White),

                _ => {}
            }
        }
    }

    fn erase_in_display(&mut self, mode: u16) {
        // Every sibling erase/insert/delete method has this prologue; this one
        // did not, and ED at 0 rows or with a stale cursor panicked the PTY
        // reader thread — which sets process_exited and quits Canopy outright,
        // taking the live Claude session with it.
        if self.rows == 0 || self.cols == 0 || self.cursor.y >= self.rows {
            return;
        }
        match mode {
            // Erase from cursor to end of screen
            0 => {
                // Clear rest of current line
                for c in self.cursor.x..self.cols {
                    self.grid[self.cursor.y][c] = Cell::default();
                }
                // Clear all lines below
                for r in (self.cursor.y + 1)..self.rows {
                    self.grid[r] = self.make_row();
                }
            }
            // Erase from start of screen to cursor
            1 => {
                // Clear all lines above
                for r in 0..self.cursor.y {
                    self.grid[r] = self.make_row();
                }
                // Clear start of current line to cursor
                for c in 0..=self.cursor.x.min(self.cols.saturating_sub(1)) {
                    self.grid[self.cursor.y][c] = Cell::default();
                }
            }
            // Erase entire screen
            2 | 3 => {
                for r in 0..self.rows {
                    self.grid[r] = self.make_row();
                }
            }
            _ => {}
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        // cols == 0 matters as well as rows: mode 1 uses an inclusive range
        // `0..=x.min(cols-1)`, which still visits column 0 of an empty row.
        if self.rows == 0 || self.cols == 0 || self.cursor.y >= self.rows {
            return;
        }
        match mode {
            // Erase from cursor to end of line
            0 => {
                for c in self.cursor.x..self.cols {
                    self.grid[self.cursor.y][c] = Cell::default();
                }
            }
            // Erase from start of line to cursor
            1 => {
                for c in 0..=self.cursor.x.min(self.cols.saturating_sub(1)) {
                    self.grid[self.cursor.y][c] = Cell::default();
                }
            }
            // Erase entire line
            2 => {
                self.grid[self.cursor.y] = self.make_row();
            }
            _ => {}
        }
    }

    fn insert_lines(&mut self, count: usize) {
        let bottom = self.scroll_bottom.min(self.grid.len());
        let count = count.min(self.rows);
        for _ in 0..count {
            if self.cursor.y >= self.scroll_top && self.cursor.y < bottom && bottom > 0 {
                // Remove bottom line of scroll region
                let remove_pos = (bottom - 1).min(self.grid.len().saturating_sub(1));
                self.grid.remove(remove_pos);
                // Insert blank line at cursor
                self.grid.insert(self.cursor.y, self.make_row());
            }
        }
    }

    fn delete_lines(&mut self, count: usize) {
        let bottom = self.scroll_bottom.min(self.grid.len());
        let count = count.min(self.rows);
        for _ in 0..count {
            if self.cursor.y >= self.scroll_top && self.cursor.y < bottom {
                self.grid.remove(self.cursor.y);
                // Insert blank line at bottom of scroll region
                let insert_pos = (bottom - 1).min(self.grid.len());
                self.grid.insert(insert_pos, self.make_row());
            }
        }
    }

    fn delete_chars(&mut self, count: usize) {
        if self.cursor.y >= self.rows {
            return;
        }
        // A CSI parameter is a u16, so this loops up to 65535 times without a
        // clamp -- each iteration an O(cols) Vec::remove plus a String free and
        // alloc. Measured: ESC[65535P took 1.9-2.2 s while HOLDING the vterm
        // mutex, against 0.07 ms for plain text. Operating past the end of the
        // line is a no-op in xterm, so clamping is behaviour-preserving.
        // insert_lines and delete_lines already clamped; these three did not.
        let count = count.min(self.cols);
        let row = &mut self.grid[self.cursor.y];
        for _ in 0..count {
            if self.cursor.x < row.len() {
                row.remove(self.cursor.x);
                row.push(Cell::default());
            }
        }
    }

    fn insert_chars(&mut self, count: usize) {
        let count = count.min(self.cols);
        if self.cursor.y >= self.rows {
            return;
        }
        let row = &mut self.grid[self.cursor.y];
        for _ in 0..count {
            if self.cursor.x < row.len() {
                row.insert(self.cursor.x, Cell::default());
                row.truncate(self.cols);
            }
        }
    }

    fn erase_chars(&mut self, count: usize) {
        let count = count.min(self.cols);
        if self.cursor.y >= self.rows {
            return;
        }
        for i in 0..count {
            let c = self.cursor.x + i;
            if c < self.cols {
                self.grid[self.cursor.y][c] = Cell::default();
            }
        }
    }

    fn enter_alternate_screen(&mut self) {
        // Entering must be idempotent: a second ESC[?1049h used to overwrite
        // the saved buffers with the ALT screen's, permanently destroying the
        // main screen the user came from.
        if self.saved_grid.is_some() {
            return;
        }
        // mem::take, not clone. The old code cloned the grid and the entire
        // scrollback and then immediately overwrote/cleared both -- measured at
        // 1.68 ms and 109,084 allocations with a full scrollback, for a copy
        // that was thrown away on the next line.
        // Retire the line numbers BEFORE taking the buffers, or the counts are
        // already zero. Both the scrollback and the grid are replaced, so every
        // currently addressable number must be retired: otherwise alt-screen
        // content reuses numbers a live selection still holds and the highlight
        // lands on unrelated text.
        self.lines_evicted += (self.scrollback.len() + self.grid.len()) as u64;
        self.saved_grid = Some(std::mem::take(&mut self.grid));
        self.saved_scrollback = Some(std::mem::take(&mut self.scrollback));
        self.saved_main_cursor = Some(std::mem::take(&mut self.cursor));
        self.grid = Self::make_grid(self.cols, self.rows);
        self.cursor = CursorState::default();
    }

    fn leave_alternate_screen(&mut self) {
        if let Some(grid) = self.saved_grid.take() {
            self.grid = grid;
        }
        if let Some(scrollback) = self.saved_scrollback.take() {
            // Same on the way out: restoring an older buffer would move
            // indices backwards, so retire everything the alt screen used.
            self.lines_evicted += (self.scrollback.len() + self.grid.len()) as u64;
            self.scrollback = scrollback;
        }
        if let Some(cursor) = self.saved_main_cursor.take().map(|c| self.clamp_cursor(c)) {
            self.cursor = cursor;
        }
    }
}

fn percent_decode(input: &str) -> String {
    let mut result = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Operate on bytes throughout. Slicing `input` by byte offsets panics
        // when the offset lands inside a multi-byte character, and OSC 7 URIs
        // can legitimately contain one (file://h/a%<U+20AC>b).
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = [bytes[i + 1], bytes[i + 2]];
            if let Some(val) = std::str::from_utf8(&hex)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                result.push(val);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

fn base64_decode(input: &str) -> Option<String> {
    fn decode_char(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let input: Vec<u8> = input
        .bytes()
        .filter(|&b| b != b'=' && b != b'\n' && b != b'\r')
        .collect();
    let mut output = Vec::with_capacity(input.len() * 3 / 4);

    for chunk in input.chunks(4) {
        let mut buf = [0u8; 4];
        let len = chunk.len();
        for (i, &byte) in chunk.iter().enumerate() {
            buf[i] = decode_char(byte)?;
        }

        output.push((buf[0] << 2) | (buf[1] >> 4));
        if len > 2 {
            output.push((buf[1] << 4) | (buf[2] >> 2));
        }
        if len > 3 {
            output.push((buf[2] << 6) | buf[3]);
        }
    }

    String::from_utf8(output).ok()
}

impl Perform for VirtualTerminal {
    fn print(&mut self, c: char) {
        self.saw_callback = true;
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        self.saw_callback = true;
        match byte {
            // BEL
            7 => {}
            // Backspace
            8 => {
                self.cursor.x = self.cursor.x.saturating_sub(1);
            }
            // Tab
            9 => {
                let tab_stop = ((self.cursor.x / 8) + 1) * 8;
                self.cursor.x = tab_stop.min(self.cols.saturating_sub(1));
            }
            // Line Feed / Vertical Tab / Form Feed
            10..=12 => {
                if self.cursor.y + 1 >= self.scroll_bottom {
                    // At the bottom of scroll region — scroll the region up
                    self.scroll_up();
                } else {
                    self.cursor.y += 1;
                }
            }
            // Carriage Return
            13 => {
                self.cursor.x = 0;
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        self.saw_callback = true;
        // DCS sequences - not needed for basic terminal emulation
    }

    fn put(&mut self, _byte: u8) {
        self.saw_callback = true;
        // DCS data bytes
    }

    fn unhook(&mut self) {
        self.saw_callback = true;
        // End of DCS sequence
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.saw_callback = true;
        // A completed OSC resets the accumulator; the counter only matters for
        // one that never terminates.
        self.osc_bytes = 0;
        if let Some(first) = params.first() {
            // OSC 7: Current working directory reporting
            // Format: OSC 7 ; file://hostname/path ST
            if *first == b"7" {
                if let Some(uri) = params.get(1) {
                    if let Ok(uri_str) = std::str::from_utf8(uri) {
                        // Strip "file://hostname" prefix
                        if let Some(path_str) = uri_str
                            .strip_prefix("file://")
                            .and_then(|s| s.find('/').map(|i| &s[i..]))
                        {
                            let decoded = percent_decode(path_str);
                            self.reported_cwd = Some(PathBuf::from(decoded));
                        }
                    }
                }
            }

            // OSC 52: Clipboard manipulation
            // Format: OSC 52 ; <selection> ; <base64-data> ST
            if *first == b"52" {
                if let Some(data_bytes) = params.get(2) {
                    if let Ok(data_str) = std::str::from_utf8(data_bytes) {
                        if let Some(decoded) = base64_decode(data_str) {
                            self.clipboard_requests.push(decoded);
                        }
                    }
                }
            }
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.saw_callback = true;
        // A CSI sequence carrying an intermediate or a private marker
        // ('?', '<', '=', '>', '!', '$', ' ', '#') shares its final byte with a
        // standard sequence but means something entirely different. vte routes
        // 0x3C..=0x3F into `intermediates` (vte-0.15.0/src/lib.rs:207), so the
        // marker is always visible here.
        //
        // Guarding arm by arm has already failed twice: ESC[>4;2m
        // (modifyOtherKeys) was executed as SGR 4;2 and underlined the whole
        // screen, and ESC[<u / ESC[>1u (kitty keyboard push/pop) were executed
        // as DECRC and teleported the cursor. Claude Code emits all three in
        // the first 50 bytes of every session.
        //
        // So the precondition is stated ONCE, here: private forms must be
        // opted into explicitly, and everything else requires no intermediate.
        let handled_private = matches!(
            (action, intermediates),
            ('h' | 'l', b"?") | ('c', b">") | ('q', b">") | ('u', b"?") | ('n', b"?")
        );
        if !intermediates.is_empty() && !handled_private {
            return;
        }
        let p: Vec<u16> = params.iter().map(|p| p[0]).collect();

        match action {
            // CUP / HVP - Cursor Position
            'H' | 'f' => {
                let row = p.first().copied().unwrap_or(1).max(1) as usize - 1;
                let col = p.get(1).copied().unwrap_or(1).max(1) as usize - 1;
                self.cursor.y = row.min(self.rows.saturating_sub(1));
                self.cursor.x = col.min(self.cols.saturating_sub(1));
            }
            // CUU - Cursor Up
            'A' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor.y = self.cursor.y.saturating_sub(n);
            }
            // CUD - Cursor Down
            'B' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor.y = (self.cursor.y + n).min(self.rows.saturating_sub(1));
            }
            // CUF - Cursor Forward
            'C' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor.x = (self.cursor.x + n).min(self.cols.saturating_sub(1));
            }
            // CUB - Cursor Backward
            'D' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor.x = self.cursor.x.saturating_sub(n);
            }
            // CNL - Cursor Next Line
            'E' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor.y = (self.cursor.y + n).min(self.rows.saturating_sub(1));
                self.cursor.x = 0;
            }
            // CPL - Cursor Previous Line
            'F' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.cursor.y = self.cursor.y.saturating_sub(n);
                self.cursor.x = 0;
            }
            // CHA - Cursor Horizontal Absolute
            'G' => {
                let col = p.first().copied().unwrap_or(1).max(1) as usize - 1;
                self.cursor.x = col.min(self.cols.saturating_sub(1));
            }
            // ED - Erase in Display
            'J' => {
                let mode = p.first().copied().unwrap_or(0);
                self.erase_in_display(mode);
            }
            // EL - Erase in Line
            'K' => {
                let mode = p.first().copied().unwrap_or(0);
                self.erase_in_line(mode);
            }
            // IL - Insert Lines
            'L' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.insert_lines(n);
            }
            // DL - Delete Lines
            'M' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.delete_lines(n);
            }
            // DCH - Delete Characters
            'P' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.delete_chars(n);
            }
            // SU - Scroll Up
            'S' => {
                let n = (p.first().copied().unwrap_or(1).max(1) as usize).min(self.rows);
                for _ in 0..n {
                    self.scroll_up();
                }
            }
            // SD - Scroll Down
            'T' => {
                let n = (p.first().copied().unwrap_or(1).max(1) as usize).min(self.rows);
                for _ in 0..n {
                    self.scroll_down();
                }
            }
            // ICH - Insert Characters
            '@' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.insert_chars(n);
            }
            // ECH - Erase Characters
            'X' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.erase_chars(n);
            }
            // VPA - Vertical Position Absolute
            'd' => {
                let row = p.first().copied().unwrap_or(1).max(1) as usize - 1;
                self.cursor.y = row.min(self.rows.saturating_sub(1));
            }
            // SGR - Select Graphic Rendition
            'm' => {
                // Only a BARE CSI ... m is SGR. Sequences carrying an
                // intermediate are private extensions that merely share the
                // final byte -- notably ESC[>4;2m (xterm modifyOtherKeys),
                // which Claude Code emits in its first 100 bytes. Feeding that
                // to the SGR parser sets underline(4) + dim(2) permanently.
                if intermediates.is_empty() {
                    self.parse_sgr(params);
                }
            }
            // DECSET / DECRST (private modes)
            'h' | 'l' => {
                if intermediates == b"?" {
                    let set = action == 'h';
                    for &code in &p {
                        match code {
                            25 => {
                                // DECTCEM - cursor visibility
                                self.cursor.visible = set;
                            }
                            1049 => {
                                // Alternate screen buffer (with save/restore cursor)
                                if set {
                                    self.enter_alternate_screen();
                                } else {
                                    self.leave_alternate_screen();
                                }
                            }
                            1047 | 47 => {
                                // Alternate screen (without save/restore cursor)
                                if set {
                                    self.enter_alternate_screen();
                                } else {
                                    self.leave_alternate_screen();
                                }
                            }
                            // 1004 = Focus event tracking
                            1004 => {
                                self.focus_tracking = set;
                            }
                            // Modes we acknowledge but don't need special handling for:
                            // 1 = DECCKM (cursor key mode), 7 = DECAWM (auto-wrap),
                            // 12 = blinking cursor, 1000/1002/1003/1006 = mouse modes,
                            // 2004 = bracketed paste
                            1 | 7 | 12 | 1000 | 1002 | 1003 | 1006 | 2004 => {
                                // Silently accept — these affect input handling,
                                // not our grid rendering
                            }
                            _ => {}
                        }
                    }
                }
            }
            // DECSC / DECRC via CSI s / CSI u
            's' if intermediates.is_empty() => {
                self.saved_cursor = Some(self.cursor.clone());
            }
            'u' if intermediates.is_empty() => {
                if let Some(ref saved) = self.saved_cursor {
                    self.cursor = self.clamp_cursor(saved.clone());
                }
            }
            // DECSTBM - Set Scrolling Region (top;bottom)
            'r' => {
                if intermediates.is_empty() {
                    if self.rows == 0 {
                        return;
                    }
                    let top = p.first().copied().unwrap_or(1).max(1) as usize - 1;
                    let bottom = p.get(1).copied().unwrap_or(self.rows as u16) as usize;
                    // top must leave room for at least one row below it.
                    // Clamping to `rows` (not `rows - 1`) let scroll_bottom be
                    // forced to rows + 1 by the .max() below, and scroll_up /
                    // scroll_down then indexed one past the grid.
                    self.scroll_top = top.min(self.rows - 1);
                    self.scroll_bottom = bottom.clamp(self.scroll_top + 1, self.rows);
                    // DECSTBM resets cursor to home
                    self.cursor.x = 0;
                    self.cursor.y = 0;
                }
            }
            // DSR - Device Status Report
            'n' => {
                let code = p.first().copied().unwrap_or(0);
                let private = intermediates == b"?";
                match code {
                    5 if !private => {
                        // DSR — respond "OK"
                        self.response_queue.push(b"\x1b[0n".to_vec());
                    }
                    6 => {
                        // CPR (1-indexed). The private form DECXCPR must be
                        // answered in kind, with the '?' retained.
                        let response = if private {
                            format!("\x1b[?{};{};1R", self.cursor.y + 1, self.cursor.x + 1)
                        } else {
                            format!("\x1b[{};{}R", self.cursor.y + 1, self.cursor.x + 1)
                        };
                        self.response_queue.push(response.into_bytes());
                    }
                    _ => {}
                }
            }
            // DA1 / DA2 — Device Attributes. Claude sends ESC[c at startup.
            // An unanswered capability probe makes us look like a broken
            // terminal to anything that asks.
            'c' => {
                if intermediates == b">" {
                    // DA2: VT220, firmware version, no cartridge.
                    self.response_queue.push(b"\x1b[>0;10;1c".to_vec());
                } else {
                    // DA1: VT220 with the feature set we actually implement.
                    self.response_queue
                        .push(b"\x1b[?62;1;2;6;9;15;22c".to_vec());
                }
            }
            // XTVERSION — Claude sends ESC[>0q at startup.
            'q' if intermediates == b">" => {
                self.response_queue
                    .push(b"\x1bP>|canopy(0.1.0)\x1b\\".to_vec());
            }
            // Kitty keyboard protocol flags query. We implement none of it, so
            // report zero rather than staying silent.
            'u' if intermediates == b"?" => {
                self.response_queue.push(b"\x1b[?0u".to_vec());
            }
            // SGR-Mouse, etc. - ignore
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.saw_callback = true;
        // Same class as csi_dispatch. ESC # 8 (DECALN) and ESC ( B (charset
        // selection) carry intermediates and share their final byte with
        // ESC 8 (DECRC) and ESC B. Discarding the intermediate meant DECALN
        // silently restored the cursor.
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            // IND - Index (move down, scroll if at bottom of scroll region)
            b'D' => {
                if self.cursor.y + 1 >= self.scroll_bottom {
                    self.scroll_up();
                } else {
                    self.cursor.y += 1;
                }
            }
            // RI - Reverse Index (move up, scroll if at top of scroll region)
            b'M' => {
                if self.cursor.y <= self.scroll_top {
                    self.scroll_down();
                } else {
                    self.cursor.y -= 1;
                }
            }
            // DECSC - Save Cursor
            b'7' => {
                self.saved_cursor = Some(self.cursor.clone());
            }
            // DECRC - Restore Cursor
            b'8' => {
                if let Some(ref saved) = self.saved_cursor {
                    self.cursor = self.clamp_cursor(saved.clone());
                }
            }
            // RIS - Full Reset
            b'c' => {
                let cols = self.cols;
                let rows = self.rows;
                let parser = self.parser.take();
                *self = Self::new(cols, rows);
                self.parser = parser;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_print() {
        let mut vt = VirtualTerminal::new(10, 5);
        vt.feed(b"Hello");
        assert_eq!(vt.grid[0][0].ch, "H");
        assert_eq!(vt.grid[0][1].ch, "e");
        assert_eq!(vt.grid[0][2].ch, "l");
        assert_eq!(vt.grid[0][3].ch, "l");
        assert_eq!(vt.grid[0][4].ch, "o");
        assert_eq!(vt.cursor.x, 5);
        assert_eq!(vt.cursor.y, 0);
    }

    #[test]
    fn test_newline() {
        let mut vt = VirtualTerminal::new(10, 5);
        vt.feed(b"AB\nCD");
        assert_eq!(vt.grid[0][0].ch, "A");
        assert_eq!(vt.grid[0][1].ch, "B");
        assert_eq!(vt.grid[1][2].ch, "C"); // LF moves down but not to col 0
        assert_eq!(vt.grid[1][3].ch, "D");
    }

    #[test]
    fn test_crlf() {
        let mut vt = VirtualTerminal::new(10, 5);
        vt.feed(b"AB\r\nCD");
        assert_eq!(vt.grid[0][0].ch, "A");
        assert_eq!(vt.grid[0][1].ch, "B");
        assert_eq!(vt.grid[1][0].ch, "C");
        assert_eq!(vt.grid[1][1].ch, "D");
    }

    #[test]
    fn test_cursor_movement() {
        let mut vt = VirtualTerminal::new(10, 5);
        // Move to row 3, col 5 (1-indexed)
        vt.feed(b"\x1b[3;5H");
        assert_eq!(vt.cursor.y, 2);
        assert_eq!(vt.cursor.x, 4);

        // Cursor up 1
        vt.feed(b"\x1b[AX");
        assert_eq!(vt.cursor.y, 1);
        assert_eq!(vt.grid[1][4].ch, "X");
    }

    #[test]
    fn test_erase_display() {
        let mut vt = VirtualTerminal::new(10, 3);
        vt.feed(b"AAAAAAAAAA");
        vt.feed(b"\r\nBBBBBBBBBB");
        vt.feed(b"\r\nCCCCCCCCCC");

        // Move to row 2, col 5 and erase below
        vt.feed(b"\x1b[2;5H");
        vt.feed(b"\x1b[0J");

        // Row 0 should be intact
        assert_eq!(vt.grid[0][0].ch, "A");
        // Row 1, cols 0-3 should be intact, 4+ cleared
        assert_eq!(vt.grid[1][3].ch, "B");
        assert_eq!(vt.grid[1][4].ch, " ");
        // Row 2 should be cleared
        assert_eq!(vt.grid[2][0].ch, " ");
    }

    #[test]
    fn test_erase_line() {
        let mut vt = VirtualTerminal::new(10, 3);
        vt.feed(b"ABCDEFGHIJ");
        // Move to col 5, erase from cursor to end of line
        vt.feed(b"\x1b[1;6H\x1b[0K");
        assert_eq!(vt.grid[0][4].ch, "E");
        assert_eq!(vt.grid[0][5].ch, " ");
        assert_eq!(vt.grid[0][9].ch, " ");
    }

    #[test]
    fn test_sgr_color() {
        let mut vt = VirtualTerminal::new(20, 5);
        // Red foreground
        vt.feed(b"\x1b[31mR");
        assert_eq!(vt.grid[0][0].ch, "R");
        assert_eq!(vt.grid[0][0].style.fg, Some(Color::Red));

        // Reset
        vt.feed(b"\x1b[0mN");
        assert_eq!(vt.grid[0][1].ch, "N");
        assert_eq!(vt.grid[0][1].style, Style::default());
    }

    #[test]
    fn test_scroll_on_overflow() {
        let mut vt = VirtualTerminal::new(5, 3);
        vt.feed(b"A\r\nB\r\nC\r\nD");
        // After 4 lines in a 3-row terminal, first line should be in scrollback
        assert_eq!(vt.scrollback.len(), 1);
        assert_eq!(vt.scrollback[0][0].ch, "A");
        assert_eq!(vt.grid[0][0].ch, "B");
        assert_eq!(vt.grid[1][0].ch, "C");
        assert_eq!(vt.grid[2][0].ch, "D");
    }

    #[test]
    fn test_line_wrap() {
        let mut vt = VirtualTerminal::new(5, 3);
        vt.feed(b"ABCDEFGH");
        assert_eq!(vt.grid[0][0].ch, "A");
        assert_eq!(vt.grid[0][4].ch, "E");
        assert_eq!(vt.grid[1][0].ch, "F");
        assert_eq!(vt.grid[1][2].ch, "H");
    }

    #[test]
    fn test_alternate_screen() {
        let mut vt = VirtualTerminal::new(10, 3);
        vt.feed(b"Main screen");

        // Enter alternate screen
        vt.feed(b"\x1b[?1049h");
        assert_eq!(vt.grid[0][0].ch, " "); // Should be blank
        vt.feed(b"Alt screen");

        // Leave alternate screen
        vt.feed(b"\x1b[?1049l");
        assert_eq!(vt.grid[0][0].ch, "M");
        assert_eq!(vt.grid[0][1].ch, "a");
    }

    #[test]
    fn test_resize() {
        let mut vt = VirtualTerminal::new(10, 5);
        vt.feed(b"Hello");
        vt.resize(5, 3);
        assert_eq!(vt.cols, 5);
        assert_eq!(vt.rows, 3);
        assert_eq!(vt.grid[0][0].ch, "H");
        assert_eq!(vt.grid[0][4].ch, "o");
    }

    #[test]
    fn test_cursor_visibility() {
        let mut vt = VirtualTerminal::new(10, 5);
        assert!(vt.cursor.visible);
        vt.feed(b"\x1b[?25l");
        assert!(!vt.cursor.visible);
        vt.feed(b"\x1b[?25h");
        assert!(vt.cursor.visible);
    }

    #[test]
    fn test_tab() {
        let mut vt = VirtualTerminal::new(20, 5);
        vt.feed(b"A\tB");
        assert_eq!(vt.grid[0][0].ch, "A");
        assert_eq!(vt.cursor.x, 9); // 'B' at col 8, cursor at 9
        assert_eq!(vt.grid[0][8].ch, "B");
    }

    #[test]
    fn test_backspace() {
        let mut vt = VirtualTerminal::new(10, 5);
        vt.feed(b"AB\x08C");
        // Backspace moves cursor back, 'C' overwrites 'B'
        assert_eq!(vt.grid[0][0].ch, "A");
        assert_eq!(vt.grid[0][1].ch, "C");
    }

    #[test]
    fn test_carriage_return_overwrite() {
        let mut vt = VirtualTerminal::new(10, 5);
        vt.feed(b"Hello\rWorld");
        assert_eq!(vt.grid[0][0].ch, "W");
        assert_eq!(vt.grid[0][1].ch, "o");
        assert_eq!(vt.grid[0][2].ch, "r");
        assert_eq!(vt.grid[0][3].ch, "l");
        assert_eq!(vt.grid[0][4].ch, "d");
    }

    #[test]
    fn test_delete_chars() {
        let mut vt = VirtualTerminal::new(10, 3);
        vt.feed(b"ABCDEF");
        // Move to col 2, delete 2 chars
        vt.feed(b"\x1b[1;3H\x1b[2P");
        assert_eq!(vt.grid[0][0].ch, "A");
        assert_eq!(vt.grid[0][1].ch, "B");
        assert_eq!(vt.grid[0][2].ch, "E");
        assert_eq!(vt.grid[0][3].ch, "F");
    }

    #[test]
    fn test_insert_lines() {
        let mut vt = VirtualTerminal::new(5, 3);
        vt.feed(b"A\r\nB\r\nC");
        // Move to row 2, insert 1 line
        vt.feed(b"\x1b[2;1H\x1b[1L");
        assert_eq!(vt.grid[0][0].ch, "A");
        assert_eq!(vt.grid[1][0].ch, " "); // Inserted blank line
        assert_eq!(vt.grid[2][0].ch, "B"); // Pushed down
    }

    #[test]
    fn test_wide_char_at_boundary() {
        // 6-column terminal: wide char at col 5 (last col) should wrap
        let mut vt = VirtualTerminal::new(6, 3);
        vt.feed("ABCDE".as_bytes());
        // Cursor at col 5 (last col). Write a wide char '한' (2 cells)
        vt.feed("한".as_bytes());
        // Col 5 should be padded with space, '한' should be on next line
        assert_eq!(vt.grid[0][5].ch, " ");
        assert_eq!(vt.grid[1][0].ch, "한");
        assert_eq!(vt.grid[1][1].ch, ""); // continuation cell
    }

    #[test]
    fn test_wide_char_fits() {
        // Wide char at col 4 of 6-col terminal should fit
        let mut vt = VirtualTerminal::new(6, 3);
        vt.feed("ABCD".as_bytes());
        vt.feed("한".as_bytes());
        assert_eq!(vt.grid[0][4].ch, "한");
        assert_eq!(vt.grid[0][5].ch, ""); // continuation cell
    }

    fn cell_at(vt: &VirtualTerminal, row: usize, col: usize) -> Cell {
        vt.grid()[row][col].clone()
    }

    #[test]
    fn test_sgr_subparam_underline_off() {
        let mut vt = VirtualTerminal::new(80, 24);
        // Curly underline on, then the SUB-PARAMETER form of underline-off.
        // Reading only param[0] makes both look like plain `4`, so the
        // underline sticks on for the rest of the session.
        vt.feed(b"\x1b[4:3mA\x1b[4:0mB");
        let a = cell_at(&vt, 0, 0);
        let b = cell_at(&vt, 0, 1);
        assert_eq!(a.ch, "A");
        assert_eq!(b.ch, "B");
        assert!(
            a.style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "4:3 should underline"
        );
        assert!(
            !b.style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "4:0 must clear the underline"
        );
    }

    #[test]
    fn test_sgr_colon_truecolor_does_not_desync() {
        let mut vt = VirtualTerminal::new(80, 24);
        // Colon-form truecolor carries its components as SUB-parameters.
        // Consuming iter.next() here eats the following `1` (bold) and
        // desynchronises everything after it.
        vt.feed(b"\x1b[38:2::255:0:0;1mX");
        let x = cell_at(&vt, 0, 0);
        assert_eq!(x.ch, "X");
        assert_eq!(
            x.style.fg,
            Some(ratatui::style::Color::Rgb(255, 0, 0)),
            "colon-form truecolor must be parsed"
        );
        assert!(
            x.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "the parameter after a colon-form colour must not be swallowed"
        );
    }

    /// The invariant every grid-mutating path must preserve. Indices were being
    /// validated against `self.rows`/`self.cols` while nothing guaranteed the
    /// grid actually had those dimensions.
    fn assert_grid_shape(vt: &VirtualTerminal, ctx: &str) {
        assert_eq!(vt.grid().len(), vt.rows(), "row count wrong after {ctx}");
        for (i, row) in vt.grid().iter().enumerate() {
            assert_eq!(row.len(), vt.cols(), "row {i} width wrong after {ctx}");
        }
    }

    #[test]
    fn resize_normalises_scrollback_width() {
        // resize() rebuilt `grid` but left scrollback rows at the OLD width,
        // while callers indexed them with the NEW one. Widening then selecting
        // in the scrollback panicked on the MAIN thread.
        let mut vt = VirtualTerminal::new(40, 5);
        for i in 0..16 {
            vt.feed(format!("row{i}\r\n").as_bytes());
        }
        assert!(!vt.scrollback().is_empty());
        vt.resize(120, 30);
        for (i, row) in vt.scrollback().iter().enumerate() {
            assert_eq!(row.len(), 120, "scrollback row {i} kept the old width");
        }
        assert_grid_shape(&vt, "resize with scrollback");
    }

    #[test]
    fn alt_screen_survives_a_resize_while_active() {
        // Entering alt screen saved the grid, a resize rebuilt only the live
        // grid, and leaving restored a saved grid of the OLD width. Printing at
        // the new right margin then indexed past the row.
        let mut vt = VirtualTerminal::new(80, 24);
        vt.feed(b"\x1b[?1049h");
        vt.resize(100, 30);
        vt.feed(b"\x1b[?1049l");
        vt.feed(b"\x1b[1;95HZ");
        assert_grid_shape(&vt, "alt screen resize round trip");
    }

    #[test]
    fn normal_white_is_not_bright_white() {
        // ratatui-core-0.1.2/src/style/color.rs:87,107 —
        //   Gray  = "ANSI Color: White. Foreground: 37, Background: 47"
        //   White = "ANSI Color: Bright White. Foreground: 97, Background: 107"
        // Mapping 37 to White rendered normal white as bright and made 37 and
        // 97 indistinguishable. Claude's palette leans on the normal set.
        use ratatui::style::Color;
        let mut vt = VirtualTerminal::new(10, 2);
        vt.feed(b"\x1b[37mA\x1b[97mB\x1b[47mC\x1b[107mD");
        let g = vt.grid();
        assert_eq!(
            g[0][0].style.fg,
            Some(Color::Gray),
            "SGR 37 is normal white"
        );
        assert_eq!(
            g[0][1].style.fg,
            Some(Color::White),
            "SGR 97 is bright white"
        );
        assert_ne!(g[0][0].style.fg, g[0][1].style.fg, "37 and 97 must differ");
        assert_eq!(
            g[0][2].style.bg,
            Some(Color::Gray),
            "SGR 47 is normal white bg"
        );
        assert_eq!(
            g[0][3].style.bg,
            Some(Color::White),
            "SGR 107 is bright white bg"
        );
    }

    #[test]
    fn private_and_intermediate_sequences_are_inert() {
        // The alias bug class, pinned. Every one of these shares a final byte
        // with a standard sequence but means something else. Two have already
        // shipped as screen-corrupting bugs (ESC[>4;2m, ESC[<u).
        use ratatui::style::Modifier;
        for seq in [
            &b"\x1b[>4;2m"[..], // modifyOtherKeys, not SGR 4;2
            b"\x1b[<u",         // kitty pop,        not DECRC
            b"\x1b[>1u",        // kitty push,       not DECRC
            b"\x1b[?1049s",     // XTSAVE,           not DECSC
            b"\x1b[=1c",        // DA3,              not DA1
            b"\x1b[>2J",        // private,          not ED
            b"\x1b[?2K",        // private,          not EL
            b"\x1b[!p",         // DECSTR,           not unknown
            b"\x1b[ q",         // DECSCUSR,         not XTVERSION
            b"\x1b#8",          // DECALN,           not DECRC
            b"\x1b(B",          // charset select,   not ESC B
        ] {
            let mut vt = VirtualTerminal::new(20, 5);
            vt.feed(b"\x1b[3;7H");
            let (x, y) = (vt.cursor().x, vt.cursor().y);
            vt.feed(seq);
            vt.feed(b"A");
            let cell = &vt.grid()[y][x];
            assert_eq!(cell.ch, "A", "{seq:?} moved the cursor");
            assert!(
                !cell.style.add_modifier.contains(Modifier::UNDERLINED)
                    && !cell.style.add_modifier.contains(Modifier::DIM)
                    && !cell.style.add_modifier.contains(Modifier::BOLD),
                "{seq:?} changed the pen"
            );
            assert_grid_shape(&vt, &format!("{seq:?}"));
        }
    }

    #[test]
    fn decset_still_works_through_the_guard() {
        // The guard must not make the ONE private form we do implement inert.
        let mut vt = VirtualTerminal::new(20, 5);
        vt.feed(b"\x1b[?25l");
        assert!(!vt.cursor().visible, "DECRST 25 should hide the cursor");
        vt.feed(b"\x1b[?25h");
        assert!(vt.cursor().visible, "DECSET 25 should show the cursor");
    }

    #[test]
    fn capability_queries_get_answered() {
        // Claude sends ESC[>0q and ESC[c in the last 8 bytes of startup.
        // Silence makes us look like a broken terminal to any probe.
        let mut vt = VirtualTerminal::new(20, 5);
        vt.feed(b"\x1b[c");
        vt.feed(b"\x1b[>0q");
        vt.feed(b"\x1b[>c");
        vt.feed(b"\x1b[?u");
        let r = vt.take_responses();
        assert_eq!(r.len(), 4, "every query must be answered");
        assert!(r[0].starts_with(b"\x1b[?62;"), "DA1: {:?}", r[0]);
        assert!(r[1].starts_with(b"\x1bP>|canopy"), "XTVERSION: {:?}", r[1]);
        assert!(r[2].starts_with(b"\x1b[>0;"), "DA2: {:?}", r[2]);
        assert_eq!(r[3], b"\x1b[?0u", "kitty flags query");
    }

    #[test]
    fn real_claude_startup_is_answered() {
        // The fixture ends with the XTVERSION + DA1 probe pair.
        let raw = include_bytes!("../tests/fixtures/claude-startup.raw");
        let mut vt = VirtualTerminal::new(100, 30);
        vt.feed(raw);
        assert!(
            !vt.take_responses().is_empty(),
            "canopy answered none of Claude's startup probes"
        );
    }

    #[test]
    fn a_restored_cursor_can_never_be_off_grid() {
        // A saved cursor outlives the geometry it was captured in. resize()
        // clamped the LIVE cursor but not the saved ones, and the wide-char
        // continuation write checked x but not y -- so alt-screen exit, DECRC
        // or CSI u after a shrink, plus one emoji, panicked on the PTY reader
        // thread and ended the session with exit code 0.
        //
        // Narrow characters never panicked, which is why this survived.
        let wide = ["\u{1F600}", "\u{4F60}", "\u{1F44D}"];
        for save in [&b"\x1b[?1049h"[..], b"\x1b7", b"\x1b[s"] {
            for restore in [&b"\x1b[?1049l"[..], b"\x1b8", b"\x1b[u"] {
                for ch in wide {
                    let mut vt = VirtualTerminal::new(140, 48);
                    vt.feed(b"\x1b[45;1H");
                    vt.feed(save);
                    vt.resize(140, 20);
                    vt.feed(restore);
                    vt.feed(ch.as_bytes());
                    vt.feed(b"AB");
                    assert!(vt.cursor().y < vt.rows(), "cursor y off grid");
                    assert!(vt.cursor().x <= vt.cols(), "cursor x off grid");
                    assert_grid_shape(&vt, "restore after shrink");
                }
            }
        }
    }

    #[test]
    fn reset_after_panic_returns_a_usable_terminal() {
        // The breaker for an emulator panic: drop everything derived from the
        // byte stream rather than carry corrupt state forward, and keep going.
        let mut vt = VirtualTerminal::new(80, 24);
        for i in 0..50 {
            vt.feed(format!("line{i}\r\n").as_bytes());
        }
        vt.feed(b"\x1b[10;10H\x1b[31m");
        let evicted_before = vt.lines_evicted();

        vt.reset_after_panic(80, 24);

        assert_grid_shape(&vt, "after reset");
        assert_eq!(vt.cursor().x, 0);
        assert_eq!(vt.cursor().y, 0);
        assert!(vt.scrollback().is_empty());
        assert_eq!(vt.scroll_offset(), 0);
        assert!(vt.take_responses().is_empty());
        // Line numbers must not be reused, or a live selection would land on
        // new text.
        assert!(vt.lines_evicted() > evicted_before);
        // And it still works.
        vt.feed(b"hello");
        assert_eq!(vt.grid()[0][0].ch, "h");
    }

    #[test]
    fn alt_screen_is_detectable_so_the_wheel_can_be_translated() {
        // Claude Code lives on the alt screen, verified from a real capture:
        // ESC[?1049h appears and is never followed by ESC[?1049l. Alt screen
        // has no scrollback by design, so a wheel event must become arrow keys
        // for the app to handle -- which is what every real terminal does.
        let mut vt = VirtualTerminal::new(80, 24);
        assert!(!vt.in_alternate_screen());
        vt.feed(b"\x1b[?1049h");
        assert!(vt.in_alternate_screen(), "alt screen must be observable");
        vt.feed(b"\x1b[?1049l");
        assert!(!vt.in_alternate_screen());
    }

    #[test]
    fn alt_screen_has_no_scrollback_to_scroll() {
        // The reason the wheel appeared broken: we were moving an offset over a
        // buffer that is always empty while the child is on the alt screen.
        let mut vt = VirtualTerminal::new(40, 5);
        for i in 0..40 {
            vt.feed(format!("main{i}\r\n").as_bytes());
        }
        assert!(
            !vt.scrollback().is_empty(),
            "main screen accrues scrollback"
        );

        vt.feed(b"\x1b[?1049h");
        assert!(vt.scrollback().is_empty(), "alt screen starts clean");
        for i in 0..40 {
            vt.feed(format!("alt{i}\r\n").as_bytes());
        }
        vt.set_scroll_offset(10);
        assert_eq!(
            vt.scroll_offset(),
            0,
            "there is nothing to scroll back to on the alt screen"
        );
    }

    #[test]
    fn real_claude_output_enters_the_alternate_screen() {
        // Captured from a live session through a PTY. This is the evidence for
        // translating the wheel into arrow keys: Claude is an alt-screen
        // application, so there is no scrollback for the wheel to move through,
        // exactly as in vim or less.
        let raw = include_bytes!("../tests/fixtures/claude-altscreen.raw");
        let mut vt = VirtualTerminal::new(100, 24);
        vt.feed(raw);
        assert!(
            vt.in_alternate_screen(),
            "Claude Code was expected to be on the alternate screen"
        );
        assert!(
            vt.scrollback().is_empty(),
            "the alt screen must not accrue scrollback"
        );
    }

    #[test]
    fn an_escape_flood_cannot_hold_the_lock_for_seconds() {
        // ICH/DCH took a raw u16 CSI parameter and looped up to 65535 times,
        // each an O(cols) Vec::remove plus a String free/alloc -- measured at
        // 1.9-2.2 s per sequence WHILE HOLDING the vterm mutex the main thread
        // needs to render. A 1 MiB file of them rendered 20 frames in 62 s.
        let mut vt = VirtualTerminal::new(140, 48);
        let flood: Vec<u8> = b"\x1b[65535@\x1b[65535P\x1b[65535X".repeat(400).to_vec();
        let start = Instant::now();
        vt.feed(&flood);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(250),
            "an escape flood held the lock for {elapsed:?}"
        );
        assert_grid_shape(&vt, "after an escape flood");
    }

    #[test]
    fn clamping_is_behaviour_preserving_for_real_counts() {
        // xterm treats operating past the end of the line as a no-op, so the
        // clamp must not change any legitimate result.
        let mut vt = VirtualTerminal::new(10, 3);
        vt.feed(b"ABCDEFGHIJ\x1b[1;1H\x1b[3P");
        let row: String = vt.grid()[0].iter().map(|c| c.ch.as_str()).collect();
        assert_eq!(row.trim_end(), "DEFGHIJ", "DCH 3 should delete three cells");

        let mut vt = VirtualTerminal::new(10, 3);
        vt.feed(b"ABCDEFGHIJ\x1b[1;1H\x1b[99P");
        let row: String = vt.grid()[0].iter().map(|c| c.ch.as_str()).collect();
        assert_eq!(row.trim_end(), "", "DCH past the line clears it");
    }

    #[test]
    fn an_unterminated_osc_does_not_swallow_the_session() {
        // vte's MAX_OSC_RAW guard is cfg(not(std)) and compiled out here, so
        // its buffer has no ceiling. The realistic failure is not OOM: while
        // the parser sits in OscString every byte vanishes, so the pane stops
        // updating forever while the app keeps drawing -- a hang, to the user.
        let mut vt = VirtualTerminal::new(80, 24);
        vt.feed(b"\x1b]52;c;");
        let junk = vec![b'A'; 1024 * 1024];
        for _ in 0..12 {
            vt.feed(&junk);
        }
        // The parser must have been reset, so ordinary text renders again.
        vt.feed(b"\x1b[2J\x1b[1;1HRECOVERED");
        let row: String = vt.grid()[0].iter().map(|c| c.ch.as_str()).collect();
        assert!(
            row.starts_with("RECOVERED"),
            "the pane never recovered from an unterminated OSC: {row:?}"
        );
        assert!(vt.escape_floods() > 0, "the discard should be counted");
    }

    #[test]
    fn a_legitimate_osc_still_works() {
        // The cap must not break real clipboard writes.
        let mut vt = VirtualTerminal::new(80, 24);
        vt.feed(b"\x1b]52;c;SGVsbG8=\x1b\\");
        assert_eq!(vt.take_clipboard_requests(), vec!["Hello".to_string()]);
        assert_eq!(vt.escape_floods(), 0, "a valid OSC must not be discarded");
    }

    #[test]
    fn every_scroll_region_survives_scrolling() {
        // 66 of these panicked: DECSTBM clamped top to `rows` instead of
        // `rows - 1`, so scroll_bottom was forced to rows + 1 and scroll_up /
        // scroll_down indexed one past the grid. On the PTY reader thread that
        // is a crash that takes the Claude session with it.
        const ROWS: usize = 30;
        for top in 0..=ROWS + 2 {
            for bottom in 0..=ROWS + 2 {
                let mut vt = VirtualTerminal::new(80, ROWS);
                vt.feed(format!("\x1b[{top};{bottom}r").as_bytes());
                for _ in 0..ROWS + 5 {
                    vt.feed(b"\n");
                }
                vt.feed(b"\x1bM\x1bM");
                vt.feed(b"\x1b[S\x1b[T");
                assert_grid_shape(&vt, &format!("ESC[{top};{bottom}r"));
            }
        }
    }

    #[test]
    fn degenerate_geometry_survives_every_erase_and_edit() {
        // rows == 0 is reachable in the real app: ui/mod.rs sizes the vterm to
        // Block::inner() every frame, so a 2-row-tall window yields zero rows.
        for (cols, rows) in [(0usize, 24usize), (78, 0), (0, 0), (1, 1)] {
            for seq in [
                &b"\x1b[J"[..],
                b"\x1b[1J",
                b"\x1b[2J",
                b"\x1b[3J",
                b"\x1b[K",
                b"\x1b[1K",
                b"\x1b[2K",
                b"\x1b[P",
                b"\x1b[@",
                b"\x1b[X",
                b"\x1b[L",
                b"\x1b[M",
                b"\x1b[S",
                b"\x1b[T",
                b"\x1bM",
                b"\x1bD",
                b"\x1b[1;1H",
                b"\x1b[99;99H",
                b"\x1b[999X",
                b"\x1b[r",
            ] {
                let mut vt = VirtualTerminal::new(cols, rows);
                vt.feed(seq);
                vt.feed(b"A");
                assert_grid_shape(&vt, &format!("{cols}x{rows} {seq:?}"));
            }
        }
    }

    #[test]
    fn osc7_percent_decode_handles_multibyte() {
        // Slicing a &str by byte offsets panics when the offset lands inside a
        // multi-byte character, and an OSC 7 URI can contain one.
        let mut vt = VirtualTerminal::new(80, 24);
        vt.feed("\x1b]7;file://h/a%\u{20AC}b\x07".as_bytes());
        vt.feed("\x1b]7;file://h/%E2%82%AC\x07".as_bytes());
        vt.feed(b"\x1b]7;file://h/trailing%\x07");
        vt.feed(b"\x1b]7;file://h/short%A\x07");
        assert_grid_shape(&vt, "osc7 percent decoding");
    }

    #[test]
    fn test_private_csi_m_is_not_sgr() {
        let mut vt = VirtualTerminal::new(80, 24);
        // ESC[>4;2m is xterm modifyOtherKeys, NOT SGR. It shares the final
        // byte but carries a '>' intermediate. Claude Code emits it within the
        // first 100 bytes of a session; parsing it as SGR sets underline(4)
        // and dim(2) permanently for every cell that follows.
        vt.feed(b"\x1b[>4;2mA");
        let a = vt.grid()[0][0].clone();
        assert_eq!(a.ch, "A");
        assert!(
            !a.style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "ESC[>4;2m must not underline"
        );
        assert!(
            !a.style.add_modifier.contains(ratatui::style::Modifier::DIM),
            "ESC[>4;2m must not dim"
        );
    }

    #[test]
    fn test_replay_real_claude_startup() {
        // Golden master: replay bytes captured from a real Claude Code session
        // through a PTY. Nothing in a plain startup screen should be underlined.
        let raw = include_bytes!("../tests/fixtures/claude-startup.raw");
        let mut vt = VirtualTerminal::new(100, 30);
        vt.feed(raw);
        let mut underlined = 0;
        let mut dim = 0;
        for row in vt.grid() {
            for cell in row {
                if cell.ch.trim().is_empty() {
                    continue;
                }
                if cell
                    .style
                    .add_modifier
                    .contains(ratatui::style::Modifier::UNDERLINED)
                {
                    underlined += 1;
                }
                if cell
                    .style
                    .add_modifier
                    .contains(ratatui::style::Modifier::DIM)
                {
                    dim += 1;
                }
            }
        }
        assert_eq!(
            underlined, 0,
            "{} cells wrongly underlined after real startup",
            underlined
        );
        assert_eq!(dim, 0, "{} cells wrongly dimmed after real startup", dim);
    }

    #[test]
    fn test_osc52_clipboard() {
        let mut vt = VirtualTerminal::new(80, 24);
        // OSC 52 with base64 "SGVsbG8=" = "Hello"
        vt.feed(b"\x1b]52;c;SGVsbG8=\x1b\\");
        let requests = vt.take_clipboard_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0], "Hello");
    }

    #[test]
    fn test_focus_tracking() {
        let mut vt = VirtualTerminal::new(80, 24);
        // Focus tracking is off by default
        assert!(!vt.focus_tracking_enabled());
        // DECSET 1004 enables focus tracking
        vt.feed(b"\x1b[?1004h");
        assert!(vt.focus_tracking_enabled());
        // Focus event sequences should be silently consumed (no visible output)
        vt.feed(b"\x1b[I");
        assert_eq!(vt.grid[0][0].ch, " ");
        vt.feed(b"\x1b[O");
        assert_eq!(vt.grid[0][0].ch, " ");
        // DECRST 1004 disables focus tracking
        vt.feed(b"\x1b[?1004l");
        assert!(!vt.focus_tracking_enabled());
    }

    #[test]
    fn test_feed_with_zero_sized_terminal_does_not_panic() {
        let mut vt = VirtualTerminal::new(80, 24);
        vt.resize(0, 0);
        vt.feed(b"hello");
        vt.feed("한".as_bytes());
        assert_eq!(vt.cols, 0);
        assert_eq!(vt.rows, 0);
    }

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
}

#[cfg(test)]
mod selection_anchor_tests {
    use super::*;

    /// Absolute line numbers must survive scrollback eviction, or a selection
    /// silently slides onto different text once the buffer wraps.
    #[test]
    fn absolute_lines_survive_eviction() {
        let mut vt = VirtualTerminal::new(20, 3);
        for i in 0..10 {
            vt.feed(format!("line{i}\r\n").as_bytes());
        }
        let before_evicted = vt.lines_evicted();
        let first_line_abs = vt.absolute_line(0);
        assert_eq!(first_line_abs, before_evicted);

        for i in 0..MAX_SCROLLBACK + 50 {
            vt.feed(format!("more{i}\r\n").as_bytes());
        }
        assert!(vt.lines_evicted() > before_evicted, "nothing was evicted");
        assert_eq!(vt.absolute_line(0), vt.lines_evicted());
        assert!(vt.absolute_line(0) > first_line_abs);
    }

    #[test]
    fn top_view_index_tracks_the_scroll_position() {
        let mut vt = VirtualTerminal::new(20, 4);
        for i in 0..20 {
            vt.feed(format!("l{i}\r\n").as_bytes());
        }
        let sb = vt.scrollback().len();
        assert!(sb > 0);

        assert_eq!(vt.scroll_offset(), 0);
        assert_eq!(vt.top_view_index(4), sb);

        vt.set_scroll_offset(3);
        let total = sb + vt.grid().len();
        assert_eq!(vt.top_view_index(4), total - 3 - 4);
    }

    #[test]
    fn clearing_scrollback_does_not_reuse_line_numbers() {
        // Alt-screen entry clears scrollback. Without bumping the counter the
        // new content reuses numbers a live selection still holds.
        let mut vt = VirtualTerminal::new(20, 3);
        for i in 0..30 {
            vt.feed(format!("a{i}\r\n").as_bytes());
        }
        let highest = vt.absolute_line(vt.scrollback().len() + vt.grid().len());
        vt.feed(b"\x1b[?1049h");
        for i in 0..30 {
            vt.feed(format!("b{i}\r\n").as_bytes());
        }
        assert!(
            vt.absolute_line(0) >= highest,
            "line numbers were reused after the buffer was cleared"
        );
    }
}
