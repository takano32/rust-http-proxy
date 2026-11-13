//! 接続 1 本の枠 ([`ConnSlot`]) とその状態 ([`ConnState`])。`recent` の一部 (T25.2 で
//! `recent.rs` から `conns.rs` へ移し、T34.2 で表 ([`ConnTable`](super::ConnTable)) と分けた)。
//!
//! **状態と転送バイトは原子で書く。** 接続の経路が触るのは原子の読み書きだけで、表の鍵は
//! 登録と抹消 (表の側) にしか無い。`/connections` の 1 行 ([`ConnSlot::to_json`]) と閉じた
//! 接続の個票 ([`ConnSlot::closed_entry`]) は「いま」を引数で受けるので、単体試験は時計を
//! 進めずに書ける。半閉じの詰め方 ([`pack_half_closed`] / [`unpack_half_closed`]) と `poll` の
//! 旗の名前 ([`revents_names`]) は純粋な関数。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `recent` の `pub use` でそのまま。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use super::clip::clip;
use super::errors::{MAX_CLIENT, MAX_MS, MAX_SECS, MAX_TARGET};
use super::{
    CLIENT_SIDE, CloseReason, ConnTally, MAX_RECENT_TARGET, MAX_SNI, ORIGIN_SIDE, RecentEntry,
    SIDE_NAMES, SIDES, STAGES,
};
use crate::sync::LockExt;

/// 接続 1 本の状態 (`/connections` の `state`)。
///
/// 更新するのは**要求ごとではない場所**だけ: 接続を受けたとき、トンネルを開いたとき、
/// 預けたとき、起こしてワーカーに渡すとき、ワーカーが取ったとき。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConnState {
    /// ワーカースレッドが持っていて、要求を処理している
    Serving = 0,
    /// ワーカースレッドが持ったまま次の要求を待っている (預けられなかった接続)
    Reading = 1,
    /// 預かり所 (epoll) にいる。スレッドは握っていない
    Parked = 2,
    /// 起こされてワーカーの空きを待っている
    Queued = 3,
    /// CONNECT トンネルとして中継中
    Relaying = 4,
}

impl ConnState {
    pub fn name(self) -> &'static str {
        match self {
            ConnState::Serving => "serving",
            ConnState::Reading => "reading",
            ConnState::Parked => "parked",
            ConnState::Queued => "queued",
            ConnState::Relaying => "relaying",
        }
    }

    fn from_u8(v: u8) -> ConnState {
        match v {
            1 => ConnState::Reading,
            2 => ConnState::Parked,
            3 => ConnState::Queued,
            4 => ConnState::Relaying,
            _ => ConnState::Serving,
        }
    }
}

/// 接続 1 本の今の姿。[`ConnTable`](super::ConnTable) と接続自身が同じものを [`Arc`](std::sync::Arc) で持つ。
///
/// **状態と転送バイトは原子で書く。** 表の `HashMap` の鍵を取るのは登録と抹消の
/// 2 回だけで、預ける / 戻す / 中継を始める のどれも表を止めない。バーストで 240 本が
/// 一斉に預け直す場面こそ `/connections` で見たい場面なので、**見るための仕掛けが
/// そこに鍵を 1 つ増やしてはいけない**。
pub struct ConnSlot {
    /// 接続の通し番号 (ログの `conn#N` と同じ)
    pub id: u64,
    /// 接続元 IP (接続ごとに 1 回だけ作った文字列の複製)
    pub client: String,
    /// 受けた時刻 (`age_secs` を出すため)
    pub started: Instant,
    state: AtomicU8,
    /// 宛先。**書くのは 1 本につき 1 回だけ**: CONNECT はトンネルを開いたとき
    /// ([`ConnSlot::begin_tunnel`])、keep-alive の HTTP 接続は**最初の要求**のとき
    /// ([`ConnSlot::set_first_target`]。T14.2)。要求ごとには触らない
    pub(super) target: Mutex<String>,
    /// CONNECT トンネルか (`false` = keep-alive の HTTP)
    connect: AtomicBool,
    /// 運んだ合計バイト数 (トンネルは中継が暇になるたび、keep-alive の HTTP は
    /// **応答を書き終えて次の要求を待つ直前に 1 回だけ**書く。T13.4 / T24.5)。
    /// どちらも**要求ごと・バイトごとには書かない**
    bytes: AtomicU64,
    /// 閉じた理由の符号 (0 = まだ決まっていない)。**先に書いた方が勝つ**
    /// (T13.2 の追い出しや監視スレッドの停止は、接続自身が理由を決める前に書くため)
    close: AtomicU16,
    /// 接続の終了で 1 回だけ書く値 (上り / 下り / 状態コード / 要求数 / 段階の ms)。
    /// 要求ごとの積み上げは接続を持っているスレッドの箱 ([`ConnTally`]) で行う
    up: AtomicU64,
    down: AtomicU64,
    status: AtomicU16,
    requests: AtomicU32,
    stage_ms: [AtomicU32; STAGES],
    /// カーネルの RTT (us) と再送の通算 (`[クライアント側, オリジン側]`。T14.5)。
    /// 読むのは接続の終わりの `getsockopt` だけで、`0` は「読めなかった」
    rtt_us: [AtomicU32; SIDES],
    retrans: [AtomicU32; SIDES],
    /// 預けられた回数と、預かり所にいた合計 ms、いま預けられた時刻
    /// (接続を受けてからの ms。[`NOT_PARKED`] なら預けられていない)。
    /// **書くのは預ける / 引き上げる瞬間だけ**で、要求ごとには触らない
    parks: AtomicU32,
    parked_ms: AtomicU64,
    parked_at: AtomicU64,
    /// 追跡中の接続元か (`PROXY_TRACE_CLIENT`。T14.27)。**立てるのは accept 直後の
    /// 1 回だけ**で、以降は要求ごとに読むだけ (要求ごとの比較はこの旗 1 つ)
    traced: AtomicBool,
    /// いまの転送速度を出すための 2 つ (T14.39)。`bytes_prev` は**前の周期で控えた
    /// `bytes`**、`rate_bps` はその差分 ÷ 周期 (バイト/秒)。**書くのは history
    /// スレッドだけ** ([`ConnSlot::sweep_rate`]) で、接続の経路は 1 命令も触らない
    /// (中継は今までどおり [`ConnSlot::set_bytes`] で累計を置くだけ)
    bytes_prev: AtomicU64,
    rate_bps: AtomicU64,
    /// CONNECT の最初のバイトから覗いた SNI (`PROXY_PEEK_SNI`。T14.38)。
    /// **書くのはトンネル 1 本につき多くて 1 回** (覗けて名前が読めたときだけ) で、
    /// 空は「覗いていない / 読めなかった」。443 以外と `--lite` では常に空
    sni: Mutex<String>,
    /// 確立までに SYN を送り直した回数 (T14.46)。**書くのは接続の終わりの
    /// [`ConnSlot::finish`] だけ**で、読んだのは `net` の確立点 (`getsockopt` 1 回)
    syn_retrans: AtomicU8,
    /// 中継が**書けるのを待った**合計 ms (`[クライアント側, オリジン側]`。T14.42)。
    /// `client` が大きい = 利用者の下り回線か端末が読まない、`origin` が大きい =
    /// オリジンか利用者の上りが詰まっている。**書くのは接続の終わりの 1 回だけ**
    /// ([`ConnSlot::finish`]) で、中継のループ (splice の往復) は 1 度も触らない
    stall_ms: [AtomicU32; SIDES],
    /// いまこの接続を受け持っているスレッドの番号 (`gettid`。T15.0)。
    /// **`0` = 預かり所にいる / 分からない**。書くのは「ワーカーが取ったとき」と
    /// 「預けるとき (0 に戻す)」だけで、要求ごとにも中継のバイトごとにも触らない。
    /// `/profile` の `threads_top` の `tid` と同じ番号なので、**CPU を食っている
    /// スレッドと、そのスレッドが抱えている接続**を突き合わせられる
    tid: AtomicU32,
    /// 中継の輪が「`poll` に起こされたのに 1 バイトも進まなかった」回数の通算
    /// (T15.0)。T15.5 の空回りはこれが秒あたり数万で増え続ける形だった。
    /// **書くのは待ちに入るたびの 1 回** ([`ConnSlot::set_relaying`]) で、数え上げ
    /// そのものは中継の輪が持つ欄 (`Idle::spins`)
    spins: AtomicU64,
    /// **前の周の** `poll` が返した旗 (`i16` 2 つを詰めた。T15.0)。
    /// 下位 16 bit がクライアント側、上位 16 bit がオリジン側。`0` は
    /// 「まだ待ちに入っていない / 何も立っていない」。`POLLHUP` や `POLLERR` が
    /// 立ったまま進まない = 空回りの型。
    /// **預かり所から戻った直後の 1 回は旗を持たない** (`0` を書く): 旗は中継の輪の
    /// 局所変数で、`run_until_idle` に入り直すたびに 0 から始まるため
    revents: AtomicU32,
    /// 半閉じの向きと、それが起きた時刻 (T15.0)。**上位 8 bit = 側 + 1**
    /// (`0` = まだ半閉じしていない)、**下位 56 bit = 接続を受けてからの ms**。
    /// [`ConnSlot::parked_at`] と同じ流儀で **1 本につき 1 回だけ**書き、
    /// `/connections` は `age − それ` で秒を、`/recent` は同じ引き算で ms を出す
    half_closed: AtomicU64,
    /// 同じ `bytes` のまま経った ms (T15.0)。**書くのは history スレッドだけ**
    /// ([`ConnSlot::sweep_rate`]) で、接続の経路は 1 命令も触らない。
    /// `bytes` を置くのはトンネル (待ちに入る回) と keep-alive の HTTP (要求を
    /// 書き終えた回。T24.5) なので、http の行では**要求と要求のあいだ**だけ伸びる。
    /// **`bytes` を書くのは中継が待ちに入る回だけ**なので、`EAGAIN` に落ちずに進み
    /// 続ける飽和したトンネルは、全速で流れていてもここが伸びる (`rate_bps` も同じ
    /// 性質。`spins` と併せて読む)
    idle_ms: AtomicU64,
    /// **前の周期で控えた** [`ConnSlot::spins`] と、その差分 (T15.0)。
    /// [`ConnSlot::bytes_prev`] / [`ConnSlot::rate_bps`] と同じ流儀で、
    /// **書くのは history スレッドだけ** ([`ConnSlot::sweep_rate`])。
    /// 通算の `spins` は「今までに何回空回りしたか」しか言わないので、
    /// 「**いま**回っている」の判定 ([`ConnTable::spinning`](super::ConnTable::spinning)) はこの差分で行う
    spins_prev: AtomicU64,
    spins_delta: AtomicU64,
}

/// [`ConnSlot::parked_at`] の「預けられていない」印。
const NOT_PARKED: u64 = u64::MAX;

/// [`ConnSlot::half_closed`] の「側 + 1」を入れる位置 (上位 8 bit)。
const HALF_CLOSED_SHIFT: u32 = 56;
/// [`ConnSlot::half_closed`] の ms の部分の覆い (56 bit = 228 万年ぶん)。
const HALF_CLOSED_MS_MASK: u64 = (1u64 << HALF_CLOSED_SHIFT) - 1;

/// 半閉じの (側, 接続を受けてからの ms) を原子 1 本に詰める (T15.0)。
/// **上位 8 bit = 側 + 1** (`0` = まだ半閉じしていない)、**下位 56 bit = ms**。
#[inline]
fn pack_half_closed(side: usize, ms: u64) -> u64 {
    ((side as u64 + 1) << HALF_CLOSED_SHIFT) | (ms & HALF_CLOSED_MS_MASK)
}

/// [`pack_half_closed`] の逆。`0` (まだ半閉じしていない) は `None`。
#[inline]
fn unpack_half_closed(v: u64) -> Option<(usize, u64)> {
    let side = (v >> HALF_CLOSED_SHIFT) as usize;
    (side > 0).then(|| (side - 1, v & HALF_CLOSED_MS_MASK))
}

/// `poll(2)` の旗の名前 (`/connections` の `revents`)。値は `poll(2)` のもので、
/// `crates/sys/src/sys.rs` の `POLLIN` … と同じ (この層は Linux 以外でも組むので、
/// 書き出しに要るぶんだけここに持つ)。
const POLL_FLAGS: [(i16, &str); 5] = [
    (0x001, "IN"),
    (0x004, "OUT"),
    (0x008, "ERR"),
    (0x010, "HUP"),
    (0x020, "NVAL"),
];

/// `poll` が返した 2 つの旗を原子 1 本に詰める (T15.0)。
/// **下位 16 bit がクライアント側、上位 16 bit がオリジン側**。
pub fn pack_revents(client: i16, origin: i16) -> u32 {
    (client as u16 as u32) | ((origin as u16 as u32) << 16)
}

/// 詰めた旗を `IN|HUP` のような名前にする (立っていなければ空文字列)。
fn revents_names(packed: u32, side: usize) -> String {
    let bits = if side == CLIENT_SIDE {
        packed as u16 as i16
    } else {
        (packed >> 16) as u16 as i16
    };
    let mut out = String::new();
    for (bit, name) in POLL_FLAGS {
        if bits & bit != 0 {
            if !out.is_empty() {
                out.push('|');
            }
            out.push_str(name);
        }
    }
    out
}

impl ConnSlot {
    /// 枠を 1 つ作る (呼ぶのは [`ConnTable::register`](super::ConnTable::register) だけ)。
    pub(super) fn new(id: u64, client: &str, started: Instant) -> ConnSlot {
        ConnSlot {
            id,
            client: clip(client, MAX_CLIENT),
            started,
            state: AtomicU8::new(ConnState::Serving as u8),
            target: Mutex::new(String::new()),
            connect: AtomicBool::new(false),
            bytes: AtomicU64::new(0),
            close: AtomicU16::new(0),
            up: AtomicU64::new(0),
            down: AtomicU64::new(0),
            status: AtomicU16::new(0),
            requests: AtomicU32::new(0),
            stage_ms: [const { AtomicU32::new(0) }; STAGES],
            rtt_us: [const { AtomicU32::new(0) }; SIDES],
            retrans: [const { AtomicU32::new(0) }; SIDES],
            parks: AtomicU32::new(0),
            parked_ms: AtomicU64::new(0),
            parked_at: AtomicU64::new(NOT_PARKED),
            traced: AtomicBool::new(false),
            bytes_prev: AtomicU64::new(0),
            rate_bps: AtomicU64::new(0),
            sni: Mutex::new(String::new()),
            syn_retrans: AtomicU8::new(0),
            stall_ms: [const { AtomicU32::new(0) }; SIDES],
            tid: AtomicU32::new(0),
            spins: AtomicU64::new(0),
            revents: AtomicU32::new(0),
            half_closed: AtomicU64::new(0),
            idle_ms: AtomicU64::new(0),
            spins_prev: AtomicU64::new(0),
            spins_delta: AtomicU64::new(0),
        }
    }

    /// この接続を追跡する (`PROXY_TRACE_CLIENT` に一致した。T14.27)。
    ///
    /// **呼ぶのは accept 直後の 1 回だけ** (一致しなかった接続では 1 度も呼ばない)。
    pub fn mark_traced(&self) {
        self.traced.store(true, Ordering::Relaxed);
    }

    /// 追跡中か (要求ごとに読む唯一の値。原子の読み 1 回)。
    pub fn traced(&self) -> bool {
        self.traced.load(Ordering::Relaxed)
    }

    /// 覗いた SNI を書く (`PROXY_PEEK_SNI`。T14.38)。
    ///
    /// **呼ぶのはトンネル 1 本につき多くて 1 回** (`recv(MSG_PEEK)` が ClientHello を
    /// 読めたときだけ)。名前が読めなかったトンネルはこの枠に 1 度も触らない。
    pub fn set_sni(&self, name: &str) {
        *self.sni.locked() = clip(name, MAX_SNI);
    }

    /// 状態を書く (原子 1 回。表の鍵は取らない)。
    pub fn set_state(&self, state: ConnState) {
        self.state.store(state as u8, Ordering::Relaxed);
    }

    pub fn state(&self) -> ConnState {
        ConnState::from_u8(self.state.load(Ordering::Relaxed))
    }

    /// keep-alive の HTTP 接続の**最初の要求の宛先**を書く (T14.2)。
    ///
    /// T13.4 では「要求ごとに宛先が変わるので空のまま」にしていたが、`/connections` で
    /// 占有の内訳を読むとき **`http` の行だけ宛先が空** だと何に使われている接続か分からない。
    /// 要求ごとには書かない方針はそのままで、**接続の最初の 1 回だけ**書く
    /// (表の鍵は取らない。取るのはこの枠の `target` の鍵 1 つで、接続あたり 1 回)。
    /// 既に何か入っていれば触らない (CONNECT の宛先を上書きしない)。
    pub fn set_first_target(&self, target: &str) {
        let mut t = self.target.locked();
        if t.is_empty() {
            *t = clip(target, MAX_TARGET);
        }
    }

    /// CONNECT がつなげなかったときに宛先だけ書く (**エラーの経路だけ**。T14.4)。
    ///
    /// 502 で終わった CONNECT も `/recent` に「誰が・どこへ・何 ms で失敗したか」を
    /// 残すため。成功した経路は [`ConnSlot::begin_tunnel`] が同じ場所に書く。
    pub fn failed_tunnel(&self, target: &str) {
        *self.target.locked() = clip(target, MAX_TARGET);
        self.connect.store(true, Ordering::Relaxed);
    }

    /// CONNECT トンネルになった (宛先が決まった)。**1 本につき 1 回だけ呼ぶ。**
    pub fn begin_tunnel(&self, target: &str) {
        *self.target.locked() = clip(target, MAX_TARGET);
        self.connect.store(true, Ordering::Relaxed);
        self.set_state(ConnState::Relaying);
    }

    /// 運んだ合計バイト数を書く (中継が止まるところで 1 回。バイトごとには書かない)。
    ///
    /// keep-alive の HTTP 接続は、**応答を書き終えて次の要求を待つ直前**に 1 回だけ
    /// 呼ぶ (T24.5)。値は「この接続で終わった要求の上り + 下り」なので、いま処理中の
    /// 1 要求はまだ入らない (`/connections` の `bytes` は要求 1 本ぶん遅れる)。
    pub fn set_bytes(&self, bytes: u64) {
        self.bytes.store(bytes, Ordering::Relaxed);
    }

    /// 中継が**待ちに入る直前**に、その 1 本の証拠をまとめて置く (原子 3 回。T15.0)。
    ///
    /// 呼ぶのは [`ConnSlot::set_bytes`] と同じ「待ちに入る所」1 か所だけで、バイトごとにも
    /// `splice` ごとにも書かない。`spins` は「起こされたのに 1 バイトも進まなかった」回数の
    /// 通算 (数え上げは中継の輪が持つ)、`revents` は**前の周の** `poll` が返した旗
    /// ([`pack_revents`] で 2 つ詰めたもの)。この 3 つが揃うと、`/connections` の 1 行から
    /// 「回っているのか (spins が伸びる) / 待っているのか (伸びない)」が読める。
    pub fn set_relaying(&self, bytes: u64, spins: u64, revents: u32) {
        self.bytes.store(bytes, Ordering::Relaxed);
        self.spins.store(spins, Ordering::Relaxed);
        self.revents.store(revents, Ordering::Relaxed);
    }

    /// いま受け持っているスレッドの番号を書く (原子 1 回。T15.0)。
    ///
    /// 書くのは**ワーカーがこの接続を取ったとき**と、**預けるとき (`0`)** だけ。
    /// 預かり所から戻ると別のスレッドが受け持つので、戻った側が書き直す。
    pub fn set_tid(&self, tid: u32) {
        self.tid.store(tid, Ordering::Relaxed);
    }

    /// 半閉じの向きを書く (**1 本につき多くて 1 回**。原子 1 回と時計 1 回。T15.0)。
    ///
    /// `side` は `0` = クライアント、`1` = オリジンで、中継の `Idle::first_eof` と同じ値。
    /// 時刻は枠が自分の [`ConnSlot::started`] から出す ([`ConnSlot::on_park`] と同じ流儀) —
    /// 中継は「接続を受けた時刻」を持っていないため。既に入っていれば触らない
    /// (先に EOF を出した側だけを残す)。
    pub fn set_half_closed(&self, side: usize) {
        let ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let _ = self.half_closed.compare_exchange(
            0,
            pack_half_closed(side, ms),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// 半閉じの (側, 起きたときの「接続を受けてからの ms」)。まだなら `None`。
    fn half_closed_at(&self) -> Option<(usize, u64)> {
        unpack_half_closed(self.half_closed.load(Ordering::Relaxed))
    }

    /// 閉じた理由を書く (**先に書いた方が勝つ**。T14.4)。
    ///
    /// T13.2 の追い出しや監視スレッドの停止は、接続自身が「なぜ終わったか」を決める前に
    /// 外から閉じる。そこで書いた理由の方が具体的なので、あとから来る既定の理由で
    /// 上書きしない (`set_first_target` と同じ方針)。
    pub fn set_close(&self, reason: CloseReason) {
        let _ = self
            .close
            .compare_exchange(0, reason.code(), Ordering::Relaxed, Ordering::Relaxed);
    }

    /// 接続の終わりに、個票に出す値をまとめて書く (**接続の終了で 1 回だけ**。T14.4)。
    ///
    /// 要求ごとの積み上げは接続を持っているスレッドの箱 ([`ConnTally`]) で行い、
    /// ここで初めて原子に移す。理由は [`ConnSlot::set_close`] と同じく先着優先。
    pub fn finish(&self, reason: CloseReason, tally: ConnTally, requests: u32) {
        self.set_close(reason);
        self.up.store(tally.up, Ordering::Relaxed);
        self.down.store(tally.down, Ordering::Relaxed);
        self.status.store(tally.status, Ordering::Relaxed);
        self.requests.store(requests, Ordering::Relaxed);
        for (cell, ms) in self.stage_ms.iter().zip(tally.stage_ms) {
            cell.store(ms.min(MAX_MS) as u32, Ordering::Relaxed);
        }
        // カーネルの RTT と再送 (T14.5)。読めなかった側は 0 のまま
        for (cell, v) in self.rtt_us.iter().zip(tally.rtt_us) {
            cell.store(v, Ordering::Relaxed);
        }
        for (cell, v) in self.retrans.iter().zip(tally.retrans) {
            cell.store(v, Ordering::Relaxed);
        }
        // 確立までの SYN の再送 (T14.46)
        self.syn_retrans.store(tally.syn_retrans, Ordering::Relaxed);
        // 中継が書けるのを待った ms (T14.42)。中継の中では方向ごとの箱に足すだけで、
        // 原子に移すのはここ 1 回 (`rtt_us` / `retrans` と同じ扱い)
        for (cell, v) in self.stall_ms.iter().zip(tally.stall_ms) {
            cell.store(v, Ordering::Relaxed);
        }
    }

    /// 預かり所に入った (原子 2 回。**預ける瞬間だけ**で、要求ごとには触らない)。
    pub fn on_park(&self) {
        self.parks.fetch_add(1, Ordering::Relaxed);
        self.parked_at.store(
            self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }

    /// 預かり所から出た (起こされた / 期限切れ / 追い出された)。預かっていた時間を足す。
    pub fn on_unpark(&self) {
        let at = self.parked_at.swap(NOT_PARKED, Ordering::Relaxed);
        if at == NOT_PARKED {
            return;
        }
        let now = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.parked_ms
            .fetch_add(now.saturating_sub(at), Ordering::Relaxed);
    }

    /// 預かり所にいた合計 ms (**中継の時間を出すために引く**。T14.25)。
    ///
    /// 読むのはトンネルの終わり (`tunnel::report`) の 1 回だけ。原子の読み 2 回で、
    /// まだ預けられたままなら最後の 1 区間もここで足す ([`ConnSlot::closed_entry`]
    /// と同じ数え方。預かり所は落とす前に必ず `on_unpark` を通るので、普通は通らない)。
    pub fn parked_ms(&self) -> u64 {
        let ms = self.parked_ms.load(Ordering::Relaxed);
        let at = self.parked_at.load(Ordering::Relaxed);
        if at == NOT_PARKED {
            return ms;
        }
        let now = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        ms.saturating_add(now.saturating_sub(at))
    }

    /// 閉じた接続の個票を 1 件作る (`ConnTable` から外すときに 1 回だけ)。
    ///
    /// **自分宛て (`/status` `/dashboard` …) だけで終わった接続は `None`**: 宛先を 1 つも
    /// 選ばず要求も 1 本も代理していない接続で、監視が 5 秒おきに引くとリングが
    /// それだけで埋まってしまう (2,000 件 = 2.8 時間ぶん)。数は `/status` にあるので、
    /// 個票としては残さない。
    pub fn closed_entry(&self, now: Instant) -> Option<RecentEntry> {
        self.closed_entry_at(now, crate::cache::now_epoch())
    }

    /// [`ConnSlot::closed_entry`] の本体 (T34.48)。`epoch` は閉じた時刻 (epoch 秒) で、
    /// 開いた時刻は `epoch − 寿命` で出す (時計を中で読まないので、試験が時刻を縛れる)。
    #[inline]
    pub(crate) fn closed_entry_at(&self, now: Instant, epoch: u64) -> Option<RecentEntry> {
        let target = clip(&self.target.locked(), MAX_RECENT_TARGET);
        let requests = self.requests.load(Ordering::Relaxed);
        if target.is_empty() && requests == 0 {
            return None;
        }
        let age = now.saturating_duration_since(self.started);
        // 預けられたまま閉じた接続は、最後の 1 区間もここで足す (`on_unpark` を通らない)
        let mut parked_ms = self.parked_ms.load(Ordering::Relaxed);
        let at = self.parked_at.load(Ordering::Relaxed);
        if at != NOT_PARKED {
            parked_ms = parked_ms
                .saturating_add((age.as_millis().min(u64::MAX as u128) as u64).saturating_sub(at));
        }
        let mut stage_ms = [0u64; STAGES];
        for (out, cell) in stage_ms.iter_mut().zip(self.stage_ms.iter()) {
            *out = cell.load(Ordering::Relaxed) as u64;
        }
        // 覗いた SNI (T14.38)。空 (覗いていない / 読めなかった) なら個票では `null`
        let sni = {
            let s = self.sni.locked();
            (!s.is_empty()).then(|| s.as_str().into())
        };
        Some(RecentEntry {
            id: self.id,
            // 開いた時刻は「いま − 寿命」で出す (接続を受けるときに時計を読まない)
            at: epoch.saturating_sub(age.as_secs()),
            client: self.client.clone(),
            target,
            connect: self.is_connect(),
            secs: age.as_secs().min(MAX_SECS),
            requests,
            up: self.up.load(Ordering::Relaxed),
            down: self.down.load(Ordering::Relaxed),
            reason: CloseReason::from_code(self.close.load(Ordering::Relaxed)),
            status: self.status.load(Ordering::Relaxed),
            parked_secs: (parked_ms / 1000).min(MAX_SECS),
            parks: self.parks.load(Ordering::Relaxed),
            stage_ms,
            rtt_us: [
                self.rtt_us[CLIENT_SIDE].load(Ordering::Relaxed),
                self.rtt_us[ORIGIN_SIDE].load(Ordering::Relaxed),
            ],
            retrans: [
                self.retrans[CLIENT_SIDE].load(Ordering::Relaxed),
                self.retrans[ORIGIN_SIDE].load(Ordering::Relaxed),
            ],
            sni,
            syn_retrans: self.syn_retrans.load(Ordering::Relaxed),
            // 中継の詰まりの向き (T14.42)。トンネルでなければ両方 0
            stall_ms: [
                self.stall_ms[CLIENT_SIDE].load(Ordering::Relaxed),
                self.stall_ms[ORIGIN_SIDE].load(Ordering::Relaxed),
            ],
            // 空回りの証拠 (T15.0)。トンネルでなければ 0
            spins: self.spins.load(Ordering::Relaxed),
            // 半閉じの向きと、そこから閉じる (= いま) までの ms。半閉じしていなければ `None`
            // (`Idle::first_eof` と同じ値になるので、T14.25 の分布と突き合わせられる)
            half_closed: self.half_closed_at().map(|(side, at)| {
                (
                    side,
                    (age.as_millis().min(u64::MAX as u128) as u64)
                        .saturating_sub(at)
                        .min(MAX_MS),
                )
            }),
        })
    }

    pub fn is_connect(&self) -> bool {
        self.connect.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// 直近の周期の転送速度 (バイト/秒)。**history スレッドが書いた最後の値**を読むだけ。
    pub fn rate_bps(&self) -> u64 {
        self.rate_bps.load(Ordering::Relaxed)
    }

    /// 直近の周期に空回りした回数 (T15.0)。[`ConnSlot::rate_bps`] と同じで、
    /// **history スレッドが書いた最後の値**を読むだけ。
    pub fn spins_delta(&self) -> u64 {
        self.spins_delta.load(Ordering::Relaxed)
    }

    /// 半閉じの向きの名前 (まだなら `None`。T15.0)。
    pub(super) fn half_closed_side(&self) -> Option<&'static str> {
        self.half_closed_at().map(|(side, _)| SIDE_NAMES[side])
    }

    /// いまの `bytes` を控え、前に控えた値との差分 ÷ `ms` を [`ConnSlot::rate_bps`] に書く。
    ///
    /// **呼ぶのは history スレッドの周期だけ** ([`ConnTable::update_rates`](super::ConnTable::update_rates))。1 本あたり
    /// 原子の読み 1 回・入れ替え 1 回・書き 1 回で、接続の経路には 1 命令も増えない
    /// (`bytes` は中継が暇になるたびに置いている累計をそのまま使う)。
    /// `ms == 0` (起動直後の 1 回目) は控えるだけで速さを出さない — 割る幅が無いので、
    /// 「起動より前から居る接続の累計 ÷ 0」という嘘の値を出さないため。
    pub fn sweep_rate(&self, ms: u64) -> u64 {
        // 空回りの**この周期ぶん** (T15.0)。同じ原子 1 対で `bytes` と同じ流儀
        let spins_now = self.spins.load(Ordering::Relaxed);
        let spins_prev = self.spins_prev.swap(spins_now, Ordering::Relaxed);
        self.spins_delta
            .store(spins_now.saturating_sub(spins_prev), Ordering::Relaxed);
        let now = self.bytes.load(Ordering::Relaxed);
        let prev = self.bytes_prev.swap(now, Ordering::Relaxed);
        // 1 バイトも動かなかった周期を足す (T15.0)。動いたら 0 に戻す。
        // **数えるのは history スレッドだけ**で、接続の経路には 1 命令も増えない
        if now == prev {
            self.idle_ms.fetch_add(ms, Ordering::Relaxed);
        } else {
            self.idle_ms.store(0, Ordering::Relaxed);
        }
        // `ms == 0` は `checked_div` が `None` = 速さ 0 (控えるだけ)
        let bps = now
            .saturating_sub(prev)
            .saturating_mul(1000)
            .checked_div(ms)
            .unwrap_or(0);
        self.rate_bps.store(bps, Ordering::Relaxed);
        bps
    }

    /// `/connections` の 1 要素。
    ///
    /// **T15.0 で足した 5 つ (`tid` / `spins` / `revents` / `half_closed` +
    /// `half_closed_secs` / `idle_secs`) は、既定のままの行では 1 バイトも出さない。**
    /// `/connections` は「256 KiB に 1,000 本入る」を守る口で、実測の余りは
    /// **1 行あたり 4.7 B しか無い** (1,000 本で 256,918 B / 予算 261,632 B) —
    /// `,"tid":0` の 8 B すら常には置けない。`/recent` の `ms` が 0 の段を出さないのと
    /// 同じ作法で、**無い = 既定値** と読む (`tid` 0 = 預かり中 / 不明、`spins` 0、
    /// 旗なし、半閉じなし、`idle_secs` 0 または http の行)。
    pub fn to_json(&self, now: Instant) -> String {
        use std::fmt::Write as _;
        let connect = self.is_connect();
        let age = now.saturating_duration_since(self.started);
        let mut out = String::with_capacity(256);
        let _ = write!(
            out,
            "{{\"id\":{},\"client\":\"{}\",\"target\":\"{}\",\"kind\":\"{}\",\"state\":\"{}\",\"age_secs\":{},\"bytes\":{},\"fds\":{},\"rate_bps\":{}",
            self.id,
            crate::json::escape(&self.client),
            crate::json::escape(&self.target.locked()),
            if connect { "connect" } else { "http" },
            self.state().name(),
            age.as_secs(),
            self.bytes(),
            // トンネルはクライアントとオリジンの 2 本、keep-alive はクライアントの 1 本
            if connect { 2 } else { 1 },
            // 直近の周期の転送速度 (T14.39)。`bytes` が累計なのに対しこちらは「いま」
            self.rate_bps(),
        );
        // 受け持っているスレッド (T15.0)。`/profile` の `threads_top` と同じ番号
        let tid = self.tid.load(Ordering::Relaxed);
        if tid != 0 {
            let _ = write!(out, ",\"tid\":{}", tid);
        }
        // 起こされたのに進まなかった回数 (T15.0)
        let spins = self.spins.load(Ordering::Relaxed);
        if spins != 0 {
            let _ = write!(out, ",\"spins\":{}", spins);
        }
        // 前の周の `poll` が返した旗 (T15.0)
        let revents = self.revents.load(Ordering::Relaxed);
        if revents != 0 {
            let _ = write!(
                out,
                ",\"revents\":{{\"client\":\"{}\",\"origin\":\"{}\"}}",
                revents_names(revents, CLIENT_SIDE),
                revents_names(revents, ORIGIN_SIDE),
            );
        }
        // 半閉じの向きと、そこから経った秒 (T15.0)
        if let Some((side, at)) = self.half_closed_at() {
            let _ = write!(
                out,
                ",\"half_closed\":\"{}\",\"half_closed_secs\":{}",
                SIDE_NAMES[side],
                age.as_secs().saturating_sub(at / 1000),
            );
        }
        // 1 バイトも動いていない秒 (T15.0)。keep-alive の HTTP の `bytes` も
        // 要求を書き終えた回にしか動かない (T24.5) が、**この欄は CONNECT の行だけ**に
        // 出す — トンネルの「止まっている」とは意味が違う (要求が来ないだけの
        // keep-alive を「止まっている」と読ませたくない) ので、出さない = `null` と読む
        if connect {
            let idle_ms = self.idle_ms.load(Ordering::Relaxed);
            if idle_ms != 0 {
                let _ = write!(out, ",\"idle_secs\":{}", idle_ms / 1000);
            }
        }
        out.push('}');
        out
    }
}

/// 接続の 1 枠の単体試験 (T34.2 で別のファイルに移した)。
#[cfg(test)]
#[path = "conn_slot_tests.rs"]
mod tests;
