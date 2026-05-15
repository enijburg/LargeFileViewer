use super::*;
mod tests {
    use super::{
        centered_top_line, classify_json_line, classify_xml_line, detect_csv_separator,
        format_json_for_display, format_xml_for_display, parse_csv_separator, skipped_prefix_len,
        JsonTokenClass, Viewer, XmlTokenClass,
    };
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn indexes_lines() {
        let offsets = Viewer::index_lines(b"a\nb\nlast");
        assert_eq!(offsets, vec![0, 2, 4]);
    }

    #[test]
    fn indexes_empty() {
        let offsets = Viewer::index_lines(b"");
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn centers_target_line_when_possible() {
        assert_eq!(centered_top_line(50, 20, 200), 40);
    }

    #[test]
    fn centers_target_line_with_small_targets() {
        assert_eq!(centered_top_line(3, 20, 200), 0);
    }

    #[test]
    fn centers_target_line_near_end() {
        assert_eq!(centered_top_line(199, 20, 200), 189);
    }

    #[test]
    fn finds_forward_and_backward() {
        let viewer = test_viewer_from_bytes(b"abc\ndef abc xyz abc");
        assert_eq!(viewer.find_forward(b"abc", 0), Some((0, 3)));
        assert_eq!(viewer.find_forward(b"abc", 4), Some((8, 11)));
        assert_eq!(viewer.find_backward(b"abc", 18), Some((16, 19)));
        assert_eq!(viewer.find_backward(b"missing", 18), None);
    }

    #[test]
    fn maps_offsets_to_lines() {
        let viewer = test_viewer_from_bytes(b"line1\nline2\nline3");
        assert_eq!(viewer.line_of_offset(0), 0);
        assert_eq!(viewer.line_of_offset(6), 1);
        assert_eq!(viewer.line_of_offset(12), 2);
    }

    fn with_temp_file(bytes: &[u8], f: impl FnOnce(PathBuf) -> Viewer) -> Viewer {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("large-file-viewer-test-{nonce}.txt"));
        fs::write(&path, bytes).expect("failed to write temp file");
        let viewer = f(path.clone());
        fs::remove_file(path).expect("failed to remove temp file");
        viewer
    }

    fn test_viewer_from_bytes(bytes: &[u8]) -> Viewer {
        with_temp_file(bytes, |path| {
            Viewer::open(path, 4, false, None, false, false, false).expect("failed to open viewer")
        })
    }

    #[test]
    fn indexes_csv_column_widths() {
        let widths = Viewer::index_csv_column_widths(b"a,bbb\ncccc,d", 4, b',');
        assert_eq!(widths, vec![4, 3]);
    }

    #[test]
    fn csv_mode_starts_below_pinned_header() {
        let viewer = test_viewer_with_options(b"h1,h2\nv1,v2\nv3,v4", 4, true);
        assert_eq!(viewer.top_line, 1);
    }

    #[test]
    fn csv_scroll_up_keeps_header_pinned() {
        let mut viewer = test_viewer_with_options(b"h1,h2\nv1,v2\nv3,v4", 4, true);
        viewer.scroll_up(10);
        assert_eq!(viewer.top_line, 1);
    }

    #[test]
    fn auto_detects_semicolon_separator() {
        assert_eq!(detect_csv_separator(b"h1;h2\na;b\nc;d"), b';');
    }

    #[test]
    fn parses_tab_separator_escape() {
        assert_eq!(parse_csv_separator(r"\t").expect("should parse"), b'\t');
    }

    fn test_viewer_with_options(bytes: &[u8], tab_width: usize, csv: bool) -> Viewer {
        with_temp_file(bytes, |path| {
            Viewer::open(path, tab_width, csv, None, false, false, false)
                .expect("failed to open viewer")
        })
    }

    #[test]
    fn classifies_xml_tags_and_attributes() {
        let classes = classify_xml_line(br#"<node attr="value">text</node>"#);
        assert_eq!(classes[0], XmlTokenClass::TagDelimiter);
        assert_eq!(classes[1], XmlTokenClass::TagName);
        assert_eq!(classes[6], XmlTokenClass::AttributeName);
        assert_eq!(classes[11], XmlTokenClass::AttributeValue);
        assert_eq!(classes[19], XmlTokenClass::Text);
    }

    #[test]
    fn classifies_xml_comments() {
        let classes = classify_xml_line(br#"<!-- comment --> <node/>"#);
        assert_eq!(classes[0], XmlTokenClass::Comment);
        assert_eq!(classes[10], XmlTokenClass::Comment);
        assert_eq!(classes[16], XmlTokenClass::Text);
        assert_eq!(classes[17], XmlTokenClass::TagDelimiter);
        assert_eq!(classes[18], XmlTokenClass::TagName);
    }

    #[test]
    fn skips_utf8_bom_prefix_on_first_line_only() {
        let bom_prefixed = [0xEF_u8, 0xBB_u8, 0xBF_u8, b'<', b'x', b'>'];
        assert_eq!(skipped_prefix_len(0, &bom_prefixed), 3);
        assert_eq!(skipped_prefix_len(1, &bom_prefixed), 0);
        assert_eq!(skipped_prefix_len(0, b"<x>"), 0);
    }

    #[test]
    fn classifies_json_tokens() {
        let json = br#"{"name":"bob","age":42,"ok":true,"v":null}"#;
        let classes = classify_json_line(json);

        let first_colon = json
            .iter()
            .position(|&b| b == b':')
            .expect("json should contain colon");
        let age_index = json
            .windows(3)
            .position(|w| w == b"age")
            .expect("json should contain age");
        let number_index = json
            .iter()
            .position(|&b| b == b'4')
            .expect("json should contain number");
        let true_index = json
            .windows(4)
            .position(|w| w == b"true")
            .expect("json should contain true");
        let null_index = json
            .windows(4)
            .position(|w| w == b"null")
            .expect("json should contain null");

        assert_eq!(classes[0], JsonTokenClass::Delimiter);
        assert_eq!(classes[1], JsonTokenClass::Key);
        assert_eq!(classes[first_colon], JsonTokenClass::Delimiter);
        assert_eq!(classes[age_index], JsonTokenClass::Key);
        assert_eq!(classes[number_index], JsonTokenClass::Number);
        assert_eq!(classes[true_index], JsonTokenClass::Keyword);
        assert_eq!(classes[null_index], JsonTokenClass::Keyword);
    }

    #[test]
    fn formats_single_line_xml_into_indented_lines() {
        let xml = br#"<root><parent><child/></parent><value>text</value></root>"#;
        let formatted = format_xml_for_display(xml);
        let formatted = String::from_utf8(formatted).expect("formatted xml should be utf8");
        let expected =
            "<root>\n  <parent>\n    <child/>\n  </parent>\n  <value>text</value>\n</root>";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn formats_xml_with_comments_and_header() {
        let xml = br#"<?xml version="1.0"?><root><!-- comment --><node/></root>"#;
        let formatted = format_xml_for_display(xml);
        let formatted = String::from_utf8(formatted).expect("formatted xml should be utf8");
        let expected = "<?xml version=\"1.0\"?>\n<root>\n  <!-- comment -->\n  <node/>\n</root>";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn formats_json_into_pretty_lines() {
        let json = br#"{"item":"Test","count":2,"ok":true,"arr":[1,2]}"#;
        let formatted = format_json_for_display(json).expect("json should format");
        let formatted = String::from_utf8(formatted).expect("formatted json should be utf8");
        let expected = "{\n  \"item\": \"Test\",\n  \"count\": 2,\n  \"ok\": true,\n  \"arr\": [\n    1,\n    2\n  ]\n}";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn invalid_json_does_not_format() {
        assert!(format_json_for_display(br#"{"bad":"unterminated}"#).is_none());
    }
}
