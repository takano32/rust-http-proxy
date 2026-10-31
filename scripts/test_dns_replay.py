#!/usr/bin/env python3
"""`scripts/dns-replay.py` (T.17.5) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から

入力は**架空の**雪像 (`testdata/replay-dns.json`、名前は `*.example.invalid`) と、
その場で組んだ到着列。**本物の雪像 (利用者の閲覧先が並ぶ) は入れない**。
"""

import contextlib
import importlib.util
import io
import json
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "testdata")


def _load(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


dr = _load("dns_replay", "dns-replay.py")
SNAP = os.path.join(DATA, "replay-dns.json")


def read(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def run_table(rule, arrivals, end, serve_stale=0, **kw):
    tb = dr.Table(dr.RULES[rule], serve_stale=serve_stale, **kw)
    tb._warm_t = 0
    for t, h in arrivals:
        tb.lookup(h, t)
    tb.advance(end)
    return tb.c


class RulesTest(unittest.TestCase):
    def test_a_name_used_once_is_one_cold_miss_and_never_warm(self):
        c = run_table("current", [(0, "a")], 7200)
        self.assertEqual(c["misses"], 1)
        self.assertEqual(c["misses_by_kind"]["cold"], 1)
        self.assertEqual(c["promotes"], 0)
        self.assertEqual(c["refreshes"], 0)

    def test_a_second_use_inside_the_ttl_is_a_hit(self):
        c = run_table("current", [(0, "a"), (30, "a")], 30)
        self.assertEqual((c["hits"], c["misses"]), (1, 0 + 1))

    def test_the_second_use_inside_the_window_makes_it_warm(self):
        # 2 回目 (期限切れのミス) で warm になり、3 回目はずっと後でも当たる
        c = run_table("current", [(0, "a"), (600, "a"), (1800, "a")], 1800)
        self.assertEqual(c["misses_by_kind"], {"cold": 1, "expired": 1, "warm_stale": 0, "negative": 0})
        self.assertEqual(c["hits"], 1)
        self.assertEqual(c["promotes"], 1)
        # 600 → 1800 秒の間 45 秒ごと = 26 回 (1,800 ちょうどの予定は当たりの前に捌く)
        self.assertEqual(c["refresh_warm"], 26)

    def test_warm_stops_after_the_window(self):
        c = run_table("current", [(0, "a"), (600, "a")], 600 + 3600 * 3)
        # 600 + 45k が 600 + 3600 を過ぎたところで外れる: 45 × 80 = 3600 ちょうどで外れるので 79 回
        self.assertEqual(c["refresh_warm"], 79)
        self.assertEqual(c["warm_max"], 1)

    def test_current_rule_does_not_promote_after_a_long_gap_but_no_idle_does(self):
        # 2 回目は窓 (3,600 秒) の外、3 回目はその 10 分後
        arr = [(0, "a"), (5000, "a"), (5600, "a")]
        cur = run_table("current", arr, 5600)
        new = run_table("no_idle", arr, 5600)
        self.assertEqual(cur["misses"], 3)   # 2 回目で上がらないので 3 回目も期限切れ
        self.assertEqual(new["misses"], 2)   # 2 回目で上がって 3 回目は当たり
        self.assertEqual(cur["promotes"], 1)  # 3 回目 (窓の中の 2 回目) で上がる
        self.assertEqual(new["promotes"], 1)

    def test_a_hot_hit_past_three_quarters_of_the_ttl_asks_for_a_refresh(self):
        c = run_table("current", [(0, "a"), (40, "a"), (50, "a")], 50)
        # 40 秒で warm に上がる (予定は 85 秒)。50 秒は齢 50 ≥ 45 かつ 1 つ前が 10 秒前 = 先回り
        self.assertEqual(c["refresh_ask"], 1)
        self.assertEqual(c["hits"], 2)

    def test_promoting_on_a_hit_can_leave_a_stale_gap(self):
        # 30 秒の当たりで warm に上がると最初の予定は 75 秒で、齢 60〜75 の間は期限切れ
        c = run_table("current", [(0, "a"), (30, "a"), (70, "a")], 70)
        self.assertEqual(c["misses_by_kind"]["warm_stale"], 1)

    def test_max_warm_evicts_the_oldest(self):
        arr = []
        for i in range(dr.MAX_WARM + 1):
            arr += [(i, f"n{i}"), (i + 100, f"n{i}")]
        arr.sort()
        tb = dr.Table(dr.RULES["current"])
        for t, h in arr:
            tb.lookup(h, t)
        self.assertEqual(tb.c["warm_evicted"], 1)
        self.assertEqual(len(tb.queue), dr.MAX_WARM)
        self.assertNotIn("n0", tb.queue)

    def test_serve_stale_answers_an_expired_entry_inside_the_window(self):
        # T.21.3: 2 回目は齢 5,000 秒 (窓 3,600 秒の外なので warm にもならない)
        arr = [(0, "a"), (5000, "a")]
        cur = run_table("current", arr, 5000)
        self.assertEqual(cur["misses_by_kind"]["expired"], 1)
        self.assertEqual((cur["stale_hits"], cur["refresh_stale"]), (0, 0))
        # 齢が窓の外なら今までどおり同期で引く (expired のミス)
        short = run_table("current", arr, 5000, serve_stale=3600)
        self.assertEqual(short["misses_by_kind"]["expired"], 1)
        self.assertEqual(short["stale_hits"], 0)
        # 窓の中なら古い答えで当たり、裏で 1 回だけ引き直す (ミスには数えない)
        long = run_table("current", arr, 5000, serve_stale=21600)
        self.assertEqual(long["misses"], 1)
        self.assertEqual(long["misses_by_kind"]["expired"], 0)
        self.assertEqual((long["stale_hits"], long["refresh_stale"], long["refreshes"]), (1, 1, 1))

    def test_serve_stale_does_not_touch_answers_inside_the_ttl(self):
        c = run_table("current", [(0, "a"), (30, "a")], 30, serve_stale=21600)
        self.assertEqual((c["hits"], c["stale_hits"], c["refresh_stale"]), (1, 0, 0))

    def test_serve_stale_refreshes_so_the_next_use_is_a_plain_hit(self):
        # 5,000 秒で古い答え → 裏で引き直したので 5,030 秒は TTL 内の当たり
        c = run_table("current", [(0, "a"), (5000, "a"), (5030, "a")], 5030, serve_stale=21600)
        self.assertEqual((c["stale_hits"], c["hits"], c["misses"]), (1, 1, 1))

    def test_host_of(self):
        self.assertEqual(dr.host_of("connect://Chat.Example.Invalid:443"), "chat.example.invalid")
        self.assertEqual(dr.host_of("chat.example.invalid:443"), "chat.example.invalid")
        self.assertEqual(dr.host_of("[2001:db8::1]:443"), "2001:db8::1")
        self.assertTrue(dr.is_ip("2001:db8::1"))


class WarmGridTest(unittest.TestCase):
    """T.22.4: warm の枠と窓、`warm_promote` の予定、名前ごとの引き直し。"""

    def test_no_warm_refreshes_only_on_lookups(self):
        # 窓 0 = warm をやめる。5 分おきの 5 回は、2 回目から古い答え + 裏の引き直し 1 回ずつ
        arr = [(i * 300, "a") for i in range(5)]
        c = run_table("current", arr, 3600, serve_stale=21600, window=0)
        self.assertEqual((c["promotes"], c["refresh_warm"]), (0, 0))
        self.assertEqual((c["stale_hits"], c["refresh_stale"], c["refreshes"]), (4, 4, 4))
        self.assertEqual(c["misses"], 1)
        # 古い答えの齢はどれも 300 秒 (裏の引き直しで 1 つ前の使用の時刻に引き直している)
        self.assertEqual(c["stale_age_sum"], 4 * 300)
        self.assertEqual(c["stale_age_max"], 300)
        # warm なら 2 回目 (300 秒) で上がり、そこから 45 秒ごとに引き直す
        w = run_table("current", arr, 3600, serve_stale=21600)
        self.assertEqual(w["stale_hits"], 1)
        self.assertGreater(w["refresh_warm"], 50)

    def test_promote_at_answer_closes_the_stale_gap(self):
        # 30 秒の当たりで上がる。いまの予定は 75 秒 (齢 75) で、70 秒は期限切れ
        arr = [(0, "a"), (30, "a"), (70, "a")]
        now = run_table("current", arr, 70)
        ans = run_table("current", arr, 70, promote_at="answer")
        self.assertEqual(now["misses_by_kind"]["warm_stale"], 1)
        # 答えの齢から出すと予定は 45 秒 = 70 秒の時点で齢 25 の当たり
        self.assertEqual(ans["misses_by_kind"]["warm_stale"], 0)
        self.assertEqual(ans["refresh_warm"], 1)

    def test_serve_stale_hides_the_gap_as_a_warm_stale_hit(self):
        # serve-stale のもとでは同じすき間はミスにならず、warm の古い答えに出る
        c = run_table("current", [(0, "a"), (30, "a"), (70, "a")], 70, serve_stale=21600)
        self.assertEqual(c["misses_by_kind"]["warm_stale"], 0)
        self.assertEqual((c["stale_hits"], c["stale_hits_warm"]), (1, 1))

    def test_per_name_refreshes_are_counted(self):
        tb = dr.Table(dr.RULES["current"])
        tb._warm_t = 0
        for t, h in [(0, "a"), (10, "a"), (0, "b")]:
            tb.lookup(h, t)
        tb.advance(3600)
        # a は 10 秒で上がって 55 秒から 45 秒ごと (窓 3,600 秒の中)、b は 1 回だけで引かない
        self.assertEqual(tb.per_name.get("b", 0), 0)
        self.assertEqual(tb.per_name["a"], tb.c["refreshes"])
        self.assertEqual(tb.per_name["a"], 79)

    def test_prime_warm_puts_the_name_on_the_queue(self):
        tb = dr.Table(dr.RULES["current"])
        tb.counting = False
        tb.prime("a", 100, warm=True)
        self.assertIn("a", tb.queue)
        self.assertEqual(tb.queue["a"], 145)
        off = dr.Table(dr.RULES["current"], window=0)
        off.prime("a", 100, warm=True)
        self.assertEqual(off.queue, {})

    def test_the_refresh_age_can_be_moved_to_the_ttl(self):
        # T.24.1: 「k (3/4 TTL) と TTL のどちらが引き直しの間隔を決めているか」を分けて見る口。
        # 12 秒ごとに使う名前は、k=45 なら 48 秒ごと、k=60 (= TTL) なら 60 秒ごとに引き直す
        arrivals = [(t, "a") for t in range(0, 3600, 12)]
        k45 = run_table("current", arrivals, 3600, window=0, serve_stale=21600, refresh_at=45)
        ttl = run_table("current", arrivals, 3600, window=0, serve_stale=21600, refresh_at=60)
        self.assertEqual(k45["refreshes"], 74)   # 48 秒ごと
        self.assertEqual(ttl["refreshes"], 59)   # 60 秒ごと
        # 既定 (None) は TTL の 3/4 = 45 秒のまま
        same = run_table("current", arrivals, 3600, window=0, serve_stale=21600)
        self.assertEqual(same["refreshes"], k45["refreshes"])

    def test_replay_reports_the_refreshes_per_name(self):
        snap = {"taken_at": 600, "recent": {"recent": [
            {"at": t, "secs": 0, "target": "a.example.invalid:443"} for t in range(0, 600, 12)]},
            "dns": {"entries": []}, "connections": {"connections": []}}
        warmup, inside, c0, taken, seen = dr.build(snap)
        c = dr.replay(warmup, inside, c0, taken, dr.promote_current, seen,
                      window=0, serve_stale=21600, refresh_at=45)
        self.assertEqual(c["per_name"], {"a.example.invalid": c["refreshes"]})
        self.assertEqual(c["per_name"]["a.example.invalid"], c["refresh_name_max"])

    def test_grid_verdict_needs_both_axes(self):
        def c(expired, warm_stale, refreshes):
            return {"misses_by_kind": {"cold": 0, "expired": expired, "warm_stale": warm_stale},
                    "refreshes": refreshes, "hours": 24.0}
        base = c(2, 0, 1000)
        self.assertTrue(dr.grid_verdict(base, c(12, 0, 500))["ok"])
        # 引き直しが半分を越える
        self.assertFalse(dr.grid_verdict(base, c(2, 0, 501))["ok"])
        # ミスの増えが 1 日 10 回を越える (24 時間で +11)
        v = dr.grid_verdict(base, c(12, 1, 100))
        self.assertFalse(v["ok"])
        self.assertEqual(v["misses_more"], 11)
        self.assertAlmostEqual(v["misses_more_per_day"], 11.0)
        # 区間が 1 時間なら +1 は 1 日 24 回
        short = dict(base, hours=1.0)
        self.assertAlmostEqual(dr.grid_verdict(short, c(3, 0, 10))["misses_more_per_day"], 24.0)


class SnapshotTest(unittest.TestCase):
    def test_build_splits_warmup_and_window(self):
        snap = read(SNAP)
        warmup, inside, c0, taken, seen = dr.build(snap)
        # 閉じた時刻の最小 = 1789000000 + 0 (区間の始まり)
        self.assertEqual(c0, 1789000000)
        self.assertEqual(taken, 1789007200)
        self.assertTrue(all(t < c0 for t, _ in warmup))
        self.assertTrue(all(t >= c0 for t, _ in inside))
        # IP リテラルは数えない、開いたままの接続は入れる
        names = {h for _, h in inside}
        self.assertNotIn("192.0.2.1", names)
        self.assertIn("live.example.invalid", names)
        # `/dns` の通算が区間のミスより多い名前 = 前から居た (区間で初めて引いた名前は入らない)
        self.assertIn("old.example.invalid", seen)
        self.assertNotIn("new.example.invalid", seen)
        self.assertNotIn("api.example.invalid", seen)

    def test_run_on_the_fixture(self):
        # T.17.5 の模型 (keep-warm あり) は T.29.17 から `keep_warm` の名前で残る
        out = dr.run(read(SNAP), ["keep_warm", "no_idle"])
        cur, new = out["rules"]["keep_warm"], out["rules"]["no_idle"]
        self.assertEqual(out["actual"], {"connects": 9, "dns_misses": 4,
                                         "miss_per_connect": 4 / 9, "refresh_per_hour": None})
        self.assertEqual(cur["lookups"], 9)
        # chat は助走で warm → 区間の前半はミスしない (最後の 1 回は窓の外で期限切れ)。
        # old は前から居たので no_idle だけ 1 回目で上がり、2 回目が当たる
        self.assertEqual(cur["misses"], 6)
        self.assertEqual(new["misses"], 5)
        self.assertEqual(cur["misses_by_kind"], {"cold": 3, "expired": 3, "warm_stale": 0, "negative": 0})
        self.assertEqual((cur["promotes"], new["promotes"]), (1, 2))
        self.assertGreater(new["refreshes"], cur["refreshes"])
        self.assertIn("delta", out)

    def test_cli_prints_the_table(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(dr.main([SNAP]), 0)
        text = buf.getvalue()
        self.assertIn("| current |", text)
        self.assertIn("| keep_warm |", text)
        self.assertIn("| no_idle |", text)
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(dr.main([SNAP, "--json", "--rule", "current"]), 0)
        self.assertEqual(list(json.loads(buf.getvalue())["rules"]), ["current"])

    def test_serve_stale_on_the_fixture(self):
        out = dr.run(read(SNAP), ["keep_warm"], stale=[3600, 86400])
        cur = out["rules"]["keep_warm"]
        s1, s2 = out["rules"]["stale_3600"], out["rules"]["stale_86400"]
        self.assertEqual(cur["misses_by_kind"]["expired"], 3)
        self.assertEqual((s1["misses_by_kind"]["expired"], s1["stale_hits"]), (1, 2))
        self.assertEqual((s2["misses_by_kind"]["expired"], s2["stale_hits"]), (0, 3))
        # 古い答えの数だけ裏の引き直しが増え、ミスは同じ数だけ減る
        self.assertEqual(s2["refreshes"] - cur["refreshes"], 3)
        self.assertEqual(cur["misses"] - s2["misses"], 3)
        self.assertTrue(out["stale"]["3600"]["ok"])
        self.assertAlmostEqual(out["stale"]["3600"]["expired_ratio"], 1 / 3)
        self.assertEqual(out["stale_pick"], 3600)

    def test_stale_verdict_needs_both_axes(self):
        base = {"misses_by_kind": {"expired": 10}, "refreshes": 100}
        self.assertTrue(dr.stale_verdict(base, {"misses_by_kind": {"expired": 5},
                                                "refreshes": 110})["ok"])
        self.assertFalse(dr.stale_verdict(base, {"misses_by_kind": {"expired": 6},
                                                 "refreshes": 100})["ok"])
        self.assertFalse(dr.stale_verdict(base, {"misses_by_kind": {"expired": 0},
                                                 "refreshes": 111})["ok"])

    def test_cli_serve_stale(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(dr.main([SNAP, "--rule", "no_idle", "--serve-stale", "3600"]), 0)
        text = buf.getvalue()
        # serve-stale は keep-warm の模型 (`keep_warm`) と比べるので、`--rule` に無くても足す
        self.assertIn("| keep_warm |", text)
        self.assertIn("| stale_3600 |", text)
        self.assertIn("目安を満たすいちばん短い窓: 3600 秒", text)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(dr.main([SNAP, "--serve-stale", "0"]), 2)

    def test_grid_on_the_fixture(self):
        out = dr.run_grid(read(SNAP))
        self.assertEqual(list(out["sides"]), ["lower", "upper"])
        names = [n for n, _ in dr.GRID]
        for side in out["sides"].values():
            self.assertEqual(list(side["rules"]), names)
            self.assertEqual(list(side["verdict"]), names[1:])
            base, off = side["rules"]["base"], side["rules"]["no_warm"]
            # warm をやめると warm の引き直しは 0、ミスは同じ (古い答えが受ける)
            self.assertEqual(off["refresh_warm"], 0)
            self.assertEqual(off["misses"], base["misses"])
            self.assertLess(off["refreshes"], base["refreshes"])
        lower, upper = out["sides"]["lower"]["rules"], out["sides"]["upper"]["rules"]
        # 上限側は前から居た名前 (old) を区間の始まりで warm に置く = warm の引き直しが増える
        self.assertGreater(upper["base"]["refresh_warm"], lower["base"]["refresh_warm"])
        self.assertEqual(lower["base"]["refreshes"], 206)
        self.assertEqual(upper["base"]["refreshes"], 284)
        self.assertEqual(out["pass"], ["window900", "no_warm"])
        # 基準は serve-stale 21,600 秒 (T.21.3 の既定) の上
        self.assertEqual(lower["base"]["misses_by_kind"]["expired"], 0)

    def test_grid_series_on_the_fixture(self):
        arrivals, t0, end = dr.build_series(read(SNAP))
        self.assertEqual(t0, 1788996400)
        self.assertEqual(end, 1789003600)
        self.assertEqual(len(arrivals), 24)
        self.assertTrue(all(t0 <= t < end for t, _ in arrivals))
        out = dr.run_grid(read(SNAP), series=True)
        self.assertEqual(list(out["sides"]), ["series"])
        self.assertIsNone(out["actual"])
        self.assertEqual(out["pass"], ["no_warm"])

    def test_actual_grid_reads_the_warm_gauge_and_the_bound(self):
        snap = read(SNAP)
        snap = dict(snap, status={"dns": {"hits": 70, "misses": 10, "stale_hits": 20,
                                          "refreshes": 400,
                                          "misses_by_kind": {"cold": 6, "expired": 4,
                                                             "warm_stale": 0}}})
        _, _, c0, taken, _ = dr.build(snap)
        a = dr.actual_grid(snap, c0, taken)
        # warm をやめたときの引き直しは名前を引いた回 (70 + 10 + 20) より多くならない
        self.assertAlmostEqual(a["no_warm_refresh_bound"], 100 / 400)
        self.assertAlmostEqual(a["refresh_per_hour_since_start"], 400 / 25.0)

    def test_cli_grid(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(dr.main([SNAP, "--grid"]), 0)
        text = buf.getvalue()
        self.assertIn("[lower]", text)
        self.assertIn("| no_warm |", text)
        self.assertIn("満たす案: window900, no_warm", text)
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(dr.main([SNAP, "--grid", "--series", "--json"]), 0)
        self.assertEqual(list(json.loads(buf.getvalue())["sides"]), ["series"])
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(dr.main([SNAP, "--series"]), 2)

    def test_no_recent_is_an_error(self):
        with self.assertRaises(ValueError):
            dr.build({"taken_at": 1, "recent": {"recent": []}})



class CurrentModelTest(unittest.TestCase):
    """T.29.17: 「今の規則」(`current`) は keep-warm を止めた模型 (warm 0・serve-stale 21,600 秒) で、
    名前解決に失敗した到着は負のキャッシュ (60 秒) で回す。一致は雪像の実機の値と比べる。"""

    def test_current_never_warms_and_answers_stale(self):
        tb = dr.Table(dr.RULES["current"], window=0, serve_stale=21600)
        tb._warm_t = 0
        for t, h in [(0, "a"), (600, "a"), (1800, "a")]:
            tb.lookup(h, t)
        tb.advance(3600)
        c = tb.c
        self.assertEqual((c["misses"], c["stale_hits"], c["promotes"]), (1, 2, 0))
        self.assertEqual((c["refresh_warm"], c["refresh_stale"]), (0, 2))

    def test_failed_lookups_use_the_negative_cache(self):
        tb = dr.Table(dr.RULES["current"], window=0, serve_stale=21600)
        tb._warm_t = 0
        for t in (0, 30, 59, 61, 200):
            tb.lookup("nx", t, fail=True)
        c = tb.c
        # 0 でミス (期限 60)、30・59 は当たり、61 でミス (期限 121)、200 でミス
        self.assertEqual((c["misses"], c["negative_hits"]), (3, 2))
        self.assertEqual(c["misses_by_kind"]["negative"], 3)
        self.assertEqual(c["refreshes"], 0, "失敗した名前は裏で引き直さない")

    def snap_with_real(self, refreshes, uptime):
        snap = read(SNAP)
        snap["status"] = {"dns": {"refreshes": refreshes}}
        snap["uptime_secs"] = uptime
        return snap

    def test_the_fit_is_against_the_real_values_of_the_snapshot(self):
        snap = self.snap_with_real(refreshes=10, uptime=3600)
        out = dr.run(snap, ["current"])
        self.assertEqual(out["fit"]["target"], {"misses": 4, "refresh_per_hour": 10.0})
        cur = out["rules"]["current"]
        self.assertEqual(out["fit"]["miss_per_lookup"], dr.within(cur["misses"], 4))
        self.assertEqual(out["fit"]["refresh_per_hour"], dr.within(cur["refresh_per_hour"], 10.0))
        # 雪像に `/status` の `dns.refreshes` が無ければ T.16.99 の物差しのまま
        out = dr.run(read(SNAP), ["current"])
        self.assertEqual(out["fit"]["target"]["refresh_per_hour"], dr.TARGET_REFRESH_PER_HOUR)

    def test_the_failed_rows_of_recent_are_found(self):
        snap = read(SNAP)
        rows = snap["recent"]["recent"]
        rows[0]["reason"] = "error:nxdomain"
        _, _, c0, _, _ = dr.build(snap)
        fails = dr.failed_lookups(snap, c0)
        self.assertIn((rows[0]["at"], dr.host_of(rows[0]["target"])), fails)
        self.assertEqual(len(fails), 1)


T2917 = os.path.join(os.path.dirname(HERE), "status", "tokyo", "2026-10-27T024200Z-snapshot.json")


@unittest.skipUnless(os.path.exists(T2917), "tokyo の実データが無い (status/tokyo/)")
class Deployed2917(unittest.TestCase):
    """T.29.17 の受け入れ基準: 10-27 の雪像で、今の規則の模型がミス・引き直しとも実機の ±30%。

    実機は同じ区間 (2.54 時間) のミス 130 回、引き直し 374 回/時 (起動から 24.1 時間。`warm_secs` 0)。
    T.29.16 までの `current` (keep-warm あり) は引き直し 2,273 回/時で「合わない」だった。
    """

    def test_the_current_model_matches_the_real_proxy(self):
        out = dr.run(read(T2917), ["current", "keep_warm"])
        cur = out["rules"]["current"]
        self.assertTrue(out["fit"]["miss_per_lookup"], (cur["misses"], out["fit"]))
        self.assertTrue(out["fit"]["refresh_per_hour"], (cur["refresh_per_hour"], out["fit"]))
        self.assertTrue(dr.within(cur["misses"], 130), cur["misses"])
        self.assertTrue(dr.within(cur["refresh_per_hour"], 373.6), cur["refresh_per_hour"])
        # keep-warm の模型は今の実機と合わない (引き直しが 6 倍)
        self.assertFalse(dr.within(out["rules"]["keep_warm"]["refresh_per_hour"], 373.6))

if __name__ == "__main__":
    unittest.main()
