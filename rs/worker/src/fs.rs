use crate::{err, Result};
use globset::GlobBuilder;
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::path::{Component, Path, PathBuf};

const VIEW_WINDOW: usize = 2000;
const VIEW_DEFAULT: usize = 200;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const GREP_MAX: usize = 500;
const GLOB_MAX: usize = 1000;
const SKIP: [&str; 3] = [crate::SNAP_DIR, crate::OUT_DIR, ".git"];

pub struct Fs {
    root: PathBuf,
}

impl Fs {
    pub fn new(root: &Path) -> Self {
        Self {
            root: normalize(root),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn view(&self, path: &str, start: Option<usize>, end: Option<usize>) -> Result<Value> {
        let target = self.resolve(path)?;
        if target.is_dir() {
            return Err(err(format!("{path} is a directory")));
        }
        let raw = std::fs::read(&target)?;
        let (text, binary) = match String::from_utf8(raw) {
            Ok(text) => (text, false),
            Err(bad) => (String::from_utf8_lossy(bad.as_bytes()).into_owned(), true),
        };
        let rows = split_lines(&text);
        let total = rows.len();
        let first = start.unwrap_or(1).max(1);
        let mut want = end.unwrap_or(first + VIEW_DEFAULT - 1).max(first);
        if want - first + 1 > VIEW_WINDOW {
            want = first + VIEW_WINDOW - 1;
        }
        let last = want.min(total);
        let slice = if first > total {
            &rows[0..0]
        } else {
            &rows[first - 1..last]
        };
        let mut out = Map::new();
        out.insert("path".into(), json!(self.relative(&target)));
        out.insert("start".into(), json!(first));
        out.insert("end".into(), json!(last));
        out.insert("lines".into(), json!(slice.len()));
        out.insert("total".into(), json!(total));
        out.insert("content".into(), json!(slice.join("\n")));
        if binary {
            out.insert("binary".into(), json!(true));
        }
        Ok(Value::Object(out))
    }

    pub fn write(&self, path: &str, content: &str) -> Result<Value> {
        let target = self.resolve(path)?;
        if target.is_dir() {
            return Err(err(format!("{path} is a directory")));
        }
        let created = !target.exists();
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, content)?;
        Ok(json!({
            "path": self.relative(&target),
            "bytes": content.len(),
            "created": created,
        }))
    }

    pub fn edit(&self, path: &str, old: &str, new: &str) -> Result<Value> {
        if old.is_empty() {
            return Err(err(format!("old is empty in {path}")));
        }
        let target = self.resolve(path)?;
        let raw = std::fs::read(&target)?;
        let text = String::from_utf8(raw).map_err(|_| err(format!("{path} is not utf8")))?;
        let hits = text.matches(old).count();
        if hits == 0 {
            return Err(err(format!("no match for old in {path}")));
        }
        if hits > 1 {
            return Err(err(format!("old is not unique in {path}: {hits} matches")));
        }
        let next = text.replacen(old, new, 1);
        std::fs::write(&target, &next)?;
        Ok(json!({
            "path": self.relative(&target),
            "replaced": 1,
            "bytes": next.len(),
        }))
    }

    pub fn grep(&self, pattern: &str, path: Option<&str>) -> Result<Value> {
        let rule = Regex::new(pattern).map_err(|error| err(format!("bad pattern: {error}")))?;
        let target = self.resolve(path.unwrap_or("."))?;
        let mut matches: Vec<Value> = Vec::new();
        let mut truncated = false;
        for file in self.walk(&target) {
            if truncated {
                break;
            }
            let Some(text) = read_text(&file) else {
                continue;
            };
            let name = self.relative(&file);
            for (index, line) in text.lines().enumerate() {
                if !rule.is_match(line) {
                    continue;
                }
                if matches.len() >= GREP_MAX {
                    truncated = true;
                    break;
                }
                matches.push(json!({"path": name, "line": index + 1, "text": line}));
            }
        }
        let count = matches.len();
        Ok(json!({"matches": matches, "count": count, "truncated": truncated}))
    }

    pub fn glob(&self, pattern: &str) -> Result<Value> {
        let rule = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| err(format!("bad glob: {error}")))?
            .compile_matcher();
        let mut paths: Vec<String> = Vec::new();
        let mut truncated = false;
        for file in self.walk(self.root.as_path()) {
            let name = self.relative(&file);
            if !rule.is_match(name.as_str()) {
                continue;
            }
            if paths.len() >= GLOB_MAX {
                truncated = true;
                break;
            }
            paths.push(name);
        }
        paths.sort();
        let count = paths.len();
        Ok(json!({"paths": paths, "count": count, "truncated": truncated}))
    }

    fn walk(&self, target: &Path) -> Vec<PathBuf> {
        if target.is_file() {
            return vec![target.to_path_buf()];
        }
        let mut found: Vec<PathBuf> = Vec::new();
        let walker = WalkBuilder::new(target)
            .hidden(false)
            .parents(false)
            .require_git(false)
            .filter_entry(|entry| !SKIP.contains(&entry.file_name().to_string_lossy().as_ref()))
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if entry.metadata().map(|data| data.len()).unwrap_or(0) > MAX_FILE_BYTES {
                continue;
            }
            found.push(entry.into_path());
        }
        found.sort();
        found
    }

    fn relative(&self, target: &Path) -> String {
        target
            .strip_prefix(&self.root)
            .unwrap_or(target)
            .to_string_lossy()
            .into_owned()
    }

    fn resolve(&self, path: &str) -> Result<PathBuf> {
        let given = Path::new(path);
        let joined = if given.is_absolute() {
            given.to_path_buf()
        } else {
            self.root.join(given)
        };
        let full = normalize(&joined);
        if !full.starts_with(&self.root) {
            return Err(err(format!("path outside workspace: {path}")));
        }
        Ok(full)
    }
}

fn read_text(path: &Path) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    String::from_utf8(raw).ok()
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut rows: Vec<&str> = text.split('\n').collect();
    if rows.last() == Some(&"") {
        rows.pop();
    }
    rows
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
