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
