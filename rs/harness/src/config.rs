use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Llm {
    #[serde(default = "provider")]
    pub provider: String,
    #[serde(default = "base_url")]
    pub base_url: String,
    #[serde(default = "model")]
    pub model: String,
    #[serde(default = "api_key_env")]
    pub api_key_env: String,
    #[serde(default)]
    pub temperature: f64,
    #[serde(default = "timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "retry_attempts")]
    pub retry_attempts: u32,
    #[serde(default = "retry_base_ms")]
    pub retry_base_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default)]
    pub llm: Llm,
    #[serde(default = "max_steps")]
    pub max_steps: usize,
    #[serde(default = "approval")]
    pub approval: String,
    #[serde(default = "tool_timeout_ms")]
    pub tool_timeout_ms: u64,
}

fn provider() -> String {
    "openai".to_string()
}

fn base_url() -> String {
    "https://api.deepseek.com".to_string()
}

fn model() -> String {
    "deepseek-v4-flash".to_string()
}

fn api_key_env() -> String {
    "DEEPSEEK_API_KEY".to_string()
}

fn timeout_ms() -> u64 {
    120_000
}

fn retry_attempts() -> u32 {
    3
}

fn retry_base_ms() -> u64 {
    200
}

fn max_steps() -> usize {
    8
}

fn approval() -> String {
    "deny".to_string()
}

fn tool_timeout_ms() -> u64 {
    20_000
}

impl Default for Llm {
    fn default() -> Self {
        Self {
            provider: provider(),
            base_url: base_url(),
            model: model(),
            api_key_env: api_key_env(),
            temperature: 0.0,
            timeout_ms: timeout_ms(),
            retry_attempts: retry_attempts(),
            retry_base_ms: retry_base_ms(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            llm: Llm::default(),
            max_steps: max_steps(),
            approval: approval(),
            tool_timeout_ms: tool_timeout_ms(),
        }
    }
}

impl Settings {
    /// Base hands the env's profile overlay down as JSON in the environment,
    /// so the harness never reads a config file itself and a `config.patch`
    /// round trip is what re-reads it.
    pub fn from_env(name: &str) -> Self {
        let Some(body) = std::env::var(name)
            .ok()
            .filter(|text| !text.trim().is_empty())
        else {
            return Settings::default();
        };
        Self::parse(&body)
    }

    pub fn parse(body: &str) -> Self {
        match serde_json::from_str::<Value>(body) {
            Ok(value) => serde_json::from_value(value).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }
}
