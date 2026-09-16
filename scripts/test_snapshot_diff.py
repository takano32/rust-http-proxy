#!/usr/bin/env python3
"""`scripts/snapshot-diff.py` (T14.17) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

架空の雪像 2 枚 (`testdata/snapshot-a.json` / `snapshot-b.json`) は **手で計算できる値**で
組んである (標本はバケツ 1 つに固め、`avg_ms` は差分が割り切れる数にしてある)。
デプロイ先の実出力 (`~/rust-http-proxy-status/`) があるときだけ回るテストが最後に 1 本あり、
**T14.0 の表と同じ数字が出ること**を見る (実データはリポジトリに入れないので、無ければ飛ばす)。
"""

import contextlib
import importlib.util
import io
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "testdata")
DEPLOYED = os.path.join(os.path.expanduser("~"), "rust-http-proxy-status")


def _load(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


sd = _load("snapshot_diff", "snapshot-diff.py")
pd = _load("proxydata", "proxydata.py")

A = os.path.join(DATA, "snapshot-a.json")
B = os.path.join(DATA, "snapshot-b.json")
AAAA = os.path.join(DATA, "aaaa.json")


def run(argv):
    """Markdown を文字列で受け取る。"""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        sd.main(argv)
    return out.getvalue()


def build(argv=None):
    """`build()` の結果 (辞書) を受け取る。"""
    args = sd.parser().parse_args(argv or [A, B])
    files = args.files + args.from_files
    a, b = (sd.load_source(f, f in args.from_files) for f in files)
    return sd.build(a, b, args)


def read(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


class Restart(unittest.TestCase):
    def test_detects_a_restart_from_version_and_uptime(self):
        d = build()
        r = d["restart"]
        self.assertTrue(r["restarted"])
        self.assertTrue(any("版が変わった" in x for x in r["reasons"]))
        self.assertTrue(any("`uptime_secs` が減った" in x for x in r["reasons"]))
        # 起動 = 後の雪像の取得時刻 − uptime
        self.assertEqual(r["started_at"], 1789086400 - 43200)
        self.assertEqual(r["boundary"], r["started_at"])
        self.assertEqual(r["wall_secs"], 86400)

    def test_without_a_restart_the_edge_is_the_first_snapshot(self):
        a = sd.load_source(A, False)
        b = read(B)
        b["version"] = a["version"]
        b["uptime_secs"] = a["uptime_secs"] + 86400
        b["status"]["version"] = a["version"]
        b["status"]["uptime_secs"] = b["uptime_secs"]
        b["status"]["since_start_secs"] = b["uptime_secs"]
        b = sd.normalize(b, "b")
        r = sd.restart_info(a, b)
        self.assertFalse(r["restarted"], r["reasons"])
        self.assertEqual(r["boundary"], a["taken_at"])

    def test_a_long_gap_in_uptime_is_a_restart_even_at_the_same_version(self):
        a = sd.load_source(A, False)
        b = read(B)
        b["version"] = a["version"]
        b["uptime_secs"] = a["uptime_secs"] + 60  # 24 時間の窓に 60 秒しか進んでいない
        b["status"]["version"] = a["version"]
        b["status"]["uptime_secs"] = b["uptime_secs"]
        b["status"]["since_start_secs"] = b["uptime_secs"]
        r = sd.restart_info(a, sd.normalize(b, "b"))
        self.assertTrue(r["restarted"])
        self.assertIn("足りない", r["reasons"][0])


class History(unittest.TestCase):
    def setUp(self):
        self.h = build()["history"]

    def test_splits_the_samples_at_the_restart(self):
        self.assertEqual(self.h["res"], "3600")
        self.assertEqual(self.h["samples"], 7)
        self.assertEqual(self.h["limit"], 300)
        self.assertEqual((self.h["before"]["samples"], self.h["before"]["burst_samples"]), (3, 1))
        self.assertEqual((self.h["after"]["samples"], self.h["after"]["burst_samples"]), (2, 1))

    def test_connect_quantiles_use_the_same_interpolation_as_history_rs(self):
        bef, aft = self.h["before"], self.h["after"]
        # 前: 300 本すべて [10,25) ms、最大 24 -> p50 = 10 + 15×0.50、p95 は ms_max で頭打ち
        self.assertEqual(bef["connects"], 300)
        self.assertAlmostEqual(bef["connect_avg"], 20.0)
        self.assertAlmostEqual(bef["connect_p50"], 17.5)
        self.assertAlmostEqual(bef["connect_p95"], 24.0)
        # 後: 400 本すべて [2,5) ms -> p50 = 2 + 3×0.50、p95 = 2 + 3×0.95
        self.assertEqual(aft["connects"], 400)
        self.assertAlmostEqual(aft["connect_avg"], 4.0)
        self.assertAlmostEqual(aft["connect_p50"], 3.5)
        self.assertAlmostEqual(aft["connect_p95"], 4.85)

    def test_the_quantile_helper_clamps_at_ms_max(self):
        bounds = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000]
        buckets = [0] * 13
        buckets[4] = 100                       # [10,25) に 100 本
        self.assertAlmostEqual(pd.quantile_ms(buckets, 100, 24, 0.5, bounds), 17.5)
        self.assertAlmostEqual(pd.quantile_ms(buckets, 100, 24, 0.95, bounds), 24.0)
        self.assertIsNone(pd.quantile_ms(buckets, 0, 0, 0.5, bounds))

    def test_dns_rates_come_out_per_connect_and_per_miss(self):
        bef, aft = self.h["before"], self.h["after"]
        self.assertAlmostEqual(bef["dns_per_connect"], 0.50)
        self.assertAlmostEqual(bef["ms_per_miss"], 12.0)
        self.assertAlmostEqual(bef["dns_ms_per_connect"], 6.0)
        self.assertAlmostEqual(aft["dns_per_connect"], 0.05)
        self.assertAlmostEqual(aft["ms_per_miss"], 11.0)
        self.assertAlmostEqual(aft["dns_ms_per_connect"], 0.55)

    def test_bursts_change_the_answer_so_they_are_kept_separate(self):
        # T14.0 の「(参考) バースト込み」の行: ミス 1 回が 12.0 -> 31.2 ms に化ける
        all_before = self.h["before_all"]
        self.assertEqual(all_before["connects"], 700)
        self.assertAlmostEqual(all_before["ms_per_miss"], 31.2)
        self.assertAlmostEqual(all_before["connect_avg"], 180.0)
        self.assertEqual(all_before["active_max"], 210)
        self.assertEqual(all_before["errors"], 5)

    def test_falls_back_to_res60_and_scales_the_burst_threshold(self):
        a = sd.load_source(A, False)
        b = sd.load_source(B, False)
        del a["history"]["3600"], b["history"]["3600"]
        h = sd.history_split(a, b, sd.restart_info(a, b), sd.BURST_PER_HOUR)
        self.assertEqual(h["res"], "60")
        self.assertEqual(h["limit"], 5)        # 1 時間 300 本 = 60 秒 5 本

    def test_the_newer_snapshot_wins_when_both_have_the_same_window(self):
        a = sd.load_source(A, False)
        b = sd.load_source(B, False)
        rows, _, _, interval = sd.merged_history(a, b, "3600")
        self.assertEqual(interval, 3600)
        self.assertEqual(len(rows), 7)         # 前 4 本は両方にあるが 1 本ずつに畳まれる
        self.assertEqual([r["t"] for r in rows], sorted(r["t"] for r in rows))


class Hosts(unittest.TestCase):
    def setUp(self):
        self.d = build([A, B, "--aaaa", AAAA])
        self.rows = {r["name"]: r for r in self.d["hosts"]["rows"]}

    def test_host_deltas_match_the_hand_calculation(self):
        alpha = self.rows["alpha.example.jp"]
        self.assertEqual(alpha["requests"], 200)
        self.assertAlmostEqual(alpha["avg_ms"], 4.0)       # (9.0×1200 − 10.0×1000) / 200
        self.assertAlmostEqual(alpha["avg_err"], 0.05 * 2200 / 200)
        self.assertEqual(alpha["dns_misses"], 2)
        self.assertAlmostEqual(pd.per_miss(alpha), 15.0)   # 30 ms / 2 回
        self.assertAlmostEqual(pd.per_conn(alpha), 5.0)    # 1,000 ms / 200 本
        beta = self.rows["beta.example.jp"]
        self.assertAlmostEqual(beta["avg_ms"], 12.0)       # (26.4×500 − 30.0×400) / 100
        self.assertEqual(beta["errors"], 3)

    def test_a_host_that_only_the_new_snapshot_has_counts_from_zero(self):
        zeta = self.rows["zeta.example.jp"]
        self.assertEqual(zeta["requests"], 30)
        self.assertAlmostEqual(zeta["avg_ms"], 20.0)
        self.assertEqual(self.d["hosts"]["gone"], 1)       # old.example.jp は消えた

    def test_major_hosts_skip_forward_rows_and_the_overflow_bucket(self):
        # `other` は Δ500 で最上位だが主要ホストではない。forward の delta も外す
        self.assertEqual([m["name"] for m in self.d["majors"]],
                         ["alpha.example.jp", "beta.example.jp", "gamma.example.net"])
        self.assertAlmostEqual(self.d["majors"][1]["miss_rate"], 0.10)

    def test_major_hosts_can_be_named(self):
        d = build([A, B, "--no-dns", "--major-hosts", "beta.example.jp,zeta.example.jp"])
        self.assertEqual([m["name"] for m in d["majors"]],
                         ["beta.example.jp", "zeta.example.jp"])

    def test_aaaa_groups_get_their_own_medians(self):
        groups = {g["label"]: g for g in sd.aaaa_groups(
            [r for r in self.d["hosts"]["rows"] if r["connect"]])}
        self.assertAlmostEqual(groups["AAAA あり"]["avg_median"], 4.5)   # alpha 4.0、gamma 5.0
        self.assertAlmostEqual(groups["AAAA なし"]["avg_median"], 12.0)  # beta
        self.assertAlmostEqual(groups["AAAA 不明"]["avg_median"], 20.0)  # zeta (表に無い)

    def test_a_rebuilt_rrd_is_called_out(self):
        a = sd.load_source(A, False)
        b = read(B)
        b["hosts"]["restored_since"] = a["taken_at"] + 60   # A を取った後に作り直された
        b = sd.normalize(b, "b")
        hs = sd.host_diff(a, b, "none", {})
        self.assertIsNotNone(hs["rrd_reset"])
        self.assertTrue(hs["rrd_reset"]["after_a"])
        self.assertIn("作り直されている", sd_render(a, b, hs))


def sd_render(a, b, hs):
    d = build([A, B, "--no-dns"])
    d["hosts"] = hs
    d["restart"] = sd.restart_info(a, b)
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        sd.render(d, 20)
    return out.getvalue()


class Clients(unittest.TestCase):
    def setUp(self):
        self.d = build([A, B, "--no-dns"])

    def test_client_deltas_and_the_new_one(self):
        rows = {r["client"]: r for r in self.d["clients"]["rows"]}
        old = rows["192.0.2.10"]
        self.assertEqual(old["requests"], 200)
        self.assertAlmostEqual(old["avg_ms"], 15.0)        # (19.8×5000 − 20.0×4800) / 200
        self.assertFalse(old["new"])
        new = rows["198.51.100.7"]
        self.assertTrue(new["new"])
        self.assertEqual(new["requests"], 300)
        self.assertEqual([r["client"] for r in self.d["clients"]["new"]], ["198.51.100.7"])

    def test_a_client_that_was_already_there_but_first_seen_later_is_new(self):
        a = sd.load_source(A, False)
        b = sd.load_source(B, False)
        b["clients"]["clients"][0]["first_seen"] = a["taken_at"] + 1
        self.assertEqual(len(sd.client_diff(a, b)["new"]), 2)


class Dns(unittest.TestCase):
    def setUp(self):
        self.dn = build([A, B, "--no-dns"])["dns"]

    def test_warm_and_refreshes(self):
        self.assertEqual(self.dn["warm"], 3)
        self.assertEqual(self.dn["warm_secs"], 900)
        self.assertEqual(self.dn["refreshes"], 24)         # 再起動をまたぐので「起動から」
        self.assertFalse(self.dn["windowed"])
        self.assertEqual(self.dn["top_refreshed"],
                         [("alpha.example.jp", 18), ("beta.example.jp", 6)])
        self.assertEqual(self.dn["warm_in_table"], 2)

    def test_idle_buckets_match_the_t140_table(self):
        self.assertEqual(self.dn["idle"],
                         {"60 秒未満": 1, "10 分未満": 1, "1 時間未満": 1, "1 時間以上": 2})

    def test_counters_are_subtracted_when_there_was_no_restart(self):
        a = sd.load_source(A, False)
        b = sd.load_source(B, False)
        b["status"]["dns"]["refreshes"] = a["status"]["dns"]["refreshes"] + 7
        dn = sd.dns_info(a, b, restarted=False)
        self.assertEqual(dn["refreshes"], 7)
        self.assertTrue(dn["windowed"])


class EventsErrorsBursts(unittest.TestCase):
    def setUp(self):
        self.d = build([A, B, "--no-dns"])

    def test_only_the_events_inside_the_window(self):
        ev = self.d["events"]
        self.assertEqual(ev["count"], 2)
        self.assertEqual([e["kind"] for e in ev["between"]], ["config"])

    def test_no_events_endpoint_is_said_so(self):
        a = sd.load_source(A, False)
        self.assertIsNone(sd.events_between(a, a))

    def test_error_causes_from_hosts_and_from_the_records(self):
        er = self.d["errors"]
        self.assertEqual(er["hosts_total"], 5)
        self.assertEqual(dict(zip(pd.CAUSE_NAMES, er["hosts_causes"]))["dns"], 2)
        self.assertEqual(dict(zip(pd.CAUSE_NAMES, er["hosts_causes"]))["reset"], 2)
        self.assertEqual(dict(zip(pd.CAUSE_NAMES, er["hosts_causes"]))["tls"], 1)
        # 個票は窓の外 (A を取る前) の 1 件を落とす
        self.assertEqual(er["recorded"], 3)
        self.assertEqual(len(er["in_window"]), 2)
        self.assertEqual(er["in_window_causes"], {"dns": 1, "reset": 1})

    def test_bursts_come_from_the_shots_and_from_the_history(self):
        bu = self.d["bursts"]
        self.assertEqual(bu["shots"], 2)
        self.assertEqual(bu["burst_windows"], [1, 1])
        self.assertEqual(bu["active_max"], [210, 120])


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


class Output(unittest.TestCase):
    def test_markdown_has_all_nine_sections(self):
        md = run([A, B, "--aaaa", AAAA, "--criteria", "phase14"])
        for i, title in enumerate(("再起動", "平常時の前後", "ホスト別", "接続元別", "名前解決",
                                   "その間の出来事", "エラーの原因別", "バースト", "完了の定義"), 1):
            self.assertIn(f"## {i}. {title}", md)
        self.assertIn("**p50 3.5**", md)
        self.assertIn("**新しく現れた接続元 1 件**", md)
        self.assertIn("届かず", md)

    def test_json_output_is_machine_readable(self):
        d = json.loads(run([A, B, "--no-dns", "--criteria", "phase14", "--out", "json"]))
        self.assertEqual(d["restart"]["restarted"], True)
        self.assertAlmostEqual(d["history"]["after"]["connect_p50"], 3.5)

    def test_the_order_of_the_two_snapshots_does_not_matter(self):
        self.assertEqual(run([A, B, "--no-dns"]), run([B, A, "--no-dns"]))


class FromFiles(unittest.TestCase):
    def test_reads_the_pre_snapshot_shape_from_a_prefix(self):
        b = read(B)
        with tempfile.TemporaryDirectory() as tmp:
            pre = os.path.join(tmp, "2026-09-11T0026Z")
            for suffix, body in (("status", b["status"]),
                                 ("history_res_3600", b["history"]["3600"]),
                                 ("hosts_sort_requests_limit_1000", b["hosts"]),
                                 ("dns_sort_misses_limit_300", b["dns"]),
                                 ("errors_n_500", b["errors"])):
                with open(f"{pre}-{suffix}", "w", encoding="utf-8") as f:
                    json.dump(body, f)
            with open(pre + "-metrics", "w", encoding="utf-8") as f:
                f.write("# JSON ではない (読み飛ばすこと)\n")
            snap = sd.load_source(pre, True)
        self.assertEqual(snap["taken_at"], 1789086360)      # 名前の 2026-09-11T0026Z
        self.assertEqual(snap["version"], "0.1.0+bbbbbbb")
        self.assertEqual(snap["uptime_secs"], 43200)
        self.assertEqual(sorted(snap["parts"]),
                         ["dns", "errors", "history.3600", "hosts", "status"])
        self.assertEqual(len(snap["hosts"]["hosts"]), 6)

    def test_prefers_the_requests_sorted_hosts_file(self):
        self.assertEqual(sd.classify("hosts_sort_requests_limit_1000"), "hosts")
        self.assertEqual(sd.preferred("hosts", "hosts_sort_requests_limit_1000"), 0)
        self.assertEqual(sd.preferred("hosts", "hosts_sort_errors_limit_1000"), 1)
        self.assertEqual(sd.classify("status_sort_dns"), "status_dns")
        self.assertEqual(sd.classify("status_sort_slow"), None)
        self.assertEqual(sd.classify("metrics"), None)
        self.assertEqual(sd.classify("history_res_60"), "history.60")

    def test_a_plain_status_json_is_enough_for_the_host_diff(self):
        b = read(B)
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "2026-09-11T0026Z-status.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(b["status"], f)
            snap = sd.load_source(path, False)
        self.assertEqual(snap["taken_at"], 1789086360)
        self.assertEqual(len(snap["hosts"]["hosts"]), 6)
        self.assertEqual(len(snap["clients"]["clients"]), 2)


@unittest.skipUnless(os.path.isdir(DEPLOYED) and
                     os.path.isfile(os.path.join(DEPLOYED, "2026-09-16T0106Z-status")),
                     "デプロイ先の実出力が無い (リポジトリには入れない)")
class Deployed(unittest.TestCase):
    """T14.17 の受け入れ基準: 実出力から **T14.0 の表と同じ数字**が出ること。"""

    def setUp(self):
        self.d = build(["--from-files", os.path.join(DEPLOYED, "2026-09-12T2018Z"),
                        "--from-files", os.path.join(DEPLOYED, "2026-09-16T0106Z"),
                        "--no-dns", "--criteria", "phase14"])

    def test_the_numbers_are_the_ones_in_t140(self):
        h = self.d["history"]
        bef, aft = h["before"], h["after"]
        self.assertEqual((bef["samples"], aft["samples"]), (58, 72))
        self.assertEqual(f"{bef['dns_per_connect']:.2f}", "0.55")
        self.assertEqual(f"{aft['dns_per_connect']:.2f}", "0.55")
        self.assertEqual(f"{bef['connect_p50']:.1f}", "8.1")
        self.assertEqual(f"{aft['connect_p50']:.1f}", "8.3")
        self.assertEqual(f"{bef['ms_per_miss']:.1f}", "12.6")
        self.assertEqual(f"{aft['ms_per_miss']:.1f}", "11.5")
        self.assertEqual(f"{bef['connect_avg']:.1f}", "20.9")
        self.assertEqual(f"{aft['connect_avg']:.1f}", "15.3")
        self.assertEqual(f"{bef['connect_p95']:.1f}", "90.8")
        self.assertEqual(f"{aft['connect_p95']:.1f}", "80.7")
        # 「(参考) バースト込み」の行 (§2 の 3 行目)
        self.assertEqual(f"{h['before_all']['dns_per_connect']:.2f}", "0.42")
        self.assertEqual(f"{h['before_all']['ms_per_miss']:.1f}", "36.0")

    def test_the_major_three_hosts_and_their_miss_rates(self):
        # **宛先の名前と接続元の IP はここに書かない** (個人の閲覧先なのでリポジトリに入れない)。
        # 数字だけで T14.0 の「主要 3 ホストのミス率」の行を守る
        majors = self.d["majors"]
        self.assertEqual(len(majors), 3)
        self.assertEqual(sorted(m["requests"] for m in majors), [489, 920, 2301])
        self.assertEqual(sorted(f"{m['miss_rate']:.2f}" for m in majors),
                         ["0.31", "0.76", "1.00"])

    def test_the_new_client_of_2026_09_16(self):
        self.assertEqual(len(self.d["clients"]["new"]), 1)
        self.assertEqual(self.d["clients"]["new"][0]["requests"], 463)

    def test_the_dns_table_and_the_refreshes(self):
        dn = self.d["dns"]
        self.assertEqual(dn["refreshes"], 442)
        self.assertEqual(dn["idle"],
                         {"60 秒未満": 2, "10 分未満": 1, "1 時間未満": 3, "1 時間以上": 84})

    def test_the_phase14_table_has_four_rows(self):
        c = self.d["criteria"]
        self.assertEqual(len(c["rows"]), 4)
        self.assertEqual([r[3] for r in c["rows"]],
                         [sd.MISSED, sd.MISSED, sd.MISSED, sd.UNKNOWN])


if __name__ == "__main__":
    unittest.main()
