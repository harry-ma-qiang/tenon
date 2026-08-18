use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tenon_sandbox::{backend, Policy, Spec};

fn workspace(tag: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("tenon-conformance-{tag}-{suffix}"))
}

fn find_cli() -> Option<&'static str> {
    let path = std::env::var_os("PATH")?;
    ["podman", "docker"]
        .into_iter()
        .find(|name| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

fn check(name: &str) {
    let sandbox = match backend(name) {
        Ok(sandbox) => sandbox,
        Err(reason) => {
            println!("skipping {name}: {reason}");
            return;
        }
    };
    let workspace = workspace(name);
    std::fs::create_dir_all(&workspace).unwrap();
    let spec = Spec {
        env: format!("conf-{name}"),
        image: None,
        workspace: workspace.clone(),
        gateway: None,
        env_passthrough: vec![],
        policy: Policy::default(),
        caps: vec![],
    };
    let instance = sandbox.spawn(&spec).expect("spawn");

    let echo = instance
        .exec(
            "sh",
            &["-c".to_string(), "echo hello-tenon".to_string()],
            Duration::from_secs(10),
        )
        .expect("exec echo");
    assert_eq!(echo.status, 0, "echo failed: {echo:?}");
    assert!(
        String::from_utf8_lossy(&echo.stdout).contains("hello-tenon"),
        "unexpected stdout: {echo:?}"
    );

    let target = if name == "oci" {
        "/workspace/conformance.txt".to_string()
    } else {
        "conformance.txt".to_string()
    };
    let write = instance
        .exec(
            "sh",
            &["-c".to_string(), format!("echo from-inside > {target}")],
            Duration::from_secs(10),
        )
        .expect("exec write");
    assert_eq!(write.status, 0, "write failed: {write:?}");
    let seen = std::fs::read_to_string(workspace.join("conformance.txt"))
        .unwrap_or_else(|error| panic!("host cannot see the written file: {error}"));
    assert!(
        seen.contains("from-inside"),
        "unexpected file body: {seen:?}"
    );

    let slow = instance
        .exec("sleep", &["5".to_string()], Duration::from_secs(1))
        .expect("exec sleep");
    assert!(
        slow.timed_out,
        "sleep 5 with a 1s timeout was not killed: {slow:?}"
    );

    if name == "oci" {
        check_memory_cap(instance.as_ref());
    }
    if name == "landlock" {
        check_read_only_escape(instance.as_ref());
    }

    instance.destroy().expect("destroy");
    if name == "oci" {
        assert_no_leaked_container(&spec.env);
    }
    let _ = std::fs::remove_dir_all(&workspace);
}

fn check_memory_cap(instance: &dyn tenon_sandbox::Instance) {
    let outcome = instance
        .exec(
            "cat",
            &["/sys/fs/cgroup/memory.max".to_string()],
            Duration::from_secs(10),
        )
        .expect("read memory.max");
    let seen = String::from_utf8_lossy(&outcome.stdout);
    let trimmed = seen.trim();
    assert!(
        trimmed != "max" && !trimmed.is_empty(),
        "memory.max was not capped: {trimmed:?}"
    );
    let bytes: u64 = trimmed.parse().expect("memory.max is a number");
    let expected = Policy::default().ram_mb * 1024 * 1024;
    assert_eq!(bytes, expected, "memory cap does not match the policy");
}

fn check_read_only_escape(instance: &dyn tenon_sandbox::Instance) {
    let outcome = instance
        .exec(
            "sh",
            &[
                "-c".to_string(),
                "echo nope > /etc/tenon-landlock-should-fail".to_string(),
            ],
            Duration::from_secs(10),
        )
        .expect("exec forbidden write");
    assert_ne!(
        outcome.status, 0,
        "landlock allowed a write outside the workspace"
    );
    let _ = std::fs::remove_file("/etc/tenon-landlock-should-fail");
}

fn assert_no_leaked_container(env: &str) {
    let Some(cli) = find_cli() else {
        return;
    };
    let output = Command::new(cli)
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label=tenon.env={env}"),
            "--format",
            "{{.ID}}",
        ])
        .output()
        .expect("ps");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.trim().is_empty(),
        "destroy left a container behind: {text}"
    );
}

#[test]
fn oci_backend_conformance() {
    check("oci");
}

#[test]
fn landlock_backend_conformance() {
    check("landlock");
}
