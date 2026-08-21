use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use tenon_base::jail::{self, JailSpec, Limits};

fn suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn scratch(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tenon-jail-{tag}-{}", suffix()))
}

fn uid_procs() -> u64 {
    let out = Command::new("bash")
        .arg("-c")
        .arg("ps -u $(id -u) --no-headers 2>/dev/null | wc -l")
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(200)
}

fn loose_limits() -> Limits {
    Limits {
        nproc: 0,
        mem_bytes: 0,
        cpu_secs: 0,
        nofile: 0,
        mem_max: 0,
        pids_max: 0,
    }
}

fn wait_out(mut jail: jail::Jail) -> i32 {
    let status = jail.child.wait().expect("wait jailed child");
    jail.kill();
    status.code().unwrap_or(-1)
}

/// THE gate (RFC section 7): a rogue agent that runs `rm -rf $HOME`, then tries
/// to delete and overwrite a canary planted in the real `~/workspace`, must
/// destroy only its scratch and leave the canary untouched. If Landlock is not
/// on this kernel the test refuses to run the destructive body at all rather
/// than risk the real tree.
#[test]
fn rogue_rm_cannot_touch_the_workspace_canary() {
    if !tenon_sandbox::landlock_available() {
        eprintln!("skipping: Landlock unavailable on this kernel");
        return;
    }
    let home = std::env::var("HOME").expect("HOME");
    let canary_dir = PathBuf::from(&home)
        .join("workspace")
        .join(format!(".tenon-jail-canary-{}", suffix()));
    std::fs::create_dir_all(&canary_dir).expect("make canary dir");
    let canary = canary_dir.join("canary");
    std::fs::write(&canary, "SAFE").expect("write canary");

    let scratch = scratch("gate");
    let tmp = scratch.join("tmp");
    let script = format!(
        "echo scratch-write > \"$HOME/wrote.txt\"; \
         rm -rf \"$HOME\"; \
         touch \"$HOME/pwned\"; \
         rm -f \"{canary}\" 2>/dev/null; \
         echo PWNED > \"{canary}\" 2>/dev/null; \
         true",
        canary = canary.display()
    );
    let spec = JailSpec {
        cmd: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), script],
        cwd: scratch.clone(),
        scratch: scratch.clone(),
        tmp,
        ro_allow: vec![],
        env: vec![("HOME".to_string(), scratch.display().to_string())],
        limits: loose_limits(),
        cgroup_parent: None,
    };
    let jail = jail::spawn(&spec).expect("spawn jailed");
    assert!(jail.landlocked, "landlock must be applied for the gate");
    let _ = wait_out(jail);

    let survived = std::fs::read_to_string(&canary);
    let _ = std::fs::remove_dir_all(&canary_dir);
    let survived = survived.expect("canary must survive the rogue rm");
    assert_eq!(survived, "SAFE", "canary content must be unchanged");

    let _ = std::fs::remove_dir_all(&scratch);
}

/// Scratch is the one writable tree: a write there succeeds while the workspace
/// write above failed, proving the jail denies rather than the command being
/// inert.
#[test]
fn scratch_is_writable() {
    if !tenon_sandbox::landlock_available() {
        eprintln!("skipping: Landlock unavailable");
        return;
    }
    let scratch = scratch("rw");
    let tmp = scratch.join("tmp");
    let spec = JailSpec {
        cmd: "/bin/sh".to_string(),
        args: vec![
            "-c".to_string(),
            "echo ok > \"$HOME/proof.txt\"".to_string(),
        ],
        cwd: scratch.clone(),
        scratch: scratch.clone(),
        tmp,
        ro_allow: vec![],
        env: vec![("HOME".to_string(), scratch.display().to_string())],
        limits: loose_limits(),
        cgroup_parent: None,
    };
    let code = wait_out(jail::spawn(&spec).expect("spawn"));
    assert_eq!(code, 0);
    let proof = scratch.join("proof.txt");
    assert_eq!(std::fs::read_to_string(&proof).unwrap().trim(), "ok");
    let _ = std::fs::remove_dir_all(&scratch);
}

/// RLIMIT_NPROC caps a fork bomb: set the ceiling a little above the current
/// per-uid process count and a 400-fork burst starts only a bounded handful,
/// never the whole 400, so the box survives.
#[test]
fn rlimit_nproc_caps_a_fork_bomb() {
    let scratch = scratch("fork");
    let tmp = scratch.join("tmp");
    let cap = uid_procs() + 40;
    let mut limits = loose_limits();
    limits.nproc = cap;
    let script = "for i in $(seq 1 400); do sleep 30 & done; true".to_string();
    let spec = JailSpec {
        cmd: "/bin/bash".to_string(),
        args: vec!["-c".to_string(), script],
        cwd: scratch.clone(),
        scratch: scratch.clone(),
        tmp,
        ro_allow: vec![],
        env: vec![("HOME".to_string(), scratch.display().to_string())],
        limits,
        cgroup_parent: None,
    };
    let mut jail = jail::spawn(&spec).expect("spawn");
    let pgid = jail.pgid;
    let _ = jail.child.wait();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let alive = pgroup_count(pgid);
    jail.kill();
    assert!(
        alive < 200,
        "fork bomb was not capped: {alive} live processes in the group (cap {cap})"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

/// How many live processes share `pgid`, via a `pgrep` that is itself outside
/// that group. The uncapped burst would leave 400; the RLIMIT_NPROC ceiling
/// leaves only the handful the headroom allowed.
fn pgroup_count(pgid: i32) -> u64 {
    let out = Command::new("pgrep")
        .arg("-g")
        .arg(pgid.to_string())
        .output();
    match out {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count() as u64,
        Err(_) => 0,
    }
}

/// RLIMIT_AS caps address space, the unprivileged stand-in for a memory cgroup
/// cap: an awk process told to grow a string past the ceiling fails to allocate
/// and exits non-zero instead of eating the host's memory.
#[test]
fn rlimit_as_caps_memory() {
    if Command::new("awk").arg("--version").output().is_err() {
        eprintln!("skipping: no awk");
        return;
    }
    let scratch = scratch("mem");
    let tmp = scratch.join("tmp");
    let mut limits = loose_limits();
    limits.mem_bytes = 256 * 1024 * 1024;
    let script =
        "awk 'BEGIN{s=\"x\"; while(length(s)<600000000){s=s s} print length(s)}'".to_string();
    let spec = JailSpec {
        cmd: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), script],
        cwd: scratch.clone(),
        scratch: scratch.clone(),
        tmp,
        ro_allow: vec![],
        env: vec![("HOME".to_string(), scratch.display().to_string())],
        limits,
        cgroup_parent: None,
    };
    let code = wait_out(jail::spawn(&spec).expect("spawn"));
    assert_ne!(
        code, 0,
        "the memory hog should have been capped, not succeed"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The cgroup path degrades cleanly: given a delegated parent that this host
/// will not let a session-scoped process migrate into, `spawn` still succeeds
/// (rlimits remain the floor) and the run completes — the documented degrade.
#[test]
fn cgroup_degrades_without_delegation() {
    let scratch = scratch("cg");
    let tmp = scratch.join("tmp");
    let uid = String::from_utf8_lossy(&Command::new("id").arg("-u").output().unwrap().stdout)
        .trim()
        .to_string();
    let parent = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ));
    if !parent.is_dir() {
        eprintln!("skipping: no delegated cgroup parent");
        return;
    }
    let mut limits = loose_limits();
    limits.mem_max = 256 * 1024 * 1024;
    limits.pids_max = 64;
    let spec = JailSpec {
        cmd: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), "echo alive".to_string()],
        cwd: scratch.clone(),
        scratch: scratch.clone(),
        tmp,
        ro_allow: vec![],
        env: vec![("HOME".to_string(), scratch.display().to_string())],
        limits,
        cgroup_parent: Some(parent),
    };
    let jail = jail::spawn(&spec).expect("spawn with cgroup parent");
    let code = wait_out(jail);
    assert_eq!(code, 0, "run completes whether or not the cgroup took");
    let _ = std::fs::remove_dir_all(&scratch);
}
