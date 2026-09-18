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


if __name__ == "__main__":
    unittest.main()
