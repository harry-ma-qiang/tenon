use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use tenon_worker::snap::Snap;

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct Temp {
    path: PathBuf,
}

impl Temp {
    fn new(tag: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("tenon-snap-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn put(&self, rel: &str, text: &str) {
        let target = self.path.join(rel);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, text).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path.join(rel)).unwrap()
    }

    fn has(&self, rel: &str) -> bool {
        self.path.join(rel).exists()
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn steps(listing: &serde_json::Value) -> Vec<u64> {
    listing["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["step"].as_u64().unwrap())
        .collect()
}

#[test]
fn commit_then_list_shows_step_one() {
    let temp = Temp::new("first");
    temp.put("a.txt", "alpha\n");
    temp.put("dir/b.txt", "beta\n");
    let snap = Snap::open(&temp.path).unwrap();

    let done = snap.commit(None).unwrap();
    assert_eq!(done["step"], 1);
    assert_eq!(done["label"], "step 1");
    assert_eq!(done["files"], 2);
    assert_eq!(done["ref"].as_str().unwrap().len(), 40);
    assert!(done["at"].as_i64().unwrap() > 0);

    let listing = snap.list().unwrap();
    assert_eq!(listing["count"], 1);
    assert_eq!(steps(&listing), vec![1]);
    assert_eq!(listing["snapshots"][0]["label"], "step 1");
    assert_eq!(snap.head().unwrap().unwrap().0, 1);
}

#[test]
fn a_second_commit_makes_head_step_two() {
    let temp = Temp::new("second");
    temp.put("a.txt", "alpha\n");
    let snap = Snap::open(&temp.path).unwrap();
    snap.commit(Some("base")).unwrap();

    temp.put("a.txt", "alpha two\n");
    let done = snap.commit(Some("change")).unwrap();
    assert_eq!(done["step"], 2);
    assert_eq!(done["label"], "change");

    let listing = snap.list().unwrap();
    assert_eq!(listing["count"], 2);
    assert_eq!(steps(&listing), vec![1, 2]);

    let (step, oid) = snap.head().unwrap().unwrap();
    assert_eq!(step, 2);
    assert_eq!(oid, done["ref"].as_str().unwrap());
}

#[test]
fn an_ignored_file_is_not_snapshotted_and_survives_restore() {
    let temp = Temp::new("ignored");
    temp.put(".gitignore", "build/\n");
    temp.put("build/out.bin", "artifact\n");
    temp.put("keep.txt", "one\n");
    let snap = Snap::open(&temp.path).unwrap();

    let first = snap.commit(None).unwrap();
    assert_eq!(first["files"], 2);

    std::fs::remove_file(temp.path.join("build/out.bin")).unwrap();
    snap.restore("1").unwrap();
    assert!(
        !temp.has("build/out.bin"),
        "restore recreated an ignored file"
    );

    temp.put("build/out.bin", "artifact\n");
    temp.put("keep.txt", "two\n");
    snap.commit(None).unwrap();
    snap.restore("1").unwrap();
    assert_eq!(temp.read("keep.txt"), "one\n");
    assert!(temp.has("build/out.bin"), "restore deleted an ignored file");
    assert_eq!(temp.read("build/out.bin"), "artifact\n");
    assert!(temp.has(".tenon-snap"));
}

#[test]
fn restore_brings_back_old_content_and_removes_new_files() {
    let temp = Temp::new("restore");
    temp.put("a.txt", "alpha\n");
    let snap = Snap::open(&temp.path).unwrap();
    let first = snap.commit(None).unwrap();

    temp.put("a.txt", "alpha two\n");
    temp.put("later.txt", "later\n");
    snap.commit(None).unwrap();

    let done = snap.restore("1").unwrap();
    assert_eq!(done["step"], 1);
    assert_eq!(done["ref"], first["ref"]);
    assert_eq!(done["files"], 1);
    assert_eq!(temp.read("a.txt"), "alpha\n");
    assert!(!temp.has("later.txt"));

    let by_oid = snap.restore(first["ref"].as_str().unwrap()).unwrap();
    assert_eq!(by_oid["step"], 1);
    assert!(snap.restore("99").is_err());
}

#[test]
fn diff_names_the_changed_file() {
    let temp = Temp::new("diff");
    temp.put("a.txt", "one\n");
    let snap = Snap::open(&temp.path).unwrap();
    snap.commit(None).unwrap();

    temp.put("a.txt", "one\ntwo\n");
    temp.put("new.txt", "fresh\n");
    snap.commit(None).unwrap();

    let report = snap.diff("1", "2").unwrap();
    let files = report["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "{report}");
    let changed = files
        .iter()
        .find(|row| row["path"] == "a.txt")
        .expect("a.txt in diff");
    assert_eq!(changed["status"], "modified");
    assert_eq!(changed["added"], 1);
    assert_eq!(changed["deleted"], 0);
    let added = files
        .iter()
        .find(|row| row["path"] == "new.txt")
        .expect("new.txt in diff");
    assert_eq!(added["status"], "added");
    assert_eq!(report["insertions"], 2);
    assert_eq!(report["deletions"], 0);
}

#[test]
fn expire_keeps_the_recent_and_the_milestones() {
    let temp = Temp::new("expire");
    let snap = Snap::open(&temp.path).unwrap();
    for n in 1..=12 {
        temp.put("a.txt", &format!("value {n}\n"));
        snap.commit(None).unwrap();
    }
    assert_eq!(snap.list().unwrap()["count"], 12);

    let done = snap.expire(2, 5).unwrap();
    assert_eq!(done["kept"], 4, "{done}");
    assert_eq!(done["removed"], 8, "{done}");
    assert_eq!(done["count"], 4);
    assert_eq!(steps(&snap.list().unwrap()), vec![5, 10, 11, 12]);

    snap.restore("5").unwrap();
    assert_eq!(temp.read("a.txt"), "value 5\n");
    let again = snap.expire(2, 0).unwrap();
    assert_eq!(again["kept"], 3, "{again}");
    assert_eq!(steps(&snap.list().unwrap()), vec![5, 11, 12]);
}

#[test]
fn a_pack_round_trips_into_a_fresh_workspace() {
    let source = Temp::new("packsrc");
    let spill = Temp::new("packfile");
    let target = Temp::new("packdst");
    source.put("a.txt", "alpha\n");
    source.put("dir/b.txt", "beta\n");
    let origin = Snap::open(&source.path).unwrap();
    origin.commit(Some("first")).unwrap();
    source.put("a.txt", "alpha two\n");
    let second = origin.commit(Some("second")).unwrap();

    let pack = origin.pack(None).unwrap();
    assert!(!pack.bytes.is_empty());
    assert_eq!(pack.step, 2);
    assert_eq!(pack.head, second["ref"].as_str().unwrap());
    assert_eq!(pack.refs.len(), 2);

    let file = spill.path.join("snaps.pack");
    std::fs::write(&file, &pack.bytes).unwrap();
    let entries: Vec<(u64, String, PathBuf)> = pack
        .refs
        .iter()
        .map(|(step, oid)| (*step, oid.clone(), file.clone()))
        .collect();

    let restored = Snap::open(&target.path).unwrap();
    let done = restored.apply(&entries, None).unwrap();
    assert_eq!(done["applied"], 2);
    assert_eq!(done["step"], 2);
    assert_eq!(done["ref"], second["ref"]);
    assert_eq!(done["files"], 2);
    assert_eq!(target.read("a.txt"), source.read("a.txt"));
    assert_eq!(target.read("dir/b.txt"), source.read("dir/b.txt"));
    assert_eq!(steps(&restored.list().unwrap()), vec![1, 2]);
    assert_eq!(restored.head().unwrap().unwrap().0, 2);

    restored.restore("1").unwrap();
    assert_eq!(target.read("a.txt"), "alpha\n");
}

#[test]
fn pack_since_carries_only_the_newer_commits() {
    let temp = Temp::new("packsince");
    let snap = Snap::open(&temp.path).unwrap();
    for n in 1..=3 {
        temp.put("a.txt", &format!("value {n}\n"));
        snap.commit(None).unwrap();
    }

    let tail = snap.pack(Some(2)).unwrap();
    assert_eq!(tail.step, 3);
    assert_eq!(tail.refs.len(), 1);
    assert_eq!(tail.refs[0].0, 3);
    assert!(!tail.bytes.is_empty());

    let empty = snap.pack(Some(3)).unwrap();
    assert!(empty.bytes.is_empty());
    assert_eq!(empty.step, 0);
    assert_eq!(empty.head, "");
    assert!(empty.refs.is_empty());
}

#[test]
fn open_is_idempotent_on_an_existing_workspace() {
    let temp = Temp::new("reopen");
    temp.put("a.txt", "alpha\n");
    let snap = Snap::open(&temp.path).unwrap();
    snap.commit(None).unwrap();
    assert_eq!(snap.root(), temp.path);

    let again = Snap::open(&temp.path).unwrap();
    assert_eq!(again.list().unwrap()["count"], 1);
    assert_eq!(temp.read("a.txt"), "alpha\n");
    assert_eq!(again.head().unwrap().unwrap().0, 1);

    let fresh = Temp::new("emptyrepo");
    let none = Snap::open(&fresh.path).unwrap();
    assert!(none.head().unwrap().is_none());
    assert_eq!(none.list().unwrap()["count"], 0);
    assert_eq!(none.pack(None).unwrap().step, 0);
}
