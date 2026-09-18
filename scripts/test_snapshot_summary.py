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
        b["parts"].append("profile")
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
