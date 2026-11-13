# 判定表 (`scripts/snapshot_criteria.py`) の **T37 の行** (T37.1)。
#
# T36 の 27 行のうち、`dns_slow` が立った窓の名前解決に成功したミスの平均の行 (11 番目) を、前の版との比ではなく
# **いつも後の平均の絶対値 (500 ms 未満)** で読む行に差し替える。並べるのは `RULES` (`criteria/rules.py`)。
# **T36 以前の行と文面は 1 つも動かさない**。
#
# なぜ (T37.1。2026-11-13、利用者の選択「絶対値 500 ms 未満に」): 0.36.34 の締め (T36.99) で、この行は 139 → 225 ms
# (1.62 倍) の届かずだった。立った窓はどれも 443・80 宛ての成功したミスが 3〜12 回しかない小さい窓で、窓ごとの
# 平均は 144〜383 ms。2026-10-27〜11-12 に立った窓も、3〜4 回の窓では 300〜900 ms がよくある。前の版の 139 ms は
# たまたま低かった 3 窓の平均で、小さい母数どうしの比はぶれで閾を越える。名前解決の遅さは上流のリゾルバが決め、
# T36 は名前解決を変えていない。閾は T36.3 で前の版に窓が無いときに使っていた 500 ms をそのまま使う。

from .common import MET, MISSED, UNKNOWN
from .phase29_30 import _p30_dns_slow, dns_slow_windows
from .t36 import DNS_SLOW_AVG_FLOOR_MS


def _t37_dns_slow(c, th):
    """T37.1: `dns_slow` が立った窓の、名前解決に成功したミスの平均を、いつも絶対値 (500 ms 未満) で読む。

    実測・前の版の値・出どころの文は `_p30_dns_slow` と同じ (前の版の値と倍率は表示だけ)。後の期間に成功したミスで
    立った窓が無ければ満たした。後の平均が確かに出せない (`dns_fail_ms_sum` の無い版・窓が重ならない) ときは判定できず。
    """
    _, _, shown, verdict, src = _p30_dns_slow(c, th)
    floor = DNS_SLOW_AVG_FLOOR_MS
    label = (f"`dns_slow` が立った窓の、名前解決に**成功した**ミスの平均が {floor} ms 未満 "
             "(T37.1。前の版の値と倍率、件/時は表示だけ)")
    limit = f"< {floor} ms (立った窓の成功したミスの平均)"
    aft = dns_slow_windows(c["b"], th)
    if aft is None or not aft["exact"] or not aft["hours"]:
        return (label, limit, shown, UNKNOWN, src)
    if not aft["fired"] or not aft["misses"]:
        return (label, limit, shown, MET, src)
    a = aft["lo"] / aft["misses"]
    return (label, limit, shown, MET if a < floor else MISSED,
            f"後の平均 {a:,.0f} ms を絶対値の閾 {floor} ms と比べた (T37.1。前の版との比は小さい窓どうしでぶれるので"
            f"表示だけ)。" + src)
