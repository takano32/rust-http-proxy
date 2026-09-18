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


class ServerSummary(unittest.TestCase):
    """サーバー側の要約 (`/history?...&summary=1`。T14.24) を読む口。"""

    def setUp(self):
        self.d = build()
        self.after = self.d["history"]["after"]

    def server_row(self, **over):
        """手元の集計と同じ数字を持つ、サーバー側の応答 (架空)。"""
        row = {
            "from": self.d["history"]["boundary"], "to": 1789086400,
            "interval_secs": 3600, "normal_hours_only": True,
            "samples": self.after["samples"], "burst_samples": self.after["burst_samples"],
            "connects": self.after["connects"],
            "p50_ms": self.after["connect_p50"], "p95_ms": self.after["connect_p95"],
            "dns_miss_per_connect": self.after["dns_per_connect"],
            "dns_miss_avg_ms": self.after["ms_per_miss"],
            "errors": self.after["errors"], "active_max": self.after["active_max"],
        }
        row.update(over)
        return row

    def test_the_url_carries_the_period_and_the_normal_hours_filter(self):
        url = self.d["summary_url"]
        self.assertIn(f"since={self.d['history']['boundary']}", url)
        self.assertIn("res=3600", url)
        self.assertIn("normal_hours_only=1", url)
        self.assertIn("summary=1", url)
        self.assertNotIn("normal", sd.summary_url(1, 2, 60, normal=False))

    def test_the_same_numbers_come_out_as_a_match(self):
        check = sd.summary_check(self.server_row(), self.after)
        self.assertTrue(all(r["same"] for r in check["rows"]), check["rows"])

    def test_a_different_window_shows_up_as_a_mismatch(self):
        check = sd.summary_check(self.server_row(p50_ms=99.9), self.after)
        bad = [r["key"] for r in check["rows"] if r["same"] is False]
        self.assertEqual(bad, ["p50_ms"])

    def test_a_source_that_cannot_be_read_is_reported_not_raised(self):
        out = sd.load_summary(os.path.join(DATA, "no-such-summary.json"))
        self.assertIn("error", out)
        self.assertIsNone(sd.load_summary(None))
        self.assertIsNone(sd.summary_check(out, self.after))

    def test_the_markdown_shows_the_one_request_url_and_the_table(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "summary.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(self.server_row(), f)
            md = run([A, B, "--no-dns", "--summary", path])
        self.assertIn("サーバー側で 1 要求", md)
        self.assertIn("| サーバー (`?summary=1`) | 手元 (この道具) |", md)
        self.assertIn("一致", md)


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

    def test_group_domain_folds_the_hosts_into_etld1(self):
        """`--group domain` (T14.54)。`alpha` と `beta` と `zeta` が `example.jp` の 1 行。"""
        d = build([A, B, "--aaaa", AAAA, "--group", "domain"])
        rows = {(r["name"], r["connect"]): r for r in d["hosts"]["rows"]}
        jp = rows[("example.jp", True)]
        self.assertEqual(jp["hosts"], 3, "alpha + beta + zeta")
        self.assertEqual(jp["requests"], 200 + 100 + 30)
        self.assertEqual(jp["errors"], 3)
        self.assertEqual(jp["dns_misses"], 2 + 10 + 15)
        # avg は計測数で重みづけ ((4.0×200 + 12.0×100 + 20.0×30) / 330)
        self.assertAlmostEqual(jp["avg_ms"], (4.0 * 200 + 12.0 * 100 + 20.0 * 30) / 330)
        self.assertIsNone(jp["p50_ms"], "分位点は足せない")
        self.assertIsNone(jp["aaaa"], "AAAA が混ざっている単位は「不明」")
        # `.net` は別の単位、forward の `.org` も別の行、`other` はそのまま
        self.assertEqual(rows[("example.net", True)]["hosts"], 1)
        self.assertEqual(rows[("example.org", False)]["requests"], 30)
        self.assertEqual(rows[("other", False)]["requests"], 500)
        self.assertEqual(d["hosts"]["group"], "domain")
        self.assertEqual(d["hosts"]["hosts"], 6, "まとめる前のホスト数")
        # Markdown にも粒度が出る
        md = run([A, B, "--aaaa", AAAA, "--group", "domain"])
        self.assertIn("**eTLD+1 でまとめた**", md)
        self.assertIn("`example.jp` (3 ホスト)", md)

    def test_the_default_keeps_one_row_per_host(self):
        self.assertEqual(self.d["hosts"]["group"], "host")
        self.assertIn("alpha.example.jp", self.rows)
        self.assertNotIn("example.jp", self.rows)

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

    def test_the_since_start_columns_are_blank_for_a_client_last_seen_before_the_restart(self):
        """T14.99: `distinct_targets` / `agent` は `.rrd` に残らない**起動からの**欄。

        窓が再起動をまたぐと、再起動より前にしか居ない接続元は「Δ要求 200 なのに
        宛先の種類 0・User-Agent は前の起動のもの」になり、同じ行で時間軸が食い違う。
        """
        b = read(B)
        # この接続元を最後に見たのは再起動 (取得 − uptime) より前
        b["clients"]["clients"][0]["last_seen"] = b["taken_at"] - b["uptime_secs"] - 1
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "b.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(b, f)
            md = run([A, path, "--no-dns"])
        self.assertIn("| 宛先の種類 (起動から) |", md)
        self.assertIn("再起動より後に一度も見ていない接続元は", md)
        self.assertRegex(md, r"\| `192\.0\.2\.10` \| 200 \|[^\n]*\| — \| — \|")
        # 再起動の後にも見ている接続元はそのまま出る
        self.assertIn("| `198.51.100.7` **新** | 300 |", md)
        self.assertIn("| 3 | Mozilla/5.0 (fictional) |", md)


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
        self.assertEqual([e["kind"] for e in ev["between"]], ["reload"])

    def test_no_events_endpoint_is_said_so(self):
        a = sd.load_source(A, False)
        self.assertIsNone(sd.events_between(a, a))

    def test_the_text_of_an_event_is_printed(self):
        """T14.99: サーバーが出す鍵は `text` (`events.rs` の `Event::to_json`)。

        `what` / `msg` だけを読んでいたので §6 の「中身」が全行 `—` になっていた。
        """
        md = run([A, B, "--no-dns"])
        self.assertIn("| reload | reload: PROXY_DNS_WARM_SECS 300 -> 900 |", md)
        self.assertNotIn("| reload | — |", md)

    def test_an_old_snapshot_with_what_still_prints(self):
        """手で組んだ古い雪像 (`what`) も読めること (保険の分岐)。"""
        b = read(B)
        e = b["events"]["events"][0]
        b["events"]["events"][0] = {"at": e["at"], "kind": e["kind"], "what": "hand made"}
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "b.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(b, f)
            md = run([A, path, "--no-dns"])
        self.assertIn("| reload | hand made |", md)

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

    def test_the_shot_prints_the_active_at_the_moment_it_crossed(self):
        """T14.99: 写真に `peak` という欄は無い (`active` / `threshold` / `max_conns`)。

        `s.get('peak')` を読んでいたので §8 が「(山 —)」になっていた。写真は
        **閾を越えた瞬間**の 1 枚なので、山 (`active_max`) と混ぜない。
        """
        md = run([A, B, "--no-dns"])
        self.assertIn("(越えた瞬間 120 本 / 閾 120・上限 240)", md)
        self.assertNotIn("(山 —)", md)
        self.assertIn("その時間帯の山は下の `active_max`", md)

    def test_the_byte_unit_matches_the_divisor(self):
        """T14.99: 1,024 で割るなら KiB / MiB / GiB (画面の `fmtBytes` と同じ)。

        `GB` / `MB` / `kB` と書いていたので、報告のバイト数が 2.4〜7.4% 小さい
        十進の量に読めていた (§4 の Δバイト は README / §2 に写す数字)。
        """
        self.assertEqual(pd.fmt_bytes(1 << 20), "1.0 MiB")
        self.assertEqual(pd.fmt_bytes(-(1 << 30)), "-1.0 GiB")
        self.assertEqual(pd.fmt_bytes(1536), "1.5 KiB")
        self.assertEqual(pd.fmt_bytes(999), "999 B")

    def test_an_old_snapshot_with_peak_still_prints(self):
        """手で組んだ古い雪像 (`peak`) も読めること (保険の分岐)。"""
        self.assertEqual(sd.shot_text({"at": 1789052400, "peak": 99}),
                         "2026-09-10 15:00:00Z (越えた瞬間 99 本)")


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


@contextlib.contextmanager
def written(**snaps):
    """書き換えた雪像を一時ディレクトリに置いて、そのパスを返す (`test_snapshot_summary.py` と同じ)。"""
    with tempfile.TemporaryDirectory() as tmp:
        paths = {}
        for name, body in snaps.items():
            paths[name] = os.path.join(tmp, name + ".json")
            with open(paths[name], "w", encoding="utf-8") as f:
                json.dump(body, f)
        yield paths


def set_column(snap, res, name, value):
    """`/history` の 1 列を全標本で書き換える (**位置ではなく `keys` の名前で引く**)。"""
    h = snap["history"][res]
    i = h["keys"].index(name)
    for row in h["samples"]:
        row[i] = value


class NewColumns(unittest.TestCase):
    """T15.0 (10) で末尾に足した 8 列 (`HFIELDS` は名前で引く)。"""

    def test_the_wait_columns_are_aggregated_like_connect(self):
        h = build([A, B, "--no-dns"])["history"]
        # 後の平常時 2 標本 × 200 本、合計 3,200 ms、[5,10) ms のバケツ
        self.assertEqual(h["after"]["waits"], 400)
        self.assertAlmostEqual(h["after"]["wait_avg"], 8.0)
        self.assertAlmostEqual(h["after"]["wait_p50"], 7.5)
        self.assertEqual(h["after"]["wait_ms_max"], 12)

    def test_the_gauge_is_averaged_and_the_peak_is_maxed(self):
        h = build([A, B, "--no-dns"])["history"]
        self.assertAlmostEqual(h["after"]["dns_warm_avg"], 25.0)   # 24 と 26
        self.assertEqual(h["after"]["active_peak"], 7)
        self.assertEqual(h["after"]["requests_delta"], 410)
        self.assertEqual(h["after"]["bytes_delta"], 410000)

    def test_an_old_snapshot_leaves_them_empty_instead_of_crashing(self):
        """欄の無い雪像 (A) でも読めること。`dns_warm` は `None` (0 と混ぜない)。"""
        a = sd.load_source(A, False)
        rows, bounds, _c, _i = sd.merged_history(a, a, "3600")
        agg = sd.aggregate(rows, bounds, None)
        self.assertIsNone(agg["dns_warm_avg"])
        self.assertEqual((agg["waits"], agg["active_peak"]), (0, 0))
        self.assertIsNone(agg["wait_buckets"])


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

    def test_the_refresh_rate_is_per_warm_name_per_hour(self):
        """**通算の平均で割らない** (実勢を 35〜60% 過小に見せる)。"""
        row = self.judge()["rows"][1]
        self.assertEqual(row[3], sd.MET)
        self.assertIn("24 回 ÷ 12.0 時間 ÷ warm 25.0 件", row[2])

    def test_too_many_refreshes_per_name_is_missed(self):
        b = read(B)
        b["status"]["dns"]["refreshes"] = 30000      # 30,000 ÷ 12 時間 ÷ 25 件 = 100 回/時
        with written(b=b) as paths:
            row = self.judge(b=paths["b"])["rows"][1]
        self.assertEqual(row[3], sd.MISSED)

    def test_without_dns_warm_the_refresh_rate_cannot_be_judged(self):
        b = read(B)
        for res in b["history"]:
            i = b["history"][res]["keys"].index("dns_warm")
            b["history"][res]["keys"][i] = "dns_warm_x"   # 名前が違えば「無い」
        with written(b=b) as paths:
            row = self.judge(b=paths["b"])["rows"][1]
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`dns_warm`", row[4])

    def test_a_miss_rate_below_the_band_is_missed_not_met(self):
        """**低すぎても見込み違い** (幅は「どこに落ち着くか」の予想)。"""
        row = self.judge()["rows"][2]
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**幅より低い**", row[2])

    def test_a_miss_rate_inside_the_band_is_met(self):
        b = read(B)
        set_column(b, "3600", "dns_misses", 14)      # 平常時 2 標本 × 14 ÷ 400 本 = 0.07
        with written(b=b) as paths:
            row = self.judge(b=paths["b"])["rows"][2]
        self.assertEqual(row[3], sd.MET)
        self.assertIn("**0.07** 回/接続", row[2])

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

    def recent(self, n_rows, idle, half, old=False, http=0):
        """閉じた接続 `n_rows` 本のうち `idle` 本が `idle_timeout`、`half` 本が半閉じ。

        `old=True` で **T15.0 (4) より前の版** (`half_closed` の欄そのものが無い)、
        `http` で forward の行 (`kind` が `http`) を後ろに足す。
        """
        rows = []
        for i in range(n_rows):
            r = {"id": i, "at": 1789000000, "secs": 10, "kind": "connect",
                 "reason": "idle_timeout" if i < idle else "client_eof"}
            if not old:
                r["half_closed"] = "client" if i < half else None
            rows.append(r)
        for i in range(http):
            r = {"id": 10000 + i, "at": 1789000000, "secs": 1, "kind": "http",
                 "reason": "idle_timeout"}
            if not old:
                r["half_closed"] = None
            rows.append(r)
        n = n_rows + http
        return {"recent": rows, "count": n, "shown": n, "truncated": False}

    def test_the_closed_shares_are_ratios_not_counts(self):
        """**本数は窓の長さで変わる** (雪像 1 枚に入るのは最後に閉じた N 本)。"""
        a, b = read(A), read(B)
        a["recent"] = self.recent(100, 20, 10)       # 0.20 / 0.10
        b["recent"] = self.recent(600, 120, 60)      # 同じ割合、本数は 6 倍
        with written(a=a, b=b) as paths:
            row = self.judge(a=paths["a"], b=paths["b"])["rows"][4]
        self.assertEqual(row[3], sd.MET)
        self.assertIn("`idle_timeout` 0.20 → 0.20 (+0%)", row[2])
        self.assertIn("100 本 → 600 本", row[4])

    def test_a_changed_share_is_missed(self):
        a, b = read(A), read(B)
        a["recent"] = self.recent(100, 20, 10)
        b["recent"] = self.recent(100, 40, 10)       # `idle_timeout` が 2 倍
        with written(a=a, b=b) as paths:
            row = self.judge(a=paths["a"], b=paths["b"])["rows"][4]
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("(+100%)", row[2])

    def test_without_recent_the_shares_cannot_be_judged(self):
        row = self.judge()["rows"][4]                # B の `recent` は落ちている (`dropped`)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("`/recent` の部が無い", row[4])

    def test_an_old_snapshot_has_no_half_closed_field_at_all(self):
        """**最初の前後比べ (再デプロイ前 × 後) が必ずこの形**になる (T15.0 (15) のレビュー)。

        前の版に `half_closed` の欄が無いのを 0.00 と読むと、半閉じが増えた形になって
        `change(0.10, 0)` が `None` を返し、直しが効いていても「届かず」と書かれる。
        """
        a, b = read(A), read(B)
        a["recent"] = self.recent(100, 20, 0, old=True)   # 欄そのものが無い版
        b["recent"] = self.recent(100, 20, 10)            # 10% が半閉じ
        with written(a=a, b=b) as paths:
            row = self.judge(a=paths["a"], b=paths["b"])["rows"][4]
        self.assertEqual(row[3], sd.MET)                  # `idle_timeout` だけで判定する
        self.assertIn("`idle_timeout` 0.20 → 0.20 (+0%)", row[2])
        self.assertIn("半閉じ —**前の版にその欄は無い**", row[2])
        self.assertNotIn("0.00", row[2])

    def test_the_newer_side_missing_the_field_is_skipped_too(self):
        a, b = read(A), read(B)
        a["recent"] = self.recent(100, 20, 10)
        b["recent"] = self.recent(100, 20, 0, old=True)
        with written(a=a, b=b) as paths:
            row = self.judge(a=paths["a"], b=paths["b"])["rows"][4]
        self.assertEqual(row[3], sd.MET)
        self.assertIn("半閉じ —**後の版にその欄は無い**", row[2])

    def test_only_the_connect_rows_are_counted(self):
        """母数は CONNECT だけ。**http の混ざり具合**で割合が動いてはいけない。"""
        a, b = read(A), read(B)
        a["recent"] = self.recent(100, 20, 10)            # http 0 本
        b["recent"] = self.recent(100, 20, 10, http=100)  # 同じ CONNECT + http 100 本
        with written(a=a, b=b) as paths:
            row = self.judge(a=paths["a"], b=paths["b"])["rows"][4]
        self.assertEqual(row[3], sd.MET)
        self.assertIn("`idle_timeout` 0.20 → 0.20 (+0%)", row[2])
        self.assertIn("100 本 → 100 本", row[4])          # 200 本ではない
        self.assertIn("**CONNECT だけ**", row[4])

    def test_a_snapshot_with_only_http_rows_cannot_be_judged(self):
        a, b = read(A), read(B)
        a["recent"] = self.recent(100, 20, 10)
        b["recent"] = self.recent(0, 0, 0, http=50)
        with written(a=a, b=b) as paths:
            row = self.judge(a=paths["a"], b=paths["b"])["rows"][4]
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("CONNECT の行が 1 本も無い", row[4])

    def test_the_markdown_says_the_missing_field_is_skipped(self):
        md = run([A, B, "--no-dns", "--criteria", "phase15"])
        self.assertIn("**CONNECT の行だけ**", md)
        self.assertIn("その欄が無い項目も同じ", md)

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
        self.assertEqual(sorted(sd.CRITERIA), ["phase14", "phase15"])


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


class Schema(unittest.TestCase):
    """応答の形の版 (`schema`。T14.49)。

    **版の無い古い出力 (版 0) が今までどおり読める**ことと、版 1 の出力を推測抜きで
    読めることの両方を見る (`testdata/snapshot-{a,b}.json` は版の無い形のまま置いてある)。
    """

    def test_the_version_of_a_json_without_schema_is_zero(self):
        self.assertEqual(pd.schema_of(read(A)), 0)
        self.assertEqual(pd.schema_of({}), 0)
        self.assertEqual(pd.schema_of({"schema": 3}), 3)
        # `true` は 1 ではない (Python の bool は int の仲間なので念のため)
        self.assertEqual(pd.schema_of({"schema": True}), 0)
        self.assertEqual(pd.schema_of("文字列"), 0)

    def test_a_snapshot_is_recognised_in_both_versions(self):
        old = read(A)                                   # 版の無い古い出力
        self.assertTrue(pd.is_snapshot(old))
        new = dict(old, schema=1)                       # 版 1
        self.assertTrue(pd.is_snapshot(new))
        # 版 1 は `parts` だけで決める (`/status` は `parts` を持たない)
        self.assertFalse(pd.is_snapshot({"schema": 1, "status": "ok", "hosts": []}))
        # 版の無い `/status` も雪像ではない (`hosts` が配列)
        self.assertFalse(pd.is_snapshot(read(A)["status"]))

    def test_unwrap_takes_the_hosts_part_in_both_versions(self):
        for snap in (read(A), dict(read(A), schema=1)):
            inner = pd.unwrap(snap)
            self.assertEqual(len(inner["hosts"]), len(snap["hosts"]["hosts"]))
            self.assertEqual(inner["snapshot_taken_at"], snap["taken_at"])
        # 雪像でない JSON はそのまま返る
        st = read(A)["status"]
        self.assertIs(pd.unwrap(st), st)

    def test_load_source_reads_both_versions(self):
        with tempfile.TemporaryDirectory() as tmp:
            for name, body in (("old.json", read(A)),
                               ("new.json", dict(read(A), schema=1))):
                path = os.path.join(tmp, name)
                with open(path, "w", encoding="utf-8") as f:
                    json.dump(body, f)
                snap = sd.load_source(path, False)
                self.assertEqual(snap["version"], "0.1.0+aaaaaaa")
                self.assertEqual(len(snap["hosts"]["hosts"]), 6)
            # 版 1 の `/status` 1 枚も推測せずに読める
            path = os.path.join(tmp, "2026-09-11T0026Z-status.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(dict(read(B)["status"], schema=1), f)
            snap = sd.load_source(path, False)
            self.assertEqual(snap["schema"], 1)
            self.assertEqual(len(snap["hosts"]["hosts"]), 6)

    def test_the_report_prints_the_version_of_both_snapshots(self):
        md = run([A, B, "--no-dns"])
        self.assertIn("- 形の版 `schema` 0 → 0 (0 = 版を持たない古い出力", md)
        d = json.loads(run([A, B, "--no-dns", "--out", "json"]))
        self.assertEqual((d["a"]["schema"], d["b"]["schema"]), (0, 0))
        with tempfile.TemporaryDirectory() as tmp:
            paths = []
            for name, src in (("a.json", A), ("b.json", B)):
                path = os.path.join(tmp, name)
                with open(path, "w", encoding="utf-8") as f:
                    json.dump(dict(read(src), schema=1), f)
                paths.append(path)
            d = json.loads(run(paths + ["--no-dns", "--out", "json"]))
        self.assertEqual((d["a"]["schema"], d["b"]["schema"]), (1, 1))

    def test_a_newer_version_is_still_read_with_one_warning(self):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            v = pd.warn_newer({"schema": pd.SCHEMA + 1}, "future.json")
        self.assertEqual(v, pd.SCHEMA + 1)
        self.assertIn("版 2 の出力です", err.getvalue())
        # 知っている版なら何も言わない
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            pd.warn_newer({"schema": pd.SCHEMA}, "now.json")
        self.assertEqual(err.getvalue(), "")


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


@unittest.skipUnless(os.path.isfile(os.path.join(DATA, "deployed-2026-09-16.anon.json")),
                     "匿名化した実データが無い")
class Anonymized(unittest.TestCase):
    """T14.35 の匿名化した実データ (`testdata/deployed-2026-09-16.anon.json`) で回る。

    上の `Deployed` は `~/rust-http-proxy-status/` があるときだけ回るが、これは
    **リポジトリの中の実データ** (ホスト名と IP と UA だけを置き換えた雪像) なのでいつでも回る。
    """

    def test_the_anonymized_snapshot_gives_the_numbers_of_t140(self):
        snap = sd.load_source(os.path.join(DATA, "deployed-2026-09-16.anon.json"), False)
        self.assertEqual((snap["taken_at"], snap["uptime_secs"]), (1789520760, 261833))
        self.assertEqual(len(snap["hosts"]["hosts"]), 817)
        self.assertEqual(len(snap["dns"]["entries"]), 90)
        # ホスト別の読み方 (`proxydata.row_of`) が実データの形で回る
        rows = [pd.row_of(h["host"], h, None) for h in snap["hosts"]["hosts"]]
        self.assertEqual(sum(r["requests"] for r in rows), 18153)
        top = max(rows, key=lambda r: r["requests"])
        self.assertEqual((top["requests"], top["connect"]), (4427, True))
        self.assertRegex(top["name"], r"^host-\d{4}\.example$")
        # `/history?res=3600` を起動時刻で切った平常時 = T14.0 の表の「あと」の列
        started = snap["taken_at"] - snap["uptime_secs"]
        hrows, bounds, _causes, interval = sd.merged_history(snap, snap, "3600")
        limit = max(1, round(sd.BURST_PER_HOUR * interval / 3600.0))
        agg = sd.aggregate([r for r in hrows if r["t"] >= started], bounds, limit)
        self.assertEqual((agg["samples"], agg["burst_samples"], agg["errors"]), (72, 0, 0))
        self.assertEqual(f"{agg['dns_per_connect']:.2f}", "0.55")
        self.assertEqual(f"{agg['connect_p50']:.1f}", "8.3")
        self.assertEqual(f"{agg['connect_p95']:.1f}", "80.7")
        self.assertEqual(f"{agg['connect_avg']:.1f}", "15.3")
        self.assertEqual(f"{agg['ms_per_miss']:.1f}", "11.5")


if __name__ == "__main__":
    unittest.main()
