//! 同時ミスの合流 (collapsed forwarding)。
//!
//! 同じキーの取得が進行中なら、後から来た要求はオリジンへ行かずに先頭 (leader) の保存完了を
//! 待ち、保存された表現をキャッシュから受け取る。leader が保存できずに終わった (保存対象外・
//! 失敗・切断) ときは待っていた側が自分で取得する。leader のガードは Drop で必ず完了を通知する。

use crate::sync::LockExt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::key::{CacheKey, KeyMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    /// 保存されたのでキャッシュから取れる
    Stored,
    /// 保存されなかった (自分で取りに行く)
    NotStored,
}

#[derive(Default)]
pub struct InFlight {
    state: Mutex<Option<FetchOutcome>>,
    cv: Condvar,
}

impl InFlight {
    /// 完了を待つ。`timeout` 内に終わらなければ `None`。
    pub fn wait(&self, timeout: Duration) -> Option<FetchOutcome> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.locked();
        while state.is_none() {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = self
                .cv
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            state = guard;
        }
        *state
    }

    fn complete(&self, outcome: FetchOutcome) {
        let mut state = self.state.locked();
        if state.is_none() {
            *state = Some(outcome);
        }
        self.cv.notify_all();
    }
}

/// 進行中の取得の一覧。
#[derive(Default)]
pub struct InFlightTable {
    map: Mutex<KeyMap<Arc<InFlight>>>,
}

pub enum FetchTicket<'a> {
    Leader(LeaderGuard<'a>),
    Follower(Arc<InFlight>),
}

impl InFlightTable {
    /// leader になれれば `Leader`、既に誰かが取得中なら `Follower`。
    pub fn begin(&self, key: CacheKey) -> FetchTicket<'_> {
        let mut map = self.map.locked();
        if let Some(existing) = map.get(&key) {
            return FetchTicket::Follower(Arc::clone(existing));
        }
        let entry = Arc::new(InFlight::default());
        map.insert(key, Arc::clone(&entry));
        FetchTicket::Leader(LeaderGuard {
            table: self,
            key,
            entry,
            done: false,
        })
    }

    pub fn len(&self) -> usize {
        self.map.locked().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn finish(&self, key: CacheKey, entry: &InFlight, outcome: FetchOutcome) {
        {
            let mut map = self.map.locked();
            map.remove(&key);
        }
        entry.complete(outcome);
    }
}

/// leader の印。`complete` を呼ばずに落ちても Drop で `NotStored` を通知する。
pub struct LeaderGuard<'a> {
    table: &'a InFlightTable,
    key: CacheKey,
    entry: Arc<InFlight>,
    done: bool,
}

impl LeaderGuard<'_> {
    pub fn complete(mut self, outcome: FetchOutcome) {
        self.done = true;
        self.table.finish(self.key, &self.entry, outcome);
    }
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.table
                .finish(self.key, &self.entry, FetchOutcome::NotStored);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn key(n: u128) -> CacheKey {
        CacheKey(n)
    }

    #[test]
    fn followers_wait_for_the_leader() {
        let table = Arc::new(InFlightTable::default());
        let FetchTicket::Leader(leader) = table.begin(key(1)) else {
            panic!("first is the leader");
        };
        let FetchTicket::Follower(f) = table.begin(key(1)) else {
            panic!("second is a follower");
        };
        assert_eq!(table.len(), 1);
        let waiter = {
            let f = Arc::clone(&f);
            thread::spawn(move || f.wait(Duration::from_secs(5)))
        };
        thread::sleep(Duration::from_millis(50));
        leader.complete(FetchOutcome::Stored);
        assert_eq!(waiter.join().unwrap(), Some(FetchOutcome::Stored));
        assert!(table.is_empty());
        // 完了後に来た要求はまた leader になる
        assert!(matches!(table.begin(key(1)), FetchTicket::Leader(_)));
    }

    #[test]
    fn dropping_the_leader_releases_followers_with_not_stored() {
        let table = InFlightTable::default();
        let FetchTicket::Leader(leader) = table.begin(key(2)) else {
            panic!();
        };
        let FetchTicket::Follower(f) = table.begin(key(2)) else {
            panic!();
        };
        drop(leader);
        assert_eq!(
            f.wait(Duration::from_millis(100)),
            Some(FetchOutcome::NotStored)
        );
        assert!(table.is_empty());
    }

    #[test]
    fn waiting_times_out() {
        let table = InFlightTable::default();
        let FetchTicket::Leader(_leader) = table.begin(key(3)) else {
            panic!();
        };
        let FetchTicket::Follower(f) = table.begin(key(3)) else {
            panic!();
        };
        let started = Instant::now();
        assert_eq!(f.wait(Duration::from_millis(60)), None);
        assert!(started.elapsed() >= Duration::from_millis(60));
    }
}
