#!/usr/bin/env python3
# `scripts/snapshot-diff.py --criteria` の部 (**Phase の完了の定義に対する判定表**。TODO.md T20.1)。
#
# `snapshot-diff.py` が 1,959 行になったので、判定の部だけをここへ割った (T18.2 の申し送り)。さらに
# このファイルが 3,454 行になったので、中身を `scripts/criteria/` に割った (T34.124〜T34.129):
#   `common.py`      小物 (`/history` の欄の名前・平常時の閾・判定の欄の文言・読む/印字の小物・履歴を重ねる)
#   `restarts.py`    再起動の部 (`restart_info` と無言の再起動)
#   `phase14_18.py`  Phase 14〜18 の行とその材料を読む小物
#   `phase20_22.py`  Phase 20〜22 の行 (`display_only` を含む) とその材料を読む小物
#   `phase23_25.py`  Phase 23〜25 の行 (T24.3 の確立の尾を含む) とその材料を読む小物
#   `phase26_28.py`  Phase 26〜28 の行とその材料を読む小物
#   `phase29_30.py`  Phase 29〜30 の行とその材料を読む小物
#   `t35.py`     Phase 31 の行 (T35.9・T35.11)
#   `t36.py`         T36 の行 (T36.1・T36.2・T36.3)
#   `t37.py`         T37 の行 (T37.1)
#   `rules.py`       閾の辞書 (`PHASE14`〜`T37_TH`)・`CRITERIA`・`FROZEN`・`RULES` (Phase の順に組む)
# ここに残っているのは、`RULES` の行を順に呼ぶ `judge()` と、判定表の印字 (`render_criteria()`) だけ。
#
# 小物までこちら側にあるのは、`snapshot-diff.py` は名前に `-` があって `import` できないため
# (共有するものは import できる側に置く)。**このファイルが割った中身を全部同じ名前で取り込み直し**、
# `snapshot-diff.py` もここから同じ名前で取り込み直すので、使い方・出力・`sd.CRITERIA` `sc.PHASE26`
# のようなテストから見える名前は割る前と変わらない。
#
# 単体では動かさない (入口は `snapshot-diff.py`)。依存は Python 3 の標準ライブラリだけ。

import os
import sys

# 読む部分は `scripts/proxydata.py` (`status-diff.py` / `snapshot-diff.py` と共有)。`CAUSE_NAMES` は
# ここでは使わないが、試験が `sc.CAUSE_NAMES` で引くので割る前と同じく名前を持っておく
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proxydata import CAUSE_NAMES  # noqa: E402,F401

# 割った中身 (`scripts/criteria/`。T34.124〜)。**同じ名前で取り込み直す**ので、ここから取り込む側
# (`snapshot-diff.py` の一覧・`snapshot-summary.py`・試験の `sc.*`) は割る前と変わらない
from criteria.common import (  # noqa: E402,F401
    BURST_MEDIAN_K, BURST_PER_HOUR, HFIELDS, MAJOR_HOSTS, MET, MISSED, REFERENCE, SHOWN, UNKNOWN,
    anomaly_rates_since_start, change, history_res, merged_histories, merged_history, miss_rate, ms, n,
    norm_history, normal_limit, part, per_hour, ratio, stamp, started_at,
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
from criteria.phase29_30 import (  # noqa: E402,F401
    _dns_failures_in, _p29_cgroup_cpu_shown, _p29_conn_per_resolved_request, _p29_connect_errors,
    _p29_dns_slow, _p29_idle_cost, _p29_miss_band, _p29_miss_band_measured, _p29_profile_sides,
    _p30_dns_slow, _p30_refresh_per_name, answered_names, dns_failures_between, dns_slow_windows,
    history_part, history_since_start, profile_roles_us, profile_window,
)
from criteria.t35 import (  # noqa: E402,F401
    ALLOC_KEY, CHUNKED_FIRST_BYTES, INDEX_BYTES_DISK, INDEX_BYTES_MEMORY, _t35_events, _t35_events_measured,
    _t35_heap, cache_owned_range,
)
from criteria.t36 import (  # noqa: E402,F401
    CONNECT_ERROR_FLOOR, DNS_ENTRY_BYTES, DNS_MAX_ENTRIES, DNS_SLOW_AVG_FLOOR_MS, MACHINE_ROLE,
    _t36_conn_per_resolved_request, _t36_connect_errors, _t36_dns_slow, _t36_heap, _t36_idle_cost,
    _t36_machine_speed, dns_owned_range, machine_cost, resolved_error_rate,
)
from criteria.t37 import _t37_dns_slow  # noqa: E402,F401
from criteria.rules import (  # noqa: E402,F401
    CRITERIA, FROZEN, PHASE14, PHASE15, PHASE17, PHASE18, PHASE20, PHASE21, PHASE22, PHASE23, PHASE24,
    PHASE25, PHASE26, PHASE27, PHASE28, PHASE29, PHASE30, T35_TH, T36_TH, T37_TH, RULES, CRITERIA_ALIASES,
    criteria_name,
)


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

# `--criteria` の名前ごとに、判定表の上に置く「材料と直したところ」の 1 段落 (T34.130 で表にした)。
# phase14 には無い。**文面は過去の判定を出し直せるように変えない**
INTRO = {
    # T15.0。材料が雪像に無い行は「0 だった」ではなく「判定できず」にする
    "phase15": ("材料は `/hosts` `/status` の `dns` `/dns` (名前ごとの引き直し)・"
                "`/history` (`dns_warm` と `errors_by_cause`、`res=60` の `closed` / `transfer`)・"
                "`/profile` (`conn` 役の CPU)・`--daily` (日ごとのミス) です。"
                "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"
                "閉じ方の行は**平常時の 1 分ごとの全数**で比べます (`/recent` は 256 KiB で切れて"
                "窓の長さが毎回違うので使いません。T15.15)。"),
    # T17.0a。前の 4 行は phase15 と同じ関数、後ろの 4 行が T16.99 で手で引いていた判定
    "phase17": ("前の 4 行は phase15 と同じ物差しです。後ろの 4 行の材料は `--profile` "
                "(`/profile?res=60`。無ければ雪像の `/profile` の部で**参考**)・`/history` の "
                "`dns_warm_max` と `/status` の `dns.warm_evicted`・`/events` の anomaly "
                "(起動からの 件/時、`cleared:` は数えない)・`/status` の "
                "`kernel.cgroup_cpu.since_start` です。**その部が雪像に無い行は「判定できず」**で、"
                "0 とは書きません。"),
    # T18.2。前の 8 行は phase17 と同じ関数、後ろの 2 行が T17.99 の但し書き (d)(e) の物差し
    "phase18": ("前の 8 行は phase17 と同じ物差しで、材料も同じです (`--daily` `--profile` "
                "`--profile-before`)。後ろの 2 行は T18.0 の (g) と (h) で、材料は `--zero` で渡す "
                "0 時間の雪像と後の雪像の `/status` の `memory` (`heap_used + mmap` の増え。"
                "`heap_free` と `rss` は表示だけ) と、後の雪像の `/status` の "
                "`ipv6.request_probes` (T18.1。0 でなければ `canary.ipv6_runs` / `ipv6_skipped` から"
                "理由を読む) です。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"),
    # T20.1。phase18 と同じ並びの 10 行で、物差しを 2 つ直してある
    "phase20": ("phase18 と同じ並びの 10 行で、材料も同じです (`--daily` `--profile` `--profile-before` "
                "`--zero`)。直したのは 2 つ (T20.1): `heap_used + mmap` の増えは**メモリのキャッシュ "
                "(`memory.cache_memory`) を引いてから**比べます (キャッシュは上限まで使ってよい作りなので、"
                "入ったぶんは漏れではありません。欄が無い版は 0 とせず「判定できず」)。"
                "ミス率 (`watch_host`)・`timeout`・`dns_warm` の最大と `warm_evicted` の 3 行は"
                "**表示だけ**で、閾を置かず、下の集計にも数えません (使われ方で動く数字なので)。"
                "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"),
    # T21.2。phase20 と同じ並びの 10 行で、T20.99 で届かなかった 3 行の読み方を直してある
    "phase21": ("phase20 と同じ並びの 10 行で、材料も同じです (`--daily` `--profile` `--profile-before` "
                "`--zero`)。直したのは 3 行 (T21.2): 平常時の名前解決のミスは**表示だけ**で、日ごとの幅と"
                "ミスの種類別の内訳 (`cold` / `expired` …) を並べます (宛先の数で動く使われ方の数字なので)。"
                "`conn` 役の CPU/要求 は、前か後の `/profile` が 12 時間 (`/profile?res=60` で 720 標本) に"
                "満たなければ**参考**で、倍率は出しますが集計に数えません。`ipv6.request_probes` は、"
                "最後に探った時刻が 0 時間の雪像の取得から 3 時間以内なら「満たした」(起動直後は canary が "
                "IPv6 をまだ見ていない) で、それより後にも探っていれば「届かず」です。表示だけの行は"
                "ほかに 3 つ (phase20 と同じ) です。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"),
    # T22.1。phase21 の 10 行 + 22a の 4 行で、平常時の切り方も違う
    "phase22": ("前の 10 行は phase21 と同じ物差しで、材料も同じです (`--daily` `--profile` `--profile-before` "
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
                "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"),
    # T23.1。phase22 の 14 行を土台に、エラーの行の分母と heap の「判定できず」の書き方を直した
    "phase23": ("前の 14 行は phase22 と同じ物差しで、材料も同じです (`--daily` `--profile` `--profile-before` "
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
                "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"),
    # T24.2。phase23 の 16 行を持ち越し、heap と無言の再起動の 2 行だけ読み方を変えた
    "phase24": ("前の 16 行は phase23 と同じ物差しで、材料も同じです (`--daily` `--profile` `--profile-before` "
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
                "この行にも書きます**。`rtt_ms.origin` の無い雪像 (Linux 以外・`--lite`) は「判定できず」です。"),
    # T27.0。phase24 の 17 行を持ち越し、T25.99 で物差しの穴だった 3 行だけ読み方を変えた
    "phase25": ("17 行の並びと材料は phase24 と同じです。直したのは 3 行 (T27.0): (1) 接続のエラー "
                "(名前解決に成功した試行あたり) は、前か後の `/recent` の試行が **1,000 本に満たなければ「判定できず」**"
                "です (サーバーの日次の雪像は 00:00 UTC に撮るので、`/recent` が 1 時間ぶんしか無いことがある。"
                "T25.99 の前は 581 本で 0 件)。(2) 裏の引き直しは、keep-warm を止めた版どうしでは 0.5 倍の目安に"
                "意味が無いので、**1 名前あたり (回/時) が前の 1.5 倍未満**で読みます。通算の回/時と "
                "`expired` + `warm_stale` の回/日は同じ行に表示だけで並べます (名前の数で動く使われ方の数字)。"
                "(3) 無言の再起動は、**前の雪像の取得より後の `start` だけ**を数えます (それより前の起動は前の締めで"
                "見ています。`boot` の無い古い `start` が `/events` に残る間、毎回「判定できず」になるのを防ぐ)。"
                "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"),
    # T29.10。phase25 の 17 行を 1 つも動かさず、T28.99 で手で読んだものと T29.99 の見ることを後ろに足した
    "phase26": ("前の 17 行は phase25 と同じ物差しで、材料も同じです。18 番目から後ろは T.28.99 で手で読んでいた"
                "「ほかに見ること」と T.29.99 の見ること (T.29.10): 408 (ヘッダーの締め切り) の件数 (`/status` の "
                "`rejected_requests.header_timeout`。表示だけ)、`/log` の error の行が 5 行以下、`/errors` の `local` "
                "(私設宛ての 403) が 0 件、3 秒で切った確立・成功した確立の最大・`syn_retrans` 1 / 2 の本数 (表示だけ)、"
                "`rejected_requests` の合計と理由別 (表示だけ)、`rss − (heap_used + mmap + cache_memory)` が 10 MB 未満 "
                "(T.29.25)、cgroup の起動からの CPU が前の版より下がる、`profile-sample`・`cache-probe` の CPU の割合が"
                "前の版より下がる (前か後の `/profile` が 12 時間に満たなければ参考)、`/log` のオリジン側の失敗 (502・時間切れ) の "
                "WARN が 1 分に 10 行以下 (T.29.15。原因ごとに 1 分 1 行)。他人の `Set-Cookie` (T.29.0) は"
                "雪像に数が無いので行にしていません。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"),
    # T30.0・T30.1。phase26 の 26 行のうち 2 行だけ読み方を変えた
    "phase27": ("26 行の並びと材料は phase26 と同じです。直したのは 2 行 (T.30.0・T.30.1): (1) 接続のエラー "
                "(名前解決に成功した試行あたり) は、前か後の**分母 (名前解決に成功した試行) が 1,000 本に満たなければ"
                "「判定できず」**です (T.29.99 は試行 1,985 本のうち 1,496 本が名前の引けないメール宛で、分母 489 本で"
                "16.40 倍になった)。外した名前解決の失敗の本数とポートの上位を書きます。(2) RSS の返していない分 "
                "(`rss − (heap_used + mmap + cache_memory)`) は固定の 10 MB ではなく**前の版の同じ値より下がる**で読み、"
                "`heap_free`・`stacks_estimate`・`malloc_trim` の回数と返した量は表示だけで並べます。"
                "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"),
    # T31.0・T31.1。phase27 の 26 行のうち 2 行を量に左右されない物差しにした
    "phase28": ("26 行の並びと材料は phase27 と同じです。直したのは 2 行 (T.31.0・T.31.1): (1) cgroup の CPU は"
                "コアの合計ではなく**起動からの要求 1 本あたり**で前の版と比べます (1.1 倍未満。T.30.99 は要求が 1.6 倍で、"
                "コアの合計は 1.09 倍の「届かず」だった)。コアの合計・user / sys・裏の役割の割合は表示だけです。"
                "(2) RSS の返していない分は、**`malloc_trim` が毎時動いて返しているか** (`memory.trim` の `count` と "
                "返した量) で読み、`rss − (heap_used + mmap + cache_memory)`・`heap_free`・`stacks_estimate` は表示だけです "
                "(空きの塊とスタックの山は量と同時の接続数で動く)。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"),
    # T32.16・T32.17・T32.29・T32.30。phase28 の 26 行のうち 6 行を名前解決の失敗の割合で動かない形にした
    "phase29": ("26 行の並びは phase28 と同じです。直したのは 6 行 (T.32.16・T.32.17・T.32.29・T.32.30): (1) `conn` 役の CPU は"
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
                "添えます (T.32.17)。**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"),
    # T33.1・T33.2。phase29 の 26 行のうち、物差しの側で届かなかった 2 行を使われ方で動かない形にした
    "phase30": ("26 行の並びと材料は phase29 と同じです。直したのは 2 行 (T.33.1・T.33.2): (1) `dns_slow` は**立った窓の、名前解決に"
                "成功したミスの平均**が前の版の 1.5 倍未満で読み、件/時は表示だけです (件/時は初めて引く名前の数で動き、"
                "T.32.99 は 0.63 件/時の「届かず」だった。平均はリゾルバの距離で決まる)。前の版に `dns_fail_ms_sum` が"
                "無ければ、失敗の混ざらない 1 分の標本は確か、混ざった 1 分は 0〜その 1 分の合計とした上下限で比べ、"
                "どちらとも言えなければ「判定できず」(上下限を書く) です。(2) 裏の引き直しは**答えのある名前 1 つあたり** "
                "(`/dns` の一覧の `addrs` のある行の数で割る) で前の期間の 1.5 倍未満で読み、前か後の一覧が切れていれば"
                "**引き直しの合計 (回/時)** で比べます (表の名前の数 `dns.entries` は表の上限と総当たりの濃さで動き、T.32.99 は "
                "7,511 → 891 名前で 6.27 倍の「届かず」だった)。**その部が雪像に無い行は「判定できず」**で、"
                "0 とは書きません。"),
    # T35.9・T35.11。phase30 の 26 行のうち、表示だけの `/events` の行を要約と同じ数え方にし、heap の行から
    # キャッシュの持ち物を引いた
    "T35": ("26 行の並びと材料は phase30 と同じです。直したのは 2 行 (T35.9・T35.11): (1) `/events` の anomaly の件/時 "
                "(表示だけ) を、**要約 (`snapshot-summary.py`) の表と同じ関数**で数えます (起動は取得 − `status.uptime_secs`、"
                "`:` の無い文面は `?` に数える、リングが満杯で起動より前の出来事が残っていなければ「切れている」。件/時は 3 桁)。"
                "phase30 までの数え方とは起動の時刻と切れの読み方が違い、同じ雪像でも要約と行がずれることがありました。"
                "(2) heap の行は `heap_used + mmap − cache_memory` の起動 60 秒の基準からの増えから、さらに**キャッシュの"
                "持ち物** (本文の確保の端数と索引・鍵) を引いた残りで読みます (キャッシュがメモリを使うのは良いことで、"
                "届かずにしない)。`cache_memory` は本文の長さの和で、長さの分からない (chunked の) 本文は 64 KiB から"
                "倍々で育った確保のまま置かれていた (T35.11 で手元で測った。0.35 からは保存の最後で余りを返す)。"
                "`/status` の `cache.memory.alloc_bytes` (本文の確保の合計。0.35 から) があればその差で 1 つの値に決め、"
                "無い版では端数を 0〜(`used_bytes` + 件数 × 64 KiB) の幅で引き、残りの上下限が閾をまたげば「判定できず」(上下限を書く) です。"
                "**その部が雪像に無い行は「判定できず」**で、0 とは書きません。"),
    # T36.1・T36.2・T36.3。T35 の 26 行のうち、heap の行から名前解決の表の持ち物も引き、前が 0 件だと読めなかった 2 行に
    # 下限を置き、CPU の 2 行を機械の速さの物差しで割ってから比べ、物差しの前後の比を表示だけの 27 行目に出した
    "T36": ("1〜26 行の並びと材料は T35 と同じで、27 行目に表示だけの行を 1 つ足しました。直したのは 5 行 (T36.1・T36.2・T36.3): (1) heap の行は、`heap_used + mmap − cache_memory` の"
            "起動 60 秒の基準からの増えから、T35 と同じキャッシュの持ち物に加えて**名前解決の表の持ち物** "
            "(`/status` の `dns.entries` × 1 名前あたり 1,070 B) も引いた残りで読みます。表は上限 4,096 名前で止まる増えで、"
            "漏れではありません (0.35.12 の締めで +5.7 MB の届かずだったのはこれ)。1 名前あたりの大きさは、実バイナリの"
            "表を埋めて `/status` を前後で読んで測りました。基準の時点の名前もまとめて引くので、多めに引く側に倒れます。"
            "`dns.entries` の無いスナップショットは 0〜上限ぶんの幅で引き、残りの上下限が閾をまたげば「判定できず」(上下限を書く) です。"
            "(2) 接続のエラー (443・80) の率は、前の版の 1.5 倍未満**か、後が 0.001 未満**なら「満たした」です (前が 0 件だと、"
            "後が 1 件でも届かずになっていた。T34.999 の 13 / 45,577)。(3) `dns_slow` は、前の版に成功したミスで立った窓が"
            "無ければ、比べる平均が無いので**後の平均を 500 ms 未満**の絶対値の閾で読みます (tokyo の立った窓の平均は "
            "144〜334 ms だった。その最大の 1.5 倍)。"
            "(4) `conn` 役の CPU ÷ 名前解決に成功した要求と、暇なときの費用 (裏の役割の us/秒) の 2 行は、前と後をそれぞれ"
            "**機械の速さの物差し**で割ってから、前の版の 1.1 倍未満かを見ます (後の値を前の機械の値段に直して書き、"
            "割る前の値は表示だけ)。物差しは `/profile` の `history` 役の CPU ÷ その役の標本の数 (us/秒)。`history` は "
            "5 秒に 1 回の決まった仕事しかしないので、同じ仕事の値段だけが出ます (0.34.165 → 0.35.12 では、コードが同じ"
            "まま 1.36 倍になっていた)。物差しそのものの前後の比は 27 行目に表示だけで出し、参考に `profile-sample` 役の "
            "CPU ÷ 読んだスレッドの標本も並べます (暇な秒は全スレッドを読まないので、要求の量で動く)。"
            "**その部がスナップショットに無い行は「判定できず」**で、0 とは書きません。"),
    # T37.1。T36 の 27 行のうち、`dns_slow` の行を前の版との比ではなく絶対値で読む
    "T37": ("1〜27 行の並びと材料は T36 と同じです。直したのは 1 行 (T37.1): 11 行目の `dns_slow` が立った窓の、"
            "名前解決に成功したミスの平均を、前の版の 1.5 倍未満ではなく、**いつも 500 ms 未満**かで読みます "
            "(前の版の値と倍率は表示だけ)。立った窓は 443・80 宛ての成功したミスが数回しかない小さい窓で、"
            "窓ごとの平均は 100〜900 ms とぶれるので、小さい母数どうしの比は閾を越えやすかった (0.36.34 の締めで "
            "139 → 225 ms の 1.62 倍)。名前解決の遅さは上流のリゾルバが決めます。ほかの 26 行は T36 と同じです。"),
}


def render_criteria(d, grouped):
    """判定表 (「完了の定義に対する判定」の節) を印字する (`snapshot-diff.py` の `render` が `"criteria" in d` のときに呼ぶ)。

    `grouped` は `--group domain` が付いているか (「主要 3 ホスト」の行の単位が変わる)。
    見出しと前置き (`render_criteria_head`)・表 (`render_criteria_rows`)・集計 (`render_criteria_tally`)
    の 3 段に割ってある (T.34.130)。
    """
    c = d["criteria"]
    render_criteria_head(c, grouped)
    render_criteria_rows(c)
    render_criteria_tally(c)


def render_criteria_head(c, grouped):
    """見出しと、`--group domain` の断りと、その定義の前置き (`INTRO`。あるときだけ)。"""
    p = print
    p(f"## 9. 完了の定義に対する判定 (`--criteria {c['name']}`)")
    p()
    if grouped:
        # 「主要 3 ホスト」は「ホスト別」の上位から採るので、まとめると**単位**になる
        p("**`--group domain` を付けているので、「主要 3 ホスト」の行はまとめの単位 "
          "(eTLD+1) で判定している**。ホスト 1 件ずつで判定するには `--group` を外すこと。")
        p()
    if c["name"] in INTRO:
        p(INTRO[c["name"]])
        p()


def render_criteria_rows(c):
    """判定表の本体 (1 行 = `RULES` の 1 つの関数が返した `(名前, 閾, 実測, 判定, 出どころ)`)。"""
    p = print
    p("| 完了の定義 | 閾値 | 実測 (後の期間) | 判定 | 出どころ |")
    p("|---|---|---|---|---|")
    for row in c["rows"]:
        p(f"| {row[0]} | {row[1]} | {row[2]} | **{row[3]}** | {row[4]} |")
    p()


def render_criteria_tally(c):
    """表の下の集計 (満たした・届かず・判定できず の本数と、集計に数えない行の本数)。"""
    p = print
    # 集計に数えない行 (表示だけ・参考) はあるときだけ添える。phase20 の文字は変わらない
    others = [f"{name} {c[key]} 行" for key, name in (("display_only", SHOWN), ("reference", REFERENCE))
              if c.get(key)]
    p("・".join(f"{k} {v} 行" for k, v in c["tally"].items() if v)
      + (f" (ほかに{'・'.join(others)}。集計に数えない)" if others else ""))
    p()
