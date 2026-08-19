use crate::support::*;
use std::time::Duration;

#[test]
fn killing_guardian_restarts_it_and_leaves_the_env_untouched() {
    let Some(fixture) = fixture("kill-guardian") else {
        return;
    };
    fixture.start();
    let guardian1 = fixture.cli_node("guardian")["pid"].as_i64().unwrap();
    let root1 = fixture.cli_node("root")["pid"].as_i64().unwrap();

    kill(guardian1, "-9");
    let guardian2 = fixture.await_fresh("guardian", guardian1);
    assert_ne!(guardian1, guardian2);
    assert_eq!(fixture.cli_node("root")["pid"].as_i64().unwrap(), root1);
    assert!(
        alive(root1),
        "root went down when only the guardian was killed"
    );
}

#[test]
fn killing_both_nodes_restarts_both() {
    let Some(fixture) = fixture("kill-both") else {
        return;
    };
    fixture.start();
    let guardian1 = fixture.cli_node("guardian")["pid"].as_i64().unwrap();
    let root1 = fixture.cli_node("root")["pid"].as_i64().unwrap();

    kill(guardian1, "-9");
    kill(root1, "-9");

    let guardian2 = fixture.await_fresh("guardian", guardian1);
    let root2 = fixture.await_fresh("root", root1);
    assert_ne!(guardian1, guardian2);
    assert_ne!(root1, root2);
    assert!(alive(guardian2) && alive(root2));
}

#[test]
fn restart_limit_stops_restarting_and_reports_give_up() {
    let Some(fixture) = fixture_with_config("restart-limit", Some("max_restarts: 1\n")) else {
        return;
    };
    fixture.start();
    let root1 = fixture.cli_node("root")["pid"].as_i64().unwrap();

    kill(root1, "-9");
    let root2 = fixture.await_fresh("root", root1);
    assert_eq!(fixture.cli_node("root")["restarts"], 1);

    kill(root2, "-9");
    wait_gone(&[root2], Duration::from_secs(5));

    let stayed_down = fixture.await_condition(Duration::from_secs(5), |status| {
        let root = status["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["env"] == "root")
            .cloned()
            .unwrap();
        root["registered"] == false && root["pid"].is_null()
    });
    assert!(
        stayed_down,
        "root should be given up on, not restarted, after hitting max_restarts"
    );

    std::thread::sleep(Duration::from_secs(2));
    let root = fixture.cli_node("root");
    assert_eq!(
        root["registered"], false,
        "base restarted root past its configured limit"
    );
    assert!(
        root["pid"].is_null(),
        "base restarted root past its configured limit"
    );
}

#[test]
fn guardian_resets_a_frozen_agent_and_sigcont_does_not_confuse_base() {
    let config = "request_timeout_ms: 700\nstop_grace_ms: 500\nguardian:\n  interval_ms: 250\n  failures: 3\n";
    let Some(fixture) = fixture_with_config("guardian-freeze", Some(config)) else {
        return;
    };
    fixture.start();
    let guardian1 = fixture.cli_node("guardian")["pid"].as_i64().unwrap();
    let root1 = fixture.cli_node("root")["pid"].as_i64().unwrap();

    kill(root1, "-STOP");
    let root2 = fixture.await_fresh("root", root1);
    assert_ne!(root1, root2, "guardian never reset the frozen agent");
    assert!(
        !alive(root1),
        "the frozen agent survived the guardian reset"
    );
    assert_eq!(
        fixture.cli_node("guardian")["pid"].as_i64().unwrap(),
        guardian1,
        "the guardian itself should not have been touched"
    );

    kill(root1, "-CONT");
    std::thread::sleep(Duration::from_millis(500));
    let status = fixture.cli_status_result();
    assert!(
        status.is_ok(),
        "status broke after SIGCONT to a dead, already-replaced pid"
    );
    let status = status.unwrap();
    let root = status["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["env"] == "root")
        .cloned()
        .unwrap();
    assert_eq!(
        root["pid"].as_i64().unwrap(),
        root2,
        "SIGCONT on the old pid confused base's view of root"
    );
    assert_eq!(root["registered"], true);
}
