//! 小さな JSON 読み。**外部クレートは使わない** (TODO.md §0) ので、個票を読むのに要る
//! だけの読み取りを自前で持つ。

/// 読み取った JSON の値。**object は `Vec` で持つ** (件数が少なく、順番も保てる)。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub(crate) fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// 負の数と NaN は 0 として読む (個票の欄はどれも 0 以上)。
    pub(crate) fn as_u64(&self) -> Option<u64> {
        self.as_f64().map(|n| {
            if n.is_finite() && n > 0.0 {
                n as u64
            } else {
                0
            }
        })
    }

    pub(crate) fn as_arr(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
}

/// 入れ子の深さの上限 (壊れた入力でスタックを使い切らないため)。
const MAX_DEPTH: u32 = 64;

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while let Some(c) = self.b.get(self.i) {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, lit: &[u8]) -> bool {
        if self.b[self.i..].starts_with(lit) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: u32) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err(format!("JSON の入れ子が深すぎます ({} 段)", MAX_DEPTH));
        }
        self.ws();
        match self.b.get(self.i) {
            None => Err("JSON が途中で終わっています".to_string()),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') if self.eat(b"true") => Ok(Json::Bool(true)),
            Some(b'f') if self.eat(b"false") => Ok(Json::Bool(false)),
            Some(b'n') if self.eat(b"null") => Ok(Json::Null),
            Some(_) => self.number(),
        }
    }

    fn object(&mut self, depth: u32) -> Result<Json, String> {
        self.i += 1; // '{'
        let mut out = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Json::Obj(out));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return Err(format!("{} 文字目: ':' が要ります", self.i));
            }
            self.i += 1;
            let value = self.value(depth + 1)?;
            out.push((key, value));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(out));
                }
                _ => return Err(format!("{} 文字目: ',' か '}}' が要ります", self.i)),
            }
        }
    }

    fn array(&mut self, depth: u32) -> Result<Json, String> {
        self.i += 1; // '['
        let mut out = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Json::Arr(out));
        }
        loop {
            out.push(self.value(depth + 1)?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(out));
                }
                _ => return Err(format!("{} 文字目: ',' か ']' が要ります", self.i)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if self.b.get(self.i) != Some(&b'"') {
            return Err(format!("{} 文字目: 文字列が要ります", self.i));
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.b.get(self.i) else {
                return Err("文字列が閉じていません".to_string());
            };
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&e) = self.b.get(self.i) else {
                        return Err("文字列が閉じていません".to_string());
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode()?),
                        _ => return Err(format!("{} 文字目: 知らない \\ の続き", self.i)),
                    }
                }
                _ => {
                    // UTF-8 の続きのバイトはそのまま積む (個票は UTF-8)
                    let start = self.i - 1;
                    let len = utf8_len(c);
                    let end = (start + len).min(self.b.len());
                    match std::str::from_utf8(&self.b[start..end]) {
                        Ok(s) => out.push_str(s),
                        Err(_) => out.push('\u{fffd}'),
                    }
                    self.i = end;
                }
            }
        }
    }

    /// `\uXXXX` を 1 文字にする (上位下位の組も 1 文字にまとめる)。
    fn unicode(&mut self) -> Result<char, String> {
        let hi = self.hex4()?;
        if (0xd800..0xdc00).contains(&hi) && self.b[self.i..].starts_with(b"\\u") {
            let save = self.i;
            self.i += 2;
            let lo = self.hex4()?;
            if (0xdc00..0xe000).contains(&lo) {
                let c = 0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00);
                return Ok(char::from_u32(c).unwrap_or('\u{fffd}'));
            }
            self.i = save;
        }
        Ok(char::from_u32(hi).unwrap_or('\u{fffd}'))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let end = self.i + 4;
        if end > self.b.len() {
            return Err("\\u の 16 進 4 桁が足りません".to_string());
        }
        let s = std::str::from_utf8(&self.b[self.i..end]).map_err(|_| "\\u が壊れています")?;
        let v = u32::from_str_radix(s, 16).map_err(|_| "\\u が 16 進ではありません")?;
        self.i = end;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        while let Some(&c) = self.b.get(self.i) {
            if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.i += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).unwrap_or("");
        s.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("{} 文字目: 数として読めません ({:?})", start, s))
    }
}

/// UTF-8 の 1 文字のバイト数 (先頭バイトから)。
fn utf8_len(c: u8) -> usize {
    match c {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// JSON を 1 つ読む (後ろに何か続いていても気にしない)。
pub(crate) fn parse(text: &str) -> Result<Json, String> {
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
    };
    p.value(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JSON の読みそのもの (エスケープ・入れ子・数)。
    #[test]
    fn parses_the_json_we_need() {
        let v = parse(r#"{"a":[1,-2.5,1e3],"b":"x\"y\\z\nあ","c":{"d":null},"e":true}"#)
            .expect("読めること");
        let a = v.get("a").and_then(Json::as_arr).expect("配列");
        assert_eq!(a[0].as_u64(), Some(1));
        assert_eq!(a[1].as_f64(), Some(-2.5));
        assert_eq!(a[1].as_u64(), Some(0)); // 負の数は 0 として読む
        assert_eq!(a[2].as_u64(), Some(1000));
        assert_eq!(v.get("b").and_then(Json::as_str), Some("x\"y\\z\nあ"));
        assert_eq!(v.get("c").and_then(|c| c.get("d")), Some(&Json::Null));
        assert_eq!(v.get("e"), Some(&Json::Bool(true)));
        assert!(parse("{\"a\":").is_err());
    }

    /// `\u` の読み (上位下位の組は 1 文字、組にならない上位は U+FFFD)、空の object と array、
    /// 深すぎる入れ子、`get` は object 以外では `None`。
    #[test]
    fn unicode_escapes_empties_and_depth_are_handled() {
        let v = parse(r#"{"s":"😀 é","o":{},"a":[],"n":false}"#).expect("読めること");
        assert_eq!(v.get("s").and_then(Json::as_str), Some("😀 é"));
        assert_eq!(v.get("o"), Some(&Json::Obj(Vec::new())));
        assert_eq!(v.get("a"), Some(&Json::Arr(Vec::new())));
        assert_eq!(v.get("n"), Some(&Json::Bool(false)));
        // 組にならない上位代用は置換文字 1 つ、下位だけでも落ちない
        let lone = parse(r#""\ud83d x""#).expect("読めること");
        assert_eq!(lone.as_str(), Some("\u{fffd} x"));
        // object でないものに `get` しても `None`
        assert_eq!(Json::Num(1.0).get("a"), None);
        assert_eq!(Json::Null.as_arr(), None);
        // 深すぎる入れ子は読まない (スタックを守る)
        let deep = format!("{}{}", "[".repeat(100), "]".repeat(100));
        assert!(parse(&deep).unwrap_err().contains("深すぎ"));
        // 壊れた形は位置を添えて断る
        assert!(parse("{\"a\" 1}").unwrap_err().contains("':'"));
        assert!(parse("[1 2]").unwrap_err().contains("','"));
        assert!(parse("tru").is_err());
        assert!(parse("").is_err());
    }
}
