#!/usr/bin/env python3
"""`scripts/dns-replay.py` (T17.5) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から

入力は**架空の**雪像 (`testdata/replay-dns.json`、名前は `*.example.invalid`) と、
その場で組んだ到着列。**本物の雪像 (利用者の閲覧先が並ぶ) は入れない**。
"""

import contextlib
import importlib.util
import io
import json
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "testdata")


def _load(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


dr = _load("dns_replay", "dns-replay.py")
SNAP = os.path.join(DATA, "replay-dns.json")


def read(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def run_table(rule, arrivals, end):
    tb = dr.Table(dr.RULES[rule])
    tb._warm_t = 0
    for t, h in arrivals:
        tb.lookup(h, t)
    tb.advance(end)
    return tb.c


class RulesTest(unittest.TestCase):
    def test_a_name_used_once_is_one_cold_miss_and_never_warm(self):
        c = run_table("current", [(0, "a")], 7200)
        self.assertEqual(c["misses"], 1)
        self.assertEqual(c["misses_by_kind"]["cold"], 1)
        self.assertEqual(c["promotes"], 0)
        self.assertEqual(c["refreshes"], 0)

    def test_a_second_use_inside_the_ttl_is_a_hit(self):
        c = run_table("current", [(0, "a"), (30, "a")], 30)
        self.assertEqual((c["hits"], c["misses"]), (1, 0 + 1))

    def test_the_second_use_inside_the_window_makes_it_warm(self):
        # 2 回目 (期限切れのミス) で warm になり、3 回目はずっと後でも当たる
        c = run_table("current", [(0, "a"), (600, "a"), (1800, "a")], 1800)
        self.assertEqual(c["misses_by_kind"], {"cold": 1, "expired": 1, "warm_stale": 0})
        self.assertEqual(c["hits"], 1)
        self.assertEqual(c["promotes"], 1)
        # 600 → 1800 秒の間 45 秒ごと = 26 回 (1,800 ちょうどの予定は当たりの前に捌く)
        self.assertEqual(c["refresh_warm"], 26)

    def test_warm_stops_after_the_window(self):
        c = run_table("current", [(0, "a"), (600, "a")], 600 + 3600 * 3)
        # 600 + 45k が 600 + 3600 を過ぎたところで外れる: 45 × 80 = 3600 ちょうどで外れるので 79 回
        self.assertEqual(c["refresh_warm"], 79)
        self.assertEqual(c["warm_max"], 1)

    def test_current_rule_does_not_promote_after_a_long_gap_but_no_idle_does(self):
        # 2 回目は窓 (3,600 秒) の外、3 回目はその 10 分後
        arr = [(0, "a"), (5000, "a"), (5600, "a")]
        cur = run_table("current", arr, 5600)
        new = run_table("no_idle", arr, 5600)
        self.assertEqual(cur["misses"], 3)   # 2 回目で上がらないので 3 回目も期限切れ
        self.assertEqual(new["misses"], 2)   # 2 回目で上がって 3 回目は当たり
        self.assertEqual(cur["promotes"], 1)  # 3 回目 (窓の中の 2 回目) で上がる
        self.assertEqual(new["promotes"], 1)

    def test_a_hot_hit_past_three_quarters_of_the_ttl_asks_for_a_refresh(self):
        c = run_table("current", [(0, "a"), (40, "a"), (50, "a")], 50)
        # 40 秒で warm に上がる (予定は 85 秒)。50 秒は齢 50 ≥ 45 かつ 1 つ前が 10 秒前 = 先回り
        self.assertEqual(c["refresh_ask"], 1)
        self.assertEqual(c["hits"], 2)

    def test_promoting_on_a_hit_can_leave_a_stale_gap(self):
        # 30 秒の当たりで warm に上がると最初の予定は 75 秒で、齢 60〜75 の間は期限切れ
        c = run_table("current", [(0, "a"), (30, "a"), (70, "a")], 70)
        self.assertEqual(c["misses_by_kind"]["warm_stale"], 1)

    def test_max_warm_evicts_the_oldest(self):
        arr = []
        for i in range(dr.MAX_WARM + 1):
            arr += [(i, f"n{i}"), (i + 100, f"n{i}")]
        arr.sort()
        tb = dr.Table(dr.RULES["current"])
        for t, h in arr:
            tb.lookup(h, t)
        self.assertEqual(tb.c["warm_evicted"], 1)
        self.assertEqual(len(tb.queue), dr.MAX_WARM)
        self.assertNotIn("n0", tb.queue)

    def test_host_of(self):
        self.assertEqual(dr.host_of("connect://Chat.Example.Invalid:443"), "chat.example.invalid")
        self.assertEqual(dr.host_of("chat.example.invalid:443"), "chat.example.invalid")
        self.assertEqual(dr.host_of("[2001:db8::1]:443"), "2001:db8::1")
        self.assertTrue(dr.is_ip("2001:db8::1"))


class SnapshotTest(unittest.TestCase):
    def test_build_splits_warmup_and_window(self):
        snap = read(SNAP)
        warmup, inside, c0, taken, seen = dr.build(snap)
        # 閉じた時刻の最小 = 1789000000 + 0 (区間の始まり)
        self.assertEqual(c0, 1789000000)
        self.assertEqual(taken, 1789007200)
        self.assertTrue(all(t < c0 for t, _ in warmup))
        self.assertTrue(all(t >= c0 for t, _ in inside))
        # IP リテラルは数えない、開いたままの接続は入れる
        names = {h for _, h in inside}
        self.assertNotIn("192.0.2.1", names)
        self.assertIn("live.example.invalid", names)
        # `/dns` の通算が区間のミスより多い名前 = 前から居た (区間で初めて引いた名前は入らない)
        self.assertIn("old.example.invalid", seen)
        self.assertNotIn("new.example.invalid", seen)
        self.assertNotIn("api.example.invalid", seen)

    def test_run_on_the_fixture(self):
        out = dr.run(read(SNAP), list(dr.RULES))
        cur, new = out["rules"]["current"], out["rules"]["no_idle"]
        self.assertEqual(out["actual"], {"connects": 9, "dns_misses": 4,
                                         "miss_per_connect": 4 / 9})
        self.assertEqual(cur["lookups"], 9)
        # chat は助走で warm → 区間の前半はミスしない (最後の 1 回は窓の外で期限切れ)。
        # old は前から居たので no_idle だけ 1 回目で上がり、2 回目が当たる
        self.assertEqual(cur["misses"], 6)
        self.assertEqual(new["misses"], 5)
        self.assertEqual(cur["misses_by_kind"], {"cold": 3, "expired": 3, "warm_stale": 0})
        self.assertEqual((cur["promotes"], new["promotes"]), (1, 2))
        self.assertGreater(new["refreshes"], cur["refreshes"])
        self.assertIn("delta", out)

    def test_cli_prints_the_table(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(dr.main([SNAP]), 0)
        text = buf.getvalue()
        self.assertIn("| current |", text)
        self.assertIn("| no_idle |", text)
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(dr.main([SNAP, "--json", "--rule", "current"]), 0)
        self.assertEqual(list(json.loads(buf.getvalue())["rules"]), ["current"])

    def test_no_recent_is_an_error(self):
        with self.assertRaises(ValueError):
            dr.build({"taken_at": 1, "recent": {"recent": []}})


if __name__ == "__main__":
    unittest.main()
