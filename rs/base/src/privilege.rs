use serde_json::{json, Value};
use std::path::Path;

pub const NONE: &str = "none";
const CAP_SETUID: u32 = 7;

/// What base decided to do about `env_user`, once per boot. RFC section 4's
/// per-env privilege drop is best effort by design: a base that may not
/// change uid says so loudly and keeps running unprivileged rather than
/// refusing to boot an env it could otherwise supervise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Off,
    Unknown(String),
    Unpermitted {
        user: String,
        uid: u32,
        gid: u32,
        reason: String,
    },
    Drop {
        user: String,
        uid: u32,
        gid: u32,
    },
}

impl Plan {
    pub fn active(&self) -> Option<(u32, u32)> {
        match self {
            Plan::Drop { uid, gid, .. } => Some((*uid, *gid)),
            _ => None,
        }
    }

    pub fn view(&self) -> Value {
        match self {
            Plan::Off => json!({"env_user": NONE, "dropping": false}),
            Plan::Unknown(user) => json!({
                "env_user": user,
                "dropping": false,
                "reason": format!("no such user {user}"),
            }),
            Plan::Unpermitted {
                user, uid, reason, ..
            } => json!({
                "env_user": user,
                "uid": uid,
                "dropping": false,
                "permitted": false,
                "reason": reason,
            }),
            Plan::Drop { user, uid, gid } => json!({
                "env_user": user,
                "uid": uid,
                "gid": gid,
                "dropping": true,
                "permitted": true,
            }),
        }
    }

    pub fn line(&self) -> Option<String> {
        match self {
            Plan::Off => None,
            Plan::Unknown(user) => Some(format!(
                "tenon base: env_user {user} does not exist; env processes stay unprivileged"
            )),
            Plan::Unpermitted { user, reason, .. } => Some(format!(
                "tenon base: cannot run env processes as {user} ({reason}); \
                 they stay unprivileged"
            )),
            Plan::Drop { user, uid, gid } => Some(format!(
                "tenon base: env processes run as {user} ({uid}:{gid})"
            )),
        }
    }
}

/// `name:x:uid:gid:...` out of a passwd file, taken as a path so the parsing
/// is testable without a real account.
pub fn lookup(user: &str, passwd: &Path) -> Option<(u32, u32)> {
    let body = std::fs::read_to_string(passwd).ok()?;
    for line in body.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some(user) {
            continue;
        }
        let _password = fields.next()?;
        let uid = fields.next()?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        return Some((uid, gid));
    }
    None
}

/// `None` when base may change uid: it is root, or its effective capability
/// set carries CAP_SETUID. Otherwise the reason it may not.
pub fn permission() -> Option<String> {
    if unsafe { libc::geteuid() } == 0 {
        return None;
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
        .unwrap_or(0);
    match effective & (1 << CAP_SETUID) != 0 {
        true => None,
        false => Some("base is not root and has no CAP_SETUID".to_string()),
    }
}

pub fn plan(user: &str, passwd: &Path, denied: Option<String>) -> Plan {
    let user = user.trim();
    if user.is_empty() || user == NONE {
        return Plan::Off;
    }
    let Some((uid, gid)) = lookup(user, passwd) else {
        return Plan::Unknown(user.to_string());
    };
    match denied {
        Some(reason) => Plan::Unpermitted {
            user: user.to_string(),
            uid,
            gid,
            reason,
        },
        None => Plan::Drop {
            user: user.to_string(),
            uid,
            gid,
        },
    }
}

pub fn resolve(env_user: &str) -> Plan {
    plan(env_user, Path::new("/etc/passwd"), permission())
}

/// setgid then setuid, before `execve` and in the forked child only, so the
/// whole lifetime of the exec'd program runs as that user. Order matters:
/// dropping the uid first would take away the right to drop the gid.
pub fn apply(command: &mut tokio::process::Command, plan: &Plan) {
    let Some((uid, gid)) = plan.active() else {
        return;
    };
    unsafe {
        command.pre_exec(move || {
            if libc::setgid(gid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setuid(uid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    };
}

fn chown(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    let raw = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::other("path has a nul byte"))?;
    match unsafe { libc::chown(raw.as_ptr(), uid, gid) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// The paths an env's own processes write to. A node that runs as another
/// user cannot use directories base owns, so they are handed over with it.
pub fn env_paths(home: &crate::home::Home, env: &str) -> Vec<std::path::PathBuf> {
    vec![
        home.env_dir(env),
        home.workspace_dir(env),
        home.gateway_dir(env),
        home.profiles().join(env),
        home.env_state_file(env),
        home.log(env),
        home.log(&format!("harness-{env}")),
    ]
}

pub fn chown_env(home: &crate::home::Home, env: &str, plan: &Plan) -> Vec<String> {
    let Some((uid, gid)) = plan.active() else {
        return Vec::new();
    };
    let mut failed = Vec::new();
    for path in env_paths(home, env) {
        if !path.exists() {
            continue;
        }
        if let Err(error) = chown(&path, uid, gid) {
            failed.push(format!("{}: {error}", path.display()));
        }
    }
    failed
}

impl crate::base::Base {
    /// Once per boot: resolve `env_user`, say out loud what will happen, and
    /// keep the plan for every node and harness spawned after it.
    pub fn load_privilege(&mut self) {
        let plan = resolve(&self.config.env_user);
        if let Some(line) = plan.line() {
            eprintln!("{line}");
        }
        self.emit("env.privilege", None, plan.view());
        self.privilege = plan;
    }

    /// The env's own directories follow the uid its processes run as. A
    /// failure is a logged fact, not a boot failure: the drop is best effort.
    pub fn hand_over(&mut self, role: &str, env: &str) {
        if role == crate::node::GUARDIAN || self.privilege.active().is_none() {
            return;
        }
        let failed = chown_env(&self.home, env, &self.privilege);
        let data = match failed.is_empty() {
            true => json!({"env": env, "ok": true}),
            false => json!({"env": env, "ok": false, "failed": failed}),
        };
        self.emit("env.chown", Some(env), data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passwd() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("tenon-passwd-{}", std::process::id()));
        std::fs::write(
            &path,
            "root:x:0:0:root:/root:/bin/sh\ntenon:x:1200:1300:Tenon:/home/tenon:/bin/sh\n",
        )
        .expect("write passwd");
        path
    }

    #[test]
    fn none_and_an_unknown_user_never_drop() {
        let passwd = passwd();
        assert_eq!(plan("none", &passwd, None), Plan::Off);
        assert_eq!(plan("", &passwd, None), Plan::Off);
        assert_eq!(
            plan("ghost", &passwd, None),
            Plan::Unknown("ghost".to_string())
        );
        assert_eq!(plan("ghost", &passwd, None).active(), None);
        assert!(plan("ghost", &passwd, None)
            .line()
            .expect("a line")
            .contains("does not exist"));
    }

    #[test]
    fn a_known_user_drops_only_when_base_is_permitted() {
        let passwd = passwd();
        let dropped = plan("tenon", &passwd, None);
        assert_eq!(dropped.active(), Some((1200, 1300)));
        assert_eq!(dropped.view()["dropping"], true);
        assert!(dropped.line().expect("a line").contains("1200:1300"));

        let denied = plan("tenon", &passwd, Some("no CAP_SETUID".to_string()));
        assert_eq!(denied.active(), None);
        assert_eq!(denied.view()["permitted"], false);
        assert_eq!(denied.view()["uid"], 1200);
        assert!(denied.line().expect("a line").contains("no CAP_SETUID"));
        let _ = std::fs::remove_file(&passwd);
    }

    #[test]
    fn the_env_paths_handed_over_are_that_env_s_own() {
        let home = crate::home::Home {
            root: std::path::PathBuf::from("/tmp/tenon-privilege"),
        };
        let paths: Vec<String> = env_paths(&home, "root")
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        assert!(paths.contains(&"/tmp/tenon-privilege/envs/root/workspace".to_string()));
        assert!(paths.contains(&"/tmp/tenon-privilege/run/gw-root".to_string()));
        assert!(paths.contains(&"/tmp/tenon-privilege/state-root.sqlite".to_string()));
        assert!(
            !paths
                .iter()
                .any(|path| path.ends_with("run/base.sock") || path.ends_with("state.sqlite")),
            "base's own files are never handed over: {paths:?}"
        );
        assert_eq!(chown_env(&home, "root", &Plan::Off), Vec::<String>::new());
    }
}
