use super::*;
use rusqlite::Connection;
use serde_json::json;

fn store() -> (tempdir::Dir, Store) {
    let dir = tempdir::Dir::make();
    let store = Store::open(&dir.path().join("state.sqlite")).unwrap();
    (dir, store)
}

#[test]
fn appends_events_in_order() {
    let (_dir, store) = store();
    let first = store.append("boot", None, &json!({"n": 1})).unwrap();
    let second = store
        .append("node", Some("root"), &json!({"n": 2}))
        .unwrap();
    assert!(second.id > first.id);
    let events = store.events_since(0, 10).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].env.as_deref(), Some("root"));
    assert_eq!(events[1].data["n"], 2);
    assert_eq!(store.last_event_id().unwrap(), second.id);
    assert_eq!(store.events_since(first.id, 10).unwrap().len(), 1);
    assert_eq!(store.event_count().unwrap(), 2);
}

#[test]
fn upserts_env_rows() {
    let (_dir, store) = store();
    store.put_env("root", "agent", Some(7), "starting").unwrap();
    store.put_env("root", "agent", Some(9), "up").unwrap();
    let envs = store.envs().unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].pid, Some(9));
    assert_eq!(envs[0].status, "up");
}

#[test]
fn stores_packs_by_step_and_prunes_the_oldest() {
    let (_dir, store) = store();
    assert_eq!(store.last_pack_step().unwrap(), 0);
    for step in 1..=5 {
        store
            .put_pack(step, &format!("ref{step}"), b"pack")
            .unwrap();
    }
    store.put_pack(5, "ref5b", b"packpack").unwrap();
    assert_eq!(store.last_pack_step().unwrap(), 5);
    assert_eq!(store.pack_count().unwrap(), 5);
    let packs = store.packs().unwrap();
    assert_eq!(packs[4].reference, "ref5b");
    assert_eq!(packs[4].bytes, b"packpack");
    assert_eq!(store.prune_packs(2).unwrap(), 3);
    let left = store.packs().unwrap();
    assert_eq!(left.len(), 2);
    assert_eq!(left[0].step, 4);
}

#[test]
fn a_pack_writes_its_snapshot_row() {
    let (_dir, store) = store();
    for step in 1..=3 {
        store
            .put_pack(step, &format!("ref{step}"), b"pack")
            .unwrap();
    }
    let rows = store.snapshots().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].reference, "ref3");
    assert_eq!(store.head_snapshot().unwrap().unwrap().step, 3);
    assert_eq!(store.pack_index().unwrap()[0], (1, "ref1".to_string()));
}

#[test]
fn records_the_environment_tree() {
    let (_dir, store) = store();
    store.put_env("root", "agent", Some(1), "up").unwrap();
    store.put_env("root.1", "agent", Some(2), "up").unwrap();
    store.put_env_parent("root.1", Some("root"), 1).unwrap();
    let envs = store.envs().unwrap();
    assert_eq!(envs[1].parent.as_deref(), Some("root"));
    assert_eq!(envs[1].depth, 1);
    assert_eq!(envs[0].depth, 0);
    store.put_env("root.1", "agent", Some(3), "down").unwrap();
    assert_eq!(store.envs().unwrap()[1].parent.as_deref(), Some("root"));
    store.drop_env("root.1").unwrap();
    assert_eq!(store.envs().unwrap().len(), 1);
}

#[test]
fn sets_the_day_one_pragmas() {
    let (_dir, store) = store();
    assert_eq!(store.pragma("journal_mode").unwrap(), "wal");
    assert_eq!(store.pragma("busy_timeout").unwrap(), "5000");
    assert_eq!(store.pragma("auto_vacuum").unwrap(), "2");
    store.checkpoint().unwrap();
}

#[test]
fn tool_results_point_at_their_event_and_blob() {
    let (_dir, store) = store();
    let event = store
        .append("tool/result", Some("root"), &json!({}))
        .unwrap();
    let hash = store.put_blob(b"a long tool output").unwrap();
    let id = store
        .put_tool_result(event.id, "bash", "ok", 42, Some(&hash))
        .unwrap();
    store
        .put_tool_result(event.id, "grep", "error", 7, None)
        .unwrap();
    let rows = store.tool_results_of_event(event.id).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].blob_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(rows[0].duration_ms, 42);
    assert_eq!(rows[1].status, "error");
    assert!(rows[1].blob_hash.is_none());
    let tail = store.tool_results_tail(1).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].name, "grep");
    assert_eq!(store.tool_result_count().unwrap(), 2);
}

#[test]
fn blobs_deduplicate_and_read_incrementally() {
    let (_dir, store) = store();
    let body: Vec<u8> = (0..40_000u32).map(|n| (n % 251) as u8).collect();
    let hash = store.put_blob(&body).unwrap();
    let again = store.put_blob(&body).unwrap();
    assert_eq!(hash, again);
    assert_eq!(hash, sha256(&body));
    assert_eq!(store.blob_count().unwrap(), 1);
    assert_eq!(store.blob_bytes().unwrap(), body.len() as i64);
    assert_eq!(store.get_blob(&hash).unwrap().unwrap(), body);
    assert!(store.get_blob("nothing").unwrap().is_none());
    let window = store.open_blob(&hash, 39_000, 1_000).unwrap();
    assert_eq!(window, body[39_000..40_000]);
    let past_end = store.open_blob(&hash, 39_990, 4_000).unwrap();
    assert_eq!(past_end.len(), 10);
    assert_eq!(store.blob(&hash).unwrap().unwrap().size, 40_000);
    assert!(store.open_blob("nothing", 0, 1).is_err());
    assert!(store.delete_blob(&hash).unwrap());
    assert_eq!(store.blob_count().unwrap(), 0);
}

#[test]
fn episodes_are_one_row_per_step_queryable_by_session() {
    let (_dir, store) = store();
    for step in 1..=3 {
        store
            .put_episode(
                "s1",
                step,
                &format!("hash{step}"),
                &json!([{"name": "bash"}]),
                Some(1.0),
                &json!({"total": 18}),
            )
            .unwrap();
    }
    store
        .put_episode("s2", 1, "other", &json!("respond"), None, &json!({}))
        .unwrap();
    let mine = store.episodes_of_session("s1", 10).unwrap();
    assert_eq!(mine.len(), 3);
    assert_eq!(mine[2].step, 3);
    assert_eq!(mine[2].action[0]["name"], "bash");
    assert_eq!(mine[2].cost["total"], 18);
    assert_eq!(mine[0].verifier_score, Some(1.0));
    let tail = store.episodes_tail(2).unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[1].session_id, "s2");
    assert_eq!(tail[1].action, json!("respond"));
    assert!(tail[1].verifier_score.is_none());
    assert_eq!(store.episode_count().unwrap(), 4);
}

#[test]
fn memory_nodes_edges_and_embeddings_round_trip() {
    let (_dir, store) = store();
    store
        .put_memory_node("n1", "fact", "cargo test is the gate", 0.5, &json!([]))
        .unwrap();
    store
        .put_memory_node("n1", "fact", "cargo test is the gate", 0.9, &json!(["ok"]))
        .unwrap();
    store
        .put_memory_node("n2", "task", "run the suite", 0.1, &json!([]))
        .unwrap();
    let node = store.memory_node("n1").unwrap().unwrap();
    assert_eq!(node.confidence, 0.9);
    assert_eq!(node.outcomes[0], "ok");
    assert!(node.updated_at >= node.created_at);
    assert_eq!(store.memory_nodes(Some("task"), 10).unwrap().len(), 1);
    assert_eq!(store.memory_nodes(None, 10).unwrap().len(), 2);
    store.put_memory_edge("n1", "n2", "supports", 0.3).unwrap();
    store.put_memory_edge("n1", "n2", "supports", 0.8).unwrap();
    let edges = store.memory_edges("n1").unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].confidence, 0.8);
    store.put_embedding("n1", "e5", &[0.5, -1.5, 2.0]).unwrap();
    assert_eq!(
        store.embedding("n1", "e5").unwrap().unwrap(),
        vec![0.5, -1.5, 2.0]
    );
    assert_eq!(store.embeddings("e5").unwrap()[0].dims, 3);
    assert!(store.embedding("n1", "other").unwrap().is_none());
    assert!(store.drop_memory_node("n1").unwrap());
    assert!(store.memory_edges("n1").unwrap().is_empty());
    assert!(store.embedding("n1", "e5").unwrap().is_none());
}

#[test]
fn approvals_move_from_pending_to_a_verdict() {
    let (_dir, store) = store();
    let pending = store
        .put_approval("root", "touch the host", approvals::PENDING)
        .unwrap();
    let denied = store
        .put_approval("root", "rm -rf /", approvals::DENIED)
        .unwrap();
    assert_eq!(
        store.approval(pending).unwrap().unwrap().status,
        approvals::PENDING
    );
    assert!(store
        .approval(pending)
        .unwrap()
        .unwrap()
        .decided_at
        .is_none());
    assert!(store
        .approval(denied)
        .unwrap()
        .unwrap()
        .decided_at
        .is_some());
    assert_eq!(
        store.approvals(Some(approvals::PENDING), 10).unwrap().len(),
        1
    );
    assert!(store.decide_approval(pending, approvals::APPROVED).unwrap());
    assert!(!store.decide_approval(pending, approvals::DENIED).unwrap());
    assert_eq!(
        store.approval(pending).unwrap().unwrap().status,
        approvals::APPROVED
    );
    let stale = store
        .put_approval("root", "waited too long", approvals::PENDING)
        .unwrap();
    assert_eq!(store.expire_approvals(0).unwrap(), 1);
    assert_eq!(
        store.approval(stale).unwrap().unwrap().status,
        approvals::EXPIRED
    );
    assert_eq!(store.approvals(None, 10).unwrap().len(), 3);
}

#[test]
fn retention_keeps_the_window_the_milestones_and_the_lkg_refs() {
    let policy = Retention {
        keep_steps: 5,
        milestone_every: 10,
        keep_refs: vec!["ref3".to_string()],
        ..Retention::default()
    };
    assert!(policy.keeps(96, 100, "x"));
    assert!(!policy.keeps(95, 100, "x"));
    assert!(policy.keeps(90, 100, "x"));
    assert!(policy.keeps(3, 100, "ref3"));
    assert!(!policy.keeps(3, 100, "ref4"));
    let (_dir, store) = store();
    for step in 1..=100 {
        store
            .put_pack(step, &format!("ref{step}"), &vec![7u8; 4_096])
            .unwrap();
    }
    let out = store.retain(&policy).unwrap();
    assert_eq!(out.packs, 100 - 5 - 9 - 1);
    assert_eq!(out.snapshots, out.packs);
    let left: Vec<i64> = store
        .pack_index()
        .unwrap()
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(left.len(), 15);
    assert!(left.contains(&3));
    assert!(left.contains(&10));
    assert!(left.contains(&100));
    assert!(!left.contains(&95));
    assert_eq!(store.snapshots().unwrap().len(), 15);
}

#[test]
fn retention_drops_unreferenced_blobs_and_the_event_window() {
    let (_dir, store) = store();
    let kept = store.put_blob(b"referenced").unwrap();
    let loose = store.put_blob(b"nothing points here").unwrap();
    for n in 0..50 {
        store
            .append("tool/result", Some("root"), &json!({"n": n}))
            .unwrap();
    }
    let last = store.last_event_id().unwrap();
    store
        .put_tool_result(last, "bash", "ok", 1, Some(&kept))
        .unwrap();
    let old = store
        .put_tool_result(1, "bash", "ok", 1, Some(&loose))
        .unwrap();
    assert!(old > 0);
    let policy = Retention {
        keep_events: 10,
        blob_grace_ms: 0,
        ..Retention::default()
    };
    let out = store.retain(&policy).unwrap();
    assert_eq!(out.events, 40);
    assert_eq!(out.tool_results, 1);
    assert_eq!(out.blobs, 1);
    assert_eq!(store.event_count().unwrap(), 10);
    assert_eq!(store.blob_count().unwrap(), 1);
    assert!(store.get_blob(&kept).unwrap().is_some());
    assert!(store.get_blob(&loose).unwrap().is_none());
    let young = store.put_blob(b"just written").unwrap();
    let out = store
        .retain(&Retention {
            blob_grace_ms: 60_000,
            ..Retention::default()
        })
        .unwrap();
    assert_eq!(out.blobs, 0);
    assert!(store.get_blob(&young).unwrap().is_some());
}

#[test]
fn migrates_a_file_written_before_schema_versions_existed() {
    let dir = tempdir::Dir::make();
    let path = dir.path().join("state.sqlite");
    {
        let old = Connection::open(&path).unwrap();
        old.execute_batch(
            "create table events (
               id integer primary key autoincrement,
               at integer not null, kind text not null, env text, data text not null);
             create table envs (
               name text primary key, role text not null, pid integer,
               status text not null, at integer not null);
             create table packs (
               step integer primary key, ref text not null,
               bytes blob not null, created_at integer not null);",
        )
        .unwrap();
        old.execute(
            "insert into events (at, kind, env, data) values (1, 'boot', null, '{}')",
            [],
        )
        .unwrap();
        old.execute(
            "insert into envs (name, role, pid, status, at) values ('root', 'agent', 1, 'up', 1)",
            [],
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.version().unwrap(), VERSION);
    assert_eq!(store.event_count().unwrap(), 1);
    assert_eq!(store.envs().unwrap()[0].depth, 0);
    store.put_env_parent("root", None, 0).unwrap();
    let hash = store.put_blob(b"new table on an old file").unwrap();
    store
        .put_episode("s1", 1, &hash, &json!("respond"), Some(1.0), &json!({}))
        .unwrap();
    assert_eq!(store.episode_count().unwrap(), 1);
    let again = Store::open(&path).unwrap();
    assert_eq!(again.version().unwrap(), VERSION);
    assert_eq!(again.episode_count().unwrap(), 1);
}

mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    pub struct Dir(PathBuf);

    impl Dir {
        pub fn make() -> Self {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("tenon-storage-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
