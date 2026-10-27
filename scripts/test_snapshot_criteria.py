#!/usr/bin/env python3
"""`scripts/snapshot-diff.py --criteria` (判定表。中身は `scripts/snapshot_criteria.py`) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

`test_snapshot_diff.py` が 1,964 行 (目安の 2,000 行の手前) になったので、判定表のクラス
(`Criteria*` / `Deployed17` 以降の `Deployed*`) と、判定表の下に付く `--daily-snapshots` の表の
クラスをここへ割った (TODO.md T21.2)。中身は割る前と同じで、雪像の読み方と小物 (`sd` `build`
`run` `read` `written` …) と架空の雪像 2 枚 (`testdata/snapshot-{a,b}.json`) は
`test_snapshot_diff.py` のものをそのまま使う。`Deployed*` はデプロイ先の雪像 (リポジトリの
`status/`。リポジトリには入れない) があるときだけ回る。
"""

import contextlib
import json
import os
import sys
import tempfile
import unittest
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
# 無言の再起動の行 (T23.2) は `sd` (snapshot_diff) が名前で取り込んでいないので、置き場から直接読む
import snapshot_criteria as sc  # noqa: E402
# **TestCase のクラスは取り込まない** (取り込むと同じテストがこのファイルでも 2 回走る)
from test_snapshot_diff import (  # noqa: E402
    A,
    B,
    DEPLOYED,
    build,
    read,
    run,
    sd,
    set_column,
    with_warm_columns,
    written,
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
        """**閾は 1 名前あたりの上限**なので、`/dns` の名前ごとの最大で判定する (T15.15 (1))。"""
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
        """**下の閾は外した** (T15.15 (2)。T15.99 の 0.05 が「幅より低い」で届かずになった)。"""
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
        """**全数の割合**で比べる (T15.15 (3)。`/recent` は 256 KiB で切れて窓が毎回違う)。"""
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
                         ["phase14", "phase15", "phase17", "phase18", "phase20", "phase21",
                          "phase22", "phase23", "phase24", "phase25", "phase26"])


def conn_profile(conn_us_per_row):
    """B の `/profile` (60 秒 × 3 標本、要求 3 / 2 / 4 本) の conn 役の CPU だけ書き換えた写し。"""
    p = json.loads(json.dumps(read(B)["profile"]))
    ti, ri = p["keys"].index("threads"), p["roles"].index("conn")
    for row in p["samples"]:
        row[ti][ri][0] = conn_us_per_row
    return p


class Criteria17(unittest.TestCase):
    """`--criteria phase17` の 8 行 (T17.0a。前の 4 行は phase15 の関数、後ろの 4 行が T16.99 の判定)。

    雪像は `testdata/snapshot-{a,b}.json` に**作り物の欄を足して**使う (ファイルは書き換えない)。
    B の起動は 1789086400 − 43200 = 1789043200 (12 時間前)。
    """

    START = 1789043200

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
        """A には `/profile` が無い。後は B の雪像の部 (80,000 us/要求) で「参考」。"""
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

    @staticmethod
    def warm(snap, peak, evicted):
        """後の期間の 1 時間の行 (バーストの行も) に `dns_warm_max` を足し、`warm_evicted` を置く。

        B の `dns_warm` は 24 / 26 / 28 なので、最大はそれより下にならない (`aggregate` の下限)。
        """
        with_warm_columns(snap, {t: (peak, 17568, 720)
                                 for t in (1789045200, 1789048800, 1789052400)})
        snap["status"]["dns"]["warm_evicted"] = evicted
        return snap

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


@unittest.skipUnless(all(os.path.isfile(os.path.join(DEPLOYED, f)) for f in (
    "2026-09-24T093400Z-snapshot.json", "2026-09-26T114602Z-snapshot.json",
    "2026-09-26T114602Z-profile_res_60.json")), "デプロイ先の雪像が無い (リポジトリには入れない)")
class Deployed17(unittest.TestCase):
    """T17.0a の受け入れ基準: 2026-09-24 → 2026-09-26 の 2 枚で T16.99 の `結果:` と同じ数字。"""

    def test_the_numbers_of_t1699(self):
        rows = build([os.path.join(DEPLOYED, "2026-09-24T093400Z-snapshot.json"),
                      os.path.join(DEPLOYED, "2026-09-26T114602Z-snapshot.json"),
                      "--no-dns", "--criteria", "phase17", "--profile",
                      os.path.join(DEPLOYED, "2026-09-26T114602Z-profile_res_60.json")]
                     )["criteria"]["rows"]
        self.assertIn("**0.0013** コア", rows[4][2])
        self.assertIn("最大 **15** 件、`warm_evicted` **0**", rows[5][2])
        # 29 件 ÷ 起動から 47.16 時間 = 0.615 (T16.99 は 47 時間で割って 0.62 と書いた)
        self.assertIn("`dns_slow` **0.61** 件/時 (29 件、前 0.09)", rows[6][2])
        self.assertIn("後 **0.0022 コア、user 31%**", rows[7][2])
        self.assertEqual(rows[7][3], sd.UNKNOWN)            # 前の版に `usage_usec` が無い


def zero_snapshot(heap_used=4_500_000, mmap=8_000_000, version="0.1.0+bbbbbbb", uptime=120):
    """B と同じ起動の「0 時間の雪像」(起動 2 分後)。B の `memory` は `heap_used` 6.0 + `mmap` 8.0 MB。"""
    memory = {"rss": 15_000_000, "heap_used": heap_used, "heap_free": 400_000, "mmap": mmap}
    return {"taken_at": Criteria17.START + uptime, "version": version, "uptime_secs": uptime,
            "parts": ["status"], "dropped": [],
            "status": {"version": version, "uptime_secs": uptime, "since_start_secs": uptime,
                       "memory": {k: v for k, v in memory.items() if v is not None}}}


def with_probes(snap, probes, runs=700, skipped=20, at=0):
    """`/status` に T18.1 の欄 (`ipv6.request_probes` と `canary.ipv6_runs` / `ipv6_skipped`) を足す。"""
    snap["status"]["ipv6"].update(request_probes=probes, request_probe_at=at)
    snap["status"]["canary"] = {"mode": "on", "runs": runs + skipped, "failures": 0,
                                "ipv6_runs": runs, "ipv6_skipped": skipped}
    return snap


class Criteria18(unittest.TestCase):
    """`--criteria phase18` の 10 行 (T18.2。前の 8 行は phase17 の関数、後ろの 2 行が T18.0 の (g)(h))。

    雪像は `testdata/snapshot-{a,b}.json` に**作り物の欄を足して**使う (T18.1 の欄は名前だけ同じ)。
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
        """版が違う 1 枚と、同じ版でも後の雪像より後に起動した 1 枚 (`uptime_secs` が減る) は使わない。"""
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
    """`--daily-snapshots DIR` (T18.2 (2)): 同じ版の日次の雪像を撮った時刻の順に並べる。"""

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


@unittest.skipUnless(all(os.path.isfile(os.path.join(DEPLOYED, f)) for f in (
    "2026-09-26T151308Z-snapshot.json", "2026-09-26T151308Z-profile_res_60.json",
    "2026-09-26T152424Z-snapshot.json", "2026-10-01T125100Z-snapshot.json",
    "2026-10-01T125100Z-daily.json", "2026-10-01T125100Z-profile_res_60.json",
    "2026-09-30T000000Z-snapshot.json")), "デプロイ先の雪像が無い (リポジトリには入れない)")
class Deployed18(unittest.TestCase):
    """T18.2 の受け入れ基準: 2026-09-26 → 2026-10-01 + 0 時間の雪像で T17.99 の `結果:` と同じ数字。"""

    def setUp(self):
        def at(name):
            return os.path.join(DEPLOYED, name)
        self.argv = [at("2026-09-26T151308Z-snapshot.json"), at("2026-10-01T125100Z-snapshot.json"),
                     "--no-dns", "--daily", at("2026-10-01T125100Z-daily.json"),
                     "--profile", at("2026-10-01T125100Z-profile_res_60.json"),
                     "--profile-before", at("2026-09-26T151308Z-profile_res_60.json")]
        self.d = build(self.argv + ["--criteria", "phase18", "--zero",
                                    at("2026-09-26T152424Z-snapshot.json"),
                                    "--daily-snapshots", DEPLOYED])

    def test_the_numbers_of_t1799(self):
        c = self.d["criteria"]
        rows = c["rows"]
        self.assertEqual(c["tally"], {sd.MET: 9, sd.MISSED: 0, sd.UNKNOWN: 1})
        # 前の 8 行は phase17 の表そのもの (T17.99 の「8 行とも満たした」)
        self.assertEqual(rows[:8], build(self.argv + ["--criteria", "phase17"])["criteria"]["rows"])
        self.assertIn("**0.01** (8 ミス / 598 要求)", rows[0][2])
        self.assertIn("**79.9** 回/時", rows[1][2])
        self.assertIn("**0.04** 回/接続、日ごとの幅 0.02〜0.07", rows[2][2])
        self.assertIn("**1.10** 倍 (1,237 → 1,357 us/要求)", rows[4][2])
        self.assertIn("最大 **18** 件、`warm_evicted` **0**", rows[5][2])
        self.assertIn("`dns_slow` **0.02** 件/時 (2 件、前 0.59)", rows[6][2])
        self.assertIn("0.0021 コア、user 31% → **0.0014 コア、user 38%**", rows[7][2])
        # (g) は +1.9 MB で満たし、(h) はこの版に欄が無い
        self.assertEqual(rows[8][3], sd.MET)
        self.assertIn("**+1.9** MB (18.0 → 19.9 MB)", rows[8][2])
        self.assertIn("`heap_free` 4.3 → 11.3、`rss` 31.6 → 40.9 MB", rows[8][2])
        self.assertEqual(rows[9][3], sd.UNKNOWN)

    def test_the_daily_snapshots_of_this_version(self):
        rows = self.d["daily_snapshots"]["rows"]
        self.assertEqual([f"{r['uptime_secs'] / 3600:.1f}" for r in rows],
                         ["8.6", "32.6", "56.6", "80.6", "104.6"])
        self.assertEqual([r["ipv6_attempts"] for r in rows], [3, 6, 6, 6, 6])
        self.assertEqual([f"{r['heap_free'] / 1e6:.1f}" for r in rows],
                         ["7.5", "8.2", "11.4", "11.3", "11.4"])


def cached(snap, cache, heap_used=None, mmap=None):
    """`/status` の `memory` に `cache_memory` (と、あれば `heap_used` / `mmap`) を置く。"""
    m = snap["status"]["memory"]
    if cache is None:
        m.pop("cache_memory", None)
    else:
        m["cache_memory"] = cache
    if heap_used is not None:
        m["heap_used"] = heap_used
    if mmap is not None:
        m["mmap"] = mmap
    return snap


class Criteria20(unittest.TestCase):
    """`--criteria phase20` (T20.1): phase18 の 10 行の物差しを直したもの。

    (g) は `cache_memory` を引いてから比べ、ミス率・`timeout`・`dns_warm` の最大の 3 行は表示だけ。
    """

    SHOWN_ROWS = (0, 3, 5)

    def judge(self, name="phase20", a=None, b=None, extra=()):
        return build([a or A, b or B, "--no-dns", "--criteria", name, *extra])["criteria"]

    def row(self, i, a=None, b=None, extra=()):
        return self.judge(a=a, b=b, extra=extra)["rows"][i]

    def test_phase20_gives_ten_rows_and_three_of_them_are_display_only(self):
        with written(z=cached(zero_snapshot(), 0), b=with_probes(read(B), 0)) as p:
            c = self.judge(b=p["b"], extra=("--zero", p["z"]))
        self.assertEqual(len(sd.RULES["phase20"]), 10)
        self.assertEqual(len(c["rows"]), 10)
        self.assertEqual([i for i, r in enumerate(c["rows"]) if r[3] == sd.SHOWN],
                         list(self.SHOWN_ROWS))
        # 表示だけの行は集計に数えない (鍵は今までの 3 つのまま、本数は別の鍵)
        self.assertEqual(sorted(c["tally"]), sorted((sd.MET, sd.MISSED, sd.UNKNOWN)))
        self.assertEqual(sum(c["tally"].values()), 7)
        self.assertEqual(c["display_only"], 3)
        self.assertEqual([r[3] for r in c["rows"][8:]], [sd.MET, sd.MET])

    def test_the_other_six_rows_are_the_phase18_ones(self):
        self.assertEqual(sd.PHASE20, sd.PHASE18)
        for i in (1, 2, 4, 6, 7, 9):
            self.assertIs(sd.RULES["phase20"][i], sd.RULES["phase18"][i], i)
        rows18, rows20 = self.judge("phase18")["rows"], self.judge()["rows"]
        for i in (1, 2, 4, 6, 7, 9):
            self.assertEqual(rows20[i], rows18[i], i)

    def test_a_display_only_row_keeps_the_number_and_drops_the_verdict(self):
        """phase18 なら「届かず」になる材料でも、phase20 は同じ実測を出して「表示だけ」。"""
        b = Criteria17.warm(read(B), 32, 26)                 # 枠が埋まり、追い出しが 26
        host = dict(next(h for h in b["hosts"]["hosts"] if h["host"].startswith("connect://")),
                    host="connect://discord.com:443", requests=100, dns_misses=23)
        b["hosts"]["hosts"].append(host)                     # A には居ない = 100 要求ぶんが差分
        i = b["history"]["3600"]["keys"].index("errors_by_cause")
        b["history"]["3600"]["samples"][-1][i] = [0, 0, 0, 9, 0, 0, 0, 0]   # timeout 9 件
        with written(b=b) as p:
            c18, c20 = self.judge("phase18", b=p["b"]), self.judge(b=p["b"])
        for i in self.SHOWN_ROWS:
            old, new = c18["rows"][i], c20["rows"][i]
            self.assertEqual(old[3], sd.MISSED, i)
            self.assertEqual(new[3], sd.SHOWN, i)
            self.assertEqual(new[2], old[2], i)              # 実測の欄は同じ文字
            self.assertEqual(new[1], f"— (phase18 は {old[1]})", i)
            self.assertTrue(new[4].startswith(old[4] + "。**閾を置かない**: "), i)
        self.assertIn("**0.23** (23 ミス / 100 要求)", c20["rows"][0][2])
        self.assertIn("最大 **32** 件、`warm_evicted` **26**", c20["rows"][5][2])
        self.assertEqual(c20["rows"][0][0], "`discord.com` のミス率")
        self.assertEqual(c20["rows"][3][0], "エラーの `timeout` (件/時。前の期間と並べる)")
        self.assertEqual(c20["rows"][5][0], "`dns_warm` の最大 (枠 32) と `dns.warm_evicted`")
        self.assertEqual(c18["tally"][sd.MISSED], 3)
        self.assertEqual(c20["tally"][sd.MISSED], 0)
        self.assertNotIn("display_only", c18)

    def test_a_display_only_row_without_its_part_still_says_why(self):
        row = self.row(0)                                    # testdata に `discord.com` は無い
        self.assertEqual(row[3], sd.SHOWN)
        self.assertEqual(row[2], "—")
        self.assertIn("`/hosts` の差分に `discord.com` が無い", row[4])

    # --- (g) `heap_used + mmap − cache_memory`

    def test_the_cache_is_taken_out_of_the_growth(self):
        """T18.99 の形: キャッシュが 13.0 MB 入って `heap_used` と `mmap` が同じだけ増えた。"""
        b = cached(read(B), 13_000_000, heap_used=10_000_000, mmap=17_000_000)
        with written(z=cached(zero_snapshot(), 0), b=b) as p:
            old = self.judge("phase18", b=p["b"], extra=("--zero", p["z"]))["rows"][8]
            row = self.row(8, b=p["b"], extra=("--zero", p["z"]))
        self.assertEqual(old[3], sd.MISSED)                  # 12.5 → 27.0 で +14.5
        self.assertIn("**+14.5** MB (12.5 → 27.0 MB)", old[2])
        self.assertEqual(row[3], sd.MET)                     # キャッシュを引くと 12.5 → 14.0
        self.assertEqual(row[0], "`heap_used + mmap − cache_memory` の 0 時間の雪像からの増えが 5 MB 未満")
        self.assertEqual(row[1], "< 5 MB")
        self.assertIn("**+1.5** MB (12.5 → 14.0 MB)", row[2])
        self.assertIn("`heap_used` 4.5 → 10.0、`mmap` 8.0 → 17.0、`cache_memory` 0.0 → 13.0、"
                      "`heap_free` 0.4 → 1.2、`rss` 15.0 → 21.0 MB (`heap_free` と `rss` は表示だけ)",
                      row[2])
        self.assertIn("`--zero` (起動から 2.0 分) と後の雪像 (起動から 12.0 時間)", row[4])

    def test_a_cache_in_the_zero_snapshot_is_taken_out_too(self):
        with written(z=cached(zero_snapshot(), 2_000_000)) as p:
            row = self.row(8, extra=("--zero", p["z"]))      # B の `cache_memory` は 0
        self.assertIn("**+3.5** MB (10.5 → 14.0 MB)", row[2])
        self.assertEqual(row[3], sd.MET)

    def test_five_megabytes_or_more_without_the_cache_is_missed(self):
        with written(z=cached(zero_snapshot(heap_used=1_000_000), 0),
                     edge=cached(zero_snapshot(heap_used=1_000_001), 0)) as p:
            row = self.row(8, extra=("--zero", p["z"]))
            edge = self.row(8, extra=("--zero", p["edge"]))
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**+5.0** MB (9.0 → 14.0 MB)", row[2])
        self.assertEqual(edge[3], sd.MET)

    def test_a_version_without_cache_memory_is_not_read_as_zero(self):
        with written(z=zero_snapshot(), full=cached(zero_snapshot(), 0),
                     b=cached(read(B), None)) as p:
            zero = self.row(8, extra=("--zero", p["z"]))     # 0 時間の雪像に欄が無い
            after = self.row(8, b=p["b"], extra=("--zero", p["full"]))
        self.assertEqual(zero[3], sd.UNKNOWN)
        self.assertIn("0 時間の `cache_memory` が無い", zero[4])
        self.assertIn("`cache_memory` — → 0.0", zero[2])
        self.assertEqual(after[3], sd.UNKNOWN)
        self.assertIn("後の `cache_memory` が無い", after[4])

    def test_without_zero_or_with_another_start_the_growth_cannot_be_judged(self):
        self.assertEqual(self.row(8)[3], sd.UNKNOWN)
        self.assertIn("`--zero` (0 時間の雪像) が渡されていない", self.row(8)[4])
        with written(other=cached(zero_snapshot(version="0.1.0+aaaaaaa"), 0)) as p:
            other = self.row(8, extra=("--zero", p["other"]))
        self.assertEqual(other[3], sd.UNKNOWN)
        self.assertIn("同じ起動ではない (版が変わった", other[4])

    # --- 印字と、古い定義を変えていないこと

    def test_the_markdown_names_the_phase20_parts(self):
        md = run([A, B, "--no-dns", "--criteria", "phase20"])
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria phase20`)", md)
        self.assertIn("phase18 と同じ並びの 10 行", md)
        self.assertIn("**メモリのキャッシュ (`memory.cache_memory`) を引いてから**", md)
        self.assertNotIn("前の 8 行は phase17 と同じ物差し", md)
        self.assertIn("| **表示だけ** |", md)
        self.assertIn("満たした 3 行・判定できず 4 行 (ほかに表示だけ 3 行。集計に数えない)", md)

    def test_the_older_criteria_do_not_mention_display_only(self):
        for name in ("phase14", "phase15", "phase17", "phase18"):
            md = run([A, B, "--no-dns", "--criteria", name])
            self.assertNotIn("表示だけ 3 行", md, name)
            self.assertNotIn("| **表示だけ** |", md, name)


@unittest.skipUnless(all(os.path.isfile(os.path.join(DEPLOYED, f)) for f in (
    "2026-10-01T133549Z-snapshot.json", "2026-10-01T133549Z-profile_res_60.json",
    "2026-10-01T133907Z-snapshot.json", "2026-10-08T092521Z-snapshot.json",
    "2026-10-08T092521Z-daily.json", "2026-10-08T092521Z-profile_res_60.json")),
    "デプロイ先の雪像が無い (リポジトリには入れない)")
class Deployed20(unittest.TestCase):
    """T20.1 の受け入れ基準: 2026-10-01 → 2026-10-08 + 0 時間の雪像 (T18.99 の 3 枚) の phase20。"""

    def setUp(self):
        def at(name):
            return os.path.join(DEPLOYED, name)
        self.argv = [at("2026-10-01T133549Z-snapshot.json"), at("2026-10-08T092521Z-snapshot.json"),
                     "--no-dns", "--daily", at("2026-10-08T092521Z-daily.json"),
                     "--profile", at("2026-10-08T092521Z-profile_res_60.json"),
                     "--profile-before", at("2026-10-01T133549Z-profile_res_60.json"),
                     "--zero", at("2026-10-01T133907Z-snapshot.json")]

    def test_phase18_is_what_t1899_wrote(self):
        c = build(self.argv + ["--criteria", "phase18"])["criteria"]
        self.assertEqual(c["tally"], {sd.MET: 6, sd.MISSED: 4, sd.UNKNOWN: 0})
        self.assertIn("**+15.7** MB (17.9 → 33.6 MB)", c["rows"][8][2])

    def test_phase20_has_no_missed_row(self):
        d = build(self.argv + ["--criteria", "phase20"])
        c, rows = d["criteria"], d["criteria"]["rows"]
        self.assertEqual(c["tally"], {sd.MET: 7, sd.MISSED: 0, sd.UNKNOWN: 0})
        self.assertEqual(c["display_only"], 3)
        self.assertEqual([r[3] for r in rows].count(sd.SHOWN), 3)
        self.assertIn("**0.23** (82 ミス / 351 要求)", rows[0][2])
        self.assertIn("**0.05** 件/時 (前 0.02 件/時)", rows[3][2])
        self.assertIn("最大 **32** 件、`warm_evicted` **26**", rows[5][2])
        # (g): キャッシュ 13.5 MB を引いて +2.2 MB
        self.assertEqual(rows[8][3], sd.MET)
        self.assertIn("**+2.2** MB (17.9 → 20.2 MB)", rows[8][2])
        self.assertIn("`cache_memory` 0.0 → 13.5", rows[8][2])
        # 成功した接続だけ: `/recent` の 398 本 (T18.99 の p50 7 / p95 66 ms)
        rc = d["recent_connect"]
        self.assertEqual((rc["n"], rc["p50"], rc["p95"], rc["failed"]), (398, 7, 66, 178))
        self.assertGreater(d["history"]["after"]["connect_p95"], 2000)

    def test_phase21_comes_out_too(self):
        """T21.2: nagoya の同じ 3 枚でも phase21 が出る (前の `/profile` が 12 時間あるので参考の行は無い)。"""
        c = build(self.argv + ["--criteria", "phase21"])["criteria"]
        self.assertEqual(c["tally"], {sd.MET: 6, sd.MISSED: 0, sd.UNKNOWN: 0})
        self.assertEqual(c["display_only"], 4)
        self.assertNotIn("reference", c)
        self.assertIn("`cold` 174、`expired` 505", c["rows"][2][2])


def long_profile(conn_us_per_row, rows):
    """`conn_profile` の 3 標本を `rows` 本に伸ばした写し (60 秒おき。要求と CPU は 3 本ずつの繰り返し)。"""
    p = conn_profile(conn_us_per_row)
    base = p["samples"]
    t0 = base[0][0]
    p["samples"] = [[t0 + 60 * i] + json.loads(json.dumps(base[i % len(base)][1:])) for i in range(rows)]
    p["count"] = p["shown"] = rows
    return p


class Criteria21(unittest.TestCase):
    """`--criteria phase21` (T21.2): phase20 の 10 行のうち、T20.99 で届かなかった 3 行の読み方を直したもの。

    平常時のミスは表示だけ (日ごとの幅と `misses_by_kind` の内訳)、conn 役の CPU/要求 は前か後の
    `/profile` が 12 時間に満たなければ「参考」、`ipv6.request_probes` は 0 時間の雪像から 3 時間以内なら
    「満たした」。B の起動は `Criteria17.START`、`zero_snapshot()` はその 2 分後に撮った 1 枚。
    """

    SHOWN_ROWS = (0, 2, 3, 5)
    ZERO_AT = Criteria17.START + 120

    def judge(self, name="phase21", a=None, b=None, extra=()):
        return build([a or A, b or B, "--no-dns", "--criteria", name, *extra])["criteria"]

    def row(self, i, a=None, b=None, extra=()):
        return self.judge(a=a, b=b, extra=extra)["rows"][i]

    def test_phase21_gives_ten_rows_and_four_of_them_are_display_only(self):
        with written(z=cached(zero_snapshot(), 0), b=with_probes(read(B), 0)) as p:
            c = self.judge(b=p["b"], extra=("--zero", p["z"]))
        self.assertEqual(len(sd.RULES["phase21"]), 10)
        self.assertEqual(len(c["rows"]), 10)
        self.assertEqual([i for i, r in enumerate(c["rows"]) if r[3] == sd.SHOWN],
                         list(self.SHOWN_ROWS))
        self.assertEqual(sorted(c["tally"]), sorted((sd.MET, sd.MISSED, sd.UNKNOWN)))
        self.assertEqual(sum(c["tally"].values()), 6)
        self.assertEqual(c["display_only"], 4)
        self.assertNotIn("reference", c)                     # 参考の行が無ければ鍵も無い
        self.assertEqual([r[3] for r in c["rows"][8:]], [sd.MET, sd.MET])

    def test_the_other_rows_are_the_phase20_ones(self):
        self.assertEqual(sd.PHASE21, dict(sd.PHASE20, profile_min_hours=12.0,
                                          request_probe_grace_hours=3.0))
        for i in (0, 1, 3, 5, 6, 7, 8):
            self.assertIs(sd.RULES["phase21"][i], sd.RULES["phase20"][i], i)
        with written(z=cached(zero_snapshot(), 0)) as p:
            rows20 = self.judge("phase20", extra=("--zero", p["z"]))["rows"]
            rows21 = self.judge(extra=("--zero", p["z"]))["rows"]
        for i in (0, 1, 3, 5, 6, 7, 8):
            self.assertEqual(rows21[i], rows20[i], i)

    # --- 平常時のミス (表示だけ)

    def test_the_miss_row_is_display_only_with_the_kinds(self):
        row = self.row(2)
        old = self.judge("phase20")["rows"][2]
        self.assertEqual(old[3], sd.MET)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertEqual(row[0], "平常時の名前解決のミス (回/接続。日ごとの幅と種類別の内訳)")
        self.assertEqual(row[1], "— (phase20 は ≤ 0.09)")
        # A → B は再起動をまたぐので、内訳は B の起動からの値
        self.assertEqual(row[2], old[2] + "。ミスの内訳 (起動から、バースト込み): "
                                          "`cold` 12、`expired` 24、`warm_stale` 3、`negative` 1")
        self.assertIn("内訳は `/status` の `dns.misses_by_kind`。**閾を置かない**: ", row[4])

    def test_a_high_miss_rate_is_only_shown(self):
        b = read(B)
        set_column(b, "3600", "dns_misses", 40)              # 0.20 (phase20 なら届かず)
        with written(b=b) as p:
            old = self.judge("phase20", b=p["b"])["rows"][2]
            row = self.row(2, b=p["b"])
        self.assertEqual(old[3], sd.MISSED)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertIn("**0.20** 回/接続", row[2])

    def test_the_daily_band_comes_before_the_kinds(self):
        days = {"days": [{"day": "2026-09-20", "connects": 100, "version": "0.1.0+bbbbbbb",
                          "dns_per_connect": 0.05},
                         {"day": "2026-09-21", "connects": 100, "version": "0.1.0+bbbbbbb",
                          "dns_per_connect": 0.24}]}
        with written(d=days) as p:
            row = self.row(2, extra=("--daily", p["d"]))
        self.assertIn("、日ごとの幅 0.05〜0.24 (2 日)。ミスの内訳 (起動から", row[2])

    def test_the_kinds_are_a_difference_without_a_restart(self):
        a, b = read(B), read(B)
        a["status"]["dns"]["misses_by_kind"] = {"cold": 2, "expired": 4, "warm_stale": 3, "negative": 1}
        b["status"]["dns"]["misses_by_kind"]["refused"] = 5        # 知らない種類は名前の順で後ろ
        c = {"a": a, "b": b, "info": {"restarted": False}}
        self.assertEqual(sd.miss_kinds(c), ({"cold": 10, "expired": 20, "warm_stale": 0,
                                             "negative": 0, "refused": 5}, True))
        self.assertEqual(sd.miss_kinds(dict(c, info={"restarted": True}))[0]["cold"], 12)

    def test_a_version_without_the_kinds_says_so(self):
        b = read(B)
        del b["status"]["dns"]["misses_by_kind"]
        with written(b=b) as p:
            row = self.row(2, b=p["b"])
        self.assertEqual(row[3], sd.SHOWN)
        self.assertNotIn("ミスの内訳", row[2])
        self.assertIn("`misses_by_kind` が無い (内訳は出せない)", row[4])

    # --- conn 役の CPU/要求 (短い材料は参考)

    def test_a_short_profile_is_only_a_reference(self):
        """testdata の `/profile` は 3 標本 (3 分) なので、倍率が出ても「参考」で集計に数えない。"""
        with written(fast=conn_profile(200_000), slow=conn_profile(150_000)) as p:
            met20 = self.judge("phase20", extra=("--profile-before", p["fast"]))["rows"][4]
            c = self.judge(extra=("--profile-before", p["slow"]))
        row = c["rows"][4]
        self.assertEqual(met20[3], sd.MET)
        self.assertEqual(row[3], sd.REFERENCE)
        self.assertIn("**1.60** 倍", row[2])
        self.assertIn("。**参考**: 前は 0.1 時間 (3 標本)、後は 0.1 時間 (3 標本) で、12 時間 "
                      "(`/profile?res=60` で 720 標本) に満たない", row[4])
        self.assertIn("倍率だけなら「届かず」でも集計に数えない", row[4])
        self.assertEqual(c["reference"], 1)
        self.assertEqual(sum(c["tally"].values()), 5)

    def test_twelve_hours_on_both_sides_is_judged(self):
        with written(after=long_profile(240_000, 720), before=long_profile(200_000, 720),
                     slow=long_profile(150_000, 720), short=long_profile(200_000, 719)) as p:
            met = self.row(4, extra=("--profile", p["after"], "--profile-before", p["before"]))
            missed = self.row(4, extra=("--profile", p["after"], "--profile-before", p["slow"]))
            short = self.row(4, extra=("--profile", p["after"], "--profile-before", p["short"]))
        self.assertEqual(met[3], sd.MET)                     # 1.20 倍
        self.assertNotIn("**参考**", met[4])
        self.assertEqual(missed[3], sd.MISSED)               # 1.60 倍
        self.assertEqual(short[3], sd.REFERENCE)             # 719 標本 = 12 時間に 1 分足りない
        self.assertIn("前は 12.0 時間 (719 標本) で", short[4])
        self.assertNotIn("後は", short[4].split("**参考**")[1])

    def test_without_a_profile_before_it_stays_unknown(self):
        row = self.row(4)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertEqual(row, self.judge("phase20")["rows"][4])

    # --- `ipv6.request_probes` (起動直後だけなら満たす)

    def probes_row(self, at, probes=3, zero=True):
        b = with_probes(read(B), probes, runs=690, skipped=30, at=at)
        with written(z=zero_snapshot(), b=b) as p:
            return self.row(9, b=p["b"], extra=("--zero", p["z"]) if zero else ())

    def test_probes_right_after_the_start_are_met(self):
        row = self.probes_row(self.ZERO_AT + 2 * 3600 + 1440)
        self.assertEqual(row[3], sd.MET)
        self.assertEqual(row[1], "= 0 か、最後が 0 時間 + 3 時間以内")
        self.assertIn("**3** 回 (最後は ", row[2])
        self.assertIn("。最後は 0 時間の雪像から 2.4 時間後 = **起動直後の 3 回は canary が IPv6 を"
                      "見ていなかったぶん**", row[2])
        self.assertIn("、`--zero` の取得 ", row[4])
        # ちょうど 3 時間は「以内」
        self.assertEqual(self.probes_row(self.ZERO_AT + 3 * 3600)[3], sd.MET)

    def test_a_probe_later_than_three_hours_is_missed(self):
        row = self.probes_row(self.ZERO_AT + 3 * 3600 + 1)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("。最後は 0 時間の雪像から **3.0 時間後** (3 時間より後にも探った)", row[2])
        self.assertEqual(self.probes_row(self.ZERO_AT + 10 * 3600)[3], sd.MISSED)

    def test_no_probe_is_met_as_before(self):
        row = self.probes_row(0, probes=0)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("**0** 回、`canary.ipv6_runs` 690", row[2])
        self.assertEqual(self.row(9)[3], sd.UNKNOWN)         # 欄の無い版は今までどおり

    def test_probes_without_zero_or_from_another_start_cannot_be_judged(self):
        row = self.probes_row(self.ZERO_AT + 600, zero=False)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`--zero` (0 時間の雪像) が渡されていないので", row[4])
        b = with_probes(read(B), 3, at=self.ZERO_AT + 600)
        with written(other=zero_snapshot(version="0.1.0+aaaaaaa"), b=b) as p:
            other = self.row(9, b=p["b"], extra=("--zero", p["other"]))
        self.assertEqual(other[3], sd.UNKNOWN)
        self.assertIn("同じ起動ではない (版が変わった", other[4])
        self.assertEqual(self.probes_row(0)[3], sd.UNKNOWN)  # 回数はあるのに時刻が無い
        self.assertIn("`request_probe_at`", self.probes_row(0)[4])

    # --- 印字と、古い定義を変えていないこと

    def test_the_markdown_names_the_phase21_parts(self):
        with written(slow=conn_profile(150_000)) as p:
            md = run([A, B, "--no-dns", "--criteria", "phase21", "--profile-before", p["slow"]])
            js = json.loads(run([A, B, "--no-dns", "--criteria", "phase21", "--profile-before",
                                 p["slow"], "--out", "json"]))
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria phase21`)", md)
        self.assertIn("phase20 と同じ並びの 10 行", md)
        self.assertNotIn("**メモリのキャッシュ (`memory.cache_memory`) を引いてから**", md)
        self.assertIn("| **参考** |", md)
        self.assertIn("(ほかに表示だけ 4 行・参考 1 行。集計に数えない)", md)
        self.assertEqual((js["criteria"]["display_only"], js["criteria"]["reference"]), (4, 1))
        # 成功した接続だけの行は phase20 と同じく出る (`FROZEN` ではない)
        self.assertIn("成功した接続だけ", md)
        self.assertNotIn("phase21", sd.FROZEN)

    def test_the_older_criteria_do_not_mention_reference(self):
        with written(slow=conn_profile(150_000)) as p:
            for name in ("phase14", "phase15", "phase17", "phase18", "phase20"):
                md = run([A, B, "--no-dns", "--criteria", name, "--profile-before", p["slow"]])
                self.assertNotIn("参考 1 行", md, name)
                self.assertNotIn("| **参考** |", md, name)
                self.assertNotIn("reference", build([A, B, "--no-dns", "--criteria", name,
                                                     "--profile-before", p["slow"]])["criteria"], name)

    def test_zero_is_used_by_phase21_too(self):
        with written(z=cached(zero_snapshot(), 0)) as p:
            with_zero = run([A, B, "--no-dns", "--criteria", "phase21", "--zero", p["z"]])
        self.assertNotEqual(with_zero, run([A, B, "--no-dns", "--criteria", "phase21"]))


TOKYO = os.path.join(DEPLOYED, "tokyo")


@unittest.skipUnless(all(os.path.isfile(os.path.join(TOKYO, f)) for f in (
    "2026-10-08T104352Z-snapshot.json", "2026-10-08T104352Z-profile_res_60.json",
    "2026-10-08T104507Z-snapshot.json", "2026-10-15T185514Z-snapshot.json",
    "2026-10-15T185514Z-daily.json", "2026-10-15T185514Z-profile_res_60.json")),
    "デプロイ先の雪像が無い (リポジトリには入れない)")
class Deployed21(unittest.TestCase):
    """T21.2 の受け入れ基準: tokyo の 2026-10-08 → 2026-10-15 + 0 時間の雪像 (T20.99 の 3 枚) の phase21。"""

    def setUp(self):
        def at(name):
            return os.path.join(TOKYO, name)
        self.argv = [at("2026-10-08T104352Z-snapshot.json"), at("2026-10-15T185514Z-snapshot.json"),
                     "--no-dns", "--daily", at("2026-10-15T185514Z-daily.json"),
                     "--profile", at("2026-10-15T185514Z-profile_res_60.json"),
                     "--profile-before", at("2026-10-08T104352Z-profile_res_60.json"),
                     "--zero", at("2026-10-08T104507Z-snapshot.json")]

    def test_phase20_is_what_t2099_wrote(self):
        c = build(self.argv + ["--criteria", "phase20"])["criteria"]
        self.assertEqual(c["tally"], {sd.MET: 4, sd.MISSED: 3, sd.UNKNOWN: 0})

    def test_phase21_has_no_missed_row(self):
        c = build(self.argv + ["--criteria", "phase21"])["criteria"]
        rows = c["rows"]
        self.assertEqual(c["tally"], {sd.MET: 5, sd.MISSED: 0, sd.UNKNOWN: 0})
        self.assertEqual((c["display_only"], c["reference"]), (4, 1))
        self.assertIn("**0.12** 回/接続、日ごとの幅 0.05〜0.24", rows[2][2])
        self.assertIn("`cold` 408、`expired` 631", rows[2][2])
        self.assertEqual(rows[4][3], sd.REFERENCE)
        self.assertIn("**2.72** 倍 (234 → 638 us/要求)", rows[4][2])
        self.assertIn("前は 1.3 時間 (78 標本) で", rows[4][4])
        self.assertEqual(rows[9][3], sd.MET)
        self.assertIn("0 時間の雪像から 2.4 時間後 = **起動直後の 3 回は", rows[9][2])
        self.assertIn("**+3.1** MB", rows[8][2])


class Cut22(unittest.TestCase):
    """phase22 の平常時の切り方 (T22.1): 300 本と、その期間の 1 標本の本数の中央値 × 3 の大きい方。

    testdata の前は 100・100・100・400 本 (中央値 100 → 閾 300 のまま、400 はバースト)、
    後は 200・200・350 本 (中央値 200 → 閾 600、350 は平常時)。
    """

    def test_normal_limit_is_the_larger_of_the_base_and_k_times_the_median(self):
        self.assertEqual(sd.normal_limit(300, [100, 100, 100, 400], 3), 300)
        self.assertEqual(sd.normal_limit(300, [200, 200, 350], 3), 600)
        self.assertEqual(sd.normal_limit(300, [780, 781], 3), 2342)       # 780.5 × 3 は切り上げ
        self.assertEqual(sd.normal_limit(300, [1000, 2000], None), 300)    # k が無ければ T14.0 のまま
        self.assertEqual(sd.normal_limit(300, [], 3), 300)
        self.assertEqual(sd.normal_limit(25, [10, 10, 10], 3), 30)         # 解像度に合わせた閾でも同じ

    def test_phase22_raises_only_the_busy_side(self):
        h = build([A, B, "--no-dns", "--criteria", "phase22"])["history"]
        self.assertEqual(h["limit"], 300)
        self.assertEqual(h["median_k"], 3)
        self.assertEqual(h["medians"], {"before": 100.0, "after": 200})
        self.assertEqual(h["limits"], {"before": 300, "after": 600})
        self.assertEqual((h["before"]["samples"], h["before"]["burst_samples"]), (3, 1))
        self.assertEqual((h["after"]["samples"], h["after"]["burst_samples"]), (3, 0))
        old = build([A, B, "--no-dns", "--criteria", "phase21"])["history"]
        self.assertEqual((old["after"]["samples"], old["after"]["burst_samples"]), (2, 1))
        self.assertEqual(h["before"], old["before"])                       # 量の少ない側は T14.0 と同じ
        self.assertEqual(h["after_all"], old["after_all"])                 # バースト込みは切り方に依らない

    def test_the_other_definitions_keep_the_old_shape(self):
        for name in (None, "phase14", "phase15", "phase17", "phase18", "phase20", "phase21"):
            argv = [A, B, "--no-dns"] + (["--criteria", name] if name else [])
            h = build(argv)["history"]
            for key in ("median_k", "medians", "limits"):
                self.assertNotIn(key, h, (name, key))
            self.assertEqual(h["after"]["burst_samples"], 1, name)
        self.assertNotIn("burst_median_k", sd.PHASE21)
        self.assertEqual(sd.PHASE22["burst_median_k"], 3)
        self.assertEqual({k: v for k, v in sd.PHASE22.items() if k in sd.PHASE21}, sd.PHASE21)

    def test_the_markdown_says_how_it_cut(self):
        md = run([A, B, "--no-dns", "--criteria", "phase22"])
        self.assertIn("**前は 1 標本 300 本以上・後は 600 本以上の窓 (バースト) を外した**もの同士で並べる "
                      "(phase22 の切り方。T.22.1: 300 本 (T.14.0) と、その期間の 1 標本の本数の中央値 "
                      "(前 100.0・後 200 本) の 3 倍の大きい方)", md)
        self.assertIn("(平常時 3 / 3 標本)", md)
        self.assertNotIn("T.14.0 と同じ切り方", md)
        self.assertIn("**1 標本 300 本以上の窓 (バースト) を外した**もの同士で並べる (T.14.0 と同じ切り方)",
                      run([A, B, "--no-dns", "--criteria", "phase21"]))

    def test_the_summary_url_drops_normal_hours_when_nothing_is_cut_after(self):
        """後の閾が上がって後にバーストが残らなければ、サーバーの全標本が同じ窓 (`normal_hours_only` なし)。"""
        url22 = build([A, B, "--no-dns", "--criteria", "phase22"])["summary_url"]
        self.assertNotIn("normal_hours_only", url22)
        self.assertIn("&normal_hours_only=1", build([A, B, "--no-dns", "--criteria", "phase21"])["summary_url"])
        # 後にバーストが残るなら URL は 300 本のまま (サーバーは k を知らない) で、窓が違うと断る
        b = read(B)
        h = b["history"]["3600"]
        i = h["keys"].index("connects")
        h["samples"][-1][i] = 5000                                          # 200・200・5,000 → 閾 600
        with written(b=b) as p:
            d = build([A, p["b"], "--no-dns", "--criteria", "phase22"])
            md = run([A, p["b"], "--no-dns", "--criteria", "phase22"])
        self.assertEqual(d["history"]["after"]["burst_samples"], 1)
        self.assertIn("&normal_hours_only=1", d["summary_url"])
        self.assertIn("後の閾が 300 本より上なので、下の「サーバー側で 1 要求」の URL "
                      "(`normal_hours_only=1` は 300 本で切る) とは平常時の窓が違う。", md)


def with_dns_slow(snap, *totals, at=None, cleared=0, text=None):
    """`/events` を `dns_slow` の出来事だけにする (`totals` は窓のミスの合計 ms。文面は `anomaly.rs` の形)。"""
    at = at if at is not None else snap["taken_at"] - 600
    rows = [{"at": at, "kind": "anomaly",
             "text": f"dns_slow: dns miss {t // 3} ms avg over 5m (threshold 100 ms; 3 misses, {t} ms total)"}
            for t in totals]
    rows += [{"at": at, "kind": "anomaly", "text": "cleared: dns_slow after 6m (dns miss 1 ms avg over 5m, "
              "1 misses)"}] * cleared
    if text:
        rows.append({"at": at, "kind": "anomaly", "text": text})
    snap["events"] = {"events": rows}
    return snap


def with_causes(snap, boundary, before, after):
    """`/history?res=3600` の `errors_by_cause` を、`boundary` より前の行は `before`、後の行は `after` にする。"""
    h = snap["history"]["3600"]
    i = h["keys"].index("errors_by_cause")
    for row in h["samples"]:
        row[i] = list(before if row[0] < boundary else after)
    return snap


class Criteria22(unittest.TestCase):
    """`--criteria phase22` (T22.1): phase21 の 10 行 + T22.2 の 1 行 + T22.4 の 3 行。

    A → B は再起動をまたぐ (A は `0.1.0+aaaaaaa` で起動から 200,000 秒、B は `0.1.0+bbbbbbb` で 43,200 秒)。
    """

    def judge(self, a=None, b=None, extra=(), name="phase22"):
        return build([a or A, b or B, "--no-dns", "--criteria", name, *extra])["criteria"]

    def row(self, i, a=None, b=None, extra=()):
        return self.judge(a, b, extra)["rows"][i]

    def boundary(self):
        return build([A, B, "--no-dns"])["history"]["boundary"]

    def test_phase22_gives_fourteen_rows(self):
        rules = sd.RULES["phase22"]
        self.assertEqual(len(rules), 14)
        self.assertIs(rules[1], sd._p22_refresh_rate)
        for i in (0, *range(2, 6), *range(7, 10)):
            self.assertIs(rules[i], sd.RULES["phase21"][i], i)
        self.assertIs(rules[6], sd._p22_events)                          # `/events` の行は表示だけ
        self.assertEqual(rules[10:], (sd._p22_dns_slow, sd._p22_stale_retries, sd._p22_connect_errors,
                                      sd._p22_refresh_and_stale_misses))
        c = self.judge()
        self.assertEqual(len(c["rows"]), 14)
        self.assertEqual(c["display_only"], 5)
        self.assertEqual([i for i, r in enumerate(c["rows"]) if r[3] == sd.SHOWN], [0, 2, 3, 5, 6])
        self.assertNotIn("phase22", sd.FROZEN)

    def test_the_phase21_rows_come_out_the_same_except_the_cut(self):
        with written(z=cached(zero_snapshot(), 0)) as p:
            rows21 = self.judge(name="phase21", extra=("--zero", p["z"]))["rows"]
            rows22 = self.judge(extra=("--zero", p["z"]))["rows"]
        for i in (0, 1, 3, 4, 5, 7, 8, 9):
            self.assertEqual(rows22[i], rows21[i], i)
        # 平常時のミス (表示だけ) は切り方が違うので標本の数が違う (後の 350 本の時間が平常時に入る)
        self.assertIn("平常時 2 標本", rows21[2][4])
        self.assertIn("平常時 3 標本", rows22[2][4])

    # --- 裏の引き直し (warm 0 でも読める)

    def test_the_refresh_row_says_why_there_is_no_average_without_warm(self):
        b = read(B)
        b["status"]["dns"].update(warm_secs=0, warm=0)
        set_column(b, "3600", "dns_warm", 0)
        with written(b=b) as p:
            row = self.row(1, b=p["b"])
            old = self.judge(b=p["b"], name="phase21")["rows"][1]
        self.assertEqual(row[3], old[3])
        self.assertTrue(row[2].startswith(old[2] + "。参考: 通算 24 回 ÷ 12.0 時間 = 2.0 回/時 (全部の名前)"), row[2])
        self.assertIn("keep-warm は止めてある (`warm_secs` 0、warm の平均 0.0 件) ので、warm の平均で割る"
                      "名前あたりの値は出ない (T22.4)", row[4])
        del b["dns"]
        with written(b=b) as p:
            row = self.row(1, b=p["b"])
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertEqual(row[2], "通算 24 回 ÷ 12.0 時間 = 2.0 回/時 (全部の名前)")
        self.assertTrue(row[4].startswith("`/dns` の部が無い。keep-warm は止めてある"))

    def test_with_warm_the_refresh_row_is_the_phase15_one(self):
        self.assertEqual(self.row(1), self.judge(name="phase21")["rows"][1])

    # --- T22.2: `dns_slow` の新しい閾

    def test_small_windows_are_counted_apart_from_the_new_threshold(self):
        with written(b=with_dns_slow(read(B), 495, 1204, 2949, cleared=2)) as p:
            row = self.row(10, b=p["b"])
        self.assertEqual(row[3], sd.MISSED)
        self.assertEqual(row[1], "合計 < 1,000 ms の窓で 0 件")
        self.assertIn("新しい閾で **0.17** 件/時 (2 件)、合計 1,000 ms 未満 **1** 件", row[2])
        self.assertNotIn("前:", row[2])
        self.assertIn("。前の雪像に `/events` が無い", row[4])
        a = with_dns_slow(read(A), 300, 400, 5000)
        a["taken_at"] = read(A)["taken_at"]
        with written(a=a, b=with_dns_slow(read(B), 1204)) as p:
            row = self.row(10, a=p["a"], b=p["b"])
        self.assertEqual(row[3], sd.MET)
        self.assertIn(" (前: 新しい閾で 0.02 件/時 (1 件)、合計 1,000 ms 未満 2 件)", row[2])

    def test_only_big_windows_are_met(self):
        with written(b=with_dns_slow(read(B), 1000, 2541)) as p:
            row = self.row(10, b=p["b"])
        self.assertEqual(row[3], sd.MET)
        self.assertIn("新しい閾で **0.17** 件/時 (2 件)、合計 1,000 ms 未満 **0** 件", row[2])
        with written(b=with_dns_slow(read(B))) as p:
            self.assertEqual(self.row(10, b=p["b"])[3], sd.MET)        # 1 件も無い

    def test_events_before_the_start_are_not_counted(self):
        b = read(B)
        with written(b=with_dns_slow(b, 300, at=b["taken_at"] - b["uptime_secs"] - 1)) as p:
            row = self.row(10, b=p["b"])
        self.assertEqual(row[3], sd.MET)

    def test_a_text_without_the_total_cannot_be_judged(self):
        old = "dns_slow: dns miss 150 ms avg over 5m (threshold 100 ms; 3 misses)"
        with written(b=with_dns_slow(read(B), 2000, text=old)) as p:
            row = self.row(10, b=p["b"])
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("、合計が読めない 1 件", row[2])
        self.assertIn("文面に `ms total` の無い `dns_slow` がある", row[4])
        b = read(B)
        del b["events"]
        with written(b=b) as p:
            self.assertEqual(self.row(10, b=p["b"])[3], sd.UNKNOWN)

    # --- T22.4 の戻す条件 (1): `stale_retries` / `stale_hits`

    def stale_row(self, hits, retries, a_hits=None, a_retries=None, same_start=False):
        a, b = read(A), read(B)
        b["status"]["dns"].update(stale_hits=hits, stale_retries=retries)
        if a_hits is not None:
            a["status"]["dns"].update(stale_hits=a_hits, stale_retries=a_retries)
        if same_start:
            a = read(B)
            a["status"]["dns"].update(stale_hits=a_hits, stale_retries=a_retries)
            a["taken_at"] -= 3600
            a["uptime_secs"] -= 3600
            a["status"]["uptime_secs"] = a["uptime_secs"]
        with written(a=a, b=b) as p:
            return self.row(11, a=p["a"], b=p["b"])

    def test_retries_under_one_percent_are_met(self):
        row = self.stale_row(1000, 9)
        self.assertEqual(row[3], sd.MET)
        self.assertEqual(row[2], "**0.90%** (9 / 1,000)")
        self.assertIn("(起動から)、keep-warm の窓 `warm_secs` 900", row[4])
        self.assertEqual(self.stale_row(1000, 10)[3], sd.MISSED)          # ちょうど 1% は戻す条件に当たる

    def test_no_stale_hit_is_met(self):
        row = self.stale_row(0, 0)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("古い答えを 1 度も使っていない", row[4])

    def test_without_a_restart_the_stale_counts_are_a_difference(self):
        row = self.stale_row(1200, 12, a_hits=200, a_retries=11, same_start=True)
        self.assertEqual(row[2], "**0.10%** (1 / 1,000)")
        self.assertIn("(その間)", row[4])

    def test_a_version_without_stale_counts_cannot_be_judged(self):
        row = self.row(11)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("T21.3 より前の版", row[4])

    # --- T22.4 の戻す条件 (2): 接続のエラーの件/時

    def errors_row(self, before, after):
        edge = self.boundary()
        a = with_causes(read(A), edge, before, after)
        b = with_causes(read(B), edge, before, after)
        with written(a=a, b=b) as p:
            return self.row(12, a=p["a"], b=p["b"])

    def test_errors_per_connect_are_compared(self):
        # 前 4 標本 (700 本)・後 3 標本 (750 本) に、1 標本あたり refused 1・timeout 1
        row = self.errors_row([0, 1, 0, 1, 0, 0, 0, 0], [0, 1, 0, 1, 0, 0, 0, 0])
        self.assertEqual(row[3], sd.MET)
        self.assertEqual(row[1], "< 前 ×1.5 (接続 1 本あたり)")
        self.assertEqual(row[2], "**0.0080** / 接続 (6 件 / 750 本: `refused` 3、`unreachable` 0、`timeout` 3。"
                                 "前 0.0114 = 8 件 / 700 本、0.70 倍)、件/時 2.00 (前 2.00)")
        self.assertIn("`errors_by_cause` ÷ `connects`", row[4])
        self.assertIn("件/時は表示だけ", row[4])

    def test_one_and_a_half_times_per_connect_is_missed(self):
        # 前 4 件 / 700 本 = 0.0057、後 9 件 / 750 本 = 0.012 (2.1 倍)
        self.assertEqual(self.errors_row([0, 1, 0, 0, 0, 0, 0, 0], [0, 2, 1, 0, 0, 0, 0, 0])[3], sd.MISSED)
        # 件/時は 2 倍でも、接続 1 本あたりは 1.4 倍なので満たした (量が増えた期間で当たらない)
        row = self.errors_row([0, 1, 0, 0, 0, 0, 0, 0], [0, 2, 0, 0, 0, 0, 0, 0])
        self.assertEqual(row[3], sd.MET)
        self.assertIn("、1.40 倍)、件/時 2.00 (前 1.00)", row[2])
        # dns や reset は数えない
        self.assertEqual(self.errors_row([0, 1, 0, 0, 0, 0, 0, 0], [9, 1, 0, 0, 9, 0, 0, 0])[3], sd.MET)

    def test_errors_after_none_before_are_missed(self):
        self.assertEqual(self.errors_row([0] * 8, [0, 0, 1, 0, 0, 0, 0, 0])[3], sd.MISSED)
        self.assertEqual(self.errors_row([0] * 8, [0] * 8)[3], sd.MET)

    # --- T22.4 の目安: 引き直しの回/時と `expired` + `warm_stale`

    def refresh_row(self, a_refreshes, b_refreshes, a_kinds, b_kinds):
        a, b = read(A), read(B)
        a["status"]["dns"].update(refreshes=a_refreshes, misses_by_kind=a_kinds)
        b["status"]["dns"].update(refreshes=b_refreshes, misses_by_kind=b_kinds)
        with written(a=a, b=b) as p:
            return self.row(13, a=p["a"], b=p["b"])

    def test_half_the_refreshes_and_few_stale_misses_are_met(self):
        # 前は 200,000 秒で 100,000 回 (1,800 回/時)、後は 43,200 秒で 10,800 回 (900 回/時 = 0.5 倍)
        row = self.refresh_row(100_000, 10_800, {"expired": 0, "warm_stale": 0},
                               {"expired": 5, "warm_stale": 0})
        self.assertEqual(row[3], sd.MET)
        self.assertIn("引き直し **900.0** 回/時 (前 1,800.0、0.50 倍)、`expired` + `warm_stale` **10.0** 回/日 "
                      "(前 0.0、増え +10.0)", row[2])
        self.assertEqual(row[1], "≤ 前 ×0.5 かつ 増え ≤ 10 回/日")

    def test_too_many_refreshes_or_stale_misses_are_missed(self):
        self.assertEqual(self.refresh_row(100_000, 10_801, {"expired": 0}, {"expired": 0})[3], sd.MISSED)
        self.assertEqual(self.refresh_row(100_000, 100, {"expired": 0, "warm_stale": 0},
                                          {"expired": 5, "warm_stale": 1})[3], sd.MISSED)

    def test_without_misses_by_kind_before_it_cannot_be_judged(self):
        row = self.row(13)                                              # A の `dns` に `misses_by_kind` が無い
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("引き直し **2.0** 回/時 (前 0.2、11.11 倍)", row[2])
        self.assertIn("`misses_by_kind` が無い", row[4])

    # --- 印字

    def test_the_markdown_names_the_phase22_parts(self):
        md = run([A, B, "--no-dns", "--criteria", "phase22"])
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria phase22`)", md)
        self.assertIn("前の 10 行は phase21 と同じ物差しで", md)
        self.assertIn("T22.3 は段 1 で止めたので行がありません", md)
        self.assertIn("| `dns_slow` が窓のミスの合計 1,000 ms 未満の窓で立っていない (新しい閾 = 合計 1,000 ms "
                      "以上での件/時は表示だけ) | 合計 < 1,000 ms の窓で 0 件 |", md)
        self.assertIn("(ほかに表示だけ 5 行。集計に数えない)", md)

    def test_the_events_row_is_only_shown(self):
        """phase21 の `/events` の行 (`dns_slow` ≤ 0.1 件/時) は phase22 では表示だけ (親の判断 2026-10-17)。"""
        b = with_dns_slow(read(B), 2000, 3000, 4000)                     # 0.25 件/時。どれも合計 1,000 ms 以上
        with written(b=b) as p:
            old = self.judge(b=p["b"], name="phase21")["rows"][6]
            row = self.row(6, b=p["b"])
            new = self.row(10, b=p["b"])
        self.assertEqual(old[3], sd.MISSED)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertEqual(row[0], "`/events` の anomaly の種類別 件/時 (`dns_slow` を含めて表示だけ)")
        self.assertEqual(row[1], "— (phase21 は `dns_slow` ≤ 0.1)")
        self.assertEqual(row[2], old[2])
        self.assertIn("**閾を置かない**: `dns_slow` の判定は新しい閾の行", row[4])
        # 件/時が高くても、小さい窓で立っていなければ新しい閾の行は満たした (件/時は表示だけ)
        self.assertEqual(new[3], sd.MET)
        self.assertIn("新しい閾で **0.25** 件/時 (3 件)", new[2])
        self.assertNotIn("前の 10 行は phase21 と同じ物差しで", run([A, B, "--no-dns", "--criteria", "phase21"]))


class PastOutputs22(unittest.TestCase):
    """phase22 を足しても phase14〜phase21 の出力 (§2 の平常時の表と §9 の判定表、json の `history` と
    `criteria`) が 1 文字も変わらないこと (T22.1)。値は phase22 を足す前の main (`1c19f9a`) で取った sha256 の頭 16 桁。
    **ここが落ちたら過去の判定を変えている** (過去の判定を同じ命令で出し直せなくなる)。"""

    DIGESTS = {
        "phase14": ("b681fecffcef4e9f", "4d192151d9ac75fa"),
        "phase15": ("bb81ecd17d2fb70d", "9e156ba611dc8241"),
        "phase17": ("36803b66e86cde71", "62e6c08ce78552a6"),
        "phase18": ("429a196382e2216d", "12ac626a2b58fb3a"),
        "phase20": ("af8f8d422cf8a838", "6d68fd96aa1c923d"),
        "phase21": ("2ccfe3e49dbb33a3", "cff8f5505a659290"),
    }

    def test_the_past_criteria_print_the_same(self):
        import hashlib
        for name, (md_digest, js_digest) in self.DIGESTS.items():
            md = run([A, B, "--no-dns", "--criteria", name])
            js = json.loads(run([A, B, "--no-dns", "--criteria", name, "--out", "json"]))
            part = md[md.index("## 2."):md.index("## 3.")] + md[md.index("## 9."):]
            body = json.dumps({"history": js["history"], "criteria": js["criteria"]},
                              ensure_ascii=False, sort_keys=True)
            self.assertEqual(hashlib.sha256(part.encode()).hexdigest()[:16], md_digest, name)
            self.assertEqual(hashlib.sha256(body.encode()).hexdigest()[:16], js_digest, name)


class Criteria23(unittest.TestCase):
    """無言の再起動 (T23.2)。`/events` の `start` の直前に `shutdown` が無い起動を数える行。

    実データでの数字は `Deployed23` (デプロイ先の雪像があるときだけ)。ここは `events` をその場で
    作って、同じ秒の出来事・窓の先頭・読み戻し (`restored`)・自己ベンチの `start` の扱いを見る。
    """

    T0 = 1790000000          # 窓の先頭の `start` の時刻 (2026-09-21 14:13:20Z)
    BOOT = ("version 0.22.6+000681e on port 60357 (profile default, cache on, timeout 30s, "
            "max conns 240)")
    STOP = "stop signal received; saving statistics"

    def snap(self, events, restored=0):
        """`testdata/snapshot-a.json` を土台に、`/events` だけを載せ替えた雪像を作る。"""
        d = read(A)
        d["events"] = {"events": events, "count": len(events), "kept": len(events),
                       "capacity": 512, "restored": restored, "truncated": False}
        return d

    def boot(self, at, text=None):
        return {"at": at, "kind": "start", "text": text or self.BOOT}

    def stop(self, at):
        return {"at": at, "kind": "shutdown", "text": self.STOP}

    def row(self, events, restored=0):
        return sc._p23_silent_restarts({"b": self.snap(events, restored)}, sd.PHASE22)

    def test_a_start_without_a_shutdown_before_it_carries_the_previous_run(self):
        """2026-10-17 22:14:52Z の形: `shutdown` が無い `start` と、前の起動の長さ。"""
        t = self.T0
        events = [self.boot(t), self.stop(t + 616), self.boot(t + 658),
                  # **同じ秒の `anomaly`** は「直前」と見ない (起動の瞬間に載る)
                  {"at": t + 658, "kind": "anomaly",
                   "text": "connect_p95: connect p95 109 ms over 5m (threshold 100 ms)"},
                  self.boot(t + 658 + 69150)]
        row = self.row(events)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn(f"1 回 (**{sd.stamp(t + 658 + 69150)}** 版 `0.22.6+000681e`", row[2])
        self.assertIn("前の起動は 19.2 時間", row[2])
        self.assertIn("窓の先頭などの `start` 1 件は判定できない", row[2])
        # 前の起動より後に `shutdown` が無いので、止まっていた秒数は言えない
        self.assertIn("止まっていた秒数は分からない", row[4])
        self.assertIn("読み戻し 0 件", row[4])

    def test_every_start_after_a_shutdown_is_zero(self):
        t = self.T0
        row = self.row([self.boot(t), self.stop(t + 616), self.boot(t + 658),
                        self.stop(t + 4000), self.boot(t + 4013)])
        self.assertEqual(row[3], sd.MET)
        self.assertEqual(row[2], "0 件 (直前が `shutdown` の起動 2 件、"
                                 "窓の先頭など 1 件は判定できない)")

    def test_a_window_that_begins_in_the_middle_of_a_run_cannot_be_judged(self):
        """**読み戻した `start` を無言の再起動と数えない**。

        窓が起動の途中から始まっている (先頭が `anomaly`) と、そのあとの `start` は直前に
        `shutdown` が無くても「落とされた」とは言えない (古い行がリングから流れただけかもしれない)。
        """
        t = self.T0
        row = self.row([{"at": t, "kind": "anomaly",
                         "text": "errors: 5 errors in 5m (threshold 5): dns 4, timeout 1"},
                        self.boot(t + 600)], restored=1)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("判定できず", row[2])

    def test_the_self_bench_line_is_not_a_start_of_the_process(self):
        """`start` の種類は自己ベンチの要約にも使われる (`PROXY_SELF_BENCH=on` で起動の 3 秒後)。"""
        t = self.T0
        events = [self.boot(t), self.stop(t + 60), self.boot(t + 98),
                  self.boot(t + 101, text="self bench: forward 8 並列 12,345 req/s, "
                                          "connect 8 並列 3,210 本/s")]
        self.assertEqual(self.row(events)[3], sd.MET)
        r = sc.silent_restarts(self.snap(events))
        self.assertEqual((r["starts"], r["other_starts"]), (2, 1))

    def test_a_snapshot_without_events_cannot_be_judged(self):
        row = sc._p23_silent_restarts({"b": read(A)}, sd.PHASE22)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("判定できず (`/events` の部が無い)", row[2])


class Criteria23Rows(unittest.TestCase):
    """`--criteria phase23` (T23.1) の行: エラーの分母を「名前解決に成功した試行あたり」にした 2 行と、
    heap の「判定できず」の理由、無言の再起動の行。

    `testdata/snapshot-{a,b}.json` には `/recent` の部が無いので、その場で作って載せる。
    数字は 2026-10-17 → 10-20 の tokyo の雪像から採った (前 24 件 / 1,991 試行、後 30 件 /
    2,000 試行のうち dns 170) — **親が T23.0 の道具で出した値と同じ**であることを見る。
    """

    # 表示だけの行 (phase22 の 3 + `/events` + phase22 の接続のエラーの行)
    SHOWN_ROWS = (0, 2, 3, 5, 6, 13)
    BOOT = ("version 0.1.0+bbbbbbb on port 8080 (profile default, cache on, "
            "timeout 30s, max conns 240)")

    def judge(self, a=None, b=None, extra=()):
        return build([a or A, b or B, "--no-dns", "--criteria", "phase23", *extra])["criteria"]

    def row(self, i, a=None, b=None, extra=()):
        return self.judge(a=a, b=b, extra=extra)["rows"][i]

    @staticmethod
    def recent(attempts, dns=0, fails=(), at=1789043200, step=60):
        """`/recent` の部を作る。先頭 `dns` 行が `error:dns`、次の `len(fails)` 行が `fails` の失敗。"""
        rows = []
        for i in range(attempts):
            reason = "ok"
            if i < dns:
                reason = "error:dns"
            elif i - dns < len(fails):
                reason = "error:" + fails[i - dns]
            rows.append({"at": at + i * step, "kind": "connect", "target": "example.com:443",
                         "client": "192.0.2.1", "reason": reason})
        return {"recent": rows, "count": len(rows), "kept": len(rows), "capacity": 2000,
                "truncated": False}

    @staticmethod
    def with_recent(snap, part):
        d = dict(snap)
        d["recent"] = part
        return d

    def pair(self):
        """tokyo の 10-17 → 10-20 の `/recent` (24 / 1,991 と 30 / 1,830)。"""
        a = self.with_recent(read(A), self.recent(1991, fails=("timeout",) * 24))
        b = self.with_recent(read(B), self.recent(
            2000, dns=170, fails=("timeout",) * 25 + ("refused",) * 3 + ("unreachable",) * 2))
        return a, b

    # --- 表の形

    def test_phase23_gives_sixteen_rows_and_six_of_them_are_display_only(self):
        c = self.judge()
        self.assertEqual(len(sd.RULES["phase23"]), 16)
        self.assertEqual(len(c["rows"]), 16)
        self.assertEqual([i for i, r in enumerate(c["rows"]) if r[3] == sd.SHOWN],
                         list(self.SHOWN_ROWS))
        self.assertEqual(c["display_only"], 6)
        self.assertEqual(sum(c["tally"].values()), 10)
        self.assertIs(sd.RULES["phase23"][15], sc._p23_silent_restarts)

    def test_the_rows_that_did_not_change_are_the_phase22_ones(self):
        self.assertEqual(sd.PHASE23, sd.PHASE22)
        for new, old in ((0, 0), (1, 1), (3, 3), (4, 4), (5, 5), (6, 6), (7, 7),
                         (9, 9), (10, 10), (11, 11), (14, 13)):
            self.assertIs(sd.RULES["phase23"][new], sd.RULES["phase22"][old], new)
        rows22, rows23 = self.judge(), self.judge()
        self.assertEqual([r[3] for r in rows23["rows"]], [r[3] for r in rows22["rows"]])

    # --- (1) 接続のエラーを「名前解決に成功した試行あたり」で

    def test_the_connect_error_row_uses_the_resolved_attempts_as_its_denominator(self):
        a, b = self.pair()
        with written(a=a, b=b) as p:
            row = self.row(12, a=p["a"], b=p["b"])
        self.assertEqual(row[3], sd.MET)                     # 0.0121 → 0.0164 = 1.36 倍 < 1.5
        self.assertIn("**0.0164** / 名前解決に成功した試行 (30 件 / 1,830 本: `refused` 3、"
                      "`unreachable` 2、`timeout` 25。", row[2])
        self.assertIn("試行 2,000 から dns の失敗 170 を外した分母。前 0.0121 = 24 件 / 1,991 本、"
                      "1.36 倍)", row[2])
        self.assertEqual(row[1], "< 前 ×1.5 (名前解決に成功した試行あたり)")
        self.assertIn("1,991 試行", row[4])
        self.assertIn("2,000 試行", row[4])

    def test_one_and_a_half_times_is_missed(self):
        a, b = self.pair()
        b["recent"] = self.recent(2000, dns=170, fails=("timeout",) * 40)
        with written(a=a, b=b) as p:
            row = self.row(12, a=p["a"], b=p["b"])
        self.assertEqual(row[3], sd.MISSED)                  # 0.0219 / 0.0121 = 1.81 倍
        self.assertIn("1.81 倍", row[2])

    def test_without_recent_the_row_is_unknown_and_not_zero(self):
        row = self.row(12)                                   # testdata に `/recent` は無い
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertEqual(row[2], "—")
        self.assertIn("`/recent` の部が無い", row[4])

    def test_a_window_with_no_resolved_attempt_cannot_be_judged(self):
        a = self.with_recent(read(A), self.recent(5, dns=5))   # 全部 dns の失敗
        b = self.with_recent(read(B), self.recent(5, fails=("timeout",)))
        with written(a=a, b=b) as p:
            row = self.row(12, a=p["a"], b=p["b"])
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("名前解決へ成功した試行が無い", row[4])

    def test_the_phase22_row_is_kept_next_to_it_as_display_only(self):
        a, b = self.pair()
        with written(a=a, b=b) as p:
            old = build([p["a"], p["b"], "--no-dns", "--criteria", "phase22"])["criteria"]["rows"][12]
            row = self.row(13, a=p["a"], b=p["b"])
        self.assertEqual(old[3], sd.MET)                      # phase22 は `/history` の connects で見る
        self.assertEqual(row[3], sd.SHOWN)
        self.assertEqual(row[2], old[2])
        self.assertEqual(row[1], f"— (phase22 は {old[1]})")
        self.assertEqual(row[0], "接続のエラー (`refused`・`unreachable`・`timeout`) の接続 1 本あたり")
        self.assertIn("**判定は上の「名前解決に成功した試行あたり」の行に任せる**", row[4])

    # --- (2) dns のミスの率を同じ分母でも並べる

    def test_the_dns_row_shows_the_rate_per_attempt_and_per_resolved_attempt(self):
        a, b = self.pair()
        with written(a=a, b=b) as p:
            row = self.row(2, a=p["a"], b=p["b"])
        self.assertEqual(row[3], sd.SHOWN)
        self.assertIn("試行あたり **0.0850** (170 / 2,000)、名前解決に成功した試行あたり "
                      "**0.0929** (170 / 1,830) (前: 試行あたり 0.0000 (0 / 1,991)、"
                      "名前解決に成功した試行あたり 0.0000 (0 / 1,991))", row[2])
        self.assertEqual(row[0], "平常時の名前解決のミス (回/接続。`/recent` の試行あたり・"
                                 "名前解決に成功した試行あたりも並べる)")
        self.assertIn("`/recent` の connect の行 (dns の失敗 ÷ 試行 と ÷ 名前解決に成功した試行)",
                      row[4])

    def test_the_dns_row_without_recent_still_shows_the_old_number(self):
        row = self.row(2)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertIn("試行あたりは出せない", row[2])

    # --- (3) heap の「判定できず」に起動の時刻を書く

    def starts(self, at):
        """`/events` に `start` 1 行だけを載せる。"""
        return {"events": [{"at": at, "kind": "start", "text": self.BOOT}], "count": 1, "kept": 1,
                "capacity": 512, "restored": 0, "truncated": False}

    def heap(self, z, b):
        with written(z=z, b=b) as p:
            return self.row(8, b=p["b"], extra=("--zero", p["z"]))

    def test_the_heap_row_says_when_each_start_began(self):
        """0 時間の雪像と後の雪像が同じ版で起動し直している (2026-10-17 22:14:52Z の形)。"""
        b = cached(read(B), 0)
        b["events"] = self.starts(Criteria17.START)
        z = cached(zero_snapshot(), 0)
        # 0 時間の雪像は 2 時間前の起動 (取得の 2 分後が起動なので、起動は taken_at − 120)。
        # 同じ UTC の日なので、2 つ目の時刻は日付を省いて出る
        z["taken_at"] = Criteria17.START - 2 * 3600 + 120
        z["events"] = self.starts(z["taken_at"] - 120)
        row = self.heap(z, b)
        self.assertEqual(row[3], sd.UNKNOWN)
        want = f"`/events` の `start` が {self.iso(Criteria17.START - 2 * 3600)} と " \
               f"{self.iso(Criteria17.START)[11:]}、同じ版 `0.1.0+bbbbbbb` で起動し直している"
        self.assertIn(want, row[2])
        self.assertIn("起動の時刻はどちらも `/events` の `start` から", row[4])

    @staticmethod
    def iso(t):
        return datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    def test_without_events_the_heap_row_falls_back_to_taken_at_minus_uptime(self):
        b = cached(read(B), 0)
        z = cached(zero_snapshot(), 0)
        z["taken_at"] = Criteria17.START - 2 * 3600 + 120
        row = self.heap(z, b)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("0 時間の雪像と起動が違う", row[2])
        self.assertIn("片方は `/events` に `start` が残っていないので", row[4])

    def test_the_heap_row_without_zero_reads_as_phase20(self):
        row = self.row(8)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertEqual(row[2], "—")
        self.assertIn("`--zero` (0 時間の雪像) が渡されていない", row[4])

    # --- 印字

    def test_the_markdown_names_the_phase23_parts(self):
        md = run([A, B, "--no-dns", "--criteria", "phase23"])
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria phase23`)", md)
        self.assertIn("前の 14 行は phase22 と同じ物差しで", md)
        self.assertIn("名前解決に失敗した行 (`error:dns`) を分母から外した", md)
        self.assertIn("無言の再起動", md)
        self.assertIn("満たした 2 行・判定できず 8 行 (ほかに表示だけ 6 行。集計に数えない)", md)
        # 過去の定義の文は変えない
        self.assertNotIn("前の 14 行は phase22 と同じ物差しで",
                         run([A, B, "--no-dns", "--criteria", "phase22"]))


def deployed_argv(d, a, b, zero):
    def at(name):
        return os.path.join(d, name)
    return [at(a + "-snapshot.json"), at(b + "-snapshot.json"), "--no-dns",
            "--daily", at(b + "-daily.json"), "--profile", at(b + "-profile_res_60.json"),
            "--profile-before", at(a + "-profile_res_60.json"), "--zero", at(zero + "-snapshot.json")]


# T18.99・T20.99・T21.99 の 3 組 (前・後・0 時間) と、そのとき使った定義
PAST_PAIRS = (
    (DEPLOYED, "2026-10-01T133549Z", "2026-10-08T092521Z", "2026-10-01T133907Z", ("phase18", "phase20", "phase21")),
    (TOKYO, "2026-10-08T104352Z", "2026-10-15T185514Z", "2026-10-08T104507Z", ("phase20", "phase21")),
    (TOKYO, "2026-10-15T201451Z", "2026-10-17T020206Z", "2026-10-15T201908Z", ("phase21",)),
)


@unittest.skipUnless(all(os.path.isfile(os.path.join(d, f"{x}-{kind}.json"))
                         for d, a, b, z, _ in PAST_PAIRS for x, kind in (
                             (a, "snapshot"), (a, "profile_res_60"), (b, "snapshot"), (b, "daily"),
                             (b, "profile_res_60"), (z, "snapshot"))),
                     "デプロイ先の雪像が無い (リポジトリには入れない)")
class Deployed22(unittest.TestCase):
    """T22.1 の受け入れ基準: 過去の 3 組に phase22 の切り方を当てても判定が変わらないことと、
    tokyo の 10-15 → 10-17 (T21.99 の 3 枚) の phase22。"""

    def test_the_phase22_cut_does_not_change_the_past_verdicts(self):
        for d, a, b, z, names in PAST_PAIRS:
            for name in names:
                args = sd.parser().parse_args(deployed_argv(d, a, b, z) + ["--criteria", name])
                A_, B_ = (sd.load_source(f, False) for f in args.files)
                out = sd.build(A_, B_, args)
                cut = sd.history_split(A_, B_, out["restart"], args.burst, sd.PHASE22["burst_median_k"])
                again = sd.judge(name, cut, out["hosts"],
                                 sd.major_hosts(out["hosts"]["rows"], None), sd.part(B_, "status"),
                                 None, sd.CRITERIA[name], a=A_, b=B_, info=out["restart"],
                                 dns=out["dns"], errors=out["errors"])
                self.assertEqual([r[3] for r in again["rows"]], [r[3] for r in out["criteria"]["rows"]],
                                 (b, name))

    def test_phase22_on_the_t2199_pair(self):
        d, a, b, z, _ = PAST_PAIRS[2]
        out = build(deployed_argv(d, a, b, z) + ["--criteria", "phase22"])
        h, c = out["history"], out["criteria"]
        # 後の期間は中央値 780 本/時 → 閾 2,340 本で、29 標本とも平常時 (300 本なら 6 標本)
        self.assertEqual(h["limits"], {"before": 300, "after": 2340})
        self.assertEqual((h["after"]["samples"], h["after"]["burst_samples"]), (29, 0))
        self.assertEqual(h["after"]["connects"], 21_920)
        self.assertEqual(c["tally"], {sd.MET: 7, sd.MISSED: 2, sd.UNKNOWN: 0})
        self.assertEqual(c["display_only"], 5)
        verdicts = [r[3] for r in c["rows"]]
        self.assertEqual(verdicts[6], sd.SHOWN)                          # `dns_slow` 0.47 件/時 は表示だけ
        self.assertIn("`dns_slow` **0.47** 件/時 (14 件、前 0.09)", c["rows"][6][2])
        self.assertEqual(verdicts[10:], [sd.MISSED, sd.MET, sd.MET, sd.MISSED])
        self.assertIn("新しい閾で **0.17** 件/時 (5 件)、合計 1,000 ms 未満 **9** 件", c["rows"][10][2])
        self.assertIn("**0.00%** (0 / 1,252)", c["rows"][11][2])
        # 件/時は 5.6 倍 (0.69 → 3.86) でも、接続 1 本あたりは 0.46 倍
        self.assertIn("**0.0051** / 接続 (112 件 / 21,920 本: `refused` 3、`unreachable` 1、`timeout` 108。"
                      "前 0.0112 = 125 件 / 11,137 本、0.46 倍)、件/時 3.86 (前 0.69)", c["rows"][12][2])
        self.assertIn("引き直し **1,990.8** 回/時 (前 309.7、6.43 倍)", c["rows"][13][2])


class Criteria24BootSplit(unittest.TestCase):
    """T24.0: 無言の再起動を**ホストの再起動**と**プロセスだけの再起動**に切り分ける。

    `start` の文面に載る `boot` (`/proc/sys/kernel/random/boot_id` の先頭 12 文字) を
    1 つ前の起動と比べる。変わっていればホスト (カーネル) の再起動 = こちらでは防げない、
    同じならプロセスだけの再起動 = 落ちたか外から止められた (こちらが原因)。

    `testdata/snapshot-a.json` の `/events` をその場で作って見る (形は 2026-10-21 のもの)。
    """

    T0 = 1790000000          # 窓の先頭の `start` の時刻 (2026-09-21 14:13:20Z)
    B1 = "aaaaaaaa-111"      # ホストの起動 1
    B2 = "bbbbbbbb-222"      # ホストの起動 2
    B3 = "cccccccc-333"      # ホストの起動 3

    def snap(self, events, restored=0):
        d = read(A)
        d["events"] = {"events": events, "count": len(events), "kept": len(events),
                       "capacity": 512, "restored": restored, "truncated": False}
        return d

    def boot(self, at, boot=None, version="0.23.3+ae7c190"):
        """`crates/run/src/lib.rs` が書くのと同じ形の `start` (T24.0 から `boot` が入る)。"""
        text = f"version {version} on port 60357"
        if boot:
            text += f" boot {boot}"
        text += " (profile default, cache on, timeout 30s, max conns 240)"
        return {"at": at, "kind": "start", "text": text}

    def stop(self, at):
        return {"at": at, "kind": "shutdown", "text": "stop signal received; saving statistics"}

    def row(self, events, restored=0):
        return sc._p24_silent_restarts({"b": self.snap(events, restored)}, sd.PHASE22)

    def test_two_host_restarts_and_no_process_restart(self):
        """2026-10-21 の形: `boot` が 2 回変わる = ホストの再起動 2 回、プロセスだけは 0 回。"""
        t = self.T0
        events = [self.boot(t, self.B1), self.stop(t + 616), self.boot(t + 658, self.B1),
                  # ホストが起動し直した (boot が B1 → B2)
                  self.boot(t + 658 + 69150, self.B2),
                  # さらにホストが起動し直した (B2 → B3)
                  self.boot(t + 658 + 69150 + 2390, self.B3)]
        r = sc.silent_restarts(self.snap(events))
        self.assertEqual(len(r["silent"]), 2)
        self.assertEqual([s["at"] for s in r["host"]],
                         [t + 658 + 69150, t + 658 + 69150 + 2390])
        self.assertEqual(r["process"], [])
        self.assertEqual(r["boot_unknown"], 0)
        text = sc.silent_restart_text(r)
        self.assertIn("2 回 (", text)
        self.assertIn("うちホストの再起動 (boot が変わった回) 2 回、プロセスだけの再起動 0 回", text)
        # 判定表の行は「こちらが原因の無言の再起動 0 回」を満たす (外からの 2 回は数から外す)
        row = self.row(events)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("こちらが原因の無言の再起動", row[0])
        self.assertIn("は数から外した", row[4])

    def test_the_same_boot_is_a_process_restart(self):
        """`boot` が同じ = **プロセスだけ**の起動し直し (落ちたか、外から止められた)。"""
        t = self.T0
        events = [self.boot(t, self.B1), self.stop(t + 616), self.boot(t + 658, self.B1),
                  self.boot(t + 658 + 69150, self.B1)]
        r = sc.silent_restarts(self.snap(events))
        self.assertEqual([s["at"] for s in r["process"]], [t + 658 + 69150])
        self.assertEqual(r["host"], [])
        self.assertIn("うちホストの再起動 (boot が変わった回) 0 回、プロセスだけの再起動 1 回",
                      sc.silent_restart_text(r))
        row = self.row(events)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("同じ `boot` のまま起動し直している", row[4])

    def test_a_start_without_boot_is_not_counted_either_way(self):
        """T24.0 より前の版が書いた `start` (boot が無い) は**どちらにも数えない**。"""
        t = self.T0
        old = ("version 0.23.3+ae7c190 on port 60357 "
               "(profile default, cache on, timeout 30s, max conns 240)")
        events = [self.boot(t, self.B1), self.stop(t + 616), self.boot(t + 658, self.B1),
                  # 旧版の行 (boot が無い)。前の起動 (B1) と同じホストかは言えない
                  {"at": t + 658 + 69150, "kind": "start", "text": old},
                  # その次も旧版の行を挟むので、こちらも「同じホストか」を言えない
                  self.boot(t + 658 + 69200, self.B2),
                  self.boot(t + 658 + 69250, self.B2)]
        r = sc.silent_restarts(self.snap(events))
        self.assertEqual(len(r["silent"]), 3)
        self.assertEqual(r["host"], [])
        self.assertEqual(r["boot_unknown"], 2)   # 前が同じホストかを言えない回が 2 つ
        self.assertEqual([s["at"] for s in r["process"]], [t + 658 + 69250])
        text = sc.silent_restart_text(r)
        self.assertIn("うちホストの再起動 (boot が変わった回) 0 回、プロセスだけの再起動 1 回", text)
        self.assertIn("`boot` の無い `start` 2 件は判定できない", text)
        self.assertEqual(self.row(events)[3], sd.MISSED)

    def test_the_phase23_text_is_unchanged_without_boot(self):
        """`boot` の無い雪像では要点の文を 1 文字も変えない (過去の phase の出力を変えない)。"""
        t = self.T0
        old = ("version 0.22.6+000681e on port 60357 "
               "(profile default, cache on, timeout 30s, max conns 240)")
        events = [{"at": t, "kind": "start", "text": old}, self.stop(t + 616),
                  {"at": t + 658, "kind": "start", "text": old},
                  {"at": t + 658 + 69150, "kind": "start", "text": old}]
        r = sc.silent_restarts(self.snap(events))
        # どの行にも `boot` が無いので切り分けは 1 件もできない (`boot_unknown` だけが増える)
        self.assertEqual((r["host"], r["process"], r["boot_unknown"]), ([], [], 1))
        self.assertEqual(sc.boot_split_text(r), "")
        row23 = sc._p23_silent_restarts({"b": self.snap(events)}, sd.PHASE22)
        self.assertEqual(row23[3], sd.MISSED)
        self.assertIn("1 回 (**", row23[2])
        self.assertNotIn("ホストの再起動", row23[2])

    def test_the_row_cannot_be_judged_without_any_start(self):
        row = sc._p24_silent_restarts({"b": read(A)}, sd.PHASE22)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("判定できず", row[2])


@unittest.skipUnless(all(os.path.isfile(os.path.join(d, f)) for d, f in (
    (TOKYO, "2026-10-20T143453Z-snapshot.json"),
    (TOKYO, "2026-10-15T185514Z-snapshot.json"),
    (DEPLOYED, "2026-10-08T092521Z-snapshot.json"))),
    "デプロイ先の雪像が無い (リポジトリには入れない)")
class Deployed23(unittest.TestCase):
    """T23.2 の受け入れ基準 (実データ): tokyo の 10-20 で 1 回 (前の起動は 19.2 時間)、10-15 と
    nagoya の 10-08 で 0 件。**10-20 の 241 件は状態ファイルから読み戻した行** (その中の
    `start` も直前に `shutdown` があるので数えない)。"""

    def load(self, d, name):
        with open(os.path.join(d, name), encoding="utf-8") as f:
            return json.load(f)

    def test_the_silent_restart_of_2026_10_17_is_found_once(self):
        row = sc._p23_silent_restarts(
            {"b": self.load(TOKYO, "2026-10-20T143453Z-snapshot.json")}, sd.PHASE22)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("1 回 (**2026-10-17 22:14:52Z** 版 `0.22.6+000681e`、前の起動は 19.2 時間)",
                      row[2])
        self.assertIn("読み戻し 241 件", row[4])

    def test_every_other_restart_has_a_shutdown_before_it(self):
        for d, name in ((TOKYO, "2026-10-15T185514Z-snapshot.json"),
                        (DEPLOYED, "2026-10-08T092521Z-snapshot.json")):
            row = sc._p23_silent_restarts({"b": self.load(d, name)}, sd.PHASE22)
            self.assertEqual(row[3], sd.MET, name)
            self.assertTrue(row[2].startswith("0 件 ("), (name, row[2]))

    def test_the_phase23_rows_on_the_tokyo_pair(self):
        """T23.1 の受け入れ基準 (実データ): 10-17 → 10-20 の phase23 の 16 行。

        親が T23.0 の道具 (`scripts/error-mix.py`) で出した数字と同じになること。
        0 時間の雪像は 10-17T030257Z (10-17T03:02:22Z の起動、2 分後)。
        """
        a = os.path.join(TOKYO, "2026-10-17T030107Z-snapshot.json")
        b = os.path.join(TOKYO, "2026-10-20T143453Z-snapshot.json")
        z = os.path.join(TOKYO, "2026-10-17T030257Z-snapshot.json")
        rows = build([a, b, "--no-dns", "--criteria", "phase23", "--zero", z])["criteria"]["rows"]
        self.assertEqual(len(rows), 16)
        # (2) dns のミスは試行あたり 0.0850 (170 / 2,000)、名前解決に成功した試行あたり 0.0929
        self.assertEqual(rows[2][3], sd.SHOWN)
        self.assertIn("試行あたり **0.0850** (170 / 2,000)、名前解決に成功した試行あたり "
                      "**0.0929** (170 / 1,830)", rows[2][2])
        self.assertIn("(前: 試行あたり 0.0000 (0 / 1,991)", rows[2][2])
        # (3) heap は「0 時間の雪像と起動が違う」に `/events` の `start` の時刻が付く
        self.assertEqual(rows[8][3], sd.UNKNOWN)
        self.assertIn("**0 時間の雪像と起動が違う** (`/events` の `start` が "
                      "2026-10-17T03:02:22Z と 22:14:52Z、同じ版 `0.22.6+000681e` で起動し直している)",
                      rows[8][2])
        # (1) 接続のエラーは名前解決に成功した試行あたり 0.0164 (30 / 1,830) で、前の 1.36 倍 → 満たした
        self.assertEqual(rows[12][3], sd.MET)
        self.assertIn("**0.0164** / 名前解決に成功した試行 (30 件 / 1,830 本: `refused` 3、"
                      "`unreachable` 2、`timeout` 25。", rows[12][2])
        self.assertIn("前 0.0121 = 24 件 / 1,991 本、1.36 倍)", rows[12][2])
        # phase22 の物差しは同じ雪像で 7.80 倍 → 表示だけとして隣に残る
        self.assertEqual(rows[13][3], sd.SHOWN)
        self.assertIn("7.80 倍", rows[13][2])
        # (4) 無言の再起動は 1 回
        self.assertEqual(rows[15][3], sd.MISSED)
        self.assertIn("1 回 (**2026-10-17 22:14:52Z**", rows[15][2])


if __name__ == "__main__":
    unittest.main()

@unittest.skipUnless(os.path.isfile(os.path.join(TOKYO, "2026-10-22T210213Z-snapshot.json")),
                     "デプロイ先の雪像が無い (リポジトリには入れない)")
class Deployed24(unittest.TestCase):
    """T24.0 の受け入れ基準 (実データ): 2026-10-22 の tokyo の雪像を**手で組み立て直した** JSON で、

    10-21 の 2 回の無言の再起動が**ホストの再起動** (boot が変わった) と分かること。

    あの雪像は T24.0 より前の版が撮ったので `start` の文面に `boot` が無い。そこで
    `/events` の `start` の 3 行 (10-17 22:14:52Z・10-21 14:31:31Z・10-21 15:11:20Z) に、
    **分かっている事実**のとおりの `boot` を手で入れる: 10-21 の 2 回はホストの管理者が
    機械ごと起動し直したので、その 2 回で値が変わる (10-17 の行は旧版なので、その前の
    起動と同じホストかは言えない = 「判定できず」に残る)。
    """

    NAME = "2026-10-22T210213Z-snapshot.json"
    # 10-21 03:25:28Z の起動 (`shutdown` のあとの普通の再起動) / 10-21 14:31:31Z / 10-21 15:11:20Z。
    # **10-21 の 2 回でホストが起動し直した**ので、そこだけ値が変わる (10-17 22:14:52Z の行は旧版なので
    # そのまま = 1 つ前の起動と比べられない)
    BOOTS = {1792511528: "11111111-111", 1792593091: "22222222-222", 1792595480: "33333333-333"}

    def snap(self):
        import re
        with open(os.path.join(TOKYO, self.NAME), encoding="utf-8") as f:
            d = json.load(f)
        rows = d["events"]["events"]
        for e in rows:
            boot = self.BOOTS.get(e.get("at")) if e.get("kind") == "start" else None
            if boot:
                e["text"] = re.sub(r"( on port \d+)", r"\1 boot " + boot, e["text"], count=1)
        return d

    def test_the_two_restarts_of_2026_10_21_are_host_restarts(self):
        r = sc.silent_restarts(self.snap())
        self.assertEqual([s["at"] for s in r["host"]], [1792593091, 1792595480])
        self.assertEqual(r["process"], [])          # プロセスだけの再起動は 0 回
        self.assertEqual(r["boot_unknown"], 1)      # 10-17 の行は前の起動の boot が無い
        text = sc.silent_restart_text(r)
        self.assertIn("3 回 (", text)
        self.assertIn("うちホストの再起動 (boot が変わった回) 2 回、プロセスだけの再起動 0 回", text)
        self.assertIn("`boot` の無い `start` 1 件は判定できない", text)
        row = sc._p24_silent_restarts({"b": self.snap()}, sd.PHASE22)
        # 「boot の無い 1 件」が残るので 0 回とは書かない (判定できず。数から外した 2 回は理由つき)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("こちらが原因の無言の再起動", row[0])
        self.assertIn("**ホストの再起動** (boot が変わった) `2026-10-21 14:31:31Z`、"
                      "`2026-10-21 15:11:20Z` は数から外した", row[4])
        self.assertIn("`boot` の無い `start` が 1 件ある (T24.0 より前の版が書いた行)", row[4])


class Criteria24(unittest.TestCase):
    """`_p24_heap` (T24.4): heap の行を**起動から 60 秒の基準** (`memory.baseline`) との差で読む。

    行そのものはここで確かめ、`RULES["phase24"]` への配線は T24.2。材料は後の雪像の
    `/status` の `memory` だけで、**0 時間の雪像 (`--zero`) は要らない**。数字は T23.99 の実測
    (0 時間 17.3 MB、29.8 時間後 19.5 MB) に合わせてある。
    """

    T0 = 1790000000

    def snap(self, uptime, memory, taken=None):
        """`testdata/snapshot-b.json` を土台に、起動からの時間と `/status` の `memory` だけ載せ替える。"""
        d = read(B)
        d["uptime_secs"] = uptime
        d["taken_at"] = taken if taken is not None else self.T0 + uptime
        d["status"] = dict(d.get("status") or {})
        d["status"]["memory"] = memory
        return d

    def row(self, uptime, memory, th=None):
        # 閾の値は phase23 と同じ (`heap_growth_mb` 5.0。T24.2 の PHASE24 も同じ値)
        return sc._p24_heap({"b": self.snap(uptime, memory)}, th or sd.PHASE23)

    @staticmethod
    def mem(baseline, heap=19_500_000, mmap=0, cache=0, **extra):
        """`/status` の `memory` (heap の行が読む欄だけ)。"""
        m = {"heap_used": heap, "mmap": mmap, "cache_memory": cache,
             "heap_free": 1_000_000, "rss": 30_100_000, "baseline": baseline}
        m.update(extra)
        return m

    @staticmethod
    def base(at, bytes_):
        return {"at": at, "bytes": bytes_}

    def test_the_baseline_alone_makes_the_row_readable(self):
        """受け入れ基準: `baseline` のある雪像では、0 時間の雪像が無くても差が出る。"""
        uptime = 29.8 * 3600
        row = self.row(uptime, self.mem(self.base(self.T0 + 60, 17_300_000)))
        self.assertEqual(row[3], sd.MET)
        self.assertIn("**+2.2** MB (17.3 → 19.5 MB)", row[2])
        # 基準がいつ取られたかが読める (起動から 60 秒)
        self.assertIn("基準は 17.3 MB", row[2])
        self.assertIn("起動から 60 秒", row[2])
        self.assertIn("**0 時間の雪像 (`--zero`) は要らない**", row[4])
        # 行の名前は「起動から 60 秒の基準からの増え」
        self.assertIn("起動から 60 秒の基準からの増えが 5 MB 未満", row[0])
        self.assertEqual(row[1], "< 5 MB")

    def test_five_megabytes_or_more_is_missed(self):
        uptime = 30 * 3600
        start = self.T0
        # ちょうど 5 MB は「届かず」(閾は「5 MB 未満」)
        for heap, verdict in ((22_299_999, sd.MET), (22_300_000, sd.MISSED)):
            row = self.row(uptime, self.mem(self.base(start + 60, 17_300_000), heap=heap))
            self.assertEqual(row[3], verdict, f"{heap}")

    def test_without_a_baseline_the_row_is_the_phase23_one(self):
        """`baseline` が無い雪像 (T24.4 より前の版・起動から 60 秒以内) は今までどおり。"""
        mem = self.mem(None)
        d = {"b": self.snap(3600, mem)}
        self.assertEqual(sc._p24_heap(d, sd.PHASE23), sc._p23_heap(d, sd.PHASE23))
        row = self.row(3600, mem)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`--zero` (0 時間の雪像) が渡されていない", row[4])

    def test_a_baseline_from_a_previous_run_cannot_be_used(self):
        """状態ファイルは起動をまたいで残る。前の起動が残した値では判定しない。"""
        uptime = 2 * 3600
        start = self.T0
        row = self.row(uptime, self.mem(self.base(start - 3600, 17_300_000)))
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("前の起動が残した基準", row[4])

    def test_a_missing_column_is_unknown_not_zero(self):
        """`cache_memory` の欄が無い版を 0 として扱わない (phase20 の決まりと同じ)。"""
        row = self.row(3600, self.mem(self.base(self.T0 + 60, 17_300_000), cache=None))
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`cache_memory` が無い", row[4])
        self.assertIn("`memory.baseline` はある", row[4])

    def test_the_baseline_is_not_mixed_with_the_zero_snapshot(self):
        """`--zero` が渡されていても、`baseline` があれば**そちらだけ**で読む (基準を混ぜない)。"""
        uptime = 29.8 * 3600
        b = self.snap(uptime, self.mem(self.base(self.T0 + 60, 17_300_000)))
        z = self.snap(120, {"heap_used": 16_000_000, "mmap": 0, "cache_memory": 0})
        row = sc._p24_heap({"b": b, "zero_snapshot": z}, sd.PHASE23)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("17.3 → 19.5 MB", row[2])
        self.assertNotIn("16.0", row[2])


class PastOutputs24(unittest.TestCase):
    """T24.2: phase24 を足しても **phase22 と phase23 の出力** (§2 の平常時の表と §9 の判定表、json の
    `history` と `criteria`) が 1 文字も変わらないこと。値は phase24 を足す前の main (`081de1c`) で取った
    sha256 の頭 16 桁 (`PastOutputs22` が見ている phase14〜phase21 と同じ形)。
    **ここが落ちたら過去の判定を変えている** (過去の判定を同じ命令で出し直せなくなる)。"""

    DIGESTS = {
        "phase22": ("8cb4871c7c1fc4b5", "5ba06e278cad255c"),
        "phase23": ("76ff06b300c560f4", "0d2bad06e37c10f4"),
    }

    def test_the_past_criteria_print_the_same(self):
        import hashlib
        for name, (md_digest, js_digest) in self.DIGESTS.items():
            md = run([A, B, "--no-dns", "--criteria", name])
            js = json.loads(run([A, B, "--no-dns", "--criteria", name, "--out", "json"]))
            part = md[md.index("## 2."):md.index("## 3.")] + md[md.index("## 9."):]
            body = json.dumps({"history": js["history"], "criteria": js["criteria"]},
                              ensure_ascii=False, sort_keys=True)
            self.assertEqual(hashlib.sha256(part.encode()).hexdigest()[:16], md_digest, name)
            self.assertEqual(hashlib.sha256(body.encode()).hexdigest()[:16], js_digest, name)


class Criteria24Rows(unittest.TestCase):
    """`--criteria phase24` (T24.2): phase23 の 16 行をそのまま持ち越し、**9 番目 (heap) と 16 番目
    (無言の再起動) の 2 行だけ**を差し替えたもの。閾の値は phase23 と同じ (`PHASE24 == PHASE23`)。

    ほかの 14 行は `RULES["phase23"]` と同じ関数なので、物差しも材料も変わらない。行 1 つずつの読み方は
    `Criteria24` (`_p24_heap`) と `Criteria24BootSplit` (`_p24_silent_restarts`) が見ているので、
    ここは**配線**と、phase24 の表として読めることを見る。実データは `Deployed24Rows`。
    """

    # 表示だけの行 (phase21 の 4 行 + `/events` の行 + phase22 の接続のエラーの行)
    SHOWN_ROWS = (0, 2, 3, 5, 6, 13)
    T0 = 1790000000
    B1 = "aaaaaaaaaaaa"           # ホストの起動 1 (`boot` は先頭 12 文字で比べる)
    B2 = "bbbbbbbbbbbb"           # ホストの起動 2 (機械ごと起動し直した)
    EV0 = 1789043200              # `testdata/snapshot-b.json` の起動の時刻 (taken_at − uptime)

    def judge(self, a=None, b=None, extra=()):
        return build([a or A, b or B, "--no-dns", "--criteria", "phase24", *extra])["criteria"]

    # --- 表の形

    def test_phase24_gives_seventeen_rows_and_the_same_display_only_rows(self):
        c = self.judge()
        self.assertEqual(len(sd.RULES["phase24"]), 17)
        self.assertEqual(len(c["rows"]), 17)
        self.assertEqual([i for i, r in enumerate(c["rows"]) if r[3] == sd.SHOWN],
                         list(self.SHOWN_ROWS))
        self.assertEqual(c["display_only"], 6)
        # 閾の値は phase23 と同じ (**T24.3 で足した `connect_tail_ms` だけが違う**)
        self.assertEqual({k: v for k, v in sd.PHASE24.items() if k != "connect_tail_ms"},
                         sd.PHASE23)
        self.assertEqual(sd.PHASE24["connect_tail_ms"], 5.0)
        self.assertEqual(c["name"], "phase24")
        self.assertIs(sd.RULES["phase24"][8], sc._p24_heap)
        self.assertIs(sd.RULES["phase24"][15], sc._p24_silent_restarts)
        self.assertIs(sd.RULES["phase24"][16], sc._p24_connect_tail)

    def test_the_other_fourteen_rows_are_the_phase23_ones(self):
        for i in list(range(8)) + list(range(9, 15)):
            self.assertIs(sd.RULES["phase24"][i], sd.RULES["phase23"][i], i)

    def test_a_snapshot_without_baseline_or_boot_reads_as_phase23(self):
        """`memory.baseline` も `boot` も無い雪像 (T24.4・T24.0 より前の版) では 2 行とも phase23 と同じ。"""
        c24 = self.judge()
        c23 = build([A, B, "--no-dns", "--criteria", "phase23"])["criteria"]
        self.assertEqual([r[3] for r in c24["rows"][:16]], [r[3] for r in c23["rows"]])
        self.assertEqual(c24["rows"][8][3], sd.UNKNOWN)
        self.assertIn("`--zero` (0 時間の雪像) が渡されていない", c24["rows"][8][4])
        # 17 番目は `testdata` に `/recent` が無いので「判定できず」(0 とは書かない)
        self.assertEqual(c24["rows"][16][3], sd.UNKNOWN)
        self.assertIn("後の雪像に `/recent` の部が無い", c24["rows"][16][4])

    # --- 9 番目: heap の行 (T24.4 の `memory.baseline`)

    def snap(self, uptime, **memory):
        """`testdata/snapshot-b.json` に、起動からの時間と `/status` の `memory` だけ載せ替える。"""
        b = read(B)
        b["uptime_secs"] = uptime
        b["taken_at"] = self.T0 + uptime
        b["status"] = dict(b.get("status") or {})
        b["status"]["memory"] = dict({"heap_used": 19_500_000, "mmap": 0, "cache_memory": 0,
                                      "heap_free": 1_000_000, "rss": 30_100_000}, **memory)
        return b

    def test_the_baseline_alone_makes_the_heap_row_readable(self):
        """`--zero` が無くても、起動から 60 秒の基準だけで「この起動でどれだけ増えたか」が読める。"""
        b = self.snap(29.8 * 3600, baseline={"at": self.T0 + 60, "bytes": 17_300_000})
        with written(b=b) as p:
            row = self.judge(b=p["b"])["rows"][8]
        self.assertEqual(row[3], sd.MET)
        self.assertIn("**+2.2** MB (17.3 → 19.5 MB)", row[2])
        self.assertIn("`heap_used + mmap − cache_memory` の起動から 60 秒の基準からの増えが 5 MB 未満",
                      row[0])
        self.assertIn("**0 時間の雪像 (`--zero`) は要らない**", row[4])

    def test_a_baseline_left_by_a_previous_run_is_not_used(self):
        """状態ファイルは起動をまたいで残る。前の起動が残した基準では判定しない (T24.4)。"""
        b = self.snap(2 * 3600, baseline={"at": self.T0 - 3600, "bytes": 17_300_000})
        with written(b=b) as p:
            row = self.judge(b=p["b"])["rows"][8]
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("前の起動が残した基準", row[4])

    # --- 16 番目: 無言の再起動を「こちらが原因」に読み替える (T24.0)

    def start(self, at, boot=None):
        text = ("version 0.1.0+bbbbbbb on port 8080"
                + (f" boot {boot}" if boot else "")
                + " (profile default, cache on, timeout 30s, max conns 240)")
        return {"at": at, "kind": "start", "text": text}

    @staticmethod
    def stop(at):
        return {"at": at, "kind": "shutdown", "text": "stop signal received; saving statistics"}

    def silent_row(self, last_boot):
        """普通の再起動のあと、`shutdown` を残さずにもう一度起動し直した雪像の 16 番目の行。"""
        t = self.EV0
        b = read(B)
        events = [self.start(t, self.B1), self.stop(t + 616), self.start(t + 658, self.B1),
                  self.start(t + 658 + 69150, last_boot)]
        b["events"] = {"events": events, "count": len(events), "kept": len(events),
                       "capacity": 512, "restored": 0, "truncated": False}
        with written(b=b) as p:
            return self.judge(b=p["b"])["rows"][15]

    def test_a_host_restart_is_written_with_its_time_and_left_out_of_the_count(self):
        """`boot` が変わった回 (ホストのカーネルごとの再起動) は時刻つきで書き、**数から外す**。"""
        row = self.silent_row(self.B2)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("こちらが原因の無言の再起動", row[0])
        self.assertIn("うちホストの再起動 (boot が変わった回) 1 回、プロセスだけの再起動 0 回", row[2])
        self.assertIn("**ホストの再起動** (boot が変わった) `2026-09-11 07:50:08Z` は数から外した", row[4])
        self.assertIn("ホストの管理者によるもの。こちらでは防げない", row[4])

    def test_the_same_boot_is_our_own_restart_and_is_missed(self):
        """`boot` が同じ = プロセスだけの起動し直し (落ちたか、外から止められた) は 0 回でない。"""
        row = self.silent_row(self.B1)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**同じ `boot` のまま起動し直している** `2026-09-11 07:50:08Z` (前の起動は",
                      row[4])
        self.assertIn("落ちたか、外から止められた", row[4])

    def test_the_markdown_names_the_phase24_parts(self):
        md = run([A, B, "--no-dns", "--criteria", "phase24"])
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria phase24`)", md)
        self.assertIn("前の 16 行は phase23 と同じ物差しで", md)
        self.assertIn("**起動から 60 秒で 1 回だけ**", md)
        self.assertIn("**数から外します**", md)
        self.assertIn("(3) 17 番目 (T24.3) は", md)
        self.assertIn("**そのときのホストの RTT + 5 ms**", md)
        self.assertIn("**その部が雪像に無い行は「判定できず」**", md)

    # --- 17 番目: 確立の尾を「そのときのホストの RTT + 5 ms」で読む (T24.3)

    @staticmethod
    def recent(rows):
        """`/recent` の部を作る。`rows` は `(ms.connect, rtt_ms.origin, reason[, reqs])` の並び。

        `at` は 60 秒ずつ進める (窓の両端が実測の欄に出る)。
        """
        out = []
        for i, row in enumerate(rows):
            cms, rtt, reason = row[:3]
            reqs = row[3] if len(row) > 3 else 0
            out.append({"at": 1789043200 + i * 60, "kind": "connect", "reqs": reqs,
                        "target": "example.com:443", "client": "192.0.2.1", "reason": reason,
                        "ms": {"dns": 0, "connect": cms},
                        "rtt_ms": {"client": None, "origin": rtt}})
        return {"recent": out, "count": len(out), "kept": len(out), "capacity": 2000,
                "truncated": False}

    def tail_row(self, rows, a=None):
        b = read(B)
        b["recent"] = self.recent(rows)
        if a:
            with written(a=a, b=b) as p:
                return self.judge(a=p["a"], b=p["b"])["rows"][16]
        with written(b=b) as p:
            return self.judge(b=p["b"])["rows"][16]

    def test_the_tail_is_distance_when_the_excess_is_under_five_ms(self):
        """確立 − RTT が 1 ms だけの 100 本。尾は「距離」で、取り分は中央値で +1.0 ms。"""
        row = self.tail_row([(100 + rtt + 1, 100 + rtt, "ok") for rtt in range(100)])
        self.assertEqual(row[3], sd.MET)
        self.assertIn("CONNECT 確立の尾 (`p95`) が、同じ窓のカーネル RTT の `p95` + 5 ms 未満", row[0])
        self.assertEqual(row[1], "< RTT p95 + 5 ms")
        self.assertIn("**+1.0** ms", row[2])
        self.assertIn("プロキシの取り分 (中央値) **+1.0** ms", row[2])
        self.assertIn("= **距離**", row[2])
        # `/recent` の窓と本数を必ず書く (2,000 件の環状)。`at` は 60 秒ずつ
        self.assertIn("の connect 100 本 (確立と RTT がそろったのは 100 本)", row[2])
        self.assertIn("〜", row[2])
        self.assertIn("Z", row[2])
        self.assertIn("同じ 100 本の `ms.connect − rtt_ms.origin` の p95", row[2])
        self.assertIn("**同じ行で対にした**差", row[4])
        self.assertIn("**2 つの分位点の差ではない**", row[4])

    def test_an_excess_of_five_ms_or_more_is_not_distance(self):
        """5 ms ちょうどから「距離では説明できない」(N = 5 は「未満」)。"""
        row = self.tail_row([(100 + rtt + 5, 100 + rtt, "ok") for rtt in range(100)])
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**+5.0** ms", row[2])
        self.assertIn("= **距離では説明できない**", row[2])

    def test_the_boundary_of_the_tail_is_strictly_under_five(self):
        row = self.tail_row([(100 + rtt + 4.9, 100 + rtt, "ok") for rtt in range(100)])
        self.assertEqual(row[3], sd.MET)
        self.assertIn("**+4.9** ms", row[2])

    def test_failed_rows_and_unread_rtt_are_left_out_and_counted(self):
        """確立していない行 (`error:`) と `rtt_ms.origin` が `null` の行は対にしない。"""
        rows = [(101 + rtt, 100 + rtt, "ok") for rtt in range(50)]
        rows += [(0, None, "error:dns")] * 3 + [(0, None, "error:timeout")] * 2
        rows += [(999, None, "ok")] * 2
        row = self.tail_row(rows)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("の connect 57 本 (確立と RTT がそろったのは 50 本。外した: 失敗 5 本、"
                      "`rtt_ms.origin` が `null` の 2 本)", row[2])
        # 外した行の 999 ms は尾に入らない (入っていれば +899 ms で「届かず」)
        self.assertIn("**+1.0** ms", row[2])

    def test_without_any_pairable_row_the_verdict_is_unknown(self):
        row = self.tail_row([(12, None, "ok")] * 4)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("対にできる行 0 本 (connect 4 本)", row[2])
        self.assertIn("`rtt_ms.origin` が `null` 4 本", row[4])

    def test_a_recent_part_without_connect_rows_is_unknown_not_zero(self):
        b = read(B)
        b["recent"] = self.recent([(5, 5, "ok")])
        b["recent"]["recent"][0]["kind"] = "http"
        with written(b=b) as p:
            row = self.judge(b=p["b"])["rows"][16]
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("connect の行 0 本 (`/recent` 1 行)", row[2])

    def test_the_weight_is_the_request_count_of_the_row(self):
        """重みは行ごとの `reqs` (「要求で重み付け」)。中央値が重い行に寄る。"""
        self.assertEqual(sc.weighted_quantile([(10.0, 1), (20.0, 9)], 0.5), 20.0)
        self.assertEqual(sc.weighted_quantile([(10.0, 9), (20.0, 1)], 0.5), 10.0)
        self.assertEqual(sc.weighted_quantile([(10.0, 1), (20.0, 1)], 0.95), 20.0)
        self.assertIsNone(sc.weighted_quantile([], 0.5))
        self.assertIsNone(sc.weighted_quantile([(10.0, 0)], 0.5))

    def test_a_row_that_carried_many_requests_pulls_the_median(self):
        """`reqs` の重い 1 本が中央値を決める (connect の行は `reqs` 0 なので 1 として数える)。"""
        rows: list = [(100 + rtt, 100 + rtt, "ok") for rtt in range(9)]   # 差 0 ms × 9 本
        rows.append((200, 100, "ok", 100))                                 # 差 +100 ms を 100 要求ぶん
        row = self.tail_row(rows)
        self.assertIn("プロキシの取り分 (中央値) **+100.0** ms", row[2])
        self.assertEqual(row[3], sd.MISSED)


@unittest.skipUnless(all(os.path.isfile(os.path.join(d, f)) for d, f in (
    (TOKYO, "2026-10-17T030107Z-snapshot.json"),
    (TOKYO, "2026-10-20T143453Z-snapshot.json"),
    (TOKYO, "2026-10-17T030257Z-snapshot.json"))),
    "デプロイ先の雪像が無い (リポジトリには入れない)")
class Deployed24Rows(unittest.TestCase):
    """T24.2 の受け入れ基準 (実データ): tokyo の 10-17 → 10-20 で `--criteria phase24` の 17 行が出て、

    heap の行と無言の再起動の行が**その雪像で読める形**になっていること。この 2 枚は T24.0 と T24.4 より
    前の版が撮ったので `start` に `boot` が無く `memory.baseline` も無い — つまり**どちらも phase23 の
    読み方に戻る** (過去の判定を同じ命令で出し直せることの、実データでの確認)。
    """

    A = os.path.join(TOKYO, "2026-10-17T030107Z-snapshot.json")
    B = os.path.join(TOKYO, "2026-10-20T143453Z-snapshot.json")
    Z = os.path.join(TOKYO, "2026-10-17T030257Z-snapshot.json")

    def rows(self):
        return build([self.A, self.B, "--no-dns", "--criteria", "phase24", "--zero", self.Z])

    def test_the_phase24_rows_on_the_tokyo_pair(self):
        c = self.rows()["criteria"]
        self.assertEqual(len(c["rows"]), 17)
        self.assertEqual(c["display_only"], 6)
        # heap は `memory.baseline` が無いので、phase23 と同じく 0 時間の雪像との差で「起動が違う」と出る
        self.assertEqual(c["rows"][8][3], sd.UNKNOWN)
        self.assertIn("**0 時間の雪像と起動が違う** (`/events` の `start` が "
                      "2026-10-17T03:02:22Z と 22:14:52Z", c["rows"][8][2])
        # 無言の再起動は 1 回 (10-17 22:14:52Z)。ただしこの 2 枚の `start` には `boot` が無いので
        # 「ホストか、プロセスだけか」を言えず、**0 回とは書かない** (phase23 は同じ 1 回を「届かず」と書く)
        self.assertEqual(c["rows"][15][3], sd.UNKNOWN)
        self.assertEqual(build([self.A, self.B, "--no-dns", "--criteria", "phase23", "--zero", self.Z])
                         ["criteria"]["rows"][15][3], sd.MISSED)
        self.assertIn("1 回 (**2026-10-17 22:14:52Z**", c["rows"][15][2])
        self.assertIn("`boot` の無い `start` が 1 件ある", c["rows"][15][4])

    def test_the_tail_row_on_the_tokyo_pair(self):
        """T24.3: 10-20 の `/recent` (2,000 本) で、尾は**距離** (+0.4 ms)。

        確立は個票の 1,800 本の**同じ行の差**で見る (失敗 200 本は確立していないので外す)。
        """
        row = self.rows()["criteria"]["rows"][16]
        self.assertEqual(row[3], sd.MET)
        self.assertIn("**+0.4** ms (同じ 1,800 本の `ms.connect − rtt_ms.origin` の p95。"
                      "確立 p50 13.0 / p95 262.0 ms、RTT p50 16.4 / p95 271.6 ms)", row[2])
        self.assertIn("プロキシの取り分 (中央値) **-1.3** ms = **距離**", row[2])
        self.assertIn("`/recent` の 2026-10-20 06:42:56Z〜2026-10-20 14:34:47Z の connect 2,000 本 "
                      "(確立と RTT がそろったのは 1,800 本。外した: 失敗 200 本)", row[2])


@unittest.skipUnless(all(os.path.isfile(os.path.join(TOKYO, f)) for f in (
    "2026-10-20T155229Z-snapshot.json",
    "2026-10-22T210213Z-snapshot.json")),
    "デプロイ先の雪像が無い (リポジトリには入れない)")
class Deployed243Rows(unittest.TestCase):
    """T24.3 の受け入れ基準 (実データ): **2026-10-22 と 2026-10-20 の 2 枚**で行が出る (数値と窓)。

    この 2 枚は T24.3 より前の版 (`0.23.3`) の雪像で、`/recent` の connect は 1,999 / 1,994 本。
    尾は**同じ行の差**で読む (`/history` の平常時の p95 318.5 ms とは母集団が違う)。
    """

    A = os.path.join(TOKYO, "2026-10-20T155229Z-snapshot.json")
    B = os.path.join(TOKYO, "2026-10-22T210213Z-snapshot.json")

    def rows(self):
        return build([self.A, self.B, "--no-dns", "--criteria", "phase24"])["criteria"]["rows"]

    def test_the_tail_row_on_the_2026_10_22_pair(self):
        row = self.rows()[16]
        self.assertEqual(len(self.rows()), 17)
        self.assertEqual(row[0], "CONNECT 確立の尾 (`p95`) が、同じ窓のカーネル RTT の `p95` + 5 ms 未満 "
                                 "(尾は距離)")
        self.assertEqual(row[3], sd.MET)
        # 1,959 本の同じ行の差の p95 は +0.1 ms = 距離。取り分 (中央値) は -1.2 ms (phase14 は -0.21)
        self.assertIn("**+0.1** ms (同じ 1,959 本の `ms.connect − rtt_ms.origin` の p95。"
                      "確立 p50 13.0 / p95 259.0 ms、RTT p50 19.0 / p95 263.5 ms)", row[2])
        self.assertIn("プロキシの取り分 (中央値) **-1.2** ms = **距離**", row[2])
        # `/recent` は 2,000 件の環状なので窓と本数が実測の欄に出る
        self.assertIn("`/recent` の 2026-10-22 17:31:23Z〜2026-10-22 21:02:09Z の connect 1,999 本 "
                      "(確立と RTT がそろったのは 1,959 本。外した: 失敗 40 本)", row[2])
        # `/history` の p95 とは母集団が違うと注記する (混ぜない)
        self.assertIn("`/history` の平常時の確立 p95 (318.5 ms) とは母集団が違う", row[4])

    def test_the_markdown_shows_the_row_with_its_window(self):
        md = run([self.A, self.B, "--no-dns", "--criteria", "phase24"])
        self.assertIn("| CONNECT 確立の尾 (`p95`) が、同じ窓のカーネル RTT の `p95` + 5 ms 未満 "
                      "(尾は距離) | < RTT p95 + 5 ms |", md)
        self.assertIn("の connect 1,999 本", md)



class Criteria25Rows(unittest.TestCase):
    """`--criteria phase25` (T27.0): phase24 の 17 行のうち、T25.99 で物差しの穴だった 3 行。

    (a) 無言の再起動は前の雪像の取得より後の `start` だけ、(b) 接続のエラーは試行 1,000 本の下限、
    (c) 引き直しは 1 名前あたり。数字は T25.99 の 2 枚 (`2026-10-22T000000Z` → `2026-10-24T184027Z`) から採った。
    """

    SINCE = 1792713604       # 前の雪像 (サーバーの日次) の取得 = 2026-10-23 00:00:04Z
    BOOT = "2c725916-e89"

    @staticmethod
    def start(at, boot=None, version="0.25.4+8b0fce0"):
        text = f"version {version} on port 60357"
        if boot:
            text += f" boot {boot}"
        return {"at": at, "kind": "start", "text": text + " (profile default, cache on, timeout 30s, max conns 240)"}

    @staticmethod
    def stop(at):
        return {"at": at, "kind": "shutdown", "text": "stop signal received; saving statistics"}

    def ctx(self, events, taken_at=1792867228, uptime=129909, since=SINCE):
        b = read(B)
        b["taken_at"], b["uptime_secs"] = taken_at, uptime
        b["events"] = {"events": events, "count": len(events), "kept": len(events),
                       "capacity": 512, "restored": 0, "truncated": False}
        return {"a": {"taken_at": since}, "b": b}

    def t2599_events(self):
        """T25.99 の形: 10-17 と 10-21 の 2 回は `boot` の無い無言の再起動、0.25.4 は `shutdown` のあとの起動。"""
        t = 1792200000
        return [self.start(t, version="0.22.6+000681e"),
                self.start(t + 60000, version="0.22.6+000681e"),          # 無言 (boot なし)
                self.start(t + 300000, version="0.23.3+ae7c190"),         # 無言 (boot なし)
                self.stop(1792737273), self.start(1792737319, self.BOOT)]

    # --- 表の形

    def test_phase25_keeps_the_seventeen_rows_and_swaps_three(self):
        p24, p25 = sd.RULES["phase24"], sd.RULES["phase25"]
        self.assertEqual(len(p25), 17)
        changed = [i for i, (x, y) in enumerate(zip(p24, p25)) if x is not y]
        self.assertEqual(changed, [12, 14, 15])
        self.assertIs(p25[12], sc._p25_connect_errors)
        self.assertIs(p25[14], sc._p25_refresh_per_name)
        self.assertIs(p25[15], sc._p25_silent_restarts)

    # --- (a) 無言の再起動

    def test_old_bootless_restarts_before_the_window_are_not_counted(self):
        c = self.ctx(self.t2599_events())
        # phase24 の行は古い 2 回の `boot` が無いので「判定できず」
        self.assertEqual(sc._p24_silent_restarts(c, sc.PHASE25)[3], sd.UNKNOWN)
        row = sc._p25_silent_restarts(c, sc.PHASE25)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("直前が `shutdown` の起動 1 件", row[2])
        self.assertIn("より後の 1 件を見た", row[4])
        self.assertIn("それより前の 3 件は前の締めで見た", row[4])

    def test_a_process_restart_inside_the_window_is_missed(self):
        ev = self.t2599_events() + [self.start(1792800000, self.BOOT)]
        row = sc._p25_silent_restarts(self.ctx(ev, uptime=67228), sc.PHASE25)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("同じ `boot` のまま起動し直している", row[4])

    def test_a_host_restart_inside_the_window_is_left_out(self):
        ev = self.t2599_events() + [self.start(1792800000, "ffffffff-999")]
        row = sc._p25_silent_restarts(self.ctx(ev, uptime=67228), sc.PHASE25)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("ホストの再起動", row[4])

    def test_a_bootless_restart_inside_the_window_is_unknown(self):
        ev = [self.start(1792700000, version="0.23.3+ae7c190"), self.start(1792750000, version="0.23.3+ae7c190")]
        row = sc._p25_silent_restarts(self.ctx(ev, uptime=117228), sc.PHASE25)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`boot` の無い `start` が 1 件", row[4])

    def test_no_start_in_the_window_is_met_only_when_the_process_is_older(self):
        ev = [self.stop(1792700000), self.start(1792700100, self.BOOT)]
        # 後の起動 (取得 − uptime) が前の雪像より前 = 2 枚は同じプロセス
        row = sc._p25_silent_restarts(self.ctx(ev, uptime=167128), sc.PHASE25)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("前の雪像より後に起動なし", row[2])
        # 後の起動が窓の中なのに `start` が環から落ちている
        row = sc._p25_silent_restarts(self.ctx(ev, uptime=100000), sc.PHASE25)
        self.assertEqual(row[3], sd.UNKNOWN)

    def test_without_events_or_taken_at_it_is_unknown(self):
        c = self.ctx([])
        del c["b"]["events"]
        self.assertEqual(sc._p25_silent_restarts(c, sc.PHASE25)[3], sd.UNKNOWN)
        c = self.ctx(self.t2599_events(), since=None)
        self.assertEqual(sc._p25_silent_restarts(c, sc.PHASE25)[3], sd.UNKNOWN)

    # --- (b) 接続のエラーの下限

    def errors_ctx(self, before, after):
        rec = Criteria23Rows.recent
        return {"a": {"recent": rec(before[0], fails=("timeout",) * before[1])},
                "b": {"recent": rec(after[0], fails=("timeout",) * after[1])}}

    def test_a_short_before_window_is_unknown_not_missed(self):
        c = self.errors_ctx((581, 0), (1999, 15))
        self.assertEqual(sc._p23_connect_errors(c, sc.PHASE25)[3], sd.MISSED)
        row = sc._p25_connect_errors(c, sc.PHASE25)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("前の試行 581 本", row[4])
        self.assertIn("1,000 本以上", row[1])

    def test_long_enough_windows_are_judged_as_before(self):
        self.assertEqual(sc._p25_connect_errors(self.errors_ctx((2000, 10), (2000, 12)), sc.PHASE25)[3], sd.MET)
        self.assertEqual(sc._p25_connect_errors(self.errors_ctx((2000, 10), (2000, 40)), sc.PHASE25)[3],
                         sd.MISSED)
        # 後が短くても同じ
        self.assertEqual(sc._p25_connect_errors(self.errors_ctx((2000, 10), (999, 1)), sc.PHASE25)[3],
                         sd.UNKNOWN)

    # --- (c) 1 名前あたりの引き直し

    @staticmethod
    def dns_ctx(before, after):
        """`(refreshes, entries, uptime_secs, expired)` の前と後。後は起動し直している (起動からの通算)。"""
        def snap(v):
            return {"uptime_secs": v[2], "status": {"dns": {"refreshes": v[0], "entries": v[1],
                                                            "misses_by_kind": {"expired": v[3], "warm_stale": 0}}}}
        return {"a": snap(before), "b": snap(after), "info": {"restarted": True}}

    def test_more_names_is_not_more_refreshes_per_name(self):
        # T25.99: 通算は 268.3 → 284.1 回/時 (1.06 倍) だが、名前は 506 → 811
        c = self.dns_ctx((8803, 506, 118120, 26), (10251, 811, 129909, 59))
        self.assertEqual(sc._p22_refresh_and_stale_misses(c, sc.PHASE25)[3], sd.MISSED)
        row = sc._p25_refresh_per_name(c, sc.PHASE25)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("**0.35** 回/時/名前 (前 0.53", row[2])
        self.assertIn("表示だけ: 通算 284.1 回/時", row[2])
        self.assertIn("`expired` + `warm_stale`", row[2])

    def test_refreshes_per_name_one_and_a_half_times_is_missed(self):
        c = self.dns_ctx((3600, 100, 3600, 0), (5400, 100, 3600, 0))
        self.assertEqual(sc._p25_refresh_per_name(c, sc.PHASE25)[3], sd.MISSED)
        c = self.dns_ctx((3600, 100, 3600, 0), (5399, 100, 3600, 0))
        self.assertEqual(sc._p25_refresh_per_name(c, sc.PHASE25)[3], sd.MET)

    def test_missing_entries_is_unknown(self):
        c = self.dns_ctx((3600, 100, 3600, 0), (3600, 100, 3600, 0))
        del c["b"]["status"]["dns"]["entries"]
        self.assertEqual(sc._p25_refresh_per_name(c, sc.PHASE25)[3], sd.UNKNOWN)


T2599_BEFORE = os.path.join(DEPLOYED, "tokyo", "2026-10-22T000000Z-snapshot.json")
T2599_AFTER = os.path.join(DEPLOYED, "tokyo", "2026-10-24T184027Z-snapshot.json")


@unittest.skipUnless(os.path.exists(T2599_BEFORE) and os.path.exists(T2599_AFTER),
                     "T25.99 の雪像 (status/tokyo/) が無い")
class Deployed25(unittest.TestCase):
    """T25.99 の 2 枚の phase25 (T27.0 の受け入れ基準): 直した 3 行が 判定できず・満たした・満たした。"""

    def test_t2599_pair(self):
        c = build([T2599_BEFORE, T2599_AFTER, "--no-dns", "--criteria", "phase25"])["criteria"]
        verdicts = [r[3] for r in c["rows"]]
        self.assertEqual(verdicts[12], sd.UNKNOWN)
        self.assertEqual(verdicts[14], sd.MET)
        self.assertEqual(verdicts[15], sd.MET)


class Criteria26Rows(unittest.TestCase):
    """`--criteria phase26` (T.29.10): phase25 の 17 行を動かさず、18〜25 番目に足した 8 行。

    数字は 0.28.48 の 24 時間 (`2026-10-27T024200Z`) の形を小さく写した作り物。
    """

    @staticmethod
    def snap(status=None, recent=None, log=None, errors=None, uptime=86400, taken_at=1793068922):
        s = {"uptime_secs": uptime, "taken_at": taken_at, "status": status or {}}
        if recent is not None:
            s["recent"] = {"recent": recent, "count": len(recent)}
        if log is not None:
            s["log"] = {"lines": log, "level": "info", "truncated": False}
        if errors is not None:
            s["errors"] = {"errors": errors}
        return s

    def ctx(self, b, a=None):
        return {"a": a or {}, "b": b, "info": {}}

    def test_phase26_keeps_the_seventeen_rows_of_phase25_and_adds_eight(self):
        p25, p26 = sd.RULES["phase25"], sd.RULES["phase26"]
        self.assertEqual(len(p26), 25)
        self.assertEqual(p26[:17], p25)
        self.assertIs(p26[17], sc._p26_header_timeouts)
        self.assertEqual(sd.CRITERIA["phase26"]["log_error_max"], 5)

    def test_the_header_timeouts_are_shown_with_the_recent_rows(self):
        rows = [{"at": 1793068000, "kind": "http", "reason": "error:header_timeout", "reqs": 1},
                {"at": 1793068100, "kind": "connect", "reason": "error:timeout", "reqs": 0}]
        b = self.snap({"rejected_requests": {"header_timeout": 12, "total": 12}}, recent=rows, uptime=43200)
        row = sc._p26_header_timeouts(self.ctx(b), sc.PHASE26)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertIn("**12** 件 (起動から 12.0 時間、24.0 件/24 時間)", row[2])
        self.assertIn("`error:header_timeout` 1 行 (うち 1 要求目を通したあとの 1 行)", row[2])
        # T.29.10 より前の版には欄が無い = 0 と書かない
        old = self.snap({"rejected_requests": {"bad_header": 0, "total": 0}})
        self.assertEqual(sc._p26_header_timeouts(self.ctx(old), sc.PHASE26)[3], sd.UNKNOWN)

    def test_log_errors_up_to_five_lines_are_met(self):
        def line(lv):
            return {"at": 1793068000, "level": lv, "msg": "connect imap.example:993 timed out"}
        ok = self.snap(log=[line("warn")] * 900 + [line("error")] * 5)
        self.assertEqual(sc._p26_log_errors(self.ctx(ok), sc.PHASE26)[3], sd.MET)
        bad = self.snap(log=[line("warn")] * 900 + [line("error")] * 6)
        row = sc._p26_log_errors(self.ctx(bad), sc.PHASE26)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**6** 行", row[2])
        self.assertEqual(sc._p26_log_errors(self.ctx(self.snap()), sc.PHASE26)[3], sd.UNKNOWN)

    def test_local_blocks_are_counted_with_repeats(self):
        errs = [{"at": 1, "cause": "nxdomain", "target": "imap.example:993", "repeats": 5},
                {"at": 2, "cause": "local", "target": "192.168.1.1:80", "repeats": 3}]
        row = sc._p26_local_blocks(self.ctx(self.snap(errors=errs)), sc.PHASE26)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**3** 件 (宛先 `192.168.1.1:80`)", row[2])
        self.assertEqual(sc._p26_local_blocks(self.ctx(self.snap(errors=errs[:1])), sc.PHASE26)[3], sd.MET)

    def test_the_connect_cut_row_counts_timeouts_and_the_slowest_success(self):
        rows = ([{"at": 10 + i, "kind": "connect", "target": "imap.example:993", "reason": "error:timeout",
                  "ms": {"connect": 3000}} for i in range(14)]
                + [{"at": 100, "kind": "connect", "target": "a.example:443", "reason": "server_eof",
                    "ms": {"connect": 2267}, "syn_retrans": 1},
                   {"at": 101, "kind": "connect", "target": "b.example:443", "reason": "client_eof",
                    "ms": {"connect": 11}, "syn_retrans": 2}])
        row = sc._p26_connect_cut(self.ctx(self.snap(recent=rows)), sc.PHASE26)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertIn("切った **14** 行 (ポート `993` 14)", row[2])
        self.assertIn("成功 2 本の確立の最大 **2,267** ms (2 秒以上 1 本)", row[2])
        self.assertIn("`syn_retrans` 1 が 1 本・2 が 1 本", row[2])

    def test_rejected_requests_are_shown_by_reason(self):
        b = self.snap({"rejected_requests": {"method": 3, "bad_header": 0, "header_timeout": 2, "total": 5}})
        row = sc._p26_rejected(self.ctx(b), sc.PHASE26)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertIn("**5** 本", row[2])
        self.assertIn("`method` 3、`header_timeout` 2", row[2])

    def test_the_rss_gap_uses_the_t2925_threshold(self):
        mem = {"rss": 65_794_048, "heap_used": 20_359_248, "mmap": 15_671_296, "cache_memory": 14_382_613,
               "heap_free": 18_302_896}
        row = sc._p26_rss_gap(self.ctx(self.snap({"memory": mem})), sc.PHASE26)
        self.assertEqual(row[3], sd.MISSED)                  # 0.28.48 の 24 時間は 15.4 MB
        self.assertIn("**15.4** MB", row[2])
        mem["rss"] = 55_000_000
        self.assertEqual(sc._p26_rss_gap(self.ctx(self.snap({"memory": mem})), sc.PHASE26)[3], sd.MET)

    def test_the_cgroup_cpu_must_go_down(self):
        def snap(usage, up=3600):
            return self.snap({"kernel": {"cgroup_cpu": {"since_start": {
                "usage_usec": usage, "user_usec": usage // 4, "system_usec": usage - usage // 4}}}}, uptime=up)
        self.assertEqual(sc._p26_cgroup_cpu(self.ctx(snap(5_000_000), snap(5_400_000)), sc.PHASE26)[3], sd.MET)
        self.assertEqual(sc._p26_cgroup_cpu(self.ctx(snap(5_400_000), snap(5_400_000)), sc.PHASE26)[3],
                         sd.MISSED)
        self.assertEqual(sc._p26_cgroup_cpu(self.ctx(snap(5_400_000), {}), sc.PHASE26)[3], sd.UNKNOWN)

    @staticmethod
    def profile(shares, rows=720):
        """`profile-sample` と `cache-probe` の割合が `shares` の `/profile?res=60` を作る。"""
        roles = ["conn", "cache-probe", "profile-sample", "other"]
        cpu = 1000
        threads = [[cpu * (1 - sum(shares)), 60, []], [cpu * shares[1], 60, []], [cpu * shares[0], 60, []], 0]
        return {"interval_secs": 60, "roles": roles, "keys": ["t", "requests", "cpu_us", "threads"],
                "samples": [[i, 1, cpu, threads] for i in range(rows)]}

    def test_the_idle_role_shares_must_go_down(self):
        a, b = {"profile_res_60": self.profile((0.325, 0.234))}, {"profile_res_60": self.profile((0.20, 0.10))}
        row = sc._p26_role_shares(self.ctx(b, a), sc.PHASE26)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("`profile-sample` 32.5% → **20.0%**", row[2])
        b = {"profile_res_60": self.profile((0.20, 0.25))}
        self.assertEqual(sc._p26_role_shares(self.ctx(b, a), sc.PHASE26)[3], sd.MISSED)
        # 12 時間に満たない材料は参考
        b = {"profile_res_60": self.profile((0.20, 0.10), rows=60)}
        self.assertEqual(sc._p26_role_shares(self.ctx(b, a), sc.PHASE26)[3], sd.REFERENCE)


T2899_BEFORE = os.path.join(DEPLOYED, "tokyo", "2026-10-26T013119Z-snapshot.json")
T2899_AFTER = os.path.join(DEPLOYED, "tokyo", "2026-10-27T024200Z-snapshot.json")


@unittest.skipUnless(os.path.exists(T2899_BEFORE) and os.path.exists(T2899_AFTER),
                     "T.28.99 の雪像 (status/tokyo/) が無い")
class Deployed26(unittest.TestCase):
    """T.28.99 の 2 枚 (0.27.8 → 0.28.48 の 24 時間) で phase26 の表が出る (T.29.10 の受け入れ基準)。"""

    def test_t2899_pair(self):
        md = run([T2899_BEFORE, T2899_AFTER, "--no-dns", "--criteria", "phase26"])
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria phase26`)", md)
        c = build([T2899_BEFORE, T2899_AFTER, "--no-dns", "--criteria", "phase26"])["criteria"]
        rows = c["rows"]
        self.assertEqual(len(rows), 25)
        verdicts = [r[3] for r in rows]
        # 0.28.48 には `header_timeout` の欄が無い (0 と書かない)
        self.assertEqual(verdicts[17], sd.UNKNOWN)
        self.assertEqual(verdicts[18], sd.MET)                # `/log` の error 0 行
        self.assertEqual(verdicts[19], sd.MET)                # `local` 0 件
        self.assertIn("切った **14** 行 (ポート `993` 14)", rows[20][2])
        self.assertIn("**2,267** ms", rows[20][2])
        self.assertIn("どの理由も 0", rows[21][2])
        self.assertIn("**15.4** MB", rows[22][2])
