//! デプロイ先のパターンを手元で再生する (`--only replay --replay-file <path>`。T14.29)。
//!
//! バーストのとき T13.2 (追い出し) と T14.6 (写真) が本当に効くかは、**バーストが来るまで
//! 分からない**。`/recent` (T14.4) には「いつ・誰が・どこへ・どれだけ」があるので、
//! **同じ時間間隔・同じ本数**で手元の内蔵オリジンへ張り直せば、バーストの形だけ再現できる。
//!
//! 読める形は 3 つ (どれも `{"…":[…]}` の中身を探すだけ):
//!
//!   1. `/recent` の応答そのもの (`{"recent":[…]}`)
//!   2. `/snapshot` (`{"recent":{"recent":[…]},…}`。`hosts` があれば RTT もそこから読む)
//!   3. `/connections` (`{"connections":[…]}`)。**まだ閉じていない接続**なので寿命が無く、
//!      `age_secs` を「開いてからの秒」と「これから生きる秒」の両方に使う。`bytes` は
//!      上り下りの合計しか無いので全部下りに寄せる。**`/recent` が無いデプロイ先 (Phase 13)
//!      でも実データを再生できる**ようにするための読み口
//!
//! 宛先は `host:port` の**ホスト名だけ**を CONNECT の target に載せ、ポートは内蔵オリジンの
//! 1 つのポートに置き換える。名前解決は `scripts/deployed-like.sh --hosts-from <path>` が
//! 名前空間の中の `/etc/hosts` を差し替えて全部 `127.0.0.1` に向ける (T14.16 の仕組み)。
//!
//! RTT は `/hosts` の `rtt_ms.avg` (T14.5) があれば**内蔵オリジンの応答遅延**で真似る
//! (無ければ 0)。デプロイ先が Phase 13 のうちは `rtt_ms` 自体が無いので 0 になる。
//!
//! **外部クレートは使わない** (TASKS.md §0) ので、JSON も下の小さな読み取りで済ませる。

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::{Report, open_tunnel};

// ------------------------------------------------------------ 小さな JSON 読み

/// 読み取った JSON の値。**object は `Vec` で持つ** (件数が少なく、順番も保てる)。
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// 負の数と NaN は 0 として読む (個票の欄はどれも 0 以上)。
    pub fn as_u64(&self) -> Option<u64> {
        self.as_f64().map(|n| {
            if n.is_finite() && n > 0.0 {
                n as u64
            } else {
                0
            }
        })
    }

    pub fn as_arr(&self) -> Option<&Vec<Json>> {
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
pub fn parse(text: &str) -> Result<Json, String> {
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
    };
    p.value(0)
}

// ------------------------------------------------------------ 個票

/// 再生する 1 本 (`/recent` の 1 件から要るものだけ抜いたもの)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shot {
    /// 開いた時刻 (epoch 秒。`/connections` から作ったときは先頭を 0 とする相対秒)
    pub at: u64,
    /// 宛先のホスト名 (**ポートは内蔵オリジンのものに置き換える**ので持たない)
    pub host: String,
    /// 元のポート (印字用)
    pub port: u16,
    /// 寿命 (秒)
    pub secs: u64,
    /// 上り (クライアント → オリジン)
    pub up: u64,
    /// 下り (オリジン → クライアント)
    pub down: u64,
    /// 内蔵オリジンの応答遅延で真似る RTT (ms。`/hosts` に無ければ 0)
    pub rtt_ms: u32,
}

/// 個票を読んだときの内訳。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadStats {
    /// 読んだ行
    pub rows: usize,
    /// 再生する CONNECT
    pub connect: usize,
    /// 飛ばした http (この口は CONNECT だけ再生する)
    pub http: usize,
    /// 宛先が読めずに落とした行
    pub dropped: usize,
    /// どの形から読んだか (`recent` / `connections`)
    pub source: &'static str,
}

impl Default for ReadStats {
    fn default() -> Self {
        ReadStats {
            rows: 0,
            connect: 0,
            http: 0,
            dropped: 0,
            source: "recent",
        }
    }
}

/// `{"<key>":[…]}` と `{"<key>":{"<key>":[…]}}` (= `/snapshot`) の両方から配列を拾う。
fn rows_of<'a>(doc: &'a Json, key: &str) -> Option<&'a Vec<Json>> {
    match doc.get(key) {
        Some(Json::Arr(a)) => Some(a),
        Some(inner @ Json::Obj(_)) => inner.get(key).and_then(Json::as_arr),
        _ => None,
    }
}

/// `host:port` を (ホスト名, ポート) に割る。`[::1]:443` と `connect://a.b:443` も読む。
pub fn split_target(target: &str) -> Option<(String, u16)> {
    let t = match target.split_once("://") {
        Some((_, rest)) => rest,
        None => target,
    };
    let (host, port) = if let Some(rest) = t.strip_prefix('[') {
        let (h, rest) = rest.split_once(']')?;
        (h, rest.strip_prefix(':').unwrap_or(""))
    } else {
        match t.rsplit_once(':') {
            Some((h, p)) => (h, p),
            None => (t, ""),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse().unwrap_or(443)))
}

/// `/recent` の 1 件の並びから個票を作る (**開いた順**に並べて返す)。
fn from_recent(rows: &[Json]) -> (Vec<Shot>, ReadStats) {
    let mut out = Vec::with_capacity(rows.len());
    let mut st = ReadStats::default();
    for r in rows {
        st.rows += 1;
        if r.get("kind").and_then(Json::as_str).unwrap_or("connect") != "connect" {
            st.http += 1;
            continue;
        }
        let Some((host, port)) = r
            .get("target")
            .and_then(Json::as_str)
            .and_then(split_target)
        else {
            st.dropped += 1;
            continue;
        };
        st.connect += 1;
        out.push(Shot {
            at: r.get("at").and_then(Json::as_u64).unwrap_or(0),
            host,
            port,
            secs: r.get("secs").and_then(Json::as_u64).unwrap_or(0),
            up: r.get("up").and_then(Json::as_u64).unwrap_or(0),
            down: r.get("down").and_then(Json::as_u64).unwrap_or(0),
            rtt_ms: 0,
        });
    }
    out.sort_by_key(|s| s.at);
    (out, st)
}

/// `/connections` の 1 件の並びから個票を作る。
///
/// **まだ閉じていない接続**なので `/recent` と違って「開いた時刻」も「寿命」も無い。
/// `age_secs` (開いてからの秒) を両方に使い、いちばん古いものを先頭 (0 秒) とする。
/// `bytes` は上り下りの合計しか無いので**全部下り**に寄せる (トンネルは下りが主)。
fn from_connections(rows: &[Json]) -> (Vec<Shot>, ReadStats) {
    let oldest = rows
        .iter()
        .filter_map(|r| r.get("age_secs").and_then(Json::as_u64))
        .max()
        .unwrap_or(0);
    let mut out = Vec::with_capacity(rows.len());
    let mut st = ReadStats {
        source: "connections",
        ..ReadStats::default()
    };
    for r in rows {
        st.rows += 1;
        if r.get("kind").and_then(Json::as_str).unwrap_or("connect") != "connect" {
            st.http += 1;
            continue;
        }
        let Some((host, port)) = r
            .get("target")
            .and_then(Json::as_str)
            .and_then(split_target)
        else {
            st.dropped += 1;
            continue;
        };
        let age = r.get("age_secs").and_then(Json::as_u64).unwrap_or(0);
        st.connect += 1;
        out.push(Shot {
            at: oldest.saturating_sub(age),
            host,
            port,
            secs: age,
            up: 0,
            down: r.get("bytes").and_then(Json::as_u64).unwrap_or(0),
            rtt_ms: 0,
        });
    }
    out.sort_by_key(|s| s.at);
    (out, st)
}

/// 個票を読む (`/recent` / `/snapshot` / `/connections` / 裸の配列)。
pub fn read_shots(text: &str) -> Result<(Vec<Shot>, ReadStats), String> {
    let doc = parse(text)?;
    if let Some(rows) = rows_of(&doc, "recent") {
        return Ok(from_recent(rows));
    }
    if let Some(rows) = rows_of(&doc, "connections") {
        return Ok(from_connections(rows));
    }
    if let Json::Arr(rows) = &doc {
        // 裸の配列は中身で見分ける (`age_secs` があれば `/connections` の形)
        if rows.iter().any(|r| r.get("age_secs").is_some()) {
            return Ok(from_connections(rows));
        }
        return Ok(from_recent(rows));
    }
    Err("\"recent\" も \"connections\" も見つかりません (/recent か /snapshot か /connections の JSON を渡してください)".to_string())
}

/// `/hosts` (や `/status` の `hosts[]`、`/snapshot` の `hosts`) から RTT を読む。
///
/// T14.5 より前のデプロイ先には `rtt_ms` が無いので、そのときは空の表になる (= 全部 0 ms)。
pub fn read_rtts(text: &str) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    let Ok(doc) = parse(text) else {
        return out;
    };
    let Some(rows) = rows_of(&doc, "hosts") else {
        return out;
    };
    for r in rows {
        let Some((host, _)) = r.get("host").and_then(Json::as_str).and_then(split_target) else {
            continue;
        };
        let Some(avg) = r
            .get("rtt_ms")
            .and_then(|v| v.get("avg"))
            .and_then(Json::as_f64)
        else {
            continue;
        };
        if avg > 0.0 {
            let ms = avg.round().clamp(0.0, 10_000.0) as u32;
            out.entry(host)
                .and_modify(|v| *v = (*v).max(ms))
                .or_insert(ms);
        }
    }
    out
}

/// 開いた時刻の**間隔**を、先頭からのミリ秒に直す (`speed` 倍速)。
///
/// 入力は昇順であること (`read_shots` が並べて返す)。`/recent` の `at` は秒刻みなので、
/// 同じ秒の個票はまとめて 0 ms 差で走り出す (= デプロイ先で同じ秒に来た山の形)。
pub fn offsets_ms(ats: &[u64], speed: f64) -> Vec<u64> {
    let speed = if speed.is_finite() && speed > 0.0 {
        speed
    } else {
        1.0
    };
    let base = ats.first().copied().unwrap_or(0);
    ats.iter()
        .map(|&at| (at.saturating_sub(base) as f64 * 1000.0 / speed).round() as u64)
        .collect()
}

// ------------------------------------------------------------ 再生の相手 (内蔵オリジン)

/// 再生の合図の大きさ (magic 4 + 下りのバイト 8 + 応答遅延の ms 4)。
const PREAMBLE: u64 = 16;
const MAGIC: &[u8; 4] = b"RPLY";
/// 1 本で流す下りの上限 (壊れた個票で何 GiB も流さないため)。
const MAX_DOWN: u64 = 1 << 30;

/// 再生の相手。トンネルの**先頭 16 バイト**を合図として読み、`delay_ms` 待ってから
/// `down` バイトを送り、あとは相手が閉じるまで読み捨てる。
///
/// `delay_ms` が `/hosts` の `rtt_ms` (T14.5) で、**内蔵オリジンの応答遅延**として
/// デプロイ先の往復を真似る。
fn spawn_replay_origin() -> io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let _ = stream.set_nodelay(true);
                let _ = serve_replay(stream);
            });
        }
    });
    Ok(addr)
}

fn serve_replay(mut sock: TcpStream) -> io::Result<()> {
    let mut head = [0u8; PREAMBLE as usize];
    sock.read_exact(&mut head)?;
    if &head[..4] != MAGIC {
        return Ok(());
    }
    let down = u64::from_le_bytes(head[4..12].try_into().expect("8 bytes")).min(MAX_DOWN);
    let delay_ms = u32::from_le_bytes(head[12..16].try_into().expect("4 bytes"));
    if delay_ms > 0 {
        thread::sleep(Duration::from_millis(delay_ms as u64));
    }
    if down > 0 {
        let chunk = vec![b'z'; (down.min(64 * 1024)) as usize];
        let mut left = down;
        while left > 0 {
            let want = left.min(chunk.len() as u64) as usize;
            sock.write_all(&chunk[..want])?;
            left -= want as u64;
        }
    }
    // 上りを読み捨てる (相手が閉じるまで)
    let mut buf = [0u8; 16 * 1024];
    while sock.read(&mut buf)? > 0 {}
    Ok(())
}

// ------------------------------------------------------------ 再生

/// 再生の途中経過 (全スレッドで 1 つ)。
#[derive(Default)]
struct Tally {
    /// CONNECT が張れなかった数
    failed: AtomicU64,
    /// 寿命より前にプロキシ側から閉じられた数 (T13.2 の追い出しはここに出る)
    cut: AtomicU64,
    up: AtomicU64,
    down: AtomicU64,
    /// CONNECT の確立時間 (us)
    connect_us: Mutex<Vec<u32>>,
}

/// `--only replay` の引数。
pub struct ReplayArgs<'a> {
    pub proxy: SocketAddr,
    /// 個票のファイル (`--replay-file`)
    pub file: &'a str,
    /// RTT を読むファイル (`--rtt-file`。個票と同じ JSON に `hosts` があればそちらも使う)
    pub rtt_file: Option<&'a str>,
    /// 何倍速か (`--speed`)
    pub speed: f64,
    /// 再生を打ち切る秒 (`--replay-secs`。0 なら最後まで)
    pub limit_secs: u64,
    /// `/status` と `/bursts` を引く先 (既定は `--proxy` と同じ)
    pub admin: SocketAddr,
}

pub fn run(a: &ReplayArgs<'_>) {
    let text = match std::fs::read_to_string(a.file) {
        Ok(t) => t,
        Err(e) => {
            println!("replay  cannot read {}: {}", a.file, e);
            std::process::exit(2);
        }
    };
    let (mut shots, st) = match read_shots(&text) {
        Ok(v) => v,
        Err(e) => {
            println!("replay  {} を読めません: {}", a.file, e);
            std::process::exit(2);
        }
    };
    if shots.is_empty() {
        println!(
            "replay  {} に再生できる CONNECT の個票がありません ({} 行、http {} 件)",
            a.file, st.rows, st.http
        );
        std::process::exit(2);
    }

    // RTT は同じ JSON の `hosts` を先に見て、`--rtt-file` があれば重ねる
    let mut rtts = read_rtts(&text);
    if let Some(path) = a.rtt_file {
        match std::fs::read_to_string(path) {
            Ok(t) => rtts.extend(read_rtts(&t)),
            Err(e) => println!("replay  cannot read {}: {}", path, e),
        }
    }
    let mut with_rtt = 0usize;
    for s in &mut shots {
        if let Some(&ms) = rtts.get(&s.host) {
            s.rtt_ms = ms;
            with_rtt += 1;
        }
    }

    let origin = match spawn_replay_origin() {
        Ok(o) => o,
        Err(e) => {
            println!("replay  cannot start the replay origin: {}", e);
            std::process::exit(2);
        }
    };

    // 名前が引けるか 1 度だけ確かめる (引けないとプロキシが全部 502 を返して何も測れない)
    let first = shots[0].host.clone();
    if (first.as_str(), origin.port()).to_socket_addrs().is_err() {
        println!(
            "replay  cannot resolve {}; run it inside \
             scripts/deployed-like.sh --hosts-from {} -- …",
            first, a.file
        );
        std::process::exit(2);
    }

    let ats: Vec<u64> = shots.iter().map(|s| s.at).collect();
    let offsets = offsets_ms(&ats, a.speed);
    let span = ats.last().copied().unwrap_or(0) - ats[0];
    let hosts: std::collections::BTreeSet<&str> = shots.iter().map(|s| s.host.as_str()).collect();
    println!(
        "replay   {} ({}): {} 個票 (connect {} / http {} / 落とした {})、宛先 {} 種、\
         span {}s -> {:.1}s (speed {}x)",
        a.file,
        st.source,
        st.rows,
        st.connect,
        st.http,
        st.dropped,
        hosts.len(),
        span,
        offsets.last().copied().unwrap_or(0) as f64 / 1000.0,
        a.speed,
    );
    println!(
        "replay   origin {} | RTT を真似る個票 {} / {} | 打ち切り {}",
        origin,
        with_rtt,
        shots.len(),
        if a.limit_secs == 0 {
            "なし".to_string()
        } else {
            format!("{}s", a.limit_secs)
        }
    );

    let tally = Arc::new(Tally::default());
    let started = Instant::now();
    let deadline = (a.limit_secs > 0).then(|| started + Duration::from_secs(a.limit_secs));
    let mut handles = Vec::with_capacity(shots.len());
    let mut skipped = 0usize;
    for (shot, off) in shots.into_iter().zip(offsets) {
        let due = Duration::from_millis(off);
        let now = started.elapsed();
        if due > now {
            thread::sleep(due - now);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            skipped += 1;
            continue;
        }
        let proxy = a.proxy;
        let port = origin.port();
        let speed = a.speed;
        let tally = Arc::clone(&tally);
        handles.push(thread::spawn(move || {
            run_one(proxy, port, &shot, speed, deadline, &tally);
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let elapsed = started.elapsed();

    let mut connect_us = tally.connect_us.lock().expect("connect_us").clone();
    connect_us.sort_unstable();
    let failed = tally.failed.load(Ordering::Relaxed);
    let cut = tally.cut.load(Ordering::Relaxed);
    println!(
        "replay   {} tunnels in {:.1}s (failed {}, cut by the proxy {}, not started {}, \
         up {} B, down {} B)",
        connect_us.len(),
        elapsed.as_secs_f64(),
        failed,
        cut,
        skipped,
        tally.up.load(Ordering::Relaxed),
        tally.down.load(Ordering::Relaxed),
    );
    Report {
        ops: connect_us.len() as u64,
        bytes: tally.up.load(Ordering::Relaxed) + tally.down.load(Ordering::Relaxed),
        elapsed,
        latencies_us: connect_us,
    }
    .print_multi("replay");
    print_proxy_side(a.admin);
}

/// 個票 1 本ぶんを再生する。
fn run_one(
    proxy: SocketAddr,
    origin_port: u16,
    shot: &Shot,
    speed: f64,
    deadline: Option<Instant>,
    tally: &Tally,
) {
    // ホスト名はそのまま、ポートだけ内蔵オリジンのものに置き換える
    let target = format!("{}:{}", shot.host, origin_port);
    let t0 = Instant::now();
    let Ok((sock, mut reader)) = open_tunnel(proxy, &target) else {
        tally.failed.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let connect_us = t0.elapsed().as_micros().min(u32::MAX as u128) as u32;
    tally
        .connect_us
        .lock()
        .expect("connect_us")
        .push(connect_us);

    let life_ms = (shot.secs as f64 * 1000.0 / speed.max(1e-9)).round() as u64;
    let mut until = t0 + Duration::from_millis(life_ms);
    if let Some(d) = deadline {
        until = until.min(d);
    }
    // 読みが永久に止まらないように、寿命 + 5 秒で必ず諦める
    let _ = sock.set_read_timeout(Some(
        (until.saturating_duration_since(Instant::now()) + Duration::from_secs(5))
            .max(Duration::from_millis(1)),
    ));

    let alive =
        transfer(&sock, &mut reader, shot, tally).is_ok() && hold(&sock, &mut reader, until);
    if !alive {
        tally.cut.fetch_add(1, Ordering::Relaxed);
    }
    let _ = sock.shutdown(Shutdown::Both);
}

/// 合図 → 上り → 下り。**上りは最低 16 バイト** (合図) 流れる。
fn transfer(
    mut sock: &TcpStream,
    reader: &mut BufReader<TcpStream>,
    shot: &Shot,
    tally: &Tally,
) -> io::Result<()> {
    let mut head = [0u8; PREAMBLE as usize];
    head[..4].copy_from_slice(MAGIC);
    head[4..12].copy_from_slice(&shot.down.min(MAX_DOWN).to_le_bytes());
    head[12..16].copy_from_slice(&shot.rtt_ms.to_le_bytes());
    sock.write_all(&head)?;
    tally.up.fetch_add(PREAMBLE, Ordering::Relaxed);

    // 合図のぶんを引いた残りを上りとして流す (個票の上りが 16 B 未満なら合図だけ)
    let mut left = shot.up.saturating_sub(PREAMBLE);
    if left > 0 {
        let chunk = vec![b'x'; left.min(64 * 1024) as usize];
        while left > 0 {
            let want = left.min(chunk.len() as u64) as usize;
            sock.write_all(&chunk[..want])?;
            tally.up.fetch_add(want as u64, Ordering::Relaxed);
            left -= want as u64;
        }
    }

    let mut left = shot.down.min(MAX_DOWN);
    if left > 0 {
        let mut buf = vec![0u8; left.min(64 * 1024) as usize];
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            let n = reader.read(&mut buf[..want])?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            tally.down.fetch_add(n as u64, Ordering::Relaxed);
            left -= n as u64;
        }
    }
    Ok(())
}

/// 寿命が尽きるまで握る。**プロキシ側から閉じられたら `false`**
/// (T13.2 の追い出しはここで見える)。
fn hold(sock: &TcpStream, reader: &mut BufReader<TcpStream>, until: Instant) -> bool {
    let mut buf = [0u8; 256];
    loop {
        let now = Instant::now();
        if now >= until {
            return true;
        }
        // 0 の待ちは `SO_RCVTIMEO` が受け付けないので 1 ms を下限にする
        let wait = (until - now).max(Duration::from_millis(1));
        if sock.set_read_timeout(Some(wait)).is_err() {
            thread::sleep(wait);
            return true;
        }
        match reader.read(&mut buf) {
            Ok(0) => return false,
            Ok(_) => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return false,
        }
    }
}

// ------------------------------------------------------------ プロキシ側の数字

/// `/status` の `evicted_idle` / `rejected_overload` と `/bursts` の枚数を 1 行で印字する。
fn print_proxy_side(admin: SocketAddr) {
    let status = fetch(admin, "/status").and_then(|b| parse(&b).ok());
    let bursts = fetch(admin, "/bursts").and_then(|b| parse(&b).ok());
    let num = |v: &Option<Json>, key: &str| -> String {
        v.as_ref()
            .and_then(|d| d.get(key))
            .and_then(Json::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string())
    };
    println!(
        "replay   /status evicted_idle {} rejected_overload {} active {}/{} | \
         /bursts recorded {} kept {}",
        num(&status, "evicted_idle"),
        num(&status, "rejected_overload"),
        num(&status, "active_connections"),
        num(&status, "max_conns"),
        num(&bursts, "recorded"),
        num(&bursts, "kept"),
    );
}

/// プロキシ自身のエンドポイントを 1 回引く (T12.3 の `local_path`。自分宛ては素通しされない)。
fn fetch(admin: SocketAddr, path: &str) -> Option<String> {
    let mut sock = TcpStream::connect_timeout(&admin, Duration::from_secs(2)).ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, admin
    );
    sock.write_all(req.as_bytes()).ok()?;
    let mut all = String::new();
    sock.read_to_string(&mut all).ok()?;
    let body = all.split_once("\r\n\r\n").map(|(_, b)| b)?;
    Some(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/recent` の 1 件をそのまま読めること (T14.29)。欄の名前と形は T14.4 の出力に合わせてある。
    #[test]
    fn reads_the_recent_form() {
        let text = r#"{"recent":[
          {"id":2,"at":1789000100,"client":"10.0.0.1","target":"b.example.invalid:443",
           "kind":"connect","secs":30,"reqs":0,"up":100,"down":2000,"reason":"client_eof",
           "status":0,"parked_secs":0,"parks":0,"ms":{"dns":1,"connect":2},
           "rtt_ms":{"client":null,"origin":null},"retrans":{"client":0,"origin":0}},
          {"id":1,"at":1789000000,"client":"10.0.0.1","target":"a.example.invalid:8443",
           "kind":"connect","secs":5,"reqs":0,"up":0,"down":0,"reason":"idle_timeout",
           "status":0,"parked_secs":0,"parks":0,"ms":{"dns":0,"connect":0}},
          {"id":3,"at":1789000200,"client":"10.0.0.2","target":"c.example.invalid:80",
           "kind":"http","secs":1,"reqs":4,"up":400,"down":900,"reason":"limit","status":200,
           "parked_secs":0,"parks":0,"ms":{"dns":0,"connect":0}}
        ],"count":3,"matched":3,"shown":3,"recorded":3,"sort":"time","truncated":false}"#;
        let (shots, st) = read_shots(text).expect("読めること");
        assert_eq!(st.rows, 3);
        assert_eq!(st.connect, 2);
        assert_eq!(st.http, 1); // http は再生しない
        assert_eq!(st.dropped, 0);
        assert_eq!(st.source, "recent");
        // **開いた順**に並ぶ (`/recent` は新しい順で返ってくるので並べ直している)
        assert_eq!(shots[0].at, 1789000000);
        assert_eq!(shots[0].host, "a.example.invalid");
        assert_eq!(shots[0].port, 8443);
        assert_eq!(shots[0].secs, 5);
        assert_eq!(shots[1].host, "b.example.invalid");
        assert_eq!((shots[1].up, shots[1].down), (100, 2000));
        assert_eq!(shots[1].secs, 30);
    }

    /// `/snapshot` (入れ子) と `/connections` (まだ閉じていない接続) も読めること。
    #[test]
    fn reads_the_snapshot_and_connections_forms() {
        let snap = r#"{"taken_at":1,"recent":{"recent":[
            {"at":10,"target":"x.example.invalid:443","kind":"connect","secs":2,"up":1,"down":2}
          ],"count":1},"hosts":{"hosts":[
            {"host":"connect://x.example.invalid:443","requests":1,
             "rtt_ms":{"avg":24.5,"min":20.0,"samples":2},"retrans":0}
          ]}}"#;
        let (shots, st) = read_shots(snap).expect("読めること");
        assert_eq!(st.connect, 1);
        assert_eq!(shots[0].host, "x.example.invalid");
        // RTT は `/hosts` の `rtt_ms.avg` を ms に丸めたもの
        let rtts = read_rtts(snap);
        assert_eq!(rtts.get("x.example.invalid"), Some(&25));

        let conns = r#"{"connections":[
            {"id":1,"client":"10.0.0.1","target":"old.example.invalid:443","kind":"connect",
             "state":"parked","age_secs":300,"bytes":9000,"fds":2},
            {"id":2,"client":"10.0.0.1","target":"new.example.invalid:443","kind":"connect",
             "state":"parked","age_secs":100,"bytes":500,"fds":2},
            {"id":3,"client":"10.0.0.2","target":"","kind":"http","state":"serving",
             "age_secs":0,"bytes":0,"fds":1}
          ],"count":3}"#;
        let (shots, st) = read_shots(conns).expect("読めること");
        assert_eq!(st.source, "connections");
        assert_eq!((st.connect, st.http), (2, 1));
        // いちばん古いものが先頭 (0 秒)、200 秒あとに 2 本目。寿命は `age_secs` をそのまま使う
        assert_eq!((shots[0].at, shots[0].secs), (0, 300));
        assert_eq!((shots[1].at, shots[1].secs), (200, 100));
        // `bytes` は合計しか無いので全部下りに寄せる
        assert_eq!((shots[0].up, shots[0].down), (0, 9000));

        // 宛先が読めない行は落とす (数だけ残す)
        let broken = r#"[{"at":1,"target":"","kind":"connect","secs":1}]"#;
        let (shots, st) = read_shots(broken).expect("読めること");
        assert!(shots.is_empty());
        assert_eq!(st.dropped, 1);
    }

    /// 間隔の計算: `at` の差をそのまま ms にし、`--speed` で割ること (T14.29)。
    #[test]
    fn keeps_the_intervals_and_scales_them_by_speed() {
        let ats = [1789000000, 1789000000, 1789000001, 1789000031];
        assert_eq!(offsets_ms(&ats, 1.0), [0, 0, 1_000, 31_000]);
        assert_eq!(offsets_ms(&ats, 10.0), [0, 0, 100, 3_100]);
        // 3 倍速は四捨五入 (1000/3 = 333.33 -> 333)
        assert_eq!(offsets_ms(&ats, 3.0), [0, 0, 333, 10_333]);
        // 0 や負の倍速は等速として扱う (割り算で無限大にしない)
        assert_eq!(offsets_ms(&ats, 0.0), [0, 0, 1_000, 31_000]);
        assert_eq!(offsets_ms(&[], 10.0), Vec::<u64>::new());
    }

    /// `host:port` の割り方 (IPv6 リテラルと `connect://` 付きも読む)。
    #[test]
    fn splits_targets() {
        assert_eq!(
            split_target("a.example.invalid:443"),
            Some(("a.example.invalid".to_string(), 443))
        );
        assert_eq!(
            split_target("connect://a.example.invalid:5228"),
            Some(("a.example.invalid".to_string(), 5228))
        );
        assert_eq!(
            split_target("[2001:db8::1]:443"),
            Some(("2001:db8::1".to_string(), 443))
        );
        // ポートが無ければ 443 とみなす
        assert_eq!(
            split_target("a.example.invalid"),
            Some(("a.example.invalid".to_string(), 443))
        );
        assert_eq!(split_target(""), None);
    }

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
}
