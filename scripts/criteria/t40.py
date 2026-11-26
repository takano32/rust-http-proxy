# 判定表 (`scripts/snapshot_criteria.py`) の **T40 の行** (T40.4・T40.5)。
#
# T39 の 27 行のうち 2 行を直す。並べるのは `RULES` (`criteria/rules.py`)。**T39 以前の行と文面は 1 つも動かさない**
# (`t39.py`・`phase29_30.py` は触らず、その行と小物をそのまま呼ぶ)。
#
# - 5 番目の `conn` 役の CPU を要求と中継の時間で値付けし合う行に、**同じ版の日ごとのぶれの帯**を入れる (T40.4)。
#   材料の窓 (`/profile?res=60` の窓と、同じスナップショットの `/history`) が合っていなければ判定できずにする (T40.5)。
# - 15 番目の裏の引き直しの行を、答えのある名前 1 つあたりではなく**合計 (回/時)** で前の期間と比べる (T40.5)。
#
# なぜ帯を入れるか (T40.4): 同じ版 0.39.13 の 2 つの 24 時間 (2026-11-24 と 2026-11-26 の `/profile?res=60`) を T39 の行で
# 比べると、同じバイナリなのに 0.799 倍、前と後を入れ替えると 1.251 倍 (= 1 ÷ 0.799) で、入れ替えた向きは届かずになる。行が
# 見ている「同じ版の中のぶれ」(1 枚を半分ずつ・1 時間おきに割って値付けし合った比) は最大 1.11 倍で、日をまたぐぶれを
# 見ていなかった。値段を決める回帰の決定係数は -0.02 と 0.45 (値段は 828 + 44.7 us と 456 + 80.5 us)。暇なときの
# 費用の行は同じ 2 日で 1.00 倍、物差しも 1.00 倍で安定している。利用者が「ぶれの帯を入れた判定表を足す」を選んだ
# (2026-11-26)。
#
# 帯の読み方 (利用者と決めた形): 値付けし合った比 (物差しで割ったあと。計算は T39 のまま) が
# - `conn_per_resolved_factor` (1.1) 倍未満なら満たした、
# - 1.1 倍以上 `conn_day_band_max` (1.3) 倍以下なら判定できず (同じ版の日ごとのぶれの内)、
# - 1.3 倍を越えたら届かず。
# T39 の「同じ版の中のぶれ (1 枚の中のぶれ) の内なら判定できず」と、材料が足りないときの判定できずは、そのまま残す
# (帯より広いぶれが 1 枚の中に出ていれば、1.3 倍を越えていても判定できず)。
#
# なぜ材料の窓を見るか (T40.5): この行は名前解決の失敗の数を、`/profile?res=60` の窓と同じ時間の `/history` (`res=60`、
# それより古い所は `res=3600`) から数える (`dns_failures_between`)。窓を覆う `/history` がスナップショットに無いと、覆われて
# いない所を黙って 0 本と数えていた。記録にある 1.299 倍がこれ: T40.1 の結果と T40.4 の目的の「向きを入れ替えると 1.299 倍」は、
# `snapshot-diff.py` に 4 つの引数 (スナップショット 2 枚と `--profile-before`・`--profile`) を入れ替えて渡した出力の値。
# 道具はスナップショットを古い順に並べ直し、`--profile` は新しい方に、`--profile-before` は古い方に付けるので、
# 2026-11-24 のスナップショットに 2026-11-26 の `/profile` が付いた組になり、名前解決の失敗が 0 本と 804 本 (組を合わせると
# 944 本と 803 本) に数えられていた。組を合わせて前と後を入れ替えた値が 1.251 倍。利用者が「判定できずと出す」を選んだ
# (2026-11-26)。前か後の窓のうち `/history` が覆っているのが `conn_history_cover_min` (99%) に満たなければ判定できず。
# 線の決め方: 締めのふつうの形は覆われていない所が無い (前の版の `/profile` を停止の 9 分前に取っていても、`res=60` に無い
# 頭の 9 分は `res=3600` が覆う。後は同じ時に取るので、ずれても 1〜2 分 = 12 時間の窓の 0.3%。手元の実物の 3 組は
# どれも 100%)。別の日の写しは丸ごと覆われない。その間で、端の数分のずれだけを許す 99% にした (24 時間の窓で 14 分)。
# 覆われていない所があると比がどれだけずれるかは、名前解決の失敗の割合で決まる。失敗が要求の 4 割の作り物 (T31.99 の
# 締めの前の版がその割合だった) で、後の窓の最後が覆われていないときの比のずれは、1% で 0.6%、5% で 2.8%、10% で 5.4%、
# 全部で 35% (閾は 1.1 倍)。失敗が 5% の実物の 2 日では、10% で 0.75%、全部で 3.6%。9 割の線だと、2.4 時間ぶんの失敗を
# 0 本と数えた比 (失敗が 4 割なら 5.4% ずれている) でも判定してしまう。
#
# なぜ引き直しを合計で読むか (T40.5): T33.2 の行 (`_p30_refresh_per_name`) は答えのある名前 1 つあたりで比べ、`/dns` の一覧が
# 切れているときだけ合計で比べる。T38.7・T39.8 で一覧が切れなくなってからは、いつも名前あたりで比べるようになった。
# 名前解決の表の名前は起動から増え続けるので、名前 1 つあたりは起動からの時間で値が変わる (`REFRESH_EVIDENCE`)。
# 0.40.0 の締めは前 = 94 時間・後 = 48 時間で、何も変わっていなくても 1.6 倍で届かずになる (締めの予行演習で見つけた)。
# 利用者が「合計で比べる行に直す」を選んだ (2026-11-26)。閾は T33.2 と同じ `refresh_per_name_factor` (1.5 倍)。
#
# なぜ割った時間が 3 時間に満たなければ判定しないか (T40.7): 合計は `dns.refreshes` ÷ 時間 (起動から、または 2 枚の間) で、
# 起動してすぐは落ち着いていない。tokyo の起動の 1 枚 (起動から 60 秒) は 0.40.0 が 0 回・0.39.13 が 1 回 (0〜60 回/時)、
# 0.40.0 の 2.7 時間は 413.2 回/時、0.39.13 の 12.0 時間は 358.5 回/時で、そのあとは 336〜359 回/時。起動してすぐの 1 枚が
# 「前」に来る (ホストの再起動の直後にデプロイしたとき) と、後が何倍にも見えて届かずになる (前が 0 回なら後が 1 回でも)。
# 利用者が歯止めの形を任せた (2026-11-26「いい感じにする」)。線は、同じ判定表の IPv6 の行が「0 時間のスナップショットから
# 3 時間」を起動直後として扱っているのに合わせて 3 時間にした (確かめてある 2.7 時間の 1.15 倍は、まだ落ち着く前の値)。

import math

from proxydata import CAUSE_NAMES

from .common import MET, MISSED, UNKNOWN, n, part, stamp
from .phase20_22 import counter_rates, dns_counter
from .phase29_30 import (
    _p29_profile_sides, answered_names, conn_per_resolved_sides, dns_failures_between, history_part, profile_window,
)
from .t36 import machine_sides
from .t39 import CONN_PRICE_MIN_HOURS, _t39_conn_priced, conn_hours, cross_index, self_spread

# 同じ版の日ごとのぶれの帯の上の端 (値付けし合った比が 1.1 倍以上でも、この倍以下なら判定できず。T40.4)
CONN_DAY_BAND_MAX = 1.3
# 帯の根拠 (行の理由に書く): 同じ版の 2 日を、組を合わせて両方の向きで比べた値 (0.799 倍と 1.251 倍)
DAY_BAND_EVIDENCE = ("同じ版 0.39.13 の 2 つの 24 時間 (2026-11-24 と 2026-11-26) を比べると、同じバイナリなのに 0.80 倍、"
                     "前と後を入れ替えると 1.25 倍だった。")
# `/profile?res=60` の窓のうち、同じスナップショットの `/history` が覆っていなければならない割合。前か後がこれに
# 満たなければ、値付けし合う行は判定できず (T40.5。線の決め方はこのファイルの頭)
CONN_HISTORY_COVER_MIN = 0.99
# 引き直しを合計で読む根拠 (行の出どころと前置きに書く。T40.5): 同じ版 0.39.13 (tokyo。2026-11-22 12:01:57Z に起動) の 5 枚の
# (起動からの時間, 答えのある名前 1 つあたりの引き直し 回/時)。合計は 335.6〜359.1 回/時、答えのある名前は 876 → 1,880
REFRESH_EVIDENCE = ((36.0, 0.383), (48.1, 0.309), (60.0, 0.250), (84.0, 0.208), (93.9, 0.191))
REFRESH_EVIDENCE_TOTAL = (336, 359)
REFRESH_EVIDENCE_NAMES = (876, 1880)
# 引き直しの合計を判定するのに要る、前と後を割った時間 (起動から、または 2 枚の間) の下の端 (時間)。前か後がこれに
# 満たなければ判定できず (T40.7。線の決め方はこのファイルの頭)
REFRESH_MIN_HOURS = 3.0
# 起動してすぐは合計が落ち着いていない根拠 (行の理由と前置きに書く。T40.7): tokyo の (起動からの時間の書き方, 合計 回/時)
REFRESH_STARTUP_EVIDENCE = (("1 分", "0〜60"), ("2.7 時間", "413"), ("12.0 時間", "359"))


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


# --- T40.5: 材料の窓 (`/profile?res=60` の窓と、同じスナップショットの `/history`) が合っているか ---

def _counts_as_seconds(h):
    """`/history` の部 1 つの写し。標本ごとの名前解決の失敗の数 (`errors_by_cause` の `dns`) を、その標本の長さ (秒) に置き換える。

    欄が無い部はそのまま返す (数える関数が「数えられない」と見るものは、写しでも同じに見える)。
    """
    if "errors_by_cause" not in h["keys"]:
        return h
    causes = h.get("causes") or list(CAUSE_NAMES)
    if "dns" not in causes:
        return h
    ei, di = h["keys"].index("errors_by_cause"), causes.index("dns")
    secs = h.get("interval_secs") or 3600
    rows = []
    for s in h["samples"]:
        by = s[ei] if s and ei < len(s) else None
        if isinstance(by, list) and di < len(by):
            s = list(s[:ei]) + [list(by[:di]) + [secs] + list(by[di + 1:])] + list(s[ei + 1:])
        rows.append(s)
    return dict(h, samples=rows)


def history_cover_secs(snap, t0, t1):
    """窓 `[t0, t1)` のうち、名前解決の失敗を数えるのに読む `/history` の標本が覆っている秒 (数えられなければ None)。

    数える関数 (`dns_failures_between`) そのものに、失敗の数の代わりに標本の長さ (秒) を入れた写しを渡して出す。
    `res=60` と `res=3600` のどちらを読むか・窓に重なる割合での按分・欄の無い行の扱いが、数えるときと必ず同じになる
    (覆われていない所 = 数えるときに 0 本と数えてしまう所)。
    """
    hist = {str(res): _counts_as_seconds(h) for res in (60, 3600) for h in (history_part(snap, res),) if h is not None}
    covered = dns_failures_between({"history": hist}, t0, t1)
    return None if covered is None else min(covered, t1 - t0)


def profile_history_cover(c, th):
    """前と後の `/profile?res=60` の窓を、同じスナップショットの `/history` がどれだけ覆っているか (T40.5)。

    窓は行が名前解決の失敗を数える窓 (`conn_per_resolved_sides` と同じ。最初の標本から、最後の標本の終わりまで)。
    返すのは `{誰: {"t0", "t1", "covered"}}` (`covered` は覆っている秒)。T39 の行が材料の不足で先に判定できずにするとき
    (`/profile` が無い・12 時間に満たない・`t` の欄や `/history` の `errors_by_cause` が無い) は None。
    """
    sides, _ = _p29_profile_sides(c, th)
    if sides is None or conn_per_resolved_sides(c, th)[0] is None:
        return None
    out = {}
    for who, snap, p, _ in sides:
        win = profile_window(p)
        t0, t1 = win[0], win[1] + (p.get("interval_secs") or 60)
        covered = history_cover_secs(snap, t0, t1)
        if covered is None:
            return None
        out[who] = {"t0": t0, "t1": t1, "covered": covered}
    return out


def span_text(secs):
    """秒を「24.0 時間」か、1 時間に満たなければ「9 分」の形にする。"""
    return f"{secs / 3600:,.1f} 時間" if secs >= 3600 else f"{secs / 60:,.0f} 分"


def history_span_text(snap):
    """スナップショットの `/history` が持っている時間の端 (`res=60` があればそれ、無ければ `res=3600`)。無ければ None。"""
    for res in (60, 3600):
        h = history_part(snap, res)
        ts = [s[0] for s in (h or {}).get("samples") or [] if s and isinstance(s[0], (int, float))]
        if ts:
            return f"`/history?res={res}` は {stamp(min(ts))}〜{stamp(max(ts) + (h.get('interval_secs') or res))}"
    return None


def window_mismatch(c, th):
    """材料の窓が合っていないときの理由の文 (T40.5)。合っているか、T39 の行が先に材料の不足を言うときは None。"""
    cover = profile_history_cover(c, th)
    if cover is None:
        return None
    need = th["conn_history_cover_min"]

    def share(v):
        return v["covered"] / (v["t1"] - v["t0"])

    def percent(v):
        # 切り捨てて書く (98.96% を「99.0%」と書くと、線の 99% に届いているように読める。100.0% は丸ごと覆っているときだけ)
        return f"{math.floor(share(v) * 1000) / 10:.1f}%"
    short = [who for who in ("前", "後") if share(cover[who]) < need]
    if not short:
        return None
    parts = []
    for who in short:
        v = cover[who]
        secs = v["t1"] - v["t0"]
        has = history_span_text(c["a" if who == "前" else "b"])
        parts.append(f"{who}の `/profile?res=60` の窓 ({stamp(v['t0'])}〜{stamp(v['t1'])} の {span_text(secs)}) のうち、"
                     f"{who}のスナップショットの `/history` が覆っているのは {percent(v)} で、"
                     f"{span_text(secs - v['covered'])}が覆われていない" + (f" ({who}のスナップショットの {has})" if has else ""))
    ok = [f"{who}の窓は {percent(cover[who])} 覆われている" for who in ("前", "後") if who not in short]
    return ("材料の窓が合っていない (T40.5): " + "。".join(parts) + "。"
            "覆われていない所の名前解決の失敗 (`/history` の `errors_by_cause.dns`) は数えられず、0 本と数えると名前解決に"
            f"成功した要求が多く出るので、判定しない (窓の {need * 100:g}% 以上が覆われていれば判定する)。"
            + "".join(x + "。" for x in ok) +
            "`/profile?res=60` は、そのスナップショットの `/history` が覆う時間のものを渡すこと (スナップショットより後に取った"
            "ものや、別の日のものは覆われない)。`snapshot-diff.py` は 2 枚を取得の古い順に並べ直し、`--profile-before` を"
            "古い方、`--profile` を新しい方に付ける (`collect-deployed.sh` の `PROFILE_BEFORE` は `--profile-before` になる)")


def _t40_conn_priced(c, th):
    """T40.4・T40.5: `conn` 役の CPU を値付けし合う行 (T39.9) に、同じ版の日ごとのぶれの帯と、材料の窓の見方を入れる。

    実測と出どころは `_t39_conn_priced` と同じ。値付けし合った比が `conn_per_resolved_factor` 倍未満なら満たした、
    それ以上で `conn_day_band_max` 倍以下なら判定できず (同じ版の日ごとのぶれの内)、越えたら届かず。T39 の行が
    判定できずにしたもの (同じ版の中のぶれ = 1 枚の中のぶれの内・材料の不足) は、帯の外でも判定できずのまま。
    前か後の `/profile?res=60` の窓のうち、同じスナップショットの `/history` が覆っているのが `conn_history_cover_min` に
    満たなければ、比を出さずに判定できず (名前解決の失敗を、覆われていない所は 0 本と数えてしまう。T40.5)。
    """
    label, limit, shown, verdict, src = _t39_conn_priced(c, th)
    k, hi = th["conn_per_resolved_factor"], th["conn_day_band_max"]
    label = label.replace("(T39.9。", f"({k} 倍以上 {hi} 倍以下は同じ版の日ごとのぶれの内で判定できず、{hi} 倍を越えたら届かず。"
                          "T40.4。前か後の `/profile?res=60` の窓を同じスナップショットの `/history` が覆っていなければ判定できず。"
                          "T40.5。T39.9。", 1)
    limit = (f"< 前 ×{k} (値付けし合った比。×{k} 以上 ×{hi} 以下は日ごとのぶれの内で判定できず、×{hi} を越えたら届かず。"
             "同じ版の中のぶれ (1 枚の中) の内も、材料の窓が合っていないときも判定できず)")
    mismatch = window_mismatch(c, th)
    if mismatch is not None:
        return (label, limit, "—", UNKNOWN, mismatch)
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


# --- T40.5: 裏の引き直しを、合計 (回/時) で前の期間と比べる (15 番目の行) ---

def refresh_evidence_text():
    """引き直しを合計で読む根拠の文 (行の出どころと前置きが同じ数字を書く。T40.5)。"""
    hours = "・".join(f"{h:.1f}" for h, _ in REFRESH_EVIDENCE)
    rates = "・".join(f"{r:.3f}" for _, r in REFRESH_EVIDENCE)
    lo, hi = REFRESH_EVIDENCE_TOTAL
    first, last = REFRESH_EVIDENCE_NAMES
    return (f"同じ版 0.39.13 で、起動から {hours} 時間の名前 1 つあたりは {rates} 回/時 "
            f"(合計は {lo}〜{hi} 回/時で一定、答えのある名前は {n(first)} → {n(last)} と増える)")


def refresh_startup_text():
    """起動してすぐは引き直しの合計が落ち着いていない根拠の文 (行の理由と前置きが同じ数字を書く。T40.7)。"""
    lo, hi = REFRESH_EVIDENCE_TOTAL
    first = "、".join(f"{at}で {rate} 回/時" for at, rate in REFRESH_STARTUP_EVIDENCE)
    return f"起動から {first}、そのあとは {lo}〜{hi} 回/時"


def span_hours_text(hours):
    """割った時間の書き方: 1 時間に満たなければ分で、それ以上は時間で (T40.7)。"""
    return f"{hours * 60:,.0f} 分" if hours < 1 else f"{hours:,.1f} 時間"


def refresh_hours(c):
    """`counter_rates` が `dns.refreshes` を割る時間 (`(前の時間, 後の時間, その間か)`。時間は時間単位)。

    前は前のスナップショットの起動から。後は、再起動していなければ 2 枚の間、していれば後のスナップショットの起動から。
    """
    windowed = dns_counter(c, "refreshes")[2]
    before = (c["a"].get("uptime_secs") or 0) / 3600.0
    after = (((c["info"] or {}).get("wall_secs") if windowed else c["b"].get("uptime_secs")) or 0) / 3600.0
    return before, after, windowed


def _t40_refresh_total(c, th):
    """T40.5: 裏の引き直しを、**合計 (回/時)** が前の期間の `refresh_per_name_factor` 倍未満かで読む。

    T33.2 の行 (`_p30_refresh_per_name`) が `/dns` の一覧の切れているときにしていた比べ方を、いつも使う: 合計は
    `counter_rates` の `dns.refreshes` ÷ 時間で、前の `refresh_per_name_factor` 倍未満なら満たした (前が 0 なら後も 0 の
    ときだけ)。答えのある名前 1 つあたり (起動からの時間で値が変わる)・表の名前の数あたり・`expired` + `warm_stale` は表示だけ。

    T40.7: 前か後を割った時間 (起動から、または 2 枚の間) が `refresh_min_hours` に満たなければ、数字は出して判定できずにする
    (起動してすぐは合計が落ち着いていない)。
    """
    k, m = th["refresh_per_name_factor"], th["refresh_min_hours"]
    label = (f"裏の引き直しの**合計** (回/時) が前の期間の {k} 倍未満 (T40.5。答えのある名前 1 つあたりは起動からの時間で値が"
             "変わるので表示だけ。表の名前の数あたりと `expired` + `warm_stale` も表示だけ。"
             f"前か後を割った時間が {m:g} 時間に満たなければ判定できず。T40.7)")
    limit = f"< 前 ×{k} (引き直しの合計。割った時間が {m:g} 時間に満たなければ判定できず)"
    rb, ra = counter_rates(c, "refreshes")
    if rb is None or ra is None:
        return (label, limit, "—", UNKNOWN, "前か後の `/status` に `dns.refreshes` か `uptime_secs` が無い")
    shown = f"合計 **{ra:,.1f}** 回/時 (前 {rb:,.1f}" + (f"、**{ra / rb:.2f}** 倍" if rb else "") + ")"
    ok = ra < rb * k if rb else ra == 0
    nb, na = answered_names(c["a"]), answered_names(c["b"])
    extra, why = [], []
    if nb and na:
        pb, pa = rb / nb, ra / na
        extra.append(f"答えのある名前 1 つあたり {pa:.3f} 回/時 (前 {pb:.3f}" + (f"、{pa / pb:.2f} 倍" if pb else "")
                     + f"。答えのある名前 {n(na)}、前 {n(nb)})")
    else:
        for side, snap, v in (("前", c["a"], nb), ("後", c["b"], na)):
            if v is None:
                d = part(snap, "dns")
                why.append(f"{side}の `/dns` の一覧が切れている ({n(len(d['entries']))} / {n(d.get('count'))} 行)"
                           if isinstance(d.get("entries"), list) else f"{side}のスナップショットに `/dns` の部が無い")
            elif not v:
                why.append(f"{side}の `/dns` の一覧に答えのある名前が無い")
        known = [f"{side} {n(v)}" for side, v in (("後", na), ("前", nb)) if v]
        if known:
            extra.append("答えのある名前 " + "・".join(known))
    eb = (part(c["a"], "status").get("dns") or {}).get("entries")
    ea = (part(c["b"], "status").get("dns") or {}).get("entries")
    if eb and ea:
        extra.append(f"表の名前あたり {ra / ea:.2f} 回/時/名前 (前 {rb / eb:.2f}、名前 {n(ea)}、前 {n(eb)})")
    rates = [counter_rates(c, "misses_by_kind", kind) for kind in ("expired", "warm_stale")]
    if not any(b is None or a is None for b, a in rates):
        mb_, ma_ = sum(b for b, _ in rates) * 24, sum(a for _, a in rates) * 24
        extra.append(f"`expired` + `warm_stale` {ma_:,.1f} 回/日 (前 {mb_:,.1f})")
    if extra:
        shown += "。表示だけ: " + "、".join(extra)
    hb, ha, windowed = refresh_hours(c)
    src = (f"`/status` の `dns.refreshes` を 1 時間あたりにした (前は前のスナップショットの起動からの {hb:,.1f} 時間、後は"
           + (f"その間の {ha:,.1f} 時間" if windowed else f"後のスナップショットの起動からの {ha:,.1f} 時間") + ")。")
    src += ("、".join(why) + "ので、表示だけの答えのある名前 1 つあたりは出ない。" if why else
            "表示だけの答えのある名前の数は、`/dns` の一覧 (切れていない) の `addrs` のある行の数。")
    src += ("合計で読む理由 (T40.5): 名前解決の表の名前は起動から増え続けるので、名前 1 つあたりは起動からの時間で値が変わる。"
            + refresh_evidence_text())
    short = [(side, h) for side, h in (("前", hb), ("後", ha)) if h < m]
    if short:
        src = ("・".join(f"{side}を割った時間が {span_hours_text(h)}" for side, h in short)
               + f"で、{m:g} 時間に満たない (T40.7)。短い時間の合計は当てにならないので判定しない "
               f"(起動してすぐは引き直しがまだ落ち着いていない: {refresh_startup_text()})。" + src)
        return (label, limit, shown, UNKNOWN, src)
    return (label, limit, shown, MET if ok else MISSED, src)
