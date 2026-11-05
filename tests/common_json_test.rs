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
