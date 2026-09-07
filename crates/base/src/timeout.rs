//! タイムアウトの `0` = 無期限 という約束事。
//!
//! `PROXY_TIMEOUT_SECS` も `PROXY_TUNNEL_IDLE_SECS` も **`0` は「無期限」** を意味する
//! (T10.6 で揃えた)。もう一つの案「1 秒未満は 1 秒に切り上げる」を採らなかったのは、
//! 切り上げると**無期限を表す手段が設定から無くなる**ため。`PROXY_TUNNEL_IDLE_SECS=0`
//! は前から無期限だったので、同じ `0` で意味が食い違わない方に寄せた。
//!
//! `std` の `set_read_timeout` / `set_write_timeout` / `connect_timeout` は
//! `Duration::ZERO` を `InvalidInput` (`cannot set a 0 duration timeout`) で断るので、
//! ソケットへ渡す前にここで `None` (= 無期限) に直す。

use std::time::Duration;

/// `set_read_timeout` / `set_write_timeout` に渡す形。`Duration::ZERO` は `None` (= 無期限)。
pub fn for_socket(timeout: Duration) -> Option<Duration> {
    (!timeout.is_zero()).then_some(timeout)
}

/// 2 つの上限のうち短いほう。**`Duration::ZERO` は無期限なので必ず相手が勝つ**
/// (`Duration::min` をそのまま使うと「無期限」が「0 秒で諦める」に化ける)。
pub fn shorter(a: Duration, b: Duration) -> Duration {
    match (a.is_zero(), b.is_zero()) {
        (true, _) => b,
        (_, true) => a,
        (false, false) => a.min(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_means_no_timeout() {
        assert_eq!(for_socket(Duration::ZERO), None);
        assert_eq!(
            for_socket(Duration::from_secs(30)),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn the_unlimited_side_never_wins_the_shorter_of_two() {
        let five = Duration::from_secs(5);
        let ten = Duration::from_secs(10);
        assert_eq!(shorter(five, ten), five);
        assert_eq!(shorter(ten, five), five);
        assert_eq!(shorter(Duration::ZERO, ten), ten);
        assert_eq!(shorter(ten, Duration::ZERO), ten);
        assert_eq!(shorter(Duration::ZERO, Duration::ZERO), Duration::ZERO);
    }
}
