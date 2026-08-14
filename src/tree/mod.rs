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
    /// Used to discard filesystem events for paths the tree never shows.
    ignores: Gitignore,
    /// Directories the user has collapsed. Stored as COLLAPSED rather than
    /// expanded so the default (everything open to max_depth) needs no state
    /// and survives a rebuild without having to be reconstructed.
    collapsed: std::collections::HashSet<PathBuf>,
}

impl FileTree {
    pub fn new(root: &Path, show_hidden: bool, max_depth: usize) -> Result<Self> {
        let mut tree = Self {
            root: root.to_path_buf(),
            nodes: Vec::new(),
            show_hidden,
            max_depth,
            offset: 0,
            ignores: build_ignores(root),
            collapsed: std::collections::HashSet::new(),
        };

        tree.rebuild_visible_nodes()?;

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

    /// The ONLY synchronous walk left, and it runs once at construction before
    /// the event loop exists. Every later walk goes through spawn_blocking.
    fn rebuild_visible_nodes(&mut self) -> Result<()> {
        self.nodes = walk(
            &self.root,
            self.show_hidden,
            self.max_depth,
            &self.collapsed,
        );
        Ok(())
    }

    /// Toggle a directory open or closed. Returns false for a file, so callers
    /// can tell a no-op from a change worth redrawing for.
    pub fn toggle(&mut self, path: &Path) -> bool {
        let Some(node) = self.nodes.iter().find(|n| n.path == path) else {
            return false;
        };
        if !node.is_dir {
            return false;
        }
        let path = path.to_path_buf();
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path);
        }
        let _ = self.rebuild_visible_nodes();
        true
    }

    pub fn is_collapsed(&self, path: &Path) -> bool {
        self.collapsed.contains(path)
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
    if is_dir {
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
mod walk_tests {
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
        let tree = FileTree::new(d.path(), false, 10).unwrap();
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
        let shallow = FileTree::new(d.path(), false, 2).unwrap();
        assert!(shallow.nodes().iter().all(|n| n.depth <= 2));
        let deep = FileTree::new(d.path(), false, 10).unwrap();
        assert!(deep.nodes().len() > shallow.nodes().len());
    }
}

#[cfg(test)]
mod visibility_tests {
    use super::*;

    fn tree_with(hidden: bool, depth: usize) -> (tempfile::TempDir, FileTree) {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/a/b/c")).unwrap();
        std::fs::create_dir_all(d.path().join(".github/workflows")).unwrap();
        let t = FileTree::new(d.path(), hidden, depth).unwrap();
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
        let mut t = FileTree::new(d.path(), false, 10).unwrap();
        let before = t.nodes().len();
        assert!(t.nodes().iter().any(|n| n.name == "main.rs"));

        assert!(
            t.toggle(&d.path().join("src")),
            "toggling a dir must report a change"
        );
        assert!(t.is_collapsed(&d.path().join("src")));
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
        assert_eq!(t.nodes().len(), before, "reopening must restore exactly");
    }

    #[test]
    fn toggling_a_file_does_nothing() {
        let d = fixture();
        let mut t = FileTree::new(d.path(), false, 10).unwrap();
        let before = t.nodes().len();
        assert!(!t.toggle(&d.path().join("src/main.rs")));
        assert_eq!(t.nodes().len(), before);
    }

    /// A rescan must not silently re-open everything the user closed. The
    /// off-thread walk takes the collapsed set for exactly this reason.
    #[test]
    fn fold_state_survives_a_rescan() {
        let d = fixture();
        let mut t = FileTree::new(d.path(), false, 10).unwrap();
        t.toggle(&d.path().join("src"));
        let folded = t.nodes().len();

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
        let mut t = FileTree::new(d.path(), false, 10).unwrap();
        let first = t.node_at_row(0).unwrap().path.clone();
        assert_eq!(first, t.root_path());
        t.set_offset(2);
        let shifted = t.node_at_row(0).unwrap().path.clone();
        assert_ne!(shifted, first, "clicking must follow the scroll offset");
        assert_eq!(shifted, t.nodes()[2].path);
    }
}
