#!/usr/bin/env python3
# `/events` の `dns_slow` (T14.23 / T17.1) を雪像の `/history?res=60` から作り直し、閾の案を並べる (T22.2)。
#
# 材料は雪像 (`/snapshot`) 1 枚の `history["60"]` (1 分 × 1,440 行 = 24 時間) と、その `canary`
# (canary の名前解決の ms)。判定の規則は `crates/metrics-watch/src/anomaly.rs` の `Detector::observe` を写した:
#   - 窓は直近 5 分 (`WINDOW_SECS`)。**1 分の行 5 本**を 1 つの窓にし、1 分ごとに判定する
#     (実機は 5 秒の標本を 5 秒ごとに畳むので、立つ瞬間は最大 1 分ずれ、5 秒の境目にまたがる窓は見えない)
#   - 立つ条件は案ごと (下の `PROPOSALS`)。閾は 100 ms と canary の中央値 (直近 1 時間) の 3 倍の大きい方
#   - 立ったら収まるまで 1 回。条件を外れたまま 5 分 (`CLEAR_SECS`) 続いたら解除
#
# **本当に遅い窓** (T22.2 の目安): ミス 10 回以上で平均 100 ms 超え。これが続いた一続き (同じ解除の規則で
# まとめる) を 1 件と数え、案がその一続きの間に 1 度も立っていなければ「落とした」とする。
#
# **「遅い 1 回を除いた平均」(`trim1`) は近似**: 履歴は 1 分ごとの合計 (`dns_ms_sum`) と回数しか持たず、
# ミス 1 回ごとの値は無い。窓の中で「1 分の平均がいちばん高い分」の平均を遅い 1 回の値と見なして除く。
# 本当の最大は 1 分の平均以上なので、これは除いたあとの平均の**上限** (立つ側に寄る)。実機に入れるなら
# 標本にミスの最大を足す要がある (`/history` の列が増える)。
#
# 実機の `/events` の `dns_slow` (履歴の範囲の中で立ったもの) と、今の条件 (`current`) の件数・時刻を突き合わせる。
#
# 使い方: scripts/dns-slow-replay.py SNAP.json [SNAP.json ...] [--json]
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import json
import statistics
import sys

# `anomaly.rs` の既定 (`DNS_MISS_MS` / `DNS_MIN_MISSES` / `DNS_CANARY_RATIO` / `WINDOW_SECS` / `BASE_SECS`)
DNS_MISS_MS = 100.0
DNS_MIN_MISSES = 3
DNS_CANARY_RATIO = 3.0
WINDOW_SECS = 300
BASE_SECS = 3600
CLEAR_SECS = WINDOW_SECS
ROW_SECS = 60
# 本当に遅い窓 (T22.2 の目安)
SLOW_MIN_MISSES = 10
SLOW_AVG_MS = 100.0
# 採る目安: tokyo の後の期間で 0.1 件/時以下、かつ本当に遅い一続きを 1 つも落とさない
TARGET_PER_HOUR = 0.1
# 実機の出来事と模型の出来事を「同じもの」と見なす時刻の幅 (秒)。行が 1 分なので前後 2 分
MATCH_SECS = 120


def history_rows(snap, res="60"):
    """`/history?res=<res>` の標本を辞書の列にする (`keys` の並びで読む)。"""
    h = (snap.get("history") or {}).get(res) or {}
    keys = h.get("keys") or []
    return [dict(zip(keys, row)) for row in (h.get("samples") or []) if isinstance(row, list)]


def canary_rows(snap, res="60"):
    """canary の (時刻, 名前解決の ms) の列 (ms が無い行は外す)。"""
    c = ((snap.get("history") or {}).get(res) or {}).get("canary") or {}
    keys = c.get("keys") or []
    if "t" not in keys or "canary_dns_ms" not in keys:
        return []
    ti, di = keys.index("t"), keys.index("canary_dns_ms")
    out = []
    for row in c.get("samples") or []:
        if isinstance(row, list) and row[di] is not None:
            out.append((row[ti], row[di]))
    return out


def canary_p50(canary, now):
    """直近 1 時間の canary の中央値 (`canaryhist::dns_p50_ms`)。無ければ None。"""
    xs = [ms for t, ms in canary if now - BASE_SECS <= t <= now]
    if not xs:
        return None
    return int(statistics.median_low(xs))


def limit_of(p50):
    """`dns_miss_limit`: 100 ms と canary の基準線の 3 倍の大きい方。"""
    return DNS_MISS_MS if p50 is None else max(DNS_MISS_MS, DNS_CANARY_RATIO * p50)


def windows(snap):
    """1 分ごとの 5 分窓 (時刻, ミスの回数, 合計 ms, 1 分の平均の最大, 閾) の列。

    最初の 4 行は窓が 5 分に満たないので外す (どの案も同じ扱い)。
    """
    rows = history_rows(snap)
    canary = canary_rows(snap)
    n = WINDOW_SECS // ROW_SECS
    out = []
    for i in range(n - 1, len(rows)):
        part = rows[i - n + 1:i + 1]
        misses = sum(r.get("dns_misses") or 0 for r in part)
        ms_sum = sum(r.get("dns_ms_sum") or 0 for r in part)
        worst = max(((r.get("dns_ms_sum") or 0) / r["dns_misses"]
                     for r in part if r.get("dns_misses")), default=0.0)
        # 行の時刻は 1 分の始まり。窓の終わり (= 判定した瞬間) はその 1 分後
        now = rows[i]["t"] + ROW_SECS
        out.append({"t": now, "misses": misses, "ms_sum": ms_sum, "worst": worst,
                    "limit": limit_of(canary_p50(canary, now))})
    return out


def avg(w):
    return w["ms_sum"] / w["misses"] if w["misses"] else 0.0


def trim1(w):
    """遅い 1 回を除いた平均の上限 (上の説明。ミスが 1 回なら 0)。"""
    if w["misses"] <= 1:
        return 0.0
    return max(0.0, w["ms_sum"] - w["worst"]) / (w["misses"] - 1)


# 案: 名前 -> (説明, 立つ条件)。`current` が今の `anomaly.rs`
PROPOSALS = {
    "current": ("min 3, avg >= limit (今)",
                lambda w: w["misses"] >= DNS_MIN_MISSES and avg(w) >= w["limit"]),
    "min5": ("min 5, avg >= limit",
             lambda w: w["misses"] >= 5 and avg(w) >= w["limit"]),
    "min8": ("min 8, avg >= limit",
             lambda w: w["misses"] >= 8 and avg(w) >= w["limit"]),
    "min10": ("min 10, avg >= limit",
              lambda w: w["misses"] >= 10 and avg(w) >= w["limit"]),
    "sum1000": ("min 3, avg >= limit, 合計 >= 1,000 ms",
                lambda w: w["misses"] >= DNS_MIN_MISSES and avg(w) >= w["limit"]
                and w["ms_sum"] >= 1000),
    "sum2000": ("min 3, avg >= limit, 合計 >= 2,000 ms",
                lambda w: w["misses"] >= DNS_MIN_MISSES and avg(w) >= w["limit"]
                and w["ms_sum"] >= 2000),
    "trim1": ("min 3, 遅い 1 回を除いた平均 >= limit (近似)",
              lambda w: w["misses"] >= DNS_MIN_MISSES and trim1(w) >= w["limit"]),
}


def slow(w):
    """本当に遅い窓 (ミス 10 回以上で平均 100 ms 超え)。"""
    return w["misses"] >= SLOW_MIN_MISSES and avg(w) > SLOW_AVG_MS


def episodes(ws, cond):
    """`Detector::observe` の立つ / 収まるを回し、(立った時刻, 解除の時刻 or None) の列を返す。"""
    out = []
    firing, since, calm = False, 0, None
    for w in ws:
        if cond(w):
            calm = None
            if not firing:
                firing, since = True, w["t"]
            continue
        if not firing:
            continue
        if calm is None:
            calm = w["t"]
        if w["t"] - calm >= CLEAR_SECS:
            out.append((since, w["t"]))
            firing, calm = False, None
    if firing:
        out.append((since, None))
    return out


def real_events(snap, t0, t1):
    """実機の `/events` の `dns_slow` (立ったもの) のうち、時刻が `[t0, t1]` のもの。"""
    ev = ((snap.get("events") or {}).get("events")) or []
    return sorted(e["at"] for e in ev
                  if e.get("kind") == "anomaly" and (e.get("text") or "").startswith("dns_slow:")
                  and t0 <= e["at"] <= t1)


def matched(a, b, secs=MATCH_SECS):
    """2 つの時刻の列を前後 `secs` 秒で 1 対 1 に組んだ数。"""
    used = set()
    n = 0
    for x in a:
        for j, y in enumerate(b):
            if j not in used and abs(x - y) <= secs:
                used.add(j)
                n += 1
                break
    return n


def overlaps(ep, fires):
    """一続き `ep` (始まり, 終わり) の間に、案が立っていた (立った一続きが重なる) か。"""
    s, e = ep
    e = float("inf") if e is None else e
    for fs, fe in fires:
        fe = float("inf") if fe is None else fe
        if fs <= e and s <= fe:
            return True
    return False


def analyze(snap):
    ws = windows(snap)
    if not ws:
        raise ValueError("the snapshot has no /history?res=60 samples")
    t0, t1 = ws[0]["t"], ws[-1]["t"]
    hours = (t1 - t0 + ROW_SECS) / 3600.0
    real = real_events(snap, t0 - ROW_SECS, t1 + ROW_SECS)
    slow_eps = episodes(ws, slow)
    out = {"taken_at": snap.get("taken_at"), "version": snap.get("version"),
           "from": t0, "until": t1, "hours": round(hours, 2),
           "real_events": len(real), "real_per_hour": round(len(real) / hours, 3),
           "slow_episodes": len(slow_eps),
           "slow_floor_per_hour": round(len(slow_eps) / hours, 3), "proposals": {}}
    for name, (_, cond) in PROPOSALS.items():
        eps = episodes(ws, cond)
        fires = [s for s, _ in eps]
        row = {"events": len(fires), "per_hour": round(len(fires) / hours, 3),
               "dropped_slow": sum(1 for ep in slow_eps if not overlaps(ep, eps))}
        if name == "current":
            row["matched_real"] = matched(real, fires)
        out["proposals"][name] = row
    return out


def render(path, a):
    lines = [f"## {path}",
             f"版 {a['version']}、履歴の範囲 {a['hours']} 時間。実機の `dns_slow` {a['real_events']} 件 "
             f"({a['real_per_hour']} 件/時)、本当に遅い一続き {a['slow_episodes']} 件",
             "",
             "| 案 | 条件 | 件 | 件/時 | 落とした本当に遅い一続き |",
             "|---|---|---:|---:|---:|"]
    for name, row in a["proposals"].items():
        extra = ""
        if "matched_real" in row:
            extra = f" (実機と前後 {MATCH_SECS // 60} 分で組めたもの {row['matched_real']})"
        lines.append(f"| `{name}` | {PROPOSALS[name][0]} | {row['events']}{extra} | "
                     f"{row['per_hour']} | {row['dropped_slow']} |")
    # 本当に遅い一続きを 1 つも落とさない案は、少なくともその件数だけ立つ (それより下がるなら落としている)
    lines.append("")
    lines.append(f"本当に遅い一続きを落とさない案の下限: {a['slow_episodes']} 件 "
                 f"({a['slow_floor_per_hour']} 件/時。目安は {TARGET_PER_HOUR} 件/時以下)")
    return "\n".join(lines)


def main(argv=None):
    p = argparse.ArgumentParser(description="dns_slow を /history?res=60 から作り直し、閾の案を並べる (T22.2)")
    p.add_argument("snapshots", nargs="+")
    p.add_argument("--json", action="store_true", help="結果を JSON で出す")
    args = p.parse_args(argv)
    results = {}
    for path in args.snapshots:
        with open(path, encoding="utf-8") as f:
            snap = json.load(f)
        results[path] = analyze(snap)
    if args.json:
        json.dump(results, sys.stdout, ensure_ascii=False, indent=1)
        print()
    else:
        print("\n\n".join(render(k, v) for k, v in results.items()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
