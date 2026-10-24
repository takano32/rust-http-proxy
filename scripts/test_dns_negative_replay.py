#!/usr/bin/env python3
"""`scripts/dns-negative-replay.py` (T.27.3 段 1) の単体テスト。雪像は架空のものをその場で作る。"""

import contextlib
import importlib.util
import io
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("dns_negative_replay", os.path.join(HERE, "dns-negative-replay.py"))
dnr = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dnr)

T0 = 1_792_000_000


def log(at, target, why=dnr.NXDOMAIN):
    return {"at": at, "level": "warn", "conn": 1, "msg": f"502 Bad Gateway: connect {target} failed: {why}"}


def snap(lines, recent=(), hosts=(), taken=None):
    return {"taken_at": taken or (max(x["at"] for x in lines) if lines else T0),
            "log": {"lines": list(lines)}, "recent": {"recent": list(recent)},
            "hosts": {"hosts": list(hosts)}, "errors": {"errors": []}}


class Replay(unittest.TestCase):
    def test_a_name_asked_every_30_seconds(self):
        # 30 秒ごとに 1 時間: 60 秒なら 2 本に 1 本、3,600 秒なら最初の 1 本だけ問い合わせる
        arr = [(T0 + 30 * i, "imap.example") for i in range(120)]
        self.assertEqual(dnr.replay(arr, 60)[:2], (60, 60))
        self.assertEqual(dnr.replay(arr, 3600)[:2], (1, 119))

    def test_the_boundary_is_asked_again(self):
        # 覚えるのは `t − f < N` まで。ちょうど N 秒後は問い合わせ直す
        arr = [(T0, "a.example"), (T0 + 59, "a.example"), (T0 + 60, "a.example")]
        self.assertEqual(dnr.replay(arr, 60)[:2], (2, 1))

    def test_names_are_kept_apart(self):
        arr = [(T0, "a.example"), (T0 + 1, "b.example"), (T0 + 2, "a.example")]
        self.assertEqual(dnr.replay(arr, 60)[:2], (2, 1))


class Run(unittest.TestCase):
    def test_only_nxdomain_lines_are_arrivals(self):
        lines = [log(T0, "imap.example:993"), log(T0 + 1, "imap.example:993", "Temporary failure in name resolution"),
                 log(T0 + 2, "[2001:db8::1]:443", "Connection refused"), {"at": T0, "msg": "something else"}]
        arr, other = dnr.arrivals(snap(lines))
        self.assertEqual(arr, [(T0, "imap.example")])
        self.assertEqual(other, 0)

    def test_saved_wait_and_the_verdict(self):
        # 1 時間に 30 秒おき = 120 本。1 回 50 ms。3,600 秒にすると 59 回減って 2.95 秒 = 1 日 70.8 秒
        lines = [log(T0 + 30 * i, "imap.example:993") for i in range(120)]
        recent = [{"at": T0 + 30 * i, "target": "imap.example:993", "reason": "error:nxdomain",
                   "ms": {"dns": 50 if i % 2 == 0 else 0}} for i in range(120)]
        out = dnr.run(snap(lines, recent, taken=T0 + 3600), [60, 3600])
        base, long_ = out["cases"]
        self.assertEqual((base["lookups"], long_["lookups"], long_["fewer_lookups"]), (60, 1, 59))
        self.assertAlmostEqual(out["avg_ms"], 50.0)
        self.assertAlmostEqual(long_["saved_secs"], 2.95)
        self.assertAlmostEqual(long_["saved_secs_per_day"], 70.8)
        self.assertTrue(long_["go"])
        self.assertFalse(base["go"])
        # 突き合わせ: 実機の問い合わせ (`ms.dns` > 0) 60 回と、模型の 60 秒の回数
        self.assertEqual((out["calibration"]["looked_up"], out["calibration"]["model_lookups"]), (60, 60))

    def test_a_success_while_remembered_is_a_false_refusal(self):
        # NXDOMAIN のあと 100 秒で引けるようになった名前: 60 秒なら断らない、300 秒なら断る
        lines = [log(T0, "flip.example:443")]
        recent = [{"at": T0 + 100, "target": "flip.example:443", "reason": "client_eof", "ms": {"dns": 20}}]
        hosts = [{"host": "connect://flip.example:443", "requests": 3, "errors": 1}]
        out = dnr.run(snap(lines, recent, hosts, taken=T0 + 200), [60, 300])
        self.assertEqual(out["cases"][0]["false_refusals"], [0, 2])
        self.assertEqual(out["cases"][1]["false_refusals"], [1, 3])
        self.assertFalse(out["cases"][1]["go"])
        self.assertEqual(out["names_with_success"], ["flip.example"])

    def test_render_prints_the_table(self):
        lines = [log(T0 + 30 * i, "imap.example:993") for i in range(10)]
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            dnr.render(dnr.run(snap(lines), [60, 300]), 3)
        text = buf.getvalue()
        self.assertIn("| 300 秒 |", text)
        self.assertIn("`imap.example` 10", text)


if __name__ == "__main__":
    unittest.main()
