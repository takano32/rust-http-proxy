//! 設定 1 つ 1 つの**効いている値がどこから来たか** (`/config` の `source`。T14.15) と、
//! `/config` / `--check` が並べる 1 行 ([`Setting`])。

use crate::envfile;

/// 設定 1 つ 1 つの**効いている値がどこから来たか** (`/config` の `source`。T14.15)。
///
/// 「書いてある場所」ではなく「**効いた値の出どころ**」を指す: 読めない書き方 (`abc` を
/// 秒数に書いたなど) は既定に落ちるので、そのキーは `Default` のままになる
/// (書いたのに効いていないことが `/config` で分かる)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Source {
    /// このコードの既定値 (どこにも書かれていない、または書いたが読めなかった)
    #[default]
    Default,
    /// 実際の環境変数 (Pterodactyl のパネルが渡すもの)
    Env,
    /// `$HOME/.env`
    EnvFile,
    /// コマンドライン引数
    Cli,
}

impl Source {
    /// いま `key` がどの層から読めるか ([`envfile::var`] と同じ優先順)。
    pub fn of(key: &str) -> Source {
        match envfile::var_source(key) {
            Some(envfile::VarSource::Cli) => Source::Cli,
            Some(envfile::VarSource::File) => Source::EnvFile,
            Some(envfile::VarSource::Env) => Source::Env,
            None => Source::Default,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Env => "env",
            Source::EnvFile => "env_file",
            Source::Cli => "cli",
        }
    }
}

/// キーごとの出どころ。**効いた値にだけ印が付く** (印の無いキーは [`Source::Default`])。
///
/// 数十件しか無く、引くのは `/config` と `--check` と再読込のときだけなので、
/// 連想配列ではなく小さな配列で足りる (設定は接続ごとに複製されるので軽い方がよい)。
#[derive(Debug, Clone, Default)]
pub struct Sources(Vec<(&'static str, Source)>);

impl Sources {
    /// `key` の値が効いたので、いまどの層から来ているかを覚える。
    pub fn mark(&mut self, key: &'static str) {
        let src = Source::of(key);
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = src,
            None => self.0.push((key, src)),
        }
    }

    pub fn get(&self, key: &str) -> Source {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map_or(Source::Default, |(_, s)| *s)
    }

    /// 再読込で作り直した設定から 1 つだけ写す (**当てた項目だけ**呼ぶ。T14.15)。
    /// 起動時に固定される項目 (ポートや TLS) は `.env` が変わっても効かないので、
    /// 写さずに起動時の出どころを残す。
    pub fn adopt(&mut self, fresh: &Sources, key: &'static str) {
        let src = fresh.get(key);
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = src,
            None if src != Source::Default => self.0.push((key, src)),
            None => {}
        }
    }
}

/// `/config` と `--check` が並べる 1 行。
#[derive(Debug, Clone)]
pub struct Setting {
    /// 環境変数名 (`PROXY_*` / `SERVER_*`)
    pub key: &'static str,
    /// **いま効いている値**を JSON の値として書いたもの (数・真偽・文字列・配列・`null`)
    pub value: String,
    /// その値の出どころ
    pub source: Source,
}

/// [`Setting::value`] (JSON の値) を人が読む印字の形にする: 文字列の引用符は、中身が空でなく `\` を
/// 含まないときだけ外す (空は `""` のまま残して「空」と分かるようにする)。
///
/// `--check` の一覧と `/events` の `reload` の前後の値が同じこれを通る (T34.153。前は `/events` の側だけ
/// 空の中身でも外していた)。
pub fn printable_value(value: &str) -> String {
    match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) if !inner.is_empty() && !inner.contains('\\') => inner.to_string(),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_only_mark_values_that_took_effect() {
        let mut src = Sources::default();
        // 印が無ければ既定
        assert_eq!(src.get("PROXY_DNS_TTL_SECS"), Source::Default);
        // どこにも書かれていないキーは、印を付けても既定のまま
        // (「効いた」と呼べる値がそもそも無い)
        src.mark("PROXY_DNS_TTL_SECS");
        assert_eq!(src.get("PROXY_DNS_TTL_SECS"), Source::Default);
        // 実環境にあるキーは `env` として読める (配線の確認。`PATH` はどこでもある)
        assert_eq!(Source::of("PATH"), Source::Env);
        assert_eq!(Source::of("PROXY_NO_SUCH_KEY_T1415"), Source::Default);
        // 再読込では**当てた項目だけ**写す
        let mut fresh = Sources::default();
        fresh.0.push(("PROXY_DNS_TTL_SECS", Source::EnvFile));
        fresh.0.push(("SERVER_PORT", Source::EnvFile));
        let mut next = Sources::default();
        next.adopt(&fresh, "PROXY_DNS_TTL_SECS");
        assert_eq!(next.get("PROXY_DNS_TTL_SECS"), Source::EnvFile);
        assert_eq!(next.get("SERVER_PORT"), Source::Default, "写していない項目");
        // 消えたら既定に戻る
        next.adopt(&Sources::default(), "PROXY_DNS_TTL_SECS");
        assert_eq!(next.get("PROXY_DNS_TTL_SECS"), Source::Default);
        assert_eq!(
            [
                Source::Default.as_str(),
                Source::Env.as_str(),
                Source::EnvFile.as_str(),
                Source::Cli.as_str()
            ],
            ["default", "env", "env_file", "cli"]
        );
    }
}
