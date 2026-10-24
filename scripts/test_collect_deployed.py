#!/usr/bin/env python3
"""`scripts/collect-deployed.sh` が `/profile?res=60` を `offset=` で追って 1 つに繋ぐ試験 (T17.0b) と、
雪像の隣に匿名化した写しを置く試験 (T.17.16)、判定表の相手を `BEFORE` / `ZERO` で渡す試験 (T.18.2)、
雪像の切れた `recent`・`hosts` を既定で追って雪像に繋ぐ試験 (T.22.5)。

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
        # 1 標本 ≈ 850 B (T.16.99 の実測: 256 KiB に 308 標本)
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
    """判定表の相手を `BEFORE`、0 時間の雪像を `ZERO` で渡す (T.18.2)。

    既定の `CRITERIA` は phase25 (T.27.0)。下のテストは `criteria="phase18"` を名指しして回す
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

    def test_the_default_criteria_is_phase25_against_the_previous_snapshot(self):
        d, out, _ = self.run_collect(criteria=None)
        tail = self.judged(out)
        self.assertIn("## 6. 完了の定義に対する判定 (snapshot-diff.py --criteria phase25)", tail)
        self.assertIn("17 行の並びと材料は phase24 と同じです", tail)
        # 前回の雪像に `/profile` も `/recent` も無いので conn 役と接続のエラーの行は「判定できず」で、
        # 参考の行は無い (表示だけは phase21 の 4 行 + `/events` の行 + phase22 の接続のエラーの行)
        self.assertIn("(ほかに表示だけ 6 行。集計に数えない)", tail)
        self.assertEqual(tail.count("\n| "), 1 + 17)          # 表の頭 + 17 行 (T.24.3 で 1 行増えた)
        # `baseline` も `boot` も無い雪像なので、heap の行は phase23 の読み方に戻る
        self.assertIn("`heap_used + mmap − cache_memory` の 0 時間の雪像からの増えが 5 MB 未満", tail)
        self.assertIn("こちらが原因の無言の再起動", tail)
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
        """phase20・phase21・phase24・phase25 と既定 (phase25) でも `ZERO` は (g) の行に届き、日次の雪像の表が付く。"""
        for criteria in ("phase20", "phase21", "phase24", "phase25", None):
            with self.subTest(criteria=criteria):
                self.zero_and_daily(criteria)

    def zero_and_daily(self, criteria):
        zero = self.zero()
        zero["status"]["memory"]["cache_memory"] = 0
        daily = self.zero()
        daily.update(uptime_secs=30_000, taken_at=daily["taken_at"] - 120 + 30_000)
        _, out, _ = self.run_collect(files={"2025-12-31T060000Z-snapshot.json": zero,
                                            "2025-12-30T000000Z-snapshot.json": daily},
                                     criteria=criteria, ZERO="{dir}/2025-12-31T060000Z-snapshot.json")
        tail = self.judged(out)
        self.assertIn("(snapshot-diff.py --criteria %s)" % (criteria or "phase25"), tail)
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
    """雪像の隣に匿名化した写し (`-snapshot.anon.json` と隣の口の `.anon.json`) を置く (T.17.16)。"""

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


class FakeRows:
    """`/recent?n=2000&offset=` と `/hosts?limit=1000&offset=` の頁の切り方だけを写したもの (T.22.5)。

    `rows` は**応答の並び** (`recent` は閉じた新しい順、`hosts` は要求数の順)。頁は
    `array_within` と同じく 1 行ずつ足して予算を越える手前で切り、`next_offset` は
    `offset + shown` が全件に届いていなければその値。行の中身は架空 (TEST-NET と example)。
    """

    def __init__(self, kind, count, pad=400):
        self.kind = kind
        self.pad = pad
        self.next_id = count
        if kind == "recent":
            self.rows = [self.recent_row(i) for i in reversed(range(count))]
        else:
            with open(SNAPSHOT) as f:
                self.host_base = json.load(f)["hosts"]["hosts"][0]
            self.rows = [self.host_row(i, count - i) for i in range(count)]
        self.busy_once = set()   # この offset の 1 回目は `busy` で断る
        self.grow_after_first = False  # 雪像を返したあとに 1 行足す (頁の間の重なり)
        self.asked = []          # 引かれた offset (続きの頁だけ)

    def recent_row(self, i):
        return {"id": i, "at": 1_789_000_000 + i, "secs": 1, "client": "192.0.2.%d" % (i % 200 + 1),
                "target": "host-%04d.example:443" % (i % 50), "kind": "connect",
                "reason": "client_eof", "up": 100, "down": 1000, "ms": {"connect": 7},
                "pad": "x" * self.pad}

    def host_row(self, i, requests):
        h = dict(self.host_base, host="connect://host-%04d.example:443" % i, requests=requests,
                 pad="x" * self.pad)
        return h

    def grow(self):
        if self.kind == "recent":
            self.rows.insert(0, self.recent_row(self.next_id))
        else:
            self.rows.insert(0, self.host_row(self.next_id, 10**6))
        self.next_id += 1

    def page(self, offset, limit=2000):
        rows, used, cut = [], 0, False
        for r in self.rows[offset:offset + limit]:
            n = len(json.dumps(r, separators=(",", ":"))) + 1
            if used + n > BUDGET:
                cut = True
                break
            used += n
            rows.append(r)
        total = len(self.rows)
        shown = len(rows)
        nxt = offset + shown if offset + shown < total else None
        if self.kind == "recent":
            return {"schema": 1, "recent": rows, "count": shown, "matched": total, "shown": shown,
                    "kept": total, "capacity": 2000, "recorded": total, "sort": "time", "since": 0,
                    "client": "", "persisted": False, "restored": 0, "truncated": cut,
                    "lite": False, "offset": offset, "next_offset": nxt}
        return {"schema": 1, "hosts": rows, "count": total, "shown": shown, "sort": "requests",
                "limit": limit, "truncated": cut, "uptime_secs": 200000, "total_requests": 5000,
                "restored_since": 1788700000, "offset": offset, "next_offset": nxt}


def serve_follow(recent, hosts):
    """雪像の `recent`・`hosts` を上の偽の 1 枚目にし、`/recent` `/hosts` で続きを返す。"""
    with open(SNAPSHOT) as f:
        base = json.load(f)
    profile = FakeProfile(10)

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
            q = parse_qs(u.query)
            if u.path == "/snapshot":
                snap = dict(base, recent=recent.page(0), hosts=hosts.page(0, 1000))
                snap["parts"] = list(base["parts"]) + ["recent"]
                for fake in (recent, hosts):
                    if fake.grow_after_first:
                        fake.grow()
                        fake.grow_after_first = False
                return self.reply(200, json.dumps(snap).encode())
            fake = {"/recent": recent, "/hosts": hosts}.get(u.path)
            if fake is not None:
                off = int(q.get("offset", ["0"])[0])
                fake.asked.append(off)
                if off in fake.busy_once:
                    fake.busy_once.discard(off)
                    return self.reply(503, b'{"schema":1,"error":"busy"}')
                limit = int(q.get("n", q.get("limit", ["2000"]))[0])
                return self.reply(200, json.dumps(fake.page(off, limit)).encode())
            if u.path == "/profile":
                return self.reply(200, json.dumps(profile.page(0)).encode())
            return self.reply(200, b'{"schema":1}')

    srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


class CollectFollowTest(unittest.TestCase):
    """切れた `recent`・`hosts` を既定で `offset=` で追い、雪像の同じ部に繋ぐ (T.22.5)。

    偽の `recent` は 2,000 件 (リングの `MAX_RECENT`) で 1 枚に約 530 件、`hosts` は
    1,001 件 (`limit=1000` でも `offset=` で全部届く) で 1 枚に約 380 件。
    """

    STAMP = "2026-01-01T000000Z"

    def run_collect(self, recent=None, hosts=None, args=(), **env):
        recent = recent or FakeRows("recent", 2000)
        hosts = hosts or FakeRows("hosts", 1001)
        self.recent, self.hosts = recent, hosts
        self.first = (len(recent.page(0)["recent"]), len(hosts.page(0, 1000)["hosts"]))
        srv = serve_follow(recent, hosts)
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        d = tempfile.mkdtemp(prefix="t225-")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", d]))
        # 偽のホスト名 1,001 件を `status-diff.py` に引かせない (AAAA の表を空で固定する)
        aaaa = os.path.join(tempfile.mkdtemp(prefix="t225-aaaa-"), "aaaa.json")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", os.path.dirname(aaaa)]))
        with open(aaaa, "w") as f:
            f.write("{}")
        e = dict(os.environ, PROBE="0", DASHBOARD="0", DIFF="0", CRITERIA="off",
                 COLLECT_STAMP=self.STAMP, AAAA=aaaa)
        for name in ("FULL", "MAX_PAGES", "FOLLOW_SECS"):
            e.pop(name, None)
        e.update(env)
        r = subprocess.run([SCRIPT, *args, "127.0.0.1:%d" % srv.server_address[1], d],
                           env=e, capture_output=True, text=True, timeout=120)
        self.assertEqual(r.returncode, 0, r.stderr)
        with open(os.path.join(d, self.STAMP + "-snapshot.json")) as f:
            return d, r.stdout, json.load(f)

    def test_recent_and_hosts_are_joined_into_the_snapshot_by_default(self):
        self.assertTrue(FakeRows("recent", 2000).page(0)["truncated"])
        d, out, snap = self.run_collect()
        rec = snap["recent"]
        ids = [r["id"] for r in rec["recent"]]
        self.assertEqual(ids, list(reversed(range(2000))))   # 新しい順・欠けも重なりも無し
        self.assertFalse(rec["truncated"])
        self.assertIsNone(rec["next_offset"])
        self.assertEqual((rec["count"], rec["shown"], rec["matched"]), (2000, 2000, 2000))
        hs = snap["hosts"]
        self.assertEqual([h["host"] for h in hs["hosts"]], [h["host"] for h in self.hosts.rows])
        self.assertEqual((hs["count"], hs["shown"], hs["truncated"]), (1001, 1001, False))
        # 形は `/snapshot` のまま (部の鍵の順も 1 枚目と同じ)
        self.assertEqual(list(rec), list(self.recent.page(0)))
        self.assertEqual(list(hs), list(self.hosts.page(0, 1000)))
        self.assertIn("recent", snap["parts"])
        pages = -(-2000 // self.first[0])
        self.assertIn("- 切れた部の続き (既定で追う。T.22.5): `recent` %d 枚で %s → 2,000 件 (全 2,000 件)"
                      % (pages, format(self.first[0], ",")), out)
        self.assertIn("`hosts` 3 枚で %d → 1,001 件 (全 1,001 件)" % self.first[1], out)
        self.assertRegex(out, r"\(続き \d+\.\d 秒。`/snapshot` 自体は \d+\.\d 秒\)")
        self.assertNotIn("打ち切った", out)
        # 要点はそのまま繋いだ 2,000 本を読む (切れている、とは言わない)
        self.assertIn("覚えている 2000 本\n", out)
        # 別ファイルには落とさない (それは `--full` の仕事)
        self.assertFalse([n for n in os.listdir(d) if "-page" in n])

    def test_the_anonymized_copy_has_the_joined_rows(self):
        d, _, _ = self.run_collect()
        with open(os.path.join(d, self.STAMP + "-snapshot.anon.json")) as f:
            anon = json.load(f)
        self.assertEqual(len(anon["recent"]["recent"]), 2000)
        self.assertEqual(len(anon["hosts"]["hosts"]), 1001)

    def test_full_0_and_no_follow_keep_the_old_default(self):
        for args, env in (((), {"FULL": "0"}), (("--no-follow",), {})):
            with self.subTest(args=args, env=env):
                _, out, snap = self.run_collect(args=args, **env)
                self.assertEqual(snap["recent"], self.recent.page(0))
                self.assertEqual(snap["hosts"], self.hosts.page(0, 1000))
                self.assertEqual(self.recent.asked, [])
                self.assertIn("- 切れた部の続き: 追っていない (`FULL=0` か `--no-follow`)", out)
                self.assertIn("(**応答は 256 KiB で切れている**)", out)

    def test_overlap_between_pages_is_dropped_by_id(self):
        # 雪像を返したあとに接続が 1 本閉じると並びがずれ、境目の 1 行が 2 枚に出る
        recent = FakeRows("recent", 2000)
        recent.grow_after_first = True
        _, out, snap = self.run_collect(recent=recent)
        ids = [r["id"] for r in snap["recent"]["recent"]]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertEqual(sorted(ids), list(range(2000)))     # 新しく閉じた 1 本は入らない
        self.assertIn("頁の重なり 1 件を落とした", out)
        self.assertFalse(snap["recent"]["truncated"])

    def test_max_pages_stops_and_says_so(self):
        _, out, snap = self.run_collect(MAX_PAGES="2")
        rec = snap["recent"]
        self.assertTrue(rec["truncated"])
        self.assertEqual(rec["next_offset"], len(rec["recent"]))  # 残りを指したまま
        self.assertEqual(rec["shown"], len(rec["recent"]))
        self.assertIn("`recent` 2 枚で", out)
        self.assertIn("**打ち切った**: `MAX_PAGES=2` で止めた (残りは offset=%d から)"
                      % rec["next_offset"], out)

    def test_follow_secs_stops_before_the_next_page(self):
        _, out, snap = self.run_collect(FOLLOW_SECS="0")
        self.assertEqual(snap["recent"]["recent"], self.recent.page(0)["recent"])
        self.assertTrue(snap["recent"]["truncated"])
        self.assertEqual(self.recent.asked, [])
        self.assertIn("**打ち切った**: 続きを引く時間が `FOLLOW_SECS=0` 秒を越えた", out)

    def test_busy_is_retried(self):
        recent = FakeRows("recent", 2000)
        second = recent.page(0)["next_offset"]
        recent.busy_once.add(second)
        _, out, snap = self.run_collect(recent=recent)
        self.assertEqual(recent.asked.count(second), 2)
        self.assertEqual(len(snap["recent"]["recent"]), 2000)
        self.assertNotIn("打ち切った", out)

    def test_a_short_snapshot_is_left_alone(self):
        recent, hosts = FakeRows("recent", 20), FakeRows("hosts", 5)
        _, out, snap = self.run_collect(recent=recent, hosts=hosts)
        self.assertEqual(snap["recent"], recent.page(0))
        self.assertEqual(recent.asked + hosts.asked, [])
        self.assertNotIn("切れた部の続き", out)

    def test_full_writes_only_what_is_still_cut(self):
        # 既定で繋いだあとの `--full` は、まだ切れている部だけを別ファイルに落とす
        d, out, _ = self.run_collect(args=("--full",), MAX_PAGES="2")
        pages = sorted(n for n in os.listdir(d) if "-page" in n)
        self.assertIn(self.STAMP + "-page2-recent.json", pages)
        self.assertIn(self.STAMP + "-page2-hosts.json", pages)
        # 繋いだ 2 枚の続き (3 枚目から) を引く
        for part, first in (("recent", self.first[0]), ("hosts", self.first[1])):
            with open(os.path.join(d, "%s-page2-%s.json" % (self.STAMP, part))) as f:
                self.assertEqual(json.load(f)["offset"], 2 * first)
        self.assertIn("**打ち切った**: `MAX_PAGES=2`", out)
        # 上限を越えなければ `--full` で別ファイルに落ちる `recent`・`hosts` は無い
        d, _, _ = self.run_collect(args=("--full",))
        self.assertFalse([n for n in os.listdir(d) if "-page" in n])
        # `--full --no-follow` は T.22.5 より前の `--full` (2 つとも別ファイル)
        d, _, snap = self.run_collect(args=("--full", "--no-follow"))
        names = os.listdir(d)
        self.assertIn(self.STAMP + "-page2-recent.json", names)
        self.assertIn(self.STAMP + "-page2-hosts.json", names)
        self.assertTrue(snap["recent"]["truncated"])


if __name__ == "__main__":
    unittest.main()
