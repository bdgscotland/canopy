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

    fn rebuild_visible_nodes(&mut self) -> Result<()> {
        self.nodes.clear();
        self.build_tree(&self.root.clone(), 0, &[])?;
        Ok(())
    }

    fn build_tree(&mut self, path: &Path, depth: usize, connector: &[bool]) -> Result<()> {
        if depth > self.max_depth {
            return Ok(());
        }

        if depth == 0 {
            // Root node
            let is_dir = path.is_dir();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string());

            let node = FileNode::new(path.to_path_buf(), name, 0, is_dir, true, vec![]);
            self.nodes.push(node);

            if is_dir {
                self.build_tree(path, depth + 1, &[])?;
            }
            return Ok(());
        }

        let walker = WalkBuilder::new(path)
            .hidden(!self.show_hidden)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .max_depth(Some(1))
            .build();

        // Collect children (skip the directory itself)
        let mut children: Vec<_> = walker
            .flatten()
            .filter(|entry| entry.path() != path)
            .filter(|entry| {
                if self.show_hidden {
                    return true;
                }
                let name = entry.file_name().to_string_lossy();
                !name.starts_with('.')
            })
            .collect();

        children.sort_by(|a, b| {
            let a_is_dir = a.path().is_dir();
            let b_is_dir = b.path().is_dir();
            match (a_is_dir, b_is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => {
                    let a_name = a.file_name().to_string_lossy().to_lowercase();
                    let b_name = b.file_name().to_string_lossy().to_lowercase();
                    a_name.cmp(&b_name)
                }
            }
        });

        let total = children.len();
        for (i, entry) in children.into_iter().enumerate() {
            let entry_path = entry.path().to_path_buf();
            let is_dir = entry_path.is_dir();
            let name = entry_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| entry_path.to_string_lossy().to_string());

            let is_last = i == total - 1;

            let node = FileNode::new(
                entry_path.clone(),
                name,
                depth,
                is_dir,
                is_last,
                connector.to_vec(),
            );
            self.nodes.push(node);

            // Recurse into directories with updated connector
            if is_dir {
                let mut child_connector = connector.to_vec();
                child_connector.push(is_last);
                self.build_tree(&entry_path, depth + 1, &child_connector)?;
            }
        }

        Ok(())
    }

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

    pub fn set_root(&mut self, new_root: PathBuf) {
        self.ignores = build_ignores(&new_root);
        self.root = new_root;
        self.offset = 0;
        let _ = self.rebuild_visible_nodes();
    }

    pub fn refresh(&mut self) {
        let _ = self.rebuild_visible_nodes();
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
