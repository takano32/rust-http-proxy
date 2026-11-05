//! 計測の**型と定数**。ホスト別統計 ([`HostStats`])、1 要求の内訳 ([`Detail`])、
//! エラーの原因 ([`ErrCause`])、`/status` の並べ替えの鍵、応答の形の版。
//!
//! [`crate::metrics::HostStats`] を持ち回る側 (`Metrics` 本体) は
//! `proxy-metrics-core` にある。ここを下の層に置いてあるのは、記録の個票
//! (`proxy-metrics-recent`) がこの型を使い、`Metrics` がその個票を持つ =
//! 間に挟まないと輪になるため (T14.55 で割った)。
//!
//! T34.1 で関心ごとにモジュールへ割った。**外から見える名前は全部ここから引ける**
//! (`crate::metrics::HostStats` のような書き方は割る前と同じに通る):
//!
//! - [`schema`](self): 応答の形の版 (`SCHEMA` / `SCHEMA_HEAD` / `with_schema`)
//! - `causes`: 結末の分け方 (`HostOutcome` / `ErrCause` / `DnsCause` / `Failure` / `BlockCause`)
//! - `bad_request`: 読めずに断った理由 (`BadRequestReason`)
//! - `detail`: 1 要求の内訳 (`Detail` / `StageMs`)
//! - `host_stats`: ホスト別の通算 (`HostStats` / `WebHour` / `LATENCY_BOUNDS_MS` / `MAX_HOSTS`)、
//!   `host_record`: その `.rrd` の形 (`encode` / `decode`)、`host_json`: その 1 行の JSON (`stats_json`)
//! - `clients`: 接続元別の表の決まり (`MAX_CLIENTS` ほか / `NewClient` / `ClientSort`)
//! - `status`: `/status` の部品 (`StatusExtras` / `Concurrency` / `HostSort`)

mod bad_request;
mod causes;
mod clients;
mod detail;
mod host_json;
mod host_record;
mod host_stats;
mod schema;
mod status;

pub use bad_request::{BAD_REQUEST_REASON_NAMES, BAD_REQUEST_REASONS, BadRequestReason};
pub use causes::{
    BlockCause, DNS_CAUSE_NAMES, DNS_CAUSES, DnsCause, ERR_CAUSE_NAMES, ERR_CAUSES, ErrCause,
    Failure, HostOutcome,
};
pub use clients::{
    ClientSort, MAX_AGENT_BYTES, MAX_CLIENT_AGENTS, MAX_CLIENT_PORTS, MAX_CLIENT_TARGETS,
    MAX_CLIENTS, NewClient,
};
pub use detail::{Detail, StageMs};
pub use host_json::stats_json;
pub use host_stats::{HostStats, LATENCY_BOUNDS_MS, MAX_HOSTS, WebHour};
pub use schema::{SCHEMA, SCHEMA_HEAD, with_schema};
pub use status::{Concurrency, HostSort, StatusExtras};

#[cfg(test)]
mod bad_request_reason_tests {
    use super::BadRequestReason;

    /// 絶対 URI のスキームの大小を問わず、ホストが無いのは `bad_uri` (T28.33)。
    #[test]
    fn of_target_reads_the_scheme_case_insensitively() {
        for t in [
            "http:///a",
            "HTTP:///a",
            "Https:///a",
            "/http//a",
            "/https//a",
        ] {
            assert_eq!(
                BadRequestReason::of_target("GET", t),
                BadRequestReason::BadUri,
                "{}",
                t
            );
        }
        for t in ["/a", "/HTTP//a"] {
            assert_eq!(
                BadRequestReason::of_target("GET", t),
                BadRequestReason::NoHost,
                "{}",
                t
            );
        }
        assert_eq!(
            BadRequestReason::of_target("G ET", "HTTP:///a"),
            BadRequestReason::Method
        );
    }
}
