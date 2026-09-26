#!/usr/bin/env python3
# 1 週間ぶんの雪像から「週次の要約」を **Markdown 1 枚**にする (TODO.md T14.40)。
#
# 入力は `status/` に溜まった雪像 (`*-snapshot.json`)。プロキシ自身が
# 1 日 1 回 (UTC 0 時) 書いている日次の `/snapshot` (T14.34) を
# `scripts/collect-deployed.sh --from-server` で取り寄せた `<日付>T000000Z-snapshot.json` も
# 同じ形なので、そのまま混ぜてよい。`/daily` (T14.20) の JSON だけでも出る。
# **7 日ぶん無ければあるぶんで**出す (何枚・何日ぶんあったかは頭に書く)。
#
# 使い方:
#   scripts/weekly-report.py status/                    # 置き場ごと渡す
#   scripts/weekly-report.py status/*-snapshot.json --days 7 -o week.md
#   scripts/weekly-report.py status/ --out json          # 表の元の辞書をそのまま
#   curl -s 'http://PROXY/daily?n=7' > daily.json && scripts/weekly-report.py daily.json
#
# 出す表 8 つ:
#   1. 要求数 (日別)              5. 山 (`/bursts` があれば枚数と最大、無ければ `active_max`)
#   2. CONNECT 確立 p50 / p95     6. 接続元の出入り (`clients[]` の初回 / 最終)
#   3. 名前解決のミス率           7. 遅かったホスト上位 N (`avg_ms` × 要求数)
#   4. エラー (原因別)            8. 新しく見たホスト (前の雪像に無かった名前)
#
# **数字の求め方は `scripts/snapshot-diff.py` (T14.17) と同じ**: `/history?res=3600` を
# **UTC の日で切って**同じ `aggregate()` に通すので、同じ期間を切れば同じ値が出る
# (分位点の補間は `crates/metrics/src/history.rs` の `Window::quantile_ms` と同じ)。
# 平常時 (1 時間 300 本未満の標本) とバースト込みを分けるのも同じ切り方。
#
# 出力は T14.99 (締めの文書) と、次の Phase の T15.0 の入力になる。
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import glob
import importlib.util
import json
import os
import sys
from datetime import datetime, timedelta, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from proxydata import CAUSE_NAMES, causes_text, fmt_bytes, host_name, row_of  # noqa: E402


def _load(name, filename):
    """名前に `-` があるモジュールを読む (`import snapshot-diff` とは書けない)。"""
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# 読み方 (窓の切り方・`aggregate()`・`/hosts` `/clients` の取り出し) は T14.17 と共有する。
# **ここからは読むだけで、snapshot-diff.py は変えない。**
sd = _load("snapshot_diff", "snapshot-diff.py")

DAY_SECS = 86400
DEFAULT_DAYS = 7
DEFAULT_TOP = 10
# 表からあふれた分の置き場 (`crates/metrics/src/metrics.rs` の MAX_HOSTS 超え)。ホストではない。
OVERFLOW = "other"


# ---------------------------------------------------------------- 日付の小物

def day_of(t):
    """epoch 秒 -> `YYYY-MM-DD` (UTC)。"""
    return datetime.fromtimestamp(int(t), timezone.utc).strftime("%Y-%m-%d")


def day_start(day):
    """`YYYY-MM-DD` -> その日の 00:00 UTC の epoch 秒。"""
    return int(datetime.strptime(day, "%Y-%m-%d").replace(tzinfo=timezone.utc).timestamp())


def day_seq(last, days):
    """`last` で終わる `days` 日の並び (古い順)。"""
    end = datetime.strptime(last, "%Y-%m-%d").replace(tzinfo=timezone.utc)
    return [(end - timedelta(days=i)).strftime("%Y-%m-%d") for i in range(days - 1, -1, -1)]


# ---------------------------------------------------------------- 読み込み

def expand(paths):
    """ディレクトリなら `*-snapshot.json` を集める (無ければ `*.json`)。"""
    out = []
    for p in paths:
        if os.path.isdir(p):
            found = sorted(glob.glob(os.path.join(p, "*-snapshot.json")))
            out.extend(found or sorted(glob.glob(os.path.join(p, "*.json"))))
        else:
            out.extend(sorted(glob.glob(p)) or [p])
    return out


def daily_lines(raw):
    """`/daily` の応答 (`{"days":[...]}`) なら 1 日 1 行の並びを返す (違えば None)。"""
    if isinstance(raw, dict) and isinstance(raw.get("days"), list):
        return [x for x in raw["days"] if isinstance(x, dict) and x.get("day")]
    return None


def daily_from_jsonl(path):
    """`$HOME/.rust-http-proxy.daily.jsonl` (プロキシが書く生のファイル) も読む。

    1 行 1 つの JSON なので、壊れた行 (手で触られた行) は読み飛ばす (口と同じ作法)。
    """
    rows = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                d = json.loads(line)
            except ValueError:
                continue
            if isinstance(d, dict) and d.get("day"):
                rows.append(d)
    return rows


def load_inputs(paths):
    """雪像と `/daily` の行を読み分ける。読めなかったものは理由つきで返す (止めない)。"""
    snaps, daily, skipped = [], {}, []
    for p in paths:
        try:
            with open(p, encoding="utf-8") as f:
                raw = json.load(f)
        except OSError as e:
            skipped.append((p, str(e)))
            continue
        except ValueError:
            rows = []
            try:
                rows = daily_from_jsonl(p)
            except OSError as e:
                skipped.append((p, str(e)))
                continue
            if not rows:
                skipped.append((p, "JSON でも /daily の行でもない"))
                continue
            for line in rows:
                daily[line["day"]] = line
            continue
        lines = daily_lines(raw)
        if lines is not None:
            for line in lines:
                daily[line["day"]] = line
            continue
        try:
            snaps.append(sd.load_source(p, False))
        except SystemExit as e:
            skipped.append((p, str(e)))
    snaps.sort(key=lambda s: (s["taken_at"] or 0, s["label"]))
    return snaps, daily, skipped


# ---------------------------------------------------------------- 履歴を日で切る

def pick_res(snaps):
    """使う解像度 (粗いほど長く残っているので 3600 が第一希望。`snapshot-diff.py` と同じ)。"""
    for res in ("3600", "60", "5"):
        for snap in reversed(snaps):
            h = (snap.get("history") or {}).get(res)
            if h and h.get("samples"):
                return res
    return None


def merged_history(snaps, res):
    """N 枚の `/history?res=` を時刻で重ねる (同じ時刻は**新しい雪像の値**を採る)。

    `snapshot-diff.py` の `merged_history` を 2 枚から N 枚に広げただけ
    (雪像の最後の窓は取得の途中までしか埋まっていないので、後の雪像が勝つ)。
    """
    rows, bounds, causes, interval = {}, [], list(CAUSE_NAMES), None
    for snap in snaps:
        h = (snap.get("history") or {}).get(res) or {}
        if not h:
            continue
        bounds = h.get("bounds_ms") or bounds
        causes = h.get("causes") or causes
        interval = h.get("interval_secs") or interval
        for row in sd.norm_history(h):
            rows[row["t"]] = row
    return [rows[t] for t in sorted(rows)], bounds, causes, (interval or 0)


def from_daily(line):
    """`/daily` の 1 行を `aggregate()` と同じ鍵に並べ替える (無い欄は None)。"""
    out = dict.fromkeys(
        ("connect_ms_sum", "connect_ms_max", "connect_buckets", "connect_avg",
         "forwards", "forward_avg", "forward_p50", "forward_p95",
         "dns_ms_sum", "dns_ms_per_connect", "causes", "last_t"))
    out.update({
        "samples": line.get("samples") or 0, "burst_samples": 0, "first_t": line.get("t"),
        "connects": line.get("connects") or 0,
        "connect_p50": line.get("connect_p50_ms"), "connect_p95": line.get("connect_p95_ms"),
        "dns_misses": line.get("dns_misses") or 0,
        "dns_per_connect": line.get("dns_per_connect"), "ms_per_miss": line.get("dns_miss_ms"),
        "errors": line.get("errors") or 0, "active_max": line.get("active_max") or 0,
        "requests": line.get("requests") or 0,
    })
    return out


def day_stats(rows, bounds, limit, days, daily):
    """1 日ぶんの標本を `snapshot-diff.py` と同じ `aggregate()` に通す。

    `normal` は**バーストの窓を外した**もの (T14.0 の切り方)、`all` は全部。
    `/history` に無い日は `/daily` の 1 行で埋める (出どころを `src` に残す)。
    """
    out = {}
    for d in days:
        lo = day_start(d)
        rs = [r for r in rows if lo <= r["t"] < lo + DAY_SECS]
        if rs:
            out[d] = {
                "day": d, "src": "history", "samples": len(rs),
                "normal": sd.aggregate(rs, bounds, limit),
                "all": sd.aggregate(rs, bounds, None),
                "requests": sum(r["requests"] or 0 for r in rs),
                "bytes": sum(r["bytes"] or 0 for r in rs),
                "secs": None,
            }
        elif d in daily:
            agg = from_daily(daily[d])
            out[d] = {
                "day": d, "src": "daily", "samples": agg["samples"],
                "normal": agg, "all": agg,
                "requests": agg["requests"], "bytes": daily[d].get("bytes"),
                "secs": daily[d].get("secs"), "bursts": daily[d].get("bursts"),
            }
    return out


# ---------------------------------------------------------------- (5) 山

def burst_shots(snaps):
    """`/bursts` の写真を重ねる (同じ雪像が何枚あっても 1 枚は 1 回)。"""
    seen = {}
    for snap in snaps:
        for b in sd.part(snap, "bursts").get("bursts") or []:
            seen[(b.get("at"), b.get("seq"))] = b
    return sorted(seen.values(), key=lambda b: (b.get("at") or 0, b.get("seq") or 0))


# ---------------------------------------------------------------- (6) 接続元

def client_window(snaps):
    """接続元ごとに「いつ初めて / 最後に見たか」と週の増分を作る。

    `/clients` (T14.7) には `first_seen` / `last_seen` があるが、`/status` の `clients[]`
    (古い版) には `last_seen` しか無い。そのときは**初めて出た雪像の日**で代える。
    """
    out = {}
    for snap in snaps:
        t = snap["taken_at"]
        for c in sd.client_rows(snap):
            key = c.get("client")
            if not key:
                continue
            e = out.get(key)
            if e is None:
                e = out[key] = {"client": key, "first_seen": c.get("first_seen"),
                                "last_seen": c.get("last_seen"), "first_snap": t, "last_snap": t,
                                "snaps": 0, "first_row": c, "last_row": c, "agent": None}
            e["snaps"] += 1
            e["last_snap"] = t
            e["last_row"] = c
            for name, pick in (("first_seen", min), ("last_seen", max)):
                v = c.get(name)
                if v:
                    e[name] = v if e[name] is None else pick(e[name], v)
            e["agent"] = c.get("agent") or e["agent"]
    for e in out.values():
        # 2 枚以上に出ていれば週の増分、1 枚しか無ければ `.rrd` の通算のまま
        multi = e["snaps"] > 1 and e["first_row"] is not e["last_row"]
        e["delta"] = (row_of(e["client"], e["first_row"], e["last_row"]) if multi
                      else row_of(e["client"], e["last_row"], None))
        e["windowed"] = multi
        e["arrived"] = day_of(e["first_seen"]) if e["first_seen"] else day_of(e["first_snap"])
        e["arrived_exact"] = e["first_seen"] is not None
        e["left"] = day_of(e["last_seen"]) if e["last_seen"] else day_of(e["last_snap"])
        e["left_exact"] = e["last_seen"] is not None
    return out


def client_moves(clients, days, first_snap_day):
    """日ごとの「新しく来た」「来なくなった」。

    - **新しく来た**: `first_seen` がその日。`first_seen` が無い版では「初めて出た雪像の日」を
      使うが、**いちばん古い雪像の日は数えない** (その日は全部が初めてに見えてしまう)。
    - **来なくなった**: `last_seen` がその日で、**窓の最後の日ではない** (最後の日はまだ来る)。
    """
    moves = {d: {"in": [], "out": []} for d in days}
    last_day = days[-1] if days else None
    for e in clients.values():
        if e["arrived"] in moves and (e["arrived_exact"] or e["arrived"] != first_snap_day):
            moves[e["arrived"]]["in"].append(e)
        if e["left"] in moves and e["left"] != last_day:
            moves[e["left"]]["out"].append(e)
    for d in moves.values():
        for side in ("in", "out"):
            d[side].sort(key=lambda e: (-(e["delta"]["requests"] or 0), e["client"]))
    return moves


# ---------------------------------------------------------------- (7)(8) ホスト

def host_delta(snaps):
    """週の初めと終わりの `/hosts` の差 (`.rrd` の通算なので引き算する)。

    雪像が 1 枚しか無ければ引き算できないので**通算のまま**返す (`windowed` が False)。
    """
    if not snaps:
        return {"rows": [], "windowed": False, "truncated": False, "source": None,
                "rrd_reset": None}
    newest, cut_b, src_b = sd.host_rows(snaps[-1])
    multi = len(snaps) > 1
    older, cut_a = {}, False
    if multi:
        first, cut_a, _src_a = sd.host_rows(snaps[0])
        older = {h["host"]: h for h in first}
    rows = [row_of(h["host"], older.get(h["host"]), h) if multi else row_of(h["host"], h, None)
            for h in newest]
    return {"rows": rows, "windowed": multi, "truncated": cut_a or cut_b, "source": src_b,
            "rrd_reset": sd.rrd_reset(snaps[0], snaps[-1], rows) if multi else None}


def slow_hosts(rows, top):
    """遅かった順 = **`avg_ms` × 要求数** (その週に待たせた合計 ms) の上位。"""
    pool = []
    for r in rows:
        if r["name"] == OVERFLOW or r["requests"] <= 0 or r["avg_ms"] is None:
            continue
        pool.append(dict(r, total_ms=r["avg_ms"] * r["requests"]))
    pool.sort(key=lambda r: (-r["total_ms"], r["name"]))
    return pool[:top]


def new_hosts(snaps, days):
    """前の雪像に無かった名前を、**その雪像が写した日**に付ける。

    日は `taken_at` の **1 秒前**の日で見る。T14.34 の日次の雪像は「終わった日」を
    翌日 00:00 UTC の直後に写すので、そのままでは 1 日ずれる。窓より後の雪像
    (窓の最後の日より新しいもの) は最後の日にまとめる。
    いちばん古い雪像は比べる相手が無いので数えない (`.rrd` の通算なので全部が初めてに見える)。
    """
    per_day = {d: set() for d in days}
    seen, pairs, outside = None, 0, 0
    for snap in snaps:
        names = {h["host"] for h in sd.host_rows(snap)[0] if h.get("host") != OVERFLOW}
        if seen is None:
            seen = names
            continue
        pairs += 1
        fresh = names - seen
        seen |= names
        if not fresh:
            continue
        d = day_of((snap["taken_at"] or 0) - 1)
        place = max((x for x in days if x <= d), default=None)
        if place is None:
            outside += 1
            continue
        per_day[place] |= fresh
    return {"per_day": {d: sorted(v) for d, v in per_day.items()}, "pairs": pairs,
            "outside": outside}


# ---------------------------------------------------------------- 組み立て

def week_total(stats, window, bounds, limit):
    """週の 1 行。`/history` があればその標本を `aggregate()` に通し、無ければ足せる欄だけ足す。

    要求数とバイトは**日ごとの値の和**なので、`/history` の日と `/daily` の日が混ざっても合う。
    分位点は日ごとの値からは作れないので、`/daily` しか無い日は「出せない」まま
    (`/history` の日が 1 つでもあれば、その日だけを足した分位点になる — `src_days` に内訳)。
    """
    week = {"normal": sd.aggregate(window, bounds, limit),
            "all": sd.aggregate(window, bounds, None),
            "requests": sum(s["requests"] or 0 for s in stats.values()),
            "bytes": sum(s["bytes"] or 0 for s in stats.values()),
            "samples": len(window),
            "src_days": {k: sum(1 for s in stats.values() if s["src"] == k)
                         for k in ("history", "daily")}}
    if window:
        return week
    week["samples"] = sum(s["samples"] or 0 for s in stats.values())
    for name in ("normal", "all"):
        agg = week[name]
        for key in ("connects", "dns_misses", "errors", "forwards"):
            agg[key] = sum(s[name].get(key) or 0 for s in stats.values())
        agg["active_max"] = max((s[name].get("active_max") or 0 for s in stats.values()),
                                default=0)
        agg["samples"] = week["samples"]
        agg["dns_per_connect"] = (agg["dns_misses"] / agg["connects"]) if agg["connects"] else None
        # ミス 1 回の ms は「ミスの数で重みを付けた平均」なら日ごとの値から作れる
        ms_sum = sum((s[name].get("ms_per_miss") or 0) * (s[name].get("dns_misses") or 0)
                     for s in stats.values())
        agg["ms_per_miss"] = (ms_sum / agg["dns_misses"]) if agg["dns_misses"] else None
    return week


def build(snaps, daily, args):
    res = pick_res(snaps)
    if res:
        rows, bounds, causes, interval = merged_history(snaps, res)
    else:
        rows, bounds, causes, interval = [], [], list(CAUSE_NAMES), 0
    limit = max(1, round(args.burst * interval / 3600.0)) if interval else args.burst
    known = sorted({day_of(r["t"]) for r in rows} | set(daily))
    if not known:
        raise SystemExit("読めた雪像にも /daily にも日が 1 つも無い")
    wanted = day_seq(known[-1], args.days) if args.days > 0 else known
    days = [d for d in wanted if d in known]
    stats = day_stats(rows, bounds, limit, days, daily)
    lo, hi = day_start(days[0]), day_start(days[-1]) + DAY_SECS
    window = [r for r in rows if lo <= r["t"] < hi]
    clients = client_window(snaps)
    return {
        "days": days, "missing": [d for d in wanted if d not in known],
        "asked": args.days, "res": res, "interval_secs": interval, "limit": limit,
        "bounds": bounds, "causes": causes, "stats": stats,
        "snaps": [{"label": s["label"], "taken_at": s["taken_at"], "version": s["version"],
                   "uptime_secs": s["uptime_secs"], "parts": s.get("parts") or [],
                   "day": day_of(s["taken_at"]) if s["taken_at"] else "?"} for s in snaps],
        "daily_days": sorted(daily),
        "week": week_total(stats, window, bounds, limit),
        "shots": [b for b in burst_shots(snaps)
                  if b.get("at") and lo <= b["at"] < hi],
        "has_bursts": any("bursts" in (s.get("parts") or []) for s in snaps),
        "clients": clients, "moves": client_moves(clients, days,
                                                  day_of(snaps[0]["taken_at"]) if snaps else None),
        "has_clients_part": any("clients" in (s.get("parts") or []) for s in snaps),
        "hosts": host_delta(snaps), "new_hosts": new_hosts(snaps, days) if snaps else None,
    }


# ---------------------------------------------------------------- Markdown

def used_causes(d):
    """その週に 1 回でも出た原因だけを列にする (8 列は広すぎる)。"""
    hit = set()
    for s in d["stats"].values():
        for i, v in enumerate(s["all"].get("causes") or []):
            if v:
                hit.add(i)
    for i, v in enumerate(d["week"]["all"].get("causes") or []):
        if v:
            hit.add(i)
    return sorted(hit)


def render(d, top):
    out = []
    p = out.append
    days, stats, week = d["days"], d["stats"], d["week"]
    n, ms, ratio = sd.n, sd.ms, sd.ratio

    # --- 頭 (件数と何日ぶんか)
    p(f"# rust-http-proxy — 週次の要約 ({days[0]} 〜 {days[-1]})")
    p("")
    p(f"- **雪像 {len(d['snaps'])} 枚、{len(days)} 日ぶん** "
      f"(求めたのは {d['asked'] if d['asked'] > 0 else len(days)} 日"
      + (f"、足りない {len(d['missing'])} 日: {', '.join(d['missing'])}" if d["missing"] else "")
      + ")")
    if d["res"]:
        p(f"- 日別の数字は `/history?res={d['res']}` を UTC の日で切ったもの "
          f"(**1 標本 {d['limit']} 本以上の窓 = バーストを外した**値を「平常時」と呼ぶ。"
          "`snapshot-diff.py` (T14.17) と同じ切り方・同じ `aggregate()`)")
        if d["limit"] <= 1:
            p("- **注意**: 解像度が細かいので平常時の閾が 1 標本 1 本になっている "
              "(1 本でも通れば「バースト」として外れる)。`res=3600` のある雪像を渡すか "
              "`--burst` を上げる")
    else:
        p("- `/history` がどの入力にも無いので、日別は `/daily` (T14.20) の 1 行から出している")
    if d["daily_days"]:
        p(f"- `/daily` の行 {len(d['daily_days'])} 日ぶん ({d['daily_days'][0]} 〜 "
          f"{d['daily_days'][-1]}) も読んだ (`/history` に無い日を埋める)")
    for s in d["snaps"]:
        p(f"  - `{os.path.basename(s['label'])}` — {sd.stamp(s['taken_at'])}、版 "
          f"`{s['version']}`、`uptime_secs` {n(s['uptime_secs'])}、{len(s['parts'])} 部")
    p("")
    p(f"**週ぜんたい (平常時)**: 要求 {n(week['requests'])}、転送 {fmt_bytes(week['bytes'])}、"
      f"CONNECT 確立 avg {ms(week['normal']['connect_avg'])} / "
      f"**p50 {ms(week['normal']['connect_p50'])}** / p95 {ms(week['normal']['connect_p95'])} ms "
      f"({n(week['normal']['connects'])} 本)、名前解決 "
      f"**{ratio(week['normal']['dns_per_connect'])} 回/接続** "
      f"(ミス 1 回 {ms(week['normal']['ms_per_miss'])} ms)、エラー {n(week['all']['errors'])}、"
      f"標本 {n(week['normal']['samples'])} / {n(week['samples'])}")
    p("")

    # --- 1
    p("## 1. 要求数 (日別)")
    p("")
    p("**バーストの窓も入れた**その日の全部 (表 2 と 3 は平常時だけなので本数が合わない)。")
    p("")
    p("| 日 | 要求 | 転送 | CONNECT 本 | forward 本 | 標本 | 出どころ |")
    p("|---|---:|---:|---:|---:|---:|---|")
    for day in days:
        s = stats[day]
        a = s["all"]
        src = "/history?res=" + d["res"] if s["src"] == "history" else "/daily"
        p(f"| {day} | {n(s['requests'])} "
          f"| {fmt_bytes(s['bytes']) if s['bytes'] is not None else '—'} "
          f"| {n(a['connects'])} | {n(a['forwards'])} | {n(s['samples'])} | `{src}` |")
    p(f"| **週** | **{n(week['requests'])}** | **{fmt_bytes(week['bytes'])}** "
      f"| **{n(week['all']['connects'])}** | **{n(week['all']['forwards'])}** "
      f"| **{n(week['samples'])}** | |")
    p("")

    # --- 2
    p("## 2. CONNECT 確立 p50 / p95 (日別)")
    p("")
    p("平常時 (バーストの窓を外した標本) の値。`/history` の対数バケツから "
      "`crates/metrics/src/history.rs` の `quantile_ms` と同じ補間で出す。")
    p("")
    p("| 日 | avg ms | p50 ms | p95 ms | 最大 ms | 本数 | forward p50 / p95 ms | 外した窓 |")
    p("|---|---:|---:|---:|---:|---:|---:|---:|")
    for day in days:
        a = stats[day]["normal"]
        p(f"| {day} | {ms(a['connect_avg'])} | **{ms(a['connect_p50'])}** | {ms(a['connect_p95'])} "
          f"| {n(a['connect_ms_max'])} | {n(a['connects'])} "
          f"| {ms(a['forward_p50'])} / {ms(a['forward_p95'])} | {n(a['burst_samples'])} |")
    w = week["normal"]
    p(f"| **週** | **{ms(w['connect_avg'])}** | **{ms(w['connect_p50'])}** "
      f"| **{ms(w['connect_p95'])}** | **{n(w['connect_ms_max'])}** | **{n(w['connects'])}** "
      f"| **{ms(w['forward_p50'])} / {ms(w['forward_p95'])}** | **{n(w['burst_samples'])}** |")
    p("")

    # --- 3
    p("## 3. 名前解決のミス率 (日別)")
    p("")
    p("| 日 | ミス/接続 | ミス | ミス 1 回 ms | 接続 1 本あたり ms |")
    p("|---|---:|---:|---:|---:|")
    for day in days:
        a = stats[day]["normal"]
        p(f"| {day} | **{ratio(a['dns_per_connect'])}** | {n(a['dns_misses'])} "
          f"| {ms(a['ms_per_miss'])} | {ms(a['dns_ms_per_connect'])} |")
    p(f"| **週** | **{ratio(w['dns_per_connect'])}** | **{n(w['dns_misses'])}** "
      f"| **{ms(w['ms_per_miss'])}** | **{ms(w['dns_ms_per_connect'])}** |")
    p("")

    # --- 4
    p("## 4. エラー (原因別)")
    p("")
    cols = used_causes(d)
    if not cols:
        if not week["all"]["errors"]:
            p(f"この {len(days)} 日でエラーは **0** 件。")
        else:
            p(f"この {len(days)} 日でエラーは **{n(week['all']['errors'])}** 件。"
              "**原因別の内訳はこの入力に無い** (`/daily` の 1 行には合計しか入っていない。"
              "内訳が要るなら雪像か `/history` を渡す)。")
    else:
        head = " | ".join(CAUSE_NAMES[i] for i in cols)
        p(f"| 日 | 合計 | {head} |")
        p("|---|---:|" + "---:|" * len(cols))
        for day in days:
            a = stats[day]["all"]
            cs = a.get("causes")
            cells = " | ".join(n(cs[i]) if cs else "—" for i in cols)
            p(f"| {day} | {n(a['errors'])} | {cells} |")
        cs = week["all"].get("causes")
        cells = " | ".join(f"**{n(cs[i])}**" if cs else "—" for i in cols)
        p(f"| **週** | **{n(week['all']['errors'])}** | {cells} |")
        p("")
        p(f"原因の並びは `/status` の `errors_by_cause` と同じ ({causes_text(week['all']['causes'])})。")
    p("")

    # --- 5
    p("## 5. 山 (バースト)")
    p("")
    if d["has_bursts"]:
        p(f"`/bursts` の写真 (T14.6) がこの週に **{len(d['shots'])} 枚**。")
        p("")
        p("| 日 | 写真 | 最大同時 | いちばん多かった時刻 | 接続元 |")
        p("|---|---:|---:|---|---|")
        for day in days:
            lo = day_start(day)
            shots = [b for b in d["shots"] if lo <= b["at"] < lo + DAY_SECS]
            if not shots:
                p(f"| {day} | 0 | — | — | — |")
                continue
            worst = max(shots, key=lambda b: b.get("active") or 0)
            who = ", ".join(f"{c.get('client')} ({c.get('conns')})"
                            for c in (worst.get("clients") or [])[:3]) or "—"
            p(f"| {day} | {n(len(shots))} | {n(worst.get('active'))} "
              f"| {sd.stamp(worst.get('at'))} | {who} |")
    else:
        p("`/bursts` (T14.6) の写真がどの入力にも無いので、その日の**同時接続の最大** "
          "(`active_max`) と「山」で代える。山は `/history` なら**外したバーストの窓の数** "
          f"(1 標本 {d['limit']} 本以上)、`/daily` なら**その日に撮れた写真の枚数** "
          "(`bursts`)。")
        p("")
        p("| 日 | 最大同時 | 山 | 出どころ |")
        p("|---|---:|---:|---|")
        total = 0
        for day in days:
            row = stats[day]
            if row["src"] == "daily":
                hill, how = row.get("bursts"), "`/daily` の `bursts`"
            else:
                hill, how = row["normal"]["burst_samples"], "バーストの窓"
            total += hill or 0
            p(f"| {day} | {n(row['all']['active_max'])} | {n(hill)} | {how} |")
        p(f"| **週** | **{n(week['all']['active_max'])}** | **{n(total)}** | |")
    p("")

    # --- 6
    p("## 6. 接続元の出入り")
    p("")
    if not d["clients"]:
        p("`/clients` (T14.7) も `/status` の `clients[]` もこの入力に無い。")
    else:
        if not d["has_clients_part"]:
            p("`/clients` (T14.7) がこの雪像に無いので `/status` の `clients[]` で代える "
              "(**`first_seen` が無い版**なので「新しく来た」は初めて出た雪像の日で見る)。")
            p("")
        p("| 日 | 新しく来た | 来なくなった | 接続元 |")
        p("|---|---:|---:|---|")
        for day in days:
            mv = d["moves"][day]
            who = []
            for e in mv["in"][:top]:
                who.append(f"+ `{e['client']}` ({n(e['delta']['requests'])})")
            for e in mv["out"][:top]:
                who.append(f"− `{e['client']}` ({n(e['delta']['requests'])})")
            p(f"| {day} | {len(mv['in'])} | {len(mv['out'])} | {', '.join(who) or '—'} |")
        p("")
        p(f"この週に見えた接続元は **{len(d['clients'])} 件**"
          + ("" if any(e["windowed"] for e in d["clients"].values())
             else " (2 枚で挟めた接続元が無いので要求数は `.rrd` の通算)") + ":")
        p("")
        p("| 接続元 | 要求 | 転送 | 初めて | 最後 | 平均 ms |")
        p("|---|---:|---:|---|---|---:|")
        order = sorted(d["clients"].values(), key=lambda e: (-(e["delta"]["requests"] or 0),
                                                            e["client"]))
        for e in order[:top]:
            first = sd.stamp(e["first_seen"]) if e["first_seen"] else f"({e['arrived']} 以前)"
            p(f"| `{e['client']}` | {n(e['delta']['requests'])} "
              f"| {fmt_bytes(e['delta']['bytes'])} | {first} "
              f"| {sd.stamp(e['last_seen']) if e['last_seen'] else '—'} "
              f"| {ms(e['delta']['avg_ms'])} |")
        if len(order) > top:
            p(f"| (ほか {len(order) - top} 件) | | | | | |")
    p("")

    # --- 7
    h = d["hosts"]
    p(f"## 7. 遅かったホスト 上位 {top} (`avg_ms` × 要求数)")
    p("")
    if not h["rows"]:
        p("`/hosts` がこの入力に無い。")
    else:
        if h["windowed"]:
            p(f"いちばん古い雪像といちばん新しい雪像の `{h['source']}` の差 "
              "(`.rrd` の通算なので引き算する)。")
        else:
            p(f"雪像が 1 枚なので引き算できない。`{h['source']}` の**通算** "
              "(状態ファイルに残っている起動前からの値も混ざる) をそのまま並べる。")
        if h["rrd_reset"]:
            p("")
            p(f"**`.rrd` が作り直されている** (`restored_since` "
              f"{sd.stamp(h['rrd_reset']['restored_since'])}、要求数が負になったホスト "
              f"{h['rrd_reset']['dropped_hosts']} 件) ので、この差は引き算できていない。")
        if h["truncated"]:
            p("")
            p("`/hosts` が 256 KiB で切れているので、裾のホストは落ちている。")
        p("")
        p("| ホスト | 要求 | 合計 ms | avg ms | p50 ms | p95 ms | 最大 ms | エラー |")
        p("|---|---:|---:|---:|---:|---:|---:|---|")
        for r in slow_hosts(h["rows"], top):
            p(f"| `{r['name']}`{'' if r['connect'] else ' (forward)'} | {n(r['requests'])} "
              f"| {n(round(r['total_ms']))} | {ms(r['avg_ms'])} | {ms(r['p50_ms'])} "
              f"| {ms(r['p95_ms'])} | {n(r['max_ms'])} | {causes_text(r['errors_by_cause'])} |")
    p("")

    # --- 8
    p("## 8. 新しく見たホスト (前の雪像に無かった名前)")
    p("")
    nh = d["new_hosts"]
    if not nh or not nh["pairs"]:
        p(f"雪像が {len(d['snaps'])} 枚しか無いので「前の雪像」が無い (2 枚目から出る)。")
    else:
        p(f"雪像 {nh['pairs'] + 1} 枚を古い順に並べて、隣どうしで比べた "
          "(`other` は表からあふれた分の置き場なので数えない)。"
          + (f" 窓の外の雪像 {nh['outside']} 枚は数えていない。" if nh["outside"] else ""))
        p("")
        p("| 日 | 件数 | 名前 |")
        p("|---|---:|---|")
        for day in days:
            fresh = nh["per_day"].get(day) or []
            shown = ", ".join(f"`{host_name(x)}`" for x in fresh[:top])
            if len(fresh) > top:
                shown += f" (ほか {len(fresh) - top} 件)"
            p(f"| {day} | {len(fresh)} | {shown or '—'} |")
    p("")
    return "\n".join(out) + "\n"


# ---------------------------------------------------------------- 入口

def parser():
    p = argparse.ArgumentParser(
        description="1 週間ぶんの雪像から週次の要約を Markdown 1 枚にする (T14.40)")
    p.add_argument("inputs", nargs="+", metavar="DIR|FILE",
                   help="雪像の置き場 (`status/`)、`*-snapshot.json`、"
                        "`/daily` の JSON のどれでも。混ぜてよい")
    p.add_argument("--days", type=int, default=DEFAULT_DAYS, metavar="N",
                   help=f"いちばん新しい日から遡る日数 (既定 {DEFAULT_DAYS}、0 で全部)")
    p.add_argument("--top", type=int, default=DEFAULT_TOP, metavar="N",
                   help=f"ホスト・接続元の一覧に出す行数 (既定 {DEFAULT_TOP})")
    p.add_argument("--burst", type=int, default=sd.BURST_PER_HOUR, metavar="N",
                   help=f"平常時の閾 (1 時間の本数。既定 {sd.BURST_PER_HOUR})")
    p.add_argument("--out", choices=["md", "json"], default="md",
                   help="出力の形 (既定 md。json は build() の辞書をそのまま出す)")
    p.add_argument("-o", "--output", metavar="FILE", help="書き出し先 (既定は標準出力)")
    return p


def main(argv=None):
    args = parser().parse_args(argv)
    paths = expand(args.inputs)
    if not paths:
        raise SystemExit("入力が 1 つも見つからない")
    snaps, daily, skipped = load_inputs(paths)
    if not snaps and not daily:
        raise SystemExit("雪像も /daily も読めなかった: "
                         + "; ".join(f"{p}: {why}" for p, why in skipped))
    d = build(snaps, daily, args)
    if args.out == "json":
        d["skipped"] = [{"path": p, "why": why} for p, why in skipped]
        md = json.dumps(d, ensure_ascii=False, indent=1, default=str) + "\n"
    else:
        md = render(d, args.top)
        if skipped:
            md += "\n" + "\n".join(f"- **読めなかった**: `{p}` ({why})" for p, why in skipped) + "\n"
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(md)
    else:
        sys.stdout.write(md)
    return 0


if __name__ == "__main__":
    sys.exit(main())
