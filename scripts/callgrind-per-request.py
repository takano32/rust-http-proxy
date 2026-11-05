#!/usr/bin/env python3
# callgrind の下でプロキシを起こし、forward / hit / connect の **Ir/要求** を印字する (T.34.7)。
#
#     scripts/callgrind-per-request.py --bin target/release/rust-http-proxy
#     scripts/callgrind-per-request.py --bin before/rust-http-proxy --bin target/release/rust-http-proxy --rounds 2
#
# 測り方は T.28.12・T.29.24・T.34.0 と同じ: `--lite` + `PROXY_LOG_LEVEL=warn`、応答本文 1 KiB、
# 並列 2 (`--conc`)。自分で立てたオリジン (127.0.0.1 の空きポート) へ **ちょうど N 本** の要求を流してから
# SIGTERM で止め、callgrind の出力の `totals:` (Ir) を読む。起動と終了の固定費を消すため、
# N を 2 通り (`--n1` 400 と `--n2` 2000) 回して **傾き (Ir2 − Ir1) / (N2 − N1)** を 1 本あたりとする。
#   - forward: `Cache-Control: no-store` の GET を keep-alive の接続 `--conc` 本で流す (キャッシュは切る)
#   - hit:     `max-age=600` の同じ URL を流す (キャッシュを入れる。初めの 1 本だけ MISS)
#   - connect: `CONNECT` で 1 本ずつトンネルを開け、中で GET を 1 回して閉じる
# `--bin` を 2 つ以上渡すと、回 (`--rounds`) ごとにバイナリを交互に測り (機械の揺れが片方に偏らないように)、
# 最初の `--bin` を基準にした増減 (%) を添える。熱い経路の小タスクの受け入れ基準 (+0.5% 以内) を見る道具。
# バイナリは `CARGO_PROFILE_RELEASE_STRIP=none cargo build --release` で作ると、callgrind_annotate で中が読める。
# callgrind の出力は `--out` (既定は一時ディレクトリ) に `<bin の番号>-<mode>-<N>.cg` で残す。
#
# 使う道具は python3 の標準ライブラリと valgrind だけ (外部依存なし)。
import argparse
import http.client
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BODY = b"x" * 1024
MODES = ("forward", "hit", "connect")


def read_totals(lines):
    """callgrind の出力の行から Ir の合計を読む (`totals:` を優先し、無ければ `summary:`)。無ければ None。"""
    totals = summary = None
    for line in lines:
        if line.startswith("totals:"):
            totals = int(line.split()[1])
        elif line.startswith("summary:"):
            summary = int(line.split()[1])
    return totals if totals is not None else summary


def per_request(n1, ir1, n2, ir2):
    """2 点 (N1, Ir1)・(N2, Ir2) の傾き = 1 本あたりの Ir (整数に切り捨て)。"""
    if n2 == n1:
        raise ValueError("N1 と N2 が同じでは傾きが取れない")
    return (ir2 - ir1) // (n2 - n1)


def change_pct(base, value):
    """基準からの増減 (%)。基準が 0 なら None。"""
    if not base:
        return None
    return (value - base) * 100.0 / base


def median(xs):
    s = sorted(xs)
    m = len(s) // 2
    return s[m] if len(s) % 2 else (s[m - 1] + s[m]) // 2


def split_counts(n, conc):
    """n 本を conc 本の流れに配る (先頭から 1 本ずつ多く)。"""
    return [n // conc + (1 if i < n % conc else 0) for i in range(conc)]


def report_lines(bins, modes, results):
    """results[(bin の番号, mode)] = [回ごとの Ir/要求] から、印字する行を作る。"""
    out = []
    for mode in modes:
        base = None
        for i, b in enumerate(bins):
            got = results.get((i, mode))
            if not got:
                out.append(f"{mode:8} [{i}] {b}: 測れなかった")
                continue
            med = median(got)
            line = f"{mode:8} [{i}] {b}: {med:,} Ir/要求"
            if len(got) > 1:
                line += " (回ごと " + " / ".join(f"{x:,}" for x in got) + ")"
            if i == 0:
                base = med
            else:
                pct = change_pct(base, med)
                if pct is not None:
                    line += f"  [0] から {pct:+.2f}%"
            out.append(line)
    return out


class Origin(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self):
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def do_GET(self):
        cc = b"Cache-Control: max-age=600\r\n" if self.path == "/c" else b"Cache-Control: no-store\r\n"
        self.wfile.write(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: %d\r\n" % len(BODY)
            + cc
            + b"\r\n"
            + BODY
        )

    def log_message(self, *a):
        pass


def wait_port(port, deadline):
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                return True
        except OSError:
            time.sleep(0.2)
    return False


def run_threads(target, counts):
    errors = []
    ts = [threading.Thread(target=target, args=(c, errors)) for c in counts]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    return errors


def drive_forward(proxy_port, origin_port, n, conc, cacheable):
    url = f"http://127.0.0.1:{origin_port}/{'c' if cacheable else 'n'}"

    def worker(count, errors):
        c = http.client.HTTPConnection("127.0.0.1", proxy_port, timeout=60)
        for _ in range(count):
            c.request("GET", url, headers={"Host": f"127.0.0.1:{origin_port}"})
            r = c.getresponse()
            r.read()
            if r.status != 200:
                errors.append(r.status)
                return
        c.close()

    return run_threads(worker, split_counts(n, conc))


def drive_connect(proxy_port, origin_port, n, conc):
    head = f"CONNECT 127.0.0.1:{origin_port} HTTP/1.1\r\nHost: 127.0.0.1:{origin_port}\r\n\r\n".encode()

    def worker(count, errors):
        for _ in range(count):
            s = socket.create_connection(("127.0.0.1", proxy_port), timeout=60)
            s.sendall(head)
            buf = b""
            while b"\r\n\r\n" not in buf:
                d = s.recv(4096)
                if not d:
                    errors.append("closed")
                    return
                buf += d
            if b" 200 " not in buf.split(b"\r\n")[0]:
                errors.append(buf[:40])
                return
            s.sendall(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            while s.recv(65536):
                pass
            s.close()

    return run_threads(worker, split_counts(n, conc))


def measure(binary, port, origin_port, mode, n, conc, out):
    """callgrind の下で 1 回起こして n 本流し、Ir の合計を返す。"""
    home = tempfile.mkdtemp(prefix="cg-home-")
    env = dict(os.environ)
    env.update(
        {
            "HOME": home,
            "PROXY_ALLOW_LOCAL": "on",
            "PROXY_LOG_LEVEL": "warn",
            "PROXY_STATS_PERSIST": "off",
            "PROXY_CACHE_ENABLED": "on" if mode == "hit" else "off",
            "PROXY_BIND": "127.0.0.1",
        }
    )
    cmd = ["valgrind", "--tool=callgrind", f"--callgrind-out-file={out}", "--quiet", binary, "--lite", "-p", str(port)]
    with open(out + ".log", "wb") as log:
        p = subprocess.Popen(cmd, env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            if not wait_port(port, time.monotonic() + 300):
                raise RuntimeError(f"プロキシが {port} で待ち受けなかった (ログ: {out}.log)")
            if mode == "connect":
                errors = drive_connect(port, origin_port, n, conc)
            else:
                errors = drive_forward(port, origin_port, n, conc, mode == "hit")
            if errors:
                raise RuntimeError(f"{mode} の要求が失敗した: {errors[:5]}")
        finally:
            p.send_signal(signal.SIGTERM)
            try:
                p.wait(timeout=120)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
    with open(out) as f:
        ir = read_totals(f)
    if ir is None:
        raise RuntimeError(f"{out} に totals が無い")
    return ir


def main(argv=None):
    ap = argparse.ArgumentParser(description="callgrind で forward / hit / connect の Ir/要求 を測る (T.34.7)")
    ap.add_argument("--bin", action="append", required=True, help="測るバイナリ (2 つ以上で交互に測り、最初を基準に比べる)")
    ap.add_argument("--port", type=int, default=19100, help="プロキシに待ち受けさせるポート (既定 19100)")
    ap.add_argument("--modes", default=",".join(MODES), help="測る形 (既定 forward,hit,connect)")
    ap.add_argument("--n1", type=int, default=400)
    ap.add_argument("--n2", type=int, default=2000)
    ap.add_argument("--conc", type=int, default=2)
    ap.add_argument("--rounds", type=int, default=1, help="交互に回す回数 (回ごとの中央値を出す)")
    ap.add_argument("--out", help="callgrind の出力を残すディレクトリ (既定は一時ディレクトリ)")
    a = ap.parse_args(argv)
    modes = [m for m in a.modes.split(",") if m]
    bad = [m for m in modes if m not in MODES]
    if bad:
        ap.error(f"知らない形: {', '.join(bad)}")
    out = a.out or tempfile.mkdtemp(prefix="callgrind-per-request-")
    os.makedirs(out, exist_ok=True)

    srv = ThreadingHTTPServer(("127.0.0.1", 0), Origin)
    srv.daemon_threads = True
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    origin_port = srv.server_address[1]

    results = {}
    failed = False
    for r in range(a.rounds):
        for i, b in enumerate(a.bin):
            for mode in modes:
                try:
                    ir1 = measure(b, a.port, origin_port, mode, a.n1, a.conc, os.path.join(out, f"{i}-{mode}-{a.n1}.cg"))
                    ir2 = measure(b, a.port, origin_port, mode, a.n2, a.conc, os.path.join(out, f"{i}-{mode}-{a.n2}.cg"))
                except (RuntimeError, OSError) as e:
                    print(f"回 {r + 1} [{i}] {mode}: {e}", file=sys.stderr)
                    failed = True
                    continue
                per = per_request(a.n1, ir1, a.n2, ir2)
                results.setdefault((i, mode), []).append(per)
                print(f"回 {r + 1} [{i}] {mode}: {per:,} Ir/要求 (N={a.n1}: {ir1:,} / N={a.n2}: {ir2:,})", file=sys.stderr)
    for line in report_lines(a.bin, modes, results):
        print(line)
    print(f"(callgrind の出力: {out})")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
