//! transfer — 転送速度と半閉じの分布 (T14.25)。
//!
//! 「遅い」には**確立が遅い**のと**転送が遅い**のがある。確立は `/history` の標本
//! (`connect_buckets`) と T14.3 の段階で読めるが、転送の速さは 1 本ごとに
//! `/recent` の寿命とバイトから割るしかなく、**分布** (どの程度のトンネルが
//! 100 KiB/s 未満か) が無かった。あわせて、トンネルが**片側だけ閉じた (半閉じ)**
//! あと反対側が閉じるまでの時間は `PROXY_TUNNEL_IDLE_SECS` の設計の材料になる。
//!
//! - 数えるのは **CONNECT のトンネルだけ** (`proxy_tunnel::tunnel::report` =
//!   トンネルの終わりの 1 回。T14.6 の `closed` は HTTP の接続も含むので数が違う)。
//! - **速さは 1 KiB 以上運んだトンネルだけ** ([`MIN_BYTES`])。ヘッダーだけで終わった
//!   トンネルの「1 バイト ÷ 0 ms」を混ぜると分布が読めなくなる。
//! - **中継の時間**は「寿命 − 確立まで − 預かり所にいた時間」。預けは利用者が待って
//!   いない時間なので中継ではない (T14.3 の「確立まで」と「その後」の分け方と同じ)。
//! - 半閉じは**半閉じで終わったトンネルだけ**。片側が EOF を出してから反対側が
//!   閉じる (= トンネルが落ちる) までの ms を数える。
//! - 窓は `closed` (T14.6) と同じ **5 秒 × 720 と 60 秒 × 1,440 のメモリ上のもの**で、
//!   `.rrd` には書かない (標本 1 本の余白は 4 B しか無い。T14.2 (3))。
//!   `/history?res=5|60` に `closed` の隣の**別の配列**として出す
//!   (既存の `closed` の `keys` は 1 つも変えていない)。
//!
//! **費用**: 要求の経路にも中継のループ (splice の往復) にも 1 命令も足していない。
//! 足すのはトンネル 1 本の終わり ([`TransferWindows::observe`] = 鍵 1 回) と、
//! 中継の中で**最初の EOF を見たとき 1 回だけ**の時計の読み (`Instant::now`。
//! 呼び出し側が `get_or_insert_with` で囲ってある) だけ。
//!
//! T14.42 で**中継の詰まりの向き**の合計 (`stall_client_ms_sum` /
//! `stall_origin_ms_sum`) を列の**末尾**に足した。1 本ごとの値は `/recent` の
//! `stall_ms` で、ここはその窓の合計 (`tunnels` で割れば 1 本あたりの平均)。
//! 時計を読むのは中継が**書けなくて待ちに入る回だけ**なので、詰まらない中継
//! (loopback) では 1 回も読まない。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

use crate::recent::SIDES;
use crate::sync::LockExt;
use crate::window::RESOLUTIONS;

/// 速さを数える下限 (バイト)。これ未満のトンネルは `tunnels` にだけ数える。
pub const MIN_BYTES: u64 = 1 << 10;

/// 速さの区間 (バイト/秒)。**12 段、1 KiB/s から 4 倍ずつ** (T14.6 の `BYTE_BOUNDS`
/// と同じ刻みにしてあるので、同じ窓のバイトの分布と並べて読める)。
///
/// 1 KiB/s (事実上止まっている) から 1 GiB/s (11 段目 = ループバックの上限) までを
/// 覆う。最後の 1 段 (4 GiB/s) は「桁が違う 1 本」を上限なしの区間に落とさないための余白。
pub const SPEED_BOUNDS_BPS: [u64; 12] = [
    1 << 10,
    1 << 12,
    1 << 14,
    1 << 16,
    1 << 18,
    1 << 20,
    1 << 22,
    1 << 24,
    1 << 26,
    1 << 28,
    1 << 30,
    1 << 32,
];

/// 半閉じからの時間の区間 (ms)。**12 段、1 ms から 4 倍ずつ**。
///
/// 1 ms 以下 (= 相手も続けて閉じた) から 262 秒までを覆う。`PROXY_TUNNEL_IDLE_SECS`
/// の既定 300 秒はその次の段 (262 秒 〜 1,049 秒) に入るので、**「打ち切りまで
/// 待たされた半閉じ」は上から 3 段目に固まって見える**。上の 2 段は
/// `PROXY_TUNNEL_IDLE_SECS` を長くしたときのための余白。
pub const HALF_CLOSE_BOUNDS_MS: [u64; 12] = [
    1, 4, 16, 64, 256, 1024, 4096, 16384, 65536, 262144, 1048576, 4194304,
];

/// 区間の数 (上限なしの 1 段を足す)。
pub const SPEED_BUCKETS: usize = SPEED_BOUNDS_BPS.len() + 1;
pub const HALF_CLOSE_BUCKETS: usize = HALF_CLOSE_BOUNDS_MS.len() + 1;

/// [`TransferCounts::push_row`] が並べる列の名前 (`/history` の `transfer.keys`)。
///
/// **末尾の 2 つは T14.42 で足した** (既存の並びは 1 つも変えていない)。
pub const TRANSFER_KEYS: [&str; 11] = [
    "t",
    "tunnels",
    "speed_n",
    "speed",
    "half_close_n",
    "half_close",
    "bytes_sum",
    "relay_ms_sum",
    "half_close_ms_sum",
    "stall_client_ms_sum",
    "stall_origin_ms_sum",
];

/// `bounds` の何段目か (どれにも収まらなければ最後の「上限なし」)。
fn bucket_of(bounds: &[u64], v: u64) -> usize {
    bounds.iter().position(|&b| v <= b).unwrap_or(bounds.len())
}

/// 「その窓に終わったトンネル」の速さと半閉じ。**累計ではない。**
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransferCounts {
    /// 終わったトンネルの数 (速さを数えなかったものも含む)
    pub tunnels: u64,
    /// 速さを数えたトンネルの数 ([`MIN_BYTES`] 以上運んだもの)
    pub speed_n: u64,
    /// 速さの区間 ([`SPEED_BOUNDS_BPS`] + 上限なし)
    pub speed: [u64; SPEED_BUCKETS],
    /// 半閉じで終わったトンネルの数
    pub half_close_n: u64,
    /// 半閉じからの時間の区間 ([`HALF_CLOSE_BOUNDS_MS`] + 上限なし)
    pub half_close: [u64; HALF_CLOSE_BUCKETS],
    /// 合計 (平均を出すため。速さを数えたトンネルのぶんだけ)
    pub bytes_sum: u64,
    pub relay_ms_sum: u64,
    pub half_close_ms_sum: u64,
    /// 中継が**書けるのを待った** ms の合計 (T14.42)。`client` は
    /// 「クライアントへ書けなかった」= 利用者の下り回線か端末が読まない、
    /// `origin` は「オリジンへ書けなかった」= オリジンか利用者の上り。
    /// **終わったトンネル全部**が対象 ([`MIN_BYTES`] の足切りは掛けない)
    pub stall_client_ms_sum: u64,
    pub stall_origin_ms_sum: u64,
}

impl TransferCounts {
    /// 終わったトンネル 1 本を足す。
    ///
    /// `bytes` は上り + 下り、`relay` は中継の時間 (預かり所にいた時間は引いてある)、
    /// `half_close` は半閉じで終わっていればその時間。
    pub fn observe(
        &mut self,
        bytes: u64,
        relay: Duration,
        half_close: Option<Duration>,
        stall_ms: [u32; SIDES],
    ) {
        self.tunnels += 1;
        // 詰まりの向き (T14.42)。1 本ごとの値は中継が既に数え終えているので、
        // ここは足し算 2 回だけ (速さと違って足切りも割り算も無い)
        self.stall_client_ms_sum = self
            .stall_client_ms_sum
            .saturating_add(stall_ms[crate::recent::CLIENT_SIDE] as u64);
        self.stall_origin_ms_sum = self
            .stall_origin_ms_sum
            .saturating_add(stall_ms[crate::recent::ORIGIN_SIDE] as u64);
        if bytes >= MIN_BYTES {
            // 速さ = バイト ÷ 中継の時間。ループバックの 1 本は 1 ms に満たないので
            // us で割る (0 us は 1 us 扱い。割り算 1 回で、時計はもう読まない)
            let us = relay.as_micros().max(1).min(u64::MAX as u128) as u64;
            let bps = bytes.saturating_mul(1_000_000) / us;
            self.speed[bucket_of(&SPEED_BOUNDS_BPS, bps)] += 1;
            self.speed_n += 1;
            self.bytes_sum = self.bytes_sum.saturating_add(bytes);
            self.relay_ms_sum = self.relay_ms_sum.saturating_add(ms(relay));
        }
        if let Some(d) = half_close {
            let took = ms(d);
            self.half_close[bucket_of(&HALF_CLOSE_BOUNDS_MS, took)] += 1;
            self.half_close_n += 1;
            self.half_close_ms_sum = self.half_close_ms_sum.saturating_add(took);
        }
    }

    /// 粗い解像度へ畳むときは足し合わせる (区間の値なので平均でも最後の値でもない)。
    pub fn merge(&mut self, o: &TransferCounts) {
        self.tunnels += o.tunnels;
        self.speed_n += o.speed_n;
        for (a, b) in self.speed.iter_mut().zip(o.speed.iter()) {
            *a += *b;
        }
        self.half_close_n += o.half_close_n;
        for (a, b) in self.half_close.iter_mut().zip(o.half_close.iter()) {
            *a += *b;
        }
        self.bytes_sum = self.bytes_sum.saturating_add(o.bytes_sum);
        self.relay_ms_sum = self.relay_ms_sum.saturating_add(o.relay_ms_sum);
        self.half_close_ms_sum = self.half_close_ms_sum.saturating_add(o.half_close_ms_sum);
        self.stall_client_ms_sum = self
            .stall_client_ms_sum
            .saturating_add(o.stall_client_ms_sum);
        self.stall_origin_ms_sum = self
            .stall_origin_ms_sum
            .saturating_add(o.stall_origin_ms_sum);
    }

    pub fn is_empty(&self) -> bool {
        self.tunnels == 0
    }

    /// 1 窓を配列 1 行として書く ([`TRANSFER_KEYS`] の順。先頭は窓の始まりの時刻)。
    pub fn push_row(&self, out: &mut String, t: u64) {
        let _ = write!(out, "[{},{},{},", t, self.tunnels, self.speed_n);
        push_u64_array(out, &self.speed);
        let _ = write!(out, ",{},", self.half_close_n);
        push_u64_array(out, &self.half_close);
        let _ = write!(
            out,
            ",{},{},{},{},{}]",
            self.bytes_sum,
            self.relay_ms_sum,
            self.half_close_ms_sum,
            self.stall_client_ms_sum,
            self.stall_origin_ms_sum
        );
    }
}

/// ms に丸める (0.5 ms 以上は 1 ms)。
fn ms(d: Duration) -> u64 {
    ((d.as_micros() + 500) / 1000).min(u64::MAX as u128) as u64
}

fn push_u64_array(out: &mut String, v: &[u64]) {
    out.push('[');
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", n);
    }
    out.push(']');
}

fn push_num_array(out: &mut String, v: &[u64]) {
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", n);
    }
}

#[derive(Default)]
struct TransferState {
    /// まだ閉じていない 5 秒の窓と、その始まり (5 秒に丸めた epoch)
    cur: TransferCounts,
    cur_t: u64,
    /// まだ閉じていない 60 秒の窓 (閉じた 5 秒の窓を足し込む)
    min_cur: TransferCounts,
    min_t: u64,
    fine: VecDeque<(u64, TransferCounts)>,
    minute: VecDeque<(u64, TransferCounts)>,
    /// 起動からの通算 (畳んだトンネルの本数)
    total: u64,
}

/// 速さと半閉じの窓 (5 秒 × 720 と 60 秒 × 1,440)。
///
/// 書くのはトンネルの終わり ([`observe`](TransferWindows::observe))、窓を閉じるのは
/// history スレッド ([`roll`](TransferWindows::roll)) — `/history` の標本や `closed`
/// と**同じ周期・同じ境目**で閉じるので、読む側は時刻で突き合わせられる。
///
/// **件数 0 の窓は残さない** (`closed` と同じ。行の先頭に窓の始まりの時刻がある)。
#[derive(Default)]
pub struct TransferWindows {
    inner: Mutex<TransferState>,
}

impl TransferWindows {
    pub fn new() -> TransferWindows {
        TransferWindows::default()
    }

    /// 終わったトンネル 1 本を今の窓に足す (**トンネルの終わりで 1 回だけ**。鍵 1 回)。
    pub fn observe(
        &self,
        bytes: u64,
        relay: Duration,
        half_close: Option<Duration>,
        stall_ms: [u32; SIDES],
    ) {
        let mut w = self.inner.locked();
        w.total += 1;
        w.cur.observe(bytes, relay, half_close, stall_ms);
    }

    /// 窓を閉じる (history スレッドが 5 秒ごとに呼ぶ)。
    pub fn roll(&self, now: u64) {
        let mut w = self.inner.locked();
        let fine_t = (now / RESOLUTIONS[0].0) * RESOLUTIONS[0].0;
        if fine_t != w.cur_t {
            let done = std::mem::take(&mut w.cur);
            let at = w.cur_t;
            w.cur_t = fine_t;
            if !done.is_empty() {
                w.min_cur.merge(&done);
                push_window(&mut w.fine, at, done, RESOLUTIONS[0].1);
            }
        }
        let min_t = (now / RESOLUTIONS[1].0) * RESOLUTIONS[1].0;
        if min_t != w.min_t {
            let done = std::mem::take(&mut w.min_cur);
            let at = w.min_t;
            w.min_t = min_t;
            if !done.is_empty() {
                push_window(&mut w.minute, at, done, RESOLUTIONS[1].1);
            }
        }
    }

    /// 2 つの窓の置き場を満杯のぶん確保して触る (**起動時に 1 回だけ**。T17.8)。
    /// 返すのは触ったバイト数。
    pub fn prefault(&self) -> usize {
        let mut w = self.inner.locked();
        crate::prefault::deque(&mut w.fine, RESOLUTIONS[0].1)
            + crate::prefault::deque(&mut w.minute, RESOLUTIONS[1].1)
    }

    /// 残してある窓の数 (5 秒 / 60 秒) と、畳んだトンネルの本数の通算。
    pub fn counts(&self) -> (usize, usize, u64) {
        let w = self.inner.locked();
        (w.fine.len(), w.minute.len(), w.total)
    }

    /// `/history` の `transfer` (解像度の添字は [`RESOLUTIONS`] と同じ)。
    ///
    /// **1 時間の解像度では残していない** (`null`)。`closed` と同じ理由で、
    /// 「いま効いている設定が長すぎるか短すぎるか」を読むための分布には 30 日は要らない。
    pub fn to_json_res(&self, res: usize) -> String {
        if res > 1 {
            return "null".to_string();
        }
        let w = self.inner.locked();
        let ring = if res == 0 { &w.fine } else { &w.minute };
        let mut out = String::with_capacity(256 + ring.len() * 120);
        let _ = write!(out, "{{\"interval_secs\":{},\"keys\":[", RESOLUTIONS[res].0);
        for (i, k) in TRANSFER_KEYS.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\"", k);
        }
        out.push_str("],\"speed_bounds_bps\":[");
        push_num_array(&mut out, &SPEED_BOUNDS_BPS);
        out.push_str("],\"half_close_bounds_ms\":[");
        push_num_array(&mut out, &HALF_CLOSE_BOUNDS_MS);
        let _ = write!(out, "],\"min_bytes\":{},\"samples\":[", MIN_BYTES);
        for (i, (t, c)) in ring.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            c.push_row(&mut out, *t);
        }
        let _ = write!(
            out,
            "],\"windows\":{},\"capacity\":{},\"recorded\":{}}}",
            ring.len(),
            RESOLUTIONS[res].1,
            w.total
        );
        out
    }
}

fn push_window(ring: &mut VecDeque<(u64, TransferCounts)>, t: u64, c: TransferCounts, cap: usize) {
    if ring.len() >= cap {
        ring.pop_front();
    }
    ring.push_back((t, c));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 詰まらなかったトンネル (T14.42)。
    const NO_STALL: [u32; SIDES] = [0; SIDES];

    /// 速さの段は「バイト ÷ 中継の時間」で決まること (1 MiB を 1 秒 = 1 MiB/s)。
    #[test]
    fn the_speed_bucket_comes_from_bytes_over_relay_time() {
        let mut c = TransferCounts::default();
        c.observe(1 << 20, Duration::from_secs(1), None, NO_STALL);
        // 1 MiB/s はちょうど境目 (`v <= b` なので 1 MiB の段に入る)
        let at = bucket_of(&SPEED_BOUNDS_BPS, 1 << 20);
        assert_eq!(SPEED_BOUNDS_BPS[at], 1 << 20);
        assert_eq!(c.speed[at], 1);
        assert_eq!(c.speed_n, 1);
        assert_eq!(c.tunnels, 1);
        assert_eq!(c.bytes_sum, 1 << 20);
        assert_eq!(c.relay_ms_sum, 1000);
        // 同じバイトを 4 倍の時間で運ぶと 1 段下がる
        c.observe(1 << 20, Duration::from_secs(4), None, NO_STALL);
        assert_eq!(c.speed[at - 1], 1);
        assert_eq!(c.speed_n, 2);
    }

    /// 1 KiB 未満のトンネルは速さを数えない (本数だけ数える)。
    #[test]
    fn tunnels_under_a_kibibyte_are_counted_but_not_measured() {
        let mut c = TransferCounts::default();
        c.observe(MIN_BYTES - 1, Duration::from_micros(10), None, NO_STALL);
        assert_eq!(c.tunnels, 1);
        assert_eq!(c.speed_n, 0);
        assert_eq!(c.speed.iter().sum::<u64>(), 0);
        assert_eq!(c.bytes_sum, 0);
        // ちょうど 1 KiB は数える
        c.observe(MIN_BYTES, Duration::from_secs(1), None, NO_STALL);
        assert_eq!(c.speed_n, 1);
        assert_eq!(c.speed[bucket_of(&SPEED_BOUNDS_BPS, 1024)], 1);
    }

    /// 中継の時間が 0 でも壊れない (上限なしの段に落ちるだけ)。
    #[test]
    fn a_zero_length_relay_does_not_divide_by_zero() {
        let mut c = TransferCounts::default();
        c.observe(1 << 30, Duration::ZERO, None, NO_STALL);
        assert_eq!(c.speed_n, 1);
        assert_eq!(c.speed[SPEED_BUCKETS - 1], 1, "上限なしの段");
    }

    /// 半閉じは半閉じで終わったトンネルだけ数える。
    #[test]
    fn only_half_closed_tunnels_land_in_the_half_close_buckets() {
        let mut c = TransferCounts::default();
        c.observe(4096, Duration::from_millis(10), None, NO_STALL);
        assert_eq!(c.half_close_n, 0);
        c.observe(
            4096,
            Duration::from_millis(10),
            Some(Duration::from_millis(120)),
            NO_STALL,
        );
        assert_eq!(c.half_close_n, 1);
        // 120 ms は 64 ms 〜 256 ms の段
        let at = bucket_of(&HALF_CLOSE_BOUNDS_MS, 120);
        assert_eq!(HALF_CLOSE_BOUNDS_MS[at], 256);
        assert_eq!(c.half_close[at], 1);
        assert_eq!(c.half_close_ms_sum, 120);
        // 相手がすぐ閉じた (0 ms) は 1 段目
        c.observe(
            4096,
            Duration::from_millis(10),
            Some(Duration::ZERO),
            NO_STALL,
        );
        assert_eq!(c.half_close[0], 1);
        assert_eq!(c.tunnels, 3);
    }

    /// 畳むと区間も合計も足し合わさる。
    #[test]
    fn merging_two_windows_adds_every_column() {
        let mut a = TransferCounts::default();
        a.observe(
            1 << 20,
            Duration::from_secs(1),
            Some(Duration::from_millis(8)),
            [20, 5],
        );
        let mut b = TransferCounts::default();
        b.observe(1 << 20, Duration::from_secs(1), None, [3, 4]);
        a.merge(&b);
        assert_eq!(a.tunnels, 2);
        assert_eq!(a.speed_n, 2);
        assert_eq!(a.speed.iter().sum::<u64>(), 2);
        assert_eq!(a.half_close_n, 1);
        assert_eq!(a.bytes_sum, 2 << 20);
        assert_eq!(a.relay_ms_sum, 2000);
        // 詰まりの向きも足し合わさる (T14.42)
        assert_eq!(a.stall_client_ms_sum, 23);
        assert_eq!(a.stall_origin_ms_sum, 9);
    }

    /// 窓は `/history` と同じ境目で閉じ、件数 0 の窓は残さない。
    #[test]
    fn windows_close_on_the_same_boundaries_as_history() {
        let w = TransferWindows::new();
        w.roll(1_700_000_000);
        w.observe(1 << 20, Duration::from_secs(1), None, NO_STALL);
        w.roll(1_700_000_005);
        // 何も終わらなかった 5 秒は窓を作らない
        w.roll(1_700_000_010);
        let (fine, _, total) = w.counts();
        assert_eq!(fine, 1, "件数 0 の窓は残さない");
        assert_eq!(total, 1);
        let json = w.to_json_res(0);
        assert!(json.contains("\"interval_secs\":5"), "{}", json);
        assert!(
            json.contains("\"keys\":[\"t\",\"tunnels\",\"speed_n\",\"speed\",\"half_close_n\""),
            "{}",
            json
        );
        assert!(json.contains("\"min_bytes\":1024"), "{}", json);
        assert!(json.contains("[1700000000,1,1,["), "{}", json);
        assert!(json.contains("\"windows\":1"), "{}", json);
        assert!(json.contains("\"recorded\":1"), "{}", json);
        // 1 時間の解像度は残していない
        assert_eq!(w.to_json_res(2), "null");
    }

    /// 5 秒の窓は 60 秒の窓にも足し込まれる (畳んだ本数は 1 本のまま)。
    #[test]
    fn the_minute_window_gets_the_closed_five_second_windows() {
        let w = TransferWindows::new();
        w.roll(1_700_000_000);
        for _ in 0..3 {
            w.observe(
                1 << 20,
                Duration::from_secs(1),
                Some(Duration::from_millis(4)),
                NO_STALL,
            );
        }
        w.roll(1_700_000_005);
        w.roll(1_700_000_065);
        let (fine, minute, total) = w.counts();
        assert_eq!((fine, minute, total), (1, 1, 3));
        let json = w.to_json_res(1);
        assert!(json.contains("\"interval_secs\":60"), "{}", json);
        // 60 秒の窓の始まりは 60 で丸めた時刻 (1,700,000,000 は 60 の倍数ではない)
        assert!(json.contains("[1699999980,3,3,["), "{}", json);
        assert!(json.contains("\"capacity\":1440"), "{}", json);
    }

    /// 窓が埋まったときの大きさ (1 窓 ≈ 120 B)。`/history` が太る分をここで押さえる。
    #[test]
    fn a_full_ring_stays_small_enough_for_history() {
        let w = TransferWindows::new();
        for i in 0..(RESOLUTIONS[0].1 as u64 + 5) {
            for _ in 0..200 {
                w.observe(
                    987_654_321,
                    Duration::from_secs(30),
                    Some(Duration::from_millis(1234)),
                    [4_294_967_295, 4_294_967_295],
                );
            }
            w.roll(1_700_000_000 + (i + 1) * 5);
        }
        let json = w.to_json_res(0);
        let windows = json.matches("[1700").count();
        assert_eq!(windows, RESOLUTIONS[0].1, "{} 窓", windows);
        assert!(json.len() <= 256 * 1024, "res=5 が {} B", json.len());
        println!(
            "transfer res=5 が満杯のとき: {} B ({} 窓 = 1 窓 {} B)",
            json.len(),
            windows,
            json.len() / windows
        );
    }
}
