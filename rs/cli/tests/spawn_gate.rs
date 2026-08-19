mod gate;

use gate::{plain, skip};
use serde_json::{json, Value};
use std::process::Command;
use std::time::{Duration, Instant};

const NAME: &str = "spawn-gate";
/// The limits under test are the environment tree's, not the human gate's, so
/// `spawn_soft_limit: 0` turns the P3.5 approval gate off for this home.
const CONFIG: &str = "sandbox: oci\nenvs:\n  max_total: 3\n  max_depth: 1\n  ram_mb: 384\n\
                      approval:\n  spawn_soft_limit: 0\n";

fn gateway_children(node: &Value) -> usize {
    fn find(tree: &Value, id: &str) -> Option<Value> {
        if tree["id"] == id {
            return Some(tree.clone());
        }
        tree["children"]
            .as_array()?
            .iter()
            .find_map(|child| find(child, id))
    }
    match find(&node["tree"], "gateway") {
        Some(gateway) => gateway["children"].as_array().map(Vec::len).unwrap_or(0),
        None => 0,
    }
}

#[tokio::test]
async fn a_child_env_is_a_fiber_of_its_parent_and_dies_with_it() {
    let Some(release) = skip(NAME) else {
        println!(
            "skipping {NAME}: no beam release. Build it with \
             `cd beam && MIX_ENV=prod mix release` or set TENON_RELEASE_DIR"
        );
        return;
    };
    let fixture = plain(NAME, release, CONFIG);
    let (ok, text) = fixture.run_text(&["start"]);
    assert!(ok, "start failed: {text}");
    fixture.worker_ready("root", Duration::from_secs(90)).await;
    let before = gateway_children(&fixture.node("root").await);

    let child = fixture
        .rpc("runtime.spawn", json!({"parent": "root", "overrides": {}}))
        .await
        .expect("runtime.spawn");
    assert_eq!(child["env"], "root.1", "{child}");
    assert_eq!(child["depth"], 1, "{child}");
    assert_eq!(child["ram_mb"], 384, "{child}");
    assert!(
        child["profile"]
            .as_str()
            .unwrap_or_default()
            .contains("overlay.patch.yml"),
        "the child got no patch layer: {child}"
    );

    fixture.registered("root.1", Duration::from_secs(60)).await;
    let root = fixture.node("root").await;
    assert_eq!(root["children"], json!(["root.1"]), "{root}");
    let spawned = fixture.node("root.1").await;
    assert_eq!(spawned["parent"], "root", "{spawned}");
    assert_eq!(spawned["depth"], 1, "{spawned}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut after = before;
    while Instant::now() < deadline {
        after = gateway_children(&fixture.node("root").await);
        if after > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    assert!(
        after > before,
        "the child never appeared as a fiber in its parent's tree ({before} -> {after})"
    );

    let deep = fixture
        .rpc(
            "runtime.spawn",
            json!({"parent": "root.1", "overrides": {}}),
        )
        .await
        .expect_err("depth 2 must be refused");
    assert!(deep.contains("depth"), "{deep}");

    fixture
        .rpc("runtime.spawn", json!({"parent": "root", "overrides": {}}))
        .await
        .expect("the second child is still inside the limit");
    let over = fixture
        .rpc("runtime.spawn", json!({"parent": "root", "overrides": {}}))
        .await
        .expect_err("a fourth environment must be refused");
    assert!(over.contains("limit"), "{over}");

    let parent_sock = fixture.home.join("run/gw-root/gateway.sock");
    let base_sock = fixture.home.join("run/base.sock");
    let seen = fixture
        .exec(
            "root.1",
            &format!(
                "test -e {} && echo parent-gateway-visible; test -e {} && echo base-sock-visible; \
                 python3 -c \"import socket;socket.socket(socket.AF_UNIX).connect('{}')\" \
                 2>/dev/null && echo connected; echo checked",
                parent_sock.display(),
                base_sock.display(),
                parent_sock.display(),
            ),
        )
        .await;
    let text = seen["stdout"].as_str().unwrap_or_default();
    assert!(text.contains("checked"), "{seen}");
    assert!(
        !text.contains("visible") && !text.contains("connected"),
        "a child reached its parent's sockets: {seen}"
    );

    let pid = fixture.node("root").await["pid"]
        .as_i64()
        .expect("root pid");
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill the parent node");
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut gone = false;
    while Instant::now() < deadline {
        if fixture.node("root.1").await.is_null() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    assert!(gone, "the child outlived its parent\n{}", fixture.log());
    fixture.registered("root", Duration::from_secs(60)).await;
}
