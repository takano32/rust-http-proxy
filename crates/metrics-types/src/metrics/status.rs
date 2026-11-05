//! `/status` を組むときに上の層から受け取る部品 ([`StatusExtras`] / [`Concurrency`]) と、
//! `hosts[]` の上位 50 を切り出す鍵 ([`HostSort`])。

/// `/status` の JSON に埋め込む、上の層が用意する部品。
///
/// 既定 (`Default`) は `"null"`。単体テストのように上の層が居ないときに使う。
pub struct StatusExtras<'a> {
    /// `.env` の再読込の状態 (`reload::status_json()`)
    pub settings: &'a str,
    /// ブロックリストの状態 (`blocklist::status_json()`)
    pub blocklist: &'a str,
    /// 状態ファイルの状態 (`persist::status_json()`)
    pub state_file: &'a str,
    /// `memory.baseline` に出す「この起動の heap の基準」(`persist::baseline_json()`。
    /// `null` なら欄は `null`。T24.4)
    pub baseline: &'a str,
    /// 動いているバイナリの版 (本体クレートの `VERSION`。`0.18.0+144b992` の形)
    pub version: &'a str,
    /// 上限といまのスレッドの数 ([`Concurrency`])
    pub concurrency: Concurrency,
    /// `hosts[]` の上位 50 をどの鍵で切り出すか (`/status?sort=`。T13.3)
    pub sort: HostSort,
    /// `hosts[]` を出すか (`/status?hosts=0` で偽。T25.1 段 2)。
    ///
    /// 既定は**真** = 今までどおり `hosts[]` を出す。偽のときは `hosts` の鍵ごと省く
    /// (他の鍵は 1 バイトも変えない)。`/status` は `hosts[]` が応答の 8 割を占めるので、
    /// 監視が 30 秒ごとに引く口としてはここを落とした軽い形を使う。
    pub hosts: bool,
}

impl Default for StatusExtras<'_> {
    fn default() -> Self {
        StatusExtras {
            settings: "null",
            blocklist: "null",
            state_file: "null",
            baseline: "null",
            version: "unknown",
            concurrency: Concurrency::default(),
            sort: HostSort::Requests,
            hosts: true,
        }
    }
}

/// 上限と、いまの接続スレッドの数 (`/status` 用)。
///
/// `auto` で決まった上限を**起動ログを見なくても確かめられる**ようにするためのもの
/// (`PROXY_MAX_CONNS` は T8.5、`PROXY_MAX_THREADS` と待ち行列は T10.5 のやり残し)。
/// 値を決めるのは上の層 (`Config` と `Workers`) で、ここは受け取って並べるだけ。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Concurrency {
    /// 同時接続数の上限 (`PROXY_MAX_CONNS` が `auto` なら決まった値。`0` で無制限)
    pub max_conns: usize,
    /// 生きていてよい接続スレッドの上限 (`PROXY_MAX_THREADS`。`0` で無制限)
    pub max_threads: usize,
    /// いま生きている接続スレッドの数
    pub live_threads: usize,
    /// そのうち空き置き場に積んである数 (仕事を待っているスレッド)
    pub idle_threads: usize,
    /// 上限に達して待たせている仕事の数 (捨てていない)
    pub queued_jobs: usize,
}

/// `/status` の `hosts[]` から上位 50 を切り出す鍵 (`?sort=`。T13.3)。
///
/// **切り出す鍵だけ**を変えるもので、JSON の形も件数も変わらない。要求数の上位 50 には
/// 「悪いホスト」が出てこないのが動機で、デプロイ後 58.6 時間の実測では
/// **エラー 99 件のうち 80 件 (名前解決の失敗) を抱えたホストが 1 つも上位 50 に居なかった**
/// (上位 50 のエラーは全部 0)。`clients[]` は要求数順のまま (接続元には内訳が無いので、
/// 名前解決や確立の鍵で並べても全部 0 の同点になるだけ)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostSort {
    /// 要求数の多い順 (既定。今までの順)
    #[default]
    Requests,
    /// エラー件数の多い順
    Errors,
    /// 名前解決に費やした合計 (`dns_ms_sum`) の大きい順
    Dns,
    /// 応答 (CONNECT は確立) の平均 (`avg_ms`) の遅い順
    Slow,
}

impl HostSort {
    /// `?sort=` の値から決める。**知らない値は既定 (`requests`) に倒す**
    /// (綴り違いで 400 を返すより、今までどおりの応答を返す方が監視の口として安全)。
    pub fn from_param(v: &str) -> Self {
        match v {
            "errors" => HostSort::Errors,
            "dns" => HostSort::Dns,
            "slow" => HostSort::Slow,
            _ => HostSort::Requests,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `?sort=` の 3 つの鍵と、知らない値 (大文字・空・綴り違い) は既定の要求数順。
    #[test]
    fn the_host_sort_reads_the_param_and_falls_back_to_requests() {
        assert_eq!(HostSort::from_param("errors"), HostSort::Errors);
        assert_eq!(HostSort::from_param("dns"), HostSort::Dns);
        assert_eq!(HostSort::from_param("slow"), HostSort::Slow);
        for v in ["", "requests", "REQUESTS", "errors?", "なにか"] {
            assert_eq!(HostSort::from_param(v), HostSort::Requests, "{v}");
        }
        assert_eq!(HostSort::default(), HostSort::Requests);
    }

    /// 上の層が居ないときの部品は `null` / `unknown` / 0 で、`hosts[]` は出す側に倒す。
    #[test]
    fn the_default_extras_stand_in_for_a_missing_upper_layer() {
        let e = StatusExtras::default();
        assert_eq!(
            (e.settings, e.blocklist, e.state_file, e.baseline, e.version),
            ("null", "null", "null", "null", "unknown")
        );
        assert_eq!(e.concurrency, Concurrency::default());
        assert_eq!(e.sort, HostSort::Requests);
        assert!(e.hosts);
    }
}
