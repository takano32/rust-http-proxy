//! 外のプロセスを起こす道具 (実バイナリの `ProxyProcess` と、python の TLS オリジン)。

use std::thread;
use std::time::Duration;

/// `HOME` を渡して**実バイナリ**を起こし、`.env` を見る配線ごと確かめるための番人
/// (T14.18。落ちても子プロセスを残さない)。
///
/// 起動ログ (`... listening on 127.0.0.1:PORT ...`) から待ち受けポートを取り、以降の行は
/// [`ProxyProcess::wait_for_log`] で待てる。標準出力は読み続けないとパイプが詰まるので、
/// 専用スレッドで全部引き取る。
pub struct ProxyProcess {
    child: std::process::Child,
    pub port: u16,
    lines: std::sync::mpsc::Receiver<String>,
    /// 待ち受けの行より前に出た起動ログの行 (`state file ...` や設定の WARN。T.34.106)
    pub startup_log: Vec<String>,
    /// 待ち受けの行そのもの (`rust-http-proxy <版> listening on ... backlog N ...`)
    pub banner: String,
}

/// [`ProxyProcess::builder`] の組み立て (T.34.106)。`HOME` のほかに渡す環境変数と、
/// 起動ログの 1 行を待つ長さを持つ。
pub struct ProxyBuilder {
    home: std::path::PathBuf,
    vars: Vec<(String, String)>,
    line_wait: Duration,
}

impl ProxyBuilder {
    /// **実際の環境変数**として渡す (`.env` とは出どころが違う)。
    pub fn env(mut self, key: &str, value: &str) -> ProxyBuilder {
        self.vars.push((key.to_string(), value.to_string()));
        self
    }

    /// 起動ログの 1 行を待つ長さ (既定 20 秒)。
    pub fn line_wait(mut self, wait: Duration) -> ProxyBuilder {
        self.line_wait = wait;
        self
    }

    /// 起動し、待ち受けポートが分かるまで待つ。`home/.env` は呼ぶ側が先に置く。
    ///
    /// 子プロセスは最初に番人へ入れるので、待ち受けの行が出ずに落ちても残らない。
    pub fn start(self) -> ProxyProcess {
        use std::io::BufRead;
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"));
        cmd.env("HOME", &self.home);
        for (k, v) in &self.vars {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("could not start the proxy binary");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let mut proc = ProxyProcess {
            child,
            port: 0,
            lines: rx,
            startup_log: Vec::new(),
            banner: String::new(),
        };
        let mut port = None;
        while let Ok(line) = proc.lines.recv_timeout(self.line_wait) {
            if let Some((_, rest)) = line.split_once("listening on ") {
                port = rest
                    .split_whitespace()
                    .next()
                    .and_then(|a| a.rsplit(':').next())
                    .and_then(|p| p.parse::<u16>().ok());
                proc.banner = line;
                break;
            }
            proc.startup_log.push(line);
        }
        proc.port = port.expect("the proxy did not log its listening port");
        proc
    }
}

impl ProxyProcess {
    /// `HOME=home` で起動し、待ち受けポートが分かるまで待つ。`home/.env` は呼ぶ側が先に置く。
    pub fn start(home: &std::path::Path) -> ProxyProcess {
        ProxyProcess::builder(home).start()
    }

    /// 環境変数や待つ長さを足して起動するための組み立て (T.34.106)。
    pub fn builder(home: &std::path::Path) -> ProxyBuilder {
        ProxyBuilder {
            home: home.to_path_buf(),
            vars: Vec::new(),
            line_wait: Duration::from_secs(20),
        }
    }

    /// 子プロセスの pid (`/proc/<pid>/fd` を数える試験用)。
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// 起動ログのうち `needle` を含む**最後の**行 (無ければ空)。
    pub fn startup_line(&self, needle: &str) -> String {
        self.startup_log
            .iter()
            .rev()
            .find(|l| l.contains(needle))
            .cloned()
            .unwrap_or_default()
    }

    /// **停止シグナル (SIGTERM) を送って終わるまで待つ** (T14.9)。
    ///
    /// 後始末 (統計の書き出しと、まだ書いていない個票の追記) を通すための口。
    /// 5 秒で終わらなければ落とす。
    pub fn stop(&mut self) {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20))
                }
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// 標準出力に `needle` を含む行が出るまで待つ (`.env` の再読込の完了を待つため。
    /// **`/status` を叩いて待つと、それ自体が接続 1 本になって数えたい指標が動く**)。
    pub fn wait_for_log(&self, needle: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match self.lines.recv_timeout(left) {
                Ok(line) if line.contains(needle) => return line,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        panic!("timed out waiting for a log line containing {:?}", needle);
    }

    /// 標準出力に届いた行を、`quiet` のあいだ何も来なくなるまで全部引き取る (T.28.9)。
    pub fn drain_log(&self, quiet: Duration) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(line) = self.lines.recv_timeout(quiet) {
            out.push(line);
        }
        out
    }
}

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 自己署名証明書を作り、python の https サーバーを立てる。道具が無ければ None (テストはスキップ)。
pub fn start_tls_origin(
    dir: &std::path::Path,
) -> Option<(u16, std::path::PathBuf, std::process::Child)> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    std::fs::create_dir_all(dir.join("www")).ok()?;
    std::fs::write(dir.join("www/hello.txt"), b"hello over tls").ok()?;
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let ok = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-keyout",
            key.to_str()?,
            "-out",
            cert.to_str()?,
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=IP:127.0.0.1,DNS:localhost",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    let script = format!(
        "import http.server, ssl, functools, sys\n\
         class H(http.server.SimpleHTTPRequestHandler):\n\
             protocol_version = 'HTTP/1.1'\n\
             def log_message(self, *a): pass\n\
         ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)\n\
         ctx.load_cert_chain({cert:?}, {key:?})\n\
         srv = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(H, directory={www:?}))\n\
         srv.socket = ctx.wrap_socket(srv.socket, server_side=True)\n\
         print(srv.server_address[1], flush=True)\n\
         srv.serve_forever()\n",
        cert = cert.to_str()?,
        key = key.to_str()?,
        www = dir.join("www").to_str()?,
    );
    let mut child = Command::new("python3")
        .args(["-c", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take()?)
        .read_line(&mut line)
        .ok()?;
    let port: u16 = line.trim().parse().ok()?;
    Some((port, cert, child))
}
