mod gate;

use gate::{alive, kill_alive, skip_release, wait_gone, Fixture, Spec, BIN};
use serde_json::Value;
use std::process::Command;
use std::time::Duration;

fn fixture(name: &str) -> Option<Fixture> {
    let release = skip_release(name)?;
    Some(Fixture::open(
        BIN,
        release,
        Spec {
            name,
            lock: true,
            ..Spec::default()
        },
    ))
}

#[test]
fn the_harness_without_a_base_socket_fails_loudly() {
    let output = Command::new(BIN)
        .arg("harness")
        .env_remove("TENON_BASE_SOCK")
        .output()
        .expect("run tenon");
    assert_eq!(output.status.code(), Some(2));
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("TENON_BASE_SOCK"), "{text}");
}

#[test]
fn the_worker_without_a_reachable_gateway_fails_loudly() {
    let dir = std::env::temp_dir().join(format!("tenon-it-{}-nowire", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let output = Command::new(BIN)
        .arg("worker")
        .arg("--workspace")
        .arg(&dir)
        .env(
            "TENON_GATEWAY",
            format!("unix:{}/absent.sock", dir.display()),
        )
        .output()
        .expect("run tenon worker");
    let text = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{text}");
    assert!(text.contains("connect"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_base_is_an_error_not_a_hang() {
    let home = std::env::temp_dir().join(format!("tenon-it-{}-nobase", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let output = Command::new(BIN)
        .arg("--home")
        .arg(&home)
        .arg("status")
        .output()
        .expect("run tenon");
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("is the base running?"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn boot_registers_both_nodes_and_mounts_the_demo_plugin() {
    let Some(fixture) = fixture("boot") else {
        return;
    };
    fixture.start();
    let status = fixture.cli_status();
    let envs: Vec<&str> = status["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["env"].as_str().unwrap())
        .collect();
    assert_eq!(envs, vec!["guardian", "root"]);

    let guardian = fixture.cli_node("guardian");
    assert_eq!(guardian["role"], "guardian");
    assert_eq!(guardian["registered"], true);
    let ids = fiber_ids(&guardian["tree"]);
    assert!(ids.contains(&"guardian".to_string()), "{ids:?}");
    assert!(ids.contains(&"link".to_string()), "{ids:?}");

    let root = fixture.cli_node("root");
    assert_eq!(root["role"], "agent");
    assert_eq!(root["registered"], true);
    assert!(
        root["sandbox"]["backend"] == "oci" || root["sandbox"]["backend"] == "landlock",
        "unexpected sandbox on the default auto profile: {}",
        root["sandbox"]
    );
    let ids = fiber_ids(&root["tree"]);
    assert!(
        ids.contains(&"demo".to_string()),
        "no demo plugin in {ids:?}"
    );
    assert!(fixture.home.join("lkg/profiles/root/tenon.yml").is_file());
}

#[test]
fn reset_replaces_the_env_and_leaves_the_guardian_alone() {
    let Some(fixture) = fixture("reset") else {
        return;
    };
    fixture.start();
    let before = fixture.cli_node("root")["pid"].as_i64().unwrap();
    let guardian = fixture.cli_node("guardian")["pid"].as_i64().unwrap();

    let (ok, text) = fixture.run_text(&["reset"]);
    assert!(ok, "reset failed: {text}");
    let after = fixture.await_fresh("root", before);
    assert_ne!(before, after, "reset kept the same pid");
    assert!(!alive(before), "the old env is still running");
    assert_eq!(
        fixture.cli_node("guardian")["pid"].as_i64().unwrap(),
        guardian
    );
    assert!(alive(guardian), "the guardian went down with the env");
}

#[test]
fn killing_base_takes_every_node_down() {
    let Some(fixture) = fixture("kill") else {
        return;
    };
    fixture.start();
    let base = fixture.base_pid();
    let nodes = vec![
        fixture.cli_node("guardian")["pid"].as_i64().unwrap(),
        fixture.cli_node("root")["pid"].as_i64().unwrap(),
    ];
    kill_alive(base, "-9");
    let took = wait_gone(&nodes, Duration::from_secs(5));
    for pid in &nodes {
        assert!(!alive(*pid), "node {pid} survived base after {took:?}");
    }
}

#[test]
fn stop_shuts_down_base_and_both_nodes() {
    let Some(fixture) = fixture("stop") else {
        return;
    };
    fixture.start();
    let base = fixture.base_pid();
    let mut pids = vec![base];
    pids.push(fixture.cli_node("guardian")["pid"].as_i64().unwrap());
    pids.push(fixture.cli_node("root")["pid"].as_i64().unwrap());
    let (ok, text) = fixture.run_text(&["stop"]);
    assert!(ok, "stop failed: {text}");
    let took = wait_gone(&pids, Duration::from_secs(15));
    for pid in &pids {
        assert!(!alive(*pid), "{pid} survived stop after {took:?}");
    }
    assert!(!fixture.home.join("run/base.sock").exists());
    assert!(!fixture.home.join("run/base.ready").exists());
}

#[test]
fn an_env_that_dies_is_restarted_by_base() {
    let Some(fixture) = fixture("restart") else {
        return;
    };
    fixture.start();
    let before = fixture.cli_node("root")["pid"].as_i64().unwrap();
    kill_alive(before, "-9");
    let after = fixture.await_fresh("root", before);
    assert_ne!(before, after);
    assert_eq!(fixture.cli_node("root")["restarts"], 1);
}

fn fiber_ids(tree: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = tree["id"].as_str() {
        ids.push(id.to_string());
    }
    if let Some(children) = tree["children"].as_array() {
        for child in children {
            ids.extend(fiber_ids(child));
        }
    }
    ids
}
