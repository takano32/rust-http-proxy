#!/usr/bin/env python3
"""判定表 phase20・phase21・phase22 の試験 (phase22 の平常時の切り方と、過去の出力が変わらないことも)。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

`scripts/test_snapshot_criteria.py` (3,874 行) を判定表ごとに割ったうちの 1 つ (T38.6)。中身は割る前と同じ。
作り物は `scripts/_criteria_testing.py` にある。
"""

import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _criteria_testing import (  # noqa: E402
    A,
    B,
    DEPLOYED,
    build,
    read,
    run,
    sd,
    set_column,
    written,
    conn_profile,
    zero_snapshot,
    with_probes,
    cached,
    long_profile,
    with_dns_slow,
    with_causes,
    Criteria17Parts,
)


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
        b = Criteria17Parts.warm(read(B), 32, 26)            # 枠が埋まり、追い出しが 26
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
            zero = self.row(8, extra=("--zero", p["z"]))     # 0 時間のスナップショットに欄が無い
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


class Criteria21(unittest.TestCase):
    """`--criteria phase21` (T21.2): phase20 の 10 行のうち、T20.99 で届かなかった 3 行の読み方を直したもの。

    平常時のミスは表示だけ (日ごとの幅と `misses_by_kind` の内訳)、conn 役の CPU/要求 は前か後の
    `/profile` が 12 時間に満たなければ「参考」、`ipv6.request_probes` は 0 時間のスナップショットから 3 時間以内なら
    「満たした」。B の起動は `Criteria17Parts.START`、`zero_snapshot()` はその 2 分後に撮った 1 枚。
    """

    SHOWN_ROWS = (0, 2, 3, 5)
    ZERO_AT = Criteria17Parts.START + 120

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
    """phase22 を足しても phase14〜phase21 の出力 (「平常時の前後」の表と判定表、json の `history` と
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


if __name__ == "__main__":
    unittest.main()
