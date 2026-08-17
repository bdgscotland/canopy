//! Reads Claude Code's live task list from its on-disk store.
//!
//! The harness persists every TaskCreate/TaskUpdate as one JSON file per
//! task under `~/.claude/tasks/<sessionId>/<taskId>.json`:
//!
//! ```jsonc
//! { "id": "1", "subject": "Fix the parser", "description": "...",
//!   "activeForm": "Fixing the parser", "status": "in_progress",
//!   "blocks": [], "blockedBy": [] }
//! ```
//!
//! Reading the store beats replaying transcript tool calls: it is the
//! harness's own current state, not our reconstruction of it. The format
//! is undocumented, so every step fails soft — an unreadable file or an
//! unknown status skips that task, a missing directory means no tasks, and
//! neither can take the UI down.

use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub active_form: String,
    pub status: TaskStatus,
}

/// How often to re-read the store. The files are small and few, so a full
/// re-read is cheaper than being clever about mtimes — updating a task
/// rewrites its file without touching the directory's own mtime, so a
/// directory stat could not detect changes anyway.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub struct TaskWatcher {
    /// `~/.claude/tasks` (or the CLAUDE_CONFIG_DIR equivalent).
    store: Option<PathBuf>,
    tasks: Vec<Task>,
    last_poll: Instant,
}

impl TaskWatcher {
    pub fn new() -> Self {
        Self::with_store(crate::activity::config_dir().map(|d| d.join("tasks")))
    }

    fn with_store(store: Option<PathBuf>) -> Self {
        Self {
            store,
            tasks: Vec::new(),
            // Far enough back that the first poll reads immediately.
            last_poll: Instant::now() - POLL_INTERVAL - Duration::from_secs(1),
        }
    }

    /// Point at an explicit store directory. Tests only.
    #[cfg(test)]
    pub fn with_dir(dir: PathBuf) -> Self {
        Self::with_store(Some(dir))
    }

    /// The current task list for `session_id`, re-read at most every
    /// POLL_INTERVAL. No session or no directory yields an empty list.
    pub fn poll(&mut self, session_id: Option<&str>) -> &[Task] {
        let now = Instant::now();
        if now.duration_since(self.last_poll) < POLL_INTERVAL {
            return &self.tasks;
        }
        self.last_poll = now;
        self.tasks = self.read(session_id);
        &self.tasks
    }

    fn read(&self, session_id: Option<&str>) -> Vec<Task> {
        let (Some(store), Some(session)) = (self.store.as_ref(), session_id) else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(store.join(session)) else {
            return Vec::new();
        };
        let mut tasks: Vec<Task> = entries
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .filter_map(|e| parse_task(&std::fs::read(e.path()).ok()?))
            .collect();
        // Numeric order, so "10" does not sort before "2".
        tasks.sort_by_key(|t| t.id.parse::<u64>().unwrap_or(u64::MAX));
        tasks
    }
}

/// One task file. Unknown shapes and statuses yield None rather than an
/// error — the format is the harness's, not ours.
fn parse_task(bytes: &[u8]) -> Option<Task> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let s = |key: &str| v.get(key).and_then(|x| x.as_str()).map(str::to_string);
    let status = match v.get("status").and_then(|x| x.as_str())? {
        "pending" => TaskStatus::Pending,
        "in_progress" => TaskStatus::InProgress,
        "completed" => TaskStatus::Completed,
        _ => return None,
    };
    Some(Task {
        id: s("id")?,
        subject: s("subject")?,
        active_form: s("activeForm").unwrap_or_default(),
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task(dir: &std::path::Path, id: &str, subject: &str, status: &str) {
        std::fs::write(
            dir.join(format!("{id}.json")),
            format!(
                r#"{{"id":"{id}","subject":"{subject}","description":"d","activeForm":"doing {subject}","status":"{status}","blocks":[],"blockedBy":[]}}"#
            ),
        )
        .unwrap();
    }

    /// The watcher is rate-limited; tests must not be.
    fn poll_now<'a>(w: &'a mut TaskWatcher, session: Option<&str>) -> &'a [Task] {
        w.last_poll = Instant::now() - POLL_INTERVAL - Duration::from_millis(10);
        w.poll(session)
    }

    #[test]
    fn reads_all_three_statuses_in_numeric_order() {
        let store = tempfile::tempdir().unwrap();
        let session = store.path().join("abc");
        std::fs::create_dir(&session).unwrap();
        write_task(&session, "2", "second", "in_progress");
        write_task(&session, "10", "tenth", "pending");
        write_task(&session, "1", "first", "completed");

        let mut w = TaskWatcher::with_dir(store.path().to_path_buf());
        let got = poll_now(&mut w, Some("abc"));
        assert_eq!(got.len(), 3);
        assert_eq!(
            got.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["1", "2", "10"],
            "numeric order, not lexicographic"
        );
        assert_eq!(got[0].status, TaskStatus::Completed);
        assert_eq!(got[1].status, TaskStatus::InProgress);
        assert_eq!(got[1].active_form, "doing second");
        assert_eq!(got[2].status, TaskStatus::Pending);
    }

    #[test]
    fn junk_files_and_unknown_statuses_are_skipped_not_fatal() {
        let store = tempfile::tempdir().unwrap();
        let session = store.path().join("abc");
        std::fs::create_dir(&session).unwrap();
        write_task(&session, "1", "good", "pending");
        std::fs::write(session.join("2.json"), b"not json").unwrap();
        write_task(&session, "3", "weird", "deleted");
        std::fs::write(session.join("notes.txt"), b"ignored").unwrap();

        let mut w = TaskWatcher::with_dir(store.path().to_path_buf());
        let got = poll_now(&mut w, Some("abc"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].subject, "good");
    }

    #[test]
    fn missing_directory_or_session_means_no_tasks() {
        let store = tempfile::tempdir().unwrap();
        let mut w = TaskWatcher::with_dir(store.path().to_path_buf());
        assert!(poll_now(&mut w, Some("nope")).is_empty());
        assert!(poll_now(&mut w, None).is_empty());
    }

    #[test]
    fn a_status_change_is_picked_up_on_the_next_poll() {
        let store = tempfile::tempdir().unwrap();
        let session = store.path().join("abc");
        std::fs::create_dir(&session).unwrap();
        write_task(&session, "1", "flip", "pending");
        let mut w = TaskWatcher::with_dir(store.path().to_path_buf());
        assert_eq!(poll_now(&mut w, Some("abc"))[0].status, TaskStatus::Pending);

        write_task(&session, "1", "flip", "completed");
        assert_eq!(
            poll_now(&mut w, Some("abc"))[0].status,
            TaskStatus::Completed
        );
    }
}
