//! ファイルの扱い (追記・切り詰め・読み戻し・`/daily` の切り方) の試験。

use super::*;

/// 上限 2 MiB を越えたら古い行から捨てる。
#[test]
fn the_file_is_capped_and_drops_the_oldest_lines() {
    let path = tmp("cap");
    // 512 B ちょうどの行で埋める (2 MiB = 4,096 行)
    let filler = format!("{{\"t\":1,\"x\":\"{}\"}}", "y".repeat(MAX_LINE - 14));
    assert_eq!(filler.len(), MAX_LINE);
    {
        let mut f = fs::File::create(&path).unwrap();
        for _ in 0..4_200 {
            f.write_all(filler.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
    }
    append(&path, "{\"t\":2}").unwrap();
    let size = fs::metadata(&path).unwrap().len();
    assert!(size <= MAX_BYTES, "{} B", size);
    let rows = lines(&path);
    assert_eq!(rows.last().map(String::as_str), Some("{\"t\":2}"));
    println!("daily file: {} lines in {} B", rows.len(), size);
    let _ = fs::remove_file(&path);
}

/// `/daily` の中身は 256 KiB に収まる (行が何行あっても)。
#[test]
fn the_endpoint_body_stays_under_the_limit() {
    let path = tmp("read");
    let filler = format!("{{\"t\":1,\"x\":\"{}\"}}", "y".repeat(MAX_LINE - 14));
    {
        let mut f = fs::File::create(&path).unwrap();
        for _ in 0..4_096 {
            f.write_all(filler.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
    }
    // 大域の口を使わずに読む側だけ確かめる (recent は WRITER を見るため)
    let all = read_lines(&path);
    assert_eq!(all.len(), 4_096);
    let (shown, truncated) = newest_within(all, MAX_DAYS, MAX_JSON);
    assert!(truncated, "4,096 行は 240 KiB に入らない");
    let used: usize = shown.iter().map(|l| l.len() + 1).sum();
    let taken = shown.len();
    assert!(used <= MAX_JSON && used + 1024 <= 256 * 1024, "{} B", used);
    println!("/daily: {} 行 {} B (上限 {} B)", taken, used, MAX_JSON);
    // 1 年ぶん (365 行) は切られない
    assert!(taken >= DEFAULT_DAYS);
    let _ = fs::remove_file(&path);
}

#[test]
fn broken_lines_are_ignored() {
    let path = tmp("broken");
    fs::write(
        &path,
        "not json\n{\"t\":1728000000}\n\n{\"half\":\n{\"t\":1728086400}\n",
    )
    .unwrap();
    let rows = lines(&path);
    assert_eq!(rows.len(), 2, "{:?}", rows);
    assert_eq!(last_day(&path), 1_728_086_400 / DAY);
    assert_eq!(last_day(Path::new("/nonexistent/rhp-t1420")), 0);
    let _ = fs::remove_file(&path);
}

/// `/daily` の切り方 (T.34.46): 新しい方から件数で切る・大きさで切る・切らない。並びは古い順。
#[test]
fn newest_within_cuts_by_count_or_by_size() {
    let all = || -> Vec<String> { (0..5).map(|i| format!("{{\"t\":{}}}", i)).collect() };
    // 1 行 7 B + 改行 = 8 B
    let (rows, cut) = newest_within(all(), 10, 1 << 20);
    assert_eq!((rows.len(), cut), (5, false), "切らない");
    assert_eq!(rows[0], "{\"t\":0}");
    let (rows, cut) = newest_within(all(), 2, 1 << 20);
    assert_eq!(
        rows,
        vec!["{\"t\":3}", "{\"t\":4}"],
        "件数で切ると新しい方が残る"
    );
    assert!(cut);
    // ちょうど 3 行ぶん (24 B) は入り、1 B 足りなければ 2 行
    let (rows, cut) = newest_within(all(), 5, 24);
    assert_eq!((rows.len(), cut), (3, true));
    assert_eq!(rows[0], "{\"t\":2}");
    let (rows, cut) = newest_within(all(), 3, 24);
    assert_eq!((rows.len(), cut), (3, true), "件数でも切った");
    let (rows, _) = newest_within(all(), 5, 23);
    assert_eq!(rows.len(), 2, "大きさで切る");
    let (rows, cut) = newest_within(Vec::new(), 5, 24);
    assert_eq!((rows.len(), cut), (0, false));
}

/// 行として読む境目: 大きさは [`MAX_LINE`] の 2 倍まで、波括弧で囲まれ、制御文字を含まない。
#[test]
fn is_line_and_field_draw_their_lines() {
    let at_cap = format!("{{\"x\":\"{}\"}}", "y".repeat(MAX_LINE * 2 - 8));
    assert_eq!(at_cap.len(), MAX_LINE * 2);
    assert!(is_line(&at_cap));
    let over = format!("{{\"x\":\"{}\"}}", "y".repeat(MAX_LINE * 2 - 7));
    assert!(!is_line(&over));
    assert!(!is_line("{\"t\":1"));
    assert!(!is_line("\"t\":1}"));
    assert!(!is_line("{\"t\":\"a\tb\"}"));
    assert!(is_line("{}"));
    // `field` は `,` か `}` までを取り、引用符と空白を外す
    let l = "{\"t\":1728000000,\"day\":\"2024-10-04\",\"last\": 7 }";
    assert_eq!(field(l, "t"), Some("1728000000"));
    assert_eq!(field(l, "day"), Some("2024-10-04"));
    assert_eq!(field(l, "last"), Some("7"));
    assert_eq!(field(l, "missing"), None);
    // 鍵は `"鍵":` で引くので、別の鍵の末尾には当たらない
    assert_eq!(field("{\"xt\":1,\"t\":2}", "t"), Some("2"));
}
