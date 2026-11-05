//! IPv6 を使うかの旗と、**IPv6 の全体の勝敗から「初めて見るホストで IPv6 を先頭に置くか」を
//! 決める判断** ([`Ipv6Policy`]。T12.1 / T17.7 / T18.1)。
//!
//! 判断は時刻 (epoch 秒) を引数で受けるので、試験は時計を進めずに「600 秒後」の姿を作れる。
//! プロセスに 1 つ ([`POLICY`]) で、数は全部 `Relaxed` の原子。要求の経路で払う費用は
//! `net.rs` に静的変数が並んでいた頃と同じ (原子の読み書きの数も場所も変えていない)。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{log_info, log_warn};

/// IPv6 を絡めるテストは**全体の状態 (連敗と勝敗の数) を共有する**ので直列に回す。
/// 判定 (`proxy-net` の `acl`) 側のテストも取るので `pub`。
pub static IPV6_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// IPv6 を使うか (既定 on)。起動時に設定から決める。
static IPV6_ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_ipv6_enabled(on: bool) {
    IPV6_ENABLED.store(on, Ordering::Relaxed);
}

pub fn ipv6_enabled() -> bool {
    IPV6_ENABLED.load(Ordering::Relaxed)
}

/// IPv6 の試行がこれだけ**続けて**負けたら、初めて見るホストも IPv4 を先頭にする (T12.1)。
pub(super) const LOSS_LIMIT: u64 = 3;
/// IPv4 を先頭にしている間、この秒数に 1 回だけ IPv6 を先頭に戻して試す (生き返れば自然に戻る)。
pub(super) const PROBE_SECS: u64 = 600;

/// IPv6 の全体の勝敗と探りの予約 (T12.1 / T17.7 / T18.1)。
///
/// 「IPv6 が負けた」の定義 (T12.1):
///
/// **IPv6 の候補を試し始めたのに、最後に確立できたのが IPv4 の候補だったとき**を 1 回の負けと数える。
/// これは 2 つの場合をまとめたもので、どちらも「IPv6 を先頭に置いたせいで `stagger()` (既定 250 ms) 待たされた」
/// という同じ結果になるので区別しない:
///
/// - IPv6 の試行が先に失敗し、そのあと IPv4 が勝った (デプロイ先の「経路はあるが約 1 秒で失敗」)
/// - IPv6 の試行が `stagger()` を過ぎても返らないうちに IPv4 が勝った (SYN が黙って落ちる網)
///
/// **どの候補も確立できなかったときは勝ちにも負けにも数えない** (IPv6 の善し悪しではなく、
/// 宛先か網が落ちているだけなので)。勝敗を書くのは確立した時点で 1 回だけ。
///
/// 欄は `pub(super)` で、試験 (`net::tests`) が起動直後の状態や「3 連敗で探りの時刻が来ている」
/// 状態を直接作れるようにしてある。本体から触るのはこの型のメソッドだけ。
pub(super) struct Ipv6Policy {
    /// IPv6 の候補を実際に試した回数 (`/status` と `/metrics`)。
    pub(super) attempts: AtomicU64,
    /// IPv6 の候補が最初に確立した回数。
    pub(super) wins: AtomicU64,
    /// IPv6 を試したのに IPv4 が勝った回数。
    pub(super) losses: AtomicU64,
    /// 連続で負けた数 (勝ったら 0 に戻す)。`wins == 0` のときだけ意味を持つ。
    pub(super) loss_streak: AtomicU64,
    /// 次に IPv6 を先頭に戻して試す時刻 (epoch 秒)。
    pub(super) probe_at: AtomicU64,
    /// 切り替えの `warn` を出したか (1 回だけ)。
    pub(super) warned: AtomicBool,
    /// canary が最後に IPv6 側の 1 本を試した時刻 (epoch 秒。0 = 1 度も試していない。T17.7)。
    ///
    /// これが [`PROBE_SECS`] より新しい間は、**利用者の経路で 600 秒に 1 回の探りをしない**
    /// (探りは canary が 1 分に 1 回、利用者を待たせずにしている)。canary が `off`・`--lite`・
    /// `PROXY_CANARY_IPV6=off`・AAAA の無い宛先しか選べない、のどれでもここが古くなるので、
    /// そのときは今までどおり利用者の経路で探る。
    pub(super) canary_at: AtomicU64,
    /// canary の IPv6 側の 1 本が 1 度でも繋がったか (T17.7)。立ったら `v4_first` を解く
    /// (利用者の要求で IPv6 が 1 度勝ったときと同じ扱い)。
    pub(super) canary_ok: AtomicBool,
    /// `v4_first` の間に**利用者の経路で** IPv6 を探った回数 (T18.1)。
    ///
    /// 数えるのは [`Ipv6Policy::first_for_new_host`] が `v4_first` のときに `true` を返した回
    /// (= 600 秒に 1 回の探りを予約から取り出した回) だけ。`attempts` とは別物で、
    /// あちらは「IPv6 の候補を起動した回数」なので、IPv4 を先頭に置いた接続が `stagger()` を
    /// 過ぎて IPv6 の候補を追い掛けで起動した回 (探りではない) でも増える。
    pub(super) request_probes: AtomicU64,
    /// 最後に利用者の経路で探った時刻 (epoch 秒。0 = 1 度も探っていない。T18.1)。
    pub(super) request_probe_at: AtomicU64,
}

/// プロセスに 1 つの状態 (`/status` の `ipv6` と `/metrics` が読む)。
pub(super) static POLICY: Ipv6Policy = Ipv6Policy::new();

impl Ipv6Policy {
    /// 起動直後の状態 (1 度も試していない)。
    pub(super) const fn new() -> Ipv6Policy {
        Ipv6Policy {
            attempts: AtomicU64::new(0),
            wins: AtomicU64::new(0),
            losses: AtomicU64::new(0),
            loss_streak: AtomicU64::new(0),
            probe_at: AtomicU64::new(0),
            warned: AtomicBool::new(false),
            canary_at: AtomicU64::new(0),
            canary_ok: AtomicBool::new(false),
            request_probes: AtomicU64::new(0),
            request_probe_at: AtomicU64::new(0),
        }
    }

    /// IPv6 の候補を 1 本起動した (1 回の接続で 1 度だけ数える)。
    pub(super) fn note_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    /// IPv6 の候補が勝った。
    pub(super) fn note_win(&self) {
        self.wins.fetch_add(1, Ordering::Relaxed);
        // 定常状態 (毎回 IPv6 が勝つ) では store を増やさない
        if self.loss_streak.load(Ordering::Relaxed) != 0 {
            self.loss_streak.store(0, Ordering::Relaxed);
        }
    }

    /// IPv6 を試したのに IPv4 が勝った。**`true` を返したら呼ぶ側が切り替えの warn を 1 行出す**
    /// (1 度しか返さない)。`now` は 3 連敗で IPv4 優先に落ちた回だけ読む (時計は閉包で受ける)。
    pub(super) fn note_loss(&self, now: impl FnOnce() -> u64) -> bool {
        self.losses.fetch_add(1, Ordering::Relaxed);
        let streak = self.loss_streak.fetch_add(1, Ordering::Relaxed) + 1;
        if streak == LOSS_LIMIT && self.wins.load(Ordering::Relaxed) == 0 {
            self.probe_at.store(now() + PROBE_SECS, Ordering::Relaxed);
            return !self.warned.swap(true, Ordering::Relaxed);
        }
        false
    }

    /// 起動から 1 度も IPv6 が勝たず (canary の IPv6 側の 1 本も繋がらず。T17.7)、
    /// 連続 [`LOSS_LIMIT`] 回負けている状態か。
    pub(super) fn v4_first(&self) -> bool {
        self.v4_first_by_losses() && !self.canary_ok.load(Ordering::Relaxed)
    }

    /// `v4_first` のうち、canary の結果を除いた部分 (解いたときのログを 1 回だけ出すため)。
    fn v4_first_by_losses(&self) -> bool {
        self.wins.load(Ordering::Relaxed) == 0
            && self.loss_streak.load(Ordering::Relaxed) >= LOSS_LIMIT
    }

    /// canary の IPv6 側の 1 本の結果を受け取る (T17.7)。繋がっても繋がらなくても
    /// 「canary が探っている」印を更新し、繋がったら `v4_first` を解く。
    /// **`true` を返したら呼ぶ側が「IPv6 reachable」の info を 1 行出す** (解けた最初の 1 回だけ)。
    pub(super) fn note_canary(&self, now: u64, connected: bool) -> bool {
        self.canary_at.store(now, Ordering::Relaxed);
        connected && !self.canary_ok.swap(true, Ordering::Relaxed) && self.v4_first_by_losses()
    }

    /// いま IPv6 の探りを canary が引き受けているか (直近 [`PROBE_SECS`] 秒に 1 本試した。T17.7)。
    fn probe_by_canary(&self, now: u64) -> bool {
        let at = self.canary_at.load(Ordering::Relaxed);
        at != 0 && now.saturating_sub(at) < PROBE_SECS
    }

    /// 記憶の無いホストで IPv6 を先頭に置くか。IPv4 を先頭にしている間も
    /// [`PROBE_SECS`] に 1 回だけ `true` を返して IPv6 を試す (勝てば解除される)。
    ///
    /// **canary が IPv6 を探っている間は利用者の経路で探らない** (T17.7。探りの 1 本は
    /// `stagger()` ぶん利用者を待たせるため)。解除は canary の 1 本が繋がったとき
    /// ([`Ipv6Policy::note_canary`])。`now` は `v4_first` のときだけ読む (時計は閉包で受ける)。
    pub(super) fn first_for_new_host(&self, now: impl FnOnce() -> u64) -> bool {
        if !self.v4_first() {
            return true;
        }
        let now = now();
        if self.probe_by_canary(now) {
            return false;
        }
        let at = self.probe_at.load(Ordering::Relaxed);
        let probe = now >= at
            && self
                .probe_at
                .compare_exchange(at, now + PROBE_SECS, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok();
        if probe {
            // 計器 (T18.1): 要求の経路に増えるのは**探りの 1 回 (600 秒に 1 回) に**この
            // `fetch_add` 1 つと `store` 1 つだけ。探らない回 (`v4_first` でない、
            // canary が探っている、予約の時刻が来ていない) は原子の操作を 1 つも足さない
            self.request_probes.fetch_add(1, Ordering::Relaxed);
            self.request_probe_at.store(now, Ordering::Relaxed);
        }
        probe
    }

    /// `/status` の `"ipv6"` 要素 (組み立ては `/status` のときだけ)。
    pub(super) fn status_json(&self, now: u64) -> String {
        format!(
            "{{\"attempts\":{},\"wins\":{},\"losses\":{},\"v4_first\":{},\"probe_by\":\"{}\",\"request_probes\":{},\"request_probe_at\":{}}}",
            self.attempts.load(Ordering::Relaxed),
            self.wins.load(Ordering::Relaxed),
            self.losses.load(Ordering::Relaxed),
            self.v4_first(),
            // 誰が IPv6 を探っているか (T17.7): canary が直近 600 秒に試していれば `canary`
            if self.probe_by_canary(now) {
                "canary"
            } else {
                "request"
            },
            // `v4_first` の間に利用者の経路で探った回数と、最後の時刻 (epoch 秒。無ければ 0。T18.1)。
            // `probe_by` は「いま」だけなので、canary が見ていなかった時間があったかはここで読む
            self.request_probes.load(Ordering::Relaxed),
            self.request_probe_at.load(Ordering::Relaxed),
        )
    }

    /// Prometheus 用のカウンタ (attempts, wins, losses)。
    pub(super) fn counters(&self) -> [u64; 3] {
        [
            self.attempts.load(Ordering::Relaxed),
            self.wins.load(Ordering::Relaxed),
            self.losses.load(Ordering::Relaxed),
        ]
    }

    /// 起動直後の状態に戻す (テスト同士が互いの連敗を見ないように)。
    #[cfg(test)]
    pub(super) fn reset(&self) {
        for c in [
            &self.attempts,
            &self.wins,
            &self.losses,
            &self.loss_streak,
            &self.probe_at,
            &self.canary_at,
            &self.request_probes,
            &self.request_probe_at,
        ] {
            c.store(0, Ordering::Relaxed);
        }
        self.warned.store(false, Ordering::Relaxed);
        self.canary_ok.store(false, Ordering::Relaxed);
    }
}

/// IPv6 の候補を起動した (`connect_candidates` が 1 回の接続で 1 度だけ呼ぶ)。
pub(super) fn note_ipv6_attempt() {
    POLICY.note_attempt();
}

/// IPv6 の候補が勝った ([`Ipv6Policy`] の doc にある「負け」の定義の裏返し)。
pub(super) fn note_ipv6_win() {
    POLICY.note_win();
}

/// IPv6 を試したのに IPv4 が勝った。3 連敗で IPv4 優先に落ちた最初の 1 回だけ warn を出す。
pub(super) fn note_ipv6_loss() {
    if POLICY.note_loss(crate::clock::now_epoch) {
        log_warn!(
            None,
            "IPv6 attempts never succeed; trying IPv4 first (set PROXY_IPV6=off to skip IPv6)"
        );
    }
}

/// 起動から 1 度も IPv6 が勝たず (canary の IPv6 側の 1 本も繋がらず。T17.7)、
/// 連続 [`LOSS_LIMIT`] 回負けている状態か。
pub fn ipv6_v4_first() -> bool {
    POLICY.v4_first()
}

/// canary の IPv6 側の 1 本の結果を受け取る (canary スレッドから 1 分に 1 回。T17.7)。
///
/// 下の層 (このクレート) は canary を知らないので、canary の方から呼んでもらう。
/// 勝敗の数 (`attempts` / `wins` / `losses`) と族の記憶には**足さない** (利用者の要求ではない)。
/// 繋がったら `v4_first` を解く。繋がっても繋がらなくても「canary が探っている」印を更新する。
pub fn note_canary_ipv6(connected: bool) {
    if POLICY.note_canary(crate::clock::now_epoch(), connected) {
        log_info!(None, "IPv6 reachable (canary); trying IPv6 first again");
    }
}

/// 記憶の無いホストで IPv6 を先頭に置くか ([`Ipv6Policy::first_for_new_host`] を今の時刻で)。
pub(super) fn ipv6_first_for_new_host() -> bool {
    POLICY.first_for_new_host(crate::clock::now_epoch)
}

/// `/status` の `"ipv6"` 要素 (組み立ては `/status` のときだけ)。
pub fn ipv6_status_json() -> String {
    POLICY.status_json(crate::clock::now_epoch())
}

/// Prometheus 用のカウンタ (attempts, wins, losses)。
pub fn ipv6_counters() -> [u64; 3] {
    POLICY.counters()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_800_000_000;

    /// 3 連敗 (勝ち無し) で IPv4 優先に落ち、warn は最初の 1 回だけ。探りは 600 秒に 1 回 (T12.1)。
    #[test]
    fn three_losses_without_a_win_switch_to_v4_first_and_probe_every_600_s() {
        let p = Ipv6Policy::new();
        assert!(p.first_for_new_host(|| panic!("v4_first でなければ時計は読まない")));
        assert!(
            !p.note_loss(|| T0) && !p.note_loss(|| T0),
            "2 連敗までは落ちない"
        );
        assert!(!p.v4_first());
        assert!(p.note_loss(|| T0), "3 連敗で落ち、warn を 1 行出す");
        assert!(p.v4_first());
        assert_eq!(p.counters(), [0, 0, 3]);
        // 予約は 600 秒後。それまでは探らず、来たら 1 回だけ探る
        assert!(!p.first_for_new_host(|| T0 + 599));
        assert!(p.first_for_new_host(|| T0 + 600), "600 秒たったら 1 回探る");
        assert!(
            !p.first_for_new_host(|| T0 + 600),
            "同じ時刻の 2 本目は探らない"
        );
        assert_eq!(p.request_probes.load(Ordering::Relaxed), 1);
        assert_eq!(p.request_probe_at.load(Ordering::Relaxed), T0 + 600);
        assert!(p.first_for_new_host(|| T0 + 1200));
        // 4 連敗目は warn を繰り返さない
        assert!(!p.note_loss(|| T0 + 1200));
    }

    /// 1 度でも勝てば連敗は 0 に戻り、以後 3 連敗しても (勝ちがあるので) IPv4 優先には落ちない。
    #[test]
    fn a_win_clears_the_streak_and_a_won_process_never_falls_back() {
        let p = Ipv6Policy::new();
        assert!(!p.note_loss(|| T0) && !p.note_loss(|| T0));
        p.note_win();
        assert_eq!(p.loss_streak.load(Ordering::Relaxed), 0);
        for _ in 0..5 {
            assert!(!p.note_loss(|| T0));
        }
        assert!(!p.v4_first(), "勝ったことのあるプロセスは落ちない");
        assert_eq!(p.counters(), [0, 1, 7]);
        assert!(p.first_for_new_host(|| panic!("時計は読まない")));
    }

    /// canary が探っている間 (600 秒以内) は利用者の経路で探らず、繋がったら `v4_first` が解ける (T17.7)。
    #[test]
    fn the_canary_takes_over_probing_while_it_is_fresh_and_lifts_v4_first_when_it_connects() {
        let p = Ipv6Policy::new();
        for _ in 0..LOSS_LIMIT {
            p.note_loss(|| T0);
        }
        assert!(p.v4_first());
        // 繋がらない canary: 印だけ更新。予約の時刻が来ていても利用者の経路では探らない
        assert!(!p.note_canary(T0 + 700, false));
        assert!(!p.first_for_new_host(|| T0 + 700));
        assert_eq!(p.request_probes.load(Ordering::Relaxed), 0);
        assert!(
            p.status_json(T0 + 700)
                .contains(",\"v4_first\":true,\"probe_by\":\"canary\",")
        );
        // 600 秒たって古くなれば利用者の経路に戻る
        assert!(
            p.status_json(T0 + 1300)
                .contains("\"probe_by\":\"request\"")
        );
        assert!(p.first_for_new_host(|| T0 + 1300));
        // 繋がった canary: 解けた最初の 1 回だけ info (2 回目は出さない)
        assert!(p.note_canary(T0 + 1400, true));
        assert!(!p.v4_first());
        assert!(!p.note_canary(T0 + 1460, true));
        assert_eq!(p.counters(), [0, 0, 3], "canary は勝敗に数えない");
    }

    /// `/status` の鍵の順は固定で、新しい 2 つは末尾 (T18.1)。
    #[test]
    fn status_json_keeps_the_key_order() {
        let p = Ipv6Policy::new();
        assert_eq!(
            p.status_json(T0),
            "{\"attempts\":0,\"wins\":0,\"losses\":0,\"v4_first\":false,\"probe_by\":\"request\",\"request_probes\":0,\"request_probe_at\":0}"
        );
        p.note_attempt();
        p.note_win();
        assert_eq!(
            p.status_json(T0),
            "{\"attempts\":1,\"wins\":1,\"losses\":0,\"v4_first\":false,\"probe_by\":\"request\",\"request_probes\":0,\"request_probe_at\":0}"
        );
    }
}
