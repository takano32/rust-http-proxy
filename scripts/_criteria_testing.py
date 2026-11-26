"""判定表の試験 (`scripts/test_snapshot_criteria*.py`) が共に使う作り物 (T38.6)。

`scripts/test_snapshot_criteria.py` が 3,874 行 (`docs/goals.md` の目安の 2,000 行の倍) になったので、
判定表ごとの試験のファイル (`test_snapshot_criteria_phase14_18.py`〜`test_snapshot_criteria_t37.py`) に割り、
2 つ以上のファイルが使う作り物をここに 1 つだけ置いた。名前が `test` で始まらないので、`unittest discover` は
試験として読まない。

スナップショットの読み方と小物 (`sd` `build` `run` `read` `written` …) と架空のスナップショット 2 枚
(`testdata/snapshot-{a,b}.json`) は `test_snapshot_diff.py` のものをそのまま使う。

**`unittest.TestCase` のクラスはここに置かない** (取り込んだファイルごとに同じ試験が走ってしまう)。いくつかの
ファイルの試験のクラスが共に使う作り物は、`TestCase` ではないクラス (`Criteria17Parts`・`Criteria29Parts`・
`Criteria30Parts`・`Criteria39Parts`) にまとめ、試験のクラスが継ぐか、名前で引く。
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
# 無言の再起動の行 (T23.2) は `sd` (snapshot_diff) が名前で取り込んでいないので、置き場から直接読む
import snapshot_criteria as sc  # noqa: E402,F401
# **TestCase のクラスは取り込まない** (取り込むと同じテストがこのファイルを取り込んだ先でも走る)
from test_snapshot_diff import (  # noqa: E402,F401
    A,
    B,
    DEPLOYED,
    build,
    read,
    run,
    sd,
    set_column,
    with_warm_columns,
    written,
)

# T34.999 の 2 枚 (0.33.3 の停止 → 0.34.165 の 48.1 時間)。T35 と T36 の実データの試験が使う
T34999_BEFORE = os.path.join(DEPLOYED, "tokyo", "2026-11-05T134450Z-shutdown-snapshot.json")
T34999_AFTER = os.path.join(DEPLOYED, "tokyo", "2026-11-07T135149Z-snapshot.json")


def conn_profile(conn_us_per_row):
    """B の `/profile` (60 秒 × 3 標本、要求 3 / 2 / 4 本) の conn 役の CPU だけ書き換えた写し。"""
    p = json.loads(json.dumps(read(B)["profile"]))
    ti, ri = p["keys"].index("threads"), p["roles"].index("conn")
    for row in p["samples"]:
        row[ti][ri][0] = conn_us_per_row
    return p


def zero_snapshot(heap_used=4_500_000, mmap=8_000_000, version="0.1.0+bbbbbbb", uptime=120):
    """B と同じ起動の「0 時間のスナップショット」(起動 2 分後)。B の `memory` は `heap_used` 6.0 + `mmap` 8.0 MB。"""
    memory = {"rss": 15_000_000, "heap_used": heap_used, "heap_free": 400_000, "mmap": mmap}
    return {"taken_at": Criteria17Parts.START + uptime, "version": version, "uptime_secs": uptime,
            "parts": ["status"], "dropped": [],
            "status": {"version": version, "uptime_secs": uptime, "since_start_secs": uptime,
                       "memory": {k: v for k, v in memory.items() if v is not None}}}


def with_probes(snap, probes, runs=700, skipped=20, at=0):
    """`/status` に T18.1 の欄 (`ipv6.request_probes` と `canary.ipv6_runs` / `ipv6_skipped`) を足す。"""
    snap["status"]["ipv6"].update(request_probes=probes, request_probe_at=at)
    snap["status"]["canary"] = {"mode": "on", "runs": runs + skipped, "failures": 0,
                                "ipv6_runs": runs, "ipv6_skipped": skipped}
    return snap


def cached(snap, cache, heap_used=None, mmap=None):
    """`/status` の `memory` に `cache_memory` (と、あれば `heap_used` / `mmap`) を置く。"""
    m = snap["status"]["memory"]
    if cache is None:
        m.pop("cache_memory", None)
    else:
        m["cache_memory"] = cache
    if heap_used is not None:
        m["heap_used"] = heap_used
    if mmap is not None:
        m["mmap"] = mmap
    return snap


def long_profile(conn_us_per_row, rows):
    """`conn_profile` の 3 標本を `rows` 本に伸ばした写し (60 秒おき。要求と CPU は 3 本ずつの繰り返し)。"""
    p = conn_profile(conn_us_per_row)
    base = p["samples"]
    t0 = base[0][0]
    p["samples"] = [[t0 + 60 * i] + json.loads(json.dumps(base[i % len(base)][1:])) for i in range(rows)]
    p["count"] = p["shown"] = rows
    return p


def with_dns_slow(snap, *totals, at=None, cleared=0, text=None):
    """`/events` を `dns_slow` の出来事だけにする (`totals` は窓のミスの合計 ms。文面は `anomaly.rs` の形)。"""
    at = at if at is not None else snap["taken_at"] - 600
    rows = [{"at": at, "kind": "anomaly",
             "text": f"dns_slow: dns miss {t // 3} ms avg over 5m (threshold 100 ms; 3 misses, {t} ms total)"}
            for t in totals]
    rows += [{"at": at, "kind": "anomaly", "text": "cleared: dns_slow after 6m (dns miss 1 ms avg over 5m, "
              "1 misses)"}] * cleared
    if text:
        rows.append({"at": at, "kind": "anomaly", "text": text})
    snap["events"] = {"events": rows}
    return snap


def with_causes(snap, boundary, before, after):
    """`/history?res=3600` の `errors_by_cause` を、`boundary` より前の行は `before`、後の行は `after` にする。"""
    h = snap["history"]["3600"]
    i = h["keys"].index("errors_by_cause")
    for row in h["samples"]:
        row[i] = list(before if row[0] < boundary else after)
    return snap



# --- いくつかのファイルの試験のクラスが共に使う作り物 (T38.6 で試験のクラスから出した。中身は同じ)


class Criteria17Parts:
    START = 1789043200

    @staticmethod
    def warm(snap, peak, evicted):
        """後の期間の 1 時間の行 (バーストの行も) に `dns_warm_max` を足し、`warm_evicted` を置く。

        B の `dns_warm` は 24 / 26 / 28 なので、最大はそれより下にならない (`aggregate` の下限)。
        """
        with_warm_columns(snap, {t: (peak, 17568, 720)
                                 for t in (1789045200, 1789048800, 1789052400)})
        snap["status"]["dns"]["warm_evicted"] = evicted
        return snap


class Criteria29Parts:
    T0 = 1_800_000_000  # 時間の境目

    def snap(self, prof, fails_per_hour, history=None):
        hours = len(prof["samples"]) // 60
        causes = ["dns", "refused", "unreachable", "timeout", "reset", "tls", "loop", "other"]
        h = {"interval_secs": 3600, "causes": causes, "keys": ["t", "errors_by_cause"],
             "samples": [[self.T0 + 3600 * i, [fails_per_hour, 0, 0, 0, 0, 0, 0, 0]] for i in range(hours)]}
        return {"profile_res_60": prof, "history": {"3600": history or h}}

    def hist_snap(self, rows, other_ports=True, up_hours=48):
        keys = ["t", "errors", "errors_by_cause", "connects"]
        if other_ports:
            keys += ["other_port_errors", "other_port_attempts"]
        taken = self.T0 + 3600 * up_hours
        samples = [[self.T0 - 3600 * 5 + 3600 * i] + r[:len(keys) - 1] for i, r in enumerate(rows)]
        return {"taken_at": taken, "uptime_secs": 3600 * up_hours - 1800,
                "history": {"3600": {"interval_secs": 3600, "causes": list(sc.CAUSE_NAMES), "keys": keys,
                                     "samples": samples}}}

    @staticmethod
    def rows(n, err, dns, conn, ope, opa):
        """起動より前の 5 時間 (数えない) と、起動からの n 時間 (合計がだいたい引数になる)。"""
        pad = [[999, [999] + [0] * 7, 999, 999, 999]] * 5
        per = [[err // n + dns // n, [dns // n, err // n, 0, 0, 0, 0, 0, 0], conn // n, ope // n, opa // n]] * n
        return pad + per


class Criteria39Parts:
    """T39 と T40 の試験が共に使う `/profile?res=60` の作り物 (T40.4 で `test_snapshot_criteria_t39.py` から出した。中身は同じ)。"""

    T0 = Criteria29Parts.T0
    STAGES = ["queue", "client_read", "dns", "connect", "first_relay", "relay", "park"]
    # 1 時間ごとの要求と中継の揺れ (2 つが比例しないように別の並び)
    REQ_WAVE = [1.0, 1.4, 0.7, 1.2, 0.9, 1.5, 0.6, 1.1]
    RELAY_WAVE = [1.0, 0.8, 1.3, 0.6, 1.5, 1.1, 0.9, 1.2]

    def profile(self, req=30, relay=60.0, us_req=1000, us_relay=50, hours=24, history_us=4800, wobble=None,
                stages=STAGES):
        """`/profile?res=60` の作り物。`conn` 役の CPU = `us_req` × 名前解決に成功した要求 (要求の 6 割) + `us_relay` ×
        中継の秒。要求と中継の秒は 1 分あたりの `req`・`relay` を時間ごとに `REQ_WAVE`・`RELAY_WAVE` で揺らす。
        `wobble` は時間ごとの CPU に掛ける並び (同じ版の中のぶれ)。物差しの `history` 役は `history_us` / 60 us/秒。"""
        roles = ["accept", "conn", "profile-sample", "other", "history"]
        rows = []
        for i in range(int(hours * 60)):
            h = i // 60
            q = round(req * self.REQ_WAVE[h % len(self.REQ_WAVE)])
            secs = relay * self.RELAY_WAVE[h % len(self.RELAY_WAVE)]
            conn = (us_req * q * 0.6 + us_relay * secs) * (wobble[h % len(wobble)] if wobble else 1)
            threads = [[1000, 60, []], [conn, 120, []], [10000, 60, []], [30000, 300, []], [history_us, 60, []]]
            connect = [0] * len(stages)
            if "relay" in stages:
                connect[stages.index("relay")] = [5, round(secs * 1000), 9000, []]
            rows.append([self.T0 + 60 * i, q, 1000 + conn + 44800, threads, connect])
        return {"interval_secs": 60, "keys": ["t", "requests", "cpu_us", "threads", "connect"], "roles": roles,
                "stages": {"connect": stages, "forward": []}, "samples": rows}

    def snap(self, p):
        """名前解決の失敗が要求の 4 割になる `/history?res=3600` を添えたスナップショット。"""
        total = sum(r[1] for r in p["samples"])
        hours = len(p["samples"]) // 60
        return Criteria29Parts.snap(self, p, round(total * 0.4 / hours))

    def c(self, pa, pb):
        return {"a": self.snap(pa), "b": self.snap(pb)}


class Criteria30Parts:
    T0 = 1_800_000_000

    def snap(self, minutes, events, exact=True):
        """1 日の `/history?res=60` (`minutes` = {分: (ミス, 失敗, 待ちの合計, 失敗の待ち)}) と、その分に立った `dns_slow`。"""
        keys = ["t", "dns_misses", "dns_fail_misses", "dns_ms_sum"] + (["dns_fail_ms_sum"] if exact else [])
        samples = []
        for i in range(1440):
            m, f, ms_, fms = minutes.get(i, (0, 0, 0, 0))
            samples.append([self.T0 + 60 * i, m, f, ms_] + ([fms] if exact else []))
        ev = [{"kind": "anomaly", "at": self.T0 + 60 * i + 30, "text": "dns_slow: dns miss 300 ms avg over 5m"}
              for i in events]
        ev.append({"kind": "anomaly", "at": self.T0 + 60 * 1430, "text": "cleared: dns_slow after 6m"})
        return {"taken_at": self.T0 + 86400, "uptime_secs": 86400 * 2,
                "events": {"events": ev}, "history": {"60": {"interval_secs": 60, "keys": keys, "samples": samples}}}

    def slow(self, avg, count, exact=True, fails=(0, 0), start=100):
        """`count` 件の窓。どの窓も成功したミス 4 回 × 平均 `avg` ms と、同じ 1 分に失敗 `fails` (回, ms)。"""
        minutes = {start + 60 * j: (4 + fails[0], fails[0], 4 * avg + fails[1], fails[1]) for j in range(count)}
        return self.snap(minutes, list(minutes), exact)
