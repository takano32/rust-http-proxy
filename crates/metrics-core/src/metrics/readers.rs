//! 内部エンドポイントを引いた接続元の表 (`/status` の `readers` と `/readers`。
//! T34.26 で `metrics.rs` から移した)。
//!
//! 行を動かしただけで中身は変えていない。

use super::*;

impl Metrics {
    /// 内部エンドポイント (`/status` `/clients` …) を 1 回引かれたことを数える (T14.53)。
    ///
    /// **呼ぶのは `endpoints::handle` の入口で 1 回だけ** (要求が自分宛てだと決まった
    /// あと)。プロキシとして通す要求 (CONNECT / forward) はこの関数を 1 度も通らないので、
    /// 熱い経路の費用は 0 で、T14.7 の `clients[]` (自分宛てを数えない表) とも混ざらない。
    ///
    /// `path` は**問い合わせ文字列を外したパス**を渡すこと (`?` 以降は記録しない)。
    /// 表が [`MAX_READERS`] で溢れたら、最後に引いたのがいちばん古い行を 1 つ捨てる。
    pub fn record_reader(&self, client: &str, path: &str) {
        // 記録の一括 off とハッシュ化 (T14.41)。読み手の表も接続元 1 人ずつの記録なので、
        // `off` なら鍵も取らずに戻り、`hashed` なら他の個票と同じ 1 関数を通す
        if !crate::records::recording() {
            return;
        }
        let client = &crate::records::client_key(client);
        let now = crate::cache::now_epoch();
        let mut readers = self.readers.locked();
        if let Some(r) = readers.get_mut(client.as_ref()) {
            r.count += 1;
            r.last_at = now;
            // 同じパスが続く間は確保しない (監視は同じ口を叩き続ける)。
            // [`MAX_READER_PATH`] より長いパスだけは毎回切り直すことになるが、
            // そういう相手は走査なので惜しまない (どのみち自分宛ての経路だけ)
            if r.last_path != path {
                r.last_path = crate::recent::clip(path, MAX_READER_PATH);
            }
            return;
        }
        if readers.len() >= MAX_READERS
            // 捨てるのは**最後に引いたのがいちばん古い行**。同着は引いた数の少ない方 →
            // 名前の順で崩すので、どの環境でも捨てる 1 行は同じに決まる
            && let Some(old) = readers
                .iter()
                .min_by(|a, b| {
                    a.1.last_at
                        .cmp(&b.1.last_at)
                        .then_with(|| a.1.count.cmp(&b.1.count))
                        .then_with(|| a.0.cmp(b.0))
                })
                .map(|(k, _)| k.clone())
        {
            readers.remove(&old);
        }
        readers.insert(
            client.to_string(),
            Reader {
                count: 1,
                last_at: now,
                last_path: crate::recent::clip(path, MAX_READER_PATH),
            },
        );
    }

    /// 内部エンドポイントを引いた接続元を、引いた回数の多い順に (T14.53)。
    ///
    /// **同点は最後に引いた時刻の新しい順 → 名前**で崩すので、順序は 1 つに決まる
    /// ([`Metrics::clients_sorted_by`] と同じ作法)。
    pub fn readers_sorted(&self) -> Vec<(String, Reader)> {
        let readers = self.readers.locked();
        let mut v: Vec<(String, Reader)> = readers
            .iter()
            .map(|(k, r)| (k.clone(), r.clone()))
            .collect();
        v.sort_by(|a, b| {
            b.1.count
                .cmp(&a.1.count)
                .then_with(|| b.1.last_at.cmp(&a.1.last_at))
                .then_with(|| a.0.cmp(&b.0))
        });
        v
    }

    /// `/status` の `readers` (上位 [`STATUS_READERS`] 件の配列だけ)。全部は `/readers`。
    pub fn readers_json(&self) -> String {
        let rows: Vec<String> = self
            .readers_sorted()
            .into_iter()
            .take(STATUS_READERS)
            .map(|(c, r)| r.to_json(&c))
            .collect();
        format!("[{}]", rows.join(","))
    }

    /// `/readers` の応答 (**全部**。`budget` バイトに収まるところまで。T14.53)。
    ///
    /// 上限のバイト数は呼ぶ側 (`crates/endpoints` の `recent::MAX_BODY` = 256 KiB) が
    /// 持っている数で、個票の口と同じ扱いにするために引数で受け取る (この層に写しを
    /// 置くと 2 か所で食い違う)。満杯 (256 行) でも 30 KB ほどなので普通は切れない。
    pub fn readers_body(&self, budget: usize) -> String {
        let all = self.readers_sorted();
        let count = all.len();
        // 末尾 (`],"count":...}`) のために空けておくぶん
        let room = budget.saturating_sub(256);
        let mut out = String::with_capacity(4096);
        out.push_str(SCHEMA_HEAD);
        out.push_str("\"readers\":[");
        let (mut shown, mut cut) = (0usize, false);
        for (c, r) in &all {
            let item = r.to_json(c);
            if out.len() + item.len() + 2 > room {
                cut = true;
                break;
            }
            if shown > 0 {
                out.push(',');
            }
            out.push_str(&item);
            shown += 1;
        }
        let _ = write!(
            out,
            "],\"count\":{},\"shown\":{},\"truncated\":{},\"max_readers\":{},\"max_path\":{},\"persisted\":false,\"uptime_secs\":{}}}",
            count,
            shown,
            cut,
            MAX_READERS,
            MAX_READER_PATH,
            self.start_time.elapsed().as_secs()
        );
        out
    }
}
