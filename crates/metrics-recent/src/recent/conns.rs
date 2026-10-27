//! 接続の表とリングの 1 枠 (`recent` の一部。T25.2 で `recent.rs` から移した)。
//!
//! [`ConnState`]・[`ConnSlot`]・[`ConnTable`] と、空回りしている接続の [`SpinningConn`]。
//! **`errors` と `recent` (個票・バースト) を呼ぶ側**で、逆向きの依存は無い。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `recent` の
//! `pub use` でそのまま (T25.2)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::errors::{MAX_CLIENT, MAX_MS, MAX_SECS, MAX_TARGET, clip};
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

/// 接続 1 本の今の姿。[`ConnTable`] と接続自身が同じものを [`Arc`] で持つ。
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
    /// ([`ConnSlot::set_first_target`]。T14.2 (5))。要求ごとには触らない
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
    /// いまこの接続を受け持っているスレッドの番号 (`gettid`。T15.0 (4))。
    /// **`0` = 預かり所にいる / 分からない**。書くのは「ワーカーが取ったとき」と
    /// 「預けるとき (0 に戻す)」だけで、要求ごとにも中継のバイトごとにも触らない。
    /// `/profile` の `threads_top` の `tid` と同じ番号なので、**CPU を食っている
    /// スレッドと、そのスレッドが抱えている接続**を突き合わせられる
    tid: AtomicU32,
    /// 中継の輪が「`poll` に起こされたのに 1 バイトも進まなかった」回数の通算
    /// (T15.0 (4))。T15.5 の空回りはこれが秒あたり数万で増え続ける形だった。
    /// **書くのは待ちに入るたびの 1 回** ([`ConnSlot::set_relaying`]) で、数え上げ
    /// そのものは中継の輪が持つ欄 (`Idle::spins`)
    spins: AtomicU64,
    /// **前の周の** `poll` が返した旗 (`i16` 2 つを詰めた。T15.0 (4))。
    /// 下位 16 bit がクライアント側、上位 16 bit がオリジン側。`0` は
    /// 「まだ待ちに入っていない / 何も立っていない」。`POLLHUP` や `POLLERR` が
    /// 立ったまま進まない = 空回りの型。
    /// **預かり所から戻った直後の 1 回は旗を持たない** (`0` を書く): 旗は中継の輪の
    /// 局所変数で、`run_until_idle` に入り直すたびに 0 から始まるため
    revents: AtomicU32,
    /// 半閉じの向きと、それが起きた時刻 (T15.0 (4))。**上位 8 bit = 側 + 1**
    /// (`0` = まだ半閉じしていない)、**下位 56 bit = 接続を受けてからの ms**。
    /// [`ConnSlot::parked_at`] と同じ流儀で **1 本につき 1 回だけ**書き、
    /// `/connections` は `age − それ` で秒を、`/recent` は同じ引き算で ms を出す
    half_closed: AtomicU64,
    /// 同じ `bytes` のまま経った ms (T15.0 (4))。**書くのは history スレッドだけ**
    /// ([`ConnSlot::sweep_rate`]) で、接続の経路は 1 命令も触らない。
    /// `bytes` を置くのはトンネル (待ちに入る回) と keep-alive の HTTP (要求を
    /// 書き終えた回。T24.5) なので、http の行では**要求と要求のあいだ**だけ伸びる。
    /// **`bytes` を書くのは中継が待ちに入る回だけ**なので、`EAGAIN` に落ちずに進み
    /// 続ける飽和したトンネルは、全速で流れていてもここが伸びる (`rate_bps` も同じ
    /// 性質。`spins` と併せて読む)
    idle_ms: AtomicU64,
    /// **前の周期で控えた** [`ConnSlot::spins`] と、その差分 (T15.0 (6))。
    /// [`ConnSlot::bytes_prev`] / [`ConnSlot::rate_bps`] と同じ流儀で、
    /// **書くのは history スレッドだけ** ([`ConnSlot::sweep_rate`])。
    /// 通算の `spins` は「今までに何回空回りしたか」しか言わないので、
    /// 「**いま**回っている」の判定 ([`ConnTable::spinning`]) はこの差分で行う
    spins_prev: AtomicU64,
    spins_delta: AtomicU64,
}

/// [`ConnSlot::parked_at`] の「預けられていない」印。
const NOT_PARKED: u64 = u64::MAX;

/// [`ConnSlot::half_closed`] の「側 + 1」を入れる位置 (上位 8 bit)。
const HALF_CLOSED_SHIFT: u32 = 56;
/// [`ConnSlot::half_closed`] の ms の部分の覆い (56 bit = 228 万年ぶん)。
const HALF_CLOSED_MS_MASK: u64 = (1u64 << HALF_CLOSED_SHIFT) - 1;

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

/// `poll` が返した 2 つの旗を原子 1 本に詰める (T15.0 (4))。
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
    fn new(id: u64, client: &str, started: Instant) -> ConnSlot {
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

    /// keep-alive の HTTP 接続の**最初の要求の宛先**を書く (T14.2 (5))。
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

    /// 中継が**待ちに入る直前**に、その 1 本の証拠をまとめて置く (原子 3 回。T15.0 (4))。
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

    /// いま受け持っているスレッドの番号を書く (原子 1 回。T15.0 (4))。
    ///
    /// 書くのは**ワーカーがこの接続を取ったとき**と、**預けるとき (`0`)** だけ。
    /// 預かり所から戻ると別のスレッドが受け持つので、戻った側が書き直す。
    pub fn set_tid(&self, tid: u32) {
        self.tid.store(tid, Ordering::Relaxed);
    }

    /// 半閉じの向きを書く (**1 本につき多くて 1 回**。原子 1 回と時計 1 回。T15.0 (4))。
    ///
    /// `side` は `0` = クライアント、`1` = オリジンで、中継の `Idle::first_eof` と同じ値。
    /// 時刻は枠が自分の [`ConnSlot::started`] から出す ([`ConnSlot::on_park`] と同じ流儀) —
    /// 中継は「接続を受けた時刻」を持っていないため。既に入っていれば触らない
    /// (先に EOF を出した側だけを残す)。
    pub fn set_half_closed(&self, side: usize) {
        let ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let v = ((side as u64 + 1) << HALF_CLOSED_SHIFT) | (ms & HALF_CLOSED_MS_MASK);
        let _ = self
            .half_closed
            .compare_exchange(0, v, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// 半閉じの (側, 起きたときの「接続を受けてからの ms」)。まだなら `None`。
    fn half_closed_at(&self) -> Option<(usize, u64)> {
        let v = self.half_closed.load(Ordering::Relaxed);
        let side = (v >> HALF_CLOSED_SHIFT) as usize;
        (side > 0).then(|| (side - 1, v & HALF_CLOSED_MS_MASK))
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
            at: crate::cache::now_epoch().saturating_sub(age.as_secs()),
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
            // 空回りの証拠 (T15.0 (4))。トンネルでなければ 0
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

    /// 直近の周期に空回りした回数 (T15.0 (6))。[`ConnSlot::rate_bps`] と同じで、
    /// **history スレッドが書いた最後の値**を読むだけ。
    pub fn spins_delta(&self) -> u64 {
        self.spins_delta.load(Ordering::Relaxed)
    }

    /// 半閉じの向きの名前 (まだなら `None`。T15.0 (6))。
    pub fn half_closed_side(&self) -> Option<&'static str> {
        self.half_closed_at().map(|(side, _)| SIDE_NAMES[side])
    }

    /// いまの `bytes` を控え、前に控えた値との差分 ÷ `ms` を [`ConnSlot::rate_bps`] に書く。
    ///
    /// **呼ぶのは history スレッドの周期だけ** ([`ConnTable::update_rates`])。1 本あたり
    /// 原子の読み 1 回・入れ替え 1 回・書き 1 回で、接続の経路には 1 命令も増えない
    /// (`bytes` は中継が暇になるたびに置いている累計をそのまま使う)。
    /// `ms == 0` (起動直後の 1 回目) は控えるだけで速さを出さない — 割る幅が無いので、
    /// 「起動より前から居る接続の累計 ÷ 0」という嘘の値を出さないため。
    pub fn sweep_rate(&self, ms: u64) -> u64 {
        // 空回りの**この周期ぶん** (T15.0 (6))。同じ原子 1 対で `bytes` と同じ流儀
        let spins_now = self.spins.load(Ordering::Relaxed);
        let spins_prev = self.spins_prev.swap(spins_now, Ordering::Relaxed);
        self.spins_delta
            .store(spins_now.saturating_sub(spins_prev), Ordering::Relaxed);
        let now = self.bytes.load(Ordering::Relaxed);
        let prev = self.bytes_prev.swap(now, Ordering::Relaxed);
        // 1 バイトも動かなかった周期を足す (T15.0 (4))。動いたら 0 に戻す。
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
    /// **T15.0 (4) で足した 5 つ (`tid` / `spins` / `revents` / `half_closed` +
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
        // 受け持っているスレッド (T15.0 (4))。`/profile` の `threads_top` と同じ番号
        let tid = self.tid.load(Ordering::Relaxed);
        if tid != 0 {
            let _ = write!(out, ",\"tid\":{}", tid);
        }
        // 起こされたのに進まなかった回数 (T15.0 (4))
        let spins = self.spins.load(Ordering::Relaxed);
        if spins != 0 {
            let _ = write!(out, ",\"spins\":{}", spins);
        }
        // 前の周の `poll` が返した旗 (T15.0 (4))
        let revents = self.revents.load(Ordering::Relaxed);
        if revents != 0 {
            let _ = write!(
                out,
                ",\"revents\":{{\"client\":\"{}\",\"origin\":\"{}\"}}",
                revents_names(revents, CLIENT_SIDE),
                revents_names(revents, ORIGIN_SIDE),
            );
        }
        // 半閉じの向きと、そこから経った秒 (T15.0 (4))
        if let Some((side, at)) = self.half_closed_at() {
            let _ = write!(
                out,
                ",\"half_closed\":\"{}\",\"half_closed_secs\":{}",
                SIDE_NAMES[side],
                age.as_secs().saturating_sub(at / 1000),
            );
        }
        // 1 バイトも動いていない秒 (T15.0 (4))。keep-alive の HTTP の `bytes` も
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

/// いま開いている接続の表 (`/connections`)。
///
/// **登録と抹消は接続の開始と終了で 1 回ずつだけ** (要求ごとには触らない)。
/// `--lite` では [`ConnTable::set_enabled`] で切り、登録もしない (空の一覧を返す。T1.4 の方針)。
///
/// `PROXY_MAX_CONNS_PER_CLIENT` が設定されている間だけ、**同じ鍵の内側で**接続元ごとの
/// 本数も持つ ([`ConnTable::at_client_limit`]。T14.13)。accept のたびに 240 本を数え直さずに
/// 済ませるためで、上限が `0` (既定) のときはこの表を作らない。
pub struct ConnTable {
    on: AtomicBool,
    /// 全接続の転送速度の和 (`/status` の `rate_bps_total`。T14.39)。
    /// 書くのは history スレッドの周期だけで、`/status` は原子を 1 回読む
    rate_total: AtomicU64,
    /// 接続元ごとの本数を数えているか (`PROXY_MAX_CONNS_PER_CLIENT` が設定されている間だけ)。
    /// **`--lite` でも数える**: 枠 ([`ConnSlot`]) を作らないまま本数だけ ±1 できるので、
    /// 記録を全部止めたプロファイルでも公平さの上限は効く (T14.13)
    counting: AtomicBool,
    inner: Mutex<Conns>,
}

/// [`ConnTable`] が 1 つの鍵で守るもの。
#[derive(Default)]
struct Conns {
    /// 接続 id → 枠 (`/connections`)
    slots: HashMap<u64, Arc<ConnSlot>>,
    /// 接続元 → いま生きている本数 (T14.13。数えているときだけ中身がある)
    per_client: HashMap<Arc<str>, u32>,
    /// 接続 id → 接続元 (抹消のときに鍵を引くため。`--lite` で枠が無くても引ける)
    client_of: HashMap<u64, Arc<str>>,
}

impl Conns {
    /// この接続元の本数を 1 増やす (**呼び出し側が鍵を持っている**)。
    fn add_client(&mut self, id: u64, client: &str) {
        let key: Arc<str> = Arc::from(client);
        *self.per_client.entry(Arc::clone(&key)).or_insert(0) += 1;
        self.client_of.insert(id, key);
    }

    /// この接続の接続元の本数を 1 減らす (数えていない接続なら何もしない)。
    fn drop_client(&mut self, id: u64) {
        let Some(key) = self.client_of.remove(&id) else {
            return;
        };
        if let Some(n) = self.per_client.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                // 0 本になった接続元は落とす (見知らぬ接続元で表が伸び続けないように)
                self.per_client.remove(&key);
            }
        }
    }

    /// いま生きている接続から数え直す (数え始めるとき)。
    ///
    /// `--lite` は枠を持たないので 0 から数え始める (= 切り替えより前から居る接続は
    /// 入らない。閉じるときも引かないので数が狂うことはない)。
    fn recount_clients(&mut self) {
        let live: Vec<(u64, Arc<str>)> = self
            .slots
            .iter()
            .map(|(id, slot)| (*id, Arc::from(slot.client.as_str())))
            .collect();
        self.forget_clients();
        for (id, key) in live {
            *self.per_client.entry(Arc::clone(&key)).or_insert(0) += 1;
            self.client_of.insert(id, key);
        }
    }

    fn forget_clients(&mut self) {
        self.per_client.clear();
        self.client_of.clear();
    }
}

impl Default for ConnTable {
    fn default() -> Self {
        ConnTable::new()
    }
}

impl ConnTable {
    pub fn new() -> ConnTable {
        ConnTable {
            on: AtomicBool::new(true),
            rate_total: AtomicU64::new(0),
            counting: AtomicBool::new(false),
            inner: Mutex::new(Conns::default()),
        }
    }

    /// 記録するかどうか (`--lite` では `false`)。起動時に 1 回だけ呼ぶ。
    pub fn set_enabled(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
        if !on {
            // 接続元ごとの本数は `--lite` でも持てるので、捨てるのは枠だけ
            self.inner.locked().slots.clear();
            self.rate_total.store(0, Ordering::Relaxed);
        }
    }

    pub fn enabled(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// 接続元ごとの本数を数えているか (`PROXY_MAX_CONNS_PER_CLIENT` が設定されている間。T14.13)。
    pub fn counting_clients(&self) -> bool {
        self.counting.load(Ordering::Relaxed)
    }

    /// この接続元の生きている接続が `limit` 本以上か (`PROXY_MAX_CONNS_PER_CLIENT`。T14.13)。
    ///
    /// **accept ごとに 1 回**、上限が設定されているときだけ呼ぶ。取るのは表の鍵 1 回
    /// (登録・抹消と同じ鍵) で、数えるのは `HashMap` を 1 回引くだけ。初めて呼ばれたときに
    /// 「数える」へ切り替え、そのとき生きている接続から数え直す。
    pub fn at_client_limit(&self, client: &str, limit: usize) -> bool {
        // 表の鍵は `register` と同じ**記録の形** (`PROXY_RECORDS=hashed` ならハッシュ)
        // なので、引くときも同じ 1 関数を通す。判定そのものは呼ぶ側が渡した**生の IP**
        // から始まっていて、同じ接続元は必ず同じ鍵になる = 数え方は 3 つの値で同じ (T14.41)
        let key = crate::records::client_key(client);
        let mut g = self.inner.locked();
        if !self.counting.load(Ordering::Relaxed) {
            g.recount_clients();
            self.counting.store(true, Ordering::Relaxed);
        }
        g.per_client
            .get(key.as_ref())
            .is_some_and(|&n| n as usize >= limit)
    }

    /// 数えるのをやめて表を捨てる (上限を `0` に戻したとき。T14.13)。
    pub fn stop_counting_clients(&self) {
        let mut g = self.inner.locked();
        self.counting.store(false, Ordering::Relaxed);
        g.forget_clients();
    }

    /// この接続元のいまの本数 (数えていなければ `0`)。
    pub fn client_conns(&self, client: &str) -> u32 {
        let key = crate::records::client_key(client);
        self.inner
            .locked()
            .per_client
            .get(key.as_ref())
            .copied()
            .unwrap_or(0)
    }

    /// 接続を 1 本登録する (接続の開始で 1 回だけ)。`--lite` なら `None`。
    ///
    /// **`PROXY_RECORDS=off` は `--lite` と同じ扱い** (枠を作らない = `/connections` も
    /// `/recent` も `/bursts` も空)。ただし接続元ごとの本数は `--lite` と同じく数え続ける
    /// ので、`PROXY_MAX_CONNS_PER_CLIENT` の公平さの上限は `off` でも効く (T14.41)。
    /// **旗を見るのはここ 1 回**で、抹消 ([`ConnTable::unregister`]) は見ない —
    /// 途中で `on` → `off` に変えたときに、既に登録してある枠が表に残ってしまうため
    pub fn register(&self, id: u64, client: &str, started: Instant) -> Option<Arc<ConnSlot>> {
        let on = self.enabled() && crate::records::recording();
        // `--lite` で上限も使っていないときは鍵を取らない (今までどおり費用 0)
        if !on && !self.counting_clients() {
            return None;
        }
        // 接続元は**この 1 回**だけ記録の形に直し、枠と本数の表の両方に同じ値を使う
        // (`client` は生のまま残す — 下の自己ベンチの判定が IP そのものを見るため)
        let key = crate::records::client_key(client);
        let mut g = self.inner.locked();
        if self.counting.load(Ordering::Relaxed) {
            g.add_client(id, &key);
        }
        if !on {
            return None;
        }
        // 起動時の自己ベンチ (T14.43) が自分で打った接続は枠を作らない (`--lite` と同じ扱い)。
        // 作ると 2,000 本の CONNECT が `/recent` の 4,096 件を半分埋め、その個票が
        // 状態ファイルに残ってしまう。**接続元ごとの上限 (すぐ上の `add_client`) には
        // ちゃんと数える** (プロキシから見れば本物の負荷なので)。
        // 費用は自己ベンチが回っていないときの原子の読み 1 回
        if crate::selfbench::is_client(client) {
            return None;
        }
        let slot = Arc::new(ConnSlot::new(id, &key, started));
        g.slots.insert(id, Arc::clone(&slot));
        Some(slot)
    }

    /// 接続を 1 本抹消し、その枠を返す (接続の終了で 1 回だけ)。
    ///
    /// 返した枠から閉じた接続の個票を作る (`/recent`。T14.4)。`--lite` と、
    /// 既に抹消済み (2 回目) では `None`。
    pub fn unregister(&self, id: u64) -> Option<Arc<ConnSlot>> {
        let on = self.enabled();
        if !on && !self.counting_clients() {
            return None;
        }
        let mut g = self.inner.locked();
        if self.counting.load(Ordering::Relaxed) {
            g.drop_client(id);
        }
        if !on {
            return None;
        }
        g.slots.remove(&id)
    }

    /// 今の一覧を**古い順** (通し番号の小さい順) で返す。
    pub fn snapshot(&self) -> Vec<Arc<ConnSlot>> {
        let mut v: Vec<Arc<ConnSlot>> =
            self.inner.locked().slots.values().map(Arc::clone).collect();
        v.sort_by_key(|s| s.id);
        v
    }

    /// 全 slot の `bytes` を控え直して、直近 `ms` の転送速度を書く (T14.39)。
    ///
    /// **呼ぶのは history スレッドの周期 (本番は 5 秒) の 1 行だけ**で、接続の経路
    /// (accept / 中継 / close) には 1 命令も増えない。費用は 240 本で原子の読み書き
    /// 240 回ぶんと表の鍵 1 回 (数 us)。`snapshot` ではなくここで鍵を取って回すのは、
    /// 240 本ぶんの [`Arc`] の複製と並べ替えの方が控える仕事より重いため。
    /// 戻り値と `/status` の `rate_bps_total` は**全接続の和**。
    pub fn update_rates(&self, ms: u64) -> u64 {
        let mut total: u64 = 0;
        {
            let g = self.inner.locked();
            for slot in g.slots.values() {
                total = total.saturating_add(slot.sweep_rate(ms));
            }
        }
        self.rate_total.store(total, Ordering::Relaxed);
        total
    }

    /// 全接続の転送速度の和 (`/status` の `rate_bps_total`)。原子の読み 1 回。
    pub fn rate_bps_total(&self) -> u64 {
        self.rate_total.load(Ordering::Relaxed)
    }

    /// **直近の周期に `min_delta` 回以上空回りした接続**を、多い順に `max` 本まで
    /// (と、閾を越えた接続の総数)。T15.0 (6)。
    ///
    /// **呼ぶのは history スレッドの周期だけ** ([`crate::anomaly`] の判定) で、接続の
    /// 経路には 1 命令も増えない。読むのは [`ConnTable::update_rates`] が同じ周期に
    /// 書いた値なので、`update_rates` の**あと**に呼ぶこと。
    /// 平常時は 1 本も越えないので、確保もほとんど起きない。
    ///
    /// **鍵の内側では文字列を作らない**: 表の鍵は accept の経路が取り合うもので、
    /// 事故の最中 (越えた接続が何十本もある) こそ長く握ってはいけない。内側で
    /// 拾うのは枠の参照 (原子の足し算 1 回) だけにして、宛先の複製は上位 `max` 本に
    /// 絞ってから行う (呼ぶ側は `max = 1`)。
    pub fn spinning(&self, min_delta: u64, max: usize) -> (Vec<SpinningConn>, usize) {
        let now = Instant::now();
        let mut over: Vec<(u64, u64, Arc<ConnSlot>)> = Vec::new();
        {
            let g = self.inner.locked();
            for slot in g.slots.values() {
                let spins_delta = slot.spins_delta();
                if spins_delta < min_delta {
                    continue;
                }
                over.push((spins_delta, slot.id, Arc::clone(slot)));
            }
        }
        let total = over.len();
        // 多い順 (同点は通し番号の小さい方 = 古い方が先)
        over.sort_by_key(|&(spins_delta, id, _)| (std::cmp::Reverse(spins_delta), id));
        over.truncate(max);
        let found = over
            .into_iter()
            .map(|(spins_delta, id, slot)| SpinningConn {
                id,
                // 鍵を放してから複製する (取るのは枠ごとの鍵で、表の鍵ではない)
                target: slot.target.locked().clone(),
                age_secs: now.saturating_duration_since(slot.started).as_secs(),
                half_closed: slot.half_closed_side(),
                spins_delta,
            })
            .collect();
        (found, total)
    }

    pub fn len(&self) -> usize {
        self.inner.locked().slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 「いま空回りしている」接続 1 本 ([`ConnTable::spinning`] が返す。T15.0 (6))。
///
/// 異常の説明 1 件は [`crate::events::MAX_TEXT`] (128 B) で切られるので、持つのは
/// **1 行に書けるものだけ** (どの接続か・どこ宛てか・いつからか・半閉じか)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpinningConn {
    /// 接続の通し番号 (ログの `conn#N` と `/connections` の `id`)
    pub id: u64,
    /// 宛先 (`host:port`)
    pub target: String,
    /// 受けてからの秒
    pub age_secs: u64,
    /// 半閉じの向き (`"client"` / `"origin"`。まだなら `None`)
    pub half_closed: Option<&'static str>,
    /// 直近の周期に空回りした回数
    pub spins_delta: u64,
}

#[cfg(test)]
mod conn_tests {
    use super::*;
    use crate::metrics::{DnsCause, ErrCause};
    use crate::recent::{
        CLOSE_REASON_NAMES, CLOSE_REASONS, CloseReason, ConnTally, MAX_RECENT, MAX_RECENT_TARGET,
        RecentRing,
    };

    #[test]
    fn the_table_registers_and_unregisters_once() {
        let t = ConnTable::new();
        assert!(t.is_empty());
        let now = Instant::now();
        let a = t.register(1, "127.0.0.1", now).expect("登録できる");
        let _b = t.register(2, "::1", now).expect("登録できる");
        assert_eq!(t.len(), 2);
        // 古い順 (通し番号の順)
        let snap = t.snapshot();
        assert_eq!(snap[0].id, 1);
        assert_eq!(snap[1].id, 2);
        assert_eq!(snap[0].state(), ConnState::Serving);
        assert!(!snap[0].is_connect());

        // 状態と宛先は表の鍵を取らずに書ける
        a.begin_tunnel("example.com:443");
        a.set_bytes(4096);
        assert_eq!(t.snapshot()[0].state(), ConnState::Relaying);
        assert!(t.snapshot()[0].is_connect());
        let json = t.snapshot()[0].to_json(Instant::now());
        assert!(json.contains("\"kind\":\"connect\""), "{}", json);
        assert!(json.contains("\"state\":\"relaying\""), "{}", json);
        assert!(json.contains("\"target\":\"example.com:443\""), "{}", json);
        assert!(json.contains("\"bytes\":4096"), "{}", json);
        assert!(json.contains("\"fds\":2"), "{}", json);

        a.set_state(ConnState::Parked);
        assert_eq!(t.snapshot()[0].state(), ConnState::Parked);
        t.unregister(1);
        assert_eq!(t.len(), 1);
        t.unregister(1);
        assert_eq!(t.len(), 1, "2 回抹消しても増減しない");
    }

    /// いまの転送速度 (T14.39)。history スレッドの周期が `bytes` の差分を速さにする。
    #[test]
    fn sweeping_turns_the_byte_counter_into_a_rate() {
        let t = ConnTable::new();
        let now = Instant::now();
        let a = t.register(1, "198.51.100.7", now).expect("登録できる");
        let b = t.register(2, "198.51.100.8", now).expect("登録できる");
        a.begin_tunnel("a.example.net:443");
        b.begin_tunnel("b.example.net:443");

        // 1 回目は控えるだけ (割る幅が無い)。5 MiB 運んでいても速さは 0
        a.set_bytes(5 << 20);
        assert_eq!(t.update_rates(0), 0);
        assert_eq!(a.rate_bps(), 0);

        // 5 秒で +5 MiB → 1 MiB/s、動かなかった方は 0
        a.set_bytes(10 << 20);
        assert_eq!(t.update_rates(5000), 1 << 20);
        assert_eq!(a.rate_bps(), 1 << 20);
        assert_eq!(b.rate_bps(), 0);
        assert_eq!(t.rate_bps_total(), 1 << 20);

        // 和は全接続ぶん
        a.set_bytes((10 << 20) + (1 << 20));
        b.set_bytes(2 << 20);
        assert_eq!(t.update_rates(1000), (1 << 20) + (2 << 20));
        assert_eq!(t.rate_bps_total(), 3 << 20);

        // `/connections` の行の末尾に出る
        let json = t.snapshot()[0].to_json(Instant::now());
        assert!(
            json.ends_with(",\"fds\":2,\"rate_bps\":1048576}"),
            "{}",
            json
        );

        // 止まれば次の周期で 0 に戻る
        assert_eq!(t.update_rates(5000), 0);
        assert_eq!(a.rate_bps(), 0);
        assert_eq!(t.rate_bps_total(), 0);
    }

    /// `--lite` (記録しない) では枠ごと捨てるので和も 0 (T14.39)。
    #[test]
    fn the_rate_total_is_zero_without_the_table() {
        let t = ConnTable::new();
        let slot = t.register(1, "198.51.100.7", Instant::now()).expect("枠");
        slot.set_bytes(1 << 20);
        t.update_rates(1000);
        assert_eq!(t.rate_bps_total(), 1 << 20);
        t.set_enabled(false);
        assert_eq!(t.rate_bps_total(), 0);
        assert!(t.register(2, "198.51.100.8", Instant::now()).is_none());
        assert_eq!(t.update_rates(1000), 0);
    }

    /// 閉じた接続の個票が 1 件ずつ残ること (`/recent`。T14.4)。
    #[test]
    fn closing_a_connection_leaves_one_entry() {
        let t = ConnTable::new();
        let ring = RecentRing::new();
        let slot = t.register(7, "198.51.100.7", Instant::now()).unwrap();
        slot.begin_tunnel("mtalk.google.com:5228");
        slot.on_park();
        slot.on_unpark();
        slot.finish(
            CloseReason::ClientEof,
            ConnTally {
                up: 4096,
                down: 65536,
                status: 0,
                stage_ms: [3, 9, 0, 0, 0, 0],
                // カーネルの RTT: 利用者は 48 ms、宛先は 30 ms (T14.5)
                rtt_us: [48_300, 30_100],
                retrans: [0, 0],
                // 確立まで SYN を 1 回送り直した (= 1 秒待たされた接続。T14.46)
                syn_retrans: 1,
                // 中継の詰まり: クライアントへ書けずに 1,800 ms 待った (T14.42)
                stall_ms: [1_800, 0],
            },
            0,
        );
        let got = t.unregister(7).expect("枠が返る");
        ring.push(
            got.closed_entry(Instant::now())
                .expect("宛先のある接続は残る"),
        );
        assert!(t.is_empty());
        assert!(t.unregister(7).is_none(), "2 回目は何も返らない");

        let (rows, total) = ring.select(0, "");
        assert_eq!(total, 1);
        assert_eq!(rows.len(), 1);
        let e = &rows[0];
        assert_eq!(e.id, 7);
        assert_eq!(e.client, "198.51.100.7");
        assert_eq!(e.target, "mtalk.google.com:5228");
        assert!(e.connect);
        assert_eq!(e.up, 4096);
        assert_eq!(e.down, 65536);
        assert_eq!(e.bytes(), 4096 + 65536);
        assert_eq!(e.reason, CloseReason::ClientEof);
        assert_eq!(e.connect_ms(), 9);
        assert_eq!(e.parks, 1);
        let json = e.to_json();
        assert!(json.contains("\"reason\":\"client_eof\""), "{}", json);
        assert!(json.contains("\"kind\":\"connect\""), "{}", json);
        assert!(
            json.contains("\"ms\":{\"dns\":3,\"connect\":9}"),
            "{}",
            json
        );
        // カーネルの RTT と再送は両側とも必ず出る (T14.5)
        assert_eq!(e.rtt_us, [48_300, 30_100]);
        assert!(
            json.contains("\"rtt_ms\":{\"client\":48.3,\"origin\":30.1}"),
            "{}",
            json
        );
        assert!(
            json.contains("\"retrans\":{\"client\":0,\"origin\":0}"),
            "{}",
            json
        );
        // T14.3 が埋める段階はまだ 0 なので 1 バイトも出さない
        assert!(!json.contains("queue"), "{}", json);
        // 空回りと半閉じ (T15.0 (4)) は、`/connections` と違ってここでは 0 でも `null` でも
        // 必ず出す (件数で切る口なので「欄が無い」= 古い版と区別させるため)
        assert!(json.contains("\"spins\":0"), "{}", json);
        assert!(
            json.contains("\"half_closed\":null,\"half_closed_ms\":null"),
            "{}",
            json
        );
        // 上限は欄が増えたぶん引き上げてある (T14.5 の `rtt_ms` / `retrans`、T14.26 の
        // 向き別、T14.46 の `syn_retrans` で 363 B、T15.0 (4) の 3 欄で +51 B = 実測 414 B)
        assert!(json.len() <= 450, "ありふれた 1 件が {} B", json.len());
        println!("closed entry: typical {} B\n  {}", json.len(), json);
    }

    /// 閉じた理由は**先に書いた方が勝つ** (T13.2 の追い出しが中継の終わり方に負けない)。
    #[test]
    fn the_first_close_reason_wins() {
        let t = ConnTable::new();
        let slot = t.register(1, "127.0.0.1", Instant::now()).unwrap();
        slot.begin_tunnel("example.net:443");
        slot.set_close(CloseReason::Evicted);
        slot.finish(CloseReason::ClientEof, ConnTally::default(), 0);
        let e = slot
            .closed_entry(Instant::now())
            .expect("宛先のある接続は残る");
        assert_eq!(e.reason, CloseReason::Evicted);
        assert_eq!(e.reason.text(), "evicted");
        // 何も書かなければ「プロキシ側の都合」
        let other = t.register(2, "127.0.0.1", Instant::now()).unwrap();
        other.begin_tunnel("example.net:443");
        assert_eq!(
            other.closed_entry(Instant::now()).unwrap().reason,
            CloseReason::Shutdown
        );
        // 宛先を 1 つも選ばなかった接続 (自分宛ての `/status` など) は残さない
        let local = t.register(3, "127.0.0.1", Instant::now()).unwrap();
        assert!(local.closed_entry(Instant::now()).is_none());
    }

    /// `error:<原因>` の綴りと、9 つの理由が往復できること。
    #[test]
    fn every_close_reason_survives_the_round_trip() {
        let all = [
            (CloseReason::ClientEof, "client_eof"),
            (CloseReason::ServerEof, "server_eof"),
            (CloseReason::IdleTimeout, "idle_timeout"),
            (CloseReason::KeepaliveTimeout, "keepalive_timeout"),
            (CloseReason::Evicted, "evicted"),
            (CloseReason::Limit, "limit"),
            (CloseReason::Shutdown, "shutdown"),
            (CloseReason::ClientDead, "client_dead"),
            (CloseReason::Error(ErrCause::Refused), "error:refused"),
            (CloseReason::Error(ErrCause::Dns), "error:dns"),
            (CloseReason::Error(ErrCause::Other), "error:other"),
            // T25.0。DNS の失敗は内訳の綴りで出る
            (CloseReason::ErrorDns(DnsCause::Nxdomain), "error:nxdomain"),
            (CloseReason::ErrorDns(DnsCause::DnsTemp), "error:dns_temp"),
            // T.29.10。408 は確立の締め切り (`error:timeout`) と別の綴り
            (CloseReason::HeaderTimeout, "error:header_timeout"),
        ];
        for (reason, name) in all {
            assert_eq!(reason.text(), name);
            assert_eq!(CloseReason::from_code(reason.code()), reason, "{}", name);
        }
        // **既存の符号は動かさない** (T25.0 の 2 つは `ClientDead` の後ろ)
        assert_eq!(CloseReason::ClientDead.code(), 16);
        assert_eq!(CloseReason::ErrorDns(DnsCause::Nxdomain).code(), 17);
        assert_eq!(CloseReason::ErrorDns(DnsCause::DnsTemp).code(), 18);
        assert_eq!(CloseReason::HeaderTimeout.code(), 19);
        assert_eq!(
            CloseReason::HeaderTimeout.index(),
            CloseReason::Error(ErrCause::Timeout).index()
        );
        // 分布は `error:*` の 1 列に畳む (列は 8 つのまま)
        assert_eq!(
            CloseReason::ErrorDns(DnsCause::Nxdomain).index(),
            CloseReason::Error(ErrCause::Dns).index()
        );
        // `/history` の分布は 8 列のままで、`client_dead` は `shutdown` に畳む (T14.52)
        assert_eq!(
            CloseReason::ClientDead.index(),
            CloseReason::Shutdown.index()
        );
        assert_eq!(CLOSE_REASON_NAMES.len(), CLOSE_REASONS);
        assert!(!CLOSE_REASON_NAMES.contains(&"client_dead"));
    }

    /// `since` と `client` で絞れ、新しい順に返ること。
    #[test]
    fn the_closed_ring_filters_and_overwrites_the_oldest() {
        let ring = RecentRing::new();
        for i in 0..(MAX_RECENT as u64 + 5) {
            ring.push(RecentEntry {
                id: i,
                at: 1_000_000 + i,
                client: if i % 2 == 0 { "10.0.0.1" } else { "10.0.0.2" }.to_string(),
                target: "example.net:443".to_string(),
                connect: true,
                secs: i,
                requests: 0,
                up: i,
                down: i * 2,
                reason: CloseReason::ClientEof,
                status: 0,
                parked_secs: 0,
                parks: 0,
                stage_ms: [0, i, 0, 0, 0, 0],
                rtt_us: [0; SIDES],
                retrans: [0; SIDES],
                stall_ms: [0; SIDES],
                sni: None,
                syn_retrans: 0,
                spins: 0,
                half_closed: None,
            });
        }
        let (all, total) = ring.select(0, "");
        assert_eq!(total, MAX_RECENT as u64 + 5);
        assert_eq!(all.len(), MAX_RECENT, "リングは 2,000 件で頭打ち");
        assert_eq!(all[0].id, MAX_RECENT as u64 + 4, "新しい順");
        assert_eq!(all[MAX_RECENT - 1].id, 5, "最古は 2,000 件前");
        // 接続元で絞る
        let (mine, _) = ring.select(0, "10.0.0.2");
        assert_eq!(mine.len(), MAX_RECENT / 2);
        assert!(mine.iter().all(|e| e.client == "10.0.0.2"));
        // 時刻で絞る (それ以降に開いたものだけ)
        let (fresh, _) = ring.select(1_000_000 + MAX_RECENT as u64, "");
        assert_eq!(fresh.len(), 5);
        assert!(ring.select(9_999_999, "").0.is_empty());
    }

    /// 桁を振り切った 1 件でも 760 B に収まること (**リングの大きさの上限**)。
    ///
    /// 1 件の目安は 300 B (T14.5 のカーネルの RTT 2 側ぶんで 256 B から上がった)。
    /// ここで見るのは「起こりえない桁 (転送 20 桁、段階の ms が 6 つとも 7 桁、
    /// RTT と再送が 4,294,967,295、T15.0 (4) の `spins` が 20 桁) を並べても
    /// リングが 2,000 × 760 B = 1,484 KiB を越えない」ことだけで、
    /// `/recent?n=2000` の応答は 256 KiB のバイト数打ち切りに当たるのが設計どおり
    /// (`crates/endpoints` のテストで見る)。
    #[test]
    fn the_worst_closed_entry_stays_small() {
        let slot = ConnSlot::new(
            u64::MAX,
            "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6xx",
            Instant::now(),
        );
        slot.begin_tunnel(&"sub.".repeat(40));
        slot.on_park();
        // トンネル 1 本の証拠も最悪の値で (T15.0 (4))
        slot.set_relaying(u64::MAX, u64::MAX, pack_revents(0x03d, 0x03d));
        slot.set_half_closed(ORIGIN_SIDE);
        slot.finish(
            CloseReason::KeepaliveTimeout,
            ConnTally {
                up: u64::MAX,
                down: u64::MAX,
                status: 599,
                stage_ms: [u64::MAX; STAGES],
                rtt_us: [u32::MAX; SIDES],
                retrans: [u32::MAX; SIDES],
                syn_retrans: u8::MAX,
                stall_ms: [u32::MAX; SIDES],
            },
            u32::MAX,
        );
        let e = slot
            .closed_entry(Instant::now())
            .expect("宛先のある接続は残る");
        assert!(e.target.len() <= MAX_RECENT_TARGET, "{}", e.target.len());
        assert!(e.client.len() <= MAX_CLIENT, "{}", e.client.len());
        let json = e.to_json();
        // 上限は欄が増えたぶん引き上げてある (T14.55 で 633 B、T15.0 (4) の
        // `spins` / `half_closed` / `half_closed_ms` で +71 B = 実測 704 B)
        assert!(json.len() <= 760, "最悪の 1 件が {} B", json.len());
        println!("closed entry: worst {} B", json.len());
    }

    /// トンネル 1 本の証拠 (`tid` / `spins` / `revents` / 半閉じ / 暇な秒) が枠に入り、
    /// `/connections` の 1 行と個票の両方に出ること (T15.0 (4))。
    ///
    /// **既定のままの行には 1 バイトも足さない**ことも一緒に縛る: `/connections` は
    /// 「256 KiB に 1,000 本」の口で、実測の余りは 1 行あたり 4.7 B しか無い。
    #[test]
    fn a_tunnel_slot_carries_the_evidence_of_a_spin() {
        let slot = ConnSlot::new(7, "198.51.100.9", Instant::now());
        slot.begin_tunnel("mtalk.google.com:5228");
        let now = Instant::now();

        // まだ何も起きていない行は、欄を足す前と 1 バイトも変わらない
        let plain = slot.to_json(now);
        for key in ["tid", "spins", "revents", "half_closed", "idle_secs"] {
            assert!(
                !plain.contains(&format!("\"{}\"", key)),
                "既定の行に {} が出ている: {}",
                key,
                plain
            );
        }

        // 中継が待ちに入るたびに置く 3 つ。旗はクライアント側に `POLLERR | POLLHUP` が
        // 立ったまま = 空回りの型 (T15.5 が消した形)
        slot.set_tid(4242);
        slot.set_relaying(9_000, 12_345, pack_revents(0x008 | 0x010, 0));
        slot.set_half_closed(CLIENT_SIDE);
        let json = slot.to_json(now);
        assert!(json.contains("\"tid\":4242"), "{}", json);
        assert!(json.contains("\"spins\":12345"), "{}", json);
        assert!(
            json.contains("\"revents\":{\"client\":\"ERR|HUP\",\"origin\":\"\"}"),
            "{}",
            json
        );
        assert!(json.contains("\"half_closed\":\"client\""), "{}", json);
        assert!(json.contains("\"half_closed_secs\":0"), "{}", json);
        assert!(!json.contains("\"idle_secs\""), "まだ動いている: {}", json);

        // 暇な秒を数えるのは history スレッドだけ (同じ `bytes` の周期を足す)
        assert_eq!(slot.sweep_rate(1000), 9_000, "1 周期で 9,000 B 動いた");
        slot.sweep_rate(1000);
        slot.sweep_rate(1000);
        let json = slot.to_json(now);
        assert!(json.contains("\"idle_secs\":2"), "{}", json);
        assert!(json.contains("\"rate_bps\":0"), "動いていない: {}", json);

        // 個票にも同じ 2 つが移る (`half_closed_ms` は半閉じから閉じるまで)
        let e = slot
            .closed_entry(Instant::now())
            .expect("宛先のある接続は残る");
        assert_eq!(e.spins, 12_345);
        let (side, ms) = e.half_closed.expect("半閉じしている");
        assert_eq!(side, CLIENT_SIDE);
        assert!(ms < 60_000, "半閉じからの ms が大きすぎる: {}", ms);
        let j = e.to_json();
        assert!(j.contains("\"spins\":12345"), "{}", j);
        assert!(j.contains("\"half_closed\":\"client\""), "{}", j);
        assert!(j.contains(&format!("\"half_closed_ms\":{}", ms)), "{}", j);
    }

    /// 半閉じは**先に EOF を出した側**だけを残す (1 本につき 1 回。T15.0 (4))。
    /// http の行には `idle_secs` を出さない (`bytes` を置くのはトンネルだけ)。
    #[test]
    fn the_first_half_close_wins_and_http_rows_have_no_idle_secs() {
        let slot = ConnSlot::new(1, "10.0.0.1", Instant::now());
        slot.begin_tunnel("example.net:443");
        slot.set_half_closed(ORIGIN_SIDE);
        slot.set_half_closed(CLIENT_SIDE);
        assert!(
            slot.to_json(Instant::now())
                .contains("\"half_closed\":\"origin\""),
            "先に書いた方が勝つ"
        );

        let http = ConnSlot::new(2, "10.0.0.2", Instant::now());
        http.set_first_target("example.net:80");
        http.sweep_rate(1000);
        http.sweep_rate(1000);
        let json = http.to_json(Instant::now());
        assert!(json.contains("\"kind\":\"http\""), "{}", json);
        assert!(
            !json.contains("idle_secs"),
            "http の行には出さない: {}",
            json
        );
        // 半閉じしていない接続の個票は `null` (「欄が無い」と区別させる)
        let e = http
            .closed_entry(Instant::now())
            .expect("宛先のある接続は残る");
        assert_eq!(e.half_closed, None);
        let j = e.to_json();
        assert!(j.contains("\"spins\":0"), "{}", j);
        assert!(
            j.contains("\"half_closed\":null,\"half_closed_ms\":null"),
            "{}",
            j
        );
    }

    /// 接続元ごとの本数は、上限が設定されたときだけ数える (T14.13)。
    #[test]
    fn counting_per_client_starts_when_a_limit_is_set() {
        let t = ConnTable::new();
        let now = Instant::now();
        // 上限を見るまでは数えない (既定の費用 0)
        let _a = t.register(1, "10.0.0.1", now);
        assert!(!t.counting_clients());
        assert_eq!(t.client_conns("10.0.0.1"), 0);

        // 初めて判定したとき、生きている接続から数え直す
        assert!(!t.at_client_limit("10.0.0.1", 2));
        assert!(t.counting_clients());
        assert_eq!(t.client_conns("10.0.0.1"), 1);

        let _b = t.register(2, "10.0.0.1", now);
        assert_eq!(t.client_conns("10.0.0.1"), 2);
        assert!(t.at_client_limit("10.0.0.1", 2), "2 本目で上限");
        assert!(!t.at_client_limit("10.0.0.2", 2), "別の接続元は 0 本から");

        // 抹消で 1 本減り、0 本になった接続元は表から落ちる
        t.unregister(2);
        assert_eq!(t.client_conns("10.0.0.1"), 1);
        assert!(!t.at_client_limit("10.0.0.1", 2));
        t.unregister(1);
        assert_eq!(t.client_conns("10.0.0.1"), 0);

        // 上限を 0 に戻したら表も捨てる
        let _c = t.register(3, "10.0.0.1", now);
        assert_eq!(t.client_conns("10.0.0.1"), 1);
        t.stop_counting_clients();
        assert!(!t.counting_clients());
        assert_eq!(t.client_conns("10.0.0.1"), 0);
        // 数えていない間の抹消は空振り (負にならない)
        t.unregister(3);
        assert!(!t.at_client_limit("10.0.0.1", 1), "数え直しても 0 本");
    }

    /// `--lite` は枠を持たないが、接続元ごとの本数は数えられる (T14.13)。
    #[test]
    fn lite_still_counts_connections_per_client() {
        let t = ConnTable::new();
        t.set_enabled(false);
        assert!(!t.at_client_limit("10.0.0.1", 1));
        assert!(
            t.register(1, "10.0.0.1", Instant::now()).is_none(),
            "枠は作らない"
        );
        assert!(t.is_empty());
        assert_eq!(t.client_conns("10.0.0.1"), 1);
        assert!(t.at_client_limit("10.0.0.1", 1));
        t.unregister(1);
        assert_eq!(t.client_conns("10.0.0.1"), 0);
    }

    /// `--lite` では登録しない (空の一覧)。
    #[test]
    fn lite_registers_nothing() {
        let t = ConnTable::new();
        t.set_enabled(false);
        assert!(t.register(1, "127.0.0.1", Instant::now()).is_none());
        assert!(t.is_empty());
        assert!(t.snapshot().is_empty());
        assert!(!t.enabled());
    }

    /// keep-alive の HTTP 接続は記述子 1 本・宛先なし。
    #[test]
    fn a_keepalive_connection_shows_one_descriptor() {
        let t = ConnTable::new();
        let slot = t.register(7, "10.0.0.1", Instant::now()).unwrap();
        slot.set_state(ConnState::Reading);
        let json = slot.to_json(Instant::now());
        assert!(json.contains("\"kind\":\"http\""), "{}", json);
        assert!(json.contains("\"state\":\"reading\""), "{}", json);
        assert!(json.contains("\"target\":\"\""), "{}", json);
        assert!(json.contains("\"fds\":1"), "{}", json);
        assert!(json.contains("\"id\":7"), "{}", json);
    }
}
