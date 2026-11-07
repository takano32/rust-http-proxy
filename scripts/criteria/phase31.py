# 判定表 (`scripts/snapshot_criteria.py`) の **Phase 31 の行** (T35.9)。
#
# phase30 の 26 行のうち、`/events` の anomaly の行 (7 番目) を要約 (`snapshot-summary.py`) と同じ数え方に
# した行。並べるのは `RULES` (`criteria/rules.py`)。**phase30 以前の行と文面は 1 つも動かさない**。

from .common import MET, MISSED, UNKNOWN, anomaly_rates_since_start
from .phase20_22 import display_only


# --- T35.9: `/events` の anomaly の件/時を要約と同じ関数で数える (7 番目の行) ---
#
# phase17〜30 の行 (`_p17_events`) は `criteria/phase14_18.py` の `anomaly_rates` で数え、要約の表は
# `snapshot-summary.py` の `anomaly_rates` で数えていた。2 つは起動の時刻の出し方 (`uptime_secs` だけか、
# `status.uptime_secs` を先に見るか)・`:` の無い文面 (外すか `?` に数えるか)・「切れている」の読み方
# (応答の `truncated` だけか、リングが満杯で起動より前の出来事が残っていない時も含めるか) が違い、同じ雪像でも
# 要約の件/時と判定表の行がずれることがあった (T34.5 のレビュー)。利用者の判断 (2026-11-07) で要約の
# 数え方に揃え、phase31 からはその 1 つの関数 (`criteria/common.py` の `anomaly_rates_since_start`) を使う。
# 件/時は要約の表と同じく 3 桁 (閾 0.1 件/時の近くで 2 桁に丸めると読み違える)。

def _p31_events_measured(c, th):
    """T35.9: `/events` の anomaly の種類別 件/時 (要約の表と同じ関数・同じ 3 桁)。

    行の形は `_p17_events` と同じ (閾のある種類を先に太字で、前の雪像の件/時を添える)。phase31 では
    下の `display_only` で表示だけにする (phase22 から `dns_slow` の判定は別の行に任せている)。
    """
    limits = th["events_per_hour"]
    label = "`/events` の anomaly の種類別 件/時 (" + "、".join(
        f"`{k}` {v} 以下" for k, v in limits.items()) + "。ほかは表示だけ)"
    limit = "、".join(f"`{k}` ≤ {v}" for k, v in limits.items())
    aft, bef = anomaly_rates_since_start(c["b"]), anomaly_rates_since_start(c["a"])
    if aft is None:
        return (label, limit, "—", UNKNOWN, "後の雪像に `/events` の部が無い")
    now = {k: (cnt, rate) for k, cnt, rate in aft["rows"]}
    was = {k: (cnt, rate) for k, cnt, rate in bef["rows"]} if bef is not None else None
    kinds = sorted(set(now) | set(limits), key=lambda k: (k not in limits, -now.get(k, (0, None))[0], k))

    def rate_text(rate):
        return "—" if rate is None else f"{rate:.3f}"
    shown, verdict = [], MET
    for k in kinds:
        cnt, rate = now.get(k, (0, 0.0 if aft["hours"] else None))
        before = ""
        if was is not None:
            before = f"、前 {rate_text(was.get(k, (0, 0.0 if bef['hours'] else None))[1])}"
        star = "**" if k in limits else ""
        shown.append(f"`{k}` {star}{rate_text(rate)}{star} 件/時 ({cnt} 件{before})")
        if k in limits and rate is not None and rate > limits[k]:
            verdict = MISSED
    hours = (f"起動からの {aft['hours']:,.1f} 時間で割った" if aft["hours"]
             else "起動からの時間が分からない (件/時は出さない)")
    src = ("`/events` の `kind == \"anomaly\"` を `text` の先頭で分け (`:` の無い文面は `?`)、`cleared:` を除き、"
           f"起動 (取得 − `status.uptime_secs`) より前の {aft['before']} 件を外し、{hours}"
           " (`snapshot-summary.py` の要約の表と同じ関数。T35.9)")
    if bef is None:
        src += "。前の雪像に `/events` が無い"
    if aft["truncated"]:
        src += "。**`/events` が満杯か切れている** (起動直後のぶんが落ちているかもしれない。件数は下限)"
    return (label, limit, "、".join(shown), verdict, src)


_p31_events = display_only(
    _p31_events_measured,
    lambda th: "`/events` の anomaly の種類別 件/時 (`dns_slow` を含めて表示だけ。要約と同じ数え方)",
    "`dns_slow` の判定は新しい閾の行 (合計 1,000 ms 未満の窓で立った件数) に任せる。T22.2 の模型で tokyo の後は"
    "本当に遅い窓だけで 0.17 件/時あり、件/時の閾は使われ方で落ちる (親の判断 2026-10-17)", was="phase21")
