//! 確立が続けて時間切れになった宛先を覚えて、しばらく待たずに断る (T36.8。利用者の判断)。
//!
//! 同じ宛先 (`host:port`) への確立が [`STRIKES`] 回続けて時間切れ (`TimedOut`) になったら、
//! `PROXY_CONNECT_NEGATIVE_SECS` (既定 30 秒、`0` で覚えない) のあいだは繋ぎに行かずに
//! [`Skipped`] で断る (呼び手は 504 を返す)。期限が過ぎたら**最初の 1 本だけ**通して試し
//! (その 1 本が走っている間も、ほかは断り続ける)、繋がれば忘れ、また時間切れなら同じ長さだけ断り直す。
//! 時間切れでない結末 (繋がった・断られた・経路が無い) は「続けて」を切るので、覚えていた数を捨てる。
//!
//! **熱い経路の費用**: 表が空のあいだは、確立 1 本につき原子の読み 1 回と分岐だけ (鍵も時計も読まない)。
//! 表に何か載っているあいだは、確立 1 本につき鍵 1 回と表の参照 1 回 (`/metrics` の `entries`)。
//! 繋がった 1 本が表に無ければ、後始末は分岐だけ。プールから出す forward は確立しないので通らない。
//!
//! 時刻は引数で受ける ([`Negative::admit`] / [`Negative::settle`]) ので、試験は時計を進めずに
//! 「30 秒後」の姿を作れる (`ipv6::Ipv6Policy` と同じ形)。

use std::collections::HashMap;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::log_debug;
use crate::sync::LockExt;

/// 時間切れがこの回数続いたら断り始める (利用者の判断: 2 回)。
pub(super) const STRIKES: u8 = 2;
/// 断る長さの既定 (`PROXY_CONNECT_NEGATIVE_SECS`。利用者の判断: 30 秒)。
pub const DEFAULT_CONNECT_NEGATIVE: Duration = Duration::from_secs(30);
/// 覚える宛先の上限。死んだ宛先は tokyo で数個なので、256 あれば溢れない。
/// 溢れたら、最後に時間切れになったのが一番古い宛先を 1 つ忘れる (時間切れの経路でだけ総当たりする)。
pub(super) const MAX_ENTRIES: usize = 256;

/// 断ったときのエラーの中身 ([`is_skipped`] で見分ける。種類は `TimedOut` なので、
/// `/errors` と `/hosts` の原因は今までの時間切れと同じ `timeout` に入る)。
#[derive(Debug)]
pub(super) struct Skipped {
    /// 次に 1 本通して試すまでの秒 (切り上げ)
    left_secs: u64,
}

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "connect timed out {} times in a row; not trying again for {} s (PROXY_CONNECT_NEGATIVE_SECS)",
            STRIKES, self.left_secs
        )
    }
}

impl std::error::Error for Skipped {}

/// 覚えていたので繋ぎに行かずに断ったエラーか (呼び手は 502 ではなく 504 を返す)。
pub fn is_skipped(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Skipped>())
}

/// 1 つの宛先について覚えていること。
#[derive(Clone, Copy, Debug)]
struct Mark {
    /// 続けて時間切れになった回数
    strikes: u8,
    /// 最後に時間切れになった時刻 (表が溢れたときに一番古いものを忘れる)
    last: Instant,
    /// この時刻までは断る ([`STRIKES`] 回に届いてから)。過ぎたら 1 本だけ通して、通した時刻 + 長さに延ばす
    until: Option<Instant>,
}

/// [`Negative::admit`] の答え。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Gate {
    /// 繋ぎに行く。`tracked` は表に載っていた宛先か (繋がったら表から消す要があるか)
    Pass { tracked: bool },
    /// 断る (次に試すまでの残り)
    Skip(Duration),
}

/// 確立の時間切れの表と数え物 (プロセスに 1 つ = [`NEGATIVE`])。
pub(super) struct Negative {
    /// 断る長さ (ms)。0 なら覚えない
    hold_ms: AtomicU64,
    /// 表の行数 (表が空のときに鍵を取らずに抜けるための写し。書くのは鍵の中だけ)
    len: AtomicUsize,
    table: Mutex<Option<HashMap<String, Mark>>>,
    /// 断った本数 (通算)
    skipped: AtomicU64,
    /// 断る期限を置いた回数 ([`STRIKES`] 回目からの時間切れごとに 1。試しの 1 本がまた時間切れになった回も入る。通算)
    opened: AtomicU64,
    /// 期限が過ぎて 1 本だけ通した回数 (通算)
    probes: AtomicU64,
    /// 試しの 1 本が繋がって (時間切れ以外で終わって) 忘れた回数 (通算)
    recovered: AtomicU64,
    /// 表が溢れて忘れた回数 (通算)
    evicted: AtomicU64,
}

/// プロセスに 1 つの表 (`/status` の `connect_negative` と `/metrics` が読む)。
pub(super) static NEGATIVE: Negative = Negative::new(DEFAULT_CONNECT_NEGATIVE);

impl Negative {
    pub(super) const fn new(hold: Duration) -> Negative {
        Negative {
            hold_ms: AtomicU64::new(hold.as_millis() as u64),
            len: AtomicUsize::new(0),
            table: Mutex::new(None),
            skipped: AtomicU64::new(0),
            opened: AtomicU64::new(0),
            probes: AtomicU64::new(0),
            recovered: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
        }
    }

    fn hold(&self) -> Duration {
        Duration::from_millis(self.hold_ms.load(Ordering::Relaxed))
    }

    /// 断る長さを変える。0 にしたら表を捨てる (覚えていた宛先もその場で断らなくなる)。
    pub(super) fn set_hold(&self, hold: Duration) {
        self.hold_ms
            .store(hold.as_millis() as u64, Ordering::Relaxed);
        if hold.is_zero() {
            let mut t = self.table.locked();
            *t = None;
            self.len.store(0, Ordering::Relaxed);
        }
    }

    /// 繋ぎに行く前に聞く。`now` は表にこの宛先が載っていたときだけ読む。
    #[inline]
    pub(super) fn admit(&self, key: &str, now: impl FnOnce() -> Instant) -> Gate {
        // 表が空 (普段と、`0` で切ったとき) は鍵も時計も読まない
        if self.len.load(Ordering::Relaxed) == 0 {
            return Gate::Pass { tracked: false };
        }
        self.admit_slow(key, now)
    }

    #[inline(never)]
    fn admit_slow(&self, key: &str, now: impl FnOnce() -> Instant) -> Gate {
        let mut t = self.table.locked();
        let Some(mark) = t.as_mut().and_then(|m| m.get_mut(key)) else {
            return Gate::Pass { tracked: false };
        };
        let Some(until) = mark.until else {
            // 1 回目の時間切れだけ覚えている: 繋ぎに行く
            return Gate::Pass { tracked: true };
        };
        let now = now();
        if now < until {
            drop(t);
            self.skipped.fetch_add(1, Ordering::Relaxed);
            return Gate::Skip(until - now);
        }
        // 期限が過ぎた: この 1 本だけ通して試す。試している間も (同じ長さのあいだ) ほかは断る。
        // 結末が返らなかった (名前解決で失敗した・スレッドが死んだ) ときは、長さが過ぎれば次の 1 本が試す
        mark.until = Some(now + self.hold());
        drop(t);
        self.probes.fetch_add(1, Ordering::Relaxed);
        Gate::Pass { tracked: true }
    }

    /// 繋ぎに行った結末を書く。`timed_out` は確立の時間切れか。`now` は書くときだけ読む。
    #[inline]
    pub(super) fn settle(
        &self,
        key: &str,
        gate: Gate,
        timed_out: bool,
        now: impl FnOnce() -> Instant,
    ) {
        match (gate, timed_out) {
            (Gate::Pass { tracked: false }, false) | (Gate::Skip(_), _) => {}
            (Gate::Pass { tracked: true }, false) => self.forget(key),
            (Gate::Pass { .. }, true) => self.strike(key, now()),
        }
    }

    /// 時間切れでない結末で、続けての数を切る (表から消す)。
    #[inline(never)]
    fn forget(&self, key: &str) {
        let mut t = self.table.locked();
        let Some(m) = t.as_mut() else { return };
        if let Some(mark) = m.remove(key) {
            self.len.store(m.len(), Ordering::Relaxed);
            drop(t);
            if mark.until.is_some() {
                self.recovered.fetch_add(1, Ordering::Relaxed);
                log_debug!(None, "{} answered again; connecting normally", key);
            }
        }
    }

    /// 時間切れを 1 回数える。[`STRIKES`] 回に届いたら断り始める。
    #[inline(never)]
    fn strike(&self, key: &str, now: Instant) {
        let hold = self.hold();
        if hold.is_zero() {
            return;
        }
        let mut t = self.table.locked();
        let m = t.get_or_insert_with(HashMap::new);
        if !m.contains_key(key) && m.len() >= MAX_ENTRIES {
            // 最後に時間切れになったのが一番古い宛先を 1 つ忘れる
            if let Some(oldest) = m.iter().min_by_key(|(_, v)| v.last).map(|(k, _)| k.clone()) {
                m.remove(&oldest);
                self.evicted.fetch_add(1, Ordering::Relaxed);
            }
        }
        let mark = m.entry(key.to_string()).or_insert(Mark {
            strikes: 0,
            last: now,
            until: None,
        });
        mark.strikes = mark.strikes.saturating_add(1);
        mark.last = now;
        let opened = mark.strikes >= STRIKES;
        if opened {
            mark.until = Some(now + hold);
        }
        let strikes = mark.strikes;
        self.len.store(m.len(), Ordering::Relaxed);
        drop(t);
        if opened {
            self.opened.fetch_add(1, Ordering::Relaxed);
            log_debug!(
                None,
                "connect to {} timed out {} times in a row; refusing it for {} s",
                key,
                strikes,
                hold.as_secs()
            );
        }
    }

    /// いま断っている宛先の数 (期限の過ぎていないもの)。
    fn blocked(&self, now: Instant) -> usize {
        let t = self.table.locked();
        t.as_ref().map_or(0, |m| {
            m.values()
                .filter(|v| v.until.is_some_and(|u| now < u))
                .count()
        })
    }

    /// `/status` の `"connect_negative"` 要素 (組み立ては `/status` のときだけ)。
    pub(super) fn status_json(&self, now: Instant) -> String {
        format!(
            "{{\"secs\":{},\"entries\":{},\"blocked\":{},\"skipped\":{},\"opened\":{},\"probes\":{},\"recovered\":{},\"evicted\":{}}}",
            self.hold().as_secs(),
            self.len.load(Ordering::Relaxed),
            self.blocked(now),
            self.skipped.load(Ordering::Relaxed),
            self.opened.load(Ordering::Relaxed),
            self.probes.load(Ordering::Relaxed),
            self.recovered.load(Ordering::Relaxed),
            self.evicted.load(Ordering::Relaxed),
        )
    }

    /// Prometheus 用 (表の行数, 断った本数)。
    pub(super) fn counters(&self) -> [u64; 2] {
        [
            self.len.load(Ordering::Relaxed) as u64,
            self.skipped.load(Ordering::Relaxed),
        ]
    }
}

/// 確立の時間切れを覚える長さを決める (起動時と再読込。`0` で覚えない)。
pub fn set_connect_negative(hold: Duration) {
    NEGATIVE.set_hold(hold);
}

/// `/status` の `"connect_negative"` 要素。
pub fn connect_negative_status_json() -> String {
    NEGATIVE.status_json(Instant::now())
}

/// Prometheus 用 (表の行数, 断った本数)。
pub fn connect_negative_counters() -> [u64; 2] {
    NEGATIVE.counters()
}

/// 断ったときのエラー (`TimedOut` の種類に [`Skipped`] を包む)。
pub(super) fn skipped_error(left: Duration) -> io::Error {
    // 0.2 秒残りを「0 秒」と書かないように切り上げる
    let left_secs = left.as_millis().div_ceil(1000) as u64;
    io::Error::new(io::ErrorKind::TimedOut, Skipped { left_secs })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "mx.example.net:25";

    fn timed_out(n: &Negative, key: &str, at: Instant) {
        let gate = n.admit(key, || at);
        assert!(matches!(gate, Gate::Pass { .. }), "{gate:?}");
        n.settle(key, gate, true, || at);
    }

    /// 1 回の時間切れでは断らない。2 回続けたら 30 秒のあいだ断り、時計を読むのは表に載った宛先だけ。
    #[test]
    fn two_timeouts_in_a_row_refuse_the_target_for_the_hold() {
        let n = Negative::new(Duration::from_secs(30));
        let t0 = Instant::now();
        assert_eq!(
            n.admit(KEY, || panic!("表が空なら時計を読まない")),
            Gate::Pass { tracked: false }
        );
        timed_out(&n, KEY, t0);
        assert_eq!(
            n.admit(KEY, || t0),
            Gate::Pass { tracked: true },
            "1 回目では断らない"
        );
        timed_out(&n, KEY, t0 + Duration::from_secs(3));
        let t1 = t0 + Duration::from_secs(3);
        assert_eq!(
            n.admit(KEY, || t1 + Duration::from_secs(10)),
            Gate::Skip(Duration::from_secs(20))
        );
        assert_eq!(
            n.admit("other.example.net:25", || panic!(
                "載っていない宛先は時計を読まない"
            )),
            Gate::Pass { tracked: false },
            "ほかの宛先は断らない"
        );
        assert_eq!(n.counters(), [1, 1]);
        assert!(
            n.status_json(t1).starts_with(
                "{\"secs\":30,\"entries\":1,\"blocked\":1,\"skipped\":1,\"opened\":1,"
            ),
            "{}",
            n.status_json(t1)
        );
    }

    /// 期限が過ぎたら 1 本だけ通す。その 1 本が走っている間は断り続け、繋がれば忘れる。
    #[test]
    fn after_the_hold_one_probe_passes_and_success_forgets() {
        let n = Negative::new(Duration::from_secs(30));
        let t0 = Instant::now();
        timed_out(&n, KEY, t0);
        timed_out(&n, KEY, t0);
        let t1 = t0 + Duration::from_secs(30);
        let probe = n.admit(KEY, || t1);
        assert_eq!(
            probe,
            Gate::Pass { tracked: true },
            "30 秒過ぎたら 1 本通す"
        );
        assert!(
            matches!(n.admit(KEY, || t1 + Duration::from_secs(1)), Gate::Skip(_)),
            "試しの 1 本が走っている間はほかを断る"
        );
        n.settle(KEY, probe, false, || panic!("繋がったら時計は要らない"));
        assert_eq!(n.admit(KEY, || t1), Gate::Pass { tracked: false }, "忘れた");
        assert_eq!(n.counters(), [0, 1]);
        assert!(
            n.status_json(t1)
                .ends_with("\"probes\":1,\"recovered\":1,\"evicted\":0}")
        );
    }

    /// 試しの 1 本がまた時間切れなら、そこから同じ長さだけ断り直す。
    #[test]
    fn a_probe_that_times_out_again_refuses_for_another_hold() {
        let n = Negative::new(Duration::from_secs(30));
        let t0 = Instant::now();
        timed_out(&n, KEY, t0);
        timed_out(&n, KEY, t0);
        let t1 = t0 + Duration::from_secs(31);
        let probe = n.admit(KEY, || t1);
        let t2 = t1 + Duration::from_secs(3);
        n.settle(KEY, probe, true, || t2);
        assert_eq!(
            n.admit(KEY, || t2 + Duration::from_secs(29)),
            Gate::Skip(Duration::from_secs(1))
        );
        assert!(matches!(
            n.admit(KEY, || t2 + Duration::from_secs(30)),
            Gate::Pass { tracked: true }
        ));
    }

    /// 時間切れの間に時間切れでない結末 (断られた・繋がった) がはさまれば、続けてではない。
    #[test]
    fn another_outcome_in_between_breaks_the_streak() {
        let n = Negative::new(Duration::from_secs(30));
        let t0 = Instant::now();
        timed_out(&n, KEY, t0);
        let gate = n.admit(KEY, || t0);
        n.settle(KEY, gate, false, || t0);
        timed_out(&n, KEY, t0);
        assert_eq!(
            n.admit(KEY, || t0),
            Gate::Pass { tracked: true },
            "まだ 1 回目"
        );
    }

    /// `0` なら覚えない。途中で `0` にしたら、覚えていた宛先もその場で断らなくなる。
    #[test]
    fn zero_turns_it_off_and_drops_the_table() {
        let n = Negative::new(Duration::ZERO);
        let t0 = Instant::now();
        timed_out(&n, KEY, t0);
        timed_out(&n, KEY, t0);
        assert_eq!(n.admit(KEY, || t0), Gate::Pass { tracked: false });
        n.set_hold(Duration::from_secs(30));
        timed_out(&n, KEY, t0);
        timed_out(&n, KEY, t0);
        assert!(matches!(n.admit(KEY, || t0), Gate::Skip(_)));
        n.set_hold(Duration::ZERO);
        assert_eq!(n.admit(KEY, || t0), Gate::Pass { tracked: false });
        assert_eq!(n.counters()[0], 0);
    }

    /// 表は [`MAX_ENTRIES`] で頭打ち。溢れたら最後の時間切れが一番古い宛先を忘れる。
    #[test]
    fn the_table_is_capped_and_forgets_the_oldest() {
        let n = Negative::new(Duration::from_secs(30));
        let t0 = Instant::now();
        for i in 0..MAX_ENTRIES {
            timed_out(
                &n,
                &format!("h{i}:25"),
                t0 + Duration::from_millis(i as u64),
            );
        }
        timed_out(&n, "new:25", t0 + Duration::from_secs(1));
        assert_eq!(n.counters()[0], MAX_ENTRIES as u64);
        assert_eq!(
            n.admit("h0:25", || t0),
            Gate::Pass { tracked: false },
            "一番古いのを忘れた"
        );
        assert_eq!(n.admit("h1:25", || t0), Gate::Pass { tracked: true });
        assert!(n.status_json(t0).ends_with("\"evicted\":1}"));
    }

    /// 断ったエラーは `TimedOut` で、見分けられ、残りの秒を切り上げて書く。
    #[test]
    fn the_skip_error_is_a_timeout_that_can_be_told_apart() {
        let e = skipped_error(Duration::from_millis(200));
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(is_skipped(&e));
        assert!(e.to_string().contains("not trying again for 1 s"), "{e}");
        assert!(!is_skipped(&io::Error::from(io::ErrorKind::TimedOut)));
    }
}
