use crate::ExecOutcome;
use anyhow::{Context, Result};
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use wait_timeout::ChildExt;

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
