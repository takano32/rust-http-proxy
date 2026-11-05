//! CONNECT の入口の検査 (純粋。ソケットも統計も見ない。T34.12)。

use super::*;

/// CONNECT を枠組みで断る理由 (無ければ `None`)。
///
/// CONNECT は `http::handle_http_with_headers` を通らずトンネルの経路に入るので、
/// T19 で入れた要求の枠組みの検査がここへ来ない。**同じ判定をこの入口にも置く** (T27.0)。
/// CONNECT に本文は無く (RFC 9110 §9.3.6)、`Host` の重複 (RFC 9110 §7.2)・obs-fold
/// (RFC 9112 §5.2)・本文の枠組みをそのままにしてトンネルを張ると、ヘッダーの後ろに
/// 付けたバイトが**トンネルの最初のデータとして宛先へ流れる** (前段がいる構成では要求の密輸)。
/// ここで断れば宛先へは接続もしない (`Step::Connect` に進まない) ので、1 バイトも届かない。
///
/// 優先は `Host` の重複 → obs-fold → 壊れたヘッダー → 本文の枠組み (先に当たったものを数える)。
#[inline]
pub(super) fn connect_header_reason(
    h: &request::RequestHeaders,
    has_body: bool,
) -> Option<metrics::BadRequestReason> {
    if h.dup_host {
        Some(metrics::BadRequestReason::DupHost)
    } else if h.obs_fold {
        Some(metrics::BadRequestReason::ObsFold)
    } else if h.bad_header {
        // 名前とコロンの間の空白・値の CR / LF / NUL (T27.5)
        Some(metrics::BadRequestReason::BadHeader)
    } else if has_body || h.content_length_invalid {
        // `has_body` はヘッダーを読むループが `Content-Length` / `Transfer-Encoding` を
        // 見たときに立つ旗 (本文を読むため)。値が数字でない `Content-Length` も
        // ここに含まれる (T19.1 と同じ扱い = `body_framing`)
        Some(metrics::BadRequestReason::BodyFraming)
    } else {
        None
    }
}

/// CONNECT の宛先が `host:port` の形か (RFC 9110 §9.3.6)。
///
/// ポートが無い・読めない・0 のときは今まで 443 とみなすか 0 番へ繋ぎに行って 502 だった。
/// クライアントの誤りなので、偽なら呼ぶ側が 400 にする (T27.8)。
#[inline]
pub(super) fn connect_target_ok(bare_host: &str, host_port: Option<u16>) -> bool {
    !bare_host.is_empty() && host_port.is_some_and(|p| p != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::BadRequestReason as R;

    fn headers(
        dup_host: bool,
        obs_fold: bool,
        bad_header: bool,
        cl_invalid: bool,
    ) -> request::RequestHeaders {
        request::RequestHeaders {
            dup_host,
            obs_fold,
            bad_header,
            content_length_invalid: cl_invalid,
            ..request::RequestHeaders::default()
        }
    }

    #[test]
    fn a_clean_connect_has_no_reason() {
        assert_eq!(
            connect_header_reason(&headers(false, false, false, false), false),
            None
        );
    }

    #[test]
    fn the_first_matching_reason_wins() {
        // 全部立っていれば `Host` の重複
        assert_eq!(
            connect_header_reason(&headers(true, true, true, true), true),
            Some(R::DupHost)
        );
        assert_eq!(
            connect_header_reason(&headers(false, true, true, true), true),
            Some(R::ObsFold)
        );
        assert_eq!(
            connect_header_reason(&headers(false, false, true, true), true),
            Some(R::BadHeader)
        );
        // 本文の枠組みは、本文の行があっても値が読めない `Content-Length` でも同じ理由
        assert_eq!(
            connect_header_reason(&headers(false, false, false, false), true),
            Some(R::BodyFraming)
        );
        assert_eq!(
            connect_header_reason(&headers(false, false, false, true), false),
            Some(R::BodyFraming)
        );
    }

    #[test]
    fn a_connect_target_needs_a_host_and_a_nonzero_port() {
        assert!(connect_target_ok("example.com", Some(443)));
        assert!(connect_target_ok("example.com", Some(1)));
        assert!(connect_target_ok("example.com", Some(u16::MAX)));
        assert!(!connect_target_ok("example.com", Some(0)), "0 番は断る");
        assert!(
            !connect_target_ok("example.com", None),
            "ポートが無い・読めない"
        );
        assert!(!connect_target_ok("", Some(443)), "ホストが空");
    }
}
