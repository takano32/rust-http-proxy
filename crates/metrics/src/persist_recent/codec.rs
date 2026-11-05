//! 個票の 5 種類のレコードの中身 (T14.9。T34.29 で `persist_recent.rs` から移した)。
//!
//! 固定幅の文字列の欄、1 レコードに収まることの組み立て時の検査、`encode_*` / `decode_*`。
//! 行を動かしただけで、並びは変えていない (変えたら [`super::MAGIC`] の版を上げる)。

use super::*;

/// 固定幅の文字列の欄 (先頭 1 バイトが長さなので、入る中身は 1 バイト少ない)。
pub(super) const W_CLIENT: usize = 48; // 接続元 IP (最長 45 B)
pub(super) const W_TARGET: usize = 52; // `/recent` の宛先 (`MAX_RECENT_TARGET` 48 B)
pub(super) const W_ETARGET: usize = 84; // `/errors` と写真の宛先 (`MAX_TARGET` 80 B)
pub(super) const W_SNI: usize = MAX_SNI + 4; // 覗いた SNI (`MAX_SNI` 64 B。T14.38)
/// ログ 1 行の本文。レコード 256 B から通し番号・時刻・レベル・conn を引いた残り
/// (メモリのリングは 1 行 256 B まで持つので、**ファイルに残すときだけ 219 B に切る**)。
pub(super) const W_MSG: usize = SMALL_RECORD - 4 - 8 * 4;
/// 出来事 1 件の説明 (`MAX_TEXT` 128 B + 長さの 1 B。T14.11)。
pub(super) const W_TEXT: usize = MAX_TEXT + 4;

/// 1 レコードに収まることを**組み立て時に**確かめる (欄を足して溢れたらここで止まる)。
/// 数は各 `encode_*` が書く u64 の本数 (先頭の通し番号を含む) + 固定幅の文字列。
pub(super) const CLOSED_PAYLOAD: usize =
    8 * (16 + STAGES + 3 * SIDES) + W_CLIENT + W_TARGET + W_SNI;
pub(super) const ERROR_PAYLOAD: usize = 8 * 7 + W_ETARGET + W_CLIENT;
pub(super) const LOG_PAYLOAD: usize = 8 * 4 + W_MSG;
pub(super) const EVENT_PAYLOAD: usize = 8 * 3 + W_TEXT;
pub(super) const SHOT_PAYLOAD: usize = 8 * (18 + CONN_STATES + 2)
    + MAX_SHOT_CLIENTS * (W_CLIENT + 8)
    + MAX_SHOT_TARGETS * (W_ETARGET + 8);
const _: () = assert!(CLOSED_PAYLOAD <= CLOSED_RECORD - 4);
const _: () = assert!(ERROR_PAYLOAD <= SMALL_RECORD - 4);
const _: () = assert!(LOG_PAYLOAD <= SMALL_RECORD - 4);
const _: () = assert!(EVENT_PAYLOAD <= SMALL_RECORD - 4);
const _: () = assert!(SHOT_PAYLOAD <= SHOT_RECORD - 4);

pub(super) fn encode_closed(seq: u64, e: &RecentEntry) -> Vec<u8> {
    let mut enc = Enc::new();
    enc.u64(seq)
        .u64(e.at)
        .u64(e.id)
        .u64(e.secs)
        .u64(e.up)
        .u64(e.down)
        .u64(e.parked_secs)
        .u64(e.requests as u64)
        .u64(e.parks as u64)
        .u64(e.status as u64)
        .u64(u64::from(e.connect))
        .u64(e.reason.code() as u64)
        // 確立までの SYN の再送 (T14.46)。**数値の末尾に足した** ので、前の版で
        // 書いたレコードは 0 (= 再送なし) で読み戻る
        .u64(e.syn_retrans as u64);
    for ms in e.stage_ms {
        enc.u64(ms);
    }
    // カーネルの RTT と再送 (クライアント側 / オリジン側。T14.5)
    for v in e.rtt_us {
        enc.u64(v as u64);
    }
    for v in e.retrans {
        enc.u64(v as u64);
    }
    enc.str(&e.client, W_CLIENT).str(&e.target, W_TARGET);
    // 覗いた SNI (T14.38)。空 = 覗いていない / 読めなかった (読み戻すと `None`)
    enc.str(e.sni.as_deref().unwrap_or(""), W_SNI);
    // 中継の詰まりの向き (T14.42)。**レコードのいちばん後ろ**に足す (T14.38 の SNI と
    // 同じ理由: 途中に入れると前の版の文字列の欄がずれる。`Dec` は足りなければ 0 を
    // 返すので、前の版のレコードは両方 0 で読み戻る = 版は上げない)
    for v in e.stall_ms {
        enc.u64(v as u64);
    }
    // 空回りの回数と半閉じ (T15.0 (4))。同じ理由で**さらにいちばん後ろ**へ 3 本
    // (`spins`、**側 + 1** = `0` は「半閉じしていない」、半閉じから閉じるまでの ms)。
    // 版は上げない (`SHPREC03` のまま) — 前の版のレコードは 3 本とも 0 で読み戻る
    enc.u64(e.spins);
    let (side, ms) = match e.half_closed {
        Some((side, ms)) => (side as u64 + 1, ms),
        None => (0, 0),
    };
    enc.u64(side).u64(ms);
    enc.0
}

pub(super) fn decode_closed(p: &[u8]) -> Option<RecentEntry> {
    let mut d = Dec(p);
    let _seq = d.u64();
    let at = d.u64();
    let id = d.u64();
    let secs = d.u64();
    let up = d.u64();
    let down = d.u64();
    let parked_secs = d.u64();
    let requests = d.u64() as u32;
    let parks = d.u64() as u32;
    let status = d.u64() as u16;
    let connect = d.u64() != 0;
    let reason = CloseReason::from_code(d.u64() as u16);
    let syn_retrans = d.u64().min(u8::MAX as u64) as u8;
    let mut stage_ms = [0u64; STAGES];
    for slot in stage_ms.iter_mut() {
        *slot = d.u64();
    }
    let mut rtt_us = [0u32; SIDES];
    for slot in rtt_us.iter_mut() {
        *slot = d.u64() as u32;
    }
    let mut retrans = [0u32; SIDES];
    for slot in retrans.iter_mut() {
        *slot = d.u64() as u32;
    }
    let client = d.str(W_CLIENT);
    let target = d.str(W_TARGET);
    let sni = d.str(W_SNI);
    // 中継の詰まりの向き (T14.42)。前の版のレコードはここが無いので 0 で戻る
    let mut stall_ms = [0u32; SIDES];
    for slot in stall_ms.iter_mut() {
        *slot = d.u64() as u32;
    }
    // 空回りと半閉じ (T15.0 (4))。前の版のレコードはここが無いので 0 = 「空回り無し・
    // 半閉じ無し」で戻る (`Dec` は足りなければ 0 を返す)
    let spins = d.u64();
    let half_side = d.u64();
    let half_ms = d.u64();
    let half_closed = (half_side > 0).then(|| ((half_side - 1) as usize, half_ms));
    if at == 0 {
        return None;
    }
    Some(RecentEntry {
        id,
        at,
        client,
        target,
        connect,
        secs,
        requests,
        up,
        down,
        reason,
        status,
        parked_secs,
        parks,
        stage_ms,
        rtt_us,
        retrans,
        sni: (!sni.is_empty()).then(|| sni.into()),
        syn_retrans,
        stall_ms,
        spins,
        half_closed,
    })
}

pub(super) fn encode_error(seq: u64, e: &ErrorEntry) -> Vec<u8> {
    let mut enc = Enc::new();
    enc.u64(seq)
        .u64(e.at)
        .u64(e.kind.code())
        .u64(e.cause.code())
        .u64(e.dns_ms)
        .u64(e.connect_ms)
        .u64(e.status as u64)
        .str(&e.target, W_ETARGET)
        .str(&e.client, W_CLIENT);
    enc.0
}

pub(super) fn decode_error(p: &[u8]) -> Option<ErrorEntry> {
    let mut d = Dec(p);
    let _seq = d.u64();
    let at = d.u64();
    let kind = EntryKind::from_code(d.u64());
    let cause = EntryCause::from_code(d.u64());
    let dns_ms = d.u64();
    let connect_ms = d.u64();
    let status = d.u64() as u16;
    let target = d.str(W_ETARGET);
    let client = d.str(W_CLIENT);
    if at == 0 {
        return None;
    }
    Some(ErrorEntry {
        at,
        kind,
        target,
        cause,
        dns_ms,
        connect_ms,
        status,
        client,
        // ファイルの形は変えない (T20.3)。読み戻した行は 1 回ぶんで、最後の時刻は `at`
        repeats: 1,
        last_at: at,
    })
}

pub(super) fn encode_event(seq: u64, e: &Event) -> Vec<u8> {
    let mut enc = Enc::new();
    enc.u64(seq)
        .u64(e.at)
        .u64(e.kind.code())
        .str(&e.text, W_TEXT);
    enc.0
}

pub(super) fn decode_event(p: &[u8]) -> Option<Event> {
    let mut d = Dec(p);
    let _seq = d.u64();
    let at = d.u64();
    let kind = EventKind::from_code(d.u64());
    let text = d.str(W_TEXT);
    if at == 0 {
        return None;
    }
    Some(Event { at, kind, text })
}

pub(super) fn encode_log(seq: u64, line: &Line) -> Vec<u8> {
    let mut enc = Enc::new();
    enc.u64(seq)
        .u64(line.at)
        .u64(line.level as u64)
        // 0 は「`[main]`」の印なので、接続の番号は 1 足して入れる
        .u64(line.conn.map(|c| c as u64 + 1).unwrap_or(0))
        .str(&line.msg, W_MSG);
    enc.0
}

pub(super) fn decode_log(p: &[u8]) -> Option<Line> {
    let mut d = Dec(p);
    let _seq = d.u64();
    let at = d.u64();
    let level = Level::from_u8(d.u64().min(u8::MAX as u64) as u8);
    let conn = d.u64();
    let msg = d.str(W_MSG);
    if at == 0 {
        return None;
    }
    Some(Line {
        at,
        level,
        // 0 は `[main]` (接続の番号は 1 足して入れてある)
        conn: if conn > 0 {
            Some(conn as usize - 1)
        } else {
            None
        },
        msg,
    })
}

pub(super) fn encode_shot(seq: u64, s: &BurstShot) -> Vec<u8> {
    let mut enc = Enc::new();
    enc.u64(seq)
        .u64(s.at)
        .u64(s.seq)
        .u64(s.active as u64)
        .u64(s.trigger_active as u64)
        .u64(s.max_conns as u64)
        .u64(s.threshold as u64)
        .u64(s.clients_distinct as u64)
        .u64(s.clients_other as u64)
        .u64(s.targets_distinct as u64)
        .u64(s.targets_other as u64)
        .u64(s.connects as u64)
        .u64(s.https as u64)
        .u64(s.evicted_idle)
        .u64(s.rejected_overload)
        .u64(s.threads)
        .u64(s.fds)
        .u64(s.max_fds);
    for n in s.states {
        enc.u64(n as u64);
    }
    let clients = s.clients.len().min(MAX_SHOT_CLIENTS);
    enc.u64(clients as u64);
    for (name, n) in s.clients.iter().take(clients) {
        enc.str(name, W_CLIENT).u64(*n as u64);
    }
    let targets = s.targets.len().min(MAX_SHOT_TARGETS);
    enc.u64(targets as u64);
    for (name, n) in s.targets.iter().take(targets) {
        enc.str(name, W_ETARGET).u64(*n as u64);
    }
    enc.0
}

pub(super) fn decode_shot(p: &[u8]) -> Option<BurstShot> {
    let mut d = Dec(p);
    let _seq = d.u64();
    let at = d.u64();
    let shot_seq = d.u64();
    let active = d.u64() as usize;
    let trigger_active = d.u64() as usize;
    let max_conns = d.u64() as usize;
    let threshold = d.u64() as usize;
    let clients_distinct = d.u64() as usize;
    let clients_other = d.u64() as u32;
    let targets_distinct = d.u64() as usize;
    let targets_other = d.u64() as u32;
    let connects = d.u64() as u32;
    let https = d.u64() as u32;
    let evicted_idle = d.u64();
    let rejected_overload = d.u64();
    let threads = d.u64();
    let fds = d.u64();
    let max_fds = d.u64();
    let mut states = [0u32; CONN_STATES];
    for slot in states.iter_mut() {
        *slot = d.u64() as u32;
    }
    let n = (d.u64() as usize).min(MAX_SHOT_CLIENTS);
    let mut clients = Vec::with_capacity(n);
    for _ in 0..n {
        let name = d.str(W_CLIENT);
        clients.push((name, d.u64() as u32));
    }
    let n = (d.u64() as usize).min(MAX_SHOT_TARGETS);
    let mut targets = Vec::with_capacity(n);
    for _ in 0..n {
        let name = d.str(W_ETARGET);
        targets.push((name, d.u64() as u32));
    }
    if at == 0 {
        return None;
    }
    Some(BurstShot {
        at,
        seq: shot_seq,
        active,
        trigger_active,
        max_conns,
        threshold,
        clients,
        clients_distinct,
        clients_other,
        targets,
        targets_distinct,
        targets_other,
        states,
        connects,
        https,
        evicted_idle,
        rejected_overload,
        threads,
        fds,
        max_fds,
    })
}
