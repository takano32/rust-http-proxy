# 判定表 (`scripts/snapshot_criteria.py`) の **Phase 26〜28 の行** (T34.128 で割った)。
#
# phase26 で 18 番目から後ろに足した 9 行 (408・`/log` の error・`/errors` の `local`・3 秒で切った確立・
# `rejected_requests`・RSS の返していない分・cgroup の CPU・裏の役割の割合・`/log` の 502 の 1 分あたり)、
# phase27 で読み方を変えた 2 行、phase28 で量に左右されない物差しにした 2 行と、その材料を読む小物。
# 並べるのは `RULES`。

from .common import MET, MISSED, REFERENCE, SHOWN, UNKNOWN, n, part, stamp
from .phase14_18 import cgroup_since_start, mb, memory_of, profile_cost, profile_source, uptime_text
from .phase20_22 import profile_span
from .phase23_25 import DNS_FAILURES, _p23_connect_errors, recent_attempts


# --- Phase 26 (T29.10。phase25 の 17 行 + T28.99 で手で読んだ「ほかに見ること」と T29.99 の見ること) ---
#
# phase25 の 17 行は 1 つも動かさず、**18 番目から後ろに足す**。T28.99 の「ほかに見ること (2)〜(8)」は
# 個票を手で数えていた (408 は数が無かった)。T29.99 の見ること (RSS・CPU) も、雪像で読めるものはここに置く。
# 他人の `Set-Cookie` (T29.0) は雪像に数が無いので行にしない (結合テストで見る)。

def snap_hours(snap):
    """雪像の起動からの時間 (時。`uptime_secs` が無ければ None)。"""
    up = snap.get("uptime_secs")
    return up / 3600.0 if up else None


def per_day(count, hours):
    """起動からの通算 `count` を 24 時間あたりにする (時間が無ければ None)。"""
    return count * 24.0 / hours if hours else None


def recent_window(rows):
    """`/recent` の行の時刻の窓の文言 (`from〜until の N 行`)。"""
    ats = [r.get("at") for r in rows if isinstance(r, dict) and r.get("at")]
    if not ats:
        return f"{n(len(rows))} 行 (時刻なし)"
    return f"{stamp(min(ats))}〜{stamp(max(ats))} の {n(len(rows))} 行"


def _p26_header_timeouts(c, th):
    """T.29.10 (a): 408 (ヘッダーの締め切り) の件数。**表示だけ** (本数は相手が決める)。

    材料は後の雪像の `/status` の `rejected_requests.header_timeout` (起動からの通算) と、`/recent` の
    `error:header_timeout` の行 (うち `reqs` ≥ 1 = 1 要求目を通したあとの keep-alive で止まった接続)。
    T.29.10 より前の版には欄が無いので「判定できず」(0 と書かない)。T.33.0 からは 1 バイトも送らない先張りの接続を
    408 にせず `/status` の `silent_closes` に数えるので、欄があれば同じ行に並べる (無い版は何も書かない)。
    """
    label = "408 (要求行とヘッダーの締め切り。`PROXY_HEADER_TIMEOUT_SECS`) の件数 (24 時間あたり)"
    limit = "— (表示だけ)"
    rej = part(c["b"], "status").get("rejected_requests") or {}
    if "header_timeout" not in rej:
        return (label, limit, "—", UNKNOWN,
                "後の雪像の `rejected_requests` に `header_timeout` が無い (T.29.10 より前の版。数は debug ログだけだった)")
    count, hours = rej["header_timeout"], snap_hours(c["b"])
    day = per_day(count, hours)
    shown = f"**{n(count)}** 件 (起動から {n(hours)} 時間" + (f"、{day:,.1f} 件/24 時間" if day is not None else "") + ")"
    silent = part(c["b"], "status").get("silent_closes")
    if isinstance(silent, int):
        sday = per_day(silent, hours)
        shown += (f"。408 を返さずに黙って閉じた先張りの接続 (`silent_closes`、T.33.0) **{n(silent)}** 本"
                  + (f" ({sday:,.1f} 本/24 時間)" if sday is not None else ""))
    rows = part(c["b"], "recent").get("recent")
    src = "`/status` の `rejected_requests.header_timeout` (起動からの通算)"
    if isinstance(rows, list):
        hit = [r for r in rows if isinstance(r, dict) and r.get("reason") == "error:header_timeout"]
        kept = sum(1 for r in hit if (r.get("reqs") or 0) >= 1)
        shown += f"。`/recent` の `error:header_timeout` {n(len(hit))} 行 (うち 1 要求目を通したあとの {n(kept)} 行)"
        src += (f"と `/recent` ({recent_window(rows)}。宛先も要求も無いまま閉じた接続 = 要求行の前の沈黙は"
                " 個票に残らないので、`/recent` に出るのは要求の途中で止まった接続だけ)")
    return (label, limit, shown, SHOWN,
            src + "。**閾を置かない**: 先張りの接続や走査が来るたびに増え、本数は相手が決める")


def _p26_log_errors(c, th):
    """T.29.10 (b): `/log` の error の行が `log_error_max` 行以下 (T.28.99 の (3)。T.28.4 の効き目)。"""
    k = th["log_error_max"]
    label = f"`/log` の error の行が {k} 行以下 (T.28.4。0.27.8 は 1,000 行中 500)"
    limit = f"≤ {k} 行"
    lg = part(c["b"], "log")
    lines = lg.get("lines")
    if not isinstance(lines, list):
        return (label, limit, "—", UNKNOWN, "後の雪像に `/log` の部が無い")
    errs = [x for x in lines if isinstance(x, dict) and x.get("level") == "error"]
    ats = [x.get("at") for x in lines if isinstance(x, dict) and x.get("at")]
    win = (f"{stamp(min(ats))}〜{stamp(max(ats))} の {n(len(lines))} 行" if ats else f"{n(len(lines))} 行")
    src = f"後の雪像の `/log` ({win}、記録の水準 `{lg.get('level') or '?'}`)"
    if lg.get("truncated"):
        src += "。**`/log` が切れている** (件数は下限)"
    shown = f"**{n(len(errs))}** 行"
    if errs:
        shown += " (" + "、".join(f"`{stamp(x.get('at'))}` {str(x.get('msg') or '')[:80]}" for x in errs[:3]) + (
            f" ほか {len(errs) - 3} 行" if len(errs) > 3 else "") + ")"
    return (label, limit, shown, MET if len(errs) <= k else MISSED, src)


def _p26_local_blocks(c, th):
    """T.29.10 (c): `/errors` の `local` (私設・ループバック宛ての 403。T.28.41) が 0 件 (T.28.99 の (8))。

    0 件でなければ「届かず」にして宛先を並べる (正規の利用を断ったかは人が読む)。
    """
    label = "`/errors` の `local` (私設・ループバック宛ての 403。T.28.41 の既定で断った宛先) が 0 件"
    limit = "0 件"
    rows = part(c["b"], "errors").get("errors")
    if not isinstance(rows, list):
        return (label, limit, "—", UNKNOWN, "後の雪像に `/errors` の部が無い")
    hit = [r for r in rows if isinstance(r, dict) and r.get("cause") == "local"]
    count = sum((r.get("repeats") or 1) for r in hit)
    src = f"後の雪像の `/errors` ({recent_window(rows)}。同じ失敗の繰り返しは `repeats` で足した)"
    if not hit:
        return (label, limit, "**0** 件", MET, src)
    targets = sorted({r.get("target") or "?" for r in hit})
    return (label, limit, f"**{n(count)}** 件 (宛先 " + "、".join(f"`{t}`" for t in targets[:5])
            + (f" ほか {len(targets) - 5}" if len(targets) > 5 else "") + ")", MISSED,
            src + "。正規の利用を断ったかは宛先を見て決める (`PROXY_ALLOW_LOCAL`)")


def _p26_connect_cut(c, th):
    """T.29.10 (d): 3 秒 (`PROXY_CONNECT_TIMEOUT_SECS`) で切った確立と、成功した確立の最大・SYN の再送。**表示だけ**。

    T.28.99 の (7) を `/recent` の connect の行で数える: 確立の締め切りで切った行 (`error:timeout`)、
    成功した行の `ms.connect` の最大と 2 秒以上の本数、`syn_retrans` が 1・2 の本数。
    """
    label = "確立の締め切り (3 秒) で切った CONNECT・成功した確立の最大・`syn_retrans` 1 / 2 の本数"
    limit = "— (表示だけ)"
    rows = part(c["b"], "recent").get("recent")
    if not isinstance(rows, list):
        return (label, limit, "—", UNKNOWN, "後の雪像に `/recent` の部が無い")
    conns = [r for r in rows if isinstance(r, dict) and (r.get("kind") or "connect") == "connect"]
    cut = [r for r in conns if r.get("reason") == "error:timeout"]
    ok = [r for r in conns if not str(r.get("reason") or "").startswith("error:")]
    best = [((r.get("ms") or {}).get("connect") or 0) for r in ok]
    ports = {}
    for r in cut:
        port = str(r.get("target") or "").rpartition(":")[2] or "?"
        ports[port] = ports.get(port, 0) + 1
    syn = {k: sum(1 for r in conns if (r.get("syn_retrans") or 0) == k) for k in (1, 2)}
    shown = (f"切った **{n(len(cut))}** 行"
             + (" (ポート " + "、".join(f"`{p}` {k}" for p, k in sorted(ports.items(), key=lambda x: -x[1])) + ")"
                if cut else "")
             + f"、成功 {n(len(ok))} 本の確立の最大 **{n(max(best)) if best else '—'}** ms"
             + f" (2 秒以上 {n(sum(1 for v in best if v >= 2000))} 本)"
             + f"、`syn_retrans` 1 が {n(syn[1])} 本・2 が {n(syn[2])} 本")
    return (label, limit, shown, SHOWN,
            f"後の雪像の `/recent` の connect の行 ({recent_window(conns)})。**閾を置かない**: "
            "切った行が困ったか (3 秒より長く待てば繋がったか) は切ったあとでは分からない (T.28.99 の (7))")


def _p26_rejected(c, th):
    """T.29.10 (e): `rejected_requests` (読めずに断った要求) の合計と理由別。**表示だけ**。

    T.29.8 (要求行とヘッダー名の文法) の「誤って断った要求 0」は、0 でない理由の `/errors` の行を人が読む。
    """
    label = "`rejected_requests` (読めずに断った要求) の合計と理由別"
    limit = "— (表示だけ)"
    rej = part(c["b"], "status").get("rejected_requests")
    if not isinstance(rej, dict) or "total" not in rej:
        return (label, limit, "—", UNKNOWN, "後の雪像の `/status` に `rejected_requests` が無い")
    nz = [(k, v) for k, v in rej.items() if k != "total" and v]
    hours = snap_hours(c["b"])
    day = per_day(rej["total"], hours)
    shown = (f"**{n(rej['total'])}** 本 (起動から {n(hours)} 時間"
             + (f"、{day:,.1f} 本/24 時間" if day is not None else "") + ")"
             + ("。" + "、".join(f"`{k}` {n(v)}" for k, v in nz) if nz else "。どの理由も 0"))
    return (label, limit, shown, SHOWN,
            "`/status` の `rejected_requests` (起動からの通算。`header_timeout` は T.29.10 から)。**閾を置かない**: "
            "走査の要求の本数は相手が決める。誤って断った形跡は `/errors` の `bad_request:<理由>` の行を見る")


def _p26_rss_gap(c, th):
    """T.29.25: `rss − (heap_used + mmap + cache_memory)` が `rss_gap_mb` 未満 (返していない free heap)。"""
    lim = th["rss_gap_mb"]
    label = f"`rss − (heap_used + mmap + cache_memory)` が {lim:.0f} MB 未満 (T.29.25。`heap_free` は表示だけ)"
    limit = f"< {lim:.0f} MB"
    m = memory_of(c["b"])
    need = ("rss", "heap_used", "mmap", "cache_memory")
    missing = [k for k in need if m.get(k) is None]
    if missing:
        return (label, limit, "—", UNKNOWN, "後の雪像の `/status` の `memory` に " + "、".join(f"`{k}`" for k in missing) + " が無い")
    gap = (m["rss"] - m["heap_used"] - m["mmap"] - m["cache_memory"]) / 1e6
    shown = (f"**{gap:,.1f}** MB (`rss` {mb(m['rss'])} − `heap_used` {mb(m['heap_used'])} − `mmap` {mb(m['mmap'])}"
             f" − `cache_memory` {mb(m['cache_memory'])} MB)")
    if m.get("heap_free") is not None:
        shown += f"。表示だけ: `heap_free` {mb(m['heap_free'])} MB"
    return (label, limit, shown, MET if gap < lim else MISSED,
            f"後の雪像の `/status` の `memory` (起動から {uptime_text(c['b'].get('uptime_secs'))})")


def _p26_cgroup_cpu(c, th):
    """T.29.20〜23: cgroup の起動からの CPU (コア) が前の版より下がる (`_p17_cgroup` は「同じ桁」だけを見る)。"""
    label = "cgroup の起動からの CPU (コア) が前の版より下がる (T.29.20〜23。user / sys は表示だけ)"
    limit = "< 前"
    aft, bef = cgroup_since_start(c["b"]), cgroup_since_start(c["a"])
    if aft is None or bef is None:
        side = "後" if aft is None else "前"
        return (label, limit, "—", UNKNOWN, f"{side}の雪像の `kernel.cgroup_cpu.since_start` に `usage_usec` が無い")

    def text(v):
        parts = [f"{v['cores']:.4f} コア"]
        if v["user_cores"] is not None and v["sys_cores"] is not None:
            parts.append(f"user {v['user_cores']:.4f}・sys {v['sys_cores']:.4f}")
        return "、".join(parts)
    shown = f"{text(bef)} → **{text(aft)}**" + (f" ({aft['cores'] / bef['cores']:.2f} 倍)" if bef["cores"] else "")
    return (label, limit, shown, MET if aft["cores"] < bef["cores"] else MISSED,
            f"`kernel.cgroup_cpu.since_start` ÷ `uptime_secs` (前 {bef['hours']:,.1f} 時間、後 {aft['hours']:,.1f} 時間)")


def role_share(p, role):
    """`/profile` の JSON 1 つで、`role` の CPU がプロセス全体の CPU に占める割合 (無ければ None)。"""
    cost = profile_cost(p, role)
    if not cost or not cost["cores"]:
        return None
    return cost["role_us"] / (cost["cores"] * cost["secs"] * 1e6)


def _p26_role_shares(c, th):
    """T.29.20〜22: 暇なときの裏の費用 (`profile-sample`・`cache-probe`) の CPU の割合が前の版より下がる。

    前か後の `/profile` が `profile_min_hours` に満たなければ「参考」(`_p21_conn_per_request` と同じ扱い)。
    """
    roles = th["idle_roles"]
    label = "暇なときの裏の役割 (" + "・".join(f"`{r}`" for r in roles) + ") の CPU の割合が前の版より下がる (T.29.20〜22)"
    limit = "どれも < 前"
    (pb, sb), (pa, sa) = profile_source(c["a"]), profile_source(c["b"])
    if not pa or not pb:
        side = "後" if not pa else "前"
        return (label, limit, "—", UNKNOWN, f"{side}の雪像に `/profile` の材料が無い (`--profile` / `--profile-before`)")
    got, worse = [], []
    for role in roles:
        b, a = role_share(pb, role), role_share(pa, role)
        if a is None or b is None:
            return (label, limit, "—", UNKNOWN, f"前か後の `/profile` に `{role}` の役割か `cpu_us` が無い")
        got.append(f"`{role}` {b * 100:.1f}% → **{a * 100:.1f}%**")
        if a >= b:
            worse.append(role)
    shown = "、".join(got)
    src = f"`/profile` の役割ごとの `cpu_us` ÷ 全体の `cpu_us` (後: {sa}、前: {sb})"
    verdict = MISSED if worse else MET
    hours = th["profile_min_hours"]
    short = [f"{who}は {s[0] / 3600:,.1f} 時間" for snap, who in ((c["a"], "前"), (c["b"], "後"))
             for s in [profile_span(snap)] if s and s[0] < hours * 3600]
    if short:
        return (label, limit, shown, REFERENCE,
                src + f"。**参考**: {'、'.join(short)} で {hours:.0f} 時間に満たない (判定は「{verdict}」でも集計に数えない)")
    return (label, limit, shown, verdict, src)


# `/log` の行のうち、オリジン側の失敗の WARN (T29.15 で原因ごとに 1 分 1 行に絞った行) の頭
ORIGIN_WARN_PREFIXES = ("502 Bad Gateway", "origin response timed out", "failed to read origin response")


def _p26_origin_warn_per_minute(c, th):
    """T.29.15: `/log` のオリジン側の失敗 (502 など) の WARN が 1 分に `origin_warn_per_minute` 行以下 (原因の数)。

    T.29.15 から WARN は原因 (`errors_by_cause` の 8 つ + 名前解決の内訳 2 つ) ごとに 1 分 1 行で、抑えた本数は
    ` (+N more since the previous line, not logged)` で次の行に添える。0.28.48 は 1,000 行が全部メール宛の 502 だった。
    """
    k = th["origin_warn_per_minute"]
    label = f"`/log` のオリジン側の失敗 (502・時間切れ) の WARN が 1 分に {k} 行以下 (T.29.15。原因ごとに 1 分 1 行)"
    limit = f"≤ {k} 行/分"
    lines = part(c["b"], "log").get("lines")
    if not isinstance(lines, list):
        return (label, limit, "—", UNKNOWN, "後の雪像に `/log` の部が無い")
    hit = [x for x in lines if isinstance(x, dict) and x.get("level") == "warn"
           and str(x.get("msg") or "").startswith(ORIGIN_WARN_PREFIXES)]
    per_min = {}
    for x in hit:
        m = (x.get("at") or 0) // 60
        per_min[m] = per_min.get(m, 0) + 1
    worst = max(per_min.items(), key=lambda kv: kv[1]) if per_min else None
    folded = sum(1 for x in hit if "more since the previous line" in str(x.get("msg") or ""))
    ats = [x.get("at") for x in lines if isinstance(x, dict) and x.get("at")]
    win = (f"{stamp(min(ats))}〜{stamp(max(ats))} の {n(len(lines))} 行" if ats else f"{n(len(lines))} 行")
    shown = (f"{n(len(hit))} 行 / {n(len(lines))} 行"
             + (f"、1 分の最大 **{worst[1]}** 行 ({stamp(worst[0] * 60)})" if worst else "、1 分の最大 **0** 行")
             + f"、抑えた本数を添えた行 {n(folded)}")
    src = f"後の雪像の `/log` ({win}) の warn のうち、頭が " + "・".join(f"`{p}`" for p in ORIGIN_WARN_PREFIXES) + " の行"
    return (label, limit, shown, MET if not worst or worst[1] <= k else MISSED, src)


# --- Phase 27 (T30.0・T30.1。phase26 の 26 行のうち 2 行だけ読み方を変えたもの) ---
#
# T29.99 で届かなかった 2 行は、どちらも物差しの穴だった: 接続のエラーの行は「試行 1,000 本以上」の下限が
# 分母 (名前解決に成功した試行) に掛かっておらず、名前の引けないメール宛の総当たりで分母が 489 本しか
# 残らなかった。RSS の行は手元から置いた 10 MB の閾で、tokyo では前の版も 31.8 MB だった。
def recent_dns_ports(snap, top=3):
    """`/recent` の connect の行のうち、名前解決に失敗した行の宛先のポートの上位 (T.30.0)。"""
    rows = part(snap, "recent").get("recent")
    by = {}
    for r in rows if isinstance(rows, list) else []:
        if not isinstance(r, dict) or (r.get("kind") or "connect") != "connect":
            continue
        reason = r.get("reason") or ""
        if reason.startswith("error:") and reason[len("error:"):] in DNS_FAILURES:
            port = str(r.get("target") or "").rsplit(":", 1)[-1] or "?"
            by[port] = by.get(port, 0) + 1
    return sorted(by.items(), key=lambda kv: (-kv[1], kv[0]))[:top]


def _p27_connect_errors(c, th):
    """T.30.0: phase23 の接続のエラーの行に、**分母 (名前解決に成功した試行) の本数の下限**を足したもの。

    phase25 (`_p25_connect_errors`) の下限は試行の本数に掛かっていて、T.29.99 は試行 1,985 本のうち
    1,496 本が名前の引けないメール宛 (T.29.36 で分母から外す) で分母 489 本、0.0107 → 0.1759 (16.40 倍) で
    「届かず」になった。前か後の分母が `min_resolved_attempts` に満たなければ「判定できず」で、外した
    名前解決の失敗の本数とポートの上位を書く。出どころの文面も今の 3 つの綴りにした (phase26 以前は変えない)。
    """
    label, limit, shown, verdict, src = _p23_connect_errors(c, th)
    src = src.replace("`reason == \"error:dns\"` を分母から外した",
                      "名前解決の失敗 (" + "・".join(f"`error:{x}`" for x in DNS_FAILURES) + ") を分母から外した")
    floor = th["min_resolved_attempts"]
    limit += f"。前も後も名前解決に成功した試行 {n(floor)} 本以上"
    if verdict not in (MET, MISSED):
        return (label, limit, shown, verdict, src)
    causes = th["connect_error_causes"]
    short = []
    for side, snap in (("前", c["a"]), ("後", c["b"])):
        v = recent_attempts(snap, causes)
        if v["denom"] < floor:
            ports = "・".join(f"`:{p}` {n(k)}" for p, k in recent_dns_ports(snap))
            short.append(f"{side}は名前解決に成功した試行 {n(v['denom'])} 本 (試行 {n(v['attempts'])} から"
                         f"名前解決の失敗 {n(v['dns'])} 本を外した" + (f"。失敗の宛先 {ports}" if ports else "") + ")")
    if short:
        return (label, limit, shown, UNKNOWN,
                "、".join(short) + f"で、{n(floor)} 本に満たないので比べない (名前の引けない宛先の総当たりで"
                "分母が小さいと、数件のエラーで倍率が振れる。T.29.99 は分母 489 本で 16.40 倍)。" + src)
    return (label, limit, shown, verdict, src)


def _p27_rss_gap(c, th):
    """T.30.1: `rss − (heap_used + mmap + cache_memory)` が**前の版の同じ値より下がる**。

    phase26 (`_p26_rss_gap`) の 10 MB は手元 (18.9 → 13.4 MiB) から置いた閾で、tokyo ではスレッドの
    スタック (`stacks_estimate` 約 22 MB) と `heap_free` で前の版も 31.8 MB、後 28.9 MB だった。
    内訳 (`heap_free`・`stacks_estimate`・`malloc_trim` の回数と返した量) は表示だけで並べる。
    """
    label = ("`rss − (heap_used + mmap + cache_memory)` が前の版より下がる (T.29.25・T.30.1。"
             "`heap_free`・`stacks_estimate`・`malloc_trim` は表示だけ)")
    limit = "< 前"
    need = ("rss", "heap_used", "mmap", "cache_memory")

    def gap(m):
        return None if any(m.get(k) is None for k in need) else \
            (m["rss"] - m["heap_used"] - m["mmap"] - m["cache_memory"]) / 1e6
    ma, mb_ = memory_of(c["a"]), memory_of(c["b"])
    ga, gb = gap(ma), gap(mb_)
    if gb is None:
        return (label, limit, "—", UNKNOWN, "後の雪像の `/status` の `memory` に " +
                "、".join(f"`{k}`" for k in need if mb_.get(k) is None) + " が無い")
    shown = (f"**{gb:,.1f}** MB (`rss` {mb(mb_['rss'])} − `heap_used` {mb(mb_['heap_used'])} − `mmap` "
             f"{mb(mb_['mmap'])} − `cache_memory` {mb(mb_['cache_memory'])} MB)")
    if ga is not None:
        shown += f"、前 {ga:,.1f} MB (`rss` {mb(ma['rss'])})"
    extra = [f"`{k}` {mb(mb_[k])} MB" for k in ("heap_free", "stacks_estimate") if mb_.get(k) is not None]
    trim = mb_.get("trim")
    if isinstance(trim, dict):
        extra.append("`malloc_trim` %s 回 (返した計 %s MB%s)" % (
            n(trim.get("count") or 0), mb(trim.get("total_returned_bytes") or 0),
            "、呼ばなかった %s 回" % n(trim["skipped"]) if trim.get("skipped") is not None else ""))
    else:
        extra.append("`malloc_trim` の欄が無い版 (T.30.1 より前)")
    shown += "。表示だけ: " + "、".join(extra)
    src = (f"前と後の雪像の `/status` の `memory` (後は起動から {uptime_text(c['b'].get('uptime_secs'))})")
    if ga is None:
        return (label, limit, shown, UNKNOWN, "前の雪像の `/status` の `memory` が欠けているので比べない。" + src)
    return (label, limit, shown, MET if gb < ga else MISSED, src)


# --- Phase 28 (T31.0・T31.1。phase27 の 26 行のうち 2 行だけ、量に左右されない物差しにしたもの) ---
#
# T30.99 で届かなかった 2 行は、どちらも前の版の値と**絶対値**で比べる行で、要求が 1 時間あたり約 1.6 倍・
# 確立の時間切れ 88 件/時 (前 12) の期間に動いた (`conn` 役の CPU/要求は 0.42 倍、`malloc_trim` は毎時動いていた)。
def _p28_cgroup_cpu_per_request(c, th):
    """T.31.0: cgroup の起動からの CPU ÷ 起動からの要求 (`total_requests`) が前の版の `cpu_per_request_factor` 倍未満。

    phase26 (`_p26_cgroup_cpu`) はコアの合計で比べていて、量が 1.6 倍の T.30.99 で 1.09 倍の「届かず」だった。
    コアの合計・user / sys と、暇なときの裏の役割の CPU の割合は表示だけで並べる。
    """
    k = th["cpu_per_request_factor"]
    label = (f"cgroup の起動からの CPU **÷ 起動からの要求**が前の版の {k} 倍未満 (T.31.0。コアの合計・user / sys・"
             "裏の役割の割合は表示だけ)")
    limit = f"< 前 ×{k} (要求 1 本あたり)"
    aft, bef = cgroup_since_start(c["b"]), cgroup_since_start(c["a"])
    if aft is None or bef is None:
        side = "後" if aft is None else "前"
        return (label, limit, "—", UNKNOWN, f"{side}の雪像の `kernel.cgroup_cpu.since_start` に `usage_usec` が無い")
    ra, rb = part(c["b"], "status").get("total_requests"), part(c["a"], "status").get("total_requests")
    if not ra or not rb:
        side = "後" if not ra else "前"
        return (label, limit, f"{bef['cores']:.4f} → {aft['cores']:.4f} コア", UNKNOWN,
                f"{side}の雪像の `/status` に `total_requests` が無い (か 0)。率が出ない")
    # 起動からの CPU (us) ÷ 起動からの要求。`cores` は ÷ uptime なので、uptime を掛け戻す
    ua = aft["cores"] * aft["hours"] * 3600e6 / ra
    ub = bef["cores"] * bef["hours"] * 3600e6 / rb
    rate_a, rate_b = ra / (aft["hours"] or 1), rb / (bef["hours"] or 1)
    shown = (f"**{ua:,.0f}** us/要求 (前 {ub:,.0f}、{ua / ub:.2f} 倍)。表示だけ: コア {bef['cores']:.4f} → "
             f"{aft['cores']:.4f}、要求 {rate_b:,.0f} → {rate_a:,.0f} 本/時 ({rate_a / rate_b:.2f} 倍)")
    if aft["user_cores"] is not None and bef["user_cores"] is not None:
        shown += f"、user {bef['user_cores']:.4f} → {aft['user_cores']:.4f}・sys {bef['sys_cores']:.4f} → {aft['sys_cores']:.4f}"
    shares = []
    for role in th["idle_roles"]:
        (pb, _), (pa, _) = profile_source(c["a"]), profile_source(c["b"])
        sb, sa = (role_share(pb, role) if pb else None), (role_share(pa, role) if pa else None)
        if sa is not None:
            shares.append(f"`{role}` " + (f"{sb * 100:.1f}% → " if sb is not None else "") + f"{sa * 100:.1f}%")
    if shares:
        shown += "、裏の役割の割合 " + "・".join(shares)
    src = (f"`kernel.cgroup_cpu.since_start.usage_usec` ÷ `/status` の `total_requests` (どちらも起動から。"
           f"前 {bef['hours']:,.1f} 時間 {n(rb)} 本、後 {aft['hours']:,.1f} 時間 {n(ra)} 本)")
    return (label, limit, shown, MET if ua < ub * k else MISSED, src)


def _p28_trim_works(c, th):
    """T.31.1: `malloc_trim` が毎時動いて返している (`memory.trim` の `count` ≥ 起動からの時間 − 1、返した量 > 0)。

    phase27 (`_p27_rss_gap`) は `rss − (heap_used + mmap + cache_memory)` を前の版と比べていて、空きの塊と
    スタックの山が量で動いた T.30.99 で 19.5 → 24.2 MB の「届かず」だった (`malloc_trim` は 24 回・71.4 MB を返していた)。
    `rss − (…)` と前の版との差・`heap_free`・`stacks_estimate` は表示だけ。
    """
    label = ("`malloc_trim` が毎時動いて返している (`memory.trim` の `count` ≥ 起動からの時間 − 1 で、返した量 > 0。"
             "T.31.1。`rss − (heap_used + mmap + cache_memory)`・`heap_free`・`stacks_estimate` は表示だけ)")
    limit = "`count` ≥ 時間 − 1 かつ 返した量 > 0"
    m = memory_of(c["b"])
    trim = m.get("trim")
    hours = (c["b"].get("uptime_secs") or 0) / 3600.0
    if not isinstance(trim, dict):
        return (label, limit, "—", UNKNOWN, "後の雪像の `/status` の `memory` に `trim` が無い (T.30.1 より前の版)")
    count, skipped = trim.get("count") or 0, trim.get("skipped") or 0
    total = trim.get("total_returned_bytes") or 0
    need = max(int(hours) - 1, 0)
    shown = (f"**{n(count)}** 回 (呼ばなかった {n(skipped)} 回、起動から {hours:,.1f} 時間)、返した計 **{mb(total)}** MB")
    if trim.get("last_us") is not None:
        shown += f"、最後は {mb(trim.get('last_returned_bytes') or 0)} MB を {n(trim['last_us'])} us"

    def gap(mm):
        keys = ("rss", "heap_used", "mmap", "cache_memory")
        return None if any(mm.get(x) is None for x in keys) else \
            (mm["rss"] - mm["heap_used"] - mm["mmap"] - mm["cache_memory"]) / 1e6
    ga, gb = gap(memory_of(c["a"])), gap(m)
    extra = []
    if gb is not None:
        extra.append(f"`rss − (…)` {gb:,.1f} MB" + (f" (前 {ga:,.1f})" if ga is not None else ""))
    # 常駐の内訳 (T32.4。欄の無い版は今までどおり 2 つだけ)
    extra += [f"`{x}` {mb(m[x])} MB" for x in ("heap_free", "stacks_estimate", "stacks_resident", "rss_anon", "rss_file")
              if m.get(x) is not None]
    if extra:
        shown += "。表示だけ: " + "、".join(extra)
    # 呼ばなかった回 (`heap_free ≤ heap_used`) は返すものが無かった時間なので、動いた回に数える。
    # 呼んだのに 1 バイトも返していなければ届かず (呼んだ回が 0 なら返す量 0 でよい)
    ok = count + skipped >= need and (total > 0 or count == 0)
    return (label, limit, shown, MET if ok else MISSED,
            f"後の雪像の `/status` の `memory.trim` (起動から {uptime_text(c['b'].get('uptime_secs'))}、1 時間に 1 回)")
