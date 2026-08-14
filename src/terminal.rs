use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, CommandBuilder, PtyPair, PtySize};

use crate::ptywrite::{PtyWriteHandle, Refused};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use tokio::sync::mpsc;

use crate::vterm::VirtualTerminal;

/// RAII guard that ensures the child process is waited on when dropped,
/// preventing zombie processes even if the reader thread panics.
/// How many emulator panics to absorb before giving up on the pane. The
/// session outlives all of them either way.
const MAX_VTERM_PANICS: u32 = 3;

/// Reaps the child when the reader thread ends.
///
/// `child_exited` means the CHILD is gone, and only that. It used to mean "the
/// reader thread stopped", which are not the same thing: any panic in the
/// reader — a vterm indexing bug, a unicode edge case — set it, Canopy read it
/// as "Claude quit", and exited 0. An unrelated bug in our own emulator ended
/// the user's session, indistinguishably from a normal quit.
struct ChildGuard {
    child: Box<dyn portable_pty::Child + Send>,
    child_exited: Arc<AtomicBool>,
    reader_failed: Arc<AtomicBool>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Ask the OS, do not infer. If the child is still alive we are here
        // because the READER died, and the session must survive that.
        let really_gone = matches!(self.child.try_wait(), Ok(Some(_)));
        if really_gone {
            self.child_exited.store(true, Ordering::SeqCst);
        } else {
            self.reader_failed.store(true, Ordering::SeqCst);
        }
        let _ = self.child.wait();
        // wait() returning means it is gone now, whatever the reason.
        self.child_exited.store(true, Ordering::SeqCst);
    }
}

/// Lock a mutex, recovering from poison (prior thread panic).
fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct TerminalPane {
    /// The MASTER only. The slave is dropped immediately after spawning the
    /// child -- see try_spawn_claude. Holding it kept the PTY open against
    /// ourselves: measured, with the parent still holding the slave, read() on
    /// the master blocks forever even after the child exits, so the reader
    /// thread never returns 0, ChildGuard never drops, and Canopy hangs at exit
    /// instead of noticing Claude is gone. Dropping it turns every one of those
    /// permanent hangs into an immediate EIO.
    pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    /// All PTY writes go through here, on a dedicated thread. Nothing else may
    /// write to the master: the event loop must never block on the child, and
    /// the reader must never wait on a writer lock.
    writer: Option<PtyWriteHandle>,
    /// The most recent refusal, for the UI to surface.
    pub last_refusal: Option<String>,
    vterm: Arc<Mutex<VirtualTerminal>>,
    cwd: PathBuf,
    child_pid: Option<u32>,
    process_exited: Arc<AtomicBool>,
    /// The reader thread stopped while the child was still alive. The pane is
    /// frozen but the session is NOT gone; Canopy must not quit on this.
    reader_failed: Arc<AtomicBool>,
    last_cols: u16,
    last_rows: u16,
    // Debounce: pending CWD change must be detected consistently before applying
}

impl TerminalPane {
    pub fn new(
        cwd: &Path,
        claude_args: &[String],
        pty_tx: mpsc::UnboundedSender<()>,
    ) -> anyhow::Result<Self> {
        let vterm = Arc::new(Mutex::new(VirtualTerminal::new(80, 24)));
        let process_exited = Arc::new(AtomicBool::new(false));
        let reader_failed = Arc::new(AtomicBool::new(false));

        let writer_slot: Arc<Mutex<Option<PtyWriteHandle>>> = Arc::new(Mutex::new(None));
        let writer_handle_slot = Arc::clone(&writer_slot);

        // Try to create PTY and spawn claude process
        let (pty_master, child_pid) = match Self::try_spawn_claude(
            cwd,
            &vterm,
            claude_args,
            &process_exited,
            &reader_failed,
            pty_tx,
            &writer_slot,
        ) {
            Ok((pair, pid)) => (Some(pair), pid),
            Err(e) => {
                // Store error message in vterm so user can see it
                let msg = format!(
                    "Failed to start Claude Code: {}\r\n\r\n\
                     Make sure 'claude' CLI is installed and in your PATH.\r\n\
                     Install: npm install -g @anthropic-ai/claude-code\r\n",
                    e
                );
                lock_or_recover(&vterm).feed(msg.as_bytes());
                (None, None)
            }
        };

        // Take the handle out of the slot now that spawn has populated it.
        let writer_handle = lock_or_recover(&writer_handle_slot).clone();

        Ok(Self {
            pty_master,
            writer: writer_handle,
            last_refusal: None,
            vterm,
            cwd: cwd.to_path_buf(),
            child_pid,
            process_exited,
            reader_failed,
            last_cols: 80,
            last_rows: 24,
        })
    }

    fn try_spawn_claude(
        cwd: &Path,
        vterm: &Arc<Mutex<VirtualTerminal>>,
        claude_args: &[String],
        process_exited: &Arc<AtomicBool>,
        reader_failed: &Arc<AtomicBool>,
        pty_tx: mpsc::UnboundedSender<()>,
        writer_slot: &Arc<Mutex<Option<PtyWriteHandle>>>,
    ) -> anyhow::Result<(Box<dyn portable_pty::MasterPty + Send>, Option<u32>)> {
        // Create PTY
        let pty_system = native_pty_system();
        let pty_pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        // Spawn claude process via login shell so that shell profiles (.bashrc, .zshrc)
        // are sourced — this ensures nvm/fnm/node and other environment setup is available.
        let command = std::env::var("CANOPY_COMMAND").unwrap_or_else(|_| "claude".to_string());
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

        // Build the full command string: "exec claude [args...]"
        // Using exec so claude replaces the shell process (no extra process)
        let mut full_cmd = format!("exec {}", command);
        for arg in claude_args {
            full_cmd.push(' ');
            // Shell-escape arguments containing special characters
            if arg.contains(' ')
                || arg.contains('\'')
                || arg.contains('"')
                || arg.contains('\\')
                || arg.contains('$')
            {
                full_cmd.push('\'');
                full_cmd.push_str(&arg.replace('\'', "'\\''"));
                full_cmd.push('\'');
            } else {
                full_cmd.push_str(arg);
            }
        }

        let mut cmd = CommandBuilder::new(&shell);
        cmd.arg("-l");
        cmd.arg("-c");
        cmd.arg(&full_cmd);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        // Remove CLAUDECODE env var to allow nested Claude sessions
        cmd.env_remove("CLAUDECODE");

        let child = pty_pair.slave.spawn_command(cmd)?;
        // The child owns its own slave fd now; ours only keeps the PTY alive
        // past the child's death.
        let PtyPair { master, slave } = pty_pair;
        drop(slave);

        // Get child PID before moving child into the thread
        let child_pid = child.process_id();

        // Take the writer from master PTY (can only be called once)
        // Store it in the shared Arc<Mutex<>> so both main thread and reader thread can use it
        if let Ok(w) = master.take_writer() {
            *lock_or_recover(writer_slot) = Some(PtyWriteHandle::spawn(w));
        }

        // Read output in background thread
        let mut reader = master.try_clone_reader()?;
        let vterm_clone = Arc::clone(vterm);
        let exited_clone = Arc::clone(process_exited);
        let reader_failed_clone = Arc::clone(reader_failed);
        let writer_for_reader = lock_or_recover(writer_slot).clone();

        thread::spawn(move || {
            // ChildGuard ensures wait() is called even on panic
            let _guard = ChildGuard {
                child,
                child_exited: exited_clone,
                reader_failed: reader_failed_clone,
            };
            let mut buf = [0u8; 4096];
            let mut vterm_panics: u32 = 0;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // The emulator is ~1,200 lines of hand-written escape
                        // handling and has already produced five reachable
                        // panics. A bug in it must never end the user's
                        // session, so catch the unwind, reset the grid, and
                        // keep reading. Budget a few per session: a permanently
                        // panicking parser is a dead pane, not a live one.
                        let feed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let mut vt = lock_or_recover(&vterm_clone);
                            vt.feed(&buf[..n]);
                            vt.take_responses()
                        }));
                        let responses = match feed {
                            Ok(r) => r,
                            Err(_) => {
                                vterm_panics += 1;
                                if vterm_panics > MAX_VTERM_PANICS {
                                    // Stop feeding, but leave the child alone.
                                    // ChildGuard will report reader_failed, and
                                    // the session keeps running.
                                    break;
                                }
                                // Poisoned by the unwind; recover and reset to
                                // a clean grid at the current geometry.
                                let mut vt = lock_or_recover(&vterm_clone);
                                let (c, r) = (vt.cols(), vt.rows());
                                vt.reset_after_panic(c, r);
                                Vec::new()
                            }
                        };
                        // Query replies go through the SAME queue as user
                        // input. This is what deletes the reverse leg of the
                        // deadlock: the reader never waits on a writer lock, so
                        // it can always keep draining the master.
                        if let Some(ref w) = writer_for_reader {
                            for resp in responses {
                                let _ = w.write(resp);
                            }
                        }
                        let _ = pty_tx.send(());
                    }
                    Err(e) => {
                        eprintln!("PTY read error: {e}");
                        break;
                    }
                }
            }
            // ChildGuard::drop will set exited flag and wait for child
        });

        Ok((master, child_pid))
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn tick(&mut self) {
        // 1. Try OSC 7 first (shell-reported CWD) — only apply if deeper or same
        //    (prevents flickering from stale reports)
        if let Ok(vt) = self.vterm.lock() {
            if let Some(reported) = vt.reported_cwd() {
                if reported != self.cwd {
                    let new_depth = reported.components().count();
                    let cur_depth = self.cwd.components().count();
                    if new_depth >= cur_depth {
                        self.cwd = reported.to_path_buf();
                        return;
                    }
                }
            }
        }

        // 2. Try OS process CWD — only apply if deeper or same
        if let Some(pid) = self.child_pid {
            if let Some(proc_cwd) = get_process_cwd(pid) {
                if proc_cwd != self.cwd {
                    let new_depth = proc_cwd.components().count();
                    let cur_depth = self.cwd.components().count();
                    if new_depth >= cur_depth {
                        self.cwd = proc_cwd;
                    }
                }
            }
        }

        // 3. NO screen scraping.
        //
        // This used to scan the top 8 rendered rows for anything shaped like a
        // `~/...` path, stat it, and adopt the DEEPEST match as the working
        // directory -- immediately, with no confirmation. Claude's output is
        // full of paths, so any file it mentioned re-rooted the tree, and the
        // re-root walks the filesystem on the event-loop thread. Observed
        // hanging Canopy outright: a sample showed the main thread with 1,224
        // of 1,770 frames in readdir_r and 458 in stat.
        //
        // It existed to answer "which directory is Claude working in", which
        // the session transcript now reports directly and correctly (see
        // src/activity.rs). OSC 7 and the process-cwd check above remain as
        // the legitimate signals for a shell that actually cds.
    }

    pub fn is_process_exited(&self) -> bool {
        self.process_exited.load(Ordering::SeqCst)
    }

    /// The reader thread died but the child is still running. The pane is dead;
    /// the session is not. Canopy stays up so the user can read the message and
    /// copy their resume command.
    pub fn reader_failed(&self) -> bool {
        self.reader_failed.load(Ordering::SeqCst)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let Some(bytes) = encode_key(key) else { return };
        self.enqueue(bytes);
    }

    /// Hand bytes to the writer thread. Never blocks; records a refusal for the
    /// UI instead. This is the only path to the PTY.
    fn enqueue(&mut self, bytes: Vec<u8>) {
        let Some(ref w) = self.writer else { return };
        match w.write(bytes) {
            Ok(()) => self.last_refusal = None,
            Err(Refused::Closed) => {}
            Err(e) => self.last_refusal = Some(e.to_string()),
        }
    }

    /// How long input has been stuck undelivered, if it is. Drives the UI
    /// notice, so a user whose Claude has stopped reading finds out rather than
    /// wondering why typing does nothing.
    pub fn write_stalled_for(&self) -> Option<std::time::Duration> {
        self.writer.as_ref()?.stalled_for()
    }

    pub fn queued_input_bytes(&self) -> usize {
        self.writer.as_ref().map_or(0, |w| w.queued_bytes())
    }
}

/// Encode a key event as the bytes a terminal would send. Pure, so it can be
/// tested without spawning a PTY -- the encoding is where the bugs live.
/// Returns None for keys that should send nothing.
pub fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    #[allow(clippy::items_after_statements)]
    {
        // Compute modifier parameter for CSI sequences (xterm style)
        // 1=none, 2=Shift, 3=Alt, 4=Shift+Alt, 5=Ctrl, 6=Ctrl+Shift, 7=Ctrl+Alt, 8=Ctrl+Shift+Alt
        let modifier_param = |mods: KeyModifiers| -> u8 {
            let mut param = 1u8;
            if mods.contains(KeyModifiers::SHIFT) {
                param += 1;
            }
            if mods.contains(KeyModifiers::ALT) {
                param += 2;
            }
            if mods.contains(KeyModifiers::CONTROL) {
                param += 4;
            }
            param
        };

        let bytes: Vec<u8> = match key.code {
            // --- Character keys ---
            KeyCode::Char(c) => {
                let mods = key.modifiers;
                if mods == KeyModifiers::NONE || mods == KeyModifiers::SHIFT {
                    // Normal or shifted character — send as UTF-8
                    let ch = if mods.contains(KeyModifiers::SHIFT) {
                        c.to_uppercase().next().unwrap_or(c)
                    } else {
                        c
                    };
                    let mut buf = [0u8; 4];
                    let s = ch.encode_utf8(&mut buf);
                    s.as_bytes().to_vec()
                } else if mods == KeyModifiers::CONTROL
                    || mods == KeyModifiers::CONTROL | KeyModifiers::SHIFT
                {
                    // CONTROL|SHIFT previously fell through to the UTF-8 branch
                    // and sent a bare letter.
                    match ctrl_byte(c) {
                        Some(b) => vec![b],
                        None => {
                            let mut buf = [0u8; 4];
                            c.encode_utf8(&mut buf).as_bytes().to_vec()
                        }
                    }
                } else if mods == KeyModifiers::ALT {
                    // Alt+char: ESC prefix + char
                    let mut v = vec![0x1b];
                    let mut buf = [0u8; 4];
                    let s = c.encode_utf8(&mut buf);
                    v.extend_from_slice(s.as_bytes());
                    v
                } else if mods == KeyModifiers::CONTROL | KeyModifiers::ALT {
                    // Ctrl+Alt+char: ESC prefix + ctrl byte
                    match ctrl_byte(c) {
                        Some(b) => vec![0x1b, b],
                        None => {
                            let mut buf = [0u8; 4];
                            let mut v = vec![0x1b];
                            v.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                            v
                        }
                    }
                } else {
                    // Fallback: send as UTF-8
                    let mut buf = [0u8; 4];
                    let s = c.encode_utf8(&mut buf);
                    s.as_bytes().to_vec()
                }
            }

            // --- Simple keys (no modifier variants) ---
            KeyCode::Enter => {
                // Alt/Option+Enter is Claude Code's documented "insert newline"
                // binding. Dropping the modifier submitted the prompt instead.
                if key.modifiers.contains(KeyModifiers::ALT) {
                    vec![0x1b, b'\r']
                } else {
                    vec![b'\r']
                }
            }
            KeyCode::Backspace => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    vec![0x1b, 127] // Alt+Backspace (delete word)
                } else if key.modifiers.contains(KeyModifiers::CONTROL) {
                    vec![0x17] // Ctrl+Backspace -> ^W (delete word), was dropped
                } else {
                    vec![127]
                }
            }
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    vec![0x1b, b'[', b'Z'] // Shift+Tab (backtab)
                } else {
                    vec![b'\t']
                }
            }
            KeyCode::Esc => vec![0x1b],
            KeyCode::Insert => {
                let m = modifier_param(key.modifiers);
                if m == 1 {
                    vec![0x1b, b'[', b'2', b'~']
                } else {
                    format!("\x1b[2;{}~", m).into_bytes()
                }
            }

            // --- Arrow keys with modifier support ---
            KeyCode::Up => {
                let m = modifier_param(key.modifiers);
                if m == 1 {
                    vec![0x1b, b'[', b'A']
                } else {
                    format!("\x1b[1;{}A", m).into_bytes()
                }
            }
            KeyCode::Down => {
                let m = modifier_param(key.modifiers);
                if m == 1 {
                    vec![0x1b, b'[', b'B']
                } else {
                    format!("\x1b[1;{}B", m).into_bytes()
                }
            }
            KeyCode::Right => {
                let m = modifier_param(key.modifiers);
                if m == 1 {
                    vec![0x1b, b'[', b'C']
                } else {
                    format!("\x1b[1;{}C", m).into_bytes()
                }
            }
            KeyCode::Left => {
                let m = modifier_param(key.modifiers);
                if m == 1 {
                    vec![0x1b, b'[', b'D']
                } else {
                    format!("\x1b[1;{}D", m).into_bytes()
                }
            }

            // --- Navigation keys with modifier support ---
            KeyCode::Home => {
                let m = modifier_param(key.modifiers);
                if m == 1 {
                    vec![0x1b, b'[', b'H']
                } else {
                    format!("\x1b[1;{}H", m).into_bytes()
                }
            }
            KeyCode::End => {
                let m = modifier_param(key.modifiers);
                if m == 1 {
                    vec![0x1b, b'[', b'F']
                } else {
                    format!("\x1b[1;{}F", m).into_bytes()
                }
            }
            KeyCode::PageUp => {
                let m = modifier_param(key.modifiers);
                if m == 1 {
                    vec![0x1b, b'[', b'5', b'~']
                } else {
                    format!("\x1b[5;{}~", m).into_bytes()
                }
            }
            KeyCode::PageDown => {
                let m = modifier_param(key.modifiers);
                if m == 1 {
                    vec![0x1b, b'[', b'6', b'~']
                } else {
                    format!("\x1b[6;{}~", m).into_bytes()
                }
            }
            KeyCode::Delete => {
                let m = modifier_param(key.modifiers);
                if m == 1 {
                    vec![0x1b, b'[', b'3', b'~']
                } else {
                    format!("\x1b[3;{}~", m).into_bytes()
                }
            }

            // --- Function keys (F1-F12) with modifier support ---
            KeyCode::F(n) => {
                let m = modifier_param(key.modifiers);
                match n {
                    // F1-F4 use SS3 sequences (no modifier) or CSI with modifier
                    1 => {
                        if m == 1 {
                            vec![0x1b, b'O', b'P']
                        } else {
                            format!("\x1b[1;{}P", m).into_bytes()
                        }
                    }
                    2 => {
                        if m == 1 {
                            vec![0x1b, b'O', b'Q']
                        } else {
                            format!("\x1b[1;{}Q", m).into_bytes()
                        }
                    }
                    3 => {
                        if m == 1 {
                            vec![0x1b, b'O', b'R']
                        } else {
                            format!("\x1b[1;{}R", m).into_bytes()
                        }
                    }
                    4 => {
                        if m == 1 {
                            vec![0x1b, b'O', b'S']
                        } else {
                            format!("\x1b[1;{}S", m).into_bytes()
                        }
                    }
                    // F5-F12 use CSI number ~ sequences
                    5 => {
                        if m == 1 {
                            b"\x1b[15~".to_vec()
                        } else {
                            format!("\x1b[15;{}~", m).into_bytes()
                        }
                    }
                    6 => {
                        if m == 1 {
                            b"\x1b[17~".to_vec()
                        } else {
                            format!("\x1b[17;{}~", m).into_bytes()
                        }
                    }
                    7 => {
                        if m == 1 {
                            b"\x1b[18~".to_vec()
                        } else {
                            format!("\x1b[18;{}~", m).into_bytes()
                        }
                    }
                    8 => {
                        if m == 1 {
                            b"\x1b[19~".to_vec()
                        } else {
                            format!("\x1b[19;{}~", m).into_bytes()
                        }
                    }
                    9 => {
                        if m == 1 {
                            b"\x1b[20~".to_vec()
                        } else {
                            format!("\x1b[20;{}~", m).into_bytes()
                        }
                    }
                    10 => {
                        if m == 1 {
                            b"\x1b[21~".to_vec()
                        } else {
                            format!("\x1b[21;{}~", m).into_bytes()
                        }
                    }
                    11 => {
                        if m == 1 {
                            b"\x1b[23~".to_vec()
                        } else {
                            format!("\x1b[23;{}~", m).into_bytes()
                        }
                    }
                    12 => {
                        if m == 1 {
                            b"\x1b[24~".to_vec()
                        } else {
                            format!("\x1b[24;{}~", m).into_bytes()
                        }
                    }
                    _ => return None, // F13+ not commonly used
                }
            }

            // --- BackTab (Shift+Tab reported as separate key by crossterm) ---
            KeyCode::BackTab => vec![0x1b, b'[', b'Z'],

            // Unknown keys — ignore rather than sending garbage
            _ => return None,
        };

        Some(bytes)
    }
}

impl TerminalPane {
    /// Send pasted text wrapped in bracketed-paste escape sequences.
    /// This prevents the terminal from interpreting newlines as Enter keypresses.
    pub fn handle_paste(&mut self, text: String) {
        // One message, so the paste is delivered whole or refused whole. Split
        // across messages, a refusal partway through would hand Claude an
        // unterminated bracketed paste and half a command it would then act on.
        let mut buf = Vec::with_capacity(text.len() + 12);
        buf.extend_from_slice(b"\x1b[200~");
        buf.extend_from_slice(text.as_bytes());
        buf.extend_from_slice(b"\x1b[201~");
        self.enqueue(buf);
    }

    pub fn send_focus_event(&mut self, gained: bool) {
        // Only forward focus events if the child process has enabled
        // focus tracking (DECSET 1004). Otherwise the raw \x1b[I / \x1b[O
        // sequence would be echoed as visible ^[[I / ^[[O by the shell.
        if !self.vterm_lock().focus_tracking_enabled() {
            return;
        }
        let seq = if gained {
            b"\x1b[I" as &[u8]
        } else {
            b"\x1b[O"
        };
        self.enqueue(seq.to_vec());
    }

    /// Acquire a poison-safe lock on the virtual terminal.
    pub fn vterm_lock(&self) -> MutexGuard<'_, VirtualTerminal> {
        lock_or_recover(&self.vterm)
    }

    /// Wheel handling, matching what real terminals do.
    ///
    /// On the alt screen there is no scrollback to move through, so the wheel
    /// becomes arrow keys and the application scrolls itself. Claude Code is an
    /// alt-screen app, which is why scrolling appeared to do nothing at all:
    /// we were moving a scroll offset over a buffer that is always empty.
    ///
    /// Returns true if the event was translated and sent to the child.
    pub fn wheel(&mut self, up: bool, lines: usize) -> bool {
        if !self.vterm_lock().in_alternate_screen() {
            return false;
        }
        let seq: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
        let mut buf = Vec::with_capacity(seq.len() * lines);
        for _ in 0..lines {
            buf.extend_from_slice(seq);
        }
        self.enqueue(buf);
        true
    }

    pub fn scroll_up(&mut self) {
        let mut vt = lock_or_recover(&self.vterm);
        let current = vt.scroll_offset();
        vt.set_scroll_offset(current + 3);
    }

    pub fn scroll_down(&mut self) {
        let mut vt = lock_or_recover(&self.vterm);
        let current = vt.scroll_offset();
        vt.set_scroll_offset(current.saturating_sub(3));
    }

    /// Extract text from a selection range (terminal-local coordinates).
    /// Coordinates are (col, row) relative to the visible terminal area.
    /// Text between two ABSOLUTE line positions. Taking screen rows here meant
    /// the copied text drifted away from the highlighted text as soon as the
    /// view scrolled.
    pub fn extract_text(&self, start: (u16, u64), end: (u16, u64)) -> String {
        let vt = lock_or_recover(&self.vterm);
        let grid = vt.grid();
        let scrollback = vt.scrollback();
        let cols = vt.cols();

        // Normalize start/end so start is before end
        let (start, end) = if (start.1, start.0) <= (end.1, end.0) {
            (start, end)
        } else {
            (end, start)
        };

        // Absolute -> index into the virtual `scrollback ++ grid` buffer.
        // Lines evicted since the selection was made are simply gone.
        let evicted = vt.lines_evicted();
        let to_view = |abs: u64| -> usize { abs.saturating_sub(evicted) as usize };

        let start_line = to_view(start.1);
        let end_line = to_view(end.1);

        let get_row = |line_idx: usize| -> Option<&Vec<crate::vterm::Cell>> {
            if line_idx < scrollback.len() {
                scrollback.get(line_idx)
            } else {
                grid.get(line_idx - scrollback.len())
            }
        };

        let mut lines = Vec::new();
        for line_idx in start_line..=end_line {
            if let Some(row) = get_row(line_idx) {
                let col_start = if line_idx == start_line {
                    start.0 as usize
                } else {
                    0
                };
                let col_end = if line_idx == end_line {
                    (end.0 as usize + 1).min(cols)
                } else {
                    cols
                };
                // Clamp BOTH ends. col_end alone is not enough: a stale-width
                // scrollback row can be shorter than col_start, which inverts
                // the range and panics.
                let hi = col_end.min(row.len());
                let lo = col_start.min(hi);
                let text: String = row[lo..hi]
                    .iter()
                    .map(|c| if c.ch.is_empty() { " " } else { c.ch.as_str() })
                    .collect();
                lines.push(text.trim_end().to_string());
            }
        }

        lines.join("\n")
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == self.last_cols && rows == self.last_rows {
            return;
        }
        self.last_cols = cols;
        self.last_rows = rows;

        // Resize the PTY
        if let Some(ref master) = self.pty_master {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        // Resize the virtual terminal grid
        let mut vt = lock_or_recover(&self.vterm);
        vt.resize(cols as usize, rows as usize);
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        // Closing the master sends SIGHUP to the child.
        self.pty_master.take();
    }
}

/// Get the current working directory of a process by PID.
/// Uses macOS `proc_pidinfo` API or Linux `/proc/PID/cwd`.
#[cfg(target_os = "macos")]
fn get_process_cwd(pid: u32) -> Option<PathBuf> {
    use std::ffi::{c_int, c_void};
    use std::mem;

    const PROC_PIDVNODEPATHINFO: c_int = 9;
    const MAXPATHLEN: usize = 1024;

    #[repr(C)]
    struct VnodeInfoPath {
        // struct vnode_info (see Darwin sys/proc_info.h: vnode_info is 152 bytes)
        _vip_vi: [u8; 152],
        vip_path: [u8; MAXPATHLEN],
    }

    #[repr(C)]
    struct ProcVnodePathInfo {
        pvi_cdir: VnodeInfoPath,
        _pvi_rdir: VnodeInfoPath,
    }

    extern "C" {
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
    }

    unsafe {
        let mut info: ProcVnodePathInfo = mem::zeroed();
        let size = mem::size_of::<ProcVnodePathInfo>() as c_int;

        let ret = proc_pidinfo(
            pid as c_int,
            PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            size,
        );

        if ret != size {
            return None;
        }

        let path_bytes = &info.pvi_cdir.vip_path;
        let len = path_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(MAXPATHLEN);
        let path_str = std::str::from_utf8(&path_bytes[..len]).ok()?;

        if path_str.is_empty() {
            None
        } else {
            Some(PathBuf::from(path_str))
        }
    }
}

#[cfg(target_os = "linux")]
fn get_process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{}/cwd", pid)).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn get_process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

/// Encode Ctrl+<char> the way a terminal does.
///
/// `(c.to_ascii_lowercase() as u8).wrapping_sub(b'a' - 1)` is only correct for
/// a-z. crossterm reports the other control bytes as their printable names
/// (crossterm-0.29.0/src/event/sys/unix/parse.rs:106-117): 0x00 arrives as
/// Char(' ')+CONTROL, 0x1C..=0x1F as Char('4'..'7')+CONTROL. Round-tripping
/// those through the subtraction produced 0xC0, 0xD4..0xD7 -- bytes that are
/// never legal UTF-8, so Claude's stdin decoder yields U+FFFD or drops them.
/// Ctrl+/ (readline undo) and Ctrl+Space (set mark) were both corrupted.
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        ' ' | '@' | '2' => 0x00,
        'a'..='z' => c as u8 & 0x1f,
        'A'..='Z' => c as u8 & 0x1f,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

#[cfg(test)]
mod key_tests {
    use super::encode_key;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn enc(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        encode_key(KeyEvent::new(code, mods)).expect("key should encode")
    }

    /// crossterm reports control bytes as their PRINTABLE names, not as the
    /// letters the old `wrapping_sub(b'a' - 1)` assumed. Each of these round
    /// trips a byte the outer terminal delivered correctly, so getting it wrong
    /// means Canopy corrupts input rather than merely mishandling it.
    /// Source: crossterm-0.29.0/src/event/sys/unix/parse.rs:106-117
    #[test]
    fn control_bytes_round_trip_exactly() {
        let ctrl = KeyModifiers::CONTROL;
        for (ch, want) in [
            (' ', 0x00u8), // Ctrl+Space, set mark
            ('a', 0x01),
            ('z', 0x1a),
            ('[', 0x1b),
            ('\\', 0x1c), // was 0xD4
            (']', 0x1d),  // was 0xD5
            ('^', 0x1e),  // was 0xD6
            ('_', 0x1f),
            ('/', 0x1f), // Ctrl+/, readline undo. was 0xD7
            ('?', 0x7f),
            ('4', 0x1c),
            ('7', 0x1f),
        ] {
            let got = enc(KeyCode::Char(ch), ctrl);
            assert_eq!(got, vec![want], "Ctrl+{ch:?} should send {want:#04x}");
        }
    }

    #[test]
    fn no_control_key_produces_invalid_utf8() {
        // 0xC0 and 0xD4..0xD7 are never legal UTF-8. Claude's stdin decoder
        // turned them into U+FFFD or dropped them.
        let ctrl = KeyModifiers::CONTROL;
        for ch in [' ', '\\', ']', '^', '_', '/', '?', '@', '2', '8'] {
            let got = enc(KeyCode::Char(ch), ctrl);
            assert!(
                std::str::from_utf8(&got).is_ok() || got.iter().all(|b| *b < 0x80),
                "Ctrl+{ch:?} produced non-ASCII bytes {got:02x?}"
            );
        }
    }

    #[test]
    fn ctrl_shift_is_not_sent_as_a_bare_letter() {
        // CONTROL|SHIFT fell through to the UTF-8 branch and sent "C".
        let got = enc(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(got, vec![0x03]);
    }

    #[test]
    fn alt_enter_inserts_a_newline_instead_of_submitting() {
        // Claude Code documents Option/Alt+Enter as "insert newline". The
        // modifier was never read, so it submitted the prompt instead.
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::ALT), vec![0x1b, b'\r']);
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::NONE), vec![b'\r']);
    }

    #[test]
    fn ctrl_backspace_deletes_a_word() {
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::CONTROL), vec![0x17]);
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::ALT), vec![0x1b, 127]);
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::NONE), vec![127]);
    }

    #[test]
    fn ctrl_alt_uses_the_same_table() {
        let m = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(enc(KeyCode::Char('a'), m), vec![0x1b, 0x01]);
        assert_eq!(enc(KeyCode::Char('/'), m), vec![0x1b, 0x1f]);
    }
}
