//! `/config` と `--check` が並べる**効いている値**の一覧 ([`Config::settings`]) と、
//! `.env` に書き戻せる形への書き戻し。値の形 (JSON の値・鍵の順) はここだけで決まる。

use std::path::PathBuf;
use std::time::Duration;

use super::{Config, Setting, TcpKeepalive};
use crate::cache::config::DEFAULT_TARGET_PERCENT;
use crate::cache::{DiskQuota, Limit, MIB};

impl Config {
    /// `PROXY_TCP_KEEPALIVE` の効いている値を `.env` に書き戻せる形で返す (T14.52)。
    pub fn tcp_keepalive_spec(&self) -> String {
        TcpKeepalive::spec(self.tcp_keepalive)
    }

    /// `PROXY_PEEK_SNI` の効いている値 (`off` / `on` / `on:<port>`。T14.38)。
    ///
    /// `on` は 443 だけ、`on:<port>` は 443 に加えてそのポートでも覗く。
    pub fn peek_sni_spec(&self) -> String {
        match self.peek_sni {
            None => "off".to_string(),
            Some(super::DEFAULT_PEEK_SNI_PORT) => "on".to_string(),
            Some(p) => format!("on:{}", p),
        }
    }

    /// 全 `PROXY_*` / `SERVER_*` の**効いている値**と、その出どころ (`/config` と `--check`。T14.15)。
    ///
    /// 値はいまプロキシが使っているもの (既定・`.env`・環境変数・引数のどれから来たかは
    /// `source`)。**このプロキシに秘密の設定は無い**ので、そのまま出してよい
    /// (`PROXY_TLS_CA_FILE` はパスだけで、証明書の中身は読まない)。
    /// 並べる順は「待ち受け → 接続 → 名前解決 → 遮断 → TLS → 記録 → キャッシュ」で、
    /// README の表と同じ流れにしてある。
    pub fn settings(&self) -> Vec<Setting> {
        let c = &self.cache;
        let mut out: Vec<Setting> = Vec::with_capacity(64);
        let mut add = |key: &'static str, value: String| {
            out.push(Setting {
                key,
                value,
                source: self.sources.get(key),
            })
        };
        let mib = |bytes: u64| (bytes / MIB).to_string();
        let secs = |d: Duration| d.as_secs().to_string();
        let path = |p: Option<&PathBuf>| {
            crate::json::quote_opt(p.map(|p| p.display().to_string()).as_deref())
        };
        // 待ち受け
        add("SERVER_PORT", self.port.to_string());
        add(
            "PROXY_BIND",
            list_value(
                &self
                    .bind_addrs
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>(),
            ),
        );
        add("PROXY_IPV6", self.ipv6.to_string());
        // backlog は実効値 (`0` を書かれても、ここには決まった数が出る。T14.47)
        add("PROXY_LISTEN_BACKLOG", self.listen_backlog.to_string());
        // 接続と上限
        add("PROXY_TIMEOUT_SECS", secs(self.timeout));
        // 実効値 (未設定なら既定の 3 秒と `PROXY_TIMEOUT_SECS` の小さい方が出る。T15.6、T.28.43)
        add("PROXY_CONNECT_TIMEOUT_SECS", secs(self.connect_timeout));
        // 実効値 (未設定なら既定の 10 秒と `PROXY_TIMEOUT_SECS` の小さい方。T.28.0)
        add("PROXY_HEADER_TIMEOUT_SECS", secs(self.header_timeout));
        add(
            "PROXY_HE_STAGGER_MS",
            self.he_stagger.as_millis().to_string(),
        );
        add("PROXY_KEEPALIVE_SECS", secs(self.keepalive));
        add("PROXY_TUNNEL_IDLE_SECS", secs(self.tunnel_idle));
        add("PROXY_MIN_TRANSFER_BPS", self.min_transfer.bps.to_string());
        add(
            "PROXY_MIN_TRANSFER_WINDOW_SECS",
            secs(self.min_transfer.window),
        );
        add("PROXY_MAX_CONNS", self.max_conns.to_string());
        add(
            "PROXY_MAX_CONNS_PER_CLIENT",
            self.max_conns_per_client.to_string(),
        );
        add("PROXY_MAX_THREADS", self.max_threads.to_string());
        add("PROXY_ORIGIN_POOL", self.pool_per_host.to_string());
        add("PROXY_ORIGIN_POOL_TOTAL", self.pool_total.to_string());
        add("PROXY_PARK_IDLE", self.park_idle.to_string());
        add(
            "PROXY_PARK_GRACE_MS",
            self.park_grace.as_millis().to_string(),
        );
        add("PROXY_MALLOC_ARENAS", self.malloc_arenas.to_string());
        add(
            "PROXY_MALLOC_MMAP_THRESHOLD_KB",
            self.malloc_mmap_threshold_kb.to_string(),
        );
        // 名前解決
        add("PROXY_DNS_TTL_SECS", secs(self.dns_ttl));
        add("PROXY_DNS_NEGATIVE_SECS", secs(self.dns_negative));
        add("PROXY_DNS_WARM_SECS", secs(self.dns_warm));
        add("PROXY_DNS_STALE_SECS", secs(self.dns_stale));
        add("PROXY_DNS_TIMEOUT_SECS", secs(self.dns_timeout));
        // canary (T14.10 / T14.37)。宛先の解釈は `proxy-metrics` の仕事だが、
        // `/config` と `--check` は「効いている設定を全部見る」ための口なので、
        // 文字列のまま 3 つとも出す (T14.57 の再デプロイの手引きで穴が見つかった)
        add("PROXY_CANARY", crate::json::quote(&self.canary));
        add("PROXY_CANARY_SECS", secs(self.canary_secs));
        add("PROXY_CANARY_IPV6", self.canary_ipv6.to_string());
        // あて先の許可・拒否
        add("PROXY_ALLOW_HOSTS", list_value(&self.acl.allow_hosts));
        add("PROXY_DENY_HOSTS", list_value(&self.acl.deny_hosts));
        add(
            "PROXY_CONNECT_PORTS",
            crate::json::quote(&self.connect_ports.spec()),
        );
        // `on` / `off` は真偽、`private` だけ文字列 (T.28.41)
        add("PROXY_ALLOW_LOCAL", self.allow_local.json().to_string());
        // 公開プロキシの止め方 (T14.18) と山の写真の閾 (T14.6)。マージのときに親が足した
        add(
            "PROXY_ENDPOINTS_READONLY",
            self.endpoints_readonly.to_string(),
        );
        add(
            "PROXY_ALLOW_CLIENTS",
            crate::json::quote(&self.allow_clients.to_string()),
        );
        // 追跡する接続元 (T14.27。空 = 追跡していない)
        add(
            "PROXY_TRACE_CLIENT",
            crate::json::quote(
                &self
                    .trace_client
                    .map(|ip| ip.to_string())
                    .unwrap_or_default(),
            ),
        );
        // CONNECT の最初のバイトから SNI を覗くか (T14.38。`off` / `on` / `on:<port>`)
        add("PROXY_PEEK_SNI", crate::json::quote(&self.peek_sni_spec()));
        add("PROXY_BURST_PERCENT", self.burst_percent.to_string());
        add("PROXY_BLOCKLIST_FILE", path(self.blocklist_file.as_ref()));
        add(
            "PROXY_BLOCKLIST_URL",
            crate::json::quote_opt(self.blocklist_url.as_deref()),
        );
        add("PROXY_BLOCKLIST_REFRESH_SECS", secs(self.blocklist_refresh));
        add("PROXY_BLOCKLIST_EXEMPT", list_value(&self.blocklist_exempt));
        add("PROXY_PAC_DIRECT", list_value(&self.pac_direct));
        // TLS (パスだけ。証明書の中身は読まない)
        add("PROXY_TLS", self.tls_enabled.to_string());
        add("PROXY_TLS_VERIFY", self.tls_verify.to_string());
        add("PROXY_TLS_CA_FILE", path(self.tls_ca_file.as_ref()));
        // 消えたクライアントの検知 (T14.52。`off` / `on:<idle>:<intvl>:<cnt>`)
        add(
            "PROXY_TCP_KEEPALIVE",
            crate::json::quote(&self.tcp_keepalive_spec()),
        );
        // 記録とプロファイル
        add("PROXY_RECORDS", crate::json::quote(self.records.name()));
        add("PROXY_STATS_PERSIST", self.stats_persist.to_string());
        add("PROXY_SNAPSHOT_DAYS", self.snapshot_days.to_string());
        add("PROXY_SLO", crate::json::quote(&self.slo.spec()));
        add(
            "PROXY_PROFILE",
            crate::json::quote_opt(self.lite.then_some("lite")),
        );
        add(
            "PROXY_PROFILE_SAMPLE_MS",
            self.profile_sample_ms.to_string(),
        );
        add("PROXY_SELF_BENCH", self.self_bench.to_string());
        // `.env` にそのまま書き戻せる形で出す (読むのは大小を問わない)
        add(
            "PROXY_LOG_LEVEL",
            crate::json::quote(
                &crate::log::current_level()
                    .as_str()
                    .trim()
                    .to_ascii_lowercase(),
            ),
        );
        // キャッシュ (値は `CacheConfig` が持っているもの)
        add("PROXY_CACHE_ENABLED", c.enabled.to_string());
        add(
            "PROXY_CACHE_DIR",
            crate::json::quote(&c.dir.display().to_string()),
        );
        add("PROXY_MEM_CACHE_MB", limit_value(c.mem_limit));
        add("PROXY_DISK_CACHE_MB", limit_value(c.disk_limit));
        add(
            "PROXY_MEM_TARGET_PERCENT",
            c.mem_limit
                .target_percent()
                .unwrap_or(DEFAULT_TARGET_PERCENT)
                .to_string(),
        );
        add(
            "PROXY_DISK_TARGET_PERCENT",
            c.disk_limit
                .target_percent()
                .unwrap_or(DEFAULT_TARGET_PERCENT)
                .to_string(),
        );
        add("PROXY_MEM_KEEP_FREE_MB", mib(c.mem_keep_free));
        add("PROXY_DISK_KEEP_FREE_MB", mib(c.disk_keep_free));
        add(
            "PROXY_CACHE_RESERVE",
            crate::json::quote(&c.reserve.to_string()),
        );
        add("PROXY_CACHE_PROBE_SECS", secs(c.probe_interval));
        add("PROXY_CACHE_TTL_SECS", secs(c.default_ttl));
        add(
            "PROXY_CACHE_HEURISTIC_PERCENT",
            c.heuristic_percent.to_string(),
        );
        add("PROXY_CACHE_HEURISTIC_MAX_SECS", secs(c.heuristic_max));
        add("PROXY_CACHE_MAX_STALE_SECS", secs(c.max_stale));
        add("PROXY_CACHE_GRACE_SECS", secs(c.grace));
        add("PROXY_STALE_WAIT_SECS", secs(c.stale_wait));
        add("PROXY_CACHE_MAX_OBJECT_MB", mib(c.max_object_size));
        add("PROXY_MEM_CACHE_MAX_OBJECT_MB", mib(c.mem_max_object_size));
        add("PROXY_DISK_MAX_ENTRIES", c.disk_max_entries.to_string());
        add("PROXY_CACHE_ADMISSION", c.admission.to_string());
        add("PROXY_NEGATIVE_TTL_SECS", secs(c.negative_ttl));
        // ディスク割当 (`SERVER_DISK` は `PROXY_DISK_QUOTA_MB` の別名。効いた方にだけ
        // `source` が付き、値はどちらの行にも同じ「効いている割当」が出る)
        add("PROXY_DISK_QUOTA_MB", quota_value(c.disk_quota));
        add("SERVER_DISK", quota_value(c.disk_quota));
        add("PROXY_DISK_QUOTA_ROOT", path(c.quota_root.as_ref()));
        add("PROXY_DISK_PROBE", c.disk_probe.to_string());
        add(
            "SERVER_MEMORY",
            c.mem_alloc
                .map(|b| (b / MIB).to_string())
                .unwrap_or_else(|| "null".to_string()),
        );
        out
    }
}

/// 一覧 1 つを書くのに使ってよいバイト数。
const LIST_CAP: usize = 1024;

/// 一覧の値 (`PROXY_PAC_DIRECT` など)。**[`LIST_CAP`] バイトで切り**、切ったときは
/// 最後の要素に `"+N more"` を入れる (JSON としては正しいまま)。
///
/// 一覧は書こうと思えばいくらでも長くできる (除外 1,000 件など) ので、`/config` が
/// 1 つの設定で埋まらないようにここで止める。全部を見たいときは `.env` そのものを読む
/// (`/config` の `env_file` にパスが出ている)。
fn list_value(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        let quoted = crate::json::quote(item);
        if out.len() + quoted.len() + 24 > LIST_CAP {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"+{} more\"]", items.len() - i));
            return out;
        }
        if i > 0 {
            out.push(',');
        }
        out.push_str(&quoted);
    }
    out.push(']');
    out
}

/// 上限を書いたときと同じ形に戻す (`auto` / `auto:85` / MiB 数)。
fn limit_value(limit: Limit) -> String {
    match limit {
        Limit::Auto {
            percent: DEFAULT_TARGET_PERCENT,
        } => "\"auto\"".to_string(),
        Limit::Auto { percent } => format!("\"auto:{}\"", percent),
        Limit::Fixed(bytes) => (bytes / MIB).to_string(),
    }
}

/// ディスク割当を書いたときと同じ形に戻す (分からなければ `null`、無制限は `0`)。
fn quota_value(quota: DiskQuota) -> String {
    match quota {
        DiskQuota::Unknown => "null".to_string(),
        DiskQuota::Unlimited => "0".to_string(),
        DiskQuota::Fixed(bytes) => (bytes / MIB).to_string(),
        DiskQuota::Auto => "\"auto\"".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_lists_are_cut_with_a_marker() {
        let many: Vec<String> = (0..100)
            .map(|i| format!("host-{}.example.net", i))
            .collect();
        let out = list_value(&many);
        assert!(out.len() <= LIST_CAP, "{} B", out.len());
        assert!(out.ends_with("more\"]"), "{}", out);
        assert!(out.starts_with("[\"host-0.example.net\","), "{}", out);
        // 短い一覧はそのまま
        assert_eq!(
            list_value(&["a".to_string(), "b".to_string()]),
            "[\"a\",\"b\"]"
        );
        assert_eq!(list_value(&[]), "[]");
    }

    /// 上限と割当は書いたときと同じ形に戻る (`auto` は既定の割合のときだけ素の `auto`)。
    #[test]
    fn limits_and_quotas_are_written_back_as_they_were_read() {
        assert_eq!(
            limit_value(Limit::Auto {
                percent: DEFAULT_TARGET_PERCENT
            }),
            "\"auto\""
        );
        assert_eq!(limit_value(Limit::Auto { percent: 85 }), "\"auto:85\"");
        assert_eq!(limit_value(Limit::Fixed(512 * MIB)), "512");
        assert_eq!(quota_value(DiskQuota::Unknown), "null");
        assert_eq!(quota_value(DiskQuota::Unlimited), "0");
        assert_eq!(quota_value(DiskQuota::Fixed(10 * 1024 * MIB)), "10240");
        assert_eq!(quota_value(DiskQuota::Auto), "\"auto\"");
    }
}
