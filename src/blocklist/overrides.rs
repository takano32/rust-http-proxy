//! 手動の上書き (一時的な許可 / 拒否)。状態ファイルの固定 256 スロットに置き、
//! いっぱいなら最も古いものを潰す。一覧より優先し、親ドメインにも効く。

use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::clock::now_epoch;
use crate::sync::RwLockExt;
use crate::{log_info, persist};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    pub host: String,
    pub block: bool,
    /// 期限 (epoch 秒)。0 なら消すまで有効
    pub expires: u64,
    pub created: u64,
    slot: usize,
}

impl Override {
    fn encode(&self) -> Vec<u8> {
        let mut e = crate::rrd::Enc::new();
        e.str(&self.host, 128)
            .u64(self.block as u64)
            .u64(self.expires)
            .u64(self.created);
        e.0
    }

    fn decode(slot: usize, payload: &[u8]) -> Option<Override> {
        let mut d = crate::rrd::Dec(payload);
        let host = d.str(128);
        if host.is_empty() {
            return None;
        }
        Some(Override {
            host,
            block: d.u64() != 0,
            expires: d.u64(),
            created: d.u64(),
            slot,
        })
    }

    fn live(&self, now: u64) -> bool {
        self.expires == 0 || self.expires > now
    }

    pub fn json(&self) -> String {
        format!(
            "{{\"host\":\"{}\",\"action\":\"{}\",\"expires\":{},\"created\":{}}}",
            crate::json::escape(&self.host),
            if self.block { "block" } else { "allow" },
            self.expires,
            self.created
        )
    }
}

static OVERRIDES: RwLock<Vec<Override>> = RwLock::new(Vec::new());
static STORE: std::sync::OnceLock<Arc<persist::Store>> = std::sync::OnceLock::new();

/// 状態ファイルをつなぎ、保存されていた上書きを読み戻す。
pub fn set_store(store: Arc<persist::Store>) {
    let now = now_epoch();
    let mut list: Vec<Override> = store
        .read_overrides()
        .iter()
        .filter_map(|(slot, p)| Override::decode(*slot, p))
        .filter(|o| o.live(now))
        .collect();
    list.sort_by_key(|o| o.created);
    let n = list.len();
    *OVERRIDES.write_locked() = list;
    let _ = STORE.set(store);
    if n > 0 {
        log_info!(None, "blocklist: {} manual overrides restored", n);
    }
}

/// 上書きを置く (同じホストの既存は置き換え)。`ttl` が 0 なら消すまで有効。
pub fn set_override(host: &str, block: bool, ttl: Duration) -> Override {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let now = now_epoch();
    let mut list = OVERRIDES.write_locked();
    list.retain(|o| o.live(now));
    let store = STORE.get();
    let slots = store.map_or(crate::rrd::OVERRIDE_SLOTS, |s| s.overrides_region().count);
    let slot = if let Some(pos) = list.iter().position(|o| o.host == host) {
        list.remove(pos).slot
    } else if list.len() < slots {
        (0..slots)
            .find(|s| !list.iter().any(|o| o.slot == *s))
            .unwrap_or(0)
    } else {
        // いっぱい: 最も古いものを潰す (環状)
        let oldest = list
            .iter()
            .enumerate()
            .min_by_key(|(_, o)| o.created)
            .map(|(i, _)| i)
            .unwrap_or(0);
        list.remove(oldest).slot
    };
    let o = Override {
        host,
        block,
        expires: if ttl.is_zero() {
            0
        } else {
            now + ttl.as_secs()
        },
        created: now,
        slot,
    };
    if let Some(st) = store {
        st.write_override(slot, &o.encode());
    }
    list.push(o.clone());
    o
}

/// 上書きを消す。あったら `true`。
pub fn clear_override(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let mut list = OVERRIDES.write_locked();
    let Some(pos) = list.iter().position(|o| o.host == host) else {
        return false;
    };
    let o = list.remove(pos);
    if let Some(st) = STORE.get() {
        st.clear_override(o.slot);
    }
    true
}

/// 生きている上書きの一覧 (新しい順)。
pub fn overrides() -> Vec<Override> {
    let now = now_epoch();
    let mut v: Vec<Override> = OVERRIDES
        .read_locked()
        .iter()
        .filter(|o| o.live(now))
        .cloned()
        .collect();
    v.sort_by_key(|o| std::cmp::Reverse(o.created));
    v
}

/// このホストか親ドメインに生きている上書きがあれば、それが拒否かどうか。
pub(super) fn lookup(host: &str) -> Option<bool> {
    let now = now_epoch();
    let list = OVERRIDES.read_locked();
    let mut hit = None;
    super::walk(host, |h| {
        hit = list
            .iter()
            .find(|o| o.live(now) && o.host == h)
            .map(|o| o.block);
        hit.is_some()
    });
    hit
}

#[cfg(test)]
pub(super) fn clear_all() {
    OVERRIDES.write_locked().clear();
}

#[cfg(test)]
pub(super) fn expire_all_for_test() {
    OVERRIDES
        .write_locked()
        .iter_mut()
        .for_each(|o| o.expires = 1);
}
