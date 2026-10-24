#!/usr/bin/env python3
# 名前解決の**負のキャッシュ** (`PROXY_DNS_NEGATIVE_SECS`、既定 60 秒) の持ち時間を変えたときに、
# 引き直し (OS への問い合わせ) と待ちがどれだけ減るか、**誤って断る本数**がいくつになるかを
# 雪像 1 枚の上で数える (T.27.3 段 1)。
#
# 材料は雪像 (`/snapshot`) 1 枚:
#   - `/log` の `502 Bad Gateway: connect <host:port> failed: failed to lookup address information:
#     Name or service not known` の行 … NXDOMAIN で断った要求 1 本ずつの時刻 (覚えていた失敗を返した
#     要求も同じ文面で書く。`/errors` の 1 行は同じ失敗をまとめたものなので、時刻はこちらから取る)
#   - `/recent` (閉じた接続の個票) … 突き合わせ。`reason == "error:nxdomain"` の行のうち `ms.dns` が 0 で
#     ないものが本当に OS に問い合わせた回 (0 は覚えていた失敗を返した回)。成功の時刻 (秒まで) もここから
#   - `/hosts` の `requests` − `errors` … 宛先ごとの成功の総数 (時刻は無い)
#
# **写した規則** (`crates/net-dns/src/dns.rs` の `resolve_host`):
#   - 名前ごとに、最後に OS へ問い合わせて失敗した時刻 `f` を覚え、`t − f < N` の要求には同じエラーを返す
#     (`negative_hits`)。`t − f >= N` なら問い合わせ直す (ミス。NXDOMAIN ならまた `f = t`)
#   - 窓の最初の 1 本は、どの N でも問い合わせたとする (窓の前の状態は分からない。案の間の差には効かない)
#   - 覚えるのは NXDOMAIN (EAI_NONAME) だけ。一時的な失敗 (`dns_temp` = EAI_AGAIN) は数に入れない
#     (本体で持ち時間を変えるなら NXDOMAIN だけにする前提。一時的な失敗はすぐ引き直したい)
#
# **数え方**:
#   - 引き直し (回) と縮む待ち: 失敗の時刻だけで決まる。縮む待ちは、減った問い合わせ × NXDOMAIN 1 回の
#     平均 (`/recent` の `ms.dns` が 0 でない NXDOMAIN の行の平均)。1 日あたりは窓の長さで割って 86,400 倍
#   - 誤って断る本数 (= 覚えている間に着いた成功): 下限は `/recent` の成功 (同じ名前、`reason` が
#     `error:` でない行) のうち覚えている間に着いたもの。上限は下限 + その名前の `/hosts` の成功の総数
#     (時刻が分からないので、全部が覚えている間に着いたとする)
#
# 使い方: scripts/dns-negative-replay.py SNAP.json [--secs N]... [--top K] [--json]
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import json
import re
import sys

# 段 1 の N (T.27.3)。最初が今の既定
DEFAULT_SECS = (60, 300, 900, 3600)
# 目安 (T.27.3): 待ちの合計が 1 日 1 分以上縮み、誤って断る本数が 0 なら段 2 (既定を変える)
TARGET_SAVED_SECS_PER_DAY = 60.0

NXDOMAIN = "failed to lookup address information: Name or service not known"
LOG_RE = re.compile(r"^502 Bad Gateway: connect (\S+) failed: (.*)$")


def host_of(target):
    """`host:port` (と `[v6]:port`) から名前を取る。"""
    t = target or ""
    if t.startswith("["):
        return t[1:t.find("]")] if "]" in t else t
    return t.rsplit(":", 1)[0] if ":" in t else t


def part(snap, name, key):
    v = snap.get(name)
    if isinstance(v, dict):
        v = v.get(key)
    return v if isinstance(v, list) else []


def arrivals(snap):
    """`/log` から NXDOMAIN で断った要求の `(時刻, 名前)` を古い順に返す。ほかの失敗の行数も数える。"""
    out, other = [], 0
    for line in part(snap, "log", "lines"):
        m = LOG_RE.match(line.get("msg") or "")
        if not m:
            continue
        if m.group(2).strip() == NXDOMAIN:
            out.append((line.get("at") or 0, host_of(m.group(1))))
        elif "lookup" in m.group(2):
            other += 1
    out.sort()
    return out, other


def replay(arr, secs):
    """負のキャッシュの持ち時間 `secs` で回し、`(問い合わせの回数, 覚えていた失敗を返した回数, 名前ごとの最後の失敗の列)`。

    3 つ目は `{名前: [問い合わせた時刻, …]}` (誤って断る本数を数えるのに使う)。
    """
    last, looked = {}, {}
    lookups = hits = 0
    for t, name in arr:
        f = last.get(name)
        if f is not None and t - f < secs:
            hits += 1
            continue
        lookups += 1
        last[name] = t
        looked.setdefault(name, []).append(t)
    return lookups, hits, looked


def remembered(looked, name, t, secs):
    """名前 `name` に時刻 `t` に着いた要求が、持ち時間 `secs` のもとで覚えていた失敗で断られるか。"""
    for f in looked.get(name, ()):
        if f <= t < f + secs:
            return True
    return False


def successes(snap):
    """`/recent` の成功 (`reason` が `error:` でない connect の行) の `{名前: [時刻, …]}` と、`/hosts` の成功の総数。"""
    timed = {}
    for r in part(snap, "recent", "recent"):
        if not isinstance(r, dict) or (r.get("reason") or "").startswith("error:"):
            continue
        timed.setdefault(host_of(r.get("target")), []).append(r.get("at") or 0)
    total = {}
    for h in part(snap, "hosts", "hosts"):
        name = host_of((h.get("host") or "").split("://", 1)[-1])
        ok = (h.get("requests") or 0) - (h.get("errors") or 0)
        if ok > 0:
            total[name] = total.get(name, 0) + ok
    return timed, total


def calibrate(snap, arr):
    """`/recent` の窓で、規則 60 秒の模型と実機の問い合わせの回数を突き合わせる。"""
    rows = [r for r in part(snap, "recent", "recent")
            if isinstance(r, dict) and r.get("reason") == "error:nxdomain"]
    if not rows:
        return None
    lo = min(r.get("at") or 0 for r in rows)
    looked = sum(1 for r in rows if ((r.get("ms") or {}).get("dns") or 0) > 0)
    ms = [(r.get("ms") or {}).get("dns") or 0 for r in rows]
    ms = [x for x in ms if x > 0]
    inside = [(t, n) for t, n in arr if t >= lo]
    # 窓の前に覚えていた失敗は模型に見えないので、窓の 60 秒前からの到着で状態を作る
    warm = [(t, n) for t, n in arr if lo - 60 <= t]
    model = sum(1 for t in replay(warm, 60)[2].values() for x in t if x >= lo)
    return {"from": lo, "rows": len(rows), "looked_up": looked, "log_rows": len(inside),
            "model_lookups": model, "avg_ms": (sum(ms) / len(ms)) if ms else None}


def run(snap, secs_list):
    arr, other = arrivals(snap)
    taken = snap.get("taken_at") or (arr[-1][0] if arr else 0)
    span = (taken - arr[0][0]) if arr else 0
    cal = calibrate(snap, arr)
    avg_ms = (cal or {}).get("avg_ms")
    if avg_ms is None:
        # T.25.0 より前の版は NXDOMAIN を `dns` の 1 つにまとめていた (`nxdomain` の綴りは 0.25 から)
        errs = [e.get("dns_ms") or 0 for e in part(snap, "errors", "errors") if e.get("cause") in ("nxdomain", "dns")]
        errs = [x for x in errs if x > 0]
        avg_ms = (sum(errs) / len(errs)) if errs else 0.0
    timed, total = successes(snap)
    names = {n for _, n in arr}
    base = None
    cases = []
    for secs in secs_list:
        lookups, hits, looked = replay(arr, secs)
        lo = sum(1 for n in names for t in timed.get(n, ()) if remembered(looked, n, t, secs))
        hi = lo + sum(total.get(n, 0) for n in names if secs > 0)
        if base is None:
            base = lookups
        saved = (base - lookups) * avg_ms / 1000.0
        per_day = saved * 86400.0 / span if span else 0.0
        cases.append({"secs": secs, "lookups": lookups, "negative_hits": hits,
                      "fewer_lookups": base - lookups, "saved_secs": saved, "saved_secs_per_day": per_day,
                      "false_refusals": [lo, hi],
                      "go": per_day >= TARGET_SAVED_SECS_PER_DAY and hi == 0})
    top = {}
    for _, n in arr:
        top[n] = top.get(n, 0) + 1
    return {"taken_at": taken, "from": arr[0][0] if arr else None, "span_secs": span,
            "arrivals": len(arr), "names": len(names), "other_lookup_failures": other,
            "avg_ms": avg_ms, "calibration": cal, "cases": cases,
            "top": sorted(top.items(), key=lambda kv: -kv[1]),
            "names_with_success": sorted(n for n in names if n in total or n in timed)}


def render(out, k):
    p = print
    h = out["span_secs"] / 3600.0
    p(f"# dns-negative-replay: NXDOMAIN {out['arrivals']:,} 本 / {out['names']:,} 名前、"
      f"窓 {h:,.2f} 時間 (`/log`)")
    p(f"# NXDOMAIN 1 回の問い合わせ {out['avg_ms']:,.1f} ms。一時的な失敗など ほかの名前解決の失敗の行 "
      f"{out['other_lookup_failures']} 本は数えない")
    c = out["calibration"]
    if c:
        p(f"# 突き合わせ (`/recent` の窓 {c['rows']:,} 行): 実機の問い合わせ {c['looked_up']:,} 回 / "
          f"模型 (60 秒) {c['model_lookups']:,} 回。`/log` の同じ窓の行 {c['log_rows']:,} 本")
    p()
    p("| 持ち時間 | 問い合わせ | 覚えていた失敗を返した | 減った問い合わせ | 縮む待ち (窓) | 縮む待ち (1 日) | 誤って断る (下限〜上限) | 段 2 |")
    p("|---:|---:|---:|---:|---:|---:|---:|---|")
    for x in out["cases"]:
        lo, hi = x["false_refusals"]
        p(f"| {x['secs']:,} 秒 | {x['lookups']:,} | {x['negative_hits']:,} | {x['fewer_lookups']:,} | "
          f"{x['saved_secs']:,.1f} 秒 | {x['saved_secs_per_day'] / 60:,.2f} 分 | {lo}〜{hi} | "
          f"{'進む' if x['go'] else '—'} |")
    p()
    p(f"目安: 1 日 {TARGET_SAVED_SECS_PER_DAY / 60:.0f} 分以上縮み、誤って断る本数の上限が 0。")
    if out["names_with_success"]:
        p(f"成功もある名前 (誤って断りうる): {', '.join(out['names_with_success'][:k])}")
    p("NXDOMAIN の多い名前: " + "、".join(f"`{n}` {c}" for n, c in out["top"][:k]))


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("snapshot")
    ap.add_argument("--secs", type=int, action="append", metavar="N",
                    help=f"負のキャッシュの持ち時間 (秒。複数回。既定 {', '.join(map(str, DEFAULT_SECS))}。最初が基準)")
    ap.add_argument("--top", type=int, default=8)
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args(argv)
    with open(a.snapshot) as f:
        snap = json.load(f)
    out = run(snap, a.secs or list(DEFAULT_SECS))
    if a.json:
        json.dump(out, sys.stdout, ensure_ascii=False, indent=1)
        print()
    else:
        render(out, a.top)
    return 0


if __name__ == "__main__":
    sys.exit(main())
