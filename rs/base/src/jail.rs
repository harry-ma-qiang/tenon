use anyhow::{Context, Result};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// The resource ceiling of a jailed process (RFC P5.0a, layer A). Every limit is
/// an absolute number; `0` leaves that limit alone. `nproc`, `cpu_secs` and
/// `nofile` become `setrlimit` calls in the child before `execve`; `mem_bytes`
/// is `RLIMIT_AS` (address space, the unprivileged stand-in for a memory cgroup
/// cap); `mem_max` and `pids_max` are the cgroup v2 knobs used only when a
/// delegated subtree is writable, and ignored otherwise.
#[derive(Debug, Clone)]
pub struct Limits {
    pub nproc: u64,
    pub mem_bytes: u64,
    pub cpu_secs: u64,
    pub nofile: u64,
    pub mem_max: u64,
    pub pids_max: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            nproc: 256,
            mem_bytes: 2 * 1024 * 1024 * 1024,
            cpu_secs: 3600,
            nofile: 1024,
            mem_max: 2 * 1024 * 1024 * 1024,
            pids_max: 256,
        }
    }
}

/// What to run and how to confine it. `scratch` and `tmp` are the only writable
/// trees; `ro_allow` names the read-only credential/config dirs the agent needs
/// (its own `~/.config`, `~/.agy`, `~/.claude`), on top of the minimal system
/// read-only set. `~/workspace`, the tenon repo, `deepseek.env.sh` and `~/.ssh`
/// are never granted, so a rogue `rm -rf` reaches only `scratch`.
pub struct JailSpec {
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub scratch: PathBuf,
    pub tmp: PathBuf,
    /// Extra read-write trees beyond scratch/tmp. Empty by default; the cli-agent
    /// opt-in `--writable-state` uses it to grant the agent's OWN credential/state
    /// dir (e.g. `~/.gemini/antigravity-cli`) read-write, which some agents need
    /// to refresh their auth token. Never `~/workspace`, the repo, or `~/.ssh`.
    pub rw_allow: Vec<PathBuf>,
    pub ro_allow: Vec<PathBuf>,
    pub env: Vec<(String, String)>,
    pub limits: Limits,
    pub cgroup_parent: Option<PathBuf>,
}

/// The minimal read-only system paths a normal binary needs to start and reach
/// its model endpoint. Deliberately excludes `/etc` as a whole: only the TLS and
/// resolver files are named, never the wider config tree.
const RO_SYSTEM: &[&str] = &[
    "/usr",
    "/lib",
    "/lib64",
    "/bin",
    "/sbin",
    "/proc",
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/resolv.conf",
    "/etc/nsswitch.conf",
    "/etc/hosts",
    "/etc/passwd",
    "/etc/group",
    "/etc/localtime",
];

/// Individual device files a shell and its children need for redirection and
/// entropy. Granted read-write one file at a time, never the whole `/dev`.
const RW_DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
];

/// A running jailed process. Dropping it does not kill the child (a supervisor
/// may outlive one handle); call `kill` for that. `pgid` is the child's own
/// process group, so `kill` reaps the whole tree, not just the leader.
pub struct Jail {
    pub child: Child,
    pub pgid: i32,
    pub landlocked: bool,
    cgroup: Option<Cgroup>,
}

impl Jail {
    /// SIGKILL the whole process group, and the cgroup too when one was created,
    /// so a fork bomb that reparented away from the leader still dies. Idempotent.
    pub fn kill(&mut self) {
        if let Some(cgroup) = &self.cgroup {
            cgroup.kill();
        }
        if self.pgid > 1 {
            unsafe { libc::killpg(self.pgid, libc::SIGKILL) };
        }
        let _ = self.child.kill();
    }

    pub fn cgroup_path(&self) -> Option<PathBuf> {
        self.cgroup.as_ref().map(|cgroup| cgroup.dir.clone())
    }
}

impl Drop for Jail {
    fn drop(&mut self) {
        if let Some(cgroup) = self.cgroup.take() {
            cgroup.remove();
        }
    }
}

/// Spawn `spec.cmd` confined by Landlock (when the kernel has it), rlimits and,
/// when a delegated cgroup v2 subtree is writable, a cgroup with `memory.max`
/// and `pids.max`. Returns the handle with its stdout/stderr piped for the
/// caller to stream. Network egress is left unrestricted in v1 (documented): the
/// agent needs its model endpoint, and the filesystem/rlimit floor is what keeps
/// the host safe regardless of what the agent does on the network.
pub fn spawn(spec: &JailSpec) -> Result<Jail> {
    std::fs::create_dir_all(&spec.scratch)
        .with_context(|| format!("create scratch {}", spec.scratch.display()))?;
    std::fs::create_dir_all(&spec.tmp)
        .with_context(|| format!("create tmp {}", spec.tmp.display()))?;

    let landlocked = tenon_sandbox::landlock_available();
    if !landlocked {
        eprintln!(
            "tenon jail: WARNING Landlock unavailable on this kernel; \
             filesystem confinement is OFF, applying rlimits only"
        );
    }

    let rw = writable_paths(spec);
    let ro = readonly_paths(spec);
    let limits = spec.limits.clone();

    let cgroup = spec
        .cgroup_parent
        .as_deref()
        .and_then(|parent| Cgroup::create(parent, &limits));

    let mut command = Command::new(&spec.cmd);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k.clone(), v.clone())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let apply_landlock = landlocked;
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            apply_rlimits(&limits)?;
            if apply_landlock {
                tenon_sandbox::landlock_confine(&rw, &ro).map_err(io::Error::other)?;
            }
            Ok(())
        });
    }

    let child = command
        .spawn()
        .with_context(|| format!("spawn jailed {}", spec.cmd))?;
    let pgid = child.id() as i32;
    if let Some(cgroup) = &cgroup {
        cgroup.add(pgid);
    }
    Ok(Jail {
        child,
        pgid,
        landlocked,
        cgroup,
    })
}

fn writable_paths(spec: &JailSpec) -> Vec<PathBuf> {
    let mut rw = vec![spec.scratch.clone(), spec.tmp.clone()];
    rw.extend(spec.rw_allow.iter().cloned());
    rw.extend(RW_DEVICES.iter().map(PathBuf::from));
    rw.retain(|path| path.exists());
    rw
}

fn readonly_paths(spec: &JailSpec) -> Vec<PathBuf> {
    let mut ro: Vec<PathBuf> = RO_SYSTEM.iter().map(PathBuf::from).collect();
    ro.extend(spec.ro_allow.iter().cloned());
    if let Some(dir) = PathBuf::from(&spec.cmd).parent() {
        if dir.is_dir() {
            ro.push(dir.to_path_buf());
        }
    }
    ro.retain(|path| path.exists());
    ro
}

fn apply_rlimits(limits: &Limits) -> io::Result<()> {
    set_one(libc::RLIMIT_NPROC, limits.nproc)?;
    set_one(libc::RLIMIT_AS, limits.mem_bytes)?;
    set_one(libc::RLIMIT_CPU, limits.cpu_secs)?;
    set_one(libc::RLIMIT_NOFILE, limits.nofile)?;
    Ok(())
}

fn set_one(resource: libc::__rlimit_resource_t, value: u64) -> io::Result<()> {
    if value == 0 {
        return Ok(());
    }
    let limit = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A best-effort cgroup v2 subtree. Creation and the write of `memory.max` /
/// `pids.max` may all fail without a delegated tree; `add` failing (the common
/// case for a process started outside the delegated hierarchy — the delegation
/// containment rule denies the migration) leaves the rlimit floor as the only
/// enforced ceiling, which is why the jail never depends on this succeeding.
struct Cgroup {
    dir: PathBuf,
    migrated: std::sync::atomic::AtomicBool,
}

impl Cgroup {
    fn create(parent: &std::path::Path, limits: &Limits) -> Option<Cgroup> {
        let dir = parent.join(format!("tenon-jail-{}", std::process::id()));
        if std::fs::create_dir(&dir).is_err() {
            return None;
        }
        if limits.mem_max > 0 {
            let _ = std::fs::write(dir.join("memory.max"), limits.mem_max.to_string());
        }
        if limits.pids_max > 0 {
            let _ = std::fs::write(dir.join("pids.max"), limits.pids_max.to_string());
        }
        Some(Cgroup {
            dir,
            migrated: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn add(&self, pid: i32) {
        match std::fs::write(self.dir.join("cgroup.procs"), pid.to_string()) {
            Ok(()) => self
                .migrated
                .store(true, std::sync::atomic::Ordering::Relaxed),
            Err(error) => eprintln!(
                "tenon jail: WARNING cgroup migration denied ({error}); \
                 rlimits remain the enforced ceiling"
            ),
        }
    }

    fn kill(&self) {
        let _ = std::fs::write(self.dir.join("cgroup.kill"), "1");
    }

    fn remove(&self) {
        let _ = std::fs::remove_dir(&self.dir);
    }
}
