//! 問い合わせの数の読み (`num_param` / `offset_param`。T.34.86 で tests.rs から分けた)。

use super::*;

#[test]
fn numeric_parameters_fall_back_to_the_default() {
    assert_eq!(num_param(None, "n", 100, 500), 100);
    assert_eq!(num_param(Some("n=7"), "n", 100, 500), 7);
    assert_eq!(num_param(Some("n=9999"), "n", 100, 500), 500);
    assert_eq!(num_param(Some("n=0"), "n", 100, 500), 1);
    assert_eq!(num_param(Some("n=abc"), "n", 100, 500), 100);
    assert_eq!(str_param(Some("sort=host"), "sort"), "host");
    assert_eq!(str_param(Some("a=1&sort=misses"), "sort"), "misses");
    assert_eq!(str_param(None, "sort"), "");
}

/// `?offset=` は**下限 0** (`num_param` は `.clamp(1, max)` なので 0 を通せない。T15.0 (11))。
#[test]
fn the_offset_parameter_starts_at_zero() {
    assert_eq!(offset_param(None, 2000), 0);
    assert_eq!(offset_param(Some("offset=0"), 2000), 0);
    assert_eq!(offset_param(Some("offset=1"), 2000), 1);
    assert_eq!(offset_param(Some("n=200&offset=1500"), 2000), 1500);
    // 端は上だけ止める (`n=` と同じ方針)
    assert_eq!(offset_param(Some("offset=99999"), 2000), 2000);
    assert_eq!(offset_param(Some("offset=abc"), 2000), 0);
    assert_eq!(offset_param(Some("offset="), 2000), 0);
    // `?n=` の方は今までどおり 1 が下限
    assert_eq!(num_param(Some("offset=0"), "n", 200, 2000), 200);
}
