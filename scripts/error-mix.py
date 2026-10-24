#!/usr/bin/env python3
# エラーを「使われ方」から切り離して数え直す (T.23.0)。
#
# いまの判定表の「接続 1 本あたりのエラー」は分母が `connects` なので、宛先の生きた割合が
# 変わると簡単に振れる (T.22.99 の 7.80 倍は、接続元 1 本が死んだメールサーバーを総当たりした
# だけで動いた)。雪像 1 枚から次の 4 つを並べる:
#
#   (1) いまの物差し = `history["3600"]` の `errors_by_cause` の合計 ÷ `connects` の合計。
#       窓は履歴の行の範囲 (全期間と、`recent` の窓と重なる行の 2 通り)。
#   (2) 名前解決に成功した試行を分母にした率。`recent` の 1 行を 1 試行とし、
#       `reason == "error:dns"` を分母から外す (`recent` は接続の個票なので、ここでの
#       試行 = 接続の試行)。dns のミスは「試行あたり」と「名前解決に成功した試行あたり」の
#       両方を出す (T.23.1 は後者を dns の行に使う)。
#   (3) 接続元×宛先の連続した失敗を 1 件にまとめた率。間隔の閾は N = 60 / 300 秒の 2 通り。
#       **間で成功したら別の一続き**にする (「連続した失敗」の意味のまま)。分母は (2) と同じ。
#       接続の失敗 (refused + unreachable + timeout) と、dns を含む全失敗の 2 通りを出す。
#   (4) 宛先ごとの「試行・成功・DNS 失敗・timeout・refused」の上位 20 (試行の多い順)。
#
# **窓を必ず書く**: `recent` の `min(at)`〜`max(at)`、件数、`errors` の `count`/`kept`。
# `recent` は 2,000 件の環状なので、窓は 1 日より短いことが多い (10-20 の雪像では 7.9 時間)。
# 部が無い雪像は「判定できず」と書き、**0 とは書かない**。
#
# 失敗の種類は `reason` の `error:<cause>` から読む (`client_eof` / `server_eof` /
# `idle_timeout` などは、接続そのものは成功しているので成功に数える)。
# `recent` の `target` は名前解決に失敗した行だけホスト名、成功した行は IP のことがある
# (`sni` は別欄)。**同じ相手でも target の綴りが違えば別の宛先として数える** — 名前で
# 結び直すと SNI の無い接続で誤って混ざるため、ここでは綴りのままにする。
#
# 使い方: scripts/error-mix.py SNAP.json [SNAP.json ...] [--json]
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import json
import sys
from datetime import datetime, timezone

# `crates/metrics-types` の ERR_CAUSE_NAMES (errors_by_cause の並び)。T.25.0 で後ろに
# 2 つ (`dns` の内訳) が足されたが、ここは**8 つだけ**を読む: `total = sum(causes)` を
# 使っているので、内訳まで足すと同じ失敗を 2 回数えることになる
CAUSE_NAMES = ["dns", "refused", "unreachable", "timeout", "reset", "tls", "loop", "other"]
# 「接続の失敗」としてまとめる原因 (T.23.1 が phase22 の行から差し替える 3 つ)
CONNECT_CAUSES = ("refused", "unreachable", "timeout")
# 連続した失敗を 1 件にまとめる間隔の閾 (秒)。2 通りを並べる
MERGE_SECS = (60, 300)
# 宛先ごとの表に出す行数
TOP_TARGETS = 20


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def stamp(t):
    """epoch 秒を UTC の読める形に (`scripts/snapshot-summary.py` の `stamp()` と同じ綴り)。"""
    if t is None:
        return "—"
    return datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")


def num(v):
    if v is None:
        return "—"
    return f"{v:,}"


def rate(n, d):
    """割り算。分母が 0 のときは None (「0」と書かないため)。"""
    if not d:
        return None
    return n / d


def fmt_rate(r):
    return "—" if r is None else f"{r:.4f}"


def cause_of(row):
    """`reason` が `error:<cause>` なら cause、そうでなければ None (接続は成功)。"""
    reason = row.get("reason") or ""
    if reason.startswith("error:"):
        return reason[len("error:"):]
    return None


def section(obj, *names):
    """雪像の部をたどる。途中が無ければ None。"""
    for name in names:
        if not isinstance(obj, dict):
            return None
        obj = obj.get(name)
    return obj


def recent_rows(snap):
    """`/recent` の個票を時刻の順に。部が無ければ None (空の環状と区別する)。"""
    part = section(snap, "recent")
    if not isinstance(part, dict):
        return None
    rows = [r for r in (part.get("recent") or []) if isinstance(r, dict)]
    rows.sort(key=lambda r: (r.get("at") or 0, r.get("id") or 0))
    return rows


def errors_meta(snap):
    part = section(snap, "errors")
    if not isinstance(part, dict):
        return None
    return {"count": part.get("count"), "kept": part.get("kept"),
            "recorded": part.get("recorded")}


def history_window(snap):
    """`history["3600"]` の行と窓。部が無ければ None。"""
    part = section(snap, "history", "3600")
    if not isinstance(part, dict):
        return None
    keys = part.get("keys") or []
    rows = [dict(zip(keys, r)) for r in (part.get("samples") or []) if isinstance(r, list)]
    if not rows:
        return None
    return rows


def history_rate(rows, t0=None, t1=None):
    """`errors_by_cause` の合計 ÷ `connects` の合計 (行を `[t0, t1]` で絞れる)。"""
    picked = [r for r in rows
              if (t0 is None or r.get("t", 0) >= t0) and (t1 is None or r.get("t", 0) <= t1)]
    if not picked:
        return None
    connects = sum(r.get("connects") or 0 for r in picked)
    causes = [sum((r.get("errors_by_cause") or [])[i]
                  for r in picked if len(r.get("errors_by_cause") or []) > i)
              for i in range(len(CAUSE_NAMES))]
    total = sum(causes)
    return {"rows": len(picked), "from": picked[0].get("t"),
            "until": picked[-1].get("t"), "connects": connects,
            "errors": total, "causes": dict(zip(CAUSE_NAMES, causes)),
            "rate": rate(total, connects)}


def merge_failures(rows, causes, secs):
    """接続元×宛先ごとに、`causes` の失敗が `secs` 秒以内に続いたら 1 件にまとめる。

    間で成功したら (その接続元×宛先について) 一続きを切る。戻りは (生の件数, まとめた件数)。
    """
    per_key = {}
    for r in rows:
        cause = cause_of(r)
        key = (r.get("client") or "", r.get("target") or "")
        per_key.setdefault(key, []).append((r.get("at") or 0, cause))
    raw = groups = 0
    for events in per_key.values():
        events.sort(key=lambda e: e[0])
        last_fail = None
        for at, cause in events:
            if cause is None:
                last_fail = None  # 成功したら「連続した失敗」は切れる
                continue
            if cause not in causes:
                continue
            raw += 1
            if last_fail is None or at - last_fail > secs:
                groups += 1
            last_fail = at
    return raw, groups


def by_target(rows):
    """宛先 (target の綴り) ごとの試行・成功・原因別の数。試行の多い順。"""
    table = {}
    for r in rows:
        key = r.get("target") or "—"
        cell = table.setdefault(key, {"target": key, "attempts": 0, "success": 0,
                                     "dns": 0, "timeout": 0, "refused": 0, "other": 0})
        cell["attempts"] += 1
        cause = cause_of(r)
        if cause is None:
            cell["success"] += 1
        elif cause in cell:
            cell[cause] += 1
        else:
            cell["other"] += 1
    return sorted(table.values(), key=lambda c: (-c["attempts"], c["target"]))


def analyze(snap):
    rows = recent_rows(snap)
    if rows is not None:
        connects = [r for r in rows if (r.get("kind") or "connect") == "connect"]
    else:
        connects = []
    out = {"version": snap.get("version"), "taken_at": snap.get("taken_at"),
           "errors": errors_meta(snap), "recent": None, "history": None}

    if rows:
        ats = [r["at"] for r in connects if isinstance(r.get("at"), (int, float))]
        causes = {}
        for r in connects:
            cause = cause_of(r)
            if cause is not None:
                causes[cause] = causes.get(cause, 0) + 1
        dns = causes.get("dns", 0)
        attempts = len(connects)
        denom = attempts - dns
        conn_fail = sum(causes.get(c, 0) for c in CONNECT_CAUSES)
        failures = attempts - sum(1 for r in connects if cause_of(r) is None)
        info = {"from": min(ats) if ats else None, "until": max(ats) if ats else None,
                "hours": ((max(ats) - min(ats)) / 3600.0) if len(ats) > 1 else 0.0,
                "entries": len(rows or []), "attempts": attempts,
                "http_rows": len(rows) - len(connects),
                "success": attempts - failures, "failures": failures,
                "causes": causes, "dns": dns, "denom": denom,
                "conn_fail": conn_fail,
                "dns_per_attempt": rate(dns, attempts),
                "dns_per_resolved": rate(dns, denom),
                "conn_per_resolved": rate(conn_fail, denom),
                "all_per_resolved": rate(failures, denom),
                "merged": {}}
        for secs in MERGE_SECS:
            raw, groups = merge_failures(connects, CONNECT_CAUSES, secs)
            raw_all, groups_all = merge_failures(connects, tuple(CAUSE_NAMES), secs)
            info["merged"][secs] = {
                "conn": {"raw": raw, "groups": groups, "rate": rate(groups, denom)},
                "all": {"raw": raw_all, "groups": groups_all, "rate": rate(groups_all, attempts)},
            }
        out["recent"] = info
    elif rows is not None:
        out["recent"] = {"entries": 0}
    hist = history_window(snap)
    if hist:
        whole = history_rate(hist)
        hours = (whole["rows"]) if whole else None
        out["history"] = {"whole": whole, "hours": hours, "recent_slice": None}
        if out["recent"] and out["recent"].get("from") is not None:
            # `recent` の窓と重なる 1 時間の行 (行の時刻は 1 時間の始まり)
            out["history"]["recent_slice"] = history_rate(
                hist, out["recent"]["from"] - 3599, out["recent"]["until"])
    return out


def render(path, a, connects):
    lines = [f"## {path}", f"版 {a['version']}、雪像 {stamp(a['taken_at'])}。"]

    e = a["errors"]
    if e is None:
        lines.append("- `errors`: 判定できず (部が無い)。")
    else:
        lines.append(f"- `errors`: count {num(e['count'])} / kept {num(e['kept'])} "
                     f"(recorded {num(e['recorded'])})。")

    r = a["recent"]
    if r is None:
        lines.append("- `recent`: 判定できず (部が無い)。")
    elif not r.get("entries"):
        lines.append("- `recent`: 判定できず (0 件。空の環状)。")
    else:
        lines.append(f"- `recent`: 窓 {stamp(r['from'])}〜{stamp(r['until'])} "
                     f"({r['hours']:.2f} 時間)、{num(r['entries'])} 件。"
                     f"試行 {num(r['attempts'])} (connect"
                     + (f"。http の行 {num(r['http_rows'])} 件は外す" if r['http_rows'] else "")
                     + f")、成功 {num(r['success'])}、失敗 {num(r['failures'])}。")

    # (1) いまの物差し
    lines += ["", "### (1) いまの物差し = `errors_by_cause` 合計 ÷ `connects`"]
    h = a["history"]
    if h is None or h["whole"] is None:
        lines.append("判定できず (`history[\"3600\"]` が無い)。")
    else:
        w = h["whole"]
        lines.append(f"窓: `history[\"3600\"]` {num(w['rows'])} 行、{stamp(w['from'])}〜"
                     f"{stamp(w['until'])} ({num(w['rows'])} 時間)。")
        lines += ["", "| 窓 | connects | errors | 回/接続 | 内訳 |", "|---|---:|---:|---:|---|"]
        inner = " / ".join(f"{k} {num(v)}" for k, v in w["causes"].items())
        lines.append(f"| 全期間 | {num(w['connects'])} | {num(w['errors'])} | "
                     f"{fmt_rate(w['rate'])} | {inner} |")
        s = h.get("recent_slice")
        if s is not None:
            inner = " / ".join(f"{k} {num(v)}" for k, v in s["causes"].items())
            lines.append(f"| うち recent の窓と重なる {num(s['rows'])} 行 | {num(s['connects'])} | "
                         f"{num(s['errors'])} | {fmt_rate(s['rate'])} | {inner} |")

    # (2) 名前解決に成功した試行あたり
    lines += ["", "### (2) 名前解決に成功した試行を分母にした率"]
    if not r or not r.get("attempts"):
        lines.append("判定できず (`recent` の試行が無い)。")
    else:
        lines.append(f"窓: `recent` {stamp(r['from'])}〜{stamp(r['until'])} "
                     f"({r['hours']:.2f} 時間)、試行 {num(r['attempts'])}。")
        lines += ["", "| 試行 | dns 失敗 | 分母 (試行−dns) | dns ミス ÷ 試行 | dns ミス ÷ 分母 | "
                      "接続の失敗 (refused+unreachable+timeout) | 接続の失敗 ÷ 分母 | 全エラー ÷ 分母 |",
                  "|---:|---:|---:|---:|---:|---:|---:|---:|"]
        lines.append(f"| {num(r['attempts'])} | {num(r['dns'])} | {num(r['denom'])} | "
                     f"{fmt_rate(r['dns_per_attempt'])} | {fmt_rate(r['dns_per_resolved'])} | "
                     f"{num(r['conn_fail'])} | {fmt_rate(r['conn_per_resolved'])} | "
                     f"{fmt_rate(r['all_per_resolved'])} |")
        lines.append("")
        lines.append("原因別: " + " / ".join(f"{k} {num(v)}" for k, v in sorted(r["causes"].items())) + "。")

    # (3) 連続した失敗をまとめた率
    lines += ["", "### (3) 接続元×宛先の連続した失敗を 1 件にまとめた率"]
    if not r or not r.get("attempts"):
        lines.append("判定できず (`recent` の試行が無い)。")
    else:
        lines.append(f"窓: `recent` {stamp(r['from'])}〜{stamp(r['until'])} "
                     f"({r['hours']:.2f} 時間)。間で成功したら一続きを切る。")
        lines += ["", "| 失敗の範囲 | 分母 | 生の失敗 | N=60 秒 | 率 | N=300 秒 | 率 |",
                  "|---|---:|---:|---:|---:|---:|---:|"]
        m60 = r["merged"][MERGE_SECS[0]]
        m300 = r["merged"][MERGE_SECS[1]]
        lines.append(f"| 接続の失敗 (refused+unreachable+timeout) | 試行−dns {num(r['denom'])} | "
                     f"{num(m60['conn']['raw'])} | {num(m60['conn']['groups'])} | "
                     f"{fmt_rate(m60['conn']['rate'])} | {num(m300['conn']['groups'])} | "
                     f"{fmt_rate(m300['conn']['rate'])} |")
        lines.append(f"| dns を含む全失敗 | 試行 {num(r['attempts'])} | {num(m60['all']['raw'])} | "
                     f"{num(m60['all']['groups'])} | {fmt_rate(m60['all']['rate'])} | "
                     f"{num(m300['all']['groups'])} | {fmt_rate(m300['all']['rate'])} |")

    # (4) 宛先ごと
    lines += ["", f"### (4) 宛先ごと (上位 {TOP_TARGETS}、試行の多い順)"]
    if not r or not r.get("attempts"):
        lines.append("判定できず (`recent` の試行が無い)。")
    else:
        lines.append(f"窓: `recent` {stamp(r['from'])}〜{stamp(r['until'])} "
                     f"({r['hours']:.2f} 時間、{num(r['attempts'])} 試行)。")
        lines += ["", "| 宛先 | 試行 | 成功 | dns | timeout | refused | その他 |",
                  "|---|---:|---:|---:|---:|---:|---:|"]
        for c in by_target(connects)[:TOP_TARGETS]:
            lines.append(f"| `{c['target']}` | {num(c['attempts'])} | {num(c['success'])} | "
                         f"{num(c['dns'])} | {num(c['timeout'])} | {num(c['refused'])} | "
                         f"{num(c['other'])} |")
    return "\n".join(lines)


def main(argv=None):
    p = argparse.ArgumentParser(
        description="エラーを「使われ方」から切り離して数える (T.23.0)")
    p.add_argument("snapshots", nargs="+")
    p.add_argument("--json", action="store_true", help="結果を JSON で出す")
    args = p.parse_args(argv)
    results = {}
    rowsets = {}
    for path in args.snapshots:
        snap = load(path)
        results[path] = analyze(snap)
        rows = recent_rows(snap) or []
        rowsets[path] = [r for r in rows if (r.get("kind") or "connect") == "connect"]
    if args.json:
        json.dump(results, sys.stdout, ensure_ascii=False, indent=1)
        print()
    else:
        print("\n\n".join(render(k, v, rowsets[k]) for k, v in results.items()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
