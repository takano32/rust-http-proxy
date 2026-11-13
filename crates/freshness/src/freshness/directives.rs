//! `Cache-Control` の指示の読み方と、`Vary` / `Accept-Encoding` の扱い。どれも入力の文字列だけを
//! 見る純粋な関数で、要求ごとの熱い経路 (応答の先頭の判定・キャッシュキーの正規形) から呼ばれる。

/// `Vary` が対応できる範囲か (`Accept-Encoding` のみ)。
pub fn vary_is_supported(v: &str) -> bool {
    v.split(',')
        .map(str::trim)
        .all(|t| t.is_empty() || t.eq_ignore_ascii_case("accept-encoding"))
}

/// キャッシュキーに含める `Accept-Encoding` の正規形 (小文字・q=0 除外・ソート・重複除去)。
pub fn accept_encoding_variant(value: Option<&str>) -> String {
    // 無い・空と、小文字のトークン 1 つ (`gzip` など) は並べ替えるものが無いので、そのまま返す
    // (`Vec` とトークンごとの `String` と `join` を作らない。T28.13)
    let v = value.unwrap_or("").trim();
    if v.is_empty() {
        return String::new();
    }
    if v.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return v.to_string();
    }
    let mut tokens: Vec<String> = value
        .unwrap_or("")
        .split(',')
        .filter_map(|t| {
            let mut parts = t.split(';');
            let name = parts.next()?.trim().to_ascii_lowercase();
            if name.is_empty() {
                return None;
            }
            let zero_q = parts.any(|p| {
                let p = p.trim();
                p.strip_prefix("q=")
                    .is_some_and(|q| q.trim().parse::<f64>().is_ok_and(|q| q <= 0.0))
            });
            (!zero_q).then_some(name)
        })
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens.join(",")
}

/// `cache_control` に指示 `name` があるか (値付き指示も含む)。`cache_control` は小文字化されていること。
pub fn has_directive(cache_control: &str, name: &str) -> bool {
    comma_parts(cache_control).any(|part| {
        let part = part.trim();
        part == name
            || part
                .strip_prefix(name)
                .is_some_and(|r| r.trim_start().starts_with('='))
    })
}

/// 指示 `name` の値 (`max-age=60` の 60)。無い・読めない (`max-age=abc`・負の数・値なし) なら `None`。
/// 同じ指示が 2 つあれば先のものを読む。
pub fn directive_value(cache_control: &str, name: &str) -> Option<u64> {
    for part in comma_parts(cache_control) {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name)
            && let Some(v) = rest.trim_start().strip_prefix('=')
        {
            return v.trim().trim_matches('"').parse::<u64>().ok();
        }
    }
    None
}

/// `s.split(',')` と同じ切り方 (空の部分も、末尾の空も出す)。
///
/// `str::split(char)` の `SplitInternal::next` は、このクレートの CGU では呼び出しに落ちる
/// (`freshness.rs` を割った T34.4 の後、HIT で要求ごとに 8 回・+180 Ir 前後。T36.7)。
/// `,` は 1 バイトなので、バイトで探して切っても文字の境目は崩れない。
#[inline]
fn comma_parts(s: &str) -> CommaParts<'_> {
    CommaParts { rest: Some(s) }
}

struct CommaParts<'a> {
    /// まだ切っていない残り (`None` = 出し終えた)
    rest: Option<&'a str>,
}

impl<'a> Iterator for CommaParts<'a> {
    type Item = &'a str;

    #[inline]
    fn next(&mut self) -> Option<&'a str> {
        let rest = self.rest?;
        match rest.bytes().position(|b| b == b',') {
            Some(i) => {
                self.rest = Some(&rest[i + 1..]);
                Some(&rest[..i])
            }
            None => {
                self.rest = None;
                Some(rest)
            }
        }
    }
}

#[cfg(test)]
mod comma_parts_tests {
    use super::comma_parts;

    /// [`comma_parts`] は `split(',')` と同じ部分を同じ順に出す (空・末尾の空・多バイト文字を含めて。T36.7)。
    #[test]
    fn comma_parts_matches_split() {
        for s in [
            "",
            ",",
            ",,",
            "a",
            "a,",
            ",a",
            "max-age=60, no-cache",
            " public ,  max-age=0 ,",
            "é,ü,,日本",
            "no-store,private,must-revalidate,s-maxage=5",
        ] {
            let got: Vec<&str> = comma_parts(s).collect();
            let want: Vec<&str> = s.split(',').collect();
            assert_eq!(got, want, "{:?}", s);
        }
    }
}
