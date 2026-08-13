//! Measures the idle CPU cost of the filesystem watcher.
//!
//! Run under `/usr/bin/time -l` and compare `user` seconds.
//!   cargo run --release --example watchcost -- poll   <path>
//!   cargo run --release --example watchcost -- events <path>
use notify::{Config as NotifyConfig, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "events".into());
    let path = std::env::args().nth(2).unwrap_or_else(|| ".".into());
    let secs: u64 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let cb = move |_res: notify::Result<notify::Event>| {
        c.fetch_add(1, Ordering::Relaxed);
    };

    let mut poll_watcher;
    let mut event_watcher;
    let w: &mut dyn Watcher = if mode == "poll" {
        poll_watcher = PollWatcher::new(
            cb,
            NotifyConfig::default().with_poll_interval(Duration::from_millis(75)),
        )
        .unwrap();
        &mut poll_watcher
    } else {
        event_watcher = RecommendedWatcher::new(cb, NotifyConfig::default()).unwrap();
        &mut event_watcher
    };

    w.watch(std::path::Path::new(&path), RecursiveMode::Recursive)
        .unwrap();
    std::thread::sleep(Duration::from_secs(secs));
    eprintln!(
        "mode={mode} path={path} idle {secs}s, events seen={}",
        count.load(Ordering::Relaxed)
    );
}
