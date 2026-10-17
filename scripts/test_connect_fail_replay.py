#!/usr/bin/env python3
"""`scripts/connect-fail-replay.py` (T22.3 段 1) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から

入力はその場で組んだ**架空の**雪像 (名前は `*.example.invalid`、接続元は 192.0.2.0/24)。
**本物の雪像 (利用者の閲覧先が並ぶ) は入れない**。
"""

import contextlib
import importlib.util
import io
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def _load(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


cf = _load("connect_fail_replay", "connect-fail-replay.py")

START = 1_000_000
A = "a.example.invalid:443"
B = "b.example.invalid:443"


def fail_line(at, target, why="connection timed out"):
    return {"at": at, "level": "warn", "conn": at,
            "msg": f"502 Bad Gateway: connect {target} failed: {why}"}


def err_row(target, cause, at, last_at, repeats, connect_ms=10000, kind="connect"):
    return {"at": at, "kind": kind, "target": target, "cause": cause, "dns_ms": 0,
            "connect_ms": connect_ms, "status": 502, "client": "192.0.2.1",
            "repeats": repeats, "last_at": last_at}


def ok_rec(i, at, target):
    return {"id": i, "at": at, "target": target, "kind": "connect", "reason": "server_eof", "status": 0}


def snap(fail_times, target=A, cause="timeout", why="connection timed out", connect_ms=10000,
         recent=(), hosts=None, hosts_truncated=False, series=None, uptime=100_000, extra_lines=()):
    taken = START + uptime
    lines = [fail_line(t, target, why) for t in fail_times] + list(extra_lines)
    lines.sort(key=lambda x: -x["at"])
    rows = [err_row(target, cause, min(fail_times), max(fail_times), len(fail_times), connect_ms)] if fail_times else []
    rec = list(recent) or [ok_rec(10**6, taken - 10, "filler.example.invalid:443")]
    s = {
        "schema": 1, "taken_at": taken, "uptime_secs": uptime,
        "log": {"lines": lines, "count": len(lines), "recorded": len(lines)},
        "errors": {"errors": rows},
        "recent": {"recent": rec},
        "hosts": {"hosts": hosts if hosts is not None else [], "truncated": hosts_truncated},
    }
    if series is not None:
        s["hosts_series"] = series
    return s


def by(res, secs):
    return next(b for b in res["by_secs"] if b["secs"] == secs)


class CauseTest(unittest.TestCase):
    def test_messages_map_to_the_three_causes(self):
        self.assertEqual(cf.cause_of_message("connection timed out"), "timeout")
        self.assertEqual(cf.cause_of_message("connection attempts timed out"), "timeout")
        self.assertEqual(cf.cause_of_message("Connection refused (os error 111)"), "refused")
        self.assertEqual(cf.cause_of_message("Network is unreachable (os error 101)"), "unreachable")
        self.assertEqual(cf.cause_of_message("No route to host (os error 113)"), "unreachable")
        # 名前解決や TLS は覚えない (名前解決には負のキャッシュがある)
        self.assertIsNone(cf.cause_of_message("failed to lookup address information: Name or service not known"))
        self.assertIsNone(cf.cause_of_message("TLS handshake failed"))

    def test_target_key_joins_forward_and_connect(self):
        self.assertEqual(cf.target_key("http://B.example.invalid:80"), "b.example.invalid:80")
        self.assertEqual(cf.target_key("b.example.invalid:80"), "b.example.invalid:80")


class ReplayTest(unittest.TestCase):
    def test_rejected_requests_do_not_extend_the_window(self):
        # 失敗が 100 秒ごと、待ちは 10 秒。N = 300 なら本当に繋ぎに行くのは 0 と 400 と 800 の組
        fails = [(START + 100 * k + 10, 10.0, "timeout") for k in range(10)]
        rejected, windows = cf.replay(fails, 300)
        self.assertEqual(len(windows), 3)
        self.assertEqual(len(rejected), 7)
        self.assertEqual([a for a, _b in windows], [START + 10, START + 410, START + 810])

    def test_a_short_window_rejects_nothing_when_requests_are_sparse(self):
        fails = [(START + 100 * k + 10, 10.0, "timeout") for k in range(10)]
        rejected, windows = cf.replay(fails, 60)
        self.assertEqual(rejected, [])
        self.assertEqual(len(windows), 10)

    def test_requests_that_arrived_before_the_failure_ended_still_go_out(self):
        # 同時に 3 本 (着いたのは同じ時刻、終わったのも同じ時刻) は全部本当に繋ぎに行く
        fails = [(START + 10, 10.0, "timeout")] * 3
        rejected, _w = cf.replay(fails, 300)
        self.assertEqual(rejected, [])


class AnalyzeTest(unittest.TestCase):
    def test_saved_wait_and_per_day(self):
        s = snap([START + 100 * k + 10 for k in range(10)], uptime=86_400)
        res = cf.analyze(s, (300,))
        b = by(res, 300)
        self.assertEqual(b["early"], 7)
        self.assertAlmostEqual(b["saved_secs"], 70.0)
        self.assertAlmostEqual(b["saved_secs_per_day"], 70.0)
        self.assertEqual(res["log_vs_errors_mismatch"], {})
        # 成功が 1 回も無い宛先 (`/hosts` に失敗だけ) は誤って断る余地が無い
        self.assertEqual((b["wrong_lo"], b["wrong_hi"]), (0, 0))
        self.assertFalse(b["meets"])  # 1 日 70 秒は 5 分に届かない

    def test_meets_when_enough_is_saved_and_nothing_is_wrong(self):
        s = snap([START + 20 * k + 10 for k in range(100)], uptime=86_400,
                 hosts=[{"host": "connect://" + A, "requests": 100, "errors": 100}])
        b = by(cf.analyze(s, (300,)), 300)
        self.assertGreaterEqual(b["saved_secs_per_day"], cf.TARGET_SAVED_SECS_PER_DAY)
        self.assertTrue(b["meets"])

    def test_lines_from_before_this_run_are_ignored(self):
        old = [fail_line(START - 50, A)]
        s = snap([START + 10, START + 20], extra_lines=old)
        res = cf.analyze(s, (300,))
        self.assertEqual(res["failures"], 2)

    def test_other_causes_are_not_remembered(self):
        s = snap([START + 10, START + 20], cause="dns",
                 why="failed to lookup address information: Name or service not known")
        res = cf.analyze(s, (300,))
        self.assertEqual(res["failures"], 0)

    def test_log_and_errors_mismatch_is_reported(self):
        s = snap([START + 10, START + 20])
        s["errors"]["errors"][0]["repeats"] = 5
        res = cf.analyze(s, (300,))
        self.assertEqual(res["log_vs_errors_mismatch"], {A: (2, 5)})

    def test_exact_success_inside_a_window_is_a_definite_wrong_rejection(self):
        rec = [ok_rec(1, START + 40, A), ok_rec(2, START + 500, A)]
        s = snap([START + 10], recent=rec,
                 hosts=[{"host": "connect://" + A, "requests": 3, "errors": 1}])
        b = by(cf.analyze(s, (60,)), 60)
        # 40 は覚えている間 (10, 70]、500 は外。`/recent` の窓が起動から覆うので、時刻の分からない成功は無い
        self.assertEqual((b["wrong_lo"], b["wrong_hi"]), (1, 1))

    def test_unknown_successes_widen_only_the_upper_bound(self):
        # `/recent` の窓は終わりの 10 秒だけ。成功 5 回の時刻は分からない
        s = snap([START + 10], hosts=[{"host": "connect://" + A, "requests": 6, "errors": 1}])
        b = by(cf.analyze(s, (60,)), 60)
        self.assertEqual((b["wrong_lo"], b["wrong_hi"]), (0, 5))

    def test_unknown_successes_do_not_count_when_every_window_is_covered(self):
        # 覚えている間 (10, 70] は `/recent` の窓 [START + 5, 取った時刻] の中。成功 5 回はその外のはず
        rec = [ok_rec(1, START + 5, "filler.example.invalid:443")]
        s = snap([START + 10], recent=rec,
                 hosts=[{"host": "connect://" + A, "requests": 6, "errors": 1}])
        b = by(cf.analyze(s, (60,)), 60)
        self.assertEqual((b["wrong_lo"], b["wrong_hi"]), (0, 0))

    def test_series_slots_bound_successes_by_five_minutes(self):
        t0 = START
        samples = [[0, 0, 0, 0, 0] for _ in range(10)]
        samples[0] = [1, 0, 0, 0, 1]   # 入った枠 (途中から数えている)
        samples[1] = [3, 0, 0, 0, 1]   # 300..600: 失敗 1 + 成功 2
        samples[5] = [4, 0, 0, 0, 0]   # 1500..1800: 成功 4
        series = {"t0": t0, "window_secs": 300,
                  "series": [{"host": "connect://" + A, "samples": samples}]}
        rec = [ok_rec(1, START + 2000, "filler.example.invalid:443")]
        s = snap([START + 290, START + 400], recent=rec, series=series,
                 hosts=[{"host": "connect://" + A, "requests": 10, "errors": 2}])
        b = by(cf.analyze(s, (60,)), 60)
        # 枠 1 の成功 2 は覚えている間 (400, 460] と重なる (上限)。枠 5 の 4 は重ならない。
        # 覚えている間 (290, 350] は数えている区間 (300 から) の外にあるので、時刻の分からない成功
        # (10 - 2 - 2 - 4 = 2) も上限に足す
        self.assertEqual((b["wrong_lo"], b["wrong_hi"]), (0, 4))

    def test_truncated_hosts_fall_back_to_the_smallest_shown(self):
        s = snap([START + 10], hosts=[{"host": "connect://" + B, "requests": 3, "errors": 0}],
                 hosts_truncated=True)
        b = by(cf.analyze(s, (60,)), 60)
        r = b["per_target"][0]
        self.assertEqual(r["successes_total"], 6)
        self.assertEqual(b["wrong_hi"], 6)


class EvidenceTest(unittest.TestCase):
    def test_same_run_snapshots_add_success_times(self):
        main = snap([START + 10], hosts=[{"host": "connect://" + A, "requests": 2, "errors": 1}])
        daily = snap([], uptime=50_000, recent=[ok_rec(7, START + 30, A), ok_rec(8, START + 20_000, B)])
        b = by(cf.analyze(main, (60,), [daily]), 60)
        self.assertEqual((b["wrong_lo"], b["wrong_hi"]), (1, 1))

    def test_snapshots_of_another_run_are_ignored(self):
        main = snap([START + 10], hosts=[{"host": "connect://" + A, "requests": 2, "errors": 1}])
        other = snap([], uptime=50_000, recent=[ok_rec(7, START + 30, A)])
        other["uptime_secs"] += 3600  # 1 時間前に起動した別の起動
        res = cf.analyze(main, (60,), [other])
        self.assertEqual(res["snapshots_used"], 1)
        self.assertEqual(by(res, 60)["wrong_lo"], 0)

    def test_the_same_record_seen_twice_counts_once(self):
        main = snap([START + 10], recent=[ok_rec(7, START + 30, A)],
                    hosts=[{"host": "connect://" + A, "requests": 2, "errors": 1}])
        daily = snap([], uptime=50_000, recent=[ok_rec(7, START + 30, A)])
        b = by(cf.analyze(main, (60,), [daily]), 60)
        self.assertEqual(b["wrong_lo"], 1)


class RenderTest(unittest.TestCase):
    def test_anon_hides_the_names(self):
        s = snap([START + 10, START + 30])
        buf = io.StringIO()
        cf.render(cf.analyze(s, (60,)), "x", anon=True, out=buf)
        self.assertNotIn("example.invalid", buf.getvalue())
        self.assertIn("宛先 A", buf.getvalue())

    def test_main_prints_json(self):
        with tempfile.TemporaryDirectory(prefix="t223-") as d:
            p = os.path.join(d, "s.json")
            with open(p, "w", encoding="utf-8") as f:
                json.dump(snap([START + 10, START + 30]), f)
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                self.assertEqual(cf.main([p, "--json", "--secs", "60"]), 0)
            got = json.loads(buf.getvalue())[p]
            self.assertEqual(got["by_secs"][0]["early"], 1)


if __name__ == "__main__":
    unittest.main()
