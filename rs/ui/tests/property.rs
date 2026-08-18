mod support;

use support::fixture_model;
use tenon_ui::render;

fn lcg_stream(seed: u64) -> impl Iterator<Item = u64> {
    let mut state = seed;
    std::iter::from_fn(move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        Some(state >> 33)
    })
}

#[test]
fn never_exceeds_cols_and_always_fills_rows() {
    let model = fixture_model();
    let mut stream = lcg_stream(0xC0FFEE);
    for _ in 0..500 {
        let cols = 40 + (stream.next().unwrap() % 161) as usize;
        let rows = 10 + (stream.next().unwrap() % 51) as usize;
        let text = render(&model, cols, rows);
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(
            lines.len(),
            rows,
            "line count mismatch at cols={cols} rows={rows}"
        );
        for line in &lines {
            assert!(
                line.chars().count() <= cols,
                "line exceeded cols={cols} rows={rows}: {line:?}"
            );
            assert!(!line.contains('\t'), "tab found at cols={cols} rows={rows}");
        }
    }
}

#[test]
fn empty_model_never_panics_across_sizes() {
    let model = tenon_ui::UiModel::new();
    let mut stream = lcg_stream(1);
    for _ in 0..200 {
        let cols = 40 + (stream.next().unwrap() % 161) as usize;
        let rows = 10 + (stream.next().unwrap() % 51) as usize;
        let text = render(&model, cols, rows);
        assert_eq!(text.split('\n').count(), rows);
    }
}
