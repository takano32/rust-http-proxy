//! 試験が JSON から値を 1 つ取り出す道具 (T34.111 で各試験の手書きを寄せた)。
//!
//! どれも**最初に出る** `"key":` を見る (入れ子の中でも構わない)。取り出したい節が決まって
//! いるなら、その節の文字列だけを渡す。落ちたときの文面には鍵と、渡した JSON を入れる。

/// `"key":` の直後の位置。無ければ `None`。
fn after_key(json: &str, key: &str) -> Option<usize> {
    let pat = format!("\"{}\":", key);
    json.find(&pat).map(|at| at + pat.len())
}

/// `"key":` の直後の位置 (無ければ落ちる)。
fn after_key_or_panic(json: &str, key: &str) -> usize {
    after_key(json, key).unwrap_or_else(|| panic!("{} が無い: {}", key, json))
}

/// `"key":<数字>` の非負の整数。
pub fn uint(json: &str, key: &str) -> u64 {
    let at = after_key_or_panic(json, key);
    digits(&json[at..])
        .parse()
        .unwrap_or_else(|_| panic!("{} が数でない: {}", key, json))
}

/// [`uint`] と同じだが、値が `null` なら `None`。
pub fn uint_opt(json: &str, key: &str) -> Option<u64> {
    let at = after_key_or_panic(json, key);
    if json[at..].starts_with("null") {
        return None;
    }
    Some(
        digits(&json[at..])
            .parse()
            .unwrap_or_else(|_| panic!("{} が数でない: {}", key, json)),
    )
}

/// `"key":<数>` の数 (小数と負号も読む)。
pub fn number(json: &str, key: &str) -> f64 {
    let at = after_key_or_panic(json, key);
    decimal(&json[at..])
        .parse()
        .unwrap_or_else(|_| panic!("{} が数でない: {}", key, json))
}

/// [`number`] と同じだが、値が `null` なら `None`。
pub fn number_opt(json: &str, key: &str) -> Option<f64> {
    let at = after_key_or_panic(json, key);
    if json[at..].starts_with("null") {
        return None;
    }
    Some(
        decimal(&json[at..])
            .parse()
            .unwrap_or_else(|_| panic!("{} が数でも null でもない: {}", key, json)),
    )
}

/// `"key":` の後ろを `,` `}` `]` の手前まで取り、前後の空白を落とす
/// (数・`true` / `false`・`null` をそのまま見たいとき)。
pub fn raw_value(json: &str, key: &str) -> String {
    let at = after_key_or_panic(json, key);
    json[at..]
        .chars()
        .take_while(|c| !matches!(c, ',' | '}' | ']'))
        .collect::<String>()
        .trim()
        .to_string()
}

/// `"key":"<値>"` の値 (エスケープは解かない。閉じの `"` の手前まで)。
pub fn string(json: &str, key: &str) -> String {
    let pat = format!("\"{}\":\"", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("{} の文字列が無い: {}", key, json))
        + pat.len();
    let rest = &json[at..];
    let end = rest
        .find('"')
        .unwrap_or_else(|| panic!("{} の閉じ引用符が無い: {}", key, json));
    rest[..end].to_string()
}

fn digits(s: &str) -> String {
    s.chars().take_while(|c| c.is_ascii_digit()).collect()
}

fn decimal(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect()
}

// ------------------------------------------------------------------
// オブジェクト・配列・行 (T34.112)。括弧は数えるが、文字列の中の括弧と `,` は数えない。
// ------------------------------------------------------------------

/// 文字列の外にいるかを 1 文字ずつ追う (`"` と、文字列の中の `\` の逃がし)。
#[derive(Default)]
struct Scan {
    in_str: bool,
    escaped: bool,
}

impl Scan {
    /// `c` が文字列の外の文字なら `true` (引用符そのものは外に数えない)。
    fn outside(&mut self, c: char) -> bool {
        if self.in_str {
            match c {
                _ if self.escaped => self.escaped = false,
                '\\' => self.escaped = true,
                '"' => self.in_str = false,
                _ => {}
            }
            return false;
        }
        if c == '"' {
            self.in_str = true;
            return false;
        }
        true
    }
}

/// `s` の先頭の `{` / `[` に対応する閉じ括弧の位置。閉じていなければ `None`。
fn matching_close(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut scan = Scan::default();
    for (i, c) in s.char_indices() {
        if !scan.outside(c) {
            continue;
        }
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `found` (`"key":` の位置) の後ろの括弧の塊を、括弧ごと返す。
fn block(json: &str, key: &str, found: Option<usize>) -> String {
    let at = found.unwrap_or_else(|| panic!("{} が無い: {}", key, json)) + key.len() + 3;
    let rest = &json[at..];
    let end = matching_close(rest).unwrap_or_else(|| panic!("{} が閉じていない: {}", key, rest));
    rest[..=end].to_string()
}

/// 最初の `"key":{…}` を、括弧ごと (入れ子つきで) 切り出す。
pub fn object(json: &str, key: &str) -> String {
    block(json, key, json.find(&format!("\"{}\":{{", key)))
}

/// 同じ名前が入れ子にもあるとき (`memory` は `cache.memory` が先に出る) の**最後の** `"key":{…}`。
pub fn last_object(json: &str, key: &str) -> String {
    block(json, key, json.rfind(&format!("\"{}\":{{", key)))
}

/// 最初の `"key":[…]` の要素を 1 つずつ (書かれたとおりの文字列で) 返す。空の配列は空。
pub fn array_items(json: &str, key: &str) -> Vec<String> {
    let array = block(json, key, json.find(&format!("\"{}\":[", key)));
    split_top(&array[1..array.len() - 1])
}

/// `"key":[…]` の要素のうち、`needle` を含む最初の 1 つ (無ければ落ちる)。
pub fn find_item(json: &str, key: &str, needle: &str) -> String {
    array_items(json, key)
        .into_iter()
        .find(|item| item.contains(needle))
        .unwrap_or_else(|| panic!("{} の中に {} を含む要素が無い: {}", key, needle, json))
}

/// `"key":["a","b",…]` の綴りの並び (引用符を落とす。エスケープは解かない)。
pub fn string_array(json: &str, key: &str) -> Vec<String> {
    array_items(json, key)
        .into_iter()
        .map(|s| s.trim_matches('"').to_string())
        .collect()
}

/// 最初の `"samples":[[…],[…]]` の各行を、列の並びにして返す (**入れ子の配列は 1 列**)。
pub fn sample_rows(json: &str) -> Vec<Vec<String>> {
    array_items(json, "samples")
        .into_iter()
        .map(|row| {
            assert!(row.starts_with('['), "標本の行が配列でない: {}", row);
            split_top(&row[1..row.len() - 1])
        })
        .collect()
}

/// 括弧と文字列の外にある `,` で割る (前後の空白は落とす。中身が空なら空の並び)。
fn split_top(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut scan = Scan::default();
    for (i, c) in inner.char_indices() {
        if !scan.outside(c) {
            continue;
        }
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(inner[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = inner[start..].trim();
    if !last.is_empty() || !out.is_empty() {
        out.push(last.to_string());
    }
    out
}
