#!/usr/bin/env python3
"""`scripts/snapshot-diff.py` (T.14.17) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

架空の雪像 2 枚 (`testdata/snapshot-a.json` / `snapshot-b.json`) は **手で計算できる値**で
組んである (標本はバケツ 1 つに固め、`avg_ms` は差分が割り切れる数にしてある)。
デプロイ先の実出力 (リポジトリの `status/`) があるときだけ回るテストが最後に 1 本あり、
**T.14.0 の表と同じ数字が出ること**を見る (実データはリポジトリに入れないので、無ければ飛ばす)。
**判定表 (`--criteria`) のテストは `test_snapshot_criteria.py`** に割ってある (T.21.2。この
ファイルの小物 `sd` `build` `run` `written` … をそちらが取り込む)。
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

    def test_dns_rate_without_failures_only_when_the_column_exists(self):
        # T.32.17: `dns_fail_misses` の無い版 (この見本) は鍵ごと足さない (今までどおり読める)
        self.assertNotIn("dns_ok_per_connect", self.h["after"])
        rows = [dict({f: None for f in sd.HFIELDS}, t=i, connects=1000, connect_ms_sum=0, dns_misses=950,
                     dns_ms_sum=57_500, dns_fail_misses=902, dns_fail_ms_sum=55_000, errors_by_cause=[])
                for i in range(2)]
        agg = sd.aggregate(rows, [1, 2], None)
        self.assertAlmostEqual(agg["dns_per_connect"], 0.95)
        self.assertAlmostEqual(agg["dns_ok_per_connect"], 0.048)
        self.assertAlmostEqual(agg["dns_ok_ms_per_connect"], 2.5)
        # 待ちの欄 (`dns_fail_ms_sum`。T.32.16) が無くても率は出す
        for r in rows:
            r["dns_fail_ms_sum"] = None
        agg = sd.aggregate(rows, [1, 2], None)
        self.assertAlmostEqual(agg["dns_ok_per_connect"], 0.048)
        self.assertNotIn("dns_ok_ms_per_connect", agg)

    def test_bursts_change_the_answer_so_they_are_kept_separate(self):
        # T.14.0 の「(参考) バースト込み」の行: ミス 1 回が 12.0 -> 31.2 ms に化ける
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
    """サーバー側の要約 (`/history?...&summary=1`。T.14.24) を読む口。"""

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
        """`--group domain` (T.14.54)。`alpha` と `beta` と `zeta` が `example.jp` の 1 行。"""
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
        """T.14.99: `distinct_targets` / `agent` は `.rrd` に残らない**起動からの**欄。

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
        """T.14.99: サーバーが出す鍵は `text` (`events.rs` の `Event::to_json`)。

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
        """T.14.99: 写真に `peak` という欄は無い (`active` / `threshold` / `max_conns`)。

        `s.get('peak')` を読んでいたので §8 が「(山 —)」になっていた。写真は
        **閾を越えた瞬間**の 1 枚なので、山 (`active_max`) と混ぜない。
        """
        md = run([A, B, "--no-dns"])
        self.assertIn("(越えた瞬間 120 本 / 閾 120・上限 240)", md)
        self.assertNotIn("(山 —)", md)
        self.assertIn("その時間帯の山は下の `active_max`", md)

    def test_the_byte_unit_matches_the_divisor(self):
        """T.14.99: 1,024 で割るなら KiB / MiB / GiB (画面の `fmtBytes` と同じ)。

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


class DnsTable(unittest.TestCase):
    """§5 の「最後に使ってから」(T.32.31): `/dns` の一覧が切れていれば表全体とは書かない。"""

    def test_a_truncated_list_says_only_the_oldest_side(self):
        b = read(B)
        whole = len(b["dns"]["entries"])
        with written(b=b) as paths:
            md = run([A, paths["b"], "--no-dns"])
        self.assertIn(f"(表 {whole} 件中、warm 印つき", md)
        b["dns"].update({"count": 7511, "shown": whole, "sort": "age", "truncated": True, "next_offset": whole})
        with written(b=b) as paths:
            md = run([A, paths["b"], "--no-dns"])
        self.assertIn(f"(表 7,511 件のうち古い側の {whole} 件だけを見た。warm 印つき", md)
        self.assertNotIn(f"(表 {whole} 件中", md)


T3199_PAIR = [os.path.join(DEPLOYED, "tokyo", x) for x in ("2026-10-30T062500Z-snapshot.json",
                                                            "2026-10-31T063104Z-snapshot.json")]


@unittest.skipUnless(all(os.path.isfile(x) for x in T3199_PAIR), "tokyo の実データが無い (status/tokyo/ の 2 枚)")
class DeployedDnsTable(unittest.TestCase):
    """T.32.31 (1): 0.30.4 の 1 枚の `/dns` は 7,511 件のうち古い側の 4,763 件で止まっている。"""

    def test_t3199_dns_table(self):
        md = run(T3199_PAIR + ["--no-dns"])
        self.assertIn("(表 7,511 件のうち古い側の 4,763 件だけを見た。", md)


class NewColumns(unittest.TestCase):
    """T.15.0 (10) で末尾に足した 8 列 (`HFIELDS` は名前で引く)。"""

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
    """`/history?res=3600` の末尾に T.16.0 の 3 列を足す (`per_row` は `t` → (max, sum, n))。

    無い `t` の行は 0 (T.16.0 より前に書かれた行と同じ形)。
    """
    h = snap["history"]["3600"]
    h["keys"] = h["keys"] + ["dns_warm_max", "dns_warm_sum", "gauge_n"]
    if h.get("key_kinds"):
        h["key_kinds"] = h["key_kinds"] + ["peak", "delta", "delta"]
    for row in h["samples"]:
        row.extend(per_row.get(row[0], (0, 0, 0)))
    return snap


class WarmMaxAndSums(unittest.TestCase):
    """T.16.0: `dns_warm` の最大と、平均を Σsum / Σn で出すこと (無い版は今の平均)。"""

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
        """T.16.0 より前の行 (3 列が 0) は 1 行 = 1 本、最大は `dns_warm` を下限にする。"""
        after = self.agg({1789048800: (27, 18864, 720)})
        self.assertAlmostEqual(after["dns_warm_avg"], (24 + 18864) / 721)
        self.assertEqual(after["dns_warm_max"], 27)
        old = self.agg({})
        self.assertAlmostEqual(old["dns_warm_avg"], 25.0, msg="列が 0 なら今の平均のまま")
        self.assertEqual(old["dns_warm_max"], 26, msg="最大は dns_warm を下限にする")

    def test_a_snapshot_without_the_columns_has_no_max(self):
        self.assertIsNone(build([A, B, "--no-dns"])["history"]["after"]["dns_warm_max"])


def with_recent(snap, connects=(), failed=0, others=0, oldest=None, truncated=False):
    """`/recent` の部を足す: 成功した CONNECT (`ms.connect` を `connects` の値で)、失敗した CONNECT
    (`error:unreachable`、1,300 ms) を `failed` 本、CONNECT ではない行を `others` 本。"""
    at = snap["taken_at"] - 60
    rows = [{"at": at, "kind": "connect", "reason": "client_eof", "status": 0,
             "ms": {"dns": 0, "connect": v, "first_relay": v + 1}} for v in connects]
    rows += [{"at": at, "kind": "connect", "reason": "error:unreachable", "status": 502,
              "ms": {"dns": 0, "connect": 1300, "first_relay": 0}} for _ in range(failed)]
    rows += [{"at": at, "kind": "http", "reason": "client_eof", "status": 200,
              "ms": {"dns": 0, "connect": 999, "first_relay": 0}} for _ in range(others)]
    if oldest is not None and rows:
        rows[-1]["at"] = oldest
    snap["recent"] = {"schema": 1, "recent": rows, "count": len(rows), "truncated": truncated}
    snap["parts"] = list(snap.get("parts") or []) + ["recent"]
    snap["dropped"] = [x for x in snap.get("dropped") or [] if x != "recent"]
    return snap


class RecentConnect(unittest.TestCase):
    """差分の「2. 平常時の前後」に足した**成功した接続だけ**の確立時間 (T.20.1 (3))。

    `/history` の確立の区間と `recent_quantiles.connect` には失敗した接続の時間も入るので、
    後の雪像の `/recent` から `reason` が `error` で始まらない CONNECT だけを読む。
    """

    ROW = "| CONNECT 確立 (**成功した接続だけ**) |"

    def test_rank_quantile_returns_one_of_the_values(self):
        self.assertIsNone(sd.rank_quantile([], 0.5))
        self.assertEqual(sd.rank_quantile([7], 0.95), 7)
        vals = list(range(1, 21))                            # 1〜20
        self.assertEqual(sd.rank_quantile(vals, 0.5), 10)
        self.assertEqual(sd.rank_quantile(vals, 0.95), 19)
        self.assertEqual(sd.rank_quantile(vals, 1.0), 20)

    def test_only_the_connects_that_did_not_fail_are_counted(self):
        b = with_recent(read(B), connects=[9, 5, 7, 6, 8, 66, 7, 7, 7, 7], failed=4, others=2,
                        oldest=read(B)["taken_at"] - 7200)
        with written(b=b) as p:
            d = build([A, p["b"], "--no-dns"])
            md = run([A, p["b"], "--no-dns"])
        rc = d["recent_connect"]
        self.assertEqual((rc["n"], rc["p50"], rc["p95"], rc["failed"], rc["rows"]),
                         (10, 7, 66, 4, 16))
        self.assertEqual(rc["taken_at"] - rc["oldest"], 7200)
        self.assertFalse(rc["truncated"])
        line = next(x for x in md.splitlines() if x.startswith(self.ROW))
        self.assertIn("| **p50 7** / p95 66 ms (10 本。ほかに失敗 4 本) |", line)
        self.assertIn("個票 16 行、2026-09-10 22:26:40Z → 取得までの 2.0 時間)", line)
        self.assertNotIn("切れている", line)
        # 表の中の位置は `/history` の「CONNECT 確立」のすぐ下
        lines = md.splitlines()
        self.assertTrue(lines[lines.index(line) - 1].startswith("| CONNECT 確立 | avg "))

    def test_a_row_without_the_connect_stage_is_left_out(self):
        b = with_recent(read(B), connects=[5, 9])
        b["recent"]["recent"].append({"at": b["taken_at"], "kind": "connect",
                                      "reason": "idle_timeout", "ms": None})
        with written(b=b) as p:
            rc = build([A, p["b"], "--no-dns"])["recent_connect"]
        self.assertEqual((rc["n"], rc["p50"], rc["p95"], rc["rows"]), (2, 5, 9, 3))

    def test_a_truncated_part_says_so(self):
        with written(b=with_recent(read(B), connects=[5], truncated=True)) as p:
            md = run([A, p["b"], "--no-dns", "--criteria", "phase20"])
        self.assertIn("。**256 KiB で切れている**ので窓は短い) |", md)

    def test_without_the_recent_part_the_row_says_so(self):
        self.assertIsNone(build([A, B, "--no-dns"])["recent_connect"])
        md = run([A, B, "--no-dns"])
        self.assertIn(self.ROW + " — | — | 後の雪像に `/recent` の部が無い |", md)

    def test_all_failed_gives_no_quantile(self):
        with written(b=with_recent(read(B), failed=3)) as p:
            md = run([A, p["b"], "--no-dns"])
        self.assertIn("| **p50 —** / p95 — ms (0 本。ほかに失敗 3 本) |", md)

    def errors(self, per_sample):
        """後の期間の 1 時間の標本すべてに `unreachable` のエラーを `per_sample` 件ずつ置く。"""
        b = read(B)
        causes = [0] * len(pd.CAUSE_NAMES)
        causes[pd.CAUSE_NAMES.index("unreachable")] = per_sample
        set_column(b, "3600", "errors", per_sample)
        set_column(b, "3600", "errors_by_cause", causes)
        return b

    def test_many_errors_add_a_note_under_the_table(self):
        """B の後の期間の平常時は 2 標本 400 本。エラー 30 件ずつ = 60 件 = 15.0%。"""
        with written(b=self.errors(30), few=self.errors(10)) as p:
            md = run([A, p["b"], "--no-dns"])
            few = run([A, p["few"], "--no-dns"])
        self.assertIn("**確立の区間には失敗した接続の時間も入っている**: 後の期間 (平常時) のエラーは "
                      "60 件で、接続 400 本の 15.0% (> 5%。原因別: unreachable 60)。", md)
        # ちょうど 5% (20 ÷ 400) は「超える」ではない
        self.assertNotIn("確立の区間には失敗した接続の時間も入っている", few)

    def test_the_older_criteria_print_exactly_what_they_did(self):
        """phase14〜phase18 の出力は 1 文字も変えない: `/recent` があっても行も断り書きも鍵も足さない。"""
        b = with_recent(self.errors(30), connects=[5, 6, 7], failed=2)
        plain = self.errors(30)
        plain["dropped"] = []                                # B は `recent` を落とした雪像
        with written(b=b, plain=plain) as p:
            for name in sd.FROZEN:
                argv = ["--no-dns", "--criteria", name]
                md = run([A, p["b"], *argv])
                self.assertNotIn("成功した接続だけ", md, name)
                self.assertNotIn("確立の区間には失敗した", md, name)
                # `/recent` の部があっても無くても同じ文字 (違うのはファイルの名前だけ)
                self.assertEqual(md.replace(p["b"], p["plain"]), run([A, p["plain"], *argv]), name)
                self.assertNotIn("recent_connect", build([A, p["b"], *argv]), name)
            self.assertIn("成功した接続だけ", run([A, p["b"], "--no-dns", "--criteria", "phase20"]))
        self.assertEqual(sd.FROZEN, ("phase14", "phase15", "phase17", "phase18"))


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
    """応答の形の版 (`schema`。T.14.49)。

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


# **T.32.34 で消した `Deployed` (5 本)**: T.14.17 の受け入れ基準 (実出力から T.14.0 の表と同じ数字) を、`status/` 直下の
# nagoya の生の出力 (`2026-09-12T2018Z`・`2026-09-16T0106Z`) で見ていたが、その出力はどの機械にも残っていない。
# 同じ 2026-09-16 の実データは匿名化した写し (`testdata/deployed-2026-09-16.anon.json`) で下の `Anonymized` が回す。
@unittest.skipUnless(os.path.isfile(os.path.join(DATA, "deployed-2026-09-16.anon.json")),
                     "匿名化した実データが無い")
class Anonymized(unittest.TestCase):
    """T.14.35 の匿名化した実データ (`testdata/deployed-2026-09-16.anon.json`) で回る。

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
        # `/history?res=3600` を起動時刻で切った平常時 = T.14.0 の表の「あと」の列
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


ANON_0926 = os.path.join(DATA, "deployed-2026-09-26.anon.json")
ANON_0926_DAILY = os.path.join(DATA, "deployed-2026-09-26-daily.anon.json")
ANON_0926_PROFILE = os.path.join(DATA, "deployed-2026-09-26-profile_res_60.anon.json")


@unittest.skipUnless(all(os.path.isfile(p) for p in (ANON_0926, ANON_0926_DAILY, ANON_0926_PROFILE)),
                     "匿名化した実データ (2026-09-26) が無い")
class Anonymized0926(unittest.TestCase):
    """T.17.16 の匿名化した実データ (`testdata/deployed-2026-09-26.anon.json` と隣の 2 つ) で回る。

    `recent` / `profile` / `events` / `hosts_series` / T.16.0 の 3 列のある版 (`0.1.0+2e57626`、起動から
    47.2 時間)。「前」は 2026-09-16 の fixture (別に匿名化したので名前の番号は前後で合わない。判定表の
    行はどれも名前に依らない数か、名前で引くものは「判定できず」になる)。
    **数字は T.16.99 の `結果:` と同じ** (`Deployed17` が `status/` の生の雪像で見ているものを、匿名化の
    あとでもリポジトリの中だけで見る)。
    """

    def rows(self, criteria, *extra):
        return build([os.path.join(DATA, "deployed-2026-09-16.anon.json"), ANON_0926, "--no-dns",
                      "--criteria", criteria, "--daily", ANON_0926_DAILY, *extra]
                     )["criteria"]["rows"]

    def test_the_parts_of_the_current_version_are_there(self):
        snap = sd.load_source(ANON_0926, False)
        self.assertEqual((snap["version"], snap["uptime_secs"]), ("0.1.0+2e57626", 169783))
        for name in ("recent", "profile", "events", "hosts_series"):
            self.assertIn(name, snap["parts"])
            self.assertTrue(snap[name], name)
        # T.16.0 の 3 列 (履歴の `dns_warm` の和と最大)
        keys = snap["history"]["3600"]["keys"]
        self.assertEqual(keys[-3:], ["dns_warm_max", "dns_warm_sum", "gauge_n"])

    def test_the_phase15_table(self):
        rows = self.rows("phase15")
        self.assertEqual([r[3] for r in rows],
                         [sd.UNKNOWN, sd.MET, sd.MET, sd.MET, sd.UNKNOWN, sd.MET])
        # 見張る名前 (`discord.com`) は匿名化で消えているので、引けないと言う
        self.assertIn("`discord.com` が無い", rows[0][4])
        self.assertIn("**0.06** 回/接続", rows[2][2])

    def test_the_phase17_table_gives_the_numbers_of_t1699(self):
        rows = self.rows("phase17", "--profile", ANON_0926_PROFILE)
        self.assertEqual(len(rows), 8)
        self.assertEqual([r[3] for r in rows],
                         [sd.UNKNOWN, sd.MET, sd.MET, sd.MET, sd.UNKNOWN, sd.MET, sd.MISSED,
                          sd.UNKNOWN])
        # 前の fixture (2026-09-16) に `/profile?res=60` が無いので倍率は出ない (値だけ)
        self.assertIn("1,038 us/要求、プロセス全体 0.0013 コア", rows[4][2])
        self.assertIn("最大 **15** 件、`warm_evicted` **0**", rows[5][2])
        self.assertIn("`dns_slow` **0.61** 件/時 (29 件", rows[6][2])
        self.assertIn("後 **0.0022 コア、user 31%**", rows[7][2])


class RttSplit(unittest.TestCase):
    """T.29.18: `/recent` を接続元の RTT (100 ms) で 2 集団に割った表 (確立・first_relay・理由・宛先の種類)。"""

    @staticmethod
    def row(i, rtt, connect, relay, target="a.example:443", reason="server_eof"):
        return {"id": i, "at": 1789080000 + i, "client": "192.0.2.1", "target": target, "kind": "connect",
                "reason": reason, "ms": {"dns": 0, "connect": connect, "first_relay": relay},
                "rtt_ms": {"client": rtt, "origin": 10.0}}

    def snap(self):
        b = read(B)
        rows = ([self.row(i, 20.0, 11, 20) for i in range(4)]
                + [self.row(10 + i, 248.0, 12, 4100, target="imap.example:993") for i in range(6)]
                + [{"id": 99, "at": 1789080100, "client": "192.0.2.1", "target": "nx.example:993",
                    "kind": "connect", "reason": "error:nxdomain", "ms": {"dns": 20, "connect": 0},
                    "rtt_ms": {"client": None, "origin": None}}])
        b["recent"] = {"recent": rows, "count": len(rows), "truncated": False}
        return b

    def test_the_two_groups_are_shown(self):
        with written(b=self.snap()) as paths:
            d = build([A, paths["b"], "--no-dns"])
            md = run([A, paths["b"], "--no-dns"])
        rs = d["rtt_split"]
        self.assertEqual((rs["lt100"]["rows"], rs["ge100"]["rows"], rs["none"]["rows"]), (4, 6, 1))
        self.assertEqual((rs["lt100"]["relay_p50"], rs["ge100"]["relay_p50"]), (20, 4100))
        self.assertEqual(rs["ge100"]["ports"], [("993", 6)])
        self.assertIn("### 接続元の RTT 別", md)
        self.assertIn("| < 100 ms (近い) | 4 | 20.0 ms | 11.0 / 11.0 ms (4) | 20.0 / 20.0 ms (4) |", md)
        self.assertIn("| ≥ 100 ms (遠い) | 6 | 248.0 ms |", md)
        self.assertIn("| RTT なし | 1 | — |", md)

    def test_the_frozen_definitions_do_not_get_the_table(self):
        with written(b=self.snap()) as paths:
            d = build([A, paths["b"], "--no-dns", "--criteria", "phase14"])
        self.assertNotIn("rtt_split", d)


TOKYO_ANON = os.path.join(DEPLOYED, "tokyo", "2026-10-27T024200Z-snapshot.anon.json")


@unittest.skipUnless(os.path.exists(TOKYO_ANON), "tokyo の実データが無い (status/tokyo/)")
class DeployedRttSplit(unittest.TestCase):
    """T.29.18 の受け入れ基準: tokyo の (匿名化した) 雪像で 2 行の表が出る (匿名化で RTT が残る)。"""

    def test_the_tokyo_snapshot_splits_into_two_groups(self):
        d = build([A, TOKYO_ANON, "--no-dns"])
        rs = d["rtt_split"]
        self.assertEqual((rs["lt100"]["rows"], rs["ge100"]["rows"]), (429, 1492))
        self.assertLess(rs["lt100"]["relay_p50"], 100)
        self.assertGreater(rs["ge100"]["relay_p50"], 1000)


class BusyHours(unittest.TestCase):
    """T.29.13: 毎時 500 本 (300 本の閾を全部の時間が越える) でも、`--burst-median-k 3` なら平常時が空にならない。

    tokyo の 0.28.48 は毎時 475〜1,760 本で、要約の §2 は「平常時 0 / 23 標本」だった。サーバーの
    `normal_hours_only=1` も T.29.13 から同じ規則 (300 本と中央値 × 3 の大きい方)。
    """

    def busy(self, *extra):
        b = read(B)
        set_column(b, "3600", "connects", 500)
        with written(b=b) as paths:
            return build([A, paths["b"], "--no-dns", *extra])["history"], run([A, paths["b"], "--no-dns", *extra])

    def test_three_hundred_alone_leaves_no_normal_hour(self):
        h, _ = self.busy()
        self.assertEqual((h["after"]["samples"], h["after"]["burst_samples"]), (0, 3))
        self.assertNotIn("limits", h)

    def test_the_median_rule_keeps_the_busy_hours(self):
        h, md = self.busy("--burst-median-k", "3")
        # 中央値 500 × 3 = 1,500 本 (前の期間も 500 本の時間ばかりなので同じ閾)
        self.assertEqual(h["limits"], {"before": 1500, "after": 1500})
        self.assertEqual((h["after"]["samples"], h["after"]["burst_samples"]), (3, 0))
        self.assertIn("(平常時 3 / 3 標本)", md)
        self.assertIn("(`--burst-median-k` の切り方。T.22.1: 300 本 (T.14.0) と、", md)


if __name__ == "__main__":
    unittest.main()
