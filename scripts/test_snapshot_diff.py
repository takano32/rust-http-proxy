#!/usr/bin/env python3
"""`scripts/snapshot-diff.py` (T14.17) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

架空の雪像 2 枚 (`testdata/snapshot-a.json` / `snapshot-b.json`) は **手で計算できる値**で
組んである (標本はバケツ 1 つに固め、`avg_ms` は差分が割り切れる数にしてある)。
デプロイ先の実出力 (リポジトリの `status/`) があるときだけ回るテストが最後に 1 本あり、
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
DEPLOYED = os.path.join(os.path.dirname(HERE), "status")


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


def with_warm_columns(snap, per_row):
    """`/history?res=3600` の末尾に T16.0 の 3 列を足す (`per_row` は `t` → (max, sum, n))。

    無い `t` の行は 0 (T16.0 より前に書かれた行と同じ形)。
    """
    h = snap["history"]["3600"]
    h["keys"] = h["keys"] + ["dns_warm_max", "dns_warm_sum", "gauge_n"]
    if h.get("key_kinds"):
        h["key_kinds"] = h["key_kinds"] + ["peak", "delta", "delta"]
    for row in h["samples"]:
        row.extend(per_row.get(row[0], (0, 0, 0)))
    return snap


class WarmMaxAndSums(unittest.TestCase):
    """T16.0: `dns_warm` の最大と、平均を Σsum / Σn で出すこと (無い版は今の平均)。"""

    def agg(self, per_row):
        b = with_warm_columns(read(B), per_row)
        with written(b=b) as paths:
            return build([A, paths["b"], "--no-dns"])["history"]["after"]

    def test_the_mean_comes_from_the_sums_and_the_max_survives(self):
        # 後の平常時は `dns_warm` 24 と 26 の 2 行 (行の平均は丸めてある)。
        # 合計は 24.4 × 720 と 26.2 × 720 = 真の平均 25.3 (行の平均の平均なら 25.0)。
        # バーストの行 (28、最大 40) は平常時の閾で外れる
        after = self.agg({1789045200: (30, 17568, 720), 1789048800: (31, 18864, 720),
                          1789052400: (40, 20160, 720)})
        self.assertAlmostEqual(after["dns_warm_avg"], 25.3)
        self.assertEqual(after["dns_warm_max"], 31)

    def test_rows_without_the_sums_count_as_one_sample(self):
        """T16.0 より前の行 (3 列が 0) は 1 行 = 1 本、最大は `dns_warm` を下限にする。"""
        after = self.agg({1789048800: (27, 18864, 720)})
        self.assertAlmostEqual(after["dns_warm_avg"], (24 + 18864) / 721)
        self.assertEqual(after["dns_warm_max"], 27)
        old = self.agg({})
        self.assertAlmostEqual(old["dns_warm_avg"], 25.0, msg="列が 0 なら今の平均のまま")
        self.assertEqual(old["dns_warm_max"], 26, msg="最大は dns_warm を下限にする")

    def test_a_snapshot_without_the_columns_has_no_max(self):
        self.assertIsNone(build([A, B, "--no-dns"])["history"]["after"]["dns_warm_max"])


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
        self.assertEqual(sorted(sd.CRITERIA), ["phase14", "phase15", "phase17"])


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

    上の `Deployed` は リポジトリの `status/` があるときだけ回るが、これは
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
