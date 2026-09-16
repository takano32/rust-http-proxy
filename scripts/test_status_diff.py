#!/usr/bin/env python3
"""`scripts/status-diff.py` の `--group domain` と `proxydata.etld1` の単体テスト (T14.54)。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

`/hosts` は `img.dlsite.jp` と `www.dlsite.jp` を別の行で持つので「dlsite 全体で何件か」が
読めない。ここで見るのは **eTLD+1 でまとめたときに 1 行になり、要求数が和になる**ことと、
3 ラベルの `co.jp` が正しくまとまることの 2 つ。
"""

import contextlib
import importlib.util
import io
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def _load(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


sd = _load("status_diff", "status-diff.py")
pd = _load("proxydata", "proxydata.py")


def host(key, requests, avg_ms=10.0, timed=None, **extra):
    """`/hosts` の 1 行 (足りない欄は 0)。"""
    row = {"host": key, "requests": requests, "hits": 0, "misses": 0, "bypass": requests,
           "errors": 0, "blocked": 0, "bytes": requests * 1000,
           "timed": requests if timed is None else timed, "avg_ms": avg_ms,
           "p50_ms": avg_ms, "p95_ms": avg_ms * 2, "max_ms": avg_ms * 10,
           "last_seen": 1789086000, "dns_ms_sum": 0, "dns_misses": 0,
           "connect_ms_sum": 0, "v4_wins": requests, "v6_wins": 0,
           "errors_by_cause": [0] * 8}
    row.update(extra)
    return row


HOSTS = {
    "schema": 1,
    "uptime_secs": 3600,
    "total_requests": 1000,
    "count": 6,
    "shown": 6,
    "hosts": [
        host("connect://img.dlsite.jp:443", 100, avg_ms=10.0),
        host("connect://www.dlsite.jp:443", 300, avg_ms=20.0),
        host("connect://www.dlsite.com:443", 7, avg_ms=30.0),
        host("connect://www.dmm.co.jp:443", 5, avg_ms=40.0),
        host("connect://a.b.example.io:443", 3, avg_ms=50.0),
        host("http://img.dlsite.jp:80", 2, avg_ms=60.0),
    ],
}


def run(argv):
    """`status-diff.py` を回して標準出力を文字列で受け取る。"""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        sd.main(argv)
    return out.getvalue()


@contextlib.contextmanager
def written(obj):
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "hosts.json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump(obj, f)
        yield path


class Etld1(unittest.TestCase):
    """まとめの単位 (eTLD+1 の近似)。"""

    def test_the_last_two_labels_are_the_unit(self):
        self.assertEqual(pd.etld1("img.dlsite.jp"), "dlsite.jp")
        self.assertEqual(pd.etld1("www.dlsite.jp"), "dlsite.jp")
        self.assertEqual(pd.etld1("a.b.example.io"), "example.io")
        self.assertEqual(pd.etld1("example.io"), "example.io")

    def test_a_different_tld_is_a_different_unit(self):
        # 本文の例は「同じ eTLD+1 のホスト同士」の話。`.jp` と `.com` は別の単位
        self.assertNotEqual(pd.etld1("img.dlsite.jp"), pd.etld1("www.dlsite.com"))
        self.assertEqual(pd.etld1("www.dlsite.com"), "dlsite.com")

    def test_three_labels_for_the_japanese_suffixes(self):
        self.assertEqual(pd.etld1("www.dmm.co.jp"), "dmm.co.jp")
        self.assertEqual(pd.etld1("a.b.example.ne.jp"), "example.ne.jp")
        self.assertEqual(pd.etld1("example.co.jp"), "example.co.jp")
        self.assertEqual(pd.etld1("www.bbc.co.uk"), "bbc.co.uk")

    def test_what_cannot_be_grouped_is_left_alone(self):
        for name in ("other", "localhost", "203.0.113.9", "2001:db8::1", ""):
            self.assertEqual(pd.etld1(name), name)
        self.assertEqual(pd.etld1("WWW.Example.COM"), "example.com", "小文字にする")
        self.assertEqual(pd.etld1("www.example.com."), "example.com", "末尾の点は落とす")


class Group(unittest.TestCase):
    """`proxydata.group_by_domain` の畳み方。"""

    def rows(self):
        rows = [pd.row_of(h["host"], h, None) for h in HOSTS["hosts"]]
        for r in rows:
            r["aaaa"] = None
        return pd.group_by_domain(rows)

    def test_the_same_unit_becomes_one_row_and_the_requests_are_summed(self):
        got = {r["name"]: r for r in self.rows() if r["connect"]}
        self.assertEqual(sorted(got), ["dlsite.com", "dlsite.jp", "dmm.co.jp", "example.io"])
        self.assertEqual(got["dlsite.jp"]["requests"], 400, "100 + 300")
        self.assertEqual(got["dlsite.jp"]["hosts"], 2)
        self.assertEqual(got["dlsite.jp"]["names"], ["www.dlsite.jp", "img.dlsite.jp"])
        self.assertEqual(got["dlsite.com"]["requests"], 7, "`.com` は別の単位")

    def test_the_average_is_weighted_by_the_timed_requests(self):
        got = {r["name"]: r for r in self.rows() if r["connect"]}
        # (10 ms × 100 + 20 ms × 300) ÷ 400
        self.assertAlmostEqual(got["dlsite.jp"]["avg_ms"], 17.5)
        self.assertAlmostEqual(got["dlsite.com"]["avg_ms"], 30.0)

    def test_the_quantiles_are_dropped_when_two_hosts_are_folded(self):
        got = {r["name"]: r for r in self.rows() if r["connect"]}
        self.assertIsNone(got["dlsite.jp"]["p50_ms"], "分位点は足せない")
        self.assertIsNone(got["dlsite.jp"]["p95_ms"])
        self.assertEqual(got["dlsite.jp"]["max_ms"], 200.0, "最大は最大")
        self.assertEqual(got["dlsite.com"]["p50_ms"], 30.0, "1 ホストならそのまま")

    def test_connect_and_forward_are_not_mixed(self):
        rows = self.rows()
        fwd = [r for r in rows if not r["connect"]]
        self.assertEqual([(r["name"], r["requests"]) for r in fwd], [("dlsite.jp", 2)])
        con = [r for r in rows if r["connect"] and r["name"] == "dlsite.jp"][0]
        self.assertEqual(con["requests"], 400, "CONNECT 側は forward を含まない")


class Cli(unittest.TestCase):
    """`status-diff.py --group domain` の表。"""

    def test_the_table_folds_the_hosts_into_one_line(self):
        with written(HOSTS) as path:
            plain = run([path, "--no-dns"])
            grouped = run([path, "--no-dns", "--group", "domain"])
        # まとめない表はホスト 1 件ずつ
        self.assertIn("img.dlsite.jp", plain)
        self.assertIn("www.dlsite.jp", plain)
        # まとめた表は `dlsite.jp` の 1 行 (要求 400 = 100 + 300)。CONNECT と forward は
        # 別の表なので、CONNECT の節だけを見る
        self.assertNotIn("img.dlsite.jp", grouped)
        connect_part = grouped.split("== forward")[0]
        line = [x for x in connect_part.splitlines() if x.strip().startswith("dlsite.jp")]
        self.assertEqual(len(line), 1, grouped)
        self.assertIn("400", line[0])
        self.assertIn("17.5", line[0], "avg_ms は計測数で重みづけ")
        self.assertIn("== CONNECT (4 まとめの単位、5 ホスト) ==", grouped)
        self.assertIn("== forward (1 まとめの単位、1 ホスト) ==", grouped)
        # 3 ラベルと `.io`
        self.assertTrue(any(x.strip().startswith("dmm.co.jp") for x in grouped.splitlines()),
                        grouped)
        self.assertTrue(any(x.strip().startswith("example.io") for x in grouped.splitlines()),
                        grouped)

    def test_the_default_is_still_one_line_per_host(self):
        with written(HOSTS) as path:
            plain = run([path, "--no-dns"])
        self.assertNotIn("まとめの単位", plain)
        self.assertIn("== CONNECT (5 ホスト) ==", plain)


if __name__ == "__main__":
    unittest.main()
