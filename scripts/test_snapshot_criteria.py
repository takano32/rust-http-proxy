#!/usr/bin/env python3
"""`scripts/snapshot-diff.py --criteria` (判定表。中身は `scripts/snapshot_criteria.py`) の単体テストの入り口。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

ここには判定表の全体を見る試験 (`RULES` の並びを固定する `RulesOrder` と、判定表の名前の別名を見る
`CriteriaAliases`) だけを置く。判定表ごとの試験は、`scripts/criteria/` の割り方に合わせて次のファイルにある
(T38.6。このファイルが 3,874 行になったので割った。試験の本数と中身は割る前と同じ)。

    test_snapshot_criteria_phase14_18.py   phase14・phase15・phase17・phase18 と `--daily-snapshots` の表
    test_snapshot_criteria_phase20_22.py   phase20・phase21・phase22
    test_snapshot_criteria_phase23_25.py   phase23・phase24・phase25
    test_snapshot_criteria_phase26_28.py   phase26・phase27・phase28
    test_snapshot_criteria_phase29_30.py   phase29・phase30
    test_snapshot_criteria_t35.py          T35
    test_snapshot_criteria_t36.py          T36
    test_snapshot_criteria_t37.py          T37
    test_snapshot_criteria_t38.py          T38
    test_snapshot_criteria_t39.py          T39
    test_snapshot_criteria_t40.py          T40

どのファイルも使う作り物は `scripts/_criteria_testing.py` にある。`Deployed*` はデプロイ先のスナップショット
(リポジトリの `status/`。リポジトリには入れない) があるときだけ回る。
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _criteria_testing import sc  # noqa: E402

# **T32.34 で消した実データの試験 (どの機械でも回らなかった 18 本)**: 次の 9 つの組は `status/` 直下の nagoya のスナップショット
# (2026-09-24〜2026-10-08) と、もう手元に無い tokyo の古いスナップショット (2026-10-08〜2026-10-22) を待っていて、main の
# `status/` を繋いでも飛ばされていた (スナップショットはリポジトリに入れず、tokyo の手元の `status/tokyo/` にも 10-22 より前は残って
# いない)。確かめていた数字は各タスクの `結果` (`tasks/T17.md`〜`tasks/T24.md`) にあり、判定の関数そのものは同じ番号の
# `Criteria*` の作り物の試験が見ている。
#   Deployed17 (T17.0a: 09-24 → 09-26 で T16.99 の数字)、Deployed18 (T18.2: 09-26 → 10-01 と 0 時間)、
#   Deployed20 (T20.1: 10-01 → 10-08 の phase20)、Deployed21 (T21.2: tokyo 10-08 → 10-15 の phase21)、
#   Deployed22 (T22.1: 過去の 3 組に phase22 の切り方を当てても判定が変わらない)、Deployed23 (T23.2: 10-20 の無言の再起動)、
#   Deployed24 (T24.0: 10-22 の起動の切り分け)、Deployed24Rows (T24.2: 10-17 → 10-20 の phase24)、
#   Deployed243Rows (T24.3: 10-22 と 10-20 の確立の尾)。
# 残る実データの試験は、いまの `status/tokyo/` にある 1 枚を使うものだけ (飛ばす理由は「tokyo の実データが無い」)。


class RulesOrder(unittest.TestCase):
    """`RULES` の各段の並びを、関数の名前 (`snapshot_criteria` の中の名前) で固定する (T.34.129)。

    段の組み立てを添字の切り貼りから「何番目を差し替える」の書き方に直したときに、並びが 1 つも
    動いていないことをここで見る。`display_only` で包んだ行は包みの名前 (`_p20_watch_host` など) で引く。
    """

    WANT = {
        "phase14": (
            "_p14_dns_per_connect", "_p14_major_hosts", "_p14_connect_p50", "_p14_overload",
        ),
        "phase15": (
            "_p15_watch_host", "_p15_refresh_rate", "_p15_miss_band", "_p15_conn_cores", "_p15_closed_shape",
            "_p15_timeout",
        ),
        "phase17": (
            "_p15_watch_host", "_p15_refresh_rate", "_p15_miss_band", "_p15_timeout", "_p17_conn_per_request",
            "_p17_warm_max", "_p17_events", "_p17_cgroup",
        ),
        "phase18": (
            "_p15_watch_host", "_p15_refresh_rate", "_p15_miss_band", "_p15_timeout", "_p17_conn_per_request",
            "_p17_warm_max", "_p17_events", "_p17_cgroup", "_p18_heap", "_p18_request_probes",
        ),
        "phase20": (
            "_p20_watch_host", "_p15_refresh_rate", "_p15_miss_band", "_p20_timeout", "_p17_conn_per_request",
            "_p20_warm_max", "_p17_events", "_p17_cgroup", "_p20_heap", "_p18_request_probes",
        ),
        "phase21": (
            "_p20_watch_host", "_p15_refresh_rate", "_p21_miss_band", "_p20_timeout", "_p21_conn_per_request",
            "_p20_warm_max", "_p17_events", "_p17_cgroup", "_p20_heap", "_p21_request_probes",
        ),
        "phase22": (
            "_p20_watch_host", "_p22_refresh_rate", "_p21_miss_band", "_p20_timeout", "_p21_conn_per_request",
            "_p20_warm_max", "_p22_events", "_p17_cgroup", "_p20_heap", "_p21_request_probes",
            "_p22_dns_slow", "_p22_stale_retries", "_p22_connect_errors", "_p22_refresh_and_stale_misses",
        ),
        "phase23": (
            "_p20_watch_host", "_p22_refresh_rate", "_p23_miss_band", "_p20_timeout", "_p21_conn_per_request",
            "_p20_warm_max", "_p22_events", "_p17_cgroup", "_p23_heap", "_p21_request_probes",
            "_p22_dns_slow", "_p22_stale_retries", "_p23_connect_errors", "_p22_connect_errors_shown",
            "_p22_refresh_and_stale_misses", "_p23_silent_restarts",
        ),
        "phase24": (
            "_p20_watch_host", "_p22_refresh_rate", "_p23_miss_band", "_p20_timeout", "_p21_conn_per_request",
            "_p20_warm_max", "_p22_events", "_p17_cgroup", "_p24_heap", "_p21_request_probes",
            "_p22_dns_slow", "_p22_stale_retries", "_p23_connect_errors", "_p22_connect_errors_shown",
            "_p22_refresh_and_stale_misses", "_p24_silent_restarts", "_p24_connect_tail",
        ),
        "phase25": (
            "_p20_watch_host", "_p22_refresh_rate", "_p23_miss_band", "_p20_timeout", "_p21_conn_per_request",
            "_p20_warm_max", "_p22_events", "_p17_cgroup", "_p24_heap", "_p21_request_probes",
            "_p22_dns_slow", "_p22_stale_retries", "_p25_connect_errors", "_p22_connect_errors_shown",
            "_p25_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail",
        ),
        "phase26": (
            "_p20_watch_host", "_p22_refresh_rate", "_p23_miss_band", "_p20_timeout", "_p21_conn_per_request",
            "_p20_warm_max", "_p22_events", "_p17_cgroup", "_p24_heap", "_p21_request_probes",
            "_p22_dns_slow", "_p22_stale_retries", "_p25_connect_errors", "_p22_connect_errors_shown",
            "_p25_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail", "_p26_header_timeouts",
            "_p26_log_errors", "_p26_local_blocks", "_p26_connect_cut", "_p26_rejected", "_p26_rss_gap",
            "_p26_cgroup_cpu", "_p26_role_shares", "_p26_origin_warn_per_minute",
        ),
        "phase27": (
            "_p20_watch_host", "_p22_refresh_rate", "_p23_miss_band", "_p20_timeout", "_p21_conn_per_request",
            "_p20_warm_max", "_p22_events", "_p17_cgroup", "_p24_heap", "_p21_request_probes",
            "_p22_dns_slow", "_p22_stale_retries", "_p27_connect_errors", "_p22_connect_errors_shown",
            "_p25_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail", "_p26_header_timeouts",
            "_p26_log_errors", "_p26_local_blocks", "_p26_connect_cut", "_p26_rejected", "_p27_rss_gap",
            "_p26_cgroup_cpu", "_p26_role_shares", "_p26_origin_warn_per_minute",
        ),
        "phase28": (
            "_p20_watch_host", "_p22_refresh_rate", "_p23_miss_band", "_p20_timeout", "_p21_conn_per_request",
            "_p20_warm_max", "_p22_events", "_p17_cgroup", "_p24_heap", "_p21_request_probes",
            "_p22_dns_slow", "_p22_stale_retries", "_p27_connect_errors", "_p22_connect_errors_shown",
            "_p25_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail", "_p26_header_timeouts",
            "_p26_log_errors", "_p26_local_blocks", "_p26_connect_cut", "_p26_rejected", "_p28_trim_works",
            "_p28_cgroup_cpu_per_request", "_p26_role_shares", "_p26_origin_warn_per_minute",
        ),
        "phase29": (
            "_p20_watch_host", "_p22_refresh_rate", "_p29_miss_band", "_p20_timeout",
            "_p29_conn_per_resolved_request", "_p20_warm_max", "_p22_events", "_p17_cgroup", "_p24_heap",
            "_p21_request_probes", "_p29_dns_slow", "_p22_stale_retries", "_p29_connect_errors",
            "_p22_connect_errors_shown", "_p25_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail",
            "_p26_header_timeouts", "_p26_log_errors", "_p26_local_blocks", "_p26_connect_cut",
            "_p26_rejected", "_p28_trim_works", "_p29_cgroup_cpu_shown", "_p29_idle_cost",
            "_p26_origin_warn_per_minute",
        ),
        "phase30": (
            "_p20_watch_host", "_p22_refresh_rate", "_p29_miss_band", "_p20_timeout",
            "_p29_conn_per_resolved_request", "_p20_warm_max", "_p22_events", "_p17_cgroup", "_p24_heap",
            "_p21_request_probes", "_p30_dns_slow", "_p22_stale_retries", "_p29_connect_errors",
            "_p22_connect_errors_shown", "_p30_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail",
            "_p26_header_timeouts", "_p26_log_errors", "_p26_local_blocks", "_p26_connect_cut",
            "_p26_rejected", "_p28_trim_works", "_p29_cgroup_cpu_shown", "_p29_idle_cost",
            "_p26_origin_warn_per_minute",
        ),
        "T35": (
            "_p20_watch_host", "_p22_refresh_rate", "_p29_miss_band", "_p20_timeout",
            "_p29_conn_per_resolved_request", "_p20_warm_max", "_t35_events", "_p17_cgroup", "_t35_heap",
            "_p21_request_probes", "_p30_dns_slow", "_p22_stale_retries", "_p29_connect_errors",
            "_p22_connect_errors_shown", "_p30_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail",
            "_p26_header_timeouts", "_p26_log_errors", "_p26_local_blocks", "_p26_connect_cut",
            "_p26_rejected", "_p28_trim_works", "_p29_cgroup_cpu_shown", "_p29_idle_cost",
            "_p26_origin_warn_per_minute",
        ),
        "T36": (
            "_p20_watch_host", "_p22_refresh_rate", "_p29_miss_band", "_p20_timeout",
            "_t36_conn_per_resolved_request", "_p20_warm_max", "_t35_events", "_p17_cgroup", "_t36_heap",
            "_p21_request_probes", "_t36_dns_slow", "_p22_stale_retries", "_t36_connect_errors",
            "_p22_connect_errors_shown", "_p30_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail",
            "_p26_header_timeouts", "_p26_log_errors", "_p26_local_blocks", "_p26_connect_cut",
            "_p26_rejected", "_p28_trim_works", "_p29_cgroup_cpu_shown", "_t36_idle_cost",
            "_p26_origin_warn_per_minute", "_t36_machine_speed",
        ),
        "T37": (
            "_p20_watch_host", "_p22_refresh_rate", "_p29_miss_band", "_p20_timeout",
            "_t36_conn_per_resolved_request", "_p20_warm_max", "_t35_events", "_p17_cgroup", "_t36_heap",
            "_p21_request_probes", "_t37_dns_slow", "_p22_stale_retries", "_t36_connect_errors",
            "_p22_connect_errors_shown", "_p30_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail",
            "_p26_header_timeouts", "_p26_log_errors", "_p26_local_blocks", "_p26_connect_cut",
            "_p26_rejected", "_p28_trim_works", "_p29_cgroup_cpu_shown", "_t36_idle_cost",
            "_p26_origin_warn_per_minute", "_t36_machine_speed",
        ),
        "T38": (
            "_p20_watch_host", "_p22_refresh_rate", "_p29_miss_band", "_p20_timeout",
            "_t38_conn_per_resolved_request", "_p20_warm_max", "_t35_events", "_p17_cgroup", "_t36_heap",
            "_p21_request_probes", "_t37_dns_slow", "_p22_stale_retries", "_t38_connect_errors",
            "_p22_connect_errors_shown", "_p30_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail",
            "_p26_header_timeouts", "_p26_log_errors", "_p26_local_blocks", "_p26_connect_cut",
            "_p26_rejected", "_p28_trim_works", "_p29_cgroup_cpu_shown", "_t36_idle_cost",
            "_p26_origin_warn_per_minute", "_t36_machine_speed",
        ),
        "T39": (
            "_p20_watch_host", "_p22_refresh_rate", "_p29_miss_band", "_p20_timeout",
            "_t39_conn_priced", "_p20_warm_max", "_t35_events", "_p17_cgroup", "_t36_heap",
            "_p21_request_probes", "_t37_dns_slow", "_p22_stale_retries", "_t38_connect_errors",
            "_p22_connect_errors_shown", "_p30_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail",
            "_p26_header_timeouts", "_p26_log_errors", "_p26_local_blocks", "_p26_connect_cut",
            "_p26_rejected", "_p28_trim_works", "_p29_cgroup_cpu_shown", "_t36_idle_cost",
            "_p26_origin_warn_per_minute", "_t36_machine_speed",
        ),
        "T40": (
            "_p20_watch_host", "_p22_refresh_rate", "_p29_miss_band", "_p20_timeout",
            "_t40_conn_priced", "_p20_warm_max", "_t35_events", "_p17_cgroup", "_t36_heap",
            "_p21_request_probes", "_t37_dns_slow", "_p22_stale_retries", "_t38_connect_errors",
            "_p22_connect_errors_shown", "_p30_refresh_per_name", "_p25_silent_restarts", "_p24_connect_tail",
            "_p26_header_timeouts", "_p26_log_errors", "_p26_local_blocks", "_p26_connect_cut",
            "_p26_rejected", "_p28_trim_works", "_p29_cgroup_cpu_shown", "_t36_idle_cost",
            "_p26_origin_warn_per_minute", "_t36_machine_speed",
        ),
    }

    def test_each_phase_lists_the_same_rules_in_the_same_order(self):
        names = {}
        for key, value in sorted(vars(sc).items()):
            if key.startswith(("_p", "_t")) and callable(value):
                names.setdefault(id(value), key)
        got = {phase: tuple(names.get(id(rule), "?") for rule in rules) for phase, rules in sc.RULES.items()}
        self.assertEqual(list(got), list(self.WANT))
        for phase, want in self.WANT.items():
            self.assertEqual(got[phase], want, phase)


class CriteriaAliases(unittest.TestCase):
    """判定表の名前はタスクの番号 (T35.14)。phase14〜30 はそのまま残り、T14〜T33 は同じ判定表の別名。"""

    def test_each_alias_names_an_existing_criteria(self):
        for alias, name in sc.CRITERIA_ALIASES.items():
            self.assertIn(name, sc.CRITERIA, alias)
            self.assertNotIn(alias, sc.CRITERIA, alias)
            self.assertEqual(sc.criteria_name(alias), name)

    def test_the_new_criteria_is_named_after_its_task(self):
        self.assertIn("T35", sc.CRITERIA)
        self.assertEqual(sc.criteria_name("T35"), "T35")
        self.assertEqual(sc.criteria_name("phase30"), "phase30")


if __name__ == "__main__":
    unittest.main()
