#!/usr/bin/env python3
"""判定表 phase23・phase24・phase25 の試験 (無言の再起動と、その起動の切り分けも)。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

`scripts/test_snapshot_criteria.py` (3,874 行) を判定表ごとに割ったうちの 1 つ (T38.6)。中身は割る前と同じ。
作り物は `scripts/_criteria_testing.py` にある。
"""

import json
from datetime import datetime, timezone
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
    sc,
    sd,
    written,
    zero_snapshot,
    cached,
    Criteria17Parts,
)


class Criteria23(unittest.TestCase):
    """無言の再起動 (T23.2)。`/events` の `start` の直前に `shutdown` が無い起動を数える行。

    実データでの数字は `Deployed23` (デプロイ先のスナップショットがあるときだけ)。ここは `events` をその場で
    作って、同じ秒の出来事・窓の先頭・読み戻し (`restored`)・自己ベンチの `start` の扱いを見る。
    """

    T0 = 1790000000          # 窓の先頭の `start` の時刻 (2026-09-21 14:13:20Z)
    BOOT = ("version 0.22.6+000681e on port 60357 (profile default, cache on, timeout 30s, "
            "max conns 240)")
    STOP = "stop signal received; saving statistics"

    def snap(self, events, restored=0):
        """`testdata/snapshot-a.json` を土台に、`/events` だけを載せ替えたスナップショットを作る。"""
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
    数字は 2026-10-17 → 10-20 の tokyo のスナップショットから採った (前 24 件 / 1,991 試行、後 30 件 /
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
        """0 時間のスナップショットと後のスナップショットが同じ版で起動し直している (2026-10-17 22:14:52Z の形)。"""
        b = cached(read(B), 0)
        b["events"] = self.starts(Criteria17Parts.START)
        z = cached(zero_snapshot(), 0)
        # 0 時間のスナップショットは 2 時間前の起動 (取得の 2 分後が起動なので、起動は taken_at − 120)。
        # 同じ UTC の日なので、2 つ目の時刻は日付を省いて出る
        z["taken_at"] = Criteria17Parts.START - 2 * 3600 + 120
        z["events"] = self.starts(z["taken_at"] - 120)
        row = self.heap(z, b)
        self.assertEqual(row[3], sd.UNKNOWN)
        want = f"`/events` の `start` が {self.iso(Criteria17Parts.START - 2 * 3600)} と " \
               f"{self.iso(Criteria17Parts.START)[11:]}、同じ版 `0.1.0+bbbbbbb` で起動し直している"
        self.assertIn(want, row[2])
        self.assertIn("起動の時刻はどちらも `/events` の `start` から", row[4])

    @staticmethod
    def iso(t):
        return datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    def test_without_events_the_heap_row_falls_back_to_taken_at_minus_uptime(self):
        b = cached(read(B), 0)
        z = cached(zero_snapshot(), 0)
        z["taken_at"] = Criteria17Parts.START - 2 * 3600 + 120
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
        """`boot` の無いスナップショットでは要点の文を 1 文字も変えない (過去の phase の出力を変えない)。"""
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


class Criteria24(unittest.TestCase):
    """`_p24_heap` (T24.4): heap の行を**起動から 60 秒の基準** (`memory.baseline`) との差で読む。

    行そのものはここで確かめ、`RULES["phase24"]` への配線は T24.2。材料は後のスナップショットの
    `/status` の `memory` だけで、**0 時間のスナップショット (`--zero`) は要らない**。数字は T23.99 の実測
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
        """受け入れ基準: `baseline` のあるスナップショットでは、0 時間のスナップショットが無くても差が出る。"""
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
        """`baseline` が無いスナップショット (T24.4 より前の版・起動から 60 秒以内) は今までどおり。"""
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
    """T24.2: phase24 を足しても **phase22 と phase23 の出力** (「平常時の前後」の表と判定表、json の
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
        """`memory.baseline` も `boot` も無いスナップショット (T24.4・T24.0 より前の版) では 2 行とも phase23 と同じ。"""
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
        """普通の再起動のあと、`shutdown` を残さずにもう一度起動し直したスナップショットの 16 番目の行。"""
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


class Criteria25Rows(unittest.TestCase):
    """`--criteria phase25` (T27.0): phase24 の 17 行のうち、T25.99 で物差しの穴だった 3 行。

    (a) 無言の再起動は前のスナップショットの取得より後の `start` だけ、(b) 接続のエラーは試行 1,000 本の下限、
    (c) 引き直しは 1 名前あたり。数字は T25.99 の 2 枚 (`2026-10-22T000000Z` → `2026-10-24T184027Z`) から採った。
    """

    SINCE = 1792713604       # 前のスナップショット (サーバーの日次) の取得 = 2026-10-23 00:00:04Z
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
        # 後の起動 (取得 − uptime) が前のスナップショットより前 = 2 枚は同じプロセス
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
                     "tokyo の実データが無い (status/tokyo/ の T25.99 の 2 枚)")
class Deployed25(unittest.TestCase):
    """T25.99 の 2 枚の phase25 (T27.0 の受け入れ基準): 直した 3 行が 判定できず・満たした・満たした。"""

    def test_t2599_pair(self):
        c = build([T2599_BEFORE, T2599_AFTER, "--no-dns", "--criteria", "phase25"])["criteria"]
        verdicts = [r[3] for r in c["rows"]]
        self.assertEqual(verdicts[12], sd.UNKNOWN)
        self.assertEqual(verdicts[14], sd.MET)
        self.assertEqual(verdicts[15], sd.MET)


if __name__ == "__main__":
    unittest.main()
