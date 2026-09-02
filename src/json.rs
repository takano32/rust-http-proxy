//! 手書き JSON のための小さな補助 (依存クレートなしなので文字列を組み立てている)。

/// 文字列リテラルの中身としてのエスケープ (`\` と `"`、制御文字)。
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 引用符付きの文字列リテラル。
pub fn quote(s: &str) -> String {
    format!("\"{}\"", escape(s))
}

/// `Some` なら文字列リテラル、`None` なら `null`。
pub fn quote_opt(s: Option<&str>) -> String {
    s.map(quote).unwrap_or_else(|| "null".to_string())
}

/// 文字列の配列 (`["a","b"]`)。
pub fn list(items: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let inner: Vec<String> = items.into_iter().map(|s| quote(s.as_ref())).collect();
    format!("[{}]", inner.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_quotes_backslashes_and_controls() {
        assert_eq!(escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(escape("x\ny\u{1}"), "x\\ny\\u0001");
        assert_eq!(quote("日本"), "\"日本\"");
        assert_eq!(quote_opt(None), "null");
        assert_eq!(list(["a", "b\""]), "[\"a\",\"b\\\"\"]");
        assert_eq!(list(Vec::<String>::new()), "[]");
    }
}
