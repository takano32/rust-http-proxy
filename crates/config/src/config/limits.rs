//! 同時接続・スレッド・記述子の上限と、その既定の決め方 (`PROXY_MAX_CONNS=auto` など)。
//! どれも入力を引数で受ける純粋な計算で、環境に触るのは [`default_max_conns`] (`RLIMIT_NOFILE`) と
//! [`default_max_threads`] (コア数) だけ。

/// 1 接続が最悪で使う記述子の数: クライアント 1 + オリジン 1 + 素通し中のパイプ 2 (`splice`)。
pub const FDS_PER_CONN: u64 = 4;

/// 接続以外で使う記述子の予備: 待ち受け (最大 2) + epoll + inotify + 状態ファイル +
/// ブロックリストの取得 + キャッシュのディスク I/O + 標準入出力。多めに見て 64。
const NOFILE_RESERVE: u64 = 64;

/// CONNECT の最初のバイトから SNI を覗くポートの既定 (`PROXY_PEEK_SNI`、既定 `on`。T14.38)。
///
/// TLS の既定のポート。**ここ以外では覗かない** (CONNECT の宛先が TLS とは限らない)。
/// `on:<port>` を指定すると 443 に加えてそのポートでも覗く (試験のオリジンを 443 に
/// 立てられないため。`proxy-tunnel` の `sni::TLS_PORT` と同じ値で、この層は
/// トンネルのクレートに依存しないので数値で持つ)。
pub const DEFAULT_PEEK_SNI_PORT: u16 = 443;

/// `PROXY_MAX_CONNS=auto` の頭打ち。
///
/// 記述子が余っていてもここで止める。上限は記述子だけの歯止めではなく、
/// **fd 以外の資源 (スレッド・RSS) の歯止め**でもあるため。T2.1 で入れた既定と同じ値で、
/// 根拠は T2.3 の実測 (同時 5,000 本のアイドル接続で RSS 198 MiB。動作環境のコンテナは小さい)。
/// これを超える値が要るなら数値で明示してもらう。
pub const MAX_CONNS_CAP: usize = 4096;

/// `RLIMIT_NOFILE` の soft limit から同時接続数の上限を決める (`PROXY_MAX_CONNS=auto`)。
///
/// `(soft - 予備 64) / 4` を [`MAX_CONNS_CAP`] で頭打ちにした値。1 接続あたり最悪 4 記述子なので、
/// `ulimit -n` が 1024 の環境なら 240、4096 なら 1008 で、**`accept` が `EMFILE` で失敗する前に
/// 503 で断れる**。`0` (無制限) には決してしない (記述子切れに戻ってしまうため下限は 1)。
pub fn auto_max_conns(nofile_soft: u64) -> usize {
    let usable = nofile_soft.saturating_sub(NOFILE_RESERVE) / FDS_PER_CONN;
    usable.clamp(1, MAX_CONNS_CAP as u64) as usize
}

/// `PROXY_MAX_CONNS` の既定 (= `auto`)。`RLIMIT_NOFILE` が読めない環境では [`MAX_CONNS_CAP`]。
pub fn default_max_conns() -> usize {
    #[cfg(target_os = "linux")]
    if let Some(soft) = crate::sys::max_open_files() {
        return auto_max_conns(soft);
    }
    MAX_CONNS_CAP
}

/// 山の写真 (`/bursts`) を撮る割合の既定 (`PROXY_BURST_PERCENT`。T14.6)。
///
/// `PROXY_MAX_CONNS` の半分を越えたら 1 枚撮る。半分なのは、デプロイ先の山 (218 / 240) が
/// 上限の 91% まで行った実測 (§2) に対して、**山が立ち始めた時点**を残したいため
/// (上限の間際まで待つと、T13.2 の追い出しが動いたあとの姿しか撮れない)。
pub const DEFAULT_BURST_PERCENT: usize = 50;

/// 日次の `/snapshot` を残す日数の既定 (`PROXY_SNAPSHOT_DAYS`。T14.34)。
///
/// 1 日 1 ファイル (`/snapshot` と同じ 4 MiB まで) を `$HOME/.rust-http-proxy/snapshots/` に
/// 残す。30 日なのは `/history` のいちばん粗い解像度 (1 時間 × 30 日) と同じ長さにするため
pub const DEFAULT_SNAPSHOT_DAYS: usize = 30;
/// 同じく上限 (書き間違いで `$HOME` を埋めないための歯止め。`proxy-metrics` の
/// `snapshots::MAX_KEPT_DAYS` と同じ値。この層は計測クレートに依存しないので数値で持つ)。
pub const MAX_SNAPSHOT_DAYS: usize = 365;

/// 「この本数を**越えた**瞬間に 1 枚撮る」の本数 (T14.6)。
///
/// [`usize::MAX`] は「撮らない」(`PROXY_BURST_PERCENT=0`、`PROXY_MAX_CONNS=0` = 無制限、
/// `--lite`)。**接続ごとの比較を 1 回で済ませるために先に計算しておく** — 撮らない設定でも
/// 比較の形は同じなので、accept の経路には分岐が 1 つ増えるだけで済む。
///
/// 割合を当てるのは `max_conns` だけで、**T13.2 の「上限の外の枠 4 本」は含めない**
/// (自分宛ての `/status` を受けるための枠なので、山の大きさの物差しに混ぜない)。
pub fn burst_threshold(max_conns: usize, percent: usize) -> usize {
    if max_conns == 0 || percent == 0 {
        return usize::MAX;
    }
    (max_conns.saturating_mul(percent.min(100)) / 100).max(1)
}

/// `PROXY_MAX_THREADS=auto` の 1 コアあたりの本数と、その下限・上限。
///
/// 接続スレッドは**ほとんどの時間 I/O で寝ている**ので、コア数そのものでは全く足りない
/// (1 本の接続を処理しているあいだずっと 1 本要る)。一方でいくら増やしても得るものは無く、
/// 1 本あたりスタック 256 KiB と切り替えの費用がかかる。
///
/// 実測 (T10.5、`--only idle-tunnels --conc 5000`、プロキシは 4 コアに固定)。
/// 暇なトンネルは 1 本ごとに猶予 100 ms のあいだワーカーを握るので、**確立できる速さは
/// おおよそ「上限 ÷ 100 ms」**になる。上限が無いと同じ場面で 4,721 スレッドまで跳ねる。
///
/// | 上限 | 5,000 本の確立 | スレッド最大 | ピーク RSS |
/// |---|---|---|---|
/// | 64 (コア数 × 16) | 8.4 s | 68 | 26.9 MB |
/// | 128 (コア数 × 32) | 4.2 s | 132 | 26.7 MB |
/// | 192 | 3.0 s | 196 | 27.6 MB |
/// | **256 (コア数 × 64、既定)** | **2.2〜2.6 s** | **260** | **28.1 MB** |
/// | 384 | 1.8 s | 388 | 31.4 MB |
/// | 無制限 (T10.5 以前) | 1.8 s | 4,721 | 72.6 MB |
///
/// 確立の速さがほぼ元に戻り、跳ね上がりも 1 桁小さいところとして **コア数 × 64** を採った。
const THREADS_PER_CORE: usize = 64;
const MIN_MAX_THREADS: usize = 128;
const MAX_MAX_THREADS: usize = 512;

/// 生きている接続スレッドの上限の既定 (`PROXY_MAX_THREADS=auto`)。
///
/// `コア数 × 64` を 128〜512 に収め、`PROXY_MAX_CONNS` があればそれも超えない
/// (受けない接続のためのスレッドは要らない)。コア数は `available_parallelism` なので、
/// `taskset` で絞られていればその数になる (使える資源に合わせる)。
/// **`0` (無制限) には決してしない** — 上限が無いと、預けた接続が一斉に切れたときに
/// スレッドが数千まで跳ねる (T8.1 で 4,621、T10.5 で 4,721 の実測)。
pub fn default_max_threads(max_conns: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    max_threads_for(cores, max_conns)
}

/// [`default_max_threads`] の計算の部 (コア数を引数で受ける)。
pub(super) fn max_threads_for(cores: usize, max_conns: usize) -> usize {
    let n = cores
        .saturating_mul(THREADS_PER_CORE)
        .clamp(MIN_MAX_THREADS, MAX_MAX_THREADS);
    if max_conns > 0 { n.min(max_conns) } else { n }
}

/// 1 本のクライアント接続で処理する要求数の既定の上限 (keep-alive)。
///
/// 認証なしの開放プロキシなので、1 本の接続を無限に使い回されないところで切る。
/// 上限の要求は普通に応答し、その応答に `Connection: close` を付けてから閉じる。
pub const DEFAULT_MAX_REQUESTS_PER_CONN: usize = 1000;

#[cfg(test)]
mod tests {
    use super::super::Config;
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_auto_max_conns() {
        // 1 接続 4 記述子 + 予備 64。記述子切れ (accept の EMFILE) より先に 503 で断れる値
        assert_eq!(auto_max_conns(256), 48);
        assert_eq!(auto_max_conns(1024), 240);
        assert_eq!(auto_max_conns(4096), 1008);
        // 記述子が余っていても頭打ち (fd 以外の資源の歯止め)
        assert_eq!(auto_max_conns(524_288), MAX_CONNS_CAP);
        assert_eq!(auto_max_conns(1_048_576), MAX_CONNS_CAP);
        assert_eq!(auto_max_conns(u64::MAX), MAX_CONNS_CAP);
        // 予備にも足りない極端な環境でも 0 (= 無制限) にはしない
        assert_eq!(auto_max_conns(64), 1);
        assert_eq!(auto_max_conns(0), 1);
        // 既定はこの計算そのもので、上限を超えない
        assert!((1..=MAX_CONNS_CAP).contains(&default_max_conns()));
    }

    #[test]
    fn test_default_max_threads() {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let want = (cores * THREADS_PER_CORE).clamp(MIN_MAX_THREADS, MAX_MAX_THREADS);
        // 接続数に余裕があればコア数から決まる
        assert_eq!(default_max_threads(0), want, "無制限のときもスレッドは有限");
        assert_eq!(default_max_threads(MAX_CONNS_CAP), want);
        // 接続数の上限の方が小さければそちらに従う (受けない接続のスレッドは要らない)
        assert_eq!(default_max_threads(16), 16);
        assert_eq!(default_max_threads(1), 1);
        // 何があっても無制限にはしない
        assert!(default_max_threads(0) > 0 && default_max_threads(0) <= MAX_MAX_THREADS);
        let cfg = Config::new("8080", None, None, Duration::from_secs(30)).unwrap();
        assert_eq!(cfg.max_threads, default_max_threads(cfg.max_conns));
    }

    /// コア数を引数で受ける計算の部: 128〜512 に収め、接続数の上限を超えない。
    #[test]
    fn max_threads_follow_the_cores_within_the_bounds() {
        assert_eq!(max_threads_for(1, 0), MIN_MAX_THREADS, "1 コアでも 128");
        assert_eq!(max_threads_for(2, 0), MIN_MAX_THREADS, "2 × 64 = 128");
        assert_eq!(max_threads_for(4, 0), 256, "既定のデプロイ先 (4 コア)");
        assert_eq!(
            max_threads_for(8, 0),
            MAX_MAX_THREADS,
            "8 × 64 は上限で頭打ち"
        );
        assert_eq!(max_threads_for(64, 0), MAX_MAX_THREADS);
        assert_eq!(max_threads_for(4, 100), 100, "接続数の上限の方が小さい");
        assert_eq!(max_threads_for(4, 1000), 256, "接続数の上限の方が大きい");
    }

    /// 山の写真の閾 (T14.6): 上限 × 割合。撮らない設定は [`usize::MAX`] (比較 1 回のまま)。
    #[test]
    fn the_burst_threshold_is_a_share_of_the_connection_limit() {
        assert_eq!(burst_threshold(8, 50), 4);
        assert_eq!(burst_threshold(240, 50), 120);
        assert_eq!(burst_threshold(4096, 90), 3686);
        // 100% は上限そのもの (T13.2 の「上限の外の枠 4 本」は含めない)
        assert_eq!(burst_threshold(240, 100), 240);
        assert_eq!(
            burst_threshold(240, 150),
            240,
            "100 を超える割合は 100 に倒す"
        );
        // 小さすぎる上限でも 0 にはしない (0 だと 1 本目から撮ってしまう)
        assert_eq!(burst_threshold(1, 50), 1);
        // 撮らない設定
        assert_eq!(burst_threshold(240, 0), usize::MAX);
        assert_eq!(burst_threshold(0, 50), usize::MAX, "無制限には閾が無い");

        let mut cfg = Config::new("8080", None, None, Duration::from_secs(30)).unwrap();
        assert_eq!(cfg.burst_percent, DEFAULT_BURST_PERCENT);
        assert_eq!(cfg.burst_at, burst_threshold(cfg.max_conns, 50));
        cfg.max_conns = 8;
        cfg.refresh_burst_at();
        assert_eq!(cfg.burst_at, 4);
        // `--lite` は個票を記録しないので閾そのものを置かない
        cfg.lite = true;
        cfg.refresh_burst_at();
        assert_eq!(cfg.burst_at, usize::MAX);
    }
}
