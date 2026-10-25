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


class Main(unittest.TestCase):
    def serve(self, healthz):
        class H(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                code, body = {"/healthz": healthz, "/events": (200, {"events": []}),
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

    def run_main(self, target):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = wd.main([target, "--timeout", "5"])
        return code, buf.getvalue()

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
