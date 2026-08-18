pub fn no_tabs(text: &str) -> String {
    text.replace('\t', "    ")
}

pub fn fixed_width(text: &str, width: usize) -> String {
    let clean = no_tabs(text);
    let mut out: String = clean.chars().take(width).collect();
    let len = out.chars().count();
    if len < width {
        out.push_str(&" ".repeat(width - len));
    }
    out
}

pub fn truncate(text: &str, width: usize) -> String {
    no_tabs(text).chars().take(width).collect()
}

pub fn wrap_line(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let clean = no_tabs(text);
    if clean.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for raw in clean.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in raw.split(' ') {
            if word.chars().count() > width {
                if !current.is_empty() {
                    out.push(current.clone());
                    current.clear();
                }
                let mut rest: &str = word;
                while rest.chars().count() > width {
                    let head: String = rest.chars().take(width).collect();
                    let head_len = head.chars().count();
                    out.push(head);
                    rest = skip_chars(rest, head_len);
                }
                current.push_str(rest);
                continue;
            }
            let candidate_len = if current.is_empty() {
                word.chars().count()
            } else {
                current.chars().count() + 1 + word.chars().count()
            };
            if candidate_len > width {
                out.push(current.clone());
                current.clear();
                current.push_str(word);
            } else {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            }
        }
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn skip_chars(text: &str, count: usize) -> &str {
    match text.char_indices().nth(count) {
        Some((idx, _)) => &text[idx..],
        None => "",
    }
}

pub fn wrap_lines(lines: &[String], width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in lines {
        out.extend(wrap_line(line, width));
    }
    out
}
