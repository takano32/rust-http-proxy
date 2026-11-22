# 判定表 (`scripts/snapshot_criteria.py`) の **T39 の行** (T39.9)。
#
# T38 の 27 行のうち、`conn` 役の CPU ÷ 名前解決に成功した要求の行 (5 番目) を、**要求と中継の時間で値付けし合う行**
# に差し替える。並べるのは `RULES` (`criteria/rules.py`)。**T38 以前の行と文面は 1 つも動かさない**。
#
# なぜ (T39.9): この行は 3 回の締めで続けて読めなかった。T36.99 は 1.08 倍で満たしたが、T37.99 は 1.30 倍の届かず
# (要求が 0.66 倍に減った)、T38.99 は要求が 0.69 倍で T38.0 の幅の外の判定できず。`conn` 役の CPU には、開いている
# トンネルの中継のように時間で払う分があり、要求の数で割ると 1 本あたりが要求の量で動く。
#
# 材料を見て決めたこと (T39.9。前の 3 回の締めの 6 枚の `/profile?res=60`):
# - **CPU を 2 つに分ける材料は無い**。`/profile` の `conn` 役は 1 分ごとの CPU の和 1 つで、`/recent` の個票にも
#   接続ごとの CPU は無い。回帰で分けた値段は版ごとにそろわない (要求 1 本 700〜1,000 us、中継 1 秒 25〜80 us)。
# - けれども**時間で払う分の量は `/profile` そのものにある**: connect の段の `relay` の時間の和 (閉じたトンネルの中継の
#   時間。1 分ごと、同じ窓)。`/history?res=60` の `closed` にも接続の寿命があるが、`--profile-before` は前の版の別の 1 枚
#   なので、前のスナップショットの `/history?res=60` (24 時間) とは窓がずれる。
# - 1 時間ごとに `CPU = a × 名前解決に成功した要求 + b × 中継の秒` を前と後で別々に決め、**前の値段で後の仕事を、
#   後の値段で前の仕事を値付けした比** (物差しで割る) の幾何平均で比べると、2 つの向きがそろう (T36.99 1.11・1.10、
#   T37.99 1.19・1.16、T38.99 0.65・0.61)。要求だけで値付けするより、同じ版の 1 枚を半分に割って値付けし合った比
#   (理想は 1.00) の外れ (|比 − 1|) が小さい (7 枚 14 組の平均で 0.14 → 0.09)。
# - それでも同じ版の中のぶれは 1.1 倍を越えることがある (1 時間おきに割っても 1.17 倍の 1 枚があった)。そこで閾
#   (前の版の 1.1 倍) を越えても、**前と後の 1 枚をそれぞれ半分ずつ・1 時間おきに割って値付けし合った比のいちばん大きい
#   外れの内**なら「判定できず」にして理由を書く。前の 3 回は、T36.99 が 1.100 倍 (閾をわずかに越えた) でぶれ 1.17 倍の
#   内の判定できず、T37.99 が 1.17 倍でぶれ 1.12 倍の外の届かず、T38.99 が 0.63 倍で満たした。
# - 分けるには本体に材料が要る (案は T39.9 の結果に書く)。

import math

from .common import MET, MISSED, UNKNOWN, n
from .phase29_30 import _p29_profile_sides, conn_per_resolved_sides
from .t36 import _t36_conn_per_resolved_request, machine_sides, machine_text
from .t38 import request_volume_sides, volume_text

# 値段を決めるのに要る 1 時間の束の数 (前と後のそれぞれ)
CONN_PRICE_MIN_HOURS = 12


def conn_hours(p):
    """`/profile?res=60` を時計の 1 時間ずつ束ねた `[{"cpu", "req", "relay", "rows"}]` (`conn` 役の us・要求・中継の秒) か、無い欄の名前。

    中継の秒は connect の段の `relay` の時間の和 (`[数, 和 ms, 最大, 分布]`。空の段は `0`)。標本が半分に満たない時間 (窓の両端) は捨てる。
    """
    keys = p.get("keys") or []
    roles = p.get("roles") or []
    stages = ((p.get("stages") or {}).get("connect")) or []
    missing = [k for k in ("threads", "requests", "connect") if k not in keys]
    if "conn" not in roles:
        missing.append("`conn` の役割")
    if "relay" not in stages:
        missing.append("connect の段の `relay`")
    interval = p.get("interval_secs") or 0
    if missing or not interval:
        return None, missing or ["`interval_secs`"]
    ti, qi, ci = keys.index("threads"), keys.index("requests"), keys.index("connect")
    ri, si = roles.index("conn"), stages.index("relay")
    tk = keys.index("t") if "t" in keys else None
    per = max(1, round(3600 / interval))
    by = {}
    for i, r in enumerate(p.get("samples") or []):
        t = r[tk] if tk is not None and tk < len(r) and isinstance(r[tk], (int, float)) else i * interval
        cur = by.setdefault(int(t // 3600), {"cpu": 0, "req": 0, "relay": 0.0, "rows": 0})
        threads = r[ti] if ti < len(r) else None
        t = threads[ri] if isinstance(threads, list) and ri < len(threads) else None
        cur["cpu"] += (t[0] or 0) if isinstance(t, list) and t else 0
        cur["req"] += (r[qi] if qi < len(r) else 0) or 0
        cf = r[ci] if ci < len(r) else None
        s = cf[si] if isinstance(cf, list) and si < len(cf) else None
        cur["relay"] += ((s[1] or 0) / 1000) if isinstance(s, list) and len(s) > 1 else 0
        cur["rows"] += 1
    return [by[h] for h in sorted(by) if by[h]["rows"] * 2 >= per], None


def fit_price(hours, share):
    """原点を通る最小二乗で `cpu = a × 要求 × share + b × 中継の秒` の `(a, b, 決定係数)`。

    中継の時間が無いか要求と比例していて 2 つを決められなければ、要求だけで決めて `b = 0`。要求も 0 なら None。
    """
    xs = [(h["req"] * share, h["relay"]) for h in hours]
    ys = [h["cpu"] for h in hours]
    s11 = sum(x * x for x, _ in xs)
    s22 = sum(y * y for _, y in xs)
    s12 = sum(x * y for x, y in xs)
    t1 = sum(x * c for (x, _), c in zip(xs, ys))
    t2 = sum(y * c for (_, y), c in zip(xs, ys))
    det = s11 * s22 - s12 * s12
    if s11 <= 0:
        return None
    if det <= 1e-9 * s11 * max(s22, 1):
        a, b = t1 / s11, 0.0
    else:
        a, b = (t1 * s22 - t2 * s12) / det, (t2 * s11 - t1 * s12) / det
    mean = sum(ys) / len(ys)
    ss = sum((c - a * x - b * y) ** 2 for (x, y), c in zip(xs, ys))
    st = sum((c - mean) ** 2 for c in ys)
    return a, b, (1 - ss / st) if st else 0.0


def price(m, hours, share):
    """値段 `m` で `hours` の仕事を値付けした us。"""
    return sum(m[0] * h["req"] * share + m[1] * h["relay"] for h in hours)


def cross_index(hb, sb, ha, sa, scale=1.0):
    """前 (`hb`) と後 (`ha`) を互いの値段で値付けした `(幾何平均, 前の値段で後, 後の値段で前, 前の値段, 後の値段)` か None。

    後の CPU と後の値段には `scale` (前 ÷ 後の物差し) を掛けて前の機械の値段に直す。値段が 0 以下なら None。
    """
    mb, ma = fit_price(hb, sb), fit_price(ha, sa)
    if mb is None or ma is None or min(mb[0], ma[0]) <= 0 or min(mb[1], ma[1]) < 0:
        return None
    ca, cb = sum(h["cpu"] for h in ha) * scale, sum(h["cpu"] for h in hb)
    pa, pb = price(mb, ha, sa), price(ma, hb, sb) * scale
    if pa <= 0 or cb <= 0:
        return None
    r1, r2 = ca / pa, pb / cb
    return math.sqrt(r1 * r2), r1, r2, mb, ma


def self_spread(hours, share):
    """同じ 1 枚を前半と後半・1 時間おきに割って値付けし合った比 (`[比]`)。理想は 1.00。"""
    half = len(hours) // 2
    out = []
    for x, y in ((hours[:half], hours[half:]), (hours[0::2], hours[1::2])):
        got = cross_index(x, share, y, share) if x and y else None
        if got is not None:
            out.append(got[0])
    return out


def _t39_conn_priced(c, th):
    """T39.9: `conn` 役の CPU を、名前解決に成功した要求と中継の時間で値付けし合って前の版と比べる。

    1 時間ごとに前と後の値段 (要求 1 本・中継 1 秒) を決め、互いの仕事を値付けした比の幾何平均を物差しで割って
    `conn_per_resolved_factor` 倍未満なら満たした。越えても、前と後のそれぞれの 1 枚の中のぶれ (`self_spread` の
    いちばん大きい外れ) の内なら判定できず。T38 の 1 本あたりの比と要求の量は表示だけ。
    """
    k = th["conn_per_resolved_factor"]
    label = (f"`conn` 役の CPU を**名前解決に成功した要求と中継の時間で値付けし合って**、前の版の {k} 倍未満 (T39.9。"
             "1 時間ごとに CPU = 要求 1 本の値段 × 要求 + 中継 1 秒の値段 × 中継の秒 で前と後の値段を決め、互いの仕事を"
             "値付けした比の幾何平均を機械の速さの物差しで割る。同じ版の中のぶれの内なら判定できず。1 本あたりの比は表示だけ)")
    limit = f"< 前 ×{k} (値付けし合った比。ぶれの内は判定できず)"
    sides, why = _p29_profile_sides(c, th)
    if sides is None:
        return (label, limit, "—", UNKNOWN, why)
    got, srcs, why = conn_per_resolved_sides(c, th)
    if got is None:
        return (label, limit, "—", UNKNOWN, why)
    m, why = machine_sides(c, th)
    if m is None:
        return (label, limit, "—", UNKNOWN, why)
    hours, shares = {}, {}
    for who, _, p, _ in sides:
        h, miss = conn_hours(p)
        if h is None:
            return (label, limit, "—", UNKNOWN, f"{who}の `/profile` に " + "・".join(miss) + " が無い")
        if len(h) < CONN_PRICE_MIN_HOURS:
            return (label, limit, "—", UNKNOWN, f"{who}の `/profile` の 1 時間の束が {len(h)} で {CONN_PRICE_MIN_HOURS} に満たない")
        g = got[who]
        hours[who], shares[who] = h, (g["req"] - g["fail"]) / g["req"]
    scale = m["前"]["us"] / m["後"]["us"]
    idx = cross_index(hours["前"], shares["前"], hours["後"], shares["後"], scale)
    t36 = _t36_conn_per_resolved_request(c, th)
    src = ("`/profile?res=60` を時計の 1 時間ずつ束ね (標本が半分に満たない両端は捨てる)、`conn` 役の `cpu_us`、名前解決に成功した要求 (要求 × (1 − "
           "`/history?res=3600` の `errors_by_cause.dns` の割合))、connect の段の `relay` の時間の和 (閉じたトンネルの中継の秒) を"
           "並べ、原点を通る最小二乗で前と後の値段を決めた。" + "。".join(srcs) + "。" + machine_text(m))
    if idx is None:
        return (label, limit, "—", UNKNOWN,
                "値段が 0 以下になった (要求と中継の時間で `conn` 役の CPU を説明できない)。" + src)
    g, r1, r2, mb, ma = idx
    spread = [x for who in ("前", "後") for x in self_spread(hours[who], shares[who])]
    worst = max((abs(math.log(x)) for x in spread), default=0.0)
    relay = {who: sum(h["relay"] for h in hours[who]) / len(hours[who]) for who in ("前", "後")}
    shown = (f"**{g:.3f}** 倍 (前の機械の値段に直した。前の値段で後の仕事 {r1:.2f}・後の値段で前の仕事 {r2:.2f})。"
             f"値段 (要求 1 本 + 中継 1 秒): 前 {mb[0]:,.0f} + {mb[1]:,.1f} us (決定係数 {mb[2]:.2f})、"
             f"後 {ma[0]:,.0f} + {ma[1]:,.1f} us ({ma[2]:.2f})。同じ版の中のぶれ (前と後の 1 枚をそれぞれ半分ずつ・"
             f"1 時間おきに値付けし合った比) は最大 {math.exp(worst):.2f} 倍。"
             f"表示だけ: 中継 {relay['前']:,.0f} → {relay['後']:,.0f} 秒/時 ({n(len(hours['前']))} → {n(len(hours['後']))} 時間)、"
             f"T38 の読み方 " + t36[2].replace("**", ""))
    q = request_volume_sides(c, th)
    if q is not None and q["前"]["ok"]:
        shown += "。" + volume_text(q).replace("**", "")
    if g < k:
        return (label, limit, shown, MET, src)
    if math.log(g) <= worst:
        return (label, limit, shown, UNKNOWN,
                f"値付けし合った比 {g:.3f} 倍は {k} 倍以上だが、同じ版の中のぶれ (最大 {math.exp(worst):.2f} 倍) の内 (T39.9)。"
                "`conn` 役の CPU は 1 分ごとの和 1 つで、要求で払う分と時間で払う分に分ける材料はスナップショットに無い。" + src)
    return (label, limit, shown, MISSED, src)
