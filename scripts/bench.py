#!/usr/bin/env python3
"""rust-http-proxy の簡易ベンチ (依存なし: python3 標準ライブラリのみ)。

    scripts/bench.py [--proxy 127.0.0.1:8080] [--conc 64] [--seconds 5]

プロキシを別途起動しておく (推奨: PROXY_CACHE_ENABLED=off PROXY_LOG_LEVEL=warn PROXY_STATS_PERSIST=off)。
このスクリプト自身がオリジン (1 KiB 応答 / 生 TCP の送信サーバー) を立てて、次の 3 つを測る:

  1. forward   : 平文 HTTP を keep-alive で転送したときの要求/秒と p50/p99 レイテンシ
  2. tunnel    : CONNECT トンネル 1 本のスループット (MiB/s)
  3. connect   : CONNECT の確立/秒 (短命トンネルを大量に張る)

Python 側のオーバーヘッドが上限になるので絶対値ではなく **同じマシンでの前後比較** に使う。
"""
import argparse, http.client, socket, socketserver, threading, time, statistics, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BODY = b"x" * 1024
SINK_MIB = 256

class Origin(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    RESP = (b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: %d\r\n"
            b"Cache-Control: no-store\r\n\r\n" % len(BODY)) + BODY
    def setup(self):
        super().setup()
        # 頭と本文を別々に書くと Nagle + delayed ACK で 40ms 止まるので、1 回で書き、NODELAY も立てる
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    def do_GET(self):
        self.wfile.write(self.RESP)
    def log_message(self, *a):
        pass

class Blaster(socketserver.BaseRequestHandler):
    """接続されたら SINK_MIB MiB を送って閉じる (トンネル用オリジン)。"""
    def handle(self):
        chunk = b"y" * (1 << 20)
        try:
            for _ in range(SINK_MIB):
                self.request.sendall(chunk)
        except OSError:
            pass

class Threaded(socketserver.ThreadingMixIn, socketserver.TCPServer):
    allow_reuse_address = True
    daemon_threads = True

def start_servers():
    http_srv = ThreadingHTTPServer(("127.0.0.1", 0), Origin)
    http_srv.daemon_threads = True
    threading.Thread(target=http_srv.serve_forever, daemon=True).start()
    blast = Threaded(("127.0.0.1", 0), Blaster)
    threading.Thread(target=blast.serve_forever, daemon=True).start()
    return http_srv.server_address[1], blast.server_address[1]

def bench_forward(proxy, origin_port, conc, seconds):
    host, port = proxy
    url = f"http://127.0.0.1:{origin_port}/"
    lat = [[] for _ in range(conc)]
    stop = time.monotonic() + seconds
    def worker(i):
        c = http.client.HTTPConnection(host, port, timeout=10)
        while time.monotonic() < stop:
            t = time.perf_counter()
            c.request("GET", url, headers={"Host": f"127.0.0.1:{origin_port}"})
            r = c.getresponse(); r.read()
            if r.status != 200:
                print("unexpected status", r.status, file=sys.stderr); return
            lat[i].append(time.perf_counter() - t)
    ts = [threading.Thread(target=worker, args=(i,)) for i in range(conc)]
    t0 = time.perf_counter()
    for t in ts: t.start()
    for t in ts: t.join()
    el = time.perf_counter() - t0
    all_lat = sorted(x for l in lat for x in l)
    n = len(all_lat)
    q = lambda p: all_lat[min(n - 1, int(n * p))] * 1000
    print(f"forward  conc={conc:<4} {n/el:9.0f} req/s   p50={q(0.5):.2f}ms p99={q(0.99):.2f}ms")

def open_tunnel(proxy, target_port):
    s = socket.create_connection(proxy)
    s.sendall(f"CONNECT 127.0.0.1:{target_port} HTTP/1.1\r\nHost: 127.0.0.1:{target_port}\r\n\r\n".encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        d = s.recv(4096)
        if not d: raise RuntimeError("proxy closed during CONNECT")
        buf += d
    head, rest = buf.split(b"\r\n\r\n", 1)
    if b" 200 " not in head.split(b"\r\n")[0]:
        raise RuntimeError(head.decode(errors="replace"))
    return s, rest

def bench_tunnel(proxy, blast_port):
    s, rest = open_tunnel(proxy, blast_port)
    total = len(rest)
    t0 = time.perf_counter()
    while True:
        d = s.recv(1 << 20)
        if not d: break
        total += len(d)
    el = time.perf_counter() - t0
    s.close()
    print(f"tunnel   1 stream   {total / el / (1 << 20):9.0f} MiB/s   ({total >> 20} MiB in {el:.2f}s)")

def bench_connect(proxy, origin_port, conc, seconds):
    counts = [0] * conc
    stop = time.monotonic() + seconds
    def worker(i):
        while time.monotonic() < stop:
            s, _ = open_tunnel(proxy, origin_port)
            s.sendall(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            while s.recv(65536): pass
            s.close(); counts[i] += 1
    ts = [threading.Thread(target=worker, args=(i,)) for i in range(conc)]
    t0 = time.perf_counter()
    for t in ts: t.start()
    for t in ts: t.join()
    el = time.perf_counter() - t0
    print(f"connect  conc={conc:<4} {sum(counts)/el:9.0f} tunnels/s")

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--proxy", default="127.0.0.1:8080")
    ap.add_argument("--conc", type=int, default=64)
    ap.add_argument("--seconds", type=float, default=5)
    a = ap.parse_args()
    host, port = a.proxy.rsplit(":", 1)
    proxy = (host, int(port))
    origin_port, blast_port = start_servers()
    bench_forward(proxy, origin_port, 8, a.seconds)
    bench_forward(proxy, origin_port, a.conc, a.seconds)
    bench_tunnel(proxy, blast_port)
    bench_connect(proxy, origin_port, a.conc, a.seconds)

if __name__ == "__main__":
    main()
