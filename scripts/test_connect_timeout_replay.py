#!/usr/bin/env python3
"""`scripts/connect-timeout-replay.py` (T.23.3 段 1) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から

入力はその場で組んだ**架空の**雪像 (宛先の名前は出てこない。時刻だけ)。本物の雪像は入れない。
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


ct = _load("connect_timeout_replay", "connect-timeout-replay.py")

START = 2_000_000
BOUNDS = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000]
CAUSES = ["dns", "refused", "unreachable", "timeout", "reset", "tls", "loop", "other"]


def rec_entry(i, at, ms, reason="server_eof", kind="connect"):
    return {"id": i, "at": at, "kind": kind, "reason": reason, "ms": {"connect": ms}}


def history(buckets, timeouts=0, samples=1, interval=3600, last_only=False):
    """`/history` の `3600` を組む (1 標本 = 直前の 1 区間ぶんの差分)。

    `last_only` なら、差分行を最後の 1 標本だけに置く (「28 日で 22 本」のような形)。
    """
    keys = ["t", "connects", "connect_ms_sum", "connect_ms_max", "connect_buckets",
            "errors_by_cause"]
    causes = [0] * len(CAUSES)
    causes[CAUSES.index("timeout")] = timeouts
    zero = [0] * len(CAUSES)
    rows = []
    for k in range(samples):
        hit = not last_only or k == samples - 1
        b = list(buckets) if hit else [0] * len(buckets)
        c = list(causes) if hit else list(zero)
        rows.append([START + k * interval, sum(b), 0, 0, b, c])
    return {"schema": 1, "interval_secs": interval, "keys": keys, "bounds_ms": BOUNDS,
            "causes": CAUSES, "samples": rows}


def snap(recent=None, hist=None, version="0.22.6+test", taken=START + 3600, uptime=3600):
    s = {"schema": 1, "taken_at": taken, "uptime_secs": uptime, "version": version}
    s["recent"] = {"recent": recent or [], "capacity": 2000, "count": len(recent or [])}
    if hist is not None:
        s["history"] = {"3600": hist}
    return s


def by(res, secs):
    return next(b for b in res["by_secs"] if b["secs"] == secs)


def empty_buckets():
    return [0] * (len(BOUNDS) + 1)


class RecentTest(unittest.TestCase):
    def test_successes_slower_than_t_are_the_ones_dropped(self):
        rec = [rec_entry(1, START + 10, 900), rec_entry(2, START + 20, 4000),
               rec_entry(3, START + 30, 6000)]
        res = ct.analyze(snap(recent=rec, taken=START + 3600), (3, 5, 10))
        self.assertEqual(by(res, 3)["recent_dropped"], 2)   # 4,000 と 6,000
        self.assertEqual(by(res, 5)["recent_dropped"], 1)   # 6,000
        self.assertEqual(by(res, 10)["recent_dropped"], 0)
        self.assertEqual(res["recent"]["max_connect_ms"], 6000)

    def test_failed_connects_are_not_successes(self):
        # 10 秒で切れた行 (`reason == "error:timeout"`) は成功の分布に入れない
        rec = [rec_entry(1, START + 10, 120),
               rec_entry(2, START + 20, 10000, reason="error:timeout"),
               rec_entry(3, START + 30, 1300, reason="error:unreachable")]
        res = ct.analyze(snap(recent=rec, taken=START + 3600), (5,))
        self.assertEqual(res["recent"]["connects"], 1)
        self.assertEqual(res["recent"]["timeouts"], 1)
        self.assertEqual(by(res, 5)["recent_dropped"], 0)

    def test_non_connect_records_are_skipped(self):
        rec = [rec_entry(1, START + 10, 0, reason="client_eof", kind="http")]
        res = ct.analyze(snap(recent=rec, taken=START + 3600), (5,))
        self.assertEqual(res["recent"]["connects"], 0)

    def test_per_day_is_the_window_extended(self):
        # 窓 1 時間 (taken − 最古) に 1 本 → 24 本/日
        rec = [rec_entry(1, START, 6000)]
        res = ct.analyze(snap(recent=rec, taken=START + 3600), (5,))
        self.assertAlmostEqual(by(res, 5)["recent_dropped_per_day"], 24.0)

    def test_no_recent_at_all_is_not_zero(self):
        res = ct.analyze(snap(hist=history(empty_buckets())), (5,))
        self.assertIsNone(res["recent"])
        self.assertIsNone(by(res, 5)["recent_dropped"])


class HistoryTest(unittest.TestCase):
    def test_buckets_fully_above_t_are_a_lower_bound(self):
        b = empty_buckets()
        b[3] = 100          # [5, 10)  ms
        b[6] = 7            # [50, 100)
        res = ct.analyze(snap(hist=history(b)), (5,))
        h = by(res, 5)
        self.assertEqual((h["history_dropped_lo"], h["history_dropped_hi"]), (0, 0))

    def test_a_bucket_straddling_t_lands_only_in_the_upper_bound(self):
        b = empty_buckets()
        b[11] = 4           # [2,500, 5,000) — 3 秒で切ると何本落ちるかは区間の中では分からない
        res = ct.analyze(snap(hist=history(b)), (3,))
        h = by(res, 3)
        self.assertEqual((h["history_dropped_lo"], h["history_dropped_hi"]), (0, 4))

    def test_the_top_bucket_loses_the_timeouts_in_old_versions(self):
        b = empty_buckets()
        b[12] = 22          # [5,000, ∞) — 0.18.0 は 10 秒で切った失敗もここに入れていた
        old = ct.analyze(snap(hist=history(b, timeouts=18), version="0.18.0+2defadb"), (5,))
        h = by(old, 5)
        self.assertEqual((h["history_dropped_lo"], h["history_dropped_hi"]), (4, 22))
        # T.20.4 からは成功だけなので、引かない
        new = ct.analyze(snap(hist=history(b, timeouts=18), version="0.22.6+000681e"), (5,))
        self.assertEqual((by(new, 5)["history_dropped_lo"],
                          by(new, 5)["history_dropped_hi"]), (22, 22))

    def test_a_version_that_cannot_be_read_does_not_subtract(self):
        self.assertIsNone(ct.failures_in_buckets("unknown"))
        self.assertTrue(ct.failures_in_buckets("0.18.0+2defadb"))
        self.assertFalse(ct.failures_in_buckets("0.20.0+d71bbdc"))
        self.assertFalse(ct.failures_in_buckets("0.22.6+000681e"))

    def test_saved_wait_is_the_gap_times_the_timeouts_per_day(self):
        # 窓 2 時間 (2 標本) に timeout 100 本 → 1,200 本/日。10 → 5 秒で 5 秒ずつ短くなる
        res = ct.analyze(snap(hist=history(empty_buckets(), timeouts=50, samples=2)), (5,))
        h = by(res, 5)
        self.assertEqual(res["history"]["window_secs"], 7200)
        self.assertAlmostEqual(h["saved_min_per_day"], 100.0)

    def test_timeout_count_after_t_includes_the_dropped_successes(self):
        b = empty_buckets()
        b[12] = 3
        res = ct.analyze(snap(hist=history(b, timeouts=10)), (5,))
        h = by(res, 5)
        self.assertEqual((h["timeouts"], h["timeouts_after_lo"], h["timeouts_after_hi"]),
                         (10, 13, 13))

    def test_t_at_the_current_deadline_changes_nothing(self):
        b = empty_buckets()
        b[12] = 3
        res = ct.analyze(snap(hist=history(b, timeouts=10)), (10,))
        h = by(res, 10)
        self.assertEqual((h["history_dropped_lo"], h["history_dropped_hi"]), (0, 0))
        self.assertEqual((h["timeouts_after_lo"], h["timeouts_after_hi"]), (10, 10))
        self.assertEqual(h["saved_min_per_day"], 0.0)

    def test_hours_limits_the_window(self):
        h = history(empty_buckets(), timeouts=10, samples=10)
        full = ct.analyze(snap(hist=h), (5,))
        self.assertEqual(full["history"]["samples"], 10)
        part = ct.analyze(snap(hist=h), (5,), hours=3)
        self.assertEqual(part["history"]["samples"], 3)
        self.assertEqual(part["history"]["window_secs"], 3 * 3600)

    def test_from_is_the_deadline_the_snapshot_was_taken_under(self):
        # T.23.3 以降の雪像は 5 秒で撮っているので `--from 5`。3 秒にすると 1 件あたり 2 秒縮む
        h = history(empty_buckets(), timeouts=50, samples=2)
        old = ct.analyze(snap(hist=h), (3, 5), ref=10)
        new = ct.analyze(snap(hist=h), (3, 5), ref=5)
        self.assertAlmostEqual(by(old, 3)["saved_min_per_day"], 140.0)   # 100 本/2 時間 → 1,200/日 × 7 秒
        self.assertAlmostEqual(by(new, 3)["saved_min_per_day"], 40.0)    # 同じ × 2 秒
        # いまの締め切り (5 秒) 以上は何も変わらない
        self.assertEqual(by(new, 5)["saved_min_per_day"], 0.0)
        self.assertEqual((by(new, 5)["history_dropped_lo"], by(new, 5)["history_dropped_hi"]), (0, 0))
        # まとめでも、いまの既定と同じ T は比べる対象にしない
        self.assertIsNone(ct.summary({"/a": new}, (3, 5), ref=5)[1]["ok"])

    def test_no_history_at_all_is_not_zero(self):
        res = ct.analyze(snap(recent=[rec_entry(1, START, 100)]), (5,))
        self.assertIsNone(res["history"])
        self.assertIsNone(by(res, 5)["history_dropped_hi"])


class SummaryTest(unittest.TestCase):
    def test_summary_takes_the_worst_snapshot(self):
        ok = snap(hist=history(empty_buckets()), version="0.22.6+test")
        old = snap(hist=history([0] * 11 + [0, 22], timeouts=18, samples=28 * 24,
                                last_only=True),
                   version="0.18.0+test")
        res = {"/a": ct.analyze(ok), "/b": ct.analyze(old)}
        s = {x["secs"]: x for x in ct.summary(res)}
        self.assertTrue(s[5]["ok"])
        # 672 時間 (28 日) の窓に 22 本
        self.assertAlmostEqual(s[5]["worst_dropped_hi_per_day"], 22 / 28.0, places=6)
        # いまの既定 (10 秒) は比べる対象にしない
        self.assertIsNone(s[10]["ok"])

    def test_summary_says_no_when_a_snapshot_is_over_the_line(self):
        # 3 秒なら 28 日で 63 本 = 2.25 本/日 (nagoya の実測の形)
        old = snap(hist=history([0] * 11 + [41, 22], timeouts=18, samples=28 * 24,
                                last_only=True),
                   version="0.18.0+test")
        s = {x["secs"]: x for x in ct.summary({"/b": ct.analyze(old)})}
        self.assertFalse(s[3]["ok"])
        self.assertTrue(s[5]["ok"])


class RenderTest(unittest.TestCase):
    def test_render_writes_unknown_instead_of_zero(self):
        buf = io.StringIO()
        ct.render(ct.analyze(snap(), (5,)), "x", out=buf)
        text = buf.getvalue()
        self.assertIn("個票が無い", text)
        self.assertIn("判定できず", text)
        self.assertNotIn("| 5 | 0 |", text)

    def test_main_prints_json(self):
        with tempfile.TemporaryDirectory(prefix="t233-") as d:
            p = os.path.join(d, "s.json")
            with open(p, "w", encoding="utf-8") as f:
                json.dump(snap(recent=[rec_entry(1, START, 6000)]), f)
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                self.assertEqual(ct.main([p, "--json", "--secs", "5"]), 0)
            got = json.loads(buf.getvalue())
            self.assertEqual(got["snapshots"][p]["by_secs"][0]["recent_dropped"], 1)
            # `/history` が無いので、まとめは「判定できず」(0 とは書かない)
            self.assertIsNone(got["summary"][0]["ok"])


if __name__ == "__main__":
    unittest.main()
