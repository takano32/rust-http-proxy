//! `/proc/self/task` を読んでスレッドの標本を取る側 ([`Sampler`])。

use super::procstat::{clock_tick, parse_stat_threads_ticks};
use super::rings::Profile;
use super::threads::{
    READ_NR, ROLES, STATE_RUNNING, TOP_THREADS, Threads, TopThread, role_of, state_slot,
};

/// スレッドの標本がどこまで取れているか (`/profile` の `sampler`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplerState {
    /// `PROXY_PROFILE_SAMPLE_MS=0`、または `/proc/self/task` が読めない
    Off = 0,
    /// スレッドは読めるが `syscall` が読めない (状態は `running` / `sleeping` だけ)
    Partial = 1,
    /// CPU も状態もシステムコールまで読めている
    On = 2,
}

impl SamplerState {
    pub fn name(self) -> &'static str {
        match self {
            SamplerState::Off => "off",
            SamplerState::Partial => "partial",
            SamplerState::On => "on",
        }
    }

    pub(super) fn from_u8(v: u8) -> SamplerState {
        match v {
            2 => SamplerState::On,
            1 => SamplerState::Partial,
            _ => SamplerState::Off,
        }
    }
}

/// [`Sampler`] が覚えておく、スレッド 1 本の前回の標本。
#[derive(Debug, Clone, Copy)]
struct PrevTask {
    /// utime + stime (clock tick)
    ticks: u64,
    /// utime (clock tick。T16.0)
    utime: u64,
    /// `stat` の状態
    state: char,
    /// `syscall` の番号 (読めなければ `None`)
    syscall: Option<i64>,
    /// `schedstat` の走れずに待った ns (読めなければ `None`)
    run_delay_ns: Option<u64>,
    /// 役割 ([`role_of`])。全スレッドを読まない秒に前回の状態を積むのに使う (T.32.37)
    role: usize,
}

/// スレッドの標本を取る側 (`profile-sample` スレッドが 1 つだけ持つ。T14.3 (2))。
///
/// **CPU はスレッドごとの増分**で積む。新しく生まれたスレッドは前回が無いので
/// 「生まれてからのぶん」がそのまま増分になり (正しい)、消えたスレッドは最後の
/// 1 標本ぶんだけ落ちる (役割別の CPU はその分だけ少なめに出る。`cpu_us` =
/// プロセス全体はこの取りこぼしが無いので、**CPU/要求 は正確**)。
pub(crate) struct Sampler {
    /// 普通は `/proc/self/task` (テストは差し替える)
    root: std::path::PathBuf,
    /// 主スレッド = accept 役の tid (= pid)
    main_tid: u32,
    /// 前回のスレッド別の標本 (増分を取るのと、休眠のままのスレッドの読みを省くため)
    prev: std::collections::HashMap<u32, PrevTask>,
    /// 読み取りの使い回し用
    buf: Vec<u8>,
    /// スレッドごとの `stat` を開いたまま持つ (T.32.21)
    files: crate::sysinfo::proc::TaskStatFiles,
    /// 標本を取るスレッド自身の tid ([`Sampler::for_self`] のときだけ。T.29.21)
    self_tid: Option<u32>,
    /// 1 clock tick の us
    tick_us: u64,
    /// 5 秒の窓へ渡す前の溜め
    pending: Threads,
    /// 同上、スレッド単位 (tid → その窓の CPU と `running` の数)。**窓ごとに空にする**
    pending_top: std::collections::HashMap<u32, TopThread>,
    /// 同上、役割ごとの「走れずに待った時間」(**ns**)。us へ落とすのは
    /// [`Sampler::take`] で 1 度だけ (標本ごとに割ると 1 us 未満の待ちが毎回消える)
    pending_run_delay: [u64; ROLES.len()],
    /// `schedstat` が 1 本でも読めたか (読めなければ `run_delay_us` は `null`)
    schedstat_readable: bool,
    /// 前に全スレッドを読んだときの (スレッド数, プロセスの utime + stime の tick, 要求の累計)。
    /// 次の秒も同じなら全スレッドを読まない (T.32.37)
    last_whole: Option<(u64, u64, u64)>,
    /// 全スレッドを読まずに済ませた回数 (試験用)
    skipped: u64,
}

/// スレッドの `stat` を開いたまま持つ本数の上限 (T.32.21)。
///
/// 暇なときは 10 本前後、`--only idle-tunnels --conc 5000` でも 132 本。越えたぶんは
/// 毎回開いて読む (記述子を接続のぶんから食い過ぎないように)。
const TASK_FILES_KEPT: usize = 128;

impl Sampler {
    pub(crate) fn new(root: std::path::PathBuf, main_tid: u32) -> Sampler {
        Sampler {
            root,
            main_tid,
            prev: std::collections::HashMap::new(),
            buf: Vec::new(),
            files: crate::sysinfo::proc::TaskStatFiles::new(TASK_FILES_KEPT),
            self_tid: None,
            tick_us: 1_000_000 / clock_tick(),
            pending: Threads::default(),
            pending_top: std::collections::HashMap::new(),
            pending_run_delay: [0; ROLES.len()],
            schedstat_readable: false,
            last_whole: None,
            skipped: 0,
        }
    }

    /// 1 回ぶん取る。**プロセスの CPU (tick) とスレッド数と要求の数が前回と同じ秒は
    /// 全スレッドを読まず**、前回の状態をもう 1 標本ぶん積む (T.32.37。利用者の決定 (a))。
    ///
    /// どのスレッドも 10 ms (1 tick) 走っていなければ、状態は「前と同じ所で寝ている」と
    /// みなせる (T.29.21 の「休眠のまま ticks が同じなら前回の値」をプロセス全体に広げた形)。
    /// 毎秒の仕事は `/proc/self/task` の readdir とスレッドごとの `stat` で、暇な秒に
    /// これを省くと `profile-sample` の CPU がおよそ半分になる。スレッドが生まれた・消えた秒は
    /// スレッド数が変わるので必ず読み直す。`/proc/self/stat` を読むのは [`Sampler::for_self`] のときだけ
    /// (試験が差し替えた `root` はこのプロセスのものではない)。
    pub(crate) fn sample_unless_idle(&mut self, profile: &Profile, requests: u64) {
        let whole = if self.self_tid.is_some() {
            crate::sysinfo::proc::read_small_into(
                std::path::Path::new("/proc/self/stat"),
                &mut self.buf,
            )
            .and_then(parse_stat_threads_ticks)
            .map(|(threads, ticks)| (threads, ticks, requests))
        } else {
            None
        };
        if whole.is_some() && whole == self.last_whole && !self.prev.is_empty() {
            self.repeat_last();
            return;
        }
        self.last_whole = whole;
        self.sample(profile);
    }

    /// 前回の状態をもう 1 標本ぶん積む (CPU と待ちの増えは 0。T.32.37)。
    fn repeat_last(&mut self) {
        self.skipped += 1;
        for (tid, p) in &self.prev {
            let w = &mut self.pending[p.role];
            w.samples += 1;
            let (slot, _) = state_slot(p.syscall, p.state);
            w.states[slot] += 1;
            let e = self.pending_top.entry(*tid).or_default();
            e.tid = *tid;
            e.role = p.role as u8;
            e.running += u32::from(slot == STATE_RUNNING);
        }
    }

    /// 自プロセス用 (`/proc/self/task`)。
    ///
    /// **標本を取るスレッドの中で呼ぶこと** (自分の tid を `/proc/thread-self` から覚え、
    /// 自分の `syscall` は読まずに `read` とする。T.29.21)。
    pub(crate) fn for_self() -> Sampler {
        let mut s = Sampler::new(
            std::path::PathBuf::from("/proc/self/task"),
            std::process::id(),
        );
        s.self_tid = std::fs::read_link("/proc/thread-self")
            .ok()
            .and_then(|p| p.file_name()?.to_str()?.parse().ok());
        s
    }

    /// 1 回ぶん取る。読めなければ [`SamplerState::Off`] を書いて何もしない。
    pub(crate) fn sample(&mut self, profile: &Profile) {
        // **休眠のまま ticks が前回と同じスレッドは `syscall` と `schedstat` を開かない**
        // (T.29.21)。走っていないので待ちの増えは 0、居るシステムコールも前回のまま
        // (10 ms 未満だけ走って別の所で寝直したスレッドは 1 標本古く見える。待ちの増えは
        // 次に読んだときに累計の差で入るので消えない)。暇なときの open は 1 本 3 → 1
        // **自分 (この標本を取っているスレッド) の `syscall` はいつも `read`** (自分の `syscall` を
        // 読むときはその `read` の中に居る) なので開かない
        let (prev, self_tid) = (&self.prev, self.self_tid);
        let reuse = |tid: u32, state: char, ticks: u64| match prev.get(&tid) {
            Some(p) if state == 'S' && p.state == 'S' && p.ticks == ticks => {
                crate::sysinfo::Known {
                    syscall: Some(p.syscall),
                    run_delay_ns: Some(p.run_delay_ns),
                }
            }
            _ => crate::sysinfo::Known {
                syscall: (Some(tid) == self_tid).then_some(Some(READ_NR)),
                run_delay_ns: None,
            },
        };
        let Some(scan) =
            crate::sysinfo::scan_tasks_reusing(&self.root, &mut self.buf, &mut self.files, reuse)
        else {
            profile.set_sampler(SamplerState::Off);
            return;
        };
        let mut next = std::collections::HashMap::with_capacity(scan.tasks.len());
        self.schedstat_readable |= scan.schedstat_readable;
        for t in &scan.tasks {
            next.insert(t.tid, self.account(profile, t));
        }
        self.prev = next;
        profile.set_sampler(if scan.syscalls_readable {
            SamplerState::On
        } else {
            SamplerState::Partial
        });
    }

    /// スレッド 1 本の読みを溜めに積み、次の回に覚えておく値を返す (増分は前回との差)。
    fn account(&mut self, profile: &Profile, t: &crate::sysinfo::proc::TaskSample) -> PrevTask {
        let role = role_of(t.tid, self.main_tid, &t.comm);
        let (prev_ticks, prev_utime, prev_delay) = self.prev.get(&t.tid).map_or((0, 0, 0), |p| {
            (p.ticks, p.utime, p.run_delay_ns.unwrap_or(0))
        });
        let delay_ns = t.run_delay_ns.unwrap_or(0);
        let cpu_us = t.ticks.saturating_sub(prev_ticks) * self.tick_us;
        let w = &mut self.pending[role];
        w.cpu_us += cpu_us;
        // ユーザー空間も同じ流儀の増分 (T16.0。同じ `stat` の 1 回の読みから取るので、
        // 役割ごとには user <= 合計 がいつも成り立つ)
        w.user_us += t.utime.saturating_sub(prev_utime) * self.tick_us;
        w.samples += 1;
        let (slot, unknown) = state_slot(t.syscall, t.state);
        w.states[slot] += 1;
        if let Some(nr) = unknown {
            profile.note_unknown_syscall(nr);
        }
        // **走れるのに走れなかった時間**も増分で積む (はじめて見たスレッドは
        // 「生まれてからのぶん」がそのまま入る。CPU と同じ流儀)。
        // **ns のまま積む** — ここで us に落とすと 1 us 未満の待ちが標本ごと・
        // スレッドごとにまるごと消え、140 スレッド × 5 標本の窓で最大 700 us
        // (平均 350 us) ぶん**下向きに**外れる。割るのは [`Sampler::take`] で 1 度だけ
        self.pending_run_delay[role] += delay_ns.saturating_sub(prev_delay);
        // スレッド単位 (上位 8 本を切り出す元。**窓のあいだだけ持つ**)。
        // **名前と役割は標本ごとに書き直す**: Linux は `clone` のとき子に親の `comm` を
        // 継がせ、子が `pthread_setname_np` を呼ぶまでそのままなので、はじめて見たときの
        // 値を入れっぱなしにすると、生まれたばかりのスレッドが**親の名前**のまま
        // 窓じゅう (60 秒の窓まで) 居座る。この欄は tid と名前で犯人を指すためのもの
        let e = self.pending_top.entry(t.tid).or_default();
        e.tid = t.tid;
        e.role = role as u8;
        e.comm = TopThread::comm_bytes(&t.comm);
        e.cpu_us += cpu_us;
        e.running += u32::from(slot == STATE_RUNNING);
        PrevTask {
            ticks: t.ticks,
            utime: t.utime,
            state: t.state,
            syscall: t.syscall,
            run_delay_ns: t.run_delay_ns,
            role,
        }
    }

    /// 溜めた標本を取り出して 0 に戻す (5 秒の窓へ)。
    ///
    /// 返すのは (役割ごと, CPU の多い順の上位 [`TOP_THREADS`] 本, 役割ごとの
    /// 走れずに待った時間 (`schedstat` が読めなければ `None`))。
    pub(super) fn take(
        &mut self,
    ) -> (
        Threads,
        [TopThread; TOP_THREADS],
        Option<[u64; ROLES.len()]>,
    ) {
        // **何もしなかったスレッドは残さない** (暇なとき 140 本のうち 8 本を
        // 「CPU 0 の役立たず」で埋めても読む人の助けにならない)
        let mut all: Vec<TopThread> = self
            .pending_top
            .drain()
            .map(|(_, t)| t)
            .filter(|t| t.cpu_us > 0 || t.running > 0)
            .collect();
        all.sort_unstable_by(|x, y| y.cpu_us.cmp(&x.cpu_us).then(x.tid.cmp(&y.tid)));
        let mut top = [TopThread::default(); TOP_THREADS];
        for (o, t) in top.iter_mut().zip(all.iter()) {
            *o = *t;
        }
        // ns で積んであるので、**ここで 1 度だけ** us に落とす
        let mut run_delay = std::mem::take(&mut self.pending_run_delay);
        for v in run_delay.iter_mut() {
            *v /= 1_000;
        }
        let run_delay = self.schedstat_readable.then_some(run_delay);
        (std::mem::take(&mut self.pending), top, run_delay)
    }
}

#[cfg(test)]
#[path = "sampler_tests.rs"]
mod tests;
