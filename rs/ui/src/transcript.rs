use crate::model::TranscriptItem;
use std::collections::HashSet;

pub fn transcript_lines(items: &[TranscriptItem], expanded: &HashSet<usize>) -> Vec<String> {
    let mut out = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if item.is_tool() {
            push_tool(index, item, expanded, &mut out);
        } else {
            push_message(item, &mut out);
        }
    }
    if out.is_empty() {
        out.push("(no transcript)".to_string());
    }
    out
}

fn push_tool(
    index: usize,
    item: &TranscriptItem,
    expanded: &HashSet<usize>,
    out: &mut Vec<String>,
) {
    let name = item.tool_name.as_deref().unwrap_or("tool");
    let lines = item.line_count.unwrap_or(0);
    if expanded.contains(&index) {
        out.push(format!("[-] tool {name} ({lines} lines)"));
        for text_line in item.text.lines() {
            out.push(format!("    {text_line}"));
        }
        if item.text.is_empty() {
            out.push("    ".to_string());
        }
    } else {
        out.push(format!("[+] tool {name} ({lines} lines)"));
    }
}

fn push_message(item: &TranscriptItem, out: &mut Vec<String>) {
    let prefix = format!("{}: ", item.role.label());
    let mut lines = item.text.lines();
    match lines.next() {
        Some(first) => out.push(format!("{prefix}{first}")),
        None => out.push(prefix.trim_end().to_string()),
    }
    let pad = " ".repeat(prefix.len());
    for rest in lines {
        out.push(format!("{pad}{rest}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn tool_item_folds_by_default() {
        let items = vec![TranscriptItem::tool("bash", "line one\nline two")];
        let expanded = HashSet::new();
        let lines = transcript_lines(&items, &expanded);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("[+] tool bash"));
    }

    #[test]
    fn tool_item_expands_when_requested() {
        let items = vec![TranscriptItem::tool("bash", "line one\nline two")];
        let mut expanded = HashSet::new();
        expanded.insert(0);
        let lines = transcript_lines(&items, &expanded);
        assert!(lines[0].starts_with("[-] tool bash"));
        assert_eq!(lines.len(), 3);
    }
}
