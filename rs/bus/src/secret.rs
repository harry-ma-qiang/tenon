use arc_swap::ArcSwap;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The per-secret leak policy of RFC 8d.4: `mask` rewrites a leaked value in
/// place, `block` refuses the whole publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leak {
    Mask,
    Block,
}

impl Leak {
    pub fn parse(text: &str) -> Leak {
        match text {
            "block" => Leak::Block,
            _ => Leak::Mask,
        }
    }
}

/// One registered secret as the guard sees it: the value to look for and what to
/// do when it appears in a payload. The name is public (it is what a mask leaves
/// behind); the value never leaves base's own memory.
#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub value: String,
    pub leak: Leak,
}

/// The single leak choke point (RFC 8d.4): base pushes the current secret set
/// here, and the hub calls `scan` on every payload before it is fanned out or
/// persisted. Empty rule set is the fast path — the common case pays one atomic
/// load and returns.
#[derive(Default)]
pub struct SecretGuard {
    rules: ArcSwap<Vec<Rule>>,
}

impl SecretGuard {
    pub fn new() -> SecretGuard {
        SecretGuard {
            rules: ArcSwap::from_pointee(Vec::new()),
        }
    }

    pub fn set(&self, rules: Vec<Rule>) {
        self.rules.store(Arc::new(rules));
    }

    pub fn is_empty(&self) -> bool {
        self.rules.load().is_empty()
    }

    /// Walk every string in the payload. A `block` secret whose value appears
    /// short-circuits with the offending secret name; otherwise every `mask`
    /// secret value is rewritten to `***<name>***` in place. Returns the name to
    /// block on, or `None` when the payload is clean (possibly after masking).
    pub fn scan(&self, payload: &mut Value) -> Result<(), String> {
        let rules = self.rules.load();
        if rules.is_empty() {
            return Ok(());
        }
        for rule in rules.iter() {
            if rule.leak == Leak::Block && contains(payload, &rule.value) {
                return Err(rule.name.clone());
            }
        }
        for rule in rules.iter() {
            if rule.leak == Leak::Mask {
                mask(payload, &rule.value, &rule.name);
            }
        }
        Ok(())
    }

    /// The envelope-wide scan (RFC 8d.4): a secret is model-visible wherever it
    /// lands in the free-text envelope, not only in `payload`. `tags` is an
    /// equally-open field, so a `block` value in either refuses the publish and a
    /// `mask` value is rewritten in both. Block is checked across every field
    /// before any masking so a blocked value never half-lands.
    pub fn scan_envelope(
        &self,
        payload: &mut Value,
        tags: &mut BTreeMap<String, String>,
    ) -> Result<(), String> {
        let rules = self.rules.load();
        if rules.is_empty() {
            return Ok(());
        }
        for rule in rules.iter() {
            if rule.leak == Leak::Block
                && (contains(payload, &rule.value) || tags_contain(tags, &rule.value))
            {
                return Err(rule.name.clone());
            }
        }
        for rule in rules.iter() {
            if rule.leak == Leak::Mask {
                mask(payload, &rule.value, &rule.name);
                mask_tags(tags, &rule.value, &rule.name);
            }
        }
        Ok(())
    }
}

fn tags_contain(tags: &BTreeMap<String, String>, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    tags.iter()
        .any(|(key, value)| key.contains(needle) || value.contains(needle))
}

fn mask_tags(tags: &mut BTreeMap<String, String>, needle: &str, name: &str) {
    if needle.is_empty() || tags.is_empty() || !tags_contain(tags, needle) {
        return;
    }
    let marker = format!("***{name}***");
    *tags = std::mem::take(tags)
        .into_iter()
        .map(|(key, value)| (key.replace(needle, &marker), value.replace(needle, &marker)))
        .collect();
}

fn contains(value: &Value, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    match value {
        Value::String(text) => text.contains(needle),
        Value::Array(rows) => rows.iter().any(|row| contains(row, needle)),
        Value::Object(map) => map.values().any(|row| contains(row, needle)),
        _ => false,
    }
}

fn mask(value: &mut Value, needle: &str, name: &str) {
    if needle.is_empty() {
        return;
    }
    match value {
        Value::String(text) => {
            if text.contains(needle) {
                *text = text.replace(needle, &format!("***{name}***"));
            }
        }
        Value::Array(rows) => {
            for row in rows.iter_mut() {
                mask(row, needle, name);
            }
        }
        Value::Object(map) => {
            for row in map.values_mut() {
                mask(row, needle, name);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mask_rewrites_the_value_and_leaves_the_name() {
        let guard = SecretGuard::new();
        guard.set(vec![Rule {
            name: "api".to_string(),
            value: "sk-123".to_string(),
            leak: Leak::Mask,
        }]);
        let mut payload = json!({"tail": "used key sk-123 here", "n": 1});
        assert!(guard.scan(&mut payload).is_ok());
        assert_eq!(payload["tail"], "used key ***api*** here");
        assert_eq!(payload["n"], 1);
    }

    #[test]
    fn block_refuses_and_names_the_secret() {
        let guard = SecretGuard::new();
        guard.set(vec![Rule {
            name: "token".to_string(),
            value: "top-secret".to_string(),
            leak: Leak::Block,
        }]);
        let mut payload = json!(["ok", {"deep": "top-secret"}]);
        assert_eq!(guard.scan(&mut payload), Err("token".to_string()));
    }

    #[test]
    fn scan_envelope_masks_and_blocks_in_tags() {
        let guard = SecretGuard::new();
        guard.set(vec![
            Rule {
                name: "api".to_string(),
                value: "sk-123".to_string(),
                leak: Leak::Mask,
            },
            Rule {
                name: "tok".to_string(),
                value: "top-secret".to_string(),
                leak: Leak::Block,
            },
        ]);
        let mut payload = json!({"ok": true});
        let mut tags: BTreeMap<String, String> = BTreeMap::new();
        tags.insert("leaked".to_string(), "value sk-123 here".to_string());
        assert!(guard.scan_envelope(&mut payload, &mut tags).is_ok());
        assert_eq!(
            tags.get("leaked").map(String::as_str),
            Some("value ***api*** here")
        );

        let mut blocked: BTreeMap<String, String> = BTreeMap::new();
        blocked.insert("leaked".to_string(), "top-secret".to_string());
        assert_eq!(
            guard.scan_envelope(&mut json!({}), &mut blocked),
            Err("tok".to_string())
        );
    }

    #[test]
    fn empty_ruleset_is_a_no_op() {
        let guard = SecretGuard::new();
        let mut payload = json!({"x": "anything"});
        assert!(guard.scan(&mut payload).is_ok());
        assert_eq!(payload["x"], "anything");
    }
}
