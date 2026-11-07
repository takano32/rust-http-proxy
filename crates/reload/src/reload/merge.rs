//! 再読込で当てる項目 (`merge_live`) と、起動時に固定されるので再起動が要る項目
//! (`restart_required`) の判断。T34.58 で `Live::reload` (286 行) から出した。
//!
//! `merge_live` の副作用 (DNS の表・canary・ブロックリスト・ログの水準などへ当てる) は、
//! 割る前と同じ順のまま中に残してある。`restart_required` は 2 つの `Config` を比べるだけ。

use crate::config::Config;
use crate::{envfile, log};

/// `old` (いま効いている設定) に `fresh` (読み直した `.env`) のうち即時反映できる項目を当てた
/// 設定と、当てた項目の名前 (当てた順) を返す。当てた項目は出どころも写す。
pub(super) fn merge_live(old: &Config, fresh: &Config) -> (Config, Vec<&'static str>) {
    let mut next = old.clone();
    let mut applied = Vec::new();
    // 当てた値は**出どころも差し替える** (`/config` の `source` が追随する。T14.15)。
    // 起動時に固定される項目 (下の `restart` の側) は写さない: `.env` に書いても
    // 効いていないので、起動時の出どころのままにしておく
    merge_access(&mut next, old, fresh, &mut applied);
    merge_connections(&mut next, old, fresh, &mut applied);
    merge_dns(&mut next, old, fresh, &mut applied);
    merge_rest(&mut next, old, fresh, &mut applied);
    // 読めなかった値と知らない名前は**いまの `.env` のもの**に差し替える (T28.6。
    // 直した行は消え、新しく書き損じた行が出る)。値そのものは上で当てた既定のまま
    next.problems = fresh.problems.clone();
    (next, applied)
}

/// 宛て先と接続元の許可、締め切り、`/` の書き込みの可否 (`merge_live` の 1 段目)。
fn merge_access(next: &mut Config, old: &Config, fresh: &Config, applied: &mut Vec<&'static str>) {
    if fresh.acl != old.acl {
        next.acl = fresh.acl.clone();
        next.sources.adopt(&fresh.sources, "PROXY_ALLOW_HOSTS");
        next.sources.adopt(&fresh.sources, "PROXY_DENY_HOSTS");
        applied.push("PROXY_ALLOW_HOSTS/PROXY_DENY_HOSTS");
    }
    if fresh.timeout != old.timeout {
        next.timeout = fresh.timeout;
        next.sources.adopt(&fresh.sources, "PROXY_TIMEOUT_SECS");
        applied.push("PROXY_TIMEOUT_SECS");
    }
    // CONNECT のオリジン接続だけの締め切り (T15.6)。`fresh` は `Config::from_env` =
    // `proxy_config::connect_timeout_for` を通った実効値なので、未設定のまま
    // `PROXY_TIMEOUT_SECS` だけが 10 秒を跨いで動いた (例: 30 → 5) ときの計算し直しも
    // ここに入る。そのとき `applied` に 2 行出るが、効く値が 2 つとも動いたのだから正しい
    if fresh.connect_timeout != old.connect_timeout {
        next.connect_timeout = fresh.connect_timeout;
        next.sources
            .adopt(&fresh.sources, "PROXY_CONNECT_TIMEOUT_SECS");
        applied.push("PROXY_CONNECT_TIMEOUT_SECS");
    }
    // 要求行とヘッダーの締め切り (T28.0)。`connect_timeout` と同じく実効値で比べる
    // (未設定のまま `PROXY_TIMEOUT_SECS` が 10 秒を跨いで動いたときもここに入る)。
    // 待ち受けに継承させた読み取りタイムアウトは、次の accept で食い違いを見て当て直す
    if fresh.header_timeout != old.header_timeout {
        next.header_timeout = fresh.header_timeout;
        next.sources
            .adopt(&fresh.sources, "PROXY_HEADER_TIMEOUT_SECS");
        applied.push("PROXY_HEADER_TIMEOUT_SECS");
    }
    if fresh.keepalive != old.keepalive {
        next.keepalive = fresh.keepalive;
        next.sources.adopt(&fresh.sources, "PROXY_KEEPALIVE_SECS");
        applied.push("PROXY_KEEPALIVE_SECS");
    }
    if fresh.connect_ports != old.connect_ports {
        next.connect_ports = fresh.connect_ports.clone();
        next.sources.adopt(&fresh.sources, "PROXY_CONNECT_PORTS");
        applied.push("PROXY_CONNECT_PORTS");
    }
    if fresh.allow_local != old.allow_local {
        next.allow_local = fresh.allow_local;
        next.sources.adopt(&fresh.sources, "PROXY_ALLOW_LOCAL");
        applied.push("PROXY_ALLOW_LOCAL");
    }
    if fresh.endpoints_readonly != old.endpoints_readonly {
        next.endpoints_readonly = fresh.endpoints_readonly;
        // 出どころも写す (T34.151。下の 2 つも。前は写しておらず `/config` の `source` が `default` のままだった)
        next.sources
            .adopt(&fresh.sources, "PROXY_ENDPOINTS_READONLY");
        applied.push("PROXY_ENDPOINTS_READONLY");
    }
    // 接続元の ACL は `serve` が accept ごとに引き直すので、次に来る接続から効く
    if fresh.allow_clients != old.allow_clients {
        next.allow_clients = fresh.allow_clients.clone();
        next.sources.adopt(&fresh.sources, "PROXY_ALLOW_CLIENTS");
        applied.push("PROXY_ALLOW_CLIENTS");
    }
    // 接続元ごとの上限も `serve` が accept ごとに引き直す (次に来る接続から効く)
    if fresh.max_conns_per_client != old.max_conns_per_client {
        next.max_conns_per_client = fresh.max_conns_per_client;
        next.sources
            .adopt(&fresh.sources, "PROXY_MAX_CONNS_PER_CLIENT");
        applied.push("PROXY_MAX_CONNS_PER_CLIENT");
    }
}

/// 記録・接続ごとの見張り・上限 (`merge_live` の 2 段目)。
fn merge_connections(
    next: &mut Config,
    old: &Config,
    fresh: &Config,
    applied: &mut Vec<&'static str>,
) {
    // 記録の一括 off とハッシュ化 (T14.41)。当てた瞬間から**次の記録**に効く
    // (既に溜まっている個票は書き換えない。消したいなら再起動する)
    if fresh.records != old.records {
        next.records = fresh.records;
        next.sources.adopt(&fresh.sources, "PROXY_RECORDS");
        crate::records::set(fresh.records);
        applied.push("PROXY_RECORDS");
    }
    // 消えたクライアントの検知 (T14.52)。当てるのは accept なので**次に来る接続から**効く
    // (いま開いている接続のソケットには当て直さない)
    if fresh.tcp_keepalive != old.tcp_keepalive {
        next.tcp_keepalive = fresh.tcp_keepalive;
        next.sources.adopt(&fresh.sources, "PROXY_TCP_KEEPALIVE");
        applied.push("PROXY_TCP_KEEPALIVE");
    }
    // 追跡する接続元 (T14.27)。旗を立てるのは accept なので**次に来る接続から**効く
    // (いま開いている接続の旗はそのまま = 途中で追跡が切れたり増えたりしない)
    if fresh.trace_client != old.trace_client {
        next.trace_client = fresh.trace_client;
        next.sources.adopt(&fresh.sources, "PROXY_TRACE_CLIENT");
        applied.push("PROXY_TRACE_CLIENT");
    }
    // 転送中の最小の速さ (T29.34)。見張りを作るのは要求・トンネルの頭なので、次に始まる転送から効く
    if fresh.min_transfer != old.min_transfer {
        next.min_transfer = fresh.min_transfer;
        next.sources.adopt(&fresh.sources, "PROXY_MIN_TRANSFER_BPS");
        next.sources
            .adopt(&fresh.sources, "PROXY_MIN_TRANSFER_WINDOW_SECS");
        applied.push("PROXY_MIN_TRANSFER_BPS/PROXY_MIN_TRANSFER_WINDOW_SECS");
    }
    if fresh.tunnel_idle != old.tunnel_idle {
        next.tunnel_idle = fresh.tunnel_idle;
        next.sources.adopt(&fresh.sources, "PROXY_TUNNEL_IDLE_SECS");
        applied.push("PROXY_TUNNEL_IDLE_SECS");
    }
    if fresh.max_conns != old.max_conns {
        next.max_conns = fresh.max_conns;
        next.sources.adopt(&fresh.sources, "PROXY_MAX_CONNS");
        applied.push("PROXY_MAX_CONNS");
    }
    // `auto` は `PROXY_MAX_CONNS` から決まるので、そちらが変わるとこの値も変わる
    // (当てるのは `serve` が接続ごとに `Workers::set_limit` で。T11.6)
    if fresh.max_threads != old.max_threads {
        next.max_threads = fresh.max_threads;
        next.sources.adopt(&fresh.sources, "PROXY_MAX_THREADS");
        // 名前解決のスレッド群の上限はワーカーの 1/4 (T32.11)
        crate::dns::set_resolver_limit(crate::dns::resolver_limit_for_threads(fresh.max_threads));
        applied.push("PROXY_MAX_THREADS");
    }
}

/// 名前解決の調整値 (`merge_live` の 3 段目)。
fn merge_dns(next: &mut Config, old: &Config, fresh: &Config, applied: &mut Vec<&'static str>) {
    if fresh.dns_ttl != old.dns_ttl {
        next.dns_ttl = fresh.dns_ttl;
        next.sources.adopt(&fresh.sources, "PROXY_DNS_TTL_SECS");
        crate::dns::set_ttl(fresh.dns_ttl);
        crate::dns::clear();
        applied.push("PROXY_DNS_TTL_SECS");
    }
    // 負のキャッシュは長さを変えるだけ (表は捨てない。覚えている失敗は次の参照で
    // 新しい長さと比べられるので、0 にすればその場で効かなくなる)
    if fresh.dns_negative != old.dns_negative {
        next.dns_negative = fresh.dns_negative;
        next.sources
            .adopt(&fresh.sources, "PROXY_DNS_NEGATIVE_SECS");
        crate::dns::set_negative_ttl(fresh.dns_negative);
        applied.push("PROXY_DNS_NEGATIVE_SECS");
    }
    // keep-warm の窓も長さを変えるだけ (表は捨てない)。0 にしたときだけ
    // 待ち行列をその場で空にする (`set_warm_window` の中。T14.1)
    if fresh.dns_warm != old.dns_warm {
        next.dns_warm = fresh.dns_warm;
        next.sources.adopt(&fresh.sources, "PROXY_DNS_WARM_SECS");
        crate::dns::set_warm_window(fresh.dns_warm);
        applied.push("PROXY_DNS_WARM_SECS");
    }
    // serve-stale の窓も長さを変えるだけ (表は捨てない。次の参照から効く。T21.3)
    if fresh.dns_stale != old.dns_stale {
        next.dns_stale = fresh.dns_stale;
        next.sources.adopt(&fresh.sources, "PROXY_DNS_STALE_SECS");
        crate::dns::set_serve_stale(fresh.dns_stale);
        applied.push("PROXY_DNS_STALE_SECS");
    }
    // 名前解決の締め切りも次の問い合わせから効く (走っているものは止めない。T32.11)
    if fresh.dns_timeout != old.dns_timeout {
        next.dns_timeout = fresh.dns_timeout;
        next.sources.adopt(&fresh.sources, "PROXY_DNS_TIMEOUT_SECS");
        crate::dns::set_timeout(fresh.dns_timeout);
        applied.push("PROXY_DNS_TIMEOUT_SECS");
    }
}

/// canary・PAC・ブロックリスト・ログの水準 (`merge_live` の 4 段目)。
fn merge_rest(next: &mut Config, old: &Config, fresh: &Config, applied: &mut Vec<&'static str>) {
    // canary の宛先と周期 (T14.10) と IPv6 側 (T14.37)。`canary` スレッドは
    // 次の周期から新しい宛先を使う
    if fresh.canary != old.canary
        || fresh.canary_secs != old.canary_secs
        || fresh.canary_ipv6 != old.canary_ipv6
    {
        next.canary = fresh.canary.clone();
        next.canary_secs = fresh.canary_secs;
        next.canary_ipv6 = fresh.canary_ipv6;
        next.sources.adopt(&fresh.sources, "PROXY_CANARY");
        next.sources.adopt(&fresh.sources, "PROXY_CANARY_SECS");
        next.sources.adopt(&fresh.sources, "PROXY_CANARY_IPV6");
        crate::canary::configure(&fresh.canary, fresh.canary_secs, fresh.canary_ipv6);
        applied.push("PROXY_CANARY");
    }
    if fresh.pac_direct != old.pac_direct {
        next.pac_direct = fresh.pac_direct.clone();
        next.sources.adopt(&fresh.sources, "PROXY_PAC_DIRECT");
        applied.push("PROXY_PAC_DIRECT");
    }
    let bl = crate::blocklist::Sources::from_config(fresh);
    if bl != crate::blocklist::Sources::from_config(old) {
        next.blocklist_file = fresh.blocklist_file.clone();
        next.blocklist_url = fresh.blocklist_url.clone();
        next.blocklist_refresh = fresh.blocklist_refresh;
        next.blocklist_exempt = fresh.blocklist_exempt.clone();
        for key in [
            "PROXY_BLOCKLIST_FILE",
            "PROXY_BLOCKLIST_URL",
            "PROXY_BLOCKLIST_REFRESH_SECS",
            "PROXY_BLOCKLIST_EXEMPT",
        ] {
            next.sources.adopt(&fresh.sources, key);
        }
        crate::blocklist::configure(bl);
        applied.push("PROXY_BLOCKLIST_*");
    }
    // 消した・読めないときは起動時と同じ既定 (lite なら warn。T35.5)。lite は起動時に
    // 固定される項目なので、いま効いている `old` の側を見る
    let level = envfile::var("PROXY_LOG_LEVEL")
        .and_then(|v| log::Level::parse(&v))
        .unwrap_or(log::default_level(old.lite));
    if level != log::current_level() {
        log::set_level(level);
        next.sources.adopt(&fresh.sources, "PROXY_LOG_LEVEL");
        applied.push("PROXY_LOG_LEVEL");
    }
}

/// 起動時に固定される項目のうち、`boot` (起動時の設定) から `fresh` で変わったものの名前 (この順)。
pub(super) fn restart_required(boot: &Config, fresh: &Config) -> Vec<&'static str> {
    let mut restart = Vec::new();
    if fresh.port != boot.port {
        restart.push("SERVER_PORT");
    }
    if fresh.bind_addrs != boot.bind_addrs || fresh.ipv6 != boot.ipv6 {
        restart.push("PROXY_BIND/PROXY_IPV6");
    }
    // Happy Eyeballs の間隔は起動時に 1 回だけ渡す (T17.6)
    if fresh.he_stagger != boot.he_stagger {
        restart.push("PROXY_HE_STAGGER_MS");
    }
    // 待ち受けは起動時に 1 回作るので backlog も起動時のまま (T14.47)
    if fresh.listen_backlog != boot.listen_backlog {
        restart.push("PROXY_LISTEN_BACKLOG");
    }
    if fresh.tls_enabled != boot.tls_enabled
        || fresh.tls_verify != boot.tls_verify
        || fresh.tls_ca_file != boot.tls_ca_file
    {
        restart.push("PROXY_TLS*");
    }
    if fresh.pool_per_host != boot.pool_per_host {
        restart.push("PROXY_ORIGIN_POOL");
    }
    if fresh.stats_persist != boot.stats_persist {
        restart.push("PROXY_STATS_PERSIST");
    }
    if fresh.cache != boot.cache {
        restart.push("cache settings (SERVER_MEMORY/SERVER_DISK/PROXY_CACHE_*)");
    }
    restart
}
