#!/usr/bin/env python3
"""`scripts/snapshot-summary.py` (T14.4) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から

入力は `snapshot-diff.py` の試験と同じ**架空の**雪像 2 枚
(`testdata/snapshot-a.json` / `snapshot-b.json`) と、その場で書き換えた写し。
**本物の雪像 (利用者の閲覧先が並ぶ) は入れない**。
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


def _load(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


ss = _load("snapshot_summary", "snapshot-summary.py")

A = os.path.join(DATA, "snapshot-a.json")
B = os.path.join(DATA, "snapshot-b.json")


def read(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def run(argv):
    """Markdown を文字列で受け取る。"""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        ss.main(argv)
    return out.getvalue()


@contextlib.contextmanager
def written(**snaps):
    """書き換えた雪像を一時ディレクトリに置いて、そのパスを返す。"""
    with tempfile.TemporaryDirectory() as tmp:
        paths = {}
        for name, body in snaps.items():
            paths[name] = os.path.join(tmp, name + ".json")
            with open(paths[name], "w", encoding="utf-8") as f:
                json.dump(body, f)
        yield paths


class Errors(unittest.TestCase):
    """§1 の「エラー」の行 (T14.99 で負の数が出た所)。"""

    def test_the_difference_is_matched_by_host_key(self):
        # beta 3 → 6、delta 1 → 3 で +5 (原因 dns +2・reset +2・tls +1)
        md = run([B, "--prev", A])
        self.assertIn("- エラー: 5 件 (`/hosts` をホストの鍵で突き合わせた差分)"
                      " / 原因 dns 2 reset 2 tls 1 / 個票 3 件", md)

    def test_without_prev_it_is_the_total_of_the_newer_table(self):
        md = run([B])
        self.assertIn("- エラー: 9 件 (`/hosts` の合計) / 原因 dns 4 refused 1 timeout 1"
                      " reset 2 tls 1 / 個票 3 件", md)

    def test_a_host_that_dropped_out_of_a_truncated_table_is_not_subtracted(self):
        """**表ごとの合計を引くと負になる**のが T14.99 で見つかった不具合。

        後の `/hosts` から消えたホストのぶん (ここでは 9 件) が、そのまま合計から
        引かれて「エラー −4 件 / 原因 dns −7」のような数になっていた。
        """
        a, b = read(A), read(B)
        a["hosts"]["hosts"].append({
            "host": "connect://vanished.example.jp:443", "requests": 10, "timed": 10,
            "avg_ms": 5.0, "p50_ms": 5.0, "p95_ms": 5.0, "max_ms": 9, "bytes": 0,
            "dns_misses": 0, "dns_ms_sum": 0.0, "connect_ms_sum": 0.0,
            "errors": 9, "errors_by_cause": [9, 0, 0, 0, 0, 0, 0, 0]})
        for snap, shown in ((a, 7), (b, 6)):
            snap["hosts"].update(truncated=True, count=1000, shown=shown)
        with written(a=a, b=b) as paths:
            md = run([paths["b"], "--prev", paths["a"]])
        self.assertIn("- エラー: 5 件 (`/hosts` をホストの鍵で突き合わせた差分)", md)
        self.assertNotIn("- エラー: -4 件", md)
        # 消えたのは足した 1 件と、もともと B に無い `old.example.jp` (エラー 0) の 2 件
        self.assertIn("消えたホスト 2 件 (通算のエラー 9 件) は差分から外した", md)
        self.assertIn("`/hosts` は **256 KiB で切れている** (7/1,000 件、6/1,000 件)", md)

    def test_a_rebuilt_rrd_says_it_cannot_subtract(self):
        """ホスト 1 件の通算が**減って**いたら `.rrd` の作り直し (引き算しない)。"""
        b = read(B)
        b["hosts"]["hosts"][1]["errors"] = 1          # 前 (3) より小さい
        with written(b=b) as paths:
            md = run([paths["b"], "--prev", A])
        self.assertIn("- エラー: **引き算できない** (1 ホストで `/hosts` の通算が減っている", md)

    def test_the_records_have_a_readable_time(self):
        """§7 (`snapshot-diff.py`) と同じ綴りにする (epoch のままだと時間帯が読めない)。"""
        md = run([B, "--prev", A])
        self.assertIn("  - `2026-09-10 14:20:00Z` connect", md)
        self.assertNotIn("`1789050000`", md)


class Restart(unittest.TestCase):
    """再起動をまたいだら `/status` の通算は引き算しない (T14.99)。"""

    def pair(self, same_version):
        """後の雪像 (`/status` の通算 1,200 件) と、その 24 時間前の雪像。"""
        b = read(B)
        b["status"].update(uptime_secs=43200, total_requests=1200)
        b["uptime_secs"] = 43200
        a = read(B)
        a["taken_at"] = b["taken_at"] - 86400
        if same_version:
            # 24 時間ぶん `uptime_secs` が伸びていれば再起動していない
            a["status"].update(uptime_secs=200000 - 86400, total_requests=200)
            b["status"].update(uptime_secs=200000)
            b["uptime_secs"] = 200000
        else:
            # 版が違えば再起動 (`uptime_secs` は 61 秒 → 43,200 秒で**減っていない**)
            a["version"] = a["status"]["version"] = "0.1.0+aaaaaaa"
            a["status"].update(uptime_secs=61, total_requests=11)
        a["status"]["dns"].update(misses=1, miss_ms_sum=10.0, refreshes=0)
        b["status"]["dns"].update(misses=484, miss_ms_sum=7900.0, refreshes=9223)
        return a, b

    def test_a_restart_that_only_shows_in_the_version_is_caught(self):
        """`uptime_secs` の大小だけだと、**再起動の直後に取った**雪像を見落とす。"""
        a, b = self.pair(same_version=False)
        with written(a=a, b=b) as paths:
            md = run([paths["b"], "--prev", paths["a"]])
        self.assertIn("**再起動をまたいでいる**", md)
        self.assertIn("は「起動から」の値**", md)
        self.assertIn("| 要求 | 1,200 ", md)              # 1,200 − 11 にしない
        # 484 − 1 にしない。閾 0.15 と読み違えないよう 1 要求あたりは 3 桁
        self.assertIn("| ミス | 484 (0.403 /要求) |", md)
        self.assertIn("| 表 / warm / 引き直し | 12 / 3 / 9,223 |", md)

    def test_without_a_restart_the_totals_are_still_subtracted(self):
        a, b = self.pair(same_version=True)
        with written(a=a, b=b) as paths:
            md = run([paths["b"], "--prev", paths["a"]])
        self.assertNotIn("再起動をまたいでいる", md)
        self.assertIn("| 要求 | 1,000 ", md)              # 1,200 − 200
        self.assertIn("| ミス | 483 ", md)                # 484 − 1


class Truncated(unittest.TestCase):
    """256 KiB で切れた部を先頭で知らせる (T14.99)。"""

    def test_the_truncated_parts_are_listed(self):
        b = read(B)
        b["hosts"].update(truncated=True, count=1000, shown=6)
        # `profile` は T15.0 の fixture で `parts` に入ったので、ここでは切れた形に差し替える
        b["profile"] = {"samples": [], "count": 720, "shown": 456, "truncated": True}
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("- **応答が 256 KiB で切れている部**: `hosts` (6/1,000 件)、"
                      "`profile` (456/720 件)", md)

    def test_a_nested_part_name_is_followed(self):
        """`parts` の名前は `history.5` のような入れ子もある。"""
        b = read(B)
        b["history"]["5"].update(truncated=True, count=4320, shown=2000)
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("`history.5` (2,000/4,320 件)", md)

    def test_nothing_is_printed_when_no_part_is_cut(self):
        self.assertNotIn("応答が 256 KiB で切れている部", run([B]))


class Recent(unittest.TestCase):
    """個票の「そろっている窓」(T14.99)。"""

    def snapshot(self, truncated):
        b = read(B)
        b["dropped"] = []
        # 3 本: 取得の 2 時間前に開いて 1 時間前に閉じた 1 本と、直前の 2 本
        b["recent"] = {
            "recent": [
                {"id": 3, "at": b["taken_at"] - 7200, "secs": 3600, "reason": "server_eof",
                 "kind": "connect", "client": "192.0.2.10", "target": "alpha.example.jp:443",
                 "up": 1024, "down": 2048, "ms": {"connect": 5, "dns": 1}},
                {"id": 4, "at": b["taken_at"] - 600, "secs": 60, "reason": "client_eof",
                 "kind": "connect", "client": "192.0.2.10", "target": "beta.example.jp:443",
                 "up": 10, "down": 20, "ms": {"connect": 7, "dns": 0}},
                {"id": 5, "at": b["taken_at"] - 300, "secs": 30, "reason": "client_eof",
                 "kind": "connect", "client": "192.0.2.10", "target": "beta.example.jp:443",
                 "up": 10, "down": 20, "ms": {"connect": 9, "dns": 0}},
            ],
            "count": 3, "shown": 3, "recorded": 900, "truncated": truncated}
        return b

    def test_the_window_starts_at_the_earliest_close_not_the_earliest_open(self):
        with written(b=self.snapshot(True)) as paths:
            md = run([paths["b"]])
        # 最小の `at` は 22:26:40Z だが、そろっているのは最小の `at + secs` から
        self.assertIn("  - そろっている窓: 2026-09-10 23:26:40Z → 2026-09-11 00:26:40Z"
                      " (`min(at + secs)` から取得まで)", md)
        self.assertIn("**これより前に閉じた接続は応答から落ちている**", md)

    def test_a_complete_answer_has_no_caveat(self):
        with written(b=self.snapshot(False)) as paths:
            md = run([paths["b"]])
        self.assertIn("  - そろっている窓: 2026-09-10 23:26:40Z", md)
        self.assertNotIn("応答から落ちている", md)


class Cpu(unittest.TestCase):
    """CPU の表 (`/profile` + `kernel.cgroup_cpu`。T15.0 (15))。"""

    def test_the_cores_are_printed_with_three_digits(self):
        """**1 桁だと 0.006 コアも 0.04 コアも「0.0」に潰れる** (張り付きが読めない)。"""
        md = run([B])
        self.assertIn("| 使用 | 0.006 コア / 割り当て 2.0 コア の **0.3%**"
                      " (`/profile` 3 標本 × 60 秒) |", md)

    def test_the_throttled_share_is_a_ratio_not_a_count(self):
        """「41 回絞られた」だけでは 41/8,123 なのか 41/41 なのか決まらない (T15.0 (6))。"""
        self.assertIn("| 絞られた周期 | **0.5%** (41/8,123)、起動から 0.8% (30/4,000) |",
                      run([B]))

    def test_without_nr_periods_the_ratio_is_not_printed(self):
        b = read(B)
        del b["status"]["kernel"]["cgroup_cpu"]["nr_periods"]
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("**この版に分母 `nr_periods` が無い**ので割合は出せない", md)
        self.assertNotIn("| 絞られた周期 | **", md)

    def test_the_top_threads_are_summed_over_the_window(self):
        """上位スレッドは**窓ごとに出る**ので、tid で足してから並べ直す。"""
        md = run([B])
        self.assertIn("| 上位スレッド | `conn-7` (tid 41、conn、540 ms、走行 126 標本)、"
                      "`conn-3` (tid 39、conn、180 ms、走行 36 標本)", md)

    def test_the_run_delay_is_per_role_and_sorted(self):
        self.assertIn("| 走れずに待った (`run_delay_us`) | conn 45.0 ms、", run([B]))

    def test_a_kernel_without_schedstat_has_no_run_delay_row(self):
        b = read(B)
        for row in b["profile"]["samples"]:
            row[9] = None
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("| 上位スレッド |", md)
        self.assertNotIn("run_delay_us", md)

    def test_the_user_and_kernel_split_is_one_row(self):
        """T16.0: コンテナ (cgroup の起動から)・プロセス・役割の上位 3 つを「ユーザー / カーネル」で。"""
        b = read(B)
        p = b["profile"]
        p["keys"] = p["keys"] + ["user_us", "cpu_user_us"]
        # プロセスの CPU 1,080 ms のうち 3 割がユーザー空間。役割も 3 割 (conn / history / accept)
        for row, user in zip(p["samples"], (108000, 102000, 114000)):
            role_user = [300, 72000, 0, 0, 2700, 0, 0, 0, 0]
            row.extend([role_user, user])
        since = b["status"]["kernel"]["cgroup_cpu"]["since_start"]
        since.update(usage_usec=4_000_000, user_usec=1_200_000, system_usec=2_800_000)
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("| ユーザー / カーネル | コンテナ (起動から) 1.2 / 2.8 秒 (ユーザー **30.0%**)。"
                      "プロセス 324 / 756 ms (ユーザー **30.0%**)。"
                      "役割の上位 conn 216 / 504 ms、history 8 / 19 ms、accept 1 / 3 ms |", md)

    def test_a_snapshot_without_the_split_has_no_user_kernel_row(self):
        """T16.0 より前の版 (`user_us` も `user_usec` も無い) では行ごと出さない。"""
        md = run([B])
        self.assertIn("| 使用 |", md)
        self.assertNotIn("| ユーザー / カーネル |", md)

    def test_a_snapshot_without_profile_or_kernel_has_no_cpu_table(self):
        """古い雪像 (A) には `/profile` も `kernel` も無いので表ごと出さない。"""
        self.assertNotIn("| CPU | 値 |", run([A]))

    def test_the_kernel_alone_is_enough_for_the_throttling_row(self):
        b = read(B)
        del b["profile"]
        b["parts"].remove("profile")
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("| 絞られた周期 |", md)
        self.assertNotIn("| 使用 |", md)


class MissKinds(unittest.TestCase):
    """名前解決のミスの種類別と引き直しの様子 (T15.0 (7))。"""

    def test_the_kinds_are_printed_in_the_order_of_the_json(self):
        self.assertIn("| ミスの種類別 (起動から) | cold 12 / expired 24 / warm_stale 3"
                      " / negative 1 |", run([B]))

    def test_the_refresh_failures_and_the_late_ones_are_printed(self):
        self.assertIn("| 引き直しの失敗 / 遅れ / 最大 (起動から) | 2 回 / 1 回 / 260.0 ms |",
                      run([B]))

    def test_an_old_snapshot_has_neither_row(self):
        md = run([A])
        self.assertNotIn("ミスの種類別", md)
        self.assertNotIn("引き直しの失敗", md)


class Wait(unittest.TestCase):
    """利用者が待つ時間の行 (`/history` の `waits`。T15.0 (2) + (10))。"""

    def test_the_row_uses_the_waits_column_for_the_count(self):
        # 直近 1 日 (res=60) の 1 標本: 3 本・合計 27 ms・[5,10) ms のバケツ
        self.assertIn("| 利用者が待つ `wait` (直近 1 日) | 3 本 | 9.0 ms | 7.5 / 9.8 ms | 12 ms |",
                      run([B]))

    def test_an_old_snapshot_says_the_column_is_missing(self):
        """**「0 本」と書くと「誰も待っていない」と読まれる**ので、列の有無は分けて書く。

        部は入っているのに列だけ無い = **その版が測っていない**。
        """
        b = read(B)
        h = b["history"]["60"]
        i = h["keys"].index("waits")
        del h["keys"][i:i + 4]                       # `waits` から `wait_buckets` までの 4 列
        for row in h["samples"]:
            del row[i:i + 4]
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("| 利用者が待つ `wait` (直近 1 日) | (この雪像にその列は無い) |", md)

    def test_a_dropped_part_is_not_a_missing_column(self):
        """`history.5` は `/snapshot` の `DROP_ORDER` に入っていて**落ちることがある**。

        落ちたときに「その列は無い」と書くと「この版は connect を測っていない」と
        読まれる (T15.0 (15) のレビュー)。
        """
        b = read(B)
        del b["history"]["5"]
        b["parts"] = [p for p in b["parts"] if p != "history.5"]
        b["dropped"] = (b.get("dropped") or []) + ["history.5"]
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("| CONNECT 確立 (直近 1 時間) | (この部は雪像に入っていない) |", md)
        # 部が丸ごと無い A も同じ (`history.60` がそもそも入っていない)
        self.assertIn("| 利用者が待つ `wait` (直近 1 日) | (この部は雪像に入っていない) |", run([A]))

    def test_a_real_zero_is_still_zero(self):
        b = read(B)
        h = b["history"]["60"]
        i = h["keys"].index("waits")
        for row in h["samples"]:
            row[i] = 0
            row[i + 3] = [0] * 13
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("| 利用者が待つ `wait` (直近 1 日) | 0 本 |", md)


class StuckTunnels(unittest.TestCase):
    """動かないトンネル (`idle_secs` ≥ 300 秒。T15.0 (4))。"""

    def test_only_the_tunnels_over_the_threshold_are_listed(self):
        """預かり中の 280 秒の 1 本は閾の下なので入らない。"""
        md = run([B])
        self.assertIn("  - **動かないトンネル (`idle_secs` ≥ 300 秒)**: 2 本"
                      " (半閉じ 2 本、`spins` の合計 3,050,000)", md)
        self.assertNotIn("zeta.example.jp", md)

    def test_the_evidence_is_on_one_line(self):
        self.assertIn("    - `alpha.example.jp:443` idle 98,800 秒 / 齢 99,000 秒 / 8.9 KiB"
                      " / 0 bps / tid 41 / spins 1,840,000 / 半閉じ client 98,800 秒"
                      " / revents client=`HUP|ERR`", run([B]))

    def test_a_default_row_prints_without_the_new_fields(self):
        """`/connections` は**既定のままの欄を 1 バイトも出さない**ので「無ければ既定値」。"""
        b = read(B)
        b["connections"]["connections"] = [
            {"id": 1, "client": "192.0.2.10", "target": "alpha.example.jp:443",
             "kind": "connect", "state": "relaying", "age_secs": 900, "bytes": 10,
             "fds": 2, "rate_bps": 0, "idle_secs": 800}]
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("    - `alpha.example.jp:443` idle 800 秒 / 齢 900 秒 / 10 B / 0 bps\n", md)
        self.assertIn("(半閉じ 0 本、`spins` の合計 0)", md)

    def test_nothing_is_printed_when_no_tunnel_is_stuck(self):
        b = read(B)
        for c in b["connections"]["connections"]:
            c.pop("idle_secs", None)
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("- いまの接続 (`/connections`): 5 本", md)
        self.assertNotIn("動かないトンネル", md)


class StatusBefore(unittest.TestCase):
    """RSS だけは雪像の**前**に取った `/status` を使う (T15.0 (15))。"""

    def test_the_rss_comes_from_the_status_taken_before_the_snapshot(self):
        # 雪像を配ること自体が RSS を約 1.0 MB 押し上げるので、雪像の中の 20.0 MiB とは違う値
        st = {"status": "ok", "cache": {"system": {"process_rss_bytes": 19 << 20}}}
        with written(before=st) as paths:
            md = run([B, "--status-before", paths["before"]])
        self.assertIn("| RSS | 19.0 MiB (**RSS は雪像の前に取った `/status` の値**) |", md)

    def test_without_the_option_the_rss_is_the_one_in_the_snapshot(self):
        self.assertIn("| RSS | 20.0 MiB |\n", run([B]))

    def test_a_status_without_rss_falls_back_to_the_snapshot(self):
        with written(before={"status": "ok"}) as paths:
            md = run([B, "--status-before", paths["before"]])
        self.assertIn("| RSS | 20.0 MiB |\n", md)


class Anomalies(unittest.TestCase):
    """`/events` の anomaly の種類別 件/時 (T17.0c)。B は起動から 12 時間 (起動 1789043200)。"""

    def snapshot(self, extra):
        b = read(B)
        b["events"]["events"] = extra + b["events"]["events"]
        return b

    def test_the_kinds_are_counted_per_hour_since_the_start(self):
        """`cleared:` と、起動より前 (状態ファイルから読み戻した前の版) の 1 件は数えない。"""
        t = read(B)["taken_at"]
        ev = [{"at": t - 100, "kind": "anomaly", "text": "cleared: dns_slow after 5m (…)"},
              {"at": t - 400, "kind": "anomaly", "text": "dns_slow: dns miss avg 400 ms over 5m (1 misses)"},
              {"at": t - 3600, "kind": "anomaly", "text": "dns_slow: dns miss avg 300 ms over 5m (1 misses)"},
              {"at": t - 7200, "kind": "anomaly", "text": "connect_p95: connect p95 105 ms over 5m"},
              {"at": t - 7000, "kind": "anomaly", "text": "cleared: connect_p95 after 6m (…)"},
              {"at": t - 40000, "kind": "anomaly", "text": "dns_slow: dns miss avg 250 ms over 5m"},
              # 起動 (t − 43,200) より前
              {"at": t - 50000, "kind": "anomaly", "text": "dns_slow: from the previous version"},
              {"at": t - 500, "kind": "new_client", "text": "new_client: 192.0.2.99 first seen (…)"}]
        with written(b=self.snapshot(ev)) as paths:
            md = run([paths["b"]])
        self.assertIn("| `/events` の anomaly (起動から 12.0 時間) | 件 | 件/時 |\n|---|---|---|\n"
                      "| `dns_slow` | 3 | 0.250 |\n| `connect_p95` | 1 | 0.083 |\n", md)
        self.assertIn("- 収まった (`cleared:`) 2 件は数えない。起動より前の 1 件"
                      " (状態ファイルから読み戻した前の版のぶん) は外した\n", md)
        self.assertNotIn("落ちているかもしれない", md)

    def test_no_anomaly_since_the_start_is_a_zero_row(self):
        md = run([B])
        self.assertIn("| (起動から 1 件も立っていない) | 0 | — |", md)

    def test_a_full_ring_says_the_start_may_be_missing(self):
        t = read(B)["taken_at"]
        b = self.snapshot([{"at": t - 60, "kind": "anomaly", "text": "active_high: 200 of 240"}])
        b["events"]["capacity"] = 3
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("**リングが満杯か応答が切れているので、起動直後のぶんが落ちているかもしれない**"
                      " (残っている最古は 2026-09-11 00:25:40Z)", md)

    def test_an_old_snapshot_has_no_table(self):
        self.assertNotIn("`/events` の anomaly", run([A]))


class ClientWatch(unittest.TestCase):
    """接続元の見張り (T17.0c)。**IP と名前はそのまま出す** (要約は手元に置くもの)。"""

    def test_a_client_missing_from_the_older_snapshot_is_new(self):
        md = run([B, "--prev", A])
        self.assertIn("「新」は 前回の取得 (2026-09-10 00:26:40Z) より後に初めて見た", md)
        # 198.51.100.7 は A に居ない。192.0.2.10 は 5,000 − 4,800 = 200 要求
        self.assertIn("| `198.51.100.7` **新** | 300 | 3 | 0 / — | 0 / — | 2026-09-10 14:20:00Z"
                      " | 2026-09-11 00:20:00Z | Mozilla/5.0 (fictional) |", md)
        # B は `/recent` を部ごと落としているので「窓」は `—` (0 本と書かない)。
        # B の 192.0.2.10 の行には `nonstandard_ports` が無い (古い版の形) ので起動からも `—`
        self.assertIn("| `192.0.2.10` | 200 | 42 | 0 / — | — / — |", md)
        self.assertIn("  - `/recent` はこの雪像に入っていない", md)
        self.assertIn("  - 新しく現れた接続元: 1 件: `198.51.100.7` (300 要求)", md)

    def test_without_prev_new_means_first_seen_after_the_start(self):
        """起動 (1789043200) より後に初めて見たのは 198.51.100.7 だけ。"""
        md = run([B])
        self.assertIn("「新」は 起動 (2026-09-10 12:26:40Z) より後に初めて見た", md)
        self.assertIn("| `198.51.100.7` **新** | 300 |", md)
        self.assertIn("| `192.0.2.10` | 5,000 |", md)

    def test_literal_targets_and_odd_ports_are_counted(self):
        """`/clients` の通算と、`/recent` の窓の中 (IPv6 の `[addr]:port` も IP リテラル)。"""
        b = read(B)
        b["clients"]["clients"][0].update(literal_targets=4, nonstandard_ports=2)
        t = b["taken_at"]
        rec = [("connect", "203.0.113.5:8443"),     # リテラル + 非標準
               ("connect", "[2001:db8::1]:443"),    # リテラル
               ("connect", "gamma.example.jp:22"),  # 非標準
               ("http", "delta.example.jp:8080"),   # forward は数えない (CONNECT だけ)
               ("connect", "alpha.example.jp:443")]
        b["recent"] = {"recent": [
            {"id": i, "at": t - 100 + i, "secs": 1, "reason": "client_eof", "kind": k,
             "client": "192.0.2.10", "target": tgt, "up": 0, "down": 0, "ms": {}}
            for i, (k, tgt) in enumerate(rec)], "count": 5, "shown": 5, "recorded": 5}
        b["parts"].append("recent")
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("| `192.0.2.10` | 5,000 | 42 | 4 / 2 | 2 / 2 |", md)
        self.assertIn("IP リテラル宛て 2 本、443・80 以外の CONNECT 2 本", md)
        self.assertNotIn("**この雪像は切れている**", md)

    def test_new_client_events_and_endpoint_only_readers_are_listed(self):
        b = read(B)
        t = b["taken_at"]
        b["events"]["events"] = [
            {"at": t - 60, "kind": "new_client",
             "text": "new_client: 198.51.100.7 first seen (1 req, first target port 443 (name), no agent)"},
            {"at": t - 90000, "kind": "new_client",
             "text": "new_client: 203.0.113.9 first seen (1 req, first target port 80 (literal), no agent)"},
        ] + b["events"]["events"]
        b["status"]["readers"] = [
            {"client": "192.0.2.10", "count": 9, "last_at": t - 5, "last_path": "/status"},
            {"client": "203.0.113.77", "count": 2, "last_at": t - 30, "last_path": "/"}]
        with written(b=b) as paths:
            md = run([paths["b"]])
        self.assertIn("  - `/events` の `new_client`: リングに 2 件 (うち 起動より後 1 件)。新しい順に:\n"
                      "    - `2026-09-11 00:25:40Z` new_client: 198.51.100.7 first seen", md)
        self.assertIn("    - `2026-09-09 23:26:40Z` (窓の外) new_client: 203.0.113.9", md)
        # プロキシとして使っている 192.0.2.10 は出さない
        self.assertIn("走査が `GET /` で来るとここにだけ残る): `203.0.113.77` 2 回"
                      " (最後 `/` 2026-09-11 00:26:10Z)\n", md)

    def test_the_host_and_port_are_split(self):
        self.assertEqual(ss.target_host_port("alpha.example.jp:443"), ("alpha.example.jp", 443))
        self.assertEqual(ss.target_host_port("[2001:db8::1]:8443"), ("2001:db8::1", 8443))
        self.assertEqual(ss.target_host_port("http://192.0.2.1:8080/x"), ("192.0.2.1", 8080))
        self.assertEqual(ss.target_host_port("noport"), ("noport", None))
        self.assertTrue(ss.is_ip_literal("2001:db8::1"))
        self.assertFalse(ss.is_ip_literal("alpha.example.jp"))

    def test_a_snapshot_without_clients_has_no_watch(self):
        b = read(B)
        del b["clients"]
        with written(b=b) as paths:
            self.assertNotIn("接続元の見張り", run([paths["b"]]))


class Bytes(unittest.TestCase):
    def test_the_unit_matches_the_divisor(self):
        """1,024 で割るなら KiB / MiB / GiB (`GB` / `MB` だと 2.4〜7.4% 小さく読める)。"""
        self.assertEqual(ss.fmt_bytes(1 << 20), "1.0 MiB")
        self.assertEqual(ss.fmt_bytes(1536), "1.5 KiB")
        self.assertEqual(ss.fmt_bytes(3 << 30), "3.0 GiB")
        self.assertEqual(ss.fmt_bytes(999), "999 B")
        self.assertIn("| 転送 | 42.0 MiB |", run([B, "--prev", A]))


if __name__ == "__main__":
    unittest.main()
