//! `--check` の行の組み立ての単体試験 (T.34.60)。印字はせず、文字列だけを見る。

use super::*;

#[test]
fn quotes_come_off_only_simple_non_empty_strings() {
    // 4 つの形: 単純な文字列・空の文字列・`\` を含む文字列・文字列でない値
    assert_eq!(setting_value_for_print("\"auto\""), "auto");
    assert_eq!(setting_value_for_print("\"\""), "\"\"", "空は引用符のまま");
    assert_eq!(
        setting_value_for_print("\"a\\\"b\""),
        "\"a\\\"b\"",
        "エスケープを含むものは JSON のまま"
    );
    assert_eq!(setting_value_for_print("60"), "60");
    assert_eq!(setting_value_for_print("true"), "true");
    assert_eq!(setting_value_for_print("null"), "null");
    // 片側だけの引用符は外さない
    assert_eq!(setting_value_for_print("\"half"), "\"half");
    assert_eq!(setting_value_for_print("[\"a\",\"b\"]"), "[\"a\",\"b\"]");
}

#[test]
fn a_setting_line_pads_the_source_and_the_name() {
    assert_eq!(
        setting_line("default", "PROXY_TIMEOUT_SECS", "30"),
        "  default   PROXY_TIMEOUT_SECS             30"
    );
    // 長い名前は詰めずにそのまま
    let long = "A_VERY_LONG_SETTING_NAME_FOR_THE_TEST";
    assert_eq!(
        setting_line("env", long, "v"),
        format!("  env       {} v", long)
    );
}

#[test]
fn a_flag_line_pads_the_name_to_16() {
    assert_eq!(
        flag_line("ok", "tcp_info", "x"),
        "  [ok] tcp_info         x"
    );
    assert_eq!(
        flag_line("--", "cgroup_cpu_path", "y"),
        "  [--] cgroup_cpu_path  y"
    );
}

#[test]
fn every_capability_has_its_own_description() {
    let names = [
        "proc_syscall",
        "tcp_info",
        "cgroup_cpu",
        "cgroup_pressure",
        "ipv6_route",
        "proc_schedstat",
    ];
    let mut seen: Vec<&str> = names.iter().map(|n| capability_description(n)).collect();
    seen.push(capability_description("home_writable"));
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "{seen:?}");
    assert_eq!(
        capability_description("anything else"),
        "$HOME is writable (statistics file, blocklist)"
    );
}

#[test]
fn the_tls_line_reads_the_three_outcomes() {
    assert_eq!(
        tls_line(&Ok(Some("OpenSSL 3.0.13".into()))),
        (
            "ok",
            "OpenSSL 3.0.13 (read on the first https:// origin, not at start)".into()
        )
    );
    assert_eq!(
        tls_line(&Ok(None)),
        (
            "--",
            "libssl not found; https:// origins are unavailable (not part of the exit code)".into()
        )
    );
    assert_eq!(
        tls_line(&Err("CA file /x: No such file".into())),
        (
            "--",
            "CA file /x: No such file (not part of the exit code)".into()
        )
    );
}
