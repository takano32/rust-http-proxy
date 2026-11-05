//! クライアントから受けた要求ヘッダーを、オリジンへ転送する形に整える。

use std::net::SocketAddr;

use super::{ConnectionNames, FRAMING_HEADERS, is_hop_by_hop, is_hop_by_hop_name};
use crate::ascii;
use crate::via;

/// 転送するリクエストヘッダーを `out` へ直接書く ([`sanitize_and_inject_headers`] と同じ規則)。
///
/// `Vec<String>` を経由しないので、ヘッダー 1 本につき小文字化した名前と行の複製を作らない
/// (実測: ヘッダー 11 本の要求で確保が 137 → 62 回/要求)。`Host:` は呼び出し側がオリジンの
/// ものに差し替えるので、ここでは落とす。
pub fn write_request_headers(out: &mut Vec<u8>, headers: &[String], client_ip: Option<&str>) {
    // Connection: で指名された名前は借用のまま覚える (溢れたら元の行を読み直す)
    let custom = ConnectionNames::collect(headers.iter().map(String::as_str));
    let named_in_connection =
        |name: &str| -> bool { custom.contains(name, || headers.iter().map(String::as_str)) };

    let mut x_forwarded_for: Option<&str> = None;
    let mut has_via = false;
    // `Content-Length` は 1 本だけ転送する。重複が**同値**であることは入口
    // (`parse_request_headers`) が見ていて、違えば 400 で断っているので、ここでは
    // 2 本目以降を落とすだけでよい。両方書くとオリジンによっては 400 になる (T19.1)
    let mut cl_written = false;
    for line in headers {
        let Some((k, v)) = ascii::split_once(line, b':') else {
            continue;
        };
        let name = k.trim_ascii();
        // 枠組みのヘッダー (Content-Length / Transfer-Encoding) は Connection: で指名されても
        // 落とさない。落とすと「ヘッダーは枠組み無し・本文は送る」というずれが起き、
        // オリジンが本文を次の要求の先頭として読む (要求スマグリング)。
        // 本文を送るかどうかは Framing::of_request が生のヘッダーから決めるので、
        // ここだけで落とすと両者が食い違う
        let framing_header = FRAMING_HEADERS.iter().any(|f| name.eq_ignore_ascii_case(f));
        // `Expect` はこちらが受け持つ (100-continue は自分で 100 を返し、それ以外は 417 で
        // 断っている。T.28.3) ので、オリジンへは転送しない
        if name.eq_ignore_ascii_case("host")
            || is_hop_by_hop_name(name)
            || name.eq_ignore_ascii_case("expect")
            || (!framing_header && named_in_connection(name))
        {
            continue;
        }
        if name.eq_ignore_ascii_case("x-forwarded-for") {
            x_forwarded_for = Some(v.trim_ascii());
            continue;
        }
        if name.eq_ignore_ascii_case("content-length") {
            if cl_written {
                continue;
            }
            cl_written = true;
        }
        if name.eq_ignore_ascii_case("via") {
            has_via = true;
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(v.trim_ascii().as_bytes());
            out.extend_from_slice(b", ");
            out.extend_from_slice(via::token().as_bytes());
            out.extend_from_slice(b"\r\n");
            continue;
        }
        // 行末は CRLF にそろえる (T.32.15 (1))。行はクライアントの行末 (LF だけのこともある) を付けたまま
        // 来るので、そのまま書くと LF だけの行とこちらが足す CRLF の行が混ざり、CRLF でしか区切らない
        // オリジンと読み方が食い違いうる。ふつうの行 (CRLF で終わる) は今までどおり 1 回で写す
        match line.as_bytes() {
            [.., b'\r', b'\n'] => out.extend_from_slice(line.as_bytes()),
            [rest @ .., b'\n'] => {
                out.extend_from_slice(rest);
                out.extend_from_slice(b"\r\n");
            }
            bytes => {
                out.extend_from_slice(bytes);
                out.extend_from_slice(b"\r\n");
            }
        }
    }

    if let Some(ip) = client_ip {
        out.extend_from_slice(b"X-Forwarded-For: ");
        if let Some(existing) = x_forwarded_for {
            out.extend_from_slice(existing.as_bytes());
            out.extend_from_slice(b", ");
        }
        out.extend_from_slice(ip.as_bytes());
        out.extend_from_slice(b"\r\n");
    } else if let Some(existing) = x_forwarded_for {
        out.extend_from_slice(b"X-Forwarded-For: ");
        out.extend_from_slice(existing.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    if !has_via {
        // 起動時に 1 回作った行をそのまま書く (要求ごとに String を作らない)
        out.extend_from_slice(via::line().as_bytes());
    }
}

/// 転送するリクエストヘッダーを `Vec<String>` で組み立てる (`Host:` は残す)。
///
/// 熱い経路は [`write_request_headers`] が同じ規則で `out` へ直接書く。こちらは組を作る
/// 読みやすい形で、`write_request_tests` が 1 バイトも違わないことを突き合わせる相手 (手本) として残す。
pub fn sanitize_and_inject_headers(
    headers: &[String],
    client_addr: Option<SocketAddr>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut custom_hop_by_hop = Vec::new();
    let mut x_forwarded_for: Option<String> = None;
    let mut has_via = false;

    // First pass: check Connection header for custom hop-by-hop header names
    for line in headers {
        if let Some((k, v)) = ascii::split_once(line, b':')
            && k.trim_ascii().eq_ignore_ascii_case("connection")
        {
            for item in v.split(',') {
                custom_hop_by_hop.push(item.trim_ascii().to_ascii_lowercase());
            }
        }
    }

    // Second pass: filter and collect
    for line in headers {
        if let Some((k, v)) = ascii::split_once(line, b':') {
            let k_trim = k.trim_ascii();
            let k_lower = k_trim.to_ascii_lowercase();

            // 枠組みのヘッダーは Connection: で指名されても落とさない
            // (要求スマグリング対策。write_request_headers 側と同じ規則)
            let framing_header = FRAMING_HEADERS.contains(&k_lower.as_str());
            if is_hop_by_hop(&k_lower)
                || k_lower == "expect"
                || (!framing_header && custom_hop_by_hop.contains(&k_lower))
            {
                continue;
            }

            if k_lower == "x-forwarded-for" {
                x_forwarded_for = Some(v.trim_ascii().to_string());
                continue;
            }

            if k_lower == "via" {
                has_via = true;
                let new_via = format!("{}: {}, {}\r\n", k_trim, v.trim_ascii(), via::token());
                out.push(new_via);
                continue;
            }

            out.push(line.clone());
        }
    }

    // Add X-Forwarded-For
    if let Some(addr) = client_addr {
        let ip_str = addr.ip().to_string();
        let xff_val = match x_forwarded_for {
            Some(existing) => format!("{}, {}", existing, ip_str),
            None => ip_str,
        };
        out.push(format!("X-Forwarded-For: {}\r\n", xff_val));
    } else if let Some(existing) = x_forwarded_for {
        out.push(format!("X-Forwarded-For: {}\r\n", existing));
    }

    // Add Via if not already updated
    if !has_via {
        out.push(via::line().to_string());
    }

    out
}
