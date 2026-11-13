//! **重い口は同時に 1 本だけ**組む (T14.51): 旗 (プロセスに 1 つ)、番人、重い口の一覧。

use std::sync::atomic::{AtomicBool, Ordering};

use super::{has_flag, parse_query};

/// **重い口を組んでいる最中か** (T14.51)。旗はプロセスに 1 つだけ。
///
/// 認証なしの公開ポートでは `/snapshot` (4 MiB を組む。T14.4) を誰でも好きなだけ叩けるので、
/// 1 秒に 10 回引かれると CPU と鍵の時間をそれだけで食う。**同時に組むのは 1 本**にして、
/// 2 本目からは断る。走査に対する最小限の保護で、**認証ではない** (仕様 S25 の認証なしは守る。
/// 順に引けば誰でも全部取れる)。
static HEAVY_BUSY: AtomicBool = AtomicBool::new(false);

/// 旗を取れているあいだ生きる番人 (T14.51)。
///
/// **落ちるときに必ず旗を戻す**ので、組んでいる途中で `?` で抜けてもパニックしても
/// 旗が立ったままにならない (`AtomicBool` を手で戻すと、戻し忘れた経路が 1 本でもあると
/// 以後ずっと 503 になる)。
pub struct HeavyGuard {
    _private: (),
}

impl Drop for HeavyGuard {
    fn drop(&mut self) {
        HEAVY_BUSY.store(false, Ordering::Release);
    }
}

/// 重い口を組み始める。**空いていれば番人**、既に 1 本走っていれば `None` (T14.51)。
///
/// 待たない (待つと、断るより高くつく「4 MiB を組む行列」ができる)。
///
/// **`.is_ok().then_some(HeavyGuard { .. })` と書いてはいけない**: `then_some` は引数を
/// 先に組むので、**取れなかったときにも番人が 1 つ出来てすぐ落ち**、その `Drop` が
/// **他人の握っている旗を戻して**しまう (同時 4 本のうち 2 本が通る、で最初に踏んだ)。
pub fn begin_heavy() -> Option<HeavyGuard> {
    if HEAVY_BUSY
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        Some(HeavyGuard { _private: () })
    } else {
        None
    }
}

/// **重い口の一覧はここ 1 か所** (T14.51)。
///
/// 重いのは「大きい応答を組む」口で、`/status` `/healthz` `/metrics` のような軽い口は
/// 入れない (監視が 5 秒ごとに叩く口を断ると、断ったこと自体が事故になる)。
/// `/hosts` `/recent` `/history` は**大きく引いたときだけ**重い (既定のままなら軽い)。
/// 問い合わせを読むのはこの 3 つのパスに来たときだけなので、他の口は文字列の照合 1 回で抜ける。
pub(super) fn is_heavy(is_get: bool, path: &str, query: Option<&str>) -> bool {
    if !is_get {
        return false;
    }
    match path {
        // 17 部を 1 つの JSON に組む (T14.4)。無条件に重い
        "/snapshot" => true,
        // 待ちの段階・スレッドの CPU と状態・ロックの取り合い (T14.3)。
        // **`?summary=1` だけは軽い** (標本を 1 本も組まず 3 段に畳んだ数字だけ返す。T15.0)
        "/profile" => !has_flag(query, "summary"),
        // 1 相手を上の口から横断して読む (T14.36)
        "/explain" => true,
        // 既定の `limit` / `n` を越えて引いたときだけ (T14.32 の `/history?res=5&n=4320` は 1.9 MB)
        "/hosts" => num_over(query, "limit", 200),
        "/recent" => num_over(query, "n", 500),
        "/history" => num_over(query, "n", 720),
        _ => false,
    }
}

/// 問い合わせの `key=` が `limit` を越えているか (無い・読めない値は「越えていない」)。
fn num_over(query: Option<&str>, key: &str, limit: u64) -> bool {
    let Some(q) = query else {
        return false;
    };
    parse_query(q)
        .iter()
        .any(|(k, v)| k == key && v.parse::<u64>().is_ok_and(|n| n > limit))
}

#[cfg(test)]
mod tests {
    use super::{begin_heavy, is_heavy, num_over};

    /// 重い口の一覧 (T14.51): `/snapshot` `/explain` は無条件、`/profile` は `?summary=1` を除き、
    /// `/hosts` `/recent` `/history` は既定を越えて引いたときだけ。軽い口と GET 以外は重くない。
    #[test]
    fn only_the_listed_endpoints_are_heavy() {
        assert!(is_heavy(true, "/snapshot", None));
        assert!(is_heavy(true, "/explain", Some("host=a.test")));
        assert!(is_heavy(true, "/profile", None));
        assert!(is_heavy(true, "/profile", Some("res=5")));
        // `?summary=1` だけは軽い (T15.0)。`summary=0` は立っていないので重いまま
        assert!(!is_heavy(true, "/profile", Some("summary=1")));
        assert!(!is_heavy(true, "/profile", Some("res=5&summary=1")));
        assert!(is_heavy(true, "/profile", Some("summary=0")));
        // 既定の上限を越えて引いたときだけ
        assert!(!is_heavy(true, "/hosts", None));
        assert!(!is_heavy(true, "/hosts", Some("limit=200")));
        assert!(is_heavy(true, "/hosts", Some("limit=201")));
        assert!(!is_heavy(true, "/recent", Some("n=500")));
        assert!(is_heavy(true, "/recent", Some("n=501&client=10.0.0.1")));
        assert!(!is_heavy(true, "/history", Some("res=5&n=720")));
        assert!(is_heavy(true, "/history", Some("res=5&n=4320")));
        // 監視が叩く軽い口は入れない
        for path in [
            "/status",
            "/healthz",
            "/metrics",
            "/",
            "/dashboard",
            "/snapshots",
        ] {
            assert!(!is_heavy(true, path, Some("n=99999")), "{}", path);
        }
        // GET (HEAD) 以外は組まないので重くない
        assert!(!is_heavy(false, "/snapshot", None));
    }

    /// `key=` が上限を越えているか: 無い・読めない値は「越えていない」。
    #[test]
    fn num_over_ignores_missing_and_unreadable_values() {
        assert!(!num_over(None, "n", 500));
        assert!(!num_over(Some(""), "n", 500));
        assert!(!num_over(Some("n=500"), "n", 500));
        assert!(num_over(Some("n=501"), "n", 500));
        assert!(!num_over(Some("n=abc"), "n", 500));
        assert!(!num_over(Some("n=-1"), "n", 500));
        assert!(!num_over(Some("limit=9999"), "n", 500), "別の鍵は見ない");
        assert!(num_over(Some("a=1&n=9999&b=2"), "n", 500));
    }

    /// 旗は 1 本しか取れず、番人が落ちると戻る (T14.51)。
    #[test]
    fn the_flag_is_held_by_one_guard_and_returns_on_drop() {
        let first = begin_heavy().expect("旗は空いているはず");
        assert!(begin_heavy().is_none(), "2 本目は取れない");
        drop(first);
        let again = begin_heavy();
        assert!(again.is_some(), "落ちたら戻る");
        drop(again);
    }
}
