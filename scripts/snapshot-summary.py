#!/usr/bin/env python3
# `/snapshot` (T14.4) を 1 枚の Markdown の「要点」に畳む (`scripts/collect-deployed.sh` が呼ぶ)。
#
# 見るのは T14.0 の分析で実際に要った順: 全体 → 名前解決 → エラー → 閉じた接続 → いまの接続。
# **`--prev` を渡すと通算の指標は差分**になる (`/status` の数はどれも起動からの通算なので、
# そのまま読むと「いつからの値か」が混ざる)。
#
# 使い方: scripts/snapshot-summary.py SNAP.json [--prev PREV.json]
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import json
import statistics
import sys

# `/status` の `errors_by_cause` の並び (`crates/metrics/src/metrics.rs` の ERR_CAUSE_NAMES)
CAUSE_NAMES = ["dns", "refused", "unreachable", "timeout", "reset", "tls", "loop", "other"]


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def num(v, unit=""):
    if v is None:
        return "—"
    if isinstance(v, float):
        return f"{v:,.1f}{unit}"
    return f"{v:,}{unit}"


def fmt_bytes(n):
    if n is None:
        return "—"
    for unit, div in (("GB", 1 << 30), ("MB", 1 << 20), ("kB", 1 << 10)):
        if abs(n) >= div:
            return f"{n / div:.1f} {unit}"
    return f"{n} B"


def quantile(buckets, count, top, p, bounds):
    """`/history` の対数バケツから分位点を出す (dashboard.html の `winQuantile` と同じ)。"""
    if not count or not buckets:
        return None
    rank = max(1.0, p * count)
    seen = 0
    for i, n in enumerate(buckets):
        if not n:
            continue
        if seen + n >= rank:
            lo = 0 if i == 0 else bounds[i - 1]
            hi = bounds[i] if i < len(bounds) else max(top, lo)
            return min(lo + (hi - lo) * ((rank - seen) / n), top)
        seen += n
    return top


def merge(history, prefix, last=None):
    """履歴の標本を足し合わせて (件数, バケツ, 最大, 合計 ms) にする。`last` で直近 N 標本だけ。"""
    if not history:
        return 0, None, 0, 0.0
    keys = history.get("keys") or []
    rows = history.get("samples") or []
    if last:
        rows = rows[-last:]
    idx = {k: i for i, k in enumerate(keys)}
    need = [prefix + "_buckets", prefix + "_ms_max", prefix + "_ms_sum",
            "connects" if prefix == "connect" else "forwards"]
    if any(k not in idx for k in need):
        return 0, None, 0, 0.0
    count, buckets, top, total = 0, None, 0, 0.0
    for r in rows:
        b = r[idx[prefix + "_buckets"]]
        if not b:
            continue
        if buckets is None:
            buckets = [0] * len(b)
        for j, v in enumerate(b):
            buckets[j] += v
        count += r[idx[need[3]]] or 0
        total += r[idx[prefix + "_ms_sum"]] or 0
        top = max(top, r[idx[prefix + "_ms_max"]] or 0)
    return count, buckets, top, total


def latency_line(label, history, prefix, last=None):
    count, buckets, top, total = merge(history, prefix, last)
    if not count:
        return f"| {label} | 0 本 | — | — | — |"
    bounds = history.get("bounds_ms") or []
    p50 = quantile(buckets, count, top, 0.5, bounds)
    p95 = quantile(buckets, count, top, 0.95, bounds)
    return (f"| {label} | {count:,} 本 | {total / count:.1f} ms | "
            f"{num(p50)} / {num(p95)} ms | {num(top)} ms |")


def tally(rows, key):
    out = {}
    for r in rows:
        out[r.get(key)] = out.get(r.get(key), 0) + 1
    return dict(sorted(out.items(), key=lambda kv: -kv[1]))


def host_errors(snap):
    """`/hosts` の全ホストを足して (エラー件数, 原因別) を出す (`/status` に合計が無いため)。"""
    rows = part(snap, "hosts").get("hosts") or []
    total = sum(h.get("errors", 0) for h in rows)
    causes = [0] * len(CAUSE_NAMES)
    for h in rows:
        for i, n in enumerate(h.get("errors_by_cause") or []):
            if i < len(causes):
                causes[i] += n
    return total, causes


def part(snap, name):
    v = snap.get(name)
    return v if isinstance(v, dict) else {}


def main():
    p = argparse.ArgumentParser(description="/snapshot を 1 枚の要点に畳む")
    p.add_argument("snapshot")
    p.add_argument("--prev", metavar="PREV.json", help="前回の雪像 (通算の指標を差分にする)")
    args = p.parse_args()
    d = load(args.snapshot)
    prev = load(args.prev) if args.prev else None
    if "parts" not in d:
        print("(これは /snapshot の JSON ではない)")
        return 1

    st = part(d, "status")
    pst = part(prev, "status") if prev else {}
    window = ""
    if prev:
        dt = (d.get("taken_at") or 0) - (prev.get("taken_at") or 0)
        restarted = (st.get("uptime_secs") or 0) < (pst.get("uptime_secs") or 0)
        window = (f" (前回からの窓 {dt / 3600:.1f} 時間"
                  + ("、**再起動をまたいでいる**" if restarted else "") + ")")

    def delta(path, default=0):
        """`status` の通算から前回を引く (`--prev` が無ければそのまま)。"""
        cur, old = st, pst
        for k in path[:-1]:
            cur, old = cur.get(k, {}) or {}, (old.get(k, {}) or {})
        c = cur.get(path[-1], default)
        if not prev:
            return c
        o = old.get(path[-1], default)
        if isinstance(c, (int, float)) and isinstance(o, (int, float)) and c >= o:
            return c - o
        return c

    print(f"- 版 `{d.get('version')}` / 起動から {(st.get('uptime_secs') or 0) / 3600:.1f} 時間"
          f" / 取得 {d.get('taken_at')}{window}")
    if d.get("dropped"):
        print(f"- **4 MiB を越えたので落とした部分**: {', '.join(d['dropped'])}")
    print()

    reqs = delta(["total_requests"])
    secs = (d.get("taken_at", 0) - prev.get("taken_at", 0)) if prev else (st.get("uptime_secs") or 0)
    print("| 全体 | 値 |")
    print("|---|---|")
    print(f"| 要求 | {num(reqs)} ({reqs / secs:.3f} /s) |" if secs else f"| 要求 | {num(reqs)} |")
    print(f"| 転送 | {fmt_bytes(delta(['bytes_forwarded']))} |")
    print(f"| いまの接続 | {num(st.get('active_connections'))} / 上限 {num(st.get('max_conns'))}"
          f" (預かり所 {num(st.get('parked_connections'))}、うちトンネル {num(st.get('parked_tunnels'))}) |")
    print(f"| スレッド / fd | {num(st.get('live_threads'))} / {num(st.get('max_threads'))}"
          f" ・ {num(st.get('fds'))} / {num(st.get('max_fds'))} |")
    print(f"| 上限で断った / 追い出した | {num(delta(['rejected_overload']))}"
          f" / {num(delta(['evicted_idle']))} |")
    cache = st.get("cache") or {}
    sysinfo = cache.get("system") or {}
    print(f"| キャッシュ | 命中率 {num(cache.get('hit_ratio'))} "
          f"(使用 {fmt_bytes((cache.get('memory') or {}).get('used_bytes'))}) |")
    print(f"| RSS | {fmt_bytes(sysinfo.get('process_rss_bytes'))} |")
    print()

    dns = st.get("dns") or {}
    pdns = (pst.get("dns") or {}) if prev else {}
    misses = dns.get("misses", 0) - (pdns.get("misses", 0) if prev else 0)
    ms_sum = dns.get("miss_ms_sum", 0.0) - (pdns.get("miss_ms_sum", 0.0) if prev else 0.0)
    print("| 名前解決 | 値 |")
    print("|---|---|")
    print(f"| ミス | {num(misses)} ({misses / reqs:.2f} /要求) |" if reqs else f"| ミス | {num(misses)} |")
    print(f"| ミス 1 回 | {ms_sum / misses:.1f} ms |" if misses else "| ミス 1 回 | — |")
    print(f"| 表 / warm / 引き直し | {num(dns.get('entries'))} / {num(dns.get('warm'))}"
          f" / {num(dns.get('refreshes', 0) - (pdns.get('refreshes', 0) if prev else 0))} |")
    print(f"| 負のキャッシュ命中 / 古い答えで代用 | {num(dns.get('negative_hits'))}"
          f" / {num(dns.get('stale_served'))} |")
    print()

    hist = (d.get("history") or {})
    print("| 待ち (履歴の全標本) | 本数 | 平均 | p50 / p95 | 最大 |")
    print("|---|---|---|---|---|")
    for label, res, last in (("CONNECT 確立 (直近 1 時間)", "5", None),
                             ("CONNECT 確立 (直近 1 日)", "60", None),
                             ("CONNECT 確立 (通算)", "3600", None),
                             ("forward 初バイト (直近 1 時間)", "5", None),
                             ("forward 初バイト (通算)", "3600", None)):
        prefix = "connect" if label.startswith("CONNECT") else "forward"
        print(latency_line(label, hist.get(res) or {}, prefix, last))
    print()

    # エラーの合計は `/status` に無いので、`/hosts` (全ホスト) を足して出す
    errors = part(d, "errors").get("errors") or []
    total_err, causes = host_errors(d)
    if prev:
        old_err, old_causes = host_errors(prev)
        total_err -= old_err
        causes = [a - b for a, b in zip(causes, old_causes)]
    shown = " ".join(f"{CAUSE_NAMES[i]} {n}" for i, n in enumerate(causes) if n) or "—"
    print(f"- エラー: {num(total_err)} 件 (`.rrd` の通算をホストで足したもの) / 原因 {shown}"
          f" / 個票 {len(errors)} 件")
    for e in errors[:5]:
        print(f"  - `{e.get('at')}` {e.get('kind')} {e.get('target')} → {e.get('status')}"
              f" ({e.get('cause')}、dns {e.get('dns_ms')} ms / connect {e.get('connect_ms')} ms、"
              f"from {e.get('client')})")
    print()

    rec = part(d, "recent")
    rows = rec.get("recent") or []
    print(f"- 閉じた接続 (`/recent`): 通算 {num(rec.get('recorded'))} 本 / 覚えている {len(rows)} 本"
          + ("" if not rec.get("truncated") else " (**応答は 256 KiB で切れている**)"))
    if rows:
        print(f"  - 閉じた理由: " + ", ".join(f"{k} {v}" for k, v in tally(rows, "reason").items()))
        print(f"  - 種類: " + ", ".join(f"{k} {v}" for k, v in tally(rows, "kind").items()))
        secs_all = [r.get("secs", 0) for r in rows]
        print(f"  - 寿命: 中央値 {statistics.median(secs_all):.0f} 秒 / 最大 {max(secs_all):,} 秒")
        up = sum(r.get("up", 0) for r in rows)
        down = sum(r.get("down", 0) for r in rows)
        print(f"  - バイト: 上り {fmt_bytes(up)} / 下り {fmt_bytes(down)}")
        top_clients = tally(rows, "client")
        print("  - 接続元: " + ", ".join(f"{k} {v}" for k, v in list(top_clients.items())[:5]))
        slow = sorted(rows, key=lambda r: -(r.get("ms", {}).get("connect", 0)))[:5]
        print("  - 確立の遅かった 5 本:")
        for r in slow:
            ms = r.get("ms", {})
            print(f"    - {r.get('target')} connect {ms.get('connect', 0)} ms"
                  f" / dns {ms.get('dns', 0)} ms ({r.get('reason')}、{r.get('secs')} 秒、"
                  f"{fmt_bytes(r.get('up', 0) + r.get('down', 0))})")
    print()

    conns = part(d, "connections").get("connections") or []
    print(f"- いまの接続 (`/connections`): {len(conns)} 本"
          + ((" / " + ", ".join(f"{k} {v}" for k, v in tally(conns, "state").items())) if conns else ""))
    log = part(d, "log").get("lines") or []
    print(f"- 警告と失敗 (`/log`): {len(log)} 行"
          + ((" / 直近 `" + (log[0].get("msg") or "")[:120] + "`") if log else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
