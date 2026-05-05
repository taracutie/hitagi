use search_relevance::parse_hunk_header;

#[test]
fn parses_hunk_header() {
    assert_eq!(parse_hunk_header("@@ -1,2 +10,4 @@"), Some((10, 4)));
}

#[test]
fn rejects_invalid_hunk_header() {
    assert_eq!(parse_hunk_header("not a hunk header"), None);
}
