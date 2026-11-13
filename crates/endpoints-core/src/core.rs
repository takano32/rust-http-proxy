//! エンドポイントの共通の土台: 要求 1 本ぶんの文脈 [`Endpoint`] と、問い合わせ
//! (`?a=1&b=2`) の読み方。
//!
//! T15.12 段 1 (ii) で `proxy-endpoints` から下ろした。**中身は 1 行も変えていない**
//! (`endpoints/mod.rs` が今までと同じ名前で出し直すので、呼ぶ側の綴りは変わらない)。
//! ここを下の層に置いたのは、`/explain` のような口を別クレートへ出すときに
//! [`Endpoint`] を共有する必要があるため。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 120 MB)。**外部クレートは 1 つも使っていない。**

use crate::cache::Cache;
use crate::metrics::{self, Metrics};

pub struct Endpoint<'a> {
    pub metrics: &'a Metrics,
    pub cache: &'a Cache,
    pub conn_id: usize,
    /// 自分の待ち受けポート (絶対形式の自分宛て判定に使う)
    pub port: u16,
    /// 要求の `Host` ヘッダー (`/proxy.pac` が自分の名前を知るため)
    pub host: Option<&'a str>,
    /// 接続元 IP (`readers` に 1 行残すため。T14.53)。**要求で来たときだけ**入り、
    /// 日次の `/snapshot` を履歴スレッドが組むときは `None` (誰も引いていないので
    /// 数えない)
    pub client: Option<&'a str>,
    /// `/proxy.pac` で DIRECT にするホストのパターン
    pub pac_direct: &'a [String],
    /// lite プロファイル (ダッシュボードを持たない)
    pub lite: bool,
    /// 書き換える口 (`/purge` / `PURGE` / `/blocklist?action=`) を 405 で断る
    /// (`PROXY_ENDPOINTS_READONLY`。読む口は今までどおり。認証ではない。T14.18)
    pub readonly: bool,
    /// 動いているバイナリの版 (`/status` に出す。本体クレートの `VERSION`)
    pub version: &'a str,
    /// ビルドの種類 = cargo のプロファイル名 (`release` / `dist` / `debug`。`/status` の
    /// 末尾の `build` に出す。本体クレートの `BUILD`。T28.42)
    pub build: &'a str,
    /// 上限といまのスレッド数を引く口 (`/status` と `/metrics` を組み立てるときだけ呼ぶ)。
    ///
    /// 値そのものではなく関数で受け取るのは、生きているスレッド数と待ち行列を数えるのに
    /// **全接続スレッドで共有している鍵**を取るため。`Endpoint` は要求ごとに組むので、
    /// ここで数えると熱い経路に乗ってしまう (この 2 つのパスに来たときだけ引く)
    pub concurrency: &'a dyn Fn() -> metrics::Concurrency,
}

/// 問い合わせに `key=` が**立っている**か (`/history?summary=1` と同じ読み方。`0` は偽)。
pub fn has_flag(query: Option<&str>, key: &str) -> bool {
    let Some(q) = query else {
        return false;
    };
    parse_query(q).iter().any(|(k, v)| k == key && v != "0")
}

/// `?offset=N` を読む (無い / 読めない値は 0。上は `max` で止める。T15.0)。
///
/// 読むのは `/recent` `/hosts` `/profile` `/dns` の 4 つで (`/dns` は T27.1)、**読み方はここ 1 か所**に置く。
/// [`num_param`] と別なのは**下限が 0** だから (あちらは `.clamp(1, max)` なので
/// 「1 件目から」を表せない)。意味は「いまの並びを何本飛ばすか」で、並びは `/recent` が
/// 閉じた新しい順、`/hosts` が `sort=` の順、`/profile` が新しい標本の順。
pub fn offset_param(query: Option<&str>, max: usize) -> usize {
    param(&parse_query(query.unwrap_or("")), "offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(max)
}

/// 解いた問い合わせ ([`parse_query`]) から `key` の値を引く。同じ鍵が 2 つあれば**最初の 1 つ**。
///
/// 問い合わせを読む口はどれもこれを通す (T34.93。前は `iter().find(…)` が 15 か所に写されていた)。
pub fn param<'p>(params: &'p [(String, String)], key: &str) -> Option<&'p str> {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// `?key=` の文字列 (無ければ `""`)。
pub fn str_param(query: Option<&str>, key: &str) -> String {
    param(&parse_query(query.unwrap_or("")), key)
        .unwrap_or_default()
        .to_string()
}

/// `?key=N` を読む (無い / 読めない / 範囲外は既定か端に倒す。`/status?sort=` と同じ方針)。
pub fn num_param(query: Option<&str>, key: &str, default: usize, max: usize) -> usize {
    param(&parse_query(query.unwrap_or("")), key)
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(1, max)
}

/// 要素を `budget` バイトまで `[...]` に並べる。入り切らなかったらそこで止める。
/// 返すのは (書けた件数, 打ち切ったか)。
///
/// `budget` は `out` 全体 (前に書いてあるぶんを含む) の上限で、呼ぶ側が末尾のぶんを
/// 引いて渡す (`/recent` などは 256 KiB − 512 B、`/explain` は 64 KiB − 2 KiB)。
/// 1 要素ごとに「`,` と閉じの `]` の 2 バイト」を見込んで判定する。前は個票の口と
/// `/explain` が同じループを別々に持っていた (T34.94)。
pub fn array_within(
    out: &mut String,
    items: impl IntoIterator<Item = String>,
    budget: usize,
) -> (usize, bool) {
    out.push('[');
    let (mut n, mut cut) = (0usize, false);
    for item in items {
        if out.len() + item.len() + 2 > budget {
            cut = true;
            break;
        }
        if n > 0 {
            out.push(',');
        }
        out.push_str(&item);
        n += 1;
    }
    out.push(']');
    (n, cut)
}

/// 次の頁の `offset` (続きが無ければ `null`)。上の 3 つが同じ綴りで応答の末尾に出す。
pub fn next_offset(offset: usize, shown: usize, total: usize) -> String {
    match offset + shown < total {
        true => (offset + shown).to_string(),
        false => "null".to_string(),
    }
}

pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

/// `%XX` を戻す (`+` はそのまま: URL の中の `+` を壊さない)。
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (
                hex(bytes[i + 1]),
                hex(bytes.get(i + 2).copied().unwrap_or(0)),
            )
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同じ鍵が 2 つあれば最初の 1 つ、値の無い鍵は `""`、無い鍵は `None` (T34.93)。
    /// 予算に収まるところまで並べ、収まらなければ `]` で閉じて打ち切りを返す (T34.94)。
    #[test]
    fn an_array_stops_at_the_budget() {
        let mut out = String::from("{\"a\":");
        // 2 本目を書く前は `{"a":[1` の 7 バイト。7 + "22" (2) + 2 = 11 ≤ 11 で入る
        let (n, cut) = array_within(&mut out, ["1".to_string(), "22".to_string()], 11);
        assert_eq!((n, cut), (2, false));
        assert_eq!(out, "{\"a\":[1,22]");
        // 予算を 1 バイト減らすと 2 本目は入らない
        let mut out = String::from("{\"a\":");
        let (n, cut) = array_within(&mut out, ["1".to_string(), "22".to_string()], 10);
        assert_eq!((n, cut), (1, true));
        assert_eq!(out, "{\"a\":[1]");
        // 空なら `[]`
        let mut out = String::new();
        assert_eq!(array_within(&mut out, Vec::<String>::new(), 0), (0, false));
        assert_eq!(out, "[]");
    }

    #[test]
    fn a_parameter_is_the_first_value_of_its_key() {
        let params = parse_query("sort=slow&n&sort=bytes&url=http%3A%2F%2Fa%2F");
        assert_eq!(param(&params, "sort"), Some("slow"));
        assert_eq!(param(&params, "n"), Some(""));
        assert_eq!(param(&params, "url"), Some("http://a/"));
        assert_eq!(param(&params, "missing"), None);
        assert_eq!(str_param(Some("sort=slow&sort=bytes"), "sort"), "slow");
        assert_eq!(num_param(Some("n=5&n=9"), "n", 100, 500), 5);
        assert_eq!(offset_param(Some("offset=9999"), 2000), 2000);
        assert_eq!(offset_param(Some("offset=x"), 2000), 0);
    }
}
