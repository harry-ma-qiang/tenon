use crate::wrap::fixed_width;

pub fn draw_box(title: &str, body: &[String], width: usize, height: usize) -> Vec<String> {
    let width = width.max(1);
    if height == 0 {
        return Vec::new();
    }
    if height == 1 {
        return vec![border_line(width)];
    }
    let mut out = Vec::with_capacity(height);
    out.push(border_line(width));
    let content_rows = height - 2;
    if content_rows > 0 {
        out.push(content_line(&format!("[{title}]"), width));
        for i in 1..content_rows {
            let text = body.get(i - 1).cloned().unwrap_or_default();
            out.push(content_line(&text, width));
        }
    }
    out.push(border_line(width));
    out.truncate(height);
    while out.len() < height {
        out.push(" ".repeat(width));
    }
    out
}

fn border_line(width: usize) -> String {
    if width < 2 {
        return "+".repeat(width);
    }
    format!("+{}+", "-".repeat(width - 2))
}

fn content_line(text: &str, width: usize) -> String {
    if width < 2 {
        return fixed_width(text, width);
    }
    let inner = width - 2;
    let padded = format!(" {text}");
    format!("|{}|", fixed_width(&padded, inner))
}

pub fn split_rows(total: usize, weights: &[u32]) -> Vec<usize> {
    if weights.is_empty() {
        return Vec::new();
    }
    let sum: u64 = weights.iter().map(|w| *w as u64).sum();
    if sum == 0 || total == 0 {
        return vec![0; weights.len()];
    }
    let mut out: Vec<usize> = weights
        .iter()
        .map(|w| ((*w as u64 * total as u64) / sum) as usize)
        .collect();
    let used: usize = out.iter().sum();
    if used < total {
        let mut remaining = total - used;
        let mut i = out.len();
        while remaining > 0 {
            if i == 0 {
                i = out.len();
            }
            i -= 1;
            out[i] += 1;
            remaining -= 1;
        }
    } else if used > total {
        let mut remaining = used - total;
        let mut i = 0;
        while remaining > 0 {
            if out[i] > 0 {
                out[i] -= 1;
                remaining -= 1;
            }
            i = (i + 1) % out.len();
        }
    }
    out
}

pub fn split_cols(total: usize, weights: &[u32]) -> Vec<usize> {
    split_rows(total, weights)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_rows_sums_to_total() {
        for total in 0..40 {
            let out = split_rows(total, &[3, 5, 2, 2]);
            assert_eq!(out.iter().sum::<usize>(), total);
        }
    }

    #[test]
    fn draw_box_matches_height_and_width() {
        let body = vec!["one".to_string(), "two".to_string()];
        for height in 0..8 {
            for width in 1..30 {
                let out = draw_box("T", &body, width, height);
                assert_eq!(out.len(), height);
                for line in &out {
                    assert!(line.chars().count() <= width);
                    assert!(!line.contains('\t'));
                }
            }
        }
    }
}
