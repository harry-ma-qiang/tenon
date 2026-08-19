mod gate;

use gate::{fixture, skip};
use serde_json::json;
use std::time::Duration;
use tenon_harness::fake::{self, Say};

const NAME: &str = "budget-gate";

fn harness(base_url: &str) -> String {
    format!(
        "llm:\n  provider: openai\n  base_url: {base_url}\n  model: fake-model\n  \
         api_key_env: TENON_TEST_NO_KEY\n  retry_base_ms: 20\nmax_steps: 2\napproval: deny\n\
         budgets:\n  tokens: 25\nusd_per_1k:\n  input: 0.001\n  output: 0.002\n"
    )
}

/// Hard rules v1: a token budget is a hard stop, not a warning. One turn costs
/// more than the limit, the harness is halted, every further prompt is refused
/// with the reason, and `tenon reset` is the way back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_budget_halts_the_env_and_reset_clears_it() {
    let Some(release) = skip(NAME) else { return };
    let server = fake::spawn(vec![Say::Text("pong".to_string())])
        .await
        .expect("fake model");
    let fixture = fixture(NAME, release, "sandbox: oci\n", &harness(&server.base_url));
    fixture.start();
    fixture.ready(Duration::from_secs(120)).await;

    // a. one turn costs 18 tokens and stays under the limit of 25
    let (ok, out, err) = fixture.run(&["run", "say pong", "--timeout", "120"]);
    assert!(ok, "the first turn failed: {out}{err}\n{}", fixture.log());
    assert!(out.contains("pong"), "{out}");
    let node = fixture.node("root").await;
    assert_eq!(node["budget"]["tokens"], 18, "{node}");
    assert!(node["budget"]["halted"].is_null(), "{node}");

    // b. the second turn crosses it: the env is halted mid-turn
    server.say(vec![Say::Text("pong".to_string())]);
    let (_ok, out, err) = fixture.run(&["run", "say pong again", "--timeout", "120"]);
    let breached = fixture
        .await_status(Duration::from_secs(30), |status| {
            status["nodes"]
                .as_array()
                .map(|nodes| {
                    nodes
                        .iter()
                        .any(|node| node["env"] == "root" && !node["budget"]["halted"].is_null())
                })
                .unwrap_or(false)
        })
        .await;
    assert!(
        breached,
        "the budget never halted root: {out}{err}\n{}",
        fixture.log()
    );
    let exceeded = fixture.of_kind("budget.exceeded").await;
    assert_eq!(exceeded[0]["budget"], "tokens", "{exceeded:?}");
    assert_eq!(exceeded[0]["limit"], 25, "{exceeded:?}");
    let node = fixture.node("root").await;
    assert!(
        node["budget"]["tokens"].as_i64().unwrap_or(0) >= 25,
        "{node}"
    );
    assert!(
        node["budget"]["usd"].as_f64().unwrap_or(0.0) > 0.0,
        "{node}"
    );

    // c. the next prompt is refused with the reason, the harness stays down
    let (ok, out, err) = fixture.run(&["run", "a third time", "--timeout", "30"]);
    assert!(!ok, "a halted env still ran a turn: {out}{err}");
    assert!(
        err.contains("halted") && err.contains("budget tokens"),
        "no reason in {err:?}"
    );

    // d. reset clears the counters and the env runs again
    let (ok, out, err) = fixture.run(&["reset", "--env", "root"]);
    assert!(ok, "reset failed: {out}{err}");
    fixture.ready(Duration::from_secs(120)).await;
    let node = fixture.node("root").await;
    assert!(node["budget"]["halted"].is_null(), "{node}");
    assert_eq!(node["budget"]["tokens"], 0, "{node}");
    server.say(vec![Say::Text("pong".to_string())]);
    let (ok, out, err) = fixture.run(&["run", "say pong once more", "--timeout", "120"]);
    assert!(ok, "the env never came back: {out}{err}\n{}", fixture.log());
    assert!(out.contains("pong"), "{out}");
}

/// The kill switch's file carrier: `<home>/run/STOP` halts every harness and
/// refuses every prompt until it is removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stop_file_halts_prompts_and_removing_it_resumes() {
    let Some(release) = skip(NAME) else { return };
    let server = fake::spawn(vec![]).await.expect("fake model");
    let fixture = fixture(
        "stop-file",
        release,
        "sandbox: oci\n",
        &harness(&server.base_url).replace("budgets:\n  tokens: 25\n", ""),
    );
    fixture.start();
    fixture.ready(Duration::from_secs(120)).await;

    let stop = fixture.home.join("run/STOP");
    std::fs::write(&stop, "human said stop\n").expect("write STOP");
    let killed = fixture
        .await_status(Duration::from_secs(20), |status| {
            !status["killed"].is_null()
        })
        .await;
    assert!(
        killed,
        "the STOP file never reached base\n{}",
        fixture.log()
    );
    let (ok, out, err) = fixture.run(&["run", "anything", "--timeout", "20"]);
    assert!(!ok, "a killed base still ran a turn: {out}{err}");
    assert!(err.contains("kill switch"), "no reason in {err:?}");
    let switched = fixture.rpc("status", json!({})).await.expect("status")["killed"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(switched.contains("STOP"), "{switched}");

    std::fs::remove_file(&stop).expect("remove STOP");
    let back = fixture
        .await_status(Duration::from_secs(30), |status| status["killed"].is_null())
        .await;
    assert!(back, "removing STOP did not resume base\n{}", fixture.log());
    fixture.ready(Duration::from_secs(120)).await;
    server.say(vec![Say::Text("back".to_string())]);
    let (ok, out, err) = fixture.run(&["run", "say back", "--timeout", "120"]);
    assert!(ok, "the env never came back: {out}{err}\n{}", fixture.log());
    assert!(out.contains("back"), "{out}");
}
