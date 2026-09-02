//! `/blocklist`: ブロックリストの判定と手動の上書き。

/// `/blocklist?host=<h>` で判定、`&action=block|allow|clear[&ttl_secs=N]` で手動の上書き。
/// 引数なしなら一覧の状態と上書きの一覧。
pub(super) fn handle(params: &[(String, String)]) -> (u16, &'static str, String) {
    use crate::blocklist;
    let get = |k: &str| params.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
    let Some(host) = get("host").map(str::trim).filter(|h| !h.is_empty()) else {
        return (200, "application/json", blocklist::status_json());
    };
    let host = crate::net::split_host_port(host).0.to_ascii_lowercase();
    if host.is_empty() || !host.contains('.') {
        return (
            400,
            "application/json",
            "{\"error\":\"host must be a domain name\"}".to_string(),
        );
    }
    let ttl = get("ttl_secs")
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(86400));
    let action = get("action").unwrap_or("");
    let changed = match action {
        "block" => Some(blocklist::set_override(&host, true, ttl).json()),
        "allow" => Some(blocklist::set_override(&host, false, ttl).json()),
        "clear" => Some(blocklist::clear_override(&host).to_string()),
        "" => None,
        _ => {
            return (
                400,
                "application/json",
                "{\"error\":\"action must be block, allow or clear\"}".to_string(),
            );
        }
    };
    let v = blocklist::check(&host);
    (
        200,
        "application/json",
        format!(
            "{{\"host\":\"{}\",\"blocked\":{},\"verdict\":\"{}\",\"action\":{},\"overrides\":[{}]}}",
            crate::json::escape(&host),
            v.blocked(),
            v.as_str(),
            changed
                .map(|c| format!("{{\"{}\":{}}}", action, c))
                .unwrap_or_else(|| "null".to_string()),
            blocklist::overrides()
                .iter()
                .map(blocklist::Override::json)
                .collect::<Vec<_>>()
                .join(",")
        ),
    )
}
