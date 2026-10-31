#!/usr/bin/env python3
"""`scripts/watch-deployed.py` (T.28.26) の単体テスト。偽のサーバーを 127.0.0.1 に立てて `main` も回す。"""

import contextlib
import importlib.util
import io
import json
import os
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("watch_deployed", os.path.join(HERE, "watch-deployed.py"))
wd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wd)

NOW = 1_800_000_000


def start(at, version="0.28.0+abcdef0"):
    return {"at": at, "kind": "start", "text": "version %s on port 60357 boot aaaa" % version}


def stop(at):
    return {"at": at, "kind": "shutdown", "text": "stop signal received; saving statistics"}


HEALTHY = (200, {"ok": True, "checks": {"fds": {"ok": True}}})
STATUS = (200, {"version": "0.28.0+abcdef0", "uptime_secs": 7200})


class Judge(unittest.TestCase):
    def test_healthy_and_graceful_restart_is_ok(self):
        ev = (200, {"events": [start(NOW - 90000), stop(NOW - 600), start(NOW - 560)]})
        problems, version, uptime = wd.judge(HEALTHY, ev, STATUS, NOW, 75)
        self.assertEqual(problems, [])
        self.assertEqual((version, uptime), ("0.28.0+abcdef0", 7200))

    def test_silent_restart_in_the_window_fails(self):
        ev = (200, {"events": [start(NOW - 90000), {"at": NOW - 700, "kind": "anomaly", "text": "x"},
                               start(NOW - 600)]})
        problems, _, _ = wd.judge(HEALTHY, ev, STATUS, NOW, 75)
        self.assertEqual(len(problems), 1)
        self.assertIn("無言の再起動が 1 回", problems[0])

    def test_old_silent_restart_is_outside_the_window(self):
        ev = (200, {"events": [start(NOW - 90000), start(NOW - 80000)]})
        self.assertEqual(wd.judge(HEALTHY, ev, STATUS, NOW, 75)[0], [])

    def test_unhealthy_names_the_failed_checks(self):
        h = (503, {"ok": False, "checks": {"fds": {"ok": False}, "cpu": {"ok": True}}})
        problems, _, _ = wd.judge(h, (200, {"events": []}), STATUS, NOW, 75)
        self.assertEqual(problems, ["/healthz が ok でない (状態 503、落ちた検査: fds)"])

    def test_unreadable_parts_fail(self):
        problems, _, _ = wd.judge((None, None), (None, None), (None, None), NOW, 75)
        self.assertEqual(len(problems), 3)


class Since(unittest.TestCase):
    """T.32.25: 窓は前回の成功した見張りから (GitHub の schedule は実際には 2〜7.5 時間おき)。"""

    def test_a_silent_restart_between_runs_7_5_hours_apart_is_caught(self):
        last = NOW - int(7.5 * 3600)
        ev = (200, {"events": [start(NOW - 90000), {"at": NOW - 6 * 3600, "kind": "anomaly", "text": "x"},
                               start(NOW - 5 * 3600)]})
        window, from_since = wd.window_minutes(last, NOW, wd.DEFAULT_WINDOW_MIN)
        self.assertTrue(from_since)
        self.assertEqual(window, 450 + 1 + 15)
        problems, _, _ = wd.judge(HEALTHY, ev, STATUS, NOW, window)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("無言の再起動が 1 回", problems[0])
        # 前の固定の 75 分では見えなかった
        self.assertEqual(wd.judge(HEALTHY, ev, STATUS, NOW, 75)[0], [])

    def test_the_run_just_after_the_last_one_still_looks_back_75_minutes(self):
        self.assertEqual(wd.window_minutes(NOW - 600, NOW, 480), (75, True))

    def test_since_is_read_as_iso_or_epoch_and_falls_back(self):
        self.assertEqual(wd.parse_since("2027-01-15T08:00:00Z"), 1800000000.0)
        self.assertEqual(wd.parse_since("1800000000"), 1800000000.0)
        self.assertIsNone(wd.parse_since("yesterday"))
        self.assertIsNone(wd.parse_since(""))
        self.assertEqual(wd.window_minutes(None, NOW, wd.DEFAULT_WINDOW_MIN), (480, False))
        self.assertEqual(wd.window_minutes(NOW + 60, NOW, 480), (480, False))  # 未来は信じない


class Main(unittest.TestCase):
    def serve(self, healthz, events=None):
        class H(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                code, body = {"/healthz": healthz, "/events": events or (200, {"events": []}),
                              "/status": STATUS}[self.path.split("?")[0]]
                data = json.dumps(body).encode()
                self.send_response(code)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        return "127.0.0.1:%d" % srv.server_address[1]

    def run_main(self, target, *extra):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = wd.main([target, "--timeout", "5", *extra])
        return code, buf.getvalue()

    def test_since_widens_the_window(self):
        import time
        now = time.time()
        ev = (200, {"events": [start(now - 90000), start(now - 5 * 3600)]})
        target = self.serve(HEALTHY, ev)
        last = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now - 7.5 * 3600))
        code, out = self.run_main(target, "--since", last)
        self.assertEqual(code, 1, out)
        self.assertIn("無言の再起動が 1 回", out)
        code, out = self.run_main(target, "--since", "garbage", "--window", "75")
        self.assertEqual(code, 0, out)
        self.assertIn("--since が読めない", out)

    def test_ok_prints_only_the_summary(self):
        code, out = self.run_main(self.serve(HEALTHY))
        self.assertEqual(code, 0, out)
        self.assertIn("版 0.28.0+abcdef0 / 起動から 2.0 時間", out)
        self.assertIn("OK:", out)

    def test_unhealthy_exits_1(self):
        code, out = self.run_main(self.serve((503, {"ok": False, "checks": {}})))
        self.assertEqual(code, 1)
        self.assertIn("NG: /healthz が ok でない (状態 503)", out)


if __name__ == "__main__":
    unittest.main()
