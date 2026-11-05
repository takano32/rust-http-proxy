//! 個票 (`/recent` / `/snapshot` / `/connections` の JSON) から再生する 1 本 ([`Shot`]) を作る。
//! ここは JSON を読んだあとの純粋な変換だけで、ソケットも時計も持たない。

use std::collections::HashMap;

use super::json::{Json, parse};

/// 再生する 1 本 (`/recent` の 1 件から要るものだけ抜いたもの)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shot {
    /// 開いた時刻 (epoch 秒。`/connections` から作ったときは先頭を 0 とする相対秒)
    pub(crate) at: u64,
    /// 宛先のホスト名 (**ポートは内蔵オリジンのものに置き換える**ので持たない)
    pub(crate) host: String,
    /// 元のポート (印字用)
    pub(crate) port: u16,
    /// 寿命 (秒)
    pub(crate) secs: u64,
    /// 上り (クライアント → オリジン)
    pub(crate) up: u64,
    /// 下り (オリジン → クライアント)
    pub(crate) down: u64,
    /// 内蔵オリジンの応答遅延で真似る RTT (ms。`/hosts` に無ければ 0)
    pub(crate) rtt_ms: u32,
}

/// 個票を読んだときの内訳。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadStats {
    /// 読んだ行
    pub(crate) rows: usize,
    /// 再生する CONNECT
    pub(crate) connect: usize,
    /// 飛ばした http (この口は CONNECT だけ再生する)
    pub(crate) http: usize,
    /// 宛先が読めずに落とした行
    pub(crate) dropped: usize,
    /// どの形から読んだか (`recent` / `connections`)
    pub(crate) source: &'static str,
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
pub(crate) fn split_target(target: &str) -> Option<(String, u16)> {
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
pub(crate) fn read_shots(text: &str) -> Result<(Vec<Shot>, ReadStats), String> {
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
pub(crate) fn read_rtts(text: &str) -> HashMap<String, u32> {
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
pub(crate) fn offsets_ms(ats: &[u64], speed: f64) -> Vec<u64> {
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

    /// 裸の配列は中身で見分け (`age_secs` があれば `/connections`)、どちらの鍵も無ければ断る。
    /// `/hosts` の RTT は 0 以下を飛ばし、同じ名前は大きい方を残す。
    #[test]
    fn bare_arrays_are_told_apart_and_rtts_keep_the_largest() {
        let bare =
            r#"[{"target":"a.example.invalid:443","kind":"connect","age_secs":5,"bytes":10}]"#;
        let (shots, st) = read_shots(bare).expect("読めること");
        assert_eq!(st.source, "connections");
        assert_eq!((shots[0].at, shots[0].secs, shots[0].down), (0, 5, 10));
        assert!(
            read_shots(r#"{"status":{}}"#)
                .unwrap_err()
                .contains("見つかりません")
        );
        assert!(read_shots("nope").is_err());

        let hosts = r#"{"hosts":[
            {"host":"connect://a.example.invalid:443","rtt_ms":{"avg":10.4}},
            {"host":"connect://a.example.invalid:8443","rtt_ms":{"avg":30.6}},
            {"host":"http://b.example.invalid:80","rtt_ms":{"avg":0}},
            {"host":"connect://c.example.invalid:443","rtt_ms":{"avg":99999}},
            {"host":"connect://d.example.invalid:443"}
          ]}"#;
        let rtts = read_rtts(hosts);
        assert_eq!(
            rtts.get("a.example.invalid"),
            Some(&31),
            "ポート違いは大きい方"
        );
        assert_eq!(rtts.get("b.example.invalid"), None, "0 は無いのと同じ");
        assert_eq!(rtts.get("c.example.invalid"), Some(&10_000), "上限 10 秒");
        assert_eq!(rtts.get("d.example.invalid"), None);
        assert!(read_rtts("garbage").is_empty());
        assert!(read_rtts(r#"{"recent":[]}"#).is_empty());
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
}
