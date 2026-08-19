use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub fn alive(pid: i64) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(tail) = stat.rsplit(')').next() else {
        return false;
    };
    !matches!(tail.split_whitespace().next(), Some("Z") | None)
}

pub fn wait_gone(pids: &[i64], limit: Duration) -> Duration {
    let started = Instant::now();
    while started.elapsed() < limit {
        if pids.iter().all(|pid| !alive(*pid)) {
            return started.elapsed();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    started.elapsed()
}

/// Signals a pid that has to be there: what a test which just read the pid out
/// of a live status expects, unlike `kill`, which may be aimed at a corpse.
pub fn kill_alive(pid: i64, signal: &str) {
    let status = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()
        .expect("kill");
    assert!(status.success(), "kill {signal} {pid}");
}

pub fn kill(pid: i64, signal: &str) {
    let _ = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status();
}

/// Every pid whose /proc/<pid>/environ names this home's socket path: covers
/// the guardian and agent BEAM nodes base spawned, no matter which generation
/// of `start` created them.
pub fn pids_by_sock(sock: &Path) -> Vec<i64> {
    let needle = format!("TENON_BASE_SOCK={}", sock.display());
    scan_proc(|pid| {
        std::fs::read(format!("/proc/{pid}/environ"))
            .map(|bytes| contains(&bytes, needle.as_bytes()))
            .unwrap_or(false)
    })
}

/// Every pid whose /proc/<pid>/cmdline names both this home directory and the
/// binary under test: covers `tenon --home <home> start --foreground`, any
/// generation, without counting the container engine's own processes.
pub fn pids_by_home(home: &Path, bin: &str) -> Vec<i64> {
    let needle = home.to_string_lossy().to_string();
    scan_proc(|pid| {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| contains(&bytes, needle.as_bytes()) && contains(&bytes, bin.as_bytes()))
            .unwrap_or(false)
    })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len().max(1))
        .any(|window| window == needle)
}

fn scan_proc(matches: impl Fn(i64) -> bool) -> Vec<i64> {
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<i64>() else {
            continue;
        };
        if matches(pid) {
            pids.push(pid);
        }
    }
    pids
}
