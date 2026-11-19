# 判定表 (`scripts/snapshot_criteria.py`) の **T38 の行** (T38.0・T38.1)。
#
# T37 の 27 行のうち、`conn` 役の CPU ÷ 名前解決に成功した要求の行 (5 番目) を、**要求の量が前の版と大きく違う
# ときは「判定できず」**にする行 (T38.0) と、接続のエラー (443・80) の行 (13 番目) を、**窓の中で 1 本も確立できな
# かった宛先を外した率**で読む行 (T38.1) に差し替える。並べるのは `RULES` (`criteria/rules.py`)。
# **T37 以前の行と文面は 1 つも動かさない**。
#
# なぜ (T38.0): 0.37.8 の締め (T37.99) で、この行は物差しで割って 1.30 倍の届かずだった。名前解決に成功した要求が
# 2,060 → 1,302 本/時 (0.63 倍) に減り、`conn` 役の CPU の合計は 1 時間あたり 0.83 倍に減っていた。`conn` 役の
# CPU には、開いていたトンネルの中継のように**時間で払う分**があり、それを少ない要求で割ったので 1 本あたりが上がった。
#
# 材料を見て決めたこと (T38.0。T37.99 の 2 枚。前は 0.36.34 の止める前の 19.9 時間の `/profile`、後は 0.37.8 の
# 24 時間の `/profile`):
# - 時間で払う分の**量**はスナップショットにある。`/profile` の `conn` 役の標本の数 (接続スレッド × 秒。状態の内訳の
#   `ppoll` は中継の待ち) は 21,521 → 20,858 /時 (0.97 倍)、`/history` の `active` (開いていた接続の平均) は 6.3 → 7.4、
#   `bytes_delta` (中継したバイト) は 213 → 136 MB/時。
# - けれども **CPU を「要求で払う分」と「時間で払う分」に分ける材料は無い**。`/profile` の `conn` 役は 1 分ごとに
#   CPU の和 (`cpu_us`) を 1 つ持つだけで、要求を捌いた分と中継した分に分かれていない。分けるには CPU を要求と標本で
#   回帰するしかないが、2 枚の係数はそろわなかった: 1 時間ごとの最小二乗で、前は 983 us/要求 + 41 us/標本 (決定係数
#   0.91)、後は 236 us/要求 + 146 us/標本 (0.75)。1 分ごとだと後の決定係数は 0.24 で、中継したバイトを足すと要求あたりが
#   負になる。分けた値を前の版と比べると、判定が回帰のぶれで決まる。
# - そこで `TASKS.md` の T38.0 の 2 つ目の道にした: **要求の量 (名前解決に成功した要求/時) が前の版の 0.8〜1.25 倍の外**
#   なら「判定できず」にして理由を書く。中なら T36 と同じ読み方 (物差しで割ってから前の版の 1.1 倍未満)。
#   要求の量と `conn` 役の標本の前後は、どちらのときも表示だけで並べる。

from .common import CAUSE_NAMES, MET, MISSED, UNKNOWN, n, part, started_at
from .phase29_30 import _p29_profile_sides, conn_per_resolved_sides, history_since_start, profile_roles_us
from .t36 import CONNECT_ERROR_FLOOR, _t36_conn_per_resolved_request, _t36_connect_errors

# 要求の量 (名前解決に成功した要求/時) の前後の比がこの幅の外なら、`conn` 役の CPU の行は判定できず (T38.0)
CONN_REQUEST_BAND = (0.8, 1.25)


def conn_samples(p):
    """`/profile` の `conn` 役の標本の数の和 (接続スレッド × 秒。`threads` の役割ごとの 2 番目)。欄が無ければ None。"""
    keys = p.get("keys") or []
    roles = p.get("roles") or []
    if "threads" not in keys or "conn" not in roles:
        return None
    ti, ri = keys.index("threads"), roles.index("conn")
    total = 0
    for r in p.get("samples") or []:
        threads = r[ti] if ti < len(r) else None
        t = threads[ri] if threads and ri < len(threads) else None
        if t and len(t) > 1:
            total += t[1] or 0
    return total


def request_volume_sides(c, th):
    """前と後の、名前解決に成功した要求/時と `conn` 役の標本/時 (`{誰: {"ok", "samples"}}`)。材料が無ければ None。

    数え方は `conn_per_resolved_sides` と同じ (`/profile` の要求 − 同じ窓の `/history` の `errors_by_cause.dns`)。
    """
    sides, _ = _p29_profile_sides(c, th)
    got, _, _ = conn_per_resolved_sides(c, th)
    if sides is None or got is None:
        return None
    out = {}
    for who, _, p, _ in sides:
        v = profile_roles_us(p)
        secs = v[3] if v else 0
        if not secs:
            return None
        g = got[who]
        s = conn_samples(p)
        out[who] = {"ok": (g["req"] - g["fail"]) / secs * 3600, "samples": None if s is None else s / secs * 3600}
    return out


def volume_text(q):
    """要求の量と `conn` 役の標本の前後 (表示だけ)。"""
    b, a = q["前"], q["後"]
    text = (f"要求の量 (T38.0): 名前解決に成功した要求 {b['ok']:,.0f} → **{a['ok']:,.0f}** 本/時 "
            f"(**{a['ok'] / b['ok']:.2f}** 倍)")
    if b["samples"] and a["samples"] is not None:
        text += (f"、`conn` 役の標本 (接続スレッド × 秒。時間で払う分の量) {b['samples']:,.0f} → {a['samples']:,.0f} /時 "
                 f"({a['samples'] / b['samples']:.2f} 倍)")
    return text


def _t38_conn_per_resolved_request(c, th):
    """T38.0: `conn` 役の CPU ÷ 名前解決に成功した要求を、要求の量が前の版の 0.8〜1.25 倍の中のときだけ判定する。

    実測と出どころは `_t36_conn_per_resolved_request` と同じ (物差しで割ってから前の版の 1.1 倍未満)。要求の量の比が
    `conn_request_band` の外なら判定できず (時間で払う分を要求の数で割るので、1 本あたりが要求の量で動く)。
    """
    label, limit, shown, verdict, src = _t36_conn_per_resolved_request(c, th)
    lo, hi = th["conn_request_band"]
    label = label.replace("(T36.2。", f"(要求の量が前の版の {lo}〜{hi} 倍の外なら判定できず。T38.0。T36.2。", 1)
    if verdict == UNKNOWN:
        return (label, limit, shown, verdict, src)
    q = request_volume_sides(c, th)
    if q is None or not q["前"]["ok"]:
        return (label, limit, shown, verdict, src)
    r = q["後"]["ok"] / q["前"]["ok"]
    shown += "。" + volume_text(q)
    if lo <= r <= hi:
        return (label, limit, shown, verdict, src)
    return (label, limit, shown, UNKNOWN,
            f"要求の量 (名前解決に成功した要求/時) が前の版の {r:.2f} 倍で、{lo}〜{hi} 倍の外 (T38.0)。`conn` 役の CPU には"
            "開いていたトンネルの中継のように時間で払う分があり、それを要求の数で割るので、1 本あたりが要求の量で動く。"
            "`/profile` の `conn` 役の CPU は 1 分ごとの和 1 つで、要求で払う分と時間で払う分に分ける材料がスナップショットに"
            "無い。物差しで割った比 (表示の値) は参考。" + src)


# --- T38.1: 接続のエラー (443・80) の率から、窓の中で 1 本も確立できなかった宛先を外す (13 番目の行) ---
#
# なぜ (T38.1): 0.37.8 の締め (T37.99) で、この行は 0.00019 → 0.00228 (67 / 29,390) の届かずだった。大半は tokyo から
# 繋がらない計測・追跡用の宛先に 1 つの接続元が繰り返し行った時間切れで、プロキシの側で直す所は無い。T36.99 の前も
# 同じ形があった。
#
# 材料を見て決めたこと (T38.1。T37.99 の 2 枚):
# - 率そのものは T36 と同じ起動からの `/history?res=3600` で数える。`/history` には宛先の別が無い。
# - 宛先の別は**各スナップショットの `/hosts`** で見る。`/hosts` は状態ファイルに残る通算で、行ごとに `requests`・
#   `errors`・`errors_by_cause`・`last_seen` がある。外すのは、鍵の末尾が `:443`・`:80` (`other_port_*` と同じ見分け) で、
#   `last_seen` が起動より後 (窓の中で試した)、**通算で 1 本も確立していない** (`requests` ≤ `errors`) 宛先の、名前解決の
#   失敗を除いたエラー (`errors − errors_by_cause.dns`)。通算で 1 本でも繋がった宛先は外さない (窓の中だけ全部落ちた
#   宛先も外さないので、外し足りない側に倒れる)。
# - `/hosts` の差分 (前 → 後) は使わない。窓が 2 枚の間 (48 時間) で、後の起動からの `/history` (26.2 時間) とずれ、
#   前の `/hosts` は止めるときの 1 枚の上限で切れていた (1,000 行のうち 588 行)。`/errors` は 500 件の環で、
#   後の 1 枚では最後の 7.5 時間しか覆わない。
# - 通算なので、窓より前の失敗も入りうる。分子を越えては外さない。T37.99 の後の 1 枚では、外す宛先が 12 個・63 件で、
#   `/history` の分子 67 件の大半だった (後の起動のときに撮られた 1 枚 (`-start-snapshot.json`。`/hosts` は切れていた)
#   からの差分で数えても 13 個・64 件で、窓より前の失敗はほぼ入っていなかった)。前の 1 枚では 3 個・4 件。
# - 外したあとの率を T36 と同じ閾 (前の版の 1.5 倍未満か、後が 0.001 未満) で読む。外した宛先の数と件数は表示だけ。


def web_error_counts(snap):
    """起動からの `/history?res=3600` の 443・80 宛ての `(エラー, 試行, 時間)` (数え方は `_p29_connect_errors`)。無ければ None。"""
    keys = ("errors", "errors_by_cause", "connects", "other_port_errors", "other_port_attempts")
    v = history_since_start(snap, 3600, keys)
    if v is None:
        return None
    causes = v["_causes"]
    dns = v["errors_by_cause"][causes.index("dns")] if "dns" in causes else 0
    errs = v["errors"] - dns
    return errs - v["other_port_errors"], v["connects"] + errs - v["other_port_attempts"], v["_samples"]


def dead_destinations(snap):
    """窓の中で試して、通算で 1 本も確立していない 443・80 の宛先 (`{"hosts", "errors", "shown", "count", "cut"}`)。

    `/hosts` の部が無ければ None。切れた `/hosts` (`truncated`) は見えた行だけから数える (`cut` が真)。
    """
    h = part(snap, "hosts")
    rows = h.get("hosts")
    if not isinstance(rows, list):
        return None
    start = started_at(snap)
    di = CAUSE_NAMES.index("dns")
    hosts = errors = 0
    for r in rows:
        key = r.get("host") if isinstance(r, dict) else None
        if not isinstance(key, str) or not key.endswith((":443", ":80")):
            continue
        seen = r.get("last_seen")
        if start is None or not isinstance(seen, (int, float)) or seen < start:
            continue
        req, err = r.get("requests") or 0, r.get("errors") or 0
        by = r.get("errors_by_cause") or []
        bad = err - ((by[di] or 0) if di < len(by) else 0)
        if bad > 0 and req <= err:
            hosts += 1
            errors += bad
    return {"hosts": hosts, "errors": errors, "shown": len(rows), "count": h.get("count"),
            "cut": bool(h.get("truncated"))}


def _t38_connect_errors(c, th):
    """T38.1: 接続のエラー (443・80) の率を、窓の中で 1 本も確立できなかった宛先を分子と分母から外して読む。

    閾は `_t36_connect_errors` と同じ (前の版の `connect_error_factor` 倍未満か、後が `CONNECT_ERROR_FLOOR` 未満)。
    外した宛先の数と件数、外す前の率は表示だけ。`/history` の欄が足りずに T36 の行が判定できないときは、その理由のまま。
    """
    label, limit, shown, verdict, src = _t36_connect_errors(c, th)
    label = label.replace("(T36.3。", "(窓の中で 1 本も確立できなかった宛先を外す。T38.1。T36.3。", 1)
    if verdict == UNKNOWN:
        return (label, limit, shown, verdict, src)
    got, notes = {}, []
    for who, snap in (("前", c["a"]), ("後", c["b"])):
        counts = web_error_counts(snap)
        if counts is None:
            return (label, limit, shown, verdict, src)
        num, den, _ = counts
        dead = dead_destinations(snap)
        if dead is None:
            notes.append(f"{who}の雪像に `/hosts` が無いので、{who}は外していない")
            dead = {"hosts": 0, "errors": 0}
        elif dead["cut"]:
            notes.append(f"{who}の `/hosts` が切れている ({n(dead['shown'])} / {n(dead['count'])} 行) ので、見えた行だけから外した")
        drop = min(dead["errors"], num)
        got[who] = {"num": num - drop, "den": den - drop, "hosts": dead["hosts"], "drop": drop}

    def text(who, bold):
        g = got[who]
        star = "**" if bold else ""
        return f"{star}{g['num'] / g['den']:.5f}{star} ({n(g['num'])} / {n(g['den'])})" if g["den"] else "— (試行 0)"
    a, b = got["後"], got["前"]
    dropped = "・".join(f"{who} {n(got[who]['hosts'])} 個 {n(got[who]['drop'])} 件" for who in ("前", "後"))
    shown = (f"{text('前', False)} → {text('後', True)} (窓の中で 1 本も確立できなかった宛先を外した)。"
             f"表示だけ (T38.1): 外した宛先 {dropped}。外す前 " + shown.replace("**", ""))
    src = (src + "。外した宛先は各スナップショットの `/hosts` (通算) のうち、鍵の末尾が `:443`・`:80`、`last_seen` が起動より後で、"
           "通算で 1 本も確立していない (`requests` ≤ `errors`) もの。その `errors − errors_by_cause.dns` を分子と分母から引いた "
           "(通算なので窓より前の失敗も入りうる。分子を越えては引かない。T38.1)")
    if notes:
        src += "。" + "。".join(notes)
    if not a["den"] or not b["den"]:
        side = "後" if not a["den"] else "前"
        return (label, limit, shown, UNKNOWN, src + f"。外したあとの{side}の 443・80 の試行が 0")
    k, floor = th["connect_error_factor"], CONNECT_ERROR_FLOOR
    ra, rb = a["num"] / a["den"], b["num"] / b["den"]
    if rb == 0:
        verdict = MET if ra == 0 else MISSED
    else:
        verdict = MET if ra < rb * k else MISSED
    if verdict == MISSED and ra < floor:
        verdict = MET
        src += f"。外したあとの後の {ra:.5f} が下限 {floor} 未満なので、前の何倍かに依らず満たした (T36.3)"
    return (label, limit, shown, verdict, src)
