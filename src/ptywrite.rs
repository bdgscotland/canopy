//! Writing to the PTY, off the event loop, with a bound and a breaker.
//!
//! # Why this exists
//!
//! Writing to a PTY master is a blocking `write(2)` with no timeout, and the
//! ceiling is set by the child's willingness to read — something Canopy does not
//! own. Measured on macOS: the raw-mode tty input queue accepts exactly 1022
//! bytes, then the write sleeps indefinitely.
//!
//! Doing that on the event-loop thread created a closed cycle that only SIGKILL
//! escaped:
//!
//! ```text
//!   main thread blocks in write(master)
//!     -> the current_thread runtime cannot poll any task
//!     -> crossterm's EventStream is never polled, no keys read, no frames drawn
//!     -> the SIGTERM handler never runs, so `kill` does nothing
//!   meanwhile the reader thread is the ONLY drainer of the master
//!     -> it blocks taking the writer lock the main thread holds
//!     -> Claude's stdout fills its 1024-byte queue
//!     -> Claude blocks in write() and never reaches read(stdin)
//!     -> the main thread's write can never complete
//! ```
//!
//! Everyday trigger: paste a stack trace. A 2 KiB paste against a slow reader
//! blocked indefinitely in testing.
//!
//! # The design
//!
//! One thread owns the writer. Everyone else hands it bytes through a bounded
//! queue and never blocks. Two properties matter:
//!
//! 1. **The event loop never touches the PTY.** It enqueues or it refuses.
//! 2. **The reader thread never takes a writer lock.** Terminal query replies go
//!    through this same queue, which deletes the reverse leg of the cycle by
//!    construction rather than by careful ordering.
//!
//! The writer thread may still park in `write()` forever — that is the child's
//! prerogative and we cannot prevent it. What we can guarantee is that nothing
//! else is waiting on it.
//!
//! # Why not `O_NONBLOCK`
//!
//! Tempting, and wrong. `take_writer()` and `try_clone_reader()` both dup the
//! master through `F_DUPFD_CLOEXEC`, so they share one open file description and
//! therefore one set of status flags. Setting `O_NONBLOCK` on the writer makes
//! `read()` on the reader return `EAGAIN`, which lands on the reader's error
//! branch, drops `ChildGuard`, and ends the session on the first idle moment.

use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, SendError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Total bytes allowed in flight. This is the real bound, and it is enforced
/// before anything is enqueued. A paste larger than this is refused outright
/// rather than truncated: handing Claude the first 256 KiB of a command it will
/// then act on is worse than refusing the whole thing.
pub const MAX_QUEUED_BYTES: usize = 256 * 1024;

/// Message-count ceiling, so per-message overhead is bounded too. Deliberately
/// generous: keystrokes are 1-3 bytes each, so a small message cap would refuse
/// a child that is draining perfectly well. At a human typing rate this is
/// minutes of input with zero drain, by which point refusing is the honest
/// answer. The first version used a 256-slot bounded channel and a test caught
/// it refusing a healthy child on the 200th keystroke.
const MAX_QUEUED_MESSAGES: usize = 8192;

/// How long the queue may be stuck before the UI says so.
pub const STALL_NOTICE: Duration = Duration::from_secs(1);

/// Why a write was refused. Never silent — the caller surfaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Larger than the whole budget. Refused before any byte was sent, so the
    /// child never sees a fragment.
    TooLarge { bytes: usize, limit: usize },
    /// The child has stopped draining and the budget is exhausted.
    NotDraining { queued: usize },
    /// The writer thread is gone; the PTY is dead.
    Closed,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::TooLarge { bytes, limit } => write!(
                f,
                "input too large ({:.1} KiB, limit {:.0} KiB)",
                *bytes as f64 / 1024.0,
                *limit as f64 / 1024.0
            ),
            Refused::NotDraining { queued } => write!(
                f,
                "Claude is not reading input — {:.1} KiB queued",
                *queued as f64 / 1024.0
            ),
            Refused::Closed => write!(f, "terminal is closed"),
        }
    }
}

/// Hand to anyone that needs to write to the PTY. Cloneable, never blocks.
#[derive(Clone)]
pub struct PtyWriteHandle {
    tx: Sender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    messages: Arc<AtomicUsize>,
    /// Millis since process start when the queue was last drained to empty.
    last_drained_ms: Arc<AtomicU64>,
    start: Instant,
}

impl PtyWriteHandle {
    /// Take ownership of the writer and start the thread that owns it.
    pub fn spawn(mut writer: Box<dyn Write + Send>) -> Self {
        // Unbounded channel, bounded by the byte and message budgets checked
        // in `write`. A bounded channel would make `send` block, which is the
        // exact failure this module exists to remove.
        let (tx, rx) = channel::<Vec<u8>>();
        let queued = Arc::new(AtomicUsize::new(0));
        let messages = Arc::new(AtomicUsize::new(0));
        let last_drained_ms = Arc::new(AtomicU64::new(0));
        let start = Instant::now();

        let queued_w = Arc::clone(&queued);
        let messages_w = Arc::clone(&messages);
        let drained_w = Arc::clone(&last_drained_ms);
        thread::spawn(move || {
            while let Ok(bytes) = rx.recv() {
                // This may park forever if the child never reads. That is
                // acceptable HERE and nowhere else: no other thread waits on
                // this one, and nothing it holds is needed elsewhere.
                let _ = writer.write_all(&bytes);
                let _ = writer.flush();
                messages_w.fetch_sub(1, Ordering::SeqCst);
                let left = queued_w.fetch_sub(bytes.len(), Ordering::SeqCst) - bytes.len();
                if left == 0 {
                    drained_w.store(start.elapsed().as_millis() as u64, Ordering::SeqCst);
                }
            }
        });

        Self {
            tx,
            queued,
            messages,
            last_drained_ms,
            start,
        }
    }

    /// Enqueue bytes. Never blocks; returns why it refused instead.
    pub fn write(&self, bytes: Vec<u8>) -> Result<(), Refused> {
        if bytes.is_empty() {
            return Ok(());
        }
        if bytes.len() > MAX_QUEUED_BYTES {
            return Err(Refused::TooLarge {
                bytes: bytes.len(),
                limit: MAX_QUEUED_BYTES,
            });
        }
        let in_flight = self.queued.load(Ordering::SeqCst);
        if in_flight + bytes.len() > MAX_QUEUED_BYTES
            || self.messages.load(Ordering::SeqCst) >= MAX_QUEUED_MESSAGES
        {
            return Err(Refused::NotDraining { queued: in_flight });
        }

        let len = bytes.len();
        self.queued.fetch_add(len, Ordering::SeqCst);
        self.messages.fetch_add(1, Ordering::SeqCst);
        match self.tx.send(bytes) {
            Ok(()) => Ok(()),
            Err(SendError(_)) => {
                self.queued.fetch_sub(len, Ordering::SeqCst);
                self.messages.fetch_sub(1, Ordering::SeqCst);
                Err(Refused::Closed)
            }
        }
    }

    /// Bytes waiting to reach the child.
    pub fn queued_bytes(&self) -> usize {
        self.queued.load(Ordering::SeqCst)
    }

    /// How long the queue has been non-empty, if it is. `None` means the child
    /// is keeping up. Drives the UI notice.
    pub fn stalled_for(&self) -> Option<Duration> {
        if self.queued.load(Ordering::SeqCst) == 0 {
            return None;
        }
        let last = self.last_drained_ms.load(Ordering::SeqCst);
        let now = self.start.elapsed().as_millis() as u64;
        Some(Duration::from_millis(now.saturating_sub(last)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that blocks until released, standing in for a child that has
    /// stopped reading its stdin.
    struct BlockingWriter {
        gate: std::sync::mpsc::Receiver<()>,
        wrote: Sender<usize>,
    }
    impl Write for BlockingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.gate.recv();
            let _ = self.wrote.send(buf.len());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn enqueue_never_blocks_even_when_the_child_never_reads() {
        // The property the whole module exists for: the caller returns
        // promptly no matter what the child does.
        let (gate_tx, gate_rx) = channel::<()>();
        let (wrote_tx, _wrote_rx) = channel::<usize>();
        let h = PtyWriteHandle::spawn(Box::new(BlockingWriter {
            gate: gate_rx,
            wrote: wrote_tx,
        }));

        let start = Instant::now();
        for _ in 0..1000 {
            let _ = h.write(vec![b'x'; 64]);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(250),
            "enqueueing blocked for {elapsed:?} against a stalled child"
        );
        drop(gate_tx);
    }

    #[test]
    fn a_stalled_child_gets_refusals_not_a_hang() {
        let (gate_tx, gate_rx) = channel::<()>();
        let (wrote_tx, _rx) = channel::<usize>();
        let h = PtyWriteHandle::spawn(Box::new(BlockingWriter {
            gate: gate_rx,
            wrote: wrote_tx,
        }));

        let mut refused = None;
        for _ in 0..10_000 {
            if let Err(e) = h.write(vec![b'x'; 4096]) {
                refused = Some(e);
                break;
            }
        }
        match refused {
            Some(Refused::NotDraining { .. }) => {}
            other => panic!("expected NotDraining, got {other:?}"),
        }
        assert!(h.queued_bytes() <= MAX_QUEUED_BYTES);
        drop(gate_tx);
    }

    #[test]
    fn an_oversized_paste_is_refused_whole_never_truncated() {
        // Delivering a prefix would hand Claude half a command it then acts on.
        let (_gate_tx, gate_rx) = channel::<()>();
        let (wrote_tx, _rx) = channel::<usize>();
        let h = PtyWriteHandle::spawn(Box::new(BlockingWriter {
            gate: gate_rx,
            wrote: wrote_tx,
        }));
        let err = h.write(vec![b'x'; MAX_QUEUED_BYTES + 1]).unwrap_err();
        assert!(matches!(err, Refused::TooLarge { .. }), "{err:?}");
        assert_eq!(h.queued_bytes(), 0, "a refused write must queue nothing");
    }

    #[test]
    fn a_draining_child_never_sees_a_refusal_and_gets_every_byte_in_order() {
        let (tx, rx) = channel::<Vec<u8>>();
        struct Collect(std::sync::mpsc::Sender<Vec<u8>>);
        impl Write for Collect {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let _ = self.0.send(buf.to_vec());
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let h = PtyWriteHandle::spawn(Box::new(Collect(tx)));
        for i in 0..500u32 {
            h.write(format!("{i}\n").into_bytes())
                .expect("a draining child must never be refused");
        }
        let mut seen = Vec::new();
        while seen.len() < 500 {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(b) => seen.push(String::from_utf8(b).unwrap()),
                Err(_) => break,
            }
        }
        assert_eq!(seen.len(), 500, "bytes were dropped");
        assert_eq!(seen[0], "0\n");
        assert_eq!(seen[499], "499\n", "order was not preserved");
    }

    #[test]
    fn stall_is_observable_so_the_ui_can_say_so() {
        let (gate_tx, gate_rx) = channel::<()>();
        let (wrote_tx, _rx) = channel::<usize>();
        let h = PtyWriteHandle::spawn(Box::new(BlockingWriter {
            gate: gate_rx,
            wrote: wrote_tx,
        }));
        assert!(
            h.stalled_for().is_none(),
            "idle queue must not report a stall"
        );
        let _ = h.write(vec![b'x'; 1024]);
        thread::sleep(Duration::from_millis(50));
        assert!(h.stalled_for().is_some(), "a stuck queue must be visible");
        drop(gate_tx);
    }
}
