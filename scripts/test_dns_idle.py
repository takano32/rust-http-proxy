#!/usr/bin/env python3
"""`scripts/dns-idle.py` (T.24.1 の段 1) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から

入力は**架空の**雪像 (`testdata/dns-idle-{a,b,c,model}.json`、名前は `*.example.invalid`)。
**本物の雪像 (利用者の閲覧先が並ぶ) は入れない**。a → b は同じプロセスの 1 時間あき、
c は版が変わった (起動が変わった) 雪像。
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


di = _load("dns_idle", "dns-idle.py")
A = os.path.join(DATA, "dns-idle-a.json")
B = os.path.join(DATA, "dns-idle-b.json")
C = os.path.join(DATA, "dns-idle-c.json")
MODEL = os.path.join(DATA, "dns-idle-model.json")


def read(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def snap(path):
    s = read(path)
    s["_path"] = os.path.basename(path)
    return s


def empty_snap(taken=1000, uptime=3600, version="0.1.0+test"):
    """`/dns` と `/hosts` だけを持つ、その場で組む雪像。"""
    return {"taken_at": taken, "uptime_secs": uptime, "version": version, "_path": "x.json",
            "status": {"dns": {"ttl_secs": 60, "warm_secs": 0, "warm": 0, "refreshes": 0}},
            "dns": {"entries": [], "count": 0, "shown": 0, "truncated": False},
            "hosts": {"hosts": [], "count": 0, "shown": 0, "truncated": False}}


class NamesTest(unittest.TestCase):
    def test_the_ports_and_the_schemes_are_summed_per_name(self):
        s = empty_snap()
        s["hosts"]["hosts"] = [
            {"host": "connect://a.example.invalid:443", "requests": 5, "last_seen": 990},
            {"host": "http://a.example.invalid:8080", "requests": 7, "last_seen": 900},
            {"host": "connect://b.example.invalid:5228", "requests": 2, "last_seen": 800},
        ]
        r = di.requests_by_name(s)
        self.assertEqual(sorted(r), ["a.example.invalid", "b.example.invalid"])
        self.assertEqual(r["a.example.invalid"]["requests"], 12)
        # 最後の使用は遅い方
        self.assertEqual(r["a.example.invalid"]["last_seen"], 990)

    def test_ip_literals_and_other_are_not_names(self):
        s = empty_snap()
        s["hosts"]["hosts"] = [
            {"host": "connect://192.0.2.7:443", "requests": 9, "last_seen": 1},
            {"host": "other", "requests": 9, "last_seen": 1},
            {"host": "connect://[2001:db8::1]:443", "requests": 9, "last_seen": 1},
            {"host": "connect://a.example.invalid:443", "requests": 1, "last_seen": 1},
        ]
        self.assertEqual(list(di.requests_by_name(s)), ["a.example.invalid"])

    def test_dns_entries_are_keyed_by_the_lowercase_name(self):
        s = empty_snap()
        s["dns"]["entries"] = [{"host": "Mixed.Example.Invalid", "refreshes": 3}]
        self.assertEqual(list(di.dns_entries(s)), ["mixed.example.invalid"])


class PairTest(unittest.TestCase):
    def test_the_difference_is_read_per_name(self):
        w = di.pair_window(snap(A), snap(B))
        self.assertEqual(w["kind"], "pair")
        self.assertAlmostEqual(w["hours"], 1.0)
        self.assertEqual(w["refreshes_total"], 100)
        rows = {r["name"]: r for r in w["rows"]}
        self.assertEqual(rows["used.example.invalid"]["refreshes"], 30)
        self.assertEqual(rows["used.example.invalid"]["requests"], 40)
        self.assertEqual(rows["idle.example.invalid"]["refreshes"], 40)
        self.assertEqual(rows["idle.example.invalid"]["requests"], 0)
        self.assertAlmostEqual(rows["sparse.example.invalid"]["refreshes_per_request"], 1.5)

    def test_the_unused_refreshes_are_the_ones_with_no_requests(self):
        w = di.pair_window(snap(A), snap(B))
        self.assertEqual(w["unused"], 40)
        self.assertEqual(w["refreshes_judged"], 100)
        self.assertAlmostEqual(w["coverage"], 1.0)
        self.assertAlmostEqual(w["unused_share"], 0.40)
        self.assertAlmostEqual(w["unused_share_total"], 0.40)

    def test_a_name_missing_from_one_of_the_hosts_lists_is_not_judged(self):
        # `/hosts` は 1,000 件で切れる。片方にしか無い名前の `Δ要求` は読めない
        a, b = snap(A), snap(B)
        b["hosts"]["hosts"] = [h for h in b["hosts"]["hosts"]
                               if h["host"] != "connect://idle.example.invalid:443"]
        w = di.pair_window(a, b)
        rows = {r["name"]: r for r in w["rows"]}
        self.assertIsNone(rows["idle.example.invalid"]["requests"])
        self.assertEqual(w["unused"], 0)
        self.assertEqual(w["refreshes_judged"], 60)
        self.assertAlmostEqual(w["coverage"], 0.6)

    def test_a_row_rebuilt_in_the_window_is_skipped(self):
        a, b = snap(A), snap(B)
        b["dns"]["entries"][0]["refreshes"] = 10  # 差が負 (表の行が作り直された)
        w = di.pair_window(a, b)
        self.assertNotIn("used.example.invalid", [r["name"] for r in w["rows"]])


class VerdictTest(unittest.TestCase):
    def test_forty_percent_is_below_the_mark(self):
        d = di.verdict(di.pair_window(snap(A), snap(B)))
        self.assertEqual(d["basis"], "delta")
        self.assertFalse(d["go"])

    def test_eighty_percent_or_more_goes_to_step_two(self):
        # 使われていない引き直しが 9 割の窓をその場で組む
        rows = [{"refreshes": 90, "requests": 0}, {"refreshes": 10, "requests": 5}]
        win = {"kind": "pair", "refreshes_total": 100, "refreshes_judged": 100,
               "coverage": 1.0, "unused": 90, "unused_share": 0.9}
        self.assertTrue(di.verdict(win)["go"])
        self.assertEqual(len(rows), 2)

    def test_a_truncated_table_does_not_decide(self):
        a, b = snap(A), snap(B)
        b["dns"]["truncated"] = True
        b["dns"]["entries"] = b["dns"]["entries"][:1]
        win = di.pair_window(a, b)
        self.assertLess(win["coverage"], 0.5)
        self.assertIsNone(di.verdict(win))

    def test_keep_warm_zero_means_no_timer_refreshes(self):
        w = di.since_start_window(snap(B))
        d = di.verdict(w)
        self.assertEqual(d["basis"], "none")
        self.assertEqual(d["share"], 0.0)
        self.assertFalse(d["go"])

    def test_one_snapshot_with_keep_warm_cannot_decide(self):
        s = snap(B)
        s["status"]["dns"]["warm_secs"] = 3600
        s["status"]["dns"]["warm"] = 7
        self.assertIsNone(di.verdict(di.since_start_window(s)))

    def test_the_model_share_is_used_when_keep_warm_runs(self):
        s = snap(B)
        s["status"]["dns"]["warm_secs"] = 3600
        d = di.verdict(di.since_start_window(s), model_share=0.97, model_warm=3600)
        self.assertEqual(d["basis"], "timer")
        self.assertTrue(d["go"])


class WindowTest(unittest.TestCase):
    def test_the_uptime_and_the_clock_must_move_together(self):
        ok, why = di.same_process(snap(A), snap(B))
        self.assertTrue(ok, why)
        ok, why = di.same_process(snap(B), snap(C))
        self.assertFalse(ok)
        self.assertIn("version", why)

    def test_a_restart_becomes_a_single_snapshot_window(self):
        out = di.windows([snap(A), snap(B), snap(C)])
        self.assertEqual([w["kind"] for w in out], ["pair", "since_start"])
        self.assertIn("restart_reason", out[1])

    def test_a_refresh_counter_going_backwards_is_not_a_pair(self):
        a, b = snap(A), snap(B)
        b["status"]["dns"]["refreshes"] = 10
        out = di.windows([a, b])
        self.assertEqual(out[0]["kind"], "since_start")
        self.assertIn("went backwards", out[0]["restart_reason"])

    def test_the_last_single_snapshot_window_is_not_repeated(self):
        out = di.windows([snap(B), snap(C)])
        self.assertEqual([w["kind"] for w in out], ["since_start"])
        out = di.windows([snap(A), snap(B)])
        self.assertEqual([w["kind"] for w in out], ["pair", "since_start"])


class ModelTest(unittest.TestCase):
    def test_the_model_runs_both_k_and_ttl(self):
        s = snap(MODEL)
        hours = s["uptime_secs"] / 3600.0
        obs = {n: e["refreshes"] / hours for n, e in di.dns_entries(s).items()}
        m = di.model(s, sorted(obs.items(), key=lambda kv: -kv[1]))
        self.assertEqual(m["rules"]["k"]["refresh_at"], 45)
        self.assertEqual(m["rules"]["ttl"]["refresh_at"], 60)
        self.assertEqual(m["window"]["warm_secs"], 0)
        self.assertEqual(m["window"]["serve_stale_secs"], 21600)
        # k=45 の方が先に引き直すので、回数は多い
        self.assertGreater(m["rules"]["k"]["refreshes"], m["rules"]["ttl"]["refreshes"])
        # 名前ごとの回数が出る
        self.assertIn("dense.example.invalid", m["rules"]["k"]["names"])

    def test_the_observed_name_picks_k(self):
        s = snap(MODEL)
        hours = s["uptime_secs"] / 3600.0
        obs = {n: e["refreshes"] / hours for n, e in di.dns_entries(s).items()}
        c = di.model(s, sorted(obs.items(), key=lambda kv: -kv[1]))["compare"]
        self.assertEqual(c["pick"], "k")
        row = c["names"][0]
        self.assertEqual(row["name"], "dense.example.invalid")
        # 12 秒ごとの使用 = k=45 なら 48 秒ごと、TTL なら 60 秒ごと
        self.assertAlmostEqual(row["interval_k"], 48, delta=1)
        self.assertAlmostEqual(row["interval_ttl"], 60, delta=1)

    def test_a_name_with_few_arrivals_is_not_compared(self):
        s = snap(MODEL)
        obs = {"sparse.example.invalid": 5.0}
        c = di.model(s, sorted(obs.items(), key=lambda kv: -kv[1]))["compare"]
        self.assertIsNone(c)

    def test_keep_warm_hides_the_k_and_ttl_difference(self):
        s = snap(MODEL)
        s["status"]["dns"]["warm_secs"] = 3600
        hours = s["uptime_secs"] / 3600.0
        obs = {n: e["refreshes"] / hours for n, e in di.dns_entries(s).items()}
        c = di.model(s, sorted(obs.items(), key=lambda kv: -kv[1]))["compare"]
        self.assertEqual(c["pick"], "warm")


class CliTest(unittest.TestCase):
    def test_the_table_and_the_verdict_come_out(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(di.main([A, B, C]), 0)
        text = buf.getvalue()
        self.assertIn("| idle.example.invalid | 40 |", text)
        self.assertIn("使われていない引き直し", text)
        self.assertIn("段 2 は要らない", text)
        self.assertIn("起動が変わったので前の雪像とは比べない", text)

    def test_json_has_the_windows_and_the_verdict(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(di.main([A, B, "--json"]), 0)
        out = json.loads(buf.getvalue())
        self.assertEqual([w["kind"] for w in out["windows"]], ["pair", "since_start"])
        self.assertEqual(len(out["verdict"]), 2)
        key = "%d-%d" % (out["windows"][0]["from"], out["windows"][0]["to"])
        self.assertAlmostEqual(out["verdict"][key]["share"], 0.40)

    def test_model_is_optional(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(di.main([MODEL, "--json"]), 0)
        self.assertNotIn("model", json.loads(buf.getvalue()))
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(di.main([MODEL, "--json", "--model"]), 0)
        self.assertIn("model", json.loads(buf.getvalue()))

    def test_a_bad_top_and_a_missing_file_are_errors(self):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(di.main([A, "--top", "0"]), 2)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(di.main([os.path.join(DATA, "nope.json")]), 2)

    def test_a_snapshot_without_hosts_or_dns_is_not_a_crash(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(di.main([C]), 0)
        self.assertIn("dns-idle-c.json", buf.getvalue())


if __name__ == "__main__":
    unittest.main()
