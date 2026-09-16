//! 接続元 1 つの追跡 (`PROXY_TRACE_CLIENT` → `/trace`。T14.27)。
//!
//! 見知らぬ接続元 (T14.0) や「この端末だけ遅い」を調べるとき、全体のログ水準を `trace` に
//! 上げるとアクセスログが**全員に**乗る (T10.10 の 7.2 us/要求)。ここは **1 つの接続元だけ**、
//! 要求行 (メソッド + パスの先頭 [`MAX_PATH`] バイト + HTTP の版) と応答の状態、段階の ms
//! (T14.3)、CONNECT の宛先と閉じた理由を [`MAX_TRACE`] 行の固定長リング 1 本に残す口。
//!
//! **照合は accept で 1 回だけ**: 接続元が `PROXY_TRACE_CLIENT` と一致するかは
//! `src/lib.rs` の accept 直後 (T14.13 / T14.18 の判定の隣) で 1 回見て、一致したときだけ
//! [`crate::recent::ConnSlot`] に旗を立てる。**要求ごとに残るのはその旗を読む分岐 1 回**で、
//! 文字列の比較も設定の引き直しもしない。`--lite` は枠 (`ConnSlot`) を作らないので旗も
//! 立たず、追跡もしない。
//!
//! **リングはメモリだけ** (T14.9 の永続化の対象にしない = 再起動で消える)。容量は
//! `/status` の `memory.rings.trace` に出る。
//!
//! **個票の決まりの例外**: T14.4〜T14.8 の共通の決まりでは個票に URL のパスを入れない。
//! **このリングだけはその例外**で、追跡中の 1 接続元に限りパスを残す。認証なしで誰でも
//! 読めるのは他の個票と同じなので、README にその旨を書いてある。`/snapshot` には
//! **入れない** (パスが雪像のファイルに残らないように)。

use std::sync::Mutex;

use crate::recent::{CloseReason, MAX_CLIENT, STAGE_CONNECT, STAGE_NAMES, STAGES, clip};
use crate::sync::LockExt;

/// 覚えておく行数 (固定)。
///
/// 1 つの接続元を追う道具なので、要求の多い相手でも直近の数分〜数時間が読めればよい。
/// 1 行は下の切り詰めで最大 [`MAX_LINE_ESTIMATE`] バイトなので、満杯でも 400 KiB ほど。
pub const MAX_TRACE: usize = 1000;

/// 1 行に残す宛先の長さ (バイト)。**forward は URL、CONNECT は `host:port`。**
///
/// 本文の「パスは先頭 256 B」。長いものは末尾に `…` を付けて切る。
pub const MAX_PATH: usize = 256;

/// メソッドと HTTP の版に残す長さ (バイト)。壊れた要求行でも行が太らないための歯止め。
pub const MAX_METHOD: usize = 16;
pub const MAX_VERSION: usize = 16;

/// 1 行が最悪で使うヒープ (`/status` の `memory.rings.trace` の見積もりに使う)。
pub const MAX_LINE_ESTIMATE: usize =
    size_of::<TraceLine>() + MAX_CLIENT + MAX_PATH + MAX_METHOD + MAX_VERSION;

/// 追跡の 1 行 (= 1 要求、または CONNECT トンネル 1 本)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceLine {
    /// いつ (epoch 秒)
    pub at: u64,
    /// 接続の通し番号 (ログの `conn#N`・`/recent` の `id` と同じ)
    pub conn_id: u64,
    /// 接続元 IP (`PROXY_TRACE_CLIENT` を `.env` で差し替えると 2 人分が並びうるので残す)
    pub client: String,
    /// 要求行のメソッド (`GET` / `CONNECT` …)
    pub method: String,
    /// forward は URL (パスの先頭 [`MAX_PATH`] バイト)、CONNECT は宛先の `host:port`
    pub target: String,
    /// 要求行の HTTP の版
    pub version: String,
    /// 応答の状態 (CONNECT は `200`)
    pub status: u16,
    /// 応答までにかかった ms (CONNECT はトンネルの寿命)
    pub took_ms: u64,
    /// 運んだバイト (forward は応答、CONNECT は上り + 下りの合計)
    pub bytes: u64,
    /// 段階の ms ([`STAGE_NAMES`] の並び。T14.3)
    pub stage_ms: [u64; STAGES],
    /// 閉じた理由 (**CONNECT だけ**。forward の 1 要求では `None` = `null`)
    pub reason: Option<CloseReason>,
}

impl TraceLine {
    /// `/trace` の 1 要素。
    ///
    /// 段階の `ms` の出し方は `/recent` と同じで、**`dns` と `connect` は 0 でも必ず出し**、
    /// 残りは 0 なら出さない (プロファイルを止めていると 0 のままなので 1 バイトも増えない)。
    pub fn to_json(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(256);
        let _ = write!(
            out,
            "{{\"at\":{},\"conn\":{},\"client\":\"{}\",\"method\":\"{}\",\"target\":\"{}\",\"version\":\"{}\",\"status\":{},\"took_ms\":{},\"bytes\":{},\"ms\":{{",
            self.at,
            self.conn_id,
            crate::json::escape(&self.client),
            crate::json::escape(&self.method),
            crate::json::escape(&self.target),
            crate::json::escape(&self.version),
            self.status,
            self.took_ms,
            self.bytes,
        );
        for (i, (name, ms)) in STAGE_NAMES.iter().zip(self.stage_ms).enumerate() {
            if i > STAGE_CONNECT && ms == 0 {
                continue;
            }
            let _ = write!(out, "{}\"{}\":{}", if i == 0 { "" } else { "," }, name, ms);
        }
        out.push('}');
        match self.reason {
            Some(r) => {
                let _ = write!(out, ",\"reason\":\"{}\"}}", r.text());
            }
            None => out.push_str(",\"reason\":null}"),
        }
        out
    }
}

/// 追跡の行を組み立てる (呼ぶ側の引数を短くするための入れ物)。
pub struct Line<'a> {
    pub conn_id: usize,
    pub client: &'a str,
    pub method: &'a str,
    pub target: &'a str,
    pub version: &'a str,
    pub status: u16,
    pub took_ms: u64,
    pub bytes: u64,
    pub stage_ms: [u64; STAGES],
    pub reason: Option<CloseReason>,
}

#[derive(Default)]
struct TraceRing {
    buf: Vec<TraceLine>,
    /// 次に書く位置 (`buf` が満杯になってからだけ意味を持つ)
    next: usize,
    /// 起動からの通算 (捨てた分も含む)
    total: u64,
}

/// 追跡のリング。**書くのは旗の立った接続だけ**なので、既定 (`PROXY_TRACE_CLIENT` が空)
/// では 1 度もこの鍵を取らず、1 バイトも確保しない。
static RING: Mutex<TraceRing> = Mutex::new(TraceRing {
    buf: Vec::new(),
    next: 0,
    total: 0,
});

/// 1 行書く (満杯なら最も古い行を上書きする)。
///
/// **旗 (`ConnSlot::traced`) が立っている接続からだけ呼ぶこと。** 追跡していない
/// 要求はこの関数まで来ない (呼ぶ側の分岐 1 回で終わる)。
pub fn push(line: Line<'_>) {
    // 記録の一括 off (T14.41)。`PROXY_TRACE_CLIENT` の照合は accept 直後に**生の IP**で
    // 済んでいるので、ここで止めるのは残す側だけ (`off` なら `/trace` は空のまま)
    if !crate::records::recording() {
        return;
    }
    let entry = TraceLine {
        at: crate::cache::now_epoch(),
        conn_id: line.conn_id as u64,
        // 接続元は他の個票と同じ 1 関数を通す (`hashed` なら 16 桁の 16 進)
        client: clip(&crate::records::client_key(line.client), MAX_CLIENT),
        method: clip(line.method, MAX_METHOD),
        target: clip(line.target, MAX_PATH),
        version: clip(line.version, MAX_VERSION),
        status: line.status,
        took_ms: line.took_ms,
        bytes: line.bytes,
        stage_ms: line.stage_ms,
        reason: line.reason,
    };
    let mut r = RING.locked();
    r.total += 1;
    if r.buf.len() < MAX_TRACE {
        r.buf.push(entry);
        return;
    }
    let at = r.next;
    r.buf[at] = entry;
    r.next = (at + 1) % MAX_TRACE;
}

/// 条件に合う行を**新しい順**で `n` 行まで返す。2 つ目は起動からの通算 (捨てた分も含む)。
///
/// 絞りはここ (鍵の内側) で済ませる。`since` は「その時刻以降に書いたもの」。
pub fn select(since: u64, n: usize) -> (Vec<TraceLine>, u64) {
    let r = RING.locked();
    let len = r.buf.len();
    let mut out = Vec::with_capacity(len.min(n));
    for i in 0..len {
        if out.len() >= n {
            break;
        }
        // `next` の 1 つ手前が最新 (満杯になる前は `next == 0` なので末尾が最新)
        let start = if len < MAX_TRACE { len } else { r.next };
        let e = &r.buf[(start + len - 1 - i) % len];
        if e.at < since {
            continue;
        }
        out.push(e.clone());
    }
    (out, r.total)
}

/// 覚えている行数。
pub fn len() -> usize {
    RING.locked().buf.len()
}

pub fn is_empty() -> bool {
    len() == 0
}

/// リングを空にする (テスト用)。
pub fn clear() {
    let mut r = RING.locked();
    r.buf.clear();
    r.next = 0;
    r.total = 0;
}

/// リングは 1 本きり (静的) なので、テストはこの鍵で 1 つずつ通す
/// (`crate::events` の `TEST_LOCK` と同じ作法)。
#[cfg(test)]
static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recent::{STAGE_DNS, STAGE_FIRST_RELAY};

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK.locked();
        clear();
        g
    }

    fn line() -> Line<'static> {
        Line {
            conn_id: 1,
            client: "127.0.0.1",
            method: "GET",
            target: "http://example.test/a",
            version: "HTTP/1.1",
            status: 200,
            took_ms: 3,
            bytes: 11,
            stage_ms: [0; STAGES],
            reason: None,
        }
    }

    /// 新しい順に返り、`since` で絞れること。
    #[test]
    fn returns_the_newest_first_and_filters_by_since() {
        let _g = guard();
        for i in 0..3 {
            let mut l = line();
            l.conn_id = i;
            push(l);
        }
        let (rows, total) = select(0, 10);
        assert_eq!(total, 3);
        assert_eq!(
            rows.iter().map(|r| r.conn_id).collect::<Vec<_>>(),
            vec![2, 1, 0]
        );
        let future = crate::cache::now_epoch() + 60;
        assert!(select(future, 10).0.is_empty());
    }

    /// 満杯になったら最も古い行から捨て、通算は数え続けること。
    #[test]
    fn the_ring_wraps_and_keeps_counting() {
        let _g = guard();
        for i in 0..(MAX_TRACE + 5) {
            let mut l = line();
            l.conn_id = i;
            push(l);
        }
        assert_eq!(len(), MAX_TRACE);
        let (rows, total) = select(0, MAX_TRACE);
        assert_eq!(total, (MAX_TRACE + 5) as u64);
        assert_eq!(rows[0].conn_id, (MAX_TRACE + 4) as u64);
        assert_eq!(rows[rows.len() - 1].conn_id, 5);
    }

    /// 長いパスは [`MAX_PATH`] で切られること (要求行がそのまま行の長さにならない)。
    #[test]
    fn a_long_path_is_clipped() {
        let _g = guard();
        let long = "http://example.test/".to_string() + &"a".repeat(4096);
        let mut l = line();
        l.target = &long;
        push(l);
        let (rows, _) = select(0, 1);
        assert!(rows[0].target.len() <= MAX_PATH, "{}", rows[0].target.len());
        assert!(rows[0].target.ends_with('…'), "{}", rows[0].target);
    }

    /// 段階は `/recent` と同じ出し方 (`dns` と `connect` は 0 でも出し、残りは 0 なら出さない)。
    #[test]
    fn the_stages_are_written_like_recent() {
        let _g = guard();
        let mut l = line();
        l.stage_ms[STAGE_DNS] = 0;
        l.stage_ms[STAGE_CONNECT] = 7;
        push(l);
        let (rows, _) = select(0, 1);
        let json = rows[0].to_json();
        assert!(
            json.contains("\"ms\":{\"dns\":0,\"connect\":7}"),
            "{}",
            json
        );
        assert!(json.contains("\"reason\":null"), "{}", json);
        assert!(!json.contains("first_relay"), "{}", json);

        clear();
        let mut l = line();
        l.method = "CONNECT";
        l.target = "example.test:443";
        l.stage_ms[STAGE_FIRST_RELAY] = 12;
        l.reason = Some(CloseReason::ClientEof);
        push(l);
        let (rows, _) = select(0, 1);
        let json = rows[0].to_json();
        assert!(json.contains("\"first_relay\":12"), "{}", json);
        assert!(json.contains("\"reason\":\"client_eof\""), "{}", json);
    }
}
