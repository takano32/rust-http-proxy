//! コマンドラインの読み方。環境 (`std::env::args`) に触るのは [`parse_args`] だけで、
//! 引数の並びから [`Args`] を組むところ ([`Args::parse`]) は純粋なので単体試験で縛れる。

pub(crate) struct Args {
    pub(crate) proxy: Option<String>,
    pub(crate) conc: usize,
    pub(crate) seconds: u64,
    pub(crate) body_bytes: usize,
    /// オリジン応答を保存可能にする (キャッシュ HIT 側を測る)
    pub(crate) cacheable: bool,
    /// 測る種類 ("all" / "direct" / "forward" / "tunnel" / "connect" / "connect-multi" /
    /// "idle-tunnels" / "idle-conns" / "syscall-cost" / "replay")
    pub(crate) only: String,
    /// 1 要求ごとに接続を張り直す (接続あたりの固定費を測る)
    pub(crate) no_keepalive: bool,
    /// `--only replay` が読む個票 (`/recent` / `/snapshot` / `/connections` の JSON。T14.29)
    pub(crate) replay_file: Option<String>,
    /// `--only replay` の RTT の出どころ (`/hosts` の JSON。個票と同じ JSON にあれば要らない)
    pub(crate) rtt_file: Option<String>,
    /// 再生の倍速 (`--only replay`。1 で等速、10 で 10 倍速)
    pub(crate) speed: f64,
    /// 再生を打ち切る秒 (`--only replay`。0 なら個票を最後まで再生する)
    pub(crate) replay_secs: u64,
    /// `/status` と `/bursts` を引く先 (既定は `--proxy` と同じ)
    pub(crate) admin: Option<String>,
}

/// 使い方を出して終わる引数 (`-h`、知らない旗、読めない値、値の無い旗、0 の並列数や秒数)。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Usage;

impl Args {
    /// 旗を 1 つも渡さないときの値。
    fn defaults() -> Args {
        Args {
            proxy: None,
            conc: 8,
            seconds: 5,
            body_bytes: 1024,
            cacheable: false,
            only: "all".to_string(),
            no_keepalive: false,
            replay_file: None,
            rtt_file: None,
            speed: 1.0,
            replay_secs: 0,
            admin: None,
        }
    }

    /// 引数の並び (プログラム名を除く) を読む。読めなければ [`Usage`]。
    pub(crate) fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, Usage> {
        let mut out = Args::defaults();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            let mut value = || it.next().ok_or(Usage);
            match a.as_str() {
                "--proxy" => out.proxy = Some(value()?),
                "--conc" => out.conc = value()?.parse().map_err(|_| Usage)?,
                "--seconds" => out.seconds = value()?.parse().map_err(|_| Usage)?,
                "--body-bytes" => out.body_bytes = value()?.parse().map_err(|_| Usage)?,
                "--cacheable" => out.cacheable = true,
                "--only" => out.only = value()?,
                "--no-keepalive" => out.no_keepalive = true,
                "--replay-file" => out.replay_file = Some(value()?),
                "--rtt-file" => out.rtt_file = Some(value()?),
                "--speed" => out.speed = value()?.parse().map_err(|_| Usage)?,
                "--replay-secs" => out.replay_secs = value()?.parse().map_err(|_| Usage)?,
                "--admin" => out.admin = Some(value()?),
                "-h" | "--help" => return Err(Usage),
                _ => return Err(Usage),
            }
        }
        if out.conc == 0 || out.seconds == 0 || !(out.speed.is_finite() && out.speed > 0.0) {
            return Err(Usage);
        }
        Ok(out)
    }
}

pub(crate) fn usage() -> ! {
    eprintln!(
        "usage: bench [--proxy HOST:PORT] [--conc N] [--seconds N] [--body-bytes N]\n\
                     [--only direct|forward|tunnel|connect|connect-multi|idle-tunnels|\n\
                             idle-conns|syscall-cost|replay|all]\n\
                     [--replay-file PATH] [--rtt-file PATH] [--speed N] [--replay-secs N]\n\
                     [--admin HOST:PORT]\n\
         \n\
         Without --proxy only the direct (origin) baseline is measured.\n\
         --only replay needs --replay-file (a /recent, /snapshot or /connections JSON)."
    );
    std::process::exit(2);
}

/// 環境の引数を読む。読めなければ使い方を出して 2 で終わる。
pub(crate) fn parse_args() -> Args {
    Args::parse(std::env::args().skip(1)).unwrap_or_else(|Usage| usage())
}

#[cfg(test)]
mod tests {
    use super::{Args, Usage};

    fn parse(args: &[&str]) -> Result<Args, Usage> {
        Args::parse(args.iter().map(|s| s.to_string()))
    }

    /// 旗を渡さなければ既定 (`--conc 8 --seconds 5 --body-bytes 1024`、全部を測る、等速)。
    #[test]
    fn defaults_measure_everything_at_normal_speed() {
        let a = parse(&[]).expect("既定");
        assert_eq!(a.proxy, None);
        assert_eq!((a.conc, a.seconds, a.body_bytes), (8, 5, 1024));
        assert!(!a.cacheable && !a.no_keepalive);
        assert_eq!(a.only, "all");
        assert_eq!((a.replay_file, a.rtt_file, a.admin), (None, None, None));
        assert_eq!((a.speed, a.replay_secs), (1.0, 0));
    }

    /// 旗はどれも読める (値付きと旗だけの両方)。
    #[test]
    fn every_flag_is_read() {
        let a = parse(&[
            "--proxy",
            "127.0.0.1:18080",
            "--conc",
            "2",
            "--seconds",
            "20",
            "--body-bytes",
            "64",
            "--cacheable",
            "--only",
            "replay",
            "--no-keepalive",
            "--replay-file",
            "recent.json",
            "--rtt-file",
            "hosts.json",
            "--speed",
            "10",
            "--replay-secs",
            "30",
            "--admin",
            "127.0.0.1:18081",
        ])
        .expect("読めること");
        assert_eq!(a.proxy.as_deref(), Some("127.0.0.1:18080"));
        assert_eq!((a.conc, a.seconds, a.body_bytes), (2, 20, 64));
        assert!(a.cacheable && a.no_keepalive);
        assert_eq!(a.only, "replay");
        assert_eq!(a.replay_file.as_deref(), Some("recent.json"));
        assert_eq!(a.rtt_file.as_deref(), Some("hosts.json"));
        assert_eq!((a.speed, a.replay_secs), (10.0, 30));
        assert_eq!(a.admin.as_deref(), Some("127.0.0.1:18081"));
    }

    /// 使い方を出して終わる形: 知らない旗・値の無い旗・読めない値・`-h`・0 の並列数と秒数・
    /// 0 や無限の倍速。
    #[test]
    fn bad_arguments_ask_for_the_usage() {
        for bad in [
            vec!["--nope"],
            vec!["--conc"],
            vec!["--conc", "abc"],
            vec!["--seconds", "-1"],
            vec!["--body-bytes", "1.5"],
            vec!["-h"],
            vec!["--help"],
            vec!["--conc", "0"],
            vec!["--seconds", "0"],
            vec!["--speed", "0"],
            vec!["--speed", "-2"],
            vec!["--speed", "inf"],
            vec!["--speed", "nan"],
            vec!["--speed", "fast"],
            vec!["--replay-secs", "x"],
        ] {
            assert_eq!(parse(&bad).err(), Some(Usage), "{:?}", bad);
        }
        // 後ろの旗が前の値を上書きする (読み方は左から右)
        let a = parse(&["--conc", "1", "--conc", "3"]).expect("読めること");
        assert_eq!(a.conc, 3);
    }
}
