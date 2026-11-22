#!/usr/bin/env python3
"""判定表 phase26・phase27・phase28 の試験 (名前解決の失敗を分母から外す数え方も)。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

`scripts/test_snapshot_criteria.py` (3,874 行) を判定表ごとに割ったうちの 1 つ (T38.6)。中身は割る前と同じ。
作り物は `scripts/_criteria_testing.py` にある。
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _criteria_testing import (  # noqa: E402
    DEPLOYED,
    build,
    run,
    sc,
    sd,
)


class RecentAttempts(unittest.TestCase):
    """名前解決の失敗は今の綴り (`error:nxdomain`・`error:dns_temp`) も分母から外す (T.29.36)。"""

    def test_all_spellings_of_a_failed_lookup_leave_the_denominator(self):
        rows = [{"kind": "connect", "at": i, "reason": r} for i, r in enumerate(
            ["ok", "ok", "error:dns", "error:nxdomain", "error:dns_temp", "error:refused", "error:timeout"])]
        got = sc.recent_attempts({"recent": {"recent": rows}}, ("refused", "unreachable", "timeout"))
        self.assertEqual((got["attempts"], got["dns"], got["denom"], got["conn"]), (7, 3, 4, 2))
        self.assertEqual(got["by"], {"refused": 1, "timeout": 1})


class Criteria26Rows(unittest.TestCase):
    """`--criteria phase26` (T.29.10): phase25 の 17 行を動かさず、18〜25 番目に足した 8 行。

    数字は 0.28.48 の 24 時間 (`2026-10-27T024200Z`) の形を小さく写した作り物。
    """

    @staticmethod
    def snap(status=None, recent=None, log=None, errors=None, uptime=86400, taken_at=1793068922):
        s = {"uptime_secs": uptime, "taken_at": taken_at, "status": status or {}}
        if recent is not None:
            s["recent"] = {"recent": recent, "count": len(recent)}
        if log is not None:
            s["log"] = {"lines": log, "level": "info", "truncated": False}
        if errors is not None:
            s["errors"] = {"errors": errors}
        return s

    def ctx(self, b, a=None):
        return {"a": a or {}, "b": b, "info": {}}

    def test_phase26_keeps_the_seventeen_rows_of_phase25_and_adds_eight(self):
        p25, p26 = sd.RULES["phase25"], sd.RULES["phase26"]
        self.assertEqual(len(p26), 26)
        self.assertEqual(p26[:17], p25)
        self.assertIs(p26[17], sc._p26_header_timeouts)
        self.assertEqual(sd.CRITERIA["phase26"]["log_error_max"], 5)

    def test_the_header_timeouts_are_shown_with_the_recent_rows(self):
        rows = [{"at": 1793068000, "kind": "http", "reason": "error:header_timeout", "reqs": 1},
                {"at": 1793068100, "kind": "connect", "reason": "error:timeout", "reqs": 0}]
        b = self.snap({"rejected_requests": {"header_timeout": 12, "total": 12}}, recent=rows, uptime=43200)
        row = sc._p26_header_timeouts(self.ctx(b), sc.PHASE26)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertIn("**12** 件 (起動から 12.0 時間、24.0 件/24 時間)", row[2])
        self.assertIn("`error:header_timeout` 1 行 (うち 1 要求目を通したあとの 1 行)", row[2])
        self.assertNotIn("silent_closes", row[2], "欄の無い版 (T.33.0 より前) には書かない")
        # T29.10 より前の版には欄が無い = 0 と書かない
        old = self.snap({"rejected_requests": {"bad_header": 0, "total": 0}})
        self.assertEqual(sc._p26_header_timeouts(self.ctx(old), sc.PHASE26)[3], sd.UNKNOWN)

    def test_the_header_timeout_row_shows_the_silent_closes_beside_the_408(self):
        # T33.0: 先張りの接続は 408 にせず `silent_closes` に数える。同じ行に並べ、判定は表示だけのまま
        b = self.snap({"rejected_requests": {"header_timeout": 3, "total": 3}, "silent_closes": 1700},
                      uptime=86400)
        row = sc._p26_header_timeouts(self.ctx(b), sc.PHASE26)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertIn("**3** 件 (起動から 24.0 時間、3.0 件/24 時間)", row[2])
        self.assertIn("(`silent_closes`、T.33.0) **1,700** 本 (1,700.0 本/24 時間)", row[2])

    def test_log_errors_up_to_five_lines_are_met(self):
        def line(lv):
            return {"at": 1793068000, "level": lv, "msg": "connect imap.example:993 timed out"}
        ok = self.snap(log=[line("warn")] * 900 + [line("error")] * 5)
        self.assertEqual(sc._p26_log_errors(self.ctx(ok), sc.PHASE26)[3], sd.MET)
        bad = self.snap(log=[line("warn")] * 900 + [line("error")] * 6)
        row = sc._p26_log_errors(self.ctx(bad), sc.PHASE26)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**6** 行", row[2])
        self.assertEqual(sc._p26_log_errors(self.ctx(self.snap()), sc.PHASE26)[3], sd.UNKNOWN)

    def test_local_blocks_are_counted_with_repeats(self):
        errs = [{"at": 1, "cause": "nxdomain", "target": "imap.example:993", "repeats": 5},
                {"at": 2, "cause": "local", "target": "192.168.1.1:80", "repeats": 3}]
        row = sc._p26_local_blocks(self.ctx(self.snap(errors=errs)), sc.PHASE26)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("**3** 件 (宛先 `192.168.1.1:80`)", row[2])
        self.assertEqual(sc._p26_local_blocks(self.ctx(self.snap(errors=errs[:1])), sc.PHASE26)[3], sd.MET)

    def test_the_connect_cut_row_counts_timeouts_and_the_slowest_success(self):
        rows = ([{"at": 10 + i, "kind": "connect", "target": "imap.example:993", "reason": "error:timeout",
                  "ms": {"connect": 3000}} for i in range(14)]
                + [{"at": 100, "kind": "connect", "target": "a.example:443", "reason": "server_eof",
                    "ms": {"connect": 2267}, "syn_retrans": 1},
                   {"at": 101, "kind": "connect", "target": "b.example:443", "reason": "client_eof",
                    "ms": {"connect": 11}, "syn_retrans": 2}])
        row = sc._p26_connect_cut(self.ctx(self.snap(recent=rows)), sc.PHASE26)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertIn("切った **14** 行 (ポート `993` 14)", row[2])
        self.assertIn("成功 2 本の確立の最大 **2,267** ms (2 秒以上 1 本)", row[2])
        self.assertIn("`syn_retrans` 1 が 1 本・2 が 1 本", row[2])

    def test_rejected_requests_are_shown_by_reason(self):
        b = self.snap({"rejected_requests": {"method": 3, "bad_header": 0, "header_timeout": 2, "total": 5}})
        row = sc._p26_rejected(self.ctx(b), sc.PHASE26)
        self.assertEqual(row[3], sd.SHOWN)
        self.assertIn("**5** 本", row[2])
        self.assertIn("`method` 3、`header_timeout` 2", row[2])

    def test_the_rss_gap_uses_the_t2925_threshold(self):
        mem = {"rss": 65_794_048, "heap_used": 20_359_248, "mmap": 15_671_296, "cache_memory": 14_382_613,
               "heap_free": 18_302_896}
        row = sc._p26_rss_gap(self.ctx(self.snap({"memory": mem})), sc.PHASE26)
        self.assertEqual(row[3], sd.MISSED)                  # 0.28.48 の 24 時間は 15.4 MB
        self.assertIn("**15.4** MB", row[2])
        mem["rss"] = 55_000_000
        self.assertEqual(sc._p26_rss_gap(self.ctx(self.snap({"memory": mem})), sc.PHASE26)[3], sd.MET)

    def test_the_cgroup_cpu_must_go_down(self):
        def snap(usage, up=3600):
            return self.snap({"kernel": {"cgroup_cpu": {"since_start": {
                "usage_usec": usage, "user_usec": usage // 4, "system_usec": usage - usage // 4}}}}, uptime=up)
        self.assertEqual(sc._p26_cgroup_cpu(self.ctx(snap(5_000_000), snap(5_400_000)), sc.PHASE26)[3], sd.MET)
        self.assertEqual(sc._p26_cgroup_cpu(self.ctx(snap(5_400_000), snap(5_400_000)), sc.PHASE26)[3],
                         sd.MISSED)
        self.assertEqual(sc._p26_cgroup_cpu(self.ctx(snap(5_400_000), {}), sc.PHASE26)[3], sd.UNKNOWN)

    @staticmethod
    def profile(shares, rows=720):
        """`profile-sample` と `cache-probe` の割合が `shares` の `/profile?res=60` を作る。"""
        roles = ["conn", "cache-probe", "profile-sample", "other"]
        cpu = 1000
        threads = [[cpu * (1 - sum(shares)), 60, []], [cpu * shares[1], 60, []], [cpu * shares[0], 60, []], 0]
        return {"interval_secs": 60, "roles": roles, "keys": ["t", "requests", "cpu_us", "threads"],
                "samples": [[i, 1, cpu, threads] for i in range(rows)]}

    def test_the_idle_role_shares_must_go_down(self):
        a, b = {"profile_res_60": self.profile((0.325, 0.234))}, {"profile_res_60": self.profile((0.20, 0.10))}
        row = sc._p26_role_shares(self.ctx(b, a), sc.PHASE26)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("`profile-sample` 32.5% → **20.0%**", row[2])
        b = {"profile_res_60": self.profile((0.20, 0.25))}
        self.assertEqual(sc._p26_role_shares(self.ctx(b, a), sc.PHASE26)[3], sd.MISSED)
        # 12 時間に満たない材料は参考
        b = {"profile_res_60": self.profile((0.20, 0.10), rows=60)}
        self.assertEqual(sc._p26_role_shares(self.ctx(b, a), sc.PHASE26)[3], sd.REFERENCE)


    def test_origin_warn_lines_are_counted_per_minute(self):
        def line(at, msg):
            return {"at": at, "level": "warn", "msg": msg}
        # T29.15 の前の形: 同じ 1 分にメール宛の 502 が 12 行
        old = [line(1793068500 + i, f"502 Bad Gateway: connect imap.example:993 failed: lookup {i}") for i in range(12)]
        row = sc._p26_origin_warn_per_minute(self.ctx(self.snap(log=old)), sc.PHASE26)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("1 分の最大 **12** 行", row[2])
        # T29.15 の後: 原因ごとに 1 行で、抑えた本数を添える
        new = [line(1793068500, "502 Bad Gateway: connect imap.example:993 failed: lookup"),
               line(1793068560, "502 Bad Gateway: connect imap.example:993 failed: lookup"
                                " (+40 more since the previous line, not logged)"),
               line(1793068570, "origin response timed out after 30 s"),
               line(1793068580, "cache write failed")]
        row = sc._p26_origin_warn_per_minute(self.ctx(self.snap(log=new)), sc.PHASE26)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("3 行 / 4 行、1 分の最大 **2** 行", row[2])
        self.assertIn("抑えた本数を添えた行 1", row[2])


T2899_BEFORE = os.path.join(DEPLOYED, "tokyo", "2026-10-26T013119Z-snapshot.json")
T2899_AFTER = os.path.join(DEPLOYED, "tokyo", "2026-10-27T024200Z-snapshot.json")


@unittest.skipUnless(os.path.exists(T2899_BEFORE) and os.path.exists(T2899_AFTER),
                     "tokyo の実データが無い (status/tokyo/ の T.28.99 の 2 枚)")
class Deployed26(unittest.TestCase):
    """T.28.99 の 2 枚 (0.27.8 → 0.28.48 の 24 時間) で phase26 の表が出る (T.29.10 の受け入れ基準)。"""

    def test_t2899_pair(self):
        md = run([T2899_BEFORE, T2899_AFTER, "--no-dns", "--criteria", "phase26"])
        self.assertIn("## 9. 完了の定義に対する判定 (`--criteria phase26`)", md)
        c = build([T2899_BEFORE, T2899_AFTER, "--no-dns", "--criteria", "phase26"])["criteria"]
        rows = c["rows"]
        self.assertEqual(len(rows), 26)
        verdicts = [r[3] for r in rows]
        # 0.28.48 には `header_timeout` の欄が無い (0 と書かない)
        self.assertEqual(verdicts[17], sd.UNKNOWN)
        self.assertEqual(verdicts[18], sd.MET)                # `/log` の error 0 行
        self.assertEqual(verdicts[19], sd.MET)                # `local` 0 件
        self.assertIn("切った **14** 行 (ポート `993` 14)", rows[20][2])
        self.assertIn("**2,267** ms", rows[20][2])
        self.assertIn("どの理由も 0", rows[21][2])
        self.assertIn("**15.4** MB", rows[22][2])


class Criteria27Rows(unittest.TestCase):
    """`--criteria phase27` (T.30.0・T.30.1): phase26 の 26 行のうち 13 番目と 23 番目だけ読み方を変えたもの。"""

    @staticmethod
    def recent(ok, dns, timeout, port="993"):
        rows = [{"kind": "connect", "at": 1793190000 + i, "reason": "server_eof", "target": "a.example:443"}
                for i in range(ok)]
        rows += [{"kind": "connect", "at": 1793191000 + i, "reason": "error:nxdomain", "target": f"pop.example:{port}"}
                 for i in range(dns)]
        rows += [{"kind": "connect", "at": 1793192000 + i, "reason": "error:timeout", "target": f"mail.example:{port}"}
                 for i in range(timeout)]
        return {"recent": {"recent": rows}}

    def test_phase27_replaces_only_two_rows_of_phase26(self):
        p26, p27 = sd.RULES["phase26"], sd.RULES["phase27"]
        self.assertEqual(len(p27), 26)
        self.assertEqual([i for i in range(26) if p26[i] is not p27[i]], [12, 22])
        self.assertIs(p27[12], sc._p27_connect_errors)
        self.assertIs(p27[22], sc._p27_rss_gap)

    def test_a_small_resolved_denominator_is_not_judged(self):
        # T29.99 の形: 後は 1,985 本のうち 1,496 本が名前の引けないメール宛で、分母 489 本
        a, b = self.recent(1937, 39, 21), self.recent(403, 1496, 86, port="995")
        row = sc._p27_connect_errors({"a": a, "b": b}, sc.PHASE27)
        self.assertEqual(row[3], sd.UNKNOWN)
        self.assertIn("後は名前解決に成功した試行 489 本", row[4])
        self.assertIn("`:995` 1,496", row[4])
        self.assertIn("`error:nxdomain`", row[4])
        self.assertNotIn('reason == "error:dns"', row[4])
        # phase25 (試行の本数の下限) なら判定して「届かず」になる
        self.assertEqual(sc._p25_connect_errors({"a": a, "b": b}, sc.PHASE25)[3], sd.MISSED)

    def test_enough_resolved_attempts_are_judged_as_before(self):
        a, b = self.recent(1950, 10, 20), self.recent(1960, 10, 20)
        self.assertEqual(sc._p27_connect_errors({"a": a, "b": b}, sc.PHASE27)[3], sd.MET)
        b = self.recent(1900, 10, 80)
        self.assertEqual(sc._p27_connect_errors({"a": a, "b": b}, sc.PHASE27)[3], sd.MISSED)

    @staticmethod
    def mem(rss, used, mmap, cache, **kw):
        m = {"rss": rss * 10**6, "heap_used": used * 10**6, "mmap": mmap * 10**6, "cache_memory": cache * 10**6}
        m.update(kw)
        return {"uptime_secs": 86400, "status": {"memory": m}}

    def test_the_rss_gap_is_compared_with_the_previous_version(self):
        # T29.99: 前 82.3 − (20.4 + 15.7 + 14.4) = 31.8、後 52.3 − (11.8 + 11.6) = 28.9
        a = self.mem(82.3, 20.4, 15.7, 14.4)
        b = self.mem(52.3, 11.8, 11.6, 0, heap_free=23.0e6, stacks_estimate=22.3e6)
        row = sc._p27_rss_gap({"a": a, "b": b}, sc.PHASE27)
        self.assertEqual(row[3], sd.MET)
        self.assertIn("**28.9** MB", row[2])
        self.assertIn("前 31.8 MB", row[2])
        self.assertIn("`stacks_estimate` 22.3 MB", row[2])
        self.assertIn("`malloc_trim` の欄が無い版", row[2])
        # phase26 の固定の 10 MB では「届かず」
        self.assertEqual(sc._p26_rss_gap({"a": a, "b": b}, sc.PHASE26)[3], sd.MISSED)
        worse = self.mem(70, 11.8, 11.6, 0, trim={"count": 3, "skipped": 21, "total_returned_bytes": 4.2e6})
        row = sc._p27_rss_gap({"a": a, "b": worse}, sc.PHASE27)
        self.assertEqual(row[3], sd.MISSED)
        self.assertIn("`malloc_trim` 3 回 (返した計 4.2 MB、呼ばなかった 21 回)", row[2])
        self.assertEqual(sc._p27_rss_gap({"a": {}, "b": b}, sc.PHASE27)[3], sd.UNKNOWN)


T2999_BEFORE = os.path.join(DEPLOYED, "tokyo", "2026-10-27T082245Z-shutdown-snapshot.json")
T2999_AFTER = os.path.join(DEPLOYED, "tokyo", "2026-10-28T143043Z-snapshot.json")


@unittest.skipUnless(os.path.exists(T2999_BEFORE) and os.path.exists(T2999_AFTER),
                     "tokyo の実データが無い (status/tokyo/ の T.29.99 の 2 枚)")
class Deployed27(unittest.TestCase):
    """T.29.99 の 2 枚 (0.28.48 の停止 → 0.29.36 の 24 時間) で、phase26 の届かなかった 2 行が直る (T.30.0・T.30.1)。"""

    def test_t2999_pair(self):
        c = build([T2999_BEFORE, T2999_AFTER, "--no-dns", "--criteria", "phase27"])["criteria"]
        rows = c["rows"]
        self.assertEqual(len(rows), 26)
        self.assertEqual(rows[12][3], sd.UNKNOWN)
        self.assertIn("後は名前解決に成功した試行 489 本", rows[12][4])
        self.assertEqual(rows[22][3], sd.MET)
        self.assertIn("**28.9** MB", rows[22][2])
        self.assertIn("前 31.8 MB", rows[22][2])


class Criteria28Rows(unittest.TestCase):
    """`--criteria phase28` (T.31.0・T.31.1): phase27 の 26 行のうち 23・24 番目だけ量に左右されない物差しにしたもの。"""

    @staticmethod
    def snap(usage_usec, uptime, requests, trim=None, mem=None):
        m = {"rss": 48.6e6, "heap_used": 12.8e6, "mmap": 11.7e6, "cache_memory": 0}
        m.update(mem or {})
        if trim is not None:
            m["trim"] = trim
        return {"uptime_secs": uptime, "status": {
            "total_requests": requests, "memory": m,
            "kernel": {"cgroup_cpu": {"since_start": {"usage_usec": usage_usec, "user_usec": usage_usec // 4}}}}}

    def test_phase28_replaces_only_two_rows_of_phase27(self):
        p27, p28 = sd.RULES["phase27"], sd.RULES["phase28"]
        self.assertEqual(len(p28), 26)
        self.assertEqual([i for i in range(26) if p27[i] is not p28[i]], [22, 23])
        self.assertIs(p28[22], sc._p28_trim_works)
        self.assertIs(p28[23], sc._p28_cgroup_cpu_per_request)

    def test_cpu_is_compared_per_request(self):
        # T30.99 の形: コアは 1.09 倍だが要求は 1.56 倍 → 要求 1 本あたりは 0.70 倍で満たす
        a = self.snap(124_000_000, 29.0 * 3600, 42_672)
        b = self.snap(113_000_000, 24.2 * 3600, 55_327)
        row = sc._p28_cgroup_cpu_per_request({"a": a, "b": b}, sc.PHASE28)
        self.assertEqual(row[3], sd.MET, row)
        # phase26 のコアの合計では届かず
        self.assertEqual(sc._p26_cgroup_cpu({"a": a, "b": b}, sc.PHASE26)[3], sd.MISSED)
        # 量が同じで CPU/要求 が 1.2 倍なら届かず
        b2 = self.snap(int(124_000_000 * 1.2), 29.0 * 3600, 42_672)
        self.assertEqual(sc._p28_cgroup_cpu_per_request({"a": a, "b": b2}, sc.PHASE28)[3], sd.MISSED)
        # 要求の数が無いスナップショットは判定できず
        nb = self.snap(113_000_000, 24.2 * 3600, None)
        self.assertEqual(sc._p28_cgroup_cpu_per_request({"a": a, "b": nb}, sc.PHASE28)[3], sd.UNKNOWN)

    def test_trim_works_every_hour(self):
        a = self.snap(1, 29.0 * 3600, 1, mem={"rss": 42.8e6})
        ok = self.snap(1, 24.2 * 3600, 1, trim={"count": 24, "skipped": 0, "total_returned_bytes": 71_400_000,
                                               "last_returned_bytes": 7_100_000, "last_us": 225},
                       mem={"heap_free": 19.9e6, "stacks_estimate": 21.5e6})
        row = sc._p28_trim_works({"a": a, "b": ok}, sc.PHASE28)
        self.assertEqual(row[3], sd.MET, row)
        self.assertIn("**24** 回", row[2])
        self.assertIn("返した計 **71.4** MB", row[2])
        self.assertIn("`rss − (…)` 24.1 MB", row[2])
        # 呼んだ回が足りない・呼んだのに返していない は届かず
        few = self.snap(1, 24.2 * 3600, 1, trim={"count": 3, "skipped": 0, "total_returned_bytes": 1_000_000})
        self.assertEqual(sc._p28_trim_works({"a": a, "b": few}, sc.PHASE28)[3], sd.MISSED)
        none = self.snap(1, 24.2 * 3600, 1, trim={"count": 24, "skipped": 0, "total_returned_bytes": 0})
        self.assertEqual(sc._p28_trim_works({"a": a, "b": none}, sc.PHASE28)[3], sd.MISSED)
        # 返すものが無くて毎回呼ばなかった (skipped) のは満たす
        idle = self.snap(1, 24.2 * 3600, 1, trim={"count": 0, "skipped": 24, "total_returned_bytes": 0})
        self.assertEqual(sc._p28_trim_works({"a": a, "b": idle}, sc.PHASE28)[3], sd.MET)
        # 欄の無い版は判定できず
        self.assertEqual(sc._p28_trim_works({"a": a, "b": self.snap(1, 3600, 1)}, sc.PHASE28)[3], sd.UNKNOWN)


T3099_BEFORE = os.path.join(DEPLOYED, "tokyo", "2026-10-28T191810Z-shutdown-snapshot.json")
T3099_AFTER = os.path.join(DEPLOYED, "tokyo", "2026-10-30T062500Z-snapshot.json")


@unittest.skipUnless(os.path.exists(T3099_BEFORE) and os.path.exists(T3099_AFTER),
                     "tokyo の実データが無い (status/tokyo/ の T.30.99 の 2 枚)")
class Deployed28(unittest.TestCase):
    """T.30.99 の 2 枚 (0.29.36 の停止 → 0.30.4 の 24.2 時間) で、phase27 の届かなかった 2 行が満たす (T.31.0・T.31.1)。"""

    def test_t3099_pair(self):
        c = build([T3099_BEFORE, T3099_AFTER, "--no-dns", "--criteria", "phase28"])["criteria"]
        rows = c["rows"]
        self.assertEqual(len(rows), 26)
        self.assertEqual(rows[22][3], sd.MET)
        self.assertIn("**24** 回", rows[22][2])
        self.assertEqual(rows[23][3], sd.MET)
        self.assertIn("0.70 倍", rows[23][2])


if __name__ == "__main__":
    unittest.main()
