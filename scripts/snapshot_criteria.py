#!/usr/bin/env python3
# `scripts/snapshot-diff.py --criteria` の部 (**Phase の完了の定義に対する判定表**。TODO.md T20.1)。
#
# `snapshot-diff.py` が 1,959 行になったので、判定の部だけをここへ割った (T18.2 の申し送り)。
# 持っているのは `PHASE14`〜`PHASE23` の閾・`CRITERIA`・`RULES`・判定の 1 行 = 1 つの関数・`judge()`・
# 判定表の印字 (`render_criteria()`) と、**判定の関数と差分の両方が使う小物**
# (`part` `stamp` `n` `ms` `ratio` `restart_info` `norm_history` `merged_history` `miss_rate`)。
# **phase24 の heap の行 (`_p24_heap`) だけ先にここにある** が、`RULES["phase24"]` への配線は T24.2。
# 小物までこちらにあるのは、`snapshot-diff.py` は名前に `-` があって `import` できないため
# (共有するものは import できる側に置く)。`snapshot-diff.py` は全部を同じ名前で取り込み直すので、
# 使い方・出力・`sd.CRITERIA` のようなテストから見える名前は割る前と変わらない。
#
# 単体では動かさない (入口は `snapshot-diff.py`)。依存は Python 3 の標準ライブラリだけ。

import math
import os
import re
import statistics
import sys
from datetime import datetime, timezone

# 読む部分は `scripts/proxydata.py` (`status-diff.py` / `snapshot-diff.py` と共有)
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proxydata import CAUSE_NAMES  # noqa: E402

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
)
# 平常時の閾 (T14.0: 1 時間 300 本未満の標本だけを「平常時」とする)。phase22 はこれと
# 「期間の中央値の k 倍」の大きい方 (下の `PHASE22` と `normal_limit`。T22.1)
BURST_PER_HOUR = 300
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


# ---------------------------------------------------------------- (9) 判定表
#
# **1 行 = 1 つの関数** (T15.0 (15))。前の版は Phase 14 の 4 行が `judge()` にべた書きで、
# `CRITERIA` に辞書を足すだけでは「Phase 14 の 4 行が phase15 の閾で出る」誤った表になった。
# いまは `(名前, 閾の文, 実測の取り方, 判定)` を返す関数を `RULES` に並べるだけで足せる。
# 関数が受け取る `c` は下の `context()` が作る辞書で、**材料の部が雪像に無ければ
# `UNKNOWN` (判定できず) を返す**のが規則 (「0 だった」と「読めなかった」を混ぜない)。

# Phase 14 の完了の定義のうち**数字で判定できる 4 行** (TODO.md §5 Phase 14 の末尾と
# Phase 13 の「状態」の表。T14.1 の「デプロイ先の受け入れ基準」と同じ閾値)。
PHASE14 = {
    "dns_per_connect": 0.15,
    "major_miss_rate": 0.05,
    "connect_p50_ms": 6.0,
    "overload": 0,
}

# T15.0 を載せて 24 時間ぶん溜めたあとに読む 6 行 (TODO.md の T15.4 / T15.5 / T15.6)。
PHASE15 = {
    # T15.4: 窓を伸ばすか一律 TTL にするかを決めるための 3 行
    "watch_host": "discord.com",     # Phase 14 で唯一届かなかった相手
    "major_miss_rate": 0.05,
    "refresh_per_warm_hour": 80.0,   # TTL 60 秒 の 3/4 = 45 秒おきに 1 名前 = 80 回/時
    # 窓 3,600 秒 で落ち着くと見込んだ上限。**下の閾は外した** (T15.15 (2)。T15.99 の 0.05 は
    # 幅 0.06〜0.09 を良い方に外れて「届かず」と書かれた。低いのは見込み違いではなく良いこと)
    "miss_max": 0.09,
    # T15.5: 空回りを直したあとに「別の空回りが無い」ことを見る 2 行
    "conn_cores": 0.01,
    "closed_tolerance": 0.10,
    # T15.6: 締め切りを 10 秒にしたら `timeout` が増えないか
    "timeout_tolerance": 0.10,
}
# T17.0a: Phase 17 の版を 24 時間走らせたあとに読む 8 行 (TODO.md の T17.99 の完了の定義)。
# 前の 4 行は phase15 の関数をそのまま使う (閾も同じ値)。後ろの 4 行は T16.99 で手で引いた
# 判定 (i)〜(iii) と、`/events` の種類別 件/時 (`dns_slow` の 7 倍を道具が見つけるため)
PHASE17 = {
    "watch_host": PHASE15["watch_host"],
    "major_miss_rate": PHASE15["major_miss_rate"],
    "refresh_per_warm_hour": PHASE15["refresh_per_warm_hour"],
    "miss_max": PHASE15["miss_max"],
    "timeout_tolerance": PHASE15["timeout_tolerance"],
    # conn 役の CPU/要求 が前の何倍までなら「桁で悪くなっていない」か (T15.12 段 7 の基準)
    "conn_per_request_factor": 1.3,
    # `MAX_WARM` (`crates/net-dns/src/dns.rs`)。最大がこれに届いたら枠の取り合いがある
    "warm_max_limit": 32,
    # `/events` の anomaly の種類ごとの閾 (起動からの 件/時)。**ここに無い種類は表示だけ**
    "events_per_hour": {"dns_slow": 0.1},
    # cgroup の起動からの user / sys (コア数) が前の何倍までなら「同じ桁」か
    "cgroup_factor": 10.0,
}
# T18.2: Phase 18 の版を 24 時間走らせたあとに読む 10 行 (TODO.md の T18.0 の「次の完了の定義」)。
# 前の 8 行は phase17 の関数と閾をそのまま使う。後ろの 2 行は T17.99 の但し書き (d) と (e) の物差し
PHASE18 = dict(
    PHASE17,
    # (g) `heap_used + mmap` の 0 時間の雪像からの増え (MB = 10^6 B)。RSS と `heap_free` は
    # バーストの大きさで決まるので閾を置かない (T18.0 (1)。T17.99 の版は 18.0 → 19.9 で +1.9)
    heap_growth_mb=5.0,
    # (h) `v4_first` の間に利用者の経路で IPv6 を探った回数 (T18.1 の `ipv6.request_probes`)
    request_probes=0,
)
# T20.1: phase18 の 10 行の**物差しを直したもの** (T18.99 で分かった 2 つ)。閾の値は phase18 と同じ。
# (g) はメモリのキャッシュ (`memory.cache_memory`) を引いてから比べる (上限まで使ってよい作りなので、
# 入ったぶんは漏れではない)。`discord.com` のミス率・`dns_warm` の最大と `warm_evicted`・`timeout` の
# 3 行は**表示だけ** (閾を置くと使われ方の変化で毎回落ちる。TODO.md Phase 20 の「候補 4 つ」)
PHASE20 = dict(PHASE18)
# T21.2: phase20 の 10 行のうち、T20.99 で「届かず」になった 3 行の読み方を直したもの。閾の値は phase20 と同じ。
# 平常時のミスの行は**表示だけ** (宛先の数で動く使われ方の数字)。conn 役の CPU/要求 は、前か後の
# `/profile` が下の時間に満たなければ「参考」(集計に数えない)。`ipv6.request_probes` は、最後に探った
# 時刻が 0 時間の雪像の取得から下の時間以内なら「満たした」(起動直後は canary が IPv6 をまだ見ていない)
PHASE21 = dict(
    PHASE20,
    # `/profile?res=60` で 720 標本。5 秒の雪像の部はいつも 1 時間に満たないので「参考」になる
    profile_min_hours=12.0,
    request_probe_grace_hours=3.0,
)
# T22.1: phase21 の 10 行に 22a (T22.2・T22.4) の行を足したもの。**平常時の切り方もこの定義だけ違う**:
# 1 時間の本数が「`BURST_PER_HOUR` (T14.0 の 300 本) と、**その期間の 1 時間の本数の中央値の
# `burst_median_k` 倍**の大きい方」以上の標本をバーストとする (`normal_limit`)。tokyo の 10-15 → 10-17 は
# 量が 12 倍 (中央値 780 本/時) で、300 本で切ると 29 標本のうち 23 が「バースト」になった。量の少ない期間
# (中央値 × k が 300 に届かない) では T14.0 の切り方そのもの
PHASE22 = dict(
    PHASE21,
    # k は雪像 4 組で決めた (TODO.md T22.1): 量の多い tokyo の後の期間は最大 / 中央値が 2.0 (1,564 / 780) で、
    # k ≤ 2 だと量の揃った日のいちばん多い 1 時間を切る。量の少ない 3 期間 (nagoya 10-08 は中央値 44、
    # tokyo 10-15 は 22〜23) は k ≤ 6 なら 300 本の切り方と同じ標本が残る。その間でいちばん小さい 3
    burst_median_k=3,
    # T22.2: `dns_slow` が立つのは窓のミスの合計がこれ以上のときだけ (`anomaly.rs` の `DNS_MIN_SUM_MS`)
    dns_slow_min_sum_ms=1000,
    # T22.4 の「既定を 3,600 に戻す」2 つの条件 (TODO.md T22.4 の結果、親の判断 2026-10-17):
    # `stale_retries` が `stale_hits` のこの割合以上、または下の原因の**接続 1 本あたり**が前の期間のこの倍以上
    # なら戻す (件/時ではない。量が 12 倍になる tokyo では件/時は悪化でなくても当たる。親の判断 2026-10-17)
    stale_retry_share=0.01,
    connect_error_factor=1.5,
    connect_error_causes=("refused", "unreachable", "timeout"),
    # T22.4 の段 1 の目安: 引き直しの回/時が前の期間のこの倍以下、`expired` + `warm_stale` の増えが 1 日これ以内
    refresh_factor=0.5,
    stale_miss_growth_per_day=10,
)
# T23.1: phase22 の 14 行を土台にしたもの。閾の値は phase22 と同じで、**エラーの行の分母**と
# **heap の行の「判定できず」の書き方**だけが違う (TODO.md Phase 23 の (1)〜(3))。
# (a) 接続のエラーは `refused` + `unreachable` + `timeout` の**名前解決に成功した試行あたり**
#     (分母 = `/recent` の connect の行から `dns` の失敗を外した本数)。phase22 の「接続 1 本あたり」は
#     使われ方で振れる (T22.99 の 7.80 倍) ので、表示だけの行として残す。
# (b) dns のミスの率は、同じ分母 (`/recent`) でも並べる (表示だけ)。
# (c) heap の行は、0 時間の雪像と起動が違うときに `/events` の `start` の時刻を添える。
# (d) 無言の再起動 (T23.2) の行を最後に足す。
PHASE23 = dict(PHASE22)
CRITERIA = {"phase14": PHASE14, "phase15": PHASE15, "phase17": PHASE17, "phase18": PHASE18,
            "phase20": PHASE20, "phase21": PHASE21, "phase22": PHASE22, "phase23": PHASE23}
# **出力を 1 文字も変えない定義** (過去の判定を同じ命令で出し直せるように)。T20.1 で差分の表に
# 足した行 (成功した接続だけの確立時間) は、`--criteria` がこの 4 つのときは出さない
FROZEN = ("phase14", "phase15", "phase17", "phase18")

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


def profile_role_cores(snap, role):
    """`/profile` の標本から**その役割の CPU** を「何コアぶん」で出す (部が無ければ None)。

    1 標本の `threads` は役割ごとに `0` (標本 0) か `[cpu_us, samples, [states...]]`
    (`crates/metrics-profile/src/profile.rs` の `push_row`)。位置ではなく `keys` と
    `roles` の名前で引くので、役割が増えても読み方は変わらない。
    """
    p = part(snap, "profile")
    rows = p.get("samples") or []
    keys = p.get("keys") or []
    roles = p.get("roles") or []
    interval = p.get("interval_secs") or 0
    if not rows or "threads" not in keys or role not in roles or not interval:
        return None
    ti, ri = keys.index("threads"), roles.index(role)
    cpu_us = 0
    for r in rows:
        threads = r[ti] if ti < len(r) else None
        t = threads[ri] if threads and ri < len(threads) else None
        if t:
            cpu_us += t[0] or 0
    secs = len(rows) * interval
    return {"cores": cpu_us / 1e6 / secs, "samples": len(rows), "secs": secs}


def minute_parts(a, b, name):
    """2 枚の `/history?res=60` の `name` (`closed` / `transfer`) を時刻で重ねる。

    どちらも `keys` と `samples` を持つ別の配列 (T14.6 / T14.25)。同じ時刻は**新しい雪像の値**。
    欄が無い版は飛ばす。返すのは `({t: {鍵: 値}}, 最初に見つかった部の頭)` か、どちらにも無ければ None。
    """
    rows, head = {}, None
    for snap in (a, b):
        part_ = ((snap.get("history") or {}).get("60") or {}).get(name)
        if not isinstance(part_, dict) or not part_.get("keys"):
            continue
        head = head or part_
        keys = part_["keys"]
        for r in part_.get("samples") or []:
            if r:
                rows[r[0]] = {k: (r[i] if i < len(r) else None) for i, k in enumerate(keys)}
    return (rows, head) if head else None


def tunnel_closes(a, b, boundary, burst):
    """`/history?res=60` から、**平常時に閉じたトンネル**の `idle_timeout` と半閉じの割合 (T15.15 (3))。

    前の版は `/recent` (雪像では 256 KiB で切れ、窓の長さが毎回違う) から割合を取っていた。
    こちらは 1 分ごとの**全数**: 母数は `transfer.tunnels` (終わったトンネルの数)、
    分子は `closed.reasons` の `idle_timeout` (トンネルにしか付かない理由) と `transfer.half_close_n`。

    **平常時だけ**: その分が入る 1 時間の標本 (`/history?res=3600`) が `burst` 本以上なら外す
    (T14.0 の平常時の定義と同じ物差し)。その時間の標本がまだ無い分 (取得の途中の 1 時間) も外す。
    """
    transfer = minute_parts(a, b, "transfer")
    if transfer is None:
        return None
    t_rows, _ = transfer
    closed = minute_parts(a, b, "closed")
    c_rows, c_head = closed if closed else ({}, None)
    reasons = (c_head or {}).get("reasons") or []
    ri = reasons.index("idle_timeout") if "idle_timeout" in reasons else None
    hours = {r["t"]: (r["connects"] or 0) for r in merged_history(a, b, "3600")[0]}

    def side():
        return {"minutes": 0, "tunnels": 0, "idle": 0 if ri is not None else None, "half": 0}
    out = {"before": side(), "after": side(), "burst_minutes": 0, "unknown_minutes": 0}
    for t in sorted(t_rows):
        c = hours.get(t // 3600 * 3600)
        if c is None:
            out["unknown_minutes"] += 1
            continue
        if c >= burst:
            out["burst_minutes"] += 1
            continue
        acc = out["before"] if t < boundary else out["after"]
        acc["minutes"] += 1
        acc["tunnels"] += t_rows[t].get("tunnels") or 0
        acc["half"] += t_rows[t].get("half_close_n") or 0
        if ri is not None:
            rs = (c_rows.get(t) or {}).get("reasons") or []
            acc["idle"] += rs[ri] if ri < len(rs) else 0
    for acc in (out["before"], out["after"]):
        n_ = acc["tunnels"]
        acc["idle_share"] = (acc["idle"] / n_) if n_ and acc["idle"] is not None else None
        acc["half_share"] = (acc["half"] / n_) if n_ else None
    return out


def dns_name_refreshes(snap):
    """`/dns` の名前ごとの引き直しを 1 時間あたりに直した最大 (T15.15 (1))。

    閾 80 回/時は **1 名前あたりの上限** (TTL 60 秒の 3/4 おき) なので、物差しは名前ごとの値。
    `refreshes` はその名前の起動からの通算なので、`uptime_secs` で割る (T15.99 の 78.2 と同じ読み方)。
    """
    entries = part(snap, "dns").get("entries")
    up = snap.get("uptime_secs") or 0
    if not isinstance(entries, list) or not entries or not up:
        return None
    best = max(entries, key=lambda e: e.get("refreshes") or 0)
    return {"host": best.get("host"), "refreshes": best.get("refreshes") or 0,
            "per_hour": (best.get("refreshes") or 0) / (up / 3600.0), "names": len(entries)}


def daily_band(snap):
    """`/daily` の 1 日ごとの `dns_per_connect` の幅 (**その版の日だけ**。T15.15 (2))。"""
    days = part(snap, "daily").get("days")
    if not isinstance(days, list):
        return None
    ver = snap.get("version")
    vals = [d["dns_per_connect"] for d in days
            if d.get("connects") and d.get("dns_per_connect") is not None
            and (not ver or d.get("version") == ver)]
    if not vals:
        return None
    return {"lo": min(vals), "hi": max(vals), "days": len(vals)}


# --- Phase 14 の 4 行 (**出力は 1 文字も変えない**。既存のテストが見張っている) ---

def _p14_dns_per_connect(c, th):
    after = c["after"]
    label, limit = "`dns.misses ÷ 要求` が 0.15 未満", f"< {th['dns_per_connect']}"
    v = after.get("dns_per_connect")
    if v is None:
        return (label, limit, "—", UNKNOWN, "`/history` にこの期間の標本が無い")
    return (label, limit, f"**{ratio(v)}** 回/接続",
            MET if v < th["dns_per_connect"] else MISSED,
            f"平常時 {after.get('samples', 0)} 標本 / {n(after.get('connects'))} 本")


def _p14_major_hosts(c, th):
    label = f"主要 {MAJOR_HOSTS} ホストのミス率が 0.05 未満"
    limit = f"< {th['major_miss_rate']}"
    if not c["majors"]:
        return (label, limit, "—", UNKNOWN, "`/hosts` にこの期間の要求が無い")
    worst, shown = None, []
    for r in c["majors"]:
        rate = miss_rate(r)
        shown.append(f"{r['name']} {ratio(rate)} ({n(r['requests'])} 要求)")
        if rate is not None:
            worst = rate if worst is None else max(worst, rate)
    return (label, limit, "、".join(shown),
            UNKNOWN if worst is None else (MET if worst < th["major_miss_rate"] else MISSED),
            "`/hosts` の差分 (要求数の上位)")


def _p14_connect_p50(c, th):
    after = c["after"]
    label, limit = "平常時の CONNECT 確立 p50 が 6 ms 以下", f"≤ {th['connect_p50_ms']} ms"
    p50 = after.get("connect_p50")
    if p50 is None:
        return (label, limit, "—", UNKNOWN, "`/history` にこの期間の標本が無い")
    return (label, limit, f"**{ms(p50)} ms**",
            MET if p50 <= th["connect_p50_ms"] else MISSED,
            f"平常時 {after.get('samples', 0)} 標本 / {n(after.get('connects'))} 本")


def _p14_overload(c, th):
    over, bursts = c["overload"], c["after"].get("burst_samples")
    if not bursts:
        note, verdict = "この期間に**バーストが無かった** (次に来たら埋まる)", UNKNOWN
    elif over is None:
        note, verdict = "`/status` に `rejected_overload` が無い", UNKNOWN
    else:
        note = f"バーストの窓 {bursts} 本 (山 {n(c['after_all'].get('active_max'))})"
        verdict = MET if over <= th["overload"] else MISSED
    return ("バーストがあっても `rejected_overload` 0 で `/status` が取れる", "= 0",
            f"`rejected_overload` {n(over)}"
            + ("" if c["status_b"] else "、`/status` が取れない"), verdict, note)


# --- Phase 15 の 6 行 (T15.4 が 3 行、T15.5 が 2 行、T15.6 が 1 行) ---

def _p15_watch_host(c, th):
    """T15.4: 窓を伸ばす相手 (`discord.com`) のミス率。"""
    host = th["watch_host"]
    label = f"`{host}` のミス率が {th['major_miss_rate']} 未満"
    limit = f"< {th['major_miss_rate']}"
    rows = [r for r in c["hosts"]["rows"]
            if r["name"] == host or r["name"].endswith("." + host)]
    if not rows:
        return (label, limit, "—", UNKNOWN, f"`/hosts` の差分に `{host}` が無い")
    req = sum(r["requests"] for r in rows)
    miss = sum(r["dns_misses"] for r in rows)
    if req <= 0:
        return (label, limit, f"要求 {n(req)}", UNKNOWN, "この窓にその相手への要求が無い")
    rate = miss / req
    return (label, limit, f"**{ratio(rate)}** ({n(miss)} ミス / {n(req)} 要求)",
            MET if rate < th["major_miss_rate"] else MISSED,
            f"`/hosts` の差分 ({len(rows)} 行)")


def _p15_refresh_rate(c, th):
    """T15.4: 裏の引き直しが 1 名前あたり何回/時か。

    **判定は `/dns` の名前ごとの最大** (T15.15 (1)。閾は 1 名前あたりの上限なので)。
    参考に「通算 ÷ 時間 ÷ warm の平均」も並べる。warm の平均は**全時間** (`after_all`) から取り、
    分子の `dns.refreshes` (起動からの通算、バーストの時間も含む) と窓を揃える。
    """
    limit = f"≤ {th['refresh_per_warm_hour']:.0f} 回/時/名前"
    label = "裏の引き直しが 1 名前あたり 1 時間 80 回以下"
    warm, hours = c["after_all"].get("dns_warm_avg"), c["hours"]
    refreshes = (c["dns"] or {}).get("refreshes")
    avg = (refreshes / hours / warm) if warm and hours and refreshes is not None else None
    avg_text = (f"通算 {n(refreshes)} 回 ÷ {hours:,.1f} 時間 ÷ warm {ms(warm)} 件 = {avg:,.1f}"
                if avg is not None else None)
    top = dns_name_refreshes(c["b"])
    if top is not None:
        v = top["per_hour"]
        return (label, limit,
                f"**{v:,.1f}** 回/時 (`{top['host']}` {n(top['refreshes'])} 回、名前ごとの最大)"
                + (f"。参考: {avg_text}" if avg_text else ""),
                MET if v <= th["refresh_per_warm_hour"] else MISSED,
                f"`/dns` の名前ごとの `refreshes` ÷ `uptime_secs` ({top['names']} 名前)")
    if avg is None:
        return (label, limit, "—", UNKNOWN,
                "`/dns` の部が無く、`/history` に `dns_warm` が無いか、窓の長さか "
                "`dns.refreshes` が取れない")
    return (label, limit, f"**{avg:,.1f}** 回/時/名前 ({avg_text.split(' = ')[0]})",
            MET if avg <= th["refresh_per_warm_hour"] else MISSED,
            "`/dns` の部が無いので `/status` の `dns.refreshes` と `/history` の `dns_warm` の"
            "全時間の平均 (名前ごとの最大より甘い)")


def _p15_miss_band(c, th):
    """T15.4: 平常時のミス率が見込んだ上限以下か (T15.15 (2) で下の閾を外した)。"""
    hi = th["miss_max"]
    label = f"平常時の名前解決のミスが {hi} 回/接続 以下"
    limit = f"≤ {hi}"
    v = c["after"].get("dns_per_connect")
    if v is None:
        return (label, limit, "—", UNKNOWN, "`/history` にこの期間の標本が無い")
    band = daily_band(c["b"])
    days = (f"、日ごとの幅 {ratio(band['lo'])}〜{ratio(band['hi'])} ({band['days']} 日)"
            if band else "")
    return (label, limit, f"**{ratio(v)}** 回/接続{days}", MET if v <= hi else MISSED,
            f"平常時 {c['after'].get('samples', 0)} 標本 / {n(c['after'].get('connects'))} 本"
            + ("、日ごとは `/daily` (その版の日だけ)" if band else ""))


def _p15_conn_cores(c, th):
    """T15.5: 中継の輪が空回りしていないか (`conn` 役の CPU)。"""
    label = "`/profile` の `conn` 役の CPU が 0.01 コア未満"
    limit = f"< {th['conn_cores']} コア"
    v = profile_role_cores(c["b"], "conn")
    if v is None:
        return (label, limit, "—", UNKNOWN, "この雪像に `/profile` の部が無い")
    return (label, limit, f"**{v['cores']:.3f}** コア",
            MET if v["cores"] < th["conn_cores"] else MISSED,
            f"`/profile` {v['samples']} 標本 ({v['secs']:,} 秒)")


def _p15_closed_shape(c, th):
    """T15.5: 直しの前後でトンネルの閉じ方が変わっていないか (T15.15 (3))。

    材料は `/history?res=60` の `closed` と `transfer` の**全数** (平常時だけ)。
    `idle_timeout` と半閉じは**同じ信号の裏表** (半閉じのまま相手が黙ると `idle_timeout` で
    閉じる) なので 1 行にまとめ、判定は `idle_timeout` の割合、半閉じは参考に並べる。
    """
    tol = th["closed_tolerance"]
    label = "トンネルの `idle_timeout` の割合が前後で変わらない (半閉じは参考)"
    limit = f"±{tol * 100:.0f}%"
    t = tunnel_closes(c["a"], c["b"], c["info"].get("boundary") or 0, c["burst"])
    if t is None:
        return (label, limit, "—", UNKNOWN,
                "前後のどちらにも `/history?res=60` の `transfer` が無い")
    bef, aft = t["before"], t["after"]
    src = (f"`/history?res=60` の `closed` / `transfer` の平常時 (前 {bef['minutes']} 分 "
           f"{n(bef['tunnels'])} 本 / 後 {aft['minutes']} 分 {n(aft['tunnels'])} 本。"
           f"バーストの時間 {t['burst_minutes']} 分・時間の標本が無い {t['unknown_minutes']} 分は外した)")
    if not bef["tunnels"] or not aft["tunnels"]:
        side = "前" if not bef["tunnels"] else "後"
        return (label, limit, "—", UNKNOWN, f"{side}の平常時に閉じたトンネルが無い。" + src)
    half = f"、半閉じ {ratio(bef['half_share'])} → {ratio(aft['half_share'])}"
    if bef["idle_share"] is None or aft["idle_share"] is None:
        return (label, limit, "`idle_timeout` —**`closed` の部が無い**" + half, UNKNOWN, src)
    d = change(aft["idle_share"], bef["idle_share"])
    shown = (f"`idle_timeout` {ratio(bef['idle_share'])} → **{ratio(aft['idle_share'])}**"
             + (f" ({d * 100:+.0f}%)" if d is not None else " (前が 0)") + half)
    return (label, limit, shown, MISSED if d is None or abs(d) > tol else MET, src)


def _p15_timeout(c, th):
    """T15.6: 締め切りを縮めても `timeout` の出方が増えないか。"""
    tol = th["timeout_tolerance"]
    label = "エラーの `timeout` が前の期間より増えない"
    limit = f"≤ 前の期間 ×{1 + tol:.1f}"
    i = CAUSE_NAMES.index("timeout")
    hist = c["hist"] or {}
    bef, aft, interval = hist.get("before_all"), hist.get("after_all"), hist.get("interval_secs")
    if not bef or not aft or not bef.get("samples"):
        return (label, limit, "—", UNKNOWN, "`/history` に前の期間の標本が無い")
    b_rate = per_hour(bef, interval, (bef.get("causes") or [0] * 8)[i])
    a_rate = per_hour(aft, interval, (aft.get("causes") or [0] * 8)[i])
    if b_rate is None or a_rate is None:
        return (label, limit, "—", UNKNOWN, "`/history` の `errors_by_cause` が読めない")
    verdict = MET if a_rate <= b_rate * (1 + tol) else MISSED
    return (label, limit, f"**{a_rate:,.2f}** 件/時 (前 {b_rate:,.2f} 件/時)", verdict,
            f"`/history` の `errors_by_cause` (前 {bef['samples']} / 後 {aft['samples']} 標本、"
            "バースト込み)")


# --- Phase 17 の 4 行 (T17.0a。前の 4 行は phase15 の関数をそのまま使う) ---

def profile_source(snap):
    """その雪像の `/profile` の材料と出どころ (無ければ `(None, None)`)。

    `--profile FILE` (`/profile?res=60` の JSON。`build` が `profile_res_60` に入れる) を先に採る。
    無ければ雪像の `/profile` の部 (5 秒の標本で 1 時間未満しか無い) で、そのときは**参考**。
    """
    p = snap.get("profile_res_60")
    if isinstance(p, dict) and p.get("samples"):
        return p, "`--profile`"
    p = part(snap, "profile")
    if p.get("samples"):
        return p, "雪像の `/profile` の部 (**参考**)"
    return None, None


def profile_cost(p, role):
    """`/profile` の JSON 1 つから、`role` の CPU/要求 とプロセス全体のコア数を出す。

    材料は各標本の `requests` と `cpu_us` (プロセス全体) と `threads` の役割ごとの
    `[cpu_us, samples, [states...]]` (`profile_role_cores` と同じ読み方)。欄が無ければ None。
    """
    keys = p.get("keys") or []
    roles = p.get("roles") or []
    rows = p.get("samples") or []
    interval = p.get("interval_secs") or 0
    if not rows or not interval or role not in roles or not all(
            k in keys for k in ("threads", "requests", "cpu_us")):
        return None
    ti, qi, ci, ri = (keys.index("threads"), keys.index("requests"), keys.index("cpu_us"),
                      roles.index(role))
    role_us = cpu_us = requests = 0
    for r in rows:
        threads = r[ti] if ti < len(r) else None
        t = threads[ri] if threads and ri < len(threads) else None
        if t:
            role_us += t[0] or 0
        requests += (r[qi] if qi < len(r) else 0) or 0
        cpu_us += (r[ci] if ci < len(r) else 0) or 0
    secs = len(rows) * interval
    return {"role_us": role_us, "requests": requests,
            "per_request_us": (role_us / requests) if requests else None,
            "cores": cpu_us / 1e6 / secs, "samples": len(rows), "secs": secs,
            "truncated": bool(p.get("truncated"))}


def _p17_conn_per_request(c, th):
    """T17.99 (c): conn 役の CPU/要求 が前の 1.3 倍以下 (T16.99 の (i) を自動で)。

    材料は `--profile` (`/profile?res=60`)。無ければ雪像の `/profile` の部で「参考」と書く。
    **プロセス全体のコア数も並べる** (要求の中身で conn 役の値が振れるので、桁はこちらで見る)。
    """
    k = th["conn_per_request_factor"]
    label = f"`conn` 役の CPU/要求 が前の {k} 倍以下"
    limit = f"≤ 前 ×{k}"

    def side(snap, who):
        p, src = profile_source(snap)
        v = profile_cost(p, "conn") if p else None
        if v is None:
            return None, None
        cut = "、256 KiB で切れている" if v["truncated"] else ""
        return v, (f"{who}: {src} {v['samples']} 標本 ({v['secs'] / 3600:,.1f} 時間{cut}、"
                   f"要求 {n(v['requests'])} 本)")
    aft, a_src = side(c["b"], "後")
    bef, b_src = side(c["a"], "前")
    if aft is None:
        return (label, limit, "—", UNKNOWN,
                "後の雪像に `/profile` の部が無く、`--profile` も渡されていない")
    after_text = f"プロセス全体 {aft['cores']:.4f} コア"
    if not aft["requests"]:
        return (label, limit, after_text, UNKNOWN, a_src + "。要求が無いので 1 要求あたりが出ない")
    if bef is None or not bef["requests"]:
        why = "前の雪像に `/profile` の部が無い" if bef is None else "前の材料に要求が無い"
        return (label, limit, f"{aft['per_request_us']:,.0f} us/要求、{after_text}", UNKNOWN,
                f"{a_src}。{why} (`--profile-before` で渡せる)")
    ratio_ = aft["per_request_us"] / bef["per_request_us"] if bef["per_request_us"] else None
    if ratio_ is None:
        return (label, limit, f"{aft['per_request_us']:,.0f} us/要求、{after_text}", UNKNOWN,
                f"{a_src}。{b_src}。前の conn 役の CPU が 0")
    return (label, limit,
            f"**{ratio_:.2f}** 倍 ({bef['per_request_us']:,.0f} → {aft['per_request_us']:,.0f} "
            f"us/要求)、プロセス全体 {bef['cores']:.4f} → **{aft['cores']:.4f}** コア",
            MET if ratio_ <= k else MISSED, f"{a_src}。{b_src}")


def _p17_warm_max(c, th):
    """T17.99 (T16.99 の (ii)): `MAX_WARM` 32 が埋まらず、warm の追い出しが 0。"""
    lim = th["warm_max_limit"]
    label = f"`dns_warm` の最大が {lim} 未満かつ `dns.warm_evicted` が 0"
    limit = f"< {lim} かつ = 0"
    mx = c["after_all"].get("dns_warm_max")
    sb = part(c["b"], "status").get("dns") or {}
    sa = part(c["a"], "status").get("dns") or {}
    ev = sb.get("warm_evicted")
    # `warm_evicted` は起動からの通算。再起動していなければ前の雪像を引いてその間の値にする
    windowed = ev is not None and not (c["info"] or {}).get("restarted") \
        and sa.get("warm_evicted") is not None
    if windowed:
        ev -= sa["warm_evicted"]
    hist = c["hist"] or {}
    src = (f"`/history?res={hist.get('res')}` の後の期間 (バースト込み) の `dns_warm_max`、"
           f"`/status` の `dns.warm_evicted` ({'その間' if windowed else '起動から'})")
    shown = f"最大 **{n(mx)}** 件、`warm_evicted` **{n(ev)}**"
    if mx is None or ev is None:
        miss = "`/history` に `dns_warm_max` が無い" if mx is None else \
            "`/status` の `dns` に `warm_evicted` が無い"
        return (label, limit, shown, UNKNOWN, f"{miss} (T16.0 より前の版)。" + src)
    return (label, limit, shown, MET if mx < lim and ev == 0 else MISSED, src)


def anomaly_rates(snap):
    """`/events` の anomaly を種類別に「起動からの 件/時」にする (部が無ければ None)。

    種類は `text` の先頭の `<kind>:` (`crates/metrics-watch/src/anomaly.rs` の文面)。
    **`cleared:` (解けた知らせ) は数えない**。前の版から読み継いだ出来事が混ざるので、
    起動 (`taken_at − uptime_secs`) より前の `at` は外す。
    """
    ev = part(snap, "events")
    rows = ev.get("events")
    up = snap.get("uptime_secs") or 0
    if not isinstance(rows, list) or not up:
        return None
    started = (snap.get("taken_at") or 0) - up
    counts = {}
    for e in rows:
        text = e.get("text") or ""
        if e.get("kind") != "anomaly" or text.startswith("cleared:") or ":" not in text:
            continue
        if started and (e.get("at") or 0) < started:
            continue
        kind = text.split(":", 1)[0].strip()
        counts[kind] = counts.get(kind, 0) + 1
    hours = up / 3600.0
    return {"hours": hours, "truncated": bool(ev.get("truncated")),
            "kinds": {k: {"count": v, "per_hour": v / hours} for k, v in counts.items()}}


def _p17_events(c, th):
    """T17.99 (a): `/events` の種類別 件/時。閾があるのは `dns_slow` (0.1 件/時) だけ。"""
    limits = th["events_per_hour"]
    label = "`/events` の anomaly の種類別 件/時 (" + "、".join(
        f"`{k}` {v} 以下" for k, v in limits.items()) + "。ほかは表示だけ)"
    limit = "、".join(f"`{k}` ≤ {v}" for k, v in limits.items())
    aft, bef = anomaly_rates(c["b"]), anomaly_rates(c["a"])
    if aft is None:
        return (label, limit, "—", UNKNOWN, "後の雪像に `/events` の部が無い")
    kinds = sorted(set(aft["kinds"]) | set(limits),
                   key=lambda k: (k not in limits, -aft["kinds"].get(k, {}).get("count", 0), k))
    shown, verdict = [], MET
    for k in kinds:
        v = aft["kinds"].get(k, {"count": 0, "per_hour": 0.0})
        before = (bef or {}).get("kinds", {}).get(k, {"count": 0, "per_hour": 0.0})
        was = f"、前 {before['per_hour']:.2f}" if bef is not None else ""
        text = f"`{k}` {v['per_hour']:.2f} 件/時 ({v['count']} 件{was})"
        if k in limits:
            text = f"`{k}` **{v['per_hour']:.2f}** 件/時 ({v['count']} 件{was})"
            if v["per_hour"] > limits[k]:
                verdict = MISSED
        shown.append(text)
    src = (f"`/events` の `kind == \"anomaly\"` を `text` の先頭で分け、`cleared:` を除き、"
           f"起動からの {aft['hours']:,.1f} 時間で割った")
    if bef is None:
        src += "。前の雪像に `/events` が無い"
    if aft["truncated"]:
        src += "。**`/events` が切れている** (件数は下限)"
    return (label, limit, "、".join(shown), verdict, src)


def cgroup_since_start(snap):
    """`/status` の `kernel.cgroup_cpu.since_start` の起動からの CPU (欄が無ければ None)。"""
    cg = (part(snap, "status").get("kernel") or {}).get("cgroup_cpu") or {}
    ss = cg.get("since_start") or {}
    up = snap.get("uptime_secs") or 0
    usage = ss.get("usage_usec")
    if usage is None or not up:
        return None
    user = ss.get("user_usec")
    system = ss.get("system_usec")
    if system is None and user is not None:
        system = usage - user
    return {"cores": usage / 1e6 / up,
            "user_cores": (user / 1e6 / up) if user is not None else None,
            "sys_cores": (system / 1e6 / up) if system is not None else None,
            "user_share": (user / usage) if user is not None and usage else None,
            "throttled": ss.get("nr_throttled"), "periods": ss.get("nr_periods"),
            "hours": up / 3600.0}


def _p17_cgroup(c, th):
    """T17.99 (c): cgroup の起動からの user / sys が前後で同じ桁 (T16.0 で足した欄)。"""
    k = th["cgroup_factor"]
    label = "cgroup の起動からの CPU (user / sys) が前と同じ桁"
    limit = f"user・sys とも ≤ 前 ×{k:.0f}"
    aft, bef = cgroup_since_start(c["b"]), cgroup_since_start(c["a"])

    def text(v):
        share = f"、user {v['user_share'] * 100:.0f}%" if v["user_share"] is not None else ""
        return f"{v['cores']:.4f} コア{share}"
    if aft is None:
        return (label, limit, "—", UNKNOWN,
                "後の雪像の `kernel.cgroup_cpu.since_start` に `usage_usec` が無い")
    thr = ""
    if aft["throttled"] is not None and aft["periods"] is not None:
        thr = f"、絞り {n(aft['throttled'])} / {n(aft['periods'])} 周期"
    src = f"`kernel.cgroup_cpu.since_start` ÷ `uptime_secs` (後 {aft['hours']:,.1f} 時間{thr})"
    if bef is None:
        return (label, limit, f"後 **{text(aft)}**", UNKNOWN,
                "前の版に欄が無い (`usage_usec` は T16.0 から)。" + src)
    worse = []
    for key, name in (("user_cores", "user"), ("sys_cores", "sys")):
        if aft[key] is None or bef[key] is None:
            return (label, limit, f"{text(bef)} → **{text(aft)}**", UNKNOWN,
                    f"`{name}_usec` がどちらかに無い。" + src)
        if (bef[key] and aft[key] > bef[key] * k) or (not bef[key] and aft[key]):
            worse.append(name)
    return (label, limit, f"{text(bef)} → **{text(aft)}**"
            + (f" ({'・'.join(worse)} が桁で増えた)" if worse else ""),
            MISSED if worse else MET, src + f"、前 {bef['hours']:,.1f} 時間")


# --- Phase 18 の 2 行 (T18.2。前の 8 行は phase17 の関数をそのまま使う) ---

def mb(v):
    """バイト数を MB (10^6 B。TODO.md の RSS の書き方) の小数 1 桁にする。"""
    return "—" if v is None else f"{v / 1e6:,.1f}"


def memory_of(snap):
    """その雪像の `/status` の `memory` (無ければ空の辞書)。"""
    m = part(snap, "status").get("memory")
    return m if isinstance(m, dict) else {}


def uptime_text(secs):
    """起動からの時間。1 時間に満たなければ分で書く (0 時間の雪像は起動の数分後)。"""
    secs = secs or 0
    return f"{secs / 60:,.1f} 分" if secs < 3600 else f"{secs / 3600:,.1f} 時間"


def _p18_heap(c, th):
    """T18.0 (g): `heap_used + mmap` が 0 時間の雪像から 5 MB 以上増えていないか。

    材料は `--zero FILE` (0 時間の雪像。`build` が `zero_snapshot` に入れる) と後の雪像の
    `$.status.memory`。**`heap_free` と `rss` は並べるだけ** (閾を置かない。T17.99 (d) で伸びたのは
    `heap_free` で、バーストの大きさで決まる)。`--zero` が無いか、後の雪像と同じ起動でなければ
    「判定できず」。
    """
    lim = th["heap_growth_mb"]
    label = f"`heap_used + mmap` の 0 時間の雪像からの増えが {lim:.0f} MB 未満"
    limit = f"< {lim:.0f} MB"
    z = c["b"].get("zero_snapshot")
    if not z:
        return (label, limit, "—", UNKNOWN, "`--zero` (0 時間の雪像) が渡されていない")
    between = restart_info(z, c["b"])
    if between["restarted"]:
        return (label, limit, "—", UNKNOWN,
                "`--zero` の雪像は後の雪像と同じ起動ではない (" + "、".join(between["reasons"]) + ")")
    mz, ma = memory_of(z), memory_of(c["b"])
    shown = "、".join(f"`{k}` {mb(mz.get(k))} → {mb(ma.get(k))}"
                     for k in ("heap_used", "mmap", "heap_free", "rss"))
    shown += " MB (`heap_free` と `rss` は表示だけ)"
    src = (f"`--zero` (起動から {uptime_text(z.get('uptime_secs'))}) と後の雪像 "
           f"(起動から {uptime_text(c['b'].get('uptime_secs'))}) の `/status` の `memory`")
    missing = [f"{who}の `{k}`" for who, m in (("0 時間", mz), ("後", ma))
               for k in ("heap_used", "mmap") if m.get(k) is None]
    if missing:
        return (label, limit, shown, UNKNOWN, "、".join(missing) + " が無い。" + src)
    before, after = mz["heap_used"] + mz["mmap"], ma["heap_used"] + ma["mmap"]
    grown = (after - before) / 1e6
    return (label, limit, f"**{grown:+.1f}** MB ({mb(before)} → {mb(after)} MB)。" + shown,
            MET if grown < lim else MISSED, src)


def _p18_request_probes(c, th):
    """T18.0 (h): `v4_first` の間に利用者の経路で IPv6 を探っていないか (T18.1 の計器)。

    材料は後の雪像の `$.status.ipv6.request_probes` (起動からの通算)。欄が無い版は「判定できず」。
    0 でなければ「届かず」で、理由を読むための `canary.ipv6_runs` / `ipv6_skipped` を並べる。
    """
    want = th["request_probes"]
    label = "`ipv6.request_probes` が 0 (`v4_first` の間に利用者の経路で IPv6 を探っていない)"
    limit = f"= {want}"
    st = part(c["b"], "status")
    ip = st.get("ipv6") if isinstance(st.get("ipv6"), dict) else {}
    can = st.get("canary") if isinstance(st.get("canary"), dict) else {}
    v = ip.get("request_probes")
    if v is None:
        return (label, limit, "—", UNKNOWN,
                "後の雪像の `/status` の `ipv6` に `request_probes` が無い (T18.1 より前の版)")
    shown = f"**{n(v)}** 回"
    if v and ip.get("request_probe_at"):
        shown += f" (最後は {stamp(ip['request_probe_at'])})"
    shown += (f"、`canary.ipv6_runs` {n(can.get('ipv6_runs'))} / "
              f"`ipv6_skipped` {n(can.get('ipv6_skipped'))}")
    if ip.get("attempts") is not None:
        shown += f"、`ipv6.attempts` {n(ip['attempts'])}"
    src = (f"後の雪像の `/status` の `ipv6.request_probes` (起動からの "
           f"{uptime_text(c['b'].get('uptime_secs'))} の通算) と `canary`")
    return (label, limit, shown, MET if v == want else MISSED, src)


# --- Phase 20 (T20.1。phase18 の 10 行の物差しを直したもの) ---

def _p20_heap(c, th):
    """T20.1 (g): `heap_used + mmap − cache_memory` が 0 時間の雪像から 5 MB 以上増えていないか。

    phase18 の (g) (`_p18_heap`) から**メモリのキャッシュ** (`memory.cache_memory`) を引いたもの。
    キャッシュは上限まで使ってよい作りなので、入ったぶんは漏れではない (T18.99 は 13.4 MB 入って
    +15.7 MB と出た。引くと +2.2 MB)。**`cache_memory` の欄が無い版は 0 として扱わず
    「判定できず」**。ほかの読み方 (`--zero` が要る・後の雪像と同じ起動であること・`heap_free` と
    `rss` は並べるだけ) は `_p18_heap` と同じ。
    """
    lim = th["heap_growth_mb"]
    label = f"`heap_used + mmap − cache_memory` の 0 時間の雪像からの増えが {lim:.0f} MB 未満"
    limit = f"< {lim:.0f} MB"
    z = c["b"].get("zero_snapshot")
    if not z:
        return (label, limit, "—", UNKNOWN, "`--zero` (0 時間の雪像) が渡されていない")
    between = restart_info(z, c["b"])
    if between["restarted"]:
        return (label, limit, "—", UNKNOWN,
                "`--zero` の雪像は後の雪像と同じ起動ではない (" + "、".join(between["reasons"]) + ")")
    mz, ma = memory_of(z), memory_of(c["b"])
    shown = "、".join(f"`{k}` {mb(mz.get(k))} → {mb(ma.get(k))}"
                     for k in ("heap_used", "mmap", "cache_memory", "heap_free", "rss"))
    shown += " MB (`heap_free` と `rss` は表示だけ)"
    src = (f"`--zero` (起動から {uptime_text(z.get('uptime_secs'))}) と後の雪像 "
           f"(起動から {uptime_text(c['b'].get('uptime_secs'))}) の `/status` の `memory`")
    missing = [f"{who}の `{k}`" for who, m in (("0 時間", mz), ("後", ma))
               for k in ("heap_used", "mmap", "cache_memory") if m.get(k) is None]
    if missing:
        return (label, limit, shown, UNKNOWN, "、".join(missing) + " が無い。" + src)
    before = mz["heap_used"] + mz["mmap"] - mz["cache_memory"]
    after = ma["heap_used"] + ma["mmap"] - ma["cache_memory"]
    grown = (after - before) / 1e6
    return (label, limit, f"**{grown:+.1f}** MB ({mb(before)} → {mb(after)} MB)。" + shown,
            MET if grown < lim else MISSED, src)


def display_only(rule, label, why, was="phase18"):
    """判定の関数 `rule` を**表示だけ**の行にする (T20.1)。

    実測と出どころは `rule` のまま出し、閾の欄は `—` (phase18 の閾を添える)、判定の欄は
    「表示だけ」(`SHOWN`) にする。`judge()` の集計は `MET` / `MISSED` / `UNKNOWN` しか数えないので、
    この行は数に入らない。`label` は `th` を受け取って行の名前を返す (元の名前は「〜未満」と
    閾を含むので使わない)。`why` は閾を置かない理由で、出どころの欄の末尾に付く。
    `was` は閾の欄に添える「前の定義」の名前 (T21.2 で足した。phase20 の 3 行は既定の phase18 のまま)。
    """
    def row(c, th):
        _, limit, shown, _, src = rule(c, th)
        return (label(th), f"— ({was} は {limit})", shown, SHOWN,
                f"{src}。**閾を置かない**: {why}")
    row.__name__ = rule.__name__ + "_shown"
    return row


_p20_watch_host = display_only(
    _p15_watch_host, lambda th: f"`{th['watch_host']}` のミス率",
    "間遠に使うと warm の窓から外れて次の 1 本がミスになる (1 回 約 10 ms)。使われ方で動く (T18.99)")
_p20_timeout = display_only(
    _p15_timeout, lambda th: "エラーの `timeout` (件/時。前の期間と並べる)",
    "バーストの中の数件で前の期間を越える (T18.99 は 8 件とも同じ 3 時間)")
_p20_warm_max = display_only(
    _p17_warm_max, lambda th: f"`dns_warm` の最大 (枠 {th['warm_max_limit']}) と `dns.warm_evicted`",
    "枠が埋まるのはバーストの間だけで、追い出された名前が払うのは次のミス 1 回 (T18.99)")


# --- Phase 21 (T21.2。phase20 の 10 行のうち、T20.99 で届かなかった 3 行の読み方を直したもの) ---

# `/status` の `dns.misses_by_kind` を並べる順 (ほかの種類はこの後ろに名前の順)
MISS_KINDS = ("cold", "expired", "warm_stale", "negative")


def miss_kinds(c):
    """`/status` の `dns.misses_by_kind` (ミスの種類別の回数)。再起動していなければその間の差にする。

    返すのは `({種類: 回数}, その間か)` か、欄が無ければ `(None, False)`。
    """
    kb = (part(c["b"], "status").get("dns") or {}).get("misses_by_kind")
    if not isinstance(kb, dict):
        return None, False
    ka = (part(c["a"], "status").get("dns") or {}).get("misses_by_kind")
    windowed = isinstance(ka, dict) and not (c["info"] or {}).get("restarted")
    out = {k: (v or 0) - ((ka.get(k) or 0) if windowed else 0) for k, v in kb.items()}
    return out, windowed


def _p21_miss_band_measured(c, th):
    """平常時のミス (phase15 の行) に、ミスの種類別の内訳 (`cold` / `expired` …) を並べたもの。"""
    label, limit, shown, verdict, src = _p15_miss_band(c, th)
    kinds, windowed = miss_kinds(c)
    if kinds is None:
        return (label, limit, shown, verdict,
                src + "。`/status` の `dns` に `misses_by_kind` が無い (内訳は出せない)")
    order = [k for k in MISS_KINDS if k in kinds] + sorted(k for k in kinds if k not in MISS_KINDS)
    text = "、".join(f"`{k}` {n(kinds[k])}" for k in order)
    return (label, limit, f"{shown}。ミスの内訳 ({'その間' if windowed else '起動から'}、バースト込み): {text}",
            verdict, src + "、内訳は `/status` の `dns.misses_by_kind`")


_p21_miss_band = display_only(
    _p21_miss_band_measured, lambda th: "平常時の名前解決のミス (回/接続。日ごとの幅と種類別の内訳)",
    "宛先の数で動く使われ方の数字 (T20.99 は宛先 395 名前で 0.12、`cold` 408 回)", was="phase20")


def profile_span(snap):
    """その雪像の `/profile` の材料の長さ (`(秒, 標本, 出どころ)`。無ければ None)。"""
    p, src = profile_source(snap)
    if not p:
        return None
    rows = p.get("samples") or []
    return len(rows) * (p.get("interval_secs") or 0), len(rows), src


def _p21_conn_per_request(c, th):
    """conn 役の CPU/要求 (phase17 の行)。**前か後の材料が短いときは「参考」** (T21.2)。

    T20.99 は「前」が起動 1.3 時間・78 標本 (要求 256 本) しか無く、2.72 倍と出て「届かず」に
    数えられた。前か後の `/profile` が `profile_min_hours` (`/profile?res=60` で 720 標本) に
    満たなければ、倍率は出したまま判定の欄を「参考」にし、集計に数えない (理由は出どころの欄)。
    倍率が出ない (材料が無い・要求が無い) ときは phase17 の行のまま「判定できず」。
    """
    label, limit, shown, verdict, src = _p17_conn_per_request(c, th)
    if verdict not in (MET, MISSED):
        return (label, limit, shown, verdict, src)
    hours = th["profile_min_hours"]
    short = []
    for snap, who in ((c["a"], "前"), (c["b"], "後")):
        span = profile_span(snap)
        if span and span[0] < hours * 3600:
            short.append(f"{who}は {span[0] / 3600:,.1f} 時間 ({n(span[1])} 標本)")
    if not short:
        return (label, limit, shown, verdict, src)
    return (label, limit, shown, REFERENCE,
            src + f"。**参考**: {'、'.join(short)} で、{hours:.0f} 時間 (`/profile?res=60` で "
            f"{int(hours * 60):,} 標本) に満たない (短い材料の 1 要求あたりは使われ方で大きく振れるので、"
            f"倍率だけなら「{verdict}」でも集計に数えない)")


def _p21_request_probes(c, th):
    """`ipv6.request_probes` (phase18 の行)。**起動直後だけ探ったのなら「満たした」** (T21.2)。

    0 回なら phase18 と同じ。0 でなければ、最後に探った時刻 (`ipv6.request_probe_at`) が
    `--zero` (0 時間の雪像) の取得から `request_probe_grace_hours` 以内なら「満たした」
    (起動直後は canary がまだ IPv6 を見ていない。T20.99 は 3 回とも起動から 2.4 時間のうち)、
    それより後に探っていれば「届かず」。`--zero` が無い・後の雪像と同じ起動でない・時刻が無いときは
    「判定できず」。
    """
    _, _, shown, verdict, src = _p18_request_probes(c, th)
    grace = th["request_probe_grace_hours"]
    label = (f"`ipv6.request_probes` が 0、または探ったのが 0 時間の雪像から {grace:.0f} 時間以内だけ "
             "(起動直後は canary が IPv6 をまだ見ていない)")
    limit = f"= 0 か、最後が 0 時間 + {grace:.0f} 時間以内"
    ip = part(c["b"], "status").get("ipv6")
    ip = ip if isinstance(ip, dict) else {}
    v = ip.get("request_probes")
    if v is None or v == 0:
        return (label, limit, shown, verdict, src)
    z = c["b"].get("zero_snapshot")
    if not z:
        return (label, limit, shown, UNKNOWN,
                "`--zero` (0 時間の雪像) が渡されていないので、探った時刻を起動直後と比べられない。" + src)
    between = restart_info(z, c["b"])
    if between["restarted"]:
        return (label, limit, shown, UNKNOWN,
                "`--zero` の雪像は後の雪像と同じ起動ではない (" + "、".join(between["reasons"]) + ")。" + src)
    at, zt = ip.get("request_probe_at") or 0, z.get("taken_at") or 0
    if not at or not zt:
        return (label, limit, shown, UNKNOWN,
                "最後に探った時刻 (`request_probe_at`) か 0 時間の雪像の取得時刻が無い。" + src)
    after = (at - zt) / 3600.0
    src += f"、`--zero` の取得 {stamp(zt)}"
    if at <= zt + grace * 3600:
        return (label, limit,
                f"{shown}。最後は 0 時間の雪像から {after:,.1f} 時間後 = **起動直後の {n(v)} 回は canary が "
                "IPv6 を見ていなかったぶん**", MET, src)
    return (label, limit, f"{shown}。最後は 0 時間の雪像から **{after:,.1f} 時間後** ({grace:.0f} 時間より後にも探った)",
            MISSED, src)


# --- Phase 22 (T22.1。phase21 の 10 行 + 22a で段 2 まで行った T22.2・T22.4 の行) ---

def _p22_refresh_rate(c, th):
    """裏の引き直し (phase15 の行)。**keep-warm を止めた版 (T22.4) でも読めるようにしたもの**。

    phase15 の参考の値「通算 ÷ 時間 ÷ warm の平均」は warm 0 (`PROXY_DNS_WARM_SECS=0` が既定) だと
    割れずに出ない (T22.4 の申し送り)。そのときは「warm 0 なので名前あたりの平均は出ない」と書き、
    通算の 1 時間あたりを並べる。判定は今までどおり `/dns` の名前ごとの最大 (warm でない名前も
    serve-stale の裏で引き直すので、閾 80 回/時/名前はそのまま効く)。`/dns` の部が無ければ「判定できず」。
    warm が 0 でなければ phase15 の行そのもの。
    """
    label, limit, shown, verdict, src = _p15_refresh_rate(c, th)
    warm = c["after_all"].get("dns_warm_avg")
    secs = (c["dns"] or {}).get("warm_secs")
    if warm or (warm is None and secs != 0):
        return (label, limit, shown, verdict, src)
    refreshes, hours = (c["dns"] or {}).get("refreshes"), c["hours"]
    total = (f"通算 {n(refreshes)} 回 ÷ {hours:,.1f} 時間 = {refreshes / hours:,.1f} 回/時 (全部の名前)"
             if refreshes is not None and hours else "通算の 1 時間あたりも出せない")
    why = (f"keep-warm は止めてある (`warm_secs` {n(secs)}、warm の平均 {ms(warm)} 件) ので、"
           "warm の平均で割る名前あたりの値は出ない (T22.4)")
    if verdict == UNKNOWN:
        return (label, limit, total, UNKNOWN, f"`/dns` の部が無い。{why}")
    return (label, limit, f"{shown}。参考: {total}", verdict, f"{src}。{why}")


def dns_slow_events(snap, min_sum):
    """`/events` の `dns_slow` (起動から) を、窓のミスの合計 (`N ms total`) で 2 つに分けて数える (T22.2)。

    文面は `anomaly.rs` の `dns_slow: dns miss X ms avg over 5m (threshold T ms; N misses, S ms total)`。
    返すのは `{"big": 合計 min_sum 以上, "small": 未満, "unparsed": 合計が読めない, "hours", "truncated"}`
    か、部が無ければ None。`cleared:` と起動より前の出来事は数えない (`anomaly_rates` と同じ)。
    """
    ev = part(snap, "events")
    rows = ev.get("events")
    up = snap.get("uptime_secs") or 0
    if not isinstance(rows, list) or not up:
        return None
    started = (snap.get("taken_at") or 0) - up
    out = {"big": 0, "small": 0, "unparsed": 0, "hours": up / 3600.0,
           "truncated": bool(ev.get("truncated"))}
    for e in rows:
        text = e.get("text") or ""
        if e.get("kind") != "anomaly" or not text.startswith("dns_slow:"):
            continue
        if started and (e.get("at") or 0) < started:
            continue
        m = re.search(r"([0-9][0-9.]*) ms total", text)
        if not m:
            out["unparsed"] += 1
        elif float(m.group(1)) >= min_sum:
            out["big"] += 1
        else:
            out["small"] += 1
    return out


def _p22_dns_slow(c, th):
    """T22.2: `dns_slow` を新しい閾 (窓のミスの合計 1,000 ms 以上) で数えた件/時と、それより小さい窓で立った件数。

    判定は「合計が閾に満たない窓で立った `dns_slow` が 0」だけで決める (T22.2 の目安を親が読み替えたもの: ミスの
    少ない窓で鳴らない)。T22.2 より前の版の雪像では、新しい閾なら立たなかった件数がここに出る。新しい閾での
    件/時は**表示だけで閾を置かない** (T22.2 の模型で tokyo の後の期間は本当に遅い窓だけで 0.17 件/時あり、
    件/時の閾は使われ方で落ちる。親の判断 2026-10-17)。phase21 の `dns_slow` ≤ 0.1 件/時の行は phase22 では表示だけ。
    """
    lim = th["dns_slow_min_sum_ms"]
    label = (f"`dns_slow` が窓のミスの合計 {lim:,} ms 未満の窓で立っていない "
             f"(新しい閾 = 合計 {lim:,} ms 以上での件/時は表示だけ)")
    limit = f"合計 < {lim:,} ms の窓で 0 件"
    aft, bef = dns_slow_events(c["b"], lim), dns_slow_events(c["a"], lim)
    if aft is None:
        return (label, limit, "—", UNKNOWN, "後の雪像に `/events` の部が無い")

    def text(v, bold):
        rate = v["big"] / v["hours"]
        star = "**" if bold else ""
        t = (f"新しい閾で {star}{rate:.2f}{star} 件/時 ({v['big']} 件)、"
             f"合計 {lim:,} ms 未満 {star}{v['small']}{star} 件")
        return t + (f"、合計が読めない {v['unparsed']} 件" if v["unparsed"] else "")
    shown = text(aft, True) + (f" (前: {text(bef, False)})" if bef is not None else "")
    src = (f"`/events` の `dns_slow:` の文面の `ms total` (起動からの {aft['hours']:,.1f} 時間、"
           "`cleared:` は数えない)")
    if bef is None:
        src += "。前の雪像に `/events` が無い"
    if aft["truncated"]:
        src += "。**`/events` が切れている** (件数は下限)"
    if aft["small"]:
        verdict = MISSED
    elif aft["unparsed"]:
        verdict = UNKNOWN
        src += "。文面に `ms total` の無い `dns_slow` がある (T17.1 より前の形)"
    else:
        verdict = MET
    return (label, limit, shown, verdict, src)


def dns_counter(c, key, sub=None):
    """`/status` の `dns.<key>` (`sub` があれば `dns.<key>.<sub>`) の前と後。

    返すのは `(前の値, 後の値, その間か)`。後は再起動していなければ前を引いた差 (その間)、していれば
    起動からの通算。前は前の雪像の起動からの通算。どちらかに欄が無ければその側は None。
    """
    def get(snap):
        v = (part(snap, "status").get("dns") or {}).get(key)
        if sub is not None:
            # 内訳の辞書があってその種類が無いのは 0 回 (`misses_by_kind` は出た種類だけのことがある)
            v = (v.get(sub) or 0) if isinstance(v, dict) else None
        return v
    va, vb = get(c["a"]), get(c["b"])
    windowed = va is not None and vb is not None and not (c["info"] or {}).get("restarted")
    return va, ((vb - va) if windowed else vb), windowed


def _p22_stale_retries(c, th):
    """T22.4 の戻す条件 (1): `stale_retries` が `stale_hits` の 1% 未満 (以上なら keep-warm の既定を戻す)。"""
    share = th["stale_retry_share"]
    label = (f"`dns.stale_retries` が `stale_hits` の {share * 100:.0f}% 未満 "
             "(古い答えで繋いで失敗し、引き直して繋ぎ直した回。T22.4 の戻す条件)")
    limit = f"< {share * 100:.0f}%"
    _, hits, windowed = dns_counter(c, "stale_hits")
    _, retries, _ = dns_counter(c, "stale_retries")
    secs = (c["dns"] or {}).get("warm_secs")
    if hits is None or retries is None:
        return (label, limit, "—", UNKNOWN,
                "後の雪像の `/status` の `dns` に `stale_hits` / `stale_retries` が無い (T21.3 より前の版)")
    src = (f"`/status` の `dns.stale_hits` / `stale_retries` ({'その間' if windowed else '起動から'})、"
           f"keep-warm の窓 `warm_secs` {n(secs)}")
    if not hits:
        return (label, limit, f"`stale_hits` 0、`stale_retries` {n(retries)}",
                MET if not retries else MISSED, src + "。古い答えを 1 度も使っていない")
    v = retries / hits
    return (label, limit, f"**{v * 100:.2f}%** ({n(retries)} / {n(hits)})",
            MET if v < share else MISSED, src)


def _p22_connect_errors(c, th):
    """T22.4 の戻す条件 (2): 接続のエラー (`refused`・`unreachable`・`timeout`) の**接続 1 本あたり**が前の期間の 1.5 倍未満。

    件/時ではなく接続 1 本あたりで比べる (親の判断 2026-10-17。量が 12 倍になる tokyo では件/時は悪化で
    なくても当たる)。母数は `/history` の `connects` (バースト込み)。件/時は表示だけ。前の期間に接続が
    無ければ「判定できず」、前が 0 なら後も 0 のときだけ満たした。
    """
    k, causes = th["connect_error_factor"], th["connect_error_causes"]
    names = "・".join(f"`{x}`" for x in causes)
    label = f"接続のエラー ({names}) の接続 1 本あたりが前の期間の {k} 倍未満 (T22.4 の戻す条件)"
    limit = f"< 前 ×{k} (接続 1 本あたり)"
    hist = c["hist"] or {}
    bef, aft, interval = hist.get("before_all"), hist.get("after_all"), hist.get("interval_secs")
    if not bef or not aft or not bef.get("samples") or not aft.get("samples"):
        return (label, limit, "—", UNKNOWN, "`/history` に前か後の期間の標本が無い")
    idx = [CAUSE_NAMES.index(x) for x in causes]

    def counts(agg):
        cs = agg.get("causes") or [0] * len(CAUSE_NAMES)
        return [cs[i] if i < len(cs) else 0 for i in idx]
    cb, ca = counts(bef), counts(aft)
    b_rate, a_rate = per_hour(bef, interval, sum(cb)), per_hour(aft, interval, sum(ca))
    src = (f"`/history?res={hist.get('res')}` の `errors_by_cause` ÷ `connects` (前 {bef['samples']} / 後 "
           f"{aft['samples']} 標本、バースト込み)。件/時は表示だけ")
    if b_rate is None or a_rate is None:
        return (label, limit, "—", UNKNOWN, "`/history` の `errors_by_cause` が読めない")
    by = "、".join(f"`{x}` {n(v)}" for x, v in zip(causes, ca))
    hourly = f"件/時 {a_rate:,.2f} (前 {b_rate:,.2f})"
    if not bef.get("connects") or not aft.get("connects"):
        side = "前" if not bef.get("connects") else "後"
        return (label, limit, f"{n(sum(ca))} 件 ({by})、{hourly}", UNKNOWN,
                f"{side}の期間に接続が無いので 1 本あたりが出ない。" + src)
    pb, pa = sum(cb) / bef["connects"], sum(ca) / aft["connects"]
    shown = (f"**{pa:.4f}** / 接続 ({n(sum(ca))} 件 / {n(aft['connects'])} 本: {by}。"
             f"前 {pb:.4f} = {n(sum(cb))} 件 / {n(bef['connects'])} 本"
             + (f"、{pa / pb:.2f} 倍" if pb else "") + f")、{hourly}")
    ok = pa < pb * k if pb else pa == 0
    return (label, limit, shown, MET if ok else MISSED, src)


def counter_rates(c, key, sub=None):
    """`dns_counter` の前と後を 1 時間あたりにする (前は前の雪像の起動から、後はその間か起動から)。

    返すのは `(前 回/時, 後 回/時)`。時間か欄が無ければその側は None。
    """
    va, vb, windowed = dns_counter(c, key, sub)
    ua = (c["a"].get("uptime_secs") or 0) / 3600.0
    ub = ((c["info"] or {}).get("wall_secs") if windowed else c["b"].get("uptime_secs")) or 0
    ub /= 3600.0
    return ((va / ua) if va is not None and ua else None,
            (vb / ub) if vb is not None and ub else None)


def _p22_refresh_and_stale_misses(c, th):
    """T22.4 の段 1 の目安: 引き直しの回/時が前の半分以下、かつ `expired` + `warm_stale` の増えが 1 日 10 回以内。"""
    k, grow = th["refresh_factor"], th["stale_miss_growth_per_day"]
    label = (f"裏の引き直しの回/時が前の期間の {k} 倍以下、かつ `expired` + `warm_stale` のミスの増えが "
             f"1 日 {grow} 回以内 (T22.4 の目安)")
    limit = f"≤ 前 ×{k} かつ 増え ≤ {grow} 回/日"
    rb, ra = counter_rates(c, "refreshes")
    rates = [counter_rates(c, "misses_by_kind", kind) for kind in ("expired", "warm_stale")]
    src = ("`/status` の `dns.refreshes` と `dns.misses_by_kind` (前は前の雪像の起動から、後は"
           + ("その間" if dns_counter(c, "refreshes")[2] else "後の雪像の起動から") + "の 1 時間あたり)")
    if rb is None or ra is None:
        return (label, limit, "—", UNKNOWN, "前か後の `/status` に `dns.refreshes` か `uptime_secs` が無い")
    shown = f"引き直し **{ra:,.1f}** 回/時 (前 {rb:,.1f}" + (f"、{ra / rb:.2f} 倍)" if rb else ")")
    if any(b is None or a is None for b, a in rates):
        return (label, limit, shown, UNKNOWN,
                "前か後の `/status` の `dns` に `misses_by_kind` が無い。" + src)
    mb_, ma_ = sum(b for b, _ in rates) * 24, sum(a for _, a in rates) * 24
    shown += f"、`expired` + `warm_stale` **{ma_:,.1f}** 回/日 (前 {mb_:,.1f}、増え {ma_ - mb_:+,.1f})"
    ok = (ra <= rb * k if rb else ra == 0) and ma_ - mb_ <= grow
    return (label, limit, shown, MET if ok else MISSED, src)


# ---------------------------------------------------------------- (23) 無言の再起動

# `start` の文面 (`crates/metrics-recent/src/events.rs` の "version {} on port {} …") から版を抜く
START_VERSION = re.compile(r"^version\s+(\S+)")

# `start` の文面に載っている `boot` の印 (T24.0)。`crates/run/src/lib.rs` が
# `/proc/sys/kernel/random/boot_id` の**先頭 12 文字**を "… on port 60357 boot 7aa666f1-264 (profile …)" と書く
# (1 件 128 B の枠に収めるため。全体は `/status` の `boot.id`)。
# 手で組んだ雪像や将来の版が全体 (36 文字) を書いても読めるように、6〜36 文字を受ける。
START_BOOT = re.compile(r"\bboot\s+([0-9a-fA-F]{6,36})\b")


def start_version(e):
    """`start` の 1 行から版を抜く (文面に無ければ空文字。`snapshot-anonymize` の `9****28` もそのまま)。"""
    m = START_VERSION.match((e or {}).get("text") or "")
    return m.group(1) if m else ""


def start_boot(e):
    """`start` の 1 行から `boot` (ホストの起動ごとに変わる印) を抜く。

    T24.0 より前の版が書いた行には無いので `None` (その行は「同じホストか」を判定できない)。
    先頭 12 文字だけでも**同じ起動かどうか**は分かる (36 文字の UUID の先頭が 32 ビット以上ある)。
    """
    m = START_BOOT.search((e or {}).get("text") or "")
    return m.group(1) if m else None


def silent_restarts(snap):
    """`/events` の `start` のうち、直前に `shutdown` が無い起動 (= 無言の再起動) を返す (T23.2)。

    2026-10-17 22:14:52Z の再起動は `shutdown` の行を残しておらず、heap の行が黙って
    「判定できず」になった (T22.99)。`/events` は 24 時間より長く残るので、`start` の直前が
    `shutdown` かどうかで見分けられる。**気づけないまま 24 時間を捨てるのを防ぐ計器**。

    見方 (ここが本体。要点と判定表の両方がこの 1 つを使う):
      - **起動の行は文面が `version …` で始まる `start` だけ**。`start` の種類は自己ベンチの要約にも
        使われていて (`PROXY_SELF_BENCH=on` で起動の 3 秒後に 1 件)、それを起動と数えると
        「直前に `shutdown` が無い」と誤って言う。数えなかった件数は `other_starts` に残す。
      - `at` が**その `start` より前**の最後の出来事が `shutdown` なら、普通の再起動
        (同じ秒の出来事は「直前」と見ない。起動の瞬間に `anomaly` が同じ `at` で載る)。
      - 直前の出来事が `shutdown` でなければ**無言の再起動**。前の `start` からの時間
        (`prev_hours`) を添える。前の起動の後に `shutdown` が残っていれば `downtime` (止まっていた秒数)、
        残っていなければ None (落とされたので分からない)。
      - **状態ファイルから読み戻した古い `start` (`restored`) も、同じ規則で見る**。読み戻した
        `shutdown` があるので、版を上げただけの再起動は無言にならない。逆に、窓の**先頭**の `start`
        (それより前の出来事が 1 つも無い) や、窓が起動の途中から始まっている `start` は
        「無言だった」とは言えないので `edge` に分ける (数えない)。
        2026-10-20 の雪像では、10-08 09:24:20Z の `start` (読み戻しの先頭) がここに入る。

    返すのは次の辞書。`/events` の部が無ければ None (呼ぶ側が「判定できず」と書く)。
      `starts` は起動の `start` の行の数 (自己ベンチの `start` は `other_starts`)、
      `silent` / `graceful` / `edge` はそれぞれの内訳、`host` / `process` / `boot_unknown` は
      **無言の再起動の切り分け** (T24.0。下の 3 つ)、
      `hours` は起動からの時間、`kept` / `count` / `capacity` / `restored` / `truncated` は `/events` の欄。

    **ホストの再起動か、プロセスだけの再起動か** (T24.0): 無言の再起動の 1 件ずつについて、
    その `start` の文面の `boot` (T24.0 から載る) と**1 つ前の起動の `boot`** を比べる。
      - `host` … `boot` が変わっている = **ホスト (カーネル) が起動し直した** (2026-10-21 の 2 回。
        利用者に聞いてホストの管理者によるものと分かった)。**こちらでは防げない**。
      - `process` … `boot` が同じ = **プロセスだけが起動し直した** (落ちた・外から止められた)。
        これが「こちらが原因の無言の再起動」で、0 回であるべきもの。
      - `boot_unknown` … どちらかの行に `boot` が無い (T24.0 より前の版が書いた行)。
        判定できないので `host` / `process` のどちらにも数えない。
    """
    ev = part(snap, "events")
    rows = ev.get("events")
    if not isinstance(rows, list):
        return None
    marks = sorted((e for e in rows if isinstance(e, dict) and e.get("at")), key=lambda e: e["at"])
    up = snap.get("uptime_secs") or 0
    out = {
        "starts": 0, "silent": [], "graceful": [], "edge": [], "other_starts": 0,
        "hours": (up / 3600.0) if up else None,
        "count": ev.get("count"), "kept": ev.get("kept", len(rows)),
        "capacity": ev.get("capacity"), "restored": ev.get("restored"),
        "truncated": bool(ev.get("truncated")),
        "oldest": marks[0]["at"] if marks else None,
        "started_at": ((snap.get("taken_at") or 0) - up) if (snap.get("taken_at") and up) else None,
        # 無言の再起動の切り分け (T24.0。上を参照)
        "host": [], "process": [], "boot_unknown": 0,
    }
    prev = None            # 直前に見た起動の `start` (無言かどうかを言うには、これが要る)
    prev_boot = None       # その `start` の `boot` (1 つ前の起動のホスト)
    for e in marks:
        if e.get("kind") != "start":
            continue
        # **起動の `start` は文面が `version …` で始まる行だけ**。`start` の種類は自己ベンチの
        # 要約にも使われており (`PROXY_SELF_BENCH=on` で起動の 3 秒後に 1 件)、それを起動と
        # 数えると「直前に `shutdown` が無い」と誤って言う
        if not START_VERSION.match(e.get("text") or ""):
            out["other_starts"] += 1
            continue
        out["starts"] += 1
        at = e["at"]
        boot = start_boot(e)
        item = {"at": at, "version": start_version(e), "boot": boot}
        # **同じ秒の出来事は「直前」と見ない** (`at` が strictly 小さいものだけ)
        earlier = [m for m in marks if m["at"] < at]
        if not earlier:
            out["edge"].append(dict(item, why="窓の先頭"))
        elif earlier[-1].get("kind") == "shutdown":
            out["graceful"].append(dict(item, downtime=at - earlier[-1]["at"]))
        elif prev is None:
            # 窓が起動の途中から始まっている (`start` が読み戻しの先頭より前にある)
            out["edge"].append(dict(item, why="窓が起動の途中から始まっている"))
        else:
            shut = [m for m in earlier if m.get("kind") == "shutdown"]
            shut = shut[-1] if shut else None
            item = dict(
                item, prev_at=prev["at"], prev_version=start_version(prev),
                prev_hours=(at - prev["at"]) / 3600.0,
                shutdown_at=(shut["at"] if shut else None),
                # 前の起動より**後**に `shutdown` が残っているときだけ「止まっていた秒数」が言える
                downtime=((at - shut["at"]) if (shut and shut["at"] > prev["at"]) else None),
                # 1 つ前の起動の `boot` (T24.0)。無ければ「同じホストか」を言えない
                prev_boot=prev_boot,
            )
            if boot and prev_boot:
                (out["process"] if boot == prev_boot else out["host"]).append(item)
            else:
                out["boot_unknown"] += 1
            out["silent"].append(item)
        prev = e
        prev_boot = boot
    return out


def boot_split_text(r):
    """無言の再起動のうち、**ホストの再起動**と**プロセスだけの再起動**の内訳を 1 文にする (T24.0)。

    **`boot` を持つ行が 1 つも無ければ何も足さない** (T24.0 より前の版が書いた `start` だけの
    雪像)。過去の phase の出力を 1 文字も変えないための線で、`boot_unknown` だけの雪像
    (切り分けられる回が 1 つも無い) でも文は変えず、判定表の「根拠」に件数を書く。
    """
    host = len(r.get("host") or [])
    proc = len(r.get("process") or [])
    unknown = r.get("boot_unknown") or 0
    if not (host or proc):
        return ""
    text = f"。うちホストの再起動 (boot が変わった回) {host} 回"
    text += f"、プロセスだけの再起動 {proc} 回"
    if unknown:
        text += f"、`boot` の無い `start` {unknown} 件は判定できない"
    return text


def silent_restart_text(r, bold=True):
    """`silent_restarts()` の結果を 1 文にする (要点の 1 行と判定表の「実測」で同じ文を使う)。"""
    if r is None:
        return "判定できず (`/events` の部が無い)"
    if not r["starts"]:
        return "判定できず (`/events` に `start` の行が無い)"
    star = "**" if bold else ""

    def one(s):
        t = f"{star}{stamp(s['at'])}{star}"
        if s.get("version"):
            t += f" 版 `{s['version']}`"
        t += f"、前の起動は {s['prev_hours']:,.1f} 時間"
        if s.get("downtime") is not None:
            t += f" (止まっていた {s['downtime']:,} 秒)"
        return t

    if r["silent"]:
        text = f"{len(r['silent'])} 回 (" + "、".join(one(s) for s in r["silent"]) + ")"
        # 無言の再起動があるときは、窓の先頭の `start` が別に残っていることだけ添える
        if r["edge"]:
            text += f"。ほかに窓の先頭などの `start` {len(r['edge'])} 件は判定できない"
        # ホストの再起動かプロセスだけの再起動か (T24.0。`boot` の無い雪像では何も足さない)
        text += boot_split_text(r)
        return text
    if not r["graceful"]:
        # 直前を見られる `start` が 1 つも無い。**0 件とは書かない** (見ていないだけ)
        return f"判定できず (`start` {r['starts']} 件とも窓の先頭で、直前に出来事が無い)"
    # 何を見て 0 と言っているかを添える (窓の先頭の `start` は判定できない)
    basis = [f"直前が `shutdown` の起動 {len(r['graceful'])} 件"]
    if r["edge"]:
        basis.append(f"窓の先頭など {len(r['edge'])} 件は判定できない")
    return "0 件 (" + "、".join(basis) + ")"


def _p23_silent_restarts(c, th):
    """T23.2: 無言の再起動 (`/events` の `start` の直前に `shutdown` が無い起動) が 0 回。

    **判定表の行** (phase23 の `RULES` には T23.1 が入れる。ここでは行を作るだけで `RULES` は触らない)。
    """
    label = "`/events` の `start` の直前に `shutdown` が無い起動 (無言の再起動) が 0 回"
    limit = "0 回"
    r = silent_restarts(c["b"])
    shown = silent_restart_text(r)
    if r is None:
        return (label, limit, shown, UNKNOWN, "後の雪像に `/events` の部が無い")
    src = (f"`/events` の `start` と `shutdown` を時刻順に並べ、その `start` より前の最後の出来事が "
           f"`shutdown` でないものを数えた (覚えている {n(r['kept'])} 件"
           + (f" / 全 {n(r['count'])} 件" if r["count"] not in (None, r["kept"]) else "")
           + (f"、うち読み戻し {n(r['restored'])} 件" if r["restored"] is not None else "")
           + f"、起動からの "
           + (f"{r['hours']:,.1f} 時間)" if r["hours"] else "時間は不明)"))
    if r["silent"]:
        src += "。**`shutdown` を残さずに起動し直している** (落とされたか、止め方が違う)"
        if all(s["downtime"] is None for s in r["silent"]):
            src += "。前の起動より後の `shutdown` が無いので、止まっていた秒数は分からない"
    elif not r["graceful"]:
        src += "。判定できる `start` が無い (窓の先頭の `start` は直前に何も無いので言えない)"
    if r["edge"]:
        src += ("。判定できない `start` "
                + "、".join(f"`{stamp(e['at'])}` ({e['why']})" for e in r["edge"][:3])
                + (f" ほか {len(r['edge']) - 3} 件" if len(r["edge"]) > 3 else ""))
    if r["truncated"]:
        src += "。**`/events` が切れている** (件数は下限)"
    if r["silent"]:
        verdict = MISSED
    elif r["graceful"]:
        verdict = MET
    else:
        verdict = UNKNOWN
    return (label, limit, shown, verdict, src)


# --- Phase 24 (T24.0 の 1 行。phase23 の 16 行 + この行が phase24 の形になる = T24.2) ---

def _p24_silent_restarts(c, th):
    """T24.0: **こちらが原因の**無言の再起動 (同じ `boot` のまま起動し直した回) が 0 回。

    phase23 の行 (`_p23_silent_restarts`) は「`shutdown` を残さずに起動し直した」を数えるだけで、
    ホストの管理者が機械ごと起動し直した回 (2026-10-21 の 2 回) も同じ 1 回に数えていた。
    ここは `start` の文面の `boot` を 1 つ前の起動と比べ、**変わっていればホストの再起動**
    (こちらでは防げない。理由つきで別に書く)、**同じならプロセスだけの再起動** (落ちた・
    外から止められた = こちらが原因) として、後者だけを 0 回の判定に使う。

    `RULES` には T24.2 が入れる (このタスクでは行を作るところまで)。
    """
    label = "こちらが原因の無言の再起動 (同じ `boot` のまま起動し直した回) が 0 回"
    limit = "0 回"
    r = silent_restarts(c["b"])
    shown = silent_restart_text(r)
    if r is None:
        return (label, limit, shown, UNKNOWN, "後の雪像に `/events` の部が無い")
    src = (f"`/events` の `start` の文面の `boot` (`/proc/sys/kernel/random/boot_id` の先頭) を"
           f" 1 つ前の起動と比べた (覚えている {n(r['kept'])} 件"
           + (f" / 全 {n(r['count'])} 件" if r["count"] not in (None, r["kept"]) else "")
           + (f"、うち読み戻し {n(r['restored'])} 件" if r["restored"] is not None else "")
           + "、起動からの "
           + (f"{r['hours']:,.1f} 時間)" if r["hours"] else "時間は不明)"))
    host, proc, unknown = r["host"], r["process"], r["boot_unknown"]
    if host:
        src += ("。**ホストの再起動** (boot が変わった) "
                + "、".join(f"`{stamp(s['at'])}`" for s in host[:4])
                + (f" ほか {len(host) - 4} 件" if len(host) > 4 else "")
                + " は数から外した (ホストの管理者によるもの。こちらでは防げない)")
    if proc:
        src += ("。**同じ `boot` のまま起動し直している** "
                + "、".join(f"`{stamp(s['at'])}` (前の起動は {s['prev_hours']:,.1f} 時間)" for s in proc[:4])
                + " — 落ちたか、外から止められた")
    if unknown:
        src += (f"。`boot` の無い `start` が {unknown} 件ある (T24.0 より前の版が書いた行)。"
                "その回は同じホストかを言えない")
    if not (host or proc or unknown) and not r["silent"]:
        src += ("。無言の再起動は 0 回 (直前に `shutdown` がある起動 "
                f"{len(r['graceful'])} 件、判定できる `start` が無ければ「判定できず」)")
    if r["truncated"]:
        src += "。**`/events` が切れている** (件数は下限)"
    if proc:
        verdict = MISSED
    elif unknown or (r["silent"] and not host) or not (r["graceful"] or r["silent"]):
        verdict = UNKNOWN
    else:
        verdict = MET
    return (label, limit, shown, verdict, src)


# --- Phase 23 (T23.1。phase22 の 14 行のうち、エラーの行の分母と heap の「判定できず」の書き方を直したもの) ---

def recent_attempts(snap, causes):
    """`/recent` の connect の 1 行を 1 試行として、試行・`dns` の失敗・接続の失敗を数える (T23.0 の (2))。

    phase22 の「接続 1 本あたり」の分母は `/history` の `connects` で、宛先の生きた割合 (使われ方) が
    変わると振れる (T22.99 の 7.80 倍は 1 本の接続元が死んだメールサーバーを総当たりしただけ)。
    ここは**名前解決に失敗した行 (`reason == "error:dns"`) を分母から外した**「名前解決に成功した
    試行あたり」を出す。`recent` の行は接続の個票なので、試行 = 接続の試行。

    返すのは `{"attempts", "dns", "denom", "conn", "by", "from", "until", "rows"}`。`conn` は `causes` の
    失敗の数で、`by` は原因ごとの内訳。**`/recent` の部が無ければ None** (「0 だった」と読まない)。
    """
    rows = part(snap, "recent").get("recent")
    if not isinstance(rows, list):
        return None
    attempts = dns = conn = 0
    ats, by = [], {}
    for r in rows:
        if not isinstance(r, dict) or (r.get("kind") or "connect") != "connect":
            continue
        attempts += 1
        if r.get("at"):
            ats.append(r["at"])
        reason = r.get("reason") or ""
        cause = reason[len("error:"):] if reason.startswith("error:") else None
        if cause == "dns":
            dns += 1
        elif cause in causes:
            conn += 1
            by[cause] = by.get(cause, 0) + 1
    return {"attempts": attempts, "dns": dns, "denom": attempts - dns, "conn": conn,
            "by": by, "from": min(ats) if ats else None,
            "until": max(ats) if ats else None, "rows": len(rows)}


def _p23_connect_errors(c, th):
    """T23.1 (T23.0 の (2)): 接続のエラー (`refused`・`unreachable`・`timeout`) の
    **名前解決に成功した試行あたり**が前の期間の 1.5 倍未満 (T22.4 の戻す条件)。

    phase22 の行 (`_p22_connect_errors`) は分母が `connects` なので、死んだ名前の総当たりのような
    使われ方の変化で 7.80 倍に振れた (T22.99)。ここは `/recent` の窓の 1 行を 1 試行とし、`dns` の
    失敗を分母から外す。**`/recent` は 2,000 件の環状なので、窓を必ず書く** (前と後で長さが違う)。
    """
    k, causes = th["connect_error_factor"], th["connect_error_causes"]
    names = "・".join(f"`{x}`" for x in causes)
    label = (f"接続のエラー ({names}) の**名前解決に成功した試行あたり**が前の期間の {k} 倍未満 "
             "(T22.4 の戻す条件を、使われ方から切り離した分母で見る)")
    limit = f"< 前 ×{k} (名前解決に成功した試行あたり)"
    bef, aft = recent_attempts(c["a"], causes), recent_attempts(c["b"], causes)
    if bef is None or aft is None:
        side = "前" if bef is None else "後"
        return (label, limit, "—", UNKNOWN, f"{side}の雪像に `/recent` の部が無い")
    if not bef["attempts"] or not aft["attempts"]:
        side = "前" if not bef["attempts"] else "後"
        return (label, limit, f"試行 0 本 (`/recent` {n(aft['rows'])} 行)",
                UNKNOWN, f"{side}の `/recent` に connect の行が無い (0 と書かない)")

    def window(v):
        return (f"{stamp(v['from'])}〜{stamp(v['until'])} の {n(v['attempts'])} 試行"
                if v["from"] else f"{n(v['attempts'])} 試行 (時刻なし)")
    src = (f"`/recent` の connect の行を 1 試行とし、`reason == \"error:dns\"` を分母から外した "
           f"(後: {window(aft)}、前: {window(bef)})")
    if not aft["denom"] or not bef["denom"]:
        side = "前" if not bef["denom"] else "後"
        return (label, limit, f"試行 {n(aft['attempts'])} の全部が dns の失敗", UNKNOWN,
                f"{side}の期間に名前解決へ成功した試行が無いので率が出ない。" + src)
    by = "、".join(f"`{x}` {n(aft['by'].get(x, 0))}" for x in causes)
    pa, pb = aft["conn"] / aft["denom"], bef["conn"] / bef["denom"]
    shown = (f"**{pa:.4f}** / 名前解決に成功した試行 ({n(aft['conn'])} 件 / {n(aft['denom'])} 本: {by}。"
             f"試行 {n(aft['attempts'])} から dns の失敗 {n(aft['dns'])} を外した分母。"
             f"前 {pb:.4f} = {n(bef['conn'])} 件 / {n(bef['denom'])} 本"
             + (f"、{pa / pb:.2f} 倍)" if pb else ")"))
    ok = pa < pb * k if pb else pa == 0
    return (label, limit, shown, MET if ok else MISSED, src)


_p22_connect_errors_shown = display_only(
    _p22_connect_errors, lambda th: "接続のエラー (`refused`・`unreachable`・`timeout`) の接続 1 本あたり",
    "分母が `/history` の `connects` なので、宛先の生きた割合 (使われ方) で振れる (T22.99 は 7.80 倍。"
    "1 本の接続元が死んだメールサーバーを総当たりしただけ)。**判定は上の「名前解決に成功した試行あたり」"
    "の行に任せる**", was="phase22")


def _p23_miss_band_measured(c, th):
    """phase21 のミスの行に、`/recent` の「試行あたり」と「名前解決に成功した試行あたり」を並べる (T23.1)。

    平常時のミス (回/接続) は `/history` の要求あたりで、これも使われ方で動く (表示だけ)。
    同じ雪像の `/recent` から、dns のミスを**試行あたり**と**名前解決に成功した試行あたり**でも出す。
    2026-10-20 の雪像では 0.0850 (170 / 2,000) と 0.0929 (170 / 1,830) で、これは死んだ名前の
    総当たりという使われ方の数字。**閾は置かない** (表示だけ)。
    """
    label, limit, shown, verdict, src = _p21_miss_band_measured(c, th)
    causes = th["connect_error_causes"]
    aft, bef = recent_attempts(c["b"], causes), recent_attempts(c["a"], causes)
    if not aft or not aft["attempts"]:
        return (label, limit, f"{shown}。`/recent` が無い (か connect の行が 0) ので試行あたりは出せない",
                verdict, src)

    def one(v, bold):
        star = "**" if bold else ""
        t = f"試行あたり {star}{v['dns'] / v['attempts']:.4f}{star} ({n(v['dns'])} / {n(v['attempts'])})"
        if v["denom"]:
            t += (f"、名前解決に成功した試行あたり {star}{v['dns'] / v['denom']:.4f}{star} "
                  f"({n(v['dns'])} / {n(v['denom'])})")
        return t
    text = one(aft, True) + (f" (前: {one(bef, False)})" if bef and bef["attempts"] else "")
    window = (f"{stamp(aft['from'])}〜{stamp(aft['until'])}" if aft["from"] else "時刻なし")
    return (label, limit, f"{shown}。dns のミスの率 (`/recent` の {window}): {text}",
            verdict, src + "、`/recent` の connect の行 (dns の失敗 ÷ 試行 と ÷ 名前解決に成功した試行)")


_p23_miss_band = display_only(
    _p23_miss_band_measured,
    lambda th: "平常時の名前解決のミス (回/接続。`/recent` の試行あたり・名前解決に成功した試行あたりも並べる)",
    "宛先の数で動く使われ方の数字 (T20.99 は宛先 395 名前で 0.12、`cold` 408 回。10-20 は死んだ名前の"
    "総当たりで 0.0850)", was="phase20")


def run_start(snap):
    """その雪像の「いまの起動」の始まり (T23.1。heap の行の理由に書く)。

    `/events` の `start` (文面が `version …` で始まる行 = 本物の起動。T23.2) のうち、
    `taken_at − uptime_secs` にいちばん近いもの (120 秒以内) を採る。返すのは
    `{"at", "version", "from_events"}`。`/events` に無ければ `taken_at − uptime_secs` を使い、
    `from_events` を False にする (推測を `/events` の事実と混ぜない)。どちらも出せなければ None。
    """
    taken, up = snap.get("taken_at"), snap.get("uptime_secs")
    guess = (taken - up) if (taken and up) else None
    r = silent_restarts(snap)
    best = None
    if r:
        for s in r["silent"] + r["graceful"] + r["edge"]:
            if guess is None or abs(s["at"] - guess) <= 120:
                best = s
    if best:
        return {"at": best["at"], "version": best.get("version") or None, "from_events": True}
    if guess is None:
        return None
    return {"at": guess, "version": snap.get("version"), "from_events": False}


def two_starts(x, y):
    """2 つの起動の時刻を「2026-10-17T03:02:22Z と 22:14:52Z」の形にする (同じ日なら 2 つ目は時刻だけ)。"""
    def iso(t):
        return datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    if x is None or y is None:
        return None
    return f"{iso(x)} と {iso(y)[11:]}" if iso(x)[:10] == iso(y)[:10] else f"{iso(x)} と {iso(y)}"


def _p23_heap(c, th):
    """T23.1: phase20 の (g) に、**0 時間の雪像と起動が違う理由** (`/events` の `start` の時刻) を足す。

    T22.99 は 2026-10-17T22:14:52Z の無言の再起動 (T23.2) で 0 時間の雪像と起動が食い違い、
    heap の行が黙って「判定できず」になった。同じ「判定できず」でも、**2 つの起動がいつ始まったか**が
    読めれば、次の再デプロイで同じことを繰り返さない。読み方 (`cache_memory` を引く・`--zero` が
    要る・`heap_free` と `rss` は表示だけ) は phase20 のまま。
    """
    label, limit, shown, verdict, src = _p20_heap(c, th)
    z = c["b"].get("zero_snapshot")
    if verdict != UNKNOWN or not z:
        return (label, limit, shown, verdict, src)
    between = restart_info(z, c["b"])
    if not between["restarted"]:
        return (label, limit, shown, verdict, src)
    said = "、".join(between["reasons"])
    zs, bs = run_start(z), run_start(c["b"])
    pair = two_starts(zs["at"] if zs else None, bs["at"] if bs else None)
    if not zs or not bs or not pair:
        return (label, limit, f"—。**0 時間の雪像と起動が違う** ({said})。`/events` の `start` の時刻は出せない",
                UNKNOWN, f"{src}。{said}")
    # 版が同じなら「同じ版で起動し直している」= 落とされたか、止め方が違う (T23.2 の無言の再起動)
    same = zs["version"] and bs["version"] and zs["version"] == bs["version"]
    if same:
        how = f"同じ版 `{zs['version']}` で起動し直している"
    elif zs["version"] and bs["version"]:
        how = f"版が違う (`{zs['version']}` → `{bs['version']}`)"
    else:
        how = "版は片方しか読めない"
    src2 = (f"{src}。" + ("起動の時刻はどちらも `/events` の `start` から"
                         if zs["from_events"] and bs["from_events"]
                         else "片方は `/events` に `start` が残っていないので、起動の時刻は取得時刻 − 起動からの値")
            + "。この行は 0 時間の雪像と後の雪像が同じ起動でないと読めない")
    return (label, limit,
            f"—。**0 時間の雪像と起動が違う** (`/events` の `start` が {pair}、{how})",
            UNKNOWN, src2)


def _p24_heap(c, th):
    """T24.4: (g) を、**起動から 60 秒たった時点の基準** (`/status` の `memory.baseline`) との差で読む。

    **判定表の行** (phase24 の `RULES` には T24.2 が入れる。ここでは行を作るだけで `RULES` は触らない)。

    状態ファイル (`.rrd`) に、起動から 60 秒たった時点の `heap_used + mmap − cache_memory` が
    1 本だけ残るようになった (`memory.baseline` = `{at, bytes}`。T24.4)。これがある雪像は
    **0 時間の雪像 (`--zero`) が無くても**「この起動でどれだけ増えたか」が読める
    (ホストの管理者に起動し直されても、起動ごとの基準なので判定できる)。

    `baseline` が無い雪像 (T24.4 より前の版・起動から 60 秒以内・`mallinfo2` が読めない環境) は
    **phase23 の行をそのまま返す** (0 時間の雪像との差、無ければ「判定できず」と理由)。
    基準は 2 つ混ぜない — どちらで判定したかが読めなくなる。
    """
    b = c["b"]
    mem = memory_of(b)
    base = mem.get("baseline")
    if not isinstance(base, dict) or base.get("bytes") is None:
        return _p23_heap(c, th)
    lim = th["heap_growth_mb"]
    label = (f"`heap_used + mmap − cache_memory` の起動から 60 秒の基準からの増えが "
             f"{lim:.0f} MB 未満")
    limit = f"< {lim:.0f} MB"
    at = base.get("at")
    taken, up = b.get("taken_at"), b.get("uptime_secs")
    start = (taken - up) if (taken and up is not None) else None
    # 雪像の `taken_at` と `uptime_secs` はどちらも整数秒だが、念のため切り捨ててから書く
    when = (f"{stamp(at)}" + (f" (起動から {n(int(at - start))} 秒)" if (at and start) else ""))
    shown = "、".join(f"`{k}` {mb(mem.get(k))}"
                     for k in ("heap_used", "mmap", "cache_memory", "heap_free", "rss"))
    shown += f" MB (`heap_free` と `rss` は表示だけ)。基準は {mb(base['bytes'])} MB ({when})"
    missing = [f"`{k}`" for k in ("heap_used", "mmap", "cache_memory") if mem.get(k) is None]
    if missing:
        return (label, limit, shown, UNKNOWN,
                "、".join(missing) + " が無い (`memory.baseline` はある)")
    # 基準がこの起動のものか。`at` が起動の始まりより前なら前の起動が残した値
    # (状態ファイルは起動をまたいで残る)。時計の切り捨てのぶんだけ許す
    if at is not None and start is not None and at < start - 5:
        return (label, limit, shown, UNKNOWN,
                f"`memory.baseline` の `at` ({stamp(at)}) がこの起動の始まり "
                f"({stamp(start)}) より前 = 前の起動が残した基準")
    now = mem["heap_used"] + mem["mmap"] - mem["cache_memory"]
    grown = (now - base["bytes"]) / 1e6
    src = (f"後の雪像 (起動から {uptime_text(up)}) の `/status` の `memory.baseline` "
           f"= 状態ファイルに起動 60 秒で 1 回だけ書いた `heap_used + mmap − cache_memory`。"
           f"**0 時間の雪像 (`--zero`) は要らない**")
    return (label, limit, f"**{grown:+.1f}** MB ({mb(base['bytes'])} → {mb(now)} MB)。" + shown,
            MET if grown < lim else MISSED, src)


RULES = {
    "phase14": (_p14_dns_per_connect, _p14_major_hosts, _p14_connect_p50, _p14_overload),
    "phase15": (_p15_watch_host, _p15_refresh_rate, _p15_miss_band,
                _p15_conn_cores, _p15_closed_shape, _p15_timeout),
    "phase17": (_p15_watch_host, _p15_refresh_rate, _p15_miss_band, _p15_timeout,
                _p17_conn_per_request, _p17_warm_max, _p17_events, _p17_cgroup),
}
# phase18 = phase17 の 8 行 + (g) + (h)
RULES["phase18"] = RULES["phase17"] + (_p18_heap, _p18_request_probes)
# phase20 = phase18 と同じ並びの 10 行。3 行が表示だけ、(g) がキャッシュを引く版、残りの 6 行はそのまま
RULES["phase20"] = (_p20_watch_host, _p15_refresh_rate, _p15_miss_band, _p20_timeout,
                    _p17_conn_per_request, _p20_warm_max, _p17_events, _p17_cgroup,
                    _p20_heap, _p18_request_probes)
# phase21 = phase20 と同じ並びの 10 行。ミスの行が表示だけ、conn 役の行が短い材料で参考、IPv6 の行が起動直後を許す
RULES["phase21"] = (_p20_watch_host, _p15_refresh_rate, _p21_miss_band, _p20_timeout,
                    _p21_conn_per_request, _p20_warm_max, _p17_events, _p17_cgroup,
                    _p20_heap, _p21_request_probes)
_p22_events = display_only(
    _p17_events, lambda th: "`/events` の anomaly の種類別 件/時 (`dns_slow` を含めて表示だけ)",
    "`dns_slow` の判定は新しい閾の行 (合計 1,000 ms 未満の窓で立った件数) に任せる。T22.2 の模型で tokyo の後は"
    "本当に遅い窓だけで 0.17 件/時あり、件/時の閾は使われ方で落ちる (親の判断 2026-10-17)", was="phase21")
# phase22 = phase21 の 10 行 (引き直しの行は warm 0 でも読める版、`/events` の行は表示だけ) + T22.2 の 1 行 + T22.4 の 3 行。
# 平常時の切り方も違う (`PHASE22` の `burst_median_k`)。T22.3 は段 1 で止めたので行を作らない
RULES["phase22"] = ((RULES["phase21"][0], _p22_refresh_rate) + RULES["phase21"][2:6] + (_p22_events,)
                    + RULES["phase21"][7:]
                    + (_p22_dns_slow, _p22_stale_retries, _p22_connect_errors,
                       _p22_refresh_and_stale_misses))
# phase23 = phase22 の 14 行を土台に、4 か所だけ違う (T23.1)。並びは phase22 と同じで、
# ミスの行 (2 番目)・heap の行 (9 番目)・接続のエラーの行 (13 番目) を差し替え、
# phase22 の接続のエラーの行をそのすぐ後ろに**表示だけ**で残し、最後に無言の再起動の行 (T23.2) を足す
RULES["phase23"] = ((RULES["phase22"][:2] + (_p23_miss_band,) + RULES["phase22"][3:8]
                     + (_p23_heap,) + RULES["phase22"][9:12]
                     + (_p23_connect_errors, _p22_connect_errors_shown)
                     + RULES["phase22"][13:] + (_p23_silent_restarts,)))
# T24.4: phase24 の heap の行は `_p24_heap` (起動から 60 秒の基準 `memory.baseline` があればそれとの差、
# 無ければ `_p23_heap` をそのまま返す)。**`RULES["phase24"]` は T24.2 が組む** ので、ここでは触らない
# (`_p23_silent_restarts` を T23.2 が作って T23.1 が入れたのと同じ形)。差し替えるのは 9 番目。


def judge(name, hist, hosts, majors, status_b, overload, th,
          a=None, b=None, info=None, dns=None, errors=None, burst=BURST_PER_HOUR):
    """`RULES[name]` の各行を順に呼んで判定表にする。

    `a` / `b` (雪像そのもの) と `info` / `dns` / `errors` は **phase15 / phase17 の行だけが使う**
    (phase14 の 4 行は今までどおりの 7 つの引数だけで足りる)。
    """
    info = info or {}
    hours = (info.get("uptime") or [0, 0])[1] if info.get("restarted") else info.get("wall_secs")
    c = {
        "hist": hist or {}, "after": (hist or {}).get("after") or {},
        "after_all": (hist or {}).get("after_all") or {},
        "hosts": hosts, "majors": majors, "status_b": status_b, "overload": overload,
        "a": a or {}, "b": b or {}, "info": info, "dns": dns, "errors": errors,
        # 平常時の閾 (1 時間の本数)。閉じ方の行が分ごとの標本をこれで切る
        "burst": burst,
        # `dns.refreshes` は再起動をまたぐと「起動から」の値なので、割る時間もそちらに合わせる
        "hours": (hours or 0) / 3600.0,
    }
    rows = [rule(c, th) for rule in RULES[name]]
    out = {"name": name, "rows": rows,
           "tally": {v: sum(1 for r in rows if r[3] == v) for v in (MET, MISSED, UNKNOWN)}}
    # 表示だけの行 (T20.1) は集計に数えない。あるときだけ本数を別の鍵で持つ (古い定義の形は変えない)
    shown = sum(1 for r in rows if r[3] == SHOWN)
    if shown:
        out["display_only"] = shown
    # 参考の行 (T21.2) も同じく集計に数えず、あるときだけ本数を別の鍵で持つ
    reference = sum(1 for r in rows if r[3] == REFERENCE)
    if reference:
        out["reference"] = reference
    return out


# ---------------------------------------------------------------- 判定表の印字

def render_criteria(d, grouped):
    """判定表 (`## 9.`) を印字する (`snapshot-diff.py` の `render` が `"criteria" in d` のときに呼ぶ)。

    `grouped` は `--group domain` が付いているか (「主要 3 ホスト」の行の単位が変わる)。
    """
    p = print
    c = d["criteria"]
    p(f"## 9. 完了の定義に対する判定 (`--criteria {c['name']}`)")
    p()
    if grouped:
        # 「主要 3 ホスト」は §3 の上位から採るので、まとめると**単位**になる
        p("**`--group domain` を付けているので、「主要 3 ホスト」の行はまとめの単位 "
          "(eTLD+1) で判定している**。ホスト 1 件ずつで判定するには `--group` を外すこと。")
        p()
    if c["name"] == "phase15":
        # T15.0 (15)。材料が雪像に無い行は「0 だった」ではなく「判定できず」にする
        p("材料は `/hosts` `/status` の `dns` `/dns` (名前ごとの引き直し)・"
          "`/history` (`dns_warm` と `errors_by_cause`、`res=60` の `closed` / `transfer`)・"
          "`/profile` (`conn` 役の CPU)・`--daily` (日ごとのミス) です。"
          "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"
          "閉じ方の行は**平常時の 1 分ごとの全数**で比べます (`/recent` は 256 KiB で切れて"
          "窓の長さが毎回違うので使いません。T15.15)。")
        p()
    if c["name"] == "phase17":
        # T17.0a。前の 4 行は phase15 と同じ関数、後ろの 4 行が T16.99 で手で引いていた判定
        p("前の 4 行は phase15 と同じ物差しです。後ろの 4 行の材料は `--profile` "
          "(`/profile?res=60`。無ければ雪像の `/profile` の部で**参考**)・`/history` の "
          "`dns_warm_max` と `/status` の `dns.warm_evicted`・`/events` の anomaly "
          "(起動からの 件/時、`cleared:` は数えない)・`/status` の "
          "`kernel.cgroup_cpu.since_start` です。**その部が雪像に無い行は「判定できず」**で、"
          "0 とは書きません。")
        p()
    if c["name"] == "phase18":
        # T18.2。前の 8 行は phase17 と同じ関数、後ろの 2 行が T17.99 の但し書き (d)(e) の物差し
        p("前の 8 行は phase17 と同じ物差しで、材料も同じです (`--daily` `--profile` "
          "`--profile-before`)。後ろの 2 行は T18.0 の (g) と (h) で、材料は `--zero` で渡す "
          "0 時間の雪像と後の雪像の `/status` の `memory` (`heap_used + mmap` の増え。"
          "`heap_free` と `rss` は表示だけ) と、後の雪像の `/status` の "
          "`ipv6.request_probes` (T18.1。0 でなければ `canary.ipv6_runs` / `ipv6_skipped` から"
          "理由を読む) です。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase20":
        # T20.1。phase18 と同じ並びの 10 行で、物差しを 2 つ直してある
        p("phase18 と同じ並びの 10 行で、材料も同じです (`--daily` `--profile` `--profile-before` "
          "`--zero`)。直したのは 2 つ (T20.1): `heap_used + mmap` の増えは**メモリのキャッシュ "
          "(`memory.cache_memory`) を引いてから**比べます (キャッシュは上限まで使ってよい作りなので、"
          "入ったぶんは漏れではありません。欄が無い版は 0 とせず「判定できず」)。"
          "ミス率 (`watch_host`)・`timeout`・`dns_warm` の最大と `warm_evicted` の 3 行は"
          "**表示だけ**で、閾を置かず、下の集計にも数えません (使われ方で動く数字なので)。"
          "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase21":
        # T21.2。phase20 と同じ並びの 10 行で、T20.99 で届かなかった 3 行の読み方を直してある
        p("phase20 と同じ並びの 10 行で、材料も同じです (`--daily` `--profile` `--profile-before` "
          "`--zero`)。直したのは 3 行 (T21.2): 平常時の名前解決のミスは**表示だけ**で、日ごとの幅と"
          "ミスの種類別の内訳 (`cold` / `expired` …) を並べます (宛先の数で動く使われ方の数字なので)。"
          "`conn` 役の CPU/要求 は、前か後の `/profile` が 12 時間 (`/profile?res=60` で 720 標本) に"
          "満たなければ**参考**で、倍率は出しますが集計に数えません。`ipv6.request_probes` は、"
          "最後に探った時刻が 0 時間の雪像の取得から 3 時間以内なら「満たした」(起動直後は canary が "
          "IPv6 をまだ見ていない) で、それより後にも探っていれば「届かず」です。表示だけの行は"
          "ほかに 3 つ (phase20 と同じ) です。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase22":
        # T22.1。phase21 の 10 行 + 22a の 4 行で、平常時の切り方も違う
        p("前の 10 行は phase21 と同じ物差しで、材料も同じです (`--daily` `--profile` `--profile-before` "
          "`--zero`)。裏の引き直しの行だけ、keep-warm を止めた版 (T22.4。warm 0) では warm の平均で割る"
          "参考の値の代わりに通算の 1 時間あたりを並べます。`/events` の行は `dns_slow` を含めて**表示だけ**です "
          "(`dns_slow` の判定は下の新しい閾の行に任せる)。後ろの 4 行は 22a で段 2 まで行ったもの: "
          "`dns_slow` が窓のミスの合計 1,000 ms 未満の窓で立った件数 (0 なら満たした。新しい閾 (T22.2) での件/時は"
          "表示だけ)、T22.4 で keep-warm の既定を戻す 2 つの条件 (`stale_retries` が "
          "`stale_hits` の 1% 以上、接続のエラー `refused`・`unreachable`・`timeout` の**接続 1 本あたり**が前の "
          "1.5 倍以上。"
          "当たれば「届かず」)、T22.4 の目安 (引き直しの回/時が前の半分以下、`expired` + `warm_stale` の"
          "増えが 1 日 10 回以内) です。**平常時は「300 本と、その期間の 1 時間の本数の中央値の 3 倍の"
          "大きい方」未満の標本**です (§2。T22.1)。T22.3 は段 1 で止めたので行がありません。"
          "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase23":
        # T23.1。phase22 の 14 行を土台に、エラーの行の分母と heap の「判定できず」の書き方を直した
        p("前の 14 行は phase22 と同じ物差しで、材料も同じです (`--daily` `--profile` `--profile-before` "
          "`--zero`)。直したのは 4 か所 (T23.1): (1) 接続のエラー `refused`・`unreachable`・`timeout` の行は、"
          "**`/recent` の connect の 1 行を 1 試行とし、名前解決に失敗した行 (`error:dns`) を分母から外した**"
          "「名前解決に成功した試行あたり」で見ます (前の期間の 1.5 倍以上なら「届かず」)。"
          "分母が `/history` の `connects` だった phase22 の行は、そのすぐ後ろに**表示だけ**で残しました "
          "(使われ方で振れるため。T22.99 の 7.80 倍)。(2) 平常時の名前解決のミスの行は表示だけのままで、"
          "同じ雪像の `/recent` から**試行あたり**と**名前解決に成功した試行あたり**の率も並べます "
          "(死んだ名前の総当たりという使われ方の数字なので閾は置きません)。(3) heap の行は、`--zero` の雪像と"
          "後の雪像の起動が違うときに `/events` の `start` の時刻を添えます (黙って「判定できず」にしない)。"
          "(4) 最後の行は**無言の再起動** (`/events` の `start` の直前に `shutdown` が無い起動) が 0 回 (T23.2)。"
          "**`/recent` は 2,000 件の環状なので、窓と試行の本数を必ず書きます** (前と後で長さが違います)。"
          "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    p("| 完了の定義 | 閾値 | 実測 (後の期間) | 判定 | 出どころ |")
    p("|---|---|---|---|---|")
    for row in c["rows"]:
        p(f"| {row[0]} | {row[1]} | {row[2]} | **{row[3]}** | {row[4]} |")
    p()
    # 集計に数えない行 (表示だけ・参考) はあるときだけ添える。phase20 の文字は変わらない
    others = [f"{name} {c[key]} 行" for key, name in (("display_only", SHOWN), ("reference", REFERENCE))
              if c.get(key)]
    p("・".join(f"{k} {v} 行" for k, v in c["tally"].items() if v)
      + (f" (ほかに{'・'.join(others)}。集計に数えない)" if others else ""))
    p()
