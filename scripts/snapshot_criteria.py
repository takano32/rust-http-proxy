#!/usr/bin/env python3
# `scripts/snapshot-diff.py --criteria` の部 (**Phase の完了の定義に対する判定表**。TODO.md T20.1)。
#
# `snapshot-diff.py` が 1,959 行になったので、判定の部だけをここへ割った (T18.2 の申し送り)。
# 持っているのは `PHASE14`〜`PHASE24` の閾・`CRITERIA`・`RULES`・判定の 1 行 = 1 つの関数・`judge()`・
# 判定表の印字 (`render_criteria()`) と、**判定の関数と差分の両方が使う小物**
# (`part` `stamp` `n` `ms` `ratio` `restart_info` `norm_history` `merged_history` `miss_rate`)。
# 小物までこちらにあるのは、`snapshot-diff.py` は名前に `-` があって `import` できないため
# (共有するものは import できる側に置く)。`snapshot-diff.py` は全部を同じ名前で取り込み直すので、
# 使い方・出力・`sd.CRITERIA` のようなテストから見える名前は割る前と変わらない。
#
# 3,454 行になったので、中身を `scripts/criteria/` に割っている (T.34.124〜):
#   `common.py`      小物 (`/history` の欄の名前・平常時の閾・判定の欄の文言・読む/印字の小物・履歴を重ねる)
#   `restarts.py`    再起動の部 (`restart_info` と無言の再起動)
#   `phase14_18.py`  Phase 14〜18 の行とその材料を読む小物
#   `phase20_22.py`  Phase 20〜22 の行 (`display_only` を含む) とその材料を読む小物
#   `phase23_25.py`  Phase 23〜25 の行 (T24.3 の確立の尾を含む) とその材料を読む小物
#   `phase26_28.py`  Phase 26〜28 の行とその材料を読む小物
# **このファイルが全部を同じ名前で取り込み直す** (下の `from criteria.… import`) ので、取り込む側から
# 見える名前は割る前と同じ。
#
# 単体では動かさない (入口は `snapshot-diff.py`)。依存は Python 3 の標準ライブラリだけ。

import os
import sys

# 読む部分は `scripts/proxydata.py` (`status-diff.py` / `snapshot-diff.py` と共有)
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proxydata import CAUSE_NAMES  # noqa: E402

# 割った中身 (`scripts/criteria/`。T.34.124〜)。**同じ名前で取り込み直す**ので、ここから取り込む側
# (`snapshot-diff.py` の一覧・`snapshot-summary.py`・試験の `sc.*`) は割る前と変わらない
from criteria.common import (  # noqa: E402,F401
    BURST_MEDIAN_K, BURST_PER_HOUR, HFIELDS, MAJOR_HOSTS, MET, MISSED, REFERENCE, SHOWN, UNKNOWN, change,
    merged_history, miss_rate, ms, n, norm_history, normal_limit, part, per_hour, ratio, stamp,
)
from criteria.restarts import (  # noqa: E402,F401
    START_BOOT, START_VERSION, boot_split_text, restart_info, silent_restart_text, silent_restarts,
    start_boot, start_version,
)
from criteria.phase14_18 import (  # noqa: E402,F401
    _p14_connect_p50, _p14_dns_per_connect, _p14_major_hosts, _p14_overload, _p15_closed_shape,
    _p15_conn_cores, _p15_miss_band, _p15_refresh_rate, _p15_timeout, _p15_watch_host, _p17_cgroup,
    _p17_conn_per_request, _p17_events, _p17_warm_max, _p18_heap, _p18_request_probes, anomaly_rates,
    cgroup_since_start, daily_band, dns_name_refreshes, mb, memory_of, minute_parts, profile_cost,
    profile_role_cores, profile_source, tunnel_closes, uptime_text,
)
from criteria.phase20_22 import (  # noqa: E402,F401
    MISS_KINDS, _p20_heap, _p20_timeout, _p20_warm_max, _p20_watch_host, _p21_conn_per_request,
    _p21_miss_band, _p21_miss_band_measured, _p21_request_probes, _p22_connect_errors, _p22_dns_slow,
    _p22_events, _p22_refresh_and_stale_misses, _p22_refresh_rate, _p22_stale_retries, counter_rates,
    display_only, dns_counter, dns_slow_events, miss_kinds, profile_span,
)
from criteria.phase23_25 import (  # noqa: E402,F401
    DNS_FAILURES, _p22_connect_errors_shown, _p23_connect_errors, _p23_heap, _p23_miss_band,
    _p23_miss_band_measured, _p23_silent_restarts, _p24_connect_tail, _p24_heap, _p24_silent_restarts,
    _p25_connect_errors, _p25_refresh_per_name, _p25_silent_restarts, recent_attempts, recent_connect_rtt,
    restarts_since, run_start, two_starts, weighted_quantile,
)
from criteria.phase26_28 import (  # noqa: E402,F401
    ORIGIN_WARN_PREFIXES, _p26_cgroup_cpu, _p26_connect_cut, _p26_header_timeouts, _p26_local_blocks,
    _p26_log_errors, _p26_origin_warn_per_minute, _p26_rejected, _p26_role_shares, _p26_rss_gap,
    _p27_connect_errors, _p27_rss_gap, _p28_cgroup_cpu_per_request, _p28_trim_works, per_day,
    recent_dns_ports, recent_window, role_share, snap_hours,
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
RULES["phase22"] = ((RULES["phase21"][0], _p22_refresh_rate) + RULES["phase21"][2:6] + (_p22_events,)
                    + RULES["phase21"][7:]
                    + (_p22_dns_slow, _p22_stale_retries, _p22_connect_errors,
                       _p22_refresh_and_stale_misses))
# phase23 = phase22 の 14 行を土台に、4 か所だけ違う (T23.1)。並びは phase22 と同じで、
# ミスの行 (2 番目)・heap の行 (9 番目)・接続のエラーの行 (13 番目) を差し替え、
# phase22 の接続のエラーの行をそのすぐ後ろに**表示だけ**で残し、最後に無言の再起動の行 (T23.2) を足す
RULES["phase23"] = ((RULES["phase22"][:2] + (_p23_miss_band,) + RULES["phase22"][3:8]
                     + (_p23_heap,) + RULES["phase22"][9:12]
                     + (_p23_connect_errors, _p22_connect_errors_shown)
                     + RULES["phase22"][13:] + (_p23_silent_restarts,)))
# T24.4 の heap の行 (`_p24_heap`) は、起動から 60 秒の基準 `memory.baseline` があればそれとの差、無ければ
# `_p23_heap` をそのまま返す (基準を混ぜない)。T24.2: phase24 = phase23 の 16 行の**9 番目と 16 番目だけ**を
# 差し替えたもの。並びは phase23 と同じ (`RULES["phase23"][9:15]` は確立・安定性などの 6 行)。
# T24.3: 17 番目 (末尾) に、確立の尾を「そのときのホストの RTT + N ms」で読む行を足した
# (`_p24_connect_tail`)。**phase23 以前の並びは 1 つも動かない**。
RULES["phase24"] = (RULES["phase23"][:8] + (_p24_heap,) + RULES["phase23"][9:15]
                    + (_p24_silent_restarts, _p24_connect_tail))


# phase25 = phase24 の 17 行の **13・15・16 番目だけ**を差し替えたもの (並びは phase24 と同じ。T27.0)。
# **phase24 以前の並びは 1 つも動かない**
RULES["phase25"] = (RULES["phase24"][:12] + (_p25_connect_errors,) + RULES["phase24"][13:14]
                    + (_p25_refresh_per_name, _p25_silent_restarts) + RULES["phase24"][16:])


# T.29.10: phase26 = phase25 の 17 行 + 18〜25 番目 (T.29.15 で 26 番目)。**phase25 以前の並びは 1 つも動かない**
PHASE26 = dict(PHASE25)
# `/log` の error の行の上限 (T.28.99 の (3)「0〜数件」。0.29.35 の起動 2 分では 2 行)
PHASE26["log_error_max"] = 5
# T.29.25 の受け入れ基準 (24 時間で `rss − (heap_used + mmap + cache_memory)` < 10 MB)
PHASE26["rss_gap_mb"] = 10.0
# T.29.20〜22 で減らす裏の役割 (0.28.48 の tokyo で `profile-sample` 32.5%・`cache-probe` 23.4%)
PHASE26["idle_roles"] = ("profile-sample", "cache-probe")
# T.29.15: オリジン側の失敗の WARN を絞る原因の数 (`crates/metrics-core` の `ORIGIN_WARN_REASONS` = 8 + 2)
PHASE26["origin_warn_per_minute"] = 10
CRITERIA["phase26"] = PHASE26
RULES["phase26"] = RULES["phase25"] + (
    _p26_header_timeouts, _p26_log_errors, _p26_local_blocks, _p26_connect_cut, _p26_rejected,
    _p26_rss_gap, _p26_cgroup_cpu, _p26_role_shares, _p26_origin_warn_per_minute)


# phase27 = phase26 の 26 行のうち 13 番目 (接続のエラー) と 23 番目 (RSS) だけ差し替えたもの (T.30.0・T.30.1)。
# **phase26 以前の並びと文面は 1 つも動かない**
RULES["phase27"] = (RULES["phase26"][:12] + (_p27_connect_errors,) + RULES["phase26"][13:22]
                    + (_p27_rss_gap,) + RULES["phase26"][23:])
PHASE27 = dict(PHASE26)
# 前も後も「名前解決に成功した試行」がこの本数に満たなければ、接続のエラーの行は判定しない (T.30.0)
PHASE27["min_resolved_attempts"] = 1000
CRITERIA["phase27"] = PHASE27


# phase28 = phase27 の 26 行のうち 23 番目 (RSS) と 24 番目 (cgroup の CPU) だけ差し替えたもの (T.31.0・T.31.1)。
# **phase27 以前の並びと文面は 1 つも動かない**
RULES["phase28"] = (RULES["phase27"][:22] + (_p28_trim_works, _p28_cgroup_cpu_per_request) + RULES["phase27"][24:])
PHASE28 = dict(PHASE27)
# cgroup の CPU/要求 が前の版のこの倍率未満なら満たす (T.31.0)
PHASE28["cpu_per_request_factor"] = 1.1
CRITERIA["phase28"] = PHASE28


# --- Phase 29 (T.32.16・T.32.29・T.32.30。phase28 の 26 行のうち 5 行を、名前解決の失敗の割合で動かない形にしたもの) ---
#
# T.31.99 (0.30.4 の 48 時間) で、名前解決に失敗した要求 (ほとんどが負のキャッシュで即 502) の割合が
# 前 40% → 後 66% に増え、要求あたりの行がその混ざり方で動いた。接続のエラーの行は `/recent` の末尾
# 4.3 時間 (48.3 時間の 9%) だけで読んでいて、起動からの `/history` では逆の結論だった。`dns_slow` の行は
# コードが T22.2 から合計 1,000 ms を条件にしているので構造上落ちず、名前解決の失敗の遅さで鳴っていた。

def history_part(snap, res):
    """雪像の `/history?res=…` の部 (`{"keys", "samples", "causes"}`。無ければ None)。"""
    h = (snap.get("history") or {}).get(str(res))
    if not isinstance(h, dict) or not isinstance(h.get("keys"), list) or not isinstance(h.get("samples"), list):
        return None
    return h


def profile_window(p):
    """`/profile?res=60` の材料の `(最初の t, 最後の t)`。`t` の欄が無ければ None。"""
    keys = p.get("keys") or []
    rows = p.get("samples") or []
    if "t" not in keys or not rows:
        return None
    ti = keys.index("t")
    ts = [r[ti] for r in rows if ti < len(r) and isinstance(r[ti], (int, float))]
    return (min(ts), max(ts)) if ts else None


def _dns_failures_in(h, t0, t1):
    """`/history` の部 1 つの `errors_by_cause.dns` を、窓 `[t0, t1)` に重なる割合で按分して足す (欄が無ければ None)。"""
    if h is None or "errors_by_cause" not in h["keys"]:
        return None
    causes = h.get("causes") or list(CAUSE_NAMES)
    if "dns" not in causes:
        return None
    ei, di = h["keys"].index("errors_by_cause"), causes.index("dns")
    res = h.get("interval_secs") or 3600
    total = 0.0
    for s in h["samples"]:
        if not s or not isinstance(s[0], (int, float)):
            continue
        share = (min(s[0] + res, t1) - max(s[0], t0)) / res
        by = s[ei] if ei < len(s) else None
        if share > 0 and isinstance(by, list) and di < len(by):
            total += (by[di] or 0) * min(share, 1.0)
    return total


def dns_failures_between(snap, t0, t1):
    """窓 `[t0, t1)` の名前解決の失敗 (`/history` の `errors_by_cause.dns`) の数 (無ければ None)。

    `/history?res=60` (1 日) が覆うところは 1 分の標本で、それより古いところは `res=3600` の標本を窓に重なる
    割合で按分して数える (`/profile` の窓は分の単位で時間の境目に揃わず、雪像を撮った時刻によっては端の 1 時間が
    まだ無い)。
    """
    h60, h3600 = history_part(snap, 60), history_part(snap, 3600)
    first60 = next((s[0] for s in (h60 or {}).get("samples") or [] if s and isinstance(s[0], (int, float))), None)
    if h60 is not None and "errors_by_cause" in h60["keys"] and first60 is not None and first60 < t1:
        cut = max(t0, first60)
        recent = _dns_failures_in(h60, cut, t1)
        older = _dns_failures_in(h3600, t0, cut) if cut > t0 else 0.0
        if recent is not None and older is not None:
            return round(recent + older)
    v = _dns_failures_in(h3600, t0, t1)
    return None if v is None else round(v)


def profile_roles_us(p):
    """`/profile` の JSON 1 つの `({役割: cpu_us の和}, 全体の cpu_us の和, 要求の和, 秒)`。欄が無ければ None。"""
    keys = p.get("keys") or []
    roles = p.get("roles") or []
    rows = p.get("samples") or []
    interval = p.get("interval_secs") or 0
    if not rows or not interval or not all(k in keys for k in ("threads", "requests", "cpu_us")):
        return None
    ti, qi, ci = keys.index("threads"), keys.index("requests"), keys.index("cpu_us")
    by = {r: 0 for r in roles}
    cpu = req = 0
    for r in rows:
        threads = r[ti] if ti < len(r) else None
        for i, role in enumerate(roles):
            t = threads[i] if threads and i < len(threads) else None
            if t:
                by[role] += t[0] or 0
        cpu += (r[ci] if ci < len(r) else 0) or 0
        req += (r[qi] if qi < len(r) else 0) or 0
    return by, cpu, req, len(rows) * interval


def _p29_profile_sides(c, th):
    """前と後の `/profile` の材料 (`[(誰, 雪像, p, 出どころ)]`) か、判定できない理由。

    phase21 以降の「参考」(材料が 12 時間に満たない) は使わず、満たなければ「判定できず」にする (T.32.29。
    `--closeout` が前の版の最後の `-profile_res_60.json` (同じ版で 12 時間以上) を前の材料に選ぶ)。
    """
    hours = th["profile_min_hours"]
    out = []
    for who, snap in (("前", c["a"]), ("後", c["b"])):
        p, src = profile_source(snap)
        if not p:
            return None, f"{who}の雪像に `/profile` の材料が無い (`--profile` / `--profile-before`)"
        span = profile_span(snap)
        if span[0] < hours * 3600:
            return None, (f"{who}の `/profile` が {span[0] / 3600:,.1f} 時間 ({n(span[1])} 標本) で {hours:.0f} 時間に"
                          "満たない (`--closeout` は前の版の最後の 12 時間以上の `-profile_res_60.json` を選ぶ)")
        out.append((who, snap, p, src))
    return out, None


def _p29_conn_per_resolved_request(c, th):
    """T.32.29 (b): `conn` 役の CPU ÷ **名前解決に成功した要求**が前の版の `conn_per_resolved_factor` 倍未満。

    名前解決に失敗した要求は負のキャッシュで即 502 になり `conn` の CPU をほとんど使わないので、要求の全数で
    割ると失敗の割合 (T.31.99 で 40% → 66%) で動いた。失敗の数は、`/profile` の窓 (最初と最後の `t`) に入る
    `/history?res=3600` の `errors_by_cause` の `dns`。要求あたり (全数) は表示だけ。
    """
    k = th["conn_per_resolved_factor"]
    label = (f"`conn` 役の CPU ÷ **名前解決に成功した要求**が前の版の {k} 倍未満 (T.32.29。要求の全数で割った値は表示だけ)")
    limit = f"< 前 ×{k}"
    sides, why = _p29_profile_sides(c, th)
    if sides is None:
        return (label, limit, "—", UNKNOWN, why)
    got, srcs = {}, []
    for who, snap, p, src in sides:
        v = profile_roles_us(p)
        win = profile_window(p)
        if v is None or win is None or "conn" not in v[0]:
            return (label, limit, "—", UNKNOWN, f"{who}の `/profile` に `t`・`threads`・`requests`・`cpu_us` か `conn` の役割が無い")
        fails = dns_failures_between(snap, win[0], win[1] + (p.get("interval_secs") or 60))
        if fails is None:
            return (label, limit, "—", UNKNOWN,
                    f"{who}の雪像の `/history?res=3600` に `errors_by_cause` (の `dns`) が無い。名前解決の失敗が数えられない")
        by, _, req, secs = v
        ok = req - fails
        if ok <= 0:
            return (label, limit, "—", UNKNOWN, f"{who}の `/profile` の窓で名前解決に成功した要求が 0 本")
        got[who] = {"us": by["conn"] / ok, "all": by["conn"] / req if req else None, "req": req, "fail": fails}
        srcs.append(f"{who}: {src} {secs / 3600:,.1f} 時間、要求 {n(req)} 本のうち名前解決の失敗 {n(fails)} 本 "
                    f"({fails / req * 100 if req else 0:.0f}%)")
    a, b = got["後"], got["前"]
    r = a["us"] / b["us"] if b["us"] else None
    if r is None:
        return (label, limit, "—", UNKNOWN, "。".join(srcs) + "。前の `conn` 役の CPU が 0")
    shown = (f"**{b['us']:,.0f} → {a['us']:,.0f}** us/名前解決に成功した要求 (**{r:.2f}** 倍)。"
             f"表示だけ: 要求の全数あたり {b['all']:,.0f} → {a['all']:,.0f} us")
    return (label, limit, shown, MET if r < k else MISSED,
            "`/profile` の `conn` の `cpu_us` ÷ (要求 − `/history?res=3600` の `errors_by_cause.dns`)。" + "。".join(srcs))


def _p29_idle_cost(c, th):
    """T.32.29 (a): 暇なときの費用 = 裏の役割 (`conn`・`accept` 以外) の CPU (us/秒) が前の版の `idle_cost_factor` 倍未満。

    裏の役割は要求の量でほとんど動かない (T.31.99 の材料で 890 → 894 us/秒) ので、割合 (phase26 の行) ではなく
    1 秒あたりの量で比べる。`profile-sample`・`cache-probe` の割合は表示だけ。
    """
    k = th["idle_cost_factor"]
    label = (f"暇なときの費用: 裏の役割 (`conn`・`accept` 以外) の CPU が前の版の {k} 倍未満 (us/秒。T.32.29。"
             "`profile-sample`・`cache-probe` の割合は表示だけ)")
    limit = f"< 前 ×{k}"
    sides, why = _p29_profile_sides(c, th)
    if sides is None:
        return (label, limit, "—", UNKNOWN, why)
    got, shares, srcs = {}, {}, []
    for who, snap, p, src in sides:
        v = profile_roles_us(p)
        if v is None or not v[3]:
            return (label, limit, "—", UNKNOWN, f"{who}の `/profile` に `threads`・`requests`・`cpu_us` が無い")
        by, cpu, _, secs = v
        got[who] = (cpu - by.get("conn", 0) - by.get("accept", 0)) / secs
        shares[who] = {role: (by[role] / cpu if cpu and role in by else None) for role in th["idle_roles"]}
        srcs.append(f"{who}: {src} {secs / 3600:,.1f} 時間")
    a, b = got["後"], got["前"]
    if not b:
        return (label, limit, "—", UNKNOWN, "。".join(srcs) + "。前の裏の役割の CPU が 0")
    r = a / b
    parts = []
    for role in th["idle_roles"]:
        sb, sa = shares["前"][role], shares["後"][role]
        if sa is not None and sb is not None:
            parts.append(f"`{role}` {sb * 100:.1f}% → {sa * 100:.1f}%")
    shown = f"**{b:,.0f} → {a:,.0f}** us/秒 (**{r:.2f}** 倍)"
    if parts:
        shown += "。表示だけ: " + "・".join(parts)
    return (label, limit, shown, MET if r < k else MISSED,
            "`/profile` の全体の `cpu_us` − `conn` − `accept` を秒で割ったもの。" + "。".join(srcs))


def history_since_start(snap, res, keys):
    """起動からの `/history?res=…` の標本 (起動を含む区間から) の `keys` の和 (`{名前: 和}`)。欄が無ければ None。

    `errors_by_cause` は原因ごとの和の並び。区間 `[t, t + res)` が起動より後に終わる標本を数える
    (起動の時間の標本は前のプロセスのぶんも含みうるが、T.32.30 の数え方と同じ)。
    """
    h = history_part(snap, res)
    up = snap.get("uptime_secs")
    taken = snap.get("taken_at")
    if h is None or not isinstance(up, (int, float)) or not isinstance(taken, (int, float)):
        return None
    hk = h["keys"]
    if any(k not in hk for k in keys):
        return None
    boot = taken - up
    out = {k: ([0] * len(h.get("causes") or CAUSE_NAMES) if k == "errors_by_cause" else 0) for k in keys}
    hours = 0
    for s in h["samples"]:
        if not s or not isinstance(s[0], (int, float)) or s[0] + res <= boot:
            continue
        hours += 1
        for k in keys:
            v = s[hk.index(k)] if hk.index(k) < len(s) else None
            if k == "errors_by_cause":
                for i, x in enumerate(v or []):
                    if i < len(out[k]):
                        out[k][i] += x or 0
            else:
                out[k] += v or 0
    out["_samples"] = hours
    out["_causes"] = list(h.get("causes") or CAUSE_NAMES)
    return out


def _p29_connect_errors(c, th):
    """T.32.30: 接続のエラーを、**起動からの `/history?res=3600` の 443・80 だけ**で読む。

    率 = (エラー − 名前解決の失敗 − `other_port_errors`) ÷ (確立 + 同じエラー − `other_port_attempts`)。
    phase27 の行は `/recent` の末尾 (48.3 時間のうち 4.3 時間) だけで 0.20 倍の「満たした」だったが、同じ定義を
    起動からの `/history` で数えると 2.1 倍で逆の結論で、中身は 443・80 以外 (メール宛) だった。前の版に
    `other_port_*` の欄が無ければ「判定できず」(理由を書く)。`/recent` の窓の値 (phase27 の行) は表示だけ。
    """
    k = th["connect_error_factor"]
    label = (f"接続のエラー (443・80 宛て、名前解決の失敗を除く) ÷ 試行 が前の版の {k} 倍未満 "
             "(起動からの `/history?res=3600`。T.32.30。`/recent` の窓の値は表示だけ)")
    limit = f"< 前 ×{k}"
    keys = ("errors", "errors_by_cause", "connects", "other_port_errors", "other_port_attempts")
    _, _, recent, _, _ = _p27_connect_errors(c, th)
    got, missing = {}, []
    for who, snap in (("前", c["a"]), ("後", c["b"])):
        v = history_since_start(snap, 3600, keys)
        if v is None:
            base = history_since_start(snap, 3600, keys[:3])
            missing.append(f"{who}の雪像の " + (
                "`/history?res=3600` に `other_port_errors`・`other_port_attempts` の欄が無い (T.30.3 より前の版)"
                if base is not None else "`/history?res=3600` の部か `errors`・`connects` の欄、`uptime_secs` が無い"))
            continue
        causes = v["_causes"]
        dns = v["errors_by_cause"][causes.index("dns")] if "dns" in causes else 0
        errs = v["errors"] - dns
        got[who] = (errs - v["other_port_errors"], v["connects"] + errs - v["other_port_attempts"], v["_samples"])

    def text(who, bold):
        num, den, _ = got[who]
        star = "**" if bold else ""
        return f"{star}{num / den:.5f}{star} ({n(num)} / {n(den)})" if den else f"— (試行 0)"
    src = ("`/history?res=3600` の起動からの `errors` − `errors_by_cause.dns` − `other_port_errors` ÷ "
           "(`connects` + 同じエラー − `other_port_attempts`)")
    tail = f"。表示だけ (`/recent` の窓): {recent}"
    if missing:
        shown = (f"後 {text('後', True)}" if "後" in got else "—") + tail
        return (label, limit, shown, UNKNOWN, "。".join(missing) + "。" + src)
    (na, da, ha), (nb, db, hb) = got["後"], got["前"]
    if not da or not db:
        side = "後" if not da else "前"
        return (label, limit, f"{text('前', False)} → {text('後', True)}" + tail, UNKNOWN, f"{side}の 443・80 の試行が 0")
    ra, rb = na / da, nb / db
    shown = f"{text('前', False)} → {text('後', True)}" + tail
    src += f" (前 {hb} 時間・後 {ha} 時間)"
    if rb == 0:
        verdict = MET if ra == 0 else MISSED
    else:
        verdict = MET if ra < rb * k else MISSED
    return (label, limit, shown, verdict, src)


def _p29_dns_slow(c, th):
    """T.32.16: `dns_slow` のうち、**名前解決に成功したミス**で立った件/時。

    `/events` の `dns_slow` (起動から) の 1 件ずつについて、立った時刻までの 5 分の `/history?res=60` の標本で
    名前解決に成功したミス (`dns_misses − dns_fail_misses`) が `dns_min_misses` 回以上で、その待ちの合計
    (`dns_ms_sum − dns_fail_ms_sum`) が `dns_slow_min_sum_ms` 以上なら「成功したミスで立った」と数える。
    件/時が `events_per_hour["dns_slow"]` 以下なら満たした。名前解決の失敗の遅さで立った件は表示だけ。
    `dns_fail_ms_sum` の欄 (T.32.16 で標本に足す) が無い版は「判定できず」。`/history?res=60` (1 日) より古い
    出来事は数えず、その時間も分母から外す。
    """
    lim = th["events_per_hour"]["dns_slow"]
    min_sum, min_n = th["dns_slow_min_sum_ms"], th["dns_min_misses"]
    label = (f"`dns_slow` のうち名前解決に**成功した**ミスで立った件/時が {lim} 以下 (T.32.16。失敗の遅さで立った件は表示だけ)")
    limit = f"≤ {lim} 件/時"
    b = c["b"]
    ev = part(b, "events")
    rows = ev.get("events")
    if not isinstance(rows, list):
        return (label, limit, "—", UNKNOWN, "後の雪像に `/events` の部が無い")
    h = history_part(b, 60)
    need = ("dns_misses", "dns_fail_misses", "dns_ms_sum", "dns_fail_ms_sum")
    if h is None or any(k not in h["keys"] for k in need):
        missing = "部" if h is None else "・".join(f"`{k}`" for k in need if k not in h["keys"])
        return (label, limit, "—", UNKNOWN,
                f"後の雪像の `/history?res=60` に {missing} が無い (`dns_fail_ms_sum` は T.32.16 で足した欄)")
    hk = h["keys"]
    idx = {k: hk.index(k) for k in need}
    samples = [s for s in h["samples"] if s and isinstance(s[0], (int, float))]
    if not samples:
        return (label, limit, "—", UNKNOWN, "後の雪像の `/history?res=60` に標本が無い")
    up, taken = b.get("uptime_secs") or 0, b.get("taken_at") or 0
    start = max(taken - up, samples[0][0] + 300)
    hours = max(taken - start, 0) / 3600.0
    ok = fail = 0
    for e in rows:
        text = e.get("text") or ""
        at = e.get("at") or 0
        if e.get("kind") != "anomaly" or not text.startswith("dns_slow:") or at < start:
            continue
        w = [s for s in samples if at - 300 < s[0] + 60 <= at + 60]
        cnt = sum((s[idx["dns_misses"]] or 0) - (s[idx["dns_fail_misses"]] or 0) for s in w)
        tot = sum((s[idx["dns_ms_sum"]] or 0) - (s[idx["dns_fail_ms_sum"]] or 0) for s in w)
        if cnt >= min_n and tot >= min_sum:
            ok += 1
        else:
            fail += 1
    if not hours:
        return (label, limit, "—", UNKNOWN, "`/history?res=60` の窓と起動からの時間が重ならない")
    rate = ok / hours
    shown = f"**{rate:.2f}** 件/時 ({ok} 件 / {hours:,.1f} 時間)。表示だけ: 名前解決の失敗の遅さで立った {fail} 件"
    src = (f"`/events` の `dns_slow:` (起動から、`/history?res=60` の窓の {hours:,.1f} 時間) を、立つまでの 5 分の "
           f"`dns_misses − dns_fail_misses` ≥ {min_n} 回・`dns_ms_sum − dns_fail_ms_sum` ≥ {min_sum:,} ms で分けた")
    if ev.get("truncated"):
        src += "。**`/events` が切れている** (件数は下限)"
    return (label, limit, shown, MET if rate <= lim else MISSED, src)


_p29_cgroup_cpu_shown = display_only(
    _p28_cgroup_cpu_per_request, lambda th: "cgroup の起動からの CPU (コアの合計と要求 1 本あたり。T.32.29)",
    "要求あたりは名前解決に失敗した要求 (負のキャッシュで即 502) の割合で動く (T.31.99 で 40% → 66%)。"
    "判定は `conn` 役の名前解決に成功した要求あたりと、裏の役割の us/秒の 2 行で見る", was="phase28")

# phase29 = phase28 の 26 行のうち 6 行を差し替えたもの (T.32.16・T.32.17・T.32.29・T.32.30)。並びは同じ:
#   3 番目 (平常時の名前解決のミス。表示だけ) → 名前解決の失敗を除いた率も添える、5 番目 (`conn` 役の CPU/要求) → 名前解決に成功した要求あたり、11 番目 (`dns_slow`) → 成功したミスで立った件/時、
#   13 番目 (接続のエラー) → 起動からの `/history` の 443・80、24 番目 (cgroup の CPU/要求) → 表示だけ、
#   25 番目 (裏の役割の割合) → 裏の役割の us/秒。**phase28 以前の並びと文面は 1 つも動かない**
def _p29_miss_band_measured(c, th):
    """T.32.17: 平常時の名前解決のミスの行 (phase23 の行) に、**名前解決の失敗を除いた**回/接続を添える。

    phase23 までの回/接続は名前解決に失敗したミス (負のキャッシュの総当たり) も数えていて、T.31.99 は 0.95 回/接続
    (失敗を除けば 0.048)。`/history` の `dns_fail_misses` の無い版は添えない (今までどおり)。
    """
    label, limit, shown, verdict, src = _p23_miss_band_measured(c, th)
    parts = []
    for key, name in (("after", "平常時"), ("after_all", "バースト込み")):
        v = (c.get(key) or {}).get("dns_ok_per_connect")
        if v is not None:
            parts.append(f"{name} **{v:.3f}**" if key == "after" else f"{name} {v:.3f}")
    if not parts:
        return (label, limit, shown, verdict, src + "。`/history` に `dns_fail_misses` が無い (失敗を除いた率は出せない)")
    return (label, limit, f"{shown}。名前解決の失敗を除くと " + "・".join(parts) + " 回/接続 (T.32.17)",
            verdict, src + "、失敗を除いた率は (`dns_misses` − `dns_fail_misses`) ÷ `connects`")


_p29_miss_band = display_only(
    _p29_miss_band_measured,
    lambda th: "平常時の名前解決のミス (回/接続。名前解決の失敗を除いた率と `/recent` の試行あたりも並べる)",
    "宛先の数で動く使われ方の数字 (T20.99 は宛先 395 名前で 0.12、`cold` 408 回。10-20 は死んだ名前の"
    "総当たりで 0.0850)", was="phase20")

_P28 = RULES["phase28"]
RULES["phase29"] = (_P28[:2] + (_p29_miss_band,) + _P28[3:4] + (_p29_conn_per_resolved_request,) + _P28[5:10] + (_p29_dns_slow,) + _P28[11:12]
                    + (_p29_connect_errors,) + _P28[13:23] + (_p29_cgroup_cpu_shown, _p29_idle_cost) + _P28[25:])
PHASE29 = dict(PHASE28)
# `conn` 役の CPU/名前解決に成功した要求 と、裏の役割の us/秒 の倍率 (前の版のこの倍未満なら満たす。T.32.29)
PHASE29["conn_per_resolved_factor"] = 1.1
PHASE29["idle_cost_factor"] = 1.1
# `dns_slow` の「成功したミスで立った」の回数の下限 (`anomaly.rs` の `DNS_MIN_MISSES`。T.32.16)
PHASE29["dns_min_misses"] = 3
CRITERIA["phase29"] = PHASE29


# --- Phase 30 (T.33.1・T.33.2。phase29 の 26 行のうち、T.32.99 で物差しの側が届かなかった 2 行を使われ方で動かない形にしたもの) ---
#
# T.32.99 (0.32.39 の 48.3 時間) で phase29 の `dns_slow` の行は 0.63 件/時 (15 件 / 23.9 時間、閾 0.1) の「届かず」。
# 15 件とも初めて引く名前 (ほとんどが 1 つの接続元のメールのホスト) の名前解決そのものが遅い窓で、件/時は
# 初めて引く名前の数 (使われ方) で動く。0.1 件/時は T.17〜T.23 の静かなころの値。1 名前あたりの引き直しの行は、
# 分母の `dns.entries` が表の上限と総当たりの濃さで 7,511 → 891 に動いて 6.27 倍の「届かず」だった (T.33.2)。

def dns_slow_windows(snap, th):
    """`/events` の `dns_slow` が立った窓の、名前解決に**成功した**ミスの回数と待ちの合計 (T.33.1)。

    窓は phase29 の行 (`_p29_dns_slow`) と同じ「立つまでの 5 分の `/history?res=60` の標本」で、起動からの
    出来事のうち `/history?res=60` (1 日) の中のものだけ。重なった窓の標本は 1 回だけ数える。
    `dns_fail_ms_sum` (T.32.16) のある版は、phase29 と同じく成功したミスで立った窓だけを選び、待ちは確か (`exact`)。
    無い版 (0.30.4 まで) は立った窓 (失敗込みの閾で立った) を全部使い、1 分の標本ごとに、失敗の混ざらない 1 分は
    確か、混ざった 1 分は成功したミスの待ちを 0〜その 1 分の合計として**上下限** (`lo`・`hi`) で返す。
    `dns_fail_misses` も無ければ (T.32.17 より前) None。`web_dns_misses`・`web_dns_ms_sum` (T.33.3) のある版は、
    本体の `dns_slow` と同じく 443・80 宛ての成功したミスだけで窓を選び、回数と待ちもそれで数える (`web`)。返すのは
    `{"fired", "hours", "misses", "lo", "hi", "exact", "web", "truncated"}`。
    """
    ev = part(snap, "events")
    rows = ev.get("events")
    h = history_part(snap, 60)
    if not isinstance(rows, list) or h is None:
        return None
    hk = h["keys"]
    if any(k not in hk for k in ("dns_misses", "dns_fail_misses", "dns_ms_sum")):
        return None
    exact = "dns_fail_ms_sum" in hk
    web = exact and "web_dns_misses" in hk and "web_dns_ms_sum" in hk
    idx = {k: hk.index(k) for k in ("dns_misses", "dns_fail_misses", "dns_ms_sum", "dns_fail_ms_sum",
                                    "web_dns_misses", "web_dns_ms_sum") if k in hk}
    samples = [s for s in h["samples"] if s and isinstance(s[0], (int, float))]
    if not samples:
        return None
    up, taken = snap.get("uptime_secs") or 0, snap.get("taken_at") or 0
    start = max(taken - up, samples[0][0] + 300)

    def get(s, k):
        return (s[idx[k]] if idx[k] < len(s) else 0) or 0

    def resolved(s):
        """その 1 分の成功したミスの (回数, 待ち)。T.33.3 の列があれば 443・80 宛てだけ。"""
        if web:
            return get(s, "web_dns_misses"), get(s, "web_dns_ms_sum")
        return get(s, "dns_misses") - get(s, "dns_fail_misses"), get(s, "dns_ms_sum") - get(s, "dns_fail_ms_sum")
    used, fired = set(), 0
    for e in rows:
        text = e.get("text") or ""
        at = e.get("at") or 0
        if e.get("kind") != "anomaly" or not text.startswith("dns_slow:") or at < start:
            continue
        w = [i for i, s in enumerate(samples) if at - 300 < s[0] + 60 <= at + 60]
        if exact:
            cnt = sum(resolved(samples[i])[0] for i in w)
            tot = sum(resolved(samples[i])[1] for i in w)
            if cnt < th["dns_min_misses"] or tot < th["dns_slow_min_sum_ms"]:
                continue
        fired += 1
        used.update(w)
    misses = lo = hi = 0
    for i in used:
        s = samples[i]
        ok = resolved(s)[0] if exact else get(s, "dns_misses") - get(s, "dns_fail_misses")
        if ok <= 0:
            continue
        misses += ok
        if exact:
            v = resolved(s)[1]
            lo += v
            hi += v
        else:
            hi += get(s, "dns_ms_sum")
            if not get(s, "dns_fail_misses"):
                lo += get(s, "dns_ms_sum")
    return {"fired": fired, "hours": max(taken - start, 0) / 3600.0, "misses": misses,
            "lo": lo, "hi": hi, "exact": exact, "web": web, "truncated": bool(ev.get("truncated"))}


def _p30_dns_slow(c, th):
    """T.33.1: `dns_slow` が立った窓の、名前解決に成功したミスの平均が前の版の 1.5 倍未満。

    phase29 の行 (`_p29_dns_slow`) は成功したミスで立った件/時 ≤ 0.1 で、件/時は初めて引く名前の数 (使われ方) で
    動く (T.32.99 は 15 件 / 23.9 時間で 0.63)。平均はリゾルバの距離で決まり、使われ方で動きにくいので判定にし、
    件/時は表示だけにする。前の版に `dns_fail_ms_sum` が無ければ平均は上下限 (`dns_slow_windows`) で、
    上限でも閾を越えれば「届かず」、下限でも越えなければ「満たした」、間なら「判定できず」(上下限を書く)。
    後の期間に成功したミスで立った窓が無ければ満たした。前の版に立った窓が無ければ比べる平均が無いので判定できず。
    """
    k = th["dns_slow_avg_factor"]
    label = (f"`dns_slow` が立った窓の、名前解決に**成功した**ミスの平均が前の版の {k} 倍未満 "
             "(T.33.1。件/時は表示だけ)")
    limit = f"< 前 ×{k} (立った窓の成功したミスの平均)"
    aft, bef = dns_slow_windows(c["b"], th), dns_slow_windows(c["a"], th)
    if aft is None or not aft["exact"]:
        return (label, limit, "—", UNKNOWN,
                "後の雪像に `/events` か `/history?res=60` の `dns_fail_ms_sum` (T.32.16 で足した欄) が無い")
    if not aft["hours"]:
        return (label, limit, "—", UNKNOWN, "`/history?res=60` の窓と起動からの時間が重ならない")

    def rate(v, what):
        return f"{v['fired'] / v['hours']:.2f} 件/時 ({v['fired']} 件 / {v['hours']:,.1f} 時間、{what})"

    def avg(v):
        if not v["misses"]:
            return None
        if v["exact"]:
            return f"{v['lo'] / v['misses']:,.0f} ms ({n(v['misses'])} 回、立った {v['fired']} 件)"
        return (f"{v['lo'] / v['misses']:,.0f}〜{v['hi'] / v['misses']:,.0f} ms ({n(v['misses'])} 回、"
                f"立った {v['fired']} 件。`dns_fail_ms_sum` の無い版なので上下限)")
    src = ("`/events` の `dns_slow:` (起動から、`/history?res=60` の窓の中) が立つまでの 5 分の `/history?res=60` の"
           "標本を重ねずに足し、(`dns_ms_sum − dns_fail_ms_sum`) ÷ (`dns_misses − dns_fail_misses`)。後は phase29 と"
           f"同じく成功したミス {th['dns_min_misses']} 回以上・{th['dns_slow_min_sum_ms']:,} ms 以上で立った窓だけ")
    if aft["web"] or (bef or {}).get("web"):
        src += ("。`web_dns_misses`・`web_dns_ms_sum` (T.33.3) のある版は本体の `dns_slow` と同じく 443・80 宛ての"
                "成功したミスだけで選び、数える (" + "・".join(name for name, v in (("後", aft), ("前", bef))
                                                         if (v or {}).get("web")) + ")")
    if bef is not None and not bef["exact"]:
        src += ("。前は `dns_fail_ms_sum` が無いので立った窓 (失敗込みの閾) を全部使い、失敗の混ざらない 1 分は確か、"
                "混ざった 1 分は成功したミスの待ちを 0〜その 1 分の合計とした上下限")
    if aft["truncated"] or (bef or {}).get("truncated"):
        src += "。**`/events` が切れている** (立った件は下限)"
    rates = "表示だけ: " + rate(aft, "成功したミスで立った")
    if bef is not None and bef["hours"]:
        rates += " (前 " + rate(bef, "成功したミスで立った" if bef["exact"] else "失敗込みの閾で立った") + ")"
    if not aft["fired"]:
        return (label, limit, f"後の期間に成功したミスで立った窓は **0** 件。{rates}", MET, src)
    a = aft["lo"] / aft["misses"]
    shown = f"**{a:,.0f}** ms ({n(aft['misses'])} 回、立った {aft['fired']} 件)"
    if bef is None:
        return (label, limit, shown + "。" + rates, UNKNOWN,
                "前の雪像に `/events` か `/history?res=60` の `dns_misses`・`dns_fail_misses` が無い "
                "(`dns_fail_misses` は T.32.17 で足した欄)。" + src)
    if not bef["misses"]:
        return (label, limit, shown + "。" + rates, UNKNOWN,
                "前の版の `/history?res=60` の窓で立った窓に成功したミスが無い (比べる平均が無い)。" + src)
    lo, hi = bef["lo"] / bef["misses"], bef["hi"] / bef["misses"]
    if bef["exact"]:
        shown += f" (前 {avg(bef)}" + (f"、{a / lo:.2f} 倍" if lo else "") + ")"
        ok = a < lo * k if lo else False
        return (label, limit, shown + "。" + rates, MET if ok else MISSED, src)
    shown += f" (前 {avg(bef)})"
    if lo and a < lo * k:
        verdict = MET
    elif a >= hi * k:
        verdict = MISSED
    else:
        verdict = UNKNOWN
        src = (f"前の平均の上下限 {lo:,.0f}〜{hi:,.0f} ms の {k} 倍 ({lo * k:,.0f}〜{hi * k:,.0f} ms) の間に後の "
               f"{a:,.0f} ms があり、どちらとも言えない。" + src)
    return (label, limit, shown + "。" + rates, verdict, src)


def answered_names(snap):
    """`/dns` の一覧のうち答えのある (`addrs` のある) 名前の数 (T.33.2)。

    一覧が切れていれば (`truncated`、または `shown` < `count`) 数えられないので None。表の大半は答えの無い
    失敗の行 (総当たりの死んだ名前) で、裏の引き直しの対象ではない。
    """
    d = part(snap, "dns")
    rows = d.get("entries")
    if not isinstance(rows, list) or d.get("truncated"):
        return None
    if isinstance(d.get("count"), int) and len(rows) < d["count"]:
        return None
    return sum(1 for e in rows if e.get("addrs") or e.get("addr_count"))


def _p30_refresh_per_name(c, th):
    """T.33.2: 裏の引き直し (回/時) を**答えのある名前 1 つあたり**で前の期間と比べ、1.5 倍未満なら満たす。

    phase25 の行 (`_p25_refresh_per_name`) は `/status` の `dns.entries` (表の名前の数) で割っていて、表の上限
    (T.32.2) や総当たりの濃さで分母が動く (T.32.99 は 7,511 → 891 名前で 0.04 → 0.25 回/時/名前、6.27 倍の
    「届かず」。引き直しの合計は 302.4 → 224.9 回/時に下がっていた)。分母は `/dns` の一覧の `addrs` のある行の数
    (`answered_names`) にする。前か後の一覧が切れていて数えられなければ、**引き直しの合計 (回/時)** が前の期間の
    1.5 倍未満で比べる (理由を書く)。表の名前の数あたりと `expired` + `warm_stale` は表示だけ。
    """
    k = th["refresh_per_name_factor"]
    label = (f"裏の引き直し (回/時) が、答えのある名前 1 つあたりで前の期間の {k} 倍未満 (T.33.2。`/dns` の一覧が"
             "切れていれば引き直しの合計で。表の名前の数あたりと `expired` + `warm_stale` は表示だけ)")
    limit = f"< 前 ×{k} (答えのある名前 1 つあたり、無ければ合計)"
    rb, ra = counter_rates(c, "refreshes")
    src = ("`/status` の `dns.refreshes` を 1 時間あたりにした (前は前の雪像の起動から、後は"
           + ("その間" if dns_counter(c, "refreshes")[2] else "後の雪像の起動から") + ")")
    if rb is None or ra is None:
        return (label, limit, "—", UNKNOWN, "前か後の `/status` に `dns.refreshes` か `uptime_secs` が無い")
    nb, na = answered_names(c["a"]), answered_names(c["b"])
    extra = []
    eb = (part(c["a"], "status").get("dns") or {}).get("entries")
    ea = (part(c["b"], "status").get("dns") or {}).get("entries")
    if eb and ea:
        extra.append(f"表の名前あたり {ra / ea:.2f} 回/時/名前 (前 {rb / eb:.2f}、名前 {n(ea)}、前 {n(eb)})")
    rates = [counter_rates(c, "misses_by_kind", kind) for kind in ("expired", "warm_stale")]
    if not any(b is None or a is None for b, a in rates):
        mb_, ma_ = sum(b for b, _ in rates) * 24, sum(a for _, a in rates) * 24
        extra.append(f"`expired` + `warm_stale` {ma_:,.1f} 回/日 (前 {mb_:,.1f})")
    if nb and na:
        pb, pa = rb / nb, ra / na
        shown = (f"**{pa:.2f}** 回/時/答えのある名前 (前 {pb:.2f}" + (f"、{pa / pb:.2f} 倍" if pb else "") + ")。"
                 f"表示だけ: 合計 {ra:,.1f} 回/時 (前 {rb:,.1f})、答えのある名前 {n(na)} (前 {n(nb)})")
        ok = pa < pb * k if pb else pa == 0
        src += "、`/dns` の一覧 (切れていない) の `addrs` のある行の数で割った"
    else:
        why = []
        for side, snap, v in (("前", c["a"], nb), ("後", c["b"], na)):
            if v is None:
                d = part(snap, "dns")
                why.append(f"{side}の `/dns` の一覧が切れている ({n(len(d['entries']))} / {n(d.get('count'))} 行)"
                           if isinstance(d.get("entries"), list) else f"{side}の雪像に `/dns` の部が無い")
            elif not v:
                why.append(f"{side}の `/dns` の一覧に答えのある名前が無い")
        shown = (f"合計 **{ra:,.1f}** 回/時 (前 {rb:,.1f}" + (f"、{ra / rb:.2f} 倍" if rb else "") + ")")
        ok = ra < rb * k if rb else ra == 0
        src = "、".join(why) + " ので答えのある名前の数が出ない。引き直しの合計で比べた。" + src
        known = [f"{side} {n(v)}" for side, v in (("後", na), ("前", nb)) if v]
        if known:
            extra.append("答えのある名前 " + "・".join(known))
    if extra:
        shown += ("。表示だけ: " if "表示だけ" not in shown else "、") + "、".join(extra)
    return (label, limit, shown, MET if ok else MISSED, src)


# phase30 = phase29 の 26 行のうち 11 番目 (`dns_slow`。T.33.1) と 15 番目 (1 名前あたりの引き直し。T.33.2) を
# 差し替えたもの。並びは同じ。**phase29 以前の並びと文面は 1 つも動かない**
_P29 = RULES["phase29"]
RULES["phase30"] = _P29[:10] + (_p30_dns_slow,) + _P29[11:14] + (_p30_refresh_per_name,) + _P29[15:]
PHASE30 = dict(PHASE29)
# `dns_slow` が立った窓の成功したミスの平均の倍率 (前の版のこの倍未満なら満たす。T.33.1)
PHASE30["dns_slow_avg_factor"] = 1.5
CRITERIA["phase30"] = PHASE30


def judge(name, hist, hosts, majors, status_b, overload, th,
          a=None, b=None, info=None, dns=None, errors=None, burst=BURST_PER_HOUR):
    """`RULES[name]` の各行を順に呼んで判定表にする。

    `a` / `b` (雪像そのもの) と `info` / `dns` / `errors` は **phase15 / phase17 の行だけが使う**
    (phase14 の 4 行は今までどおりの 7 つの引数だけで足りる)。
    """
    info = info or {}
    hours = (info.get("uptime") or [0, 0])[1] if info.get("restarted") else info.get("wall_secs")
    c = {
        "hist": hist or {}, "after": (hist or {}).get("after") or {},
        "after_all": (hist or {}).get("after_all") or {},
        "hosts": hosts, "majors": majors, "status_b": status_b, "overload": overload,
        "a": a or {}, "b": b or {}, "info": info, "dns": dns, "errors": errors,
        # 平常時の閾 (1 時間の本数)。閉じ方の行が分ごとの標本をこれで切る
        "burst": burst,
        # `dns.refreshes` は再起動をまたぐと「起動から」の値なので、割る時間もそちらに合わせる
        "hours": (hours or 0) / 3600.0,
    }
    rows = [rule(c, th) for rule in RULES[name]]
    out = {"name": name, "rows": rows,
           "tally": {v: sum(1 for r in rows if r[3] == v) for v in (MET, MISSED, UNKNOWN)}}
    # 表示だけの行 (T20.1) は集計に数えない。あるときだけ本数を別の鍵で持つ (古い定義の形は変えない)
    shown = sum(1 for r in rows if r[3] == SHOWN)
    if shown:
        out["display_only"] = shown
    # 参考の行 (T21.2) も同じく集計に数えず、あるときだけ本数を別の鍵で持つ
    reference = sum(1 for r in rows if r[3] == REFERENCE)
    if reference:
        out["reference"] = reference
    return out


# ---------------------------------------------------------------- 判定表の印字

def render_criteria(d, grouped):
    """判定表 (`## 9.`) を印字する (`snapshot-diff.py` の `render` が `"criteria" in d` のときに呼ぶ)。

    `grouped` は `--group domain` が付いているか (「主要 3 ホスト」の行の単位が変わる)。
    """
    p = print
    c = d["criteria"]
    p(f"## 9. 完了の定義に対する判定 (`--criteria {c['name']}`)")
    p()
    if grouped:
        # 「主要 3 ホスト」は §3 の上位から採るので、まとめると**単位**になる
        p("**`--group domain` を付けているので、「主要 3 ホスト」の行はまとめの単位 "
          "(eTLD+1) で判定している**。ホスト 1 件ずつで判定するには `--group` を外すこと。")
        p()
    if c["name"] == "phase15":
        # T15.0 (15)。材料が雪像に無い行は「0 だった」ではなく「判定できず」にする
        p("材料は `/hosts` `/status` の `dns` `/dns` (名前ごとの引き直し)・"
          "`/history` (`dns_warm` と `errors_by_cause`、`res=60` の `closed` / `transfer`)・"
          "`/profile` (`conn` 役の CPU)・`--daily` (日ごとのミス) です。"
          "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"
          "閉じ方の行は**平常時の 1 分ごとの全数**で比べます (`/recent` は 256 KiB で切れて"
          "窓の長さが毎回違うので使いません。T15.15)。")
        p()
    if c["name"] == "phase17":
        # T17.0a。前の 4 行は phase15 と同じ関数、後ろの 4 行が T16.99 で手で引いていた判定
        p("前の 4 行は phase15 と同じ物差しです。後ろの 4 行の材料は `--profile` "
          "(`/profile?res=60`。無ければ雪像の `/profile` の部で**参考**)・`/history` の "
          "`dns_warm_max` と `/status` の `dns.warm_evicted`・`/events` の anomaly "
          "(起動からの 件/時、`cleared:` は数えない)・`/status` の "
          "`kernel.cgroup_cpu.since_start` です。**その部が雪像に無い行は「判定できず」**で、"
          "0 とは書きません。")
        p()
    if c["name"] == "phase18":
        # T18.2。前の 8 行は phase17 と同じ関数、後ろの 2 行が T17.99 の但し書き (d)(e) の物差し
        p("前の 8 行は phase17 と同じ物差しで、材料も同じです (`--daily` `--profile` "
          "`--profile-before`)。後ろの 2 行は T18.0 の (g) と (h) で、材料は `--zero` で渡す "
          "0 時間の雪像と後の雪像の `/status` の `memory` (`heap_used + mmap` の増え。"
          "`heap_free` と `rss` は表示だけ) と、後の雪像の `/status` の "
          "`ipv6.request_probes` (T18.1。0 でなければ `canary.ipv6_runs` / `ipv6_skipped` から"
          "理由を読む) です。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase20":
        # T20.1。phase18 と同じ並びの 10 行で、物差しを 2 つ直してある
        p("phase18 と同じ並びの 10 行で、材料も同じです (`--daily` `--profile` `--profile-before` "
          "`--zero`)。直したのは 2 つ (T20.1): `heap_used + mmap` の増えは**メモリのキャッシュ "
          "(`memory.cache_memory`) を引いてから**比べます (キャッシュは上限まで使ってよい作りなので、"
          "入ったぶんは漏れではありません。欄が無い版は 0 とせず「判定できず」)。"
          "ミス率 (`watch_host`)・`timeout`・`dns_warm` の最大と `warm_evicted` の 3 行は"
          "**表示だけ**で、閾を置かず、下の集計にも数えません (使われ方で動く数字なので)。"
          "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase21":
        # T21.2。phase20 と同じ並びの 10 行で、T20.99 で届かなかった 3 行の読み方を直してある
        p("phase20 と同じ並びの 10 行で、材料も同じです (`--daily` `--profile` `--profile-before` "
          "`--zero`)。直したのは 3 行 (T21.2): 平常時の名前解決のミスは**表示だけ**で、日ごとの幅と"
          "ミスの種類別の内訳 (`cold` / `expired` …) を並べます (宛先の数で動く使われ方の数字なので)。"
          "`conn` 役の CPU/要求 は、前か後の `/profile` が 12 時間 (`/profile?res=60` で 720 標本) に"
          "満たなければ**参考**で、倍率は出しますが集計に数えません。`ipv6.request_probes` は、"
          "最後に探った時刻が 0 時間の雪像の取得から 3 時間以内なら「満たした」(起動直後は canary が "
          "IPv6 をまだ見ていない) で、それより後にも探っていれば「届かず」です。表示だけの行は"
          "ほかに 3 つ (phase20 と同じ) です。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase22":
        # T22.1。phase21 の 10 行 + 22a の 4 行で、平常時の切り方も違う
        p("前の 10 行は phase21 と同じ物差しで、材料も同じです (`--daily` `--profile` `--profile-before` "
          "`--zero`)。裏の引き直しの行だけ、keep-warm を止めた版 (T22.4。warm 0) では warm の平均で割る"
          "参考の値の代わりに通算の 1 時間あたりを並べます。`/events` の行は `dns_slow` を含めて**表示だけ**です "
          "(`dns_slow` の判定は下の新しい閾の行に任せる)。後ろの 4 行は 22a で段 2 まで行ったもの: "
          "`dns_slow` が窓のミスの合計 1,000 ms 未満の窓で立った件数 (0 なら満たした。新しい閾 (T22.2) での件/時は"
          "表示だけ)、T22.4 で keep-warm の既定を戻す 2 つの条件 (`stale_retries` が "
          "`stale_hits` の 1% 以上、接続のエラー `refused`・`unreachable`・`timeout` の**接続 1 本あたり**が前の "
          "1.5 倍以上。"
          "当たれば「届かず」)、T22.4 の目安 (引き直しの回/時が前の半分以下、`expired` + `warm_stale` の"
          "増えが 1 日 10 回以内) です。**平常時は「300 本と、その期間の 1 時間の本数の中央値の 3 倍の"
          "大きい方」未満の標本**です (§2。T22.1)。T22.3 は段 1 で止めたので行がありません。"
          "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase23":
        # T23.1。phase22 の 14 行を土台に、エラーの行の分母と heap の「判定できず」の書き方を直した
        p("前の 14 行は phase22 と同じ物差しで、材料も同じです (`--daily` `--profile` `--profile-before` "
          "`--zero`)。直したのは 4 か所 (T23.1): (1) 接続のエラー `refused`・`unreachable`・`timeout` の行は、"
          "**`/recent` の connect の 1 行を 1 試行とし、名前解決に失敗した行 (`error:dns`) を分母から外した**"
          "「名前解決に成功した試行あたり」で見ます (前の期間の 1.5 倍以上なら「届かず」)。"
          "分母が `/history` の `connects` だった phase22 の行は、そのすぐ後ろに**表示だけ**で残しました "
          "(使われ方で振れるため。T22.99 の 7.80 倍)。(2) 平常時の名前解決のミスの行は表示だけのままで、"
          "同じ雪像の `/recent` から**試行あたり**と**名前解決に成功した試行あたり**の率も並べます "
          "(死んだ名前の総当たりという使われ方の数字なので閾は置きません)。(3) heap の行は、`--zero` の雪像と"
          "後の雪像の起動が違うときに `/events` の `start` の時刻を添えます (黙って「判定できず」にしない)。"
          "(4) 最後の行は**無言の再起動** (`/events` の `start` の直前に `shutdown` が無い起動) が 0 回 (T23.2)。"
          "**`/recent` は 2,000 件の環状なので、窓と試行の本数を必ず書きます** (前と後で長さが違います)。"
          "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase24":
        # T24.2。phase23 の 16 行を持ち越し、heap と無言の再起動の 2 行だけ読み方を変えた
        p("前の 16 行は phase23 と同じ物差しで、材料も同じです (`--daily` `--profile` `--profile-before` "
          "`--zero`)。直したのは 2 行 (T24.2): (1) heap の行は、`/status` の `memory.baseline` "
          "(状態ファイルに**起動から 60 秒で 1 回だけ**書く `heap_used + mmap − cache_memory`。T24.4) が"
          "あれば**それとの差**で読みます (`--zero` の 0 時間の雪像は要りません。ホストの管理者に起動し直されても、"
          "起動ごとの基準なので判定できます)。`baseline` が無い雪像 (T24.4 より前の版・起動から 60 秒以内) は "
          "phase23 と同じ 0 時間の雪像との差で、どちらも無ければ「判定できず」です。"
          "(2) 最後の行は、`/events` の `start` の `boot` (`/proc/sys/kernel/random/boot_id`) を 1 つ前の起動と"
          "比べた**こちらが原因の**無言の再起動 (同じ `boot` のまま起動し直した回) が 0 回です。`boot` が変わった回 "
          "(ホストのカーネルごとの再起動。2026-10-21 の 2 回) は時刻つきで理由を書き、**数から外します** "
          "(こちらでは防げない)。`boot` の無い `start` はどちらにも数えません。この行を読み違えると T24.99 の"
          "「こちらが原因の無言の再起動 0 回」が崩れるので、**その部が雪像に無い行は「判定できず」**で、"
          "0 とは書きません。"
          "(3) 17 番目 (T24.3) は、確立の**尾**を固定の 6 ms ではなく**そのときのホストの RTT + 5 ms**で"
          "読みます (T14.99 の決まり)。`/recent` の connect の行の `ms.connect` と `rtt_ms.origin` を"
          "**同じ行で対にした**差の中央値 (プロキシの取り分) と p95 (尾) を出し、尾が 5 ms 未満なら"
          "「距離」(相手までの RTT で説明できる) です。**`/recent` は 2,000 件の環状なので、窓と本数を"
          "この行にも書きます**。`rtt_ms.origin` の無い雪像 (Linux 以外・`--lite`) は「判定できず」です。")
        p()
    if c["name"] == "phase25":
        # T27.0。phase24 の 17 行を持ち越し、T25.99 で物差しの穴だった 3 行だけ読み方を変えた
        p("17 行の並びと材料は phase24 と同じです。直したのは 3 行 (T27.0): (1) 接続のエラー "
          "(名前解決に成功した試行あたり) は、前か後の `/recent` の試行が **1,000 本に満たなければ「判定できず」**"
          "です (サーバーの日次の雪像は 00:00 UTC に撮るので、`/recent` が 1 時間ぶんしか無いことがある。"
          "T25.99 の前は 581 本で 0 件)。(2) 裏の引き直しは、keep-warm を止めた版どうしでは 0.5 倍の目安に"
          "意味が無いので、**1 名前あたり (回/時) が前の 1.5 倍未満**で読みます。通算の回/時と "
          "`expired` + `warm_stale` の回/日は同じ行に表示だけで並べます (名前の数で動く使われ方の数字)。"
          "(3) 無言の再起動は、**前の雪像の取得より後の `start` だけ**を数えます (それより前の起動は前の締めで"
          "見ています。`boot` の無い古い `start` が `/events` に残る間、毎回「判定できず」になるのを防ぐ)。"
          "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase26":
        # T.29.10。phase25 の 17 行を 1 つも動かさず、T.28.99 で手で読んだものと T.29.99 の見ることを後ろに足した
        p("前の 17 行は phase25 と同じ物差しで、材料も同じです。18 番目から後ろは T.28.99 で手で読んでいた"
          "「ほかに見ること」と T.29.99 の見ること (T.29.10): 408 (ヘッダーの締め切り) の件数 (`/status` の "
          "`rejected_requests.header_timeout`。表示だけ)、`/log` の error の行が 5 行以下、`/errors` の `local` "
          "(私設宛ての 403) が 0 件、3 秒で切った確立・成功した確立の最大・`syn_retrans` 1 / 2 の本数 (表示だけ)、"
          "`rejected_requests` の合計と理由別 (表示だけ)、`rss − (heap_used + mmap + cache_memory)` が 10 MB 未満 "
          "(T.29.25)、cgroup の起動からの CPU が前の版より下がる、`profile-sample`・`cache-probe` の CPU の割合が"
          "前の版より下がる (前か後の `/profile` が 12 時間に満たなければ参考)、`/log` のオリジン側の失敗 (502・時間切れ) の "
          "WARN が 1 分に 10 行以下 (T.29.15。原因ごとに 1 分 1 行)。他人の `Set-Cookie` (T.29.0) は"
          "雪像に数が無いので行にしていません。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase27":
        # T.30.0・T.30.1。phase26 の 26 行のうち 2 行だけ読み方を変えた
        p("26 行の並びと材料は phase26 と同じです。直したのは 2 行 (T.30.0・T.30.1): (1) 接続のエラー "
          "(名前解決に成功した試行あたり) は、前か後の**分母 (名前解決に成功した試行) が 1,000 本に満たなければ"
          "「判定できず」**です (T.29.99 は試行 1,985 本のうち 1,496 本が名前の引けないメール宛で、分母 489 本で"
          "16.40 倍になった)。外した名前解決の失敗の本数とポートの上位を書きます。(2) RSS の返していない分 "
          "(`rss − (heap_used + mmap + cache_memory)`) は固定の 10 MB ではなく**前の版の同じ値より下がる**で読み、"
          "`heap_free`・`stacks_estimate`・`malloc_trim` の回数と返した量は表示だけで並べます。"
          "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase28":
        # T.31.0・T.31.1。phase27 の 26 行のうち 2 行を量に左右されない物差しにした
        p("26 行の並びと材料は phase27 と同じです。直したのは 2 行 (T.31.0・T.31.1): (1) cgroup の CPU は"
          "コアの合計ではなく**起動からの要求 1 本あたり**で前の版と比べます (1.1 倍未満。T.30.99 は要求が 1.6 倍で、"
          "コアの合計は 1.09 倍の「届かず」だった)。コアの合計・user / sys・裏の役割の割合は表示だけです。"
          "(2) RSS の返していない分は、**`malloc_trim` が毎時動いて返しているか** (`memory.trim` の `count` と "
          "返した量) で読み、`rss − (heap_used + mmap + cache_memory)`・`heap_free`・`stacks_estimate` は表示だけです "
          "(空きの塊とスタックの山は量と同時の接続数で動く)。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase29":
        # T.32.16・T.32.17・T.32.29・T.32.30。phase28 の 26 行のうち 6 行を名前解決の失敗の割合で動かない形にした
        p("26 行の並びは phase28 と同じです。直したのは 6 行 (T.32.16・T.32.17・T.32.29・T.32.30): (1) `conn` 役の CPU は"
          "**名前解決に成功した要求 1 本あたり**で前の版と比べます (1.1 倍未満。名前解決に失敗した要求は負のキャッシュで"
          "即 502 になり、T.31.99 では割合が 40% → 66% に増えて要求の全数あたりが動いた)。失敗の数は `/profile` の窓の"
          "`/history?res=3600` の `errors_by_cause.dns` です。(2) 暇なときの費用は割合ではなく**裏の役割 (`conn`・`accept` "
          "以外) の us/秒**が前の版の 1.1 倍未満で、`profile-sample`・`cache-probe` の割合は表示だけです。"
          "(1)(2) は前か後の `/profile` が 12 時間に満たなければ「参考」ではなく「判定できず」で、`--closeout` は"
          "前の版の最後の 12 時間以上の `-profile_res_60.json` を前の材料に選びます。cgroup の CPU (コアの合計と要求あたり) は"
          "表示だけです。(3) 接続のエラーは `/recent` の末尾ではなく**起動からの `/history?res=3600` の 443・80 宛て**"
          "(`other_port_*` を引く) で読み、`/recent` の窓の値は表示だけです (前の版に欄が無ければ「判定できず」)。"
          "(4) `dns_slow` は**名前解決に成功したミスで立った件/時**で読みます (立つまでの 5 分の "
          "`dns_misses − dns_fail_misses` と `dns_ms_sum − dns_fail_ms_sum`。`dns_fail_ms_sum` の無い版は「判定できず」)。"
          "(5) 表示だけの平常時の名前解決のミスの行に、名前解決の失敗を除いた回/接続 (`dns_misses − dns_fail_misses`) を"
          "添えます (T.32.17)。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。")
        p()
    if c["name"] == "phase30":
        # T.33.1・T.33.2。phase29 の 26 行のうち、物差しの側で届かなかった 2 行を使われ方で動かない形にした
        p("26 行の並びと材料は phase29 と同じです。直したのは 2 行 (T.33.1・T.33.2): (1) `dns_slow` は**立った窓の、名前解決に"
          "成功したミスの平均**が前の版の 1.5 倍未満で読み、件/時は表示だけです (件/時は初めて引く名前の数で動き、"
          "T.32.99 は 0.63 件/時の「届かず」だった。平均はリゾルバの距離で決まる)。前の版に `dns_fail_ms_sum` が"
          "無ければ、失敗の混ざらない 1 分の標本は確か、混ざった 1 分は 0〜その 1 分の合計とした上下限で比べ、"
          "どちらとも言えなければ「判定できず」(上下限を書く) です。(2) 裏の引き直しは**答えのある名前 1 つあたり** "
          "(`/dns` の一覧の `addrs` のある行の数で割る) で前の期間の 1.5 倍未満で読み、前か後の一覧が切れていれば"
          "**引き直しの合計 (回/時)** で比べます (表の名前の数 `dns.entries` は表の上限と総当たりの濃さで動き、T.32.99 は "
          "7,511 → 891 名前で 6.27 倍の「届かず」だった)。**その部が雪像に無い行は「判定できず」**で、"
          "0 とは書きません。")
        p()
    p("| 完了の定義 | 閾値 | 実測 (後の期間) | 判定 | 出どころ |")
    p("|---|---|---|---|---|")
    for row in c["rows"]:
        p(f"| {row[0]} | {row[1]} | {row[2]} | **{row[3]}** | {row[4]} |")
    p()
    # 集計に数えない行 (表示だけ・参考) はあるときだけ添える。phase20 の文字は変わらない
    others = [f"{name} {c[key]} 行" for key, name in (("display_only", SHOWN), ("reference", REFERENCE))
              if c.get(key)]
    p("・".join(f"{k} {v} 行" for k, v in c["tally"].items() if v)
      + (f" (ほかに{'・'.join(others)}。集計に数えない)" if others else ""))
    p()
