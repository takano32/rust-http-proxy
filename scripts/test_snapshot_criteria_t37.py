#!/usr/bin/env python3
"""判定表 T37 の試験。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

`scripts/test_snapshot_criteria.py` (3,874 行) を判定表ごとに割ったうちの 1 つ (T38.6)。中身は割る前と同じ。
作り物は `scripts/_criteria_testing.py` にある。
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _criteria_testing import (  # noqa: E402
    sc,
    sd,
    Criteria30Parts,
)


class Criteria37DnsSlow(unittest.TestCase):
    """`--criteria T37` (T37.1): `dns_slow` の行を、前の版との比ではなく、いつも後の平均の 500 ms 未満で読む。"""

    T0 = Criteria30Parts.T0
    snap = Criteria30Parts.snap
    slow = Criteria30Parts.slow

    def dns_slow(self, a, b):
        return sc._t37_dns_slow({"a": a, "b": b}, sc.T37_TH)

    def test_T37_replaces_only_the_dns_slow_row_of_T36(self):
        t36, t37 = sd.RULES["T36"], sd.RULES["T37"]
        self.assertEqual(len(t36), len(t37))
        self.assertEqual([i for i in range(len(t36)) if t36[i] is not t37[i]], [10])
        self.assertIs(t37[10], sc._t37_dns_slow)
        self.assertEqual(sc.T37_TH, sc.T36_TH)
        self.assertEqual(sc.criteria_name("T37"), "T37")

    def test_a_ratio_over_1_5_is_met_under_500_ms(self):
        # 0.36.34 の締め (T36.99) の形 (139 → 225 ms の 1.62 倍): 窓が立つ 1,000 ms 以上になるよう 4 回 × 260 → 420 ms
        # で作る。T36 では届かず、T37 では満たした
        a, b = self.slow(260, 3), self.slow(420, 7)
        self.assertEqual(sc._t36_dns_slow({"a": a, "b": b}, sc.T36_TH)[3], sd.MISSED)
        row = self.dns_slow(a, b)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("ミスの平均が 500 ms 未満 (T37.1。", row[0])
        self.assertEqual(row[1], "< 500 ms (立った窓の成功したミスの平均)")
        self.assertIn("後の平均 420 ms を絶対値の閾 500 ms と比べた (T37.1。", row[4])
        # 実測の欄は phase30 の行と同じ (前の値と倍率は表示だけ)
        self.assertEqual(row[2], sc._p30_dns_slow({"a": a, "b": b}, sc.PHASE30)[2])

    def test_over_500_ms_is_missed_even_when_before_was_as_slow(self):
        self.assertEqual(sc._p30_dns_slow({"a": self.slow(600, 5), "b": self.slow(600, 5)}, sc.PHASE30)[3], sd.MET)
        self.assertEqual(self.dns_slow(self.slow(600, 5), self.slow(600, 5))[3], sd.MISSED)

    def test_without_before_or_without_windows(self):
        quiet = self.snap({}, [])
        self.assertEqual(self.dns_slow(quiet, self.slow(300, 2))[3], sd.MET)
        self.assertEqual(self.dns_slow({}, self.slow(300, 2))[3], sd.MET)
        self.assertEqual(self.dns_slow(self.slow(300, 5), quiet)[3], sd.MET)
        # 後の待ちが確かに出せない (`dns_fail_ms_sum` の無い版) なら判定できず
        self.assertEqual(self.dns_slow(self.slow(300, 5), self.slow(300, 5, exact=False))[3], sd.UNKNOWN)


if __name__ == "__main__":
    unittest.main()
