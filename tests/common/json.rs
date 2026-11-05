//! 試験が JSON から値を 1 つ取り出す道具 (T.34.111 で各試験の手書きを寄せた)。
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
