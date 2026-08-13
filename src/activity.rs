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

/// What Claude did to a file. Drives how strongly it is highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Read,
    Write,
}

impl ActivityKind {
    fn from_tool_name(name: &str) -> Option<Self> {
        match name {
            "Read" | "NotebookRead" => Some(Self::Read),
            "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => Some(Self::Write),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Activity {
    pub path: PathBuf,
    pub kind: ActivityKind,
}

/// Claude Code mangles a project path into a directory name by replacing every
/// path separator with `-`. `/Users/me/proj` becomes `-Users-me-proj`.
fn mangle_project_path(root: &Path) -> String {
    let s = root.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        out.push(if ch == '/' || ch == '\\' { '-' } else { ch });
    }
    out
}

fn projects_dir(root: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".claude/projects").join(mangle_project_path(root)))
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
        }
    }

    /// Follow `path`, starting at its current end. Existing content is history,
    /// not activity, so we never replay it.
    fn adopt(&mut self, path: PathBuf) {
        self.offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.transcript = Some(path);
    }

    /// Returns whatever Claude has done since the last call. Cheap to call on
    /// every tick; internally rate-limited.
    pub fn poll(&mut self) -> Vec<Activity> {
        let now = Instant::now();
        if now.duration_since(self.last_poll) < POLL_INTERVAL {
            return Vec::new();
        }
        self.last_poll = now;

        if self.transcript.is_none()
            && now.duration_since(self.last_discovery) >= DISCOVERY_INTERVAL
        {
            self.discover();
        }

        let Some(path) = self.transcript.clone() else {
            return Vec::new();
        };

        let Ok(meta) = std::fs::metadata(&path) else {
            // Transcript vanished. Drop it and look again next time.
            self.transcript = None;
            self.offset = 0;
            return Vec::new();
        };

        let len = meta.len();
        if len < self.offset {
            // Truncated or replaced underneath us. Resync to the new end.
            self.offset = len;
            return Vec::new();
        }
        if len == self.offset {
            // Nothing new. Periodically check whether a newer session started.
            if self.pinned_session.is_none()
                && now.duration_since(self.last_discovery) >= DISCOVERY_INTERVAL
            {
                self.discover();
            }
            return Vec::new();
        }

        let Ok(mut file) = File::open(&path) else {
            return Vec::new();
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut consumed = 0u64;
        let reader = BufReader::new(&mut file);

        for line in reader.split(b'\n') {
            let Ok(bytes) = line else { break };
            // split() strips the delimiter, so account for it separately. If
            // the final chunk has no newline it is a partial write; leave it
            // for the next poll rather than parsing half a record.
            let is_complete = self.offset + consumed + bytes.len() as u64 + 1 <= len;
            if !is_complete {
                break;
            }
            consumed += bytes.len() as u64 + 1;
            if let Some(activity) = parse_line(&bytes, &self.root) {
                out.extend(activity);
            }
        }

        self.offset += consumed;
        out
    }
}

/// Pull any file-touching tool uses out of one transcript line.
///
/// Deliberately tolerant: unknown shapes yield nothing rather than an error.
fn parse_line(bytes: &[u8], root: &Path) -> Option<Vec<Activity>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;

    // `message.content` is an array of blocks for assistant turns, but can be a
    // bare string for user turns. Only the array form carries tool uses.
    let content = value.get("message")?.get("content")?.as_array()?;

    let mut out = Vec::new();
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            continue;
        }
        let Some(name) = block.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let Some(kind) = ActivityKind::from_tool_name(name) else {
            continue;
        };
        let Some(raw_path) = block
            .get("input")
            .and_then(|i| i.get("file_path"))
            .and_then(|p| p.as_str())
        else {
            continue;
        };

        let path = PathBuf::from(raw_path);
        // Ignore anything outside the tree; we cannot show it.
        if !path.starts_with(root) {
            continue;
        }
        out.push(Activity { path, kind });
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mangles_project_path_like_claude_does() {
        assert_eq!(
            mangle_project_path(Path::new("/Users/me/Developer/canopy")),
            "-Users-me-Developer-canopy"
        );
    }

    #[test]
    fn extracts_edit_from_a_real_transcript_line() {
        let root = Path::new("/Users/duncan/Developer/canopy");
        let line = br#"{"timestamp":"2026-08-13T13:16:45.953Z","sessionId":"abc","cwd":"/Users/duncan/Developer/canopy","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/Users/duncan/Developer/canopy/DECISIONS.md"}}]}}"#;
        let got = parse_line(line, root).expect("should find one activity");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, ActivityKind::Write);
        assert_eq!(
            got[0].path,
            PathBuf::from("/Users/duncan/Developer/canopy/DECISIONS.md")
        );
    }

    #[test]
    fn read_and_write_are_distinguished() {
        let root = Path::new("/r");
        let read = br#"{"message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/r/a.rs"}}]}}"#;
        let write = br#"{"message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"/r/b.rs"}}]}}"#;
        assert_eq!(parse_line(read, root).unwrap()[0].kind, ActivityKind::Read);
        assert_eq!(parse_line(write, root).unwrap()[0].kind, ActivityKind::Write);
    }

    #[test]
    fn ignores_paths_outside_the_tree() {
        let root = Path::new("/r");
        let line = br#"{"message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/elsewhere/x.rs"}}]}}"#;
        assert!(parse_line(line, root).is_none());
    }

    #[test]
    fn ignores_tools_without_a_file_path() {
        let root = Path::new("/r");
        let line = br#"{"message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
        assert!(parse_line(line, root).is_none());
    }

    #[test]
    fn survives_junk_without_panicking() {
        let root = Path::new("/r");
        // The transcript format is undocumented; none of this may crash us.
        assert!(parse_line(b"", root).is_none());
        assert!(parse_line(b"not json at all", root).is_none());
        assert!(parse_line(b"{}", root).is_none());
        assert!(parse_line(br#"{"message":{"content":"a string"}}"#, root).is_none());
        assert!(parse_line(br#"{"message":{"content":[{"type":"tool_use"}]}}"#, root).is_none());
        assert!(parse_line(br#"{"message":null}"#, root).is_none());
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
    fn poll_now(w: &mut ActivityWatcher) -> Vec<Activity> {
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
        assert!(poll_now(&mut w).is_empty(), "existing lines must not replay");

        let touched = root.join("src/main.rs");
        write_line(&transcript, "Edit", touched.to_str().unwrap());
        let got = poll_now(&mut w);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, touched);
        assert_eq!(got[0].kind, ActivityKind::Write);

        // Nothing new appended.
        assert!(poll_now(&mut w).is_empty());
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

        assert!(poll_now(&mut w).is_empty(), "partial line must not parse");

        // Now complete it.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(f, r#""}}}}]}}}}"#).unwrap();
        drop(f);

        let got = poll_now(&mut w);
        assert_eq!(got.len(), 1, "completed line should parse exactly once");
        assert_eq!(got[0].path, root.join("a.rs"));
    }

    #[test]
    fn resyncs_when_the_transcript_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let transcript = root.join("session.jsonl");
        write_line(&transcript, "Edit", root.join("a.rs").to_str().unwrap());
        let mut w = ActivityWatcher::with_transcript(root, transcript.clone(), false);

        std::fs::write(&transcript, b"").unwrap(); // truncated underneath us
        assert!(poll_now(&mut w).is_empty());

        write_line(&transcript, "Read", root.join("b.rs").to_str().unwrap());
        let got = poll_now(&mut w);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, ActivityKind::Read);
    }

    #[test]
    fn finds_several_tool_uses_in_one_line() {
        let root = Path::new("/r");
        let line = br#"{"message":{"content":[
            {"type":"text","text":"working"},
            {"type":"tool_use","name":"Read","input":{"file_path":"/r/a.rs"}},
            {"type":"tool_use","name":"Edit","input":{"file_path":"/r/b.rs"}}]}}"#;
        let got = parse_line(line, root).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].path, PathBuf::from("/r/b.rs"));
    }
}
