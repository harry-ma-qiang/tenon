use serde::de::DeserializeOwned;
use serde_json::Value;

/// One typed view of a params object. A handler whose parameters have more
/// than a field or two names a `#[derive(Deserialize)]` struct and asks for it
/// here instead of pulling every key out of the `Value` by hand.
pub fn parse<T: DeserializeOwned>(params: &Value) -> Result<T, String> {
    serde_json::from_value(params.clone()).map_err(|error| error.to_string())
}

pub fn str_of<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

/// The string at `key`, or an empty one: the shape every optional text field
/// on the wire has.
pub fn text(params: &Value, key: &str) -> String {
    str_of(params, key).unwrap_or_default().to_string()
}

pub fn text_or(params: &Value, key: &str, fallback: &str) -> String {
    str_of(params, key).unwrap_or(fallback).to_string()
}

pub fn opt_text(params: &Value, key: &str) -> Option<String> {
    str_of(params, key).map(str::to_string)
}

pub fn i64_or(params: &Value, key: &str, fallback: i64) -> i64 {
    params.get(key).and_then(Value::as_i64).unwrap_or(fallback)
}

pub fn u64_or(params: &Value, key: &str, fallback: u64) -> u64 {
    params.get(key).and_then(Value::as_u64).unwrap_or(fallback)
}

pub fn f64_or(params: &Value, key: &str, fallback: f64) -> f64 {
    params.get(key).and_then(Value::as_f64).unwrap_or(fallback)
}

pub fn bool_or(params: &Value, key: &str, fallback: bool) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(fallback)
}

/// The value at `key`, or an empty object: what a patch, an overlay or an
/// artifact defaults to.
pub fn object(params: &Value, key: &str) -> Value {
    match params.get(key) {
        Some(Value::Null) | None => Value::Object(serde_json::Map::new()),
        Some(value) => value.clone(),
    }
}

pub fn value(params: &Value, key: &str) -> Value {
    params.get(key).cloned().unwrap_or(Value::Null)
}

pub fn array(params: &Value, key: &str) -> Vec<Value> {
    params
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

pub fn strings(params: &Value, key: &str) -> Vec<String> {
    array(params, key)
        .iter()
        .filter_map(|row| row.as_str().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize)]
    struct Exec {
        cmd: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default = "thirty_seconds")]
        timeout: u64,
    }

    fn thirty_seconds() -> u64 {
        30_000
    }

    #[test]
    fn parse_fills_the_defaults_the_old_hand_extraction_had() {
        let row: Exec = parse(&json!({"cmd": "sh", "extra": 1})).expect("parse");
        assert_eq!(row.cmd, "sh");
        assert!(row.args.is_empty());
        assert_eq!(row.timeout, 30_000);
    }

    #[test]
    fn parse_reports_the_missing_field_by_name() {
        let error = parse::<Exec>(&json!({})).expect_err("no cmd");
        assert!(error.contains("cmd"), "{error}");
    }

    #[test]
    fn readers_fall_back_instead_of_failing() {
        let row = json!({"n": 5, "name": "a", "on": true, "rows": ["x", 1]});
        assert_eq!(i64_or(&row, "n", 0), 5);
        assert_eq!(i64_or(&row, "missing", 7), 7);
        assert_eq!(text(&row, "name"), "a");
        assert_eq!(text(&row, "missing"), "");
        assert_eq!(text_or(&row, "missing", "d"), "d");
        assert!(bool_or(&row, "on", false));
        assert_eq!(strings(&row, "rows"), vec!["x".to_string()]);
        assert_eq!(object(&row, "missing"), json!({}));
    }
}
