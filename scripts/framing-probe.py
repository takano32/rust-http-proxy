#!/usr/bin/env python3
"""要求の枠組み (Content-Length / Host / 折り返し) を実機で流して、状態を並べる。

使い方:
    python3 scripts/framing-probe.py ./target/release/rust-http-proxy
    python3 scripts/framing-probe.py ./target/release/rust-http-proxy --port 19099

オリジン (127.0.0.1:<port+1>) とプロキシ (127.0.0.1:<port>) を自分で起こし、
終わったら自分が起こした PID だけを止める。判定は「期待する状態コード」との
突き合わせで、1 つでも外れたら exit 1。

この表は 2026-10-23 に 0.26.0 で見つけた穴 (T19.1 〜 T19.3、CONNECT は T19.8) を固定するためのもの。
`Content-Length` の重複不一致・21 桁・カンマ・負、`Host` 2 本、obs-fold は
どれも「オリジンへ素通し」だった。直ったあとは全部 400 になる。

2026-10-24 に 0.27.1 で見つけた穴 (T27.5 〜 T27.8) も足した (`t27_cases` と `t27_response_cases`):
名前とコロンの間の空白・値の CR / NUL (400)、`Transfer-Encoding` の最後が chunked でない・2 回・
HTTP/1.0 (400)、応答の値の CR / NUL (SP に置き換えて返す)、`Max-Forwards`・userinfo・fragment・
版・CONNECT のポート・壊れた chunk。0.27.1 ではこの節の行が外れる。

2026-10-25 に応答の `Transfer-Encoding` (最後が chunked でない) + `Content-Length` を足した (T28.1):
CL を捨てて閉じるまでを本文にする。chunked + CL は今までどおり chunked。0.27.8 では前の行が外れる。
同じ日に TE の行の扱いを足した (T28.35): HTTP/1.1 へは TE の行を残して閉じて区切り、HTTP/1.0 へは 502。
T28.1 の時点ではどちらも TE の行を落としていた (この 2 行が外れる)。

2026-10-27 に 1xx の中間応答を足した (T29.3): HTTP/1.0 のクライアントへは 103 を送らず 200 だけ、
HTTP/1.1 へは今までどおり 103 → 200。0.29.35 では HTTP/1.0 の行が外れる。
同じ日に要求行とヘッダー名の文法を足した (T29.8): 要求行が 4 要素・後ろに junk・対象の DEL / NUL、
ヘッダー名の空白・`"`・制御文字・空の名前・コロンの無い行はどれも 400。0.29.35 ではこの 8 行が外れる。

2026-10-31 に不正な `Host`・authority を足した (T32.12。`t32_cases`): `Host: a b`・`a, b`・`a/b`・
`h:99999`・絶対形式のポートの `+` は 400 (オリジンへ届かない)、大文字のホストは通る。
0.30.4 では 400 の行が 502 (名前解決の失敗か接続の失敗) で外れる。
同じ日に T32.15 の小物を足した: LF だけの行末の要求は CRLF にそろえて届く、chunk のデータの後ろのごみと
`+2` は 400、版の `http/1.1` (小文字) は 400、応答の token でない名前 (`X A: b`・`: v`) は落とす、
CONNECT の 502 に `Date` と `Content-Length: 0`、`connect` (小文字) はトンネルにしない。0.30.4 ではこの 7 行が外れる。
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
    """T27.5 〜 T27.8 の要求側。5 つ目は「届いたときに満たすべきこと」(届いた要求の辞書を受け取る)。"""
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
        # T28.3: 本文つきの 100-continue には先に 100 (最初の状態行)、Expect は転送しない。ほかの期待は 417
        ("Expect: 100-continue", post(b"Expect: 100-continue\r\nContent-Length: 5\r\n", b"hello"), 100, True,
         lambda seen: not any(h.lower().startswith("expect:") for h in seen["headers"])
         and seen["body"] == "hello"),
        ("Expect: fancy は 417", post(b"Expect: fancy\r\nContent-Length: 5\r\n", b"hello"), 417, False, None),
        # T29.8: 要求行はちょうど 3 要素・対象は VCHAR・版は HTTP/d.d、ヘッダー名は token
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
    """T32 の要求側 (形は `t27_cases` と同じ 5 つ組)。"""
    port = b"%d" % origin_port

    def get(host, target=b"/a"):
        return b"GET " + target + b" HTTP/1.1\r\nHost: " + host + b"\r\n\r\n"

    return [
        # T32.12: `Host` と authority の形 (reg-name の字・IP リテラル・ポートは 5 桁まで 65535 以下)
        ("Host: a b", get(b"a b"), 400, False, None),
        ("Host: a, b", get(b"a, b"), 400, False, None),
        ("Host: a/b", get(b"a/b"), 400, False, None),
        ("Host: h:99999", get(b"127.0.0.1:99999"), 400, False, None),
        ("authority のポートの +", get(b"127.0.0.1:" + port, b"http://127.0.0.1:+" + port + b"/a"), 400, False,
         None),
        ("Host の大文字 (通る)", get(b"LOCALHOST:" + port), 200, True, None),
        # T32.15: 版の `HTTP` は大小を区別する。chunk の 2 行は本文の途中で断るので最後に置く
        ("版の http/1.1 (小文字)", b"GET http://127.0.0.1:" + port + b"/a http/1.1\r\nHost: 127.0.0.1:" + port
         + b"\r\n\r\n", 400, False, None),
        # T32.15: LF だけの行末は CRLF にそろえて転送する (オリジンには X-A が 1 本の行として届く)
        ("LF だけの行末の要求", b"GET http://127.0.0.1:" + port + b"/lf HTTP/1.1\nHost: 127.0.0.1:" + port
         + b"\nX-A: 1\n\n", 200, True, lambda seen: "X-A: 1" in seen["headers"]),
        ("chunk のデータの後ろのごみ", b"POST http://127.0.0.1:" + port + b"/c HTTP/1.1\r\nHost: 127.0.0.1:" + port
         + b"\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nokXX\r\n0\r\n\r\n", 400, True, None),
        ("chunk の大きさ +2", b"POST http://127.0.0.1:" + port + b"/c HTTP/1.1\r\nHost: 127.0.0.1:" + port
         + b"\r\nTransfer-Encoding: chunked\r\n\r\n+2\r\nok\r\n0\r\n\r\n", 400, True, None),
    ]


def t32_response_cases(origin_port, closed_port, echo_port):
    """T32.15 の応答側 (形は `t27_response_cases` と同じ 4 つ組)。"""
    host = b"127.0.0.1:%d" % origin_port

    def no_bad_names(resp):
        head = resp.split(b"\r\n\r\n", 1)[0].split(b"\r\n")
        return b"X-Ok: y" in head and not any(l.startswith((b"X A", b":")) for l in head)

    def framed_502(resp):
        head = resp.split(b"\r\n\r\n", 1)[0].lower().split(b"\r\n")
        return any(l.startswith(b"date: ") for l in head) and b"content-length: 0" in head

    def not_a_tunnel(resp):
        return not resp.startswith(b"HTTP/1.1 200 ")

    connect = b"%d" % closed_port
    echo = b"127.0.0.1:%d" % echo_port
    return [
        ("応答の token でない名前", b"GET http://" + host + b"/resp-badname HTTP/1.1\r\nHost: " + host + b"\r\n\r\n",
         200, no_bad_names),
        ("CONNECT の 502 の Date と CL", b"CONNECT 127.0.0.1:" + connect + b" HTTP/1.1\r\nHost: 127.0.0.1:" + connect
         + b"\r\n\r\n", 502, framed_502),
        ("connect (小文字) はトンネルにしない", b"connect " + echo + b" HTTP/1.1\r\nHost: " + echo + b"\r\n\r\nping",
         502, not_a_tunnel),
    ]


def t27_response_cases(origin_port):
    """T27.7 の応答側。オリジンが壊れた値を返し、プロキシは SP に置き換えて 200 で返す。"""
    host = b"127.0.0.1:%d" % origin_port

    def get(path):
        return b"GET http://" + host + path + b" HTTP/1.1\r\nHost: " + host + b"\r\n\r\n"

    def clean(resp):
        head = resp.split(b"\r\n\r\n", 1)[0]
        return b"\x00" not in head and head.count(b"\r") == head.count(b"\r\n")

    def get10(path):
        return b"GET http://" + host + path + b" HTTP/1.0\r\nHost: " + host + b"\r\n\r\n"

    def until_close(resp):
        # T28.1: CL を捨てて閉じるまでを本文にする (CL は付かない)。
        # T28.35: TE の行はオリジンのまま残し、包み直さずに閉じて区切る
        head, _, body = resp.partition(b"\r\n\r\n")
        lines = head.lower().split(b"\r\n")
        return (b"content-length" not in head.lower() and b"transfer-encoding: gzip" in lines
                and b"connection: close" in lines and body == b"helloEXTRA")

    def no_te(resp):
        # T28.35: HTTP/1.0 へは TE を送れないので 502 (符号化済みのバイトを本文として渡さない)
        head, _, body = resp.partition(b"\r\n\r\n")
        return b"transfer-encoding" not in head.lower() and body == b""

    def chunked_wins(resp):
        # chunked と CL が両方なら今までどおり chunked (余分な "EXTRA" は本文に入らない)
        head, _, body = resp.partition(b"\r\n\r\n")
        return b"content-length" not in head.lower() and body == b"5\r\nhello\r\n0\r\n\r\n"

    def no_interim(resp):
        # T29.3: HTTP/1.0 へは 1xx を送らない (最初の状態行が最終応答)
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
    """CONNECT の入口 (T27.0)。`http` の経路を通らないので、同じ検査が効いているかを見る。"""
    authority = b"127.0.0.1:%d" % echo_port
    head = b"CONNECT " + authority + b" HTTP/1.1\r\n"
    return [
        ("CONNECT 正常", head + b"Host: " + authority + b"\r\n\r\n", 200, True),
        ("CONNECT + Host 2 本", head + b"Host: " + authority + b"\r\nHost: evil.example\r\n\r\n", 400, False),
        ("CONNECT + obs-fold", head + b"Host: " + authority + b"\r\nX-A: 1\r\n 2\r\n\r\n", 400, False),
        ("CONNECT + CL (本文 5 B)", head + b"Host: " + authority + b"\r\nContent-Length: 5\r\n\r\nhello", 400, False),
        # T27.5 と T27.8
        ("CONNECT + 名前とコロンの間の空白", head + b"Host : " + authority + b"\r\n\r\n", 400, False),
        ("CONNECT + 値の CR", head + b"Host: " + authority + b"\r\nX-A: a\rb\r\n\r\n", 400, False),
        ("CONNECT のポートなし", b"CONNECT 127.0.0.1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", 400, False),
        ("CONNECT のポート 0", b"CONNECT 127.0.0.1:0 HTTP/1.1\r\nHost: 127.0.0.1:0\r\n\r\n", 400, False),
    ]


class _Origin(socketserver.StreamRequestHandler):
    """受け取ったヘッダーと本文を覚えておくだけのオリジン。

    いま受けている本数を `server.active` に数える (T36.5。400 のあとに記録が済むのを待つため)。
    """

    def handle(self):
        with self.server.lock:
            self.server.active += 1
        try:
            self._handle()
        finally:
            with self.server.lock:
                self.server.active -= 1

    def _handle(self):
        try:
            raw = b""
            while b"\r\n\r\n" not in raw:
                chunk = self.rfile.read1(65536)
                if not chunk:
                    return
                raw += chunk
            head, _, rest = raw.partition(b"\r\n\r\n")
            lines = [l.decode("latin1") for l in head.split(b"\r\n")]
            # T27.7: 壊れた値を返すオリジン (プロキシが SP に置き換えるかを見る)
            if " /resp-cr " in lines[0] or " /resp-nul " in lines[0]:
                bad = b"X-A: a\rSet-Cookie: evil=1" if "/resp-cr" in lines[0] else b"X-A: a\x00b"
                self.wfile.write(b"HTTP/1.1 200 OK\r\n" + bad + b"\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                return
            # T32.15: 名前が token でない行を返すオリジン (プロキシが落とすかを見る)
            if " /resp-badname " in lines[0]:
                self.wfile.write(b"HTTP/1.1 200 OK\r\nX A: b\r\n: v\r\nX-Ok: y\r\nContent-Length: 2\r\n"
                                 b"Connection: close\r\n\r\nok")
                return
            # T29.3: 103 を返してから 200
            if " /resp-103-" in lines[0]:
                self.wfile.write(b"HTTP/1.1 103 Early Hints\r\nLink: </a.css>; rel=preload\r\n\r\n"
                                 b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                return
            # T28.1: 最後が chunked でない TE と CL を一緒に返し、CL の後ろにも続ける
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
    active: int


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


# ---------------------------------------------------------------- 判断 (純粋な関数。試験は test_framing_probe.py)

def judge(got, want, may_reach=True, reached=False, check=None, evidence=None):
    """1 本の判定: 状態が期待どおりで、届いてはいけない要求が届いておらず、`check` があれば
    `evidence` (届いた要求の辞書・応答のバイト列) が満たすこと (T34.136 で 4 つの節の式を 1 つにした)。

    `check` は状態が合ったときだけ呼ぶ。`evidence` が None (届かなかった) なら `check` は満たさない。
    """
    return (got == want and (may_reach or not reached)
            and (check is None or (evidence is not None and check(evidence))))


def row(name, got, want, shown, ok):
    """表の 1 行 (名前・状態・期待・見たもの、外れたら印)。`shown` は `%s` でそのまま出す。"""
    return "%-28s %-8s %-8s %s%s" % (name, got if got is not None else "(無応答)", want, shown,
                                     "" if ok else "   ← ちがう")


def heading(title, what):
    """表の見出しの 1 行。"""
    return "%-28s %-8s %-8s %s" % (title, "状態", "期待", what)


def summarize(failed):
    """最後の 2 行と終了コード (外れが 1 本でもあれば 1)。"""
    if failed:
        return ["", "期待とちがう: %d 本" % failed], 1
    return ["", "全部期待どおり。"], 0


# ---------------------------------------------------------------- 実機で流す (配線)

# 400 を受け取ったあと、オリジンの記録を待つ時間 (T36.5)。プロキシは本文の途中で断ると (壊れた chunk)、400 を
# 返してからオリジンへの接続を閉じる。オリジンはその閉じで本文を読み終えて記録するので、400 を受け取った直後に
# 記録を読むと、届いた要求の行が回ごとに入れ替わっていた (T34.136 で見つけた)。最初に `SETTLE_FIRST` 秒待ち
# (オリジンが受け付けるまで)、そのあと受けている本数が 0 になるまで最大 `SETTLE_LIMIT` 秒待つ
SETTLE_FIRST = 0.1
SETTLE_LIMIT = 2.0


def settle(origin, got, sleep=time.sleep, clock=time.monotonic):
    """`got` が 400 なら、オリジンが受けている要求を記録し終えるまで待つ (T36.5)。待った秒を返す。

    400 以外は待たない (200 はオリジンの応答を受け取った後なので記録は済んでいる。ほかの断りは
    オリジンへ流さない)。
    """
    if got != 400:
        return 0.0
    t0 = clock()
    sleep(SETTLE_FIRST)
    while origin.active and clock() - t0 < SETTLE_LIMIT:
        sleep(0.01)
    return clock() - t0


def probe_framing(port, origin, timeout, emit=print):
    """`cases` の節を流して 1 行ずつ `emit` し、外れの本数を返す。"""
    emit(heading("要求", "オリジンに届いたヘッダー"))
    failed = 0
    for name, raw, want, may_reach in cases(port + 1):
        before = len(origin.seen)
        got = _status(_send(port, raw, timeout))
        settle(origin, got)
        reached = len(origin.seen) > before
        headers = origin.seen[-1]["headers"] if reached else []
        ok = judge(got, want, may_reach, reached)
        failed += not ok
        shown = ", ".join(h for h in headers if h.lower().startswith(("host:", "content-length:", "x-a")))
        emit(row(name, got, want, shown or "-", ok))
    return failed


def probe_t27(port, origin, timeout, emit=print):
    """T27.5 〜 T27.8・T32 の要求側と応答側を流して 1 行ずつ `emit` し、外れの本数を返す。"""
    emit("")
    emit(heading("T27.5〜T27.8・T32", "オリジンに届いた要求行"))
    failed = 0
    # 5 つ目は届いた要求が満たすべきこと
    for name, raw, want, may_reach, check in t27_cases(port + 1) + t32_cases(port + 1):
        before = len(origin.seen)
        got = _status(_send(port, raw, timeout))
        settle(origin, got)
        reached = len(origin.seen) > before
        seen = origin.seen[-1] if reached else None
        ok = judge(got, want, may_reach, reached, check, seen)
        failed += not ok
        emit(row(name, got, want, seen["request_line"] if seen else "-", ok))
    for name, raw, want, check in t27_response_cases(port + 1) + t32_response_cases(port + 1, port + 3, port + 2):
        resp = _send(port, raw, timeout)
        got = _status(resp)
        ok = judge(got, want, check=check, evidence=resp)
        failed += not ok
        emit(row(name, got, want, "期待どおり" if ok else repr(resp[:60]), ok))
    return failed


def probe_connect(port, echo, timeout, emit=print):
    """CONNECT の節を流して 1 行ずつ `emit` し、外れの本数を返す (`http` の経路を通らないので別に見る。T19.8)。"""
    emit("")
    emit(heading("CONNECT", "トンネル先が受け取ったバイト"))
    failed = 0
    for name, raw, want, may_reach in connect_cases(port + 2):
        before = len(echo.seen)
        got = _status(_send(port, raw, timeout))
        seen_bytes = b"".join(echo.seen[before:])
        ok = judge(got, want, may_reach, bool(seen_bytes))
        failed += not ok
        emit(row(name, got, want, seen_bytes[:40] or "-", ok))
    return failed


def start_servers(port):
    """オリジン (port + 1) と CONNECT のトンネル先 (port + 2。受け取ったバイトをそのまま返す) を起こす。"""
    origin = _Threaded(("127.0.0.1", port + 1), _Origin)
    origin.seen = []  # type: ignore[attr-defined]
    origin.active = 0  # type: ignore[attr-defined]
    origin.lock = threading.Lock()  # type: ignore[attr-defined]
    threading.Thread(target=origin.serve_forever, daemon=True).start()
    echo = _Threaded(("127.0.0.1", port + 2), _Echo)
    echo.seen = []  # type: ignore[attr-defined]
    threading.Thread(target=echo.serve_forever, daemon=True).start()
    return origin, echo


def start_proxy(binary, port, work):
    """プロキシを起こす。キャッシュと状態ファイルはこの回だけの置き場 (`work`) に書く。

    既定の置き場 (root なら `/var/cache/rust-http-proxy`) を使うと、2 回目の実行が前の回の項目に HIT して外れる。
    """
    env = dict(
        os.environ,
        PROXY_ALLOW_LOCAL="1",
        PROXY_LOG_LEVEL="warn",
        PROXY_CACHE_DIR=os.path.join(work, "cache"),
        PROXY_STATS_PERSIST="off",
    )
    return subprocess.Popen([binary, "-p", str(port)], env=env, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)


def wait_listening(port):
    """プロキシが受け付けるまで待つ (最大 100 回 × 0.1 秒)。受け付けたら True。"""
    for _ in range(100):
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
            return True
        except OSError:
            time.sleep(0.1)
    return False


def main(argv=None):
    ap = argparse.ArgumentParser(description="要求の枠組みを実機で流して状態を並べる")
    ap.add_argument("binary", help="プロキシのバイナリ (例 ./target/release/rust-http-proxy)")
    ap.add_argument("--port", type=int, default=19098, help="プロキシの port (既定 19098)")
    ap.add_argument("--timeout", type=float, default=3.0, help="1 本あたりの待ち (秒)")
    args = ap.parse_args(argv)

    origin, echo = start_servers(args.port)
    work = tempfile.TemporaryDirectory(prefix="framing-probe-")
    proxy = start_proxy(args.binary, args.port, work.name)
    failed = 0
    try:
        if not wait_listening(args.port):
            print("プロキシが起動しない: %s -p %d" % (args.binary, args.port), file=sys.stderr)
            return 1
        # 節ごとに流して、1 本ごとに印字する (割る前と同じく、流したそばから出る)
        failed += probe_framing(args.port, origin, args.timeout)
        failed += probe_t27(args.port, origin, args.timeout)
        failed += probe_connect(args.port, echo, args.timeout)
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

    lines, code = summarize(failed)
    for line in lines:
        print(line)
    return code


if __name__ == "__main__":
    sys.exit(main())
