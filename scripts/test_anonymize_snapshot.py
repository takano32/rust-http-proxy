#!/usr/bin/env python3
"""`scripts/anonymize-snapshot.py` (T14.35) の単体テスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

架空の雪像 (下の `sample()`) で「**名前と IP と UA だけが変わり、数字は 1 つも変わらない**」
「同じ入力からは同じ出力」を見る。あわせて、**コミットしてある匿名化済みの実データ**
(`testdata/deployed-2026-09-16.anon.json`) に元の名前が残っていないことも見る
(実データそのものはリポジトリに入れないので、突き合わせは作った人が 1 回やる。ここでは
「匿名化済みの形になっているか」だけを見る)。
"""

import importlib.util
import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stderr

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "testdata")
FIXTURE = os.path.join(DATA, "deployed-2026-09-16.anon.json")


def _load(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


an = _load("anonymize_snapshot", "anonymize-snapshot.py")
pd = _load("proxydata", "proxydata.py")


def sample():
    """架空の雪像 1 枚 (宛先は `.example.net`、接続元は置き換えの対象になる範囲)。"""
    return {
        "taken_at": 1789520760,
        "version": "0.1.0+abcdef1",
        "uptime_secs": 3600,
        "parts": ["status", "history.5", "dns", "errors", "connections", "clients", "log",
                  "events"],
        "dropped": [],
        "status": {
            "status": "ok",
            "version": "0.1.0+abcdef1",
            "uptime_secs": 3600,
            "total_requests": 30,
            "hosts": [
                {"host": "connect://one.example.net:443", "requests": 20, "timed": 20,
                 "avg_ms": 7.5, "errors": 1, "bytes": 4096, "dns_ms_sum": 120, "dns_misses": 10,
                 "connect_ms_sum": 150, "errors_by_cause": [1, 0, 0, 0, 0, 0, 0, 0]},
                {"host": "http://two.example.net:80", "requests": 6, "timed": 6, "avg_ms": 12.0,
                 "errors": 0, "bytes": 512, "dns_ms_sum": 0, "dns_misses": 0,
                 "connect_ms_sum": 30, "errors_by_cause": [0] * 8},
                {"host": "connect://[2400:2410:abcd::1]:443", "requests": 3, "timed": 3,
                 "avg_ms": 30.0, "errors": 0, "bytes": 0, "dns_ms_sum": 0, "dns_misses": 0,
                 "connect_ms_sum": 90, "errors_by_cause": [0] * 8},
                {"host": "connect://93.184.216.34:443", "requests": 1, "timed": 1, "avg_ms": 9.0,
                 "errors": 0, "bytes": 0, "dns_ms_sum": 0, "dns_misses": 0, "connect_ms_sum": 9,
                 "errors_by_cause": [0] * 8},
                {"host": "connect://intranet:8080", "requests": 2, "timed": 2, "avg_ms": 1.0,
                 "errors": 0, "bytes": 0, "dns_ms_sum": 0, "dns_misses": 0, "connect_ms_sum": 2,
                 "errors_by_cause": [0] * 8},
                {"host": "other", "requests": 5, "timed": 5, "avg_ms": 4.0, "errors": 0,
                 "bytes": 64, "dns_ms_sum": 0, "dns_misses": 0, "connect_ms_sum": 5,
                 "errors_by_cause": [0] * 8},
            ],
            "clients": [
                {"client": "100.64.3.9", "requests": 25, "bytes": 4096, "avg_ms": 8.0},
                {"client": "2400:2410:1234::9", "requests": 5, "bytes": 512, "avg_ms": 9.0},
                {"client": "other", "requests": 1, "bytes": 0, "avg_ms": 1.0},
            ],
            "dns": {"ttl_secs": 60, "hits": 18, "misses": 12, "miss_ms_sum": 150.0,
                    "miss_avg_ms": 12.5, "refreshes": 4},
            "settings": {"path": "/home/container/.env", "reloads": 0},
            "kernel": {
                "at": 1789520760,
                "cgroup_cpu": {
                    "nr_throttled": 41282, "nr_periods": 41905, "quota_cores": 1.0,
                    # コンテナを 1 つに特定できる道 (T15.0 (6))
                    "path": "/sys/fs/cgroup/system.slice/pterodactyl-6f2c.scope/cpu.stat",
                },
            },
        },
        "history": {"5": {
            "interval_secs": 5,
            "keys": ["t", "requests", "connects", "connect_ms_sum", "connect_ms_max",
                     "connect_buckets", "errors", "errors_by_cause"],
            "bounds_ms": [1, 2, 4, 8],
            "causes": ["dns", "refused", "unreachable", "timeout", "reset", "tls", "loop",
                       "other"],
            "samples": [
                [1789520755, 20, 4, 40, 15, [0, 0, 2, 1, 1], 1, [1, 0, 0, 0, 0, 0, 0, 0]],
                [1789520760, 10, 2, 18, 9, [0, 1, 1, 0, 0], 0, [0] * 8],
            ],
            "canary": {"keys": ["t", "canary_dns_ms", "canary_connect_ms", "canary_host"],
                       "samples": [[1789520755, 3, 8, "one.example.net:443"]]},
        }},
        "dns": {
            "entries": [
                {"host": "one.example.net", "addrs": ["93.184.216.34", "2400:2410:abcd::1"],
                 "addr_count": 2, "age_secs": 1, "ttl_left": 58, "misses": 10, "refreshes": 4},
                {"host": "two.example.net", "addrs": ["100.100.7.7"], "addr_count": 1,
                 "age_secs": 30, "ttl_left": 30, "misses": 2, "refreshes": 0},
            ],
            "count": 2, "ttl_secs": 60,
        },
        "errors": {
            "errors": [
                {"at": 1789520700, "kind": "connect", "target": "three.example.net:443",
                 "cause": "dns", "dns_ms": 2013, "connect_ms": 0, "status": 502,
                 "client": "100.64.3.9"},
            ],
            "count": 1, "kept": 1, "capacity": 500, "recorded": 1, "truncated": False,
        },
        "connections": {
            "connections": [
                {"id": 3, "client": "100.64.3.9:51514", "target": "one.example.net:443",
                 "kind": "connect", "state": "relaying", "age_secs": 120, "bytes": 4096,
                 "fds": 2},
                {"id": 4, "client": "[2400:2410:1234::9]:51515", "target": "",
                 "kind": "http", "state": "serving", "age_secs": 0, "bytes": 0, "fds": 1},
            ],
            "count": 2, "shown": 2, "truncated": False, "lite": False,
        },
        "clients": {
            "clients": [
                {"client": "100.64.3.9", "requests": 25,
                 "agents": ["curl/8.5.0", "Mozilla/5.0 (X11; Linux x86_64) Firefox/140.0"],
                 "agents_dropped": 0, "ports": [443, 80], "rejected": 0},
            ],
            "count": 1,
        },
        "log": {
            "lines": [
                {"at": 1789520701, "level": "warn", "conn": 12,
                 "msg": "connect one.example.net:443 from 100.64.3.9:51514 failed after 250 ms"
                        " (dns 12.5 ms, version 0.1.0+abcdef1, see dashboard.html at 10:30:00)"},
                {"at": 1789520702, "level": "info", "conn": None,
                 "msg": "parked a tunnel to intranet:8080 for [2400:2410:1234::9]"},
            ],
            "count": 2, "kept": 2, "capacity": 1000, "recorded": 2, "level": "info",
        },
        "events": {
            "events": [
                {"at": 1789520703, "kind": "blocklist", "text": "blocked ads.example.org (2)"},
            ],
            "count": 1, "kinds": ["start", "blocklist"],
        },
    }


def anonymized(snap=None):
    return an.Anonymizer().run(snap if snap is not None else sample())


def numbers(node, path="", acc=None):
    """文字列でない葉 (数・真偽・null) を出てくる順に集める。"""
    acc = [] if acc is None else acc
    if isinstance(node, dict):
        for k, v in node.items():
            numbers(v, path + "." + k, acc)
    elif isinstance(node, list):
        for v in node:
            numbers(v, path + "[]", acc)
    elif not isinstance(node, str):
        acc.append((path, node))
    return acc


def strings(node, key=None, acc=None):
    acc = [] if acc is None else acc
    if isinstance(node, dict):
        for k, v in node.items():
            strings(v, k, acc)
    elif isinstance(node, list):
        for v in node:
            strings(v, key, acc)
    elif isinstance(node, str):
        acc.append((key, node))
    return acc


class Hosts(unittest.TestCase):
    def test_host_names_are_numbered_in_the_order_they_appear(self):
        out = anonymized()
        got = [h["host"] for h in out["status"]["hosts"]]
        # `gNNNN` は**まとめの単位 (eTLD+1)** の番号。この 2 つはどちらも
        # `example.net` なので同じ単位に落ちる (T14.54)
        self.assertEqual(got[0], "connect://host-0001.g0001.example:443")
        self.assertEqual(got[1], "http://host-0002.g0001.example:80")

    def test_the_scheme_and_the_port_are_kept(self):
        out = anonymized()
        for row in out["status"]["hosts"]:
            self.assertRegex(row["host"], r"^(connect|http)://|^(other|\[|host-|203\.0\.113\.)")
        self.assertTrue(out["status"]["hosts"][1]["host"].endswith(":80"))
        self.assertTrue(out["status"]["hosts"][4]["host"].endswith(":8080"))

    def test_an_ipv6_literal_target_stays_an_ipv6_literal(self):
        host = anonymized()["status"]["hosts"][2]["host"]
        self.assertEqual(host, "connect://[2001:db8:1::1]:443")

    def test_an_ip_literal_target_becomes_an_ip_not_a_name(self):
        # 宛先が IP リテラルのときは答えの IP と同じ表を使う (名前に化けさせない)
        self.assertEqual(anonymized()["status"]["hosts"][3]["host"],
                         "connect://203.0.113.1:443")

    def test_the_overflow_row_named_other_is_kept(self):
        # `crates/metrics/src/metrics.rs` の `MAX_HOSTS` を越えた分をまとめる行
        out = anonymized()
        self.assertEqual(out["status"]["hosts"][5]["host"], "other")
        self.assertEqual(out["status"]["clients"][2]["client"], "other")

    def test_the_same_name_gets_the_same_replacement_everywhere(self):
        out = anonymized()
        first = out["status"]["hosts"][0]["host"]           # connect://host-0001.example:443
        name = first.split("://", 1)[1].rsplit(":", 1)[0]
        self.assertEqual(out["dns"]["entries"][0]["host"], name)
        self.assertEqual(out["connections"]["connections"][0]["target"], name + ":443")
        self.assertEqual(out["history"]["5"]["canary"]["samples"][0][3], name + ":443")


class Addresses(unittest.TestCase):
    def test_clients_use_the_documentation_ranges(self):
        out = anonymized()
        self.assertEqual(out["status"]["clients"][0]["client"], "198.51.100.1")
        self.assertEqual(out["status"]["clients"][1]["client"], "2001:db8::1")

    def test_a_client_with_a_port_keeps_the_port(self):
        rows = anonymized()["connections"]["connections"]
        self.assertEqual(rows[0]["client"], "198.51.100.1:51514")
        self.assertEqual(rows[1]["client"], "[2001:db8::1]:51515")

    def test_dns_answers_use_a_different_range_than_clients(self):
        entries = anonymized()["dns"]["entries"]
        self.assertEqual(entries[0]["addrs"], ["203.0.113.1", "2001:db8:1::1"])
        self.assertEqual(entries[1]["addrs"], ["203.0.113.2"])

    def test_agents_become_ua_numbers(self):
        self.assertEqual(anonymized()["clients"]["clients"][0]["agents"], ["ua-01", "ua-02"])


class Grouping(unittest.TestCase):
    """**まとめの粒度が残る** (T14.54)。匿名化しても `--group domain` が同じ形にまとまる。"""

    def test_the_same_etld1_lands_in_the_same_unit(self):
        snap = {"parts": ["hosts"], "hosts": {"hosts": [
            {"host": "connect://img.dlsite.jp:443", "requests": 100},
            {"host": "connect://www.dlsite.jp:443", "requests": 300},
            {"host": "connect://www.dlsite.com:443", "requests": 7},
            {"host": "connect://www.dmm.co.jp:443", "requests": 5},
            {"host": "http://a.b.example.io:80", "requests": 2},
        ]}}
        before = [h["host"] for h in snap["hosts"]["hosts"]]
        got = [h["host"] for h in an.Anonymizer().run(snap)["hosts"]["hosts"]]
        # 元とは 1 つも同じ名前が残っていない
        self.assertFalse(set(before) & set(got))
        units_before = [pd.etld1(pd.host_name(k)) for k in before]
        units_after = [pd.etld1(pd.host_name(k)) for k in got]
        # 「同じ単位か」の組み合わせが元と後で一致する (dlsite.jp の 2 件だけが同じ)
        def pairs(units):
            return {(i, j) for i in range(len(units)) for j in range(i + 1, len(units))
                    if units[i] == units[j]}
        self.assertEqual(pairs(units_after), pairs(units_before))
        self.assertEqual(pairs(units_after), {(0, 1)})
        self.assertEqual(len(set(units_after)), 4, "単位の数も同じ")
        self.assertEqual(units_after[0], "g0001.example")


class Text(unittest.TestCase):
    def test_a_log_line_keeps_everything_but_the_host_and_the_ip(self):
        out = anonymized()
        msg = out["log"]["lines"][0]["msg"]
        self.assertEqual(
            msg,
            "connect host-0001.g0001.example:443 from 198.51.100.1:51514 failed after 250 ms"
            " (dns 12.5 ms, version 0.1.0+abcdef1, see dashboard.html at 10:30:00)")

    def test_a_dotless_host_and_a_bracketed_ipv6_in_a_log_line(self):
        msg = anonymized()["log"]["lines"][1]["msg"]
        self.assertEqual(msg,
                         "parked a tunnel to host-0003.g0002.example:8080 for [2001:db8::1]")

    def test_a_name_that_only_shows_up_in_a_line_is_numbered_too(self):
        text = anonymized()["events"]["events"][0]["text"]
        self.assertEqual(text, "blocked host-0005.g0003.example (2)")

    def test_a_path_and_a_version_are_not_touched(self):
        self.assertEqual(anonymized()["status"]["settings"]["path"], "/home/container/.env")
        self.assertEqual(anonymized()["version"], "0.1.0+abcdef1")

    def test_the_cgroup_path_is_flattened(self):
        """`kernel.cgroup_cpu.path` はコンテナを特定できるので深さだけ残す (T15.0 (6))。"""
        cpu = anonymized()["status"]["kernel"]["cgroup_cpu"]
        self.assertEqual(cpu["path"], "/sys/fs/cgroup/…/…/cpu.stat")
        self.assertNotIn("pterodactyl", json.dumps(anonymized(), ensure_ascii=False))
        self.assertEqual(cpu["nr_throttled"], 41282)  # 数字は 1 つも変わらない

    def test_the_cgroup_path_survives_a_second_pass(self):
        once = anonymized()
        twice = an.Anonymizer().run(json.loads(json.dumps(once)))
        self.assertEqual(twice["status"]["kernel"]["cgroup_cpu"]["path"],
                         "/sys/fs/cgroup/…/…/cpu.stat")

    def test_a_cgroup_path_outside_the_usual_root_is_flattened_too(self):
        got = an.Anonymizer().run(
            {"kernel": {"cgroup_cpu": {"path": "/somewhere/else/cpu.stat"}}})
        self.assertEqual(got["kernel"]["cgroup_cpu"]["path"], "/…/…/cpu.stat")


class Numbers(unittest.TestCase):
    def test_not_one_number_changes(self):
        before, after = sample(), anonymized()
        self.assertEqual(numbers(before), numbers(after))

    def test_the_sums_that_the_tools_read_are_the_same(self):
        before, after = sample(), anonymized()
        for snap in (before, after):
            snap["_sum"] = sum(h["requests"] for h in snap["status"]["hosts"])
        self.assertEqual(before["_sum"], after["_sum"])
        self.assertEqual([h["avg_ms"] for h in before["status"]["hosts"]],
                         [h["avg_ms"] for h in after["status"]["hosts"]])
        self.assertEqual(before["status"]["hosts"][0]["errors_by_cause"],
                         after["status"]["hosts"][0]["errors_by_cause"])
        self.assertEqual(before["history"]["5"]["samples"], after["history"]["5"]["samples"])

    def test_only_the_names_and_the_ips_change(self):
        pairs = list(zip(strings(sample()), strings(anonymized())))
        changed = [a for (ka, a), (kb, b) in pairs if a != b]
        kept = [a for (ka, a), (kb, b) in pairs if a == b]
        # 置き換わるのは 25 か所 (宛先 7・接続元 6・答えの IP 3・UA 2・canary 1・文 3 ほか、
        # それに cgroup の道 1 = T15.0 (6))
        self.assertEqual(len(changed), 25)
        self.assertIn("ok", kept)          # `/status` の `status`
        self.assertIn("connect", kept)     # 種類
        self.assertIn("dns", kept)         # 原因の名前


class Deterministic(unittest.TestCase):
    def test_the_same_input_gives_the_same_output(self):
        self.assertEqual(anonymized(), anonymized())

    def test_running_it_on_its_own_output_changes_nothing(self):
        once = anonymized()
        twice = an.Anonymizer().run(json.loads(json.dumps(once)))
        self.assertEqual(once, twice)

    def test_a_number_that_is_already_taken_is_not_handed_out_twice(self):
        snap = sample()
        # 入力に匿名化済みの名前が混ざっていても、別の名前とぶつからない
        snap["status"]["hosts"][1]["host"] = "connect://host-0001.g0001.example:443"
        out = an.Anonymizer().run(snap)
        got = [h["host"] for h in out["status"]["hosts"][:2]]
        self.assertEqual(got[1], "connect://host-0001.g0001.example:443")
        self.assertNotEqual(got[0], got[1])

    def test_the_old_names_without_a_group_are_left_alone(self):
        # T14.35 の最初の版が作った fixture (`host-0001.example`) を通しても変わらない
        snap = sample()
        snap["status"]["hosts"][1]["host"] = "connect://host-0009.example:443"
        got = an.Anonymizer().run(snap)["status"]["hosts"][1]["host"]
        self.assertEqual(got, "connect://host-0009.example:443")


class Bundle(unittest.TestCase):
    """`/snapshot` より前の形 (1 本ずつ取ったファイル群) から組める。"""

    def files(self, tmp):
        snap = sample()
        names = {
            "2026-09-16T0106Z-status": snap["status"],
            "2026-09-16T0106Z-history_res_5": snap["history"]["5"],
            "2026-09-16T0106Z-dns_sort_misses_limit_300": snap["dns"],
            "2026-09-16T0106Z-errors_n_500": snap["errors"],
            "2026-09-16T0106Z-connections": snap["connections"],
            "2026-09-16T0106Z-log_n_500": snap["log"],
        }
        for name, body in names.items():
            with open(os.path.join(tmp, name), "w", encoding="utf-8") as f:
                json.dump(body, f)
        # JSON でないもの (`-metrics` `-dashboard`) は黙って飛ばす
        with open(os.path.join(tmp, "2026-09-16T0106Z-metrics"), "w", encoding="utf-8") as f:
            f.write("# HELP proxy_requests_total\nproxy_requests_total 30\n")
        return sorted(os.path.join(tmp, n) for n in list(names) + ["2026-09-16T0106Z-metrics"])

    def test_the_bundle_becomes_a_snapshot(self):
        with tempfile.TemporaryDirectory() as tmp:
            snap = an.load_inputs(self.files(tmp))
        self.assertEqual(snap["taken_at"], 1789520760)  # 名前の UTC 時刻から
        self.assertEqual(snap["version"], "0.1.0+abcdef1")
        self.assertEqual(snap["parts"],
                         ["status", "history.5", "dns", "errors", "connections", "log"])
        self.assertEqual(list(snap["history"]), ["5"])
        self.assertEqual(len(snap["status"]["hosts"]), 6)

    def test_the_command_line_writes_one_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "out.json")
            err = io.StringIO()
            with redirect_stderr(err):
                an.main(self.files(tmp) + ["-o", out])
            with open(out, encoding="utf-8") as f:
                got = json.load(f)
            self.assertIn("ホスト 4 件 (まとめの単位 2 件)", err.getvalue())
        self.assertEqual(got["status"]["hosts"][0]["host"], "connect://host-0001.g0001.example:443")
        self.assertEqual(got["connections"]["connections"][0]["client"], "198.51.100.1:51514")

    def test_a_plain_status_json_is_enough(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "2026-09-10T0537Z-status.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(sample()["status"], f)
            snap = an.load_inputs([path])
        self.assertEqual(snap["parts"], ["status"])
        self.assertEqual(snap["taken_at"], 1789018620)


@unittest.skipUnless(os.path.isfile(FIXTURE), "匿名化した実データが無い")
class Fixture(unittest.TestCase):
    """コミットしてある匿名化済みの実データ (T14.35) に、元の名前が残っていないこと。"""

    @classmethod
    def setUpClass(cls):
        with open(FIXTURE, encoding="utf-8") as f:
            cls.snap = json.load(f)

    def test_every_host_is_anonymized(self):
        seen = 0
        for key, value in strings(self.snap):
            if key not in an.HOST_KEYS or not value:
                continue
            seen += 1
            rest = value.split("://", 1)[-1]
            name = (rest[1:rest.find("]")] if rest.startswith("[")
                    else rest.rsplit(":", 1)[0] if ":" in rest else rest)
            self.assertTrue(
                an.ANON_HOST_RE.match(name) or name in an.RESERVED_NAMES
                or name.startswith(("203.0.113.", "192.0.2.", "2001:db8:1::")),
                "匿名化されていない宛先がある (長さ %d)" % len(name))
        self.assertGreater(seen, 800)

    def test_every_client_and_answer_is_in_the_documentation_ranges(self):
        for key, value in strings(self.snap):
            if not value:
                continue
            if key in an.CLIENT_KEYS:
                self.assertTrue(
                    value.startswith(("198.51.100.", "198.18.", "2001:db8::"))
                    or value in an.RESERVED_NAMES, "匿名化されていない接続元がある")
            elif key in an.ADDR_KEYS:
                self.assertTrue(value.startswith(("203.0.113.", "192.0.2.", "198.19.",
                                                  "2001:db8:1::")),
                                "匿名化されていない答えの IP がある")
            elif key in an.AGENT_KEYS:
                self.assertRegex(value, an.ANON_UA_RE)

    def test_the_numbers_of_2026_09_16_are_still_there(self):
        # T14.0 / T14.17 が読んだ数字 (匿名化で 1 つも変えていないこと)
        st = self.snap["status"]
        self.assertEqual(self.snap["taken_at"], 1789520760)
        self.assertEqual(st["uptime_secs"], 261833)
        self.assertEqual(st["total_requests"], 3930)
        self.assertEqual((st["dns"]["hits"], st["dns"]["misses"], st["dns"]["refreshes"]),
                         (1788, 2141, 442))
        self.assertEqual(len(self.snap["hosts"]["hosts"]), 817)
        self.assertEqual(sum(h["requests"] for h in self.snap["hosts"]["hosts"]), 18153)
        self.assertEqual(len(self.snap["dns"]["entries"]), 90)
        self.assertEqual([c["requests"] for c in st["clients"]], [17925, 463])
        self.assertEqual({res: len(h["samples"]) for res, h in self.snap["history"].items()},
                         {"5": 720, "60": 1440, "3600": 136})

    def test_it_does_not_change_if_it_is_anonymized_again(self):
        self.assertEqual(an.Anonymizer().run(json.loads(json.dumps(self.snap))), self.snap)


@unittest.skipUnless(os.path.isfile(os.path.join(DATA, "snapshot-local.json")),
                     "手元の `/snapshot` の作り置きが無い")
class WholeSnapshot(unittest.TestCase):
    """**いまの `/snapshot` の全部の部**を通す (T14.8 の `snapshot-local.json`)。

    部が増えたときに「置き換え忘れた欄」があればここで気づく (新しい欄にホスト名が入ったら、
    `HOST_KEYS` に足す)。中身は手元のベンチのものなので、値そのものは見ない。
    """

    @classmethod
    def setUpClass(cls):
        with open(os.path.join(DATA, "snapshot-local.json"), encoding="utf-8") as f:
            cls.raw = json.load(f)
        cls.out = an.Anonymizer().run(json.loads(json.dumps(cls.raw)))

    def test_every_part_survives_and_no_number_changes(self):
        self.assertEqual(list(self.out), list(self.raw))
        self.assertEqual(self.out["parts"], self.raw["parts"])
        self.assertEqual(numbers(self.raw), numbers(self.out))

    def test_no_field_is_left_behind(self):
        keys = an.HOST_KEYS | an.CLIENT_KEYS | an.ADDR_KEYS | an.AGENT_KEYS
        left = [(k, v) for (k, v), (k2, w) in zip(strings(self.raw), strings(self.out))
                if v == w and v and k in keys and v.lower() not in an.RESERVED_NAMES]
        self.assertEqual(left, [])

    def test_the_names_in_the_log_lines_are_replaced_too(self):
        for line in self.out.get("log", {}).get("lines", []):
            self.assertNotRegex(line["msg"], r"(?<![\w.-])localhost(?![\w-])")
            self.assertNotIn("127.0.0.1", line["msg"])


if __name__ == "__main__":
    unittest.main()
