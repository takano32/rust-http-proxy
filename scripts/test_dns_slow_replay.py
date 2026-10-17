#!/usr/bin/env python3
"""`scripts/dns-slow-replay.py` (T22.2) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から

入力は**その場で組んだ架空の**雪像 (`history["60"]` と `events` だけ)。本物の雪像は入れない。
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


ds = _load("dns_slow_replay", "dns-slow-replay.py")

T0 = 1_800_000_000


def snap(minutes, misses_at=None, canary_ms=12, events=()):
    """`minutes` 行の履歴。`misses_at` は {行の番号: (ミスの回数, 合計 ms)}。"""
    misses_at = misses_at or {}
    rows, canary = [], []
    for i in range(minutes):
        n, ms = misses_at.get(i, (0, 0))
        rows.append([T0 + i * 60, n, ms])
        canary.append([T0 + i * 60, canary_ms, 10, "canary.example.invalid:443", None])
    return {
        "schema": 1,
        "taken_at": T0 + minutes * 60,
        "version": "0.0.0+test",
        "history": {"60": {
            "keys": ["t", "dns_misses", "dns_ms_sum"],
            "samples": rows,
            "canary": {"keys": ["t", "canary_dns_ms", "canary_connect_ms", "canary_host",
                                "canary_ipv6_connect_ms"], "samples": canary},
        }},
        "events": {"events": [{"at": at, "kind": "anomaly", "text": text} for at, text in events]},
    }


class Windows(unittest.TestCase):
    def test_five_rows_make_one_window_and_the_first_four_are_skipped(self):
        ws = ds.windows(snap(10, {0: (1, 50), 4: (2, 300)}))
        self.assertEqual(len(ws), 6)
        self.assertEqual(ws[0]["t"], T0 + 5 * 60, "窓の終わり = 5 本目の行の 1 分後")
        self.assertEqual((ws[0]["misses"], ws[0]["ms_sum"]), (3, 350))
        self.assertEqual((ws[1]["misses"], ws[1]["ms_sum"]), (2, 300), "1 行目は窓から出た")
        self.assertEqual(ws[0]["worst"], 150.0, "1 分の平均がいちばん高い分")

    def test_a_slow_canary_raises_the_limit(self):
        self.assertEqual(ds.windows(snap(6, canary_ms=12))[0]["limit"], 100.0)
        self.assertEqual(ds.windows(snap(6, canary_ms=50))[0]["limit"], 150.0)

    def test_trim1_drops_the_slowest_minute_once(self):
        w = {"misses": 3, "ms_sum": 420, "worst": 400.0}
        self.assertEqual(ds.trim1(w), 10.0)
        self.assertEqual(ds.trim1({"misses": 1, "ms_sum": 500, "worst": 500.0}), 0.0)


class Rules(unittest.TestCase):
    def test_three_slow_misses_fire_the_current_rule_but_not_min5(self):
        ws = ds.windows(snap(30, {10: (3, 420)}))
        self.assertEqual(len(ds.episodes(ws, ds.PROPOSALS["current"][1])), 1)
        self.assertEqual(ds.episodes(ws, ds.PROPOSALS["min5"][1]), [])

    def test_it_clears_after_five_calm_minutes(self):
        ws = ds.windows(snap(30, {10: (3, 420)}))
        (since, until), = ds.episodes(ws, ds.PROPOSALS["current"][1])
        # 行 10 を含む窓は行 10〜14 の終わり。行 15 の終わりで外れ、そこから 5 分で解除
        self.assertEqual(since, T0 + 11 * 60)
        self.assertEqual(until, T0 + 21 * 60)

    def test_one_long_episode_fires_once(self):
        ws = ds.windows(snap(30, {i: (3, 900) for i in range(5, 20)}))
        self.assertEqual(len(ds.episodes(ws, ds.PROPOSALS["min10"][1])), 1)

    def test_a_slow_episode_counts_as_dropped_when_the_rule_never_fires(self):
        s = snap(40, {10: (6, 900), 11: (6, 900)})  # 12 回で平均 150 ms = 本当に遅い
        a = ds.analyze(s)
        self.assertEqual(a["slow_episodes"], 1)
        self.assertEqual(a["proposals"]["current"]["dropped_slow"], 0)
        self.assertEqual(a["proposals"]["sum2000"]["dropped_slow"], 1, "合計 1,800 ms は 2,000 に届かない")


class Real(unittest.TestCase):
    def test_real_events_are_matched_within_two_minutes(self):
        fired = T0 + 11 * 60
        s = snap(30, {10: (3, 420)}, events=[
            (fired - 30, "dns_slow: dns miss 140 ms avg over 5m (threshold 100 ms; 3 misses, 420 ms total)"),
            (fired + 400, "cleared: dns_slow after 6m (dns miss 0.0 ms avg over 5m, 0 misses)"),
            (fired, "errors: 6 errors in 5m (threshold 5)"),
        ])
        a = ds.analyze(s)
        self.assertEqual(a["real_events"], 1, "解除と別の種類は数えない")
        self.assertEqual(a["proposals"]["current"]["matched_real"], 1)

    def test_main_prints_a_table_and_json(self):
        with tempfile.TemporaryDirectory(prefix="t222-") as d:
            p = os.path.join(d, "snap.json")
            with open(p, "w", encoding="utf-8") as f:
                json.dump(snap(30, {10: (3, 420)}), f)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(ds.main([p]), 0)
            self.assertIn("| `current` |", out.getvalue())
            self.assertIn("下限", out.getvalue())
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(ds.main([p, "--json"]), 0)
            self.assertEqual(json.loads(out.getvalue())[p]["proposals"]["current"]["events"], 1)


if __name__ == "__main__":
    unittest.main()
