#!/usr/bin/env python3
"""`scripts/callgrind-per-request.py` (T.34.7) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から

valgrind もプロキシも起こさない。傾きの計算・totals の読み取り・増減と表の行だけを見る。
"""

import os
import unittest

import _testing

HERE = os.path.dirname(os.path.abspath(__file__))


cg = _testing.load("callgrind_per_request", "callgrind-per-request.py")


class ReadTotals(unittest.TestCase):
    def test_totals_line_is_read(self):
        lines = ["version: 1\n", "events: Ir\n", "fn=main\n", "1 5\n", "totals: 45662457\n"]
        self.assertEqual(cg.read_totals(lines), 45662457)

    def test_summary_is_used_when_there_is_no_totals(self):
        self.assertEqual(cg.read_totals(["events: Ir\n", "summary: 1234\n"]), 1234)

    def test_totals_wins_over_summary_whichever_comes_first(self):
        self.assertEqual(cg.read_totals(["totals: 10\n", "summary: 99\n"]), 10)
        self.assertEqual(cg.read_totals(["summary: 99\n", "totals: 10\n"]), 10)

    def test_no_total_is_none(self):
        self.assertIsNone(cg.read_totals(["events: Ir\n", "fn=main\n", "3 7\n"]))

    def test_the_last_totals_line_counts(self):
        # 出力を足し継いだときは最後の行が全体
        self.assertEqual(cg.read_totals(["totals: 1\n", "totals: 2\n"]), 2)


class PerRequest(unittest.TestCase):
    def test_slope_of_two_runs(self):
        # T.34.0 の base forward: N=400 で 10,259,244、N=2000 で 45,662,457 → 22,127 Ir/要求
        self.assertEqual(cg.per_request(400, 10259244, 2000, 45662457), 22127)

    def test_fixed_cost_cancels(self):
        fixed = 9_000_000
        self.assertEqual(cg.per_request(400, fixed + 400 * 20000, 2000, fixed + 2000 * 20000), 20000)

    def test_slope_is_truncated_to_an_integer(self):
        self.assertEqual(cg.per_request(1, 0, 3, 3), 1)

    def test_same_n_is_an_error(self):
        with self.assertRaises(ValueError):
            cg.per_request(400, 1, 400, 2)


class ChangeAndReport(unittest.TestCase):
    def test_change_pct(self):
        self.assertAlmostEqual(cg.change_pct(22127, 22803), 3.055, places=3)
        self.assertAlmostEqual(cg.change_pct(200, 199), -0.5)
        self.assertIsNone(cg.change_pct(0, 5))

    def test_median(self):
        self.assertEqual(cg.median([3, 1, 2]), 2)
        self.assertEqual(cg.median([10, 20]), 15)

    def test_split_counts(self):
        self.assertEqual(cg.split_counts(5, 2), [3, 2])
        self.assertEqual(cg.split_counts(400, 2), [200, 200])
        self.assertEqual(sum(cg.split_counts(2001, 3)), 2001)

    def test_report_lines_compare_to_the_first_binary(self):
        lines = cg.report_lines(
            ["a", "b"],
            ["forward", "connect"],
            {(0, "forward"): [22127], (1, "forward"): [22803, 22801, 22900], (0, "connect"): [19182]},
        )
        self.assertEqual(lines[0], "forward  [0] a: 22,127 Ir/要求")
        self.assertEqual(lines[1], "forward  [1] b: 22,803 Ir/要求 (回ごと 22,803 / 22,801 / 22,900)  [0] から +3.06%")
        self.assertEqual(lines[2], "connect  [0] a: 19,182 Ir/要求")
        self.assertEqual(lines[3], "connect  [1] b: 測れなかった")


if __name__ == "__main__":
    unittest.main()
