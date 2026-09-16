//! 記録を一括で止める / 接続元をハッシュにする旗 (`PROXY_RECORDS`。T14.41)。
//!
//! T13.4〜T14.7 で個票 (接続元 IP・宛先・`User-Agent`・SNI) が増えた。**認証は入れない**
//! 方針 (§0) なので、公開ポートで動かすと `/recent` `/errors` `/connections` `/clients`
//! `/log` `/events` `/trace` `/bursts` は誰でも読める。利用者以外の人の情報を残さないために、
//! **記録を 1 つの旗で全部止める** ([`Mode::Off`]) か、**接続元 IP を復元できない形で持つ**
//! ([`Mode::Hashed`]) 選択肢をここに置く。
//!
//! **なぜいちばん下のクレートに置くか**: `/log` のリング ([`crate::log`]) はこの
//! `proxy-base` にあり、`proxy-metrics` へは依存できない (依存の向きが逆)。旗を 2 か所に
//! 持つと必ずずれるので、**唯一の置き場をここにして**上の層 (`proxy-metrics` の各リング、
//! `proxy-config`、`proxy-reload`) から読む。
//!
//! **費用**: 記録の入口ごとに[原子 1 回の読みと分岐 1 回]だけ。既定 (`on`) では
//! [`client_key`] が借りたままの文字列を返すので、確保も複製も増えない
//! (`--lite` はそもそも個票を作らないので、この旗を見るところまで来ない)。
//!
//! **止めないもの**: ホスト別の統計 (`/hosts`) と `/history` と `/status` の数字は
//! `off` でも残る。個人に結びつくのは接続元の側だけで、宛先ホストの集計と時系列は
//! 「誰が」を含まないため (README に書いてある)。

use std::borrow::Cow;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// `PROXY_RECORDS` の値。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// 今までどおり全部残す (既定)
    #[default]
    On,
    /// 個票のリングに 1 件も書かない (`/hosts` と `/history` は残る)
    Off,
    /// 残すが、接続元 IP は[起動ごとの乱数つき FNV-1a 64 ビットの 16 進 16 桁]に置き換える
    Hashed,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::On => "on",
            Mode::Off => "off",
            Mode::Hashed => "hashed",
        }
    }

    fn code(self) -> u8 {
        match self {
            Mode::On => 0,
            Mode::Off => 1,
            Mode::Hashed => 2,
        }
    }

    fn from_code(v: u8) -> Mode {
        match v {
            1 => Mode::Off,
            2 => Mode::Hashed,
            _ => Mode::On,
        }
    }
}

/// `.env` の書き方を読む。読めない書き方は `None` (呼ぶ側が既定に落とす)。
///
/// `PROXY_STATS_PERSIST` などと同じく `off` / `no` / `0` / `false` も受ける
/// (利用者が `off` と書けば通ることを優先。理由は §0 の「既定で安全な方」)。
pub fn parse(s: &str) -> Option<Mode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "on" | "yes" | "1" | "true" => Some(Mode::On),
        "off" | "no" | "0" | "false" => Some(Mode::Off),
        "hashed" | "hash" => Some(Mode::Hashed),
        _ => None,
    }
}

/// いま効いている値 (既定は [`Mode::On`])。
static MODE: AtomicU8 = AtomicU8::new(0);

/// いま効いている値を読む (原子 1 回)。
#[inline]
pub fn mode() -> Mode {
    Mode::from_code(MODE.load(Ordering::Relaxed))
}

/// 値を当てる (起動時の `Live::new` と、`.env` の再読込で 1 回ずつ)。
///
/// 当たった瞬間から**次の記録**に効く。既に書いてあるものは書き換えない
/// (`on` → `off` にしても、それまでに溜まった個票は残る。消したいなら再起動する)。
pub fn set(m: Mode) {
    MODE.store(m.code(), Ordering::Relaxed);
}

/// 記録してよいか (`off` 以外)。**各リングの書き込みの入口はこれ 1 つで分岐する。**
#[inline]
pub fn recording() -> bool {
    MODE.load(Ordering::Relaxed) != Mode::Off.code()
}

/// この起動の塩 (`RandomState` の種 + 時刻 + プロセス番号 + スタックのアドレス)。
///
/// 外部クレートは足さない方針 (§0) なので、[`crate::via`] の印と同じ作り方をする。
/// **起動ごとに変わる**ので、同じ接続元でも再起動すれば別の値になり、値から IP を
/// 引き当てる表 (IPv4 は 43 億通りしかない) を作り置きできない。
fn salt() -> u64 {
    static SALT: OnceLock<u64> = OnceLock::new();
    *SALT.get_or_init(|| {
        let anchor = 0u8;
        let mut h = RandomState::new().build_hasher();
        h.write_usize(&anchor as *const u8 as usize);
        h.write_u64(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
                .unwrap_or(0),
        );
        h.write_u32(std::process::id());
        h.finish()
    })
}

/// 16 進 16 桁 (塩つき FNV-1a 64 ビット)。**塩を先に流してから**本文を流す。
fn hash_hex(s: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in salt().to_le_bytes().iter().chain(s.as_bytes()) {
        h ^= *b as u64;
        h = h.wrapping_mul(PRIME);
    }
    format!("{:016x}", h)
}

/// **接続元 IP を記録に載せる形に直す唯一の関数。**
///
/// `/clients` の鍵 (T14.7)、`/recent` `/errors` の個票の `client`、`/connections` の
/// `client` (と、そこから作る `/bursts` の写真の接続元別)、`/trace` (T14.27) は
/// **全部ここを通す** — 別々に書くと必ずどれかが生の IP のまま残るため。
///
/// `on` / `off` は借りたまま返す (確保 0)。`hashed` のときだけ 16 桁の 16 進にする。
/// 空文字列 (canary など「自分」の意味) はそのまま返す。
#[inline]
pub fn client_key(ip: &str) -> Cow<'_, str> {
    if ip.is_empty() || mode() != Mode::Hashed {
        return Cow::Borrowed(ip);
    }
    Cow::Owned(hash_hex(ip))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 旗はプロセス全体で 1 つなので、触るテストは順番に回す。
    static LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parses_the_three_spellings_and_rejects_the_rest() {
        assert_eq!(parse("on"), Some(Mode::On));
        assert_eq!(parse(" HASHED "), Some(Mode::Hashed));
        assert_eq!(parse("off"), Some(Mode::Off));
        assert_eq!(parse("0"), Some(Mode::Off));
        assert_eq!(parse("sometimes"), None);
        assert_eq!(parse(""), None);
        assert_eq!(Mode::default(), Mode::On);
    }

    #[test]
    fn off_stops_recording_and_on_restores_it() {
        let _g = crate::sync::LockExt::locked(&LOCK);
        set(Mode::On);
        assert!(recording());
        set(Mode::Hashed);
        assert!(recording(), "hashed は記録する (形を変えるだけ)");
        set(Mode::Off);
        assert!(!recording());
        set(Mode::On);
    }

    #[test]
    fn hashed_is_sixteen_hex_and_stable_within_a_process() {
        let _g = crate::sync::LockExt::locked(&LOCK);
        set(Mode::On);
        assert_eq!(client_key("127.0.0.1"), "127.0.0.1", "on はそのまま");
        set(Mode::Off);
        assert_eq!(client_key("127.0.0.1"), "127.0.0.1", "off も変換はしない");
        set(Mode::Hashed);
        let a = client_key("127.0.0.1").into_owned();
        let b = client_key("127.0.0.1").into_owned();
        assert_eq!(a, b, "同じ接続元は同じ値");
        assert_eq!(a.len(), 16, "{}", a);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()), "{}", a);
        assert_ne!(a, client_key("127.0.0.2"), "違う接続元は違う値");
        assert_eq!(client_key(""), "", "空 (自分) はそのまま");
        set(Mode::On);
    }

    #[test]
    fn the_salt_makes_the_value_unguessable() {
        // 塩を混ぜていない素の FNV-1a とは違う値になること (作り置きの表が効かない)
        let _g = crate::sync::LockExt::locked(&LOCK);
        set(Mode::Hashed);
        let plain = {
            let mut h = 0xcbf2_9ce4_8422_2325u64;
            for b in "10.0.0.1".as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            format!("{:016x}", h)
        };
        assert_ne!(client_key("10.0.0.1"), plain);
        set(Mode::On);
    }
}
