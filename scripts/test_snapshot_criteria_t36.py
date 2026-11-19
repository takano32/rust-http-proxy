#!/usr/bin/env python3
"""判定表 T36 の試験。

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
    DEPLOYED,
    build,
    sc,
    sd,
    Criteria29Parts,
    Criteria30Parts,
    T34999_BEFORE,
    T34999_AFTER,
)


class Criteria36Heap(unittest.TestCase):
    """`--criteria T36` (T36.1): heap の行を、キャッシュに加えて名前解決の表の持ち物も引いた残りで読む。"""

    T0 = 1_800_000_000
    UP = 41 * 3600

    def snap(self, grown_mb, entries=4096, cache_mb=0.0, mem_n=0, disk_n=0, alloc=0.0, dns=True, baseline=True):
        """起動から 41 時間のスナップショット。`heap_used + mmap − cache_memory` は基準 18.7 MB から `grown_mb` 増えている。"""
        base = 18_700_000
        cm = int(cache_mb * 1e6)
        now = base + int(grown_mb * 1e6)
        memory = {"heap_used": 6_600_000, "mmap": now + cm - 6_600_000, "cache_memory": cm,
                  "heap_free": 7_500_000, "rss": 33_400_000}
        if baseline:
            memory["baseline"] = {"at": self.T0 - self.UP + 60, "bytes": base}
        status = {"uptime_secs": self.UP, "memory": memory,
                  "cache": {"memory": {"used_bytes": cm, "entries": mem_n},
                            "disk": {"used_bytes": 81_900_000, "entries": disk_n}}}
        if alloc is not None:
            status["cache"]["memory"]["alloc_bytes"] = cm + int(alloc * 1e6)
        if dns:
            status["dns"] = {"entries": entries}
        return {"taken_at": self.T0, "uptime_secs": self.UP, "status": status}

    def row(self, b, rule=None):
        return (rule or sc._t36_heap)({"a": {}, "b": b, "info": {}}, sc.T36_TH)

    def test_T36_replaces_the_heap_row_of_T35(self):
        t35, t36 = sd.RULES["T35"], sd.RULES["T36"]
        self.assertEqual(len(t36), 27)
        # 9 番目 (T36.1)。11・13 番目は T36.3 (`Criteria36ZeroBefore`)、5・25・27 番目は T36.2 (`Criteria36MachineSpeed`)
        self.assertEqual([i for i in range(26) if t35[i] is not t36[i]], [4, 8, 10, 12, 24])
        self.assertIs(t36[8], sc._t36_heap)
        self.assertEqual(sc.T36_TH, sc.T35_TH)
        self.assertEqual(sc.criteria_name("T36"), "T36")

    def test_the_table_at_its_limit_is_taken_out(self):
        # 0.35.12 の締め (T35.99): キャッシュはほぼ空で +5.8 MB。T35 は届かず、T36 は表の 4,096 名前 × 1,070 B を引いて満たす
        b = self.snap(5.811, cache_mb=0.211342, mem_n=6, disk_n=134)
        self.assertEqual(self.row(b, sc._t35_heap)[3], sd.MISSED)
        row = self.row(b)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("残り **+1.4** MB (増え +5.8 MB − キャッシュ 0.1 MB − 名前解決の表 4.4 MB)", row[2])
        self.assertIn("名前解決の表 4,096 名前 × 1,070 B = 4.4 MB", row[4])
        self.assertEqual(sc.DNS_ENTRY_BYTES, 1070)

    def test_growth_beyond_the_table_is_still_missed(self):
        # 表のぶんを引いてもなお 5 MB 以上増えていれば、届かずのまま
        row = self.row(self.snap(10.0))
        self.assertEqual(row[3], sd.MISSED, row)
        self.assertIn("残り **+5.6** MB", row[2])
        # 表が小さければ引く量も小さい
        self.assertEqual(self.row(self.snap(5.5, entries=100))[3], sd.MISSED)
        self.assertEqual(self.row(self.snap(4.9, entries=0))[3], sd.MET)

    def test_without_dns_entries_the_table_is_a_range(self):
        # `dns.entries` が無ければ 0〜上限 4,096 名前ぶんの幅で引き、閾をまたげば判定できず
        row = self.row(self.snap(6.0, dns=False))
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("残り **+1.6〜+6.0** MB (増え +6.0 MB − キャッシュ 0.0 MB − 名前解決の表 0.0〜4.4 MB)", row[2])
        self.assertIn("`dns.entries` が無いので 0〜4.4 MB (上限 4,096 名前ぶん)", row[4])
        self.assertIn("**残りの幅が閾 5 MB をまたぐ**", row[4])
        self.assertEqual(self.row(self.snap(4.0, dns=False))[3], sd.MET)
        self.assertEqual(self.row(self.snap(10.0, dns=False))[3], sd.MISSED)

    def test_the_cache_range_and_the_table_add_up(self):
        # 0.34.165 の締め (T34.999): `alloc_bytes` が無いのでキャッシュは幅、表は 1 つの値
        b = self.snap(6.287, cache_mb=30.402326, mem_n=25, disk_n=132, alloc=None)
        row = self.row(b)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("残り **-30.2〜+1.8** MB (増え +6.3 MB − キャッシュ 0.1〜32.1 MB − 名前解決の表 4.4 MB)", row[2])

    def test_without_the_baseline_it_is_the_phase24_row(self):
        b = self.snap(6.0, baseline=False)
        self.assertEqual(self.row(b), self.row(b, sc._p24_heap))

    def test_an_old_baseline_cannot_be_judged(self):
        b = self.snap(1.0)
        b["status"]["memory"]["baseline"]["at"] = self.T0 - self.UP - 3600
        row = self.row(b)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("前の起動が残した基準", row[4])


class Criteria36ZeroBefore(unittest.TestCase):
    """`--criteria T36` (T36.3): 前の期間が 0 件だと読めなかった `dns_slow` と接続のエラーの行を、下限つきで読む。"""

    T0 = 1_800_000_000
    # 作り物は phase29・phase30 の試験と同じものを使う
    snap = Criteria30Parts.snap
    slow = Criteria30Parts.slow
    hist_snap = Criteria29Parts.hist_snap
    rows = staticmethod(Criteria29Parts.rows)

    def errors(self, a, b):
        return sc._t36_connect_errors({"a": a, "b": b}, sc.T36_TH)

    def dns_slow(self, a, b):
        return sc._t36_dns_slow({"a": a, "b": b}, sc.T36_TH)

    def test_T36_replaces_the_two_rows_of_T35(self):
        t35, t36 = sd.RULES["T35"], sd.RULES["T36"]
        self.assertEqual([i for i in range(26) if t35[i] is not t36[i]], [4, 8, 10, 12, 24])
        self.assertIs(t36[10], sc._t36_dns_slow)
        self.assertIs(t36[12], sc._t36_connect_errors)
        self.assertEqual((sc.CONNECT_ERROR_FLOOR, sc.DNS_SLOW_AVG_FLOOR_MS), (0.001, 500))

    def test_connect_errors_under_the_floor_are_met_even_after_zero(self):
        # T34.999 の形: 前が 0 件で、後が 1 件でもあると phase29 の行は届かず。後が 0.001 未満なら T36 は満たした
        a = self.hist_snap(self.rows(24, 0, 0, 9984, 0, 0))
        b = self.hist_snap(self.rows(48, 48, 0, 95952, 0, 0))
        self.assertEqual(sc._p29_connect_errors({"a": a, "b": b}, sc.PHASE30)[3], sd.MISSED)
        row = self.errors(a, b)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("0.00000 (0 / 9,984) → **0.00050** (48 / 96,000)", row[2])
        self.assertIn("前の版の 1.5 倍未満か、後が 0.001 未満 (T36.3。", row[0])
        self.assertEqual(row[1], "< 前 ×1.5 か < 0.001")
        self.assertIn("が下限 0.001 未満なので、前の何倍かに依らず満たした (T36.3)", row[4])

    def test_connect_errors_over_the_floor_still_compare_with_before(self):
        a = self.hist_snap(self.rows(24, 0, 0, 9984, 0, 0))
        # 後が 0.001 以上なら、前が 0 件なので届かずのまま
        b = self.hist_snap(self.rows(48, 96, 0, 45504, 0, 0))
        row = self.errors(a, b)
        self.assertEqual(row[3], sd.MISSED, row)
        self.assertNotIn("下限", row[4])
        # 前と比べて満たす行・判定できずの行は、phase29 と同じ判定
        b2 = self.hist_snap(self.rows(48, 3024, 71856, 33168, 3024, 7200))
        a2 = self.hist_snap(self.rows(48, 1440, 48000, 24000, 1392, 6960))
        self.assertEqual(self.errors(a2, b2)[3:], sc._p29_connect_errors({"a": a2, "b": b2}, sc.PHASE30)[3:])
        a3 = self.hist_snap([r[:3] for r in self.rows(48, 1440, 48000, 24000, 1392, 6960)], other_ports=False)
        self.assertEqual(self.errors(a3, b)[3], sd.UNKNOWN)

    def test_dns_slow_without_before_windows_is_read_against_500_ms(self):
        # 前の版に立った窓が無いと phase30 の行は判定できず。T36 は後の平均を 500 ms 未満で読む
        quiet = self.snap({}, [])
        self.assertEqual(sc._p30_dns_slow({"a": quiet, "b": self.slow(300, 2)}, sc.PHASE30)[3], sd.UNKNOWN)
        row = self.dns_slow(quiet, self.slow(300, 2))
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("**300** ms (8 回、立った 2 件)", row[2])
        self.assertIn("前の版に立った窓が無ければ 500 ms 未満。T36.3。", row[0])
        self.assertIn("前の版に成功したミスで立った窓が無いので、後の平均 300 ms を絶対値の閾 500 ms と比べた", row[4])
        self.assertNotIn("比べる平均が無い", row[4])
        self.assertEqual(self.dns_slow(quiet, self.slow(600, 2))[3], sd.MISSED)

    def test_dns_slow_with_before_windows_is_the_phase30_row(self):
        for a, b in ((self.slow(300, 5), self.slow(330, 15)), (self.slow(300, 5), self.slow(600, 5)),
                     ({}, self.slow(300, 5)), (self.slow(300, 5), self.slow(300, 5, exact=False))):
            self.assertEqual(self.dns_slow(a, b)[2:], sc._p30_dns_slow({"a": a, "b": b}, sc.PHASE30)[2:])


class Criteria36MachineSpeed(unittest.TestCase):
    """`--criteria T36` (T36.2): CPU の 2 行を、機械の速さの物差し (`history` 役の us/秒) で割ってから比べる。"""

    T0 = 1_800_000_000
    snap = Criteria29Parts.snap

    def profile(self, slow=1.0, conn_us_per_ok=1000, bg_us_per_sec=900, req_per_min=30, fail_share=0.4,
                history_us_per_sec=80, hours=24, history=True):
        """`/profile?res=60` の作り物。`slow` 倍だけ機械が遅いと、同じ仕事の CPU がどの役割も `slow` 倍になる。

        役割は accept・conn・profile-sample・other・history。`history` と `profile-sample` は 1 本 (60 標本/分)、
        全役割の標本は 1 分に 600 (= 10 本ぶん)。裏の役割 (`conn`・`accept` 以外) の和は `bg_us_per_sec`。
        """
        roles = ["accept", "conn", "profile-sample", "other"] + (["history"] if history else [])
        rows = []
        for i in range(int(hours * 60)):
            ok = req_per_min * (1 - fail_share)
            conn = conn_us_per_ok * ok * slow
            hist = history_us_per_sec * 60 * slow if history else 0
            bg = bg_us_per_sec * 60 * slow - hist
            threads = [[1000, 60, []], [conn, 120, []], [bg * 0.2, 60, []], [bg * 0.8, 300, []]]
            if history:
                threads.append([hist, 60, []])
            rows.append([self.T0 + 60 * i, req_per_min, 1000 + conn + bg + hist, threads])
        return {"interval_secs": 60, "keys": ["t", "requests", "cpu_us", "threads"], "roles": roles, "samples": rows}

    def c(self, pa, pb):
        return {"a": self.snap(pa, 720), "b": self.snap(pb, 720)}

    def test_T36_replaces_the_two_cpu_rows_and_adds_the_yardstick(self):
        t36 = sd.RULES["T36"]
        self.assertIs(t36[4], sc._t36_conn_per_resolved_request)
        self.assertIs(t36[24], sc._t36_idle_cost)
        self.assertIs(t36[26], sc._t36_machine_speed)
        self.assertEqual(sc.MACHINE_ROLE, "history")

    def test_machine_cost_is_history_cpu_per_sample(self):
        m = sc.machine_cost(self.profile(slow=1.5, hours=1))
        self.assertAlmostEqual(m["us"], 120.0)
        self.assertEqual((m["cpu"], m["samples"]), (120 * 3600, 3600))
        # 参考の `profile-sample` は全役割の標本 (1 分に 600) で割る
        self.assertAlmostEqual(m["sampler"], (900 - 80) * 60 * 1.5 * 0.2 / 600)
        # 標本 0 の役割は `0` 1 文字 (`[cpu, samples, states]` でない)
        p = self.profile(hours=1)
        for r in p["samples"]:
            r[3][4] = 0
        self.assertIsNone(sc.machine_cost(p))
        self.assertIsNone(sc.machine_cost(self.profile(hours=1, history=False)))
        self.assertIsNone(sc.machine_cost({"keys": ["t"], "roles": ["history"], "samples": []}))

    def test_a_slower_machine_alone_is_met_after_dividing(self):
        # 0.34.165 → 0.35.12 の形: コードは同じで、機械だけが 1.36 倍遅い。割る前は 2 行とも届かず
        c = self.c(self.profile(), self.profile(slow=1.36))
        self.assertEqual(sc._p29_conn_per_resolved_request(c, sc.T36_TH)[3], sd.MISSED)
        self.assertEqual(sc._p29_idle_cost(c, sc.T36_TH)[3], sd.MISSED)
        row = sc._t36_conn_per_resolved_request(c, sc.T36_TH)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("**1,000 → 1,000** us/名前解決に成功した要求 (前の機械の値段に直した。**1.00** 倍)", row[2])
        self.assertIn("表示だけ: 割る前 1,000 → 1,360 us (1.36 倍)、要求の全数あたり 600 → 816 us", row[2])
        self.assertIn("機械の速さの物差し (`history` 役の CPU ÷ その役の標本の数) は前 80.0 → 後 108.8 us/秒 (1.36 倍)",
                      row[4])
        self.assertIn("要求 43,200 本のうち名前解決の失敗 17,280 本 (40%)", row[4])
        self.assertIn("機械の速さの物差しで割って**、前の版の 1.1 倍未満 (T36.2。", row[0])
        self.assertEqual(row[1], "< 前 ×1.1 (物差しで割って)")
        row = sc._t36_idle_cost(c, sc.T36_TH)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("**900 → 900** us/秒 (前の機械の値段に直した。**1.00** 倍)。表示だけ: 割る前 900 → 1,224 us/秒 (1.36 倍)",
                      row[2])

    def test_more_work_on_the_same_machine_is_still_missed(self):
        # 機械が同じで `conn` の仕事が 1.2 倍・裏の役割が 1.15 倍なら、割っても届かず
        pa = self.profile()
        c = self.c(pa, self.profile(conn_us_per_ok=1200, bg_us_per_sec=1035))
        row = sc._t36_conn_per_resolved_request(c, sc.T36_TH)
        self.assertEqual(row[3], sd.MISSED, row)
        self.assertIn("(前の機械の値段に直した。**1.20** 倍)", row[2])
        row = sc._t36_idle_cost(c, sc.T36_TH)
        self.assertEqual(row[3], sd.MISSED, row)
        self.assertIn("**900 → 1,035** us/秒", row[2])
        # 機械が 1.36 倍遅く、そのうえ `conn` の仕事が 1.2 倍なら、割ったあとに 1.2 倍が残る
        row = sc._t36_conn_per_resolved_request(self.c(pa, self.profile(slow=1.36, conn_us_per_ok=1200)), sc.T36_TH)
        self.assertEqual(row[3], sd.MISSED, row)
        self.assertIn("**1.20** 倍", row[2])

    def test_the_yardstick_row_is_shown_only(self):
        row = sc._t36_machine_speed(self.c(self.profile(), self.profile(slow=1.36)), sc.T36_TH)
        self.assertEqual(row[3], sd.SHOWN, row)
        self.assertEqual(row[1], "—")
        self.assertIn("**80.0 → 108.8** us/秒 (**1.36** 倍)", row[2])
        self.assertIn("参考: `profile-sample` 役の CPU ÷ 読んだスレッドの標本 16.40 → 22.30 us (1.36 倍)", row[2])
        self.assertIn("前: `history` 役の `cpu_us` 6,912,000 us ÷ 標本 86,400", row[4])
        self.assertIn("**閾を置かない**", row[4])

    def test_without_the_history_role_or_long_material_it_cannot_be_judged(self):
        pa = self.profile()
        c = self.c(pa, self.profile(slow=1.36, history=False))
        for rule in (sc._t36_conn_per_resolved_request, sc._t36_idle_cost):
            row = rule(c, sc.T36_TH)
            self.assertEqual(row[3], sd.UNKNOWN, row)
            self.assertIn("後の `/profile` に `history` 役の CPU か標本が無い", row[4])
        row = sc._t36_machine_speed(c, sc.T36_TH)
        self.assertEqual((row[2], row[3]), ("—", sd.SHOWN))
        # 12 時間に満たない材料は、phase29 の行と同じ理由で判定できず
        short = self.c(pa, self.profile(hours=6))
        for rule, base in ((sc._t36_conn_per_resolved_request, sc._p29_conn_per_resolved_request),
                           (sc._t36_idle_cost, sc._p29_idle_cost)):
            self.assertEqual(rule(short, sc.T36_TH)[2:], base(short, sc.T36_TH)[2:])


T3599_BEFORE = os.path.join(DEPLOYED, "tokyo", "2026-11-07T214622Z-shutdown-snapshot.json")
T3599_AFTER = os.path.join(DEPLOYED, "tokyo", "2026-11-09T215159Z-snapshot.json")
T36_NOW = os.path.join(DEPLOYED, "tokyo", "2026-11-12T100706Z-snapshot.json")


@unittest.skipUnless(all(os.path.exists(x) for x in (T34999_BEFORE, T34999_AFTER, T3599_BEFORE, T3599_AFTER, T36_NOW)),
                     "tokyo の実データが無い (status/tokyo/ の T34.999・T35.99 の締めと 2026-11-12 のスナップショット)")
class Deployed36(unittest.TestCase):
    """T36.1 の受け入れ基準: 0.34.165・0.35.12 の締めと 2026-11-12 の 1 枚で、heap の行の残りがどれも 5 MB 未満。"""

    def heap(self, before, after, criteria):
        return build([before, after, "--no-dns", "--criteria", criteria])["criteria"]["rows"][8]

    def test_each_closeout_is_under_the_limit(self):
        for before, after, t35, want in (
                (T34999_BEFORE, T34999_AFTER, sd.UNKNOWN, "残り **-30.2〜+1.8** MB"),
                (T3599_BEFORE, T3599_AFTER, sd.MISSED, "残り **+1.4** MB"),
                (T3599_AFTER, T36_NOW, sd.MISSED, "残り **+2.1** MB")):
            with self.subTest(after=os.path.basename(after)):
                self.assertEqual(self.heap(before, after, "T35")[3], t35)
                row = self.heap(before, after, "T36")
                self.assertEqual(row[3], sd.MET, row)
                self.assertIn(want, row[2])

    def test_t34999_rows_with_nothing_before(self):
        # T36.3 の受け入れ基準: T34.999 の 2 枚で、前が 0 件だった 2 行が満たしたになる
        c = build([T34999_BEFORE, T34999_AFTER, "--no-dns", "--criteria", "T35"])["criteria"]["rows"]
        self.assertEqual((c[10][3], c[12][3]), (sd.UNKNOWN, sd.MISSED))
        rows = build([T34999_BEFORE, T34999_AFTER, "--no-dns", "--criteria", "T36"])["criteria"]["rows"]
        self.assertEqual(rows[10][3], sd.MET, rows[10])
        self.assertIn("**219** ms (13 回、立った 2 件)", rows[10][2])
        self.assertEqual(rows[12][3], sd.MET, rows[12])
        self.assertIn("0.00000 (0 / 9,981) → **0.00029** (13 / 45,577)", rows[12][2])


T36_PROFILES = [os.path.join(DEPLOYED, "tokyo", f"{t}-{k}.json")
                for t in ("2026-11-07T135149Z", "2026-11-09T215159Z", "2026-11-12T100706Z")
                for k in ("snapshot", "profile_res_60")]


@unittest.skipUnless(all(os.path.exists(x) for x in T36_PROFILES),
                     "tokyo の実データが無い (status/tokyo/ の 0.34.165・0.35.12 の締めと 2026-11-12 の `/profile`)")
class Deployed36MachineSpeed(unittest.TestCase):
    """T36.2 の受け入れ基準: 0.34.165・0.35.12 の締めと 2026-11-12 の 3 枚で、CPU の 2 行を物差しで割った結果。"""

    def rows(self, i, j):
        sa, pa, sb, pb = T36_PROFILES[2 * i], T36_PROFILES[2 * i + 1], T36_PROFILES[2 * j], T36_PROFILES[2 * j + 1]
        argv = [sa, sb, "--no-dns", "--criteria", "T36", "--profile-before", pa, "--profile", pb]
        return build(argv)["criteria"]["rows"]

    def test_the_three_snapshots(self):
        for i, j, yard, conn, idle in (
                (0, 1, "**81.3 → 110.5** us/秒 (**1.36** 倍)", ("**1.39** 倍", sd.MISSED), ("**1.21** 倍", sd.MISSED)),
                (1, 2, "**110.5 → 112.4** us/秒 (**1.02** 倍)", ("**0.99** 倍", sd.MET), ("**0.98** 倍", sd.MET)),
                (0, 2, "**81.3 → 112.4** us/秒 (**1.38** 倍)", ("**1.38** 倍", sd.MISSED), ("**1.19** 倍", sd.MISSED))):
            with self.subTest(before=i, after=j):
                rows = self.rows(i, j)
                self.assertEqual(len(rows), 27)
                self.assertIn(yard, rows[26][2])
                self.assertEqual(rows[26][3], sd.SHOWN)
                self.assertIn(conn[0], rows[4][2])
                self.assertEqual(rows[4][3], conn[1], rows[4])
                self.assertIn(idle[0], rows[24][2])
                self.assertEqual(rows[24][3], idle[1], rows[24])


if __name__ == "__main__":
    unittest.main()
