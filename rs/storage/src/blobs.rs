use crate::{now, Store};
use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension, MAIN_DB};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

#[derive(Debug, Clone, Serialize)]
pub struct Blob {
    pub sha256: String,
    pub size: i64,
    pub created_at: i64,
}

impl Store {
    /// Content addressed and deduplicated: the same bytes stored twice are one
    /// row, and the hash is the only handle anything else keeps. Large tool
    /// outputs and any other payload too big for an event row live here.
    pub fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let hash = sha256(bytes);
        self.conn.execute(
            "insert or ignore into blobs (sha256, bytes, size, created_at) values (?1, ?2, ?3, ?4)",
            params![hash, bytes, bytes.len() as i64, now()],
        )?;
        Ok(hash)
    }

    pub fn get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "select bytes from blobs where sha256 = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?;
        Ok(bytes)
    }

    pub fn blob(&self, hash: &str) -> Result<Option<Blob>> {
        let row = self
            .conn
            .query_row(
                "select sha256, size, created_at from blobs where sha256 = ?1",
                params![hash],
                |row| {
                    Ok(Blob {
                        sha256: row.get(0)?,
                        size: row.get(1)?,
                        created_at: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// The incremental read SQLite's `blob_open` is for: a window of a stored
    /// blob without materialising the whole row, which is what makes a 100 MB
    /// tool output paged rather than loaded.
    pub fn open_blob(&self, hash: &str, offset: i64, len: i64) -> Result<Vec<u8>> {
        let rowid: Option<i64> = self
            .conn
            .query_row(
                "select rowid from blobs where sha256 = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?;
        let Some(rowid) = rowid else {
            bail!("unknown blob {hash}");
        };
        let mut blob = self
            .conn
            .blob_open(MAIN_DB, "blobs", "bytes", rowid, true)?;
        let size = blob.size() as i64;
        if offset < 0 || len < 0 || offset > size {
            bail!("blob range out of bounds: offset {offset} len {len} size {size}");
        }
        let want = len.min(size - offset) as usize;
        blob.seek(SeekFrom::Start(offset as u64))?;
        let mut out = vec![0u8; want];
        let mut done = 0usize;
        while done < want {
            let read = blob.read(&mut out[done..])?;
            if read == 0 {
                break;
            }
            done += read;
        }
        out.truncate(done);
        Ok(out)
    }

    pub fn blob_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("select count(*) from blobs", [], |row| row.get(0))?)
    }

    pub fn blob_bytes(&self) -> Result<i64> {
        let sum: Option<i64> = self
            .conn
            .query_row("select sum(size) from blobs", [], |row| row.get(0))
            .optional()?
            .flatten();
        Ok(sum.unwrap_or(0))
    }

    pub fn delete_blob(&self, hash: &str) -> Result<bool> {
        let gone = self
            .conn
            .execute("delete from blobs where sha256 = ?1", params![hash])?;
        Ok(gone > 0)
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
