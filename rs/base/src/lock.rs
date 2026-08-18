use crate::home::Home;
use anyhow::{Context, Result};
use std::io::Write;
use std::os::fd::AsRawFd;

pub struct Lock {
    _file: std::fs::File,
}

impl Lock {
    pub fn try_acquire(home: &Home) -> Result<Option<Self>> {
        let path = home.lock_file();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error).with_context(|| format!("flock {}", path.display()));
        }
        file.set_len(0)
            .with_context(|| format!("truncate {}", path.display()))?;
        write!(file, "{}", std::process::id())
            .with_context(|| format!("write {}", path.display()))?;
        file.flush().ok();
        Ok(Some(Self { _file: file }))
    }

    pub fn holder_pid(home: &Home) -> Option<i64> {
        std::fs::read_to_string(home.lock_file())
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}
