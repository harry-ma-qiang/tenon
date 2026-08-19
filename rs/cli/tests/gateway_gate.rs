mod gate;

use gate::{plain, skip};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};

const NAME: &str = "gateway-gate";

fn root_tree(status: &Value) -> Value {
    status["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["env"] == "root")
        .cloned()
        .unwrap_or(Value::Null)["tree"]
        .clone()
}

fn find(tree: &Value, id: &str) -> Option<Value> {
    if tree["id"] == id {
        return Some(tree.clone());
    }
    tree["children"]
        .as_array()?
        .iter()
        .find_map(|child| find(child, id))
}

fn gateway_children(status: &Value) -> Vec<Value> {
    let tree = root_tree(status);
    match find(&tree, "gateway") {
        Some(gateway) => gateway["children"].as_array().cloned().unwrap_or_default(),
        None => vec![],
    }
}

fn active(children: &[Value]) -> Vec<String> {
    children
        .iter()
        .filter(|child| child["status"] != "failed")
        .filter_map(|child| child["id"].as_str().map(str::to_string))
        .collect()
}

const PLUGIN_BODY: &str = r#"
import sys
sys.path.insert(0, "/workspace")
import tenon

plugin = tenon.Plugin(inject=[])
plugin.provide("inside", {"ping": lambda: "pong"})
plugin.run()
"#;

#[tokio::test]
async fn a_plugin_started_inside_the_sandbox_registers_through_the_gateway() {
    let Some(release) = skip(NAME) else {
        println!(
            "skipping {NAME}: no beam release. Build it with \
             `cd beam && MIX_ENV=prod mix release` or set TENON_RELEASE_DIR"
        );
        return;
    };
    let fixture = plain(NAME, release, "sandbox: oci\n");
    fixture.start();

    let status = fixture.status().await;
    let root = status["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["env"] == "root")
        .cloned()
        .unwrap();
    assert_eq!(root["sandbox"]["backend"], "oci", "{status}");
    // Wait until this env's own gateway fibers — the worker and, since P3.3,
    // the harness — are up, so "a new fiber appeared" below can only be the
    // plugin the test starts inside the sandbox.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut before = active(&gateway_children(&status));
    while Instant::now() < deadline {
        let status = fixture.status().await;
        let root = status["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["env"] == "root")
            .cloned()
            .unwrap();
        before = active(&gateway_children(&status));
        if root["worker"]["state"] == "ready" && root["harness"]["state"] == "ready" {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }

    let workspace = fixture.workspace();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !workspace.is_dir() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(workspace.is_dir(), "no workspace dir at {workspace:?}");

    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/py/tenon.py");
    std::fs::copy(&sdk, workspace.join("tenon.py")).expect("copy sdk/py/tenon.py");
    std::fs::write(workspace.join("inside_plugin.py"), PLUGIN_BODY).expect("write plugin");

    let launch = fixture
        .rpc(
            "sandbox.exec",
            json!({
                "env": "root",
                "cmd": "sh",
                "args": [
                    "-c",
                    "nohup python3 /workspace/inside_plugin.py \
                     >/workspace/inside.log 2>&1 </dev/null & echo started"
                ],
                "timeout": 10_000,
            }),
        )
        .await
        .expect("sandbox.exec launch");
    assert_eq!(launch["status"], 0, "{launch}\n{}", fixture.log());

    // The service answering is the registration, not the fiber count: poll it,
    // then assert the fiber is there too.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ping = Err("never answered".to_string());
    while Instant::now() < deadline {
        ping = fixture
            .rpc(
                "svc",
                json!({"env": "root", "name": "inside", "method": "ping", "args": []}),
            )
            .await;
        if ping.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(ping.expect("svc ping"), "pong", "{}", fixture.log());
    let after = active(&gateway_children(&fixture.status().await));
    assert!(
        after.len() > before.len(),
        "no new gateway fiber appeared: before={before:?} after={after:?}\n{}",
        fixture.log()
    );

    let destroyed = fixture
        .rpc("sandbox.destroy", json!({"env": "root"}))
        .await
        .expect("sandbox.destroy");
    assert_eq!(destroyed["ok"], true);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut gone = false;
    while Instant::now() < deadline {
        let status = fixture.status().await;
        if active(&gateway_children(&status)).len() < after.len() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(gone, "the plugin fiber outlived its sandbox instance");

    let status = fixture.status().await;
    assert_eq!(status["nodes"].as_array().unwrap().len(), 2, "{status}");

    let (ok, text) = fixture.run_text(&["reset"]);
    assert!(ok, "reset failed: {text}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut registered = false;
    while Instant::now() < deadline {
        let status = fixture.status().await;
        let root = status["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["env"] == "root")
            .cloned()
            .unwrap();
        if root["registered"] == true {
            registered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(registered, "root did not come back after the reset");
    let status = fixture.status().await;
    assert_eq!(status["nodes"].as_array().unwrap().len(), 2);
}
