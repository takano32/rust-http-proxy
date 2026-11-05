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
}

impl ProxyProcess {
    /// `HOME=home` で起動し、待ち受けポートが分かるまで待つ。`home/.env` は呼ぶ側が先に置く。
    pub fn start(home: &std::path::Path) -> ProxyProcess {
        use std::io::BufRead;
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
            .env("HOME", home)
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
        let mut port = None;
        while let Ok(line) = rx.recv_timeout(Duration::from_secs(20)) {
            if let Some((_, rest)) = line.split_once("listening on ") {
                port = rest
                    .split_whitespace()
                    .next()
                    .and_then(|a| a.rsplit(':').next())
                    .and_then(|p| p.parse::<u16>().ok());
                break;
            }
        }
        let port = port.expect("the proxy did not log its listening port");
        ProxyProcess {
            child,
            port,
            lines: rx,
        }
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
