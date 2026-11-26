#!/usr/bin/env python3
"""判定表 T40 の試験。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

作り物は `scripts/_criteria_testing.py` にある (T38.6。`/profile?res=60` の作り物は T39 の試験と同じ `Criteria39Parts`)。

判定表 T40 が T39 から直したのは 2 行の 3 か所で、試験の組もそれに合わせてある:

    Criteria40ConnDayBand     5 行目 (`conn` 役の CPU を値付けし合う行) の、同じ版の日ごとのぶれの帯 (T40.4)
    Criteria40WindowCover     同じ行の、材料の窓 (`/profile?res=60` の窓と同じスナップショットの `/history`) の見方 (T40.5)
    Criteria40RefreshTotal    15 行目 (裏の引き直し) を合計で読むこと (T40.5)。実物の数字は、数だけを作り物に入れる
    TwoDaysOfTheSameVersion   実物の 2 日の組 (下)
    DeployedTwoDays・DeployedCloseout   手元に実物があるときだけ回る

**実物の 2 日の組** (`TwoDaysOfTheSameVersion`) は、同じ版 0.39.13 の 2 つの 24 時間 (2026-11-24 と 2026-11-26) を、
リポジトリに入れた材料だけで比べる (いつも回る)。1 日ぶんは `scripts/testdata/` の 2 つ:

    deployed-<日付>-profile_res_60.anon.json   `/profile?res=60` の匿名の写し (CPU の標本だけ。T40.1 と T40.4 で入れた)
    deployed-<日付>-history_dns.json           その日のスナップショットの `/history` から、行が読む数字だけを抜いたもの

行がスナップショットの側から読むのは、`/profile` の窓の名前解決の失敗の数 (`/history?res=60` と `res=3600` の
`errors_by_cause` の `dns`) だけ。スナップショットそのもの (1 枚 4.8 MB。接続元と宛先が入る) はリポジトリに入れないので、
その数と標本の時刻だけを抜いた: `t0` (最初の標本の時刻)・`interval_secs`・`dns` (数の並び)。res=60 は全部の 1,440 標本
(`/profile?res=60` の窓と同じ 24 時間)、res=3600 は最後の 72 標本 (行は、窓を res=60 が覆っていない所だけこちらを読む。
下の「`/profile` だけ入れ替わった組」の後の 804 本と、「停止の 9 分前に取った `/profile`」の頭の 9 分がそれ)。ほかに入って
いるのは、説明 (`what`)・元のスナップショットの時刻 (`snapshot`)・版 (`version`) の 3 つの文字列だけ。試験はこれから最小の
スナップショットの形 (`profile_res_60` と `history`) を組む。
手元に実物のスナップショット (リポジトリの `status/`。リポジトリには入れない) があれば、`DeployedTwoDays` が、
最小の形と実物で行が 1 文字も違わないことを見る。

**引き直しの行の実物の数字** (`SAME_VERSION`・`SHUTDOWN`・`NEXT_VERSION`) は、同じ版 0.39.13 の 6 枚と次の版の 1 枚から、
行が読む数 (起動からの秒・`dns.refreshes`・答えのある名前の数・表の名前の数・`misses_by_kind.expired`) だけを写したもの。
名前は 1 つも入れていない (`/dns` の一覧は、数だけ合わせた作り物の名前で組む)。手元に実物があれば、`DeployedCloseout` が、
この数を入れた作り物と実物で行が 1 文字も違わないことを見る。
"""

import json
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


def judged(argv):
    """道具の入口 (`main`) を通した判定表 (`--out json` の `criteria`)。`build` と違い、2 枚を取得の古い順に並べ直すところも通る。"""
    return json.loads(run(argv + ["--out", "json"]))["criteria"]


def taken_later(history, minutes):
    """最小の形の `/history` (`history_of`) を、`minutes` 分あとに撮ったスナップショットの形にする。

    `/history?res=60` は 1,440 標本の環なので、頭の `minutes` 標本が落ち、後ろに同じ数だけ続く (続きは元の `/profile?res=60` の
    窓の外なので、何を入れても行の数字は変わらない。0 を入れる)。落ちた頭の `minutes` 分は `/history?res=3600` にある。
    """
    h60 = history["60"]
    end = h60["samples"][-1][0]
    tail = [[end + 60 * (i + 1), [0]] for i in range(minutes)]
    return dict(history, **{"60": dict(h60, samples=h60["samples"][minutes:] + tail)})


class Criteria40ConnDayBand(Criteria39Parts, unittest.TestCase):
    """`--criteria T40` (T40.4): `conn` 役の CPU を値付けし合う行 (T39.9) に、同じ版の日ごとのぶれの帯を入れる。"""

    def row(self, c, **th):
        return sc._t40_conn_priced(c, dict(sc.T40_TH, **th))

    def t39(self, c):
        return sc._t39_conn_priced(c, sc.T39_TH)

    def dearer(self, k, wobble=None):
        """後の値段 (要求 1 本・中継 1 秒) がどちらも `k` 倍の組。値付けし合った比は `k` 倍になる。"""
        return self.c(self.profile(30, wobble=wobble), self.profile(30, us_req=1000 * k, us_relay=50 * k, wobble=wobble))

    def test_T40_replaces_the_conn_row_and_the_refresh_row_of_T39(self):
        t39, t40 = sd.RULES["T39"], sd.RULES["T40"]
        self.assertEqual(len(t39), len(t40))
        # 5 行目 (T40.4・T40.5) と 15 行目 (T40.5)
        self.assertEqual([i for i in range(len(t39)) if t39[i] is not t40[i]], [4, 14])
        self.assertIs(t40[4], sc._t40_conn_priced)
        self.assertIs(t40[14], sc._t40_refresh_total)
        self.assertIs(t39[14], sc._p30_refresh_per_name)
        # 閾は T39 のものに、帯の上の端と、窓を覆う割合の線を名前つきで足しただけ (帯の下の端は「満たした」の閾 1.1 と同じ。
        # 引き直しの閾は T33.2 から同じ鍵の 1.5 倍)
        self.assertEqual(sc.T40_TH, dict(sc.T39_TH, conn_day_band_max=1.3, conn_history_cover_min=0.99))
        self.assertEqual(sc.CONN_DAY_BAND_MAX, 1.3)
        self.assertEqual(sc.CONN_HISTORY_COVER_MIN, 0.99)
        self.assertEqual(sc.T40_TH["conn_per_resolved_factor"], 1.1)
        self.assertEqual(sc.T40_TH["refresh_per_name_factor"], 1.5)
        self.assertNotIn("conn_day_band_max", sc.T39_TH)
        self.assertNotIn("conn_history_cover_min", sc.T39_TH)
        self.assertEqual(sc.criteria_name("T40"), "T40")
        self.assertIn("T40", sc.INTRO)
        self.assertIn("T40", sd.CRITERIA)

    def test_the_intro_says_the_three_steps_with_the_same_numbers_as_the_thresholds(self):
        k, hi = sc.T40_TH["conn_per_resolved_factor"], sc.T40_TH["conn_day_band_max"]
        intro = sc.INTRO["T40"]
        self.assertTrue(intro.startswith("1〜27 行の並びと材料は T39 と同じです。直したのは 2 行の 3 か所です (T40.4・T40.5)。"
                                         "T40.4: 5 行目の `conn` 役の CPU を値付けし合う行に、**同じ版の日ごとのぶれの帯**を入れました。"), intro)
        self.assertIn(f"前の版の {k} 倍未満なら「満たした」、{k} 倍以上 {hi} 倍以下なら「判定できず」(同じ版の日ごとのぶれの内)、"
                      f"{hi} 倍を越えたら「届かず」です。", intro)
        self.assertIn("同じバイナリなのに 0.80 倍、前と後を入れ替えると 1.25 倍でした", intro)
        self.assertTrue(intro.endswith("ほかの 25 行は T39 と同じです。"), intro)

    def test_under_the_limit_is_met_and_only_the_label_and_the_limit_differ_from_T39(self):
        c = self.dearer(1.05)
        row, t39 = self.row(c), self.t39(c)
        self.assertEqual(row[3], sd.MET, row)
        self.assertEqual(t39[3], sd.MET)
        self.assertTrue(row[2].startswith("**1.050** 倍 (前の機械の値段に直した。"), row[2])
        # 実測・判定・出どころは T39 の行のまま。名前と閾の欄だけ、帯の読み方 (T40.4) と材料の窓の見方 (T40.5) を足してある
        self.assertEqual(row[2:], t39[2:])
        self.assertEqual(row[0], t39[0].replace(
            "(T39.9。", "(1.1 倍以上 1.3 倍以下は同じ版の日ごとのぶれの内で判定できず、1.3 倍を越えたら届かず。T40.4。"
            "前か後の `/profile?res=60` の窓を同じスナップショットの `/history` が覆っていなければ判定できず。T40.5。T39.9。"))
        self.assertIn("値付けし合って**、前の版の 1.1 倍未満 (1.1 倍以上 1.3 倍以下は", row[0])
        self.assertEqual(row[1], "< 前 ×1.1 (値付けし合った比。×1.1 以上 ×1.3 以下は日ごとのぶれの内で判定できず、×1.3 を越えたら届かず。"
                                 "同じ版の中のぶれ (1 枚の中) の内も、材料の窓が合っていないときも判定できず)")

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
                self.assertIn("届かず。T40.4。前か後の", row[0])
                self.assertIn("判定できず。T40.5。T39.9。", row[0])
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

    def test_the_other_25_rows_are_the_same_as_T39(self):
        # 架空の 2 枚 (`/profile?res=60` を渡していないので、5 行目は材料が無くて判定できず。15 行目は、前の `/dns` の一覧に
        # 答えのある名前が無いので T39 の行も合計で比べていて、判定は同じ届かず)
        d39 = build([A, B, "--criteria", "T39", "--no-dns"])["criteria"]
        d40 = build([A, B, "--criteria", "T40", "--no-dns"])["criteria"]
        self.assertEqual((d39["name"], d40["name"]), ("T39", "T40"))
        self.assertEqual(len(d40["rows"]), 27)
        self.assertEqual([i for i in range(27) if d39["rows"][i] != d40["rows"][i]], [4, 14])
        self.assertEqual(d40["rows"][4][2:], d39["rows"][4][2:])
        self.assertEqual((d39["rows"][14][3], d40["rows"][14][3]), (sd.MISSED, sd.MISSED))
        self.assertTrue(d39["rows"][14][2].startswith("合計 **2.0** 回/時 (前 0.2、11.11 倍)"), d39["rows"][14][2])
        self.assertTrue(d40["rows"][14][2].startswith("合計 **2.0** 回/時 (前 0.2、**11.11** 倍)"), d40["rows"][14][2])
        self.assertEqual(d40["tally"], d39["tally"])
        out = run([A, B, "--criteria", "T40", "--no-dns"])
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria T40`)\n\n" + sc.INTRO["T40"] + "\n\n| 完了の定義 |", out)


# 材料の窓が合っていないときの理由の文の、後ろの決まった部分 (どちらの窓がどれだけ覆われていないかの文のあとに続く)
MISMATCH_WHY = ("覆われていない所の名前解決の失敗 (`/history` の `errors_by_cause.dns`) は数えられず、0 本と数えると名前解決に"
                "成功した要求が多く出るので、判定しない (窓の 99% 以上が覆われていれば判定する)。")
MISMATCH_HOW = ("`/profile?res=60` は、そのスナップショットの `/history` が覆う時間のものを渡すこと (スナップショットより後に取った"
                "ものや、別の日のものは覆われない)。`snapshot-diff.py` は 2 枚を取得の古い順に並べ直し、`--profile-before` を"
                "古い方、`--profile` を新しい方に付ける (`collect-deployed.sh` の `PROFILE_BEFORE` は `--profile-before` になる)")


def mismatch(*sides, covered=None):
    """材料の窓が合っていないときの理由の文。`sides` は `(誰, 窓の始まり, 窓の終わり, 窓の長さ, 覆っている割合, 覆われていない長さ,
    そのスナップショットの `/history` が持っている時間の端)`、`covered` は合っている方 (`(誰, 割合)`。両方とも合っていなければ無い)。"""
    parts = "".join(f"{who}の `/profile?res=60` の窓 ({start}〜{end} の {span}) のうち、{who}のスナップショットの `/history` が"
                    f"覆っているのは {share} で、{gap}が覆われていない ({who}のスナップショットの {has})。"
                    for who, start, end, span, share, gap, has in sides)
    ok = f"{covered[0]}の窓は {covered[1]} 覆われている。" if covered else ""
    return "材料の窓が合っていない (T40.5): " + parts + MISMATCH_WHY + ok + MISMATCH_HOW


class Criteria40WindowCover(Criteria39Parts, unittest.TestCase):
    """`--criteria T40` (T40.5): `conn` 役の CPU を値付けし合う行は、材料の窓が合っていなければ判定できず。

    行は名前解決の失敗の数を、`/profile?res=60` の窓と同じ時間の `/history` から数える。別の日の `/profile?res=60` を渡すと
    窓を覆う `/history` がスナップショットに無く、T39 の行は覆われていない所を黙って 0 本と数える。作り物は名前解決の失敗が
    要求の 4 割なので、それだけで比が大きく動く (同じ値段の組が 1.546 倍の届かずにも、1.4 倍の組が 0.905 倍の満たしたにもなる)。
    """

    DAY = 86400
    # 作り物の `/profile?res=60` の窓 (`T0` から 24 時間)
    WINDOW = ("2027-01-15 08:00:00Z", "2027-01-16 08:00:00Z", "24.0 時間")
    # 1 日ずらした作り物の `/history` が持っている時間の端 (作り物のスナップショットは `res=3600` だけ)
    HAS = "`/history?res=3600` は %s 08:00:00Z〜%s 08:00:00Z"

    def row(self, c, **th):
        return sc._t40_conn_priced(c, dict(sc.T40_TH, **th))

    def t39(self, c):
        return sc._t39_conn_priced(c, sc.T39_TH)

    def same(self):
        """前も後も同じ値段の組 (値付けし合った比は 1.000 倍)。"""
        return self.c(self.profile(30), self.profile(30))

    @staticmethod
    def moved(c, who, secs):
        """`who` (`a` = 前、`b` = 後) のスナップショットの `/history` の標本を `secs` 秒ずらす (別の時に撮ったスナップショットの形)。"""
        for h in c[who]["history"].values():
            for row in h["samples"]:
                row[0] += secs
        return c

    def minutes(self, p, late=0):
        """`/profile?res=60` の作り物 `p` と同じ数の 1 分の標本の `/history?res=60`。`late` 分だけ後ろにずれている
        (`late` 分あとに撮ったスナップショットの形)。名前解決の失敗は、どの 1 分も `/history?res=3600` の作り物と同じ割合 (要求の 4 割)。"""
        ti, qi = p["keys"].index("t"), p["keys"].index("requests")
        per_minute = sum(r[qi] for r in p["samples"]) * 0.4 / len(p["samples"])
        t0 = p["samples"][0][ti] + 60 * late
        return {"interval_secs": 60, "causes": ["dns"], "keys": ["t", "errors_by_cause"],
                "samples": [[t0 + 60 * i, [per_minute]] for i in range(len(p["samples"]))]}

    def test_the_intro_says_the_line_with_the_same_number_as_the_threshold(self):
        intro = sc.INTRO["T40"]
        self.assertIn("T40.5: 同じ 5 行目は、**材料の窓が合っていなければ「判定できず」**です。", intro)
        self.assertEqual("%g%%" % (sc.T40_TH["conn_history_cover_min"] * 100), "99%")
        self.assertIn("前か後の窓のうち、同じスナップショットの `/history` が覆っているのが 99% に満たなければ、覆われていない所を "
                      "0 本と数えてしまうので比を出さず、どちらの窓がどれだけ覆われていないかを理由に書きます", intro)
        # 行の理由の文も同じ線を書く
        self.assertIn("(窓の 99% 以上が覆われていれば判定する)", MISMATCH_WHY)

    def test_matching_windows_are_judged_as_before(self):
        c = self.same()
        row, t39 = self.row(c), self.t39(c)
        self.assertEqual(row[3], sd.MET, row)
        self.assertEqual(row[2:], t39[2:])
        self.assertTrue(row[2].startswith("**1.000** 倍"), row[2])
        self.assertEqual(sc.profile_history_cover(c, sc.T40_TH),
                         {"前": {"t0": self.T0, "t1": self.T0 + self.DAY, "covered": self.DAY},
                          "後": {"t0": self.T0, "t1": self.T0 + self.DAY, "covered": self.DAY}})
        self.assertIsNone(sc.window_mismatch(c, sc.T40_TH))
        self.assertIn("要求 45,360 本のうち名前解決の失敗 18,144 本 (40%)", row[4])

    def test_a_before_profile_of_another_day_cannot_be_judged(self):
        # 前のスナップショットは 1 日前に撮ったもの (前の `/profile?res=60` の窓を、その `/history` が覆っていない)。
        # T39 の行は前の名前解決の失敗を 0 本と数え、同じ値段の組が 1.546 倍の届かずになる
        c = self.moved(self.same(), "a", -self.DAY)
        row, t39 = self.row(c), self.t39(c)
        self.assertEqual(t39[3], sd.MISSED)
        self.assertTrue(t39[2].startswith("**1.546** 倍"), t39[2])
        self.assertIn("前: `--profile` 24.0 時間、要求 45,360 本のうち名前解決の失敗 0 本 (0%)。", t39[4])
        self.assertEqual(row[3], sd.UNKNOWN, row)
        # 比は出さない (名前と閾の欄は、合っているときと同じ)
        self.assertEqual(row[2], "—")
        self.assertEqual(row[:2], self.row(self.same())[:2])
        self.assertEqual(row[4], mismatch(("前",) + self.WINDOW + ("0.0%", "24.0 時間", self.HAS % ("2027-01-14", "2027-01-15")),
                                          covered=("後", "100.0%")))
        self.assertEqual(row[4], sc.window_mismatch(c, sc.T40_TH))

    def test_an_after_profile_of_another_day_cannot_be_judged(self):
        # 後だけ合っていない。T39 の行は後の失敗を 0 本と数え、0.647 倍の満たしたになる
        c = self.moved(self.same(), "b", -self.DAY)
        row, t39 = self.row(c), self.t39(c)
        self.assertEqual(t39[3], sd.MET)
        self.assertTrue(t39[2].startswith("**0.647** 倍"), t39[2])
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertEqual(row[2], "—")
        self.assertEqual(row[4], mismatch(("後",) + self.WINDOW + ("0.0%", "24.0 時間", self.HAS % ("2027-01-14", "2027-01-15")),
                                          covered=("前", "100.0%")))

    def test_a_real_rise_is_not_hidden_by_a_window_that_does_not_match(self):
        # 後の値段が 1.4 倍の組 (合っていれば届かず)。後の窓が合っていないと、T39 の行は 0.905 倍の満たしたと出す
        dear = lambda: self.c(self.profile(30), self.profile(30, us_req=1400, us_relay=70))  # noqa: E731
        self.assertEqual(self.row(dear())[3], sd.MISSED)
        c = self.moved(dear(), "b", self.DAY)
        t39 = self.t39(c)
        self.assertEqual(t39[3], sd.MET)
        self.assertTrue(t39[2].startswith("**0.905** 倍"), t39[2])
        self.assertEqual(self.row(c)[3], sd.UNKNOWN)
        self.assertTrue(self.row(c)[4].startswith("材料の窓が合っていない (T40.5): 後の `/profile?res=60` の窓 "), self.row(c)[4])

    def test_both_sides_are_named_when_neither_matches(self):
        c = self.moved(self.moved(self.same(), "a", -self.DAY), "b", self.DAY)
        # 両方とも 0 本と数えると、T39 の行は 1.000 倍の満たした (たまたま打ち消し合う)
        self.assertEqual(self.t39(c)[3], sd.MET)
        row = self.row(c)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertEqual(row[4], mismatch(("前",) + self.WINDOW + ("0.0%", "24.0 時間", self.HAS % ("2027-01-14", "2027-01-15")),
                                          ("後",) + self.WINDOW + ("0.0%", "24.0 時間", self.HAS % ("2027-01-16", "2027-01-17"))))

    def test_the_line_is_99_percent_of_the_window(self):
        # 後のスナップショットが 1 時間前のもの: 窓の 24 時間のうち最後の 1 時間が覆われていない (95.8%)
        c = self.moved(self.same(), "b", -3600)
        row = self.row(c)
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertEqual(row[4], mismatch(("後",) + self.WINDOW + (
            "95.8%", "1.0 時間", "`/history?res=3600` は 2027-01-15 07:00:00Z〜2027-01-16 07:00:00Z"), covered=("前", "100.0%")))
        self.assertEqual(sc.profile_history_cover(c, sc.T40_TH)["後"]["covered"], 23 * 3600)
        # T39 の行はその 1 時間の失敗を 0 本と数えて 0.977 倍と出す (名前解決の失敗が 4 割だと、4% の欠けで比が 2% 動く)
        self.assertTrue(self.t39(c)[2].startswith("**0.977** 倍"), self.t39(c)[2])
        self.assertIn("後: `--profile` 24.0 時間、要求 45,360 本のうち名前解決の失敗 17,388 本 (38%)。", self.t39(c)[4])
        # 線ちょうどは覆っているうち (「99% に満たなければ」)。線を覆っている割合そのものに置いて見る
        on_the_line = self.row(c, conn_history_cover_min=23 / 24)
        self.assertEqual(on_the_line[3], sd.MET)
        self.assertTrue(on_the_line[2].startswith("**0.977** 倍"), on_the_line[2])
        self.assertEqual(self.row(c, conn_history_cover_min=23 / 24 + 1e-9)[3], sd.UNKNOWN)

    def test_a_few_minutes_at_the_end_are_inside_the_line(self):
        # 後の `/profile?res=60` をスナップショットの数分あとに取った形 (`/history?res=60` は窓の最後の数分を持っていない)。
        # 24 時間の窓の 1% は 14.4 分: 14 分の欠けは判定し、15 分の欠けは判定できず。名前解決の失敗が 4 割のこの作り物で、
        # 線の内に残る比のずれは 0.6% まで (14 分の欠けで 1.000 倍が 0.994 倍)
        for late, want, shown in ((1, sd.MET, "**1.000** 倍"), (14, sd.MET, "**0.994** 倍"), (15, sd.UNKNOWN, "—")):
            with self.subTest(late=late):
                c = self.same()
                c["b"]["history"]["60"] = self.minutes(c["b"]["profile_res_60"], late=-late)
                cover = sc.profile_history_cover(c, sc.T40_TH)["後"]
                self.assertEqual(cover["covered"], self.DAY - 60 * late)
                row = self.row(c)
                self.assertEqual(row[3], want, row)
                self.assertTrue(row[2].startswith(shown), row[2])
                if want == sd.UNKNOWN:
                    # 覆っている割合は切り捨てて書く (98.96% を 99.0% と書かない)
                    self.assertEqual(row[4], mismatch(("後",) + self.WINDOW + (
                        "98.9%", "15 分", "`/history?res=60` は 2027-01-15 07:45:00Z〜2027-01-16 07:45:00Z"), covered=("前", "100.0%")))

    def test_a_profile_taken_minutes_before_the_snapshot_is_covered_by_the_hourly_history(self):
        # 締めの前の形: `/profile?res=60` は停止の 9 分前に取った 24 時間ぶん、スナップショットは停止のときの 1 枚。
        # `/history?res=60` は窓の頭の 9 分を持っていないが、そこは `/history?res=3600` を窓に重なる割合で按分して数える
        c = self.same()
        c["a"]["history"]["60"] = self.minutes(c["a"]["profile_res_60"], late=9)
        row, t39 = self.row(c), self.t39(c)
        self.assertEqual(sc.profile_history_cover(c, sc.T40_TH)["前"]["covered"], self.DAY)
        self.assertEqual(row[3], sd.MET, row)
        self.assertEqual(row[2:], t39[2:])
        # 12 時間の `/profile` (行が判定する最短) でも同じ
        c = self.c(self.profile(30, hours=12), self.profile(30, hours=12))
        c["a"]["history"]["60"] = self.minutes(c["a"]["profile_res_60"], late=9)
        self.assertEqual(sc.profile_history_cover(c, sc.T40_TH)["前"]["covered"], 12 * 3600)
        self.assertEqual(self.row(c)[3], sd.MET)

    def test_a_profile_taken_long_before_the_snapshot_is_covered_by_the_hourly_history(self):
        # 前の版の `/profile?res=60` を停止の何時間も前に取っていた形 (`--closeout` が手元の前の版の最後の 12 時間以上のものを
        # 選んだとき)。`/history?res=60` は窓にまったく重ならないが、`/history?res=3600` が窓を覆っている
        c = self.same()
        c["a"]["history"]["60"] = self.minutes(c["a"]["profile_res_60"], late=30 * 60)
        self.assertEqual(sc.profile_history_cover(c, sc.T40_TH)["前"]["covered"], self.DAY)
        row = self.row(c)
        self.assertEqual(row[3], sd.MET, row)
        self.assertEqual(row[2:], self.row(self.same())[2:])

    def test_the_material_reasons_of_the_T39_row_come_first(self):
        # `/profile` が 12 時間に満たない・`/history` に `errors_by_cause` が無い、は T39 の行の理由のまま (窓は見ない)
        short = self.moved(self.c(self.profile(30, hours=6), self.profile(30)), "a", -self.DAY)
        self.assertIsNone(sc.profile_history_cover(short, sc.T40_TH))
        self.assertTrue(self.row(short)[4].startswith("前の `/profile` が 6.0 時間 (360 標本) で 12 時間に満たない"), self.row(short)[4])
        bare = self.same()
        bare["b"]["history"]["3600"]["keys"] = ["t", "errors"]
        self.assertIsNone(sc.profile_history_cover(bare, sc.T40_TH))
        self.assertEqual(self.row(bare)[4], self.t39(bare)[4])
        self.assertIn("名前解決の失敗が数えられない", self.row(bare)[4])
        # 名前解決の失敗を数えられた先の理由 (値段が 0 以下) より、窓が合っていないことを先に言う
        priced = self.moved(self.c(self.profile(30, us_relay=-400), self.profile(30)), "a", -self.DAY)
        self.assertTrue(self.t39(priced)[4].startswith("値段が 0 以下になった"), self.t39(priced)[4])
        self.assertTrue(self.row(priced)[4].startswith("材料の窓が合っていない (T40.5): 前の"), self.row(priced)[4])

    def test_the_cover_is_what_the_failure_count_reads(self):
        # 覆っている秒は、名前解決の失敗を数える関数 (`dns_failures_between`) が読む標本の長さの和。1 秒に 1 本ずつ失敗した
        # `/history` なら、数えた失敗の数と覆っている秒は同じ数になる
        p = self.profile(30)
        t0, t1 = self.T0, self.T0 + self.DAY
        h60, h3600 = self.minutes(p, late=9), self.snap(p)["history"]["3600"]
        for row in h60["samples"]:
            row[1] = [60]
        for row in h3600["samples"]:
            row[1][0] = 3600
        cases = {"res=60 と res=3600": ({"60": h60, "3600": h3600}, self.DAY),
                 "res=3600 だけ": ({"3600": h3600}, self.DAY),
                 "res=60 の真ん中の 30 分が無い (res=3600 では埋めない)":
                     ({"60": dict(h60, samples=h60["samples"][:600] + h60["samples"][630:]), "3600": h3600}, self.DAY - 1800),
                 "res=3600 の最後の 2 時間が無い": ({"3600": dict(h3600, samples=h3600["samples"][:-2])}, self.DAY - 7200)}
        for name, (history, want) in cases.items():
            with self.subTest(name=name):
                snap = {"history": history}
                self.assertEqual(sc.history_cover_secs(snap, t0, t1), want)
                self.assertEqual(sc.dns_failures_between(snap, t0, t1), want)
        # 数えられない (`errors_by_cause` が無い) ときは、覆っている秒も無い
        self.assertIsNone(sc.history_cover_secs({"history": {"3600": dict(h3600, keys=["t", "errors"])}}, t0, t1))
        self.assertIsNone(sc.history_cover_secs({}, t0, t1))
        # 元のスナップショットは書き換えない
        before = self.snap(p)
        snap = self.snap(p)
        sc.history_cover_secs(snap, t0, t1)
        self.assertEqual(snap, before)


# 同じ版 0.39.13 (tokyo。2026-11-22 12:01:57Z に起動) の 5 枚の、引き直しの行が読む数:
# 起動からの時間 (表示の桁) → (起動からの秒, `dns.refreshes`, 答えのある名前, 表の名前の数, `misses_by_kind.expired`)
SAME_VERSION = {
    "36.0": (129_484, 12_070, 876, 1_263, 173),
    "48.1": (173_174, 16_454, 1_107, 1_666, 318),
    "60.0": (215_887, 20_873, 1_390, 2_095, 474),
    "84.0": (302_284, 29_977, 1_720, 2_682, 835),
    "93.9": (338_067, 33_724, 1_880, 3_006, 989),
}
# 同じ版の停止の 1 枚 (起動から 94.1 時間。0.40.0 の締めの「前」) と、次の版 0.40.0 の起動から 2.7 時間の 1 枚
SHUTDOWN = (338_597, 33_789, 1_881, 3_009, 990)
NEXT_VERSION = (9_844, 1_130, 195, 282, 0)
# 停止の 1 枚を撮った時刻 (2026-11-26 10:05:14Z)
SHUTDOWN_AT = 1_795_687_514


def dns_snap(uptime_secs, refreshes, answered, entries, expired=0, listed=None, **top):
    """`/status` の `dns` と `/dns` の一覧だけのスナップショット。表の `entries` 行のうち `answered` 行だけに答えがある。

    `listed` を渡すと、`/dns` の一覧をその行数で切る (`truncated`)。名前は数を合わせるだけの作り物。
    """
    rows = ([{"host": "ok%d.example" % i, "addrs": ["192.0.2.1"], "addr_count": 1, "failed": None} for i in range(answered)]
            + [{"host": "dead%d.example" % i, "addrs": [], "addr_count": 0, "failed": {"secs_ago": 1}}
               for i in range(entries - answered)])
    shown = rows if listed is None else rows[:listed]
    return dict({"uptime_secs": uptime_secs,
                 "status": {"dns": {"refreshes": refreshes, "entries": entries,
                                    "misses_by_kind": {"expired": expired, "warm_stale": 0}}},
                 "dns": {"count": entries, "shown": len(shown), "truncated": listed is not None, "entries": shown}}, **top)


class Criteria40RefreshTotal(unittest.TestCase):
    """`--criteria T40` (T40.5): 裏の引き直しを、答えのある名前 1 つあたりではなく合計 (回/時) で前の期間と比べる。

    T33.2 の行は答えのある名前 1 つあたりで比べ、`/dns` の一覧が切れているときだけ合計で比べる。名前解決の表の名前は起動から
    増え続けるので、名前 1 つあたりは起動からの時間で値が変わり、起動からの時間が違う 2 枚では動きが同じでも 1.5 倍を越える。
    """

    LABEL = ("裏の引き直しの**合計** (回/時) が前の期間の 1.5 倍未満 (T40.5。答えのある名前 1 つあたりは起動からの時間で値が"
             "変わるので表示だけ。表の名前の数あたりと `expired` + `warm_stale` も表示だけ)")
    LIMIT = "< 前 ×1.5 (引き直しの合計)"
    EVIDENCE = ("同じ版 0.39.13 で、起動から 36.0・48.1・60.0・84.0・93.9 時間の名前 1 つあたりは 0.383・0.309・0.250・0.208・0.191 回/時 "
                "(合計は 336〜359 回/時で一定、答えのある名前は 876 → 1,880 と増える)")
    WHY = "合計で読む理由 (T40.5): 名前解決の表の名前は起動から増え続けるので、名前 1 つあたりは起動からの時間で値が変わる。" + EVIDENCE
    NAMES = "表示だけの答えのある名前の数は、`/dns` の一覧 (切れていない) の `addrs` のある行の数。"

    @staticmethod
    def c(a, b, info=None):
        # 既定は再起動をまたいだ 2 枚 (締めの形。どちらも起動からの通算で読む)
        return {"a": a, "b": b, "info": info or {"restarted": True}}

    def row(self, a, b, info=None):
        return sc._t40_refresh_total(self.c(a, b, info), sc.T40_TH)

    def t39(self, a, b, info=None):
        return sc._p30_refresh_per_name(self.c(a, b, info), sc.T39_TH)

    @staticmethod
    def hours(h, per_hour, answered, entries=None, **kw):
        """起動から `h` 時間で、引き直しが 1 時間に `per_hour` 回のスナップショット。"""
        return dns_snap(h * 3600, round(per_hour * h), answered, entries or answered, **kw)

    def test_the_same_total_is_met_though_each_name_is_1_6_times(self):
        # 動きは同じ (合計 350 回/時) で、起動からの時間だけが違う 2 枚: 前は 94 時間で答えのある名前 1,880、後は 48 時間で 1,175
        a, b = self.hours(94, 350, 1_880), self.hours(48, 350, 1_175)
        t39, row = self.t39(a, b), self.row(a, b)
        self.assertEqual(t39[3], sd.MISSED)
        self.assertTrue(t39[2].startswith("**0.30** 回/時/答えのある名前 (前 0.19、1.60 倍)。表示だけ: 合計 350.0 回/時 (前 350.0)"), t39[2])
        self.assertEqual(row[3], sd.MET, row)
        self.assertEqual(row[:2], (self.LABEL, self.LIMIT))
        self.assertEqual(row[2], "合計 **350.0** 回/時 (前 350.0、**1.00** 倍)。表示だけ: 答えのある名前 1 つあたり 0.298 回/時 "
                                 "(前 0.186、1.60 倍。答えのある名前 1,175、前 1,880)、表の名前あたり 0.30 回/時/名前 "
                                 "(前 0.19、名前 1,175、前 1,880)、`expired` + `warm_stale` 0.0 回/日 (前 0.0)")
        self.assertEqual(row[4], "`/status` の `dns.refreshes` を 1 時間あたりにした (前は前のスナップショットの起動からの 94.0 時間、"
                                 "後は後のスナップショットの起動からの 48.0 時間)。" + self.NAMES + self.WHY)

    def test_a_total_over_the_limit_is_missed_though_each_name_is_refreshed_less(self):
        # 合計が 1.6 倍。答えのある名前が 2 倍に増えているので、名前 1 つあたりは 0.8 倍 (T33.2 の読み方では満たした)
        a, b = self.hours(48, 350, 1_000), self.hours(48, 560, 2_000)
        t39, row = self.t39(a, b), self.row(a, b)
        self.assertEqual(t39[3], sd.MET)
        self.assertIn("(前 0.35、0.80 倍)", t39[2])
        self.assertEqual(row[3], sd.MISSED, row)
        self.assertTrue(row[2].startswith("合計 **560.0** 回/時 (前 350.0、**1.60** 倍)。表示だけ: 答えのある名前 1 つあたり 0.280 回/時 "
                                          "(前 0.350、0.80 倍。答えのある名前 2,000、前 1,000)"), row[2])

    def test_the_limit_is_under_1_5_times_the_total(self):
        a = self.hours(48, 300, 1_000)
        for per_hour, want in ((449, sd.MET), (450, sd.MISSED), (451, sd.MISSED), (150, sd.MET)):
            with self.subTest(per_hour=per_hour):
                self.assertEqual(self.row(a, self.hours(48, per_hour, 1_000))[3], want)
        # 前が 0 回なら、後も 0 回のときだけ満たした (倍率は出ない。T33.2 の合計の比べ方と同じ)
        none = self.hours(48, 0, 1_000)
        self.assertEqual(self.row(none, self.hours(48, 0, 1_000))[3], sd.MET)
        self.assertEqual(self.row(none, self.hours(48, 1, 1_000))[3], sd.MISSED)
        self.assertTrue(self.row(none, self.hours(48, 1, 1_000))[2].startswith("合計 **1.0** 回/時 (前 0.0)。表示だけ: "))

    def test_the_verdict_is_the_one_the_T33_2_row_gives_when_the_list_is_cut(self):
        # 合計の出し方と比べ方は、T33.2 の行が `/dns` の一覧の切れているときにしていたもの。一覧を切った 2 枚を T33.2 の行に
        # 渡した判定と合計の数字が、切っていない 2 枚を T40 の行に渡したものと同じ
        a = self.hours(94, 350, 1_880, 3_000)
        for name, b in (("同じ", self.hours(48, 350, 1_175, 1_700)), ("1.6 倍", self.hours(48, 560, 2_000, 2_500)),
                        ("ちょうど 1.5 倍", self.hours(48, 525, 1_000)), ("0.5 倍", self.hours(36, 175, 500))):
            with self.subTest(name=name):
                row = self.row(a, b)
                cut_a, cut_b = (dict(s, dns=dict(s["dns"], truncated=True, entries=s["dns"]["entries"][:300])) for s in (a, b))
                old = self.t39(cut_a, cut_b)
                self.assertIn("引き直しの合計で比べた。", old[4])
                self.assertEqual(row[3], old[3])
                self.assertEqual(row[2].split("。")[0].replace("**", ""), old[2].split("。")[0].replace("**", ""))

    def test_a_cut_list_only_drops_the_names_from_the_display(self):
        # `/dns` の一覧が切れていても判定は同じ (合計は `/status` から読む)。答えのある名前 1 つあたりの表示だけが出ない
        a, b = self.hours(94, 350, 1_880, 3_000), self.hours(48, 350, 1_175, 1_700)
        whole = self.row(a, b)
        cut = self.row(dict(a, dns=dict(a["dns"], truncated=True, entries=a["dns"]["entries"][:300])), b)
        self.assertEqual(cut[:2] + cut[3:4], whole[:2] + whole[3:4])
        self.assertEqual(cut[2], "合計 **350.0** 回/時 (前 350.0、**1.00** 倍)。表示だけ: 答えのある名前 後 1,175、"
                                 "表の名前あたり 0.21 回/時/名前 (前 0.12、名前 1,700、前 3,000)、`expired` + `warm_stale` 0.0 回/日 (前 0.0)")
        self.assertEqual(cut[4], "`/status` の `dns.refreshes` を 1 時間あたりにした (前は前のスナップショットの起動からの 94.0 時間、"
                                 "後は後のスナップショットの起動からの 48.0 時間)。前の `/dns` の一覧が切れている (300 / 3,000 行)ので、"
                                 "表示だけの答えのある名前 1 つあたりは出ない。" + self.WHY)
        # `/dns` の部が無いスナップショットでも判定する。`dns.refreshes` が無ければ判定できず
        nodns = {k: v for k, v in b.items() if k != "dns"}
        row = self.row(a, nodns)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("後のスナップショットに `/dns` の部が無いので、表示だけの答えのある名前 1 つあたりは出ない。", row[4])
        self.assertIn("表示だけ: 答えのある名前 前 1,880、", row[2])
        unknown = self.row(a, {"uptime_secs": 3600})
        self.assertEqual(unknown, (self.LABEL, self.LIMIT, "—", sd.UNKNOWN, "前か後の `/status` に `dns.refreshes` か `uptime_secs` が無い"))

    def test_between_two_snapshots_of_the_same_run_the_after_side_is_the_time_between(self):
        # 再起動をまたいでいない 2 枚: 後はその間の差を、その間の時間で割る (T33.2 の行と同じ `counter_rates`)
        a, b = self.hours(48, 350, 1_175), self.hours(94, 350, 1_880)
        info = {"restarted": False, "wall_secs": 46 * 3600}
        row = self.row(a, b, info)
        self.assertEqual(row[3], sd.MET)
        self.assertTrue(row[2].startswith("合計 **350.0** 回/時 (前 350.0、**1.00** 倍)。"), row[2])
        self.assertTrue(row[4].startswith("`/status` の `dns.refreshes` を 1 時間あたりにした (前は前のスナップショットの起動からの 48.0 時間、"
                                          "後はその間の 46.0 時間)。"), row[4])
        self.assertEqual(sc.refresh_hours(self.c(a, b, info)), (48.0, 46.0, True))
        self.assertEqual(sc.refresh_hours(self.c(a, b)), (48.0, 94.0, False))
        # 行に書く時間は、合計を割った時間そのもの
        for ctx in (self.c(a, b, info), self.c(a, b), self.c(b, a)):
            before, after, windowed = sc.refresh_hours(ctx)
            va, vb, _ = sc.dns_counter(ctx, "refreshes")
            self.assertEqual(sc.counter_rates(ctx, "refreshes"), (va / before, vb / after))
            self.assertEqual(windowed, not ctx["info"]["restarted"])

    # --- 実物の数字 (`SAME_VERSION`・`SHUTDOWN`・`NEXT_VERSION`。数だけを作り物に入れる) -------------------------

    def test_the_evidence_in_the_row_and_the_intro_is_the_five_snapshots_of_the_same_version(self):
        per_name, totals = [], []
        for shown, (up, refreshes, answered, entries, _) in SAME_VERSION.items():
            snap = dns_snap(up, refreshes, answered, entries)
            self.assertEqual("%.1f" % (up / 3600), shown)
            self.assertEqual(sc.answered_names(snap), answered)
            total = refreshes / (up / 3600)
            totals.append(total)
            per_name.append(total / answered)
        self.assertEqual(sc.REFRESH_EVIDENCE, tuple((float(h), round(v, 3)) for h, v in zip(SAME_VERSION, per_name)))
        self.assertEqual(sc.REFRESH_EVIDENCE_TOTAL, (round(min(totals)), round(max(totals))))
        self.assertEqual(["%.1f" % v for v in totals], ["335.6", "342.1", "348.1", "357.0", "359.1"])
        self.assertEqual(sc.REFRESH_EVIDENCE_NAMES, (SAME_VERSION["36.0"][2], SAME_VERSION["93.9"][2]))
        self.assertEqual(sc.refresh_evidence_text(), self.EVIDENCE)
        intro = sc.INTRO["T40"]
        self.assertIn("T40.5: 15 行目の裏の引き直しは、答えのある名前 1 つあたりではなく、**引き直しの合計 (回/時)** が前の期間の "
                      "1.5 倍未満かで読みます", intro)
        self.assertIn("名前 1 つあたりは起動からの時間で値が変わります。" + self.EVIDENCE + "。起動からの時間が違う 2 枚を名前あたりで"
                      "比べると、動きが同じでも 1.5 倍を越えます (前 94 時間・後 48 時間なら 1.6 倍)。", intro)

    def test_the_closeout_of_0_40_0_would_be_missed_per_name_and_is_met_by_the_total(self):
        # 0.40.0 の締めの形: 前 = 前の版の停止の 1 枚 (起動から 94.1 時間)、後 = 起動から 48 時間。同じ版 0.39.13 の 48.1 時間の
        # 1 枚を後に置くと、何も変わっていないのに T33.2 の読み方 (T39 までの判定表) は 1.62 倍で届かず
        a, b = dns_snap(*SHUTDOWN), dns_snap(*SAME_VERSION["48.1"])
        t39, row = self.t39(a, b), self.row(a, b)
        self.assertEqual(t39[3], sd.MISSED)
        self.assertEqual(t39[2], "**0.31** 回/時/答えのある名前 (前 0.19、1.62 倍)。表示だけ: 合計 342.1 回/時 (前 359.2)、"
                                 "答えのある名前 1,107 (前 1,881)、表の名前あたり 0.21 回/時/名前 (前 0.12、名前 1,666、前 3,009)、"
                                 "`expired` + `warm_stale` 158.7 回/日 (前 252.6)")
        self.assertEqual(row[3], sd.MET, row)
        self.assertEqual(row[2], "合計 **342.1** 回/時 (前 359.2、**0.95** 倍)。表示だけ: 答えのある名前 1 つあたり 0.309 回/時 "
                                 "(前 0.191、1.62 倍。答えのある名前 1,107、前 1,881)、表の名前あたり 0.21 回/時/名前 "
                                 "(前 0.12、名前 1,666、前 3,009)、`expired` + `warm_stale` 158.7 回/日 (前 252.6)")
        self.assertEqual(row[4], "`/status` の `dns.refreshes` を 1 時間あたりにした (前は前のスナップショットの起動からの 94.1 時間、"
                                 "後は後のスナップショットの起動からの 48.1 時間)。" + self.NAMES + self.WHY)

    def test_the_same_two_snapshots_in_the_order_they_were_taken(self):
        # 同じ 2 枚を取得の古い順に (前 = 48.1 時間、後 = 停止の 1 枚。再起動をまたがないので、後はその間の 46.0 時間)
        a, b = dns_snap(*SAME_VERSION["48.1"]), dns_snap(*SHUTDOWN)
        info = {"restarted": False, "wall_secs": SHUTDOWN[0] - SAME_VERSION["48.1"][0]}
        t39, row = self.t39(a, b, info), self.row(a, b, info)
        self.assertEqual((t39[3], row[3]), (sd.MET, sd.MET))
        self.assertTrue(t39[2].startswith("**0.20** 回/時/答えのある名前 (前 0.31、0.65 倍)。表示だけ: 合計 377.3 回/時 (前 342.1)"), t39[2])
        self.assertTrue(row[2].startswith("合計 **377.3** 回/時 (前 342.1、**1.10** 倍)。表示だけ: 答えのある名前 1 つあたり 0.201 回/時 "
                                          "(前 0.309、0.65 倍。答えのある名前 1,881、前 1,107)"), row[2])
        self.assertIn("(前は前のスナップショットの起動からの 48.1 時間、後はその間の 46.0 時間)", row[4])

    def test_the_first_hours_of_the_next_version_are_met_by_the_total(self):
        # 手元の実物の組: 前 = 0.39.13 の停止の 1 枚、後 = 0.40.0 の起動から 2.7 時間 (表の名前がまだ 282)。
        # 名前 1 つあたりは 11.10 倍、合計は 1.15 倍
        a, b = dns_snap(*SHUTDOWN), dns_snap(*NEXT_VERSION)
        t39, row = self.t39(a, b), self.row(a, b)
        self.assertEqual(t39[3], sd.MISSED)
        self.assertTrue(t39[2].startswith("**2.12** 回/時/答えのある名前 (前 0.19、11.10 倍)。表示だけ: 合計 413.2 回/時 (前 359.2)"), t39[2])
        self.assertEqual(row[3], sd.MET, row)
        self.assertEqual(row[2], "合計 **413.2** 回/時 (前 359.2、**1.15** 倍)。表示だけ: 答えのある名前 1 つあたり 2.119 回/時 "
                                 "(前 0.191、11.10 倍。答えのある名前 195、前 1,881)、表の名前あたり 1.47 回/時/名前 "
                                 "(前 0.12、名前 282、前 3,009)、`expired` + `warm_stale` 0.0 回/日 (前 252.6)")
        self.assertIn("後は後のスナップショットの起動からの 2.7 時間)", row[4])

    def test_no_pair_of_the_same_version_is_missed_by_the_total(self):
        # 同じ版の 6 枚 (起動から 36.0〜94.1 時間) のどの 2 枚を前と後に置いても (それぞれ別の起動の 1 枚として読む)、
        # 合計は 0.93〜1.07 倍で満たした。名前 1 つあたりだと、前が後より長く動いていた 15 組のうち 6 組が 1.5 倍以上で届かず
        snaps = {shown: dns_snap(*v) for shown, v in SAME_VERSION.items()}
        snaps["94.1"] = dns_snap(*SHUTDOWN)
        total, per_name = {}, {}
        for x, a in snaps.items():
            for y, b in snaps.items():
                if x != y:
                    total[x, y] = self.row(a, b)
                    per_name[x, y] = self.t39(a, b)
        self.assertEqual(len(total), 30)
        self.assertEqual({row[3] for row in total.values()}, {sd.MET})
        ratios = [float(row[2].split("、**")[1].split("**")[0]) for row in total.values()]
        self.assertEqual((min(ratios), max(ratios)), (0.93, 1.07))
        missed = sorted(pair for pair, row in per_name.items() if row[3] == sd.MISSED)
        self.assertEqual(missed, [("60.0", "36.0"), ("84.0", "36.0"), ("93.9", "36.0"), ("93.9", "48.1"),
                                  ("94.1", "36.0"), ("94.1", "48.1")])
        # 前の方が短い 15 組は、名前 1 つあたりが下がる向きなので、どちらの読み方でも満たした
        self.assertEqual({row[3] for (x, y), row in per_name.items() if float(x) < float(y)}, {sd.MET})

    def test_the_tool_prints_the_row_for_the_closeout_shape(self):
        # 道具の入口を通した締めの形: 前 = 前の版の停止の 1 枚、後 = 次の版の起動から 48.1 時間の 1 枚 (数は同じ版の 48.1 時間のもの)
        def whole(v, taken_at, version):
            return dns_snap(*v, schema=1, parts=["status", "dns"], dropped=[], taken_at=taken_at, version=version)
        before = whole(SHUTDOWN, SHUTDOWN_AT, "0.39.13+11e9be52")
        after = whole(SAME_VERSION["48.1"], SHUTDOWN_AT + 90 + SAME_VERSION["48.1"][0], "0.40.0+71301a2d")
        with written(before=before, after=after) as paths:
            args = [paths["after"], paths["before"], "--no-dns"]      # 渡す順は問わない (道具が古い方を前にする)
            d40, d39 = judged(args + ["--criteria", "T40"]), judged(args + ["--criteria", "T39"])
            out = run(args + ["--criteria", "T40"])
        self.assertEqual(tuple(d40["rows"][14]), self.row(dns_snap(*SHUTDOWN), dns_snap(*SAME_VERSION["48.1"])))
        self.assertEqual((d39["rows"][14][3], d40["rows"][14][3]), (sd.MISSED, sd.MET))
        self.assertIn("(前 0.19、1.62 倍)", d39["rows"][14][2])
        self.assertIn("| %s | %s | %s | **%s** | %s |" % tuple(d40["rows"][14]), out.split("\n"))
        # ほかの行は T39 と同じ (5 行目は名前と閾の欄だけ。`/profile?res=60` を渡していないので材料が無くて判定できず)
        self.assertEqual([i for i in range(27) if d39["rows"][i] != d40["rows"][i]], [4, 14])
        self.assertEqual(d40["tally"][sd.MET], d39["tally"][sd.MET] + 1)
        self.assertEqual(d40["tally"][sd.MISSED], d39["tally"][sd.MISSED] - 1)


class TwoDaysOfTheSameVersion(unittest.TestCase):
    """同じ版 0.39.13 の 2 つの 24 時間 (2026-11-24 と 2026-11-26) を、どちらの向きで比べても届かずにならない (T40.4)。

    材料は `scripts/testdata/` の 4 つ (このファイルの頭)。実物を `snapshot-diff.py --criteria T39` に渡したときと同じ数字が、
    表示の桁で出る:

    - 前 = 2026-11-24、後 = 2026-11-26 … 0.799 倍。T39 でも T40 でも満たした。
    - 前 = 2026-11-26、後 = 2026-11-24 … 1.251 倍 (= 1 ÷ 0.799)。T39 では届かず、T40 では判定できず (帯の内)。
    - 記録にある 1.299 倍 (T40.1 の結果・T40.4 の目的の「向きを入れ替えると 1.299 倍」) … `snapshot-diff.py` に 4 つの引数を
      入れ替えて渡した出力の値。道具はスナップショットを古い順に並べ直し、`--profile` は新しい方に、`--profile-before` は
      古い方に付けるので、2026-11-24 のスナップショットに 2026-11-26 の `/profile` が付いた組になる。名前解決の失敗は、
      窓の合わない `/history` から 0 本と 804 本に数えられる (組を合わせると 944 本と 803 本)。T39 では 1.299 倍の届かず。
      T40 では、前の窓を前のスナップショットの `/history` が覆っていないので、比を出さずに判定できず (T40.5)。
    - 停止の 9 分前に取った `/profile` (締めの前の形) … `/history?res=60` は窓の頭の 9 分を持っていないが、`res=3600` が
      覆うので、窓は合っている。判定は組を合わせたときと同じ (T40.5)。
    """

    @classmethod
    def setUpClass(cls):
        cls.profile = {day: profile_of(day) for day in DAYS}
        cls.history = {day: history_of(day) for day in DAYS}

    def snap(self, history_day, profile_day=None):
        """その日の `/history` に、その日 (か、取り違えたときは別の日) の `/profile?res=60` を付けた最小のスナップショット。"""
        return {"history": self.history[history_day], "profile_res_60": self.profile[profile_day or history_day]}

    def later(self, day, minutes=9):
        """その日の `/profile?res=60` に、`minutes` 分あとに撮ったスナップショットの `/history` を付けたもの (締めの前の形:
        `/profile` は停止の 9 分前に取り、スナップショットは停止のときの 1 枚)。"""
        return {"history": taken_later(self.history[day], minutes), "profile_res_60": self.profile[day]}

    def pair(self, name):
        old, new = "2026-11-24", "2026-11-26"
        return {"古い日 → 新しい日": {"a": self.snap(old), "b": self.snap(new)},
                "新しい日 → 古い日": {"a": self.snap(new), "b": self.snap(old)},
                "`/profile` だけ入れ替わった組": {"a": self.snap(old, new), "b": self.snap(new, old)},
                "後の `/profile` だけ別の日": {"a": self.snap(old), "b": self.snap(old, new)},
                "停止の 9 分前に取った `/profile` → 新しい日": {"a": self.later(old), "b": self.snap(new)},
                "停止の 9 分前に取った `/profile` → 古い日": {"a": self.later(new), "b": self.snap(old)}}[name]

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

    # 2026-11-26 の `/profile?res=60` の窓 (2026-11-26 09:56:23Z に取った 24 時間ぶん)
    WINDOW_1126 = ("2026-11-25 09:56:00Z", "2026-11-26 09:56:00Z", "24.0 時間")
    # 2026-11-24 のスナップショット (12:08:11Z) の `/history?res=60` が持っている時間の端
    HAS_1124 = "`/history?res=60` は 2026-11-23 12:08:00Z〜2026-11-24 12:08:00Z"

    def test_the_recorded_1_299_is_the_pair_with_the_profiles_swapped_and_cannot_be_judged(self):
        # T40.4 のときは「1.299 倍は帯の内なので判定できず」だった。T40.5 からは、比を出す前に材料の窓が合っていないと言う
        # (1.299 倍は、前の名前解決の失敗を 0 本と数えた値。帯の内か外かを読んでよい数字ではない)
        row, t39 = self.t40("`/profile` だけ入れ替わった組"), self.t39("`/profile` だけ入れ替わった組")
        self.assertEqual(t39[3], sd.MISSED)
        self.assertTrue(t39[2].startswith(
            "**1.299** 倍 (前の機械の値段に直した。前の値段で後の仕事 1.38・後の値段で前の仕事 1.22)。"
            "値段 (要求 1 本 + 中継 1 秒): 前 432 + 80.5 us (決定係数 0.45)、後 828 + 44.7 us (-0.02)。"), t39[2])
        # 組が合っていない印: 2026-11-26 の `/profile` の窓を 2026-11-24 の `/history` は覆っていないので、失敗が 0 本になる
        self.assertIn("前: `--profile` 24.0 時間、要求 17,932 本のうち名前解決の失敗 0 本 (0%)。"
                      "後: `--profile` 24.0 時間、要求 17,089 本のうち名前解決の失敗 804 本 (5%)。", t39[4])
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertEqual(row[2], "—")
        self.assertEqual(row[4], mismatch(("前",) + self.WINDOW_1126 + ("0.0%", "24.0 時間", self.HAS_1124), covered=("後", "100.0%")))
        # 後 (2026-11-26 のスナップショットに 2026-11-24 の `/profile`) は、`/history?res=60` が窓に重ならないが `res=3600` が覆う
        self.assertEqual(sc.profile_history_cover(self.pair("`/profile` だけ入れ替わった組"), sc.T40_TH),
                         {"前": {"t0": 1795600560, "t1": 1795686960, "covered": 0},
                          "後": {"t0": 1795435680, "t1": 1795522080, "covered": 86400}})
        # 前置きに書く 2 つの数字は、この組の T39 の値と、組を合わせて入れ替えた向きの値
        self.assertTrue(self.t39("新しい日 → 古い日")[2].startswith("**1.251** 倍"))
        self.assertIn("同じ版の 2 日で `/profile` だけが入れ替わった組は、前の窓がまったく覆われておらず、失敗を 0 本と数えて 1.299 倍と"
                      "出ていました。組を合わせた値は 1.251 倍)。", sc.INTRO["T40"])

    def test_an_after_profile_of_another_day_cannot_be_judged(self):
        # 後だけ合っていない (2026-11-24 のスナップショットに 2026-11-26 の `/profile`)。T39 の行は後の失敗を 0 本と数える
        row, t39 = self.t40("後の `/profile` だけ別の日"), self.t39("後の `/profile` だけ別の日")
        self.assertIn("後: `--profile` 24.0 時間、要求 17,932 本のうち名前解決の失敗 0 本 (0%)。", t39[4])
        self.assertEqual(t39[3], sd.MET)
        self.assertTrue(t39[2].startswith("**0.770** 倍"), t39[2])
        self.assertEqual(row[3], sd.UNKNOWN, row)
        self.assertEqual(row[2], "—")
        self.assertEqual(row[4], mismatch(("後",) + self.WINDOW_1126 + ("0.0%", "24.0 時間", self.HAS_1124), covered=("前", "100.0%")))

    def test_a_profile_taken_9_minutes_before_the_shutdown_snapshot_is_judged(self):
        # 締めの前の形 (0.40.0 の締めの「前」: `/profile?res=60` は 09:56:23Z、停止の 1 枚は 10:05:14Z)。`/history?res=60` は
        # 窓の 24 時間のうち頭の 9 分を持っていない (99.4%) が、そこは `/history?res=3600` が覆うので、窓は丸ごと合っている
        for name, same, want, fails in (("停止の 9 分前に取った `/profile` → 新しい日", "古い日 → 新しい日", sd.MET, "804"),
                                        ("停止の 9 分前に取った `/profile` → 古い日", "新しい日 → 古い日", sd.UNKNOWN, "945")):
            with self.subTest(name=name):
                c = self.pair(name)
                h60 = c["a"]["history"]["60"]["samples"]
                win = sc.profile_window(c["a"]["profile_res_60"])
                self.assertEqual((len(h60), h60[0][0] - win[0]), (1440, 9 * 60))
                self.assertEqual(sc.profile_history_cover(c, sc.T40_TH)["前"]["covered"], 86400)
                self.assertIsNone(sc.window_mismatch(c, sc.T40_TH))
                row, whole = self.t40(name), self.t40(same)
                self.assertEqual(row[3], want, row)
                # 頭の 9 分の失敗は 1 時間の標本を按分して数えるので、1 分の標本で数えたとき (803 本・944 本) と 1 本違う
                self.assertIn("前: `--profile` 24.0 時間、要求 %s 本のうち名前解決の失敗 %s 本 (5%%)。"
                              % ("17,089" if want == sd.MET else "17,932", fails), row[4])
                # 比は表示の桁で同じ (0.799 倍・1.251 倍)。帯の内の判定できずは、材料の窓ではなく日ごとのぶれの理由
                self.assertEqual(row[2][:11], whole[2][:11])
                self.assertNotIn("材料の窓が合っていない", row[4])
        self.assertTrue(self.t40("停止の 9 分前に取った `/profile` → 新しい日")[2].startswith("**0.799** 倍"))
        self.assertTrue(self.t40("停止の 9 分前に取った `/profile` → 古い日")[4].startswith(
            "値付けし合った比 1.251 倍は 1.1 倍以上だが、同じ版の日ごとのぶれの帯 (1.1〜1.3 倍。T40.4) の内。"))

    def test_the_tool_prints_these_numbers_and_swapping_its_four_arguments_swaps_only_the_profiles(self):
        """道具の入口を通しても同じ行が出る: 2 日を古い順に渡すと 0.799 倍、4 つの引数を入れ替えて渡すと、T39 は 1.299 倍
        (記録の値) の届かず、T40 は材料の窓が合っていないので判定できず (T40.5)。

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
        swapped_row = printed(self.t40("`/profile` だけ入れ替わった組"))
        self.assertIn(swapped_row, out_swapped.split("\n"))
        self.assertIn(" | — | **判定できず** | 材料の窓が合っていない (T40.5): 前の `/profile?res=60` の窓 (2026-11-25 09:56:00Z〜"
                      "2026-11-26 09:56:00Z の 24.0 時間) のうち、前のスナップショットの `/history` が覆っているのは 0.0% で、", swapped_row)
        self.assertNotIn("1.299", swapped_row)
        # 入れ替えて渡しても、題の行 (前 → 後) は古い順のまま
        self.assertEqual(out_swapped.split("\n")[0], out.split("\n")[0])
        self.assertIn("2026-11-24T120811Z-snapshot.json` → `", out.split("\n")[0])
        # 記録の「届かず」は、この入れ替えた命令を T39 で出した行
        self.assertIn(printed(self.t39("`/profile` だけ入れ替わった組")), out_swapped_t39.split("\n"))
        self.assertEqual(self.t39("`/profile` だけ入れ替わった組")[3], sd.MISSED)
        self.assertIn("| **1.299** 倍 (前の機械の値段に直した。", printed(self.t39("`/profile` だけ入れ替わった組")))

    def test_no_pairing_of_the_two_days_is_missed(self):
        names = ("古い日 → 新しい日", "新しい日 → 古い日", "`/profile` だけ入れ替わった組", "後の `/profile` だけ別の日",
                 "停止の 9 分前に取った `/profile` → 新しい日", "停止の 9 分前に取った `/profile` → 古い日")
        self.assertEqual([self.t40(name)[3] for name in names],
                         [sd.MET, sd.UNKNOWN, sd.UNKNOWN, sd.UNKNOWN, sd.MET, sd.UNKNOWN])
        # 判定できずの理由は 2 つ: 組の合った入れ替えた向きは日ごとのぶれの帯の内 (T40.4)、組の合っていないものは材料の窓 (T40.5)
        self.assertEqual([self.t40(name)[4].startswith("材料の窓が合っていない (T40.5): ") for name in names],
                         [False, False, True, True, False, False])
        # T39 の行は今までどおり (同じ材料で、入れ替えた向きは届かず。組の合っていないものにも黙って数字を出す)
        self.assertEqual([self.t39(name)[3] for name in names], [sd.MET, sd.MISSED, sd.MISSED, sd.MET, sd.MET, sd.MISSED])

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
                                       ("`/profile` だけ入れ替わった組", (old, new), (new, old)),
                                       ("後の `/profile` だけ別の日", (old, old), (old, new))):
            with self.subTest(name=name):
                real = {"a": dict(self.real[a], profile_res_60=self.real[pa]["profile_res_60"]),
                        "b": dict(self.real[b], profile_res_60=self.real[pb]["profile_res_60"])}
                small = {"a": dict(self.small[a], profile_res_60=self.small[pa]["profile_res_60"]),
                         "b": dict(self.small[b], profile_res_60=self.small[pb]["profile_res_60"])}
                for rule, th in ((sc._t40_conn_priced, sc.T40_TH), (sc._t39_conn_priced, sc.T39_TH)):
                    self.assertEqual(rule(small, th), rule(real, th))
                # 窓を覆っている秒 (T40.5) も、最小の形 (`res=3600` は最後の 72 標本だけ) と実物 (720 標本) で同じ
                self.assertEqual(sc.profile_history_cover(small, sc.T40_TH), sc.profile_history_cover(real, sc.T40_TH))

    def test_the_tool_prints_the_same_row_for_the_real_pair(self):
        # 道具を通した形 (古い日 → 新しい日): 判定表 T40 の 5 行目は、最小の形から出した行と同じ
        old, new = "2026-11-24", "2026-11-26"
        d = build([REAL[old][0], REAL[new][0], "--criteria", "T40", "--no-dns",
                   "--profile-before", REAL[old][1], "--profile", REAL[new][1]])["criteria"]
        self.assertEqual(tuple(d["rows"][4]), sc._t40_conn_priced({"a": self.small[old], "b": self.small[new]}, sc.T40_TH))
        self.assertEqual(d["rows"][4][3], sd.MET)

    def test_swapping_the_four_arguments_cannot_be_judged_for_the_real_pair_either(self):
        # 実物の 2 枚に 4 つの引数を入れ替えて渡した形 (T40.4 の目的の 1.299 倍を出した命令): T39 は届かず、T40 は判定できず
        old, new = "2026-11-24", "2026-11-26"
        args = [REAL[new][0], REAL[old][0], "--no-dns", "--profile-before", REAL[new][1], "--profile", REAL[old][1]]
        t39 = judged(args + ["--criteria", "T39"])["rows"][4]
        t40 = judged(args + ["--criteria", "T40"])["rows"][4]
        self.assertEqual((t39[3], t40[3]), (sd.MISSED, sd.UNKNOWN))
        self.assertTrue(t39[2].startswith("**1.299** 倍"), t39[2])
        self.assertEqual(t40[2], "—")
        self.assertEqual(t40[4], mismatch(("前",) + TwoDaysOfTheSameVersion.WINDOW_1126 + ("0.0%", "24.0 時間", TwoDaysOfTheSameVersion.HAS_1124),
                                          covered=("後", "100.0%")))


# 0.40.0 の締めの「前」と、手元にある「後」(T40.5): 0.39.13 の停止の 1 枚 (2026-11-26 10:05:14Z。その 9 分前に取った
# `/profile?res=60` が `REAL["2026-11-26"]` の隣のもの) と、0.40.0 の起動から 2.7 時間の 1 枚とその `/profile?res=60`
CLOSEOUT = {name: os.path.join(DEPLOYED, "tokyo", stamp) for name, stamp in (
    ("shutdown", "2026-11-26T100514Z-shutdown-snapshot.json"),
    ("next", "2026-11-26T124944Z-snapshot.json"), ("next_profile", "2026-11-26T124944Z-profile_res_60.json"))}
# `SAME_VERSION` の 5 枚の元のスナップショット (日次の 1 枚の名前の日付は、撮った日の前の日)
SAME_VERSION_FILES = {"36.0": "2026-11-23T000000Z", "48.1": "2026-11-24T120811Z", "60.0": "2026-11-24T000000Z",
                      "84.0": "2026-11-25T000000Z", "93.9": "2026-11-26T095623Z"}


def refresh_numbers(snap):
    """実物のスナップショットから、引き直しの行が読む数 (`SAME_VERSION` と同じ並び) を抜く。"""
    dns = snap["status"]["dns"]
    return (snap["uptime_secs"], dns["refreshes"], sc.answered_names(snap), dns["entries"], dns["misses_by_kind"]["expired"])


@unittest.skipUnless(all(os.path.isfile(p) for p in list(CLOSEOUT.values()) + [x for pair in REAL.values() for x in pair]),
                     "tokyo の実データが無い")
class DeployedCloseout(unittest.TestCase):
    """手元に実物があるときだけ回る: 0.40.0 の締めの形で、直した 2 つの行が実物からも同じに出る (T40.5)。"""

    @classmethod
    def setUpClass(cls):
        new = "2026-11-26"
        cls.shutdown = dict(sd.load_source(CLOSEOUT["shutdown"], False), profile_res_60=read(REAL[new][1]))
        cls.next = dict(sd.load_source(CLOSEOUT["next"], False), profile_res_60=read(CLOSEOUT["next_profile"]))
        cls.day = {day: dict(sd.load_source(snap, False), profile_res_60=read(profile)) for day, (snap, profile) in REAL.items()}

    def test_the_numbers_in_this_file_are_the_numbers_of_the_real_snapshots(self):
        self.assertEqual(refresh_numbers(self.shutdown), SHUTDOWN)
        self.assertEqual(self.shutdown["taken_at"], SHUTDOWN_AT)
        self.assertEqual(refresh_numbers(self.next), NEXT_VERSION)
        for shown, stamp in SAME_VERSION_FILES.items():
            path = os.path.join(DEPLOYED, "tokyo", stamp + "-snapshot.json")
            if os.path.isfile(path):
                with self.subTest(shown=shown):
                    snap = sd.load_source(path, False)
                    self.assertEqual(refresh_numbers(snap), SAME_VERSION[shown])
                    self.assertEqual(snap["version"], "0.39.13+11e9be52")
                    # `/dns` の一覧は切れていない (T38.7・T39.8 から。だから T33.2 の行はいつも名前あたりで比べていた)
                    self.assertFalse(snap["dns"]["truncated"])
                    self.assertEqual(len(snap["dns"]["entries"]), snap["dns"]["count"])

    def test_the_refresh_row_from_the_numbers_is_the_row_from_the_real_snapshots(self):
        day = self.day["2026-11-24"]
        cases = (("締めの形 (前 = 停止の 1 枚、後 = 48.1 時間)", self.shutdown, day, {"restarted": True},
                  SHUTDOWN, SAME_VERSION["48.1"], sd.MISSED, "0.95"),
                 ("取得の古い順", day, self.shutdown, sd.restart_info(day, self.shutdown),
                  SAME_VERSION["48.1"], SHUTDOWN, sd.MET, "1.10"),
                 ("手元の実物の組 (後 = 0.40.0 の 2.7 時間)", self.shutdown, self.next, sd.restart_info(self.shutdown, self.next),
                  SHUTDOWN, NEXT_VERSION, sd.MISSED, "1.15"))
        for name, a, b, info, na, nb, t39_verdict, ratio in cases:
            with self.subTest(name=name):
                real = {"a": a, "b": b, "info": info}
                small = {"a": dns_snap(*na), "b": dns_snap(*nb), "info": info}
                row = sc._t40_refresh_total(real, sc.T40_TH)
                self.assertEqual(row, sc._t40_refresh_total(small, sc.T40_TH))
                self.assertEqual(row[3], sd.MET)
                self.assertIn("、**%s** 倍)。表示だけ: " % ratio, row[2])
                t39 = sc._p30_refresh_per_name(real, sc.T39_TH)
                self.assertEqual(t39, sc._p30_refresh_per_name(small, sc.T39_TH))
                self.assertEqual(t39[3], t39_verdict)
        self.assertEqual(sd.restart_info(day, self.shutdown)["restarted"], False)
        self.assertEqual(sd.restart_info(self.shutdown, self.next)["restarted"], True)

    def test_the_window_of_the_profile_taken_before_the_shutdown_is_covered(self):
        # 停止の 1 枚の `/history?res=60` は、9 分前に取った `/profile?res=60` の窓の頭の 9 分を持っていない (99.4%)。
        # そこは `/history?res=3600` が覆うので、窓は丸ごと覆われている
        p = self.shutdown["profile_res_60"]
        win = sc.profile_window(p)
        h60 = self.shutdown["history"]["60"]["samples"]
        self.assertEqual((len(p["samples"]), len(h60), h60[0][0] - win[0]), (1440, 1440, 9 * 60))
        self.assertEqual(sc.history_cover_secs(self.shutdown, win[0], win[1] + 60), 86400)
        self.assertEqual(sc.dns_failures_between(self.shutdown, win[0], win[1] + 60), 945)
        # 0.40.0 の 1 枚に、同じ時に取った `/profile?res=60` (2.7 時間ぶん) も丸ごと覆われている
        win = sc.profile_window(self.next["profile_res_60"])
        self.assertEqual(sc.history_cover_secs(self.next, win[0], win[1] + 60), win[1] + 60 - win[0])

    def test_the_conn_row_is_judged_for_the_closeout_shape(self):
        # 前 = 停止の 1 枚と 9 分前の `/profile?res=60`。後に同じ版のもう 1 日 (2026-11-24) を置くと、行は材料の窓ではなく
        # 比で判定する (1.251 倍。日ごとのぶれの帯の内)。最小の形 (`taken_later`) から出した行と 1 文字も違わない
        old, new = "2026-11-24", "2026-11-26"
        small = {day: {"history": history_of(day), "profile_res_60": profile_of(day)} for day in DAYS}
        later = {"history": taken_later(history_of(new), 9), "profile_res_60": profile_of(new)}
        for name, real, tiny, want, shown in (
                ("前 = 停止の 1 枚", {"a": self.shutdown, "b": self.day[old]}, {"a": later, "b": small[old]}, sd.UNKNOWN, "**1.251** 倍"),
                ("後 = 停止の 1 枚", {"a": self.day[old], "b": self.shutdown}, {"a": small[old], "b": later}, sd.MET, "**0.799** 倍")):
            with self.subTest(name=name):
                row = sc._t40_conn_priced(real, sc.T40_TH)
                self.assertEqual(row[3], want, row)
                self.assertTrue(row[2].startswith(shown), row[2])
                self.assertNotIn("材料の窓が合っていない", row[4])
                self.assertEqual(row, sc._t40_conn_priced(tiny, sc.T40_TH))
                self.assertIsNone(sc.window_mismatch(real, sc.T40_TH))

    def test_the_tool_prints_the_two_rows_for_the_real_pair(self):
        # 手元の実物の組を道具に通す: 前 = 0.39.13 の停止の 1 枚と 9 分前の `/profile?res=60`、後 = 0.40.0 の 2.7 時間
        args = [CLOSEOUT["shutdown"], CLOSEOUT["next"], "--no-dns",
                "--profile-before", REAL["2026-11-26"][1], "--profile", CLOSEOUT["next_profile"]]
        t40, t39 = judged(args + ["--criteria", "T40"]), judged(args + ["--criteria", "T39"])
        self.assertEqual([i for i in range(27) if t39["rows"][i] != t40["rows"][i]], [4, 14])
        # 5 行目: 後の `/profile?res=60` が 2.7 時間ぶんしか無いので、T39 の行の理由のまま判定できず
        self.assertEqual(t40["rows"][4][2:], t39["rows"][4][2:])
        self.assertEqual(t40["rows"][4][2:], ["—", sd.UNKNOWN, "後の `/profile` が 2.7 時間 (164 標本) で 12 時間に満たない "
                                              "(`--closeout` は前の版の最後の 12 時間以上の `-profile_res_60.json` を選ぶ)"])
        # 15 行目: 名前 1 つあたりは 11.10 倍の届かず、合計は 1.15 倍の満たした
        self.assertEqual((t39["rows"][14][3], t40["rows"][14][3]), (sd.MISSED, sd.MET))
        self.assertTrue(t39["rows"][14][2].startswith("**2.12** 回/時/答えのある名前 (前 0.19、11.10 倍)。"), t39["rows"][14][2])
        self.assertTrue(t40["rows"][14][2].startswith("合計 **413.2** 回/時 (前 359.2、**1.15** 倍)。"), t40["rows"][14][2])
        self.assertEqual(tuple(t40["rows"][14]), sc._t40_refresh_total(
            {"a": dns_snap(*SHUTDOWN), "b": dns_snap(*NEXT_VERSION), "info": {"restarted": True}}, sc.T40_TH))


if __name__ == "__main__":
    unittest.main()
