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

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
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
                          "phase22"])


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
                      "(phase22 の切り方。T22.1: 300 本 (T14.0) と、その期間の 1 標本の本数の中央値 "
                      "(前 100.0・後 200 本) の 3 倍の大きい方)", md)
        self.assertIn("(平常時 3 / 3 標本)", md)
        self.assertNotIn("T14.0 と同じ切り方", md)
        self.assertIn("**1 標本 300 本以上の窓 (バースト) を外した**もの同士で並べる (T14.0 と同じ切り方)",
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
        "phase14": ("a1276a7505c3bee8", "4d192151d9ac75fa"),
        "phase15": ("df7bf774ea9fec51", "9e156ba611dc8241"),
        "phase17": ("f7ec2a16f67c387d", "62e6c08ce78552a6"),
        "phase18": ("91d2e653088a51b0", "12ac626a2b58fb3a"),
        "phase20": ("e7a15a65fcb5d79d", "6d68fd96aa1c923d"),
        "phase21": ("fe89711b73a976e4", "cff8f5505a659290"),
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


if __name__ == "__main__":
    unittest.main()
