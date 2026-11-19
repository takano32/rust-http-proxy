#!/usr/bin/env python3
"""判定表 T38 の試験。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

作り物は `scripts/_criteria_testing.py` にある (T38.6)。
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _criteria_testing import (  # noqa: E402
    Criteria29Parts,
    sc,
    sd,
)


class Criteria38ConnVolume(unittest.TestCase):
    """`--criteria T38` (T38.0): `conn` 役の CPU の行を、要求の量が前の版の 0.8〜1.25 倍の外なら判定できずにする。"""

    T0 = Criteria29Parts.T0
    snap = Criteria29Parts.snap

    def profile(self, req_per_min=30, conn_us_per_ok=1000, relay_us_per_min=0, conn_samples=120, fail_share=0.4,
                hours=24):
        """`/profile?res=60` の作り物。`conn` 役の CPU は要求で払う分 (`conn_us_per_ok` × 成功した要求) と時間で払う分
        (`relay_us_per_min`) の和。`conn` 役の標本は 1 分に `conn_samples` (= 接続スレッド × 秒)。物差しの `history` 役は
        どちらも 80 us/秒。"""
        roles = ["accept", "conn", "profile-sample", "other", "history"]
        rows = []
        for i in range(int(hours * 60)):
            conn = conn_us_per_ok * req_per_min * (1 - fail_share) + relay_us_per_min
            threads = [[1000, 60, []], [conn, conn_samples, []], [10000, 60, []], [30000, 300, []], [4800, 60, []]]
            rows.append([self.T0 + 60 * i, req_per_min, 1000 + conn + 44800, threads])
        return {"interval_secs": 60, "keys": ["t", "requests", "cpu_us", "threads"], "roles": roles, "samples": rows}

    def c(self, pa, pb, fa=None, fb=None):
        """前と後の雪像。名前解決の失敗は要求の 4 割 (`/history?res=3600` の `dns`)。"""
        def fails(p):
            return round(p["samples"][0][1] * 0.4 * 60)
        return {"a": self.snap(pa, fa if fa is not None else fails(pa)),
                "b": self.snap(pb, fb if fb is not None else fails(pb))}

    def row(self, c):
        return sc._t38_conn_per_resolved_request(c, sc.T38_TH)

    def test_T38_replaces_only_the_conn_row_of_T37(self):
        t37, t38 = sd.RULES["T37"], sd.RULES["T38"]
        self.assertEqual(len(t37), len(t38))
        self.assertEqual([i for i in range(len(t37)) if t37[i] is not t38[i]], [4])
        self.assertIs(t38[4], sc._t38_conn_per_resolved_request)
        self.assertEqual(sc.T38_TH, dict(sc.T37_TH, conn_request_band=(0.8, 1.25)))
        self.assertEqual(sc.criteria_name("T38"), "T38")
        self.assertIn("T38", sc.INTRO)

    def test_fewer_requests_with_the_same_relaying_cannot_be_judged(self):
        # T37.99 の形: 時間で払う分 (中継) はそのままで要求が 0.6 倍に減ると、1 本あたりが上がる
        c = self.c(self.profile(30, relay_us_per_min=20000), self.profile(18, relay_us_per_min=20000))
        self.assertEqual(sc._t36_conn_per_resolved_request(c, sc.T36_TH)[3], sd.MISSED)
        row = self.row(c)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("要求の量が前の版の 0.8〜1.25 倍の外なら判定できず。T38.0。T36.2。", row[0])
        self.assertIn("要求の量 (T38.0): 名前解決に成功した要求 1,080 → **648** 本/時 (**0.60** 倍)", row[2])
        self.assertIn("`conn` 役の標本 (接続スレッド × 秒。時間で払う分の量) 7,200 → 7,200 /時 (1.00 倍)", row[2])
        self.assertIn("前の版の 0.60 倍で、0.8〜1.25 倍の外 (T38.0)", row[4])
        self.assertIn("要求で払う分と時間で払う分に分ける材料がスナップショットに無い", row[4])
        # 実測の欄の頭は T36 の行と同じ (物差しで割った比は参考に残す)
        self.assertTrue(row[2].startswith(sc._t36_conn_per_resolved_request(c, sc.T36_TH)[2]))

    def test_more_requests_out_of_the_band_cannot_be_judged_either(self):
        row = self.row(self.c(self.profile(30), self.profile(40)))
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("前の版の 1.33 倍で", row[4])

    def test_inside_the_band_it_is_the_T36_row(self):
        for after, want in ((self.profile(27), sd.MET), (self.profile(33, conn_us_per_ok=1200), sd.MISSED)):
            c = self.c(self.profile(30), after)
            t36 = sc._t36_conn_per_resolved_request(c, sc.T36_TH)
            row = self.row(c)
            self.assertEqual(row[3], want, row)
            self.assertEqual(row[3], t36[3])
            self.assertEqual(row[4], t36[4])
            self.assertTrue(row[2].startswith(t36[2] + "。要求の量 (T38.0): "), row[2])
        # 幅の端 (0.8 倍・1.25 倍) は中に入れる
        self.assertEqual(self.row(self.c(self.profile(30), self.profile(24)))[3], sd.MET)
        self.assertEqual(self.row(self.c(self.profile(24), self.profile(30)))[3], sd.MET)

    def test_without_long_material_it_is_the_T36_reason(self):
        c = self.c(self.profile(30, hours=6), self.profile(30))
        row = self.row(c)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertEqual(row[4], sc._t36_conn_per_resolved_request(c, sc.T36_TH)[4])
        self.assertNotIn("T38.0", row[4])

    def test_conn_samples_add_the_second_field_of_the_conn_role(self):
        self.assertEqual(sc.conn_samples(self.profile(30, conn_samples=7, hours=1)), 7 * 60)
        self.assertIsNone(sc.conn_samples({"keys": ["t"], "roles": ["conn"], "samples": []}))


if __name__ == "__main__":
    unittest.main()
