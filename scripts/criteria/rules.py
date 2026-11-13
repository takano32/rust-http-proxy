# 判定表 (`scripts/snapshot_criteria.py`) の**閾と並び** (T34.129 で割った)。
#
# Phase ごとの閾の辞書 (`PHASE14`〜`T37_TH`)・`--criteria` の名前から閾を引く `CRITERIA`・出力を
# 変えない定義 `FROZEN`・名前から判定の関数の並びを引く `RULES` を、Phase の順に組む。判定の関数は
# `criteria/phase*.py` にある。`RULES` の並びは `test_snapshot_criteria.py` の `RulesOrder` が固定している。

from .phase14_18 import (
    _p14_connect_p50, _p14_dns_per_connect, _p14_major_hosts, _p14_overload, _p15_closed_shape,
    _p15_conn_cores, _p15_miss_band, _p15_refresh_rate, _p15_timeout, _p15_watch_host, _p17_cgroup,
    _p17_conn_per_request, _p17_events, _p17_warm_max, _p18_heap, _p18_request_probes,
)
from .phase20_22 import (
    _p20_heap, _p20_timeout, _p20_warm_max, _p20_watch_host, _p21_conn_per_request, _p21_miss_band,
    _p21_request_probes, _p22_connect_errors, _p22_dns_slow, _p22_events, _p22_refresh_and_stale_misses,
    _p22_refresh_rate, _p22_stale_retries,
)
from .phase23_25 import (
    _p22_connect_errors_shown, _p23_connect_errors, _p23_heap, _p23_miss_band, _p23_silent_restarts,
    _p24_connect_tail, _p24_heap, _p24_silent_restarts, _p25_connect_errors, _p25_refresh_per_name,
    _p25_silent_restarts,
)
from .phase26_28 import (
    _p26_cgroup_cpu, _p26_connect_cut, _p26_header_timeouts, _p26_local_blocks, _p26_log_errors,
    _p26_origin_warn_per_minute, _p26_rejected, _p26_role_shares, _p26_rss_gap, _p27_connect_errors,
    _p27_rss_gap, _p28_cgroup_cpu_per_request, _p28_trim_works,
)
from .phase29_30 import (
    _p29_cgroup_cpu_shown, _p29_conn_per_resolved_request, _p29_connect_errors, _p29_dns_slow,
    _p29_idle_cost, _p29_miss_band, _p30_dns_slow, _p30_refresh_per_name,
)
from .t35 import _t35_events, _t35_heap
from .t37 import _t37_dns_slow
from .t36 import (
    _t36_conn_per_resolved_request, _t36_connect_errors, _t36_dns_slow, _t36_heap, _t36_idle_cost,
    _t36_machine_speed,
)


# ---------------------------------------------------------------- (9) 判定表
#
# **1 行 = 1 つの関数** (T15.0 (15))。前の版は Phase 14 の 4 行が `judge()` にべた書きで、
# `CRITERIA` に辞書を足すだけでは「Phase 14 の 4 行が phase15 の閾で出る」誤った表になった。
# いまは `(名前, 閾の文, 実測の取り方, 判定)` を返す関数を `RULES` に並べるだけで足せる。
# 関数が受け取る `c` は下の `context()` が作る辞書で、**材料の部が雪像に無ければ
# `UNKNOWN` (判定できず) を返す**のが規則 (「0 だった」と「読めなかった」を混ぜない)。

# Phase 14 の完了の定義のうち**数字で判定できる 4 行** (TODO.md §5 Phase 14 の末尾と
# Phase 13 の「状態」の表。T14.1 の「デプロイ先の受け入れ基準」と同じ閾値)。
PHASE14 = {
    "dns_per_connect": 0.15,
    "major_miss_rate": 0.05,
    "connect_p50_ms": 6.0,
    "overload": 0,
}

# T15.0 を載せて 24 時間ぶん溜めたあとに読む 6 行 (TODO.md の T15.4 / T15.5 / T15.6)。
PHASE15 = {
    # T15.4: 窓を伸ばすか一律 TTL にするかを決めるための 3 行
    "watch_host": "discord.com",     # Phase 14 で唯一届かなかった相手
    "major_miss_rate": 0.05,
    "refresh_per_warm_hour": 80.0,   # TTL 60 秒 の 3/4 = 45 秒おきに 1 名前 = 80 回/時
    # 窓 3,600 秒 で落ち着くと見込んだ上限。**下の閾は外した** (T15.15 (2)。T15.99 の 0.05 は
    # 幅 0.06〜0.09 を良い方に外れて「届かず」と書かれた。低いのは見込み違いではなく良いこと)
    "miss_max": 0.09,
    # T15.5: 空回りを直したあとに「別の空回りが無い」ことを見る 2 行
    "conn_cores": 0.01,
    "closed_tolerance": 0.10,
    # T15.6: 締め切りを 10 秒にしたら `timeout` が増えないか
    "timeout_tolerance": 0.10,
}
# T17.0a: Phase 17 の版を 24 時間走らせたあとに読む 8 行 (TODO.md の T17.99 の完了の定義)。
# 前の 4 行は phase15 の関数をそのまま使う (閾も同じ値)。後ろの 4 行は T16.99 で手で引いた
# 判定 (i)〜(iii) と、`/events` の種類別 件/時 (`dns_slow` の 7 倍を道具が見つけるため)
PHASE17 = {
    "watch_host": PHASE15["watch_host"],
    "major_miss_rate": PHASE15["major_miss_rate"],
    "refresh_per_warm_hour": PHASE15["refresh_per_warm_hour"],
    "miss_max": PHASE15["miss_max"],
    "timeout_tolerance": PHASE15["timeout_tolerance"],
    # conn 役の CPU/要求 が前の何倍までなら「桁で悪くなっていない」か (T15.12 段 7 の基準)
    "conn_per_request_factor": 1.3,
    # `MAX_WARM` (`crates/net-dns/src/dns.rs`)。最大がこれに届いたら枠の取り合いがある
    "warm_max_limit": 32,
    # `/events` の anomaly の種類ごとの閾 (起動からの 件/時)。**ここに無い種類は表示だけ**
    "events_per_hour": {"dns_slow": 0.1},
    # cgroup の起動からの user / sys (コア数) が前の何倍までなら「同じ桁」か
    "cgroup_factor": 10.0,
}
# T18.2: Phase 18 の版を 24 時間走らせたあとに読む 10 行 (TODO.md の T18.0 の「次の完了の定義」)。
# 前の 8 行は phase17 の関数と閾をそのまま使う。後ろの 2 行は T17.99 の但し書き (d) と (e) の物差し
PHASE18 = dict(
    PHASE17,
    # (g) `heap_used + mmap` の 0 時間の雪像からの増え (MB = 10^6 B)。RSS と `heap_free` は
    # バーストの大きさで決まるので閾を置かない (T18.0 (1)。T17.99 の版は 18.0 → 19.9 で +1.9)
    heap_growth_mb=5.0,
    # (h) `v4_first` の間に利用者の経路で IPv6 を探った回数 (T18.1 の `ipv6.request_probes`)
    request_probes=0,
)
# T20.1: phase18 の 10 行の**物差しを直したもの** (T18.99 で分かった 2 つ)。閾の値は phase18 と同じ。
# (g) はメモリのキャッシュ (`memory.cache_memory`) を引いてから比べる (上限まで使ってよい作りなので、
# 入ったぶんは漏れではない)。`discord.com` のミス率・`dns_warm` の最大と `warm_evicted`・`timeout` の
# 3 行は**表示だけ** (閾を置くと使われ方の変化で毎回落ちる。TODO.md Phase 20 の「候補 4 つ」)
PHASE20 = dict(PHASE18)
# T21.2: phase20 の 10 行のうち、T20.99 で「届かず」になった 3 行の読み方を直したもの。閾の値は phase20 と同じ。
# 平常時のミスの行は**表示だけ** (宛先の数で動く使われ方の数字)。conn 役の CPU/要求 は、前か後の
# `/profile` が下の時間に満たなければ「参考」(集計に数えない)。`ipv6.request_probes` は、最後に探った
# 時刻が 0 時間の雪像の取得から下の時間以内なら「満たした」(起動直後は canary が IPv6 をまだ見ていない)
PHASE21 = dict(
    PHASE20,
    # `/profile?res=60` で 720 標本。5 秒の雪像の部はいつも 1 時間に満たないので「参考」になる
    profile_min_hours=12.0,
    request_probe_grace_hours=3.0,
)
# T22.1: phase21 の 10 行に 22a (T22.2・T22.4) の行を足したもの。**平常時の切り方もこの定義だけ違う**:
# 1 時間の本数が「`BURST_PER_HOUR` (T14.0 の 300 本) と、**その期間の 1 時間の本数の中央値の
# `burst_median_k` 倍**の大きい方」以上の標本をバーストとする (`normal_limit`)。tokyo の 10-15 → 10-17 は
# 量が 12 倍 (中央値 780 本/時) で、300 本で切ると 29 標本のうち 23 が「バースト」になった。量の少ない期間
# (中央値 × k が 300 に届かない) では T14.0 の切り方そのもの
PHASE22 = dict(
    PHASE21,
    # k は雪像 4 組で決めた (TODO.md T22.1): 量の多い tokyo の後の期間は最大 / 中央値が 2.0 (1,564 / 780) で、
    # k ≤ 2 だと量の揃った日のいちばん多い 1 時間を切る。量の少ない 3 期間 (nagoya 10-08 は中央値 44、
    # tokyo 10-15 は 22〜23) は k ≤ 6 なら 300 本の切り方と同じ標本が残る。その間でいちばん小さい 3
    burst_median_k=3,
    # T22.2: `dns_slow` が立つのは窓のミスの合計がこれ以上のときだけ (`anomaly.rs` の `DNS_MIN_SUM_MS`)
    dns_slow_min_sum_ms=1000,
    # T22.4 の「既定を 3,600 に戻す」2 つの条件 (TODO.md T22.4 の結果、親の判断 2026-10-17):
    # `stale_retries` が `stale_hits` のこの割合以上、または下の原因の**接続 1 本あたり**が前の期間のこの倍以上
    # なら戻す (件/時ではない。量が 12 倍になる tokyo では件/時は悪化でなくても当たる。親の判断 2026-10-17)
    stale_retry_share=0.01,
    connect_error_factor=1.5,
    connect_error_causes=("refused", "unreachable", "timeout"),
    # T22.4 の段 1 の目安: 引き直しの回/時が前の期間のこの倍以下、`expired` + `warm_stale` の増えが 1 日これ以内
    refresh_factor=0.5,
    stale_miss_growth_per_day=10,
)
# T23.1: phase22 の 14 行を土台にしたもの。閾の値は phase22 と同じで、**エラーの行の分母**と
# **heap の行の「判定できず」の書き方**だけが違う (TODO.md Phase 23 の (1)〜(3))。
# (a) 接続のエラーは `refused` + `unreachable` + `timeout` の**名前解決に成功した試行あたり**
#     (分母 = `/recent` の connect の行から `dns` の失敗を外した本数)。phase22 の「接続 1 本あたり」は
#     使われ方で振れる (T22.99 の 7.80 倍) ので、表示だけの行として残す。
# (b) dns のミスの率は、同じ分母 (`/recent`) でも並べる (表示だけ)。
# (c) heap の行は、0 時間の雪像と起動が違うときに `/events` の `start` の時刻を添える。
# (d) 無言の再起動 (T23.2) の行を最後に足す。
PHASE23 = dict(PHASE22)
# T24.2: phase24 は phase23 の 16 行を**そのまま持ち越し**、2 行だけ読み方を変える (TODO.md Phase 24 の (1)(2))。
# (a) heap の行 (9 番目) は、`/status` の `memory.baseline` (起動から 60 秒の基準。T24.4) があれば
#     **それとの差**で読み、無ければ phase23 と同じ 0 時間の雪像との差にする (`_p24_heap`)。
# (b) 無言の再起動の行 (16 番目) は、`start` の `boot` が変わった回 (ホストの再起動) を**理由つきで数から外し**、
#     同じ `boot` のまま起動し直した回 (こちらが原因) だけを 0 回の判定に使う (`_p24_silent_restarts`)。
# 閾の値は phase23 と同じなので、辞書は写しで足りる (T24.3 で 1 つだけ足した。下)。
PHASE24 = dict(PHASE23)
# T24.3: phase24 の**17 番目**に、確立の尾を「そのときのホストの RTT + N ms」で読む行を足した
# (`_p24_connect_tail`)。N は T14.99 の決まりの 5 ms で、**固定の 6 ms ではない**。
# RTT の測れない雪像 (`rtt_ms.origin` が `null`) では判定しない。
PHASE24["connect_tail_ms"] = 5.0
# T27.0: phase25 は phase24 の 17 行を**そのまま持ち越し**、T25.99 で物差しの穴だった 3 行だけ読み方を変える。
# (a) 無言の再起動の行 (16 番目) は、**前の雪像の取得より後の `start` だけ**を数える (それより前は前の締めで
#     見た。`boot` の無い古い `start` が 512 件の環に残る間ずっと「判定できず」になるのを防ぐ)。
# (b) 接続のエラー (名前解決に成功した試行あたり) の行 (13 番目) は、前か後の試行がこの本数に満たなければ
#     「判定できず」 (T25.99 の前は日次の雪像の 1 時間 581 本で 0 件 = どんな後でも「届かず」になった)。
# (c) 引き直しの行 (15 番目) は、keep-warm を止めた版どうしでは 0.5 倍の目安に意味が無い (T24.1 の段 2 は
#     入れていない) ので、**1 名前あたりの引き直し (回/時) が前のこの倍未満**で読む。通算の回/時と
#     `expired` + `warm_stale` の回/日は同じ行に表示だけで並べる (名前の数で動く使われ方の数字)。
PHASE25 = dict(PHASE24)
PHASE25["min_recent_attempts"] = 1000
PHASE25["refresh_per_name_factor"] = 1.5
CRITERIA = {"phase14": PHASE14, "phase15": PHASE15, "phase17": PHASE17, "phase18": PHASE18,
            "phase20": PHASE20, "phase21": PHASE21, "phase22": PHASE22, "phase23": PHASE23,
            "phase24": PHASE24, "phase25": PHASE25}
# **出力を 1 文字も変えない定義** (過去の判定を同じ命令で出し直せるように)。T20.1 で差分の表に
# 足した行 (成功した接続だけの確立時間) は、`--criteria` がこの 4 つのときは出さない
FROZEN = ("phase14", "phase15", "phase17", "phase18")


def replaced(base, at):
    """`base` の並びのうち、`at` の**番目 (1 から数える)** だけを差し替えた並びを返す (T.34.129)。

    値が tuple なら、その番目に複数の行を入れる (前の行を残してすぐ後ろに足すとき)。下の注釈の
    「13 番目」と同じ数え方で書けるので、添字の切り貼り (`[:12] + (…,) + [13:]`) を読み解かなくてよい。
    """
    assert all(1 <= i <= len(base) for i in at), (len(base), sorted(at))
    out = []
    for i, rule in enumerate(base, 1):
        new = at.get(i, rule)
        out.extend(new if isinstance(new, tuple) else (new,))
    return tuple(out)


RULES = {
    "phase14": (_p14_dns_per_connect, _p14_major_hosts, _p14_connect_p50, _p14_overload),
    "phase15": (_p15_watch_host, _p15_refresh_rate, _p15_miss_band,
                _p15_conn_cores, _p15_closed_shape, _p15_timeout),
    "phase17": (_p15_watch_host, _p15_refresh_rate, _p15_miss_band, _p15_timeout,
                _p17_conn_per_request, _p17_warm_max, _p17_events, _p17_cgroup),
}
# phase18 = phase17 の 8 行 + (g) + (h)
RULES["phase18"] = RULES["phase17"] + (_p18_heap, _p18_request_probes)
# phase20 = phase18 と同じ並びの 10 行。3 行が表示だけ、(g) がキャッシュを引く版、残りの 6 行はそのまま
RULES["phase20"] = (_p20_watch_host, _p15_refresh_rate, _p15_miss_band, _p20_timeout,
                    _p17_conn_per_request, _p20_warm_max, _p17_events, _p17_cgroup,
                    _p20_heap, _p18_request_probes)
# phase21 = phase20 と同じ並びの 10 行。ミスの行が表示だけ、conn 役の行が短い材料で参考、IPv6 の行が起動直後を許す
RULES["phase21"] = (_p20_watch_host, _p15_refresh_rate, _p21_miss_band, _p20_timeout,
                    _p21_conn_per_request, _p20_warm_max, _p17_events, _p17_cgroup,
                    _p20_heap, _p21_request_probes)
# phase22 = phase21 の 10 行 (引き直しの行は warm 0 でも読める版、`/events` の行は表示だけ) + T22.2 の 1 行 + T22.4 の 3 行。
# 平常時の切り方も違う (`PHASE22` の `burst_median_k`)。T22.3 は段 1 で止めたので行を作らない
RULES["phase22"] = (replaced(RULES["phase21"], {2: _p22_refresh_rate, 7: _p22_events})
                    + (_p22_dns_slow, _p22_stale_retries, _p22_connect_errors,
                       _p22_refresh_and_stale_misses))
# phase23 = phase22 の 14 行を土台に、4 か所だけ違う (T23.1)。並びは phase22 と同じで、
# ミスの行 (3 番目)・heap の行 (9 番目)・接続のエラーの行 (13 番目) を差し替え、
# phase22 の接続のエラーの行をそのすぐ後ろに**表示だけ**で残し、最後に無言の再起動の行 (T23.2) を足す
RULES["phase23"] = (replaced(RULES["phase22"], {3: _p23_miss_band, 9: _p23_heap,
                                                13: (_p23_connect_errors, _p22_connect_errors_shown)})
                    + (_p23_silent_restarts,))
# T24.4 の heap の行 (`_p24_heap`) は、起動から 60 秒の基準 `memory.baseline` があればそれとの差、無ければ
# `_p23_heap` をそのまま返す (基準を混ぜない)。T24.2: phase24 = phase23 の 16 行の**9 番目と 16 番目だけ**を
# 差し替えたもの。並びは phase23 と同じ (10〜15 番目は確立・安定性などの 6 行)。
# T24.3: 17 番目 (末尾) に、確立の尾を「そのときのホストの RTT + N ms」で読む行を足した
# (`_p24_connect_tail`)。**phase23 以前の並びは 1 つも動かない**。
RULES["phase24"] = replaced(RULES["phase23"], {9: _p24_heap, 16: (_p24_silent_restarts, _p24_connect_tail)})


# phase25 = phase24 の 17 行の **13・15・16 番目だけ**を差し替えたもの (並びは phase24 と同じ。T27.0)。
# **phase24 以前の並びは 1 つも動かない**
RULES["phase25"] = replaced(RULES["phase24"], {13: _p25_connect_errors, 15: _p25_refresh_per_name,
                                               16: _p25_silent_restarts})


# T29.10: phase26 = phase25 の 17 行 + 18〜25 番目 (T29.15 で 26 番目)。**phase25 以前の並びは 1 つも動かない**
PHASE26 = dict(PHASE25)
# `/log` の error の行の上限 (T28.99 の (3)「0〜数件」。0.29.35 の起動 2 分では 2 行)
PHASE26["log_error_max"] = 5
# T29.25 の受け入れ基準 (24 時間で `rss − (heap_used + mmap + cache_memory)` < 10 MB)
PHASE26["rss_gap_mb"] = 10.0
# T29.20〜22 で減らす裏の役割 (0.28.48 の tokyo で `profile-sample` 32.5%・`cache-probe` 23.4%)
PHASE26["idle_roles"] = ("profile-sample", "cache-probe")
# T29.15: オリジン側の失敗の WARN を絞る原因の数 (`crates/metrics-core` の `ORIGIN_WARN_REASONS` = 8 + 2)
PHASE26["origin_warn_per_minute"] = 10
CRITERIA["phase26"] = PHASE26
RULES["phase26"] = RULES["phase25"] + (
    _p26_header_timeouts, _p26_log_errors, _p26_local_blocks, _p26_connect_cut, _p26_rejected,
    _p26_rss_gap, _p26_cgroup_cpu, _p26_role_shares, _p26_origin_warn_per_minute)


# phase27 = phase26 の 26 行のうち 13 番目 (接続のエラー) と 23 番目 (RSS) だけ差し替えたもの (T30.0・T30.1)。
# **phase26 以前の並びと文面は 1 つも動かない**
RULES["phase27"] = replaced(RULES["phase26"], {13: _p27_connect_errors, 23: _p27_rss_gap})
PHASE27 = dict(PHASE26)
# 前も後も「名前解決に成功した試行」がこの本数に満たなければ、接続のエラーの行は判定しない (T30.0)
PHASE27["min_resolved_attempts"] = 1000
CRITERIA["phase27"] = PHASE27


# phase28 = phase27 の 26 行のうち 23 番目 (RSS) と 24 番目 (cgroup の CPU) だけ差し替えたもの (T31.0・T31.1)。
# **phase27 以前の並びと文面は 1 つも動かない**
RULES["phase28"] = replaced(RULES["phase27"], {23: _p28_trim_works, 24: _p28_cgroup_cpu_per_request})
PHASE28 = dict(PHASE27)
# cgroup の CPU/要求 が前の版のこの倍率未満なら満たす (T31.0)
PHASE28["cpu_per_request_factor"] = 1.1
CRITERIA["phase28"] = PHASE28


# phase29 = phase28 の 26 行のうち 6 行を差し替えたもの (T32.16・T32.17・T32.29・T32.30)。並びは同じ:
#   3 番目 (平常時の名前解決のミス。表示だけ) → 名前解決の失敗を除いた率も添える、5 番目 (`conn` 役の CPU/要求) → 名前解決に成功した要求あたり、11 番目 (`dns_slow`) → 成功したミスで立った件/時、
#   13 番目 (接続のエラー) → 起動からの `/history` の 443・80、24 番目 (cgroup の CPU/要求) → 表示だけ、
#   25 番目 (裏の役割の割合) → 裏の役割の us/秒。**phase28 以前の並びと文面は 1 つも動かない**
RULES["phase29"] = replaced(RULES["phase28"], {
    3: _p29_miss_band, 5: _p29_conn_per_resolved_request, 11: _p29_dns_slow, 13: _p29_connect_errors,
    24: _p29_cgroup_cpu_shown, 25: _p29_idle_cost})
PHASE29 = dict(PHASE28)
# `conn` 役の CPU/名前解決に成功した要求 と、裏の役割の us/秒 の倍率 (前の版のこの倍未満なら満たす。T32.29)
PHASE29["conn_per_resolved_factor"] = 1.1
PHASE29["idle_cost_factor"] = 1.1
# `dns_slow` の「成功したミスで立った」の回数の下限 (`anomaly.rs` の `DNS_MIN_MISSES`。T32.16)
PHASE29["dns_min_misses"] = 3
CRITERIA["phase29"] = PHASE29


# phase30 = phase29 の 26 行のうち 11 番目 (`dns_slow`。T33.1) と 15 番目 (1 名前あたりの引き直し。T33.2) を
# 差し替えたもの。並びは同じ。**phase29 以前の並びと文面は 1 つも動かない**
RULES["phase30"] = replaced(RULES["phase29"], {11: _p30_dns_slow, 15: _p30_refresh_per_name})
PHASE30 = dict(PHASE29)
# `dns_slow` が立った窓の成功したミスの平均の倍率 (前の版のこの倍未満なら満たす。T33.1)
PHASE30["dns_slow_avg_factor"] = 1.5
CRITERIA["phase30"] = PHASE30


# T35 = phase30 の 26 行のうち 7 番目 (`/events` の anomaly。表示だけ) を、要約 (`snapshot-summary.py`) と
# 同じ関数で数える行に (T35.9)、9 番目 (heap) をキャッシュの持ち物 (本文の確保の端数と索引) を引いた残りで
# 読む行に (T35.11) 差し替えたもの。並びは同じ。**phase30 以前の並びと文面は 1 つも動かない**
RULES["T35"] = replaced(RULES["phase30"], {7: _t35_events, 9: _t35_heap})
T35_TH = dict(PHASE30)
CRITERIA["T35"] = T35_TH


# T36 = T35 の 26 行のうち 9 番目 (heap) を、キャッシュの持ち物に加えて名前解決の表の持ち物 (`dns.entries` ×
# 1 名前あたりの大きさ) も引いた残りで読む行に (T36.1)、11 番目 (`dns_slow`) と 13 番目 (接続のエラー) を前の期間が
# 0 件でも読める下限つきの行に (T36.3)、5 番目 (`conn` 役の CPU) と 25 番目 (暇なときの費用) を機械の速さの物差し
# (`history` 役の us/秒) で前と後を割ってから比べる行に (T36.2) 差し替え、27 番目 (末尾) に物差しの前後の比を出す
# 表示だけの行を足したもの (T36.2)。1〜26 番目の並びは同じ。**T35 以前の並びと文面は 1 つも動かない**
RULES["T36"] = replaced(RULES["T35"], {5: _t36_conn_per_resolved_request, 9: _t36_heap, 11: _t36_dns_slow,
                                       13: _t36_connect_errors, 25: _t36_idle_cost}) + (_t36_machine_speed,)
T36_TH = dict(T35_TH)
CRITERIA["T36"] = T36_TH

# T37 = T36 の 27 行のうち 11 番目 (`dns_slow` が立った窓の成功したミスの平均) を、前の版との比ではなく、いつも
# 後の平均の絶対値 (500 ms 未満) で読む行に差し替えたもの (T37.1)。並びは同じ。**T36 以前の並びと文面は 1 つも動かない**
RULES["T37"] = replaced(RULES["T36"], {11: _t37_dns_slow})
T37_TH = dict(T36_TH)
CRITERIA["T37"] = T37_TH

# 判定表の名前は「足したタスクの番号」(2026-11-07、利用者の指示。T35.14)。phase14〜phase30 は過去の締めを
# 同じ命令で出し直せるように名前を残し、タスクの番号の名前を**別名**として足す (中身と出力は同じもの)。
# phase24 まではタスクと同じ番号、その後は phase25 = T27・phase26 = T29・phase27 = T30・phase28 = T31・
# phase29 = T32・phase30 = T33。
CRITERIA_ALIASES = {
    "T14": "phase14", "T15": "phase15", "T17": "phase17", "T18": "phase18", "T20": "phase20",
    "T21": "phase21", "T22": "phase22", "T23": "phase23", "T24": "phase24", "T27": "phase25",
    "T29": "phase26", "T30": "phase27", "T31": "phase28", "T32": "phase29", "T33": "phase30",
}


def criteria_name(name):
    """`--criteria` に渡された名前を、判定表の本当の名前に引く (別名なら元の phaseNN に)。"""
    return CRITERIA_ALIASES.get(name, name)
