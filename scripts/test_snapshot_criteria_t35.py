#!/usr/bin/env python3
"""判定表 T35 の試験。

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
    build,
    sc,
    sd,
    T34999_BEFORE,
    T34999_AFTER,
)


class Criteria35Events(unittest.TestCase):
    """`--criteria T35` (T35.9): `/events` の anomaly の行を要約 (`snapshot-summary.py`) と同じ関数で数える。"""

    T0 = 1_800_000_000

    def snap(self, events, uptime=7200, top_uptime=True, capacity=None):
        """起動から `uptime` 秒のスナップショット。`events` は `(起動からの秒, 文面)`、負の秒は起動より前。"""
        start = self.T0 - uptime
        ev = {"events": [{"kind": "anomaly", "at": start + t, "text": text} for t, text in events]}
        if capacity is not None:
            ev["capacity"] = capacity
        s = {"taken_at": self.T0, "status": {"uptime_secs": uptime}, "events": ev}
        if top_uptime:
            s["uptime_secs"] = uptime
        return s

    def row(self, rule, a, b):
        return rule({"a": a, "b": b}, sc.T35_TH)

    def test_T35_replaces_the_events_and_the_heap_rows_of_phase30(self):
        p30, p31 = sd.RULES["phase30"], sd.RULES["T35"]
        self.assertEqual(len(p31), 26)
        # 7 番目 (T35.9) と 9 番目 (T35.11。`Criteria35Heap`) だけ
        self.assertEqual([i for i in range(26) if p30[i] is not p31[i]], [6, 8])
        self.assertIs(p31[6], sc._t35_events)
        self.assertIs(p31[8], sc._t35_heap)
        self.assertEqual(sc.T35_TH, sc.PHASE30)

    def test_the_summary_and_the_row_share_one_function(self):
        import _testing
        ss = _testing.load("snapshot_summary_t359", "snapshot-summary.py")
        self.assertIs(ss.anomaly_rates, sc.anomaly_rates_since_start)
        self.assertIs(ss.started_at, sc.started_at)

    def test_the_row_prints_the_summary_numbers(self):
        a = self.snap([(60, "dns_slow: x")])
        b = self.snap([(-30, "dns_slow: before the start"), (60, "dns_slow: a"), (120, "dns_slow: b"),
                       (130, "cleared: dns_slow after 1m"), (200, "connect_p95: c"), (300, "no colon here")])
        rates = sc.anomaly_rates_since_start(b)
        self.assertEqual(rates["rows"], [("dns_slow", 2, 1.0), ("?", 1, 0.5), ("connect_p95", 1, 0.5)])
        row = self.row(sc._t35_events, a, b)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertEqual(row[1], "— (phase21 は `dns_slow` ≤ 0.1)")
        self.assertEqual(row[2], "`dns_slow` **1.000** 件/時 (2 件、前 0.500)、`?` 0.500 件/時 (1 件、前 0.000)、"
                                 "`connect_p95` 0.500 件/時 (1 件、前 0.000)")
        self.assertIn("起動 (取得 − `status.uptime_secs`) より前の 1 件を外し、起動からの 2.0 時間で割った", row[4])
        # phase30 の行は `:` の無い文面を数えない (古い数え方のまま)
        old = self.row(sc._p22_events, a, b)
        self.assertNotIn("`?`", old[2])
        self.assertIn("`dns_slow` **1.00** 件/時 (2 件、前 0.50)", old[2])

    def test_the_uptime_comes_from_the_status_part_first(self):
        # スナップショットの上の段に `uptime_secs` の無いスナップショット: phase30 は数えられず、T35 (要約) は `status` から読む
        b = self.snap([(60, "dns_slow: a")], top_uptime=False)
        self.assertEqual(self.row(sc._p22_events, {}, b)[2], "—")
        row = self.row(sc._t35_events, {}, b)
        self.assertIn("`dns_slow` **0.500** 件/時 (1 件)", row[2])
        self.assertIn("前の雪像に `/events` が無い", row[4])

    def test_a_full_ring_without_older_events_is_read_as_cut(self):
        b = self.snap([(60, "dns_slow: a"), (70, "dns_slow: b")], capacity=2)
        self.assertIn("**`/events` が満杯か切れている**", self.row(sc._t35_events, {}, b)[4])
        self.assertNotIn("切れている", self.row(sc._p22_events, {}, b)[4])
        # 起動より前の出来事が 1 件でも残っていれば、起動からのぶんは落ちていない (T32.31)
        b = self.snap([(-10, "dns_slow: old"), (60, "dns_slow: a")], capacity=2)
        self.assertNotIn("切れている", self.row(sc._t35_events, {}, b)[4])

    def test_a_snapshot_without_events_cannot_be_judged(self):
        row = self.row(sc._t35_events, {}, {"taken_at": self.T0, "uptime_secs": 60})
        self.assertEqual(row[2], "—")
        self.assertIn("後の雪像に `/events` の部が無い", row[4])


class Criteria35Heap(unittest.TestCase):
    """`--criteria T35` (T35.11): heap の行を、キャッシュの持ち物 (本文の確保の端数と索引) を引いた残りで読む。"""

    T0 = 1_800_000_000
    UP = 48 * 3600

    def snap(self, grown_mb, cache_mb=0.0, mem_n=0, disk_n=0, alloc=None, cache=True, baseline=True):
        """起動から 48 時間のスナップショット。`heap_used + mmap − cache_memory` は基準 18.8 MB から `grown_mb` 増えている。"""
        base = 18_812_256
        cm = int(cache_mb * 1e6)
        now = base + int(grown_mb * 1e6)
        memory = {"heap_used": 7_050_400, "mmap": now + cm - 7_050_400, "cache_memory": cm,
                  "heap_free": 8_837_984, "rss": 64_290_816}
        if baseline:
            memory["baseline"] = {"at": self.T0 - self.UP + 60, "bytes": base}
        status = {"uptime_secs": self.UP, "memory": memory}
        if cache:
            status["cache"] = {"memory": {"used_bytes": cm, "entries": mem_n},
                               "disk": {"used_bytes": 83_289_626, "entries": disk_n}}
            if alloc is not None:
                status["cache"]["memory"]["alloc_bytes"] = cm + int(alloc * 1e6)
        return {"taken_at": self.T0, "uptime_secs": self.UP, "status": status}

    def row(self, b, rule=None):
        return (rule or sc._t35_heap)({"a": {}, "b": b, "info": {}}, sc.T35_TH)

    def test_the_tokyo_numbers_cannot_be_judged_without_the_allocation(self):
        # 0.34.165 の tokyo (T34.999): +6.3 MB、メモリ 25 件 30.4 MB、ディスク 132 件。phase30 は「届かず」
        b = self.snap(6.287, cache_mb=30.402326, mem_n=25, disk_n=132)
        self.assertEqual(self.row(b, sc._p24_heap)[3], sd.MISSED)
        row = self.row(b)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("残り **-25.8〜+6.2** MB (増え +6.3 MB − 持ち物 0.1〜32.1 MB)", row[2])
        self.assertIn("メモリ 25 件 (`used_bytes` 30.4 MB)・ディスク 132 件、索引の見積もり 0.1 MB", row[4])
        self.assertIn("本文の確保の端数 0〜32.0 MB (`alloc_bytes` が無いので `used_bytes` + 件数 × 64 KiB まで)", row[4])
        self.assertIn("**残りの幅が閾 5 MB をまたぐ**", row[4])

    def test_the_allocation_decides_the_row(self):
        # 本体が `cache.memory.alloc_bytes` を出せば端数は 1 つに決まる
        b = self.snap(6.287, cache_mb=30.402326, mem_n=25, disk_n=132, alloc=3.0)
        row = self.row(b)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("残り **+3.2** MB (増え +6.3 MB − 持ち物 3.1 MB)", row[2])
        self.assertIn("本文の確保の端数 3.0 MB (`alloc_bytes` 33.4 − `used_bytes`)", row[4])
        self.assertEqual(self.row(self.snap(6.287, cache_mb=30.4, mem_n=25, disk_n=132, alloc=0.0))[3], sd.MISSED)

    def test_without_memory_entries_only_the_index_is_taken(self):
        # メモリのキャッシュが空なら端数の幅は 0 で、索引 (ディスク 10 件 × 0.5 KiB) だけを引いて判定する
        row = self.row(self.snap(6.0, disk_n=10))
        self.assertEqual(row[3], sd.MISSED, row)
        self.assertIn("残り **+6.0** MB (増え +6.0 MB − 持ち物 0.0 MB)", row[2])
        self.assertEqual(self.row(self.snap(4.0, disk_n=10))[3], sd.MET)

    def test_both_ends_on_one_side_are_judged(self):
        # 端数を目一杯引いても閾以上なら届かず、何も引かなくても閾未満なら満たす
        self.assertEqual(self.row(self.snap(40.0, cache_mb=10.0, mem_n=5))[3], sd.MISSED)
        row = self.row(self.snap(3.0, cache_mb=30.0, mem_n=25))
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("残り **-28.7〜+3.0** MB (増え +3.0 MB − 持ち物 0.0〜31.7 MB)", row[2])

    def test_without_the_cache_part(self):
        self.assertEqual(self.row(self.snap(3.0, cache=False))[3], sd.MET)
        row = self.row(self.snap(3.0, cache_mb=1.0, cache=False))
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`/status` に `cache` の部が無い", row[4])

    def test_without_the_baseline_it_is_the_phase24_row(self):
        b = self.snap(6.0, cache_mb=1.0, mem_n=1, baseline=False)
        self.assertEqual(self.row(b), self.row(b, sc._p24_heap))

    def test_an_old_baseline_cannot_be_judged(self):
        b = self.snap(1.0)
        b["status"]["memory"]["baseline"]["at"] = self.T0 - self.UP - 3600
        row = self.row(b)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("前の起動が残した基準", row[4])


@unittest.skipUnless(all(os.path.exists(x) for x in (T34999_BEFORE, T34999_AFTER)),
                     "tokyo の実データが無い (status/tokyo/ の T34.999 の 2 枚)")
class Deployed35(unittest.TestCase):
    """T34.999 の 2 枚 (0.33.3 の停止 → 0.34.165 の 48.1 時間。キャッシュが初めて使われた) に T35 を当てる (T35.11)。"""

    def rows(self, criteria):
        return build([T34999_BEFORE, T34999_AFTER, "--no-dns", "--criteria", criteria])["criteria"]["rows"]

    def test_t34999_heap(self):
        # phase30 は +6.3 MB の「届かず」。T35 はキャッシュの持ち物 (メモリ 25 件 30.4 MB の端数 0〜32.0 MB と
        # 索引 0.1 MB) を引くと残りが -25.8〜+6.2 MB で閾 5 MB をまたぐので「判定できず」
        self.assertEqual(self.rows("phase30")[8][3], sd.MISSED)
        row = self.rows("T35")[8]
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("残り **-25.8〜+6.2** MB (増え +6.3 MB − 持ち物 0.1〜32.1 MB)", row[2])

    def test_t34999_events_match_the_summary(self):
        row = self.rows("T35")[6]
        self.assertIn("`dns_slow` **0.062** 件/時 (3 件、前 0.000)、`connect_p95` 0.125 件/時 (6 件、前 0.087)", row[2])
        self.assertIn("より前の 461 件を外し", row[4])


if __name__ == "__main__":
    unittest.main()
