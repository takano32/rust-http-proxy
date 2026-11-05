//! 個票のリングへの記録 (`/errors` `/recent` `/bursts`) と、断った要求・オリジン側の失敗の WARN の門
//! (T34.26 で `metrics.rs` から移した)。
//!
//! 行を動かしただけで中身は変えていない。

use super::*;

impl Metrics {
    /// エラー 1 件を個票のリングに写す (`/errors`。T13.4)。
    ///
    /// **エラーを返す経路からだけ呼ぶこと。** 集計 (`/status`) では「どのホストで何件」
    /// までしか分からず、デプロイ先で 2 秒かかって失敗した名前解決の**相手と時刻**が
    /// 読めなかった (T13.0)。原因の分からないエラー (`detail.cause` が `None`) は
    /// 書かない — 原因なしの行が並んでも読む人の手が増えないため。
    pub fn record_error(
        &self,
        connect: bool,
        target: &str,
        client: &str,
        status: u16,
        detail: &Detail,
    ) {
        // 名前解決の失敗は内訳 (`nxdomain` / `dns_temp`) で個票に出す (T25.0)。
        // `detail.cause` が `Dns` でも `dns_cause` が無い (古い経路・canary) ときは
        // 今までどおり `dns` のまま
        let cause = match (detail.cause, detail.dns_cause) {
            (Some(ErrCause::Dns), Some(d)) => crate::recent::EntryCause::ErrorDns(d),
            (Some(c), _) => crate::recent::EntryCause::Error(c),
            (None, _) => return,
        };
        self.errors.push(crate::recent::ErrorEntry::new(
            crate::recent::EntryKind::from_connect(connect),
            target,
            client,
            status,
            cause,
            detail.dns_ms,
            detail.connect_ms,
        ));
    }

    /// canary (T14.10) の失敗を個票のリングに 1 件だけ残す (`/errors` の `kind: "canary"`)。
    ///
    /// **集計 (`errors` / `errors_by_cause`) には足さない**: canary は利用者の要求では
    /// ないので、「利用者に返したエラー」の数に混ざると `/status` が読めなくなる。
    /// 接続元は空 (自分) で、返した状態コードも無い (0)。
    pub fn record_canary_error(&self, target: &str, cause: ErrCause, dns_ms: u64, connect_ms: u64) {
        self.errors.push(crate::recent::ErrorEntry::new(
            crate::recent::EntryKind::Canary,
            target,
            "",
            0,
            crate::recent::EntryCause::Error(cause),
            dns_ms,
            connect_ms,
        ));
    }

    /// 閉じた接続を `/connections` から外し、個票を 1 件残す (`/recent`。T14.4)。
    ///
    /// **接続の終了で 1 回だけ呼ぶこと** (`ActiveGuard::drop` = 接続の寿命そのもの)。
    /// 取る鍵は表の鍵 1 回 (元からある抹消のぶん) とリングの鍵 1 回だけで、
    /// 何を書くかは枠 ([`crate::recent::ConnSlot`]) に既に載っている。
    pub fn record_closed(&self, id: u64) {
        if let Some(slot) = self.conns.unregister(id)
            && let Some(entry) = slot.closed_entry(std::time::Instant::now())
        {
            // 閉じた理由・寿命・上り下りのバイト・預けられていた秒を窓に畳む (T14.6)。
            // **2,000 件のリングを読み直さない**: 1 件を作ったこの場で、区間の値として
            // 足しておく (どちらも鍵 1 回、原子操作もシステムコールも増えない)
            self.history.closed.observe(&entry);
            self.closed.push(entry);
        }
    }

    /// 山の写真を 1 枚撮る (**history スレッドが 5 秒ごとに呼ぶ**。T14.6)。
    ///
    /// 頼まれていなければ、山が引いたかどうかだけ見て戻る (原子の読み 2 回)。
    /// 頼まれていたら `/connections` の表から 1 枚作る (**表の鍵 1 回**)。
    /// 越えた接続を受けたスレッドは旗を立てるだけなので、accept の経路に鍵は増えない。
    pub fn take_burst_shot(&self) {
        let active = self.active_connections.load(Ordering::Relaxed);
        let Some(trigger) = self.bursts.take_pending() else {
            self.bursts.rearm_if_calm(active);
            return;
        };
        let rows = self.conns.snapshot();
        let shot = crate::recent::BurstShot::take(
            &rows,
            self.bursts.next_seq(),
            active,
            trigger,
            self.bursts.max_conns(),
            self.bursts.threshold(),
            self.evicted_idle.load(Ordering::Relaxed),
            self.rejected_overload.load(Ordering::Relaxed),
        );
        self.bursts.push(shot);
    }

    /// 403 で拒否した 1 件を個票のリングに写す (`/errors`。T14.2 (4))。
    ///
    /// 集計 (`errors_by_cause`) は**変えない** ([`BlockCause`] の説明のとおり、
    /// 配列を伸ばすと `.rrd` の版が上がる)。ここも**拒否した経路からだけ**通るので、
    /// 通した要求には 1 命令も足さない。
    pub fn record_blocked(&self, connect: bool, target: &str, client: &str, cause: BlockCause) {
        self.errors.push(crate::recent::ErrorEntry::new(
            crate::recent::EntryKind::from_connect(connect),
            target,
            client,
            403,
            crate::recent::EntryCause::Blocked(cause),
            0,
            0,
        ));
    }

    /// 読めなかった要求を 1 件数え、個票のリングにも写す (`/errors`。T14.28)。
    ///
    /// **断る経路からだけ呼ぶこと** (400 / 414 / 431 を返して閉じる場所)。
    /// `status` は**実際にクライアントへ返した状態コード**で、要求行が壊れていた
    /// ときと絶対 URI / `Host` が無いときは 400、要求行が長すぎたときは 414、
    /// ヘッダーが長すぎたときは 431 になる (`/errors` に嘘を書かないため)。
    ///
    /// **個票に要求行そのものは入れない** (個票の決まり: 入れてよいのは接続元 IP・
    /// 宛先 `host:port`・時刻・数字だけ。壊れた要求行には URL もヘッダーも載っている)。
    /// 宛先はまだ解けていないので空のままにする。集計 (`errors_by_cause`) にも
    /// 足さない: 4xx はエラー (5xx) ではないので、混ぜると `/status` が読めなくなる。
    ///
    /// **戻り値は WARN を 1 行出してよいか** (T28.9)。不正な要求の本数は攻撃者が決めるので、
    /// 理由ごとに 1 分に 1 回 (`log::WARN_EVERY_SECS`) だけ `Some` を返し、中身は前の行から
    /// 抑えた本数 (行の終わりにそのまま `{}` で付ける)。呼ぶ側は
    /// `if let Some(more) = metrics.record_bad_request(..) { log_warn!(.., "…{}", more) }` の形で出す
    #[must_use = "WARN を出すかどうかを返す (T28.9)"]
    pub fn record_bad_request(
        &self,
        reason: BadRequestReason,
        client: &str,
        status: u16,
    ) -> Option<crate::log::Skipped> {
        self.rejected_requests[reason as usize].fetch_add(1, Ordering::Relaxed);
        self.errors.push(crate::recent::ErrorEntry::new(
            // まだメソッドを解いていない (CONNECT か転送か分からない) 段階なので
            // `forward` で揃える
            crate::recent::EntryKind::Forward,
            "",
            client,
            status,
            crate::recent::EntryCause::BadRequest(reason),
            0,
            0,
        ));
        self.rejected_warn[reason as usize].pass(crate::log::WARN_EVERY_SECS)
    }

    /// オリジン側の失敗の WARN を 1 行出してよいか (T29.15)。**理由ごとに 1 分に 1 回**だけ `Some` を返し、
    /// 中身は前の行から抑えた本数 (行の終わりにそのまま `{}` で付ける)。
    ///
    /// 0.28.48 の tokyo の `/log` は 1,000 行 (19.95 時間) が 100% `502 Bad Gateway: connect <mail>:993/995
    /// failed: lookup` で、24 時間で 2,228 行あった (本数は相手 = 総当たりする利用者が決める)。理由は
    /// [`ErrCause`] の 8 つと、名前解決の内訳 (`nxdomain` / `dns_temp`) の 2 つ。呼ぶ側は
    /// `if let Some(more) = metrics.origin_warn(f) { log_warn!(.., "…{}", more) }` の形で出す
    #[must_use = "WARN を出すかどうかを返す (T29.15)"]
    pub fn origin_warn(&self, f: Failure) -> Option<crate::log::Skipped> {
        let i = match f.dns {
            Some(d) => ERR_CAUSES + d as usize,
            None => f.cause as usize,
        };
        self.origin_warn[i].pass(crate::log::WARN_EVERY_SECS)
    }

    /// `/status` の `rejected_requests` (理由別と合計)。
    pub fn rejected_requests_json(&self) -> String {
        let mut out = String::with_capacity(160);
        let mut total = 0u64;
        out.push('{');
        for (i, name) in BAD_REQUEST_REASON_NAMES.iter().enumerate() {
            let n = self.rejected_requests[i].load(Ordering::Relaxed);
            total += n;
            let _ = write!(out, "\"{}\":{},", name, n);
        }
        let _ = write!(out, "\"total\":{}}}", total);
        out
    }
}
