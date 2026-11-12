//! 試験の道具 `common::json` (と `common` のほかの小さな道具) の単体試験 (T34.111)。
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
// オブジェクト・配列・行 (T34.112)
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

/// キャッシュの置き場 (`cache_cfg` / `disk_only` が使う) には pid が付く。付かないと、
/// worktree を並べて回したとき別の実行が使っている最中の置き場を消す (T34.162)。
#[test]
fn fresh_dir_puts_the_pid_in_the_name() {
    let dir = common::fresh_dir("rhp-t34162-fresh");
    assert_eq!(
        dir.file_name().unwrap().to_str().unwrap(),
        format!("rhp-t34162-fresh-{}", std::process::id())
    );
    assert!(!dir.exists(), "空にして返す (作りはしない)");
}

/// 試験の置き場の名前 (`rhp-…-<pid>` / `shp-…-<pid>`) だけを pid つきで読む (T36.24)。
#[test]
fn scratch_pid_reads_only_test_dirs_ending_in_a_pid() {
    assert_eq!(common::scratch_pid("rhp-t1415-check-1234"), Some(1234));
    assert_eq!(common::scratch_pid("shp-it-304-age-disk-77"), Some(77));
    assert_eq!(common::scratch_pid("rhp-t1415-check"), None, "pid が無い");
    assert_eq!(common::scratch_pid("rhp-t1415-12x"), None, "数字だけでない");
    assert_eq!(common::scratch_pid("rhp-"), None);
    assert_eq!(
        common::scratch_pid("rust-http-proxy-cache-1"),
        None,
        "試験の名前でない"
    );
    assert_eq!(common::scratch_pid("systemd-private-1"), None);
}

/// 前の実行が残した置き場は、pid のプロセスが居なければ消し、居れば残す (T36.24)。
#[cfg(target_os = "linux")]
#[test]
fn sweep_dead_removes_dirs_of_gone_pids_and_keeps_live_ones() {
    let root = std::env::temp_dir().join(format!("t3624-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    // 居ない pid: 上限 (pid_max の最大 2^22) の近くで、`/proc` に無いものを探す
    let dead = (4_000_000u32..4_194_304)
        .find(|p| !std::path::Path::new(&format!("/proc/{p}")).exists())
        .expect("空いている pid");
    let me = std::process::id();
    let gone = root.join(format!("rhp-t3624-a-{dead}"));
    let alive = root.join(format!("rhp-t3624-b-{me}"));
    let other = root.join(format!("other-{dead}"));
    let no_pid = root.join("shp-t3624-c");
    for d in [&gone, &alive, &other, &no_pid] {
        std::fs::create_dir_all(d.join("sub")).unwrap();
    }
    assert_eq!(common::sweep_dead(&root), 1);
    assert!(!gone.exists(), "居ない pid の置き場が残った");
    assert!(alive.exists(), "生きている pid の置き場を消した");
    assert!(other.exists(), "試験の名前でないものを消した");
    assert!(no_pid.exists(), "pid の付かないものを消した");
    let _ = std::fs::remove_dir_all(&root);
}

/// `read_response` は `Content-Length` の名前を大小を区別せずに読む (オリジン側の
/// 読み取りと同じ。T34.163)。区別すると本文を読まずに返し、次の応答の頭がずれる。
#[test]
fn read_response_reads_content_length_in_any_case() {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\nabc")
            .unwrap();
        s.write_all(b"HTTP/1.1 204 No Content\r\nCONTENT-LENGTH:0\r\n\r\n")
            .unwrap();
    });
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let (head, body) = common::read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"abc");
    let (head, body) = common::read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 204 No Content"), "{}", head);
    assert!(body.is_empty());
    server.join().unwrap();
}
