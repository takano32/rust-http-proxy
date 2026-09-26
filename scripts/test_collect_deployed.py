#!/usr/bin/env python3
"""`scripts/collect-deployed.sh` が `/profile?res=60` を `offset=` で追って 1 つに繋ぐ試験 (T17.0b)。

    python3 -m unittest discover -s scripts        # リポジトリの根から

手元のプロキシは標本を 60 秒に 1 本しか作らず、環も保存されないので、起動して数分では
1 枚 (256 KiB) に収まってしまい、続きを追う枝を通らない。そこで**プロキシの頁の切り方
(`Profile::rows_within_page` と `next_offset`) を写した偽のサーバー**を 127.0.0.1 に立てて回す。
標本の中身は架空 (時刻と詰め物だけ)。デプロイ先には行かない。
"""

import json
import os
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "collect-deployed.sh")
SNAPSHOT = os.path.join(HERE, "testdata", "snapshot-a.json")

KEYS = ["t", "requests", "cpu_us", "threads", "pad"]

# プロキシの `MAX_BODY` (256 KiB) − `HEADER_ROOM` (4 KiB)
BUDGET = 256 * 1024 - 4096


class FakeProfile:
    """`/profile?res=60` の頁の切り方だけを写したもの。"""

    def __init__(self, count, pad=800):
        # 1 標本 ≈ 850 B (T16.99 の実測: 256 KiB に 308 標本)
        # 並びは `keys` のとおり: 時刻・要求・プロセスの CPU (us)・役割ごとの `[cpu_us, 標本, 状態]`・詰め物
        self.samples = [[1_000_000 + 60 * i, 10, 2000, [0, [500, 60, 0]], "x" * pad]
                        for i in range(count)]
        self.busy_once = set()  # この offset の 1 回目は `busy` で断る
        self.grow_after_first = False  # 1 枚目のあとに新しい標本を 1 本足す (頁の間の重なり)

    def page(self, offset):
        total = len(self.samples)
        rows, used, cut = [], 0, False
        for s in reversed(self.samples[: max(0, total - offset)]):
            row = json.dumps(s, separators=(",", ":"))
            if used + len(row) + 1 > BUDGET:
                cut = True
                break
            used += len(row) + 1
            rows.append(s)
        rows.reverse()
        shown = len(rows)
        nxt = offset + shown if offset + shown < total else None
        return {
            "schema": 1,
            "interval_secs": 60,
            "roles": ["accept", "conn"],
            "keys": KEYS,
            "samples": rows,
            "count": total,
            "shown": shown,
            "truncated": cut,
            "n": 1440,
            "offset": offset,
            "next_offset": nxt,
        }


def serve(profile):
    with open(SNAPSHOT, "rb") as f:
        snapshot = f.read()

    class H(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def reply(self, code, body):
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            u = urlparse(self.path)
            if u.path == "/snapshot":
                return self.reply(200, snapshot)
            if u.path == "/profile":
                off = int(parse_qs(u.query).get("offset", ["0"])[0])
                if off in profile.busy_once:
                    profile.busy_once.discard(off)
                    return self.reply(503, b'{"schema":1,"error":"busy"}')
                body = json.dumps(profile.page(off)).encode()
                if off == 0 and profile.grow_after_first:
                    t = profile.samples[-1][0] + 60
                    profile.samples.append([t, 10, 2000, [0, [500, 60, 0]], "y" * 800])
                    profile.grow_after_first = False
                return self.reply(200, body)
            return self.reply(200, b'{"schema":1}')

    srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


class CollectProfileTest(unittest.TestCase):
    def run_collect(self, profile, prev=None, **env):
        srv = serve(profile)
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        d = tempfile.mkdtemp(prefix="t170b-")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", d]))
        # 前回の雪像とその隣の口 (`{名前: 中身}`) を先に置いておく
        for name, body in (prev or {}).items():
            with open(os.path.join(d, name), "w") as f:
                json.dump(body, f)
        e = dict(os.environ, PROBE="0", DASHBOARD="0", DIFF="0", CRITERIA="off",
                 COLLECT_STAMP="2026-01-01T000000Z")
        e.update(env)
        r = subprocess.run(
            [SCRIPT, "127.0.0.1:%d" % srv.server_address[1], d],
            env=e, capture_output=True, text=True, timeout=120,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        with open(os.path.join(d, "2026-01-01T000000Z-profile_res_60.json")) as f:
            return json.load(f), r.stdout

    def test_joins_all_pages(self):
        # 24 時間ぶん (1,440 標本) は 1 枚に約 300 本しか入らない → 5 枚を 1 つに
        p = FakeProfile(1440)
        self.assertTrue(p.page(0)["truncated"])
        got, out = self.run_collect(p)
        ts = [s[0] for s in got["samples"]]
        self.assertEqual(ts, [s[0] for s in p.samples])  # 古い順・欠けも重なりも無し
        self.assertFalse(got["truncated"])
        self.assertIsNone(got["next_offset"])
        self.assertEqual(got["shown"], 1440)
        self.assertEqual(got["keys"], KEYS)
        self.assertEqual(got["roles"], ["accept", "conn"])
        self.assertIn("5 枚を繋いだ (1440 / 1440 標本、truncated=false)", out)

    def test_single_page_untouched(self):
        # 1 枚に収まるときは取ったまま (追わない・書き直さない)
        p = FakeProfile(10)
        got, out = self.run_collect(p)
        self.assertEqual(got, p.page(0))
        self.assertNotIn("枚を繋いだ", out)

    def test_busy_retry_and_overlap(self):
        # 2 枚目の 1 回目は `busy` (503) → 1 秒おいて引き直す。1 枚目のあとに標本が 1 本
        # 増えると境目の 1 本が 2 枚に出るので `t` で落とす
        p = FakeProfile(700)
        first = p.page(0)
        p.busy_once.add(first["next_offset"])
        p.grow_after_first = True
        got, _ = self.run_collect(p)
        ts = [s[0] for s in got["samples"]]
        self.assertEqual(len(ts), len(set(ts)))
        self.assertEqual(ts, sorted(ts))
        self.assertEqual(ts[0], p.samples[0][0])
        self.assertEqual(ts[-1], first["samples"][-1][0])
        self.assertFalse(got["truncated"])

    def test_max_pages_leaves_rest(self):
        # `MAX_PAGES` (1 枚目を含む) で止めたら、残りを指したまま `truncated` を立てておく
        p = FakeProfile(1440)
        got, out = self.run_collect(p, MAX_PAGES="2")
        self.assertTrue(got["truncated"])
        self.assertIsNotNone(got["next_offset"])
        self.assertEqual(got["shown"], got["next_offset"])
        self.assertEqual(got["samples"][-1][0], p.samples[-1][0])
        self.assertIn("2 枚を繋いだ", out)

    def test_profile_passed_to_diff(self):
        # 繋いだ `-profile_res_60.json` は `snapshot-diff.py --profile` に、前回の雪像の隣の
        # 同じ時刻の `-profile_res_60.json` は `--profile-before` に渡る (phase17 の conn 役の行)
        p = FakeProfile(1440)
        before = FakeProfile(100).page(0)
        with open(SNAPSHOT) as f:
            snap = json.load(f)
        _, out = self.run_collect(p, prev={
            "2025-12-31T000000Z-snapshot.json": snap,
            "2025-12-31T000000Z-profile_res_60.json": before,
        }, DIFF="1", CRITERIA="phase17")
        self.assertIn("後: `--profile` 1440 標本 (24.0 時間", out)
        self.assertIn("前: `--profile` 100 標本", out)

    def test_profile_before_missing(self):
        # 前回の隣に `-profile_res_60.json` が無ければ `--profile-before` は渡さない
        p = FakeProfile(20)
        with open(SNAPSHOT) as f:
            snap = json.load(f)
        _, out = self.run_collect(p, prev={"2025-12-31T000000Z-snapshot.json": snap},
                                  DIFF="1", CRITERIA="phase17")
        self.assertIn("後: `--profile` 20 標本", out)
        self.assertNotIn("前: `--profile`", out)


if __name__ == "__main__":
    unittest.main()
