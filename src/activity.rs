//! Reads what Claude Code is doing from its session transcript.
//!
//! Claude Code appends one JSON object per line to
//! `~/.claude/projects/<mangled-cwd>/<session-id>.jsonl`. Every tool use is
//! recorded there with the tool name and, for file tools, the path it touched:
//!
//! ```jsonc
//! { "timestamp": "...", "sessionId": "...", "cwd": "/Users/me/proj",
//!   "message": { "content": [
//!       { "type": "tool_use", "name": "Edit",
//!         "input": { "file_path": "/Users/me/proj/src/main.rs" } } ] } }
//! ```
//!
//! That is the whole integration: no hooks, no settings.json, no MCP, and no
//! scraping of Claude's rendered output.
//!
//! The format is undocumented and can change between Claude Code releases, so
//! every step here fails soft. An unparseable line is skipped, a missing
//! transcript means no highlighting, and neither can take the tree down.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What Claude did to a file. Drives how strongly it is highlighted and
/// which glyph the tree shows next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Read,
    Edit,
    Write,
}

impl ActivityKind {
    fn from_tool_name(name: &str) -> Option<Self> {
        match name {
            "Read" | "NotebookRead" => Some(Self::Read),
            "Edit" | "MultiEdit" | "NotebookEdit" => Some(Self::Edit),
            "Write" => Some(Self::Write),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Activity {
    pub path: PathBuf,
    pub kind: ActivityKind,
}

/// What actually happened to a file, once the tree has said whether the
/// path already existed. Drives the per-file glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    Read,
    Edit,
    Create,
    Overwrite,
}

impl FileAction {
    pub fn classify(kind: ActivityKind, existed: bool) -> Self {
        match kind {
            ActivityKind::Read => Self::Read,
            ActivityKind::Edit => Self::Edit,
            ActivityKind::Write if existed => Self::Overwrite,
            ActivityKind::Write => Self::Create,
        }
    }
}

/// How strongly a glyph renders, by age. Computed at draw time so the
/// widget itself stays clock-free and testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fade {
    Bright,
    Dim,
}

/// Glyphs are full-color this long...
pub const GLYPH_BRIGHT: Duration = Duration::from_secs(10);
/// ...dim after that, gone after this.
pub const GLYPH_EXPIRY: Duration = Duration::from_secs(60);
/// A now-line older than this defers to the in-progress task instead.
pub const NOW_STALE: Duration = Duration::from_secs(30);

/// A non-file action worth narrating: what Claude is doing, not just what
/// it touched. Labels are display-ready strings because the transcript is
/// the only place the human-written descriptions exist.
#[derive(Debug, Clone)]
pub enum Event {
    Command { label: String },
    Search { pattern: String },
    Agent { label: String },
}

/// One poll's worth of transcript: file touches and narratable events,
/// separated because they feed different surfaces.
#[derive(Debug, Default)]
pub struct Polled {
    pub files: Vec<Activity>,
    pub events: Vec<Event>,
}

/// Cap a raw command used as a label. 60 columns keeps a one-line pane
/// row; the human-written description is preferred and never truncated.
fn truncate_label(s: &str) -> String {
    let mut out: String = s.chars().take(60).collect();
    if s.chars().nth(60).is_some() {
        out.push('…');
    }
    out
}

/// Claude Code mangles a project path into a directory name by replacing every
/// character that is not `[a-zA-Z0-9]` with `-` — not just the separators.
/// Extracted from the shipped binary (2.1.231):
///
/// ```js
/// function zmo(e){ return e.replace(/[^a-zA-Z0-9]/g,"-") }
/// ```
///
/// So `/Users/me/my_proj-v1.2` becomes `-Users-me-my-proj-v1-2`. Mapping only
/// the separators produces a directory that does not exist, and discovery then
/// fails silently for the whole session.
///
/// Claude also truncates names over a length limit and appends a hash of an
/// internal, non-portable function. We deliberately do not reimplement that —
/// `find_by_cwd` covers truncation, `CLAUDE_CONFIG_DIR`, and any future change
/// to the scheme with one mechanism.
fn mangle_project_path(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Claude Code resolves its config root as `CLAUDE_CONFIG_DIR ?? ~/.claude`.
pub(crate) fn config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    Some(dirs::home_dir()?.join(".claude"))
}

fn projects_dir(root: &Path) -> Option<PathBuf> {
    Some(
        config_dir()?
            .join("projects")
            .join(mangle_project_path(root)),
    )
}

/// Fallback when the mangled directory does not exist: scan every project
/// directory for the most recently modified transcript whose first line reports
/// this `cwd`. Covers Claude's name truncation, a changed mangling scheme, and
/// any case our reimplementation gets wrong.
fn find_by_cwd(root: &Path) -> Option<PathBuf> {
    let projects = config_dir()?.join("projects");
    let want = root.to_string_lossy();
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;

    for project in std::fs::read_dir(projects).ok()?.flatten() {
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().is_some_and(|(t, _)| modified <= *t) {
                continue;
            }
            // Only the first line is read, so this stays cheap even though some
            // transcripts are megabytes.
            let Ok(file) = File::open(&path) else {
                continue;
            };
            let mut first = String::new();
            if BufReader::new(file).read_line(&mut first).is_err() {
                continue;
            }
            let matches = serde_json::from_str::<serde_json::Value>(&first)
                .ok()
                .and_then(|v| v.get("cwd").and_then(|c| c.as_str()).map(String::from))
                .is_some_and(|cwd| cwd == want);
            if matches {
                best = Some((modified, path));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Tails the transcript of the Claude session working in `root`.
pub struct ActivityWatcher {
    root: PathBuf,
    /// Transcript we are following, and how far into it we have read.
    transcript: Option<PathBuf>,
    offset: u64,
    /// Session id pinned by the launcher, if it supplied one. When set, the
    /// transcript is unambiguous even with several Claude sessions in one repo.
    pinned_session: Option<String>,
    last_poll: Instant,
    last_discovery: Instant,
    /// Why no transcript is being followed, for diagnostics. Discovery failing
    /// silently is half of what made the mangling bug invisible.
    no_transcript_reason: Option<String>,
}

/// How often to look for new transcript bytes. Reading from a stored offset is
/// a stat plus a short read, so this is cheap.
const POLL_INTERVAL: Duration = Duration::from_millis(120);
/// How often to re-check which transcript is the live one. Only matters until
/// the session's file first appears.
const DISCOVERY_INTERVAL: Duration = Duration::from_millis(1500);

impl ActivityWatcher {
    pub fn new(root: &Path, pinned_session: Option<String>) -> Self {
        // Escape hatch: follow an explicit transcript. Useful when discovery
        // picks the wrong session, and for debugging.
        if let Ok(explicit) = std::env::var("CANOPY_TRANSCRIPT") {
            return Self::with_transcript(root, PathBuf::from(explicit), false);
        }
        // Start far enough in the past that the first tick polls immediately.
        let stale = Instant::now() - DISCOVERY_INTERVAL - Duration::from_secs(1);
        let mut w = Self {
            root: root.to_path_buf(),
            transcript: None,
            offset: 0,
            pinned_session,
            last_poll: stale,
            last_discovery: stale,
            no_transcript_reason: None,
        };
        w.discover();
        w
    }

    pub fn set_root(&mut self, root: PathBuf) {
        if root != self.root {
            self.root = root;
            self.transcript = None;
            self.offset = 0;
            self.discover();
        }
    }

    /// The session id of the transcript being followed, if any. Used to build
    /// the resume command shown when something goes wrong.
    pub fn session_id(&self) -> Option<String> {
        self.transcript
            .as_ref()?
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
    }

    /// The transcript currently being followed, for diagnostics.
    #[allow(dead_code)]
    pub fn transcript_path(&self) -> Option<&Path> {
        self.transcript.as_deref()
    }

    /// Follow a specific transcript, bypassing discovery. Used by tests and by
    /// the `CANOPY_TRANSCRIPT` escape hatch.
    #[allow(dead_code)]
    pub fn with_transcript(root: &Path, transcript: PathBuf, from_start: bool) -> Self {
        let stale = Instant::now() - DISCOVERY_INTERVAL - Duration::from_secs(1);
        let offset = if from_start {
            0
        } else {
            std::fs::metadata(&transcript).map(|m| m.len()).unwrap_or(0)
        };
        Self {
            root: root.to_path_buf(),
            transcript: Some(transcript),
            offset,
            pinned_session: None,
            last_poll: stale,
            last_discovery: Instant::now(),
            no_transcript_reason: None,
        }
    }

    /// Pick the transcript to follow. A pinned session id wins outright.
    /// Otherwise take the most recently modified `.jsonl` in the project
    /// directory, which is the session that most recently did anything.
    fn discover(&mut self) {
        self.last_discovery = Instant::now();
        let Some(dir) = projects_dir(&self.root) else {
            return;
        };

        if let Some(session) = &self.pinned_session {
            let candidate = dir.join(format!("{session}.jsonl"));
            if candidate.is_file() {
                self.adopt(candidate);
            }
            return;
        }

        let Ok(entries) = std::fs::read_dir(&dir) else {
            // The mangled name did not resolve. Fall back to matching on the
            // `cwd` recorded inside the transcripts themselves.
            if let Some(path) = find_by_cwd(&self.root) {
                self.adopt(path);
            } else {
                self.no_transcript_reason =
                    Some(format!("no transcript directory at {}", dir.display()));
            }
            return;
        };

        let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
                newest = Some((modified, path));
            }
        }

        if let Some((_, path)) = newest {
            if Some(path.as_path()) != self.transcript.as_deref() {
                self.adopt(path);
            }
        } else if self.transcript.is_none() {
            if let Some(path) = find_by_cwd(&self.root) {
                self.adopt(path);
            } else {
                self.no_transcript_reason =
                    Some(format!("no transcript found for {}", self.root.display()));
            }
        }
    }

    /// Follow `path`, starting at its current end. Existing content is history,
    /// not activity, so we never replay it.
    /// Why nothing is being followed, if nothing is.
    #[allow(dead_code)]
    pub fn no_transcript_reason(&self) -> Option<&str> {
        if self.transcript.is_some() {
            None
        } else {
            self.no_transcript_reason.as_deref()
        }
    }

    fn adopt(&mut self, path: PathBuf) {
        self.no_transcript_reason = None;
        self.offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.transcript = Some(path);
    }

    /// Returns whatever Claude has done since the last call. Cheap to call on
    /// every tick; internally rate-limited.
    pub fn poll(&mut self) -> Polled {
        let now = Instant::now();
        if now.duration_since(self.last_poll) < POLL_INTERVAL {
            return Polled::default();
        }
        self.last_poll = now;

        if self.transcript.is_none()
            && now.duration_since(self.last_discovery) >= DISCOVERY_INTERVAL
        {
            self.discover();
        }

        let Some(path) = self.transcript.clone() else {
            return Polled::default();
        };

        let Ok(meta) = std::fs::metadata(&path) else {
            // Transcript vanished. Drop it and look again next time.
            self.transcript = None;
            self.offset = 0;
            return Polled::default();
        };

        let len = meta.len();
        if len < self.offset {
            // Truncated or replaced underneath us. Resync to the new end.
            self.offset = len;
            return Polled::default();
        }
        if len == self.offset {
            // Nothing new. Periodically check whether a newer session started.
            if self.pinned_session.is_none()
                && now.duration_since(self.last_discovery) >= DISCOVERY_INTERVAL
            {
                self.discover();
            }
            return Polled::default();
        }

        let Ok(mut file) = File::open(&path) else {
            return Polled::default();
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Polled::default();
        }

        let mut out = Polled::default();
        let mut consumed = 0u64;
        let reader = BufReader::new(&mut file);

        for line in reader.split(b'\n') {
            let Ok(bytes) = line else { break };
            // split() strips the delimiter, so account for it separately. If
            // the final chunk has no newline it is a partial write; leave it
            // for the next poll rather than parsing half a record.
            let is_complete = self.offset + consumed + (bytes.len() as u64) < len;
            if !is_complete {
                break;
            }
            consumed += bytes.len() as u64 + 1;
            let polled = parse_line(&bytes, &self.root);
            out.files.extend(polled.files);
            out.events.extend(polled.events);
        }

        self.offset += consumed;
        out
    }
}

/// Pull file touches and narratable events out of one transcript line.
///
/// Deliberately tolerant: unknown shapes yield nothing rather than an error.
fn parse_line(bytes: &[u8], root: &Path) -> Polled {
    let mut out = Polled::default();
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return out;
    };
    // `message.content` is an array of blocks for assistant turns, but can
    // be a bare string for user turns. Only the array form carries tool uses.
    let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return out;
    };

    for block in content {
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            continue;
        }
        let Some(name) = block.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let input = block.get("input");
        let field = |key: &str| input.and_then(|i| i.get(key)).and_then(|v| v.as_str());

        if let Some(kind) = ActivityKind::from_tool_name(name) {
            if let Some(raw) = field("file_path") {
                let path = PathBuf::from(raw);
                // Ignore anything outside the tree; we cannot show it.
                if path.starts_with(root) {
                    out.files.push(Activity { path, kind });
                }
            }
            continue;
        }
        match name {
            "Bash" => {
                if let Some(label) = field("description")
                    .map(str::to_string)
                    .or_else(|| field("command").map(truncate_label))
                {
                    out.events.push(Event::Command { label });
                }
            }
            "Grep" | "Glob" => {
                if let Some(pattern) = field("pattern") {
                    out.events.push(Event::Search {
                        pattern: pattern.to_string(),
                    });
                }
            }
            "Agent" => {
                if let Some(label) = field("description") {
                    out.events.push(Event::Agent {
                        label: label.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write is create-or-overwrite; only the tree knows which. A Write to
    /// a path already in the tree reads as an edit to the user.
    #[test]
    fn write_classifies_by_whether_the_file_already_existed() {
        assert_eq!(FileAction::classify(ActivityKind::Write, false), FileAction::Create);
        assert_eq!(FileAction::classify(ActivityKind::Write, true), FileAction::Overwrite);
        assert_eq!(FileAction::classify(ActivityKind::Edit, true), FileAction::Edit);
        assert_eq!(FileAction::classify(ActivityKind::Edit, false), FileAction::Edit);
        assert_eq!(FileAction::classify(ActivityKind::Read, true), FileAction::Read);
    }

    #[test]
    fn mangles_project_path_like_claude_does() {
        // Claude Code 2.1.231, extracted from the binary:
        //   function zmo(e){ return e.replace(/[^a-zA-Z0-9]/g,"-") }
        // The previous version of this test used an all-alphanumeric path, so
        // it passed while the implementation only mapped separators. Every
        // non-alphanumeric character must map.
        assert_eq!(
            mangle_project_path(Path::new("/Users/me/Developer/canopy")),
            "-Users-me-Developer-canopy"
        );
        assert_eq!(
            mangle_project_path(Path::new("/Users/me/Developer/fs-uae-3.2.35")),
            "-Users-me-Developer-fs-uae-3-2-35"
        );
        assert_eq!(
            mangle_project_path(Path::new("/Users/me/_scratch/my proj")),
            "-Users-me--scratch-my-proj"
        );
        assert_eq!(mangle_project_path(Path::new("/tmp/a@b.c")), "-tmp-a-b-c");
    }

    #[test]
    fn config_dir_honours_claude_config_dir() {
        // Claude resolves CLAUDE_CONFIG_DIR ?? ~/.claude. Hardcoding ~/.claude
        // sends discovery to a directory that does not exist for anyone who
        // sets it.
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test, restored immediately after.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", dir.path()) };
        let got = config_dir().unwrap();
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        assert_eq!(got, dir.path());
    }

    #[test]
    fn extracts_edit_from_a_real_transcript_line() {
        let root = Path::new("/Users/duncan/Developer/canopy");
        let line = br#"{"timestamp":"2026-08-13T13:16:45.953Z","sessionId":"abc","cwd":"/Users/duncan/Developer/canopy","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/Users/duncan/Developer/canopy/DECISIONS.md"}}]}}"#;
        let got = parse_line(line, root);
        assert_eq!(got.files.len(), 1);
        assert_eq!(got.files[0].kind, ActivityKind::Edit);
        assert_eq!(
            got.files[0].path,
            PathBuf::from("/Users/duncan/Developer/canopy/DECISIONS.md")
        );
    }

    #[test]
    fn read_edit_and_write_are_distinguished() {
        let root = Path::new("/r");
        let mk = |tool: &str| {
            format!(
                r#"{{"message":{{"content":[{{"type":"tool_use","name":"{tool}","input":{{"file_path":"/r/a.rs"}}}}]}}}}"#
            )
        };
        assert_eq!(parse_line(mk("Read").as_bytes(), root).files[0].kind, ActivityKind::Read);
        assert_eq!(parse_line(mk("Edit").as_bytes(), root).files[0].kind, ActivityKind::Edit);
        assert_eq!(parse_line(mk("MultiEdit").as_bytes(), root).files[0].kind, ActivityKind::Edit);
        assert_eq!(parse_line(mk("Write").as_bytes(), root).files[0].kind, ActivityKind::Write);
    }

    /// Bash carries a human-written description; that is the label. A raw
    /// command is the fallback, truncated so a five-line heredoc cannot
    /// flood the pane.
    #[test]
    fn bash_prefers_its_description_and_falls_back_to_the_command() {
        let root = Path::new("/r");
        let with = br#"{"message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"git fetch origin","description":"Fetch origin and compare"}}]}}"#;
        let got = parse_line(with, root);
        assert_eq!(got.events.len(), 1);
        assert!(matches!(&got.events[0], Event::Command { label } if label == "Fetch origin and compare"));

        let without = br#"{"message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la"}}]}}"#;
        let got = parse_line(without, root);
        assert!(matches!(&got.events[0], Event::Command { label } if label == "ls -la"));

        let long = format!(
            r#"{{"message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"command":"{}"}}}}]}}}}"#,
            "x".repeat(200)
        );
        let got = parse_line(long.as_bytes(), root);
        let Event::Command { label } = &got.events[0] else { panic!("command") };
        assert!(label.chars().count() <= 61, "60 chars plus the ellipsis");
        assert!(label.ends_with('…'));
    }

    #[test]
    fn grep_glob_and_agent_become_events() {
        let root = Path::new("/r");
        let grep = br#"{"message":{"content":[{"type":"tool_use","name":"Grep","input":{"pattern":"scrollbar_thumb"}}]}}"#;
        assert!(matches!(
            &parse_line(grep, root).events[0],
            Event::Search { pattern } if pattern == "scrollbar_thumb"
        ));
        let glob = br#"{"message":{"content":[{"type":"tool_use","name":"Glob","input":{"pattern":"**/*.rs"}}]}}"#;
        assert!(matches!(&parse_line(glob, root).events[0], Event::Search { .. }));
        let agent = br#"{"message":{"content":[{"type":"tool_use","name":"Agent","input":{"description":"Review Task 3","prompt":"..."}}]}}"#;
        assert!(matches!(
            &parse_line(agent, root).events[0],
            Event::Agent { label } if label == "Review Task 3"
        ));
    }

    #[test]
    fn ignores_paths_outside_the_tree() {
        let root = Path::new("/r");
        let line = br#"{"message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/elsewhere/x.rs"}}]}}"#;
        assert!(parse_line(line, root).files.is_empty());
    }

    #[test]
    fn ignores_tools_without_a_file_path() {
        let root = Path::new("/r");
        let line = br#"{"message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
        let got = parse_line(line, root);
        assert!(got.files.is_empty());
        assert_eq!(got.events.len(), 1);
    }

    #[test]
    fn survives_junk_without_panicking() {
        let root = Path::new("/r");
        // The transcript format is undocumented; none of this may crash us.
        let empty = |b: &[u8]| {
            let got = parse_line(b, root);
            got.files.is_empty() && got.events.is_empty()
        };
        assert!(empty(b""));
        assert!(empty(b"not json at all"));
        assert!(empty(b"{}"));
        assert!(empty(br#"{"message":{"content":"a string"}}"#));
        assert!(empty(br#"{"message":{"content":[{"type":"tool_use"}]}}"#));
        assert!(empty(br#"{"message":null}"#));
        assert!(empty(br#"{"message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#));
        assert!(empty(br#"{"message":{"content":[{"type":"tool_use","name":"Grep","input":{}}]}}"#));
    }

    fn write_line(path: &Path, tool: &str, file: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(
            f,
            r#"{{"message":{{"content":[{{"type":"tool_use","name":"{tool}","input":{{"file_path":"{file}"}}}}]}}}}"#
        )
        .unwrap();
    }

    /// The watcher is rate-limited; tests must not be.
    fn poll_now(w: &mut ActivityWatcher) -> Polled {
        w.last_poll = Instant::now() - POLL_INTERVAL - Duration::from_millis(10);
        w.poll()
    }

    #[test]
    fn tails_only_lines_appended_after_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let transcript = root.join("session.jsonl");
        let before = root.join("already-there.rs");
        write_line(&transcript, "Edit", before.to_str().unwrap());

        // Starting mid-file: history is not activity, so this must be ignored.
        let mut w = ActivityWatcher::with_transcript(root, transcript.clone(), false);
        assert!(
            poll_now(&mut w).files.is_empty(),
            "existing lines must not replay"
        );

        let touched = root.join("src/main.rs");
        write_line(&transcript, "Edit", touched.to_str().unwrap());
        let got = poll_now(&mut w);
        assert_eq!(got.files.len(), 1);
        assert_eq!(got.files[0].path, touched);
        assert_eq!(got.files[0].kind, ActivityKind::Edit);

        // Nothing new appended.
        assert!(poll_now(&mut w).files.is_empty());
    }

    #[test]
    fn does_not_parse_a_half_written_line() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let transcript = root.join("session.jsonl");
        std::fs::write(&transcript, b"").unwrap();
        let mut w = ActivityWatcher::with_transcript(root, transcript.clone(), false);

        // A record still being written, with no terminating newline.
        let partial = format!(
            r#"{{"message":{{"content":[{{"type":"tool_use","name":"Edit","input":{{"file_path":"{}/a.rs"#,
            root.display()
        );
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        f.write_all(partial.as_bytes()).unwrap();
        drop(f);

        assert!(
            poll_now(&mut w).files.is_empty(),
            "partial line must not parse"
        );

        // Now complete it.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(f, r#""}}}}]}}}}"#).unwrap();
        drop(f);

        let got = poll_now(&mut w);
        assert_eq!(got.files.len(), 1, "completed line should parse exactly once");
        assert_eq!(got.files[0].path, root.join("a.rs"));
    }

    #[test]
    fn resyncs_when_the_transcript_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let transcript = root.join("session.jsonl");
        write_line(&transcript, "Edit", root.join("a.rs").to_str().unwrap());
        let mut w = ActivityWatcher::with_transcript(root, transcript.clone(), false);

        std::fs::write(&transcript, b"").unwrap(); // truncated underneath us
        assert!(poll_now(&mut w).files.is_empty());

        write_line(&transcript, "Read", root.join("b.rs").to_str().unwrap());
        let got = poll_now(&mut w);
        assert_eq!(got.files.len(), 1);
        assert_eq!(got.files[0].kind, ActivityKind::Read);
    }

    #[test]
    fn finds_several_tool_uses_in_one_line() {
        let root = Path::new("/r");
        let line = br#"{"message":{"content":[
            {"type":"text","text":"working"},
            {"type":"tool_use","name":"Read","input":{"file_path":"/r/a.rs"}},
            {"type":"tool_use","name":"Edit","input":{"file_path":"/r/b.rs"}}]}}"#;
        let got = parse_line(line, root);
        assert_eq!(got.files.len(), 2);
        assert_eq!(got.files[1].kind, ActivityKind::Edit);
    }
}
