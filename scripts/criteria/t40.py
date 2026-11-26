# 判定表 (`scripts/snapshot_criteria.py`) の **T40 の行** (T40.4)。
#
# T39 の 27 行のうち、`conn` 役の CPU を要求と中継の時間で値付けし合う行 (5 番目) に、**同じ版の日ごとのぶれの帯**を
# 入れる。並べるのは `RULES` (`criteria/rules.py`)。**T39 以前の行と文面は 1 つも動かさない** (`t39.py` は触らず、
# その行をそのまま呼ぶ)。
#
# なぜ (T40.4): 同じ版 0.39.13 の 2 つの 24 時間 (2026-11-24 と 2026-11-26 の `/profile?res=60`) を T39 の行で比べると、
# 同じバイナリなのに 0.799 倍、前と後を入れ替えると 1.251 倍 (= 1 ÷ 0.799) で、入れ替えた向きは届かずになる。行が
# 見ている「同じ版の中のぶれ」(1 枚を半分ずつ・1 時間おきに割って値付けし合った比) は最大 1.11 倍で、日をまたぐぶれを
# 見ていなかった。値段を決める回帰の決定係数は -0.02 と 0.45 (値段は 828 + 44.7 us と 456 + 80.5 us)。暇なときの
# 費用の行は同じ 2 日で 1.00 倍、物差しも 1.00 倍で安定している。利用者が「ぶれの帯を入れた判定表を足す」を選んだ
# (2026-11-26)。
#
# 読み方 (利用者と決めた形): 値付けし合った比 (物差しで割ったあと。計算は T39 のまま) が
# - `conn_per_resolved_factor` (1.1) 倍未満なら満たした、
# - 1.1 倍以上 `conn_day_band_max` (1.3) 倍以下なら判定できず (同じ版の日ごとのぶれの内)、
# - 1.3 倍を越えたら届かず。
# T39 の「同じ版の中のぶれ (1 枚の中のぶれ) の内なら判定できず」と、材料が足りないときの判定できずは、そのまま残す
# (帯より広いぶれが 1 枚の中に出ていれば、1.3 倍を越えていても判定できず)。
#
# 記録にある 1.299 倍について: T40.1 の結果と T40.4 の目的には「向きを入れ替えると 1.299 倍」とある。これは
# `snapshot-diff.py` に 4 つの引数 (スナップショット 2 枚と `--profile-before`・`--profile`) を入れ替えて渡した出力の値。
# 道具はスナップショットを古い順に並べ直し、`--profile` は新しい方に、`--profile-before` は古い方に付けるので、
# 2026-11-24 のスナップショットに 2026-11-26 の `/profile` が付いた組になり、名前解決の失敗が窓の合わない `/history` から
# 0 本と 804 本 (組を合わせると 944 本と 803 本) に数えられていた。組を合わせて前と後を入れ替えた値が 1.251 倍。
# どれも帯の内で、判定できずになる (`test_snapshot_criteria_t40.py` が 3 つとも実物の材料で確かめる)。

import math

from .common import MET, MISSED, UNKNOWN
from .phase29_30 import _p29_profile_sides, conn_per_resolved_sides
from .t36 import machine_sides
from .t39 import CONN_PRICE_MIN_HOURS, _t39_conn_priced, conn_hours, cross_index, self_spread

# 同じ版の日ごとのぶれの帯の上の端 (値付けし合った比が 1.1 倍以上でも、この倍以下なら判定できず。T40.4)
CONN_DAY_BAND_MAX = 1.3
# 帯の根拠 (行の理由に書く): 同じ版の 2 日を、組を合わせて両方の向きで比べた値 (0.799 倍と 1.251 倍)
DAY_BAND_EVIDENCE = ("同じ版 0.39.13 の 2 つの 24 時間 (2026-11-24 と 2026-11-26) を比べると、同じバイナリなのに 0.80 倍、"
                     "前と後を入れ替えると 1.25 倍だった。")


def conn_priced_index(c, th):
    """T39 の行 (`_t39_conn_priced`) と同じ材料・同じ関数で、`(値付けし合った比, 1 枚の中のぶれ)` を出し直す。

    ぶれは、前と後の 1 枚をそれぞれ半分ずつ・1 時間おきに値付けし合った比の、対数の絶対値の最大 (`math.exp` で倍に戻る)。
    T39 の行が材料の不足で判定できずにするとき (`/profile` が 12 時間に満たない・欄が無い・値段が 0 以下) は None。
    T39 の行は文面しか返さず、`t39.py` は凍結しているので、数字はここで同じ手順をもう 1 回踏んで得る (行の表示の
    桁でこの 2 つが T39 の行の文面と合うことを、試験が見る)。
    """
    sides, _ = _p29_profile_sides(c, th)
    if sides is None:
        return None
    got, _, _ = conn_per_resolved_sides(c, th)
    m, _ = machine_sides(c, th)
    if got is None or m is None:
        return None
    hours, shares = {}, {}
    for who, _, p, _ in sides:
        h, _ = conn_hours(p)
        if h is None or len(h) < CONN_PRICE_MIN_HOURS:
            return None
        g = got[who]
        hours[who], shares[who] = h, (g["req"] - g["fail"]) / g["req"]
    idx = cross_index(hours["前"], shares["前"], hours["後"], shares["後"], m["前"]["us"] / m["後"]["us"])
    if idx is None:
        return None
    spread = [x for who in ("前", "後") for x in self_spread(hours[who], shares[who])]
    return idx[0], max((abs(math.log(x)) for x in spread), default=0.0)


def _t40_conn_priced(c, th):
    """T40.4: `conn` 役の CPU を値付けし合う行 (T39.9) に、同じ版の日ごとのぶれの帯を入れる。

    実測と出どころは `_t39_conn_priced` と同じ。値付けし合った比が `conn_per_resolved_factor` 倍未満なら満たした、
    それ以上で `conn_day_band_max` 倍以下なら判定できず (同じ版の日ごとのぶれの内)、越えたら届かず。T39 の行が
    判定できずにしたもの (同じ版の中のぶれ = 1 枚の中のぶれの内・材料の不足) は、帯の外でも判定できずのまま。
    """
    label, limit, shown, verdict, src = _t39_conn_priced(c, th)
    k, hi = th["conn_per_resolved_factor"], th["conn_day_band_max"]
    label = label.replace("(T39.9。", f"({k} 倍以上 {hi} 倍以下は同じ版の日ごとのぶれの内で判定できず、{hi} 倍を越えたら届かず。"
                          "T40.4。T39.9。", 1)
    limit = (f"< 前 ×{k} (値付けし合った比。×{k} 以上 ×{hi} 以下は日ごとのぶれの内で判定できず、×{hi} を越えたら届かず。"
             "同じ版の中のぶれ (1 枚の中) の内も判定できず)")
    got = conn_priced_index(c, th)
    if got is None or verdict == MET:
        return (label, limit, shown, verdict, src)
    g, worst = got
    # ここに来るのは 1.1 倍以上。T39 の行は、同じ版の中のぶれ (1 枚の中) の内なら判定できず (理由を出どころの頭に書く)、
    # 外なら届かず
    within_one = verdict == UNKNOWN
    if g <= hi:
        why = (f"値付けし合った比 {g:.3f} 倍は {k} 倍以上だが、同じ版の日ごとのぶれの帯 ({k}〜{hi} 倍。T40.4) の内。"
               + DAY_BAND_EVIDENCE)
        why += ("同じ版の中のぶれ (1 枚の中) で見ても判定できず: " if within_one else
                f"同じ版の中のぶれ (1 枚の中。最大 {math.exp(worst):.2f} 倍) は越えている (T39 の読み方では届かず)。")
        return (label, limit, shown, UNKNOWN, why + src)
    if within_one:
        return (label, limit, shown, UNKNOWN,
                f"値付けし合った比は同じ版の日ごとのぶれの帯 ({k}〜{hi} 倍。T40.4) を越えているが、同じ版の中のぶれ "
                "(1 枚の中) の内なので届かずにしない: " + src)
    return (label, limit, shown, MISSED,
            f"値付けし合った比 {g:.3f} 倍は、同じ版の日ごとのぶれの帯 ({k}〜{hi} 倍。T40.4) も、同じ版の中のぶれ "
            f"(1 枚の中。最大 {math.exp(worst):.2f} 倍) も越えている。" + src)
