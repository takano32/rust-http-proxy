#!/usr/bin/env python3
"""`/hosts` の `other` 行 (表からあふれたホストの畳み先) を分位点の比べから外す試験 (T.28.19)。

    python3 -m unittest discover -s scripts        # リポジトリの根から

`other` は生きた CONNECT の溢れ・forward の溢れ・死んだ宛先への総当たりを 1 行に混ぜた通算
(版をまたぐ) なので、`status-diff.py` は forward の表に p95 10 秒を並べていた。見るのは:
(1) `status-diff.py` の表と AAAA 別の中央値に `other` が入らず、「混在・通算」の印つきの 1 行が出る、
(2) `snapshot-diff.py` の §3 も同じ (`--criteria` の無いとき)、
(3) `--criteria` のときは出力が変わらない (`hosts` に `folded` を足さず、§3 にも `other` が残る)。
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


status_diff = _load("status_diff_t2819", "status-diff.py")
snapshot_diff = _load("snapshot_diff_t2819", "snapshot-diff.py")
pd = _load("proxydata_t2819", "proxydata.py")

A = os.path.join(DATA, "snapshot-a.json")
B = os.path.join(DATA, "snapshot-b.json")


def run(mod, argv):
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        mod.main(argv)
    return out.getvalue()


def host(key, requests, p95):
    return {"host": key, "requests": requests, "hits": 0, "misses": 0, "bypass": requests,
            "errors": 3, "blocked": 0, "bytes": requests * 100, "timed": requests,
            "avg_ms": 20.0, "p50_ms": 15.0, "p95_ms": p95, "max_ms": p95,
            "last_seen": 1789086000, "dns_ms_sum": 0, "dns_misses": 0, "connect_ms_sum": 0,
            "v4_wins": requests, "v6_wins": 0, "errors_by_cause": [0, 0, 0, 3, 0, 0, 0, 0]}


HOSTS = {"schema": 1, "uptime_secs": 3600, "total_requests": 600, "count": 3, "shown": 3,
         "hosts": [host("other", 500, 10006.3),
                   host("connect://alpha.example.jp:443", 80, 30.0),
                   host("http://delta.example.org:80", 20, 40.0)]}


class IsFolded(unittest.TestCase):
    def test_only_the_overflow_row_is_folded(self):
        rows = [pd.row_of(h["host"], h, None) for h in HOSTS["hosts"]]
        self.assertEqual([pd.is_folded(r) for r in rows], [True, False, False])


class StatusDiff(unittest.TestCase):
    def setUp(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "hosts.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(HOSTS, f)
            self.out = run(status_diff, [path, "--no-dns"])

    def test_other_is_not_in_the_forward_table(self):
        fwd = self.out.split("== forward", 1)[1].split("-- `other`", 1)[0]
        self.assertIn("delta.example.org", fwd)
        self.assertNotIn("10006.3", fwd, "other の p95 10 秒が forward の表に並ばない")
        self.assertIn("== forward (1 ホスト) ==", self.out)

    def test_other_gets_one_marked_line_without_quantiles(self):
        tail = self.out.split("-- `other`", 1)[1]
        self.assertIn(pd.FOLDED_MARK, tail.splitlines()[0])
        self.assertIn("要求 500", tail)
        self.assertNotIn("10006", tail, "分位点は出さない")

    def test_with_two_files_the_line_is_a_delta(self):
        with tempfile.TemporaryDirectory() as tmp:
            a, b = os.path.join(tmp, "a.json"), os.path.join(tmp, "b.json")
            with open(a, "w", encoding="utf-8") as f:
                json.dump(HOSTS, f)
            later = json.loads(json.dumps(HOSTS))
            later["hosts"][0]["requests"] = 700
            later["hosts"][0]["timed"] = 700
            later["uptime_secs"] = 7200
            with open(b, "w", encoding="utf-8") as f:
                json.dump(later, f)
            out = run(status_diff, [a, b, "--no-dns"])
        self.assertIn("要求 200、計測 200、その間の avg", out)


class SnapshotDiff(unittest.TestCase):
    def test_section_3_drops_other_and_marks_it(self):
        out = run(snapshot_diff, [A, B, "--no-dns"])
        sec3 = out.split("## 3.", 1)[1].split("## 4.", 1)[0]
        table = [line for line in sec3.splitlines() if line.startswith("| `")]
        self.assertFalse(any(line.startswith("| `other`") for line in table), table)
        self.assertIn(f"- `other` (**{pd.FOLDED_MARK}**", sec3)

    def test_the_json_has_the_folded_row_only_without_criteria(self):
        args = snapshot_diff.parser().parse_args([A, B, "--no-dns"])
        a, b = (snapshot_diff.load_source(f, False) for f in (A, B))
        d = snapshot_diff.build(a, b, args)
        self.assertEqual([r["key"] for r in d["hosts"]["folded"]], ["other"])
        # 行そのものは残す (エラーの合計 §7 を変えないため)
        self.assertIn("other", [r["key"] for r in d["hosts"]["rows"]])

    def test_criteria_output_keeps_other_in_section_3(self):
        args = snapshot_diff.parser().parse_args([A, B, "--no-dns", "--criteria", "phase25"])
        a, b = (snapshot_diff.load_source(f, False) for f in (A, B))
        d = snapshot_diff.build(a, b, args)
        self.assertNotIn("folded", d["hosts"])
        out = run(snapshot_diff, [A, B, "--no-dns", "--criteria", "phase25"])
        sec3 = out.split("## 3.", 1)[1].split("## 4.", 1)[0]
        self.assertTrue(any(line.startswith("| `other`") for line in sec3.splitlines()))
        self.assertNotIn(pd.FOLDED_MARK, sec3)


if __name__ == "__main__":
    unittest.main()
