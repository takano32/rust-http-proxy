#!/usr/bin/env python3
"""`scripts/weekly-report.py` (T14.40) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

受け入れ基準の本体は `SevenDays`: **匿名化した実データ (T14.35) の 1 日ぶんを 7 日に複製**
(日付をずらして 7 枚の雪像にする) して回し、表 8 つが出て、**日別の数字が
`snapshot-diff.py` (T14.17) の `aggregate()` の値と 1 つ残らず一致する**ことを見る。
実データそのもの (`testdata/deployed-2026-09-16.anon.json`) は `/history?res=3600` に
7 日ぶん (2026-09-10 〜 09-16) 入っているので、1 枚でも 7 日の表が出る。
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
ANON = os.path.join(DATA, "deployed-2026-09-16.anon.json")
LOCAL = os.path.join(DATA, "snapshot-local.json")


def _load(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


wr = _load("weekly_report", "weekly-report.py")
sd = _load("snapshot_diff", "snapshot-diff.py")

DAY = 86400
# 複製のもとにする 1 日 (24 標本、バースト無し、エラー 0 の平常な日)
SRC_DAY = "2026-09-15"


def read(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def run(argv):
    """Markdown を文字列で受け取る。"""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        wr.main(argv)
    return out.getvalue()


def build(argv):
    """`build()` の結果 (辞書) を受け取る。"""
    args = wr.parser().parse_args(argv)
    snaps, daily, _skipped = wr.load_inputs(wr.expand(args.inputs))
    return wr.build(snaps, daily, args)


# `.rrd` の通算 (`/hosts` `/clients`) で日ごとに増える欄。`avg_ms` は平均なので増やさない。
CUMULATIVE = ("requests", "timed", "bytes", "hits", "misses", "bypass", "errors", "blocked",
              "dns_ms_sum", "dns_misses", "connect_ms_sum", "v4_wins", "v6_wins")


def grow(row, times):
    """`.rrd` の通算を `times` 日ぶんにする (同じ 1 日が `times` 回あった形)。"""
    for key in CUMULATIVE:
        if isinstance(row.get(key), int):
            row[key] *= times
    if isinstance(row.get("errors_by_cause"), list):
        row["errors_by_cause"] = [v * times for v in row["errors_by_cause"]]
    return row


def one_day(base, k, drop_host=None, drop_client=None):
    """匿名化した実データの `SRC_DAY` 1 日ぶんを `k` 日ずらした雪像にする。

    T14.34 の日次の雪像と同じ形 (その 1 日を写したものが翌日 00:00 UTC に残る) にしたい
    ので、`taken_at` は**翌日の 00:00**。`/history?res=3600` はその日の 24 標本だけで、
    `/hosts` と `/status` の `clients[]` は `.rrd` の通算なので `k + 1` 日ぶんに増やす。
    """
    off = k * DAY
    lo = wr.day_start(SRC_DAY)
    d = json.loads(json.dumps(base))  # 深い複製
    h = d["history"]["3600"]
    h["samples"] = [[s[0] + off] + list(s[1:]) for s in h["samples"] if lo <= s[0] < lo + DAY]
    d["history"] = {"3600": h}
    d["taken_at"] = lo + DAY + off
    d["uptime_secs"] = d["status"]["uptime_secs"] = base["uptime_secs"] + off
    for row in d["hosts"]["hosts"] + d["status"]["clients"]:
        grow(row, k + 1)
        if row.get("last_seen"):
            row["last_seen"] += off
    d["hosts"]["hosts"] = [r for r in d["hosts"]["hosts"] if r["host"] != drop_host]
    d["status"]["clients"] = [c for c in d["status"]["clients"] if c["client"] != drop_client]
    return d


def week_of(tmp, count=7, drop_host=None, drop_client=None):
    """`SRC_DAY` を `count` 日に複製した雪像を `tmp` に書く (名前は T14.34 の流儀)。

    `drop_host` は**いちばん古い 1 枚だけ**から外す (2 枚目で「新しく見たホスト」になる)、
    `drop_client` は**2 枚目から**外す (1 日目で「来なくなった」になる)。
    """
    base = read(ANON)
    paths = []
    for k in range(count):
        day = wr.day_of(wr.day_start(SRC_DAY) + k * DAY)
        path = os.path.join(tmp, f"{day}T000000Z-snapshot.json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump(one_day(base, k, drop_host=drop_host if k == 0 else None,
                              drop_client=drop_client if k > 0 else None), f)
        paths.append(path)
    return paths


def src_day_agg(limit=True):
    """`snapshot-diff.py` の読み方で `SRC_DAY` 1 日ぶんを畳んだもの (答え合わせの相手)。"""
    snap = sd.load_source(ANON, False)
    rows, bounds, _causes, interval = sd.merged_history(snap, snap, "3600")
    lo = wr.day_start(SRC_DAY)
    rows = [r for r in rows if lo <= r["t"] < lo + DAY]
    lim = max(1, round(sd.BURST_PER_HOUR * interval / 3600.0)) if limit else None
    return sd.aggregate(rows, bounds, lim)


class Days(unittest.TestCase):
    def test_days_are_utc(self):
        self.assertEqual(wr.day_of(1789520760), "2026-09-16")   # 01:06 UTC
        self.assertEqual(wr.day_start("2026-09-16"), 1789516800)
        self.assertEqual(wr.day_of(wr.day_start("2026-09-16") + DAY - 1), "2026-09-16")

    def test_a_week_counts_back_from_the_last_day(self):
        self.assertEqual(wr.day_seq("2026-09-16", 7)[0], "2026-09-10")
        self.assertEqual(wr.day_seq("2026-09-16", 7)[-1], "2026-09-16")
        self.assertEqual(len(wr.day_seq("2026-09-16", 7)), 7)
        self.assertEqual(wr.day_seq("2026-03-01", 3), ["2026-02-27", "2026-02-28", "2026-03-01"])


class Inputs(unittest.TestCase):
    def test_a_directory_becomes_the_snapshots_in_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = week_of(tmp)
            self.assertEqual(wr.expand([tmp]), sorted(paths))

    def test_a_daily_response_is_read_as_days_not_as_a_snapshot(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "daily.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump({"days": [{"day": "2026-09-15", "requests": 7}], "count": 1}, f)
            snaps, daily, skipped = wr.load_inputs([path])
            self.assertEqual((snaps, skipped), ([], []))
            self.assertEqual(daily["2026-09-15"]["requests"], 7)

    def test_the_raw_jsonl_the_proxy_writes_is_read_too(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "daily.jsonl")
            with open(path, "w", encoding="utf-8") as f:
                f.write('{"day":"2026-09-14","requests":1}\n')
                f.write("{ こわれた行\n")          # 手で触られた行は読み飛ばす
                f.write('{"day":"2026-09-15","requests":2}\n')
            _snaps, daily, skipped = wr.load_inputs([path])
            self.assertEqual(skipped, [])
            self.assertEqual(sorted(daily), ["2026-09-14", "2026-09-15"])

    def test_a_file_that_is_neither_is_reported_not_raised(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "x.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump({"hello": 1}, f)
            snaps, daily, skipped = wr.load_inputs([path])
            self.assertEqual((snaps, daily), ([], {}))
            self.assertEqual(len(skipped), 1)

    def test_snapshots_are_ordered_oldest_first(self):
        with tempfile.TemporaryDirectory() as tmp:
            paths = week_of(tmp)
            snaps, _daily, _skipped = wr.load_inputs(list(reversed(paths)))
            self.assertEqual([s["label"] for s in snaps], paths)


class SevenDays(unittest.TestCase):
    """受け入れ基準: 1 日ぶんを 7 日に複製した入力から表 8 つが出て、数字が一致する。"""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        week_of(cls.tmp.name, drop_host="connect://host-0009.example:443",
                drop_client="198.51.100.2")
        cls.d = build([cls.tmp.name])
        cls.md = run([cls.tmp.name])

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_seven_days_and_seven_snapshots(self):
        self.assertEqual(len(self.d["days"]), 7)
        self.assertEqual(self.d["days"][0], SRC_DAY)
        self.assertEqual(self.d["missing"], [])
        self.assertEqual(len(self.d["snaps"]), 7)
        self.assertIn("**雪像 7 枚、7 日ぶん**", self.md)

    def test_all_eight_tables_are_there(self):
        for head in ("## 1. 要求数", "## 2. CONNECT 確立", "## 3. 名前解決のミス率",
                     "## 4. エラー", "## 5. 山", "## 6. 接続元の出入り",
                     "## 7. 遅かったホスト", "## 8. 新しく見たホスト"):
            self.assertIn(head, self.md)

    def test_every_day_is_the_same_as_snapshot_diff_aggregate(self):
        """**同じ `aggregate()` を使う**ので、日で切った標本の値がそのまま一致する。"""
        want = src_day_agg()
        want_all = src_day_agg(limit=False)
        for day in self.d["days"]:
            got = self.d["stats"][day]
            self.assertEqual(got["src"], "history")
            self.assertEqual(got["samples"], 24, day)
            for key in ("connects", "connect_avg", "connect_p50", "connect_p95",
                        "connect_ms_max", "forwards", "forward_p50", "forward_p95",
                        "dns_misses", "dns_per_connect", "ms_per_miss", "dns_ms_per_connect",
                        "burst_samples"):
                self.assertEqual(got["normal"][key], want[key], f"{day} {key}")
            self.assertEqual(got["all"]["errors"], want_all["errors"], day)
            self.assertEqual(got["all"]["causes"], want_all["causes"], day)
            self.assertEqual(got["all"]["active_max"], want_all["active_max"], day)

    def test_the_numbers_are_the_ones_of_the_source_day(self):
        want = src_day_agg()
        self.assertEqual(f"{want['connect_p50']:.1f}", "8.3")
        self.assertEqual(f"{want['connect_p95']:.1f}", "79.0")
        self.assertEqual(f"{want['dns_per_connect']:.2f}", "0.55")
        one = self.d["stats"][SRC_DAY]["normal"]
        self.assertEqual(f"{one['connect_p50']:.1f}", "8.3")
        self.assertIn("| **8.3** |", self.md)        # 7 日ぶん同じ日なので週も同じ p50

    def test_the_week_is_the_seven_days_added_up(self):
        week = self.d["week"]
        day = self.d["stats"][SRC_DAY]
        self.assertEqual(week["requests"], day["requests"] * 7)
        self.assertEqual(week["normal"]["connects"], day["normal"]["connects"] * 7)
        self.assertEqual(week["normal"]["dns_misses"], day["normal"]["dns_misses"] * 7)
        self.assertEqual(week["samples"], 24 * 7)
        # 同じ分布を 7 回足しただけなので分位点は動かない
        self.assertEqual(f"{week['normal']['connect_p50']:.1f}",
                         f"{day['normal']['connect_p50']:.1f}")

    def test_the_week_aggregate_is_the_same_as_aggregating_the_window_at_once(self):
        rows, bounds, _c, interval = wr.merged_history(
            [sd.load_source(p, False) for p in sorted(wr.expand([self.tmp.name]))], "3600")
        limit = max(1, round(sd.BURST_PER_HOUR * interval / 3600.0))
        self.assertEqual(self.d["week"]["normal"], sd.aggregate(rows, bounds, limit))

    def test_the_host_dropped_from_the_first_snapshot_shows_up_as_new(self):
        fresh = self.d["new_hosts"]
        self.assertEqual(fresh["pairs"], 6)
        # 2 枚目 (SRC_DAY の翌日を写したもの) で初めて見える
        got = {d: v for d, v in fresh["per_day"].items() if v}
        self.assertEqual(list(got), [wr.day_of(wr.day_start(SRC_DAY) + DAY)])
        self.assertEqual(got[list(got)[0]], ["connect://host-0009.example:443"])
        self.assertIn("`host-0009.example`", self.md)

    def test_the_client_dropped_from_the_later_snapshots_stopped_coming(self):
        # `198.51.100.2` は 1 枚目にしか居ないので、その日で「来なくなった」
        left = [e["client"] for mv in self.d["moves"].values() for e in mv["out"]]
        self.assertIn("198.51.100.2", left)
        self.assertIn("− `198.51.100.2`", self.md)

    def test_the_slow_hosts_are_ranked_by_avg_times_requests(self):
        rows = wr.slow_hosts(self.d["hosts"]["rows"], 10)
        self.assertEqual(len(rows), 10)
        self.assertEqual([r["total_ms"] for r in rows],
                         sorted((r["total_ms"] for r in rows), reverse=True))
        self.assertTrue(all(r["name"] != "other" for r in rows))
        for r in rows:
            self.assertAlmostEqual(r["total_ms"], r["avg_ms"] * r["requests"])

    def test_the_hosts_table_is_a_difference_because_there_are_seven_snapshots(self):
        self.assertTrue(self.d["hosts"]["windowed"])
        self.assertIn("いちばん古い雪像といちばん新しい雪像", self.md)


class OneSnapshot(unittest.TestCase):
    """実データ 1 枚 (`/history?res=3600` に 7 日ぶん入っている) でも 7 日の表が出る。"""

    @classmethod
    def setUpClass(cls):
        cls.d = build([ANON])
        cls.md = run([ANON])

    def test_seven_days_from_one_snapshot(self):
        self.assertEqual(self.d["days"], ["2026-09-10", "2026-09-11", "2026-09-12",
                                          "2026-09-13", "2026-09-14", "2026-09-15",
                                          "2026-09-16"])
        self.assertIn("**雪像 1 枚、7 日ぶん**", self.md)

    def test_the_day_numbers_match_snapshot_diff(self):
        snap = sd.load_source(ANON, False)
        rows, bounds, _c, interval = sd.merged_history(snap, snap, "3600")
        limit = max(1, round(sd.BURST_PER_HOUR * interval / 3600.0))
        for day in self.d["days"]:
            lo = wr.day_start(day)
            want = sd.aggregate([r for r in rows if lo <= r["t"] < lo + DAY], bounds, limit)
            self.assertEqual(self.d["stats"][day]["normal"], want, day)

    def test_the_two_burst_days_are_the_ones_t140_found(self):
        # 2026-09-11 と 09-12 だけ「1 標本 300 本以上」の窓がある (T14.0 の山)
        burst = {d: s["normal"]["burst_samples"] for d, s in self.d["stats"].items()}
        self.assertEqual({d: v for d, v in burst.items() if v},
                         {"2026-09-11": 4, "2026-09-12": 2})
        self.assertEqual(self.d["week"]["all"]["active_max"], 218)

    def test_the_errors_are_the_101_of_the_burst_days(self):
        self.assertEqual(self.d["week"]["all"]["errors"], 101)
        self.assertEqual(self.d["stats"]["2026-09-11"]["all"]["errors"], 85)
        self.assertEqual(self.d["stats"]["2026-09-12"]["all"]["errors"], 16)
        self.assertIn("原因の並びは `/status` の `errors_by_cause` と同じ", self.md)

    def test_the_client_that_stopped_coming(self):
        out = [e["client"] for e in self.d["moves"]["2026-09-15"]["out"]]
        self.assertEqual(out, ["198.51.100.1"])     # last_seen は 09-15 16:28 UTC
        self.assertEqual(self.d["moves"]["2026-09-16"]["out"], [])  # 窓の最後の日は数えない

    def test_new_hosts_need_a_second_snapshot(self):
        self.assertEqual(self.d["new_hosts"]["pairs"], 0)
        self.assertIn("雪像が 1 枚しか無いので「前の雪像」が無い", self.md)

    def test_the_hosts_table_says_it_could_not_subtract(self):
        self.assertFalse(self.d["hosts"]["windowed"])
        self.assertIn("雪像が 1 枚なので引き算できない", self.md)
        self.assertIn("`/hosts` が 256 KiB で切れている", self.md)   # 817 / 1,000 件

    def test_fewer_days_than_asked_is_said_at_the_head(self):
        md = run([ANON, "--days", "30"])
        self.assertIn("**雪像 1 枚、7 日ぶん** (求めたのは 30 日、足りない 23 日", md)

    def test_days_zero_means_everything_there_is(self):
        self.assertEqual(len(build([ANON, "--days", "0"])["days"]), 7)

    def test_a_shorter_window_trims_from_the_old_end(self):
        d = build([ANON, "--days", "3"])
        self.assertEqual(d["days"], ["2026-09-14", "2026-09-15", "2026-09-16"])
        self.assertEqual(d["week"]["requests"],
                         sum(d["stats"][x]["requests"] for x in d["days"]))


class FromDaily(unittest.TestCase):
    """`/daily` (T14.20) の JSON だけでも日別の表が出る (ホストと接続元は出ない)。"""

    @classmethod
    def setUpClass(cls):
        cls.lines = [{"day": f"2026-09-{10 + i}", "t": wr.day_start(f"2026-09-{10 + i}"),
                      "secs": 86395, "samples": 17280, "requests": 100 * (i + 1),
                      "bytes": 1024 * (i + 1), "connects": 10 * (i + 1),
                      "connect_p50_ms": 8.3, "connect_p95_ms": 80.7,
                      "dns_misses": 5 * (i + 1), "dns_per_connect": 0.5,
                      "dns_miss_ms": 11.5, "errors": i, "bursts": i,
                      "active_max": 4 + i, "evicted_idle": 0, "rss_max": 21147648,
                      "rss_avg": 20000000, "version": "0.1.0+test"} for i in range(7)]
        cls.tmp = tempfile.TemporaryDirectory()
        cls.path = os.path.join(cls.tmp.name, "daily.json")
        with open(cls.path, "w", encoding="utf-8") as f:
            json.dump({"days": cls.lines, "count": 7, "path": "/home/x/.daily.jsonl"}, f)
        cls.d = build([cls.path])
        cls.md = run([cls.path])

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_the_seven_days_come_from_daily(self):
        self.assertEqual(len(self.d["days"]), 7)
        self.assertTrue(all(s["src"] == "daily" for s in self.d["stats"].values()))
        self.assertIsNone(self.d["res"])
        self.assertIn("`/history` がどの入力にも無いので", self.md)
        self.assertIn("| `/daily` |", self.md)

    def test_the_quantiles_are_the_ones_in_the_line(self):
        self.assertEqual(self.d["stats"]["2026-09-16"]["normal"]["connect_p50"], 8.3)
        self.assertEqual(self.d["stats"]["2026-09-16"]["normal"]["connect_p95"], 80.7)
        self.assertEqual(self.d["stats"]["2026-09-16"]["requests"], 700)

    def test_the_tables_that_need_a_snapshot_say_so(self):
        self.assertIn("`/clients` (T14.7) も `/status` の `clients[]` もこの入力に無い", self.md)
        self.assertIn("`/hosts` がこの入力に無い", self.md)
        self.assertIn("## 8. 新しく見たホスト", self.md)

    def test_the_hills_come_from_the_daily_bursts_count(self):
        # `/bursts` の写真が無いので「山」は `/daily` の `bursts` (その日に撮れた枚数)
        self.assertFalse(self.d["has_bursts"])
        self.assertEqual(self.d["stats"]["2026-09-16"]["bursts"], 6)
        self.assertIn("| `/daily` の `bursts` |", self.md)
        self.assertIn("| **週** | **10** | **21** | |", self.md)   # 0+1+...+6

    def test_the_week_line_adds_the_days_up_when_there_is_no_history(self):
        self.assertEqual(self.d["week"]["requests"], sum(100 * (i + 1) for i in range(7)))
        self.assertEqual(self.d["week"]["normal"]["connects"], sum(10 * (i + 1) for i in range(7)))
        self.assertEqual(f"{self.d['week']['normal']['dns_per_connect']:.2f}", "0.50")
        self.assertEqual(f"{self.d['week']['normal']['ms_per_miss']:.1f}", "11.5")
        self.assertIsNone(self.d["week"]["normal"]["connect_p50"])   # 日ごとの分位点は足せない
        self.assertEqual(self.d["week"]["src_days"], {"history": 0, "daily": 7})

    def test_the_error_total_is_there_but_the_causes_are_not(self):
        self.assertEqual(self.d["week"]["all"]["errors"], 21)
        self.assertIn("**原因別の内訳はこの入力に無い**", self.md)

    def test_history_wins_over_daily_for_a_day_that_has_both(self):
        d = build([ANON, self.path])
        self.assertEqual(d["stats"]["2026-09-16"]["src"], "history")
        self.assertNotEqual(d["stats"]["2026-09-16"]["requests"], 700)


class LocalSnapshot(unittest.TestCase):
    """`/bursts` `/clients` の**ある**雪像 (T14.8 の `snapshot-local.json`) で表 5 と 6 を見る。"""

    @classmethod
    def setUpClass(cls):
        cls.d = build([LOCAL, "--days", "0"])
        cls.md = run([LOCAL, "--days", "0"])

    def test_bursts_come_from_the_shots(self):
        self.assertTrue(self.d["has_bursts"])
        self.assertTrue(self.d["shots"])
        self.assertIn("`/bursts` の写真 (T14.6) がこの週に", self.md)
        self.assertIn("| 日 | 写真 | 最大同時 | いちばん多かった時刻 | 接続元 |", self.md)

    def test_a_fine_resolution_warns_that_the_normal_threshold_is_one(self):
        # `res=3600` の標本が無い雪像なので `res=5` に落ち、平常時の閾が 1 本になる
        self.assertEqual((self.d["res"], self.d["limit"]), ("5", 1))
        self.assertIn("平常時の閾が 1 標本 1 本になっている", self.md)

    def test_burst_raises_the_threshold_back(self):
        d = build([LOCAL, "--days", "0", "--burst", "3600"])
        self.assertEqual(d["limit"], 5)          # 1 標本 (5 秒) 5 本まで平常時
        self.assertLess(d["week"]["normal"]["burst_samples"],
                        self.d["week"]["normal"]["burst_samples"])
        self.assertIsNone(self.d["week"]["normal"]["connect_p50"])
        self.assertIsNotNone(d["week"]["normal"]["connect_p50"])

    def test_clients_come_from_the_clients_endpoint(self):
        self.assertTrue(self.d["has_clients_part"])
        self.assertTrue(all(e["arrived_exact"] for e in self.d["clients"].values()))
        # `/clients` には `first_seen` があるので、いちばん古い雪像の日でも「新しく来た」
        self.assertTrue(any(mv["in"] for mv in self.d["moves"].values()))


class Output(unittest.TestCase):
    def test_o_writes_the_markdown_to_a_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "week.md")
            self.assertEqual(run([ANON, "-o", out]), "")
            with open(out, encoding="utf-8") as f:
                md = f.read()
            self.assertTrue(md.startswith("# rust-http-proxy — 週次の要約"))
            self.assertIn("## 8. 新しく見たホスト", md)

    def test_an_unreadable_input_is_listed_at_the_end_not_fatal(self):
        with tempfile.TemporaryDirectory() as tmp:
            bad = os.path.join(tmp, "bad.json")
            with open(bad, "w", encoding="utf-8") as f:
                f.write("{")
            md = run([ANON, bad])
            self.assertIn("**読めなかった**", md)
            self.assertIn("## 1. 要求数", md)

    def test_nothing_readable_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            bad = os.path.join(tmp, "bad.json")
            with open(bad, "w", encoding="utf-8") as f:
                f.write("{")
            with self.assertRaises(SystemExit):
                run([bad])

    def test_top_limits_the_host_and_client_lists(self):
        self.assertEqual(run([ANON, "--top", "3"]).count("`host-0"), 3)
        self.assertIn("## 7. 遅かったホスト 上位 3 ", run([ANON, "--top", "3"]))


class OutJson(unittest.TestCase):
    """`--out json` は `build()` の辞書をそのまま出す (T14.40 の申し送り → T14.44)。"""

    @classmethod
    def setUpClass(cls):
        cls.j = json.loads(run([ANON, "--out", "json"]))
        cls.d = build([ANON])

    def test_the_json_is_the_dict_build_returns(self):
        self.assertEqual(self.j["days"], self.d["days"])
        self.assertEqual(self.j["week"]["all"]["errors"], self.d["week"]["all"]["errors"])
        self.assertEqual(self.j["stats"]["2026-09-11"]["all"], self.d["stats"]["2026-09-11"]["all"])

    def test_the_week_numbers_are_the_ones_of_the_markdown(self):
        self.assertEqual(self.j["week"]["all"]["errors"], 101)
        self.assertEqual(self.j["week"]["all"]["active_max"], 218)
        self.assertEqual(self.j["week"]["samples"], self.d["week"]["samples"])

    def test_unreadable_inputs_are_listed_in_the_json_too(self):
        with tempfile.TemporaryDirectory() as tmp:
            bad = os.path.join(tmp, "bad.json")
            with open(bad, "w", encoding="utf-8") as f:
                f.write("{")
            j = json.loads(run([ANON, bad, "--out", "json"]))
            self.assertEqual([e["path"] for e in j["skipped"]], [bad])

    def test_md_is_still_the_default(self):
        self.assertIn("## 1. 要求数", run([ANON]))


if __name__ == "__main__":
    unittest.main()
