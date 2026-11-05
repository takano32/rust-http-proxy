//! 本文を運ぶ中継バッファ: 大きさの決まり ([`COPY_BUF_SIZE`] と 2 つの不変条件) と、プロセス共用のプール ([`CopyBuf`])。

use std::sync::Mutex;

use crate::sync::LockExt;

/// 本文を運ぶときの 1 回ぶんの大きさ。
///
/// 64 KiB より 32 KiB の方が速い (実測、大コア固定・3 回とも): 本文 64 KiB で
/// CPU/要求 93.5 → 85.9 us (-8%)、本文 1 MiB で 750.6 → 666.7 us (-11%)、
/// 主力の 1 KiB 経路は 47.6 → 46.5 us で退行なし、アイドル接続あたりのメモリも同じ。
/// キャッシュ階層に収まりやすいためと思われる。
pub(super) const COPY_BUF_SIZE: usize = 32 * 1024;

/// クライアントへ書くときのバッファ。**必ず [`COPY_BUF_SIZE`] より大きくすること。**
/// 同じ大きさだと std の BufWriter が「容量以上の write はバッファを空けてから直書き」
/// する経路に入り、ヘッダーだけが単独の sendto になる (実測: 本文 1 MiB で sendto が
/// 9 → 42.7 回/要求)。
pub(super) const CLIENT_WRITE_BUF: usize = 2 * COPY_BUF_SIZE;

/// オリジンから読むときのバッファ。**必ず [`COPY_BUF_SIZE`] 以下にすること。**
/// これより大きいと body::BodyReader の「要求が容量以上なら内部バッファを迂回して直読み」
/// が効かず、本文をまるごと余計に memcpy する (実測: 本文 1 MiB で user CPU +60%)。
pub(super) const ORIGIN_READ_BUF: usize = COPY_BUF_SIZE;

// 上の 2 つの不変条件をコンパイル時に固定する (定数を片方だけ触ったときに気付けるように)
const _: () = assert!(CLIENT_WRITE_BUF > COPY_BUF_SIZE);
const _: () = assert!(ORIGIN_READ_BUF <= COPY_BUF_SIZE);

/// 本文の中継バッファのプロセス共用プール。
///
/// スレッドローカルに置くと、アイドルな keep-alive 接続が「使っていない 64 KiB」を
/// スレッドごとに抱え続ける (実測: アイドル 1 接続あたり 109.6 kB のうち約 67 kB がこれ)。
/// 借りている間だけ持つ形にすると、同時に本文を運んでいる数ぶんしか要らない。
static COPY_POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// プールに置いておく上限。これを超えたぶんは返さずに解放する
/// (同時に本文を運ぶ数がこれを超えるのは一時的な山なので、抱え続けない)。
const COPY_POOL_MAX: usize = 64;

/// スレッドから借りた中継バッファ。Drop で返すので途中で return しても失わない。
pub(super) struct CopyBuf(Vec<u8>);

impl CopyBuf {
    pub(super) fn take() -> CopyBuf {
        let mut buf = COPY_POOL.locked().pop().unwrap_or_default();
        if buf.len() < COPY_BUF_SIZE {
            buf.resize(COPY_BUF_SIZE, 0);
        }
        CopyBuf(buf)
    }
}

impl std::ops::Deref for CopyBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::DerefMut for CopyBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

impl Drop for CopyBuf {
    fn drop(&mut self) {
        let mut pool = COPY_POOL.locked();
        if pool.len() < COPY_POOL_MAX {
            pool.push(std::mem::take(&mut self.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 借りたバッファは [`COPY_BUF_SIZE`] あり、返してもプールは上限を超えない
    /// (プールはプロセスで 1 つなので、他の試験と同時でも成り立つことだけを見る)。
    #[test]
    fn a_borrowed_buffer_is_full_sized_and_the_pool_is_bounded() {
        let a = CopyBuf::take();
        assert_eq!(a.len(), COPY_BUF_SIZE);
        drop(a);
        assert!(!COPY_POOL.locked().is_empty(), "返したぶんがプールにある");
        let many: Vec<CopyBuf> = (0..COPY_POOL_MAX + 8).map(|_| CopyBuf::take()).collect();
        assert!(many.iter().all(|b| b.len() == COPY_BUF_SIZE));
        drop(many);
        assert!(COPY_POOL.locked().len() <= COPY_POOL_MAX);
    }
}
