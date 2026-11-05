# 判定表 (`scripts/snapshot_criteria.py`) の**小物** (T.34.124 で割った)。
#
# `/history` の欄の名前・平常時の閾・判定の欄の文言と、判定の関数と `snapshot-diff.py` の差分の
# 両方が使う読む小物・印字の小物・履歴を重ねる小物。`snapshot-diff.py` は名前に `-` があって
# `import` できないので、共有するものは import できる側 (ここ) に置く (T.20.1 の決まり)。

import math
import statistics
from datetime import datetime, timezone

from proxydata import CAUSE_NAMES


# `/history` の標本のうちこの道具が読む欄 (`crates/metrics/src/history.rs` の KEYS)。
# **版によって欄の数が違う** (デプロイ先の 2026-09-16 は 31、いまの main は 32) ので、
# 位置ではなく `keys` の名前で引く。無い欄は None にして「出せない」と印字する。
HFIELDS = (
    "requests", "bytes", "active", "active_max", "rss",
    "connects", "connect_ms_sum", "connect_ms_max", "connect_buckets",
    "forwards", "forward_ms_sum", "forward_ms_max", "forward_buckets",
    "errors", "errors_by_cause", "dns_misses", "dns_ms_sum", "evicted_idle",
    # T15.0 (10) で末尾に足した 8 列 (**名前で引くので古い雪像では `None`**)。
    # `waits` は利用者が待つ時間 (`queue + client_read + dns + connect`)、`dns_warm` は
    # その瞬間の warm な名前の件数、`*_delta` は区間の増分、`active_peak` は区間の真の山
    "waits", "wait_ms_sum", "wait_ms_max", "wait_buckets",
    "dns_warm", "requests_delta", "bytes_delta", "active_peak",
    # T16.0 で末尾に足した 3 列: `dns_warm` の区間の最大と、平均を出すための合計と標本数
    # (5 秒の行は `sum = 値, n = 1`。無い版と T16.0 より前の行は下の `aggregate` が今の平均に戻す)
    "dns_warm_max", "dns_warm_sum", "gauge_n",
    # T.25.0 の名前解決に失敗したミスの数と、T.32.16 のその待ちの合計 (無い版では `None`)。名前解決のミスの率の
    # 表示から失敗を除くのに使う (T.32.17)
    "dns_fail_misses", "dns_fail_ms_sum",
)
# 平常時の閾 (T14.0: 1 時間 300 本未満の標本だけを「平常時」とする)。phase22 はこれと
# 「期間の中央値の k 倍」の大きい方 (下の `PHASE22` と `normal_limit`。T22.1)
BURST_PER_HOUR = 300
# 平常時を「300 本と、その期間の中央値のこの倍の大きい方」で切るときの倍率 (phase22 の `burst_median_k`。T22.1)。
# T.29.13 からサーバーの `/history?summary=1&normal_hours_only=1` も同じ規則で、`collect-deployed.sh` は
# `--criteria` を付けない差分 (要約の §2) にも `--burst-median-k` でこの値を渡す
BURST_MEDIAN_K = 3
# Phase 14 の完了の定義で名指しされている主要ホスト以外を選ぶときの数
MAJOR_HOSTS = 3


# ---------------------------------------------------------------- 読む小物

def part(snap, name):
    v = (snap or {}).get(name)
    return v if isinstance(v, dict) else {}


def miss_rate(row):
    return (row["dns_misses"] / row["requests"]) if row["requests"] else None


# ---------------------------------------------------------------- 印字の小物

def stamp(t):
    if not t:
        return "?"
    return datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")


def n(v, unit=""):
    if v is None:
        return "—"
    if isinstance(v, float):
        return f"{v:,.1f}{unit}"
    return f"{v:,}{unit}"


def ms(v):
    return "—" if v is None else f"{v:,.1f}"


def ratio(v):
    return "—" if v is None else f"{v:.2f}"


# ---------------------------------------------------------------- 履歴を重ねる

def norm_history(h):
    idx = {k: i for i, k in enumerate(h.get("keys") or [])}
    rows = []
    for r in h.get("samples") or []:
        if not r:
            continue
        row = {"t": r[0]}
        for f in HFIELDS:
            i = idx.get(f)
            row[f] = r[i] if i is not None and i < len(r) else None
        rows.append(row)
    return rows


def merged_history(a, b, res):
    """2 枚の `/history?res=` を時刻で重ねる (同じ時刻は**新しい雪像の値**を採る)。

    A の最後の窓は取得の途中までしか埋まっていないことがあるので、B が持っていれば B が勝つ。
    """
    rows, bounds, causes, interval = {}, [], list(CAUSE_NAMES), None
    for snap in (a, b):
        h = (snap.get("history") or {}).get(res) or {}
        if not h:
            continue
        bounds = h.get("bounds_ms") or bounds
        causes = h.get("causes") or causes
        interval = h.get("interval_secs") or interval
        for row in norm_history(h):
            rows[row["t"]] = row
    return [rows[t] for t in sorted(rows)], bounds, causes, (interval or 0)


MET, MISSED, UNKNOWN = "満たした", "届かず", "判定できず"
# 閾を置かない行の判定の欄 (T20.1)。`judge()` の集計 (`tally`) には数えない
SHOWN = "表示だけ"
# 材料が短くて判定に使わない行の判定の欄 (T21.2)。`SHOWN` と同じく集計 (`tally`) には数えない
REFERENCE = "参考"


def normal_limit(base, counts, k):
    """平常時の閾 (1 標本の本数。これ以上はバースト) を、その期間の標本の本数 `counts` から出す (T22.1)。

    `base` は `BURST_PER_HOUR` を解像度に合わせた値 (T14.0 の切り方)。返すのは `base` と
    「本数の中央値 × `k`」(切り上げ) の大きい方で、`k` が無ければ `base` のまま。標本が無ければ `base`。
    """
    if not k or not counts:
        return base
    return max(base, math.ceil(statistics.median(counts) * k))


def per_hour(agg, interval, value):
    """区間の件数を 1 時間あたりに直す (標本の数 × 解像度がその区間の秒)。"""
    secs = (agg.get("samples") or 0) * (interval or 0)
    return (value / (secs / 3600.0)) if secs and value is not None else None


def change(now, before):
    """変化率 ((後 − 前) ÷ 前)。前が 0 なら後も 0 のときだけ 0、そうでなければ None。"""
    if before:
        return (now - before) / before
    return 0.0 if not now else None
