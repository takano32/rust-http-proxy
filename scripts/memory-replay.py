#!/usr/bin/env python3
"""RSS の段を手元で再生して、どの確保が残るかを測る (T.32.0)。

    scripts/memory-replay.py target/release/rust-http-proxy            # 4 つとも
    scripts/memory-replay.py BIN --only a,d                             # 選んで
    scripts/memory-replay.py BIN --env MALLOC_MMAP_THRESHOLD_=131072     # 環境を足して

場面ごとに**新しいプロセス**を 127.0.0.1 の空いた番号で起こし (HOME は使い捨ての
ディレクトリ、キャッシュの置き場もその下、`PROXY_CANARY=off`)、手元のオリジン (このスクリプトの中のスレッド) を使う
(`PROXY_ALLOW_LOCAL=on`)。

どの場面も、まず手元のオリジンへの CONNECT を `--warm` 本 (既定 2,000) 流して `/recent` を埋める。

- (a) 重い口: `/snapshot` を 5 回 (日次の雪像は同じ組み立てで上限が大きいだけ。T.29.16)
- (b) 名前解決の総当たり: 名前の引けない宛先 (`*.invalid`) への CONNECT を `--nx` 本 (既定 4,000、8 並列)
- (c) (b) のあとに (a)
- (d) メモリのキャッシュに 14 MiB の forward GET を入れて PURGE (= 追い出し) を 3 回、
  閾が上がった状態で (a)

1 つの段ごとに「前・後 (山)・trim 後」を 1 行ずつ出す。山は段の前に
`/proc/<pid>/clear_refs` へ 5 を書いて VmHWM を戻し、段の後の VmHWM を読む。
trim は gdb で `malloc_trim(0)` を 1 回呼ぶ (1 時間ごとの trim の代わり)。
glibc の `mp_.mmap_threshold`・`mp_.no_dyn_threshold`・`narenas` も gdb で読む
(gdb か glibc の記号が無ければ `-`)。数字は MB (10^6)。

要るもの: Linux・python3・curl は不要 (ソケットで直接話す)・gdb (無ければ trim と閾は飛ばす)。
"""

import argparse
import http.server
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

BIG_MIB = 14


class Origin(http.server.BaseHTTPRequestHandler):
    """GET /big/<MiB>/<n> に <MiB> MiB の本文を `public, max-age=60` で返す。"""

    protocol_version = "HTTP/1.1"

    def do_GET(self):
        parts = self.path.strip("/").split("/")
        mib = int(parts[1]) if len(parts) > 1 and parts[0] == "big" else 0
        n = mib << 20
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(n))
        self.send_header("Cache-Control", "public, max-age=60")
        self.end_headers()
        chunk = b"z" * 65536
        left = n
        while left > 0:
            k = min(left, len(chunk))
            self.wfile.write(chunk[:k])
            left -= k

    def log_message(self, *a):
        pass


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def raw(port, data, timeout=120):
    """1 本つないで `data` を送り、閉じられるまで読む (応答のバイト列)。"""
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    s.sendall(data)
    out = []
    while True:
        c = s.recv(1 << 16)
        if not c:
            break
        out.append(c)
    s.close()
    return b"".join(out)


def get(port, path):
    req = "GET %s HTTP/1.1\r\nHost: 127.0.0.1:%d\r\nConnection: close\r\n\r\n" % (path, port)
    return raw(port, req.encode())


def body_json(resp):
    return json.loads(resp.split(b"\r\n\r\n", 1)[1])


def connect_nx(port, i, prefix):
    target = b"%s-%d-mail.example-nx.invalid:443" % (prefix.encode(), i)
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=60)
        s.sendall(b"CONNECT %s HTTP/1.1\r\nHost: %s\r\n\r\n" % (target, target))
        buf = b""
        while b"\r\n\r\n" not in buf:
            c = s.recv(4096)
            if not c:
                break
            buf += c
        s.close()
    except OSError:
        pass


def connect_local(port, oport):
    """手元のオリジンへの CONNECT を 1 本 (数バイト通して閉じる。`/recent` を埋めるため)。"""
    target = b"127.0.0.1:%d" % oport
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=60)
        s.sendall(b"CONNECT %s HTTP/1.1\r\nHost: %s\r\n\r\n" % (target, target))
        buf = b""
        while b"\r\n\r\n" not in buf:
            c = s.recv(4096)
            if not c:
                break
            buf += c
        if buf.startswith(b"HTTP/1.1 200"):
            s.sendall(b"GET /big/0/x HTTP/1.0\r\n\r\n")
            while s.recv(65536):
                pass
        s.close()
    except OSError:
        pass


def run_many(count, conc, fn):
    idx = [0]
    lock = threading.Lock()

    def worker():
        while True:
            with lock:
                if idx[0] >= count:
                    return
                i = idx[0]
                idx[0] += 1
            fn(i)

    ts = [threading.Thread(target=worker) for _ in range(conc)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()


def nx_storm(port, count, conc, prefix):
    run_many(count, conc, lambda i: connect_nx(port, i, prefix))


def forward(port, oport, path, method="GET"):
    url = "http://127.0.0.1:%d%s" % (oport, path)
    req = "%s %s HTTP/1.1\r\nHost: 127.0.0.1:%d\r\nConnection: close\r\n\r\n" % (method, url, oport)
    return raw(port, req.encode())


def proc_status(pid):
    out = {}
    with open("/proc/%d/status" % pid) as f:
        for line in f:
            k, _, v = line.partition(":")
            if k in ("VmRSS", "VmHWM", "RssAnon", "RssFile"):
                out[k] = int(v.split()[0]) * 1024
    return out


def reset_hwm(pid):
    try:
        with open("/proc/%d/clear_refs" % pid, "w") as f:
            f.write("5")
    except OSError:
        pass


def gdb(pid, exprs):
    if not shutil.which("gdb"):
        return [None] * len(exprs)
    args = ["gdb", "-p", str(pid), "-batch", "-nx"]
    for e in exprs:
        args += ["-ex", e]
    try:
        r = subprocess.run(args, capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.TimeoutExpired):
        return [None] * len(exprs)
    vals = [line.split("=", 1)[1].strip() for line in r.stdout.splitlines() if line.startswith("$")]
    return vals + [None] * (len(exprs) - len(vals))


def glibc_knobs(pid):
    v = gdb(pid, ["p mp_.mmap_threshold", "p mp_.no_dyn_threshold", "p narenas"])
    return v


def trim(pid):
    gdb(pid, ["call (int)malloc_trim(0)"])


def measure(pid, port, label, knobs=False):
    st = proc_status(pid)
    try:
        m = body_json(get(port, "/status"))["memory"]
    except (OSError, ValueError, KeyError):
        m = {}
    row = {
        "label": label,
        "rss": st.get("VmRSS"),
        "hwm": st.get("VmHWM"),
        "anon": st.get("RssAnon"),
        "file": st.get("RssFile"),
        "heap_used": m.get("heap_used"),
        "heap_free": m.get("heap_free"),
        "mmap": m.get("mmap"),
    }
    if knobs:
        row["thr"], row["nodyn"], row["arenas"] = glibc_knobs(pid)
    return row


def mb(v):
    return "-" if v is None else "%.1f" % (v / 1e6)


def show(row):
    cols = [row["label"], mb(row["rss"]), mb(row["hwm"]), mb(row["anon"]), mb(row["file"]),
            mb(row["heap_used"]), mb(row["heap_free"]), mb(row["mmap"]),
            row.get("thr") or "-", row.get("nodyn") or "-", row.get("arenas") or "-"]
    print("| " + " | ".join(str(c) for c in cols) + " |", flush=True)


def start(binary, envs):
    home = tempfile.mkdtemp(prefix="memory-replay-")
    port = free_port()
    env = dict(os.environ)
    env.update({
        "HOME": home, "XDG_CACHE_HOME": os.path.join(home, ".cache"),
        "SERVER_PORT": str(port), "PROXY_BIND": "127.0.0.1", "PROXY_CANARY": "off",
        "PROXY_CACHE_DIR": os.path.join(home, "cache"), "PROXY_ALLOW_LOCAL": "on",
    })
    env.update(envs)
    log = open(os.path.join(home, "proxy.log"), "wb")
    p = subprocess.Popen([binary], env=env, cwd=home, stdout=log, stderr=log)
    for _ in range(200):
        try:
            if b" 200 " in get(port, "/healthz")[:16]:
                break
        except OSError:
            time.sleep(0.05)
    time.sleep(3)
    return p, port, home


def heavy(pid, port, tag):
    reset_hwm(pid)
    size = 0
    for _ in range(5):
        size = len(get(port, "/snapshot"))
    row = measure(pid, port, "%s/snapshot x5 (%.2f MB)" % (tag, size / 1e6))
    show(row)
    trim(pid)
    show(measure(pid, port, tag + "/snapshot trim", knobs=True))


def scenario(name, args, envs, oport):
    p, port, home = start(args.binary, envs)
    pid = p.pid
    try:
        print("\n### (%s)\n" % name)
        print("| 段 | rss | 山 (VmHWM) | anon | file | heap_used | heap_free | mmap | mmap_threshold | no_dyn | arenas |")
        print("|---|---|---|---|---|---|---|---|---|---|---|")
        # `/recent` (2,000 件) を埋めてから測る (tokyo の雪像はいつも満杯。空だと重い口が軽い)
        run_many(args.warm, 8, lambda i: connect_local(port, oport))
        time.sleep(1)
        show(measure(pid, port, "idle (CONNECT x%d)" % args.warm, knobs=True))
        if name in ("b", "c"):
            reset_hwm(pid)
            nx_storm(port, args.nx, 8, "nx%s" % name)
            show(measure(pid, port, "nx x%d" % args.nx))
            trim(pid)
            show(measure(pid, port, "nx trim", knobs=True))
        if name == "d":
            reset_hwm(pid)
            for i in range(3):
                forward(port, oport, "/big/%d/%d" % (BIG_MIB, i))
                forward(port, oport, "/big/%d/%d" % (BIG_MIB, i), method="PURGE")
            show(measure(pid, port, "14MiB GET->PURGE x3"))
            trim(pid)
            show(measure(pid, port, "GET->PURGE trim", knobs=True))
        if name in ("a", "c", "d"):
            heavy(pid, port, "")
    finally:
        p.terminate()
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            p.kill()
        shutil.rmtree(home, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("binary")
    ap.add_argument("--only", default="a,b,c,d")
    ap.add_argument("--nx", type=int, default=4000)
    ap.add_argument("--warm", type=int, default=2000, help="測る前の手元への CONNECT の本数")
    ap.add_argument("--env", action="append", default=[], help="K=V (何度でも)")
    args = ap.parse_args()
    envs = dict(e.split("=", 1) for e in args.env)
    oport = free_port()
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", oport), Origin)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    print("binary: %s  env: %s" % (args.binary, " ".join(args.env) or "(既定)"))
    for name in args.only.split(","):
        scenario(name.strip(), args, envs, oport)
    srv.shutdown()


if __name__ == "__main__":
    sys.exit(main())
