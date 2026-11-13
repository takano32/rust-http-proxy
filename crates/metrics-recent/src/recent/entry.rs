//! 閉じた接続 1 本の個票 ([`RecentEntry`]) と、接続を持つスレッドが要求ごとに積む箱
//! ([`ConnTally`])。`recent` の一部 (T34.2 で `recent/mod.rs` から移した)。
//!
//! 個票を作るのは [`ConnSlot::closed_entry`](super::ConnSlot::closed_entry) (接続の終了で 1 回)、
//! 書き出すのは [`RecentEntry::to_json`] (`/recent` の 1 要素。**鍵の順は末尾に足すだけ**)。
//! どちらも時計を引数か枠から受けるので、ここだけで単体試験が書ける。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `recent` の `pub use` でそのまま。

use super::{
    CLIENT_SIDE, CloseReason, ORIGIN_SIDE, SIDE_NAMES, SIDES, STAGE_CONNECT, STAGE_NAMES, STAGES,
};

/// 1 接続ぶんの「個票に足す値」(T14.4)。
///
/// **原子ではない。** 1 本の接続を同時に触るスレッドは 1 つだけなので、要求ごとの
/// 積み上げは [`std::cell::Cell`] に置いた普通の変数で行い、**接続の終了で 1 回だけ**
/// [`ConnSlot::finish`](super::ConnSlot::finish) で個票へ移す (要求ごとの原子操作を 1 つも増やさないため)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnTally {
    /// クライアントから受けたバイト (要求のヘッダーと本文、トンネルは上り)
    pub up: u64,
    /// クライアントへ返したバイト (応答、トンネルは下り)
    pub down: u64,
    /// 最後の応答の状態コード (http だけ)
    pub status: u16,
    /// 段階の ms (いちばん大きかった要求の値)。並びは [`STAGE_NAMES`]
    pub stage_ms: [u64; STAGES],
    /// カーネルの平滑化 RTT (us) と再送の通算 (`[クライアント側, オリジン側]`。T14.5)。
    /// 書くのは**接続の終わりに `getsockopt` を呼んだ 1 回だけ**で、`0` は「読めなかった」
    pub rtt_us: [u32; SIDES],
    pub retrans: [u32; SIDES],
    /// 確立までに SYN を送り直した回数 (T14.46)。keep-alive の HTTP 接続では
    /// **いちばん多かった要求**の値 (プールが接続を張った要求だけ 0 でない)
    pub syn_retrans: u8,
    /// 中継が**書けるのを待った**合計 ms (`[クライアント側, オリジン側]`。T14.42)。
    /// トンネルだけが埋める (http の接続と、繋がらなかった CONNECT は 0)
    pub stall_ms: [u32; SIDES],
}

impl ConnTally {
    /// 1 要求ぶんを足す (段階の ms と SYN の再送は**いちばん大きかった要求**を採る)。
    pub fn add_request(
        &mut self,
        status: u16,
        up: u64,
        down: u64,
        stage_ms: [u64; STAGES],
        syn_retrans: u8,
    ) {
        self.up = self.up.saturating_add(up);
        self.down = self.down.saturating_add(down);
        self.status = status;
        for (slot, ms) in self.stage_ms.iter_mut().zip(stage_ms) {
            *slot = (*slot).max(ms);
        }
        self.syn_retrans = self.syn_retrans.max(syn_retrans);
    }
}

/// 閉じた接続 1 本の個票 (`/recent`)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentEntry {
    /// 接続の通し番号 (ログの `conn#N`、`/connections` の `id` と同じ)
    pub id: u64,
    /// 開いた時刻 (epoch 秒)
    pub at: u64,
    /// 接続元 IP
    pub client: String,
    /// 宛先 (`host:port`。CONNECT はトンネルの相手、http は最初の要求の宛先)
    pub target: String,
    /// CONNECT トンネルか (`false` = keep-alive の HTTP)
    pub connect: bool,
    /// 寿命 (秒)
    pub secs: u64,
    /// 処理した要求の数 (http だけ)
    pub requests: u32,
    /// クライアントから受けたバイト / クライアントへ返したバイト
    pub up: u64,
    pub down: u64,
    /// 閉じた理由
    pub reason: CloseReason,
    /// 最後の応答の状態コード (http だけ。0 = 無し)
    pub status: u16,
    /// 預かり所にいた合計秒と、預けられた回数
    pub parked_secs: u64,
    pub parks: u32,
    /// 段階の ms ([`STAGE_NAMES`] の並び)
    pub stage_ms: [u64; STAGES],
    /// カーネルの平滑化 RTT (us) と再送の通算 (`[クライアント側, オリジン側]`。T14.5)。
    /// `0` は「読めなかった」(Linux 以外・`--lite`・もう閉じていた) で JSON では `null`
    pub rtt_us: [u32; SIDES],
    pub retrans: [u32; SIDES],
    /// CONNECT の最初のバイトから覗いた SNI (`PROXY_PEEK_SNI`。T14.38)。
    /// `None` は「覗いていない (443 以外・`--lite`・`off`) / 読めなかった」で JSON では `null`。
    /// **IP リテラル宛ての CONNECT では、これが「本当の宛先」**
    pub sni: Option<Box<str>>,
    /// 確立までに SYN を送り直した回数 (T14.46)。`0` は「再送なし / 読めなかった /
    /// Linux 以外」。**1 回で確立が 1 秒、2 回で 3 秒に飛ぶ**ので、`ms.connect` が
    /// 1,000 ms 台の個票はここが 1 以上になる (どちら側の待ち受けが溢れたかの手掛かり)
    pub syn_retrans: u8,
    /// 中継が**書けるのを待った**合計 ms (`[クライアント側, オリジン側]`。T14.42)。
    /// `client` が大きい = 利用者の下り回線か端末が読んでいない、`origin` が大きい =
    /// オリジンか利用者の上りが詰まっている。CONNECT のトンネルだけが埋める
    pub stall_ms: [u32; SIDES],
    /// 中継の輪が「起こされたのに 1 バイトも進まなかった」回数の通算 (T15.0)。
    /// 寿命に対して桁違いに大きければ空回り (T15.5 の形)。トンネルだけが埋める
    pub spins: u64,
    /// 半閉じの (向き, 半閉じから閉じるまでの ms)。半閉じしていなければ `None`。
    /// 向きは「**先に EOF を出した側**」で、閉じた理由 (`client_eof` / `server_eof`) と
    /// 同じ出どころ (`Idle::first_eof`)。T15.0
    pub half_closed: Option<(usize, u64)>,
}

/// `us` を ms の JSON にする (`0` = 読めなかった → `null`。T14.5)。
///
/// 桁は成り行きに任せる (`{}` は往復できる最短の表記を出すので、us を 1000 で割った値は
/// `0.052` / `30.1` / `4294967.295` のように**余計な 0 が付かない**)。us のままにしないのは、
/// `/hosts` の `rtt_ms` と `/recent` の `ms`(段階) が ms なので単位を揃えるため。
fn rtt_ms_json(us: u32) -> String {
    if us == 0 {
        return "null".to_string();
    }
    format!("{}", us as f64 / 1000.0)
}

impl RecentEntry {
    /// 並べ替えに使う転送量の合計。
    pub fn bytes(&self) -> u64 {
        self.up.saturating_add(self.down)
    }

    /// 並べ替えに使う確立の ms (`?sort=slow`)。
    pub fn connect_ms(&self) -> u64 {
        self.stage_ms[STAGE_CONNECT]
    }

    /// `/recent` の 1 要素。
    ///
    /// `ms` の中身だけ可変で、**`dns` と `connect` は 0 でも必ず出す**
    /// (`?sort=slow` が読む値なので、無いのと 0 を区別させない)。残りの 4 つは
    /// 0 なら出さない — T14.3 が段階の時計を足すまで 0 のままなので、それまでは
    /// 1 バイトも増えない。
    pub fn to_json(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(256);
        let _ = write!(
            out,
            "{{\"id\":{},\"at\":{},\"client\":\"{}\",\"target\":\"{}\",\"kind\":\"{}\",\"secs\":{},\"reqs\":{},\"up\":{},\"down\":{},\"reason\":\"{}\",\"status\":{},\"parked_secs\":{},\"parks\":{},\"ms\":{{",
            self.id,
            self.at,
            crate::json::escape(&self.client),
            crate::json::escape(&self.target),
            if self.connect { "connect" } else { "http" },
            self.secs,
            self.requests,
            self.up,
            self.down,
            self.reason.text(),
            self.status,
            self.parked_secs,
            self.parks,
        );
        for (i, (name, ms)) in STAGE_NAMES.iter().zip(self.stage_ms).enumerate() {
            // `dns` と `connect` は必ず、残りは 0 でないときだけ
            if i > STAGE_CONNECT && ms == 0 {
                continue;
            }
            let _ = write!(out, "{}\"{}\":{}", if i == 0 { "" } else { "," }, name, ms);
        }
        out.push('}');
        // カーネルの RTT と再送 (T14.5)。**両側とも必ず出す**が、読めなかった側
        // (Linux 以外・`--lite`・もう閉じていた) は `null` — 0 ms と区別させないため
        let _ = write!(
            out,
            ",\"rtt_ms\":{{\"client\":{},\"origin\":{}}},\"retrans\":{{\"client\":{},\"origin\":{}}}",
            rtt_ms_json(self.rtt_us[CLIENT_SIDE]),
            rtt_ms_json(self.rtt_us[ORIGIN_SIDE]),
            self.retrans[CLIENT_SIDE],
            self.retrans[ORIGIN_SIDE],
        );
        // 覗いた SNI (T14.38)。**末尾に足した** (既存の鍵の順は変えない)。
        // 覗いていない (443 以外・`--lite`・`off`) と読めなかったときは `null`
        match &self.sni {
            Some(name) => {
                let _ = write!(out, ",\"sni\":\"{}\"", crate::json::escape(name));
            }
            None => out.push_str(",\"sni\":null"),
        }
        // 確立までの SYN の再送 (T14.46)。**末尾に足した** (既存の鍵の順は変えない)。
        // 0 でも必ず出す (「欄が無い」= 古い版と区別させるため)
        let _ = write!(out, ",\"syn_retrans\":{}", self.syn_retrans);
        // 中継の詰まりの向き (T14.42)。**末尾に足した** (既存の鍵の順は変えない)。
        // 両方 0 でも必ず出す (「詰まっていない」と「欄が無い」を区別させるため)
        let _ = write!(
            out,
            ",\"stall_ms\":{{\"client\":{},\"origin\":{}}}",
            self.stall_ms[CLIENT_SIDE], self.stall_ms[ORIGIN_SIDE],
        );
        // 空回りの回数と半閉じの向き (T15.0)。**末尾に足した** (既存の鍵の順は
        // 変えない)。`/connections` と違ってここは件数で切る口なので、0 でも必ず出す
        // (`syn_retrans` / `stall_ms` と同じ扱い)。半閉じしていない接続は `null`
        let _ = write!(out, ",\"spins\":{}", self.spins);
        match self.half_closed {
            Some((side, ms)) => {
                let _ = write!(
                    out,
                    ",\"half_closed\":\"{}\",\"half_closed_ms\":{}}}",
                    SIDE_NAMES[side.min(SIDES - 1)],
                    ms
                );
            }
            None => out.push_str(",\"half_closed\":null,\"half_closed_ms\":null}"),
        }
        out
    }
}
