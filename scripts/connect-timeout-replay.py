#!/usr/bin/env python3
# CONNECT のオリジン接続の締め切りを T 秒にしたら、**落とす成功が何本**・**待ちが何分/日 縮む**かを
# 雪像の上で数える (T23.3 段 1)。
#
# 材料は雪像 1 枚の中の 2 つ:
#   - `/recent` (閉じた接続の個票のリング。2,000 件で切れる) … **成功した CONNECT 1 本ずつの
#     `ms.connect`** (`kind == "connect"` で `reason` が `error` で始まらない行) と、
#     `reason == "error:timeout"` の行の `secs` (1 件が待たされた秒)。
#     窓は「いちばん古い個票の `at` から取得まで」で、**リングの容量で短くなる** (窓と件数を必ず出す)。
#   - `/history` の `3600` … `connect_buckets` (1 時間ごとの、確立までの ms のヒストグラム) と
#     `errors_by_cause` の `timeout`。**状態ファイルから読み戻すので起動をまたいで残る** (10 日ほど)。
#     窓は標本の数 × `interval_secs`。
#
# **`/history` の `connect_buckets` は成功だけとは限らない**: T20.4 より前の版は、繋がらなかった
# 接続の「失敗するまでの時間」も同じ窓に入れていた (0.18.0 の nagoya の雪像がそう)。
# 10 秒で切った `timeout` は一番上の区間 (5,000 ms 以上) に入るので、そこだけ `timeout` の本数を
# 引いた値を「下限」に、引かない値を「上限」にする (T が区間の途中に落ちるときは、その区間を
# 丸ごと上限にだけ足す。区間の中は分からないため)。
#
# 数えるもの (T = 3 / 5 / 10 秒。`--from` は雪像を撮ったときの既定 = 10 秒):
#   - **落とす成功** … 確立に T 秒より掛かった成功の本数。T 秒で切ると 502 になるはずのもの。
#     `/recent` では 1 本ずつ分かるので**ちょうど**、`/history` では区間の幅だけ**下限〜上限**。
#   - **縮む待ち (分/日)** … `timeout` 1 件はそのときの既定の締め切り (`--from`、既定 10 秒) を
#     丸ごと待っているので、T 秒にすれば 1 件あたり (from − T) 秒短くなる。
#     `timeout` の本数/日 × (from − T) / 60。
#     **落とす成功は待ちを縮めない** (成功していたものが失敗に変わるだけ) ので、ここには入れない。
#   - **timeout の件数** … いまの本数と、T を当てたあとの本数 (いま + 落とす成功)。
#
# 使い方: scripts/connect-timeout-replay.py SNAP.json... [--secs N]... [--hours H] [--from S] [--json]
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import json
import re
import sys

# 段 1 で比べる締め切り (TODO.md の T23.3)
DEFAULT_SECS = (3, 5, 10)
# **雪像を撮ったときの**既定の締め切り (秒)。この道具は「その締め切りから T に下げたら」を数える。
# T23.3 (2026-10-20) に 5 秒へ下げたので、それ以降の雪像は `--from 5` で数える
# (ここを書き換えない。古い雪像を数え直したときに意味が変わらないようにするため)。
DEFAULT_FROM_SECS = 10.0
# T20.4 で、繋がらなかった接続は connect の窓 (`connects` / `connect_buckets`) に入らなくなった。
# これより前の Phase の版の雪像は、失敗するまでの時間も区間に入っている (下限を出すときに timeout を引く)。
# クレートの版は T21.6 まで `0.<Phase>.<番号>` で、T26.3 からはデプロイの番号 `0.<波>.<番>`。
# どちらも 2 番目の数が「T20.4 より後か」を表す (0.20.0 以降は T20.4 を含む。デプロイの番号は前にしか進まない) ので、
# 2 番目の数だけで見る。古い雪像は撮ったときの版のまま残っている。
FIRST_PHASE_WITHOUT_FAILURES = 20
# 目安 (T23.3 の段 2 に進む条件): 落とす成功が 1 日 1 本未満
MAX_DROPPED_PER_DAY = 1.0
DAY = 86400.0

VERSION_RE = re.compile(r"^0\.(\d+)\.(\d+)")


def part(snap, name, key):
    """`/snapshot` の部 (`{"recent": [...], "count": N, ...}` の形) を安全に取る。"""
    v = snap.get(name)
    if isinstance(v, dict) and isinstance(v.get(key), list):
        return v, v[key]
    return {}, []


def failures_in_buckets(version):
    """この版の `connect_buckets` に、繋がらなかった接続も入っているか。

    `True` = 入っている (T20.4 より前)、`False` = 成功だけ、`None` = 版が読めない。
    """
    m = VERSION_RE.match(version or "")
    if not m:
        return None
    return int(m.group(1)) < FIRST_PHASE_WITHOUT_FAILURES


def is_success(e):
    """成功した CONNECT の個票か (`/recent` の 1 行)。"""
    if e.get("kind") != "connect":
        return False
    return not str(e.get("reason") or "").startswith("error")


def recent_window(snap):
    """`/recent` から、窓・成功の確立時間・timeout を取る (個票が無ければ `None`)。"""
    r, rows = part(snap, "recent", "recent")
    if not rows:
        return None
    taken = int(snap.get("taken_at", 0))
    first = min(int(e.get("at", taken)) for e in rows)
    ok = [int((e.get("ms") or {}).get("connect") or 0) for e in rows if is_success(e)]
    tmo = [e for e in rows if e.get("kind") == "connect" and e.get("reason") == "error:timeout"]
    return {
        "window_secs": max(1, taken - first),
        "entries": len(rows),
        "capacity": r.get("capacity"),
        "truncated": bool(r.get("truncated")),
        "restored": r.get("restored"),
        "connects": len(ok),
        "max_connect_ms": max(ok) if ok else None,
        "timeouts": len(tmo),
        "timeout_ms_avg": (sum(int((e.get("ms") or {}).get("connect") or 0) for e in tmo) / len(tmo)
                           if tmo else None),
        "connect_ms": ok,
    }


def history_window(snap, hours=None):
    """`/history` の `3600` を畳んで、窓・成功のヒストグラム・timeout を取る。"""
    h = (snap.get("history") or {}).get("3600")
    if not isinstance(h, dict):
        return None
    keys = h.get("keys") or []
    rows = h.get("samples") or []
    if not keys or not rows:
        return None
    idx = {k: i for i, k in enumerate(keys)}
    need = ("t", "connects", "connect_buckets", "errors_by_cause")
    if any(k not in idx for k in need):
        return None
    causes = h.get("causes") or []
    ti = causes.index("timeout") if "timeout" in causes else None
    step = int(h.get("interval_secs") or 3600)
    rows = sorted(rows, key=lambda r: r[idx["t"]])
    if hours:
        # `--hours H` = 直近 H 時間ぶんの標本 (1 標本 = 1 区間なので H 本)。境目は含めない
        lo = rows[-1][idx["t"]] - hours * step
        rows = [r for r in rows if r[idx["t"]] > lo]
        if not rows:
            return None
    buckets = None
    conn = tmo = 0
    for r in rows:
        conn += r[idx["connects"]] or 0
        if ti is not None:
            tmo += (r[idx["errors_by_cause"]] or [])[ti] or 0
        b = r[idx["connect_buckets"]] or []
        if buckets is None:
            buckets = [0] * len(b)
        for j, v in enumerate(b[:len(buckets)]):
            buckets[j] += v or 0
    step = int(h.get("interval_secs") or 3600)
    # 標本は 1 本 = 直前の 1 区間ぶんの差分なので、覆う時間は「最後と最初の差 + 1 区間」
    window = (rows[-1][idx["t"]] - rows[0][idx["t"]]) + step
    bounds = h.get("bounds_ms") or []
    version = snap.get("version")
    return {
        "window_secs": window,
        "samples": len(rows),
        "connects": conn,
        "timeouts": tmo,
        "buckets": buckets or [],
        "bounds": bounds,
        "failures_in_buckets": failures_in_buckets(version),
        "version": version,
    }


def bounds_of(bounds, j):
    """区間 j の下端と上端 (ms)。上端が `None` なら一番上 (無限大)。"""
    lo = 0.0 if j == 0 else float(bounds[j - 1])
    hi = float(bounds[j]) if j < len(bounds) else None
    return lo, hi


def dropped_from_buckets(buckets, bounds, secs, timeouts, subtract):
    """確立に `secs` 秒より掛かった成功の本数を、区間から (下限, 上限) で出す。

    `subtract` = 一番上の区間から `timeout` の本数を引く (T20.4 より前の版。10 秒で切った失敗が
    そこに入っているため)。引いた方を下限、引かない方を上限にする。
    `secs` が区間の途中に落ちる (例: 2,500〜5,000 ms に 3,000 ms) ときは、その区間を丸ごと上限に足す。
    """
    lo = hi = 0
    last = len(buckets) - 1
    for j, n in enumerate(buckets):
        a, b = bounds_of(bounds, j)
        if a >= secs:
            hi += n
            if j == last and subtract:
                lo += max(0, n - timeouts)
            else:
                lo += n
        elif b is None or b > secs:
            hi += n
    return lo, hi


def analyze(snap, secs_list=DEFAULT_SECS, hours=None, ref=DEFAULT_FROM_SECS):
    rec = recent_window(snap)
    hist = history_window(snap, hours)
    res = {
        "taken_at": int(snap.get("taken_at", 0)),
        "version": snap.get("version"),
        "uptime_secs": int(snap.get("uptime_secs", 0)),
        "recent": {k: v for k, v in (rec or {}).items() if k != "connect_ms"} if rec else None,
        "history": hist,
        "by_secs": [],
    }
    for secs in secs_list:
        # いまの締め切り以上に緩めても何も変わらない (成功した接続は締め切りを超えられない)。
        # 区間から数えると一番上の区間 (5,000 ms 以上) を丸ごと数えてしまうので、ここで 0 と決める。
        noop = secs >= ref
        row = {"secs": secs, "recent_dropped": None, "recent_dropped_per_day": None,
               "history_dropped_lo": None, "history_dropped_hi": None,
               "history_dropped_lo_per_day": None, "history_dropped_hi_per_day": None,
               "saved_min_per_day": None, "saved_min_per_day_recent": None,
               "timeouts": None, "timeouts_after_lo": None, "timeouts_after_hi": None}
        if rec:
            n = 0 if noop else sum(1 for ms in rec["connect_ms"] if ms > secs * 1000)
            row["recent_dropped"] = n
            row["recent_dropped_per_day"] = n * DAY / rec["window_secs"]
            if rec["timeouts"]:
                row["saved_min_per_day_recent"] = (
                    rec["timeouts"] * (ref - secs) * DAY / rec["window_secs"] / 60.0)
        if hist:
            if noop:
                lo = hi = 0
            else:
                lo, hi = dropped_from_buckets(hist["buckets"], hist["bounds"], secs * 1000.0,
                                              hist["timeouts"],
                                              hist["failures_in_buckets"] is not False)
            per_day = DAY / hist["window_secs"]
            row.update({
                "history_dropped_lo": lo, "history_dropped_hi": hi,
                "history_dropped_lo_per_day": lo * per_day,
                "history_dropped_hi_per_day": hi * per_day,
                "saved_min_per_day": hist["timeouts"] * (ref - secs) * per_day / 60.0,
                "timeouts": hist["timeouts"],
                "timeouts_after_lo": hist["timeouts"] + lo,
                "timeouts_after_hi": hist["timeouts"] + hi,
            })
        res["by_secs"].append(row)
    return res


def summary(allres, secs_list=DEFAULT_SECS, ref=DEFAULT_FROM_SECS):
    """雪像をまたいだ「最悪の 1 日」— 段 2 に進むかどうかはここで決める。"""
    out = []
    for secs in secs_list:
        if secs >= ref:
            # いまの既定以上なので何も変わらない (比べる対象ではない)
            out.append({"secs": secs, "worst_dropped_hi_per_day": 0.0,
                        "saved_min_per_day_max": 0.0, "ok": None})
            continue
        his = [b for r in allres.values() for b in r["by_secs"] if b["secs"] == secs]
        his = [b for b in his if b["history_dropped_hi_per_day"] is not None]
        if not his:
            out.append({"secs": secs, "worst_dropped_hi_per_day": None, "ok": None})
            continue
        worst = max(h["history_dropped_hi_per_day"] for h in his)
        saved = max((h["saved_min_per_day"] or 0.0) for h in his)
        out.append({"secs": secs, "worst_dropped_hi_per_day": worst,
                    "saved_min_per_day_max": saved,
                    "ok": worst < MAX_DROPPED_PER_DAY})
    return out


def hours_of(secs):
    return secs / 3600.0


def render(res, label, out=sys.stdout):
    rec, hist = res["recent"], res["history"]
    print(f"## {label} (版 {res['version']}、起動から {hours_of(res['uptime_secs']):.1f} 時間)", file=out)
    if rec:
        cap = rec.get("capacity")
        print(f"/recent: 窓 {hours_of(rec['window_secs']):.2f} 時間 (成功 {rec['connects']} 本、"
              f"timeout {rec['timeouts']} 本、個票 {rec['entries']} 件"
              + (f" / 容量 {cap}" if cap else "")
              + ("、切り詰めあり = この窓より前の個票は雪像に残っていない"
                 if rec.get("truncated") else "") + ")", file=out)
        if rec.get("max_connect_ms") is not None:
            print(f"  成功の確立の最大 {rec['max_connect_ms']} ms"
                  + (f"、timeout 1 件の待ち {rec['timeout_ms_avg'] / 1000.0:.1f} 秒"
                     if rec["timeout_ms_avg"] is not None else ""), file=out)
    else:
        print("/recent: 個票が無い (判定できず)", file=out)
    if hist:
        ver = hist.get("version")
        note = {None: "版が読めないので timeout を引かない (上限は多め)",
                True: "この版は失敗も区間に入れているので一番上の区間から timeout を引いた",
                False: "この版は成功だけを区間に入れている"}[hist["failures_in_buckets"]]
        print(f"/history: 窓 {hours_of(hist['window_secs']):.1f} 時間 ({hours_of(hist['window_secs']) / 24:.1f} 日、"
              f"{hist['samples']} 標本、成功 {hist['connects']} 本、timeout {hist['timeouts']} 本。{note})",
              file=out)
    else:
        print("/history: 標本が無い (判定できず)", file=out)
    if not rec and not hist:
        print("", file=out)
        return
    print("", file=out)
    print("| T 秒 | 落とす成功 (/recent) | 落とす成功 (/history、下限〜上限) "
          "| 落とす成功 本/日 (/history) | 縮む待ち 分/日 | timeout 件数 (いま → T 適用後) |", file=out)
    print("|---:|---:|---:|---:|---:|---:|", file=out)
    for b in res["by_secs"]:
        rd = "判定できず" if b["recent_dropped"] is None else str(b["recent_dropped"])
        hd = ("判定できず" if b["history_dropped_lo"] is None
              else f"{b['history_dropped_lo']}〜{b['history_dropped_hi']}")
        pd = ("判定できず" if b["history_dropped_hi_per_day"] is None
              else f"{b['history_dropped_lo_per_day']:.2f}〜{b['history_dropped_hi_per_day']:.2f}")
        sv = "判定できず" if b["saved_min_per_day"] is None else f"{b['saved_min_per_day']:.1f}"
        to = ("判定できず" if b["timeouts"] is None
              else f"{b['timeouts']} → {b['timeouts_after_lo']}〜{b['timeouts_after_hi']}")
        print(f"| {b['secs']} | {rd} | {hd} | {pd} | {sv} | {to} |", file=out)
    print("", file=out)


def render_summary(summ, out=sys.stdout):
    print("## まとめ (雪像をまたいだ最悪の 1 日)", file=out)
    print("", file=out)
    print("| T 秒 | 落とす成功 本/日 (最悪、上限) | 縮む待ち 分/日 (最大) | 段 2 に進むか |", file=out)
    print("|---:|---:|---:|---|", file=out)
    for s in summ:
        if s["worst_dropped_hi_per_day"] is None:
            print(f"| {s['secs']} | 判定できず | 判定できず | 判定できず |", file=out)
            continue
        verdict = ("いまの既定 (変わらない)" if s["ok"] is None
                   else ("進む" if s["ok"] else "進まない (1 日 1 本以上)"))
        print(f"| {s['secs']} | {s['worst_dropped_hi_per_day']:.2f} "
              f"| {s['saved_min_per_day_max']:.1f} | {verdict} |", file=out)
    print("", file=out)
    print("本/日 と 分/日 は窓の値をそのまま 1 日に延ばしたもの (使い方が変われば動く)。", file=out)
    print("", file=out)


def main(argv=None):
    p = argparse.ArgumentParser(
        description="count the successes a shorter CONNECT deadline would drop")
    p.add_argument("snapshot", nargs="+")
    p.add_argument("--secs", type=int, action="append", default=[], metavar="T",
                   help="deadline to try, in seconds (repeatable; default 3, 5, 10)")
    p.add_argument("--hours", type=int, default=None, metavar="H",
                   help="use only the last H hours of /history (default: the whole window)")
    p.add_argument("--from", dest="ref", type=float, default=DEFAULT_FROM_SECS, metavar="S",
                   help="the deadline in force when the snapshot was taken, in seconds "
                        "(default 10; use 5 for snapshots from T23.3 on)")
    p.add_argument("--json", action="store_true")
    a = p.parse_args(argv)
    secs = tuple(a.secs) or DEFAULT_SECS
    allres = {}
    for path in a.snapshot:
        with open(path, encoding="utf-8") as f:
            snap = json.load(f)
        res = analyze(snap, secs, a.hours, a.ref)
        allres[path] = res
        if not a.json:
            render(res, path)
    summ = summary(allres, secs, a.ref)
    if a.json:
        json.dump({"snapshots": allres, "summary": summ}, sys.stdout, ensure_ascii=False, indent=1)
        print()
    else:
        render_summary(summ)
    return 0


if __name__ == "__main__":
    sys.exit(main())
