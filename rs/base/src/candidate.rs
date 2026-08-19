use crate::drive::{Answer, Drive};
use crate::rpc::Cmd;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(300);
const CANARY_SUFFIX: &str = "-canary";
const WORKER: &str = "worker";

fn canary_id(id: &str) -> String {
    format!("{id}{CANARY_SUFFIX}")
}

fn plugin_id(drive: &Drive) -> String {
    match drive.text("id").is_empty() {
        true => drive.text("name"),
        false => drive.text("id"),
    }
}

/// What is mounted now, so a rollback knows what to put back and a promote
/// knows what to take out. The fiber the canary replaces is found by name and,
/// when the artifact names a service, by who provides it.
pub async fn plugin_snapshot(drive: &Drive) -> Answer {
    let id = plugin_id(drive);
    if id.is_empty() {
        return Err("an upgrade of a plugin needs artifact.name or artifact.id".to_string());
    }
    if !drive.artifact["spec"].is_object() {
        return Err("an upgrade of a plugin needs artifact.spec".to_string());
    }
    let list = drive.plugin("list", json!({})).await?;
    let mounted = list["plugins"]
        .as_array()
        .map(|rows| rows.iter().any(|row| row["id"] == json!(id)))
        .unwrap_or(false);
    let service = drive.text("service");
    let owner = match service.is_empty() {
        true => None,
        false => drive.owner(&service).await,
    };
    Ok(json!({
        "target": "plugin",
        "id": id,
        "mounted": mounted,
        "service": service,
        "owner": owner,
        "plugins": list["plugins"],
    }))
}

/// Mounted beside the old one, never over it: the kernel is the single
/// authority over service names, so a canary names its own service by the
/// `TENON_CANARY_SERVICE` its spec is handed, and the old plugin keeps
/// answering under the real name until the promotion.
pub async fn plugin_canary(drive: &Drive) -> Answer {
    let id = canary_id(&plugin_id(drive));
    let service = drive.text("service");
    let spec = canary_spec(drive, &id, &service);
    let mounted = drive
        .plugin("mount", json!({"plugin_id": id, "spec": spec}))
        .await?;
    if mounted["status"] != json!("active") {
        return Err(format!(
            "the canary plugin did not become active: {}",
            mounted["status"]
        ));
    }
    let check = selfcheck(drive, &service).await?;
    Ok(json!({"id": id, "status": mounted["status"], "selfcheck": check}))
}

fn canary_spec(drive: &Drive, id: &str, service: &str) -> Value {
    let mut spec = drive.artifact["spec"].clone();
    let mut env = spec["env"].as_array().cloned().unwrap_or_default();
    env.push(json!(["TENON_CANARY", id]));
    if !service.is_empty() {
        env.push(json!([
            "TENON_CANARY_SERVICE",
            format!("{service}{CANARY_SUFFIX}")
        ]));
    }
    if let Some(object) = spec.as_object_mut() {
        object.insert("env".to_string(), Value::Array(env));
    }
    spec
}

/// The conformance of a plugin candidate: the wire handshake happened (the
/// fiber is active) and, when the artifact declares one, its `selfcheck`
/// method answers. A declared check that fails is a rollback with its reason.
async fn selfcheck(drive: &Drive, service: &str) -> Answer {
    let declared = drive.artifact.get("selfcheck").cloned();
    if service.is_empty() || declared.is_none() {
        return Ok(json!({"ran": false, "reason": "the artifact declares no selfcheck"}));
    }
    let declared = declared.unwrap_or(json!({}));
    let method = declared
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("selfcheck")
        .to_string();
    let name = format!("{service}{CANARY_SUFFIX}");
    let answer = drive
        .svc_args(&name, &method, vec![])
        .await
        .map_err(|error| format!("selfcheck {name}.{method}: {error}"))?;
    if let Some(wanted) = declared.get("expect") {
        let same =
            &answer == wanted || answer.as_str().map(str::to_string) == Some(wanted.to_string());
        if !same {
            return Err(format!(
                "selfcheck {name}.{method} answered {answer}, not {wanted}"
            ));
        }
    }
    Ok(json!({"ran": true, "method": method, "answer": answer}))
}

/// The swap: the canary goes, the old fiber goes, the artifact is mounted
/// under the real id, and the LKG manifest is rewritten over what is installed
/// now. Nothing here is reversible, which is why it runs last.
pub async fn plugin_promote(drive: &Drive, snapshot: &Value) -> Answer {
    let id = plugin_id(drive);
    let canary = canary_id(&id);
    let _ = drive.plugin("unmount", json!({"plugin_id": canary})).await;
    let old = snapshot["owner"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| id.clone());
    if snapshot["mounted"] == json!(true) || snapshot["owner"].is_string() {
        let _ = drive.plugin("unmount", json!({"plugin_id": old})).await;
    }
    let mounted = drive
        .plugin(
            "mount",
            json!({"plugin_id": id, "spec": drive.artifact["spec"]}),
        )
        .await?;
    if mounted["status"] != json!("active") {
        return Err(format!(
            "the promoted plugin did not become active: {}",
            mounted["status"]
        ));
    }
    let manifest = install(drive)?;
    Ok(json!({"id": id, "replaced": old, "manifest": manifest}))
}

/// The installed plugin version, written where the loader resolves names and
/// where the LKG manifest pins hashes.
fn install(drive: &Drive) -> Answer {
    let name = drive.text("name");
    if name.is_empty() {
        return Ok(json!({"installed": false}));
    }
    let version = match drive.text("version").is_empty() {
        true => format!("0.0.{}", drive.id),
        false => drive.text("version"),
    };
    let cmd = drive.artifact["spec"]["cmd"].as_str().unwrap_or_default();
    let hash = crate::manifest::file_hash(std::path::Path::new(cmd))
        .unwrap_or_else(|| crate::manifest::tree_hash(std::path::Path::new(".")));
    let dir = drive.home.plugins_dir().join(format!("{name}@{version}"));
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let manifest = json!({
        "name": name,
        "version": version,
        "hash": hash,
        "cmd": cmd,
        "args": drive.artifact["spec"]["args"],
        "protocol": "wire/1",
    });
    std::fs::write(
        dir.join(crate::manifest::FILE),
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    )
    .map_err(|error| error.to_string())?;
    let lkg =
        crate::manifest::write(&drive.home, &drive.release).map_err(|error| error.to_string())?;
    Ok(json!({"installed": true, "name": name, "version": version, "lkg": lkg}))
}

pub async fn plugin_rollback(drive: &Drive) -> Value {
    let canary = canary_id(&plugin_id(drive));
    let _ = drive.plugin("unmount", json!({"plugin_id": canary})).await;
    json!({"unmounted": canary})
}

pub async fn worker_snapshot(drive: &Drive) -> Answer {
    if drive.text("cmd").is_empty() {
        return Err("an upgrade of the worker needs artifact.cmd".to_string());
    }
    if drive.instance.is_none() {
        return Err(format!("env {} has no sandbox instance", drive.env));
    }
    let info = drive.svc(WORKER, "info", json!({})).await.ok();
    let spec = std::fs::read_to_string(drive.home.worker_spec_file(&drive.env)).ok();
    Ok(json!({
        "target": "worker",
        "info": info,
        "spec": spec.and_then(|body| serde_json::from_str::<Value>(&body).ok()),
        "builtin": true,
    }))
}

/// The candidate worker runs beside the built-in one, providing
/// `worker-canary`: the built-in keeps the agent's hands working while the
/// conformance calls run against the candidate.
pub async fn worker_canary(drive: &Drive) -> Answer {
    let service = format!("{WORKER}{CANARY_SUFFIX}");
    launch(drive, &service).await?;
    ready(drive, &service).await?;
    let conformance = conformance(drive, &service).await?;
    Ok(json!({"service": service, "conformance": conformance}))
}

async fn launch(drive: &Drive, service: &str) -> Answer {
    let Some(instance) = drive.instance.clone() else {
        return Err(format!("env {} has no sandbox instance", drive.env));
    };
    let workspace = instance.workspace_path();
    let mut line = format!("cd {workspace} && TENON_GATEWAY={} ", drive.gateway);
    line.push_str(&format!(
        "TENON_ENV={} TENON_WORKSPACE={workspace} TENON_WORKER_SERVICE={service} ",
        drive.env
    ));
    for pair in drive.artifact["env"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let name = pair[0].as_str().unwrap_or_default();
        let value = pair[1].as_str().unwrap_or_default();
        if !name.is_empty() {
            line.push_str(&format!("{name}={value} "));
        }
    }
    let args: Vec<String> = drive.artifact["args"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    line.push_str(&format!(
        "nohup {} {} >> {workspace}/.tenon-{service}.log 2>&1 </dev/null & echo started",
        drive.text("cmd"),
        args.join(" ")
    ));
    let outcome = tokio::task::spawn_blocking(move || {
        instance.exec("sh", &["-c".to_string(), line], LAUNCH_TIMEOUT)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    match outcome.status {
        0 => Ok(json!({"launched": true, "service": service})),
        status => Err(format!(
            "the candidate worker exited {status}: {}",
            String::from_utf8_lossy(&outcome.stderr).trim()
        )),
    }
}

/// How long a candidate worker has to answer on the wire: the env's own worker
/// boot timeout, or whatever the artifact asks for. A candidate that never
/// speaks is the failure the built-in fallback exists for.
fn ready_timeout(drive: &Drive) -> Duration {
    match drive.artifact["ready_timeout_ms"].as_u64() {
        Some(ms) if ms > 0 => Duration::from_millis(ms),
        _ => drive.worker_timeout,
    }
}

async fn ready(drive: &Drive, service: &str) -> Answer {
    let deadline = Instant::now() + ready_timeout(drive);
    let mut last = "no answer".to_string();
    while Instant::now() < deadline {
        match drive.svc(service, "info", json!({})).await {
            Ok(info) => return Ok(info),
            Err(error) => last = error,
        }
        tokio::time::sleep(POLL).await;
    }
    Err(format!(
        "the candidate worker never answered {service}.info: {last}"
    ))
}

/// The worker conformance: a shell command, a file round trip and a snapshot
/// commit. Anything wire-speaking that provides `worker` with these methods
/// may replace the built-in one; anything that cannot is rolled back.
async fn conformance(drive: &Drive, service: &str) -> Answer {
    let marker = format!("tenon-canary-{}", drive.id);
    let bash = drive
        .svc(service, "bash", json!({"cmd": format!("echo {marker}")}))
        .await
        .map_err(|error| format!("conformance bash: {error}"))?;
    let tail = bash["tail"].as_str().unwrap_or_default().to_string();
    if bash["status"].as_i64().unwrap_or(-1) != 0 || !tail.contains(&marker) {
        return Err(format!("conformance bash answered {bash}"));
    }
    let path = format!(".tenon-canary-{}.txt", drive.id);
    drive
        .svc(
            service,
            "fs.write",
            json!({"path": path, "content": marker}),
        )
        .await
        .map_err(|error| format!("conformance fs.write: {error}"))?;
    let view = drive
        .svc(service, "fs.view", json!({"path": path}))
        .await
        .map_err(|error| format!("conformance fs.view: {error}"))?;
    if !view["content"]
        .as_str()
        .unwrap_or_default()
        .contains(&marker)
    {
        return Err(format!("conformance fs.view answered {view}"));
    }
    let snap = drive
        .svc(service, "snap.commit", json!({"label": "canary"}))
        .await
        .map_err(|error| format!("conformance snap.commit: {error}"))?;
    Ok(json!({"bash": tail.trim(), "view": path, "snap": snap["ref"]}))
}

/// The candidate takes the name: the old fiber is unmounted (which is what
/// ends the old worker process), and the candidate is started again under
/// `worker`, so the harness's tools bus keeps the target it already has.
pub async fn worker_promote(drive: &Drive, _snapshot: &Value) -> Answer {
    let canary = format!("{WORKER}{CANARY_SUFFIX}");
    if let Some(id) = drive.owner(&canary).await {
        let _ = drive.plugin("unmount", json!({"plugin_id": id})).await;
    }
    if let Some(id) = drive.owner(WORKER).await {
        let _ = drive.plugin("unmount", json!({"plugin_id": id})).await;
    }
    match promote_worker(drive).await {
        Ok(info) => {
            let spec = json!({
                "cmd": drive.text("cmd"),
                "args": drive.artifact["args"],
                "env": drive.artifact["env"],
            });
            let _ = drive.cmds.send(Cmd::UpgradeWorker {
                env: drive.env.clone(),
                spec: Some(spec.clone()),
            });
            Ok(json!({"service": WORKER, "info": info, "spec": spec}))
        }
        Err(reason) => Err(fallback(drive, reason).await),
    }
}

async fn promote_worker(drive: &Drive) -> Answer {
    launch(drive, WORKER).await?;
    ready(drive, WORKER).await
}

/// The built-in worker is the LKG fallback: a candidate that cannot take the
/// name leaves base booting its own again rather than an env with no hands.
async fn fallback(drive: &Drive, reason: String) -> String {
    let _ = drive.cmds.send(Cmd::UpgradeWorker {
        env: drive.env.clone(),
        spec: None,
    });
    let _ = drive.cmds.send(Cmd::WorkerReady {
        env: drive.env.clone(),
        pid: None,
        error: Some(format!("the candidate worker failed: {reason}")),
    });
    let _ = drive.cmds.send(Cmd::WorkerBoot {
        env: drive.env.clone(),
    });
    format!("{reason}; the built-in worker was booted again")
}

pub async fn worker_rollback(drive: &Drive) -> Value {
    let canary = format!("{WORKER}{CANARY_SUFFIX}");
    if let Some(id) = drive.owner(&canary).await {
        let _ = drive.plugin("unmount", json!({"plugin_id": id})).await;
        return json!({"unmounted": id});
    }
    json!({"unmounted": Value::Null})
}

pub fn kernel_snapshot(drive: &Drive) -> Answer {
    let beam = drive.text("beam");
    if beam.is_empty() {
        return Err("an upgrade of the kernel needs artifact.beam".to_string());
    }
    if !PathBuf::from(&beam).is_file() {
        return Err(format!("{beam} is not a file"));
    }
    let shipped = crate::check::shipped_beam(&drive.release).map_err(|error| error.to_string())?;
    Ok(json!({
        "target": "kernel",
        "release": drive.release,
        "beam": shipped,
        "hash": crate::manifest::file_hash(&shipped),
        "candidate_hash": crate::manifest::file_hash(std::path::Path::new(&beam)),
    }))
}

/// The kernel's conformance is the contract suite of `tenon check kernel`,
/// run against the candidate beam in a fresh node. A beam that fails it never
/// reaches a node A.
pub async fn kernel_canary(drive: &Drive) -> Answer {
    let release = drive.release.clone();
    let beam = PathBuf::from(drive.text("beam"));
    let report = tokio::task::spawn_blocking(move || crate::check::run(&release, Some(&beam)))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    match report.ok {
        true => Ok(json!({"contract": report.contract, "passed": report.passed})),
        false => Err(format!("tenon check kernel: {}", report.reason())),
    }
}

pub async fn kernel_promote(drive: &Drive) -> Answer {
    let staged =
        crate::bluegreen::stage(&drive.home, drive.id, &drive.release, &drive.text("beam"))
            .map_err(|error| error.to_string())?;
    let started = drive
        .ask(|reply| Cmd::KernelSwitch {
            id: drive.id,
            env: drive.env.clone(),
            release: staged.clone(),
            reply,
        })
        .await?;
    let green = started["green"].as_str().unwrap_or_default().to_string();
    let outcome = crate::bluegreen::await_green(drive, &green).await;
    drive
        .ask(|reply| Cmd::KernelReady {
            id: drive.id,
            env: drive.env.clone(),
            outcome: outcome.clone(),
            reply,
        })
        .await
}
