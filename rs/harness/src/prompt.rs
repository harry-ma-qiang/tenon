use crate::bus::Answer;
use serde_json::{json, Value};
use std::sync::Mutex;
use tenon_base::params::{i64_or, str_of, text, u64_or};

const EXTEND: &str = "\
How to extend Tenon. You are running inside a Tenon environment and may change it while \
you work, through these tools:
- plugin.list / plugin.mount / plugin.unmount / plugin.restart: the fibers of this \
environment's kernel node. Mount by registry name or by an explicit {module} or \
{cmd, args, env} spec; a mounted plugin shows up in `tenon status` under the tree.
- config.get / config.patch: this environment's profile overlay. Every patch is \
snapshotted first and the node reloads its profile afterwards.
- snapshot.list / snapshot.restore: the workspace snapshots the host holds for this \
environment; restore rewinds the workspace to one of them.
- runtime.spawn: ask the barebone for a child environment of this one.
- approval.request: ask a human for something that would affect the host.
Tool failures come back as the reason, not as a crash: read it and adapt.";

struct Section {
    id: u64,
    name: String,
    order: i64,
    text: String,
}

/// The system prompt as registered sections (seam 1). Every registration is
/// addressable: `unregister` with the id `register` handed back is the disposer.
#[derive(Default)]
pub struct Prompt {
    sections: Mutex<Vec<Section>>,
    seq: Mutex<u64>,
}

impl Prompt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn builtin(&self, workspace: &str, env: &str) {
        self.register(
            "identity",
            -100,
            &format!(
                "You are the agent of the Tenon environment `{env}`. Your workspace is \
                 `{workspace}` inside a sandbox; the tools below are the only way to reach it. \
                 Be brief and act rather than describe."
            ),
        );
        self.register("extend", 100, EXTEND);
    }

    pub fn register(&self, name: &str, order: i64, text: &str) -> u64 {
        let mut seq = self.seq.lock().expect("prompt lock");
        *seq += 1;
        let id = *seq;
        let mut sections = self.sections.lock().expect("prompt lock");
        sections.retain(|section| section.name != name);
        sections.push(Section {
            id,
            name: name.to_string(),
            order,
            text: text.to_string(),
        });
        id
    }

    pub fn unregister(&self, id: u64, name: Option<&str>) -> bool {
        let mut sections = self.sections.lock().expect("prompt lock");
        let before = sections.len();
        sections.retain(|section| section.id != id && Some(section.name.as_str()) != name);
        sections.len() != before
    }

    pub fn list(&self) -> Value {
        let sections = self.sections.lock().expect("prompt lock");
        let mut rows: Vec<&Section> = sections.iter().collect();
        rows.sort_by_key(|section| (section.order, section.id));
        json!({
            "sections": rows
                .iter()
                .map(|section| json!({
                    "id": section.id,
                    "name": section.name,
                    "order": section.order,
                    "bytes": section.text.len(),
                }))
                .collect::<Vec<Value>>(),
        })
    }

    pub fn render(&self) -> String {
        let sections = self.sections.lock().expect("prompt lock");
        let mut rows: Vec<&Section> = sections.iter().collect();
        rows.sort_by_key(|section| (section.order, section.id));
        rows.iter()
            .map(|section| section.text.clone())
            .collect::<Vec<String>>()
            .join("\n\n")
    }

    pub fn call(&self, method: &str, args: &[Value]) -> Answer {
        let params = args.first().cloned().unwrap_or(Value::Null);
        match method {
            "register" => {
                let name = text(&params, "name");
                if name.is_empty() {
                    return Err("prompt.register needs a name".to_string());
                }
                let order = i64_or(&params, "order", 0);
                let id = self.register(&name, order, &text(&params, "text"));
                Ok(json!({"ok": true, "id": id, "name": name}))
            }
            "unregister" => {
                let id = u64_or(&params, "id", 0);
                let name = str_of(&params, "name");
                Ok(json!({"ok": self.unregister(id, name)}))
            }
            "list" => Ok(self.list()),
            "render" => Ok(json!({"text": self.render()})),
            other => Err(format!("unknown method {other}")),
        }
    }
}
