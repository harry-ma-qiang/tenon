use crate::{err, Result};
use git2::build::CheckoutBuilder;
use git2::{
    Buf, Delta, IndexAddOption, ObjectType, Oid, Repository, RepositoryInitOptions, Signature,
    Tree, TreeWalkMode, TreeWalkResult,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

const SNAP_HEAD: &str = "refs/heads/snap";
const EXCLUDE: &str = ".tenon-snap/\n.tenon-out/\n.git/\n";

pub struct Pack {
    pub bytes: Vec<u8>,
    pub head: String,
    pub step: u64,
    pub refs: Vec<(u64, String)>,
}

pub struct Snap {
    root: PathBuf,
}

impl Snap {
    /// Names the workspace without touching it. Every method opens (and, the
    /// first time, initialises) the repository itself, so a worker that never
    /// snapshots never writes a `.tenon-snap` into whatever directory it was
    /// started in.
    pub fn at(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    pub fn open(root: &Path) -> Result<Self> {
        let snap = Self {
            root: root.to_path_buf(),
        };
        snap.repo()?;
        Ok(snap)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn commit(&self, label: Option<&str>) -> Result<Value> {
        let repo = self.repo()?;
        let mut index = repo.index().map_err(|e| oops("open index", e))?;
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .map_err(|e| oops("stage", e))?;
        index
            .update_all(["*"].iter(), None)
            .map_err(|e| oops("stage removals", e))?;
        index.write().map_err(|e| oops("write index", e))?;
        let tree_id = index.write_tree().map_err(|e| oops("write tree", e))?;
        let tree = repo.find_tree(tree_id).map_err(|e| oops("find tree", e))?;
        let step = snaps(&repo)?.last().map(|row| row.0).unwrap_or(0) + 1;
        let text = label
            .map(str::to_string)
            .unwrap_or_else(|| format!("step {step}"));
        let who =
            Signature::now("tenon", "worker@tenon.local").map_err(|e| oops("signature", e))?;
        let oid = repo
            .commit(None, &who, &who, &text, &tree, &[])
            .map_err(|e| oops("commit", e))?;
        repo.reference(&format!("refs/snaps/{step}"), oid, true, &text)
            .map_err(|e| oops("set snap ref", e))?;
        repo.reference(SNAP_HEAD, oid, true, &text)
            .map_err(|e| oops("set snap head", e))?;
        let at = repo
            .find_commit(oid)
            .map_err(|e| oops("find commit", e))?
            .time()
            .seconds()
            * 1000;
        Ok(json!({
            "ref": oid.to_string(),
            "step": step,
            "label": text,
            "files": count_files(&tree)?,
            "at": at,
        }))
    }

    pub fn list(&self) -> Result<Value> {
        let repo = self.repo()?;
        let mut rows = Vec::new();
        for (step, oid) in snaps(&repo)? {
            let commit = repo.find_commit(oid).map_err(|e| oops("find commit", e))?;
            rows.push(json!({
                "ref": oid.to_string(),
                "step": step,
                "label": commit.message().unwrap_or("").trim_end().to_string(),
                "at": commit.time().seconds() * 1000,
            }));
        }
        let count = rows.len();
        Ok(json!({"snapshots": rows, "count": count}))
    }

    pub fn restore(&self, reference: &str) -> Result<Value> {
        let repo = self.repo()?;
        self.checkout(&repo, reference)
    }

    pub fn diff(&self, a: &str, b: &str) -> Result<Value> {
        let repo = self.repo()?;
        let tree_a = self.tree_of(&repo, a)?;
        let tree_b = self.tree_of(&repo, b)?;
        let diff = repo
            .diff_tree_to_tree(Some(&tree_a), Some(&tree_b), None)
            .map_err(|e| oops("diff", e))?;
        let mut counts: HashMap<String, (u64, u64)> = HashMap::new();
        diff.foreach(
            &mut |_, _| true,
            None,
            None,
            Some(&mut |delta, _hunk, line| {
                let row = counts.entry(delta_path(&delta)).or_insert((0, 0));
                match line.origin() {
                    '+' => row.0 += 1,
                    '-' => row.1 += 1,
                    _ => {}
                }
                true
            }),
        )
        .map_err(|e| oops("walk diff", e))?;
        let mut files = Vec::new();
        for delta in diff.deltas() {
            let path = delta_path(&delta);
            let (added, deleted) = counts.get(&path).copied().unwrap_or((0, 0));
            files.push(json!({
                "path": path,
                "status": status_name(delta.status()),
                "added": added,
                "deleted": deleted,
            }));
        }
        let stats = diff.stats().map_err(|e| oops("diff stats", e))?;
        Ok(json!({
            "a": a,
            "b": b,
            "files": files,
            "insertions": stats.insertions(),
            "deletions": stats.deletions(),
        }))
    }

    pub fn pack(&self, since: Option<u64>) -> Result<Pack> {
        let repo = self.repo()?;
        let rows: Vec<(u64, Oid)> = snaps(&repo)?
            .into_iter()
            .filter(|row| since.is_none_or(|floor| row.0 > floor))
            .collect();
        let Some(last) = rows.last().copied() else {
            return Ok(Pack {
                bytes: Vec::new(),
                head: String::new(),
                step: 0,
                refs: Vec::new(),
            });
        };
        let mut builder = repo.packbuilder().map_err(|e| oops("packbuilder", e))?;
        for (_, oid) in &rows {
            builder
                .insert_commit(*oid)
                .map_err(|e| oops("pack commit", e))?;
        }
        let mut buf = Buf::new();
        builder
            .write_buf(&mut buf)
            .map_err(|e| oops("write pack", e))?;
        Ok(Pack {
            bytes: buf.to_vec(),
            head: last.1.to_string(),
            step: last.0,
            refs: rows
                .iter()
                .map(|(step, oid)| (*step, oid.to_string()))
                .collect(),
        })
    }

    pub fn apply(&self, packs: &[(u64, String, PathBuf)], head: Option<&str>) -> Result<Value> {
        let repo = self.repo()?;
        let mut seen: Vec<&PathBuf> = Vec::new();
        for (_, _, path) in packs {
            if seen.contains(&path) {
                continue;
            }
            seen.push(path);
            let bytes = std::fs::read(path)?;
            if bytes.is_empty() {
                continue;
            }
            let odb = repo.odb().map_err(|e| oops("open odb", e))?;
            let mut writer = odb.packwriter().map_err(|e| oops("packwriter", e))?;
            writer.write_all(&bytes)?;
            writer.commit().map_err(|e| oops("apply pack", e))?;
        }
        for (step, oid, _) in packs {
            let target = Oid::from_str(oid).map_err(|e| oops("read pack oid", e))?;
            repo.reference(&format!("refs/snaps/{step}"), target, true, "apply")
                .map_err(|e| oops("set snap ref", e))?;
        }
        let target = match head {
            Some(name) => name.to_string(),
            None => snaps(&repo)?
                .last()
                .map(|row| row.0.to_string())
                .ok_or_else(|| err("apply: no snapshots"))?,
        };
        let done = self.checkout(&repo, &target)?;
        Ok(json!({
            "applied": packs.len(),
            "ref": done["ref"],
            "step": done["step"],
            "files": done["files"],
        }))
    }

    pub fn expire(&self, keep_last: usize, milestone_every: usize) -> Result<Value> {
        let repo = self.repo()?;
        let rows = snaps(&repo)?;
        let pinned = repo
            .find_reference(SNAP_HEAD)
            .ok()
            .and_then(|entry| entry.target());
        let total = rows.len();
        let mut removed = 0usize;
        for (index, (step, oid)) in rows.iter().enumerate() {
            let recent = index + keep_last >= total;
            let milestone = milestone_every > 0 && step % milestone_every as u64 == 0;
            if recent || milestone || pinned == Some(*oid) {
                continue;
            }
            repo.find_reference(&format!("refs/snaps/{step}"))
                .map_err(|e| oops("find snap ref", e))?
                .delete()
                .map_err(|e| oops("delete snap ref", e))?;
            removed += 1;
        }
        let kept = total - removed;
        Ok(json!({"kept": kept, "removed": removed, "count": kept}))
    }

    pub fn head(&self) -> Result<Option<(u64, String)>> {
        let repo = self.repo()?;
        Ok(snaps(&repo)?
            .last()
            .map(|(step, oid)| (*step, oid.to_string())))
    }

    fn repo(&self) -> Result<Repository> {
        let dir = self.root.join(crate::SNAP_DIR);
        let mut opts = RepositoryInitOptions::new();
        opts.bare(false)
            .no_dotgit_dir(true)
            .mkpath(true)
            .workdir_path(&self.root);
        let repo = Repository::init_opts(&dir, &opts).map_err(|e| oops("init", e))?;
        let mut config = repo.config().map_err(|e| oops("config", e))?;
        config
            .set_str("user.name", "tenon")
            .map_err(|e| oops("config user.name", e))?;
        config
            .set_str("user.email", "worker@tenon.local")
            .map_err(|e| oops("config user.email", e))?;
        let info = repo.path().join("info");
        let exclude = info.join("exclude");
        if std::fs::read_to_string(&exclude).ok().as_deref() != Some(EXCLUDE) {
            std::fs::create_dir_all(&info)?;
            std::fs::write(&exclude, EXCLUDE)?;
        }
        Ok(repo)
    }

    fn checkout(&self, repo: &Repository, reference: &str) -> Result<Value> {
        let (oid, step) = locate(repo, reference)?;
        let commit = repo.find_commit(oid).map_err(|e| oops("find commit", e))?;
        let tree = commit.tree().map_err(|e| oops("read tree", e))?;
        let mut opts = CheckoutBuilder::new();
        opts.force().remove_untracked(true);
        repo.checkout_tree(commit.as_object(), Some(&mut opts))
            .map_err(|e| oops("checkout", e))?;
        let mut index = repo.index().map_err(|e| oops("open index", e))?;
        index.read_tree(&tree).map_err(|e| oops("reset index", e))?;
        index.write().map_err(|e| oops("write index", e))?;
        repo.reference(SNAP_HEAD, oid, true, "restore")
            .map_err(|e| oops("set snap head", e))?;
        repo.set_head(SNAP_HEAD).map_err(|e| oops("set head", e))?;
        Ok(json!({"ref": oid.to_string(), "step": step, "files": count_files(&tree)?}))
    }

    fn tree_of<'a>(&self, repo: &'a Repository, reference: &str) -> Result<Tree<'a>> {
        let (oid, _) = locate(repo, reference)?;
        repo.find_commit(oid)
            .map_err(|e| oops("find commit", e))?
            .tree()
            .map_err(|e| oops("read tree", e))
    }
}

fn snaps(repo: &Repository) -> Result<Vec<(u64, Oid)>> {
    let mut rows: Vec<(u64, Oid)> = Vec::new();
    let refs = repo
        .references_glob("refs/snaps/*")
        .map_err(|e| oops("list snap refs", e))?;
    for entry in refs.flatten() {
        let step = entry
            .name()
            .and_then(|name| name.rsplit('/').next())
            .and_then(|tail| tail.parse::<u64>().ok());
        if let (Some(step), Some(oid)) = (step, entry.target()) {
            rows.push((step, oid));
        }
    }
    rows.sort_by_key(|row| row.0);
    Ok(rows)
}

fn locate(repo: &Repository, reference: &str) -> Result<(Oid, u64)> {
    let rows = snaps(repo)?;
    if let Ok(step) = reference.parse::<u64>() {
        return rows
            .iter()
            .find(|row| row.0 == step)
            .map(|row| (row.1, row.0))
            .ok_or_else(|| err(format!("no snapshot for step {step}")));
    }
    let commit = repo
        .revparse_single(reference)
        .map_err(|e| oops(&format!("resolve {reference}"), e))?
        .peel_to_commit()
        .map_err(|e| oops(&format!("peel {reference}"), e))?;
    let oid = commit.id();
    let step = rows
        .iter()
        .find(|row| row.1 == oid)
        .map(|row| row.0)
        .unwrap_or(0);
    Ok((oid, step))
}

fn count_files(tree: &Tree) -> Result<usize> {
    let mut total = 0usize;
    tree.walk(TreeWalkMode::PreOrder, |_, entry| {
        if entry.kind() == Some(ObjectType::Blob) {
            total += 1;
        }
        TreeWalkResult::Ok
    })
    .map_err(|e| oops("walk tree", e))?;
    Ok(total)
}

fn delta_path(delta: &git2::DiffDelta) -> String {
    delta
        .new_file()
        .path()
        .or_else(|| delta.old_file().path())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn status_name(status: Delta) -> &'static str {
    match status {
        Delta::Added => "added",
        Delta::Deleted => "deleted",
        Delta::Modified => "modified",
        Delta::Renamed => "renamed",
        Delta::Copied => "copied",
        Delta::Ignored => "ignored",
        Delta::Untracked => "untracked",
        Delta::Typechange => "typechange",
        Delta::Unreadable => "unreadable",
        Delta::Conflicted => "conflicted",
        Delta::Unmodified => "unmodified",
    }
}

fn oops(what: &str, error: git2::Error) -> crate::Error {
    err(format!("snap {what}: {error}"))
}
