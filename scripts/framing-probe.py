#!/usr/bin/env python3
"""要求の枠組み (Content-Length / Host / 折り返し) を実機で流して、状態を並べる。

使い方:
    python3 scripts/framing-probe.py ./target/release/rust-http-proxy
    python3 scripts/framing-probe.py ./target/release/rust-http-proxy --port 19099

オリジン (127.0.0.1:<port+1>) とプロキシ (127.0.0.1:<port>) を自分で起こし、
終わったら自分が起こした PID だけを止める。判定は「期待する状態コード」との
突き合わせで、1 つでも外れたら exit 1。

この表は 2026-10-23 に 0.26.0 で見つけた穴 (T27.0 〜 T27.2) を固定するためのもの。
`Content-Length` の重複不一致・21 桁・カンマ・負、`Host` 2 本、obs-fold は
どれも「オリジンへ素通し」だった。直ったあとは全部 400 になる。
"""
import argparse
import json
import os
import socket
import socketserver
import subprocess
import sys
import threading
import time

# 期待する状態コードと、オリジンに届いてはいけないかどうか
#   (名前, 要求バイト列の組み立て, 期待する状態, オリジンに届いてよいか)
def cases(origin_port):
    base = b"http://127.0.0.1:%d/c" % origin_port
    host = b"127.0.0.1:%d" % origin_port
    tail = b"\r\n\r\n"

    def req(line, headers, body=b""):
        return b"POST " + base + b" HTTP/1.1\r\nHost: " + host + b"\r\n" + headers + tail + body

    return [
        ("正常な POST (5 B)", req(b"", b"Content-Length: 5\r\n", b"hello"), 200, True),
        ("CL の重複が同値 (5/5)", req(b"", b"Content-Length: 5\r\nContent-Length: 5\r\n", b"hello"), 200, True),
        ("CL の重複が不一致 (5/6)", req(b"", b"Content-Length: 5\r\nContent-Length: 6\r\n", b"hello"), 400, False),
        ("CL が 21 桁", req(b"", b"Content-Length: 99999999999999999999\r\n"), 400, False),
        ("CL がカンマ区切り (5, 5)", req(b"", b"Content-Length: 5, 5\r\n", b"hello"), 400, False),
        ("CL が負 (-1)", req(b"", b"Content-Length: -1\r\n"), 400, False),
        ("CL が数字でない (abc)", req(b"", b"Content-Length: abc\r\n"), 400, False),
        ("Host が 2 本", b"GET " + base + b" HTTP/1.1\r\nHost: " + host + b"\r\nHost: evil.example\r\n\r\n", 400, False),
        ("obs-fold (空白始まり)", req(b"", b"X-A: 1\r\n 2\r\nContent-Length: 0\r\n"), 400, False),
        ("obs-fold (タブ始まり)", req(b"", b"X-A: 1\r\n\t2\r\nContent-Length: 0\r\n"), 400, False),
    ]


def connect_cases(echo_port):
    """CONNECT の入口 (T27.0)。`http` の経路を通らないので、同じ検査が効いているかを見る。"""
    authority = b"127.0.0.1:%d" % echo_port
    head = b"CONNECT " + authority + b" HTTP/1.1\r\n"
    return [
        ("CONNECT 正常", head + b"Host: " + authority + b"\r\n\r\n", 200, True),
        ("CONNECT + Host 2 本", head + b"Host: " + authority + b"\r\nHost: evil.example\r\n\r\n", 400, False),
        ("CONNECT + obs-fold", head + b"Host: " + authority + b"\r\nX-A: 1\r\n 2\r\n\r\n", 400, False),
        ("CONNECT + CL (本文 5 B)", head + b"Host: " + authority + b"\r\nContent-Length: 5\r\n\r\nhello", 400, False),
    ]


class _Origin(socketserver.StreamRequestHandler):
    """受け取ったヘッダーと本文を覚えておくだけのオリジン。"""

    def handle(self):
        try:
            raw = b""
            while b"\r\n\r\n" not in raw:
                chunk = self.rfile.read1(65536)
                if not chunk:
                    return
                raw += chunk
            head, _, rest = raw.partition(b"\r\n\r\n")
            lines = [l.decode("latin1") for l in head.split(b"\r\n")]
            self.server.seen.append(
                {"request_line": lines[0], "headers": [l for l in lines[1:] if l], "body": rest.decode("latin1")}
            )
            payload = json.dumps(self.server.seen[-1]).encode()
            self.wfile.write(
                b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(payload) + payload
            )
        except Exception:  # noqa: BLE001 オリジンは落ちても判定を止めない
            return


class _Echo(socketserver.StreamRequestHandler):
    """トンネルの相手。受け取ったバイトをそのまま返し、回数と中身を覚える。"""

    def handle(self):
        try:
            while True:
                chunk = self.rfile.read1(65536)
                if not chunk:
                    break
                self.server.seen.append(chunk)
                self.wfile.write(b"[" + chunk + b"]")
        except Exception:  # noqa: BLE001
            return


class _Threaded(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
    seen: list


def _send(port, raw, wait=3.0):
    sock = socket.create_connection(("127.0.0.1", port), timeout=5)
    sock.sendall(raw)
    sock.settimeout(wait)
    out = b""
    try:
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            out += chunk
    except socket.timeout:
        pass
    sock.close()
    return out


def _status(raw):
    line = raw.split(b"\r\n", 1)[0].decode("latin1") if raw else ""
    parts = line.split(" ")
    try:
        return int(parts[1])
    except (IndexError, ValueError):
        return None


def main(argv=None):
    ap = argparse.ArgumentParser(description="要求の枠組みを実機で流して状態を並べる")
    ap.add_argument("binary", help="プロキシのバイナリ (例 ./target/release/rust-http-proxy)")
    ap.add_argument("--port", type=int, default=19098, help="プロキシの port (既定 19098)")
    ap.add_argument("--timeout", type=float, default=3.0, help="1 本あたりの待ち (秒)")
    args = ap.parse_args(argv)

    origin = _Threaded(("127.0.0.1", args.port + 1), _Origin)
    origin.seen = []  # type: ignore[attr-defined]
    threading.Thread(target=origin.serve_forever, daemon=True).start()

    # CONNECT のトンネル先 (受け取ったバイトをそのまま返す)
    echo = _Threaded(("127.0.0.1", args.port + 2), _Echo)
    echo.seen = []  # type: ignore[attr-defined]
    threading.Thread(target=echo.serve_forever, daemon=True).start()

    env = dict(os.environ, PROXY_ALLOW_LOCAL="1", PROXY_LOG_LEVEL="warn")
    proxy = subprocess.Popen(
        [args.binary, "-p", str(args.port)],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    failed = 0
    try:
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", args.port), timeout=0.2).close()
                break
            except OSError:
                time.sleep(0.1)
        else:
            print("プロキシが起動しない: %s -p %d" % (args.binary, args.port), file=sys.stderr)
            return 1

        print("%-28s %-8s %-8s %s" % ("要求", "状態", "期待", "オリジンに届いたヘッダー"))
        for name, raw, want, may_reach in cases(args.port + 1):
            before = len(origin.seen)
            got = _status(_send(args.port, raw, args.timeout))
            reached = len(origin.seen) > before
            headers = origin.seen[-1]["headers"] if reached else []
            ok = got == want and (may_reach or not reached)
            if not ok:
                failed += 1
            shown = ", ".join(h for h in headers if h.lower().startswith(("host:", "content-length:", "x-a")))
            print(
                "%-28s %-8s %-8s %s%s"
                % (name, got if got is not None else "(無応答)", want, shown or "-", "" if ok else "   ← ちがう")
            )

        # CONNECT は `http` の経路を通らないので別に見る (T27.0)
        print("")
        print("%-28s %-8s %-8s %s" % ("CONNECT", "状態", "期待", "トンネル先が受け取ったバイト"))
        for name, raw, want, may_reach in connect_cases(args.port + 2):
            before = len(echo.seen)
            got = _status(_send(args.port, raw, args.timeout))
            seen_bytes = b"".join(echo.seen[before:])
            ok = got == want and (may_reach or not seen_bytes)
            if not ok:
                failed += 1
            print(
                "%-28s %-8s %-8s %s%s"
                % (name, got if got is not None else "(無応答)", want, seen_bytes[:40] or "-", "" if ok else "   ← ちがう")
            )
    finally:
        proxy.terminate()
        try:
            proxy.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proxy.kill()
        origin.shutdown()
        origin.server_close()
        echo.shutdown()
        echo.server_close()

    print("")
    if failed:
        print("期待とちがう: %d 本" % failed)
        return 1
    print("全部期待どおり。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
