# 判定表 (`scripts/snapshot_criteria.py`) の **Phase 23〜25 の行** (T34.127 で割った)。
#
# phase23 で直した接続のエラー (名前解決に成功した試行あたり)・ミスの行・heap の行と無言の再起動の行、
# phase24 の無言の再起動・heap (起動 60 秒の基準から)・確立の尾 (RTT + N ms) の行、phase25 で読み方を
# 直した 3 行と、その材料を読む小物 (`/recent` の試行・`/events` の `start`)。並べるのは `RULES`。

from datetime import datetime, timezone

from .common import MET, MISSED, UNKNOWN, ms, n, part, stamp
from .phase14_18 import mb, memory_of, uptime_text
from .phase20_22 import (
    _p20_heap, _p21_miss_band_measured, _p22_connect_errors, counter_rates, display_only, dns_counter,
)
from .restarts import boot_split_text, restart_info, silent_restart_text, silent_restarts


# --- Phase 23 (T23.1。phase22 の 14 行のうち、エラーの行の分母と heap の「判定できず」の書き方を直したもの) ---

# `/recent` の理由のうち名前解決の失敗 (`error:` の後ろ)。`dns` は T25.0 より前の綴り (T29.36)
DNS_FAILURES = ("dns", "nxdomain", "dns_temp")


def recent_attempts(snap, causes):
    """`/recent` の connect の 1 行を 1 試行として、試行・`dns` の失敗・接続の失敗を数える (T23.0)。

    phase22 の「接続 1 本あたり」の分母は `/history` の `connects` で、宛先の生きた割合 (使われ方) が
    変わると振れる (T22.99 の 7.80 倍は 1 本の接続元が死んだメールサーバーを総当たりしただけ)。
    ここは**名前解決に失敗した行 (`error:dns`・`error:nxdomain`・`error:dns_temp`) を分母から外した**「名前解決に成功した
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
        # T25.0 からは名前解決の失敗を `error:nxdomain` / `error:dns_temp` と綴る (T29.36。今まで
        # `error:dns` だけを外していたので、phase23 以降の分母に名前解決の失敗が混ざっていた)
        if cause in DNS_FAILURES:
            dns += 1
        elif cause in causes:
            conn += 1
            by[cause] = by.get(cause, 0) + 1
    return {"attempts": attempts, "dns": dns, "denom": attempts - dns, "conn": conn,
            "by": by, "from": min(ats) if ats else None,
            "until": max(ats) if ats else None, "rows": len(rows)}


def _p23_connect_errors(c, th):
    """T23.1 (T23.0): 接続のエラー (`refused`・`unreachable`・`timeout`) の
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
    """T23.1: phase20 の `heap_used + mmap − cache_memory` の行に、**0 時間の雪像と起動が違う理由** (`/events` の `start` の時刻) を足す。

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
           + "、起動からの "
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


# --- T24.3: 確立の尾を「そのときのホストの RTT + N ms」で読む (phase24 の 17 番目の行) ---
#
# T14.99 は「6 ms のような固定値ではなく、そのときのホストの RTT + N ms で書く」と決めたが、
# 判定表はまだ数値 (`connect_p50`) を並べるだけだった (2026-10-22 の確立 p95 は 318.5 ms で、
# 中身は遠い相手の connect)。**phase14 の 4 行は 1 文字も変えない**ので、行はここに足し、
# `RULES["phase24"]` の末尾にだけ並べる (`RULES["phase14"]` には入れない)。

def weighted_quantile(pairs, q):
    """`(値, 重み)` の重み付き分位点 (T24.3)。重みは**その行が運んだ要求の数**。

    重みの合計を 1 本の列と見て、累積が `q × 合計` に届いた所の値を返す (最近接)。
    重みが 0 以下の組は数えない (`/recent` の connect の行は `reqs` が 0 = CONNECT は
    HTTP の要求ではないので、呼ぶ側が 1 に置き換える)。組が無ければ None。
    """
    pairs = [(v, w) for v, w in pairs if v is not None and w > 0]
    if not pairs:
        return None
    pairs.sort()
    total = sum(w for _, w in pairs)
    seen = 0
    for v, w in pairs:
        seen += w
        if seen >= q * total:
            return v
    return pairs[-1][0]


def recent_connect_rtt(snap):
    """`/recent` の connect の行から、**確立した接続だけ**の `(確立 ms, RTT ms, 重み)` を集める (T24.3)。

    `rtt_ms.origin` はカーネルが測ったオリジン側の RTT (`/recent` の 1 行 = 1 接続) なので、
    **同じ行の `ms.connect` と対にする**と母集団を混ぜずに「確立 − RTT」が出せる。

    外す行: `reason` が `error:` で始まる行 (確立していない。`error:dns` は
    `connect_ms = total_ms − dns_ms` なので 0 に落ちる。T15.0) と、`rtt_ms.origin` が
    `null` の行 (Linux 以外・`--lite`・閉じるのが早すぎた)。外した数も返す。

    返すのは `{"pairs", "rows", "connect", "failed", "no_rtt", "no_ms", "from", "until", "ats"}`。
    **`/recent` の部が無ければ None** (「0 だった」と読まない)。
    """
    part_ = snap.get("recent")
    rows = part_.get("recent") if isinstance(part_, dict) else None
    if not isinstance(rows, list):
        return None
    out = {"pairs": [], "rows": len(rows), "connect": 0, "failed": 0, "no_rtt": 0,
           "no_ms": 0, "ats": []}
    for r in rows:
        if not isinstance(r, dict) or (r.get("kind") or "connect") != "connect":
            continue
        out["connect"] += 1
        if (r.get("reason") or "").startswith("error:"):
            out["failed"] += 1
            continue
        ms_ = (r.get("ms") or {}).get("connect")
        rtt = (r.get("rtt_ms") or {}).get("origin")
        if ms_ is None:
            out["no_ms"] += 1
            continue
        if rtt is None:
            out["no_rtt"] += 1
            continue
        # 重みは**その行が運んだ要求の数** (「要求で重み付け」)。connect の行は `reqs` が 0 なので 1
        out["pairs"].append((float(ms_), float(rtt), max(int(r.get("reqs") or 0), 1)))
        if r.get("at"):
            out["ats"].append(r["at"])
    out["from"] = min(out["ats"]) if out["ats"] else None
    out["until"] = max(out["ats"]) if out["ats"] else None
    return out


def _p24_connect_tail(c, th):
    """T24.3: 確立の**尾**が「そのときのホストの RTT + N ms」に収まるかで読む (T14.99 の決まり)。

    `/recent` の connect の行で `ms.connect` と `rtt_ms.origin` を**同じ行で対にし**、その差の
    分位点を見る。差の中央値が「プロキシの取り分」(phase14 は −0.21 ms)、差の p95 が「尾」で、
    `N` (5 ms) 未満なら尾は**距離** (相手までの RTT) で説明できる。

    **2 つの分位点の差では見ない**。`p50(確立) − p50(RTT)` は母数の違う行を混ぜる (同じ行の差の
    中央値とは一致しない) ので、取り分も尾も差の分位点で書く。`/recent` は 2,000 件の環状なので、
    **窓と本数を必ず書く**。
    """
    need = th["connect_tail_ms"]
    label = (f"CONNECT 確立の尾 (`p95`) が、同じ窓のカーネル RTT の `p95` + {need:.0f} ms 未満 "
             "(尾は距離)")
    limit = f"< RTT p95 + {need:.0f} ms"
    w = recent_connect_rtt(c["b"])
    if w is None:
        return (label, limit, "—", UNKNOWN, "後の雪像に `/recent` の部が無い")
    if not w["connect"]:
        return (label, limit, f"connect の行 0 本 (`/recent` {n(w['rows'])} 行)", UNKNOWN,
                "後の雪像の `/recent` に connect の行が無い (0 と書かない)")
    if not w["pairs"]:
        return (label, limit, f"対にできる行 0 本 (connect {n(w['connect'])} 本)", UNKNOWN,
                f"確立と RTT がそろった行が無い (失敗 {n(w['failed'])} 本、`rtt_ms.origin` が "
                f"`null` {n(w['no_rtt'])} 本)")
    pairs = [(m - r, wt) for m, r, wt in w["pairs"]]
    share = weighted_quantile(pairs, 0.5)
    tail = weighted_quantile(pairs, 0.95)
    p50c = weighted_quantile([(m, wt) for m, _, wt in w["pairs"]], 0.5)
    p95c = weighted_quantile([(m, wt) for m, _, wt in w["pairs"]], 0.95)
    p50r = weighted_quantile([(r, wt) for _, r, wt in w["pairs"]], 0.5)
    p95r = weighted_quantile([(r, wt) for _, r, wt in w["pairs"]], 0.95)
    distance = tail < need
    dropped = []
    if w["failed"]:
        dropped.append(f"失敗 {n(w['failed'])} 本")
    if w["no_rtt"]:
        dropped.append(f"`rtt_ms.origin` が `null` の {n(w['no_rtt'])} 本")
    if w["no_ms"]:
        dropped.append(f"`ms.connect` の無い {n(w['no_ms'])} 本")
    shown = (f"**{tail:+.1f}** ms (同じ {n(len(pairs))} 本の `ms.connect − rtt_ms.origin` の p95。"
             f"確立 p50 {ms(p50c)} / p95 {ms(p95c)} ms、RTT p50 {ms(p50r)} / p95 {ms(p95r)} ms)"
             f"。プロキシの取り分 (中央値) **{share:+.1f}** ms"
             f" = **{'距離' if distance else '距離では説明できない'}**"
             f"。`/recent` の {stamp(w['from'])}〜{stamp(w['until'])} の connect {n(w['connect'])} 本"
             f" (確立と RTT がそろったのは {n(len(pairs))} 本"
             + (f"。外した: {'、'.join(dropped)}" if dropped else "")
             + ")")
    src = ("`/recent` の connect の行の `ms.connect` と `rtt_ms.origin` を**同じ行で対にした**差 "
           "(T14.99 の「そのときのホストの RTT + N ms」)。取り分は差の中央値、尾は差の p95 で、"
           "**2 つの分位点の差ではない** (母数の違う行を混ぜない)。重みは行ごとの `reqs` "
           "(connect の行は 0 = CONNECT は HTTP の要求ではないので 1)。"
           "**`/recent` は 2,000 件の環状なので、窓と本数を書く**")
    hist = c["after"].get("connect_p95")
    if hist is not None:
        src += (f"。`/history` の平常時の確立 p95 ({ms(hist)} ms) とは母集団が違う "
                "(バーストを外した 1 時間の標本) ので、この行では見比べない")
    return (label, limit, shown, MET if distance else MISSED, src)


# --- Phase 25 (T27.0。phase24 の 17 行のうち、T25.99 で物差しの穴だった 3 行を直したもの) ---

def restarts_since(r, since):
    """`silent_restarts()` の結果から、**`since` (前の雪像の取得) より後の `start` だけ**を残す (T27.0)。

    それより前の起動は前の締めで見ている。`boot` の無い古い `start` (T24.0 より前の版の行) は
    `/events` の 512 件の環に何日も残るので、窓を切らないと、その間の締めはずっと「判定できず」になる
    (T25.99)。切った件数は `before` に残す (出どころに書く)。`since` が無ければ `r` をそのまま返す。
    """
    if r is None or not since:
        return r
    keep = lambda xs: [x for x in xs if (x.get("at") or 0) > since]
    out = dict(r)
    for key in ("silent", "graceful", "edge", "host", "process"):
        out[key] = keep(r[key])
    out["boot_unknown"] = len(out["silent"]) - len(out["host"]) - len(out["process"])
    out["starts"] = len(out["silent"]) + len(out["graceful"]) + len(out["edge"])
    out["before"] = r["starts"] - out["starts"]
    out["since"] = since
    return out


def _p25_silent_restarts(c, th):
    """T27.0: **前の雪像より後の**起動で、こちらが原因の無言の再起動 (同じ `boot` のまま) が 0 回。

    読み方は phase24 の行 (`_p24_silent_restarts`) と同じで、数える `start` を前の雪像の取得より後に
    切るところだけが違う。その間に起動が 1 つも無い (同じプロセスの 2 枚) なら、後の雪像の起動時刻が
    前の雪像より前であることを確かめて「満たした」にする。後の起動が窓の中なのに `start` の行が
    見つからないとき (環から落ちた) は「判定できず」。
    """
    label = "前の雪像より後の起動で、こちらが原因の無言の再起動 (同じ `boot` のまま起動し直した回) が 0 回"
    limit = "0 回"
    since = c["a"].get("taken_at")
    full = silent_restarts(c["b"])
    if full is None:
        return (label, limit, "判定できず (`/events` の部が無い)", UNKNOWN, "後の雪像に `/events` の部が無い")
    if not since:
        return (label, limit, "—", UNKNOWN, "前の雪像に `taken_at` が無いので窓を切れない")
    r = restarts_since(full, since)
    src = (f"`/events` の `start` のうち、前の雪像の取得 ({stamp(since)}) より後の {n(r['starts'])} 件を見た "
           f"(それより前の {n(r['before'])} 件は前の締めで見たので数えない)。`boot` の比べ方は phase24 と同じ")
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
        src += f"。`boot` の無い `start` が {unknown} 件ある (T24.0 より前の版が書いた行)。その回は同じホストかを言えない"
    if r["truncated"]:
        src += "。**`/events` が切れている** (件数は下限)"
    started = full["started_at"]
    if not r["starts"]:
        # 窓の中に起動が無い = 2 枚は同じプロセス。後の起動が前の雪像より前なら、それで言い切れる
        if started and started <= since:
            return (label, limit, f"0 件 (前の雪像より後に起動なし。後の起動は {stamp(started)})", MET, src)
        return (label, limit, "判定できず (後の起動が窓の中なのに `start` の行が見つからない)", UNKNOWN,
                src + (f"。`/events` の最古は {stamp(full['oldest'])}" if full["oldest"] else ""))
    if r["silent"]:
        shown = f"{len(r['silent'])} 回 (" + "、".join(
            f"**{stamp(s['at'])}** 版 `{s.get('version') or '?'}`、前の起動は {s['prev_hours']:,.1f} 時間"
            for s in r["silent"]) + ")" + boot_split_text(r)
    else:
        shown = f"0 件 (直前が `shutdown` の起動 {len(r['graceful'])} 件" + (
            f"、窓の先頭など {len(r['edge'])} 件は判定できない" if r["edge"] else "") + ")"
    if proc:
        verdict = MISSED
    elif unknown or (r["silent"] and not host) or not (r["graceful"] or r["silent"]):
        verdict = UNKNOWN
    else:
        verdict = MET
    return (label, limit, shown, verdict, src)


def _p25_connect_errors(c, th):
    """T27.0: phase23 の接続のエラーの行 (`_p23_connect_errors`) に、**試行の本数の下限**を足したもの。

    前か後の `/recent` の connect の試行が `min_recent_attempts` に満たなければ「判定できず」。
    T25.99 の前はサーバーの日次の雪像 (00:00 UTC に撮る) で、`/recent` は 1 時間 581 本・接続のエラー 0 件
    だった。前が 0 件だと後が 1 件でも「届かず」になる (後の 15 件はどれもメール宛の総当たり)。
    """
    label, limit, shown, verdict, src = _p23_connect_errors(c, th)
    floor = th["min_recent_attempts"]
    if verdict not in (MET, MISSED):
        return (label, limit, shown, verdict, src)
    short = [(side, v["attempts"]) for side, v in
             (("前", recent_attempts(c["a"], th["connect_error_causes"])),
              ("後", recent_attempts(c["b"], th["connect_error_causes"])))
             if v["attempts"] < floor]
    if short:
        why = "、".join(f"{side}の試行 {n(k)} 本" for side, k in short)
        return (label, limit + f"。前も後も試行 {n(floor)} 本以上", shown, UNKNOWN,
                f"{why}が {n(floor)} 本に満たないので比べない (T25.99 の前は 1 時間 581 本で 0 件)。" + src)
    return (label, limit + f"。前も後も試行 {n(floor)} 本以上", shown, verdict, src)


def _p25_refresh_per_name(c, th):
    """T27.0: 1 名前あたりの裏の引き直し (回/時) が前の期間の 1.5 倍未満。

    phase22 の T22.4 の目安 (`_p22_refresh_and_stale_misses`) は「keep-warm を止めたら引き直しが半分以下」で、
    止めた版どうし (0.22.6 以降) を比べると意味が無い (T25.99 は 1.06 倍で「届かず」)。通算の回/時は
    名前の数 (宛先の数 = 使われ方) で動くので、`/status` の `dns.entries` (表の名前の数) で割る。
    通算の回/時と `expired` + `warm_stale` の回/日は表示だけで並べる。
    """
    k = th["refresh_per_name_factor"]
    label = f"1 名前あたりの裏の引き直し (回/時) が前の期間の {k} 倍未満 (通算の回/時と `expired` + `warm_stale` は表示だけ)"
    limit = f"< 前 ×{k} (1 名前あたり)"
    rb, ra = counter_rates(c, "refreshes")
    eb = (part(c["a"], "status").get("dns") or {}).get("entries")
    ea = (part(c["b"], "status").get("dns") or {}).get("entries")
    src = ("`/status` の `dns.refreshes` を 1 時間あたりにし (前は前の雪像の起動から、後は"
           + ("その間" if dns_counter(c, "refreshes")[2] else "後の雪像の起動から")
           + ")、`dns.entries` (表の名前の数) で割った")
    if rb is None or ra is None:
        return (label, limit, "—", UNKNOWN, "前か後の `/status` に `dns.refreshes` か `uptime_secs` が無い")
    if not eb or not ea:
        return (label, limit, f"引き直し {ra:,.1f} 回/時 (前 {rb:,.1f})", UNKNOWN,
                "前か後の `/status` に `dns.entries` が無い (0 と書かない)。" + src)
    pb, pa = rb / eb, ra / ea
    shown = (f"**{pa:.2f}** 回/時/名前 (前 {pb:.2f}" + (f"、{pa / pb:.2f} 倍" if pb else "") + ")。"
             f"表示だけ: 通算 {ra:,.1f} 回/時 (前 {rb:,.1f})、名前 {n(ea)} (前 {n(eb)})")
    rates = [counter_rates(c, "misses_by_kind", kind) for kind in ("expired", "warm_stale")]
    if not any(b is None or a is None for b, a in rates):
        mb_, ma_ = sum(b for b, _ in rates) * 24, sum(a for _, a in rates) * 24
        shown += f"、`expired` + `warm_stale` {ma_:,.1f} 回/日 (前 {mb_:,.1f})"
    ok = pa < pb * k if pb else pa == 0
    return (label, limit, shown, MET if ok else MISSED, src)
