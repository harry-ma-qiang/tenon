use crate::bus::Facades;
use crate::facaderpc::Conn;
use crate::params::{strings, text_or};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tenon_bus::{Hub, Leak, Rule};

type Answer = Result<Value, String>;

/// One stored secret (RFC 8d.4): its value lives only here, in base's own
/// `secrets.yml`, never in an env's state file and never inside an envelope. A
/// secret is readable by an env only if that env is in `grants` (base and the
/// CLI, being unscoped, always read).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    name: String,
    value: String,
    #[serde(default = "default_leak")]
    leak: String,
    #[serde(default)]
    grants: Vec<String>,
}

fn default_leak() -> String {
    "mask".to_string()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    secrets: Vec<Entry>,
}

/// The secrets facade: `set`/`get`/`list` over a base-only file, plus the push
/// of the current value+policy set into the hub so its leak guard can mask or
/// block those values before any fan-out or persistence.
pub struct Secrets {
    path: PathBuf,
    hub: Arc<Hub>,
    entries: Mutex<Vec<Entry>>,
}

impl Secrets {
    pub fn new(path: PathBuf, hub: Arc<Hub>) -> Arc<Secrets> {
        let entries = load(&path);
        let secrets = Arc::new(Secrets {
            path,
            hub,
            entries: Mutex::new(entries),
        });
        secrets.push_to_hub();
        secrets
    }

    fn push_to_hub(&self) {
        let rules = self
            .entries
            .lock()
            .expect("secrets")
            .iter()
            .map(|entry| Rule {
                name: entry.name.clone(),
                value: entry.value.clone(),
                leak: Leak::parse(&entry.leak),
            })
            .collect();
        self.hub.set_secrets(rules);
    }

    fn set(&self, name: &str, value: &str, leak: &str, grants: Vec<String>) -> Answer {
        if name.is_empty() {
            return Err("secret name required".to_string());
        }
        {
            let mut entries = self.entries.lock().expect("secrets");
            entries.retain(|entry| entry.name != name);
            entries.push(Entry {
                name: name.to_string(),
                value: value.to_string(),
                leak: match leak {
                    "block" => "block".to_string(),
                    _ => "mask".to_string(),
                },
                grants,
            });
        }
        self.persist()?;
        self.push_to_hub();
        Ok(json!({"ok": true, "name": name}))
    }

    /// The value, but only to a caller allowed to read it: an unscoped base/CLI
    /// caller always, a scoped env only when it is granted. The value is never
    /// logged and never travels in an event.
    fn get(&self, scope: Option<&str>, name: &str) -> Answer {
        let entries = self.entries.lock().expect("secrets");
        let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
            return Err("no_such_secret".to_string());
        };
        if let Some(env) = scope {
            if !entry.grants.iter().any(|grant| grant == env) {
                return Err("not_granted".to_string());
            }
        }
        Ok(json!({"name": name, "value": entry.value, "leak": entry.leak}))
    }

    fn list(&self) -> Answer {
        let entries = self.entries.lock().expect("secrets");
        let names: Vec<Value> = entries
            .iter()
            .map(|entry| json!({"name": entry.name, "leak": entry.leak, "grants": entry.grants}))
            .collect();
        Ok(json!({"count": names.len(), "secrets": names}))
    }

    fn persist(&self) -> Result<(), String> {
        let file = File {
            secrets: self.entries.lock().expect("secrets").clone(),
        };
        let body = serde_yaml::to_string(&file).map_err(|error| error.to_string())?;
        write_private(&self.path, &body).map_err(|error| error.to_string())
    }
}

fn load(path: &PathBuf) -> Vec<Entry> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_yaml::from_str::<File>(&body)
        .map(|file| file.secrets)
        .unwrap_or_default()
}

fn write_private(path: &PathBuf, body: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("yml.tmp");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(body.as_bytes())?;
        file.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `secret.set/get/list` on the front door. `set`/`list` are base/management
/// operations; `get` is grant-checked against the caller's bound env (RFC
/// 8d.4). The single 8d.2 scope resolver decides who is scoped.
pub async fn handle(method: &str, body: &Value, conn: &Conn, facades: &Facades) -> Answer {
    let secrets = &facades.secrets;
    match method {
        "secret.set" => {
            let name = text_or(body, "name", "");
            let value = text_or(body, "value", "");
            let leak = text_or(body, "leak", "mask");
            let grants = strings(body, "grants");
            secrets.set(&name, &value, &leak, grants)
        }
        "secret.get" => {
            let name = text_or(body, "name", "");
            secrets.get(conn.bound_scope().as_deref(), &name)
        }
        "secret.list" => secrets.list(),
        other => Err(format!("unknown_method:{other}")),
    }
}

pub fn is_secret(method: &str) -> bool {
    matches!(method, "secret.set" | "secret.get" | "secret.list")
}
