//! 壁時計 (epoch 秒)。キャッシュの鮮度、履歴、状態ファイルの時刻に使う。

use std::time::{SystemTime, UNIX_EPOCH};

/// 現在時刻 (epoch 秒)。時計が 1970 年より前なら 0。
pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
