//! 向きの送り出し ([`Dir::pump`]): 送信元 → 中継 → 送信先を、動かせるだけ動かす。
//!
//! 書き込む先は `Idle` の 1 つの欄 [`Flow`] にまとめてある (輪の冒頭で欄を 1 本ずつ
//! `&mut` に割いていた形の置き換え。T.34.10)。
use std::io;
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::dir::{Dir, Relay, went_away};
use crate::recent::{CLIENT_SIDE, CloseReason, ConnSlot, SIDES};
use crate::sys;
use crate::tunnel::open::Info;
use proxy_base::minrate::RateGuard;

/// [`Dir::pump`] の結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Progress {
    /// 1 バイトでも動いた (EOF を読んだのも、書き先が閉じたのも「動いた」)
    Moved,
    /// 何も動かなかった (待ちに入ってよい)
    Stuck,
}

/// 先に EOF を出したのはどちらと、それがいつか (`0` = クライアント、`1` = 宛先)。
///
/// 側は閉じた理由 (T14.4)、時刻は**半閉じから反対側が閉じるまで**の分布 (T14.25)。
/// 書くのは [`EofNote::note`] の中 = **トンネル 1 本につき多くて 1 回**で、
/// バイトを動かす道 (splice の往復) には 1 命令も足していない
#[derive(Default)]
pub(super) struct EofNote(pub(super) Option<(usize, Instant)>);

impl EofNote {
    /// 先に EOF を出した側を 1 回だけ控える (T14.4 の閉じた理由 / T14.25 の
    /// 半閉じの秒) のと同じ場所で、`/connections` の枠にも向きを置く
    /// (T15.0 (4))。**トンネル 1 本につき多くて 1 回**で、2 回目からは
    /// `is_none()` の比較だけ
    #[inline]
    pub(super) fn note(&mut self, side: usize, slot: Option<&Arc<ConnSlot>>) {
        if self.0.is_none() {
            self.0 = Some((side, Instant::now()));
            if let Some(s) = slot {
                s.set_half_closed(side);
            }
        }
    }
}

/// 向きの送り出しが書く、トンネル 1 本ぶんの数え (`Idle` の欄。預けても引き継ぐ)。
pub(super) struct Flow {
    /// 双方向に運んだ合計バイト数
    pub(super) transferred: u64,
    /// `200` を書いてから最初の中継バイトまで (ms。T14.3 (1))
    pub(super) first_relay_ms: u32,
    /// 中継の中で決まった閉じた理由 (アイドル打ち切り / `poll` の失敗。T14.4)。
    /// 外から閉じられたとき (追い出し・監視の停止) は枠に直接書いてあるので
    /// ここは `None` のままで、`ConnSlot::finish` の先着優先でそちらが勝つ
    pub(super) close: Option<CloseReason>,
    /// 先に EOF を出した側 (T14.4 / T14.25)
    pub(super) eof: EofNote,
    /// まだ SNI を覗いていないか (`PROXY_PEEK_SNI`。T14.38)。
    ///
    /// 立っているのは「個票の枠があり (= `--lite` ではない)、宛先が 443
    /// (または試験用の口) の CONNECT」だけで、**最初にクライアント側が読めた
    /// ときに 1 回覗いて倒す** (トンネル 1 本に `recv(MSG_PEEK)` は多くて 1 回)
    pub(super) peek_sni: bool,
    /// 覗いた SNI が CONNECT のホストと食い違ったか (T14.38)。`Idle` が落ちるときに
    /// `Info::sni_mismatch` へ写す (統計に足すのは `report`)
    pub(super) sni_mismatch: bool,
    /// 書き側の最小の速さの見張り (`[クライアントへ, オリジンへ]`。添字は `socks` と同じ。T.29.34)。
    ///
    /// 積むのは `stall_us` と同じ「書けなくて `poll` で待った時間」と、その向きへ渡したバイト。
    /// **送るものが無くて待った時間 (片側が黙っているだけの half-idle) は入らない**ので、
    /// 暇なトンネルは今までどおり `PROXY_TUNNEL_IDLE_SECS` で閉じる。預けても引き継ぐ
    pub(super) rate: [RateGuard; SIDES],
}

/// CONNECT の最初のバイトから SNI を覗く (T14.38)。**`200` を
/// 書いたあと、最初の中継の前に 1 回だけ** `recv(MSG_PEEK)` で、
/// バイトは消費しないので後の `fill` (splice) はそのまま通る。
/// 壊れていれば `None` で中継は続く (ここで閉じない)
#[inline]
pub(super) fn peek_sni_once(flow: &mut Flow, info: &Info, socks: &[TcpStream; 2]) {
    flow.peek_sni = false;
    let mut buf = [0u8; crate::sni::PEEK_LEN];
    if let Ok(n) = sys::peek(socks[0].as_raw_fd(), &mut buf)
        && let Some(name) = crate::sni::parse_client_hello(&buf[..n])
    {
        if let Some(s) = &info.slot {
            s.set_sni(name);
        }
        flow.sni_mismatch = crate::sni::differs(&info.addr_str, name);
    }
}

impl Dir {
    /// この向きを動かせるだけ動かす (送信元 → 中継 → 送信先、終わったら相手に `shutdown`)。
    #[inline(always)]
    pub(super) fn pump(
        &mut self,
        socks: &[TcpStream; 2],
        flow: &mut Flow,
        info: &Info,
    ) -> Progress {
        if self.done {
            return Progress::Stuck;
        }
        let mut progressed = false;
        // 送信元 → 中継 (読めると分かってから中継バッファを用意する)
        if !self.src_eof && self.pending == 0 && self.readable {
            if flow.peek_sni && self.src == 0 {
                peek_sni_once(flow, info, socks);
            }
            match self.fill(socks) {
                Ok(0) => {
                    self.src_eof = true;
                    flow.eof.note(self.src, info.slot.as_ref());
                    progressed = true;
                }
                Ok(n) => {
                    self.pending = n;
                    self.offset = 0;
                    progressed = true;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => self.readable = false,
                Err(ref e) => {
                    // 消えたクライアント (T14.52)。keepalive が尽きた
                    // ソケットは `ETIMEDOUT` で返るので、`client_eof`
                    // (ふつうに切った) と区別して記録する。オリジン側の
                    // `ETIMEDOUT` は「相手が消えた」ではあってもクライアント
                    // ではないので、従来どおり EOF として扱う
                    if self.src == CLIENT_SIDE && went_away(e) {
                        flow.close = Some(CloseReason::ClientDead);
                    }
                    self.src_eof = true;
                    flow.eof.note(self.src, info.slot.as_ref());
                    progressed = true;
                }
            }
        }
        // 中継 → 送信先
        while self.pending > 0 {
            match self.drain(socks) {
                Ok(0) => break,
                Ok(n) => {
                    // 最初の中継バイト (T14.3 (1))。比較 1 回で、時計を読むのは
                    // 1 本につき 1 回だけ (`--lite` では `established` が `None`)
                    if flow.transferred == 0
                        && let Some(t) = info.established
                    {
                        flow.first_relay_ms = crate::profile::ms_u32(t.elapsed());
                    }
                    self.pending -= n;
                    self.offset += n;
                    self.moved += n as u64;
                    flow.transferred += n as u64;
                    // 渡したバイトを見張りに足す (待ちが 0 なので足し算だけ。T.29.34)
                    let _ = flow.rate[self.dst].note(Duration::ZERO, n as u64);
                    progressed = true;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                // 送信先が閉じた: この方向は終わり、相手にも伝える
                Err(ref e) => {
                    // 書こうとした先が消えたクライアントだったとき (T14.52)
                    if self.dst == CLIENT_SIDE && went_away(e) {
                        flow.close = Some(CloseReason::ClientDead);
                    }
                    // 先に手を引いたのは**送信先**の側 (T14.4)
                    flow.eof.note(self.dst, info.slot.as_ref());
                    self.pending = 0;
                    // パイプに残ったぶんはもう渡せない。置き場へ返さずに閉じる
                    // (返すと次のトンネルに他人のバイトが混ざる)
                    self.relay = Relay::Unset;
                    self.src_eof = true;
                    progressed = true;
                    break;
                }
            }
        }
        if self.src_eof && self.pending == 0 {
            let _ = socks[self.dst].shutdown(Shutdown::Write);
            self.done = true;
            progressed = true;
        }
        if progressed {
            Progress::Moved
        } else {
            Progress::Stuck
        }
    }
}
