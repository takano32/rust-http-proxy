#!/usr/bin/env python3
"""判定表 phase14・phase15・phase17・phase18 と、判定表の下に付く `--daily-snapshots` の表の試験。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

`scripts/test_snapshot_criteria.py` (3,874 行) を判定表ごとに割ったうちの 1 つ (T38.6)。中身は割る前と同じ。
作り物は `scripts/_criteria_testing.py` にある。
"""

import contextlib
import json
import tempfile
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _criteria_testing import (  # noqa: E402
    A,
    B,
    build,
    read,
    run,
    sd,
    set_column,
    written,
    conn_profile,
    zero_snapshot,
    with_probes,
    Criteria17Parts,
)


class Criteria(unittest.TestCase):
    def test_the_overload_counter_is_a_difference_when_there_was_no_restart(self):
        a = sd.load_source(A, False)
        b = read(B)
        b["version"] = a["version"]
        b["uptime_secs"] = a["uptime_secs"] + 86400
        b["status"].update(version=a["version"], uptime_secs=b["uptime_secs"],
                           since_start_secs=b["uptime_secs"], rejected_overload=9)
        a["status"]["rejected_overload"] = 4
        args = sd.parser().parse_args([A, B, "--no-dns", "--criteria", "phase14"])
        d = sd.build(a, sd.normalize(b, "b"), args)
        self.assertIn("`rejected_overload` 5", d["criteria"]["rows"][3][2])
        self.assertEqual(d["criteria"]["rows"][3][3], sd.MISSED)

    def test_phase14_gives_four_rows_with_a_verdict_each(self):
        d = build([A, B, "--no-dns", "--criteria", "phase14"])
        c = d["criteria"]
        self.assertEqual(len(c["rows"]), 4)
        self.assertEqual([r[3] for r in c["rows"]],
                         [sd.MET, sd.MISSED, sd.MET, sd.MET])
        self.assertEqual(c["tally"][sd.MET], 3)
        self.assertEqual(c["tally"][sd.MISSED], 1)

    def test_a_missing_history_makes_the_verdict_unknown(self):
        rows = sd.judge("phase14", None, {"rows": []}, [], {}, None, sd.PHASE14)
        self.assertEqual(rows["tally"][sd.UNKNOWN], 4)

    def test_no_burst_means_the_overload_row_cannot_be_judged(self):
        d = build([A, B, "--no-dns"])
        d["history"]["after"]["burst_samples"] = 0
        c = sd.judge("phase14", d["history"], d["hosts"], [], {"rejected_overload": 0}, 0,
                     sd.PHASE14)
        self.assertEqual(c["rows"][3][3], sd.UNKNOWN)
        self.assertIn("バーストが無かった", c["rows"][3][4])


class Criteria15(unittest.TestCase):
    """`--criteria phase15` の 6 行 (T15.4 が 3 行、T15.5 が 2 行、T15.6 が 1 行)。"""

    def judge(self, a=None, b=None, th=None, argv=None):
        args = sd.parser().parse_args(argv or [A, B, "--no-dns", "--criteria", "phase15"])
        sa, sb = sd.load_source(a or A, False), sd.load_source(b or B, False)
        d = sd.build(sa, sb, args)
        if th is None:
            return d["criteria"]
        return sd.judge("phase15", d["history"], d["hosts"], d["majors"],
                        sd.part(sb, "status"), 0, th, a=sa, b=sb,
                        info=d["restart"], dns=d["dns"], errors=d["errors"])

    def test_phase15_gives_six_rows_with_a_verdict_each(self):
        c = self.judge()
        self.assertEqual(len(c["rows"]), 6)
        self.assertEqual(sum(c["tally"].values()), 6)
        self.assertTrue(all(r[3] in (sd.MET, sd.MISSED, sd.UNKNOWN) for r in c["rows"]))

    def test_phase14_still_has_its_own_four_rows(self):
        """**閾の表を足すだけでは Phase 14 の行が phase15 の閾で出る**のが前の版の穴。"""
        self.assertEqual(len(self.judge(argv=[A, B, "--no-dns",
                                              "--criteria", "phase14"])["rows"]), 4)
        self.assertEqual(len(sd.RULES["phase14"]), 4)
        self.assertEqual(len(sd.RULES["phase15"]), 6)

    def test_a_host_that_is_not_in_the_table_cannot_be_judged(self):
        row = self.judge()["rows"][0]
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`discord.com` が無い", row[4])

    def test_the_named_host_is_judged_from_the_hosts_diff(self):
        th = dict(sd.PHASE15, watch_host="beta.example.jp")
        row = self.judge(th=th)["rows"][0]
        self.assertEqual(row[3], sd.MISSED)          # 10 ミス / 100 要求 = 0.10
        self.assertIn("**0.10** (10 ミス / 100 要求)", row[2])
        row = self.judge(th=dict(sd.PHASE15, watch_host="alpha.example.jp"))["rows"][0]
        self.assertEqual(row[3], sd.MET)             # 2 ミス / 200 要求 = 0.01

    def test_the_refresh_rate_is_judged_per_name_from_the_dns_part(self):
        """**閾は 1 名前あたりの上限**なので、`/dns` の名前ごとの最大で判定する (T15.15)。"""
        row = self.judge()["rows"][1]
        self.assertEqual(row[3], sd.MET)
        # alpha の 18 回 ÷ 起動から 12 時間
        self.assertIn("**1.5** 回/時 (`alpha.example.jp` 18 回、名前ごとの最大)", row[2])
        self.assertIn("5 名前", row[4])

    def test_the_average_uses_the_warm_mean_over_all_hours(self):
        """参考の平均は warm を**全時間** (バーストの時間も) から取り、通算の分子と窓を揃える。"""
        row = self.judge()["rows"][1]
        # 後の期間の `dns_warm` は 24 / 26 / 28 (28 はバーストの時間)。平常時だけなら 25.0
        self.assertIn("通算 24 回 ÷ 12.0 時間 ÷ warm 26.0 件", row[2])

    def test_one_busy_name_is_missed_even_if_the_average_is_low(self):
        b = read(B)
        b["dns"]["entries"][0]["refreshes"] = 1200    # 1,200 ÷ 12 時間 = 100 回/時
        with written(b=b) as paths:
            row = self.judge(b=paths["b"])["rows"][1]
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**100.0** 回/時", row[2])

    def test_without_the_dns_part_the_average_is_used(self):
        b = read(B)
        del b["dns"]
        b["status"]["dns"]["refreshes"] = 30000       # 30,000 ÷ 12 時間 ÷ 26 件 ≒ 96 回/時
        with written(b=b) as paths:
            row = self.judge(b=paths["b"])["rows"][1]
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("`/dns` の部が無い", row[4])

    def test_without_dns_or_dns_warm_the_refresh_rate_cannot_be_judged(self):
        b = read(B)
        del b["dns"]
        for res in b["history"]:
            i = b["history"][res]["keys"].index("dns_warm")
            b["history"][res]["keys"][i] = "dns_warm_x"   # 名前が違えば「無い」
        with written(b=b) as paths:
            row = self.judge(b=paths["b"])["rows"][1]
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`dns_warm`", row[4])

    def test_a_low_miss_rate_is_met(self):
        """**下の閾は外した** (T15.15。T15.99 の 0.05 が「幅より低い」で届かずになった)。"""
        row = self.judge()["rows"][2]
        self.assertEqual(row[3], sd.MET)
        self.assertEqual(row[1], "≤ 0.09")
        self.assertNotIn("幅より", row[2])

    def test_a_miss_rate_above_the_limit_is_missed(self):
        b = read(B)
        set_column(b, "3600", "dns_misses", 40)      # 平常時 2 標本 × 40 ÷ 400 本 = 0.20
        with written(b=b) as paths:
            row = self.judge(b=paths["b"])["rows"][2]
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**0.20** 回/接続", row[2])

    def daily(self, version="0.1.0+bbbbbbb"):
        day = {"connects": 100, "version": version}
        return {"days": [dict(day, day="2026-09-20", dns_per_connect=0.02),
                         dict(day, day="2026-09-21", dns_per_connect=0.12),
                         dict(day, day="2026-09-22", dns_per_connect=0.30, version="0.1.0+old"),
                         dict(day, day="2026-09-23", dns_per_connect=0.0, connects=0)]}

    def test_the_daily_band_is_shown_for_this_version_only(self):
        with written(d=self.daily()) as paths:
            row = self.judge(argv=[A, B, "--no-dns", "--criteria", "phase15",
                                   "--daily", paths["d"]])["rows"][2]
        # 前の版の日 (0.30) と接続 0 の日は入れない
        self.assertIn("日ごとの幅 0.02〜0.12 (2 日)", row[2])
        self.assertIn("`/daily`", row[4])

    def test_without_daily_there_is_no_band(self):
        self.assertNotIn("日ごとの幅", self.judge()["rows"][2][2])

    def test_a_daily_file_next_to_the_prefix_is_read(self):
        self.assertEqual(sd.classify("daily.json"), "daily")

    def test_the_conn_role_cpu_comes_from_the_profile_part(self):
        row = self.judge()["rows"][3]
        self.assertEqual(row[3], sd.MET)
        self.assertIn("**0.004** コア", row[2])       # 720,000 us ÷ 180 秒
        self.assertIn("3 標本 (180 秒)", row[4])

    def test_a_spinning_conn_role_is_missed(self):
        b = read(B)
        ti = b["profile"]["keys"].index("threads")
        for row in b["profile"]["samples"]:
            row[ti][1][0] = 30_000_000               # 1 標本 30 秒ぶん = 0.5 コア
        with written(b=b) as paths:
            row = self.judge(b=paths["b"])["rows"][3]
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**0.500** コア", row[2])

    def test_a_snapshot_without_profile_cannot_judge_the_cpu(self):
        b = read(B)
        del b["profile"]
        with written(b=b) as paths:
            row = self.judge(b=paths["b"])["rows"][3]
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`/profile` の部が無い", row[4])

    def minutes(self, snap, rows, closed=True):
        """`/history?res=60` に `closed` と `transfer` を足す (`rows` は `(t, 終わったトンネル, idle_timeout, 半閉じ)`)。

        `closed=False` で `closed` の部が無い形。
        """
        h = snap["history"].setdefault("60", {"interval_secs": 60, "keys": [], "samples": []})
        h["transfer"] = {"interval_secs": 60,
                         "keys": ["t", "tunnels", "speed_n", "speed", "half_close_n", "half_close"],
                         "samples": [[t, n, 0, [], half, []] for t, n, _i, half in rows]}
        if closed:
            reasons = ["client_eof", "server_eof", "idle_timeout", "keepalive_timeout",
                       "evicted", "limit", "shutdown", "error"]
            # 母数に forward が混ざっても (closed は http も数える) 割合は動かない
            h["closed"] = {"interval_secs": 60, "keys": ["t", "closed", "reasons"],
                           "reasons": reasons,
                           "samples": [[t, n + 50, [n - i, 50, i, 0, 0, 0, 0, 0]]
                                       for t, n, i, _h in rows]}
        return snap

    # 前 = 1789020000 台の平常時 (100 本/時)、1789030800 はバースト (400 本/時)。
    # 後 = 1789045200 台の平常時 (200 本/時)、1789052400 はバースト (350 本/時)
    BEFORE = 1789020000
    AFTER = 1789045200

    def closed_row(self, before, after, closed=True):
        a, b = read(A), read(B)
        self.minutes(a, before, closed)
        self.minutes(b, after, closed)
        with written(a=a, b=b) as paths:
            return self.judge(a=paths["a"], b=paths["b"])["rows"][4]

    def test_the_closed_shares_come_from_the_minute_counts(self):
        """**全数の割合**で比べる (T15.15。`/recent` は 256 KiB で切れて窓が毎回違う)。"""
        row = self.closed_row([(self.BEFORE, 100, 20, 10)],
                              [(self.AFTER, 300, 60, 30), (self.AFTER + 60, 300, 60, 30)])
        self.assertEqual(row[3], sd.MET)
        self.assertIn("`idle_timeout` 0.20 → **0.20** (+0%)、半閉じ 0.10 → 0.10", row[2])
        self.assertIn("前 1 分 100 本 / 後 2 分 600 本", row[4])

    def test_a_changed_share_is_missed(self):
        row = self.closed_row([(self.BEFORE, 100, 20, 10)], [(self.AFTER, 100, 40, 10)])
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("(+100%)", row[2])

    def test_burst_hours_and_unknown_hours_are_left_out(self):
        """平常時だけ: バーストの時間の分と、1 時間の標本がまだ無い分は数えない。"""
        row = self.closed_row([(self.BEFORE, 100, 20, 10), (1789030800 + 60, 1000, 900, 0)],
                              [(self.AFTER, 100, 20, 10), (1789052400, 1000, 900, 0),
                               (1789086000, 1000, 900, 0)])
        self.assertEqual(row[3], sd.MET)
        self.assertIn("バーストの時間 2 分・時間の標本が無い 1 分は外した", row[4])

    def test_without_transfer_the_shape_cannot_be_judged(self):
        row = self.judge()["rows"][4]
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`transfer` が無い", row[4])

    def test_a_side_with_no_tunnels_cannot_be_judged(self):
        row = self.closed_row([], [(self.AFTER, 100, 20, 10)])
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("前の平常時に閉じたトンネルが無い", row[4])

    def test_without_closed_only_the_half_close_is_shown(self):
        row = self.closed_row([(self.BEFORE, 100, 20, 10)], [(self.AFTER, 100, 20, 10)],
                              closed=False)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`closed` の部が無い", row[2])
        self.assertIn("半閉じ 0.10 → 0.10", row[2])

    def test_the_markdown_names_the_minute_counts(self):
        md = run([A, B, "--no-dns", "--criteria", "phase15"])
        self.assertIn("**平常時の 1 分ごとの全数**", md)

    def test_the_timeout_errors_are_compared_per_hour(self):
        """前後で標本の数が違うので、件数ではなく**1 時間あたり**で比べる。"""
        row = self.judge()["rows"][5]
        self.assertEqual(row[3], sd.MET)
        self.assertIn("**0.00** 件/時 (前 0.25 件/時)", row[2])

    def test_more_timeouts_than_before_is_missed(self):
        b = read(B)
        i = b["history"]["3600"]["keys"].index("errors_by_cause")
        for row in b["history"]["3600"]["samples"]:
            if row[0] >= 1789043200:                 # 再起動より後の標本だけ
                row[i] = [0, 0, 0, 9, 0, 0, 0, 0]    # timeout は 4 番目
        with written(b=b) as paths:
            row = self.judge(b=paths["b"])["rows"][5]
        self.assertEqual(row[3], sd.MISSED)

    def test_the_markdown_names_the_parts_it_needs(self):
        md = run([A, B, "--no-dns", "--criteria", "phase15"])
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria phase15`)", md)
        self.assertIn("**その部が雪像に無い行は「判定できず」**", md)
        self.assertIn("判定できず", md)

    def test_both_criteria_can_be_chosen(self):
        self.assertEqual(sorted(sd.CRITERIA),
                         ["T35", "T36", "T37", "T38", "phase14", "phase15", "phase17", "phase18", "phase20", "phase21",
                          "phase22", "phase23", "phase24", "phase25", "phase26", "phase27", "phase28", "phase29",
                          "phase30"])


class Criteria17(Criteria17Parts, unittest.TestCase):
    """`--criteria phase17` の 8 行 (T17.0a。前の 4 行は phase15 の関数、後ろの 4 行が T16.99 の判定)。

    スナップショットは `testdata/snapshot-{a,b}.json` に**作り物の欄を足して**使う (ファイルは書き換えない)。
    B の起動は 1789086400 − 43200 = 1789043200 (12 時間前)。
    """

    # `START` と `warm` は `Criteria17Parts` (`_criteria_testing.py`。後の判定表の試験も使う。T38.6)

    def judge(self, a=None, b=None, extra=()):
        argv = [a or A, b or B, "--no-dns", "--criteria", "phase17", *extra]
        return build(argv)["criteria"]

    def row(self, i, a=None, b=None, extra=()):
        return self.judge(a, b, extra)["rows"][i]

    def test_phase17_gives_eight_rows_with_a_verdict_each(self):
        c = self.judge()
        self.assertEqual(len(sd.RULES["phase17"]), 8)
        self.assertEqual(len(c["rows"]), 8)
        self.assertEqual(sum(c["tally"].values()), 8)
        self.assertTrue(all(r[3] in (sd.MET, sd.MISSED, sd.UNKNOWN) for r in c["rows"]))

    def test_the_first_four_rows_are_the_phase15_ones(self):
        self.assertEqual(sd.RULES["phase17"][:4], (sd._p15_watch_host, sd._p15_refresh_rate,
                                                   sd._p15_miss_band, sd._p15_timeout))
        rows15 = build([A, B, "--no-dns", "--criteria", "phase15"])["criteria"]["rows"]
        rows17 = self.judge()["rows"]
        self.assertEqual(rows17[:3], rows15[:3])
        self.assertEqual(rows17[3], rows15[5])

    def test_every_row_can_come_out_as_met_missed_and_unknown(self):
        """受け入れ基準: 後ろの 4 行のどれもが 3 つの判定のどれにもなれること (下の各テストの要約)。"""
        seen = {i: set() for i in range(4, 8)}
        for i, a, b, extra in self.cases():
            seen[i].add(self.row(i, a, b, extra)[3])
        for i, got in seen.items():
            self.assertEqual(got, {sd.MET, sd.MISSED, sd.UNKNOWN}, f"{i} 行目")

    def cases(self):
        """`(行, A, B, 追加の引数)` を順に返す (一時ファイルは呼ぶ側の `with` の中で使い切る)。"""
        with written(a=self.cgroup(read(A), 100, 30), b=self.cgroup(read(B), 150, 40),
                     big=self.cgroup(read(B), 5000, 4000),
                     warm=self.warm(read(B), 30, 0), full=self.warm(read(B), 32, 0),
                     slow=self.slow(read(B), 24), noev=self.drop(read(B), "events"),
                     noprof=self.drop(read(B), "profile"),
                     fast=conn_profile(200_000), slowp=conn_profile(150_000)) as p:
            yield 4, None, None, ("--profile-before", p["fast"])
            yield 4, None, None, ("--profile-before", p["slowp"])
            yield 4, None, p["noprof"], ()
            yield 5, None, p["warm"], ()
            yield 5, None, p["full"], ()
            yield 5, None, None, ()
            yield 6, None, None, ()
            yield 6, None, p["slow"], ()
            yield 6, None, p["noev"], ()
            yield 7, p["a"], p["b"], ()
            yield 7, p["a"], p["big"], ()
            yield 7, None, p["b"], ()

    @staticmethod
    def drop(snap, name):
        del snap[name]
        return snap

    # --- conn 役の CPU/要求 (`--profile`)

    def test_without_a_profile_before_the_ratio_cannot_be_judged(self):
        """A には `/profile` が無い。後は B のスナップショットの部 (80,000 us/要求) で「参考」。"""
        row = self.row(4)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("80,000 us/要求", row[2])
        self.assertIn("前の雪像に `/profile` の部が無い", row[4])
        self.assertIn("雪像の `/profile` の部 (**参考**)", row[4])

    def test_the_ratio_to_the_profile_before_is_judged(self):
        with written(fast=conn_profile(200_000), slow=conn_profile(150_000)) as p:
            met = self.row(4, extra=("--profile-before", p["fast"]))
            missed = self.row(4, extra=("--profile-before", p["slow"]))
        # 720,000 us ÷ 9 本 = 80,000。前は 600,000 ÷ 9 = 66,667 (1.20 倍) と 50,000 (1.60 倍)
        self.assertEqual(met[3], sd.MET)
        self.assertIn("**1.20** 倍 (66,667 → 80,000 us/要求)", met[2])
        self.assertEqual(missed[3], sd.MISSED)
        self.assertIn("**1.60** 倍", missed[2])
        # プロセス全体のコア数も並ぶ (1,080,000 us ÷ 180 秒)
        self.assertIn("プロセス全体 0.0060 → **0.0060** コア", met[2])

    def test_the_profile_file_wins_over_the_snapshot_part(self):
        with written(after=conn_profile(450_000), before=conn_profile(300_000)) as p:
            row = self.row(4, extra=("--profile", p["after"], "--profile-before", p["before"]))
        self.assertEqual(row[3], sd.MISSED)                  # 1.50 倍
        self.assertIn("**1.50** 倍 (100,000 → 150,000 us/要求)", row[2])
        self.assertIn("後: `--profile` 3 標本", row[4])
        self.assertNotIn("参考", row[4])

    def test_without_any_profile_the_cpu_cannot_be_judged(self):
        b = read(B)
        del b["profile"]
        with written(b=b) as p:
            row = self.row(4, b=p["b"])
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`--profile` も渡されていない", row[4])

    # --- `dns_warm_max` と `warm_evicted`

    def test_the_warm_peak_and_no_evictions_are_met(self):
        with written(b=self.warm(read(B), 30, 0)) as p:
            row = self.row(5, b=p["b"])
        self.assertEqual(row[3], sd.MET)
        self.assertIn("最大 **30** 件、`warm_evicted` **0**", row[2])
        self.assertIn("(起動から)", row[4])

    def test_a_full_table_or_an_eviction_is_missed(self):
        with written(full=self.warm(read(B), 32, 0), ev=self.warm(read(B), 30, 1)) as p:
            self.assertEqual(self.row(5, b=p["full"])[3], sd.MISSED)
            self.assertEqual(self.row(5, b=p["ev"])[3], sd.MISSED)

    def test_an_old_version_without_the_columns_cannot_be_judged(self):
        row = self.row(5)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`dns_warm_max` が無い", row[4])

    # --- `/events` の種類別 件/時

    def slow(self, snap, count, cleared=True):
        """起動より後に `dns_slow` を `count` 件 (と解けた知らせ)、起動より前に 5 件足す。"""
        ev = snap["events"]["events"]
        for i in range(count):
            at = self.START + 60 + i * 600
            ev.append({"at": at, "kind": "anomaly",
                       "text": "dns_slow: dns miss 400 ms avg over 5m (threshold 100 ms; "
                               "1 misses, 400 ms total)"})
            if cleared:
                ev.append({"at": at + 360, "kind": "anomaly",
                           "text": "cleared: dns_slow after 6m (dns miss 0.0 ms avg over 5m, "
                                   "0 misses)"})
        ev.append({"at": self.START + 100, "kind": "anomaly",
                   "text": "connect_p95: connect p95 105 ms over 5m is 10.6x the 1h baseline"})
        for i in range(5):                     # 前の版から読み継いだ出来事 (数えない)
            ev.append({"at": self.START - 1000 - i, "kind": "anomaly",
                       "text": "dns_slow: dns miss 300 ms avg over 5m"})
        return snap

    def test_no_anomaly_is_zero_per_hour_and_met(self):
        row = self.row(6)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("`dns_slow` **0.00** 件/時 (0 件)", row[2])
        self.assertIn("前の雪像に `/events` が無い", row[4])
        self.assertIn("起動からの 12.0 時間", row[4])

    def test_the_rates_are_per_hour_since_start_without_cleared(self):
        with written(b=self.slow(read(B), 24)) as p:
            row = self.row(6, b=p["b"])
        # 24 件 ÷ 12 時間 = 2.0 (解けた知らせ 24 件と起動より前の 5 件は数えない)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("`dns_slow` **2.00** 件/時 (24 件)", row[2])
        self.assertIn("`connect_p95` 0.08 件/時 (1 件)", row[2])     # 閾の無い種類は表示だけ

    def test_one_slow_event_in_twelve_hours_is_met(self):
        with written(b=self.slow(read(B), 1)) as p:
            row = self.row(6, b=p["b"])
        self.assertEqual(row[3], sd.MET)                  # 1 ÷ 12 = 0.08 ≤ 0.1
        self.assertIn("**0.08**", row[2])

    def test_the_before_snapshot_is_shown_next_to_it(self):
        a = read(A)
        a["events"] = {"events": [], "count": 0}
        with written(a=a, b=self.slow(read(B), 24)) as p:
            row = self.row(6, a=p["a"], b=p["b"])
        self.assertIn("(24 件、前 0.00)", row[2])
        self.assertNotIn("前の雪像に `/events` が無い", row[4])

    def test_without_events_the_rates_cannot_be_judged(self):
        with written(b=self.drop(read(B), "events")) as p:
            row = self.row(6, b=p["b"])
        self.assertEqual(row[3], sd.UNKNOWN)

    # --- cgroup の起動からの CPU

    @staticmethod
    def cgroup(snap, user_s, sys_s):
        """`/status` の `kernel.cgroup_cpu.since_start` に T16.0 の 3 欄を足す (秒で与える)。"""
        k = snap["status"].setdefault("kernel", {})
        cg = k.setdefault("cgroup_cpu", {})
        ss = cg.setdefault("since_start", {"nr_periods": 10, "nr_throttled": 0,
                                           "throttled_usec": 0})
        ss.update(usage_usec=(user_s + sys_s) * 1_000_000, user_usec=user_s * 1_000_000,
                  system_usec=sys_s * 1_000_000)
        return snap

    def test_the_cgroup_cpu_of_both_sides_in_the_same_order_is_met(self):
        with written(a=self.cgroup(read(A), 100, 300), b=self.cgroup(read(B), 150, 280)) as p:
            row = self.row(7, a=p["a"], b=p["b"])
        self.assertEqual(row[3], sd.MET)
        # 前 400 秒 ÷ 200,000 秒 = 0.0020、後 430 ÷ 43,200 = 0.0100 (user 35%)
        self.assertIn("0.0020 コア、user 25% → **0.0100 コア、user 35%**", row[2])
        self.assertIn("絞り 30 / 4,000 周期", row[4])

    def test_ten_times_more_is_missed(self):
        with written(a=self.cgroup(read(A), 100, 300), b=self.cgroup(read(B), 2000, 280)) as p:
            row = self.row(7, a=p["a"], b=p["b"])
        self.assertEqual(row[3], sd.MISSED)                # user 0.0005 → 0.0463 コア
        self.assertIn("user が桁で増えた", row[2])

    def test_an_old_before_snapshot_says_the_column_is_missing(self):
        with written(b=self.cgroup(read(B), 150, 280)) as p:
            row = self.row(7, b=p["b"])
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("前の版に欄が無い", row[4])
        self.assertIn("後 **0.0100 コア、user 35%**", row[2])

    def test_without_the_columns_the_cgroup_cannot_be_judged(self):
        row = self.row(7)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`usage_usec` が無い", row[4])

    def test_the_markdown_names_the_phase17_parts(self):
        md = run([A, B, "--no-dns", "--criteria", "phase17"])
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria phase17`)", md)
        self.assertIn("前の 4 行は phase15 と同じ物差しです", md)
        self.assertIn("**その部が雪像に無い行は「判定できず」**", md)


class Criteria18(unittest.TestCase):
    """`--criteria phase18` の 10 行 (T18.2。前の 8 行は phase17 の関数、後ろの 2 行が T18.0 の `heap_used + mmap` と IPv6 の探り)。

    スナップショットは `testdata/snapshot-{a,b}.json` に**作り物の欄を足して**使う (T18.1 の欄は名前だけ同じ)。
    """

    def judge(self, a=None, b=None, extra=()):
        argv = [a or A, b or B, "--no-dns", "--criteria", "phase18", *extra]
        return build(argv)["criteria"]

    def row(self, i, a=None, b=None, extra=()):
        return self.judge(a, b, extra)["rows"][i]

    def test_phase18_gives_ten_rows_with_a_verdict_each(self):
        with written(z=zero_snapshot(), b=with_probes(read(B), 0)) as p:
            c = self.judge(b=p["b"], extra=("--zero", p["z"]))
        self.assertEqual(len(sd.RULES["phase18"]), 10)
        self.assertEqual(len(c["rows"]), 10)
        self.assertEqual(sum(c["tally"].values()), 10)
        self.assertTrue(all(r[3] in (sd.MET, sd.MISSED, sd.UNKNOWN) for r in c["rows"]))
        self.assertEqual([r[3] for r in c["rows"][8:]], [sd.MET, sd.MET])

    def test_the_first_eight_rows_are_the_phase17_ones(self):
        self.assertEqual(sd.RULES["phase18"][:8], sd.RULES["phase17"])
        self.assertEqual({k: sd.PHASE18[k] for k in sd.PHASE17}, sd.PHASE17)
        rows17 = build([A, B, "--no-dns", "--criteria", "phase17"])["criteria"]["rows"]
        with written(z=zero_snapshot()) as p:
            rows18 = self.judge(extra=("--zero", p["z"]))["rows"]
        self.assertEqual(rows18[:8], rows17)

    def test_both_new_rows_can_come_out_as_met_missed_and_unknown(self):
        """受け入れ基準: 後ろの 2 行のどちらも 3 つの判定のどれにもなれること (下の各テストの要約)。"""
        seen = {8: set(), 9: set()}
        with written(z=zero_snapshot(), small=zero_snapshot(heap_used=1_000_000),
                     ok=with_probes(read(B), 0), bad=with_probes(read(B), 3)) as p:
            for i, b, extra in ((8, None, ("--zero", p["z"])), (8, None, ("--zero", p["small"])),
                                (8, None, ()), (9, p["ok"], ()), (9, p["bad"], ()), (9, None, ())):
                seen[i].add(self.row(i, b=b, extra=extra)[3])
        for i, got in seen.items():
            self.assertEqual(got, {sd.MET, sd.MISSED, sd.UNKNOWN}, f"{i} 行目")

    # --- (g) `heap_used + mmap` の増え (`--zero`)

    def test_the_growth_of_heap_used_plus_mmap_is_judged(self):
        with written(z=zero_snapshot()) as p:
            row = self.row(8, extra=("--zero", p["z"]))
        # 0 時間 4.5 + 8.0 = 12.5 MB、後 6.0 + 8.0 = 14.0 MB
        self.assertEqual(row[3], sd.MET)
        self.assertEqual(row[1], "< 5 MB")
        self.assertIn("**+1.5** MB (12.5 → 14.0 MB)", row[2])
        self.assertIn("`heap_used` 4.5 → 6.0、`mmap` 8.0 → 8.0", row[2])
        # `heap_free` と `rss` は並べるだけ (判定に使わない)
        self.assertIn("`heap_free` 0.4 → 1.2、`rss` 15.0 → 21.0 MB (`heap_free` と `rss` は表示だけ)",
                      row[2])
        self.assertIn("`--zero` (起動から 2.0 分) と後の雪像 (起動から 12.0 時間)", row[4])

    def test_five_megabytes_or_more_is_missed(self):
        with written(z=zero_snapshot(heap_used=1_000_000),
                     edge=zero_snapshot(heap_used=1_000_001)) as p:
            row = self.row(8, extra=("--zero", p["z"]))
            edge = self.row(8, extra=("--zero", p["edge"]))
        self.assertEqual(row[3], sd.MISSED)                  # ちょうど +5.0 は「未満」ではない
        self.assertIn("**+5.0** MB (9.0 → 14.0 MB)", row[2])
        self.assertEqual(edge[3], sd.MET)                    # 1 B でも下なら満たす

    def test_a_shrink_is_met_and_printed_with_its_sign(self):
        with written(z=zero_snapshot(heap_used=7_000_000)) as p:
            row = self.row(8, extra=("--zero", p["z"]))
        self.assertEqual(row[3], sd.MET)
        self.assertIn("**-1.0** MB", row[2])

    def test_without_zero_the_growth_cannot_be_judged(self):
        row = self.row(8)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`--zero` (0 時間の雪像) が渡されていない", row[4])

    def test_a_zero_snapshot_of_another_start_cannot_be_judged(self):
        """版が違う 1 枚と、同じ版でも後のスナップショットより後に起動した 1 枚 (`uptime_secs` が減る) は使わない。"""
        with written(other=zero_snapshot(version="0.1.0+aaaaaaa"),
                     later=zero_snapshot(uptime=50_000)) as p:
            other = self.row(8, extra=("--zero", p["other"]))
            later = self.row(8, extra=("--zero", p["later"]))
        self.assertEqual(other[3], sd.UNKNOWN)
        self.assertIn("同じ起動ではない (版が変わった", other[4])
        self.assertEqual(later[3], sd.UNKNOWN)
        self.assertIn("`uptime_secs` が減った", later[4])

    def test_a_memory_part_without_the_columns_cannot_be_judged(self):
        b = read(B)
        del b["status"]["memory"]
        with written(z=zero_snapshot(mmap=None), full=zero_snapshot(), b=b) as p:
            zero = self.row(8, extra=("--zero", p["z"]))
            after = self.row(8, b=p["b"], extra=("--zero", p["full"]))
        self.assertEqual(zero[3], sd.UNKNOWN)
        self.assertIn("0 時間の `mmap` が無い", zero[4])
        self.assertIn("`mmap` — → 8.0", zero[2])
        self.assertEqual(after[3], sd.UNKNOWN)
        self.assertIn("後の `heap_used`、後の `mmap` が無い", after[4])

    def test_zero_is_ignored_by_the_older_criteria(self):
        with written(z=zero_snapshot()) as p:
            for name in ("phase14", "phase15", "phase17"):
                argv = [A, B, "--no-dns", "--criteria", name]
                self.assertEqual(run(argv + ["--zero", p["z"]]), run(argv), name)

    # --- (h) `ipv6.request_probes`

    def test_no_request_probe_is_met(self):
        with written(b=with_probes(read(B), 0)) as p:
            row = self.row(9, b=p["b"])
        self.assertEqual(row[3], sd.MET)
        self.assertEqual(row[1], "= 0")
        self.assertIn("**0** 回、`canary.ipv6_runs` 700 / `ipv6_skipped` 20、`ipv6.attempts` 4", row[2])
        self.assertIn("起動からの 12.0 時間 の通算", row[4])

    def test_a_request_probe_is_missed_with_the_canary_counts_next_to_it(self):
        with written(b=with_probes(read(B), 3, runs=690, skipped=30, at=1789050000)) as p:
            row = self.row(9, b=p["b"])
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**3** 回 (最後は 2026-09-10 14:20:00Z)、"
                      "`canary.ipv6_runs` 690 / `ipv6_skipped` 30", row[2])

    def test_a_request_probe_without_the_canary_columns_still_prints(self):
        b = read(B)
        b["status"]["ipv6"]["request_probes"] = 2
        with written(b=b) as p:
            row = self.row(9, b=p["b"])
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**2** 回、`canary.ipv6_runs` — / `ipv6_skipped` —", row[2])

    def test_a_version_without_the_column_cannot_be_judged(self):
        row = self.row(9)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertEqual(row[2], "—")
        self.assertIn("`request_probes` が無い (T18.1 より前の版)", row[4])

    def test_the_markdown_names_the_phase18_parts(self):
        md = run([A, B, "--no-dns", "--criteria", "phase18"])
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria phase18`)", md)
        self.assertIn("前の 8 行は phase17 と同じ物差し", md)
        self.assertNotIn("前の 4 行は phase15 と同じ物差しです", md)
        self.assertIn("**その部が雪像に無い行は「判定できず」**", md)
        self.assertNotIn("**日次の雪像**", md)              # `--daily-snapshots` を渡したときだけ


class DailySnapshots(unittest.TestCase):
    """`--daily-snapshots DIR` (T18.2): 同じ版の日次のスナップショットを撮った時刻の順に並べる。"""

    @staticmethod
    def daily(uptime, version="0.1.0+bbbbbbb", **memory):
        snap = zero_snapshot(version=version, uptime=uptime)
        snap["status"]["memory"].update(memory)
        snap["status"]["ipv6"] = {"attempts": uptime // 10_000, "wins": 0, "losses": 0}
        return snap

    @contextlib.contextmanager
    def directory(self):
        with tempfile.TemporaryDirectory(prefix="t182-") as tmp:
            files = {
                # 名前の順と撮った順をわざと逆にしてある (並べるのは `taken_at`)
                "2026-01-01T000000Z-snapshot.json": self.daily(
                    40_000, rss=20_000_000, heap_free=1_100_000,
                    rings_used={"history": 1, "total": 4_900_000}),
                "2026-01-02T000000Z-snapshot.json": self.daily(30_000, rss=18_000_000),
                "2026-01-03T000000Z-snapshot.json": self.daily(20_000, version="0.1.0+aaaaaaa"),
                # 日次ではない 1 枚 (同じ版でも並べない) と、匿名化した写し
                "2026-01-03T101010Z-snapshot.json": self.daily(10_000),
                "2026-01-04T000000Z-snapshot.anon.json": self.daily(5_000),
            }
            for name, body in files.items():
                with open(os.path.join(tmp, name), "w", encoding="utf-8") as f:
                    json.dump(body, f)
            with open(os.path.join(tmp, "2026-01-05T000000Z-snapshot.json"), "w") as f:
                f.write("{ 途中で切れた")                    # 読めない 1 枚は飛ばす
            yield tmp

    def test_the_same_version_is_listed_in_the_order_taken(self):
        with self.directory() as tmp:
            ds = build([A, B, "--no-dns", "--daily-snapshots", tmp])["daily_snapshots"]
        self.assertEqual((ds["dir"], ds["version"], ds["files"]), (tmp, "0.1.0+bbbbbbb", 4))
        self.assertEqual([r["file"] for r in ds["rows"]],
                         ["2026-01-02T000000Z-snapshot.json", "2026-01-01T000000Z-snapshot.json"])
        self.assertEqual([r["uptime_secs"] for r in ds["rows"]], [30_000, 40_000])
        self.assertEqual(ds["rows"][1], {
            "file": "2026-01-01T000000Z-snapshot.json", "taken_at": Criteria17.START + 40_000,
            "uptime_secs": 40_000, "rss": 20_000_000, "heap_used": 4_500_000,
            "heap_free": 1_100_000, "mmap": 8_000_000, "rings_used_total": 4_900_000,
            "ipv6_attempts": 4})
        # 欄の無い版 (`rings_used` が無い) は `None` (0 と混ぜない)
        self.assertIsNone(ds["rows"][0]["rings_used_total"])

    def test_the_table_is_printed_under_the_criteria(self):
        with self.directory() as tmp:
            md = run([A, B, "--no-dns", "--criteria", "phase18", "--daily-snapshots", tmp])
        tail = md[md.index("## 9. 完了の定義に対する判定"):]
        self.assertLess(tail.index("判定できず 6 行"), tail.index("**日次の雪像**"))
        self.assertIn("4 枚のうち、後の雪像と同じ版 `0.1.0+bbbbbbb` の **2 枚**", tail)
        self.assertIn("| 雪像 | 起動から (時間) | `rss` | `heap_used` | `heap_free` | `mmap` "
                      "| `rings_used.total` | `ipv6.attempts` |", tail)
        self.assertIn("| `2026-01-02T000000Z-snapshot.json` | 8.3 | 18.0 | 4.5 | 0.4 | 8.0 | — | 3 |",
                      tail)
        self.assertIn("| `2026-01-01T000000Z-snapshot.json` | 11.1 | 20.0 | 4.5 | 1.1 | 8.0 | 4.9 | 4 |",
                      tail)
        self.assertEqual(tail.count("T000000Z-snapshot.json` |"), 2)

    def test_a_directory_without_the_version_says_so(self):
        with tempfile.TemporaryDirectory(prefix="t182-") as tmp:
            md = run([A, B, "--no-dns", "--daily-snapshots", tmp])
        self.assertIn("0 枚のうち、後の雪像と同じ版 `0.1.0+bbbbbbb` の **0 枚**", md)
        self.assertIn("(同じ版の日次の雪像が無い。", md)

    def test_without_the_flag_nothing_is_added(self):
        self.assertNotIn("daily_snapshots", build([A, B, "--no-dns", "--criteria", "phase18"]))


if __name__ == "__main__":
    unittest.main()
