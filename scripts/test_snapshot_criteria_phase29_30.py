#!/usr/bin/env python3
"""判定表 phase29・phase30 の試験。

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
)


class Criteria29Rows(Criteria29Parts, unittest.TestCase):
    """`--criteria phase29` (T.32.16・T.32.29・T.32.30): phase28 の 26 行のうち 5 行を名前解決の失敗の割合で動かない形にしたもの。"""

    # `T0`・`snap`・`hist_snap`・`rows` は `Criteria29Parts` (`_criteria_testing.py`。T36 の試験も使う。T38.6)

    def profile(self, hours, req_per_min, conn_us_per_ok, fail_share, bg_us_per_sec=900):
        """`/profile?res=60` の作り物。名前解決に失敗した要求は `conn` の CPU を使わない。"""
        rows = []
        for i in range(int(hours * 60)):
            ok = req_per_min * (1 - fail_share)
            conn = conn_us_per_ok * ok
            bg = bg_us_per_sec * 60
            rows.append([self.T0 + 60 * i, req_per_min, conn + bg + 1000,
                         [[1000, 60, []], [conn, 60, []], [bg * 0.2, 60, []], [bg * 0.15, 60, []], [bg * 0.65, 60, []]]])
        return {"interval_secs": 60, "keys": ["t", "requests", "cpu_us", "threads"],
                "roles": ["accept", "conn", "profile-sample", "cache-probe", "other"], "samples": rows}

    def test_phase29_replaces_six_rows_of_phase28(self):
        p28, p29 = sd.RULES["phase28"], sd.RULES["phase29"]
        self.assertEqual(len(p29), 26)
        self.assertEqual([i for i in range(26) if p28[i] is not p29[i]], [2, 4, 10, 12, 23, 24])
        self.assertIs(p29[4], sc._p29_conn_per_resolved_request)
        self.assertIs(p29[10], sc._p29_dns_slow)
        self.assertIs(p29[12], sc._p29_connect_errors)
        self.assertIs(p29[24], sc._p29_idle_cost)
        self.assertEqual(p29[23]({"a": {}, "b": {}}, sc.PHASE29)[3], sd.SHOWN)

    def test_conn_per_resolved_request_does_not_move_with_the_failure_share(self):
        # T31.99 の形: 名前解決に失敗した要求の割合だけが 40% → 66% に増える (成功した要求あたりの CPU は同じ)
        pa, pb = self.profile(24, 30, 1000, 0.40), self.profile(24, 60, 1000, 0.66)
        a, b = self.snap(pa, 30 * 60 * 0.40), self.snap(pb, 60 * 60 * 0.66)
        row = sc._p29_conn_per_resolved_request({"a": a, "b": b}, sc.PHASE29)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("**1,000 → 1,000** us/名前解決に成功した要求 (**1.00** 倍)", row[2])
        self.assertIn("要求 43,200 本のうち名前解決の失敗 17,280 本 (40%)", row[4])
        self.assertIn("(66%)", row[4])
        # 要求の全数あたりで比べた phase28 以前の読み方なら 0.57 倍に「下がって」見える (表示だけ)
        self.assertIn("要求の全数あたり 600 → 340 us", row[2])
        # 成功した要求あたりが 1.2 倍なら届かず
        pc = self.profile(24, 60, 1200, 0.66)
        row = sc._p29_conn_per_resolved_request({"a": a, "b": self.snap(pc, 60 * 60 * 0.66)}, sc.PHASE29)
        self.assertEqual(row[3], sd.MISSED, row)
        # 窓の端の 1 時間は重なった割合だけ数える (30 分ずらすと端の 2 時間が半分ずつ)
        shifted = self.snap(pb, 60 * 60 * 0.66)
        for s in shifted["history"]["3600"]["samples"]:
            s[0] -= 1800
        shifted["history"]["3600"]["samples"].append([self.T0 + 3600 * 24 - 1800, [60 * 60 * 0.66] + [0] * 7])
        row = sc._p29_conn_per_resolved_request({"a": a, "b": shifted}, sc.PHASE29)
        self.assertIn("**1,000 → 1,000**", row[2])

    def test_miss_band_shows_the_rate_without_failures(self):
        # T32.17: 表示だけのミスの行に、名前解決の失敗を除いた回/接続を添える (欄の無い版は今までどおり)
        c = {"a": {}, "b": {}, "after": {"dns_per_connect": 0.95, "dns_ok_per_connect": 0.048, "samples": 47,
                                         "connects": 32637},
             "after_all": {"dns_ok_per_connect": 0.047}}
        row = sc._p29_miss_band(c, sc.PHASE29)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertIn("**0.95** 回/接続", row[2])
        self.assertIn("名前解決の失敗を除くと 平常時 **0.048**・バースト込み 0.047 回/接続", row[2])
        old = {"a": {}, "b": {}, "after": {"dns_per_connect": 0.95, "samples": 47, "connects": 32637}, "after_all": {}}
        row = sc._p29_miss_band(old, sc.PHASE29)
        self.assertNotIn("失敗を除くと", row[2])
        self.assertIn("`dns_fail_misses` が無い", row[4])

    def test_short_or_missing_material_is_unknown_not_reference(self):
        pa, pb = self.profile(24, 30, 1000, 0.4), self.profile(6, 30, 1000, 0.4)
        a, b = self.snap(pa, 720), self.snap(pb, 720)
        for rule in (sc._p29_conn_per_resolved_request, sc._p29_idle_cost):
            row = rule({"a": a, "b": b}, sc.PHASE29)
            self.assertEqual(row[3], sd.UNKNOWN, row)
            self.assertIn("後の `/profile` が 6.0 時間", row[4])
        # `/history` に `errors_by_cause` が無ければ失敗が数えられない
        nb = self.snap(self.profile(24, 30, 1000, 0.4), 720,
                       history={"interval_secs": 3600, "keys": ["t", "errors"], "samples": [[self.T0, 1]]})
        row = sc._p29_conn_per_resolved_request({"a": a, "b": nb}, sc.PHASE29)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("`errors_by_cause`", row[4])

    def test_idle_cost_is_us_per_second(self):
        pa = self.profile(24, 30, 1000, 0.4, bg_us_per_sec=890)
        pb = self.profile(24, 90, 1000, 0.7, bg_us_per_sec=894)
        row = sc._p29_idle_cost({"a": self.snap(pa, 720), "b": self.snap(pb, 720)}, sc.PHASE29)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("**890 → 894** us/秒 (**1.00** 倍)", row[2])
        self.assertIn("`profile-sample`", row[2])
        pc = self.profile(24, 30, 1000, 0.4, bg_us_per_sec=1000)
        self.assertEqual(sc._p29_idle_cost({"a": self.snap(pa, 720), "b": self.snap(pc, 720)}, sc.PHASE29)[3],
                         sd.MISSED)

    def test_connect_errors_read_443_and_80_since_start(self):
        b = self.hist_snap(self.rows(48, 3024, 71856, 33168, 3024, 7200))
        a = self.hist_snap(self.rows(48, 1440, 48000, 24000, 1392, 6960))
        row = sc._p29_connect_errors({"a": a, "b": b}, sc.PHASE29)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("0.00260 (48 / 18,480) → **0.00000** (0 / 28,992)", row[2])
        # 前の版に `other_port_*` が無ければ判定できず (後の値は出す)
        a2 = self.hist_snap([r[:3] for r in self.rows(48, 1440, 48000, 24000, 1392, 6960)], other_ports=False)
        row = sc._p29_connect_errors({"a": a2, "b": b}, sc.PHASE29)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("後 **0.00000** (0 / 28,992)", row[2])
        self.assertIn("前の雪像の `/history?res=3600` に `other_port_errors`・`other_port_attempts` の欄が無い", row[4])
        # 443・80 のエラーが前の 2 倍なら届かず
        b3 = self.hist_snap(self.rows(48, 3024 + 4800, 71856, 33168, 3024, 7200))
        self.assertEqual(sc._p29_connect_errors({"a": a, "b": b3}, sc.PHASE29)[3], sd.MISSED)

    def slow_snap(self, events, with_fail_ms=True):
        keys = ["t", "dns_misses", "dns_fail_misses", "dns_ms_sum"] + (["dns_fail_ms_sum"] if with_fail_ms else [])
        samples = [[self.T0 + 60 * i, 0, 0, 0] + ([0] if with_fail_ms else []) for i in range(1440)]
        # 100 分目までの 5 分: 失敗 5 回 × 2 秒 (成功したミスは 0) → 失敗の遅さで立った
        for i in range(96, 101):
            samples[i][1:] = [1, 1, 2000] + ([2000] if with_fail_ms else [])
        # 700 分目までの 5 分: 成功した名前 10 回の平均 300 ms → 成功したミスで立った
        for i in range(696, 701):
            samples[i][1:] = [2, 0, 600] + ([0] if with_fail_ms else [])
        return {"taken_at": self.T0 + 86400, "uptime_secs": 86400 * 2,
                "events": {"events": events},
                "history": {"60": {"interval_secs": 60, "keys": keys, "samples": samples}}}

    def test_dns_slow_counts_only_resolved_misses(self):
        ev = [{"kind": "anomaly", "at": self.T0 + 60 * 100, "text": "dns_slow: dns miss 2000 ms avg over 5m"},
              {"kind": "anomaly", "at": self.T0 + 60 * 700, "text": "dns_slow: dns miss 300 ms avg over 5m"},
              {"kind": "anomaly", "at": self.T0 + 60 * 700 + 30, "text": "cleared: dns_slow"}]
        row = sc._p29_dns_slow({"a": {}, "b": self.slow_snap(ev)}, sc.PHASE29)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("(1 件 / 23.9 時間)", row[2])
        self.assertIn("失敗の遅さで立った 1 件", row[2])
        # 成功したミスで立った件が 1 時間に 0.1 を越えれば届かず
        many = [dict(ev[1]) for _ in range(3)]
        self.assertEqual(sc._p29_dns_slow({"a": {}, "b": self.slow_snap(many)}, sc.PHASE29)[3], sd.MISSED)
        # `dns_fail_ms_sum` の欄が無い版は判定できず
        row = sc._p29_dns_slow({"a": {}, "b": self.slow_snap(ev, with_fail_ms=False)}, sc.PHASE29)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("`dns_fail_ms_sum`", row[4])


T3199_BEFORE = os.path.join(DEPLOYED, "tokyo", "2026-10-28T191810Z-shutdown-snapshot.json")
T3199_BEFORE_PROFILE = os.path.join(DEPLOYED, "tokyo", "2026-10-28T143043Z-profile_res_60.json")
T3199_AFTER = os.path.join(DEPLOYED, "tokyo", "2026-10-31T063104Z-snapshot.json")
T3199_AFTER_PROFILE = os.path.join(DEPLOYED, "tokyo", "2026-10-31T063104Z-profile_res_60.json")


@unittest.skipUnless(all(os.path.exists(x) for x in (T3199_BEFORE, T3199_BEFORE_PROFILE,
                                                     T3199_AFTER, T3199_AFTER_PROFILE)),
                     "tokyo の実データが無い (status/tokyo/ の T.31.99 の 2 枚と `-profile_res_60.json`)")
class Deployed29(unittest.TestCase):
    """T.31.99 の 2 枚 (0.29.36 の停止 → 0.30.4 の 48.3 時間) と、前の版の 24 時間の `/profile` (T.32.29・T.32.30)。"""

    def test_t3199_pair(self):
        c = build([T3199_BEFORE, T3199_AFTER, "--no-dns", "--criteria", "phase29",
                   "--profile", T3199_AFTER_PROFILE, "--profile-before", T3199_BEFORE_PROFILE])["criteria"]
        rows = c["rows"]
        self.assertEqual(len(rows), 26)
        self.assertNotIn("reference", c)  # 「参考」が 0 行
        self.assertIn("**1,116 → 1,301** us/名前解決に成功した要求 (**1.17** 倍)", rows[4][2])
        self.assertIn("(41%)", rows[4][4])
        self.assertIn("(66%)", rows[4][4])
        self.assertIn("**890 → 894** us/秒", rows[24][2])
        self.assertEqual(rows[24][3], sd.MET)
        # 接続のエラー: 後は 8 / 29,008、前 (0.29.36) は欄が無いので判定できず
        self.assertIn("後 **0.00028** (8 / 29,008)", rows[12][2])
        self.assertEqual(rows[12][3], sd.UNKNOWN)
        self.assertIn("T.30.3 より前の版", rows[12][4])
        self.assertEqual(rows[23][3], sd.SHOWN)


class Criteria30Rows(Criteria30Parts, unittest.TestCase):
    """`--criteria phase30` (T.33.1): phase29 の `dns_slow` の行を「立った窓の成功したミスの平均」で読む。"""

    # `T0`・`snap`・`slow` は `Criteria30Parts` (`_criteria_testing.py`。T36・T37 の試験も使う。T38.6)

    def row(self, a, b):
        return sc._p30_dns_slow({"a": a, "b": b}, sc.PHASE30)

    def test_phase30_replaces_two_rows_of_phase29(self):
        p29, p30 = sd.RULES["phase29"], sd.RULES["phase30"]
        self.assertEqual(len(p30), 26)
        self.assertEqual([i for i in range(26) if p29[i] is not p30[i]], [10, 14])
        self.assertIs(p30[10], sc._p30_dns_slow)
        self.assertIs(p30[14], sc._p30_refresh_per_name)
        self.assertEqual(sc.PHASE30["dns_slow_avg_factor"], 1.5)

    def test_the_same_slowness_is_met_whatever_the_rate(self):
        # T32.99 の形: 立った件が前の 3 倍 (初めて引く名前が多い = 使われ方) でも、平均が同じ程度なら満たす
        row = self.row(self.slow(300, 5), self.slow(330, 15))
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("**330** ms (60 回、立った 15 件) (前 300 ms (20 回、立った 5 件)、1.10 倍)", row[2])
        self.assertIn("表示だけ: 0.63 件/時 (15 件 / 23.9 時間、成功したミスで立った)", row[2])
        # 平均が前の 2 倍なら届かず (件の数は同じでも)
        self.assertEqual(self.row(self.slow(300, 5), self.slow(600, 5))[3], sd.MISSED)
        # 後の期間に成功したミスで立った窓が無ければ満たした
        quiet = self.snap({}, [])
        self.assertEqual(self.row(self.slow(300, 5), quiet)[3], sd.MET)

    def test_failed_misses_do_not_move_the_average(self):
        # 同じ 1 分に 2 秒の失敗が混ざっても、`dns_fail_ms_sum` のある版では成功したミスの平均は同じ
        row = self.row(self.slow(300, 5), self.slow(300, 5, fails=(3, 6000)))
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("**300** ms (20 回、立った 5 件)", row[2])
        # 成功したミスが 3 回に満たない窓 (phase29 と同じく失敗の遅さで立った) は選ばない
        failing = self.snap({100: (5, 4, 4100, 4000)}, [100])
        self.assertIn("**0** 件", self.row(self.slow(300, 5), failing)[2])

    def test_a_version_without_fail_ms_is_read_with_bounds(self):
        # 前の版に `dns_fail_ms_sum` が無い (0.30.4): 失敗の混ざらない 1 分は確か、混ざった 1 分は 0〜合計
        clean = self.slow(300, 5, exact=False)
        row = self.row(clean, self.slow(330, 5))
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("前 300〜300 ms (20 回、立った 5 件。`dns_fail_ms_sum` の無い版なので上下限)", row[2])
        self.assertIn("立った窓 (失敗込みの閾) を全部使い", row[4])
        # 失敗の混ざった 1 分ばかりで上下限が広ければ判定できず (上下限とその 1.5 倍を書く)
        mixed = self.slow(300, 5, exact=False, fails=(2, 1000))
        row = self.row(mixed, self.slow(330, 5))
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("前 0〜550 ms", row[2])
        self.assertIn("前の平均の上下限 0〜550 ms の 1.5 倍 (0〜825 ms) の間に後の 330 ms があり", row[4])
        # 上限の 1.5 倍でも越えれば届かず
        self.assertEqual(self.row(mixed, self.slow(900, 5))[3], sd.MISSED)

    def test_the_web_columns_count_only_misses_for_443_and_80(self):
        # T33.3: `web_dns_misses`・`web_dns_ms_sum` のある版は 443・80 宛ての成功したミスだけで窓を選び、平均も出す。
        # どの窓も成功したミス 4 回 × 300 ms のうち 443 宛ては 3 回 × 400 ms、残り 1 回 (`:993`) は 0 ms
        keys = ["t", "dns_misses", "dns_fail_misses", "dns_ms_sum", "dns_fail_ms_sum", "web_dns_misses", "web_dns_ms_sum"]
        after = self.slow(300, 5)
        h = after["history"]["60"]
        h["keys"] = keys
        h["samples"] = [r + ([3, 1200] if r[1] else [0, 0]) for r in h["samples"]]
        row = self.row(self.slow(300, 5), after)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("**400** ms (15 回、立った 5 件)", row[2])
        self.assertIn("443・80 宛ての成功したミスだけで選び、数える (後)", row[4])
        # 443・80 宛てが 2 回 (合計 800 ms) しかない窓は選ばない (`:993` の遅い名前解決だけで立った窓)
        h["samples"] = [r[:5] + ([2, 800] if r[1] else [0, 0]) for r in h["samples"]]
        self.assertIn("**0** 件", self.row(self.slow(300, 5), after)[2])

    def test_missing_material_is_unknown(self):
        # 後の版に `dns_fail_ms_sum` が無い・前の版に `/history?res=60` が無い・前の版で立った窓が無い
        self.assertEqual(self.row(self.slow(300, 5), self.slow(300, 5, exact=False))[3], sd.UNKNOWN)
        row = self.row({}, self.slow(300, 5))
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("前の雪像に `/events` か `/history?res=60`", row[4])
        row = self.row(self.snap({}, []), self.slow(300, 5))
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("比べる平均が無い", row[4])


class Criteria30Refresh(unittest.TestCase):
    """`--criteria phase30` (T.33.2): 裏の引き直しを答えのある名前 1 つあたりで読み、一覧が切れていれば合計で比べる。"""

    @staticmethod
    def snap(refreshes, names, answered, hours=48, truncated=False, expired=0):
        """`/status` の `dns` と `/dns` の一覧。表の `names` 行のうち `answered` 行だけに答えがある。"""
        rows = ([{"host": f"ok{i}.example", "addrs": ["192.0.2.1"], "addr_count": 1, "failed": None}
                 for i in range(answered)]
                + [{"host": f"dead{i}.example", "addrs": [], "addr_count": 0, "failed": {"secs_ago": 1}}
                   for i in range(names - answered)])
        shown = rows[:657] if truncated else rows
        return {"uptime_secs": hours * 3600,
                "status": {"dns": {"refreshes": refreshes, "entries": names,
                                   "misses_by_kind": {"expired": expired, "warm_stale": 0}}},
                "dns": {"count": names, "shown": len(shown), "truncated": truncated, "entries": shown}}

    @staticmethod
    def row(rule, a, b):
        # 再起動をまたいだ 2 枚 (どちらも起動からの通算で読む)
        return rule({"a": a, "b": b, "info": {"restarted": True}}, sc.PHASE30)

    def test_the_number_of_names_alone_does_not_move_the_verdict(self):
        # 受け入れ基準: 表の名前の数だけを 7,511 → 891 に変えた作り物 (答えのある名前 560 と引き直しは同じ)
        a, b = self.snap(14_000, 7_511, 560), self.snap(10_800, 891, 560, hours=36)
        row = self.row(sc._p30_refresh_per_name, a, b)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("**0.54** 回/時/答えのある名前 (前 0.52、1.03 倍)", row[2])
        self.assertIn("表の名前あたり 0.34 回/時/名前 (前 0.04、名前 891、前 7,511)", row[2])
        same = self.row(sc._p30_refresh_per_name, self.snap(14_000, 891, 560), b)
        self.assertEqual(same[3], row[3])
        # phase25〜29 の読み方 (表の名前の数で割る) は同じ作り物で「届かず」に動く
        self.assertEqual(self.row(sc._p25_refresh_per_name, a, b)[3], sd.MISSED)
        self.assertEqual(self.row(sc._p25_refresh_per_name, self.snap(14_000, 891, 560), b)[3], sd.MET)
        # 答えのある名前あたりが前の 2 倍なら届かず
        self.assertEqual(self.row(sc._p30_refresh_per_name, a, self.snap(21_600, 891, 560, hours=36))[3],
                         sd.MISSED)

    def test_a_truncated_list_falls_back_to_the_total(self):
        # 前の `/dns` の一覧が切れている (T32.99 の前は 657 / 7,511 行) と答えのある名前が数えられない
        a = self.snap(14_000, 7_511, 560, truncated=True)
        b = self.snap(10_800, 891, 560, hours=36)
        row = self.row(sc._p30_refresh_per_name, a, b)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("合計 **300.0** 回/時 (前 291.7、1.03 倍)", row[2])
        self.assertIn("答えのある名前 後 560", row[2])
        self.assertIn("前の `/dns` の一覧が切れている (657 / 7,511 行) ので答えのある名前の数が出ない", row[4])
        # 合計でも名前の数だけを変えた作り物で判定は動かない
        self.assertEqual(self.row(sc._p30_refresh_per_name, self.snap(14_000, 891, 560, truncated=True), b)[3],
                         row[3])
        # 合計が前の 1.5 倍以上なら届かず
        self.assertEqual(self.row(sc._p30_refresh_per_name, a, self.snap(16_000, 891, 560, hours=36))[3],
                         sd.MISSED)
        # `/dns` の部が無いスナップショットも合計で比べる。`dns.refreshes` が無ければ判定できず
        nodns = {k: v for k, v in a.items() if k != "dns"}
        row = self.row(sc._p30_refresh_per_name, nodns, b)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("前の雪像に `/dns` の部が無い", row[4])
        self.assertEqual(self.row(sc._p30_refresh_per_name, {"uptime_secs": 3600}, b)[3], sd.UNKNOWN)


T3299_BEFORE = os.path.join(DEPLOYED, "tokyo", "2026-10-31T123625Z-shutdown-snapshot.json")
T3299_BEFORE_PROFILE = os.path.join(DEPLOYED, "tokyo", "2026-10-31T063104Z-profile_res_60.json")
T3299_AFTER = os.path.join(DEPLOYED, "tokyo", "2026-11-02T125840Z-snapshot.json")
T3299_AFTER_PROFILE = os.path.join(DEPLOYED, "tokyo", "2026-11-02T125840Z-profile_res_60.json")


@unittest.skipUnless(all(os.path.exists(x) for x in (T3299_BEFORE, T3299_BEFORE_PROFILE,
                                                     T3299_AFTER, T3299_AFTER_PROFILE)),
                     "tokyo の実データが無い (status/tokyo/ の T.32.99 の 2 枚と `-profile_res_60.json`)")
class Deployed30(unittest.TestCase):
    """T.32.99 の 2 枚 (0.30.4 の停止 → 0.32.39 の 48.3 時間) と、前の版の 24 時間の `/profile` (T.33.1)。"""

    def rows(self, criteria):
        return build([T3299_BEFORE, T3299_AFTER, "--no-dns", "--criteria", criteria,
                      "--profile", T3299_AFTER_PROFILE, "--profile-before", T3299_BEFORE_PROFILE])["criteria"]["rows"]

    def test_t3299_dns_slow(self):
        # phase29 は 0.63 件/時の「届かず」。phase30 では平均で読み、後は 15 件の窓の成功したミス 72 回で 334 ms。
        # 前 (0.30.4) は `dns_fail_ms_sum` が無く、立った 18 件の窓の成功したミス 50 回の平均は 98〜729 ms までしか
        # 絞れないので、334 ms は 1.5 倍 (146〜1,094 ms) の間で「判定できず」(上下限を書く)
        self.assertEqual(self.rows("phase29")[10][3], sd.MISSED)
        row = self.rows("phase30")[10]
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertIn("**334** ms (72 回、立った 15 件) (前 98〜729 ms (50 回、立った 18 件。", row[2])
        self.assertIn("0.63 件/時 (15 件 / 23.9 時間、成功したミスで立った)", row[2])
        self.assertIn("前 0.75 件/時 (18 件 / 23.9 時間、失敗込みの閾で立った)", row[2])
        self.assertIn("146〜1,094 ms", row[4])

    def test_t3299_refreshes(self):
        # phase29 は表の名前の数 (7,511 → 891) で割って 6.27 倍の「届かず」。phase30 は前の `/dns` の一覧が
        # 切れている (657 / 7,511 行) ので引き直しの合計 302.4 → 224.9 回/時 (0.74 倍) で満たす
        old = self.rows("phase29")[14]
        self.assertEqual(old[3], sd.MISSED)
        self.assertIn("6.27 倍", old[2])
        row = self.rows("phase30")[14]
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("合計 **224.9** 回/時 (前 302.4、0.74 倍)", row[2])
        self.assertIn("答えのある名前 後 563", row[2])
        self.assertIn("前の `/dns` の一覧が切れている (657 / 7,511 行)", row[4])


if __name__ == "__main__":
    unittest.main()
