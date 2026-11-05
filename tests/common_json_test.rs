//! 試験の道具 `common::json` の単体試験 (T.34.111)。
//!
//! 道具そのものが誤っていると、それを使う試験が全部「通るはずのないものが通る」に
//! なるので、取り出し方の約束をここで縛る。

mod common;

use common::json;

const ROW: &str =
    r#"{"id":7,"rtt_ms":12.5,"delta":-3,"note":"a,b}","none":null,"ok":true,"nested":{"id":9}}"#;

#[test]
fn json_uint_reads_the_first_key_and_stops_at_the_first_non_digit() {
    assert_eq!(json::uint(ROW, "id"), 7);
    // 入れ子の中でも、最初に出るものを読む (節を決めたいときは節だけを渡す)
    assert_eq!(json::uint(r#"{"a":{"id":9},"id":7}"#, "id"), 9);
}

#[test]
fn json_uint_opt_and_number_opt_read_null_as_none() {
    assert_eq!(json::uint_opt(ROW, "none"), None);
    assert_eq!(json::uint_opt(ROW, "id"), Some(7));
    assert_eq!(json::number_opt(ROW, "none"), None);
    assert_eq!(json::number_opt(ROW, "rtt_ms"), Some(12.5));
}

#[test]
fn json_number_reads_fractions_and_negatives() {
    assert_eq!(json::number(ROW, "rtt_ms"), 12.5);
    assert_eq!(json::number(ROW, "delta"), -3.0);
    assert_eq!(json::number(ROW, "id"), 7.0);
}

#[test]
fn json_raw_value_and_string_take_the_value_as_written() {
    assert_eq!(json::raw_value(ROW, "ok"), "true");
    assert_eq!(json::raw_value(ROW, "none"), "null");
    assert_eq!(json::raw_value(r#"{"k": 5 }"#, "k"), "5");
    // 文字列の中の `,` や `}` は値の一部
    assert_eq!(json::string(ROW, "note"), "a,b}");
}

#[test]
fn json_a_missing_key_fails_with_the_key_and_the_json() {
    let err = std::panic::catch_unwind(|| json::uint(ROW, "absent")).unwrap_err();
    let msg = err.downcast_ref::<String>().expect("文面が String");
    assert!(msg.contains("absent"), "{}", msg);
    assert!(msg.contains(ROW), "{}", msg);
}

#[test]
fn json_a_value_that_is_not_a_number_fails_with_the_key_and_the_json() {
    let err = std::panic::catch_unwind(|| json::uint(ROW, "note")).unwrap_err();
    let msg = err.downcast_ref::<String>().expect("文面が String");
    assert!(msg.contains("note"), "{}", msg);
    assert!(msg.contains(ROW), "{}", msg);
}

// ------------------------------------------------------------------
// オブジェクト・配列・行 (T.34.112)
// ------------------------------------------------------------------

#[test]
fn json_object_takes_the_nested_block_with_its_braces() {
    let doc = r#"{"memory":{"rss":1,"rings":{"used":2}},"x":3}"#;
    assert_eq!(
        json::object(doc, "memory"),
        r#"{"rss":1,"rings":{"used":2}}"#
    );
    assert_eq!(json::object(doc, "rings"), r#"{"used":2}"#);
}

#[test]
fn json_last_object_takes_the_last_one_of_the_same_name() {
    let doc = r#"{"cache":{"memory":{"a":1}},"memory":{"b":2}}"#;
    assert_eq!(json::object(doc, "memory"), r#"{"a":1}"#);
    assert_eq!(json::last_object(doc, "memory"), r#"{"b":2}"#);
}

#[test]
fn json_braces_and_commas_inside_strings_are_not_counted() {
    let doc = r#"{"o":{"s":"}{,][","t":"a\"}"},"rows":[{"n":"x,}"},{"n":"y"}],"keys":["a,b","c"]}"#;
    assert_eq!(json::object(doc, "o"), r#"{"s":"}{,][","t":"a\"}"}"#);
    assert_eq!(
        json::array_items(doc, "rows"),
        vec![r#"{"n":"x,}"}"#.to_string(), r#"{"n":"y"}"#.to_string()]
    );
    assert_eq!(json::string_array(doc, "keys"), vec!["a,b", "c"]);
}

#[test]
fn json_an_empty_array_has_no_items() {
    let doc = r#"{"rows":[],"keys":[],"samples":[]}"#;
    assert!(json::array_items(doc, "rows").is_empty());
    assert!(json::string_array(doc, "keys").is_empty());
    assert!(json::sample_rows(doc).is_empty());
}

#[test]
fn json_sample_rows_count_a_nested_array_as_one_column() {
    let doc = r#"{"samples":[[1,2,[3,4]],[5,null,[]]],"after":[[9]]}"#;
    assert_eq!(
        json::sample_rows(doc),
        vec![vec!["1", "2", "[3,4]"], vec!["5", "null", "[]"]]
    );
}

#[test]
fn json_find_item_takes_the_first_item_with_the_needle() {
    let doc = r#"{"connections":[{"kind":"forward","id":1},{"kind":"connect","id":2,"ms":{"a":1}},{"kind":"connect","id":3}]}"#;
    assert_eq!(
        json::find_item(doc, "connections", "\"kind\":\"connect\""),
        r#"{"kind":"connect","id":2,"ms":{"a":1}}"#
    );
}
