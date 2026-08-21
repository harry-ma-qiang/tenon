use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tenon_sandbox::{backend, Policy, Spec};

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn workspace(tag: &str, suffix: u128) -> PathBuf {
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
    let suffix = unique_suffix();
    let workspace = workspace(name, suffix);
    std::fs::create_dir_all(&workspace).unwrap();
    let home_hash = format!("conf{suffix:x}");
    let spec = Spec {
        env: format!("conf-{name}-{suffix}"),
        image: None,
        binary: None,
        workspace: workspace.clone(),
        gateway: None,
        env_passthrough: vec![],
        policy: Policy::default(),
        caps: vec![],
        home_hash: home_hash.clone(),
        base_pid: std::process::id() as i32,
        images: None,
        ingress_ports: Vec::new(),
        mounts: Vec::new(),
        hostname: None,
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
        assert_no_leaked_container(&spec.env, &home_hash);
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

fn assert_no_leaked_container(env: &str, home_hash: &str) {
    let Some(cli) = find_cli() else {
        return;
    };
    let output = Command::new(cli)
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label=tenon.env={env}"),
            "--filter",
            &format!("label=tenon.home={home_hash}"),
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

/// The krun conformance run, which no machine without a hypervisor can perform:
/// it boots a real microVM. Every reason it cannot run is printed and the test
/// passes, because a skipped backend is a fact about the host, not a failure of
/// the code. `scripts/krun-smoke.sh` sets the two variables and runs exactly
/// this test on a KVM or HVF machine.
#[test]
fn krun_backend_conformance() {
    if let Some(reason) = tenon_sandbox::krun::unavailable() {
        println!("skipping krun: {reason}");
        return;
    }
    let Some(rootfs) = std::env::var_os("TENON_KRUN_ROOTFS").map(PathBuf::from) else {
        println!("skipping krun: TENON_KRUN_ROOTFS unset (tenon sandbox image pull writes one)");
        return;
    };
    let Some(binary) = std::env::var_os("TENON_BIN").map(PathBuf::from) else {
        println!("skipping krun: TENON_BIN unset (the tenon binary runs `sandbox vmm`)");
        return;
    };
    assert!(rootfs.is_dir(), "TENON_KRUN_ROOTFS is not a directory");
    assert!(binary.is_file(), "TENON_BIN is not a file");

    let suffix = unique_suffix();
    let workspace = workspace("krun", suffix);
    std::fs::create_dir_all(&workspace).unwrap();
    let status = tenon_sandbox::krun::smoke(&binary, &rootfs, &workspace, "conformance.txt", 512)
        .expect("boot the smoke microVM");
    assert_eq!(status, 0, "the guest exited {status}");
    let seen = std::fs::read_to_string(workspace.join("conformance.txt"))
        .expect("host cannot see what the guest wrote over virtio-fs");
    assert!(seen.contains("hello-tenon"), "unexpected body: {seen:?}");

    let spec = Spec {
        env: format!("conf-krun-{suffix}"),
        image: Some(rootfs.display().to_string()),
        binary: Some(binary.clone()),
        workspace: workspace.clone(),
        gateway: Some(tenon_sandbox::krun::gateway_address("root")),
        env_passthrough: vec![],
        policy: Policy::default(),
        caps: vec![],
        home_hash: format!("conf{suffix:x}"),
        base_pid: std::process::id() as i32,
        images: None,
        ingress_ports: Vec::new(),
        mounts: Vec::new(),
        hostname: None,
    };
    let sandbox = backend("krun").expect("the krun backend");
    let instance = sandbox.spawn(&spec).expect("spawn");
    assert_eq!(instance.backend(), "krun");
    assert_eq!(instance.workspace_path(), "/workspace");
    assert!(
        instance.exec("sh", &[], Duration::from_secs(1)).is_err(),
        "a microVM must not pretend to have an exec"
    );
    assert!(
        instance
            .start_worker(&spec.env, &tenon_sandbox::krun::gateway_address("root"))
            .expect("start the worker as the guest init"),
        "krun owns the worker launch"
    );
    instance.destroy().expect("destroy");
    let _ = std::fs::remove_dir_all(&workspace);
}
