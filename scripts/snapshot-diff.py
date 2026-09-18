#!/usr/bin/env python3
# 2 枚の `/snapshot` から「**何が変わったか**」を全部出す (TASKS.md T14.17)。
#
# T14.0 の分析は `/status` の差分・`/history` の再起動時刻での切り分け・`/dns` の個票・
# `/hosts` の差分を手作業で組み合わせたもので、Python を 5 回書いた。それを 1 コマンドにする。
# 出力は Markdown なので `TASKS.md` にそのまま貼れる。
#
# 使い方:
#   scripts/snapshot-diff.py A.json B.json [--aaaa FILE | --no-dns] [--criteria phase14]
#                            [--out md|json] [--top N] [--burst N] [--major-hosts a,b,c]
#                            [--group domain]
#     scripts/snapshot-diff.py ~/rust-http-proxy-status/2026-09-1*-snapshot.json
#     scripts/snapshot-diff.py a.json b.json --criteria phase14 >> TASKS.md
#
#   **`/snapshot` より前の形** (`/status` と `/history` を 1 本ずつ curl で取ったファイル群) からも
#   組める。`--from-files` に **時刻までの接頭辞**を渡すと `PREFIX-*` を集めて 1 枚に見立てる:
#     scripts/snapshot-diff.py --from-files ~/rust-http-proxy-status/2026-09-12T2018Z \
#                              --from-files ~/rust-http-proxy-status/2026-09-16T0106Z
#   (`-status` `-history_res_3600` `-hosts_sort_requests_limit_1000` `-dns_sort_misses_limit_300`
#    `-errors_n_500` `-connections` `-log_n_500` … を名前で見分ける。取得時刻は接頭辞の
#    `YYYY-MM-DDTHHMM[SS]Z` から読む。`-metrics` と `-dashboard` は JSON ではないので見ない。)
#
# 出すもの (T14.17 の (1)〜(9)):
#   1. 再起動をまたいでいるか (`uptime_secs` / `since_start_secs` と `version`)
#   2. `/history` を**再起動時刻で切った**平常時 (1 時間 300 本未満の標本) の前後 — T14.0 の表の形
#   3. ホスト別 (`/hosts` 最大 1,000 件) の差分 (`status-diff.py` と同じ読み方)。
#      **`--group domain` で eTLD+1 にまとめられる** (T14.54。`img.dlsite.jp` と
#      `www.dlsite.jp` が `dlsite.jp` の 1 行。`www.dlsite.com` は別の単位)
#   4. 接続元別 (`/clients`) の差分と**新しく現れた接続元**
#   5. 名前解決 (`/dns`) の warm と引き直し
#   6. その間の出来事 (`/events`。無い版では飛ばす)
#   7. エラーの原因別の件数 (`/hosts` の `errors_by_cause` の差分と `/errors` の個票)
#   8. バーストの写真 (`/bursts`。無ければ `/history` から山の数だけ出す)
#   9. `--criteria phase14` で **Phase の完了の定義に対する判定表** (満たした / 届かず / 判定できず)
#
# **応答の形の版 (`schema`。T14.49)**: 新しいプロキシの応答は先頭に `"schema":1` を持ちます。
# 読む側は版で分岐しますが、**版の無い古い出力 (版 0) も今までどおり読めます**
# (`~/rust-http-proxy-status/` に残っている雪像はどれも版の無い形)。読んだ版は出力の
# 「形の版 `schema` A → B」の行と `--out json` の `a.schema` / `b.schema` に出ます。
#
# **平常時の切り出し方** (T14.0 と同じ): `/history?res=3600` の標本のうち **1 時間 300 本未満**
# のものだけを使う。バーストの時間帯 (2026-09-11 17〜23 時のような) を混ぜると、ミス 1 回の平均が
# 12.6 ms から 36.0 ms に化けて「直した効き」が読めなくなる。分位点の補間は
# `crates/metrics/src/history.rs` の `Window::quantile_ms` と同じ (`proxydata.quantile_ms`)。
#
# **前後の境目**: 2 枚の間に再起動があれば**再起動の時刻**、無ければ **A の取得時刻**。
# 再起動をまたぐと `/status` の通算 (`total_requests` や `dns.misses`) は引き算できないので、
# そのときは「起動から」の値として出す (引き算できるのは `.rrd` に残る `/hosts` `/clients`)。
#
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import glob
import json
import os
import re
import sys
from datetime import datetime, timezone

# 読む部分は `scripts/proxydata.py` (`status-diff.py` と共有)
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proxydata import (  # noqa: E402
    CAUSE_NAMES,
    fmt_bytes,
    group_by_domain,
    load,
    median,
    quantile_ms,
    resolve_aaaa,
    row_of,
    schema_of,
    warn_newer,
)

# `/history` の標本のうちこの道具が読む欄 (`crates/metrics/src/history.rs` の KEYS)。
# **版によって欄の数が違う** (デプロイ先の 2026-09-16 は 31、いまの main は 32) ので、
# 位置ではなく `keys` の名前で引く。無い欄は None にして「出せない」と印字する。
HFIELDS = (
    "requests", "bytes", "active", "active_max", "rss",
    "connects", "connect_ms_sum", "connect_ms_max", "connect_buckets",
    "forwards", "forward_ms_sum", "forward_ms_max", "forward_buckets",
    "errors", "errors_by_cause", "dns_misses", "dns_ms_sum", "evicted_idle",
)
# 平常時の閾 (T14.0: 1 時間 300 本未満の標本だけを「平常時」とする)
BURST_PER_HOUR = 300
# Phase 14 の完了の定義で名指しされている主要ホスト以外を選ぶときの数
MAJOR_HOSTS = 3


# ---------------------------------------------------------------- 読み込み

def taken_at_from_name(path):
    """`…/2026-09-16T0106Z-status` の `2026-09-16T0106Z` を epoch 秒にする (読めなければ 0)。"""
    m = re.search(r"(\d{4})-(\d{2})-(\d{2})T(\d{2})(\d{2})(\d{2})?Z", os.path.basename(path))
    if not m:
        return 0
    y, mo, d, hh, mm, ss = m.groups()
    return int(datetime(int(y), int(mo), int(d), int(hh), int(mm), int(ss or 0),
                        tzinfo=timezone.utc).timestamp())


def classify(suffix):
    """ファイル名の `PREFIX-` より後ろを `/snapshot` の部分の名前にする (使わないなら None)。"""
    s = suffix
    if s.startswith("status_sort_errors"):
        return "status_errors"
    if s.startswith("status_sort_dns"):
        return "status_dns"
    if s.startswith("status_sort_"):
        return None  # slow は `/status` と同じ中身なので読まない
    if s == "status" or s.startswith("status.json"):
        return "status"
    m = re.match(r"history_res_(\d+)", s)
    if m:
        return "history." + m.group(1)
    for name in ("hosts", "clients", "dns", "errors", "connections", "recent",
                 "log", "events", "bursts"):
        if s.startswith(name):
            return name
    return None


def preferred(name, suffix):
    """同じ部分の候補が複数あるときの好み (小さいほうを採る)。

    `/hosts` と `/clients` は**要求数順**、`/dns` は**ミスの多い順**が使いやすい
    (どの並びでも中身は同じだが、切り詰められたときに残る行が違う)。
    """
    want = {"hosts": "requests", "clients": "requests", "dns": "misses"}.get(name)
    return 0 if want is None or want in suffix else 1


def load_prefix(prefix):
    """`PREFIX-*` を集めて `/snapshot` と同じ形にする (`/snapshot` より前の形の入口)。"""
    base = os.path.basename(prefix)
    found, parts = {}, {}
    for path in sorted(glob.glob(prefix + "-*")):
        suffix = os.path.basename(path)[len(base) + 1:]
        name = classify(suffix)
        if name is None:
            continue
        rank = preferred(name.split(".", 1)[0], suffix)
        if name in found and found[name][0] <= rank:
            continue
        try:
            with open(path, encoding="utf-8") as f:
                parts[name] = json.load(f)
        except (OSError, ValueError):
            continue  # `-metrics` のような JSON でないものは黙って飛ばす
        found[name] = (rank, path)
    if not parts:
        raise SystemExit(f"{prefix}-* に読めるファイルが無い")
    snap = {"taken_at": taken_at_from_name(prefix), "parts": sorted(parts), "dropped": [],
            "history": {}}
    for name, body in parts.items():
        res = name.split(".", 1)
        if res[0] == "history":
            snap["history"][res[1]] = body
        else:
            snap[name] = body
    return normalize(snap, prefix, from_files=True)


def normalize(snap, label, from_files=False):
    """`taken_at` / `version` / `uptime_secs` を埋めて、`history` の鍵を文字列に揃える。"""
    st = part(snap, "status")
    snap["label"] = label
    snap["from_files"] = from_files
    # 応答の形の版 (T14.49)。雪像に無ければ `/status` の部の版、それも無ければ 0
    snap["schema"] = schema_of(snap) or schema_of(st)
    snap["history"] = {str(k): v for k, v in (snap.get("history") or {}).items()}
    if not snap.get("version"):
        snap["version"] = st.get("version")
    if not snap.get("uptime_secs"):
        snap["uptime_secs"] = st.get("uptime_secs") or st.get("since_start_secs") or 0
    snap["since_start_secs"] = st.get("since_start_secs", snap["uptime_secs"])
    if not snap.get("taken_at"):
        snap["taken_at"] = taken_at_from_name(label) or last_sample_time(snap)
    return snap


def last_sample_time(snap):
    """取得時刻が名前から読めないときの当て推量 (履歴のいちばん新しい標本の終わり)。"""
    best = 0
    for h in (snap.get("history") or {}).values():
        rows = (h or {}).get("samples") or []
        if rows:
            best = max(best, rows[-1][0] + (h.get("interval_secs") or 0))
    return best


def as_status(d, src):
    """`/status` 1 枚を `/snapshot` と同じ形に見立てる (`/snapshot` より前の形の入口)。"""
    return normalize({"taken_at": taken_at_from_name(src), "parts": ["status"], "dropped": [],
                      "status": d, "history": {},
                      "hosts": {"hosts": d.get("hosts") or []},
                      "clients": {"clients": d.get("clients") or []}}, src)


def load_source(src, from_files):
    """1 枚を読む。**応答の形の版 (`schema`。T14.49) で分岐する**。

    版 1 以降は先頭の版で形が決まっているので推測しない (`parts` があれば `/snapshot`、
    `"status":"ok"` があれば `/status`)。**版の無い古い出力 (版 0) は今までどおり**
    鍵の有無から推測する — 手元に残っている雪像はどれも版の無い形なので、
    そちらが読めなくなったら過去の分析をやり直せない。
    """
    if from_files or not os.path.isfile(src):
        return load_prefix(src)
    with open(src, encoding="utf-8") as f:
        d = json.load(f)
    if not isinstance(d, dict):
        raise SystemExit(f"{src}: JSON の object ではない")
    warn_newer(d, src)
    if schema_of(d) >= 1:
        # 版 1: 先頭の版があるので、あとは `parts` (雪像) か `status` (1 枚) かだけ
        if isinstance(d.get("parts"), list):
            return normalize(d, src)
        if d.get("status") == "ok":
            return as_status(d, src)
    # 版 0 (`schema` の無い古い出力): 今までどおり鍵の有無から推測する
    if "parts" in d:
        return normalize(d, src)
    if "uptime_secs" in d or "since_start_secs" in d:
        return as_status(d, src)
    raise SystemExit(f"{src}: /snapshot でも /status でもない JSON")


def part(snap, name):
    v = (snap or {}).get(name)
    return v if isinstance(v, dict) else {}


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


# ---------------------------------------------------------------- (1) 再起動

def restart_info(a, b):
    ta, tb = a["taken_at"], b["taken_at"]
    ua, ub = a["uptime_secs"], b["uptime_secs"]
    wall = (tb - ta) if (ta and tb) else None
    reasons = []
    if a["version"] and b["version"] and a["version"] != b["version"]:
        reasons.append(f"版が変わった (`{a['version']}` → `{b['version']}`)")
    if ub < ua:
        reasons.append(f"`uptime_secs` が減った ({ua:,} → {ub:,} 秒)")
    elif wall is not None and wall > 0:
        # 時計のずれと取得の間の分を見込んで、窓の 1% (最低 60 秒) は許す
        slack = max(60, wall // 100)
        if (ub - ua) + slack < wall:
            reasons.append(f"`uptime_secs` の伸び {ub - ua:,} 秒が窓 {wall:,} 秒に足りない "
                           f"(差 {wall - (ub - ua):,} 秒)")
    restarted = bool(reasons)
    started = (tb - ub) if tb else None
    return {
        "restarted": restarted,
        "reasons": reasons,
        "wall_secs": wall,
        "uptime": [ua, ub],
        "since_start": [a["since_start_secs"], b["since_start_secs"]],
        "version": [a["version"], b["version"]],
        "taken_at": [ta, tb],
        "started_at": started,
        # 前後を切る境目: 再起動があればその時刻、無ければ A の取得時刻
        "boundary": (started if restarted else ta) or 0,
    }


# ---------------------------------------------------------------- (2) 履歴

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


def pick_res(a, b):
    """使う解像度 (粗いほど長く残っているので 3600 を第一希望)。"""
    for res in ("3600", "60", "5"):
        for snap in (b, a):
            h = (snap.get("history") or {}).get(res)
            if h and h.get("samples"):
                return res
    return None


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


def aggregate(rows, bounds, limit):
    """標本を足し合わせて T14.0 の表の 1 列にする。`limit` 以上の標本はバーストとして外す。"""
    out = {"samples": 0, "burst_samples": 0, "first_t": None, "last_t": None,
           "connects": 0, "connect_ms_sum": 0.0, "connect_ms_max": 0, "connect_buckets": None,
           "forwards": 0, "forward_ms_sum": 0.0, "forward_ms_max": 0, "forward_buckets": None,
           "dns_misses": 0, "dns_ms_sum": 0.0, "errors": 0,
           "causes": [0] * len(CAUSE_NAMES), "active_max": 0, "requests": None}
    for r in rows:
        c = r["connects"] or 0
        if limit is not None and c >= limit:
            out["burst_samples"] += 1
            continue
        out["samples"] += 1
        out["first_t"] = r["t"] if out["first_t"] is None else min(out["first_t"], r["t"])
        out["last_t"] = r["t"] if out["last_t"] is None else max(out["last_t"], r["t"])
        for kind in ("connect", "forward"):
            out[kind + "s"] += (r[kind + "s"] or 0)
            out[kind + "_ms_sum"] += (r[kind + "_ms_sum"] or 0)
            out[kind + "_ms_max"] = max(out[kind + "_ms_max"], r[kind + "_ms_max"] or 0)
            b = r[kind + "_buckets"]
            if b:
                if out[kind + "_buckets"] is None:
                    out[kind + "_buckets"] = [0] * len(b)
                for j, v in enumerate(b):
                    out[kind + "_buckets"][j] += v
        out["dns_misses"] += (r["dns_misses"] or 0)
        out["dns_ms_sum"] += (r["dns_ms_sum"] or 0)
        out["errors"] += (r["errors"] or 0)
        for j, v in enumerate(r["errors_by_cause"] or []):
            if j < len(out["causes"]):
                out["causes"][j] += v
        out["active_max"] = max(out["active_max"], r["active_max"] or r["active"] or 0)
    for kind in ("connect", "forward"):
        cnt = out[kind + "s"]
        out[kind + "_avg"] = (out[kind + "_ms_sum"] / cnt) if cnt else None
        for q, name in ((0.5, "_p50"), (0.95, "_p95")):
            out[kind + name] = quantile_ms(out[kind + "_buckets"], cnt,
                                           out[kind + "_ms_max"], q, bounds)
    conn = out["connects"]
    miss = out["dns_misses"]
    out["dns_per_connect"] = (miss / conn) if conn else None
    out["ms_per_miss"] = (out["dns_ms_sum"] / miss) if miss else None
    out["dns_ms_per_connect"] = (out["dns_ms_sum"] / conn) if conn else None
    return out


def history_split(a, b, info, burst):
    res = pick_res(a, b)
    if res is None:
        return None
    rows, bounds, causes, interval = merged_history(a, b, res)
    # 平常時の閾は 1 時間あたりの本数なので、解像度に合わせて割る
    limit = max(1, round(burst * interval / 3600.0)) if interval else burst
    edge = info["boundary"]
    before = [r for r in rows if r["t"] < edge]
    after = [r for r in rows if r["t"] >= edge]
    return {
        "res": res, "interval_secs": interval, "limit": limit, "bounds": bounds,
        "causes": causes, "boundary": edge, "samples": len(rows),
        "before": aggregate(before, bounds, limit),
        "after": aggregate(after, bounds, limit),
        "before_all": aggregate(before, bounds, None),
        "after_all": aggregate(after, bounds, None),
    }


# --------------------------------------------- (2b) サーバー側の要約 (`?summary=1`)

def summary_url(since, until, res, normal=True):
    """同じ期間・同じ切り方をサーバーに畳ませる URL (T14.24)。

    この道具が §2 でやっている集計 (1 時間 300 本以上の標本を外して 12 段の
    ヒストグラムを足す) と**同じ求め方**がプロキシ側に入っているので、
    `/history` を丸ごと取らずに 1 要求で同じ数字が取れる。
    """
    q = f"/history?summary=1&since={int(since or 0)}"
    if until:
        q += f"&until={int(until)}"
    if res:
        q += f"&res={int(res)}"
    if normal:
        q += "&normal_hours_only=1"
    return q


def load_summary(src):
    """`/history?...&summary=1` の応答を読む (ファイルか `http://…`)。

    **手元の集計は残す** (これは突き合わせ用)。読めなければ `{"error": …}` を返す。
    """
    if not src:
        return None
    try:
        if src.startswith(("http://", "https://")):
            import urllib.request
            with urllib.request.urlopen(src, timeout=10) as r:  # noqa: S310
                return json.loads(r.read().decode("utf-8"))
        return load(src)
    except Exception as e:                                       # noqa: BLE001
        return {"error": f"{type(e).__name__}: {e}", "source": src}


def summary_check(server, local):
    """サーバー側の要約と手元の集計を並べる (差が出たら**どちらかの窓が違う**)。"""
    if not server or "error" in server or not local:
        return None
    pairs = [
        ("標本", "samples", local.get("samples"), 0),
        ("外した標本", "burst_samples", local.get("burst_samples"), 0),
        ("CONNECT 確立", "connects", local.get("connects"), 0),
        ("p50 (ms)", "p50_ms", local.get("connect_p50"), 1),
        ("p95 (ms)", "p95_ms", local.get("connect_p95"), 1),
        ("ミス/接続", "dns_miss_per_connect", local.get("dns_per_connect"), 2),
        ("ミス 1 回 (ms)", "dns_miss_avg_ms", local.get("ms_per_miss"), 1),
        ("エラー", "errors", local.get("errors"), 0),
        ("山", "active_max", local.get("active_max"), 0),
    ]
    rows = []
    for label, key, mine, digits in pairs:
        theirs = server.get(key)
        same = None
        if theirs is not None and mine is not None:
            same = round(float(theirs), digits) == round(float(mine), digits)
        rows.append({"label": label, "key": key, "server": theirs, "local": mine, "same": same})
    return {"rows": rows, "from": server.get("from"), "to": server.get("to"),
            "normal_hours_only": server.get("normal_hours_only"),
            "interval_secs": server.get("interval_secs")}


# ---------------------------------------------------------------- (3) ホスト別

def host_rows(snap):
    """`/hosts` (最大 1,000 件) を使う。無ければ `/status` の `hosts[]` (上位 50)。"""
    h = part(snap, "hosts")
    rows = h.get("hosts")
    if isinstance(rows, list):
        return rows, h.get("truncated", False), "/hosts"
    rows = part(snap, "status").get("hosts")
    return (rows if isinstance(rows, list) else []), False, "/status の hosts[]"


def restored_since(snap):
    """`.rrd` の通算が**いつから**のものか (無ければ None)。"""
    for name in ("hosts", "clients", "status"):
        v = part(snap, name).get("restored_since")
        if v:
            return v
    return None


def rrd_reset(a, b, rows):
    """`.rrd` の通算が作り直されていないか (作り直されていたら差分は引き算できない)。

    ホスト別・接続元別は状態ファイルの通算なので、**ファイルを捨てて作り直すと数が減る**。
    2026-09-10 の実例: 05:37 の雪像の後 09:38 に `.rrd` が作り直され、09-10 → 09-12 の
    差分では AAAA ありのホストの Δ要求 が −1,529 と負になった。
    """
    since = restored_since(b)
    dropped = sum(1 for r in rows if r["requests"] < 0)
    after_a = bool(since and a["taken_at"] and since > a["taken_at"])
    if not dropped and not after_a:
        return None
    return {"restored_since": since, "dropped_hosts": dropped, "after_a": after_a}


def host_diff(a, b, aaaa_mode, aaaa_table, group="host"):
    a_rows, a_cut, a_src = host_rows(a)
    b_rows, b_cut, b_src = host_rows(b)
    older = {h["host"]: h for h in a_rows}
    rows = [row_of(k, older.get(k), h) for k, h in ((x["host"], x) for x in b_rows)]
    names = sorted({r["name"] for r in rows})
    flags = resolve_aaaa(names, aaaa_mode, aaaa_table)
    for r in rows:
        r["aaaa"] = flags.get(r["name"])
    # `.rrd` の作り直しはまとめる前のホストの数で数える (まとめると負の行が打ち消し合う)
    reset = rrd_reset(a, b, rows)
    if group == "domain":
        rows = group_by_domain(rows)  # eTLD+1 でまとめる (T14.54)
    rows.sort(key=lambda r: (-r["requests"], r["name"]))
    gone = [h for k, h in older.items() if k not in {x["host"] for x in b_rows}]
    return {"rows": rows, "truncated": a_cut or b_cut, "source": [a_src, b_src],
            "gone": len(gone), "aaaa": aaaa_mode != "none", "group": group,
            "hosts": len(names), "rrd_reset": reset}


def aaaa_groups(rows):
    out = []
    total = sum(r["requests"] for r in rows)
    for flag, label in ((True, "AAAA あり"), (False, "AAAA なし"), (None, "AAAA 不明")):
        g = [r for r in rows if r["aaaa"] is flag]
        if not g:
            continue
        avgs = [r["avg_ms"] for r in g if r["avg_ms"] is not None]
        out.append({"label": label, "hosts": len(g),
                    "requests": sum(r["requests"] for r in g), "total": total,
                    "avg_median": median(avgs)})
    return out


def major_hosts(rows, names, count=MAJOR_HOSTS):
    """完了の定義が名指しする「主要ホスト」。既定は**その間の要求数の上位 3** (CONNECT)。

    `other` は「表からあふれたぶん」の疑似ホストなので外す
    (`crates/metrics/src/metrics.rs` の `MAX_HOSTS` を越えた要求の置き場)。
    """
    if names:
        want = [x.strip() for x in names if x.strip()]
        return [r for w in want for r in rows if r["name"] == w or r["key"] == w]
    pool = [r for r in rows if r["connect"] and r["name"] != "other" and r["requests"] > 0]
    return pool[:count]


def miss_rate(row):
    return (row["dns_misses"] / row["requests"]) if row["requests"] else None


# ---------------------------------------------------------------- (4) 接続元別

def client_rows(snap):
    c = part(snap, "clients").get("clients")
    if isinstance(c, list):
        return c
    c = part(snap, "status").get("clients")
    return c if isinstance(c, list) else []


def client_diff(a, b):
    older = {c["client"]: c for c in client_rows(a)}
    rows = []
    for c in client_rows(b):
        r = row_of(c["client"], older.get(c["client"]), c)
        r["client"] = c["client"]
        r["first_seen"] = c.get("first_seen")
        r["last_seen"] = c.get("last_seen")
        r["agent"] = c.get("agent")
        r["distinct_targets"] = c.get("distinct_targets")
        r["literal_targets"] = c.get("literal_targets")
        r["ports"] = c.get("ports")
        # **新しく現れた**: 前の雪像に居ない、または初めて見たのが前の雪像より後
        r["new"] = (c["client"] not in older) or bool(
            c.get("first_seen") and a["taken_at"] and c["first_seen"] > a["taken_at"])
        rows.append(r)
    rows.sort(key=lambda r: (-r["requests"], r["client"]))
    gone = [c for k, c in older.items() if k not in {x["client"] for x in client_rows(b)}]
    return {"rows": rows, "new": [r for r in rows if r["new"]], "gone": gone}


# ---------------------------------------------------------------- (5) 名前解決

IDLE_BUCKETS = ((60, "60 秒未満"), (600, "10 分未満"), (3600, "1 時間未満"), (None, "1 時間以上"))


def dns_info(a, b, restarted):
    sa, sb = part(a, "status").get("dns") or {}, part(b, "status").get("dns") or {}

    def d(key, default=0):
        cur = sb.get(key, default)
        if restarted or key not in sa:
            return cur
        return cur - sa.get(key, default)

    entries = part(b, "dns").get("entries") or []
    idle = {label: 0 for _, label in IDLE_BUCKETS}
    warm = 0
    for e in entries:
        v = e.get("idle_secs")
        for edge, label in IDLE_BUCKETS:
            if edge is None or (v is not None and v < edge):
                idle[label] += 1
                break
        if e.get("warm"):
            warm += 1
    refreshed = sorted(((e.get("refreshes") or 0, e.get("host")) for e in entries), reverse=True)
    return {
        "entries": sb.get("entries"), "warm": sb.get("warm"), "warm_secs": sb.get("warm_secs"),
        "warm_in_table": warm, "ttl_secs": sb.get("ttl_secs"),
        "misses": d("misses"), "refreshes": d("refreshes"),
        "hits": d("hits"), "negative_hits": d("negative_hits"), "stale_served": d("stale_served"),
        "miss_ms_sum": d("miss_ms_sum", 0.0),
        "table": len(entries), "idle": idle,
        "top_refreshed": [(h, n_) for n_, h in refreshed if n_][:5],
        "refreshes_in_table": sum(n_ for n_, _ in refreshed),
        "before": {"entries": sa.get("entries"), "misses": sa.get("misses"),
                   "refreshes": sa.get("refreshes"), "warm": sa.get("warm")},
        "windowed": not restarted and bool(sa),
    }


# ---------------------------------------------------------------- (6) 出来事

def events_between(a, b):
    ev = part(b, "events").get("events")
    if not isinstance(ev, list):
        return None
    lo, hi = a["taken_at"], b["taken_at"]
    rows = [e for e in ev if not lo or not e.get("at") or lo <= e["at"] <= (hi or e["at"])]
    return {"count": len(ev), "between": rows}


# ---------------------------------------------------------------- (7) エラー

def errors_info(a, b, hosts, hist):
    causes = [0] * len(CAUSE_NAMES)
    for r in hosts["rows"]:
        for i, v in enumerate(r["errors_by_cause"]):
            if i < len(causes):
                causes[i] += v
    recorded = part(b, "errors").get("errors")
    lo, hi = a["taken_at"], b["taken_at"]
    in_window = []
    if isinstance(recorded, list):
        in_window = [e for e in recorded
                     if not lo or not e.get("at") or lo <= e["at"] <= (hi or e["at"])]
    by_cause = {}
    for e in in_window:
        by_cause[e.get("cause")] = by_cause.get(e.get("cause"), 0) + 1
    return {
        "hosts_total": sum(r["errors"] for r in hosts["rows"]),
        "hosts_causes": causes,
        "history_total": (hist or {}).get("after_all", {}).get("errors"),
        "history_causes": (hist or {}).get("after_all", {}).get("causes"),
        "recorded": len(recorded) if isinstance(recorded, list) else None,
        "in_window": in_window,
        "in_window_causes": dict(sorted(by_cause.items(), key=lambda kv: -kv[1])),
    }


# ---------------------------------------------------------------- (8) バースト

def shot_text(s):
    """`/bursts` の写真 1 枚の 1 行。

    **`peak` という欄は無い** (`crates/metrics-recent/src/recent.rs` の `Shot::to_json` は
    `at` / `seq` / `active` / `trigger_active` / `max_conns` / `threshold` …)。
    しかも写真は**閾を越えた瞬間**の 1 枚なので、その `active` はその時間帯の山ではない
    (山は `/history` の `active_max`)。`peak` は手で組んだ古い雪像のための保険。
    """
    act = s.get("active", s.get("peak"))
    th = s.get("threshold")
    mx = s.get("max_conns")
    tail = ""
    if th is not None:
        tail = f" / 閾 {n(th)}" + (f"・上限 {n(mx)}" if mx is not None else "")
    return f"{stamp(s.get('at'))} (越えた瞬間 {n(act)} 本{tail})"


def bursts_info(b, hist):
    shots = part(b, "bursts").get("bursts")
    out = {"shots": len(shots) if isinstance(shots, list) else None, "rows": shots or []}
    if hist:
        out["burst_windows"] = [hist["before"]["burst_samples"], hist["after"]["burst_samples"]]
        out["active_max"] = [hist["before_all"]["active_max"], hist["after_all"]["active_max"]]
        out["limit"] = hist["limit"]
    return out


# ---------------------------------------------------------------- (9) 判定表

# Phase 14 の完了の定義のうち**数字で判定できる 4 行** (TASKS.md §5 Phase 14 の末尾と
# Phase 13 の「状態」の表。T14.1 の「デプロイ先の受け入れ基準」と同じ閾値)。
PHASE14 = {
    "dns_per_connect": 0.15,
    "major_miss_rate": 0.05,
    "connect_p50_ms": 6.0,
    "overload": 0,
}
CRITERIA = {"phase14": PHASE14}

MET, MISSED, UNKNOWN = "満たした", "届かず", "判定できず"


def judge(name, hist, hosts, majors, status_b, overload, th):
    after = (hist or {}).get("after") or {}
    after_all = (hist or {}).get("after_all") or {}
    rows = []

    v = after.get("dns_per_connect")
    if v is None:
        rows.append(("`dns.misses ÷ 要求` が 0.15 未満", f"< {th['dns_per_connect']}", "—",
                     UNKNOWN, "`/history` にこの期間の標本が無い"))
    else:
        rows.append(("`dns.misses ÷ 要求` が 0.15 未満", f"< {th['dns_per_connect']}",
                     f"**{ratio(v)}** 回/接続",
                     MET if v < th["dns_per_connect"] else MISSED,
                     f"平常時 {after.get('samples', 0)} 標本 / {n(after.get('connects'))} 本"))

    if not majors:
        rows.append((f"主要 {MAJOR_HOSTS} ホストのミス率が 0.05 未満", f"< {th['major_miss_rate']}",
                     "—", UNKNOWN, "`/hosts` にこの期間の要求が無い"))
    else:
        worst, shown = None, []
        for r in majors:
            rate = miss_rate(r)
            shown.append(f"{r['name']} {ratio(rate)} ({n(r['requests'])} 要求)")
            if rate is not None:
                worst = rate if worst is None else max(worst, rate)
        rows.append((f"主要 {MAJOR_HOSTS} ホストのミス率が 0.05 未満", f"< {th['major_miss_rate']}",
                     "、".join(shown),
                     UNKNOWN if worst is None else
                     (MET if worst < th["major_miss_rate"] else MISSED),
                     "`/hosts` の差分 (要求数の上位)"))

    p50 = after.get("connect_p50")
    if p50 is None:
        rows.append(("平常時の CONNECT 確立 p50 が 6 ms 以下", f"≤ {th['connect_p50_ms']} ms", "—",
                     UNKNOWN, "`/history` にこの期間の標本が無い"))
    else:
        rows.append(("平常時の CONNECT 確立 p50 が 6 ms 以下", f"≤ {th['connect_p50_ms']} ms",
                     f"**{ms(p50)} ms**",
                     MET if p50 <= th["connect_p50_ms"] else MISSED,
                     f"平常時 {after.get('samples', 0)} 標本 / {n(after.get('connects'))} 本"))

    over = overload
    bursts = after.get("burst_samples")
    if not bursts:
        note = "この期間に**バーストが無かった** (次に来たら埋まる)"
        verdict = UNKNOWN
    elif over is None:
        note, verdict = "`/status` に `rejected_overload` が無い", UNKNOWN
    else:
        note = f"バーストの窓 {bursts} 本 (山 {n(after_all.get('active_max'))})"
        verdict = MET if over <= th["overload"] else MISSED
    rows.append(("バーストがあっても `rejected_overload` 0 で `/status` が取れる", "= 0",
                 f"`rejected_overload` {n(over)}" + ("" if status_b else "、`/status` が取れない"),
                 verdict, note))
    return {"name": name, "rows": rows,
            "tally": {v: sum(1 for r in rows if r[3] == v) for v in (MET, MISSED, UNKNOWN)}}


# ---------------------------------------------------------------- 組み立て

def build(a, b, args):
    info = restart_info(a, b)
    hist = history_split(a, b, info, args.burst)
    mode, table = "dns", {}
    if args.no_dns:
        mode = "none"
    elif args.aaaa:
        mode, table = "file", load(args.aaaa)
    hosts = host_diff(a, b, mode, table, getattr(args, "group", "host"))
    majors = major_hosts(hosts["rows"], args.major_hosts.split(",") if args.major_hosts else None)
    status_b = part(b, "status")
    # `rejected_overload` は起動からの通算なので、再起動していなければその間の差を見る
    over_a, over_b = part(a, "status").get("rejected_overload"), status_b.get("rejected_overload")
    overload = over_b if (info["restarted"] or over_a is None or over_b is None) else over_b - over_a
    out = {
        "a": {"label": a["label"], "taken_at": a["taken_at"], "version": a["version"],
              "uptime_secs": a["uptime_secs"], "parts": a.get("parts") or [],
              "dropped": a.get("dropped") or [], "schema": a.get("schema", 0)},
        "b": {"label": b["label"], "taken_at": b["taken_at"], "version": b["version"],
              "uptime_secs": b["uptime_secs"], "parts": b.get("parts") or [],
              "dropped": b.get("dropped") or [], "schema": b.get("schema", 0)},
        "aaaa_source": {"dns": "getaddrinfo", "file": args.aaaa, "none": "引かない"}[mode],
        "restart": info,
        "history": hist,
        "hosts": hosts,
        "majors": [{"name": r["name"], "requests": r["requests"], "dns_misses": r["dns_misses"],
                    "miss_rate": miss_rate(r)} for r in majors],
        "clients": client_diff(a, b),
        "dns": dns_info(a, b, info["restarted"]),
        "events": events_between(a, b),
        "bursts": bursts_info(b, hist),
    }
    out["errors"] = errors_info(a, b, hosts, hist)
    # サーバー側の要約 (T14.24)。`--summary` があれば読んで手元の集計と並べる
    if hist:
        out["summary_url"] = summary_url(info["boundary"], b["taken_at"],
                                         int(hist["res"]), True)
        server = load_summary(getattr(args, "summary", None))
        out["summary"] = server
        out["summary_check"] = summary_check(server, hist["after"])
    if args.criteria:
        out["criteria"] = judge(args.criteria, hist, hosts, majors, status_b, overload,
                                CRITERIA[args.criteria])
    return out


# ---------------------------------------------------------------- Markdown

def render(d, top):
    p = print
    a, b = d["a"], d["b"]
    p(f"# snapshot-diff: `{a['label']}` → `{b['label']}`")
    p()
    p(f"- 取得 {stamp(a['taken_at'])} → {stamp(b['taken_at'])}"
      + (f" (窓 {d['restart']['wall_secs'] / 3600:.1f} 時間)" if d["restart"]["wall_secs"] else ""))
    # 応答の形の版 (T14.49)。0 = `schema` を持たない古い出力 (推測で読んだ)
    p(f"- 形の版 `schema` {a.get('schema', 0)} → {b.get('schema', 0)}"
      + ("" if a.get("schema") and b.get("schema")
         else " (0 = 版を持たない古い出力。鍵の有無から推測して読んだ)"))
    for who in (a, b):
        if who["dropped"]:
            p(f"- **4 MiB を越えて落とした部分** (`{who['label']}`): "
              f"{', '.join(who['dropped'])}")
    p()

    # --- 1
    r = d["restart"]
    p("## 1. 再起動")
    p()
    if r["restarted"]:
        p(f"**再起動をまたいでいる** ({'、'.join(r['reasons'])})。"
          f"起動は {stamp(r['started_at'])} ごろ。")
        p()
        p("`/status` の通算 (`total_requests` `dns.*` `rejected_overload`) は**引き算できない**ので "
          "「起動から」の値として読む。`.rrd` に残る `/hosts` `/clients` の差分と "
          "`/history` の標本は再起動をまたいでも続いている。")
    else:
        p(f"再起動はしていない (版 `{r['version'][1]}`、"
          f"`uptime_secs` {r['uptime'][0]:,} → {r['uptime'][1]:,} 秒)。")
    p()
    p("| | 前 | 後 |")
    p("|---|---|---|")
    p(f"| 版 | `{r['version'][0]}` | `{r['version'][1]}` |")
    p(f"| `uptime_secs` | {n(r['uptime'][0])} | {n(r['uptime'][1])} |")
    p(f"| `since_start_secs` | {n(r['since_start'][0])} | {n(r['since_start'][1])} |")
    p()

    # --- 2
    h = d["history"]
    p("## 2. 平常時の前後 (`/history` を再起動時刻で切る)")
    p()
    if not h:
        p("(`/history` がこの雪像に入っていない)")
        p()
    else:
        bef, aft = h["before"], h["after"]
        bef_all, aft_all = h["before_all"], h["after_all"]
        p(f"`/history?res={h['res']}` の {h['samples']} 標本を {stamp(h['boundary'])} で切り、"
          f"**1 標本 {h['limit']} 本以上の窓 (バースト) を外した**もの同士で並べる (T14.0 と同じ切り方)。")
        p()
        p("| 項目 | 前 | 後 | 出どころ |")
        p("|---|---|---|---|")
        p(f"| 期間 | {stamp(bef['first_t'])} → {stamp(h['boundary'])} "
          f"(平常時 {bef['samples']} / {bef['samples'] + bef['burst_samples']} 標本) "
          f"| {stamp(h['boundary'])} → {stamp(aft['last_t'])} "
          f"(平常時 {aft['samples']} / {aft['samples'] + aft['burst_samples']} 標本) "
          f"| `/history?res={h['res']}` |")
        p(f"| CONNECT 確立 | avg {ms(bef['connect_avg'])} / **p50 {ms(bef['connect_p50'])}** "
          f"/ p95 {ms(bef['connect_p95'])} ms ({n(bef['connects'])} 本) "
          f"| avg {ms(aft['connect_avg'])} / **p50 {ms(aft['connect_p50'])}** "
          f"/ p95 {ms(aft['connect_p95'])} ms ({n(aft['connects'])} 本) | 同上 |")
        p(f"| 名前解決のミス | **{ratio(bef['dns_per_connect'])} 回/接続**、"
          f"ミス 1 回 {ms(bef['ms_per_miss'])} ms、接続 1 本あたり {ms(bef['dns_ms_per_connect'])} ms "
          f"| **{ratio(aft['dns_per_connect'])} 回/接続**、"
          f"ミス 1 回 {ms(aft['ms_per_miss'])} ms、接続 1 本あたり {ms(aft['dns_ms_per_connect'])} ms "
          "| 同上 |")
        p(f"| forward 初バイト | avg {ms(bef['forward_avg'])} / p50 {ms(bef['forward_p50'])} "
          f"/ p95 {ms(bef['forward_p95'])} ms ({n(bef['forwards'])} 本) "
          f"| avg {ms(aft['forward_avg'])} / p50 {ms(aft['forward_p50'])} "
          f"/ p95 {ms(aft['forward_p95'])} ms ({n(aft['forwards'])} 本) | 同上 |")
        p(f"| (参考) バースト込み | {ratio(bef_all['dns_per_connect'])} 回/接続、"
          f"ミス 1 回 {ms(bef_all['ms_per_miss'])} ms、avg {ms(bef_all['connect_avg'])} "
          f"/ p50 {ms(bef_all['connect_p50'])} / p95 {ms(bef_all['connect_p95'])} ms "
          f"({n(bef_all['connects'])} 本、最大 {n(bef_all['connect_ms_max'])} ms) "
          f"| {ratio(aft_all['dns_per_connect'])} 回/接続、"
          f"ミス 1 回 {ms(aft_all['ms_per_miss'])} ms、avg {ms(aft_all['connect_avg'])} "
          f"/ p50 {ms(aft_all['connect_p50'])} / p95 {ms(aft_all['connect_p95'])} ms "
          f"({n(aft_all['connects'])} 本、最大 {n(aft_all['connect_ms_max'])} ms) | 同上 |")
        p(f"| 山 (`active_max`) / エラー | {n(bef_all['active_max'])} / {n(bef_all['errors'])} 件 "
          f"| {n(aft_all['active_max'])} / {n(aft_all['errors'])} 件 | 同上 |")
        p()
        if d.get("summary_url"):
            p(f"「後」の列と同じ数字は**サーバー側で 1 要求**でも取れる (T14.24): "
              f"`curl \"http://PROXY{d['summary_url']}\"` "
              "(`since=restart` なら起動から。`/history` を丸ごと取らなくてよい)")
            p()
        sc = d.get("summary_check")
        if d.get("summary") and "error" in d["summary"]:
            p(f"**`--summary` が読めなかった** (`{d['summary']['error']}`)。手元の集計だけで読む。")
            p()
        elif sc:
            p(f"サーバー側の要約 (`?summary=1`、{stamp(sc['from'])} → {stamp(sc['to'])}、"
              f"{sc['interval_secs']} 秒の窓、平常時 {'あり' if sc['normal_hours_only'] else 'なし'}) "
              "と手元の集計を並べる (**窓が同じなら一致する**)。")
            p()
            p("| 項目 | サーバー (`?summary=1`) | 手元 (この道具) | |")
            p("|---|---|---|---|")
            for r in sc["rows"]:
                mark = "" if r["same"] is None else ("一致" if r["same"] else "**違う**")
                p(f"| {r['label']} | {n(r['server'])} | {n(r['local'])} | {mark} |")
            p()

    # --- 3
    hs = d["hosts"]
    grouped = hs.get("group") == "domain"
    p("## 3. ホスト別 (`/hosts` の差分)"
      + ("、**eTLD+1 でまとめた** (`--group domain`)" if grouped else ""))
    p()
    if grouped:
        p(f"**{hs.get('hosts', 0)} ホストを {len(hs['rows'])} のまとめの単位に畳んだ** "
          "(`img.dlsite.jp` と `www.dlsite.jp` は `dlsite.jp` の 1 行。`www.dlsite.com` は"
          "別の単位)。要求数・名前解決・エラーは和、`Δavg` は計測数で重みづけ。"
          "**分位点 (p50 / p95) は足せない**ので 2 つ以上まとまった行では出さない。")
        p()
    p(f"出どころ `{hs['source'][0]}` → `{hs['source'][1]}`"
      + ("、**途中で切れている雪像がある** (`?limit=` か `?sort=` で絞ること)" if hs["truncated"] else "")
      + f"。AAAA の判定: {d['aaaa_source']}")
    p()
    if hs["rrd_reset"]:
        rr = hs["rrd_reset"]
        p(f"**`.rrd` の通算が作り直されている** (通算は {stamp(rr['restored_since'])} から"
          + (f"、{rr['dropped_hosts']} ホストで要求数が減った" if rr["dropped_hosts"] else "")
          + ")。**この差分は引き算できない** (下の Δ は当てにならない。"
            "新しいほうを 1 枚で読むこと: `scripts/status-diff.py B.json`)。")
        p()
    if d["restart"]["restarted"]:
        # T14.0 の注意: `.rrd` の通算は再起動をまたぐので、差分の窓は「取得から取得まで」になる
        before = d["restart"]["started_at"] - a["taken_at"]
        p(f"**この差分の窓は {stamp(a['taken_at'])} → {stamp(b['taken_at'])}** "
          f"(`.rrd` の通算の引き算なので、**再起動前の {before / 3600:.1f} 時間を含む**)。"
          "再起動で切った平常時の前後は §2 を見ること。")
        p()
    p(("| まとめの単位 (ホスト数)" if grouped else "| ホスト")
      + " | Δ要求 | Δ計測 | Δavg (±) | Δ名前解決 | ミス率 | Δ確立/本 | Δエラー |")
    p("|---|---|---|---|---|---|---|---|")
    shown = [r for r in hs["rows"] if r["requests"] or r["errors"]][:top]
    for r in shown:
        per_m = (r["dns_ms_sum"] / r["dns_misses"]) if r["dns_misses"] else None
        per_c = (r["connect_ms_sum"] / r["timed"]) if r["timed"] else None
        flag = " (AAAA)" if hs["aaaa"] and r["aaaa"] else ""
        # `other` は表からあふれたぶんの置き場なので種類を持たない
        kind = "" if r["connect"] or "://" not in r["key"] else " (forward)"
        if grouped:
            kind += f" ({r['hosts']} ホスト)"
        dns = n(r["dns_misses"]) + " 回" + (f" / 1 回 {ms(per_m)} ms" if per_m is not None else "")
        p(f"| `{r['name']}`{kind}{flag} | {n(r['requests'])} | {n(r['timed'])} "
          f"| {ms(r['avg_ms'])} (±{ms(r['avg_err'])}) "
          f"| {dns} | {ratio(miss_rate(r))} "
          f"| {ms(per_c)} ms | {n(r['errors'])} |")
    if not shown:
        p("| (この間に動いたホストは無い) | | | | | | | |")
    p()
    if hs["aaaa"]:
        for g in aaaa_groups([r for r in hs["rows"] if r["connect"]]):
            share = (100.0 * g["requests"] / g["total"]) if g["total"] else 0.0
            p(f"- CONNECT / {g['label']}: {g['hosts']} ホスト、要求 {n(g['requests'])} "
              f"({share:.1f}%)、その間の avg の中央値 **{ms(g['avg_median'])} ms**")
        p()

    # --- 4
    cl = d["clients"]
    # `distinct_targets` / `agent` / `ports` は `.rrd` に残らない**起動からの**欄なので、
    # 窓が再起動をまたぐと同じ行の中で時間軸が食い違う (Δ要求 9 なのに宛先 0 種、など)
    boundary = d["restart"]["started_at"] if d["restart"]["restarted"] else None
    p("## 4. 接続元別 (`/clients` の差分)")
    p()
    if boundary:
        p(f"**`宛先の種類` と `User-Agent` は差分ではなく起動 ({stamp(boundary)}) からの値** "
          "(`.rrd` に残らないので再起動で 0 に戻る)。**再起動より後に一度も見ていない接続元は "
          "`—`** にしてある — その行の Δ要求 は再起動の前に入ったぶん。")
        p()
    p("| 接続元 | Δ要求 | Δバイト | Δavg (±) | 宛先の種類 (起動から) | User-Agent | 初めて見た |")
    p("|---|---|---|---|---|---|---|")
    for r in cl["rows"][:top]:
        mark = " **新**" if r["new"] else ""
        seen_now = not boundary or (r.get("last_seen") or 0) >= boundary
        targets = n(r["distinct_targets"]) if seen_now else "—"
        agent = (r["agent"] or "—") if seen_now else "—"
        p(f"| `{r['client']}`{mark} | {n(r['requests'])} | {fmt_bytes(r['bytes'])} "
          f"| {ms(r['avg_ms'])} (±{ms(r['avg_err'])}) | {targets} "
          f"| {agent} | {stamp(r['first_seen'])} |")
    if not cl["rows"]:
        p("| (接続元の記録が無い) | | | | | | |")
    p()
    if cl["new"]:
        p(f"**新しく現れた接続元 {len(cl['new'])} 件**: "
          + "、".join(f"`{r['client']}` ({n(r['requests'])} 要求)" for r in cl["new"]))
    else:
        p("新しく現れた接続元は無い。")
    if cl["gone"]:
        p()
        p(f"前の雪像に居て今は居ない接続元 {len(cl['gone'])} 件: "
          + "、".join(f"`{c['client']}`" for c in cl["gone"][:10]))
    p()

    # --- 5
    dn = d["dns"]
    p("## 5. 名前解決 (`/dns` の warm と引き直し)")
    p()
    win = "その間" if dn["windowed"] else "起動から"
    p(f"- 表 {n(dn['entries'])} 件 / warm **{n(dn['warm'])}** 件"
      + (f" (`PROXY_DNS_WARM_SECS` {n(dn['warm_secs'])} 秒)" if dn["warm_secs"] else "")
      + f" / TTL {n(dn['ttl_secs'])} 秒")
    per = ms(dn["miss_ms_sum"] / dn["misses"]) if dn["misses"] else None
    p(f"- {win}のミス **{n(dn['misses'])}** 回 (1 回 {per or '—'} ms)")
    p(f"- {win}の裏の引き直し **{n(dn['refreshes'])}** 回"
      + (f" (表の内訳: {'、'.join(f'{h} {v}' for h, v in dn['top_refreshed'])})"
         if dn["top_refreshed"] else " (引き直した名前は表に無い)"))
    p(f"- 負のキャッシュ命中 {n(dn['negative_hits'])} / 古い答えで代用 {n(dn['stale_served'])}")
    if dn["table"]:
        p(f"- 最後に使ってから: "
          + "、".join(f"{label} **{dn['idle'][label]}** 件" for _, label in IDLE_BUCKETS)
          + f" (表 {dn['table']} 件中、warm 印つき {dn['warm_in_table']} 件)")
    p()

    # --- 6
    ev = d["events"]
    p("## 6. その間の出来事 (`/events`)")
    p()
    if ev is None:
        p("(この版に `/events` は無い — T14.11 が入ったら埋まる)")
    elif not ev["between"]:
        p(f"この窓の出来事は無い (`/events` 全体で {ev['count']} 件)。")
    else:
        p("| 時刻 | 種類 | 中身 |")
        p("|---|---|---|")
        for e in ev["between"][:top]:
            # 本文の鍵は `text` (`crates/metrics-recent/src/events.rs` の `Event::to_json`)。
            # `what` / `msg` は手で組んだ古い雪像のための保険。
            p(f"| {stamp(e.get('at'))} | {e.get('kind', '—')} "
              f"| {e.get('text') or e.get('what') or e.get('msg') or '—'} |")
    p()

    # --- 7
    er = d["errors"]
    p("## 7. エラーの原因別")
    p()
    shown = "、".join(f"{CAUSE_NAMES[i]} {v}" for i, v in enumerate(er["hosts_causes"]) if v) or "—"
    p(f"- `/hosts` の差分: **{n(er['hosts_total'])} 件** / 原因 {shown}")
    if er["history_total"] is not None:
        hshown = "、".join(f"{CAUSE_NAMES[i]} {v}"
                           for i, v in enumerate(er["history_causes"] or []) if v) or "—"
        p(f"- `/history` (後の期間、バースト込み): {n(er['history_total'])} 件 / 原因 {hshown}")
    if er["recorded"] is None:
        p("- `/errors` の個票はこの雪像に入っていない")
    else:
        p(f"- `/errors` の個票: 窓の中 **{len(er['in_window'])} 件** (雪像には {er['recorded']} 件)"
          + (f" / 原因 {'、'.join(f'{k} {v}' for k, v in er['in_window_causes'].items())}"
             if er["in_window_causes"] else ""))
        for e in er["in_window"][:5]:
            p(f"  - `{stamp(e.get('at'))}` {e.get('kind')} {e.get('target')} → {e.get('status')} "
              f"({e.get('cause')}、dns {e.get('dns_ms')} ms / connect {e.get('connect_ms')} ms、"
              f"from {e.get('client')})")
    p()

    # --- 8
    bu = d["bursts"]
    p("## 8. バースト (`/bursts`)")
    p()
    if bu["shots"] is None:
        p("(この版に `/bursts` は無い — T14.6 が入ったら埋まる)")
    else:
        p(f"写真 **{bu['shots']} 枚**"
          + (f": " + "、".join(shot_text(s) for s in bu["rows"][:top]) if bu["rows"] else ""))
        if bu["rows"]:
            # 写真は**閾を越えた瞬間**の 1 枚なので、その `active` はその時間帯の山ではない
            p("(写真の数は閾を越えた瞬間の同時接続。その時間帯の山は下の `active_max`)")
    if "burst_windows" in bu:
        p(f"- `/history` から数えたバーストの窓 (1 標本 {bu['limit']} 本以上): "
          f"前 **{bu['burst_windows'][0]}** / 後 **{bu['burst_windows'][1]}**"
          f"、同時接続の山 {n(bu['active_max'][0])} → {n(bu['active_max'][1])}")
    p()

    # --- 9
    if "criteria" in d:
        c = d["criteria"]
        p(f"## 9. 完了の定義に対する判定 (`--criteria {c['name']}`)")
        p()
        if grouped:
            # 「主要 3 ホスト」は §3 の上位から採るので、まとめると**単位**になる
            p("**`--group domain` を付けているので、「主要 3 ホスト」の行はまとめの単位 "
              "(eTLD+1) で判定している**。ホスト 1 件ずつで判定するには `--group` を外すこと。")
            p()
        p("| 完了の定義 | 閾値 | 実測 (後の期間) | 判定 | 出どころ |")
        p("|---|---|---|---|---|")
        for row in c["rows"]:
            p(f"| {row[0]} | {row[1]} | {row[2]} | **{row[3]}** | {row[4]} |")
        p()
        p("・".join(f"{k} {v} 行" for k, v in c["tally"].items() if v))
        p()


# ---------------------------------------------------------------- 入口

def parser():
    p = argparse.ArgumentParser(
        description="2 枚の /snapshot から「何が変わったか」を全部出す (T14.17)")
    p.add_argument("files", nargs="*", metavar="SNAPSHOT.json",
                   help="/snapshot の JSON を 2 つ (古いほう・新しいほうの順は問わない)")
    p.add_argument("--from-files", action="append", metavar="PREFIX", default=[],
                   help="`/snapshot` より前の形 (`PREFIX-status` `PREFIX-history_res_3600` …) "
                        "から組む。2 回まで渡せる")
    p.add_argument("--aaaa", metavar="FILE", help='{"host": true/false} の JSON で AAAA の有無を与える')
    p.add_argument("--no-dns", action="store_true", help="AAAA を引かない")
    p.add_argument("--criteria", choices=sorted(CRITERIA), metavar="NAME",
                   help="完了の定義に対する判定表を出す (いまは phase14)")
    p.add_argument("--out", choices=["md", "json"], default="md", help="出力の形 (既定 md)")
    p.add_argument("--top", type=int, default=20, metavar="N", help="各表に出す行数 (既定 20)")
    p.add_argument("--burst", type=int, default=BURST_PER_HOUR, metavar="N",
                   help=f"平常時の閾 (1 時間の本数。既定 {BURST_PER_HOUR})")
    p.add_argument("--summary", metavar="SRC",
                   help="`/history?...&summary=1` の応答 (ファイルか http:// の URL) を読んで "
                        "手元の集計と並べる (T14.24。手元の集計はそのまま残る)")
    p.add_argument("--major-hosts", metavar="a,b,c",
                   help="主要ホストを名指しする (既定はその間の要求数の上位 3)")
    p.add_argument("--group", choices=["host", "domain"], default="host", metavar="KEY",
                   help="ホスト別の表のまとめ方 (既定 host)。domain は eTLD+1 でまとめる "
                        "(img.dlsite.jp と www.dlsite.jp が dlsite.jp の 1 行。T14.54)")
    return p


def main(argv=None):
    p = parser()
    args = p.parse_args(argv)
    sources = [(f, False) for f in args.files] + [(f, True) for f in args.from_files]
    if len(sources) != 2:
        p.error("渡せるのは 2 つ (位置引数か --from-files で 2 枚)")
    a, b = (load_source(src, ff) for src, ff in sources)
    if a["taken_at"] and b["taken_at"] and a["taken_at"] > b["taken_at"]:
        a, b = b, a  # 古いほうを「前」にする
    d = build(a, b, args)
    if args.out == "json":
        json.dump(d, sys.stdout, ensure_ascii=False, indent=1, default=str)
        print()
    else:
        render(d, args.top)
    return 0


if __name__ == "__main__":
    sys.exit(main())
