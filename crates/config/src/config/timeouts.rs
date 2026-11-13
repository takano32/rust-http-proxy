//! 締め切りの既定と、**実効値を決める唯一の場所** (`PROXY_CONNECT_TIMEOUT_SECS` /
//! `PROXY_HEADER_TIMEOUT_SECS` / 1 要求目の読み取り / `PROXY_HE_STAGGER_MS`)。
//! `Config::new`・`Config::from_env`・再読込 (`Config::from_env` を回す) が全部ここを通る。

use std::time::Duration;

/// CONNECT のオリジン接続の締め切りの既定 (`PROXY_CONNECT_TIMEOUT_SECS`。T15.6、T23.3)。
///
/// **10 秒 → 5 秒 (2026-10-20、T23.3)**。雪像 4 枚 (tokyo の 10-15 / 10-17 / 10-20 と nagoya の
/// 10-08) の `connect_buckets` を数えると、**成功した確立が 5 秒を越えたものは 12 日ぶんの
/// 91,258 本で 0 本**・nagoya の 28 日ぶん (258,951 本、失敗も区間に入れた古い版) でも 22 本まで
/// (= 1 日 0.79 本の上限、`timeout` 18 本を引けば 4 本)。2.5〜5 秒も tokyo の 12 日で 4 本
/// (= 1 日 0.33 本) だけなので、5 秒で切っても落とす成功は 1 日 1 本に届かない。
/// 一方 `timeout` は tokyo の 62 時間に 3,869 本あり、1 件 10 秒を丸ごと待っていた
/// (**待ち 10.4 分/時 = 4.2 時間/日**)。5 秒にするとこれが半分 (**2.1 時間/日**) になる。
/// 遠いオリジンで SYN の再送 (1 秒 → 3 秒) が重なる 4 秒台は残るので、T15.6 が
/// 「5 秒は攻めすぎ」と言った根拠 (再送の重なり) は 5 秒では踏まない。
/// 戻す条件は「5 秒で切った接続のうち本当は繋がったはずのものが 1 日 1 本以上」(T23.99)。
///
/// **5 秒 → 3 秒 (2026-10-26、T28.43。利用者の決定 6)**。タスク 28 の洗い出しの見積もりで、
/// 縮む待ちは 4.6〜9.0 分/日、落とす成功は 0〜0.23 本/日 (T23.3 の「1 日 1 本未満」を満たす)。
/// forward の確立も T28.8 からこの値を使う。戻す条件は T23.3 と同じ (切った成功が 1 日 1 本以上)。
pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// 要求行とヘッダーを読み終えるまでの締め切りの既定 (`PROXY_HEADER_TIMEOUT_SECS`。T28.0)。
///
/// **10 秒**。tokyo の雪像 (`2026-10-25T203134Z`、1 時間ぶん) の `/profile` では、要求行から
/// `Host` まで (`client_read`) は CONNECT 1,105 本で**最大 0 ms** (ヘッダーは 1 つの塊で届く)。
/// accept から要求行が届くまでは計器に無い (`wait` の外) が、正規のクライアントは握手の直後に
/// 要求を送るので 1 RTT (tokyo の接続元は約 248 ms) で届く。10 秒は その 40 倍で、
/// 落ちた塊の再送 (RTO 0.2〜1 秒から倍々) が 3〜4 回重なっても間に合う。一方、沈黙と
/// 1 バイトずつの trickle (slowloris) が 1 本でワーカーと接続枠を握れる時間は 30 秒
/// (`PROXY_TIMEOUT_SECS`) から 10 秒に縮む。
pub(crate) const DEFAULT_HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// 要求行とヘッダーの締め切りの**実効値を決める唯一の場所** (T28.0。[`connect_timeout_for`] と同じ形)。
///
/// - `explicit` = `PROXY_HEADER_TIMEOUT_SECS` に書かれた値。書いてあれば**そのまま**
///   (`0` と明示すれば締め切り無し = 今までどおり `PROXY_TIMEOUT_SECS` の無通信の締め切りだけ)。
/// - 未設定なら [`DEFAULT_HEADER_TIMEOUT`] (10 秒)。`PROXY_TIMEOUT_SECS` が `0` でなく
///   10 秒より短ければ、その値に合わせる (= `min(10, timeout)`)。
/// - `timeout` が `0` (無期限。T10.6) でも、未設定なら **10 秒** (黙った接続を無期限に
///   置いておく理由は無い。置いておきたければ `0` と明示する)。
pub(crate) fn header_timeout_for(timeout: Duration, explicit: Option<Duration>) -> Duration {
    match explicit {
        Some(d) => d,
        None if timeout.is_zero() => DEFAULT_HEADER_TIMEOUT,
        None => timeout.min(DEFAULT_HEADER_TIMEOUT),
    }
}

/// 1 要求目の要求行の 1 バイト目を待つ読み取りタイムアウト (T28.0)。
///
/// 1 要求目は accept からこの長さで黙った接続を切る。`timeout` と `header` の小さい方で、
/// どちらかが `0` (無期限 / 締め切り無し) ならもう片方、両方 `0` なら `0` (無期限)。
/// 2 要求目以降の待ち (keep-alive のアイドル `PROXY_KEEPALIVE_SECS`) には使わない。
pub fn first_read_timeout(timeout: Duration, header: Duration) -> Duration {
    match (timeout.is_zero(), header.is_zero()) {
        (_, true) => timeout,
        (true, false) => header,
        (false, false) => timeout.min(header),
    }
}

/// `PROXY_HE_STAGGER_MS` の値を読む (T17.6)。数でないか範囲 ([`crate::net::STAGGER_RANGE_MS`]) の
/// 外なら `None` (呼ぶ側が既定に戻して warn する)。
pub(crate) fn parse_stagger_ms(v: &str) -> Option<Duration> {
    v.trim()
        .parse::<u64>()
        .ok()
        .filter(|ms| crate::net::STAGGER_RANGE_MS.contains(ms))
        .map(Duration::from_millis)
}

/// CONNECT のオリジン接続の締め切りの**実効値を決める唯一の場所** (T15.6)。
///
/// `Config::new`・`Config::from_env`・再読込 (`Config::from_env` を回す) が全部ここを通る。
///
/// - `explicit` = `PROXY_CONNECT_TIMEOUT_SECS` に書かれた値。書いてあれば**そのまま**
///   (`0` と明示すれば無期限)。
/// - 未設定なら [`DEFAULT_CONNECT_TIMEOUT`] (3 秒。T28.43)。ただし `PROXY_TIMEOUT_SECS`
///   (`timeout`) が `0` でなく 3 秒より短ければ、その値に合わせる (= `min(3, timeout)`。
///   全体の締め切りを 2 秒にした人の connect が 3 秒になるのはおかしい)。
/// - `timeout` が `0` (無期限。T10.6) でも、未設定の connect は **3 秒**
///   (つながらない相手を無期限に待つ意味は無い。無期限にしたければ `0` と明示する)。
pub fn connect_timeout_for(timeout: Duration, explicit: Option<Duration>) -> Duration {
    match explicit {
        Some(d) => d,
        // `0` = 無期限は「3 秒より短い」ではないので、既定の 3 秒に落とす
        None if timeout.is_zero() => DEFAULT_CONNECT_TIMEOUT,
        None => timeout.min(DEFAULT_CONNECT_TIMEOUT),
    }
}
