//! Linux の 1 スレッド中継 (`poll` + `splice`) と、暇なときの預け入れ。
//!
//! 片方向の中継 (`Dir`) と中継パイプの置き場は `dir` にある。
use std::io;
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::open::{Info, Opened};
use super::report::report;
use crate::log_trace;
use crate::metrics::ErrCause;
use crate::recent::{CLIENT_SIDE, CloseReason, ConnSlot, ConnState, ORIGIN_SIDE, SIDES};
use crate::sys::{self, POLLERR, POLLHUP, POLLIN, POLLOUT, PollFd};
use proxy_base::minrate::{MinRate, RateGuard};

mod dir;
#[cfg(test)]
mod tests;

use dir::{Dir, Relay, went_away};

/// `poll` に渡す記述子。**関心 (`events`) が無いなら `-1`** (T15.5)。
///
/// `poll(2)` は負の記述子を無視して `revents` を 0 にする。関心が 0 の記述子を
/// そのまま渡すと、`events` に関係なく返る `POLLERR` / `POLLHUP` で
/// 「誰も面倒を見ない起床」が起きて空回りになる (呼ぶ側の注記を見よ)。
#[inline]
fn poll_fd(sock: &TcpStream, events: i16) -> RawFd {
    if events == 0 { -1 } else { sock.as_raw_fd() }
}

/// 無期限 (`PROXY_TUNNEL_IDLE_SECS=0`) のトンネルを預けるときの「遠い期限」。
/// 預かり所は期限の早い順に並べた集合で待つので、期限そのものは必ず要る。
const FOREVER: Duration = Duration::from_secs(365 * 86400);
/// 預ける前に同じスレッドで待ってみる猶予の下限 (`PROXY_PARK_GRACE_MS` が短くてもこれ以上)。
///
/// トンネルの預け入れは HTTP の keep-alive より往復が高い (記述子 2 本の epoll 出し入れ、
/// 中継パイプの作り直し、ワーカーの受け渡し) 一方、預けたいのは「秒〜分の単位で暇な
/// トンネル」なので、100 ms 待って損はない。実測 (`--only connect`、短命トンネル):
/// 猶予 3 ms だと CPU/本 175.2 → 190.2 us (+8.6%)、p99 1.709 → 2.263 ms (+32%) で、
/// 「閉じられる直前に預けて、すぐ起こされる」ぶんを丸ごと払っていた。
pub(super) const MIN_PARK_GRACE: Duration = Duration::from_millis(100);
/// 猶予を伸ばす上限 (T.32.19)。
///
/// 預けて **[`SHORT_PARK`] 未満で起こされた**トンネルは、次の猶予を倍にしていく
/// (100 → 200 → 400 → 800 → 1,000 ms)。0.5 秒ごとに小さく往復するトンネル
/// (tokyo の login.live.com は 220 秒に 407 回預けていた) は数回で預けなくなり、
/// 預けて引き上げる 1 往復 (epoll_ctl 4・futex 約 4・スレッドの起床 3、実測 約 115 us)
/// を払わなくなる。秒〜分の単位で暇なトンネルは預けたあと長く眠るので、
/// 起こされた時点で猶予は下限に戻る (預かりの数とスレッドの数は今のまま)
const MAX_PARK_GRACE: Duration = Duration::from_secs(1);
/// これより短く預けられていたら「預けたのは損だった」とみなす (T.32.19)。
const SHORT_PARK: Duration = Duration::from_secs(1);

/// 起こされたあとの次の猶予 (T.32.19)。`parked` は預けられていた時間、`base` は下限。
fn next_grace(grace: Duration, base: Duration, parked: Duration) -> Duration {
    if parked < SHORT_PARK {
        grace.saturating_mul(2).min(MAX_PARK_GRACE.max(base))
    } else {
        base
    }
}

/// [`Idle::run_until_idle`] の結果。
enum Outcome {
    /// トンネルは終わった (運んだ合計は `Idle::transferred` にある)
    Done,
    /// 両方向とも暇になった (預けてよい)
    Idle,
}

/// 暇になったトンネルの預け先 (実体は本体クレートの `IdleWatch`)。
///
/// 依存の向きが 本体 → `proxy-tunnel` なので、預け先そのものは型として受け取れない。
/// 預かれたら `Ok(())`、断られたら `Err(idle)` で戻ってきて、呼び出し側が続ける。
pub trait Park: Send + Sync {
    fn park(&self, idle: Box<Idle>) -> Result<(), Box<Idle>>;
}

/// トンネル 1 本ぶんの、持ち運べる状態。
///
/// 落ちるとソケットが閉じ、アクセスログと統計が 1 回だけ出る ([`Drop`])。
/// どのワーカーで終わっても、預かり所が期限切れで落としても同じ場所を通る。
pub struct Idle {
    socks: [TcpStream; 2],
    dirs: [Dir; 2],
    /// 双方向に運んだ合計バイト数 (預けても引き継ぐ)
    transferred: u64,
    /// 無通信で打ち切るまでの時間 (`None` で無期限)
    idle: Option<Duration>,
    /// 預け先と、預ける前に同じスレッドで待ってみる猶予の下限 (`None` なら預けない)
    park: Option<(Arc<dyn Park>, Duration)>,
    /// いまの猶予 (下限から始め、すぐ起こされるたびに倍にする。[`next_grace`]。T.32.19)
    grace: Duration,
    /// アクセスログと統計に要る素性
    info: Info,
    /// `200` を書いてから最初の中継バイトまで (ms。T14.3 (1))
    first_relay_ms: u32,
    /// いま預けられているなら、預けた時刻 (T14.3 (1))
    parked_at: Option<Instant>,
    /// 預けられていた合計 (ms)
    parked_ms: u32,
    /// 中継の中で決まった閉じた理由 (アイドル打ち切り / `poll` の失敗。T14.4)。
    /// 外から閉じられたとき (追い出し・監視の停止) は枠に直接書いてあるので
    /// ここは `None` のままで、`ConnSlot::finish` の先着優先でそちらが勝つ
    close: Option<CloseReason>,
    /// 先に EOF を出したのはどちらと、それがいつか (`0` = クライアント、`1` = 宛先)。
    ///
    /// 側は閉じた理由 (T14.4)、時刻は**半閉じから反対側が閉じるまで**の分布 (T14.25)。
    /// 書くのは `get_or_insert_with` の中 = **トンネル 1 本につき多くて 1 回**で、
    /// バイトを動かす道 (splice の往復) には 1 命令も足していない
    first_eof: Option<(usize, Instant)>,
    /// まだ SNI を覗いていないか (`PROXY_PEEK_SNI`。T14.38)。
    ///
    /// 立っているのは「個票の枠があり (= `--lite` ではない)、宛先が 443
    /// (または試験用の口) の CONNECT」だけで、**最初にクライアント側が読めた
    /// ときに 1 回覗いて倒す** (トンネル 1 本に `recv(MSG_PEEK)` は多くて 1 回)
    peek_sni: bool,
    /// 書けるのを待った合計 (us。`[クライアント側, オリジン側]`。T14.42)。
    ///
    /// 添字は `socks` の添字と同じで、`0` は「**クライアントへ**書けなくて待った」
    /// (= 利用者の下り回線か端末が読んでいない)、`1` は「**オリジンへ**書けなくて
    /// 待った」(= オリジンか利用者の上りが詰まっている)。**時計を読むのは
    /// `poll` で書けるのを待ちに入る回だけ**で、64 KiB ごとにも splice ごとにも
    /// 読まない (詰まらない中継は 1 回も読まない)。ms ではなく us で積むのは、
    /// 1 ms に満たない待ちを何度も繰り返すトンネルで切り捨てが積み上がらないように
    stall_us: [u64; SIDES],
    /// **起こされたのに 1 バイトも進まなかった**周の数 (T15.0 (4))。
    ///
    /// T15.5 で消した空回りの型 (`poll` が `POLLHUP` で返るのに誰も読まない) は、
    /// これが秒あたり数万で増え続ける形だった。預けても引き継ぐので `Idle` の欄
    /// (`stall_us` と同じ扱い) で、`/connections` と個票へは
    /// [`ConnSlot::set_relaying`] が待ちに入るたびに 1 回だけ写す
    spins: u64,
    /// 書き側の最小の速さの見張り (`[クライアントへ, オリジンへ]`。添字は `socks` と同じ。T.29.34)。
    ///
    /// 積むのは `stall_us` と同じ「書けなくて `poll` で待った時間」と、その向きへ渡したバイト。
    /// **送るものが無くて待った時間 (片側が黙っているだけの half-idle) は入らない**ので、
    /// 暇なトンネルは今までどおり `PROXY_TUNNEL_IDLE_SECS` で閉じる。預けても引き継ぐ
    rate: [RateGuard; SIDES],
    /// 本体クレートの持ち分 (同時接続数と `active_connections`)。中身は見ない
    _hold: Box<dyn Send>,
}

/// 積んだ us を個票の ms にする (**時計は読まない**。四捨五入は T14.25 と同じ)。
#[inline]
fn stall_ms(us: [u64; SIDES]) -> [u32; SIDES] {
    let mut out = [0u32; SIDES];
    for (o, v) in out.iter_mut().zip(us) {
        *o = ((v + 500) / 1000).min(u32::MAX as u64) as u32;
    }
    out
}

impl Drop for Idle {
    fn drop(&mut self) {
        // 空のパイプはスレッドの置き場へ返す (次のトンネルが pipe2 と fcntl を省ける)
        for d in self.dirs.iter_mut() {
            d.drop_relay();
        }
        // 段階 (T14.3 (1))。`relay` は `report` が引き算で出す
        self.info.detail.stages.first_relay = self.first_relay_ms;
        self.info.detail.stages.park = self.parked_ms;
        // 閉じた理由 (T14.4): 中継の中で決まっていればそれ、決まっていなければ
        // **先に EOF を出した側**。どちらも無ければプロキシ側の都合 (`shutdown`)
        let reason = self.close.unwrap_or(match self.first_eof {
            Some((0, _)) => CloseReason::ClientEof,
            Some(_) => CloseReason::ServerEof,
            None => CloseReason::Shutdown,
        });
        // 半閉じで終わったトンネルだけ、半閉じから反対側が閉じる (= いま) までを
        // 数える (T14.25)。両側とも EOF を出さずに終わったものは `None`
        let half_close = self.first_eof.map(|(_, at)| at.elapsed());
        // `dirs[0]` はクライアント → 宛先 (上り)、`dirs[1]` は宛先 → クライアント (下り)
        // `socks` はまだ生きている (落ちるのはこの関数を抜けたあと)。T14.5
        report(
            &self.info,
            self.dirs[0].moved,
            self.dirs[1].moved,
            reason,
            Some(&self.socks),
            half_close,
            // 中継の詰まりの向き (T14.42)。中継のループで積んだ us を ms に丸めるだけ
            stall_ms(self.stall_us),
        );
    }
}

impl Idle {
    /// ログ用の接続番号。
    pub fn id(&self) -> usize {
        self.info.conn_id
    }

    /// epoll に入れる記述子 (クライアントとサーバーの 2 本)。
    pub fn fds(&self) -> [RawFd; 2] {
        [self.socks[0].as_raw_fd(), self.socks[1].as_raw_fd()]
    }

    /// `/connections` の枠 (預かり所が状態を書くために借りる。T13.4)。
    pub fn slot(&self) -> Option<&Arc<ConnSlot>> {
        self.info.slot.as_ref()
    }

    /// 預かるときの期限。作れなければ `None` (預けない)。
    pub fn deadline(&self) -> Option<Instant> {
        // 無期限のトンネルも、期限の集合に入れるために遠い期限を置く
        Instant::now().checked_add(self.idle.unwrap_or(FOREVER))
    }

    /// 以後は預けない (断られたトンネルが猶予のたびに預け直そうとして空回りしないように)。
    pub fn no_park(&mut self) {
        self.park = None;
        self.parked_at = None;
    }

    /// 預かり所から戻ってきた (預けられていた時間を足す。T14.3 (1))。
    fn unpark(&mut self) {
        if let Some(t) = self.parked_at.take() {
            let parked = t.elapsed();
            if crate::profile::on() {
                self.parked_ms = self
                    .parked_ms
                    .saturating_add(crate::profile::ms_u32(parked));
            }
            // 次の猶予 (T.32.19)。時計はここで 1 回読んだぶんを使い回す
            if let Some((_, base)) = &self.park {
                self.grace = next_grace(self.grace, *base, parked);
            }
        }
    }

    /// 預ける前に中継の資源を手放す。
    ///
    /// 預けるのは両方向とも `pending == 0` のときだけなので、パイプの中身は空。
    /// 方向あたり記述子 2 本 (splice が使えない相手では 64 KiB のバッファ) を暇な間ずっと
    /// 抱えないようにする。戻ってきたら遅延生成のまま作り直す。
    fn release(&mut self) {
        for d in self.dirs.iter_mut() {
            d.drop_relay();
            d.readable = false;
        }
        // ここまでに運んだバイト数を `/connections` に見せる (預ける直前の 1 回)
        if let Some(s) = &self.info.slot {
            s.set_bytes(self.transferred);
        }
        // 預けた時刻 (T14.3 (1))。預かってもらえなければ `no_park` が消す。
        // 猶予を伸ばす判定 (T.32.19) にも使うので `--lite` でも読む (預ける 1 回に 1 回)
        self.parked_at = Some(Instant::now());
    }

    /// 動かせるだけ動かして、終わるか暇になるまで回す。
    ///
    /// 預け先があるときは「猶予のあいだ `poll` が空振りしたら [`Outcome::Idle`]」で戻る
    /// (打ち切りの期限は預かり所が見る)。預け先が無いときは従来どおり、
    /// アイドル打ち切りまでこのスレッドで待つ。
    fn run_until_idle(&mut self) -> Outcome {
        let conn_id = self.info.conn_id;
        let timeout_ms: i32 = match self.idle {
            Some(d) => d.as_millis().min(i32::MAX as u128) as i32,
            None => -1,
        };
        let grace = self.grace;
        let grace_ms: Option<i32> = self
            .park
            .as_ref()
            .map(|_| grace.as_millis().min(i32::MAX as u128) as i32);
        let socks = &self.socks;
        let slot = self.info.slot.as_ref();
        let established = self.info.established;
        let dirs = &mut self.dirs;
        let transferred = &mut self.transferred;
        let first_relay = &mut self.first_relay_ms;
        // 閉じた理由に使う 2 つ (T14.4)。どちらも 1 本の終わりに 1 回書くだけ
        let close = &mut self.close;
        let first_eof = &mut self.first_eof;
        // SNI を覗く枠 (T14.38)。`Info` の別々の欄なので `slot` と一緒に借りられる
        let peek_sni = &mut self.peek_sni;
        let sni_mismatch = &mut self.info.sni_mismatch;
        let addr_str = &self.info.addr_str;
        // 書けるのを待った時間 (T14.42)。預けても引き継ぐので `Idle` の欄
        let stall_us = &mut self.stall_us;
        let rate = &mut self.rate;
        // 空回りの証拠 (T15.0 (4))。`spins` は預けても引き継ぐので `Idle` の欄、
        // 残りの 2 つは**前の周の `poll`** の結果なので輪の外の局所変数
        // (待ちに入る所は `poll` の前なので、そこで置けるのは 1 周前の旗)
        let spins = &mut self.spins;
        let mut poll_woke = false;
        let mut last_revents = 0u32;
        // 先に EOF を出した側を 1 回だけ控える (T14.4 の閉じた理由 / T14.25 の
        // 半閉じの秒) のと同じ場所で、`/connections` の枠にも向きを置く
        // (T15.0 (4))。**トンネル 1 本につき多くて 1 回**で、2 回目からは
        // `is_none()` の比較だけ
        let mut note_eof = |side: usize| {
            if first_eof.is_none() {
                *first_eof = Some((side, Instant::now()));
                if let Some(s) = slot {
                    s.set_half_closed(side);
                }
            }
        };

        loop {
            let mut progressed = false;
            for d in dirs.iter_mut() {
                if d.done {
                    continue;
                }
                // 送信元 → 中継 (読めると分かってから中継バッファを用意する)
                if !d.src_eof && d.pending == 0 && d.readable {
                    // CONNECT の最初のバイトから SNI を覗く (T14.38)。**`200` を
                    // 書いたあと、最初の中継の前に 1 回だけ** `recv(MSG_PEEK)` で、
                    // バイトは消費しないので下の `fill` (splice) はそのまま通る。
                    // 壊れていれば `None` で中継は続く (ここで閉じない)
                    if *peek_sni && d.src == 0 {
                        *peek_sni = false;
                        let mut buf = [0u8; crate::sni::PEEK_LEN];
                        if let Ok(n) = sys::peek(socks[0].as_raw_fd(), &mut buf)
                            && let Some(name) = crate::sni::parse_client_hello(&buf[..n])
                        {
                            if let Some(s) = slot {
                                s.set_sni(name);
                            }
                            *sni_mismatch = crate::sni::differs(addr_str, name);
                        }
                    }
                    match d.fill(socks) {
                        Ok(0) => {
                            d.src_eof = true;
                            note_eof(d.src);
                            progressed = true;
                        }
                        Ok(n) => {
                            d.pending = n;
                            d.offset = 0;
                            progressed = true;
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => d.readable = false,
                        Err(ref e) => {
                            // 消えたクライアント (T14.52)。keepalive が尽きた
                            // ソケットは `ETIMEDOUT` で返るので、`client_eof`
                            // (ふつうに切った) と区別して記録する。オリジン側の
                            // `ETIMEDOUT` は「相手が消えた」ではあってもクライアント
                            // ではないので、従来どおり EOF として扱う
                            if d.src == CLIENT_SIDE && went_away(e) {
                                *close = Some(CloseReason::ClientDead);
                            }
                            d.src_eof = true;
                            note_eof(d.src);
                            progressed = true;
                        }
                    }
                }
                // 中継 → 送信先
                while d.pending > 0 {
                    match d.drain(socks) {
                        Ok(0) => break,
                        Ok(n) => {
                            // 最初の中継バイト (T14.3 (1))。比較 1 回で、時計を読むのは
                            // 1 本につき 1 回だけ (`--lite` では `established` が `None`)
                            if *transferred == 0
                                && let Some(t) = established
                            {
                                *first_relay = crate::profile::ms_u32(t.elapsed());
                            }
                            d.pending -= n;
                            d.offset += n;
                            d.moved += n as u64;
                            *transferred += n as u64;
                            // 渡したバイトを見張りに足す (待ちが 0 なので足し算だけ。T.29.34)
                            let _ = rate[d.dst].note(Duration::ZERO, n as u64);
                            progressed = true;
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        // 送信先が閉じた: この方向は終わり、相手にも伝える
                        Err(ref e) => {
                            // 書こうとした先が消えたクライアントだったとき (T14.52)
                            if d.dst == CLIENT_SIDE && went_away(e) {
                                *close = Some(CloseReason::ClientDead);
                            }
                            // 先に手を引いたのは**送信先**の側 (T14.4)
                            note_eof(d.dst);
                            d.pending = 0;
                            // パイプに残ったぶんはもう渡せない。置き場へ返さずに閉じる
                            // (返すと次のトンネルに他人のバイトが混ざる)
                            d.relay = Relay::Unset;
                            d.src_eof = true;
                            progressed = true;
                            break;
                        }
                    }
                }
                if d.src_eof && d.pending == 0 {
                    let _ = socks[d.dst].shutdown(Shutdown::Write);
                    d.done = true;
                    progressed = true;
                }
            }
            // 相手が消えたトンネルは、残った向き (オリジン → クライアント) を
            // 待たずにここで閉じる (T14.52)。書き先が死んでいるのでオリジンから
            // 来るバイトはもう誰にも渡せず、待てば `PROXY_TUNNEL_IDLE_SECS`
            // (300 秒) までこのトンネルが席を占め続ける
            if matches!(close, Some(CloseReason::ClientDead)) {
                log_trace!(Some(conn_id), "tunnel closed: the client went away");
                break;
            }
            if dirs.iter().all(|d| d.done) {
                break;
            }
            if progressed {
                // 1 バイトでも動いた (EOF を読んだのも「動いた」) ので、前の周の
                // 起床はちゃんと仕事になった = 空回りではない (T15.0 (4))。
                // **ここで倒さないと、普通に流れているトンネルが 1 往復ごとに
                // 1 回数えて**しまう: 起こされた次の周は `fill` が進んで
                // `continue` し、その次の周に `EAGAIN` で待ちへ来るため
                poll_woke = false;
                continue;
            }

            // どちらも動かなかったので待つ
            let mut events = [0i16; 2];
            for d in dirs.iter() {
                if d.done {
                    continue;
                }
                if !d.src_eof && d.pending == 0 {
                    events[d.src] |= POLLIN;
                }
                if d.pending > 0 {
                    events[d.dst] |= POLLOUT;
                }
            }
            // **関心の無い記述子は `poll` に渡さない** (T15.5)。`poll(2)` は
            // `events` が 0 でも `POLLERR` / `POLLHUP` を必ず返すので、渡すと
            // 「誰も面倒を見ない起床」が生まれる: 上の輪は `readable` を立てるだけで、
            // その記述子を読む向きが `done` なら輪の先頭で飛ばされ、反対の向きは
            // 自分の送信元しか見ていないので 1 バイトも進まない (`progressed = false`)。
            // `poll` が 0 を返さないのでアイドル打ち切りにも永久に当たらず、
            // **1 本のトンネルが 1 スレッドを回し続ける** (2026-09-18 のデプロイ先は
            // この形のトンネル 2 本で CPU 割り当て 0.5 コアを 27 時間食い切っていた。
            // `mtalk.google.com:5228`、齢 25.4 時間、通算 9 kB、0 bps)。
            //
            // 負の記述子は `poll(2)` が無視して `revents` を 0 にするので、これで
            // **`poll` が返した起床は必ず、その記述子への `fill` か `drain` の
            // システムコール 1 回につながる**という不変条件が立つ (関心が `POLLIN`
            // なら `fill` が EOF かエラーを返して進み、`POLLOUT` なら `drain` が
            // 書けるかエラーを返して進む)。どの向きも `done` でなければ `POLLIN` か
            // `POLLOUT` を 1 つは立てる (すぐ上の輪) ので、両方 0 になるのは
            // 「全部 `done`」= この手前で輪を抜けるときだけ。死んだ半閉じの
            // トンネルは今までどおりアイドル打ち切り (`PROXY_TUNNEL_IDLE_SECS`) で閉じる
            let mut fds = [
                PollFd::new(poll_fd(&socks[0], events[0]), events[0]),
                PollFd::new(poll_fd(&socks[1], events[1]), events[1]),
            ];
            // 両方向とも暇なら、待つのは猶予のあいだだけ。空振りしたら預ける。
            // **片方向だけ EOF (half-close) のトンネルは預けない**: 残った方向は
            // 「相手に shutdown を伝えて閉じる」までが仕事で、そこまで預かり所に
            // 持たせると起こし方が 2 通りになる。寿命も短いので単純さを採る
            // ここで止まる = 今の合計が落ち着いた値。`/connections` に見せるのは
            // この 1 回だけで、バイトごとにも splice ごとにも書かない (T13.4)。
            // **T15.0 (4)**: 同じ 1 回で空回りの証拠も置く。ここに来た時点で
            // 「1 バイトも進まなかった」は確定している (上の `if progressed` を
            // 通り抜けている) ので、空回りかどうかは「**前の周の `poll` が
            // 起床を返したか**」だけで決まる。数え上げも書き込みも `slot` の
            // 内側なので、**`--lite` は 0 増**
            if let Some(s) = slot {
                if poll_woke {
                    *spins = spins.saturating_add(1);
                }
                s.set_relaying(*transferred, *spins, last_revents);
            }
            let parkable = grace_ms.is_some() && dirs.iter().all(Dir::quiet);
            let mut wait_ms = match grace_ms {
                Some(g) if parkable => g,
                _ => timeout_ms,
            };
            // 書けずに待つ向きは、最小の速さの窓の残りより長く眠らない (T.29.34)。
            // 読まない相手はめったに `POLLOUT` を返さないので、無通信の締め切り (300 秒) まで
            // 眠ると判定が遅れる。縮めたときは、`poll` が 0 で戻ってもアイドル打ち切りにしない
            let mut capped = false;
            for (side, ev) in events.iter().enumerate() {
                if ev & POLLOUT != 0
                    && let Some(left) = rate[side].left()
                {
                    let ms = left.as_millis().clamp(1, i32::MAX as u128) as i32;
                    if wait_ms < 0 || ms < wait_ms {
                        wait_ms = ms;
                        capped = true;
                    }
                }
            }
            // 中継の詰まりの向き (T14.42)。**書けなくて待ちに入る回だけ**時計を
            // 読む: `events` に `POLLOUT` が立つのは、直前の `drain` (splice) が
            // `EAGAIN` で止まって `pending` が残っている方向だけなので、
            // **書けば必ず入る相手 (loopback) では `Instant::now()` を 1 度も
            // 呼ばない** — ここの比較 1 回で終わる。64 KiB ごとでも splice ごとでも
            // なく「待ちに入る回」なので、`--lite` でも同じように読む
            // (T14.25 の「最初の EOF の 1 回」と同じ扱い)
            let stall_from =
                ((events[CLIENT_SIDE] | events[ORIGIN_SIDE]) & POLLOUT != 0).then(Instant::now);
            let polled = sys::poll_fds(&mut fds, wait_ms);
            // 待ち終わったのでもう 1 回読み、**書けなかった向きだけ**に積む。
            // 両方詰まっていれば両方に積む (どちらも「その間書けなかった」ため)
            let mut too_slow = false;
            if let Some(t) = stall_from {
                let elapsed = t.elapsed();
                let waited = elapsed.as_micros().min(u64::MAX as u128) as u64;
                for (side, ev) in events.iter().enumerate() {
                    if ev & POLLOUT != 0 {
                        stall_us[side] = stall_us[side].saturating_add(waited);
                        // 同じ待ちを最小の速さの見張りにも積む (T.29.34)
                        too_slow |= rate[side].note(elapsed, 0).is_err();
                    }
                }
            }
            if too_slow {
                log_trace!(Some(conn_id), "tunnel closed: slower than the minimum rate");
                *close = Some(CloseReason::Error(ErrCause::Timeout));
                break;
            }
            // 次に待ちへ入るときに置く証拠 (T15.0 (4))。旗は**この周の `poll` が
            // 返したもの**で、置くのは次の周の待ちの直前 (待ちに入る所は `poll` の
            // 前なので、そこで置けるのは 1 周前の旗)。T15.5 で関心の無い記述子には
            // `-1` を渡すようにしたので、`revents` が立つのは面倒を見る向きがある
            // ときだけ
            poll_woke = matches!(polled, Ok(n) if n > 0);
            last_revents = crate::recent::pack_revents(fds[0].revents, fds[1].revents);
            match polled {
                Ok(0) if parkable => return Outcome::Idle,
                Ok(0) if wait_ms >= 0 && !capped => {
                    log_trace!(Some(conn_id), "tunnel idle timeout after {}ms", wait_ms);
                    *close = Some(CloseReason::IdleTimeout);
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    log_trace!(Some(conn_id), "tunnel poll failed: {}", e);
                    // `poll` が `ETIMEDOUT` を返すことはまず無いが、返ってきたなら
                    // 「相手が消えた」で間違いない (T14.52。比較 1 回)
                    *close = Some(if went_away(&e) {
                        CloseReason::ClientDead
                    } else {
                        CloseReason::error_from_io(&e)
                    });
                    break;
                }
            }
            for d in dirs.iter_mut() {
                // HUP / ERR でも read して EOF を確かめる
                if fds[d.src].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
                    d.readable = true;
                }
            }
        }

        log_trace!(
            Some(conn_id),
            "tunnel finished: {}B up / {}B down",
            dirs[0].moved,
            dirs[1].moved
        );
        Outcome::Done
    }
}

/// 中継を始める。暇になったら預け、預けられなければこのスレッドで続ける。
pub(super) fn start(
    opened: Opened,
    idle: Option<Duration>,
    min_transfer: MinRate,
    park: Option<(Arc<dyn Park>, Duration)>,
    hold: Box<dyn Send>,
) -> io::Result<()> {
    let Opened {
        client,
        server,
        info,
    } = opened;
    client.set_nonblocking(true)?;
    server.set_nonblocking(true)?;
    // 覗くかどうかは**トンネル 1 本につきここで 1 回**決める (中継のループでは
    // 旗を見るだけ)。個票の枠が無い `--lite` と、443 以外のポート (TLS とは
    // 限らない) では覗かない。T14.38
    let peek_sni = info.slot.is_some()
        && crate::sni::peek_on(
            crate::net::split_host_port_ref(&info.addr_str)
                .1
                .unwrap_or(0),
        );
    // 書き側の見張りは向きごとに 1 つ (T.29.34)
    // 判定は相手に届いた量で見る (送信キューに残った分を引く)
    let guard = min_transfer.guard(proxy_base::minrate::Side::Tunnel, info.conn_id);
    let mut rate = [guard.clone(), guard];
    rate[0].watch_queue(client.as_raw_fd());
    rate[1].watch_queue(server.as_raw_fd());
    let grace = park.as_ref().map_or(Duration::ZERO, |(_, g)| *g);
    drive(Box::new(Idle {
        socks: [client, server],
        dirs: [Dir::new(0, 1), Dir::new(1, 0)],
        transferred: 0,
        idle,
        park,
        grace,
        info,
        first_relay_ms: 0,
        parked_at: None,
        parked_ms: 0,
        close: None,
        first_eof: None,
        peek_sni,
        stall_us: [0; SIDES],
        spins: 0,
        rate,
        _hold: hold,
    }));
    Ok(())
}

/// 預かっていたトンネルをワーカーで再開する (事象が来て起こされたとき)。
pub fn resume(mut idle: Box<Idle>) {
    idle.unpark();
    if let Some(s) = idle.slot() {
        s.set_state(ConnState::Relaying);
        // 預かり所から戻ると**別のスレッド**が受け持つ (T15.0 (4))。
        // 番号を引くのはスレッドの一生に 1 回で、ここは原子の書き込み 1 回
        s.set_tid(sys::gettid());
    }
    drive(idle);
}

/// 期限切れで引き上げたトンネルを閉じる (`poll` が 0 を返したときと同じログと統計)。
pub fn expire(mut idle: Box<Idle>) {
    idle.unpark();
    log_trace!(Some(idle.id()), "tunnel idle timeout while parked");
    // 落ちるとソケットが閉じ、アクセスログと統計が出る
    drop(idle);
}

/// 暇になるまで回し、暇になったら預ける。
///
/// 終わるとここで `Idle` が落ちる (= どのワーカーで終わっても、アクセスログと統計は
/// 同じ場所を 1 回だけ通る)。
fn drive(mut idle: Box<Idle>) {
    loop {
        match idle.run_until_idle() {
            Outcome::Done => return,
            Outcome::Idle => {
                let Some((watch, _)) = idle.park.clone() else {
                    // 預け先が無ければ run_until_idle は Idle を返さない。
                    // 念のため、空回りせずに閉じる
                    return;
                };
                idle.release();
                match watch.park(idle) {
                    // 預けられた: このスレッドは解放される (続きは監視スレッドが起こす)
                    Ok(()) => return,
                    Err(mut back) => {
                        // 預かってもらえなかった (監視スレッドが死んだ、記述子が epoll に
                        // 入らない)。以後はこのスレッドが最後まで面倒をみる
                        back.no_park();
                        idle = back;
                    }
                }
            }
        }
    }
}
