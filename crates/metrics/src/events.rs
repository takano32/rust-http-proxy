//! プロセスに起きた「出来事」の時系列 (`/events`。T14.11)。
//!
//! 数字が動いたとき「**そのとき何を変えたか**」が今までどこにも残っていなかった。
//! 再読込 (`applied` / `restart_required`) は `/status` の `settings` に**最後の 1 回**しか
//! 残らず、起動 (= 再デプロイ) の時刻は `since_start_secs` から逆算するしかなく、
//! `/log` は warn 以上なので info の出来事 (再読込、ブロックリストの取得、バラストの増減) は
//! 1 行も入らない。ここはその 1 本の時系列で、[`MAX_EVENTS`] 件の固定長リング 1 本だけ。
//!
//! **書くのは稀な経路だけ**で、要求ごとの経路には 1 命令も無い (`--lite` でも同じ)。
//! 頻度の高いものには歯止めを掛けてある: [`note_evict`] と [`note_accept_error`] は
//! **1 時間に初めて起きたときだけ**、状態ファイルの書込エラーは最初の 1 回だけ。
//!
//! 種類は [`EventKind`] の **12 種で固定** (増やすなら README も)。書く場所:
//!
//! | 種類 | 書く場所 |
//! |---|---|
//! | `start` / `shutdown` | `src/main.rs` (起動の最後、停止シグナルの後始末) |
//! | `reload` | `crates/reload/src/reload.rs` (`.env` を読み直したとき) |
//! | `blocklist` | `crates/blocklist` (一覧を組み直したとき) |
//! | `ipv6` / `pressure` / `ballast` | [`poll`] (履歴スレッドの周期。下を参照) |
//! | `state_file` | `crates/metrics/src/persist.rs` (書込エラーの最初の 1 回) |
//! | `evict` / `emfile` | `src/lib.rs` (上限に当たって閉じた / accept が失敗した) |
//! | `anomaly` | [`crate::anomaly`] (標本が基準値から外れた / 戻った。T14.23) |
//! | `new_client` | [`crate::anomaly`] (初めて見た接続元。T14.54 の規則 6) |
//!
//! `ipv6` / `pressure` / `ballast` の 3 つだけ**変わり目を [`poll`] で見る**のは、
//! それを起こす `proxy-net` と `proxy-cache` が**この層より下**にあるため
//! (下から上を呼ぶと依存が輪になる)。履歴スレッドの周期 (5 秒) で状態を読み、
//! 前回と違えば 1 件書く。**時刻は変わり目そのものではなく気づいた時刻**で、
//! 最大 1 周期ぶん遅れる (`--lite` と `PROXY_STATS_PERSIST=off` では履歴スレッドが
//! 無いのでこの 3 種は残らない)。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::sync::LockExt;

/// 覚えておく出来事の数 (固定)。
///
/// 起きるのは 1 日に数件 (起動・再読込・ブロックリストの取得) なので、512 件あれば
/// 数か月さかのぼれる。1 件は下の切り詰めで 128 B 以内なので、全部で 96 KiB 以下。
pub const MAX_EVENTS: usize = 512;

/// 1 件の説明に収める長さ (バイト)。長いものは末尾に `…` を付けて切る。
pub const MAX_TEXT: usize = 128;

/// 出来事の種類 (**12 種で固定**)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// 起動した (版と、効いている設定の要約)
    Start,
    /// `.env` を読み直した (変わった名前と前後の値)
    Reload,
    /// ブロックリストを組み直した (件数と、取得の成否)
    Blocklist,
    /// IPv4 優先に切り替えた / 解除した (T12.1)
    Ipv6,
    /// メモリの圧迫を検知した / 解けた (PSI)
    Pressure,
    /// 先行確保 (バラスト) が増えた / 減った
    Ballast,
    /// 状態ファイルの書き込みに失敗した (最初の 1 回)
    StateFile,
    /// 上限に当たって暇なトンネルを閉じた (T13.2。1 時間に初めて起きたとき)
    Evict,
    /// accept が失敗した (記述子の枯渇など。1 時間に初めて起きたとき)
    Emfile,
    /// 停止シグナルを受けた
    Shutdown,
    /// 標本が直近 1 時間の基準値から外れた / 戻った ([`crate::anomaly`]。T14.23)
    Anomaly,
    /// 初めて見た接続元 ([`crate::anomaly`] の規則 6。T14.54)
    NewClient,
}

/// `/events` の `kinds` に出す全種類 (README の一覧と同じ並び)。
pub const KINDS: [EventKind; 12] = [
    EventKind::Start,
    EventKind::Reload,
    EventKind::Blocklist,
    EventKind::Ipv6,
    EventKind::Pressure,
    EventKind::Ballast,
    EventKind::StateFile,
    EventKind::Evict,
    EventKind::Emfile,
    EventKind::Shutdown,
    EventKind::Anomaly,
    // **末尾に足すこと**: ファイルに書く符号は [`KINDS`] の添字なので、間に挟むと
    // 前の版が書いた個票の種類が 1 つずつずれる (T14.9)
    EventKind::NewClient,
];

impl EventKind {
    /// ファイルに書くときの符号 ([`KINDS`] の添字。T14.9)。
    pub fn code(self) -> u64 {
        KINDS.iter().position(|&k| k == self).unwrap_or(0) as u64
    }

    /// 符号から戻す。知らない値は [`EventKind::Start`]。
    pub fn from_code(v: u64) -> EventKind {
        KINDS.get(v as usize).copied().unwrap_or(EventKind::Start)
    }

    /// `/events` の `kind` に出す名前。
    pub fn name(self) -> &'static str {
        match self {
            EventKind::Start => "start",
            EventKind::Reload => "reload",
            EventKind::Blocklist => "blocklist",
            EventKind::Ipv6 => "ipv6",
            EventKind::Pressure => "pressure",
            EventKind::Ballast => "ballast",
            EventKind::StateFile => "state_file",
            EventKind::Evict => "evict",
            EventKind::Emfile => "emfile",
            EventKind::Shutdown => "shutdown",
            EventKind::Anomaly => "anomaly",
            EventKind::NewClient => "new_client",
        }
    }
}

/// 出来事 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// いつ (epoch 秒)
    pub at: u64,
    pub kind: EventKind,
    /// 短い説明 ([`MAX_TEXT`] バイトまで。ログと同じく英語)
    pub text: String,
}

impl Event {
    /// `/events` の 1 要素。
    pub fn to_json(&self) -> String {
        format!(
            "{{\"at\":{},\"kind\":\"{}\",\"text\":\"{}\"}}",
            self.at,
            self.kind.name(),
            crate::json::escape(&self.text)
        )
    }
}

#[derive(Default)]
struct EventRing {
    buf: Vec<Event>,
    /// 次に書く位置 (`buf` が満杯になってからだけ意味を持つ)
    next: usize,
    /// 起動からの通算 (捨てた分も含む)
    total: u64,
    /// **個票のファイルに書いた所までの通算** (T14.9)
    written: u64,
    /// 起動時に読み戻した件数 (`/events` の `"restored"`)
    restored: usize,
}

/// 出来事のリング。**書くのは稀な経路だけ**なので、要求を処理する経路はこの鍵を
/// 1 度も取らない (置き場は使った分だけ伸び、1 件も起きなければ 1 バイトも確保しない)。
static RING: Mutex<EventRing> = Mutex::new(EventRing {
    buf: Vec::new(),
    next: 0,
    total: 0,
    written: 0,
    restored: 0,
});

/// 1 件書く (満杯なら最も古いものを上書きする)。**稀な経路からだけ呼ぶこと。**
pub fn push(kind: EventKind, text: &str) {
    let event = Event {
        at: crate::cache::now_epoch(),
        kind,
        text: crate::recent::clip(text, MAX_TEXT),
    };
    let mut r = RING.locked();
    r.total += 1;
    if r.buf.len() < MAX_EVENTS {
        r.buf.push(event);
        return;
    }
    let at = r.next;
    r.buf[at] = event;
    r.next = (at + 1) % MAX_EVENTS;
}

/// 条件に合うものを**新しい順**で `n` 件まで返す。2 つ目は起動からの通算 (捨てた分も含む)。
///
/// 絞りはここ (鍵の内側) で済ませる。`since` は「その時刻以降に起きたもの」。
pub fn select(since: u64, n: usize) -> (Vec<Event>, u64) {
    let r = RING.locked();
    let len = r.buf.len();
    let mut out = Vec::with_capacity(len.min(n));
    for i in 0..len {
        if out.len() >= n {
            break;
        }
        // `next` の 1 つ手前が最新 (満杯になる前は `next == 0` なので末尾が最新)
        let start = if len < MAX_EVENTS { len } else { r.next };
        let e = &r.buf[(start + len - 1 - i) % len];
        if e.at < since {
            continue;
        }
        out.push(e.clone());
    }
    (out, r.total)
}

/// 覚えている件数。
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
    r.written = 0;
    r.restored = 0;
}

/// まだ個票のファイルに書いていない件を**古い順**で取り出す (T14.9)。
///
/// 呼ぶのは **history スレッドだけ** (5 秒ごと)。`max` を越える分は古い方から落とし、
/// 落とした件数を 2 つ目に返す。印を位置ではなく通算の件数にしてあるのは、
/// リングが古い件を上書きするため。
pub fn take_unwritten(max: usize) -> (Vec<Event>, u64) {
    let mut r = RING.locked();
    let len = r.buf.len();
    let pending = r.total.saturating_sub(r.written).min(len as u64) as usize;
    r.written = r.total;
    if pending == 0 {
        return (Vec::new(), 0);
    }
    let take = pending.min(max);
    let start = if len < MAX_EVENTS { 0 } else { r.next };
    let out = ((len - take)..len)
        .map(|i| r.buf[(start + i) % len].clone())
        .collect();
    (out, (pending - take) as u64)
}

/// 個票のファイルから読み戻す (**起動時に 1 回だけ**。T14.9)。
///
/// 読み戻した件は**書き直さない** (印を通算に合わせる)。件数は `/events` の
/// `"restored"` に出す。**前の版の `shutdown` や `start` が残るのはこの読み戻しのおかげ。**
pub fn restore(events: Vec<Event>) {
    let n = events.len().min(MAX_EVENTS);
    for e in events {
        let mut r = RING.locked();
        r.total += 1;
        if r.buf.len() < MAX_EVENTS {
            r.buf.push(e);
            continue;
        }
        let at = r.next;
        r.buf[at] = e;
        r.next = (at + 1) % MAX_EVENTS;
    }
    let mut r = RING.locked();
    r.written = r.total;
    r.restored = n;
}

/// 再起動前から引き継いだ件数 (`/events` の `"restored"`)。
pub fn restored_count() -> usize {
    RING.locked().restored
}

/// `slot` が覚えている時刻と違う時 (= その 1 時間で最初) なら `true`。
///
/// 起きるたびに書くと 1 つの出来事でリングが埋まるものに掛ける歯止め。
/// 初期値は [`u64::MAX`] にしておくと最初の 1 回は必ず通る。
fn once_per_hour(slot: &AtomicU64) -> bool {
    let hour = crate::cache::now_epoch() / 3600;
    let last = slot.load(Ordering::Relaxed);
    last != hour
        && slot
            .compare_exchange(last, hour, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// 上限に当たって暇なトンネルを 1 本閉じた (T13.2)。**1 時間に初めて起きたときだけ**書く。
pub fn note_evict(open: usize, max: usize) {
    static LAST_HOUR: AtomicU64 = AtomicU64::new(u64::MAX);
    if once_per_hour(&LAST_HOUR) {
        push(
            EventKind::Evict,
            &format!(
                "closed an idle tunnel to make room ({} open, PROXY_MAX_CONNS={})",
                open, max
            ),
        );
    }
}

/// accept が失敗した (記述子の枯渇 `EMFILE` / `ENFILE` など)。
/// **1 時間に初めて起きたときだけ**書く (そのまま回すと 1 秒に何度も来るため)。
pub fn note_accept_error(err: &std::io::Error) {
    static LAST_HOUR: AtomicU64 = AtomicU64::new(u64::MAX);
    if once_per_hour(&LAST_HOUR) {
        push(EventKind::Emfile, &format!("accept failed: {}", err));
    }
}

/// 状態ファイルの書き込みに失敗した (呼ぶ側が**最初の 1 回**に絞っている)。
pub fn note_state_file(path: &std::path::Path, what: &str, err: &std::io::Error) {
    push(
        EventKind::StateFile,
        &format!("{}: {} failed: {}", path.display(), what, err),
    );
}

/// [`poll`] が覚えている前回の状態 (0 = まだ見ていない / 偽、1 = 真)。
static LAST_V4_FIRST: AtomicU8 = AtomicU8::new(0);
static LAST_PRESSURE: AtomicU8 = AtomicU8::new(0);
/// 前に書いたバラストの合計 (バイト)。
static LAST_BALLAST: AtomicU64 = AtomicU64::new(0);

/// バラストが動いたと見なす幅 (これ未満の増減は書かない)。
const BALLAST_STEP: u64 = 64 * 1024 * 1024;

/// 真偽の状態が前回と変わっていたら 1 件書く。**説明を組むのは変わったときだけ**。
fn on_change(state: &AtomicU8, now: bool, kind: EventKind, text: impl FnOnce() -> String) -> bool {
    let changed = u8::from(now) != state.swap(u8::from(now), Ordering::Relaxed);
    if changed {
        push(kind, &text());
    }
    changed
}

/// 先行確保 (バラスト) が [`BALLAST_STEP`] 以上動いていたら 1 件書く。
///
/// 細かい増減で埋めないために幅を置く。書いたときだけ覚えている値を進めるので、
/// 少しずつ積み上がった場合も合計が幅を越えた時点で 1 件になる。
fn on_ballast(state: &AtomicU64, mem: u64, disk: u64) -> bool {
    let now = mem.saturating_add(disk);
    let last = state.load(Ordering::Relaxed);
    if now.abs_diff(last) < BALLAST_STEP {
        return false;
    }
    state.store(now, Ordering::Relaxed);
    let mib = |b: u64| b / (1024 * 1024);
    push(
        EventKind::Ballast,
        &format!(
            "ballast {}{} MiB -> {} MiB (memory {} MiB, disk {} MiB)",
            if now >= last { "+" } else { "-" },
            mib(now.abs_diff(last)),
            mib(now),
            mib(mem),
            mib(disk)
        ),
    );
    true
}

/// 下の層 (`proxy-net` / `proxy-cache`) の状態の変わり目を見て 1 件書く。
///
/// **履歴スレッドの周期から呼ぶ** (5 秒に 1 回。要求の経路からは呼ばない)。
/// 読むのは原子 3 つとプローブが既に取ってある雪像だけで、システムコールは増えない。
pub fn poll(cache: &crate::cache::Cache) {
    // IPv4 優先の切替・解除 (T12.1)
    let v4_first = crate::net::ipv6_v4_first();
    on_change(&LAST_V4_FIRST, v4_first, EventKind::Ipv6, || {
        let [attempts, wins, losses] = crate::net::ipv6_counters();
        if v4_first {
            format!(
                "IPv4 first: IPv6 never succeeded (attempts {}, wins {}, losses {})",
                attempts, wins, losses
            )
        } else {
            format!(
                "IPv6 first again (attempts {}, wins {}, losses {})",
                attempts, wins, losses
            )
        }
    });

    // メモリの圧迫 (PSI)。判定はプローブが行い、ここは結果を読むだけ
    let snap = cache.snapshot();
    let pressure = snap.mem.as_ref().is_some_and(|m| m.under_pressure());
    on_change(&LAST_PRESSURE, pressure, EventKind::Pressure, || {
        let p = snap.mem.as_ref().and_then(|m| m.max_pressure());
        if pressure {
            format!(
                "memory pressure detected (PSI some={:.1}% full={:.1}%): releasing reservations",
                p.map_or(0.0, |p| p.some_avg10),
                p.map_or(0.0, |p| p.full_avg10)
            )
        } else {
            "memory pressure is over".to_string()
        }
    });

    // 先行確保 (バラスト) の増減
    on_ballast(&LAST_BALLAST, cache.mem_reserved(), cache.disk_reserved());
}

/// リングは 1 本きり (静的) なので、テストはこの鍵で 1 つずつ通す
/// ([`crate::anomaly`] のテストもここに書くので、モジュールの外に出してある)。
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK.locked();
        clear();
        g
    }

    #[test]
    fn the_newest_event_comes_first() {
        let _g = guard();
        push(EventKind::Start, "first");
        push(EventKind::Reload, "second");
        let (events, total) = select(0, 10);
        assert_eq!(total, 2);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].text, "second");
        assert_eq!(events[0].kind, EventKind::Reload);
        assert_eq!(events[1].text, "first");
    }

    #[test]
    fn the_ring_keeps_the_last_512_and_counts_the_rest() {
        let _g = guard();
        for i in 0..MAX_EVENTS + 100 {
            push(EventKind::Reload, &format!("n{}", i));
        }
        assert_eq!(len(), MAX_EVENTS);
        let (events, total) = select(0, MAX_EVENTS * 2);
        assert_eq!(total, (MAX_EVENTS + 100) as u64);
        assert_eq!(events.len(), MAX_EVENTS);
        assert_eq!(events[0].text, format!("n{}", MAX_EVENTS + 99));
        assert_eq!(events[MAX_EVENTS - 1].text, "n100");
    }

    #[test]
    fn n_and_since_narrow_the_answer() {
        let _g = guard();
        push(EventKind::Start, "old");
        let mut r = RING.locked();
        r.buf[0].at = 1000;
        drop(r);
        push(EventKind::Reload, "new");
        let now = crate::cache::now_epoch();
        let (events, _) = select(now, 10);
        assert_eq!(events.len(), 1, "since で古い 1 件が落ちる");
        assert_eq!(events[0].text, "new");
        let (events, _) = select(0, 1);
        assert_eq!(events.len(), 1, "n で 1 件に絞れる");
        assert_eq!(events[0].text, "new");
        assert!(select(now + 3600, 10).0.is_empty(), "先の時刻なら 0 件");
    }

    #[test]
    fn the_text_is_clipped_to_128_bytes() {
        let _g = guard();
        push(EventKind::Reload, &"あ".repeat(100));
        let (events, _) = select(0, 1);
        assert!(
            events[0].text.len() <= MAX_TEXT,
            "{} B",
            events[0].text.len()
        );
        assert!(events[0].text.ends_with('…'));
    }

    #[test]
    fn the_twelve_kinds_have_distinct_names() {
        let mut names: Vec<&str> = KINDS.iter().map(|k| k.name()).collect();
        assert_eq!(names.len(), 12);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 12, "名前が重なっている");
    }

    /// 永続化の符号 ([`KINDS`] の添字。T14.9) は**前の版から動かさない**。
    /// 12 種目を足したときに 0〜10 がずれていないことを、ここで押さえる。
    #[test]
    fn the_codes_of_the_old_kinds_never_move() {
        for (want, kind) in [
            (0, EventKind::Start),
            (1, EventKind::Reload),
            (2, EventKind::Blocklist),
            (3, EventKind::Ipv6),
            (4, EventKind::Pressure),
            (5, EventKind::Ballast),
            (6, EventKind::StateFile),
            (7, EventKind::Evict),
            (8, EventKind::Emfile),
            (9, EventKind::Shutdown),
            (10, EventKind::Anomaly),
            (11, EventKind::NewClient),
        ] {
            assert_eq!(kind.code(), want, "{} の符号が動いた", kind.name());
            assert_eq!(EventKind::from_code(want), kind);
        }
    }

    #[test]
    fn once_per_hour_lets_the_first_one_through_only() {
        static SLOT: AtomicU64 = AtomicU64::new(u64::MAX);
        assert!(once_per_hour(&SLOT), "最初の 1 回は通る");
        assert!(!once_per_hour(&SLOT), "同じ時間は通さない");
        SLOT.store(0, Ordering::Relaxed);
        assert!(once_per_hour(&SLOT), "時間が変われば通る");
    }

    #[test]
    fn the_json_escapes_quotes_and_keeps_the_arrow() {
        let e = Event {
            at: 1789251465,
            kind: EventKind::Reload,
            text: "PROXY_TIMEOUT_SECS 30 -> 10 \"x\"".to_string(),
        };
        assert_eq!(
            e.to_json(),
            "{\"at\":1789251465,\"kind\":\"reload\",\"text\":\"PROXY_TIMEOUT_SECS 30 -> 10 \\\"x\\\"\"}"
        );
    }

    #[test]
    fn a_change_is_written_once_per_edge() {
        let _g = guard();
        let state = AtomicU8::new(0);
        assert!(!on_change(&state, false, EventKind::Ipv6, || "no".to_string()));
        assert!(on_change(&state, true, EventKind::Ipv6, || "on".to_string()));
        assert!(!on_change(&state, true, EventKind::Ipv6, || {
            panic!("変わっていないのに説明を組んだ")
        }));
        assert!(on_change(&state, false, EventKind::Ipv6, || "off".to_string()));
        let (events, _) = select(0, 10);
        assert_eq!(events.len(), 2, "変わり目の数だけ");
        assert_eq!(events[0].text, "off");
        assert_eq!(events[1].text, "on");
        assert_eq!(events[0].kind, EventKind::Ipv6);
    }

    #[test]
    fn the_ballast_is_written_only_when_it_moves_64_mib() {
        let _g = guard();
        let state = AtomicU64::new(0);
        let mib = 1024 * 1024;
        assert!(!on_ballast(&state, 32 * mib, 0), "幅に満たない");
        assert!(on_ballast(&state, 64 * mib, 0), "64 MiB で 1 件");
        assert!(
            !on_ballast(&state, 64 * mib, 32 * mib),
            "そこから 32 MiB は幅の中"
        );
        assert!(on_ballast(&state, 0, 0), "返したときも 1 件");
        let (events, _) = select(0, 10);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1].text,
            "ballast +64 MiB -> 64 MiB (memory 64 MiB, disk 0 MiB)"
        );
        assert_eq!(
            events[0].text,
            "ballast -64 MiB -> 0 MiB (memory 0 MiB, disk 0 MiB)"
        );
        assert_eq!(events[0].kind, EventKind::Ballast);
    }

    /// 静かな (圧迫も IPv6 の負けもバラストも無い) プロセスでは 1 件も書かない。
    #[test]
    fn polling_a_quiet_process_writes_nothing() {
        let _g = guard();
        let cache = crate::cache::Cache::new(proxy_cache::config::CacheConfig {
            enabled: false,
            ..Default::default()
        });
        poll(&cache);
        poll(&cache);
        assert!(is_empty(), "静かなときは 1 件も書かない");
    }

    #[test]
    fn a_full_ring_fits_in_96_kib() {
        let _g = guard();
        for i in 0..MAX_EVENTS {
            push(EventKind::Blocklist, &format!("{}{}", "x".repeat(120), i));
        }
        let (events, _) = select(0, MAX_EVENTS);
        let bytes: usize = events.iter().map(|e| e.to_json().len() + 1).sum();
        assert!(bytes <= 96 * 1024, "{} B", bytes);
    }
}
