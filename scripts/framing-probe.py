#!/usr/bin/env python3
"""要求の枠組み (Content-Length / Host / 折り返し) を実機で流して、状態を並べる。

使い方:
    python3 scripts/framing-probe.py ./target/release/rust-http-proxy
    python3 scripts/framing-probe.py ./target/release/rust-http-proxy --port 19099

オリジン (127.0.0.1:<port+1>) とプロキシ (127.0.0.1:<port>) を自分で起こし、
終わったら自分が起こした PID だけを止める。判定は「期待する状態コード」との
突き合わせで、1 つでも外れたら exit 1。

この表は 2026-10-23 に 0.26.0 で見つけた穴 (T.19.1 〜 T.19.3、CONNECT は T.19.8) を固定するためのもの。
`Content-Length` の重複不一致・21 桁・カンマ・負、`Host` 2 本、obs-fold は
どれも「オリジンへ素通し」だった。直ったあとは全部 400 になる。

2026-10-24 に 0.27.1 で見つけた穴 (T.27.5 〜 T.27.8) も足した (`t27_cases` と `t27_response_cases`):
名前とコロンの間の空白・値の CR / NUL (400)、`Transfer-Encoding` の最後が chunked でない・2 回・
HTTP/1.0 (400)、応答の値の CR / NUL (SP に置き換えて返す)、`Max-Forwards`・userinfo・fragment・
版・CONNECT のポート・壊れた chunk。0.27.1 ではこの節の行が外れる。

2026-10-25 に応答の `Transfer-Encoding` (最後が chunked でない) + `Content-Length` を足した (T.28.1):
CL を捨てて閉じるまでを本文にする。chunked + CL は今までどおり chunked。0.27.8 では前の行が外れる。
同じ日に TE の行の扱いを足した (T.28.35): HTTP/1.1 へは TE の行を残して閉じて区切り、HTTP/1.0 へは 502。
T.28.1 の時点ではどちらも TE の行を落としていた (この 2 行が外れる)。

2026-10-27 に 1xx の中間応答を足した (T.29.3): HTTP/1.0 のクライアントへは 103 を送らず 200 だけ、
HTTP/1.1 へは今までどおり 103 → 200。0.29.35 では HTTP/1.0 の行が外れる。
同じ日に要求行とヘッダー名の文法を足した (T.29.8): 要求行が 4 要素・後ろに junk・対象の DEL / NUL、
ヘッダー名の空白・`"`・制御文字・空の名前・コロンの無い行はどれも 400。0.29.35 ではこの 8 行が外れる。

2026-10-31 に不正な `Host`・authority を足した (T.32.12。`t32_cases`): `Host: a b`・`a, b`・`a/b`・
`h:99999`・絶対形式のポートの `+` は 400 (オリジンへ届かない)、大文字のホストは通る。
0.30.4 では 400 の行が 502 (名前解決の失敗か接続の失敗) で外れる。
"""
import argparse
import json
import os
import socket
import socketserver
import subprocess
import sys
import tempfile
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


def t27_cases(origin_port):
    """T.27.5 〜 T.27.8 の要求側。5 つ目は「届いたときに満たすべきこと」(届いた要求の辞書を受け取る)。"""
    base = b"http://127.0.0.1:%d" % origin_port
    host = b"Host: 127.0.0.1:%d\r\n" % origin_port

    def post(headers, body=b"", version=b"HTTP/1.1"):
        return b"POST " + base + b"/c " + version + b"\r\n" + host + headers + b"\r\n" + body

    def get(path, headers=b"", method=b"GET", version=b"HTTP/1.1"):
        return method + b" " + base + path + b" " + version + b"\r\n" + host + headers + b"\r\n"

    chunked = b"5\r\nhello\r\n0\r\n\r\n"
    return [
        ("名前とコロンの間の空白 (CL)", post(b"Content-Length : 5\r\n", b"hello"), 400, False, None),
        ("値の途中の CR", get(b"/a", b"X-A: a\rTransfer-Encoding: chunked\r\n"), 400, False, None),
        ("値の NUL", get(b"/a", b"X-A: a\x00b\r\n"), 400, False, None),
        ("値の HTAB (通る)", get(b"/a", b"X-A: a\tb\r\n"), 200, True, None),
        ("TE: xchunked", post(b"Transfer-Encoding: xchunked\r\n", chunked), 400, False, None),
        ("TE: gzip だけ", post(b"Transfer-Encoding: gzip\r\n", b"hello"), 400, False, None),
        ("TE: chunked, identity", post(b"Transfer-Encoding: chunked, identity\r\n", chunked), 400, False, None),
        ("TE: chunked を 2 行", post(b"Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n", chunked),
         400, False, None),
        ("TE: xchunked + CL", post(b"Content-Length: 5\r\nTransfer-Encoding: xchunked\r\n", b"hello"), 400, False,
         None),
        ("HTTP/1.0 の TE", post(b"Transfer-Encoding: chunked\r\n", chunked, b"HTTP/1.0"), 400, False, None),
        ("TE: gzip, chunked (通る)", post(b"Transfer-Encoding: gzip, chunked\r\n", chunked), 200, True,
         lambda seen: seen["body"].startswith("5\r\nhello")),
        ("Max-Forwards: 0 の OPTIONS", get(b"/a", b"Max-Forwards: 0\r\n", b"OPTIONS"), 200, False, None),
        ("Max-Forwards: 0 の TRACE", get(b"/a", b"Max-Forwards: 0\r\nCookie: s=1\r\n", b"TRACE"), 200, False, None),
        ("Max-Forwards: 3 は 2 で届く", get(b"/a", b"Max-Forwards: 3\r\n", b"OPTIONS"), 200, True,
         lambda seen: "Max-Forwards: 2" in seen["headers"]),
        ("URI の userinfo", b"GET http://u:p@127.0.0.1:%d/a HTTP/1.1\r\n" % origin_port + host + b"\r\n", 400, False,
         None),
        ("URI の #fragment は落とす", get(b"/a?q#frag"), 200, True,
         lambda seen: seen["request_line"].startswith("GET /a?q ")),
        ("版 HTTP/9.9", get(b"/a", version=b"HTTP/9.9"), 505, False, None),
        ("HTTP/2 の preface", b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n", 505, False, None),
        ("chunk の大きさ 0x5", post(b"Transfer-Encoding: chunked\r\n", b"0x5\r\nhello\r\n0\r\n\r\n"), 400, True,
         None),
        # T.28.3: 本文つきの 100-continue には先に 100 (最初の状態行)、Expect は転送しない。ほかの期待は 417
        ("Expect: 100-continue", post(b"Expect: 100-continue\r\nContent-Length: 5\r\n", b"hello"), 100, True,
         lambda seen: not any(h.lower().startswith("expect:") for h in seen["headers"])
         and seen["body"] == "hello"),
        ("Expect: fancy は 417", post(b"Expect: fancy\r\nContent-Length: 5\r\n", b"hello"), 417, False, None),
        # T.29.8: 要求行はちょうど 3 要素・対象は VCHAR・版は HTTP/d.d、ヘッダー名は token
        ("要求行が 4 要素 (a b)", b"GET " + base + b"/a b HTTP/1.1\r\n" + host + b"\r\n", 400, False, None),
        ("要求行の後ろに junk", get(b"/a", version=b"HTTP/1.1 junk"), 400, False, None),
        ("対象に DEL", get(b"/a\x7f"), 400, False, None),
        ("対象に NUL", get(b"/a\x00b"), 400, False, None),
        ("名前の中の空白", get(b"/a", b"X A: 1\r\n"), 400, False, None),
        ("名前の \" と制御文字", get(b"/a", b"X\"\x01A: 1\r\n"), 400, False, None),
        ("空の名前", get(b"/a", b": v\r\n"), 400, False, None),
        ("コロンの無い行", get(b"/a", b"Junk\r\n"), 400, False, None),
    ]


def t32_cases(origin_port):
    """T.32 の要求側 (形は `t27_cases` と同じ 5 つ組)。"""
    port = b"%d" % origin_port

    def get(host, target=b"/a"):
        return b"GET " + target + b" HTTP/1.1\r\nHost: " + host + b"\r\n\r\n"

    return [
        # T.32.12: `Host` と authority の形 (reg-name の字・IP リテラル・ポートは 5 桁まで 65535 以下)
        ("Host: a b", get(b"a b"), 400, False, None),
        ("Host: a, b", get(b"a, b"), 400, False, None),
        ("Host: a/b", get(b"a/b"), 400, False, None),
        ("Host: h:99999", get(b"127.0.0.1:99999"), 400, False, None),
        ("authority のポートの +", get(b"127.0.0.1:" + port, b"http://127.0.0.1:+" + port + b"/a"), 400, False,
         None),
        ("Host の大文字 (通る)", get(b"LOCALHOST:" + port), 200, True, None),
    ]


def t27_response_cases(origin_port):
    """T.27.7 の応答側。オリジンが壊れた値を返し、プロキシは SP に置き換えて 200 で返す。"""
    host = b"127.0.0.1:%d" % origin_port

    def get(path):
        return b"GET http://" + host + path + b" HTTP/1.1\r\nHost: " + host + b"\r\n\r\n"

    def clean(resp):
        head = resp.split(b"\r\n\r\n", 1)[0]
        return b"\x00" not in head and head.count(b"\r") == head.count(b"\r\n")

    def get10(path):
        return b"GET http://" + host + path + b" HTTP/1.0\r\nHost: " + host + b"\r\n\r\n"

    def until_close(resp):
        # T.28.1: CL を捨てて閉じるまでを本文にする (CL は付かない)。
        # T.28.35: TE の行はオリジンのまま残し、包み直さずに閉じて区切る
        head, _, body = resp.partition(b"\r\n\r\n")
        lines = head.lower().split(b"\r\n")
        return (b"content-length" not in head.lower() and b"transfer-encoding: gzip" in lines
                and b"connection: close" in lines and body == b"helloEXTRA")

    def no_te(resp):
        # T.28.35: HTTP/1.0 へは TE を送れないので 502 (符号化済みのバイトを本文として渡さない)
        head, _, body = resp.partition(b"\r\n\r\n")
        return b"transfer-encoding" not in head.lower() and body == b""

    def chunked_wins(resp):
        # chunked と CL が両方なら今までどおり chunked (余分な "EXTRA" は本文に入らない)
        head, _, body = resp.partition(b"\r\n\r\n")
        return b"content-length" not in head.lower() and body == b"5\r\nhello\r\n0\r\n\r\n"

    def no_interim(resp):
        # T.29.3: HTTP/1.0 へは 1xx を送らない (最初の状態行が最終応答)
        return resp.startswith(b"HTTP/1.1 200 ") and b" 103 " not in resp and resp.endswith(b"\r\n\r\nok")

    def interim_first(resp):
        # HTTP/1.1 へは今までどおり 103 を中継してから 200
        return resp.startswith(b"HTTP/1.1 103 ") and b"\r\n\r\nHTTP/1.1 200 " in resp

    return [
        ("応答の値の CR", get(b"/resp-cr"), 200, clean),
        ("HTTP/1.0 + 103", get10(b"/resp-103-10"), 200, no_interim),
        ("HTTP/1.1 + 103 (中継する)", get(b"/resp-103-11"), 103, interim_first),
        ("応答の値の NUL", get(b"/resp-nul"), 200, clean),
        ("応答の TE: gzip + CL", get(b"/resp-te-cl"), 200, until_close),
        ("応答の TE: gzip を HTTP/1.0 へ", get10(b"/resp-te-cl"), 502, no_te),
        ("応答の chunked + CL", get(b"/resp-chunked-cl"), 200, chunked_wins),
    ]


def connect_cases(echo_port):
    """CONNECT の入口 (T.27.0)。`http` の経路を通らないので、同じ検査が効いているかを見る。"""
    authority = b"127.0.0.1:%d" % echo_port
    head = b"CONNECT " + authority + b" HTTP/1.1\r\n"
    return [
        ("CONNECT 正常", head + b"Host: " + authority + b"\r\n\r\n", 200, True),
        ("CONNECT + Host 2 本", head + b"Host: " + authority + b"\r\nHost: evil.example\r\n\r\n", 400, False),
        ("CONNECT + obs-fold", head + b"Host: " + authority + b"\r\nX-A: 1\r\n 2\r\n\r\n", 400, False),
        ("CONNECT + CL (本文 5 B)", head + b"Host: " + authority + b"\r\nContent-Length: 5\r\n\r\nhello", 400, False),
        # T.27.5 と T.27.8
        ("CONNECT + 名前とコロンの間の空白", head + b"Host : " + authority + b"\r\n\r\n", 400, False),
        ("CONNECT + 値の CR", head + b"Host: " + authority + b"\r\nX-A: a\rb\r\n\r\n", 400, False),
        ("CONNECT のポートなし", b"CONNECT 127.0.0.1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", 400, False),
        ("CONNECT のポート 0", b"CONNECT 127.0.0.1:0 HTTP/1.1\r\nHost: 127.0.0.1:0\r\n\r\n", 400, False),
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
            # T.27.7: 壊れた値を返すオリジン (プロキシが SP に置き換えるかを見る)
            if " /resp-cr " in lines[0] or " /resp-nul " in lines[0]:
                bad = b"X-A: a\rSet-Cookie: evil=1" if "/resp-cr" in lines[0] else b"X-A: a\x00b"
                self.wfile.write(b"HTTP/1.1 200 OK\r\n" + bad + b"\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                return
            # T.29.3: 103 を返してから 200
            if " /resp-103-" in lines[0]:
                self.wfile.write(b"HTTP/1.1 103 Early Hints\r\nLink: </a.css>; rel=preload\r\n\r\n"
                                 b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                return
            # T.28.1: 最後が chunked でない TE と CL を一緒に返し、CL の後ろにも続ける
            if " /resp-te-cl " in lines[0]:
                self.wfile.write(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: 5\r\n\r\nhelloEXTRA")
                return
            if " /resp-chunked-cl " in lines[0]:
                self.wfile.write(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 3\r\n"
                                 b"Connection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\nEXTRA")
                return
            # 本文がまだ届き切っていなければ少し待つ (`Content-Length` か chunked の終わりまで)
            lower = head.lower()
            for _ in range(20):
                if b"content-length:" in lower:
                    want = int(lower.split(b"content-length:")[1].split(b"\r\n")[0].strip() or 0)
                    if len(rest) >= want:
                        break
                elif b"transfer-encoding:" in lower:
                    if rest.endswith(b"0\r\n\r\n"):
                        break
                else:
                    break
                more = self.rfile.read1(65536)
                if not more:
                    break
                rest += more
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

    # キャッシュと状態ファイルはこの回だけの置き場に書く。既定の置き場 (root なら
    # `/var/cache/rust-http-proxy`) を使うと、2 回目の実行が前の回の項目に HIT して外れる
    work = tempfile.TemporaryDirectory(prefix="framing-probe-")
    env = dict(
        os.environ,
        PROXY_ALLOW_LOCAL="1",
        PROXY_LOG_LEVEL="warn",
        PROXY_CACHE_DIR=os.path.join(work.name, "cache"),
        PROXY_STATS_PERSIST="off",
    )
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

        # T.27.5 〜 T.27.8 の要求側 (5 つ目は届いた要求が満たすべきこと)
        print("")
        print("%-28s %-8s %-8s %s" % ("T.27.5〜T.27.8・T.32", "状態", "期待", "オリジンに届いた要求行"))
        for name, raw, want, may_reach, check in t27_cases(args.port + 1) + t32_cases(args.port + 1):
            before = len(origin.seen)
            got = _status(_send(args.port, raw, args.timeout))
            reached = len(origin.seen) > before
            seen = origin.seen[-1] if reached else None
            ok = got == want and (may_reach or not reached) and (check is None or (seen is not None and check(seen)))
            if not ok:
                failed += 1
            print(
                "%-28s %-8s %-8s %s%s"
                % (name, got if got is not None else "(無応答)", want, seen["request_line"] if seen else "-",
                   "" if ok else "   ← ちがう")
            )
        for name, raw, want, check in t27_response_cases(args.port + 1):
            resp = _send(args.port, raw, args.timeout)
            got = _status(resp)
            ok = got == want and check(resp)
            if not ok:
                failed += 1
            print("%-28s %-8s %-8s %s%s" % (name, got if got is not None else "(無応答)", want,
                                            "期待どおり" if ok else repr(resp[:60]),
                                            "" if ok else "   ← ちがう"))

        # CONNECT は `http` の経路を通らないので別に見る (T.19.8)
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
        work.cleanup()

    print("")
    if failed:
        print("期待とちがう: %d 本" % failed)
        return 1
    print("全部期待どおり。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
