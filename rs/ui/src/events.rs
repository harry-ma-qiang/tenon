use crate::model::EventLine;

pub fn event_lines(events: &[EventLine]) -> Vec<String> {
    let mut out: Vec<String> = events
        .iter()
        .map(|event| format!("{} {} {}", event.ts, event.kind, event.summary))
        .collect();
    if out.is_empty() {
        out.push("(no events)".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_events_has_placeholder() {
        assert_eq!(event_lines(&[]), vec!["(no events)".to_string()]);
    }

    #[test]
    fn formats_ts_kind_summary() {
        let events = vec![EventLine::new(100, "tool/call", "bash echo hi")];
        assert_eq!(
            event_lines(&events),
            vec!["100 tool/call bash echo hi".to_string()]
        );
    }
}
