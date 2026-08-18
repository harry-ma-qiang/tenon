mod support;

use support::fixture_model;
use tenon_ui::render;

#[test]
fn expanding_a_tool_item_shows_its_text() {
    let mut model = fixture_model();
    model.expanded.clear();
    let folded = render(&model, 100, 30);
    assert!(folded.contains("[+] tool bash (3 lines)"));
    assert!(!folded.contains("Compiling tenon-ui"));

    model.expanded.insert(2);
    let expanded = render(&model, 100, 30);
    assert!(expanded.contains("[-] tool bash (3 lines)"));
    assert!(expanded.contains("Compiling tenon-ui"));
}

#[test]
fn folding_does_not_change_line_or_column_bounds() {
    let mut model = fixture_model();
    model.expanded.insert(2);
    model.expanded.insert(4);
    let text = render(&model, 100, 30);
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(lines.len(), 30);
    for line in &lines {
        assert!(line.chars().count() <= 100);
    }
}
