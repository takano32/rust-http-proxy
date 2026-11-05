#!/usr/bin/env python3
"""`scripts/framing-probe.py` の判断 (判定・表の行・まとめ・ケースの表) の単体テスト (T34.136)。

    python3 -m unittest discover -s scripts        # リポジトリの根から

実機 (プロキシのバイナリ) は使わない。流す部分 (`probe_*`・`main`) は実機の印字で見る
(`python3 scripts/framing-probe.py --port <空き> target/release/rust-http-proxy`)。
"""

import unittest

import _testing

fp = _testing.load("framing_probe", "framing-probe.py")


class Judge(unittest.TestCase):
    def test_the_status_must_match(self):
        self.assertTrue(fp.judge(400, 400))
        self.assertFalse(fp.judge(200, 400))
        self.assertFalse(fp.judge(None, 400))

    def test_a_request_that_must_not_reach_the_origin_fails_when_it_does(self):
        self.assertTrue(fp.judge(400, 400, may_reach=False, reached=False))
        self.assertFalse(fp.judge(400, 400, may_reach=False, reached=True))
        self.assertTrue(fp.judge(200, 200, may_reach=True, reached=True))

    def test_the_check_needs_evidence_and_must_hold(self):
        has_a = lambda seen: "X-A: 1" in seen["headers"]  # noqa: E731
        self.assertTrue(fp.judge(200, 200, True, True, has_a, {"headers": ["X-A: 1"]}))
        self.assertFalse(fp.judge(200, 200, True, True, has_a, {"headers": []}))
        # 届かなかった (evidence が None) なら check は満たさない
        self.assertFalse(fp.judge(200, 200, True, False, has_a, None))

    def test_the_check_is_not_called_when_the_status_is_wrong(self):
        calls = []
        fp.judge(502, 200, check=lambda resp: calls.append(resp) or True, evidence=b"HTTP/1.1 502 x\r\n\r\n")
        self.assertEqual(calls, [])

    def test_an_empty_response_is_still_evidence_for_the_response_checks(self):
        # 応答側の check は空の応答 (b"") でも呼ぶ (割る前の `got == want and check(resp)` と同じ)
        self.assertTrue(fp.judge(None, None, check=lambda resp: resp == b"", evidence=b""))


class Rows(unittest.TestCase):
    def test_a_row_lines_up_the_columns(self):
        line = fp.row("CL が負 (-1)", 400, 400, "-", True)
        self.assertEqual(line, "%-28s %-8s %-8s %s" % ("CL が負 (-1)", 400, 400, "-"))

    def test_no_response_and_a_miss_are_marked(self):
        line = fp.row("x", None, 400, "-", False)
        self.assertIn("(無応答)", line)
        self.assertTrue(line.endswith("-   ← ちがう"))

    def test_bytes_are_shown_as_python_prints_them(self):
        # CONNECT の節はトンネル先が受け取ったバイト列をそのまま `%s` で出す
        self.assertTrue(fp.row("CONNECT 正常", 200, 200, b"ping", True).endswith(" b'ping'"))

    def test_the_heading(self):
        self.assertEqual(fp.heading("CONNECT", "トンネル先が受け取ったバイト"),
                         "%-28s %-8s %-8s %s" % ("CONNECT", "状態", "期待", "トンネル先が受け取ったバイト"))


class Summary(unittest.TestCase):
    def test_all_as_expected(self):
        self.assertEqual(fp.summarize(0), (["", "全部期待どおり。"], 0))

    def test_any_miss_fails(self):
        self.assertEqual(fp.summarize(3), (["", "期待とちがう: 3 本"], 1))


class Cases(unittest.TestCase):
    PORT = 19099

    def tables(self):
        return {
            "cases": (fp.cases(self.PORT), 4),
            "t27_cases": (fp.t27_cases(self.PORT), 5),
            "t32_cases": (fp.t32_cases(self.PORT), 5),
            "t27_response_cases": (fp.t27_response_cases(self.PORT), 4),
            "t32_response_cases": (fp.t32_response_cases(self.PORT, self.PORT + 2, self.PORT + 1), 4),
            "connect_cases": (fp.connect_cases(self.PORT + 1), 4),
        }

    def test_every_case_has_the_shape_its_section_reads(self):
        for name, (rows, width) in self.tables().items():
            for case in rows:
                self.assertEqual(len(case), width, (name, case[0]))
                self.assertIsInstance(case[0], str)
                self.assertIsInstance(case[1], bytes)
                self.assertIsInstance(case[2], int)

    def test_names_are_unique_within_the_printed_sections(self):
        t = self.tables()
        sections = [t["cases"][0], t["t27_cases"][0] + t["t32_cases"][0]
                    + t["t27_response_cases"][0] + t["t32_response_cases"][0], t["connect_cases"][0]]
        for rows in sections:
            names = [c[0] for c in rows]
            self.assertEqual(len(names), len(set(names)))

    def test_the_tables_are_the_same_on_every_call(self):
        # 表は port だけで決まる (時刻や乱数を混ぜない)
        self.assertEqual(fp.cases(self.PORT), fp.cases(self.PORT))
        self.assertEqual([c[:4] for c in fp.t27_cases(self.PORT)], [c[:4] for c in fp.t27_cases(self.PORT)])

    def test_the_origin_port_is_in_the_requests(self):
        self.assertIn(b"127.0.0.1:%d" % self.PORT, fp.cases(self.PORT)[0][1])


class Status(unittest.TestCase):
    def test_the_status_line_is_read(self):
        self.assertEqual(fp._status(b"HTTP/1.1 400 Bad Request\r\n\r\n"), 400)
        self.assertIsNone(fp._status(b""))
        self.assertIsNone(fp._status(b"garbage"))


if __name__ == "__main__":
    unittest.main()
