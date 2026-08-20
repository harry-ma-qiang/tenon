mod gate;

use gate::{fixture, repo, skip, Fixture};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tenon_harness::fake::{self, Fake, Say};

const NAME: &str = "mcp-gate";

/// A ~35-line stdlib MCP server over stdio: newline-delimited JSON-RPC 2.0 with
/// one `echo` tool. What our client mounts and forwards `tools/call` to.
const ECHO_PY: &str = r#"import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id"); method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"echo","version":"1"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"echo","description":"echo text back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}})
    elif method == "tools/call":
        args = (msg.get("params") or {}).get("arguments") or {}
        send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":args.get("text","")}],"isError":False}})
    elif method and method.startswith("notifications/"):
        pass
    elif mid is not None:
        send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"no"}})
"#;

/// A guard plugin that denies the bridged MCP tool through the `tools/pre-execute`
/// waterfall, proving guard/budget/approval hooks apply to bridged tools.
const GUARD: &str = r#"import json
from tenon import Plugin
plugin = Plugin(inject=[])
REASON = "blocked by mcp guard"

@plugin.on_load
def load(config):
    plugin.provide("guard", {"reason": lambda: REASON})
    plugin.log("mcp guard active")

@plugin.on("tools/pre-execute", mode="call", prepend=True, arity=1)
def pre_execute(args, next):
    call = args[0] if args else {}
    if "mcp/echoserver/echo" in json.dumps(call):
        return {"deny": REASON}
    return next([call])

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
    std::fs::copy(&sdk, workspace.join("tenon.py")).expect("copy sdk");
    std::fs::write(workspace.join(file), body).expect("write plugin");
    let launched = fixture
        .rpc(
            "sandbox.exec",
            json!({
                "env": "root", "cmd": "sh",
                "args": ["-c", format!(
                    "nohup python3 /workspace/{file} >/workspace/{file}.log 2>&1 </dev/null & echo started"
                )],
                "timeout": 10_000,
            }),
        )
        .await
        .expect("sandbox.exec");
    assert_eq!(launched["status"], 0, "{launched}");
}

async fn wait_for_service(fixture: &Fixture, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if fixture
            .rpc(
                "svc",
                json!({"env": "root", "name": name, "method": "reason", "args": []}),
            )
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    panic!("service {name} never registered\n{}", fixture.log());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mounted_mcp_tool_is_callable_and_a_guard_hook_denies_it() {
    let Some(release) = skip(NAME) else {
        return;
    };
    let server: Fake = fake::spawn(vec![
        Say::Tool("mcp/echoserver/echo".to_string(), json!({"text": "hi"})),
        Say::Text("done".to_string()),
    ])
    .await
    .expect("fake model");
    let fixture = fixture(NAME, release, "sandbox: oci\n", &harness(&server.base_url));
    fixture.start();
    fixture.ready(Duration::from_secs(120)).await;
    fixture.worker_ready("root", Duration::from_secs(120)).await;

    // The echo MCP server script lives on the host; the harness spawns it.
    let echo_path = fixture.home.join("mcp_echo.py");
    std::fs::write(&echo_path, ECHO_PY).expect("write echo server");

    // A pre-execute guard denies the bridged tool.
    launch_in_sandbox(&fixture, "guard.py", GUARD).await;
    wait_for_service(&fixture, "guard").await;

    // Mount the external MCP server into our tools bus.
    let mounted = fixture
        .rpc(
            "svc",
            json!({
                "env": "root", "name": "mcp", "method": "mount",
                "args": [{"name": "echoserver", "cmd": "python3",
                          "args": [echo_path.display().to_string()]}],
            }),
        )
        .await
        .expect("mcp.mount");
    assert_eq!(mounted["ok"], true, "{mounted}");

    // The bridged tool is registered under single authority.
    let listed = fixture
        .rpc(
            "svc",
            json!({"env": "root", "name": "tools", "method": "list", "args": [{}]}),
        )
        .await
        .expect("tools.list");
    let has = listed["tools"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .any(|row| row["name"] == json!("mcp/echoserver/echo"))
        })
        .unwrap_or(false);
    assert!(has, "bridged tool not registered: {listed}");

    // The model calls it through our loop; the guard denies it with the reason.
    let (_ok, out, err) = fixture.run(&["run", "echo hi through mcp", "--timeout", "120"]);
    let denied = fixture
        .of_kind("tool/result")
        .await
        .into_iter()
        .any(|data| {
            data["name"] == json!("mcp/echoserver/echo")
                && data["denied"] == json!(true)
                && data["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("blocked by mcp guard")
        });
    assert!(
        denied,
        "bridged tool was not denied\n{out}{err}\n{}",
        fixture.log()
    );

    fixture.run(&["stop"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tenon_mcp_server_exposes_the_tools_bus_over_stdio() {
    let Some(release) = skip(NAME) else {
        return;
    };
    let server: Fake = fake::spawn(vec![]).await.expect("fake model");
    let fixture = fixture(
        "mcp-server-gate",
        release,
        "sandbox: oci\n",
        &harness(&server.base_url),
    );
    fixture.start();
    fixture.ready(Duration::from_secs(120)).await;
    fixture.worker_ready("root", Duration::from_secs(120)).await;

    let mut child = Command::new(env!("CARGO_BIN_EXE_tenon"))
        .arg("--home")
        .arg(&fixture.home)
        .args(["mcp", "--env", "root"])
        .env("TENON_RELEASE_DIR", &fixture.release)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn tenon mcp");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();

    let mut request = |value: Value| {
        writeln!(stdin, "{value}").expect("write request");
        stdin.flush().expect("flush");
        let line = lines.next().expect("a line").expect("read line");
        serde_json::from_str::<Value>(&line).expect("parse response")
    };

    let init = request(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}));
    assert_eq!(
        init["result"]["protocolVersion"],
        json!("2024-11-05"),
        "{init}"
    );

    let listed = request(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}));
    let names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        names.iter().any(|name| name == "bash"),
        "no bash tool: {names:?}"
    );

    // tools/call bash runs in root's sandbox (env-scoped by construction: the
    // stdio server names env=root and routes through that env's tools bus).
    let called = request(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "bash", "arguments": {"cmd": "echo mcp-server-ok"}},
    }));
    let text = called["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("mcp-server-ok"),
        "bash did not run in the sandbox: {called}"
    );

    let _ = child.kill();
    let _ = child.wait();
    fixture.run(&["stop"]);
}
