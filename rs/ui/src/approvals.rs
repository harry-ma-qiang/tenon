use crate::model::Approval;

pub fn approval_lines(approvals: &[Approval]) -> Vec<String> {
    let mut out: Vec<String> = approvals
        .iter()
        .map(|approval| {
            format!(
                "[{}] env={} reason={}",
                approval.id, approval.env, approval.reason
            )
        })
        .collect();
    if out.is_empty() {
        out.push("(no pending approvals)".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_approvals_has_placeholder() {
        assert_eq!(
            approval_lines(&[]),
            vec!["(no pending approvals)".to_string()]
        );
    }

    #[test]
    fn formats_id_env_reason() {
        let approvals = vec![Approval::new("a1", "root", "publish workspace")];
        assert_eq!(
            approval_lines(&approvals),
            vec!["[a1] env=root reason=publish workspace".to_string()]
        );
    }
}
