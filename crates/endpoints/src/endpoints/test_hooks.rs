//! **試験のときだけ効く口** (T22.6)。`test-hooks` の機能を立てたビルドにだけ入る。
//!
//! 既定のビルド (`cargo build --release`) にはこのファイルごと入らず、要求の経路に
//! 分岐は 1 つも増えない。立てるのはルートのパッケージの `[dev-dependencies]` だけなので、
//! 効くのは結合テストのビルド (`cargo test` / `cargo clippy --all-targets`) に限られる。
//!
//! 使い道は `tests/heavy_test.rs` の「`/snapshot` は同時に 1 本だけ」(T14.51): 旗を取った
//! 1 本目を**組み立ての中で止めて**おけば、残りが届く時刻を timing に頼らずに作れる
//! (負荷の下では 4 本が同時に届かず、20 回数え直しても揺れた。Phase 21 の全体チェック)。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// 止めているか (真のあいだ `/snapshot` は組み始めずに待つ)。
static HELD: Mutex<bool> = Mutex::new(false);
static RELEASED: Condvar = Condvar::new();
/// いま止まっている `/snapshot` の本数 (テストが「1 本目が旗を握った」を確かめるのに読む)。
static PAUSED: AtomicUsize = AtomicUsize::new(0);

/// 生きているあいだ `/snapshot` を組み立ての手前で止める番人。落ちると全部を起こす。
pub struct SnapshotHold {
    _private: (),
}

impl Drop for SnapshotHold {
    fn drop(&mut self) {
        *HELD.lock().unwrap_or_else(|e| e.into_inner()) = false;
        RELEASED.notify_all();
    }
}

/// 以後の `/snapshot` を、**重い口の旗を取ったあと・組み立ての前**で止める。
pub fn hold_snapshots() -> SnapshotHold {
    *HELD.lock().unwrap_or_else(|e| e.into_inner()) = true;
    SnapshotHold { _private: () }
}

/// いま止まっている `/snapshot` の本数。
pub fn paused_snapshots() -> usize {
    PAUSED.load(Ordering::Acquire)
}

/// `/snapshot` の枝の頭で呼ぶ。止めていなければ鍵 1 回で戻る。
pub(super) fn pause_snapshot() {
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    if !*held {
        return;
    }
    PAUSED.fetch_add(1, Ordering::AcqRel);
    while *held {
        held = RELEASED.wait(held).unwrap_or_else(|e| e.into_inner());
    }
    PAUSED.fetch_sub(1, Ordering::AcqRel);
}
