use crate::model::NodeInfo;

pub fn tree_lines(envs: &[NodeInfo]) -> Vec<String> {
    let mut out = Vec::new();
    for node in envs {
        push_node(node, 0, &mut out);
    }
    if out.is_empty() {
        out.push("(no environments)".to_string());
    }
    out
}

fn push_node(node: &NodeInfo, depth: usize, out: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    let mut line = format!("{indent}{} [{}] {}", node.name, node.role, node.status);
    if let Some(sandbox) = &node.sandbox {
        line.push_str(&format!(" sandbox={sandbox}"));
    }
    if node.restarts > 0 {
        line.push_str(&format!(" restarts={}", node.restarts));
    }
    out.push(line);
    for child in &node.children {
        push_node(child, depth + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree_has_placeholder() {
        let lines = tree_lines(&[]);
        assert_eq!(lines, vec!["(no environments)".to_string()]);
    }

    #[test]
    fn children_are_indented() {
        let child = NodeInfo::new("b", "agent", "up");
        let root = NodeInfo::new("a", "agent", "up").with_children(vec![child]);
        let lines = tree_lines(&[root]);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("  b"));
    }
}
