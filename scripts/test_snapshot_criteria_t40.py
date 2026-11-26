#!/usr/bin/env python3
"""判定表 T40 の試験。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

作り物は `scripts/_criteria_testing.py` にある (T38.6。`/profile?res=60` の作り物は T39 の試験と同じ `Criteria39Parts`)。

**実物の 2 日の組** (`TwoDaysOfTheSameVersion`) は、同じ版 0.39.13 の 2 つの 24 時間 (2026-11-24 と 2026-11-26) を、
リポジトリに入れた材料だけで比べる (いつも回る)。1 日ぶんは `scripts/testdata/` の 2 つ:

    deployed-<日付>-profile_res_60.anon.json   `/profile?res=60` の匿名の写し (CPU の標本だけ。T40.1 と T40.4 で入れた)
    deployed-<日付>-history_dns.json           その日のスナップショットの `/history` から、行が読む数字だけを抜いたもの

行がスナップショットの側から読むのは、`/profile` の窓の名前解決の失敗の数 (`/history?res=60` と `res=3600` の
`errors_by_cause` の `dns`) だけ。スナップショットそのもの (1 枚 4.8 MB。接続元と宛先が入る) はリポジトリに入れないので、
その数と標本の時刻だけを抜いた: `t0` (最初の標本の時刻)・`interval_secs`・`dns` (数の並び)。res=60 は全部の 1,440 標本
(`/profile?res=60` の窓と同じ 24 時間)、res=3600 は最後の 72 標本 (行は、窓を res=60 が覆っていないときだけこちらを読む。
下の「`/profile` だけ入れ替わった組」の 804 本がそれ)。ほかに入っているのは、説明 (`what`)・元のスナップショットの時刻
(`snapshot`)・版 (`version`) の 3 つの文字列だけ。試験はこれから最小のスナップショットの形 (`profile_res_60` と `history`) を組む。
手元に実物のスナップショット (リポジトリの `status/`。リポジトリには入れない) があれば、`DeployedTwoDays` が、
最小の形と実物で行が 1 文字も違わないことを見る。
"""

import math
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _criteria_testing import (  # noqa: E402
    A,
    B,
    DEPLOYED,
    Criteria39Parts,
    build,
    read,
    run,
    sc,
    sd,
    written,
)

DATA = os.path.join(os.path.dirname(os.path.abspath(__file__)), "testdata")
# 同じ版 0.39.13 の 2 つの 24 時間。値は元のスナップショットの時刻 (手元の `status/tokyo/` の名前の頭)
DAYS = {"2026-11-24": "2026-11-24T120811Z", "2026-11-26": "2026-11-26T095623Z"}
# `deployed-<日付>-history_dns.json` の `what` (説明)。どちらの日も同じ文
HISTORY_DNS_WHAT = ("スナップショットの `/history` から、名前解決の失敗の数 (`errors_by_cause` の `dns`) だけを抜いたもの (T40.4)。"
                    "中身と使い方は `scripts/test_snapshot_criteria_t40.py` の頭に書いてある")


def history_dns_path(day):
    return os.path.join(DATA, "deployed-%s-history_dns.json" % day)


def profile_of(day):
    """その日の `/profile?res=60` の匿名の写し。"""
    return read(os.path.join(DATA, "deployed-%s-profile_res_60.anon.json" % day))


def history_of(day):
    """その日の `/history` を、名前解決の失敗の数だけの最小の形に組む (`errors_by_cause` は `dns` の 1 つだけ)。"""
    return {res: {"interval_secs": h["interval_secs"], "causes": ["dns"], "keys": ["t", "errors_by_cause"],
                  "samples": [[h["t0"] + h["interval_secs"] * i, [v]] for i, v in enumerate(h["dns"])]}
            for res, h in read(history_dns_path(day))["history"].items()}


class Criteria40ConnDayBand(Criteria39Parts, unittest.TestCase):
    """`--criteria T40` (T40.4): `conn` 役の CPU を値付けし合う行 (T39.9) に、同じ版の日ごとのぶれの帯を入れる。"""

    def row(self, c, **th):
        return sc._t40_conn_priced(c, dict(sc.T40_TH, **th))

    def t39(self, c):
        return sc._t39_conn_priced(c, sc.T39_TH)

    def dearer(self, k, wobble=None):
        """後の値段 (要求 1 本・中継 1 秒) がどちらも `k` 倍の組。値付けし合った比は `k` 倍になる。"""
        return self.c(self.profile(30, wobble=wobble), self.profile(30, us_req=1000 * k, us_relay=50 * k, wobble=wobble))

    def test_T40_replaces_only_the_conn_row_of_T39(self):
        t39, t40 = sd.RULES["T39"], sd.RULES["T40"]
        self.assertEqual(len(t39), len(t40))
        self.assertEqual([i for i in range(len(t39)) if t39[i] is not t40[i]], [4])
        self.assertIs(t40[4], sc._t40_conn_priced)
        # 閾は T39 のものに、帯の上の端を名前つきで足しただけ (下の端は「満たした」の閾 1.1 と同じ)
        self.assertEqual(sc.T40_TH, dict(sc.T39_TH, conn_day_band_max=1.3))
        self.assertEqual(sc.CONN_DAY_BAND_MAX, 1.3)
        self.assertEqual(sc.T40_TH["conn_per_resolved_factor"], 1.1)
        self.assertNotIn("conn_day_band_max", sc.T39_TH)
        self.assertEqual(sc.criteria_name("T40"), "T40")
        self.assertIn("T40", sc.INTRO)
        self.assertIn("T40", sd.CRITERIA)

    def test_the_intro_says_the_three_steps_with_the_same_numbers_as_the_thresholds(self):
        k, hi = sc.T40_TH["conn_per_resolved_factor"], sc.T40_TH["conn_day_band_max"]
        intro = sc.INTRO["T40"]
        self.assertTrue(intro.startswith("1〜27 行の並びと材料は T39 と同じです。直したのは 1 行 (T40.4): "), intro)
        self.assertIn(f"前の版の {k} 倍未満なら「満たした」、{k} 倍以上 {hi} 倍以下なら「判定できず」(同じ版の日ごとのぶれの内)、"
                      f"{hi} 倍を越えたら「届かず」です。", intro)
        self.assertIn("同じバイナリなのに 0.80 倍、前と後を入れ替えると 1.25 倍でした", intro)
        self.assertTrue(intro.endswith("ほかの 26 行は T39 と同じです。"), intro)

    def test_under_the_limit_is_met_and_only_the_label_and_the_limit_differ_from_T39(self):
        c = self.dearer(1.05)
        row, t39 = self.row(c), self.t39(c)
        self.assertEqual(row[3], sd.MET, row)
        self.assertEqual(t39[3], sd.MET)
        self.assertTrue(row[2].startswith("**1.050** 倍 (前の機械の値段に直した。"), row[2])
        # 実測・判定・出どころは T39 の行のまま。名前と閾の欄だけ、帯の読み方を足してある
        self.assertEqual(row[2:], t39[2:])
        self.assertEqual(row[0], t39[0].replace(
            "(T39.9。", "(1.1 倍以上 1.3 倍以下は同じ版の日ごとのぶれの内で判定できず、1.3 倍を越えたら届かず。T40.4。T39.9。"))
        self.assertIn("値付けし合って**、前の版の 1.1 倍未満 (1.1 倍以上 1.3 倍以下は", row[0])
        self.assertEqual(row[1], "< 前 ×1.1 (値付けし合った比。×1.1 以上 ×1.3 以下は日ごとのぶれの内で判定できず、×1.3 を越えたら届かず。"
                                 "同じ版の中のぶれ (1 枚の中) の内も判定できず)")

    def test_fewer_requests_with_the_same_prices_is_still_met(self):
        # T39 の試験と同じ形 (中継はそのままで要求が 0.6 倍): 値段が同じなら 1.000 倍
        row = self.row(self.c(self.profile(30), self.profile(18)))
        self.assertEqual(row[3], sd.MET, row)
        self.assertTrue(row[2].startswith("**1.000** 倍"), row[2])

    def test_inside_the_day_band_cannot_be_judged(self):
        # 同じ版の 2 日で出た 1.25 倍と同じくらい: T39 では届かず、T40 では判定できず
        c = self.dearer(1.2)
        row, t39 = self.row(c), self.t39(c)
        self.assertEqual(t39[3], sd.MISSED)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertEqual(row[2], t39[2])
        self.assertTrue(row[2].startswith("**1.200** 倍"), row[2])
        self.assertTrue(row[4].startswith(
            "値付けし合った比 1.200 倍は 1.1 倍以上だが、同じ版の日ごとのぶれの帯 (1.1〜1.3 倍。T40.4) の内。"
            "同じ版 0.39.13 の 2 つの 24 時間 (2026-11-24 と 2026-11-26) を比べると、同じバイナリなのに 0.80 倍、"
            "前と後を入れ替えると 1.25 倍だった。"
            "同じ版の中のぶれ (1 枚の中。最大 1.00 倍) は越えている (T39 の読み方では届かず)。"), row[4])
        # 理由のあとに、T39 の行の出どころがそのまま続く
        self.assertTrue(row[4].endswith(t39[4]), row[4])

    def test_over_the_day_band_is_missed(self):
        c = self.dearer(1.4)
        row, t39 = self.row(c), self.t39(c)
        self.assertEqual(t39[3], sd.MISSED)
        self.assertEqual(row[3], sd.MISSED, row)
        self.assertEqual(row[2], t39[2])
        self.assertTrue(row[4].startswith(
            "値付けし合った比 1.400 倍は、同じ版の日ごとのぶれの帯 (1.1〜1.3 倍。T40.4) も、同じ版の中のぶれ "
            "(1 枚の中。最大 1.00 倍) も越えている。"), row[4])
        self.assertTrue(row[4].endswith(t39[4]), row[4])

    def test_just_inside_and_just_outside_each_end(self):
        for k, want in ((1.09, sd.MET), (1.11, sd.UNKNOWN), (1.29, sd.UNKNOWN), (1.31, sd.MISSED)):
            with self.subTest(k=k):
                self.assertEqual(self.row(self.dearer(k))[3], want)

    def test_both_ends_of_the_band_are_inside(self):
        # 「1.1 倍以上 1.3 倍以下」: 端の値そのものは帯の内。浮動小数の丸めに頼らないよう、出た比そのものを閾に置いて見る
        c = self.dearer(1.2)
        g, _ = sc.conn_priced_index(c, sc.T40_TH)
        just_under, just_over = g * (1 - 1e-12), g * (1 + 1e-12)
        self.assertEqual(self.row(c, conn_day_band_max=g)[3], sd.UNKNOWN)
        self.assertEqual(self.row(c, conn_day_band_max=just_under)[3], sd.MISSED)
        self.assertEqual(self.row(c, conn_per_resolved_factor=g)[3], sd.UNKNOWN)
        self.assertEqual(self.row(c, conn_per_resolved_factor=just_over)[3], sd.MET)

    def test_the_machine_ruler_divides_before_the_band_is_read(self):
        # 後の機械が 2 倍遅く (物差しも 2 倍)、値段は 2.4 倍: 前の機械の値段に直すと 1.2 倍で、帯の内
        row = self.row(self.c(self.profile(30), self.profile(30, us_req=2400, us_relay=120, history_us=9600)))
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertTrue(row[2].startswith("**1.200** 倍"), row[2])

    # --- T39 から引き継いだ判定できず (同じ版の中のぶれの内・材料の不足) ---------------------------------

    # 同じ版の中で 1 時間おきに CPU が 1.3 倍と 0.77 倍に揺れる 1 枚 (T39 の試験と同じ)。1 枚の中のぶれは最大 1.69 倍
    WOBBLE = [1.3, 0.77]

    def test_over_the_band_but_inside_the_spread_of_one_profile_cannot_be_judged(self):
        # 帯 (1.3 倍) は越えたが、帯より広いぶれが 1 枚の中に出ている: T39 と同じく判定できず
        c = self.dearer(1.4, self.WOBBLE)
        row, t39 = self.row(c), self.t39(c)
        self.assertEqual(t39[3], sd.UNKNOWN)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertEqual(row[2], t39[2])
        self.assertIn("は最大 1.69 倍。", row[2])
        self.assertEqual(row[4], "値付けし合った比は同じ版の日ごとのぶれの帯 (1.1〜1.3 倍。T40.4) を越えているが、同じ版の中のぶれ "
                                 "(1 枚の中) の内なので届かずにしない: " + t39[4])
        # T39 の理由 (ぶれの大きさと、分ける材料が無いこと) はそのまま読める
        self.assertIn("値付けし合った比 1.400 倍は 1.1 倍以上だが、同じ版の中のぶれ (最大 1.69 倍) の内 (T39.9)。", row[4])
        self.assertIn("要求で払う分と時間で払う分に分ける材料はスナップショットに無い", row[4])

    def test_inside_both_the_band_and_the_spread_says_both(self):
        c = self.dearer(1.2, self.WOBBLE)
        row, t39 = self.row(c), self.t39(c)
        self.assertEqual(t39[3], sd.UNKNOWN)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertTrue(row[4].startswith("値付けし合った比 1.200 倍は 1.1 倍以上だが、同じ版の日ごとのぶれの帯 (1.1〜1.3 倍。T40.4) の内。"),
                        row[4])
        self.assertTrue(row[4].endswith("前と後を入れ替えると 1.25 倍だった。同じ版の中のぶれ (1 枚の中) で見ても判定できず: " + t39[4]),
                        row[4])

    def test_over_both_the_band_and_the_spread_is_missed(self):
        c = self.dearer(1.8, self.WOBBLE)
        row = self.row(c)
        self.assertEqual(self.t39(c)[3], sd.MISSED)
        self.assertEqual(row[3], sd.MISSED, row)
        self.assertTrue(row[4].startswith(
            "値付けし合った比 1.800 倍は、同じ版の日ごとのぶれの帯 (1.1〜1.3 倍。T40.4) も、同じ版の中のぶれ "
            "(1 枚の中。最大 1.69 倍) も越えている。"), row[4])

    def test_without_material_it_is_the_T39_row(self):
        # 材料が足りないとき (12 時間に満たない・`relay` の段が無い・値段が 0 以下) は、T39 の行の判定できずと理由のまま
        old = [s for s in self.STAGES if s != "relay"]
        for name, c, why in (
                ("短い", self.c(self.profile(30, hours=6), self.profile(30)), "前の `/profile` が 6.0 時間 (360 標本) で 12 時間に満たない"),
                ("段が無い", self.c(self.profile(30, stages=old), self.profile(30)),
                 "前の `/profile` に connect の段の `relay` が無い"),
                ("値段が 0 以下", self.c(self.profile(30, us_relay=-400), self.profile(30)),
                 "値段が 0 以下になった (要求と中継の時間で `conn` 役の CPU を説明できない)。"),
                ("後が短い", self.c(self.profile(30), self.profile(30, us_req=1200, us_relay=60, hours=11)),
                 "後の `/profile` が 11.0 時間 (660 標本) で 12 時間に満たない")):
            with self.subTest(name=name):
                row, t39 = self.row(c), self.t39(c)
                self.assertEqual(row[3], sd.UNKNOWN, row)
                self.assertEqual(row[2], "—")
                self.assertEqual(row[2:], t39[2:])
                self.assertTrue(row[4].startswith(why), row[4])
                self.assertIn("T40.4。T39.9。", row[0])
                self.assertIsNone(sc.conn_priced_index(c, sc.T40_TH))

    def test_the_index_is_the_number_the_T39_row_shows(self):
        # 行は T39 の行の文面に、同じ手順で出し直した数字 (`conn_priced_index`) で判定を足す。2 つが食い違わないこと
        for name, c in (("満たした", self.dearer(1.05)), ("帯の内", self.dearer(1.2)), ("帯の外", self.dearer(1.4)),
                        ("ぶれの内", self.dearer(1.4, self.WOBBLE)), ("要求が減る", self.c(self.profile(30), self.profile(18))),
                        ("中継が高い", self.c(self.profile(30), self.profile(18, us_relay=100)))):
            with self.subTest(name=name):
                g, worst = sc.conn_priced_index(c, sc.T40_TH)
                shown = self.t39(c)[2]
                self.assertTrue(shown.startswith(f"**{g:.3f}** 倍 (前の機械の値段に直した。"), shown)
                self.assertIn(f"値付けし合った比) は最大 {math.exp(worst):.2f} 倍。", shown)

    def test_the_other_26_rows_and_the_tally_are_the_same_as_T39(self):
        # 架空の 2 枚 (`/profile?res=60` を渡していないので、5 行目は材料が無くて判定できず)
        d39 = build([A, B, "--criteria", "T39", "--no-dns"])["criteria"]
        d40 = build([A, B, "--criteria", "T40", "--no-dns"])["criteria"]
        self.assertEqual((d39["name"], d40["name"]), ("T39", "T40"))
        self.assertEqual(len(d40["rows"]), 27)
        self.assertEqual([i for i in range(27) if d39["rows"][i] != d40["rows"][i]], [4])
        self.assertEqual(d40["rows"][4][2:], d39["rows"][4][2:])
        self.assertEqual(d40["tally"], d39["tally"])
        out = run([A, B, "--criteria", "T40", "--no-dns"])
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria T40`)\n\n" + sc.INTRO["T40"] + "\n\n| 完了の定義 |", out)


class TwoDaysOfTheSameVersion(unittest.TestCase):
    """同じ版 0.39.13 の 2 つの 24 時間 (2026-11-24 と 2026-11-26) を、どちらの向きで比べても届かずにならない (T40.4)。

    材料は `scripts/testdata/` の 4 つ (このファイルの頭)。実物を `snapshot-diff.py --criteria T39` に渡したときと同じ数字が、
    表示の桁で出る:

    - 前 = 2026-11-24、後 = 2026-11-26 … 0.799 倍。T39 でも T40 でも満たした。
    - 前 = 2026-11-26、後 = 2026-11-24 … 1.251 倍 (= 1 ÷ 0.799)。T39 では届かず、T40 では判定できず (帯の内)。
    - 記録にある 1.299 倍 (T40.1 の結果・T40.4 の目的の「向きを入れ替えると 1.299 倍」) … `snapshot-diff.py` に 4 つの引数を
      入れ替えて渡した出力の値。道具はスナップショットを古い順に並べ直し、`--profile` は新しい方に、`--profile-before` は
      古い方に付けるので、2026-11-24 のスナップショットに 2026-11-26 の `/profile` が付いた組になる。名前解決の失敗は、
      窓の合わない `/history` から 0 本と 804 本に数えられる (組を合わせると 944 本と 803 本)。これも帯の内で判定できず。
    """

    @classmethod
    def setUpClass(cls):
        cls.profile = {day: profile_of(day) for day in DAYS}
        cls.history = {day: history_of(day) for day in DAYS}

    def snap(self, history_day, profile_day=None):
        """その日の `/history` に、その日 (か、取り違えたときは別の日) の `/profile?res=60` を付けた最小のスナップショット。"""
        return {"history": self.history[history_day], "profile_res_60": self.profile[profile_day or history_day]}

    def pair(self, name):
        old, new = "2026-11-24", "2026-11-26"
        return {"古い日 → 新しい日": {"a": self.snap(old), "b": self.snap(new)},
                "新しい日 → 古い日": {"a": self.snap(new), "b": self.snap(old)},
                "`/profile` だけ入れ替わった組": {"a": self.snap(old, new), "b": self.snap(new, old)}}[name]

    def t40(self, name):
        return sc._t40_conn_priced(self.pair(name), sc.T40_TH)

    def t39(self, name):
        return sc._t39_conn_priced(self.pair(name), sc.T39_TH)

    def test_the_material_is_a_whole_day_of_the_same_version(self):
        for day, stamp in DAYS.items():
            with self.subTest(day=day):
                body = read(history_dns_path(day))
                self.assertEqual((body["snapshot"], body["version"]), (stamp, "0.39.13+11e9be52"))
                p, h60 = self.profile[day], self.history[day]["60"]
                self.assertEqual((p["interval_secs"], len(p["samples"])), (60, 1440))
                # `/history?res=60` の 1,440 標本は `/profile?res=60` の窓と同じ 24 時間 (名前解決の失敗はここから数える)
                self.assertEqual((h60["samples"][0][0], h60["samples"][-1][0]), sc.profile_window(p))

    def test_the_history_material_holds_only_counts_and_three_plain_strings(self):
        for day in DAYS:
            with self.subTest(day=day):
                path = history_dns_path(day)
                body = read(path)
                self.assertEqual(sorted(body), ["history", "snapshot", "version", "what"])
                self.assertEqual(sorted(body["history"]), ["3600", "60"])
                for res, want in (("60", 1440), ("3600", 72)):
                    h = body["history"][res]
                    self.assertEqual(sorted(h), ["dns", "interval_secs", "t0"])
                    self.assertEqual((h["interval_secs"], len(h["dns"])), (int(res), want))
                    self.assertIs(type(h["t0"]), int)
                    self.assertTrue(all(type(v) is int and v >= 0 for v in h["dns"]))
                # 文字列は 3 つだけ: どちらの日も同じ決まった説明と、決まった形の時刻と版
                self.assertEqual(body["what"], HISTORY_DNS_WHAT)
                self.assertRegex(body["snapshot"], r"\A\d{4}-\d\d-\d\dT\d{6}Z\Z")
                self.assertRegex(body["version"], r"\A\d+\.\d+\.\d+\+[0-9a-f]{8}\Z")
                self.assertLess(os.path.getsize(path), 8 << 10)

    def test_older_to_newer_is_met(self):
        row = self.t40("古い日 → 新しい日")
        self.assertEqual(row[3], sd.MET, row)
        self.assertTrue(row[2].startswith(
            "**0.799** 倍 (前の機械の値段に直した。前の値段で後の仕事 0.85・後の値段で前の仕事 0.75)。"
            "値段 (要求 1 本 + 中継 1 秒): 前 828 + 44.7 us (決定係数 -0.02)、後 456 + 80.5 us (0.45)。"
            "同じ版の中のぶれ (前と後の 1 枚をそれぞれ半分ずつ・1 時間おきに値付けし合った比) は最大 1.11 倍。"
            "表示だけ: 中継 2,463 → 3,966 秒/時 (24 → 24 時間)、"), row[2])
        self.assertIn("前: `--profile` 24.0 時間、要求 17,089 本のうち名前解決の失敗 803 本 (5%)。"
                      "後: `--profile` 24.0 時間、要求 17,932 本のうち名前解決の失敗 944 本 (5%)。", row[4])
        self.assertIn("は前 100.8 → 後 100.5 us/秒 (1.00 倍)。", row[4])
        # 満たした行は、実測も出どころも T39 の行のまま
        self.assertEqual(row[2:], self.t39("古い日 → 新しい日")[2:])

    def test_newer_to_older_is_inside_the_day_band(self):
        row, t39 = self.t40("新しい日 → 古い日"), self.t39("新しい日 → 古い日")
        self.assertEqual(t39[3], sd.MISSED)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertEqual(row[2], t39[2])
        self.assertTrue(row[2].startswith(
            "**1.251** 倍 (前の機械の値段に直した。前の値段で後の仕事 1.34・後の値段で前の仕事 1.17)。"
            "値段 (要求 1 本 + 中継 1 秒): 前 456 + 80.5 us (決定係数 0.45)、後 828 + 44.7 us (-0.02)。"), row[2])
        self.assertTrue(row[4].startswith(
            "値付けし合った比 1.251 倍は 1.1 倍以上だが、同じ版の日ごとのぶれの帯 (1.1〜1.3 倍。T40.4) の内。"
            "同じ版 0.39.13 の 2 つの 24 時間 (2026-11-24 と 2026-11-26) を比べると、同じバイナリなのに 0.80 倍、"
            "前と後を入れ替えると 1.25 倍だった。"
            "同じ版の中のぶれ (1 枚の中。最大 1.11 倍) は越えている (T39 の読み方では届かず)。"), row[4])
        self.assertIn("前: `--profile` 24.0 時間、要求 17,932 本のうち名前解決の失敗 944 本 (5%)。"
                      "後: `--profile` 24.0 時間、要求 17,089 本のうち名前解決の失敗 803 本 (5%)。", row[4])

    def test_the_two_directions_are_reciprocal_and_the_evidence_in_the_row_is_these_numbers(self):
        g, worst = sc.conn_priced_index(self.pair("古い日 → 新しい日"), sc.T40_TH)
        back, worst_back = sc.conn_priced_index(self.pair("新しい日 → 古い日"), sc.T40_TH)
        self.assertEqual((f"{g:.3f}", f"{back:.3f}"), ("0.799", "1.251"))
        self.assertAlmostEqual(g * back, 1.0, places=12)
        self.assertEqual(worst, worst_back)
        self.assertEqual(f"{math.exp(worst):.2f}", "1.11")
        # 行の理由と前置きに書く根拠は、この 2 つを 2 桁にしたもの
        self.assertIn(f"{g:.2f} 倍、前と後を入れ替えると {back:.2f} 倍だった。", sc.DAY_BAND_EVIDENCE)
        self.assertIn(f"{g:.2f} 倍、前と後を入れ替えると {back:.2f} 倍でした", sc.INTRO["T40"])
        # 帯の上の端 (1.3) までの余裕
        self.assertLess(back, sc.CONN_DAY_BAND_MAX)

    def test_the_recorded_1_299_is_the_pair_with_the_profiles_swapped_and_is_inside_the_band_too(self):
        row, t39 = self.t40("`/profile` だけ入れ替わった組"), self.t39("`/profile` だけ入れ替わった組")
        self.assertEqual(t39[3], sd.MISSED)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertTrue(row[2].startswith(
            "**1.299** 倍 (前の機械の値段に直した。前の値段で後の仕事 1.38・後の値段で前の仕事 1.22)。"
            "値段 (要求 1 本 + 中継 1 秒): 前 432 + 80.5 us (決定係数 0.45)、後 828 + 44.7 us (-0.02)。"), row[2])
        self.assertTrue(row[4].startswith("値付けし合った比 1.299 倍は 1.1 倍以上だが、同じ版の日ごとのぶれの帯 (1.1〜1.3 倍。T40.4) の内。"),
                        row[4])
        # 組が合っていない印: 2026-11-26 の `/profile` の窓を 2026-11-24 の `/history` は覆っていないので、失敗が 0 本になる
        self.assertIn("前: `--profile` 24.0 時間、要求 17,932 本のうち名前解決の失敗 0 本 (0%)。"
                      "後: `--profile` 24.0 時間、要求 17,089 本のうち名前解決の失敗 804 本 (5%)。", row[4])

    def test_the_tool_prints_these_numbers_and_swapping_its_four_arguments_swaps_only_the_profiles(self):
        """道具の入口を通しても同じ数字が出る: 2 日を古い順に渡すと 0.799 倍、4 つの引数を入れ替えて渡すと 1.299 倍 (記録の値)。

        スナップショットは名前解決の失敗の数だけの最小の形 (取得の時刻はファイルの名前から読まれる)。入れ替えて渡しても、道具は
        スナップショットを古い順に並べ直し (題の行が同じ)、`--profile` を新しい方に、`--profile-before` を古い方に付ける。
        """
        old, new = "2026-11-24", "2026-11-26"
        prof = {day: os.path.join(DATA, "deployed-%s-profile_res_60.anon.json" % day) for day in DAYS}
        snaps = {DAYS[day] + "-snapshot": {"schema": 1, "parts": ["history.60", "history.3600"], "dropped": [],
                                           "history": self.history[day]} for day in DAYS}

        def printed(row):
            return "| %s | %s | %s | **%s** | %s |" % tuple(row)

        with written(**snaps) as paths:
            a, b = paths[DAYS[old] + "-snapshot"], paths[DAYS[new] + "-snapshot"]
            straight = [a, b, "--profile-before", prof[old], "--profile", prof[new], "--no-dns"]
            swapped = [b, a, "--profile-before", prof[new], "--profile", prof[old], "--no-dns"]
            out = run(straight + ["--criteria", "T40"])
            out_swapped = run(swapped + ["--criteria", "T40"])
            out_swapped_t39 = run(swapped + ["--criteria", "T39"])
        self.assertIn(printed(self.t40("古い日 → 新しい日")), out.split("\n"))
        self.assertIn(printed(self.t40("`/profile` だけ入れ替わった組")), out_swapped.split("\n"))
        # 入れ替えて渡しても、題の行 (前 → 後) は古い順のまま
        self.assertEqual(out_swapped.split("\n")[0], out.split("\n")[0])
        self.assertIn("2026-11-24T120811Z-snapshot.json` → `", out.split("\n")[0])
        # 記録の「届かず」は、この入れ替えた命令を T39 で出した行
        self.assertIn(printed(self.t39("`/profile` だけ入れ替わった組")), out_swapped_t39.split("\n"))
        self.assertEqual(self.t39("`/profile` だけ入れ替わった組")[3], sd.MISSED)

    def test_no_pairing_of_the_two_days_is_missed(self):
        self.assertEqual({name: self.t40(name)[3] for name in ("古い日 → 新しい日", "新しい日 → 古い日", "`/profile` だけ入れ替わった組")},
                         {"古い日 → 新しい日": sd.MET, "新しい日 → 古い日": sd.UNKNOWN, "`/profile` だけ入れ替わった組": sd.UNKNOWN})
        # T39 の行は今までどおり (同じ材料で、入れ替えた向きは届かず)
        self.assertEqual([self.t39(name)[3] for name in ("古い日 → 新しい日", "新しい日 → 古い日", "`/profile` だけ入れ替わった組")],
                         [sd.MET, sd.MISSED, sd.MISSED])

    def test_the_idle_cost_and_the_ruler_are_stable_between_the_two_days(self):
        # 帯を入れるのは値付けし合う行だけ。暇なときの費用の行と物差しは、同じ 2 日で 1.00 倍
        c = self.pair("古い日 → 新しい日")
        idle = sc._t36_idle_cost(c, sc.T40_TH)
        self.assertEqual(idle[3], sd.MET, idle)
        self.assertTrue(idle[2].startswith("**548 → 550** us/秒 (前の機械の値段に直した。**1.00** 倍)。"), idle[2])
        ruler = sc._t36_machine_speed(c, sc.T40_TH)
        self.assertTrue(ruler[2].startswith("**100.8 → 100.5** us/秒 (**1.00** 倍)"), ruler[2])
        back = sc._t36_idle_cost(self.pair("新しい日 → 古い日"), sc.T40_TH)
        self.assertEqual(back[3], sd.MET, back)
        self.assertIn("**1.00** 倍", back[2])


REAL = {day: (os.path.join(DEPLOYED, "tokyo", stamp + "-snapshot.json"),
              os.path.join(DEPLOYED, "tokyo", stamp + "-profile_res_60.json")) for day, stamp in DAYS.items()}


@unittest.skipUnless(all(os.path.isfile(p) for pair in REAL.values() for p in pair), "tokyo の実データが無い")
class DeployedTwoDays(unittest.TestCase):
    """手元に実物のスナップショットと `/profile?res=60` があるときだけ回る: 最小の形 (上の組) と実物で、行が 1 文字も違わない。"""

    @classmethod
    def setUpClass(cls):
        cls.real, cls.small = {}, {}
        for day, (snap, profile) in REAL.items():
            cls.real[day] = dict(sd.load_source(snap, False), profile_res_60=read(profile))
            cls.small[day] = {"history": history_of(day), "profile_res_60": profile_of(day)}

    def test_the_anonymized_profile_is_the_real_one(self):
        for day in DAYS:
            self.assertEqual(self.small[day]["profile_res_60"], self.real[day]["profile_res_60"], day)

    def test_the_rows_from_the_small_material_are_the_rows_from_the_real_snapshots(self):
        old, new = "2026-11-24", "2026-11-26"
        for name, (a, b), (pa, pb) in (("古い日 → 新しい日", (old, new), (old, new)), ("新しい日 → 古い日", (new, old), (new, old)),
                                       ("`/profile` だけ入れ替わった組", (old, new), (new, old))):
            with self.subTest(name=name):
                real = {"a": dict(self.real[a], profile_res_60=self.real[pa]["profile_res_60"]),
                        "b": dict(self.real[b], profile_res_60=self.real[pb]["profile_res_60"])}
                small = {"a": dict(self.small[a], profile_res_60=self.small[pa]["profile_res_60"]),
                         "b": dict(self.small[b], profile_res_60=self.small[pb]["profile_res_60"])}
                for rule, th in ((sc._t40_conn_priced, sc.T40_TH), (sc._t39_conn_priced, sc.T39_TH)):
                    self.assertEqual(rule(small, th), rule(real, th))

    def test_the_tool_prints_the_same_row_for_the_real_pair(self):
        # 道具を通した形 (古い日 → 新しい日): 判定表 T40 の 5 行目は、最小の形から出した行と同じ
        old, new = "2026-11-24", "2026-11-26"
        d = build([REAL[old][0], REAL[new][0], "--criteria", "T40", "--no-dns",
                   "--profile-before", REAL[old][1], "--profile", REAL[new][1]])["criteria"]
        self.assertEqual(tuple(d["rows"][4]), sc._t40_conn_priced({"a": self.small[old], "b": self.small[new]}, sc.T40_TH))
        self.assertEqual(d["rows"][4][3], sd.MET)


if __name__ == "__main__":
    unittest.main()
