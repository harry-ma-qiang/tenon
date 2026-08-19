use tenon_test_support::Temp;
use tenon_worker::fs::Fs;

fn ten_lines() -> String {
    (1..=10)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[test]
fn view_returns_a_range_and_a_whole_file() {
    let temp = Temp::new("view");
    temp.put("data.txt", &ten_lines());
    let fs = Fs::new(temp.path());

    let window = fs.view("data.txt", Some(2), Some(4)).unwrap();
    assert_eq!(window["path"], "data.txt");
    assert_eq!(window["start"], 2);
    assert_eq!(window["end"], 4);
    assert_eq!(window["lines"], 3);
    assert_eq!(window["total"], 10);
    assert_eq!(window["content"], "line 2\nline 3\nline 4");
    assert!(window.get("binary").is_none());

    let whole = fs.view("data.txt", None, None).unwrap();
    assert_eq!(whole["start"], 1);
    assert_eq!(whole["end"], 10);
    assert_eq!(whole["lines"], 10);
    assert_eq!(whole["content"], ten_lines().trim_end());
}

#[test]
fn view_rejects_a_directory() {
    let temp = Temp::new("viewdir");
    temp.put("sub/x.txt", "x\n");
    let fs = Fs::new(temp.path());
    assert!(fs.view("sub", None, None).is_err());
}

#[test]
fn write_then_view_round_trips() {
    let temp = Temp::new("write");
    let fs = Fs::new(temp.path());

    let first = fs.write("deep/nest/note.txt", "hello\nworld\n").unwrap();
    assert_eq!(first["path"], "deep/nest/note.txt");
    assert_eq!(first["bytes"], 12);
    assert_eq!(first["created"], true);

    let again = fs.write("deep/nest/note.txt", "hello\nworld\n").unwrap();
    assert_eq!(again["created"], false);

    let seen = fs.view("deep/nest/note.txt", None, None).unwrap();
    assert_eq!(seen["content"], "hello\nworld");
    assert_eq!(temp.read("deep/nest/note.txt"), "hello\nworld\n");
}

#[test]
fn edit_replaces_a_unique_string() {
    let temp = Temp::new("edit");
    temp.put("code.rs", "let a = 1;\nlet b = 2;\n");
    let fs = Fs::new(temp.path());

    let done = fs.edit("code.rs", "let b = 2;", "let b = 3;").unwrap();
    assert_eq!(done["replaced"], 1);
    assert_eq!(done["path"], "code.rs");
    assert_eq!(temp.read("code.rs"), "let a = 1;\nlet b = 3;\n");
}

#[test]
fn edit_fails_loudly_without_a_match() {
    let temp = Temp::new("editnone");
    temp.put("code.rs", "let a = 1;\n");
    let fs = Fs::new(temp.path());

    let error = fs.edit("code.rs", "let z = 9;", "x").unwrap_err();
    assert!(
        error.to_string().contains("no match for old in code.rs"),
        "{error}"
    );
    assert_eq!(temp.read("code.rs"), "let a = 1;\n");
    assert!(fs.edit("code.rs", "", "x").is_err());
}

#[test]
fn edit_fails_loudly_on_two_matches() {
    let temp = Temp::new("edittwo");
    temp.put("code.rs", "same\nsame\n");
    let fs = Fs::new(temp.path());

    let error = fs.edit("code.rs", "same", "other").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("old is not unique in code.rs: 2 matches"),
        "{error}"
    );
    assert_eq!(temp.read("code.rs"), "same\nsame\n");
}

#[test]
fn grep_walks_nested_dirs_and_skips_ignored_files() {
    let temp = Temp::new("grep");
    temp.put(".gitignore", "secret.txt\nbuild/\n");
    temp.put("a/b/hit.txt", "nothing\nneedle 42 here\n");
    temp.put("a/miss.txt", "nothing at all\n");
    temp.put("secret.txt", "needle 7\n");
    temp.put("build/out.txt", "needle 8\n");
    temp.put(".tenon-out/spill.log", "needle 9\n");
    temp.put(".tenon-snap/objects/x", "needle 10\n");
    let fs = Fs::new(temp.path());

    let found = fs.grep(r"needle \d+", None).unwrap();
    assert_eq!(found["count"], 1, "{found}");
    assert_eq!(found["truncated"], false);
    assert_eq!(found["matches"][0]["path"], "a/b/hit.txt");
    assert_eq!(found["matches"][0]["line"], 2);
    assert_eq!(found["matches"][0]["text"], "needle 42 here");

    let scoped = fs.grep("needle", Some("a/miss.txt")).unwrap();
    assert_eq!(scoped["count"], 0);
    assert!(fs.grep("needle(", None).is_err());
}

#[test]
fn glob_finds_rust_files() {
    let temp = Temp::new("glob");
    temp.put("top.rs", "");
    temp.put("src/main.rs", "");
    temp.put("src/deep/mod.rs", "");
    temp.put("notes.txt", "");
    temp.put("build/gen.rs", "");
    temp.put(".gitignore", "build/\n");
    let fs = Fs::new(temp.path());

    let found = fs.glob("**/*.rs").unwrap();
    let paths: Vec<String> = found["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        paths,
        ["src/deep/mod.rs", "src/main.rs", "top.rs"],
        "{paths:?}"
    );
    assert!(paths.contains(&"src/main.rs".to_string()), "{paths:?}");
    assert!(paths.contains(&"src/deep/mod.rs".to_string()), "{paths:?}");
    assert!(!paths.contains(&"notes.txt".to_string()), "{paths:?}");
    assert!(!paths.contains(&"build/gen.rs".to_string()), "{paths:?}");
    assert_eq!(found["count"], paths.len());
    assert_eq!(found["truncated"], false);

    let one = fs.glob("src/*.rs").unwrap();
    assert_eq!(one["count"], 1);
    assert_eq!(one["paths"][0], "src/main.rs");
}

#[test]
fn paths_outside_the_workspace_are_rejected() {
    let temp = Temp::new("escape");
    temp.put("inside.txt", "ok\n");
    let fs = Fs::new(temp.path());

    for path in ["../etc/passwd", "a/../../etc/passwd", "/etc/passwd"] {
        let error = fs.view(path, None, None).unwrap_err();
        assert!(
            error.to_string().contains("path outside workspace"),
            "{path}: {error}"
        );
        assert!(fs.write(path, "no").is_err());
        assert!(fs.edit(path, "a", "b").is_err());
        assert!(fs.grep("x", Some(path)).is_err());
    }

    let inside = temp.path().join("inside.txt");
    let absolute = fs.view(inside.to_str().unwrap(), None, None).unwrap();
    assert_eq!(absolute["path"], "inside.txt");
    assert_eq!(
        fs.view("./inside.txt", None, None).unwrap()["path"],
        "inside.txt"
    );
}
