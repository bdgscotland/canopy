use ratatui::prelude::*;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
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
    pub fn feed(&mut self, bytes: &[u8]) {
        // Take the parser out temporarily to avoid double borrow
        let mut parser = self.parser.take().unwrap_or_default();
        parser.advance(self, bytes);
        self.parser = Some(parser);
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

    pub fn scrollback(&self) -> &VecDeque<Vec<Cell>> {
        &self.scrollback
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn set_scroll_offset(&mut self, offset: usize) {
        self.scroll_offset = offset.min(self.scrollback.len());
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
        // Only push to scrollback if scrolling from the very top of the screen
        if self.scroll_top == 0 {
            self.scrollback.push_back(removed);
            if self.scrollback.len() > MAX_SCROLLBACK {
                self.scrollback.pop_front();
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

        // Handle wide characters
        if w == 2 && self.cursor.x < self.cols {
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
                37 => self.current_style = self.current_style.fg(Color::White),
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
                                5 => { iter.next(); }
                                2 => { iter.next(); iter.next(); iter.next(); }
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
                47 => self.current_style = self.current_style.bg(Color::White),
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
        let row = &mut self.grid[self.cursor.y];
        for _ in 0..count {
            if self.cursor.x < row.len() {
                row.remove(self.cursor.x);
                row.push(Cell::default());
            }
        }
    }

    fn insert_chars(&mut self, count: usize) {
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
        self.saved_grid = Some(self.grid.clone());
        self.saved_scrollback = Some(self.scrollback.clone());
        self.saved_main_cursor = Some(self.cursor.clone());
        self.grid = Self::make_grid(self.cols, self.rows);
        self.scrollback.clear();
        self.cursor = CursorState::default();
    }

    fn leave_alternate_screen(&mut self) {
        if let Some(grid) = self.saved_grid.take() {
            self.grid = grid;
        }
        if let Some(scrollback) = self.saved_scrollback.take() {
            self.scrollback = scrollback;
        }
        if let Some(cursor) = self.saved_main_cursor.take() {
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
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
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
        // DCS sequences - not needed for basic terminal emulation
    }

    fn put(&mut self, _byte: u8) {
        // DCS data bytes
    }

    fn unhook(&mut self) {
        // End of DCS sequence
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
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
            's' => {
                self.saved_cursor = Some(self.cursor.clone());
            }
            'u' => {
                if let Some(ref saved) = self.saved_cursor {
                    self.cursor = saved.clone();
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
                match code {
                    5 => {
                        // Status report — respond "OK"
                        self.response_queue.push(b"\x1b[0n".to_vec());
                    }
                    6 => {
                        // CPR — Cursor Position Report (1-indexed)
                        let response = format!("\x1b[{};{}R", self.cursor.y + 1, self.cursor.x + 1);
                        self.response_queue.push(response.into_bytes());
                    }
                    _ => {}
                }
            }
            // SGR-Mouse, etc. - ignore
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
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
                    self.cursor = saved.clone();
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
        assert!(a.style.add_modifier.contains(ratatui::style::Modifier::UNDERLINED),
                "4:3 should underline");
        assert!(!b.style.add_modifier.contains(ratatui::style::Modifier::UNDERLINED),
                "4:0 must clear the underline");
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
        assert_eq!(x.style.fg, Some(ratatui::style::Color::Rgb(255, 0, 0)),
                   "colon-form truecolor must be parsed");
        assert!(x.style.add_modifier.contains(ratatui::style::Modifier::BOLD),
                "the parameter after a colon-form colour must not be swallowed");
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
                &b"\x1b[J"[..], b"\x1b[1J", b"\x1b[2J", b"\x1b[3J",
                b"\x1b[K", b"\x1b[1K", b"\x1b[2K",
                b"\x1b[P", b"\x1b[@", b"\x1b[X", b"\x1b[L", b"\x1b[M",
                b"\x1b[S", b"\x1b[T", b"\x1bM", b"\x1bD",
                b"\x1b[1;1H", b"\x1b[99;99H", b"\x1b[999X", b"\x1b[r",
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
        assert!(!a.style.add_modifier.contains(ratatui::style::Modifier::UNDERLINED),
                "ESC[>4;2m must not underline");
        assert!(!a.style.add_modifier.contains(ratatui::style::Modifier::DIM),
                "ESC[>4;2m must not dim");
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
                if cell.ch.trim().is_empty() { continue; }
                if cell.style.add_modifier.contains(ratatui::style::Modifier::UNDERLINED) {
                    underlined += 1;
                }
                if cell.style.add_modifier.contains(ratatui::style::Modifier::DIM) {
                    dim += 1;
                }
            }
        }
        assert_eq!(underlined, 0, "{} cells wrongly underlined after real startup", underlined);
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
}
