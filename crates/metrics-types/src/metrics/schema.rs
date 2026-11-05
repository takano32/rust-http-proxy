//! 応答の形の版 (`schema`。T14.49) — 全エンドポイントの応答の先頭の鍵と、
//! 入れ子にも使う JSON を応答そのものとして返すときに版を足す口。

/// 応答の JSON の形の版 (T14.49)。**全エンドポイントの応答の先頭の鍵** `"schema"` に出る。
///
/// 読む道具 (`scripts/status-diff.py` `scripts/snapshot-diff.py` `scripts/check-dashboard.js`) は
/// 「この JSON はどの版か」を `parts` の有無などで**推測**していた。先頭に版を書いておけば
/// 推測が要らない。**形を変えた (鍵を消す・意味を変える・入れ子を変える) ときは +1** し、
/// README の「応答の形の版 (`schema`) の履歴」の表に 1 行足すこと
/// (**鍵を末尾に足すだけなら上げない** — 読む側は知らない鍵を無視できる)。
///
/// 版 1 = 2026-09-16 の Phase 14 の形。定義はこの 1 か所だけで、`crates/endpoints` は
/// ここを読む。
pub const SCHEMA: u32 = 1;

/// JSON を組み始める先頭 (`{` の代わりにこれを書く = `{"schema":1,`)。
///
/// 組み立ての熱くない経路でも、要求ごとに整形し直す理由が無いので定数にしてある。
/// [`SCHEMA`] と食い違ったら**ビルドが止まる** (下の `const _`)。
pub const SCHEMA_HEAD: &str = "{\"schema\":1,";

// `SCHEMA` と `SCHEMA_HEAD` が食い違わないように (片方だけ直したらここで止まる)。
// 版が 2 桁になったらこの検査ごと書き換えること
const _: () = assert!(
    SCHEMA < 10 && SCHEMA_HEAD.as_bytes()[10] == b'0' + SCHEMA as u8,
    "SCHEMA と SCHEMA_HEAD が食い違っている"
);

/// **入れ子にも使う JSON を、応答そのものとして返すとき**に先頭へ版を足す (T14.49)。
///
/// 使うのは `/blocklist` (引数なしなら `/status` の `blocklist` と同じ状態をそのまま返す)
/// のように、1 つの関数の出力が入れ子と応答の両方になる口だけ。**入れ子の側は版を持たない**
/// (版を持つのは応答の 1 番外側と、`/snapshot` の各部 = それぞれの口の出力そのもの)。
pub fn with_schema(body: &str) -> String {
    match body.strip_prefix('{') {
        // `{}` (空) は `{"schema":N}` に (末尾の `,` を残さない)
        Some("}") => format!("{{\"schema\":{}}}", SCHEMA),
        Some(rest) => format!("{}{}", SCHEMA_HEAD, rest),
        // `{` で始まらないもの (`null` など) は触らない
        None => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版を足す 3 つの形 (T14.49): 空の `{}`・ふつうの入れ子・`{` で始まらないもの。
    #[test]
    fn with_schema_prefixes_the_version_and_leaves_non_objects_alone() {
        assert_eq!(with_schema("{}"), "{\"schema\":1}");
        assert_eq!(with_schema("{\"a\":1}"), "{\"schema\":1,\"a\":1}");
        assert_eq!(with_schema("null"), "null");
        assert_eq!(with_schema(""), "");
        assert_eq!(SCHEMA_HEAD, format!("{{\"schema\":{},", SCHEMA));
    }
}
