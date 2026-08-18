use crate::approvals::approval_lines;
use crate::boxes::{draw_box, split_cols, split_rows};
use crate::events::event_lines;
use crate::model::UiModel;
use crate::transcript::transcript_lines;
use crate::tree::tree_lines;
use crate::wrap::{fixed_width, truncate, wrap_lines};

enum Layout {
    One,
    Two,
    Three,
}

impl Layout {
    fn of(cols: usize) -> Layout {
        if cols < 80 {
            Layout::One
        } else if cols <= 140 {
            Layout::Two
        } else {
            Layout::Three
        }
    }
}

pub fn render(model: &UiModel, cols: usize, rows: usize) -> String {
    let cols = cols.max(1);
    let lines = match Layout::of(cols) {
        Layout::One => render_one(model, cols, rows),
        Layout::Two => render_two(model, cols, rows),
        Layout::Three => render_three(model, cols, rows),
    };
    fit(lines, cols, rows).join("\n")
}

fn render_one(model: &UiModel, cols: usize, rows: usize) -> Vec<String> {
    let bottom = rows.min(2);
    let body_rows = rows - bottom;
    let heights = split_rows(body_rows, &[3, 5, 2, 2]);
    let wrap_width = cols.saturating_sub(4);
    let mut out = Vec::with_capacity(rows);
    out.extend(draw_box(
        "TREE",
        &wrap_lines(&tree_lines(&model.envs), wrap_width),
        cols,
        heights[0],
    ));
    out.extend(draw_box(
        "TRANSCRIPT",
        &wrap_lines(
            &transcript_lines(&model.transcript, &model.expanded),
            wrap_width,
        ),
        cols,
        heights[1],
    ));
    out.extend(draw_box(
        "EVENTS",
        &wrap_lines(&event_lines(&model.events), wrap_width),
        cols,
        heights[2],
    ));
    out.extend(draw_box(
        "APPROVALS",
        &wrap_lines(&approval_lines(&model.approvals), wrap_width),
        cols,
        heights[3],
    ));
    out.extend(bottom_lines(model, cols, bottom));
    out
}

fn render_two(model: &UiModel, cols: usize, rows: usize) -> Vec<String> {
    let bottom = rows.min(2);
    let body_rows = rows - bottom;
    let vsplit = split_rows(body_rows, &[3, 1]);
    let top_area = vsplit[0];
    let events_height = vsplit[1];
    let widths = split_cols(cols, &[1, 2]);
    let left_w = widths[0];
    let right_w = widths[1];

    let tree_box = draw_box(
        "TREE",
        &wrap_lines(&tree_lines(&model.envs), left_w.saturating_sub(4)),
        left_w,
        top_area,
    );
    let right_split = split_rows(top_area, &[3, 1]);
    let transcript_box = draw_box(
        "TRANSCRIPT",
        &wrap_lines(
            &transcript_lines(&model.transcript, &model.expanded),
            right_w.saturating_sub(4),
        ),
        right_w,
        right_split[0],
    );
    let approvals_box = draw_box(
        "APPROVALS",
        &wrap_lines(&approval_lines(&model.approvals), right_w.saturating_sub(4)),
        right_w,
        right_split[1],
    );
    let mut right_lines = transcript_box;
    right_lines.extend(approvals_box);

    let mut out = Vec::with_capacity(rows);
    for i in 0..top_area {
        let mut line = tree_box.get(i).cloned().unwrap_or_default();
        line.push_str(&right_lines.get(i).cloned().unwrap_or_default());
        out.push(line);
    }
    out.extend(draw_box(
        "EVENTS",
        &wrap_lines(&event_lines(&model.events), cols.saturating_sub(4)),
        cols,
        events_height,
    ));
    out.extend(bottom_lines(model, cols, bottom));
    out
}

fn render_three(model: &UiModel, cols: usize, rows: usize) -> Vec<String> {
    let vsplit = split_rows(rows, &[5, 2]);
    let top_rows = vsplit[0];
    let bottom_total = vsplit[1];
    let widths = split_cols(cols, &[3, 4, 3]);
    let (tree_w, transcript_w, events_w) = (widths[0], widths[1], widths[2]);

    let tree_box = draw_box(
        "TREE",
        &wrap_lines(&tree_lines(&model.envs), tree_w.saturating_sub(4)),
        tree_w,
        top_rows,
    );
    let transcript_box = draw_box(
        "TRANSCRIPT",
        &wrap_lines(
            &transcript_lines(&model.transcript, &model.expanded),
            transcript_w.saturating_sub(4),
        ),
        transcript_w,
        top_rows,
    );
    let events_box = draw_box(
        "EVENTS",
        &wrap_lines(&event_lines(&model.events), events_w.saturating_sub(4)),
        events_w,
        top_rows,
    );

    let mut out = Vec::with_capacity(rows);
    for i in 0..top_rows {
        let mut line = tree_box.get(i).cloned().unwrap_or_default();
        line.push_str(&transcript_box.get(i).cloned().unwrap_or_default());
        line.push_str(&events_box.get(i).cloned().unwrap_or_default());
        out.push(line);
    }

    let reserved = bottom_total.min(2);
    let approvals_height = bottom_total - reserved;
    out.extend(draw_box(
        "APPROVALS",
        &wrap_lines(&approval_lines(&model.approvals), cols.saturating_sub(4)),
        cols,
        approvals_height,
    ));
    out.extend(bottom_lines(model, cols, reserved));
    out
}

fn bottom_lines(model: &UiModel, cols: usize, bottom: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(bottom);
    if bottom >= 1 {
        out.push(status_line(model, cols));
    }
    if bottom >= 2 {
        out.push(input_line(model, cols));
    }
    while out.len() < bottom {
        out.push(" ".repeat(cols));
    }
    out.truncate(bottom);
    out
}

fn status_line(model: &UiModel, cols: usize) -> String {
    let mut text = format!(
        "base pid={} attached={}",
        model.status.base_pid, model.status.attached
    );
    if let Some(budgets) = &model.status.budgets {
        if !budgets.is_empty() {
            text.push(' ');
            text.push_str(budgets);
        }
    }
    fixed_width(&text, cols)
}

fn input_line(model: &UiModel, cols: usize) -> String {
    fixed_width(&format!("> {}", model.input_hint), cols)
}

fn fit(mut lines: Vec<String>, cols: usize, rows: usize) -> Vec<String> {
    for line in lines.iter_mut() {
        if line.chars().count() > cols {
            *line = truncate(line, cols);
        }
    }
    lines.truncate(rows);
    while lines.len() < rows {
        lines.push(" ".repeat(cols));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UiModel {
        let mut model = UiModel::new();
        model
            .envs
            .push(crate::model::NodeInfo::new("root", "agent", "up"));
        model.status.base_pid = 1234;
        model.status.attached = 1;
        model.input_hint = "type a task".to_string();
        model
    }

    #[test]
    fn line_count_matches_rows_across_layouts() {
        for (cols, rows) in [(60, 20), (100, 30), (160, 40)] {
            let model = sample();
            let text = render(&model, cols, rows);
            let lines: Vec<&str> = text.split('\n').collect();
            assert_eq!(lines.len(), rows);
            for line in &lines {
                assert!(line.chars().count() <= cols);
                assert!(!line.contains('\t'));
            }
        }
    }
}
