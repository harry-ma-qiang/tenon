use crate::envelope::{Envelope, Level};
use std::collections::BTreeMap;

/// The server-side subscription filter of RFC section 3: topic globs, a level
/// set, and the env/session/tags a subscriber narrows to. Every field is
/// conjunctive; an empty list means "no constraint on this axis".
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub topics: Vec<String>,
    pub levels: Vec<Level>,
    pub env: Option<String>,
    pub session: Option<String>,
    pub tags: BTreeMap<String, String>,
}

impl Filter {
    pub fn all() -> Self {
        Filter::default()
    }

    /// The env-scope of RFC section 8d.2: a scoped caller's filter is pinned to
    /// its env and can never widen past it. base/barebone callers leave it None.
    pub fn scoped_to(env: &str) -> Self {
        Filter {
            env: Some(env.to_string()),
            ..Filter::default()
        }
    }

    pub fn matches(&self, envelope: &Envelope) -> bool {
        if !self.topics.is_empty() && !self.topics.iter().any(|p| glob(p, &envelope.topic)) {
            return false;
        }
        if !self.levels.is_empty() && !self.levels.contains(&envelope.level) {
            return false;
        }
        if let Some(env) = &self.env {
            if envelope.env.as_deref() != Some(env.as_str()) {
                return false;
            }
        }
        if let Some(session) = &self.session {
            if envelope.session.as_deref() != Some(session.as_str()) {
                return false;
            }
        }
        for (key, value) in &self.tags {
            if envelope.tags.get(key).map(String::as_str) != Some(value.as_str()) {
                return false;
            }
        }
        true
    }
}

/// MQTT-style topic glob over `/` segments: `*` matches exactly one segment,
/// `**` matches zero or more, a literal segment matches itself. `**` alone
/// matches everything, which is the "subscribe to the firehose" case.
pub fn glob(pattern: &str, topic: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let topic: Vec<&str> = topic.split('/').collect();
    seg_match(&pattern, &topic)
}

fn seg_match(pattern: &[&str], topic: &[&str]) -> bool {
    match pattern.first() {
        None => topic.is_empty(),
        Some(&"**") => {
            if pattern.len() == 1 {
                return true;
            }
            (0..=topic.len()).any(|skip| seg_match(&pattern[1..], &topic[skip..]))
        }
        Some(&"*") => !topic.is_empty() && seg_match(&pattern[1..], &topic[1..]),
        Some(head) => match topic.first() {
            Some(first) if first == head => seg_match(&pattern[1..], &topic[1..]),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn glob_segments() {
        assert!(glob("session/*", "session/tool"));
        assert!(!glob("session/*", "session/tool/x"));
        assert!(glob("session/**", "session/tool/x"));
        assert!(glob("**", "anything/at/all"));
        assert!(glob("base/boot", "base/boot"));
        assert!(!glob("base/boot", "base/stop"));
        assert!(glob("worker/**", "worker/step"));
        assert!(!glob("worker/**", "guardian/probe"));
    }

    #[test]
    fn filter_conjunction() {
        let mut env = Envelope::new("session/tool", Level::Info, json!({}));
        env.env = Some("root".to_string());
        env.session = Some("s1".to_string());
        env.tags.insert("k".to_string(), "v".to_string());

        let mut filter = Filter {
            topics: vec!["session/*".to_string()],
            levels: vec![Level::Info],
            env: Some("root".to_string()),
            session: Some("s1".to_string()),
            ..Filter::default()
        };
        filter.tags.insert("k".to_string(), "v".to_string());
        assert!(filter.matches(&env));

        filter.env = Some("other".to_string());
        assert!(!filter.matches(&env), "env-scope must reject a foreign env");
    }

    #[test]
    fn level_set_membership() {
        let env = Envelope::new("x/y", Level::Warn, json!({}));
        let filter = Filter {
            levels: vec![Level::Error],
            ..Filter::default()
        };
        assert!(!filter.matches(&env));
    }
}
