mod support;

use support::fixture_model;
use tenon_ui::render;

const CASES: &[(usize, usize, &str)] = &[
    (60, 20, "tests/golden/60x20.txt"),
    (100, 30, "tests/golden/100x30.txt"),
    (160, 40, "tests/golden/160x40.txt"),
];

#[test]
fn matches_golden_snapshots() {
    let model = fixture_model();
    for (cols, rows, path) in CASES {
        let actual = render(&model, *cols, *rows);
        let expected = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read golden {path}: {error}"));
        assert_eq!(
            actual, expected,
            "golden mismatch for {cols}x{rows}; regenerate with TENON_UI_BLESS=1"
        );
        let lines: Vec<&str> = actual.split('\n').collect();
        assert_eq!(lines.len(), *rows);
        for line in &lines {
            assert!(line.chars().count() <= *cols);
        }
    }
}

#[test]
fn bless_goldens_when_requested() {
    if std::env::var("TENON_UI_BLESS").is_err() {
        return;
    }
    let model = fixture_model();
    for (cols, rows, path) in CASES {
        let actual = render(&model, *cols, *rows);
        std::fs::write(path, actual).unwrap_or_else(|error| panic!("write golden {path}: {error}"));
    }
}
