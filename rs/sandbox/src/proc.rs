use crate::ExecOutcome;
use anyhow::{Context, Result};
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use wait_timeout::ChildExt;

pub fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

pub fn run(mut command: Command, timeout: Duration) -> Result<ExecOutcome> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn exec")?;
    let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    let stdout_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let (status, timed_out) = match child.wait_timeout(timeout).context("wait exec")? {
        Some(status) => (status.code().unwrap_or(-1), false),
        None => {
            let _ = child.kill();
            let status = child.wait().context("wait after kill")?;
            (status.code().unwrap_or(-1), true)
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(ExecOutcome {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

/// SIGTERM, a grace period, then SIGKILL, and always a `wait` — a VMM child is
/// spawned by base and reaped by base, never left as a zombie for init.
pub fn terminate(mut child: std::process::Child, grace: Duration) {
    let pid = child.id() as i32;
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
