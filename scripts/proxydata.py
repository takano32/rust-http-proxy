#!/usr/bin/env python3
"""デプロイ先の JSON (`/status` `/hosts` `/history` `/snapshot`) を読むための共通部品。

`scripts/status-diff.py` (T12.0) と `scripts/snapshot-diff.py` (T14.17) が同じ読み方を
するための置き場。**ここには「読む」しか置かない** (印字は呼ぶ側)。
依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。
"""

import json
import socket
import statistics
import unicodedata
from concurrent.futures import ThreadPoolExecutor

CONNECT = "connect://"
# `/status` の `errors_by_cause` の並び (`crates/metrics/src/metrics.rs` の ERR_CAUSE_NAMES)
CAUSE_NAMES = ["dns", "refused", "unreachable", "timeout", "reset", "tls", "loop", "other"]


def host_name(key):
    """`connect://www.example.com:443` -> `www.example.com`"""
    rest = key.split("://", 1)[-1]
    if rest.startswith("["):  # IPv6 リテラル
        return rest.split("]", 1)[0][1:]
    return rest.rsplit(":", 1)[0] if ":" in rest else rest


def load(path):
    with open(path, encoding="utf-8") as f:
        return unwrap(json.load(f))


def unwrap(snap):
    """`/snapshot` (T14.4) なら、その中の `/hosts` の部分を `/hosts` と同じ形で返す。

    `/snapshot` は `/status` も `/hosts` も持っているが、**ここで使うのは `/hosts`**
    (`.rrd` にある全ホスト。`/status` の `hosts[]` は要求数の上位 50 だけ)。
    窓の目印 (`uptime_secs` / `total_requests`) は `/hosts` にも同じ名前で入っている。
    """
    if not (isinstance(snap, dict) and "parts" in snap and isinstance(snap.get("hosts"), dict)):
        return snap
    inner = dict(snap["hosts"])
    inner.setdefault("uptime_secs", snap.get("uptime_secs", 0))
    inner["snapshot_taken_at"] = snap.get("taken_at")
    return inner


def up(snap):
    """起動からの秒 (`/hosts` にも同じ名前で入っている。無ければ 0)。"""
    return snap.get("uptime_secs", 0)


def reqs(snap):
    """起動からの要求数 (同上)。"""
    return snap.get("total_requests", 0)


def has_aaaa(name):
    try:
        return bool(socket.getaddrinfo(name, None, socket.AF_INET6))
    except OSError:
        return False


def resolve_aaaa(names, mode, table):
    """ホスト名 -> True / False / None (不明)"""
    if mode == "none":
        return {n: None for n in names}
    if mode == "file":
        return {n: table.get(n) for n in names}
    with ThreadPoolExecutor(max_workers=16) as pool:
        return dict(zip(names, pool.map(has_aaaa, names)))


def row_of(key, a, b):
    """1 ホストぶんの値。b が None なら通算、あれば差分。"""
    zero = {"requests": 0, "timed": 0, "avg_ms": 0.0, "bytes": 0, "hits": 0,
            "misses": 0, "errors": 0, "blocked": 0}
    if b is None:
        cur, old = a, zero
    else:
        cur, old = b, (a or zero)
    d_timed = cur["timed"] - old["timed"]
    ms_sum = cur["avg_ms"] * cur["timed"] - old["avg_ms"] * old["timed"]
    if d_timed > 0:
        avg = ms_sum / d_timed
        # avg_ms は小数第 1 位までなので、合計 ms の丸め誤差は最悪 0.05 × timed。
        err = 0.05 * (cur["timed"] + old["timed"]) / d_timed
    else:
        avg, err = None, None
    # 待ちの内訳 (T12.4 (2) より前の古い雪像には無いので、無ければ 0)
    def d(name):
        return cur.get(name, 0) - old.get(name, 0)
    causes = [x - y for x, y in zip(cur.get("errors_by_cause", [0] * len(CAUSE_NAMES)),
                                    old.get("errors_by_cause", [0] * len(CAUSE_NAMES)))]
    return {
        "key": key,
        "name": host_name(key),
        "connect": key.startswith(CONNECT),
        "requests": cur["requests"] - old["requests"],
        "timed": d_timed,
        "avg_ms": avg,
        "avg_err": err,
        "bytes": cur["bytes"] - old["bytes"],
        "errors": cur["errors"] - old["errors"],
        "p50_ms": cur.get("p50_ms"),
        "p95_ms": cur.get("p95_ms"),
        "max_ms": cur.get("max_ms"),
        "dns_ms_sum": d("dns_ms_sum"),
        "dns_misses": d("dns_misses"),
        "connect_ms_sum": d("connect_ms_sum"),
        "errors_by_cause": causes,
    }


def causes_text(counts):
    """`[80, 0, ...]` -> `dns 80`。全部 0 なら `—`。"""
    shown = [f"{CAUSE_NAMES[i]} {n}" for i, n in enumerate(counts) if n]
    return " ".join(shown) if shown else "—"


def per_miss(row):
    """名前解決のミス 1 回あたりの ms (ミスが無ければ None)。"""
    return row["dns_ms_sum"] / row["dns_misses"] if row["dns_misses"] else None


def per_conn(row):
    """確立 1 回あたりの ms (計った要求が無ければ None)。"""
    return row["connect_ms_sum"] / row["timed"] if row["timed"] else None


def fmt_bytes(n):
    if n < 0:
        return "-" + fmt_bytes(-n)
    for unit, div in (("GB", 1 << 30), ("MB", 1 << 20), ("kB", 1 << 10)):
        if n >= div:
            return f"{n / div:.1f} {unit}"
    return f"{n} B"


def cell(s, width):
    """全角を 2 桁と数えて右詰めする (AAAA の欄の 有 / 無 用)。"""
    shown = sum(2 if unicodedata.east_asian_width(c) in "WF" else 1 for c in s)
    return " " * max(0, width - shown) + s


def fmt_ms(v):
    return "—" if v is None else f"{v:.1f}"


def median(values):
    return statistics.median(values) if values else None


def quantile_ms(buckets, count, top, q, bounds):
    """対数バケツから分位点を出す。

    **`crates/metrics/src/history.rs` の `Window::quantile_ms` と同じ補間**
    (バケツの中では一様分布とみなして直線で補い、`ms_max` で頭を止める)。
    `dashboard.html` の `winQuantile` と `scripts/snapshot-summary.py` の `quantile` も同じ。
    """
    if not count or not buckets:
        return None
    rank = max(1.0, min(max(q, 0.0), 1.0) * count)
    seen = 0
    for i, n in enumerate(buckets):
        if not n:
            continue
        if seen + n >= rank:
            lo = 0.0 if i == 0 else float(bounds[i - 1])
            hi = float(bounds[i]) if i < len(bounds) else max(float(top), lo)
            return min(lo + (hi - lo) * ((rank - seen) / n), float(top))
        seen += n
    return float(top)
