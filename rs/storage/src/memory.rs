use crate::{now, Store};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

/// The memory graph the P5 plugin will own. Nothing writes these tables yet;
/// the schema and the accessors are here so that plugin is a reader of an
/// existing file rather than a migration.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryNode {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub confidence: f64,
    pub outcomes: Value,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryEdge {
    pub src: String,
    pub dst: String,
    pub rel: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Embedding {
    pub node_id: String,
    pub model: String,
    pub dims: i64,
}

impl Store {
    pub fn put_memory_node(
        &self,
        id: &str,
        kind: &str,
        text: &str,
        confidence: f64,
        outcomes: &Value,
    ) -> Result<()> {
        let at = now();
        self.conn.execute(
            "insert into memory_nodes
               (id, kind, text, confidence, outcomes, created_at, updated_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             on conflict(id) do update set
               kind = ?2, text = ?3, confidence = ?4, outcomes = ?5, updated_at = ?6",
            params![id, kind, text, confidence, outcomes.to_string(), at],
        )?;
        Ok(())
    }

    pub fn memory_node(&self, id: &str) -> Result<Option<MemoryNode>> {
        let row = self
            .conn
            .query_row(
                "select id, kind, text, confidence, outcomes, created_at, updated_at
                 from memory_nodes where id = ?1",
                params![id],
                node,
            )
            .optional()?;
        Ok(row)
    }

    pub fn memory_nodes(&self, kind: Option<&str>, limit: i64) -> Result<Vec<MemoryNode>> {
        let mut stmt = self.conn.prepare(
            "select id, kind, text, confidence, outcomes, created_at, updated_at
             from memory_nodes where (?1 is null or kind = ?1)
             order by updated_at desc limit ?2",
        )?;
        let rows = stmt.query_map(params![kind, limit.max(1)], node)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn drop_memory_node(&self, id: &str) -> Result<bool> {
        let gone = self
            .conn
            .execute("delete from memory_nodes where id = ?1", params![id])?;
        self.conn.execute(
            "delete from memory_edges where src = ?1 or dst = ?1",
            params![id],
        )?;
        self.conn
            .execute("delete from embeddings where node_id = ?1", params![id])?;
        Ok(gone > 0)
    }

    pub fn put_memory_edge(&self, src: &str, dst: &str, rel: &str, confidence: f64) -> Result<()> {
        self.conn.execute(
            "insert into memory_edges (src, dst, rel, confidence) values (?1, ?2, ?3, ?4)
             on conflict(src, dst, rel) do update set confidence = ?4",
            params![src, dst, rel, confidence],
        )?;
        Ok(())
    }

    pub fn memory_edges(&self, src: &str) -> Result<Vec<MemoryEdge>> {
        let mut stmt = self.conn.prepare(
            "select src, dst, rel, confidence from memory_edges where src = ?1 order by dst, rel",
        )?;
        let rows = stmt.query_map(params![src], |row| {
            Ok(MemoryEdge {
                src: row.get(0)?,
                dst: row.get(1)?,
                rel: row.get(2)?,
                confidence: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The vector is stored as raw little-endian f32, which is what every
    /// index this would ever feed wants back; `dims` is kept beside it so a
    /// reader never has to trust the byte length alone.
    pub fn put_embedding(&self, node_id: &str, model: &str, vector: &[f32]) -> Result<()> {
        let mut bytes = Vec::with_capacity(vector.len() * 4);
        for value in vector {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        self.conn.execute(
            "insert into embeddings (node_id, model, vector, dims) values (?1, ?2, ?3, ?4)
             on conflict(node_id, model) do update set vector = ?3, dims = ?4",
            params![node_id, model, bytes, vector.len() as i64],
        )?;
        Ok(())
    }

    pub fn embedding(&self, node_id: &str, model: &str) -> Result<Option<Vec<f32>>> {
        let row: Option<(Vec<u8>, i64)> = self
            .conn
            .query_row(
                "select vector, dims from embeddings where node_id = ?1 and model = ?2",
                params![node_id, model],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((bytes, dims)) = row else {
            return Ok(None);
        };
        let mut vector = Vec::with_capacity(dims as usize);
        for chunk in bytes.chunks_exact(4) {
            vector.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        Ok(Some(vector))
    }

    pub fn embeddings(&self, model: &str) -> Result<Vec<Embedding>> {
        let mut stmt = self.conn.prepare(
            "select node_id, model, dims from embeddings where model = ?1 order by node_id",
        )?;
        let rows = stmt.query_map(params![model], |row| {
            Ok(Embedding {
                node_id: row.get(0)?,
                model: row.get(1)?,
                dims: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn node(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryNode> {
    let outcomes: String = row.get(4)?;
    Ok(MemoryNode {
        id: row.get(0)?,
        kind: row.get(1)?,
        text: row.get(2)?,
        confidence: row.get(3)?,
        outcomes: serde_json::from_str(&outcomes).unwrap_or_else(|_| json!([])),
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}
