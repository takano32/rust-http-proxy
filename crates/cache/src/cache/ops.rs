//! `Cache` の読み書き操作: 参照 (L1 → L2、昇格)、ストリーミング保存、延命、無効化。

use crate::sync::LockExt;
use std::io::{BufReader, Read};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::entry::{Body, CachedResponse, head_len, read_head};
use super::format::Meta;
use super::key::{CacheKey, KeyMap, cache_key};
use super::quota::is_permanent;
use super::sink::{StoreOutcome, StoreSink};
use super::{Cache, CacheSource, now_epoch};
use crate::sysinfo;
use crate::{log_debug, log_info, log_warn};

/// プローブが予算で追い出した 1 回の様子 (T.32.7。`/status` の `cache.last_budget_eviction`)。
///
/// 10-31 02:34 の tokyo で、大きな転送 (32.6・375.5・253.8 MB) の間にメモリのキャッシュが
/// `evictions` 20 で 13.87 MB → 0 になり、1 分ごとの `mem_limit` は 189 → 126 → 53 → 164 MB と
/// 動いていたが、どの 5 秒で何が上限を下げたかは残っていなかった。追い出したその 5 秒の
/// 上限と、上限を決めた材料 (余白・cgroup の使用量・空き・活性なファイルの頁) を残す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetEviction {
    /// いつ (epoch 秒)
    pub at: u64,
    /// 追い出した件数 (メモリの層)
    pub entries: u64,
    /// 追い出す前と後のメモリの層の使用量 (バイト)
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// そのときの上限 (バイト)
    pub limit_bytes: u64,
    /// 上限を決めた材料 (プローブのその回の値。読めなければ `None`)
    pub keep_free_bytes: u64,
    pub cgroup_keep_free_bytes: u64,
    pub mem_available_bytes: Option<u64>,
    pub mem_active_file_bytes: Option<u64>,
    pub cgroup_usage_bytes: Option<u64>,
    pub cgroup_limit_bytes: Option<u64>,
}

impl BudgetEviction {
    /// `/status` の 1 要素 (鍵の並びは固定)。
    pub fn to_json(self) -> String {
        let opt = |v: Option<u64>| v.map_or_else(|| "null".to_string(), |x| x.to_string());
        format!(
            concat!(
                "{{\"at\":{},\"entries\":{},\"bytes_before\":{},\"bytes_after\":{},",
                "\"limit_bytes\":{},\"keep_free_bytes\":{},\"cgroup_keep_free_bytes\":{},",
                "\"mem_available_bytes\":{},\"mem_active_file_bytes\":{},",
                "\"cgroup_usage_bytes\":{},\"cgroup_limit_bytes\":{}}}"
            ),
            self.at,
            self.entries,
            self.bytes_before,
            self.bytes_after,
            self.limit_bytes,
            self.keep_free_bytes,
            self.cgroup_keep_free_bytes,
            opt(self.mem_available_bytes),
            opt(self.mem_active_file_bytes),
            opt(self.cgroup_usage_bytes),
            opt(self.cgroup_limit_bytes),
        )
    }
}

/// `peek` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeekInfo {
    pub memory: bool,
    pub disk: bool,
    /// メモリにあればその長さ、無ければディスク上のファイルサイズ
    pub size: u64,
    pub meta: Meta,
}

impl Cache {
    /// LRU や統計に触らずにエントリの有無と状態を見る。
    pub fn peek(&self, key: CacheKey) -> Option<PeekInfo> {
        let mem = self.mem.peek(key);
        let disk = self.disk.lookup_entry(key);
        match (mem, disk) {
            (Some((size, meta)), d) => Some(PeekInfo {
                memory: true,
                disk: d.is_some(),
                size,
                meta,
            }),
            (None, Some((size, meta))) => Some(PeekInfo {
                memory: false,
                disk: true,
                size,
                meta,
            }),
            (None, None) => None,
        }
    }

    /// プローブの追い出し (両層の上限まで)。メモリの層から追い出したら、その回の上限と
    /// 材料を [`BudgetEviction`] に残し、info のログに 1 行出す (T.32.7)。追い出した件数の合計を返す。
    pub(super) fn enforce_budget(&self) -> usize {
        let (before, _) = self.mem.usage();
        let limit = self.mem.capacity();
        let mem = self.mem.enforce();
        let disk = self.disk.enforce();
        if mem > 0 {
            let snap = self.snapshot();
            let m = snap.mem.as_ref();
            let rec = BudgetEviction {
                at: now_epoch(),
                entries: mem as u64,
                bytes_before: before,
                bytes_after: self.mem.usage().0,
                limit_bytes: limit,
                keep_free_bytes: snap.mem_keep_free,
                cgroup_keep_free_bytes: snap.cgroup_keep_free,
                mem_available_bytes: m.map(|m| m.available),
                mem_active_file_bytes: m.map(|m| m.active_file),
                cgroup_usage_bytes: m.and_then(|m| m.cgroup_usage()),
                cgroup_limit_bytes: m.and_then(|m| m.cgroup_limit()),
            };
            self.budget_evictions.fetch_add(1, Ordering::Relaxed);
            *self.last_budget_eviction.locked() = Some(rec);
            let mib = |v: u64| v as f64 / (1u64 << 20) as f64;
            let mib_opt =
                |v: Option<u64>| v.map_or_else(|| "?".to_string(), |x| format!("{:.1}", mib(x)));
            log_info!(
                None,
                "memory cache evicted {} entries for the budget: {:.1} -> {:.1} MiB (limit {:.1} MiB; keep free {:.1} / cgroup {:.1} MiB, available {} MiB, active file {} MiB, cgroup usage {} of {} MiB)",
                mem,
                mib(rec.bytes_before),
                mib(rec.bytes_after),
                mib(limit),
                mib(rec.keep_free_bytes),
                mib(rec.cgroup_keep_free_bytes),
                mib_opt(rec.mem_available_bytes),
                mib_opt(rec.mem_active_file_bytes),
                mib_opt(rec.cgroup_usage_bytes),
                mib_opt(rec.cgroup_limit_bytes),
            );
        }
        mem + disk
    }

    /// プローブが予算で追い出した回数 (メモリの層。T.32.7)。
    pub fn budget_evictions(&self) -> u64 {
        self.budget_evictions.load(Ordering::Relaxed)
    }

    /// 最後に予算で追い出した 1 回 (T.32.7)。
    pub fn last_budget_eviction(&self) -> Option<BudgetEviction> {
        *self.last_budget_eviction.locked()
    }

    /// 全エントリを消し、消した件数 (両層の合計) を返す。
    pub fn clear_all(&self) -> usize {
        let n = self.mem.clear() + self.disk.clear();
        self.variants.locked().clear();
        log_debug!(None, "cache CLEAR removed {} entries", n);
        n
    }

    /// キャッシュを引く。メモリ → ディスクの順に探索し、小さなディスクヒットはメモリへ昇格させる。
    /// 期限切れでもバリデータ付きなら返す (呼び出し側が `is_fresh` で判断して再検証する)。
    pub fn get(&self, key: CacheKey, conn_id: usize) -> Option<(CachedResponse, CacheSource)> {
        if !self.cfg.enabled {
            return None;
        }
        let now = now_epoch();

        if let Some(hit) = self.mem.get(key, now, self.tick()) {
            let offset = head_len(&hit.data);
            let resp = CachedResponse {
                head: hit.data[..offset].to_vec(),
                size: hit.data.len() as u64,
                body: Body::Memory {
                    data: hit.data,
                    offset,
                },
                meta: hit.meta,
            };
            self.hits_mem.fetch_add(1, Ordering::Relaxed);
            self.bytes_served.fetch_add(resp.size, Ordering::Relaxed);
            log_debug!(
                Some(conn_id),
                "cache L1 {} key={} size={}B age={}s",
                if resp.is_fresh(now) { "HIT" } else { "STALE" },
                key,
                resp.size,
                resp.age()
            );
            return Some((resp, CacheSource::Memory));
        }

        if let Some(resp) = self.get_from_disk(key, now, conn_id) {
            return Some((resp, CacheSource::Disk));
        }

        self.misses.fetch_add(1, Ordering::Relaxed);
        log_debug!(Some(conn_id), "cache MISS key={}", key);
        None
    }
    /// L2 を引き、小さければ L1 へ昇格させる。壊れた・再検証できない期限切れのエントリはここで消す。
    fn get_from_disk(&self, key: CacheKey, now: u64, conn_id: usize) -> Option<CachedResponse> {
        if !self.disk.is_ready() {
            return None;
        }
        let indexed = self.disk.lookup(key)?;
        if indexed.expires_at <= now && !indexed.validators {
            self.disk.remove(key);
            return None;
        }
        let hit = match self.disk.open(key) {
            Ok(Some(hit)) if hit.meta.expires_at > now || hit.meta.validators => hit,
            Ok(_) => {
                self.disk.remove(key);
                return None;
            }
            Err(e) => {
                // 壊れている・無い場合だけ消す。一時的な I/O エラー (fd 枯渇など) では残す
                if is_permanent(&e) {
                    log_warn!(Some(conn_id), "cache L2 read failed key={}: {}", key, e);
                    self.disk.remove(key);
                } else {
                    log_warn!(
                        Some(conn_id),
                        "cache L2 temporarily unreadable key={}: {}",
                        key,
                        e
                    );
                }
                return None;
            }
        };
        let meta = hit.meta;
        let size = hit.size;
        let mut reader = BufReader::with_capacity(256 * 1024, hit.file);
        let resp = if size <= self.cfg.mem_max_object_size {
            let mut data = Vec::with_capacity(size as usize);
            if let Err(e) = reader.read_to_end(&mut data) {
                log_warn!(Some(conn_id), "cache L2 read failed key={}: {}", key, e);
                if is_permanent(&e) {
                    self.disk.remove(key);
                }
                return None;
            }
            sysinfo::drop_page_cache(reader.get_ref());
            let data = Arc::new(data);
            let evicted = self.mem.insert(key, Arc::clone(&data), meta, self.tick());
            self.count_evictions(evicted);
            let offset = head_len(&data);
            CachedResponse {
                head: data[..offset].to_vec(),
                size,
                body: Body::Memory { data, offset },
                meta,
            }
        } else {
            let head = match read_head(&mut reader) {
                Ok(h) => h,
                Err(e) => {
                    log_warn!(Some(conn_id), "cache L2 read failed key={}: {}", key, e);
                    self.disk.remove(key);
                    return None;
                }
            };
            CachedResponse {
                head,
                size,
                body: Body::File(reader),
                meta,
            }
        };
        self.disk.touch(key, self.tick());
        self.hits_disk.fetch_add(1, Ordering::Relaxed);
        self.bytes_served.fetch_add(size, Ordering::Relaxed);
        log_debug!(
            Some(conn_id),
            "cache L2 {} key={} size={}B age={}s{}",
            if resp.is_fresh(now) { "HIT" } else { "STALE" },
            key,
            size,
            now.saturating_sub(meta.stored_at),
            if size <= self.cfg.mem_max_object_size {
                " (promoted to L1)"
            } else {
                " (streaming from disk)"
            }
        );
        Some(resp)
    }
    /// URL とバリアントの対応を覚えておく (無効化用)。
    ///
    /// **表に上限がある** (T.32.6)。0.30.4 までは保存を始めるたび (保存に至らなかった回も) に 1 行
    /// 足し、消すのは無効化と全消しだけだったので、名前の違う GET を撃たれると伸び続けた。
    /// - バリアントの無いキー (= URL そのもののキー) は覚えない ([`invalidate`](Self::invalidate)
    ///   はそれを必ず消すので、覚えても使わない)
    /// - 行の数が [`variants_limit`](Self::variants_limit) に届いたら、もう両層に無いキーを
    ///   捨てる (追い出し・期限切れ・保存に至らなかったもの)。残るのは両層に居るバリアントだけ
    ///   なので、表は両層の件数を越えない
    pub fn remember_variant(&self, url: &str, key: CacheKey) {
        let base = cache_key("GET", url);
        if key == base {
            return;
        }
        let full = {
            let mut v = self.variants.locked();
            let list = v.entry(base).or_default();
            if !list.contains(&key) {
                list.push(key);
            }
            v.len() >= self.variants_limit()
        };
        if full {
            self.prune_variants();
        }
    }

    /// バリアントの表の行の数 (T.32.6。試験と `/status` 向け)。
    pub fn variants_len(&self) -> usize {
        self.variants.locked().len()
    }

    /// 掃除を始める行の数: 両層の件数の 2 倍 (下限 4,096)。掃除のあとは両層の件数以下に
    /// なるので、次の掃除までに少なくとも同じだけ足される (1 回あたりの手間は均すと定数)。
    fn variants_limit(&self) -> usize {
        let live = self.mem.usage().1 + self.disk.usage().1;
        live.saturating_mul(2).max(4096)
    }

    /// もう両層に無いバリアントを表から捨てる (T.32.6)。層を引く間は表の鍵を持たない
    /// (取り出して調べ、その間に足された行と混ぜて戻す)。
    fn prune_variants(&self) {
        let taken = std::mem::take(&mut *self.variants.locked());
        let before = taken.len();
        let mut kept: KeyMap<Vec<CacheKey>> = KeyMap::default();
        for (base, mut list) in taken {
            list.retain(|k| self.mem.peek(*k).is_some() || self.disk.lookup(*k).is_some());
            if !list.is_empty() {
                kept.insert(base, list);
            }
        }
        let mut v = self.variants.locked();
        for (base, list) in v.drain() {
            let into = kept.entry(base).or_default();
            for k in list {
                if !into.contains(&k) {
                    into.push(k);
                }
            }
        }
        *v = kept;
        log_debug!(
            None,
            "cache variants pruned: {} -> {} urls",
            before,
            v.len()
        );
    }
    /// URL のすべてのバリアントを消す (unsafe メソッドへの成功応答時, RFC 9111 §4.4)。
    /// 再起動前に保存されたバリアントは把握できないので、ベストエフォート。
    pub fn invalidate(&self, url: &str, conn_id: usize) -> usize {
        let base = cache_key("GET", url);
        let mut keys = {
            let mut v = self.variants.locked();
            v.remove(&base).unwrap_or_default()
        };
        if !keys.contains(&base) {
            keys.push(base);
        }
        let mut removed = 0;
        for k in keys {
            let had = self.mem.remove(k) | self.disk.lookup(k).is_some();
            self.disk.remove(k);
            if had {
                removed += 1;
            }
        }
        if removed > 0 {
            log_debug!(
                Some(conn_id),
                "cache INVALIDATE url={} ({} variants)",
                url,
                removed
            );
        }
        removed
    }
    /// ストリーミング保存を開始する。`expected` はレスポンスの Content-Length (分かれば)、
    /// `age` は受信時点で既に経過している時間 (上流キャッシュの Age / Date から)。
    #[allow(clippy::too_many_arguments)]
    pub fn begin_store(
        &self,
        key: CacheKey,
        url: &str,
        ttl: Duration,
        age: u64,
        validators: bool,
        expected: Option<u64>,
        conn_id: usize,
    ) -> StoreSink<'_> {
        let now = now_epoch();
        let meta = Meta {
            stored_at: now.saturating_sub(age),
            expires_at: now.saturating_add(ttl.as_secs().saturating_sub(age)),
            validators,
        };
        self.remember_variant(url, key);
        let mem_buf =
            if self.cfg.enabled && expected.is_none_or(|e| e <= self.cfg.mem_max_object_size) {
                Some(Vec::with_capacity(
                    expected
                        .unwrap_or(64 * 1024)
                        .min(self.cfg.mem_max_object_size) as usize,
                ))
            } else {
                None
            };
        let disk = if self.cfg.enabled {
            match self.disk.begin(
                key,
                url,
                meta,
                self.tick(),
                expected,
                self.cfg.max_object_size,
            ) {
                Ok(w) => w,
                Err(e) => {
                    log_warn!(Some(conn_id), "cache L2 write failed key={}: {}", key, e);
                    None
                }
            }
        } else {
            None
        };
        StoreSink {
            cache: self,
            key,
            url: url.to_string(),
            meta,
            ttl,
            mem_buf,
            disk,
            total: 0,
            conn_id,
        }
    }
    /// 一括保存 (小さなレスポンス・テスト用)。
    pub fn put(&self, key: CacheKey, url: &str, bytes: Vec<u8>, ttl: Duration, conn_id: usize) {
        self.put_with(key, url, bytes, ttl, false, conn_id);
    }
    pub fn put_with(
        &self,
        key: CacheKey,
        url: &str,
        bytes: Vec<u8>,
        ttl: Duration,
        validators: bool,
        conn_id: usize,
    ) -> StoreOutcome {
        if !self.cfg.enabled {
            return StoreOutcome::default();
        }
        let mut sink = self.begin_store(
            key,
            url,
            ttl,
            0,
            validators,
            Some(bytes.len() as u64),
            conn_id,
        );
        sink.write(&bytes);
        sink.finish()
    }
    /// 再検証 (304) に成功したので有効期限を延ばし、経過時間 (Age) を `age` から数え直す。
    /// 戻り値は新しい期限。
    pub fn refresh(&self, key: CacheKey, ttl: Duration, age: u64, conn_id: usize) -> u64 {
        let now = now_epoch();
        let stored_at = now.saturating_sub(age);
        let expires_at = now.saturating_add(ttl.as_secs().saturating_sub(age));
        let in_mem = self.mem.refresh(key, stored_at, expires_at, self.tick());
        let on_disk = self.disk.refresh(key, stored_at, expires_at, self.tick());
        if in_mem || on_disk {
            self.revalidations.fetch_add(1, Ordering::Relaxed);
        }
        log_debug!(
            Some(conn_id),
            "cache REFRESH key={} ttl={}s (L1={} L2={})",
            key,
            ttl.as_secs(),
            in_mem,
            on_disk
        );
        expires_at
    }
    /// 再検証 (304) で届いたヘッダーで書き直した先頭 `head` に差し替えて置き直し、延命する
    /// (RFC 9111 §3.2・§4.3.4。T.28.2)。戻り値は新しい期限 ([`Cache::refresh`] と同じ)。
    ///
    /// [`Cache::refresh`] は期限 (メタ) だけを延ばしていたので、304 が `Cache-Control: max-age=600`
    /// を返しても、配る先頭は古いまま (`max-age=1`) で、下流のキャッシュとクライアントの鮮度の
    /// 計算を誤らせていた。**本文は 1 バイトも変えない** (先頭の後ろをそのまま写す)。
    ///
    /// メモリ (L1) にあれば `head + 本文` を組み直して入れ替え、ディスク (L2) にあれば本文を
    /// 写した新しいファイルに置き換える (書き込みは今までの保存と同じく一時ファイル → rename)。
    /// どちらの層にも、無い層へは足さない。ディスクの書き直しに失敗したら期限だけ延ばす。
    /// **命中・保存の数は動かさない** (数えるのは `revalidations` だけ。[`Cache::refresh`] と同じ)。
    #[allow(clippy::too_many_arguments)]
    pub fn refresh_with_head(
        &self,
        key: CacheKey,
        url: &str,
        head: &[u8],
        ttl: Duration,
        age: u64,
        conn_id: usize,
    ) -> u64 {
        let now = now_epoch();
        let stored_at = now.saturating_sub(age);
        let expires_at = now.saturating_add(ttl.as_secs().saturating_sub(age));
        let mut in_mem = false;
        if let Some(hit) = self.mem.get(key, now, self.tick()) {
            let offset = head_len(&hit.data);
            let mut wire = Vec::with_capacity(head.len() + hit.data.len() - offset);
            wire.extend_from_slice(head);
            wire.extend_from_slice(&hit.data[offset..]);
            let meta = Meta {
                stored_at,
                expires_at,
                validators: hit.meta.validators,
            };
            let evicted = self.mem.insert(key, Arc::new(wire), meta, self.tick());
            self.count_evictions(evicted);
            in_mem = true;
        }
        let on_disk = self.rewrite_disk_head(key, url, head, stored_at, expires_at, conn_id);
        if in_mem || on_disk {
            self.revalidations.fetch_add(1, Ordering::Relaxed);
        }
        log_debug!(
            Some(conn_id),
            "cache REFRESH key={} ttl={}s with updated headers (L1={} L2={})",
            key,
            ttl.as_secs(),
            in_mem,
            on_disk
        );
        expires_at
    }

    /// L2 のエントリを、先頭だけ `head` に替えたファイルで置き換える ([`Cache::refresh_with_head`])。
    /// 置き換えられなければ期限だけ延ばす。戻り値は L2 にエントリがあったか。
    fn rewrite_disk_head(
        &self,
        key: CacheKey,
        url: &str,
        head: &[u8],
        stored_at: u64,
        expires_at: u64,
        conn_id: usize,
    ) -> bool {
        let only_expiry = || self.disk.refresh(key, stored_at, expires_at, self.tick());
        if !self.disk.is_ready() {
            return false;
        }
        let hit = match self.disk.open(key) {
            Ok(Some(hit)) => hit,
            _ => return only_expiry(),
        };
        let meta = Meta {
            stored_at,
            expires_at,
            validators: hit.meta.validators,
        };
        let mut reader = BufReader::with_capacity(64 * 1024, hit.file);
        let Ok(old_head) = read_head(&mut reader) else {
            return only_expiry();
        };
        let body_len = hit.size.saturating_sub(old_head.len() as u64);
        let expected = (head.len() as u64).saturating_add(body_len);
        let mut writer = match self.disk.begin(
            key,
            url,
            meta,
            self.tick(),
            Some(expected),
            self.cfg.max_object_size,
        ) {
            Ok(Some(w)) => w,
            _ => return only_expiry(),
        };
        let copied = writer.write(head).and_then(|_| {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                writer.write(&buf[..n])?;
            }
        });
        sysinfo::drop_page_cache(reader.get_ref());
        if let Err(e) = copied {
            log_warn!(
                Some(conn_id),
                "cache L2 header rewrite failed key={}: {}",
                key,
                e
            );
            writer.abort();
            return only_expiry();
        }
        match writer.finish(self.tick()) {
            Ok(r) if r.stored => {
                self.count_evictions(r.evicted);
                true
            }
            Ok(r) => {
                self.count_evictions(r.evicted);
                only_expiry()
            }
            Err(e) => {
                log_warn!(
                    Some(conn_id),
                    "cache L2 header rewrite failed key={}: {}",
                    key,
                    e
                );
                only_expiry()
            }
        }
    }

    pub fn remove(&self, key: CacheKey) {
        self.mem.remove(key);
        self.disk.remove(key);
    }
}
