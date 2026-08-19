mod gate;

use gate::{fixture, repo, skip, Fixture};
use serde_json::json;
use std::time::{Duration, Instant};
use tenon_harness::fake::{self, Fake, Say};

const NAME: &str = "harness-gate";

const GUARD: &str = r#"
import json
from tenon import Plugin

plugin = Plugin(inject=[])
REASON = "blocked by the sandbox guard"

@plugin.on_load
def load(config):
    plugin.provide("guard", {"reason": lambda: REASON})
    plugin.log("guard: active")

@plugin.on("tools/pre-execute", mode="call", prepend=True, arity=1)
def pre_execute(args, next):
    call = args[0] if args else {}
    if "rm -rf" in json.dumps(call):
        return {"deny": REASON}
    return next([call])

plugin.run()
"#;

const PROBE: &str = r#"
from tenon import Plugin
plugin = Plugin(inject=[])

@plugin.on_load
def load(config):
    plugin.provide("probe", {"ping": lambda: "pong"})

plugin.run()
"#;

fn harness(base_url: &str) -> String {
    format!(
        "llm:\n  provider: openai\n  base_url: {base_url}\n  model: fake-model\n  \
         api_key_env: TENON_TEST_NO_KEY\n  retry_base_ms: 20\nmax_steps: 4\napproval: deny\n"
    )
}

async fn launch_in_sandbox(fixture: &Fixture, file: &str, body: &str) {
    let workspace = fixture.workspace();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !workspace.is_dir() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let sdk = repo().join("sdk/py/tenon.py");
    std::fs::copy(&sdk, workspace.join("tenon.py")).expect("copy sdk/py/tenon.py");
    std::fs::write(workspace.join(file), body).expect("write the plugin");
    let launched = fixture
        .rpc(
            "sandbox.exec",
            json!({
                "env": "root",
                "cmd": "sh",
                "args": [
                    "-c",
                    format!(
                        "nohup python3 /workspace/{file} >/workspace/{file}.log 2>&1 \
                         </dev/null & echo started"
                    ),
                ],
                "timeout": 10_000,
            }),
        )
        .await
        .expect("sandbox.exec");
    assert_eq!(launched["status"], 0, "{launched}");
}

async fn wait_for_service(fixture: &Fixture, name: &str, method: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let answer = fixture
            .rpc(
                "svc",
                json!({"env": "root", "name": name, "method": method, "args": []}),
            )
            .await;
        if answer.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    panic!("the service {name} never registered\n{}", fixture.log());
}

fn ask(fixture: &Fixture, task: &str) -> (bool, String, String) {
    fixture.run(&["run", task, "--timeout", "120"])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_harness_runs_a_turn_a_tool_call_a_denial_and_resumes_after_a_restart() {
    let Some(release) = skip(NAME) else {
        return;
    };
    let server: Fake = fake::spawn(vec![Say::Text("pong".to_string())])
        .await
        .expect("fake model");
    let fixture = fixture(NAME, release, "sandbox: oci\n", &harness(&server.base_url));
    fixture.start();
    let root = fixture.ready(Duration::from_secs(120)).await;
    assert_eq!(root["sandbox"]["backend"], "oci", "{root}");
    assert!(root["harness"]["pid"].as_i64().unwrap_or(0) > 0, "{root}");

    // a. one model turn, streamed out of the session log
    let (ok, out, err) = ask(&fixture, "reply with the single word pong");
    assert!(ok, "tenon run failed: {out}{err}\n{}", fixture.log());
    assert!(out.contains("pong"), "no answer in {out:?} {err:?}");
    let kinds: Vec<String> = fixture
        .events()
        .await
        .iter()
        .map(|event| event["kind"].as_str().unwrap_or_default().to_string())
        .collect();
    for wanted in [
        "harness/ready",
        "session/created",
        "user/message",
        "turn/start",
        "step/start",
        "assistant/chunk",
        "assistant/message",
        "turn/end",
    ] {
        assert!(
            kinds.contains(&wanted.to_string()),
            "{wanted} not in {kinds:?}"
        );
    }
    let ended = fixture.of_kind("turn/end").await;
    assert_eq!(ended[0]["ok"], true, "{:?}", ended[0]);
    let first_session = ended[0]["session"].as_str().unwrap().to_string();
    assert_eq!(
        server.requests()[0]["model"],
        json!("fake-model"),
        "the profile's provider config never reached the harness"
    );

    // b. a tool call, executed by the worker inside the sandbox
    server.say(vec![
        Say::Tool("bash".to_string(), json!({"cmd": "echo tenon-ok"})),
        Say::Text("the output was tenon-ok".to_string()),
    ]);
    let (ok, out, err) = ask(&fixture, "run echo tenon-ok with bash");
    assert!(ok, "tenon run failed: {out}{err}");
    let calls = fixture.of_kind("tool/call").await;
    assert_eq!(calls.last().unwrap()["name"], "bash", "{calls:?}");
    let results = fixture.of_kind("tool/result").await;
    let result = results.last().unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert!(
        result["text"]
            .as_str()
            .unwrap_or_default()
            .contains("tenon-ok"),
        "the sandbox worker did not run it: {result}"
    );

    // c. the agent mounts a plugin through its own tools and sees it in the tree
    let probe = fixture.workspace().join("probe_plugin.py");
    std::fs::write(&probe, PROBE).unwrap();
    let mounted = fixture
        .rpc(
            "svc",
            json!({
                "env": "root",
                "name": "tools",
                "method": "execute",
                "args": [{
                    "name": "plugin",
                    "args": {
                        "op": "mount",
                        "id": "probe",
                        "spec": {
                            "cmd": which("python3"),
                            "args": [probe],
                            "env": [["PYTHONPATH", repo().join("sdk/py")]],
                        },
                    },
                }],
            }),
        )
        .await
        .expect("plugin mount");
    assert_eq!(mounted["ok"], true, "{mounted}");
    assert_eq!(mounted["result"]["status"], "active", "{mounted}");
    let tree = fixture.node("root").await["tree"].to_string();
    assert!(
        tree.contains("probe"),
        "the mounted plugin is not in the tree"
    );
    let pinged = fixture
        .rpc(
            "svc",
            json!({"env": "root", "name": "probe", "method": "ping", "args": []}),
        )
        .await
        .expect("svc probe.ping");
    assert_eq!(pinged, "pong");

    // d. a python guard mounted through the gateway denies the call
    launch_in_sandbox(&fixture, "guard_plugin.py", GUARD).await;
    wait_for_service(&fixture, "guard", "reason").await;
    server.say(vec![
        Say::Tool("bash".to_string(), json!({"cmd": "rm -rf /workspace/keep"})),
        Say::Text("that was blocked".to_string()),
    ]);
    let (ok, out, err) = ask(&fixture, "delete the workspace with rm -rf");
    assert!(ok, "tenon run failed: {out}{err}");
    let results = fixture.of_kind("tool/result").await;
    let denied = results.last().unwrap();
    assert_eq!(denied["denied"], true, "{denied}");
    assert_eq!(denied["text"], "blocked by the sandbox guard", "{denied}");
    assert!(err.contains("denied"), "tenon run did not report it: {err}");

    // e. a restarted harness resumes the session from the log
    let pid = fixture.node("root").await["harness"]["pid"]
        .as_i64()
        .expect("harness pid") as i32;
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut fresh = pid;
    while Instant::now() < deadline {
        let node = fixture.node("root").await;
        let now = node["harness"]["pid"].as_i64().unwrap_or(0) as i32;
        if node["harness"]["state"] == "ready" && now != pid && now > 0 {
            fresh = now;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_ne!(
        fresh,
        pid,
        "base never restarted the harness\n{}",
        fixture.log()
    );
    let resumed = fixture
        .rpc(
            "session.resume",
            json!({"env": "root", "session_id": first_session}),
        )
        .await
        .expect("session.resume");
    assert!(
        resumed["messages"].as_i64().unwrap_or(0) >= 2,
        "the fresh harness rebuilt nothing: {resumed}"
    );
    server.say(vec![Say::Text("still here".to_string())]);
    let before = server.requests().len();
    let prompted = fixture
        .rpc(
            "session.prompt",
            json!({"env": "root", "session_id": first_session, "text": "and now"}),
        )
        .await
        .expect("session.prompt");
    assert_eq!(prompted["ok"], true, "{prompted}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && server.requests().len() == before {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let sent = server
        .requests()
        .last()
        .cloned()
        .expect("a request")
        .to_string();
    assert!(
        sent.contains("reply with the single word pong"),
        "the resumed context lost the first turn: {sent}"
    );
}

fn which(name: &str) -> String {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
        })
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| name.to_string())
}
