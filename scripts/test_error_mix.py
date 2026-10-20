#!/usr/bin/env python3
"""`scripts/error-mix.py` (T23.0) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から

入力は**その場で組んだ架空の**雪像 (`recent` / `errors` / `history["3600"]`)。本物の雪像は入れない。
時刻は `T0` からの相対で書き、窓と間隔の計算だけを見る。
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


em = _load("error_mix", "error-mix.py")

T0 = 1_800_000_000


def row(i, at, reason="client_eof", client="c1", target="a.example:443",
        kind="connect", dns=0):
    return {"id": i, "at": T0 + at, "client": client, "target": target, "kind": kind,
            "reason": reason, "ms": {"dns": dns, "connect": 10}}


def history(*rows):
    """(connects, errors_by_cause) の列。時刻は 1 時間ずつ。"""
    return {"3600": {"keys": ["t", "connects", "errors", "errors_by_cause"],
                     "samples": [[T0 + i * 3600, c, sum(e), list(e)] for i, (c, e) in enumerate(rows)]}}


def snap(recent=None, errors=None, hist=None):
    s = {"schema": 1, "taken_at": T0 + 1000, "version": "0.0.0+test"}
    if recent is not None:
        s["recent"] = {"schema": 1, "recent": recent, "count": len(recent), "kept": 2000}
    if errors is not None:
        s["errors"] = {"schema": 1, "errors": [], "count": errors[0], "kept": errors[1],
                       "recorded": errors[2]}
    if hist is not None:
        s["history"] = hist
    return s


def E8(**kw):
    return [kw.get(c, 0) for c in em.CAUSE_NAMES]


class Window(unittest.TestCase):
    def test_recent_window_and_errors_counts_are_reported(self):
        s = snap([row(1, 10), row(2, 100), row(3, 4600)], errors=(7, 5, 40))
        a = em.analyze(s)
        self.assertEqual((a["recent"]["from"], a["recent"]["until"]), (T0 + 10, T0 + 4600))
        self.assertAlmostEqual(a["recent"]["hours"], 4590 / 3600.0)
        self.assertEqual(a["recent"]["entries"], 3)
        self.assertEqual((a["errors"]["count"], a["errors"]["kept"]), (7, 5))

    def test_missing_recent_is_cannot_judge_not_zero(self):
        s = snap(hist=history((100, E8(dns=1))))
        a = em.analyze(s)
        self.assertIsNone(a["recent"])
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            em.main([_write(s)])
        text = out.getvalue()
        self.assertIn("`recent`: 判定できず", text)
        self.assertIn("### (2) 名前解決に成功した試行を分母にした率\n判定できず", text)
        self.assertIn("### (4) 宛先ごと", text)
        self.assertNotIn("試行 0", text)

    def test_missing_history_is_cannot_judge(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            em.main([_write(snap([row(1, 10)]))])
        self.assertIn("判定できず (`history[\"3600\"]` が無い)", out.getvalue())


class Yardstick(unittest.TestCase):
    def test_old_yardstick_is_all_causes_over_connects(self):
        s = snap(hist=history((100, E8(dns=10, refused=5)), (300, E8(timeout=20))))
        w = em.analyze(s)["history"]["whole"]
        self.assertEqual((w["rows"], w["connects"], w["errors"]), (2, 400, 35))
        self.assertAlmostEqual(w["rate"], 35 / 400)
        self.assertEqual(w["causes"]["dns"], 10)
        self.assertEqual(w["causes"]["timeout"], 20)

    def test_the_recent_slice_uses_only_overlapping_hours(self):
        # 1 時間の行が 22 本。recent の窓は行 21 の中だけ
        rows = [(100, E8())] * 21 + [(900, E8(dns=45))]
        s = snap([row(1, 21 * 3600 + 60, dns=1)], hist=history(*rows))
        a = em.analyze(s)
        self.assertEqual(a["history"]["whole"]["connects"], 3_000)
        s = a["history"]["recent_slice"]
        self.assertEqual((s["rows"], s["connects"], s["errors"]), (1, 900, 45))
        self.assertAlmostEqual(s["rate"], 45 / 900)


class Denominator(unittest.TestCase):
    def test_dns_failures_leave_the_denominator(self):
        rows = ([row(1, i, reason="error:dns") for i in range(2)]
                + [row(10 + i, 10 + i) for i in range(6)]
                + [row(20 + i, 20 + i, reason="error:timeout") for i in range(2)])
        r = em.analyze(snap(rows))["recent"]
        self.assertEqual((r["attempts"], r["dns"], r["denom"]), (10, 2, 8))
        self.assertEqual(r["conn_fail"], 2)
        self.assertAlmostEqual(r["dns_per_attempt"], 0.2)
        self.assertAlmostEqual(r["dns_per_resolved"], 0.25)
        self.assertAlmostEqual(r["conn_per_resolved"], 0.25)
        self.assertAlmostEqual(r["all_per_resolved"], 0.5)

    def test_zero_denominator_renders_a_dash(self):
        rows = [row(1, 10, reason="error:dns"), row(2, 20, reason="error:dns")]
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            em.main([_write(snap(rows))])
        self.assertIn("| 2 | 2 | 0 | 1.0000 | — | 0 | — | — |", out.getvalue())

    def test_http_rows_are_left_out_of_the_attempts(self):
        rows = [row(1, 10), row(2, 20, kind="http", reason=None, target="http://x/")]
        r = em.analyze(snap(rows))["recent"]
        self.assertEqual((r["entries"], r["attempts"], r["http_rows"]), (2, 1, 1))


class Merge(unittest.TestCase):
    def test_consecutive_failures_merge_by_interval_and_success_breaks_the_run(self):
        rows = [row(1, 0, reason="error:timeout"),
                row(2, 120, reason="error:timeout"),
                row(3, 200, reason="error:timeout"),
                row(4, 210),  # 成功で一続きが切れる
                row(5, 220, reason="error:timeout"),
                row(6, 125, reason="error:refused", target="b.example:443")]
        a = em.analyze(snap(rows))["recent"]["merged"]
        self.assertEqual(a[60]["conn"]["raw"], 5)
        self.assertEqual(a[60]["conn"]["groups"], 5, "0 / 120 / 200 / (成功) 220 / 別の宛先 125")
        self.assertEqual(a[300]["conn"]["groups"], 3, "0・120・200 が 1 件、220 が 1 件、別の宛先が 1 件")
        self.assertAlmostEqual(a[300]["conn"]["rate"], 3 / 6)

    def test_the_all_variant_counts_dns_failures_too(self):
        rows = [row(1, 0, reason="error:dns"), row(2, 30, reason="error:dns"),
                row(3, 60, reason="error:timeout")]
        m = em.analyze(snap(rows))["recent"]["merged"]
        self.assertEqual((m[60]["conn"]["raw"], m[60]["conn"]["groups"]), (1, 1))
        self.assertEqual((m[60]["all"]["raw"], m[60]["all"]["groups"]), (3, 1))
        self.assertEqual(m[60]["conn"]["rate"], 1.0, "分母は試行 − dns = 1")
        self.assertEqual(m[60]["all"]["rate"], 1 / 3, "分母は試行 3")

    def test_different_clients_do_not_merge(self):
        rows = [row(1, 0, reason="error:timeout", client="c1"),
                row(2, 10, reason="error:timeout", client="c2")]
        m = em.analyze(snap(rows))["recent"]["merged"][300]["conn"]
        self.assertEqual((m["raw"], m["groups"]), (2, 2))

    def test_failures_at_the_same_second_do_not_crash(self):
        rows = [row(1, 0, reason="error:dns"), row(2, 0, reason="error:timeout")]
        m = em.analyze(snap(rows))["recent"]["merged"][60]
        self.assertEqual((m["all"]["raw"], m["all"]["groups"]), (2, 1))


class Targets(unittest.TestCase):
    def test_counts_by_target_and_top_20(self):
        rows = []
        i = 0
        for n in range(25):
            for _ in range(25 - n):
                i += 1
                rows.append(row(i, i * 10, target=f"t{n:02d}.example:443"))
        rows.append(row(1000, 500, reason="error:dns", target="bad.example:995"))
        rows.append(row(1001, 510, reason="error:dns", target="bad.example:995"))
        rows.append(row(1002, 520, reason="error:timeout", target="bad.example:995"))
        rows.append(row(1003, 530, reason="error:refused", target="bad.example:995"))
        rows.append(row(1004, 540, reason="error:unreachable", target="bad.example:995"))
        table = em.by_target([r for r in rows if r["kind"] == "connect"])
        self.assertEqual(table[0]["target"], "t00.example:443")
        self.assertEqual(table[0]["attempts"], 25)
        self.assertEqual(len(em.by_target(rows)[:em.TOP_TARGETS]), 20)
        bad = next(c for c in table if c["target"] == "bad.example:995")
        self.assertEqual((bad["attempts"], bad["success"], bad["dns"], bad["timeout"],
                          bad["refused"], bad["other"]), (5, 0, 2, 1, 1, 1))


class Main(unittest.TestCase):
    def test_four_tables_with_windows_and_json(self):
        rows = [row(1, 10, reason="error:timeout", target="a.example:443"),
                row(2, 20), row(3, 30, reason="error:dns", target="b.example:443")]
        s = snap(rows, errors=(3, 2, 9), hist=history((50, E8(timeout=5))))
        p = _write(s)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(em.main([p]), 0)
        text = out.getvalue()
        for want in ("### (1) いまの物差し", "### (2) 名前解決に成功した試行を分母にした率",
                     "### (3) 接続元×宛先の連続した失敗", "### (4) 宛先ごと",
                     "窓 2027-01-15", "count 3 / kept 2"):
            self.assertIn(want, text)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(em.main([p, "--json"]), 0)
        got = json.loads(out.getvalue())[p]
        self.assertEqual(got["recent"]["attempts"], 3)
        self.assertEqual(got["history"]["whole"]["rate"], 0.1)


def _write(s):
    d = tempfile.mkdtemp(prefix="t230-")
    p = os.path.join(d, "snap.json")
    with open(p, "w", encoding="utf-8") as f:
        json.dump(s, f)
    return p


if __name__ == "__main__":
    unittest.main()
