use super::*;

#[test]
fn gutter_preserves_empty_and_trailing_lines() {
    for (content, expected) in [
        ("", vec![""]),
        ("\n", vec!["", ""]),
        ("first\n\nlast\n", vec!["first", "", "last", ""]),
        ("first\r\nlast\r\n", vec!["first\r", "last\r", ""]),
    ] {
        let lines = editor_lines_for(content);
        let actual: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert_eq!(actual, expected);
    }
}

#[test]
fn gutter_keeps_long_unicode_lines_for_layout_measurement() {
    let paragraph = "中文 text with spaces and a tab\t".repeat(100);
    let content = format!("{paragraph}\nnext");
    let lines = editor_lines_for(&content);
    assert_eq!(lines.row_count(), 2);
    assert_eq!(lines.row_data(0).unwrap().as_str(), paragraph);
    assert_eq!(lines.row_data(1).unwrap().as_str(), "next");
}
