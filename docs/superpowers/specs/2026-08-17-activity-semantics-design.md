# Activity semantics: file glyphs, current action, Claude's tasks

**Date:** 2026-08-17
**Status:** Approved

## Problem

Canopy tails the Claude Code session transcript but reduces everything to
two highlight strengths (read / write) on a single file — the last one
touched. The transcript and the harness's on-disk task store carry far
richer semantics that are currently thrown away: what *kind* of touch each
file got, what command or search Claude is running right now, and Claude's
own task list with live statuses.

## Data sources (verified empirically on Claude Code 2.x, 2026-08-17)

1. **Transcript** `~/.claude/projects/<mangled-cwd>/<session>.jsonl` — the
   file the existing `ActivityWatcher` already tails. `tool_use` blocks
   carry, beyond `file_path`:
   - `Bash`: `input.command` plus a human-readable `input.description`
     ("Fetch origin and compare main with origin/main")
   - `Grep` / `Glob`: `input.pattern`
   - `Agent`: `input.description` (short subagent task description)
   - `Edit` / `Write` / `Read` / notebook variants: `input.file_path`
2. **Task store** `~/.claude/tasks/<sessionId>/<taskId>.json` — one JSON
   file per task, rewritten by the harness on every TaskCreate/TaskUpdate:
   `{ id, subject, description, activeForm, status, blocks, blockedBy }`
   with `status ∈ { pending, in_progress, completed }`. The directory name
   is the session id, which `ActivityWatcher::session_id()` already knows
   from the transcript filename.

Both formats are undocumented and may change between Claude Code releases.
Every consumer fails soft: an unparseable record yields nothing, a missing
directory means an empty/hidden surface, and neither can take the UI down.

## Architecture

One extended data layer feeding three surfaces:

- `activity.rs` grows richer parsing (kinds + non-file events).
- A new small `tasks.rs` module reads the task store.
- `App` keeps a decaying per-file activity map and the latest "current
  action" line.
- The right-hand column becomes two panes: the file tree (existing) and,
  beneath it, an activity pane (current action + task list), hidden when
  empty.

## 1. Richer activity parsing (`src/activity.rs`)

`ActivityKind` splits `Write` into `Edit` and `Write`:

- `Read` — `Read`, `NotebookRead`
- `Edit` — `Edit`, `MultiEdit`, `NotebookEdit`
- `Write` — `Write` (creation vs overwrite is decided later, in App,
  by whether the path was already in the tree)

A new enum alongside `Activity` for non-file events:

- `Event::Command { label }` — from `Bash`; `label` prefers
  `input.description`, falling back to a truncated `input.command`
- `Event::Search { pattern }` — from `Grep` / `Glob`
- `Event::Agent { label }` — from `Agent`'s `input.description`

`poll()` returns both streams (a struct or a tuple of Vecs). Unknown tool
names and shapes continue to yield nothing.

## 2. Per-file glyphs with recency (tree pane)

`App` maintains `recent_activity: HashMap<PathBuf, (FileAction, Instant)>`
updated from polled activities, where `FileAction ∈ { Read, Edit, Create,
Overwrite }`. `Create` vs `Overwrite`: a `Write` to a path not currently in
the tree's node list is a creation; otherwise an overwrite.

Rendering, per node in `FileTreeWidget`:

- The **current** file (most recent activity, unchanged concept) keeps the
  existing strong row highlight (loud for writes, quiet for reads).
- Any file touched within the last **60 s** gets a trailing glyph after its
  name: `+` create (green), `✎` edit or overwrite (amber), `·` read (blue)
  — an overwrite of an existing file reads as an edit to the user, so it
  shares the edit glyph; only genuinely new files earn the `+`.
  Full color while the touch is under **10 s** old, dim thereafter, gone at
  60 s. Entries older than 60 s are pruned on poll.
- Glyphs render only when the row has room (they participate in the
  existing width/truncation logic as one more segment; the horizontal
  scroll treats them like any other content).

The tick loop already redraws on activity; expiry alone must also trigger
redraws — reuse the existing redraw-on-change mechanism by treating "an
entry crossed a fade boundary" as a change.

## 3. Activity pane (below the tree)

Layout: the right column splits vertically — tree on top, activity pane
below, the pane sized to its content (1 line for "now" + one per task) and
capped at 40% of the column height; scrollable is NOT needed in v1 (cap +
"…N more" summary line if tasks overflow). The pane (and its border) is
entirely absent when there is nothing to show, restoring today's layout.

Content:

- **Line 1 — now:** the most recent `Event` or file activity, rendered as
  `⚒ <command description>` / `🔍 <pattern>` / `✎ <relative path>` /
  `⧉ <agent description>`. When the last event is older than ~30 s and a
  task is `in_progress`, fall back to that task's `activeForm`. When
  nothing qualifies, blank (and if there are also no tasks, the whole pane
  hides).
- **Tasks:** from `tasks.rs`, sorted by numeric id: `☐ subject` (pending),
  `◐ activeForm` (in_progress), `☑ subject` (completed, dim). Completed
  tasks render dimmest and are dropped first when the height cap bites.

### `src/tasks.rs`

`TaskWatcher` mirrors `ActivityWatcher`'s shape: constructed with the
config dir and a session id (re-checked when the watcher's session id
changes), polls at ~500 ms, and re-reads only when the directory's mtime
set changes (stat pass first). Returns `Vec<Task { id, subject,
active_form, status }>`. Missing directory, unreadable file, unknown
status string → skip that file; never error out.

## Mouse / input

None in v1. The pane is display-only; existing mouse routing is untouched
(the tree's areas shrink but all coordinates already flow through
`app.tree_area`, which the layout sets per-frame).

## Error handling

- Both sources parsed defensively; anything unrecognized is skipped.
- The pane and glyphs degrade to absent; the tree and terminal are never
  blocked or blanked by this feature.
- Time arithmetic uses `Instant` and saturating comparisons; no panics on
  clock oddities.

## Testing

- `activity.rs`: parse tests from real captured lines for Bash
  (description present and absent), Grep, Agent; Edit/Write/Read kind
  mapping; junk survival extended to the new shapes.
- `tasks.rs`: fixture directory with the three statuses + one junk file;
  ordering; missing-directory and empty-directory cases.
- Tree widget: a glyph renders for a recently-touched file, participates
  in truncation/horizontal scroll, and disappears past expiry (inject
  time by passing `Instant`s in, not by sleeping).
- Activity pane widget: renders now-line + three statuses; caps height;
  hides when empty.
- App-level glue (`Create` vs `Overwrite` classification) tested at the
  function level with a synthetic node list.
