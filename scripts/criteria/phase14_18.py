# 判定表 (`scripts/snapshot_criteria.py`) の **Phase 14〜18 の行** (T.34.125 で割った)。
#
# phase14 の 4 行・phase15 の 6 行・phase17 で足した 4 行・phase18 で足した 2 行と、その材料を読む小物
# (`/profile` の役割の CPU・`/history?res=60` の閉じ方・`/dns` の引き直し・`/daily` の幅・メモリ)。
# 1 行 = 1 つの関数で、`(名前, 閾の文, 実測, 判定, 出どころ)` を返す。並べるのは `RULES`。

from proxydata import CAUSE_NAMES

from .common import (
    MAJOR_HOSTS, MET, MISSED, UNKNOWN, change, merged_history, miss_rate, ms, n, part, per_hour, ratio,
    stamp,
)
from .restarts import restart_info


def profile_role_cores(snap, role):
    """`/profile` の標本から**その役割の CPU** を「何コアぶん」で出す (部が無ければ None)。

    1 標本の `threads` は役割ごとに `0` (標本 0) か `[cpu_us, samples, [states...]]`
    (`crates/metrics-profile/src/profile/sample.rs` の `push_row`)。位置ではなく `keys` と
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

    種類は `text` の先頭の `<kind>:` (`crates/metrics-watch/src/anomaly/text.rs` の文面)。
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
