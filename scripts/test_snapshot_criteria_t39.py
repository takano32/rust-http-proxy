#!/usr/bin/env python3
"""判定表 T39 の試験。

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

STAGES = ["queue", "client_read", "dns", "connect", "first_relay", "relay", "park"]
# 1 時間ごとの要求と中継の揺れ (2 つが比例しないように別の並び)
REQ_WAVE = [1.0, 1.4, 0.7, 1.2, 0.9, 1.5, 0.6, 1.1]
RELAY_WAVE = [1.0, 0.8, 1.3, 0.6, 1.5, 1.1, 0.9, 1.2]


class Criteria39ConnPriced(unittest.TestCase):
    """`--criteria T39` (T39.9): `conn` 役の CPU を、名前解決に成功した要求と中継の時間で値付けし合って読む。"""

    T0 = Criteria29Parts.T0

    def profile(self, req=30, relay=60.0, us_req=1000, us_relay=50, hours=24, history_us=4800, wobble=None,
                stages=STAGES):
        """`/profile?res=60` の作り物。`conn` 役の CPU = `us_req` × 名前解決に成功した要求 (要求の 6 割) + `us_relay` ×
        中継の秒。要求と中継の秒は 1 分あたりの `req`・`relay` を時間ごとに `REQ_WAVE`・`RELAY_WAVE` で揺らす。
        `wobble` は時間ごとの CPU に掛ける並び (同じ版の中のぶれ)。物差しの `history` 役は `history_us` / 60 us/秒。"""
        roles = ["accept", "conn", "profile-sample", "other", "history"]
        rows = []
        for i in range(int(hours * 60)):
            h = i // 60
            q = round(req * REQ_WAVE[h % len(REQ_WAVE)])
            secs = relay * RELAY_WAVE[h % len(RELAY_WAVE)]
            conn = (us_req * q * 0.6 + us_relay * secs) * (wobble[h % len(wobble)] if wobble else 1)
            threads = [[1000, 60, []], [conn, 120, []], [10000, 60, []], [30000, 300, []], [history_us, 60, []]]
            connect = [0] * len(stages)
            if "relay" in stages:
                connect[stages.index("relay")] = [5, round(secs * 1000), 9000, []]
            rows.append([self.T0 + 60 * i, q, 1000 + conn + 44800, threads, connect])
        return {"interval_secs": 60, "keys": ["t", "requests", "cpu_us", "threads", "connect"], "roles": roles,
                "stages": {"connect": stages, "forward": []}, "samples": rows}

    def snap(self, p):
        """名前解決の失敗が要求の 4 割になる `/history?res=3600` を添えたスナップショット。"""
        total = sum(r[1] for r in p["samples"])
        hours = len(p["samples"]) // 60
        return Criteria29Parts.snap(self, p, round(total * 0.4 / hours))

    def c(self, pa, pb):
        return {"a": self.snap(pa), "b": self.snap(pb)}

    def row(self, c):
        return sc._t39_conn_priced(c, sc.T39_TH)

    def test_T39_replaces_only_the_conn_row_of_T38(self):
        t38, t39 = sd.RULES["T38"], sd.RULES["T39"]
        self.assertEqual(len(t38), len(t39))
        self.assertEqual([i for i in range(len(t38)) if t38[i] is not t39[i]], [4])
        self.assertIs(t39[4], sc._t39_conn_priced)
        self.assertEqual(sc.T39_TH, sc.T38_TH)
        self.assertEqual(sc.criteria_name("T39"), "T39")
        self.assertIn("T39", sc.INTRO)
        self.assertIn("T39", sd.CRITERIA)

    def test_fewer_requests_with_the_same_prices_is_met(self):
        # T37.99・T38.99 の形: 中継はそのままで要求が 0.6 倍に減る。値段が同じなら 1.00 倍 (T38 は判定できず)
        c = self.c(self.profile(30), self.profile(18))
        self.assertEqual(sc._t38_conn_per_resolved_request(c, sc.T38_TH)[3], sd.UNKNOWN)
        row = self.row(c)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("値付けし合って**、前の版の 1.1 倍未満 (T39.9。", row[0])
        self.assertTrue(row[2].startswith("**1.000** 倍 (前の機械の値段に直した。前の値段で後の仕事 1.00・後の値段で前の仕事 1.00)"),
                        row[2])
        self.assertIn("値段 (要求 1 本 + 中継 1 秒): 前 1,000 + 50.0 us (決定係数 1.00)、後 1,000 + 50.0 us (1.00)", row[2])
        self.assertIn("同じ版の中のぶれ", row[2])
        # T38 の読み方 (1 本あたりと要求の量) は表示だけで残る
        self.assertIn("T38 の読み方 ", row[2])
        self.assertIn("要求の量 (T38.0): ", row[2])
        self.assertIn("connect の段の `relay` の時間の和", row[4])

    def test_a_dearer_request_is_missed(self):
        row = self.row(self.c(self.profile(30), self.profile(30, us_req=1300)))
        self.assertEqual(row[3], sd.MISSED, row)
        self.assertIn("後 1,300 + 50.0 us", row[2])

    def test_a_dearer_relay_second_is_missed_too(self):
        row = self.row(self.c(self.profile(30), self.profile(18, us_relay=100)))
        self.assertEqual(row[3], sd.MISSED, row)

    def test_over_the_limit_but_inside_the_spread_cannot_be_judged(self):
        # 同じ版の中で 1 時間おきに CPU が 1.3 倍と 0.77 倍に揺れると、ぶれは 1.1 倍を大きく越える
        wobble = [1.3, 0.77]
        row = self.row(self.c(self.profile(30, wobble=wobble), self.profile(30, us_req=1150, wobble=wobble)))
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("倍以上だが、同じ版の中のぶれ (最大 ", row[4])
        self.assertIn("要求で払う分と時間で払う分に分ける材料はスナップショットに無い", row[4])

    def test_the_machine_ruler_divides_the_after_side(self):
        # 後の機械が 2 倍遅い (物差しも CPU も 2 倍): 前の値段に直すと 1.00 倍
        row = self.row(self.c(self.profile(30), self.profile(30, us_req=2000, us_relay=100, history_us=9600)))
        self.assertEqual(row[3], sd.MET, row)
        self.assertTrue(row[2].startswith("**1.000** 倍"), row[2])

    def test_without_the_relay_stage_it_cannot_be_judged(self):
        old = [s for s in STAGES if s != "relay"]
        row = self.row(self.c(self.profile(30, stages=old), self.profile(30)))
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("前の `/profile` に connect の段の `relay` が無い", row[4])

    def test_without_long_material_it_is_the_T36_reason(self):
        c = self.c(self.profile(30, hours=6), self.profile(30))
        row = self.row(c)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertEqual(row[4], sc._t36_conn_per_resolved_request(c, sc.T36_TH)[4])

    def test_conn_hours_bundles_clock_hours_and_drops_short_ends(self):
        p = self.profile(30, hours=3)
        p["samples"] = p["samples"][20:]   # 最初の時間は 40 標本 (半分以上なので残す)
        p["samples"] = p["samples"][:-35]  # 最後の時間は 25 標本 (半分に満たないので捨てる)
        hours, miss = sc.conn_hours(p)
        self.assertIsNone(miss)
        self.assertEqual([h["rows"] for h in hours], [40, 60])
        self.assertEqual(hours[1]["req"], 60 * round(30 * REQ_WAVE[1]))
        self.assertAlmostEqual(hours[1]["relay"], 60 * 60.0 * RELAY_WAVE[1])

    def test_fit_price_falls_back_to_requests_without_relaying(self):
        hours = [{"cpu": 600 * q, "req": q, "relay": 0} for q in (10, 20, 30)]
        a, b, r2 = sc.fit_price(hours, 0.6)
        self.assertAlmostEqual(a, 1000)
        self.assertEqual(b, 0.0)
        self.assertAlmostEqual(r2, 1.0)
        self.assertIsNone(sc.fit_price([{"cpu": 0, "req": 0, "relay": 5}], 0.6))


if __name__ == "__main__":
    unittest.main()
