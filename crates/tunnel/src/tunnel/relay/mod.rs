//! Linux の 1 スレッド中継 (`poll` + `splice`) と、暇なときの預け入れ。
//!
//! 片方向の中継 (`Dir`) と中継パイプの置き場は `dir`、向きの送り出し (`Dir::pump`) は `pump`、
//! 待ちの決め方 (`poll` の関心・待つ長さ・詰まりの計測) は `wait` にある。
use std::io;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::open::{Info, Opened};
use super::report::report;
use crate::log_trace;
use crate::metrics::ErrCause;
use crate::recent::{CloseReason, ConnSlot, ConnState, SIDES};
use crate::sys;
use proxy_base::minrate::MinRate;

mod dir;
mod pump;
#[cfg(test)]
mod tests;
mod wait;

use dir::{Dir, went_away};
use pump::{EofNote, Flow, Progress};
use wait::{charge_stall, interest, mark_readable, poll_set, stall_clock, wait_budget};

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
/// 猶予を伸ばす上限 (T32.19)。
///
/// 預けて **[`SHORT_PARK`] 未満で起こされた**トンネルは、次の猶予を倍にしていく
/// (100 → 200 → 400 → 800 → 1,000 ms)。0.5 秒ごとに小さく往復するトンネル
/// (tokyo の login.live.com は 220 秒に 407 回預けていた) は数回で預けなくなり、
/// 預けて引き上げる 1 往復 (epoll_ctl 4・futex 約 4・スレッドの起床 3、実測 約 115 us)
/// を払わなくなる。秒〜分の単位で暇なトンネルは預けたあと長く眠るので、
/// 起こされた時点で猶予は下限に戻る (預かりの数とスレッドの数は今のまま)
const MAX_PARK_GRACE: Duration = Duration::from_secs(1);
/// これより短く預けられていたら「預けたのは損だった」とみなす (T32.19)。
const SHORT_PARK: Duration = Duration::from_secs(1);

/// 起こされたあとの次の猶予 (T32.19)。`parked` は預けられていた時間、`base` は下限。
fn next_grace(grace: Duration, base: Duration, parked: Duration) -> Duration {
    if parked < SHORT_PARK {
        grace.saturating_mul(2).min(MAX_PARK_GRACE.max(base))
    } else {
        base
    }
}

/// [`Idle::run_until_idle`] の結果。
enum Outcome {
    /// トンネルは終わった (運んだ合計は `Flow::transferred` にある)
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
    /// 向きの送り出しが書く数え (運んだ合計・閉じた理由・先に EOF を出した側・SNI・
    /// 最小の速さの見張り。預けても引き継ぐ)
    flow: Flow,
    /// 無通信で打ち切るまでの時間 (`None` で無期限)
    idle: Option<Duration>,
    /// 預け先と、預ける前に同じスレッドで待ってみる猶予の下限 (`None` なら預けない)
    park: Option<(Arc<dyn Park>, Duration)>,
    /// いまの猶予 (下限から始め、すぐ起こされるたびに倍にする。[`next_grace`]。T32.19)
    grace: Duration,
    /// アクセスログと統計に要る素性
    info: Info,
    /// いま預けられているなら、預けた時刻 (T14.3 (1))
    parked_at: Option<Instant>,
    /// 預けられていた合計 (ms)
    parked_ms: u32,
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
        self.info.detail.stages.first_relay = self.flow.first_relay_ms;
        self.info.detail.stages.park = self.parked_ms;
        // 閉じた理由 (T14.4): 中継の中で決まっていればそれ、決まっていなければ
        // **先に EOF を出した側**。どちらも無ければプロキシ側の都合 (`shutdown`)
        let reason = self.flow.close.unwrap_or(match self.flow.eof.0 {
            Some((0, _)) => CloseReason::ClientEof,
            Some(_) => CloseReason::ServerEof,
            None => CloseReason::Shutdown,
        });
        // 半閉じで終わったトンネルだけ、半閉じから反対側が閉じる (= いま) までを
        // 数える (T14.25)。両側とも EOF を出さずに終わったものは `None`
        let half_close = self.flow.eof.0.map(|(_, at)| at.elapsed());
        // 覗いた SNI の食い違い (T14.38)。中継の入口で 1 回だけ立てた旗を、統計に足す
        // `report` が読む `Info` へ写す
        self.info.sni_mismatch = self.flow.sni_mismatch;
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
            // 次の猶予 (T32.19)。時計はここで 1 回読んだぶんを使い回す
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
            s.set_bytes(self.flow.transferred);
        }
        // 預けた時刻 (T14.3 (1))。預かってもらえなければ `no_park` が消す。
        // 猶予を伸ばす判定 (T32.19) にも使うので `--lite` でも読む (預ける 1 回に 1 回)
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
        let slot = self.info.slot.as_ref();
        let info = &self.info;
        let socks = &self.socks;
        let dirs = &mut self.dirs;
        // 向きの送り出しが書く数え (`Idle` の別々の欄なので一緒に借りられる)
        let flow = &mut self.flow;
        // 書けるのを待った時間 (T14.42)。預けても引き継ぐので `Idle` の欄
        let stall_us = &mut self.stall_us;
        // 空回りの証拠 (T15.0 (4))。`spins` は預けても引き継ぐので `Idle` の欄、
        // 残りの 2 つは**前の周の `poll`** の結果なので輪の外の局所変数
        // (待ちに入る所は `poll` の前なので、そこで置けるのは 1 周前の旗)
        let spins = &mut self.spins;
        let mut poll_woke = false;
        let mut last_revents = 0u32;

        loop {
            let mut progressed = false;
            for d in dirs.iter_mut() {
                progressed |= d.pump(socks, flow, info) == Progress::Moved;
            }
            // 相手が消えたトンネルは、残った向き (オリジン → クライアント) を
            // 待たずにここで閉じる (T14.52)。書き先が死んでいるのでオリジンから
            // 来るバイトはもう誰にも渡せず、待てば `PROXY_TUNNEL_IDLE_SECS`
            // (300 秒) までこのトンネルが席を占め続ける
            if matches!(flow.close, Some(CloseReason::ClientDead)) {
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

            // どちらも動かなかったので待つ (関心の無い記述子は渡さない。T15.5)
            let events = interest(dirs);
            let mut fds = poll_set(socks, events);
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
                s.set_relaying(flow.transferred, *spins, last_revents);
            }
            let parkable = grace_ms.is_some() && dirs.iter().all(Dir::quiet);
            let (wait_ms, capped) = wait_budget(
                grace_ms,
                parkable,
                timeout_ms,
                |side| flow.rate[side].left(),
                events,
            );
            let stall_from = stall_clock(events);
            let polled = sys::poll_fds(&mut fds, wait_ms);
            if charge_stall(stall_from, events, stall_us, &mut flow.rate) {
                log_trace!(Some(conn_id), "tunnel closed: slower than the minimum rate");
                flow.close = Some(CloseReason::Error(ErrCause::Timeout));
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
                    flow.close = Some(CloseReason::IdleTimeout);
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    log_trace!(Some(conn_id), "tunnel poll failed: {}", e);
                    // `poll` が `ETIMEDOUT` を返すことはまず無いが、返ってきたなら
                    // 「相手が消えた」で間違いない (T14.52。比較 1 回)
                    flow.close = Some(if went_away(&e) {
                        CloseReason::ClientDead
                    } else {
                        CloseReason::error_from_io(&e)
                    });
                    break;
                }
            }
            mark_readable(dirs, &fds);
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
    // 書き側の見張りは向きごとに 1 つ (T29.34)
    // 判定は相手に届いた量で見る (送信キューに残った分を引く)
    let guard = min_transfer.guard(proxy_base::minrate::Side::Tunnel, info.conn_id);
    let mut rate = [guard.clone(), guard];
    rate[0].watch_queue(client.as_raw_fd());
    rate[1].watch_queue(server.as_raw_fd());
    let grace = park.as_ref().map_or(Duration::ZERO, |(_, g)| *g);
    drive(Box::new(Idle {
        socks: [client, server],
        dirs: [Dir::new(0, 1), Dir::new(1, 0)],
        flow: Flow {
            transferred: 0,
            first_relay_ms: 0,
            close: None,
            eof: EofNote::default(),
            peek_sni,
            sni_mismatch: false,
            rate,
        },
        idle,
        park,
        grace,
        info,
        parked_at: None,
        parked_ms: 0,
        stall_us: [0; SIDES],
        spins: 0,
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
