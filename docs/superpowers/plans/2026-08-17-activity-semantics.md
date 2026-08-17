# Activity Semantics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Per-file action glyphs with recency in the tree, a "current action" line, and Claude's live task list in a pane under the tree — all parsed from the session transcript and the harness's on-disk task store.

**Architecture:** `activity.rs` grows richer parsing (Read/Edit/Write kinds plus non-file `Event`s from Bash/Grep/Agent). A new `tasks.rs` reads `~/.claude/tasks/<sessionId>/*.json`. `App` keeps a decaying `path → (FileAction, Instant)` map and a preformatted "now" line. The right column splits into tree + activity pane; the pane hides entirely when empty. Every source is undocumented, so all parsing fails soft — junk yields nothing, never a crash.

**Tech Stack:** Rust, ratatui, serde_json, unicode-width (all already dependencies — add nothing to Cargo.toml).

**Spec:** `docs/superpowers/specs/2026-08-17-activity-semantics-design.md` — read it first.

## Global Constraints

- No new dependencies.
- Commit messages are descriptive sentences in this repo's style (e.g. "Wheel scrolls Claude; click folds directories") — NOT conventional-commit prefixes. End every commit message with a blank line then `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Run `cargo test` (full suite) before every commit; it must pass.
- Comments explain *why*, never narrate the next line. Match the codebase's comment density and voice.
- All parsing fails soft: unparseable JSON, missing fields, unknown statuses, missing directories → skip / show nothing. No panics, no errors surfaced to the UI.
- Glyph/color vocabulary (spec-fixed): `+` create → `Color::Green`; `✎` edit or overwrite → `Color::Rgb(255, 214, 120)` (the existing "write" amber); `·` read → `Color::Rgb(150, 190, 240)` (the existing "read" blue). Bright while the touch is < 10 s old, dim (`Color::DarkGray`) until 60 s, gone after.
- The tick event already marks the frame dirty every tick (src/main.rs:253-259), so fades repaint automatically — do NOT add any new redraw mechanism.
- `src/activity.rs` is compiled only into the bin crate (declared in src/main.rs); `src/tasks.rs` will be too. Neither goes in src/lib.rs.

---

### Task 1: Richer parsing in activity.rs — kinds split, events added

**Files:**
- Modify: `src/activity.rs` (ActivityKind at :27-41, parse_line at :376-415, poll at :302-370, tests), `src/app.rs:277-329` (one line: adapt to the new return type), `src/ui/file_tree_widget.rs` (two match sites gain the `Edit` arm)

**Interfaces:**
- Consumes: nothing new.
- Produces (Tasks 3-6 depend on these exact shapes):
  - `pub enum ActivityKind { Read, Edit, Write }` (Copy, Eq — as today, plus Edit)
  - `pub enum Event { Command { label: String }, Search { pattern: String }, Agent { label: String } }` (Debug, Clone)
  - `#[derive(Default)] pub struct Polled { pub files: Vec<Activity>, pub events: Vec<Event> }`
  - `ActivityWatcher::poll(&mut self) -> Polled`

- [ ] **Step 1: Write the failing tests**

In `src/activity.rs`'s tests module, update the existing tests to the new return shape and add event tests. The changed/new tests (existing tests not listed here keep their bodies, only `parse_line(...)`/`poll` result access changes from `Option<Vec<Activity>>` to `Polled` as shown):

```rust
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
```

Existing tests to mechanically adapt (same assertions, new accessors):
- `ignores_paths_outside_the_tree` and `ignores_tools_without_a_file_path`: `parse_line(...)` no longer returns `Option`; assert `.files.is_empty()` (and for the Bash one, note it now produces an event instead: assert `events.len() == 1`).
- `finds_several_tool_uses_in_one_line`: `let got = parse_line(line, root); assert_eq!(got.files.len(), 2); assert_eq!(got.files[1].kind, ActivityKind::Edit);` (it's an Edit now, not a Write).
- Watcher tests (`tails_only_lines_appended_after_it_started`, `does_not_parse_a_half_written_line`, `resyncs_when_the_transcript_is_truncated`): `poll_now` returns `Polled`; use `.files` everywhere, and where they previously asserted `ActivityKind::Write` for an `Edit` tool line, assert `ActivityKind::Edit`.
- `read_and_write_are_distinguished` is replaced by `read_edit_and_write_are_distinguished` above.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test activity`
Expected: FAIL to compile — `Polled`, `Event`, the `Edit` variant don't exist.

- [ ] **Step 3: Implement**

Replace `ActivityKind` (src/activity.rs:27-41):

```rust
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
```

Add below the `Activity` struct:

```rust
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
```

Replace `parse_line` (keeping its doc comment's spirit):

```rust
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
```

In `poll()` (src/activity.rs:302-370): change the return type to `Polled`, `let mut out = Polled::default();`, replace the per-line accumulation with:

```rust
            let polled = parse_line(&bytes, &self.root);
            out.files.extend(polled.files);
            out.events.extend(polled.events);
```

and every early `return Vec::new();` in the function becomes `return Polled::default();`.

**Call-site fixes so the crate compiles:**

`src/app.rs:299-300` — `poll_activity` currently does `let events = self.activity.poll(); let Some(latest) = events.last()`. Change to:

```rust
        let polled = self.activity.poll();
        let Some(latest) = polled.files.last() else {
            return;
        };
```

(`polled.events` is consumed in Task 3; until then it is simply dropped, which is not a warning.)

`src/ui/file_tree_widget.rs` — the two matches on `ActivityKind` (the `active_bg` match and the `node_style` match) each gain `Edit`, styled identically to `Write` for the row highlight (the glyph, not the row, is what distinguishes them — Task 5):

```rust
            let active_bg = match active {
                Some(ActivityKind::Write) | Some(ActivityKind::Edit) => Some(Color::Rgb(72, 52, 20)),
                Some(ActivityKind::Read) => Some(Color::Rgb(34, 42, 56)),
                None => None,
            };
```

and in the style match: `ActivityKind::Write | ActivityKind::Edit => Style::default().bg(bg).fg(Color::Rgb(255, 214, 120)).bold(),`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test activity`
Expected: PASS. Then `cargo test` (full suite) — the widget tests must still pass with the Edit arm added.

Expected transient: `cargo build` may warn that `Polled`'s `events` field is never read — app.rs consumes it in Task 3. Note it in the report; do not suppress it.

- [ ] **Step 5: Commit**

```bash
git add src/activity.rs src/app.rs src/ui/file_tree_widget.rs
git commit -m "The transcript tells us more: edits split from writes, commands and searches narrated

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: tasks.rs — read Claude's live task store

**Files:**
- Create: `src/tasks.rs`
- Modify: `src/main.rs:1-8` (add `mod tasks;`), `src/activity.rs:73-80` (make `config_dir` `pub(crate)`)

**Interfaces:**
- Consumes: `crate::activity::config_dir()` (make it `pub(crate) fn config_dir() -> Option<PathBuf>` — change `fn` to `pub(crate) fn`, body untouched).
- Produces (Tasks 3, 4, 6 depend on these exact shapes):
  - `pub enum TaskStatus { Pending, InProgress, Completed }` (Copy, Eq)
  - `pub struct Task { pub id: String, pub subject: String, pub active_form: String, pub status: TaskStatus }` (Clone)
  - `TaskWatcher::new() -> Self`
  - `TaskWatcher::poll(&mut self, session_id: Option<&str>) -> &[Task]` — rate-limited internally (~500 ms), returns the cached list between refreshes
  - `#[cfg(test)] TaskWatcher::with_dir(dir: PathBuf) -> Self` and a test-only way to bypass the rate limit

- [ ] **Step 1: Write the failing tests**

Create `src/tasks.rs`:

```rust
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
            .filter(|e| {
                e.path().extension().and_then(|x| x.to_str()) == Some("json")
            })
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
        assert_eq!(poll_now(&mut w, Some("abc"))[0].status, TaskStatus::Completed);
    }
}
```

(Yes, this step writes tests and implementation into one new file — the file is new, so TDD's red step is the compile failure of `mod tasks;` referencing the not-yet-created file. Write the tests FIRST within the file, run to see them fail against `todo!()` bodies if you prefer strict red; the enforced gate is Step 2.)

Add `mod tasks;` to src/main.rs's module list (alphabetical: after `mod scrollbar;`, before `mod terminal;`). Change `fn config_dir` to `pub(crate) fn config_dir` in src/activity.rs.

- [ ] **Step 2: Run the tests**

Run: `cargo test tasks`
Expected: the four tests PASS. If `with_dir` triggers dead_code in non-test builds, its `#[cfg(test)]` gate is missing — fix the gate, don't allow the warning.

Note: `TaskWatcher::new` and `poll` are unconsumed by non-test code until Task 3, so a transient bin-crate dead_code warning on them is expected here — note it in the report, do not suppress it.

- [ ] **Step 3: Full suite and commit**

Run: `cargo test`
Expected: PASS.

```bash
git add src/tasks.rs src/main.rs src/activity.rs
git commit -m "Read Claude's task list from the store the harness itself writes

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: App state — recency map, now-line, task polling

**Files:**
- Modify: `src/activity.rs` (add `FileAction` + `Fade` + classify, with tests), `src/app.rs` (fields, ctor, `tick`, `poll_activity`)

**Interfaces:**
- Consumes: `Polled`/`Event` (Task 1), `TaskWatcher`/`Task` (Task 2).
- Produces (Tasks 5-6 depend on these):
  - In activity.rs: `pub enum FileAction { Read, Edit, Create, Overwrite }` (Copy, Eq), `pub enum Fade { Bright, Dim }` (Copy, Eq), `impl FileAction { pub fn classify(kind: ActivityKind, existed: bool) -> Self }`
  - On App (all `pub`): `recent_activity: HashMap<PathBuf, (FileAction, Instant)>`, `now: Option<(String, Instant)>` (display-ready label, icon included), `tasks: Vec<Task>`
  - Recency constants in activity.rs: `pub const GLYPH_BRIGHT: Duration = Duration::from_secs(10);` `pub const GLYPH_EXPIRY: Duration = Duration::from_secs(60);` `pub const NOW_STALE: Duration = Duration::from_secs(30);`

- [ ] **Step 1: Write the failing classify tests**

In src/activity.rs's tests module:

```rust
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test activity`
Expected: FAIL to compile — `FileAction` doesn't exist.

- [ ] **Step 3: Implement in activity.rs**

Below `ActivityKind`:

```rust
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
```

- [ ] **Step 4: Wire App state**

In src/app.rs — imports: extend the activity import to `use crate::activity::{ActivityKind, ActivityWatcher, Event, FileAction, GLYPH_EXPIRY};` and add `use crate::tasks::{Task, TaskWatcher};` plus `use std::collections::HashMap;`.

Fields on `App` (near `highlight`):

```rust
    /// Files Claude touched recently and how, pruned past GLYPH_EXPIRY.
    /// Feeds the per-file glyphs; `highlight` stays the single loud row.
    pub recent_activity: HashMap<PathBuf, (FileAction, Instant)>,
    /// The latest narratable action, display-ready ("⚒ Run the tests").
    pub now: Option<(String, Instant)>,
    pub tasks: Vec<Task>,
    task_watcher: TaskWatcher,
```

Constructor init: `recent_activity: HashMap::new(), now: None, tasks: Vec::new(), task_watcher: TaskWatcher::new(),`.

In `tick()`, after `self.poll_activity();`:

```rust
        self.tasks = self
            .task_watcher
            .poll(self.activity.session_id().as_deref())
            .to_vec();
```

In `poll_activity()`, replace the block from `let polled = self.activity.poll();` down to the `self.highlight = ...` line (keep everything about reveal untouched below it):

```rust
        let polled = self.activity.poll();

        // Narrate the newest event. A file touch also narrates, but a
        // command/search/agent event is more specific, so it wins the tie
        // within one poll.
        let now = Instant::now();
        if let Some(event) = polled.events.last() {
            let label = match event {
                Event::Command { label } => format!("⚒ {label}"),
                Event::Search { pattern } => format!("🔍 {pattern}"),
                Event::Agent { label } => format!("⧉ {label}"),
            };
            self.now = Some((label, now));
        } else if let Some(f) = polled.files.last() {
            let shown = f
                .path
                .strip_prefix(self.tree.root_path())
                .unwrap_or(&f.path);
            self.now = Some((format!("✎ {}", shown.display()), now));
        }

        // Classify BEFORE reveal_ancestors/request_refresh can add the file
        // to the tree: existence-now is what separates create from overwrite.
        for f in &polled.files {
            let existed = self.tree.nodes().iter().any(|n| n.path == f.path);
            self.recent_activity
                .insert(f.path.clone(), (FileAction::classify(f.kind, existed), now));
        }
        self.recent_activity
            .retain(|_, (_, t)| t.elapsed() < GLYPH_EXPIRY);

        let Some(latest) = polled.files.last() else {
            return;
        };
        self.highlight = Some((latest.path.clone(), latest.kind));
```

(The `✎` in the now-line is generic "touching this file" narration; per-file glyph specificity lives in the tree. Keep it simple here.)

- [ ] **Step 5: Full suite and commit**

Run: `cargo test`
Expected: PASS. `cargo build 2>&1 | grep -ci warn` — the only acceptable transient warnings are on `App::recent_activity`/`now`/`tasks` **if** rustc flags pub fields (it does not for pub struct fields read nowhere; if any warning does appear, note it — Tasks 5-6 consume them).

```bash
git add src/activity.rs src/app.rs
git commit -m "App remembers what happened to each file, what Claude is doing now, and its task list

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: The activity pane widget

**Files:**
- Create: `src/ui/activity_pane.rs`
- Modify: `src/ui/mod.rs:1-2` (add `mod activity_pane;` — wiring into `draw` is Task 6)

**Interfaces:**
- Consumes: `Task`/`TaskStatus` (Task 2).
- Produces (Task 6 depends on these):
  - `ActivityPaneWidget::new(now: Option<&'a str>, tasks: &'a [Task]) -> Self` implementing `Widget`
  - `pub fn content_height(now: bool, task_count: usize, cap: u16) -> u16` — rows the pane wants: `now as u16 + task_count as u16 + 1` overflow line if capped, all `min(cap)`; 0 when `!now && task_count == 0`

- [ ] **Step 1: Write the failing tests**

Create `src/ui/activity_pane.rs` with tests first (stub the widget with `todo!()` in render if you want strict red; the gate is Step 2):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{Task, TaskStatus};
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

    fn task(id: &str, subject: &str, status: TaskStatus) -> Task {
        Task {
            id: id.into(),
            subject: subject.into(),
            active_form: format!("doing {subject}"),
            status,
        }
    }

    fn render(now: Option<&str>, tasks: &[Task], width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        ActivityPaneWidget::new(now, tasks).render(area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn renders_now_line_then_tasks_with_status_glyphs() {
        let tasks = [
            task("1", "first", TaskStatus::Completed),
            task("2", "second", TaskStatus::InProgress),
            task("3", "third", TaskStatus::Pending),
        ];
        let rows = render(Some("⚒ Run the tests"), &tasks, 30, 4);
        assert!(rows[0].contains("Run the tests"));
        assert!(rows[1].contains("☑ first"));
        assert!(
            rows[2].contains("◐ doing second"),
            "in_progress shows activeForm: {:?}",
            rows[2]
        );
        assert!(rows[3].contains("☐ third"));
    }

    #[test]
    fn without_a_now_line_tasks_start_at_the_top() {
        let tasks = [task("1", "only", TaskStatus::Pending)];
        let rows = render(None, &tasks, 20, 2);
        assert!(rows[0].contains("☐ only"));
    }

    /// The area is the cap: when tasks overflow it, completed ones are
    /// dropped first and the last row summarizes what is hidden.
    #[test]
    fn overflow_drops_completed_first_and_summarizes() {
        let tasks = [
            task("1", "done-a", TaskStatus::Completed),
            task("2", "done-b", TaskStatus::Completed),
            task("3", "live", TaskStatus::InProgress),
            task("4", "next", TaskStatus::Pending),
        ];
        // Room for now + 2 task rows. One row goes to the summary, so a
        // single task survives -- and it must be a live one, not the
        // completed pair that precedes it in id order.
        let rows = render(Some("now"), &tasks, 24, 3);
        assert!(rows[1].contains("◐ doing live"), "{:?}", rows[1]);
        assert!(rows[2].contains("… 3 more"), "hidden count: {:?}", rows[2]);
    }

    #[test]
    fn content_height_is_zero_only_when_truly_empty() {
        assert_eq!(content_height(false, 0, 10), 0);
        assert_eq!(content_height(true, 0, 10), 1);
        assert_eq!(content_height(false, 3, 10), 3);
        assert_eq!(content_height(true, 3, 10), 4);
        assert_eq!(content_height(true, 20, 8), 8, "capped");
    }

    #[test]
    fn long_rows_truncate_inside_the_area() {
        let tasks = [task("1", &"x".repeat(100), TaskStatus::Pending)];
        let rows = render(None, &tasks, 10, 1);
        assert_eq!(rows[0].chars().count(), 10, "must clip, not overflow");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test activity_pane`
Expected: FAIL to compile — the widget doesn't exist.

- [ ] **Step 3: Implement**

Above the tests in the same file:

```rust
use ratatui::{prelude::*, widgets::Widget};

use crate::tasks::{Task, TaskStatus};

/// The pane under the tree: what Claude is doing now, then its task list.
/// Display-only; the caller sizes the area (and thereby caps the rows).
pub struct ActivityPaneWidget<'a> {
    now: Option<&'a str>,
    tasks: &'a [Task],
}

/// Rows the pane wants for this content, capped. Zero means "hide the
/// pane entirely" — an empty bordered box would just be noise.
pub fn content_height(now: bool, task_count: usize, cap: u16) -> u16 {
    let rows = now as usize + task_count;
    if rows == 0 {
        return 0;
    }
    (rows as u16).min(cap)
}

impl<'a> ActivityPaneWidget<'a> {
    pub fn new(now: Option<&'a str>, tasks: &'a [Task]) -> Self {
        Self { now, tasks }
    }
}

impl<'a> Widget for ActivityPaneWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut y = area.y;
        let bottom = area.y + area.height;
        let width = area.width as usize;

        if let Some(now) = self.now {
            buf.set_stringn(area.x, y, now, width, Style::default().fg(Color::White).bold());
            y += 1;
        }

        // When the area cannot hold every task, completed ones go first --
        // they are the least interesting -- and the final row says how many
        // rows are hidden.
        let room = (bottom - y) as usize;
        let mut shown: Vec<&Task> = self.tasks.iter().collect();
        if shown.len() > room {
            let keep = room.saturating_sub(1);
            let mut live: Vec<&Task> = shown
                .iter()
                .copied()
                .filter(|t| t.status != TaskStatus::Completed)
                .collect();
            let mut done: Vec<&Task> = shown
                .iter()
                .copied()
                .filter(|t| t.status == TaskStatus::Completed)
                .collect();
            live.truncate(keep);
            let slots_left = keep.saturating_sub(live.len());
            done.truncate(slots_left);
            let hidden = self.tasks.len() - live.len() - done.len();
            shown = self
                .tasks
                .iter()
                .filter(|t| {
                    live.iter().any(|l| l.id == t.id) || done.iter().any(|d| d.id == t.id)
                })
                .collect();
            for t in &shown {
                if y >= bottom {
                    break;
                }
                render_task(buf, area.x, y, width, t);
                y += 1;
            }
            if y < bottom {
                buf.set_stringn(
                    area.x,
                    y,
                    format!("… {hidden} more"),
                    width,
                    Style::default().fg(Color::DarkGray),
                );
            }
            return;
        }
        for t in shown {
            if y >= bottom {
                break;
            }
            render_task(buf, area.x, y, width, t);
            y += 1;
        }
    }
}

fn render_task(buf: &mut Buffer, x: u16, y: u16, width: usize, t: &Task) {
    let (glyph, text, style) = match t.status {
        TaskStatus::Pending => ("☐", t.subject.as_str(), Style::default().fg(Color::Gray)),
        TaskStatus::InProgress => (
            "◐",
            t.active_form.as_str(),
            Style::default().fg(Color::Rgb(255, 214, 120)).bold(),
        ),
        TaskStatus::Completed => ("☑", t.subject.as_str(), Style::default().fg(Color::DarkGray)),
    };
    buf.set_stringn(x, y, format!("{glyph} {text}"), width, style);
}
```

Add `mod activity_pane;` to src/ui/mod.rs's module list.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test activity_pane`
Expected: PASS (6 tests). Expected transient: dead_code on `ActivityPaneWidget`/`content_height` until Task 6 wires them — note in the report.

- [ ] **Step 5: Full suite and commit**

Run: `cargo test`

```bash
git add src/ui/activity_pane.rs src/ui/mod.rs
git commit -m "A pane that narrates: the current action and Claude's own task list

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Per-file glyphs in the tree widget

**Files:**
- Modify: `src/ui/file_tree_widget.rs` (builder, render segments, tests)

**Interfaces:**
- Consumes: `FileAction`, `Fade` (Task 3).
- Produces (Task 6 calls it): `FileTreeWidget::recent(self, recent: &'a HashMap<PathBuf, (FileAction, Fade)>) -> Self` — builder like `.highlight(...)`; default is an empty map (store `Option<&'a HashMap<...>>`, treat `None` as empty).

- [ ] **Step 1: Write the failing tests**

In a new `glyph_tests` module in src/ui/file_tree_widget.rs (reuse the file's existing tempdir/render pattern):

```rust
#[cfg(test)]
mod glyph_tests {
    use super::*;
    use crate::activity::{Fade, FileAction};
    use crate::tree::FileTree;
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn render_with(
        recent: &HashMap<PathBuf, (FileAction, Fade)>,
        root_file: &str,
        width: u16,
    ) -> Vec<String> {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(root_file), "x").unwrap();
        let mut tree = FileTree::new(d.path(), false, 10).unwrap();
        tree.set_nodes(crate::tree::walk(d.path(), false, 10, tree.collapsed_set()));

        // The tempdir path varies per run; key the map on the real path.
        let mut keyed = HashMap::new();
        for (_, v) in recent {
            keyed.insert(d.path().join(root_file), *v);
        }

        let area = Rect::new(0, 0, width, 5);
        let mut buf = Buffer::empty(area);
        let mut state = FileTreeWidgetState { offset: 0 };
        FileTreeWidget::new(&tree, None)
            .recent(&keyed)
            .render(area, &mut buf, &mut state);
        (0..5)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_recently_edited_file_gets_its_glyph() {
        let mut recent = HashMap::new();
        recent.insert(PathBuf::new(), (FileAction::Edit, Fade::Bright));
        let rows = render_with(&recent, "touched.rs", 30);
        let row = rows.iter().find(|r| r.contains("touched.rs")).unwrap();
        assert!(row.contains("touched.rs ✎"), "glyph after the name: {row:?}");
    }

    #[test]
    fn create_and_read_have_their_own_glyphs() {
        for (action, glyph) in [(FileAction::Create, "+"), (FileAction::Read, "·")] {
            let mut recent = HashMap::new();
            recent.insert(PathBuf::new(), (action, Fade::Bright));
            let rows = render_with(&recent, "f.rs", 30);
            let row = rows.iter().find(|r| r.contains("f.rs")).unwrap();
            assert!(
                row.contains(&format!("f.rs {glyph}")),
                "{action:?} should show {glyph}: {row:?}"
            );
        }
    }

    #[test]
    fn untouched_files_get_no_glyph() {
        let recent = HashMap::new();
        let rows = render_with(&recent, "quiet.rs", 30);
        let row = rows.iter().find(|r| r.contains("quiet.rs")).unwrap();
        assert!(!row.contains('✎') && !row.contains('+'), "{row:?}");
    }

    /// The glyph is one more segment: it must clip at the pane edge like
    /// everything else, not paint past it.
    #[test]
    fn glyphs_respect_the_pane_edge() {
        let width = 10u16;
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a_much_longer_name.rs"), "x").unwrap();
        let mut tree = FileTree::new(d.path(), false, 10).unwrap();
        tree.set_nodes(crate::tree::walk(d.path(), false, 10, tree.collapsed_set()));
        let mut keyed = HashMap::new();
        keyed.insert(
            d.path().join("a_much_longer_name.rs"),
            (FileAction::Edit, Fade::Bright),
        );
        let area = Rect::new(0, 0, width, 5);
        let mut buf = Buffer::empty(Rect::new(0, 0, width + 4, 5));
        let mut state = FileTreeWidgetState { offset: 0 };
        FileTreeWidget::new(&tree, None)
            .recent(&keyed)
            .render(area, &mut buf, &mut state);
        for y in 0..5 {
            for x in width..width + 4 {
                assert_eq!(buf.cell((x, y)).map_or(" ", |c| c.symbol()), " ");
            }
        }
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test glyph`
Expected: FAIL to compile — `.recent(...)` doesn't exist.

- [ ] **Step 3: Implement**

In src/ui/file_tree_widget.rs — imports: `use crate::activity::{ActivityKind, Fade, FileAction};` and `use std::collections::HashMap;`.

Struct + builder (next to `highlight`):

```rust
    /// Files touched recently and how, for the trailing glyphs. Fade is
    /// precomputed by the caller so this widget stays clock-free.
    recent: Option<&'a HashMap<PathBuf, (FileAction, Fade)>>,
```

(`use std::path::PathBuf;` — the file currently imports `Path` only; extend it.) Initialize `recent: None` in `new`, and add:

```rust
    pub fn recent(mut self, recent: &'a HashMap<PathBuf, (FileAction, Fade)>) -> Self {
        self.recent = Some(recent);
        self
    }
```

In the render loop, after the `segments.push((display, node_style));` line, append the glyph segment:

```rust
            // Trailing action glyph for recently-touched files. One more
            // segment, so shifting and truncation treat it like content.
            if let Some((action, fade)) = self.recent.and_then(|m| m.get(&node.path)) {
                let glyph = match action {
                    FileAction::Create => "+",
                    FileAction::Edit | FileAction::Overwrite => "✎",
                    FileAction::Read => "·",
                };
                let color = match (fade, action) {
                    (Fade::Dim, _) => Color::DarkGray,
                    (Fade::Bright, FileAction::Create) => Color::Green,
                    (Fade::Bright, FileAction::Edit | FileAction::Overwrite) => {
                        Color::Rgb(255, 214, 120)
                    }
                    (Fade::Bright, FileAction::Read) => Color::Rgb(150, 190, 240),
                };
                segments.push((format!(" {glyph}"), Style::default().fg(color)));
            }
```

(Known, accepted: `FileTree::content_width` does not count the 2 glyph columns, so at full horizontal scroll a glyphed row may show the `…` marker — same accepted transience as the CWD marker, per the scrollbars spec.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test` (full suite — the pre-existing width/render tests must still pass; they use no recent map, so nothing changes for them).

- [ ] **Step 5: Commit**

```bash
git add src/ui/file_tree_widget.rs
git commit -m "The tree says what happened to each file, and how recently

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Layout split and wiring in ui::draw

**Files:**
- Modify: `src/ui/mod.rs` (the tree-pane section, :90-163)

**Interfaces:**
- Consumes: `ActivityPaneWidget`/`content_height` (Task 4), `.recent(...)` (Task 5), `App::{recent_activity, now, tasks}` (Task 3), `Fade`/`GLYPH_BRIGHT`/`GLYPH_EXPIRY`/`NOW_STALE` (Task 3), `TaskStatus` (Task 2).
- Produces: nothing for later tasks. App cannot be unit-tested (live PTY), so this task's verification is the full suite + build + a manual run.

- [ ] **Step 1: Implement the split**

In src/ui/mod.rs — imports: `use activity_pane::{content_height, ActivityPaneWidget};`, `use crate::activity::{Fade, GLYPH_BRIGHT, GLYPH_EXPIRY, NOW_STALE};`, `use crate::tasks::TaskStatus;`, `use std::collections::HashMap;`.

Replace `let tree_area = chunks[1];` with a vertical split driven by content:

```rust
    // Right column: tree above, activity pane below. The pane exists only
    // when it has something to say -- an empty bordered box under the tree
    // would be pure noise -- so an idle session looks exactly like today.
    let right = chunks[1];

    let now_line: Option<String> = app
        .now
        .as_ref()
        .filter(|(_, t)| t.elapsed() < NOW_STALE)
        .map(|(label, _)| label.clone())
        .or_else(|| {
            // Tools have gone quiet; the in-progress task is the best
            // description of what Claude is doing.
            app.tasks
                .iter()
                .find(|t| t.status == TaskStatus::InProgress)
                .map(|t| format!("◐ {}", t.active_form))
        });

    let cap = (right.height * 2) / 5; // spec: at most 40% of the column
    let pane_rows = content_height(now_line.is_some(), app.tasks.len(), cap);
    let (tree_area, pane_area) = if pane_rows == 0 {
        (right, None)
    } else {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(pane_rows + 2)])
            .split(right);
        (split[0], Some(split[1]))
    };
```

(The `+ 2` is the pane's own border. The existing `tree_block` code continues unchanged against this `tree_area`.)

After the tree render (the end of the `else` branch that renders `file_tree_widget`), add the pane render at the same nesting level as the tree block (outside that if/else — the pane shows during "Scanning files..." too):

```rust
    if let Some(pane_area) = pane_area {
        let pane_block = Block::default()
            .title(" claude ")
            .title_style(Style::default().fg(Color::Cyan))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let pane_inner = pane_block.inner(pane_area);
        frame.render_widget(pane_block, pane_area);
        frame.render_widget(
            ActivityPaneWidget::new(now_line.as_deref(), &app.tasks),
            pane_inner,
        );
    }
```

- [ ] **Step 2: Feed the recent map to the tree widget**

Still in the tree-render branch, before building `file_tree_widget`:

```rust
        // Fade is computed here, once per frame, so the widget itself
        // never consults a clock and stays deterministic under test.
        let recent: HashMap<std::path::PathBuf, (crate::activity::FileAction, Fade)> = app
            .recent_activity
            .iter()
            .filter(|(_, (_, t))| t.elapsed() < GLYPH_EXPIRY)
            .map(|(p, (action, t))| {
                let fade = if t.elapsed() < GLYPH_BRIGHT {
                    Fade::Bright
                } else {
                    Fade::Dim
                };
                (p.clone(), (*action, fade))
            })
            .collect();
```

and chain `.recent(&recent)` onto the `FileTreeWidget::new(...)` builder next to `.highlight(...)`.

- [ ] **Step 3: Verify**

Run: `cargo test` — full suite must pass (no ui tests exist for draw; the suite catches compile and regression). `cargo build 2>&1 | grep -ci warn` must print 0 — every transient dead_code from Tasks 2-5 is consumed now.

Manual (controller/human): `cargo run` in a real project with a Claude session; confirm — the pane is absent when idle with no tasks; it appears with a now-line when Claude runs a Bash command (showing the description); tasks appear/flip states as Claude works (`◐` shows activeForm); glyphs appear next to touched files, fade to gray after ~10 s, vanish after ~60 s; the tree's scrollbars still work with the shorter tree pane; mouse folding still lands on the right rows.

- [ ] **Step 4: Commit**

```bash
git add src/ui/mod.rs
git commit -m "The right column narrates: tree on top, current action and tasks below

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```
