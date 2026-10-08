#!/usr/bin/env python3
"""`scripts/collect-deployed.sh` が `/profile?res=60` を `offset=` で追って 1 つに繋ぐ試験 (T17.0b) と、
雪像の隣に匿名化した写しを置く試験 (T17.16)、判定表の相手を `BEFORE` / `ZERO` で渡す試験 (T18.2)。

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
SNAPSHOT_B = os.path.join(HERE, "testdata", "snapshot-b.json")

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


def serve(profile, snapshot=SNAPSHOT):
    with open(snapshot, "rb") as f:
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


class CollectBeforeZeroTest(unittest.TestCase):
    """判定表の相手を `BEFORE`、0 時間の雪像を `ZERO` で渡す (T18.2)。

    既定の `CRITERIA` は phase20 (T20.1)。下のテストは `criteria="phase18"` を名指しして回す
    (`BEFORE` / `ZERO` の受け渡しは定義に依らない)。

    偽のサーバーが返す「いまの雪像」は `testdata/snapshot-b.json` (版 `0.1.0+bbbbbbb`、起動から 12 時間、
    `memory` は `heap_used` 6.0 + `mmap` 8.0 MB)。保存先には前回の雪像 (直前の 1 枚) を置いておく。
    """

    PREV = "2025-12-31T120000Z-snapshot.json"

    def setUp(self):
        with open(SNAPSHOT) as f:
            self.a = json.load(f)
        with open(SNAPSHOT_B) as f:
            self.b = json.load(f)

    def zero(self):
        """B と同じ起動の 2 分後の 1 枚 (`heap_used` 4.5 + `mmap` 8.0 MB)。"""
        up = 120
        return {"taken_at": self.b["taken_at"] - self.b["uptime_secs"] + up,
                "version": self.b["version"], "uptime_secs": up, "parts": ["status"], "dropped": [],
                "status": {"version": self.b["version"], "uptime_secs": up, "since_start_secs": up,
                           "memory": {"rss": 15_000_000, "heap_used": 4_500_000,
                                      "heap_free": 400_000, "mmap": 8_000_000},
                           "ipv6": {"attempts": 1}}}

    def run_collect(self, files=None, criteria="phase18", **env):
        srv = serve(FakeProfile(20), SNAPSHOT_B)
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        d = tempfile.mkdtemp(prefix="t182-")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", d]))
        files = dict(files or {})
        files.setdefault(self.PREV, self.a)
        for name, body in files.items():
            with open(os.path.join(d, name), "w") as f:
                json.dump(body, f)
        # 時刻は 00:00:00 にしない (いま撮る 1 枚が日次の雪像の名前になって、下の表に並んでしまう)
        e = dict(os.environ, PROBE="0", DASHBOARD="0", DIFF="1", ANON="0",
                 COLLECT_STAMP="2026-01-01T120000Z")
        for name in ("CRITERIA", "BEFORE", "ZERO", "AAAA"):
            e.pop(name, None)
        if criteria:
            e["CRITERIA"] = criteria
        e.update({k: v.replace("{dir}", d) for k, v in env.items()})
        r = subprocess.run([SCRIPT, "127.0.0.1:%d" % srv.server_address[1], d],
                           env=e, capture_output=True, text=True, timeout=120)
        self.assertEqual(r.returncode, 0, r.stderr)
        return d, r.stdout, r.stderr

    @staticmethod
    def judged(out):
        """要約の末尾の判定の節。"""
        return out[out.index("## 6. 完了の定義に対する判定"):]

    def test_the_default_criteria_is_phase20_against_the_previous_snapshot(self):
        d, out, _ = self.run_collect(criteria=None)
        tail = self.judged(out)
        self.assertIn("## 6. 完了の定義に対する判定 (snapshot-diff.py --criteria phase20)", tail)
        self.assertIn("phase18 と同じ並びの 10 行", tail)
        self.assertIn("(ほかに表示だけ 3 行。集計に数えない)", tail)
        self.assertEqual(tail.count("\n| "), 1 + 10)          # 表の頭 + 10 行
        # `BEFORE` も `ZERO` も無ければ今までどおり直前の 1 枚が相手で、(g) は判定できない
        self.assertNotIn("`BEFORE`", out)
        self.assertIn("`--zero` (0 時間の雪像) が渡されていない", tail)
        self.assertIn("### snapshot-diff: `%s` →" % os.path.join(d, self.PREV), out)

    def test_before_becomes_the_other_side_of_the_criteria_only(self):
        before = "2025-12-30T101010Z-snapshot.json"
        d, out, _ = self.run_collect(files={
            before: self.a,
            "2025-12-30T101010Z-profile_res_60.json": FakeProfile(50).page(0),
            # 直前の 1 枚の隣の `/profile?res=60` は、判定表には使わない
            "2025-12-31T120000Z-profile_res_60.json": FakeProfile(100).page(0),
        }, BEFORE="{dir}/" + before)
        path = os.path.join(d, before)
        self.assertIn("- 前回: `%s`" % os.path.join(d, self.PREV), out)
        self.assertIn("- 判定表の相手 (`BEFORE`): `%s`" % path, out)
        # 「2. 前回との差分」の相手は直前の 1 枚のまま。判定表はそこには出ない
        head = out[:out.index("## 3. ホスト別")]
        self.assertIn("### snapshot-diff: `%s` →" % os.path.join(d, self.PREV), head)
        self.assertNotIn("完了の定義に対する判定", head)
        tail = self.judged(out)
        self.assertIn("相手は `BEFORE` の雪像 `%s` です" % path, tail)
        self.assertIn("後: `--profile` 20 標本", tail)
        self.assertIn("前: `--profile` 50 標本", tail)       # `BEFORE` の隣のもの
        self.assertNotIn("100 標本", tail)
        self.assertEqual(tail.count("\n| "), 1 + 10)

    def test_before_without_a_profile_next_to_it(self):
        before = "2025-12-30T101010Z-snapshot.json"
        _, out, _ = self.run_collect(files={before: self.a}, BEFORE="{dir}/" + before)
        tail = self.judged(out)
        self.assertIn("後: `--profile` 20 標本", tail)
        self.assertNotIn("前: `--profile`", tail)

    def test_zero_and_the_daily_snapshots_are_passed_under_phase20_too(self):
        """既定 (phase20) でも `ZERO` は (g) の行に届き、日次の雪像の表が付く。"""
        zero = self.zero()
        zero["status"]["memory"]["cache_memory"] = 0
        daily = self.zero()
        daily.update(uptime_secs=30_000, taken_at=daily["taken_at"] - 120 + 30_000)
        _, out, _ = self.run_collect(files={"2025-12-31T060000Z-snapshot.json": zero,
                                            "2025-12-30T000000Z-snapshot.json": daily},
                                     criteria=None, ZERO="{dir}/2025-12-31T060000Z-snapshot.json")
        tail = self.judged(out)
        self.assertIn("`heap_used + mmap − cache_memory` の 0 時間の雪像からの増えが 5 MB 未満", tail)
        self.assertIn("**+1.5** MB (12.5 → 14.0 MB)", tail)
        self.assertIn("`cache_memory` 0.0 → 0.0", tail)
        self.assertIn("**日次の雪像**", tail)
        # 「2. 前回との差分」の表には成功した接続だけの行が出る (この雪像に `/recent` の部は無い)
        self.assertIn("| CONNECT 確立 (**成功した接続だけ**) |", out[:out.index("## 3. ホスト別")])

    def test_zero_is_passed_to_the_heap_row(self):
        _, out, _ = self.run_collect(files={"2025-12-31T060000Z-snapshot.json": self.zero()},
                                     ZERO="{dir}/2025-12-31T060000Z-snapshot.json")
        tail = self.judged(out)
        self.assertIn("- 0 時間の雪像 (`ZERO`): ", out)
        self.assertIn("**+1.5** MB (12.5 → 14.0 MB)", tail)
        self.assertIn("`--zero` (起動から 2.0 分)", tail)

    def test_before_and_zero_together(self):
        before = "2025-12-30T101010Z-snapshot.json"
        _, out, _ = self.run_collect(files={before: self.a,
                                            "2025-12-31T060000Z-snapshot.json": self.zero()},
                                     BEFORE="{dir}/" + before,
                                     ZERO="{dir}/2025-12-31T060000Z-snapshot.json")
        tail = self.judged(out)
        self.assertIn("相手は `BEFORE` の雪像", tail)
        self.assertIn("**+1.5** MB (12.5 → 14.0 MB)", tail)

    def test_the_daily_snapshots_of_the_directory_are_listed_under_phase18(self):
        daily = self.zero()
        daily.update(uptime_secs=30_000, taken_at=daily["taken_at"] - 120 + 30_000)
        files = {"2025-12-30T000000Z-snapshot.json": daily}
        _, out, _ = self.run_collect(files=files)
        tail = self.judged(out)
        self.assertIn("**日次の雪像**", tail)
        self.assertIn("| `2025-12-30T000000Z-snapshot.json` | 8.3 | 15.0 | 4.5 | 0.4 | 8.0 | — | 1 |",
                      tail)
        # 古い定義の判定表には足さない (今までと同じ命令で出す)
        _, old, _ = self.run_collect(files=files, criteria="phase17")
        self.assertIn("(snapshot-diff.py --criteria phase17)", old)
        self.assertNotIn("**日次の雪像**", old)
        self.assertNotIn("`--zero`", old)

    def test_a_missing_before_or_zero_falls_back_with_a_note(self):
        d, out, err = self.run_collect(BEFORE="{dir}/nowhere-snapshot.json",
                                       ZERO="{dir}/nowhere-zero.json")
        self.assertIn("- **`BEFORE` の雪像が無い**", out)
        self.assertIn("- **`ZERO` の雪像が無い**", out)
        self.assertIn("is not a file", err)
        tail = self.judged(out)
        self.assertNotIn("相手は `BEFORE` の雪像", tail)
        self.assertEqual(tail.count("\n| "), 1 + 10)        # 直前の 1 枚との 10 行は出る
        self.assertIn("`--zero` (0 時間の雪像) が渡されていない", tail)

    def test_criteria_off_and_diff_0_still_suppress_the_table(self):
        before = "2025-12-30T101010Z-snapshot.json"
        _, off, _ = self.run_collect(files={before: self.a}, criteria="off",
                                     BEFORE="{dir}/" + before)
        self.assertIn("(CRITERIA=off なので出していない)", off)
        _, nodiff, _ = self.run_collect(files={before: self.a}, BEFORE="{dir}/" + before, DIFF="0")
        self.assertIn("(前回の雪像が無いか DIFF=0 なので判定できない)", nodiff)



class CollectAnonTest(unittest.TestCase):
    """雪像の隣に匿名化した写し (`-snapshot.anon.json` と隣の口の `.anon.json`) を置く (T17.16)。"""

    def run_collect(self, **env):
        srv = serve(FakeProfile(10))
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        d = tempfile.mkdtemp(prefix="t1716-")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", d]))
        e = dict(os.environ, PROBE="0", DASHBOARD="0", DIFF="0", CRITERIA="off",
                 COLLECT_STAMP="2026-01-01T000000Z")
        e.update(env)
        r = subprocess.run([SCRIPT, "127.0.0.1:%d" % srv.server_address[1], d],
                           env=e, capture_output=True, text=True, timeout=120)
        self.assertEqual(r.returncode, 0, r.stderr)
        return d, r.stdout

    def test_the_anonymized_copies_are_next_to_the_snapshot(self):
        d, out = self.run_collect()
        stamp = os.path.join(d, "2026-01-01T000000Z-")
        with open(stamp + "snapshot.anon.json") as f:
            text = f.read()
        snap = json.loads(text)
        with open(SNAPSHOT) as f:
            raw = json.load(f)
        # 数字と部はそのまま、宛先と接続元だけが置き換わる
        self.assertEqual(snap["parts"], raw["parts"])
        self.assertEqual(snap["uptime_secs"], raw["uptime_secs"])
        for h in raw["hosts"]["hosts"]:
            name = h["host"].split("://")[-1].rsplit(":", 1)[0]
            if name != "other":
                self.assertNotIn(name, text)
        self.assertRegex(snap["hosts"]["hosts"][0]["host"], r"host-\d{4}\.g\d{4}\.example")
        # 隣の口 (`/daily` と繋いだ `/profile?res=60`) も同じ表で
        for name in ("daily", "profile_res_60"):
            with open(stamp + name + ".json") as f, open(stamp + name + ".anon.json") as g:
                self.assertEqual(json.load(f), json.load(g))
        self.assertIn("- 匿名化した写し: `%ssnapshot.anon.json` (隣の口 2 つも同じ表で)" % stamp, out)

    def test_the_anonymized_copy_is_not_the_previous_snapshot(self):
        # 次の回の「前回」は生の雪像 (`-snapshot.json`) で、`.anon.json` は拾わない
        d, _ = self.run_collect()
        e = dict(os.environ, PROBE="0", DASHBOARD="0", DIFF="0", CRITERIA="off",
                 COLLECT_STAMP="2026-01-02T000000Z")
        srv = serve(FakeProfile(10))
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        r = subprocess.run([SCRIPT, "127.0.0.1:%d" % srv.server_address[1], d],
                           env=e, capture_output=True, text=True, timeout=120)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("- 前回: `%s`" % os.path.join(d, "2026-01-01T000000Z-snapshot.json"), r.stdout)

    def test_anon_0_skips_it(self):
        d, out = self.run_collect(ANON="0")
        self.assertFalse([n for n in os.listdir(d) if n.endswith(".anon.json")])
        self.assertNotIn("匿名化した写し", out)


if __name__ == "__main__":
    unittest.main()
