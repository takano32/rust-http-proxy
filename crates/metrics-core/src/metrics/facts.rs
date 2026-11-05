//! `/status` の組み立てが読む、このプロセスの値 (`/proc`・`mallinfo2`・`boot_id`。T34.41)。
//!
//! 組み立て (`Metrics::to_json_with_facts`) はこれを引数で受けるので、固定の値を渡せば
//! `/status` の欄をバイト単位で試験できる。本番の [`LiveProcess`] は**前と同じ場所で同じ順に**読む
//! (読むのはどれも `/status` に来たときだけ。要求の経路からは呼ばない)。

/// `/status` が読むこのプロセスの値。どれも読めなければ `None` (`/status` は落とさない)。
pub(super) trait ProcessFacts {
    /// スレッドの本数 (`/proc/self/status` の `Threads`)
    fn threads(&self) -> Option<u64>;
    /// 開いている記述子の数と上限 (`/proc/self/fd` と `RLIMIT_NOFILE`)
    fn fds(&self) -> Option<(u64, u64)>;
    /// 常駐の内訳 (`VmRSS`・`RssAnon`・`RssFile`・`RssShmem`。T32.4)
    fn rss(&self) -> Option<crate::sysinfo::ProcessRss>;
    /// スタックの写像の常駐の和 (smaps。T32.4)
    fn stacks_resident(&self) -> Option<u64>;
    /// ヒープの内訳 (`mallinfo2`。T14.21)
    fn heap(&self) -> Option<crate::sysinfo::MallocInfo>;
    /// `boot` の欄 (`{"id":…,"pid":…}`。T24.0)
    fn boot_json(&self) -> String;
}

/// 本番の値 (今のプロセスを読む)。
pub(super) struct LiveProcess;

impl ProcessFacts for LiveProcess {
    fn threads(&self) -> Option<u64> {
        crate::sysinfo::process_threads()
    }

    fn fds(&self) -> Option<(u64, u64)> {
        crate::sysinfo::process_fds()
    }

    fn rss(&self) -> Option<crate::sysinfo::ProcessRss> {
        crate::sysinfo::process_rss_detail()
    }

    fn stacks_resident(&self) -> Option<u64> {
        crate::sysinfo::stacks_resident()
    }

    fn heap(&self) -> Option<crate::sysinfo::MallocInfo> {
        crate::sysinfo::malloc_info()
    }

    fn boot_json(&self) -> String {
        super::status::boot_json()
    }
}
