#![cfg(feature = "http")]

mod gate;

use gate::{repo, skip, Fixture, Spec, BIN};
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const NAME: &str = "ingress-gate";
const TOKEN: &str = "ingress-gate-token";
const APP: &str = "hello-app";

/// A ~30-line stdlib app: it serves `GET /hello` and `POST /echo` on its ingress
/// port, registers the route through the gateway's `link` service, and records
/// the outcome to `register.json` so a second env's rejection is observable.
const APP_PY: &str = r#"import json, os, sys, threading
sys.path.insert(0, "/workspace")
from http.server import BaseHTTPRequestHandler, HTTPServer
from tenon import Plugin

NAME = os.environ.get("TENON_APP_NAME", "hello-app")
PORT = int(os.environ.get("TENON_INGRESS_PORTS", "18080").split(",")[0])
state = {"env": "?"}

class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def _send(self, code, body):
        data = body.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        if self.path == "/hello":
            self._send(200, "hi from %s" % state["env"])
        else:
            self._send(404, "no")
    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0"))
        posted = self.rfile.read(n).decode("utf-8", "replace")
        self._send(200, json.dumps({
            "app": self.headers.get("X-Tenon-App", ""),
            "env": self.headers.get("X-Tenon-Env", ""),
            "body": posted,
        }))

def serve():
    HTTPServer(("0.0.0.0", PORT), H).serve_forever()

plugin = Plugin(inject=[])

@plugin.on_load
def load(config):
    with open("/workspace/app.pid", "w") as f:
        f.write(str(os.getpid()))
    threading.Thread(target=serve, daemon=True).start()
    try:
        res = plugin.svc("link", "request", ["ingress.register", {"name": NAME, "port": PORT, "public": False}])
        if isinstance(res, list) and len(res) == 2 and res[0] in ("ok", "error"):
            if res[0] == "error":
                raise Exception(res[1] if isinstance(res[1], str) else json.dumps(res[1]))
            res = res[1]
        state["env"] = res.get("env", "?")
        out = {"ok": True, "res": res}
    except Exception as exc:
        out = {"ok": False, "error": str(exc)}
    with open("/workspace/register.json", "w") as f:
        f.write(json.dumps(out))
    return "ok"

plugin.run()
"#;

fn workspace(fixture: &Fixture, env: &str) -> std::path::PathBuf {
    fixture.home.join(format!("envs/{env}/workspace"))
}

/// Drops the SDK and the app into an env's workspace and launches it detached
/// inside that env's sandbox through the worker's `bash`.
async fn launch_app(fixture: &Fixture, env: &str) {
    let ws = workspace(fixture, env);
    std::fs::create_dir_all(&ws).expect("workspace");
    let sdk = repo().join("sdk/py/tenon.py");
    std::fs::copy(&sdk, ws.join("tenon.py")).expect("copy sdk");
    std::fs::write(ws.join("app.py"), APP_PY).expect("write app");
    fixture
        .tool(
            env,
            "bash",
            json!({
                "cmd": "nohup python3 /workspace/app.py > /workspace/app.log 2>&1 &",
                "pty": false,
                "timeout_ms": 8000,
            }),
        )
        .await
        .expect("launch app");
}

fn register_json(fixture: &Fixture, env: &str, limit: Duration) -> serde_json::Value {
    let path = workspace(fixture, env).join("register.json");
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Ok(body) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
                return value;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    panic!("no register.json for {env}\n{}", fixture.log());
}

/// Spawns `tenon serve --https`, returning the child and the bound URL.
fn serve(fixture: &Fixture) -> (Child, String) {
    let mut child = fixture.spawn(&[
        "serve",
        "--https",
        "--http",
        "127.0.0.1:0",
        "--auth-token",
        TOKEN,
    ]);
    let stdout = child.stdout.take().expect("serve stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    for _ in 0..20 {
        line.clear();
        if reader.read_line(&mut line).expect("read serve stdout") == 0 {
            break;
        }
        if let Some(index) = line.find("://") {
            return (child, line[index + 3..].trim().to_string());
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("serve never printed its address");
}

fn curl(url: &str, method: &str, token: Option<&str>, form: Option<&str>) -> (u16, String) {
    let mut command = Command::new("curl");
    command.arg("-sk").arg("-X").arg(method);
    command.arg("-w").arg("\n%{http_code}");
    if let Some(token) = token {
        command
            .arg("-H")
            .arg(format!("Authorization: Bearer {token}"));
    }
    if let Some(form) = form {
        command.arg("--data").arg(form);
    }
    let output = command.arg(url).output().expect("run curl");
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, "0"));
    (code.trim().parse().unwrap_or(0), body.to_string())
}

async fn route_named(fixture: &Fixture, name: &str, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Ok(list) = fixture.rpc("ingress.list", json!({})).await {
            if list["routes"]
                .as_array()
                .map(|rows| rows.iter().any(|row| row["name"] == name))
                .unwrap_or(false)
            {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_in_sandbox_app_is_reachable_through_the_proxy_and_its_route_expires() {
    let Some(release) = skip(NAME) else {
        return;
    };
    if !Path::new(&repo().join("sdk/py/tenon.py")).is_file() {
        println!("skipping {NAME}: no python sdk");
        return;
    }
    let fixture = Fixture::open(
        BIN,
        release,
        Spec {
            name: NAME,
            config: Some(
                "sandbox: oci\napproval:\n  spawn_soft_limit: 0\ningress:\n  probe_ms: 500\n",
            ),
            harness: None,
            reap_pids: true,
            lock: true,
            limit: Some(Duration::from_secs(120)),
        },
    );
    fixture.start();
    fixture.worker_ready("root", Duration::from_secs(120)).await;

    launch_app(&fixture, "root").await;
    let registered = register_json(&fixture, "root", Duration::from_secs(60));
    assert_eq!(
        registered["ok"], true,
        "root app did not register: {registered}"
    );
    assert!(
        route_named(&fixture, APP, Duration::from_secs(30)).await,
        "route never appeared in ingress.list\n{}",
        fixture.log()
    );

    let (mut serve_child, url) = serve(&fixture);
    let base = format!("https://{url}");

    // The token renders the app's own body through the proxy.
    let (code, body) = curl(&format!("{base}/app/{APP}/hello"), "GET", Some(TOKEN), None);
    assert_eq!(code, 200, "authenticated /hello should be 200: {body}");
    assert_eq!(body, "hi from root", "unexpected app body: {body}");

    // POST /echo proves the X-Tenon-* headers reached the app.
    let (code, body) = curl(
        &format!("{base}/app/{APP}/echo"),
        "POST",
        Some(TOKEN),
        Some("ping"),
    );
    assert_eq!(code, 200, "echo should be 200: {body}");
    assert!(body.contains(APP), "X-Tenon-App missing at the app: {body}");
    assert!(
        body.contains("\"env\": \"root\""),
        "X-Tenon-Env missing at the app: {body}"
    );

    // No token is 401 (the app is not public).
    let (code, _body) = curl(&format!("{base}/app/{APP}/hello"), "GET", None, None);
    assert_eq!(code, 401, "unauthenticated /app should be 401");

    // A second env cannot claim a name another env owns.
    let child = fixture
        .rpc("runtime.spawn", json!({"parent": "root", "overrides": {}}))
        .await
        .expect("runtime.spawn");
    let child_env = child["env"].as_str().expect("child env").to_string();
    fixture
        .worker_ready(&child_env, Duration::from_secs(120))
        .await;
    launch_app(&fixture, &child_env).await;
    let rejected = register_json(&fixture, &child_env, Duration::from_secs(60));
    assert_eq!(
        rejected["ok"], false,
        "the second env was allowed to steal the name: {rejected}"
    );
    assert!(
        rejected["error"]
            .as_str()
            .unwrap_or_default()
            .contains("owned by env root"),
        "unexpected rejection reason: {rejected}"
    );

    // Killing the app expires its route: the proxy then 404s or 502s.
    fixture
        .tool(
            "root",
            "bash",
            json!({"cmd": "kill -9 $(cat /workspace/app.pid)", "pty": false, "timeout_ms": 5000}),
        )
        .await
        .expect("kill app");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = 0;
    while Instant::now() < deadline {
        let (code, _body) = curl(&format!("{base}/app/{APP}/hello"), "GET", Some(TOKEN), None);
        last = code;
        if code == 404 || code == 502 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        last == 404 || last == 502,
        "the route did not expire after the app was killed: {last}\n{}",
        fixture.log()
    );

    let _ = serve_child.kill();
    let _ = serve_child.wait();
}
