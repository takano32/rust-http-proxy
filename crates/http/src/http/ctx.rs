//! 1 要求の文脈 ([`Ctx`]): アクセスログ・ホスト別統計・接続元の個票に要るもの。

use std::time::Instant;

use crate::log::{Access, access};
use crate::metrics::{Detail, HostOutcome, Metrics};
use crate::recent::{
    ConnTally, STAGE_CLIENT_READ, STAGE_CONNECT, STAGE_DNS, STAGE_FIRST_BYTE, STAGE_QUEUE, STAGES,
};
use crate::request::RequestHeaders;

/// アクセスログと配信に必要なリクエストの文脈。
pub(super) struct Ctx<'a> {
    pub client_ip: &'a str,
    pub method: &'a str,
    pub url: &'a str,
    pub version: &'a str,
    pub started: Instant,
    pub conn_id: usize,
    pub metrics: &'a Metrics,
    pub req: &'a RequestHeaders,
    /// 応答後もクライアント接続を維持できるか
    pub keep_client: bool,
    pub head_only: bool,
    /// マッピング形式の要求 (Location を書き換える)
    pub mapped: bool,
    /// ホスト別統計のキー (`scheme://host:port`)
    pub pool_key: &'a str,
    /// 1 要求の内訳 (名前解決 / 接続 / 勝った族 / エラーの原因。T12.4 (2))。
    /// オリジンへ行った経路だけが埋める (キャッシュ HIT では全部 0)
    pub detail: Detail,
    /// クライアントから受けたバイト (要求のヘッダーと本文)。接続の個票の「上り」(T14.4)
    pub up_bytes: u64,
    /// 接続の個票に積み上げる箱 (T14.4)。`--lite` では `None`
    pub tally: Option<&'a std::cell::Cell<ConnTally>>,
    /// 追跡中の接続元か (`PROXY_TRACE_CLIENT`。T14.27)。立っていなければ分岐 1 回で飛ばす
    pub traced: bool,
}

impl Ctx<'_> {
    pub(super) fn log(&self, status: u16, bytes: u64, cache: &str) {
        let outcome = HostOutcome::from_access(cache, status);
        // 経過時間は 1 回だけ引く (clock_gettime は要求ごとに効いてくる)
        let took = self.started.elapsed();
        // 本文を流した時間 = 全体 − 初バイトまで (T14.3 (1))。**時計は足さない**
        // (ここまでに読んだ 2 つの差で出す。キャッシュ HIT は初バイトが無いので全部が本文)
        let mut detail = self.detail;
        if crate::profile::on() {
            let total = took.as_millis().min(u64::MAX as u128) as u64;
            detail.stages.body = total.saturating_sub(detail.first_byte_ms.unwrap_or(0)) as u32;
        }
        // 接続の個票 (`/recent`) の積み上げ (T14.4)。箱の中の足し算だけで、原子も鍵も無い。
        // **段階は T14.3 の `StageMs` からも入れる** (`first_relay` は CONNECT だけなので 0)
        if let Some(cell) = self.tally {
            let mut t = cell.get();
            let mut stage_ms = [0u64; STAGES];
            stage_ms[STAGE_DNS] = detail.dns_ms;
            stage_ms[STAGE_CONNECT] = detail.connect_ms;
            stage_ms[STAGE_FIRST_BYTE] = detail.first_byte_ms.unwrap_or(0);
            stage_ms[STAGE_QUEUE] = detail.stages.queue as u64;
            stage_ms[STAGE_CLIENT_READ] = detail.stages.client_read as u64;
            t.add_request(status, self.up_bytes, bytes, stage_ms, detail.syn_retrans);
            cell.set(t);
            // 接続元 1 つの追跡 (`/trace`。T14.27)。**旗が立っている接続だけ** 1 行書く。
            // 立っていない要求の費用はこの分岐 1 回だけで、段階の ms も所要時間も
            // すぐ上で既に組んだものをそのまま渡す (時計も確保も増やさない)。
            // ここは個票の決まり (パスを入れない) の**唯一の例外**で、追跡中の
            // 1 接続元に限り URL を先頭 256 B まで残す
            if self.traced {
                crate::trace::push(crate::trace::Line {
                    conn_id: self.conn_id,
                    client: self.client_ip,
                    method: self.method,
                    target: self.url,
                    version: self.version,
                    status,
                    took_ms: took.as_millis().min(u64::MAX as u128) as u64,
                    bytes,
                    stage_ms,
                    // 閉じた理由は接続 1 本のものなので CONNECT だけ (T14.4 の `/recent` にある)
                    reason: None,
                });
            }
        }
        // 向き別のバイト (T14.26)。**新しい計数はしていない**: 上りは要求本文
        // (個票が既に使っている `up_bytes`)、下りはこの応答のバイト (アクセスログに
        // 出すのと同じ値) をそのまま運ぶだけ。ホスト別統計の鍵の内側で足す
        detail.bytes_in = self.up_bytes;
        detail.bytes_out = bytes;
        // 読んだ壁時計は接続元の個票にも渡す (要求 1 本で 1 回。T34.146)
        let now =
            self.metrics
                .record_host_detail(self.pool_key, outcome, bytes, Some(took), detail);
        self.metrics
            // 宛先はホスト別統計と同じ鍵 (`scheme://host:port`)。接続元の個票 (`/clients`) は
            // この鍵からホストとポートを取る (T14.7)
            .record_client_at(
                self.client_ip,
                outcome,
                bytes,
                (self.up_bytes, bytes),
                Some(took),
                Some(self.pool_key),
                now,
            );
        let mut digits = [0u8; 5];
        access(
            self.conn_id,
            &Access {
                client: self.client_ip,
                method: self.method,
                target: self.url,
                version: self.version,
                status: decimal(&mut digits, status as u64),
                bytes,
                duration_ms: took.as_secs_f64() * 1000.0,
                cache,
            },
        );
    }

    /// エラーを返したときのログと統計。個票のリング (`/errors`) にも 1 件残す (T13.4)。
    ///
    /// [`Ctx::log`] と分けてあるのは、**エラーの経路からしか個票を書かない**ため
    /// (成功の熱い経路には 1 命令も足さない)。
    pub(super) fn log_failure(&self, status: u16) {
        self.metrics
            .record_error(false, self.pool_key, self.client_ip, status, &self.detail);
        self.log(status, 0, "ERROR");
    }

    pub(super) fn connection_line(&self) -> &'static str {
        if self.keep_client {
            "Connection: keep-alive"
        } else {
            "Connection: close"
        }
    }
}

/// 10 進の数字を確保せずに書く (`to_string` / `format!` は `String` を 1 本作る)。
fn decimal(buf: &mut [u8; 5], mut v: u64) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 || i == 0 {
            break;
        }
    }
    // 数字しか書いていないので UTF-8 として必ず妥当
    std::str::from_utf8(&buf[i..]).unwrap_or("0")
}

/// `Content-Length: N` を確保せずに組み立てる (`buf` は呼び出し側のスタック)。
pub(super) fn content_length_line(buf: &mut [u8; 40], n: u64) -> &str {
    const NAME: &[u8] = b"Content-Length: ";
    buf[..NAME.len()].copy_from_slice(NAME);
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    let mut v = n;
    loop {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let end = NAME.len() + (digits.len() - i);
    buf[NAME.len()..end].copy_from_slice(&digits[i..]);
    std::str::from_utf8(&buf[..end]).unwrap_or("Content-Length: 0")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 確保しない 10 進が `to_string` / `format!` と同じこと (状態コードと `Content-Length`)。
    #[test]
    fn stack_built_numbers_match_format() {
        for v in [0u64, 7, 99, 100, 404, 65535, 99999] {
            let mut buf = [0u8; 5];
            assert_eq!(decimal(&mut buf, v), v.to_string());
        }
        for n in [0u64, 1, 1024, 4_294_967_296, u64::MAX] {
            let mut buf = [0u8; 40];
            assert_eq!(
                content_length_line(&mut buf, n),
                format!("Content-Length: {}", n)
            );
        }
    }
}
