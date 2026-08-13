//! Measures how long the tree walk blocks. It runs on the event-loop thread,
//! and in App::new it runs BEFORE the PTY is opened, so Claude does not start
//! until it finishes.
//!   cargo run --release --example treecost -- <path> [max_depth]
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let depth: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let p = std::path::Path::new(&path);
    let mut best = std::time::Duration::MAX;
    let mut nodes = 0;
    for _ in 0..3 {
        let t = Instant::now();
        let tree = canopy::tree::FileTree::new(p, false, depth).expect("build");
        let d = t.elapsed();
        nodes = tree.nodes().len();
        if d < best {
            best = d;
        }
    }
    println!("{path}: {nodes} nodes, best of 3 = {best:?}");
}
