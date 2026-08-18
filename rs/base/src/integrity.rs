use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::time::Duration;

const BUSY_TIMEOUT_MS: u64 = 5000;

pub fn is_healthy(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() == 0 {
        return false;
    }
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    if conn
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .is_err()
    {
        return false;
    }
    matches!(
        conn.query_row("pragma integrity_check", [], |row| row.get::<_, String>(0)),
        Ok(result) if result == "ok"
    )
}

pub fn restore_if_corrupt(path: &Path, lkg: &Path) -> Result<bool> {
    if !path.is_file() || is_healthy(path) {
        return Ok(false);
    }
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    if lkg.is_file() {
        std::fs::copy(lkg, path)
            .with_context(|| format!("restore {} from {}", path.display(), lkg.display()))?;
    }
    Ok(true)
}
