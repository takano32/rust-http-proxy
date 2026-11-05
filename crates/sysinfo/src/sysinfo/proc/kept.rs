//! 小さな `/proc`・`/sys` のファイルの読み方 (`read_small*`) と、決まったパスを開いたままにして
//! `pread` 1 回で読む `KeptFiles` (T32.21)。T34.61 で `proc.rs` から移した (本体は変えていない)。

/// 小さな `/proc` / `/sys` のファイルを読む。
///
/// (T29.20) `fs::read_to_string` は大きさを当てにするために `statx` と `lseek` を呼び、procfs は大きさ 0 を
/// 返すので 32 B から倍々に読み直す (`/proc/self/status` で `read` 8 回 = 1 ファイル 11 回の
/// システムコール)。1 秒ごとに読む所 (`cache-probe`・`profile-sample`・`history`) はこちらを使う。
/// `buf` は使い回し (毎回確保しない)。読めなければ `None`。
///
/// **`/proc`・`/sys` の決まったパスは開いたままにして `pread` 1 回で読む** (T32.21。[`KeptFiles`])。
/// open・read・read (EOF)・close の 4 回が 1 回になり、procfs の open (パスの引き直しと
/// seq_file の用意) も払わない (`/proc/self/status` 7.1 → 3.0 us)。スレッドごとのファイル
/// (`/proc/self/task/<tid>/…`・`/proc/thread-self/…`) は開いたままにしない ([`keepable`])。
pub fn read_small_into<'a>(path: &std::path::Path, buf: &'a mut Vec<u8>) -> Option<&'a str> {
    if keepable(path) {
        return KEPT.read(path, buf);
    }
    read_by_open(path, buf)
}

/// 開いて読み、閉じる (open・read・read (EOF)・close の 4 回)。
fn read_by_open<'a>(path: &std::path::Path, buf: &'a mut Vec<u8>) -> Option<&'a str> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path).ok()?;
    if buf.len() < 4096 {
        buf.resize(4096, 0);
    }
    let mut n = 0usize;
    loop {
        if n == buf.len() {
            let more = buf.len();
            buf.resize(more * 2, 0);
        }
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    std::str::from_utf8(&buf[..n]).ok()
}

/// [`read_small_into`] の、持ち帰る版 (`fs::read_to_string(path).ok()` の置き換え)。
///
/// 読む器はスレッドごとに使い回す (T32.21。呼ぶたびに 4 KiB を確保してゼロで埋めていた)。
/// 返すのは中身の長さぶんの `String` だけ。
pub fn read_small(path: &std::path::Path) -> Option<String> {
    thread_local! {
        static BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    BUF.try_with(|b| {
        let mut b = b.try_borrow_mut().ok()?;
        read_small_into(path, &mut b).map(str::to_owned)
    })
    .ok()
    .flatten()
}

/// 開いたままにしてよいパスか (T32.21)。
///
/// `/proc`・`/sys` の下で、**どのスレッドが開いても同じものを指す**パスだけ。
/// `/proc/thread-self` は開いたスレッドを、`/proc/self/task/<tid>` はスレッドを 1 本ずつ指す
/// (スレッドの数だけ記述子が要り、死んだスレッドの分が残る) ので外す。
pub(crate) fn keepable(path: &std::path::Path) -> bool {
    let s = path.as_os_str().as_encoded_bytes();
    (s.starts_with(b"/proc/") || s.starts_with(b"/sys/"))
        && !s.starts_with(b"/proc/thread-self")
        && !s.windows(6).any(|w| w == b"/task/")
}

/// 開いたままにする数の上限 (T32.21)。いまの読み手が使うのは 30 本ほどで、
/// cgroup の階層が深くても 64 に届かない。越えたぶんは今までどおり開いて読む。
const KEPT_MAX: usize = 64;
/// 開いたままのファイルをこの回数読んだら 1 度開き直す (T32.21)。
///
/// 消えた cgroup のファイルは `ENODEV` を返すので開き直せるが、**消えて同じパスに
/// 作り直された**ふつうのファイルは古い中身を返し続ける。ときどき開き直して追いつく
/// (5 秒ごとの読みなら 5 分 20 秒に 1 回、1 秒ごとでも 64 秒に 1 回)。
pub(super) const KEPT_REOPEN_EVERY: u32 = 64;

enum Kept {
    /// 開いている記述子と、開いてから読んだ回数
    Open(std::sync::Arc<std::fs::File>, u32),
    /// 一度は読めたが、いまは開けない (毎回開き直しを試す)
    Gone,
    /// 初めて開こうとしたら無かった (cgroup v1 の `*.pressure` など)。次の起動まで開き直さない
    Missing,
}

/// 開いたままの小さなファイルの置き場 (T32.21)。プロセスに 1 つ ([`KEPT`]) で、試験は自前で作る。
///
/// - 読むのは `pread(fd, buf, 0)` で、1 頁に収まるファイルは 1 回 (短く返ったら EOF とみなす。
///   procfs・sysfs の seq_file は器が埋まるか終わりまで詰めて返す)。4 KiB を越えれば器を倍にして続きを読む
/// - 読めなくなったら (`ENODEV`・`ESRCH` など) その場で開き直して 1 回だけ読み直す
/// - **初めて開こうとして無かった**パスは覚えておき、次の起動まで開き直さない。
///   一度でも読めたパスは、消えても毎回開き直しを試す (消えて現れるファイルを取りこぼさない)
pub struct KeptFiles {
    map: std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, Kept>>,
}

static KEPT: std::sync::LazyLock<KeptFiles> = std::sync::LazyLock::new(KeptFiles::new);

impl Default for KeptFiles {
    fn default() -> Self {
        Self::new()
    }
}

impl KeptFiles {
    pub fn new() -> Self {
        Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 開いたままにしている数 (試験だけが使う。T34.71 で `#[cfg(test)]` にした)。
    #[cfg(test)]
    pub(crate) fn open_count(&self) -> usize {
        self.lock()
            .values()
            .filter(|k| matches!(k, Kept::Open(..)))
            .count()
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<std::path::PathBuf, Kept>> {
        // 中で panic する処理は無い (毒が付いても中身は壊れていない)
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `path` を `buf` に読む。読めなければ `None`。
    pub fn read<'a>(&self, path: &std::path::Path, buf: &'a mut Vec<u8>) -> Option<&'a str> {
        // 鍵の内では記述子を写すだけ (読むのは鍵の外)
        let (kept, seen) = {
            let mut map = self.lock();
            match map.get_mut(path) {
                Some(Kept::Missing) => return None,
                Some(Kept::Gone) => (None, true),
                Some(Kept::Open(f, reads)) => {
                    *reads += 1;
                    if *reads >= KEPT_REOPEN_EVERY {
                        (None, true)
                    } else {
                        (Some(f.clone()), true)
                    }
                }
                None => (None, false),
            }
        };
        // 読めなくなった (消えた cgroup は ENODEV) ときは下で開き直す
        if let Some(f) = kept
            && let Some(n) = pread_all(&f, buf)
        {
            return std::str::from_utf8(&buf[..n]).ok();
        }
        let opened = std::fs::File::open(path);
        let read = match &opened {
            Ok(f) => pread_all(f, buf),
            Err(_) => None,
        };
        let state = match (opened, read) {
            (Ok(f), Some(_)) => Kept::Open(std::sync::Arc::new(f), 0),
            (Err(e), _) if e.kind() == std::io::ErrorKind::NotFound && !seen => Kept::Missing,
            _ => Kept::Gone,
        };
        let mut map = self.lock();
        if map.len() < KEPT_MAX || map.contains_key(path) {
            map.insert(path.to_path_buf(), state);
        }
        drop(map);
        std::str::from_utf8(&buf[..read?]).ok()
    }
}

/// 先頭から `pread` で読む。短く返ったら終わり (読み足りなければ器を倍にして続ける)。
pub(super) fn pread_all(f: &std::fs::File, buf: &mut Vec<u8>) -> Option<usize> {
    use std::os::unix::fs::FileExt as _;
    if buf.len() < 4096 {
        buf.resize(4096, 0);
    }
    let mut n = 0usize;
    loop {
        if n == buf.len() {
            let more = buf.len();
            buf.resize(more * 2, 0);
        }
        match f.read_at(&mut buf[n..], n as u64) {
            Ok(0) => return Some(n),
            Ok(k) => {
                n += k;
                if n < buf.len() {
                    return Some(n);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}
