# 判定表 (`scripts/snapshot_criteria.py`) の **Phase 20〜22 の行** (T.34.126 で割った)。
#
# phase20 で直した heap の行と、行を**表示だけ**にする `display_only`・phase21 で読み方を直した 3 行・
# phase22 で足した `dns_slow` と keep-warm を戻す条件の行と、その材料を読む小物
# (`/status` の `dns` の数・`/events` の `dns_slow`・`/profile` の長さ)。並べるのは `RULES`。

import re

from proxydata import CAUSE_NAMES

from .common import MET, MISSED, REFERENCE, SHOWN, UNKNOWN, ms, n, part, per_hour, stamp
from .phase14_18 import (
    _p15_miss_band, _p15_refresh_rate, _p15_timeout, _p15_watch_host, _p17_conn_per_request, _p17_events,
    _p17_warm_max, _p18_request_probes, mb, memory_of, profile_source, uptime_text,
)
from .restarts import restart_info


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


_p22_events = display_only(
    _p17_events, lambda th: "`/events` の anomaly の種類別 件/時 (`dns_slow` を含めて表示だけ)",
    "`dns_slow` の判定は新しい閾の行 (合計 1,000 ms 未満の窓で立った件数) に任せる。T22.2 の模型で tokyo の後は"
    "本当に遅い窓だけで 0.17 件/時あり、件/時の閾は使われ方で落ちる (親の判断 2026-10-17)", was="phase21")
