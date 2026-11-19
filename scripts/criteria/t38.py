# 判定表 (`scripts/snapshot_criteria.py`) の **T38 の行** (T38.0)。
#
# T37 の 27 行のうち、`conn` 役の CPU ÷ 名前解決に成功した要求の行 (5 番目) を、**要求の量が前の版と大きく違う
# ときは「判定できず」**にする行に差し替える。並べるのは `RULES` (`criteria/rules.py`)。
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

from .common import UNKNOWN
from .phase29_30 import _p29_profile_sides, conn_per_resolved_sides, profile_roles_us
from .t36 import _t36_conn_per_resolved_request

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
