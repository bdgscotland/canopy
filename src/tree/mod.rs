mod file_node;

pub use file_node::FileNode;

use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Directories never worth reacting to, even when not gitignored. These churn
/// constantly during a build and are never displayed.
const ALWAYS_NOISY: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".cache",
    "vendor",
];

pub struct FileTree {
    root: PathBuf,
    nodes: Vec<FileNode>,
    pub show_hidden: bool,
    max_depth: usize,
    offset: usize,
    /// Columns the view is scrolled right. 0 = flush left.
    h_offset: usize,
    /// Widest line in the tree in display columns, cached by set_nodes so
    /// the horizontal scrollbar geometry never walks the node list.
    content_width: usize,
    /// Used to discard filesystem events for paths the tree never shows.
    ignores: Gitignore,
    /// Directories the user has collapsed. Stored as COLLAPSED rather than
    /// expanded so the default (everything open to max_depth) needs no state
    /// and survives a rebuild without having to be reconstructed.
    collapsed: std::collections::HashSet<PathBuf>,
    /// Directories un-folded automatically to show what Claude just touched,
    /// remembered so they can be re-folded. A user's explicit click always
    /// wins over these.
    auto_expanded: Vec<PathBuf>,
}

impl FileTree {
    pub fn new(root: &Path, show_hidden: bool, max_depth: usize) -> Result<Self> {
        let tree = Self {
            root: root.to_path_buf(),
            nodes: Vec::new(),
            show_hidden,
            max_depth,
            offset: 0,
            h_offset: 0,
            content_width: 0,
            ignores: build_ignores(root),
            collapsed: std::collections::HashSet::new(),
            auto_expanded: Vec::new(),
        };

        // Deliberately NOT walked here.
        //
        // This used to walk synchronously, justified as "before the event loop
        // exists". That was a category error: the hazard was never that the
        // loop was running. By this point raw mode is on (so cfmakeraw has
        // cleared ISIG and Ctrl-C is a 0x03 byte nobody reads), the alternate
        // screen is blank, and no signal disposition exists yet. Uncapped
        // blocking I/O in that window is only escapable with SIGKILL from
        // another terminal.
        //
        // Measured: this repo 0.7-1.5 ms, but $HOME at depth 5 is 416 ms,
        // ~/Developer at depth 10 is 699 ms, and on a cold cache or an NFS or
        // SMB mount there is no ceiling at all.
        //
        // The caller draws a frame first, then issues the initial walk through
        // the same spawn_walk path, with the same caps as every other walk.
        Ok(tree)
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    pub fn nodes(&self) -> &[FileNode] {
        &self.nodes
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn set_offset(&mut self, offset: usize) {
        self.offset = offset;
    }

    /// Un-fold whatever is hiding `path`, so following Claude survives a fold.
    ///
    /// Without this the premise fails silently: fold `src/` and every file
    /// Claude touches beneath it is unreachable -- `index_of` returns None,
    /// `reveal` returns false, and `poll_activity` burns its whole retry budget
    /// on walks that cannot possibly help. Folding the repo name, one
    /// documented click, reduced the tree to a single node and killed following
    /// for the rest of the session.
    ///
    /// Anything auto-expanded for a PREVIOUS reveal and not needed for this one
    /// is re-folded here, so the restore rides along on the same walk and needs
    /// no separate "activity left the subtree" test.
    ///
    /// Returns true when the fold set changed and a walk is needed.
    pub fn reveal_ancestors(&mut self, path: &Path) -> bool {
        let mut changed = false;

        // Re-fold anything we opened for an earlier reveal that this one does
        // not need. An explicit click removes entries from auto_expanded, so
        // this can never undo a deliberate expansion.
        let still_needed: Vec<PathBuf> = self
            .auto_expanded
            .iter()
            .filter(|d| path.starts_with(d))
            .cloned()
            .collect();
        for dir in std::mem::replace(&mut self.auto_expanded, still_needed) {
            if !path.starts_with(&dir) {
                changed |= self.collapsed.insert(dir);
            }
        }

        // Open every folded ancestor of the target.
        let mut ancestor = path.parent();
        while let Some(dir) = ancestor {
            if self.collapsed.remove(dir) {
                self.auto_expanded.push(dir.to_path_buf());
                changed = true;
            }
            if dir == self.root {
                break;
            }
            ancestor = dir.parent();
        }
        changed
    }

    #[cfg(test)]
    pub fn expand_all_for_test(&mut self) {
        self.collapsed.clear();
        self.auto_expanded.clear();
    }

    /// Toggle a directory open or closed. Returns true when the fold state
    /// changed, so the caller knows to schedule a walk.
    ///
    /// Deliberately does NOT walk: this is called from the mouse handler on the
    /// event-loop thread, and walking there is the hazard the whole
    /// spawn_blocking arrangement exists to remove. The caller issues the walk;
    /// the fold appears when it lands.
    pub fn toggle(&mut self, path: &Path) -> bool {
        let Some(node) = self.nodes.iter().find(|n| n.path == path) else {
            return false;
        };
        if !node.is_dir {
            return false;
        }
        let path = path.to_path_buf();
        // A deliberate click takes this directory out of automatic control.
        self.auto_expanded.retain(|d| *d != path);
        let is_root = path == self.root;
        if self.collapsed.contains(&path) {
            // Reopening unfolds only this directory. Reopening the root shows
            // the top level with its children still folded, which is the point
            // of folding the root in the first place.
            self.collapsed.remove(&path);
        } else if is_root {
            // Folding the root folds everything, so reopening it shows the
            // top level rather than the whole tree again.
            self.collapse_recursive(&path);
        } else {
            self.collapsed.insert(path);
        }
        true
    }

    pub fn is_collapsed(&self, path: &Path) -> bool {
        self.collapsed.contains(path)
    }

    /// Where the scrollbar thumb sits, as (first_row, height) in viewport
    /// rows, or None when everything fits and no scrollbar is drawn.
    ///
    /// Shared by the renderer and the mouse handler deliberately: two copies of
    /// this arithmetic would drift, and the click would stop landing where the
    /// thumb is drawn.
    pub fn scrollbar_thumb(&self, visible_height: usize) -> Option<(usize, usize)> {
        crate::scrollbar::thumb(self.nodes.len(), visible_height, self.offset)
    }

    /// Scroll so the thumb's TOP lands on `row`, clamped. Used for a drag.
    pub fn scroll_to_thumb_row(&mut self, row: usize, visible_height: usize) {
        if self.scrollbar_thumb(visible_height).is_none() {
            return;
        }
        self.offset =
            crate::scrollbar::offset_for_thumb_pos(row, self.nodes.len(), visible_height);
    }

    /// Page up or down, for a click on the scrollbar track above or below the
    /// thumb -- what every scrollbar does.
    pub fn page(&mut self, down: bool, visible_height: usize) {
        let max_offset = self.nodes.len().saturating_sub(visible_height);
        self.offset = if down {
            (self.offset + visible_height).min(max_offset)
        } else {
            self.offset.saturating_sub(visible_height)
        };
    }

    pub fn h_offset(&self) -> usize {
        self.h_offset
    }

    pub fn set_h_offset(&mut self, offset: usize) {
        self.h_offset = offset;
    }

    pub fn content_width(&self) -> usize {
        self.content_width
    }

    /// Horizontal scrollbar thumb as (column, length) in track cells, or
    /// None when the tree fits. Shared by renderer and mouse handler for
    /// the same reason as the vertical one.
    pub fn hscrollbar_thumb(&self, visible_width: usize) -> Option<(usize, usize)> {
        crate::scrollbar::thumb(self.content_width, visible_width, self.h_offset)
    }

    /// Scroll so the thumb's LEFT edge lands on `col`, clamped. For drags.
    pub fn scroll_to_hthumb_col(&mut self, col: usize, visible_width: usize) {
        if self.hscrollbar_thumb(visible_width).is_none() {
            return;
        }
        self.h_offset =
            crate::scrollbar::offset_for_thumb_pos(col, self.content_width, visible_width);
    }

    /// Page left or right, for a click on the track either side of the thumb.
    pub fn hpage(&mut self, right: bool, visible_width: usize) {
        let max_offset = self.content_width.saturating_sub(visible_width);
        self.h_offset = if right {
            (self.h_offset + visible_width).min(max_offset)
        } else {
            self.h_offset.saturating_sub(visible_width)
        };
    }

    /// Collapse a directory and every directory beneath it.
    ///
    /// Folding the root should fold the whole tree, not just hide it: when the
    /// root is reopened the user expects to see top-level entries, not the
    /// fully-expanded tree they just closed.
    pub fn collapse_recursive(&mut self, path: &Path) {
        let descendants: Vec<PathBuf> = self
            .nodes
            .iter()
            .filter(|n| n.is_dir && n.path.starts_with(path))
            .map(|n| n.path.clone())
            .collect();
        // Folding a subtree also drops any automatic expansions inside it, or
        // the next reveal would resurrect them.
        self.auto_expanded.retain(|d| !d.starts_with(path));
        self.collapsed.extend(descendants);
    }

    /// The node at a viewport row, accounting for the scroll offset.
    pub fn node_at_row(&self, row: usize) -> Option<&FileNode> {
        self.nodes.get(self.offset + row)
    }

    pub fn collapsed_set(&self) -> &std::collections::HashSet<PathBuf> {
        &self.collapsed
    }

    /// Replace the node list with one produced off-thread by [`walk`].
    pub fn set_nodes(&mut self, nodes: Vec<FileNode>) {
        self.nodes = nodes;
        let max_offset = self.nodes.len().saturating_sub(1);
        self.offset = self.offset.min(max_offset);
        self.content_width = self.nodes.iter().map(line_width).max().unwrap_or(0);
        // A fold that narrows the tree must not leave the view scrolled
        // past the content, same as the vertical clamp above.
        self.h_offset = self.h_offset.min(self.content_width);
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    pub fn max_depth(&self) -> usize {
        self.max_depth
    }
}

/// Walk `root` and produce the visible node list.
///
/// A free function, and deliberately so: this is unbounded I/O whose cost is a
/// property of the filesystem, not of anything Canopy controls. It must be
/// callable from `spawn_blocking`, off the event-loop thread. Running it inline
/// froze the UI hard enough to need SIGKILL.
/// Children of a collapsed directory are skipped.
pub fn walk(
    root: &Path,
    show_hidden: bool,
    max_depth: usize,
    collapsed: &std::collections::HashSet<PathBuf>,
) -> Vec<FileNode> {
    // ONE walker for the whole tree. The previous implementation created a
    // fresh WalkBuilder per directory with max_depth(1) and recursed -- 2,045
    // walkers and 186,649 is_dir() stats on a 20k-node repo, 443 ms. Every
    // DirEntry already carries its file type from the readdir, so is_dir() was
    // re-stat-ing paths the walker had just described.
    let mut children: std::collections::HashMap<PathBuf, Vec<(PathBuf, String, bool)>> =
        std::collections::HashMap::new();

    let walker = WalkBuilder::new(root)
        .hidden(!show_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .max_depth(Some(max_depth))
        .build();

    for entry in walker.flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let Some(parent) = path.parent() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        children
            .entry(parent.to_path_buf())
            .or_default()
            .push((path.to_path_buf(), name, is_dir));
    }

    // Dirs first, then case-insensitive by name, sorted once per directory
    // with a precomputed key.
    for list in children.values_mut() {
        list.sort_by_cached_key(|(_, name, is_dir)| (!*is_dir, name.to_lowercase()));
    }

    let mut nodes = Vec::new();
    let is_dir = root.is_dir();
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| root.to_string_lossy().to_string());
    nodes.push(FileNode::new(
        root.to_path_buf(),
        name,
        0,
        is_dir,
        true,
        vec![],
    ));
    // The root's own fold was not honoured here -- only its descendants' --
    // so collapsing the repo name still listed the top level.
    if is_dir && !collapsed.contains(root) {
        emit_children(&mut nodes, root, 1, &[], &children, collapsed);
    }
    nodes
}

/// Emit a directory's children depth-first, tracking the connector state each
/// row needs to draw its ancestry.
#[allow(clippy::too_many_arguments)]
fn emit_children(
    nodes: &mut Vec<FileNode>,
    parent: &Path,
    depth: usize,
    connector: &[bool],
    children: &std::collections::HashMap<PathBuf, Vec<(PathBuf, String, bool)>>,
    collapsed: &std::collections::HashSet<PathBuf>,
) {
    let Some(list) = children.get(parent) else {
        return;
    };
    let total = list.len();
    for (i, (path, name, is_dir)) in list.iter().enumerate() {
        let is_last = i == total - 1;
        nodes.push(FileNode::new(
            path.clone(),
            name.clone(),
            depth,
            *is_dir,
            is_last,
            connector.to_vec(),
        ));
        if *is_dir && !collapsed.contains(path) {
            let mut child_connector = connector.to_vec();
            child_connector.push(is_last);
            emit_children(
                nodes,
                path,
                depth + 1,
                &child_connector,
                children,
                collapsed,
            );
        }
    }
}

/// Display width of a node's rendered line: indent, icon, name. The
/// connectors cost 2 columns per depth level and the icon is always 2
/// ("▾ ", "▸ ", "· "). The transient 2-column CWD marker is deliberately
/// excluded: it moves with the child's cwd, and the right-edge truncation
/// marker covers the rare frame where the CWD line is also the widest.
fn line_width(node: &FileNode) -> usize {
    node.depth * 2 + 2 + unicode_width::UnicodeWidthStr::width(node.name.as_str())
}

impl FileTree {
    /// Index of the node for `path`, if it is currently in the tree.
    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.nodes.iter().position(|n| n.path == path)
    }

    /// Scroll so `path` is on screen, roughly centred. Returns false when the
    /// path is not in the tree — it may be newly created, or deeper than
    /// `max_depth`. Callers can `refresh()` and try once more.
    pub fn reveal(&mut self, path: &Path, visible_height: usize) -> bool {
        let Some(idx) = self.index_of(path) else {
            return false;
        };

        if visible_height == 0 {
            return true;
        }

        // Already comfortably on screen: leave the view alone rather than
        // yanking it around on every event.
        let margin = 2usize;
        let first_settled = self.offset + margin.min(visible_height / 2);
        let last_settled = (self.offset + visible_height).saturating_sub(margin + 1);
        if idx >= first_settled && idx <= last_settled {
            return true;
        }

        let max_offset = self.nodes.len().saturating_sub(visible_height);
        self.offset = idx.saturating_sub(visible_height / 2).min(max_offset);
        true
    }

    /// True when `path` can NEVER appear in the tree, so rebuilding to look for
    /// it is guaranteed waste. `is_noise` alone is not enough: the commonest
    /// unshowable path is not gitignored, it is HIDDEN. With show_hidden false,
    /// `.github/workflows/*.yml`, `.claude/*` and `.env` are never in the tree,
    /// and a tool-use on any of them cost a full walk every time.
    pub fn can_never_show(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return true; // outside the tree entirely
        };
        let components: Vec<_> = relative.components().collect();
        if components.len() > self.max_depth {
            return true;
        }
        if !self.show_hidden
            && components
                .iter()
                .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        {
            return true;
        }
        // NOTE: a folded ancestor is deliberately NOT "can never show". Folds
        // are transient and reveal_ancestors opens them, so treating a folded
        // path as unshowable would be the same silent failure from the other
        // direction.
        self.is_noise(path)
    }

    /// True when a filesystem event for `path` cannot affect what is displayed,
    /// so the tree need not be rebuilt. Build output is the case that matters:
    /// `cargo build` touches thousands of files under `target/`, and rebuilding
    /// the tree for each one would stall the UI.
    pub fn is_noise(&self, path: &Path) -> bool {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        if relative
            .components()
            .any(|c| ALWAYS_NOISY.contains(&c.as_os_str().to_string_lossy().as_ref()))
        {
            return true;
        }
        self.ignores
            .matched_path_or_any_parents(path, path.is_dir())
            .is_ignore()
    }

    /// Re-root WITHOUT walking. The caller must schedule a walk via
    /// `App::request_refresh`, which runs it off the event-loop thread.
    ///
    /// This used to walk inline. It is called from `tick()`, so re-rooting to a
    /// large directory froze the UI until the walk finished — the failure that
    /// needed SIGKILL.
    pub fn set_root(&mut self, new_root: PathBuf) {
        self.ignores = build_ignores(&new_root);
        self.root = new_root;
        self.offset = 0;
        self.nodes.clear();
    }
}

/// Compile the root `.gitignore`, if there is one. Nested ignore files are not
/// consulted: this only filters event noise, and the tree walk itself already
/// applies full gitignore semantics via `WalkBuilder`.
fn build_ignores(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    let _ = builder.add(root.join(".gitignore"));
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

#[cfg(test)]
mod noise_tests {
    use super::*;

    fn tree_with_gitignore(contents: &str) -> (tempfile::TempDir, FileTree) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), contents).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let tree = FileTree::new(dir.path(), false, 5).unwrap();
        (dir, tree)
    }

    #[test]
    fn build_output_is_noise_even_when_not_gitignored() {
        // The case that pegged a core: a recursive watcher reporting every file
        // cargo writes under target/, each one triggering a tree rebuild.
        let (dir, tree) = tree_with_gitignore("");
        assert!(tree.is_noise(&dir.path().join("target/debug/build/x/out")));
        assert!(tree.is_noise(&dir.path().join("node_modules/react/index.js")));
        assert!(tree.is_noise(&dir.path().join(".git/objects/ab/cdef")));
    }

    #[test]
    fn gitignored_paths_are_noise() {
        let (dir, tree) = tree_with_gitignore("*.log\nscratch/\n");
        assert!(tree.is_noise(&dir.path().join("debug.log")));
        assert!(tree.is_noise(&dir.path().join("scratch/notes.txt")));
    }

    #[test]
    fn real_source_files_are_never_noise() {
        let (dir, tree) = tree_with_gitignore("*.log\n");
        assert!(!tree.is_noise(&dir.path().join("src/main.rs")));
        assert!(!tree.is_noise(&dir.path().join("README.md")));
        assert!(!tree.is_noise(&dir.path().join("Cargo.toml")));
    }

    #[test]
    fn a_directory_merely_named_like_a_source_dir_is_still_shown() {
        // "build" is in ALWAYS_NOISY, but only as a path component of the root.
        let (dir, tree) = tree_with_gitignore("");
        assert!(!tree.is_noise(&dir.path().join("src/builder.rs")));
    }
}

#[cfg(test)]
fn built_local(root: &std::path::Path, hidden: bool, depth: usize) -> FileTree {
    let mut t = FileTree::new(root, hidden, depth).unwrap();
    let nodes = walk(root, hidden, depth, t.collapsed_set());
    t.set_nodes(nodes);
    t
}

#[cfg(test)]
mod walk_tests {
    use super::built_local;
    use super::*;

    /// Deterministic fixture exercising ordering, nesting, connectors and
    /// gitignore. The golden below is the OUTPUT OF THE ORIGINAL recursive
    /// implementation, captured before it was rewritten, so any change in
    /// shape, order or connector state fails loudly.
    fn fixture() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        // The `ignore` crate applies .gitignore only inside a git repository,
        // which is correct git semantics -- a .gitignore outside a repo has no
        // meaning. Without this the fixture would silently test nothing.
        std::fs::create_dir_all(p.join(".git")).unwrap();
        std::fs::write(p.join(".gitignore"), "ignored.txt\nbuilt/\n").unwrap();
        for dir in [
            "src",
            "src/inner",
            "src/inner/deep",
            "zed",
            "Alpha",
            "built",
        ] {
            std::fs::create_dir_all(p.join(dir)).unwrap();
        }
        for f in [
            "README.md",
            "Cargo.toml",
            "ignored.txt",
            "src/main.rs",
            "src/lib.rs",
            "src/Zed.rs",
            "src/alpha.rs",
            "src/inner/a.rs",
            "src/inner/deep/b.rs",
            "zed/z.rs",
            "Alpha/a.rs",
            "built/artifact.o",
        ] {
            std::fs::write(p.join(f), "x").unwrap();
        }
        d
    }

    fn render(tree: &FileTree) -> Vec<String> {
        tree.nodes()
            .iter()
            .map(|n| {
                format!(
                    "{}{}{} last={} conn={:?}",
                    "  ".repeat(n.depth),
                    if n.is_dir { "d " } else { "f " },
                    n.name,
                    n.is_last,
                    n.connector
                )
            })
            .collect()
    }

    #[test]
    fn walk_output_is_stable() {
        let d = fixture();
        let tree = built_local(d.path(), false, 10);
        let got = render(&tree);

        // dirs before files, case-insensitive within each group, gitignore
        // respected, depth-first with connectors tracking ancestry.
        let names: Vec<&str> = got.iter().map(|s| s.as_str()).collect();
        assert!(names[0].starts_with("d "), "root first: {:?}", names[0]);

        let body: Vec<String> = got[1..].to_vec();
        let expected = vec![
            "  d Alpha last=false conn=[]",
            "    f a.rs last=true conn=[false]",
            "  d src last=false conn=[]",
            "    d inner last=false conn=[false]",
            "      d deep last=false conn=[false, false]",
            "        f b.rs last=true conn=[false, false, false]",
            "      f a.rs last=true conn=[false, false]",
            "    f alpha.rs last=false conn=[false]",
            "    f lib.rs last=false conn=[false]",
            "    f main.rs last=false conn=[false]",
            "    f Zed.rs last=true conn=[false]",
            "  d zed last=false conn=[]",
            "    f z.rs last=true conn=[false]",
            "  f Cargo.toml last=false conn=[]",
            "  f README.md last=true conn=[]",
        ];
        assert_eq!(body, expected, "walk output changed");

        assert!(
            !got.iter().any(|s| s.contains("ignored.txt")),
            "gitignore ignored"
        );
        assert!(
            !got.iter().any(|s| s.contains("built")),
            "built/ not excluded"
        );
    }

    #[test]
    fn max_depth_is_respected() {
        let d = fixture();
        let shallow = built_local(d.path(), false, 2);
        assert!(shallow.nodes().iter().all(|n| n.depth <= 2));
        let deep = built_local(d.path(), false, 10);
        assert!(deep.nodes().len() > shallow.nodes().len());
    }
}

#[cfg(test)]
mod visibility_tests {
    use super::built_local;
    use super::*;

    fn tree_with(hidden: bool, depth: usize) -> (tempfile::TempDir, FileTree) {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/a/b/c")).unwrap();
        std::fs::create_dir_all(d.path().join(".github/workflows")).unwrap();
        let t = built_local(d.path(), hidden, depth);
        (d, t)
    }

    #[test]
    fn hidden_paths_can_never_show_so_never_justify_a_rebuild() {
        // The refresh amplifier: poll_activity rebuilt the whole tree on any
        // failed reveal. The commonest unshowable path is not gitignored, it is
        // hidden -- .github, .claude, .env -- so a tool-use on one cost a full
        // filesystem walk every time.
        let (d, t) = tree_with(false, 10);
        assert!(t.can_never_show(&d.path().join(".github/workflows/ci.yml")));
        assert!(t.can_never_show(&d.path().join(".env")));
        assert!(!t.can_never_show(&d.path().join("src/main.rs")));
    }

    #[test]
    fn paths_outside_the_root_and_below_max_depth_can_never_show() {
        let (d, t) = tree_with(false, 2);
        assert!(t.can_never_show(Path::new("/somewhere/else.rs")));
        assert!(t.can_never_show(&d.path().join("src/a/b/c/deep.rs")));
        assert!(!t.can_never_show(&d.path().join("src/main.rs")));
    }

    #[test]
    fn show_hidden_makes_dotfiles_showable_again() {
        let (d, t) = tree_with(true, 10);
        assert!(!t.can_never_show(&d.path().join(".github/workflows/ci.yml")));
    }
}

#[cfg(test)]
mod collapse_tests {
    use super::built_local;
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/inner")).unwrap();
        std::fs::create_dir_all(d.path().join("docs")).unwrap();
        std::fs::write(d.path().join("src/main.rs"), "x").unwrap();
        std::fs::write(d.path().join("src/inner/deep.rs"), "x").unwrap();
        std::fs::write(d.path().join("docs/readme.md"), "x").unwrap();
        d
    }

    /// The tree drew expand/collapse chevrons from the start but had no
    /// expansion state at all and never received a keystroke, so the glyph
    /// promised something that did not exist.
    #[test]
    fn collapsing_a_directory_hides_its_children() {
        let d = fixture();
        let mut t = built_local(d.path(), false, 10);
        let before = t.nodes().len();
        assert!(t.nodes().iter().any(|n| n.name == "main.rs"));

        assert!(
            t.toggle(&d.path().join("src")),
            "toggling a dir must report a change"
        );
        assert!(t.is_collapsed(&d.path().join("src")));
        // toggle() only records the fold; the walk that applies it runs
        // off-thread, because walking from the mouse handler is the hazard the
        // whole spawn_blocking arrangement exists to remove.
        let nodes = walk(d.path(), false, 10, t.collapsed_set());
        t.set_nodes(nodes);
        assert!(
            !t.nodes().iter().any(|n| n.name == "main.rs"),
            "children still shown"
        );
        assert!(
            !t.nodes().iter().any(|n| n.name == "deep.rs"),
            "grandchildren still shown"
        );
        assert!(
            t.nodes().iter().any(|n| n.name == "src"),
            "the dir itself must remain"
        );
        assert!(
            t.nodes().iter().any(|n| n.name == "readme.md"),
            "siblings must be untouched"
        );
        assert!(t.nodes().len() < before);

        assert!(t.toggle(&d.path().join("src")));
        assert!(!t.is_collapsed(&d.path().join("src")));
        let nodes = walk(d.path(), false, 10, t.collapsed_set());
        t.set_nodes(nodes);
        assert_eq!(t.nodes().len(), before, "reopening must restore exactly");
    }

    #[test]
    fn toggling_a_file_does_nothing() {
        let d = fixture();
        let mut t = built_local(d.path(), false, 10);
        let before = t.nodes().len();
        assert!(!t.toggle(&d.path().join("src/main.rs")));
        assert_eq!(t.nodes().len(), before);
    }

    /// A rescan must not silently re-open everything the user closed. The
    /// off-thread walk takes the collapsed set for exactly this reason.
    #[test]
    fn fold_state_survives_a_rescan() {
        let d = fixture();
        let mut t = built_local(d.path(), false, 10);
        t.toggle(&d.path().join("src"));
        // Apply the fold, as the off-thread walk would.
        let nodes = walk(d.path(), false, 10, t.collapsed_set());
        t.set_nodes(nodes);
        let folded = t.nodes().len();

        // A LATER rescan -- triggered by a filesystem event, say -- must not
        // quietly re-open what the user closed.
        let nodes = walk(d.path(), false, 10, t.collapsed_set());
        assert_eq!(
            nodes.len(),
            folded,
            "an off-thread rescan re-opened the tree"
        );
        assert!(!nodes.iter().any(|n| n.name == "main.rs"));
    }

    #[test]
    fn node_at_row_accounts_for_scrolling() {
        let d = fixture();
        let mut t = built_local(d.path(), false, 10);
        let first = t.node_at_row(0).unwrap().path.clone();
        assert_eq!(first, t.root_path());
        t.set_offset(2);
        let shifted = t.node_at_row(0).unwrap().path.clone();
        assert_ne!(shifted, first, "clicking must follow the scroll offset");
        assert_eq!(shifted, t.nodes()[2].path);
    }
}

#[cfg(test)]
mod scrollbar_tests {
    use super::*;

    fn tall(n: usize) -> (tempfile::TempDir, FileTree) {
        let d = tempfile::tempdir().unwrap();
        for i in 0..n {
            std::fs::write(d.path().join(format!("f{i:03}.txt")), "x").unwrap();
        }
        let t = built_local(d.path(), false, 10);
        (d, t)
    }

    #[test]
    fn no_thumb_when_everything_fits() {
        let (_d, t) = tall(5);
        assert!(
            t.scrollbar_thumb(50).is_none(),
            "no scrollbar when it all fits"
        );
        assert!(
            t.scrollbar_thumb(0).is_none(),
            "no scrollbar in zero height"
        );
    }

    /// The renderer and the mouse handler share this arithmetic on purpose. Two
    /// copies would drift and the drag would stop landing where the thumb is.
    #[test]
    fn the_thumb_stays_inside_the_track_at_every_offset() {
        let (_d, mut t) = tall(200);
        let visible = 20;
        let max_offset = t.nodes().len() - visible;
        for offset in 0..=max_offset {
            t.set_offset(offset);
            let (pos, height) = t.scrollbar_thumb(visible).expect("thumb");
            assert!(height >= 1, "thumb must be visible");
            assert!(
                pos + height <= visible,
                "thumb ran past the track at offset {offset}: {pos}+{height} > {visible}"
            );
        }
    }

    #[test]
    fn dragging_the_thumb_to_the_ends_reaches_the_ends() {
        let (_d, mut t) = tall(200);
        let visible = 20;
        let max_offset = t.nodes().len() - visible;

        t.scroll_to_thumb_row(0, visible);
        assert_eq!(t.offset(), 0, "dragging to the top must show the first row");

        t.scroll_to_thumb_row(visible, visible);
        assert_eq!(
            t.offset(),
            max_offset,
            "dragging to the bottom must show the last row"
        );
    }

    #[test]
    fn a_drag_round_trips_through_the_thumb_position() {
        // Grab the thumb, move it, and the thumb should follow the pointer.
        let (_d, mut t) = tall(200);
        let visible = 20;
        for target in [0usize, 3, 7, 11, 15] {
            t.scroll_to_thumb_row(target, visible);
            let (pos, _) = t.scrollbar_thumb(visible).expect("thumb");
            let drift = pos.abs_diff(target);
            assert!(
                drift <= 1,
                "thumb landed at {pos} for a drag to {target} (drift {drift})"
            );
        }
    }

    #[test]
    fn clicking_the_track_pages() {
        let (_d, mut t) = tall(200);
        let visible = 20;
        t.page(true, visible);
        assert_eq!(t.offset(), visible, "page down moves one screen");
        t.page(false, visible);
        assert_eq!(t.offset(), 0, "page up returns");
        // And cannot run off either end.
        for _ in 0..50 {
            t.page(true, visible);
        }
        assert_eq!(t.offset(), t.nodes().len() - visible);
        for _ in 0..50 {
            t.page(false, visible);
        }
        assert_eq!(t.offset(), 0);
    }
}

#[cfg(test)]
mod recursive_collapse_tests {
    use super::*;

    fn nested() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for p in ["a/b/c", "x/y", "z"] {
            std::fs::create_dir_all(d.path().join(p)).unwrap();
        }
        std::fs::write(d.path().join("a/b/c/deep.rs"), "x").unwrap();
        std::fs::write(d.path().join("x/y/mid.rs"), "x").unwrap();
        std::fs::write(d.path().join("z/leaf.rs"), "x").unwrap();
        d
    }

    fn rewalk(t: &mut FileTree, root: &Path) {
        let nodes = walk(root, false, 10, t.collapsed_set());
        t.set_nodes(nodes);
    }

    /// Folding the root folds everything beneath it, so reopening shows the top
    /// level rather than the fully-expanded tree you just closed.
    /// Following Claude must survive a fold. This is the premise of the whole
    /// tool, and adding click-to-collapse silently broke it: folding `src/`
    /// made every file beneath it unreachable -- index_of None, reveal false --
    /// while can_never_show said false, so poll_activity burned its entire
    /// retry budget on walks that could not possibly help. Folding the repo
    /// name, one documented click, reduced the tree to a single node and killed
    /// following for the rest of the session.
    #[test]
    fn following_survives_a_fold() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/tree")).unwrap();
        std::fs::write(d.path().join("src/tree/mod.rs"), "x").unwrap();
        std::fs::write(d.path().join("README.md"), "x").unwrap();
        let mut t = built_local(d.path(), false, 10);
        let target = d.path().join("src/tree/mod.rs");

        for fold in [d.path().join("src"), d.path().to_path_buf()] {
            t.toggle(&fold);
            rewalk(&mut t, d.path());
            assert!(t.index_of(&target).is_none(), "precondition: it is hidden");

            // What the app does when Claude touches a hidden file.
            assert!(t.reveal_ancestors(&target), "should need a walk");
            rewalk(&mut t, d.path());
            assert!(
                t.reveal(&target, 20),
                "Claude touched a file and the tree could not show it (folded {})",
                fold.display()
            );

            // Put it back for the next case.
            t.expand_all_for_test();
            rewalk(&mut t, d.path());
        }
    }

    /// An automatic expansion must not leak: once activity moves elsewhere, the
    /// directory folds again, so the tree does not slowly unfold itself.
    #[test]
    fn auto_expansion_is_undone_when_activity_moves_on() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/deep")).unwrap();
        std::fs::create_dir_all(d.path().join("b")).unwrap();
        std::fs::write(d.path().join("a/deep/one.rs"), "x").unwrap();
        std::fs::write(d.path().join("b/two.rs"), "x").unwrap();
        let mut t = built_local(d.path(), false, 10);

        t.toggle(&d.path().join("a"));
        rewalk(&mut t, d.path());
        assert!(t.is_collapsed(&d.path().join("a")));

        t.reveal_ancestors(&d.path().join("a/deep/one.rs"));
        rewalk(&mut t, d.path());
        assert!(
            !t.is_collapsed(&d.path().join("a")),
            "opened to show the file"
        );

        // Activity moves to an unrelated subtree.
        t.reveal_ancestors(&d.path().join("b/two.rs"));
        rewalk(&mut t, d.path());
        assert!(
            t.is_collapsed(&d.path().join("a")),
            "an automatic expansion leaked; the tree would unfold itself over time"
        );
    }

    /// A deliberate click outranks the automatic machinery, always.
    #[test]
    fn an_explicit_click_beats_an_automatic_expansion() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/deep")).unwrap();
        std::fs::create_dir_all(d.path().join("b")).unwrap();
        std::fs::write(d.path().join("a/deep/one.rs"), "x").unwrap();
        std::fs::write(d.path().join("b/two.rs"), "x").unwrap();
        let mut t = built_local(d.path(), false, 10);

        t.toggle(&d.path().join("a"));
        t.reveal_ancestors(&d.path().join("a/deep/one.rs"));
        // The user now opens it deliberately.
        t.toggle(&d.path().join("a"));
        assert!(t.is_collapsed(&d.path().join("a")), "click folds it");

        // Activity elsewhere must not resurrect the automatic expansion.
        t.reveal_ancestors(&d.path().join("b/two.rs"));
        rewalk(&mut t, d.path());
        assert!(
            t.is_collapsed(&d.path().join("a")),
            "the automatic machinery overrode a deliberate click"
        );
    }

    #[test]
    fn collapsing_the_root_collapses_every_directory_under_it() {
        let d = nested();
        let mut t = built_local(d.path(), false, 10);
        assert!(t.nodes().iter().any(|n| n.name == "deep.rs"));

        t.toggle(d.path());
        rewalk(&mut t, d.path());
        assert_eq!(t.nodes().len(), 1, "only the root should remain");

        for dir in ["a", "a/b", "a/b/c", "x", "x/y", "z"] {
            assert!(
                t.is_collapsed(&d.path().join(dir)),
                "{dir} should have been folded with the root"
            );
        }

        // Reopening shows the top level, with its children still folded.
        t.toggle(d.path());
        rewalk(&mut t, d.path());
        let names: Vec<&str> = t.nodes().iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"a") && names.contains(&"x") && names.contains(&"z"));
        assert!(
            !names.contains(&"deep.rs"),
            "reopening the root must not re-expand the whole tree"
        );
    }

    #[test]
    fn collapsing_a_subtree_leaves_siblings_alone() {
        let d = nested();
        let mut t = built_local(d.path(), false, 10);
        t.toggle(&d.path().join("a"));
        rewalk(&mut t, d.path());
        assert!(!t.is_collapsed(&d.path().join("x")), "a sibling was folded");
        assert!(
            t.nodes().iter().any(|n| n.name == "mid.rs"),
            "sibling content vanished"
        );
        assert!(!t.nodes().iter().any(|n| n.name == "deep.rs"));
    }
}

#[cfg(test)]
mod hscrollbar_tests {
    use super::*;

    /// A tree whose widest line is `deep/a_really_quite_long_file_name.rs`
    /// at depth 2: 2*2 connector columns + 2 icon columns + 32 name = 38.
    fn wide_tree() -> FileTree {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("deep")).unwrap();
        std::fs::write(d.path().join("deep/a_really_quite_long_file_name.rs"), "x").unwrap();
        let mut t = FileTree::new(d.path(), false, 10).unwrap();
        t.set_nodes(walk(d.path(), false, 10, t.collapsed_set()));
        t
    }

    #[test]
    fn content_width_is_the_widest_line_in_columns() {
        let t = wide_tree();
        assert_eq!(t.content_width(), 38, "2*depth + icon + name");
    }

    #[test]
    fn no_thumb_when_the_pane_is_wide_enough() {
        let t = wide_tree();
        assert!(t.hscrollbar_thumb(38).is_none());
        assert!(t.hscrollbar_thumb(80).is_none());
        assert!(t.hscrollbar_thumb(0).is_none());
    }

    #[test]
    fn the_thumb_stays_inside_the_track_at_every_offset() {
        let mut t = wide_tree();
        let visible = 20;
        for offset in 0..=(t.content_width() - visible) {
            t.set_h_offset(offset);
            let (pos, len) = t.hscrollbar_thumb(visible).expect("thumb");
            assert!(pos + len <= visible, "escaped the track at offset {offset}");
        }
    }

    #[test]
    fn a_drag_lands_where_the_thumb_is_drawn() {
        let mut t = wide_tree();
        let visible = 20;
        t.scroll_to_hthumb_col(visible, visible); // past the end: clamps
        assert_eq!(t.h_offset(), t.content_width() - visible);
        t.scroll_to_hthumb_col(0, visible);
        assert_eq!(t.h_offset(), 0);
    }

    #[test]
    fn hpage_moves_one_viewport_and_clamps() {
        let mut t = wide_tree();
        let visible = 20;
        t.hpage(true, visible);
        assert_eq!(t.h_offset(), t.content_width() - visible, "one page covers it");
        t.hpage(true, visible);
        assert_eq!(t.h_offset(), t.content_width() - visible, "clamped at the end");
        t.hpage(false, visible);
        assert_eq!(t.h_offset(), 0);
    }

    /// A fold that narrows the tree must not leave the view scrolled past
    /// the content -- the vertical axis clamps in set_nodes for the same
    /// reason.
    #[test]
    fn set_nodes_clamps_a_stale_h_offset() {
        let mut t = wide_tree();
        t.set_h_offset(30);
        t.set_nodes(Vec::new());
        assert_eq!(t.h_offset(), 0, "empty tree has nowhere to scroll");
    }
}
