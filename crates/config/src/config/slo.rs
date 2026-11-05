//! SLO の 4 つの閾 (`PROXY_SLO`。T14.50) の読み方と書き戻し。判定そのものは `proxy-metrics` の `slo`。

/// SLO の 4 つの閾 (`PROXY_SLO`。T14.50)。**満たす = 値がこれ以下**。
///
/// 履歴スレッドが 5 秒の標本 1 本ごとにこの 4 つを判定し、時間ごと・日ごとの達成率を
/// `/slo` で返す (判定そのものは `proxy-metrics` の `slo`。この層は値を運ぶだけで、
/// **既定は [`DEFAULT_SLO`] = `proxy_metrics::slo::DEFAULT` と同じ値**)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slo {
    /// CONNECT 1 本ごとの「確立 − カーネルの RTT」の p50 (ms。T.28.45 から。鍵の綴りは今までのまま)
    pub connect_p50_ms: f64,
    /// 同じ窓の p95 (ms)
    pub connect_p95_ms: f64,
    /// 名前解決に成功した試みあたりのエラー (T.28.45)
    pub error_rate: f64,
    /// 名前解決のミス (失敗した試みのぶんを除く。T.28.45) ÷ 確立
    pub dns_miss_per_connect: f64,
}

/// `PROXY_SLO` の既定 (`connect_p50_ms=5,connect_p95_ms=50,error_rate=0.005,dns_miss_per_connect=0.2`)。
///
/// `proxy-metrics` の `slo::DEFAULT` と同じ値 (この層は計測クレートに依存しないので数値で持つ。
/// 食い違ったら `tests/slo_test.rs` が落ちる)。確立の 2 つは T.28.45 で 10 / 100 → 5 / 50。
pub const DEFAULT_SLO: Slo = Slo {
    connect_p50_ms: 5.0,
    connect_p95_ms: 50.0,
    error_rate: 0.005,
    dns_miss_per_connect: 0.2,
};

impl Default for Slo {
    fn default() -> Self {
        DEFAULT_SLO
    }
}

impl Slo {
    /// `connect_p50_ms=10,error_rate=0.01` のように**書いたものだけ**を当てる
    /// (書いていない閾と、数として読めない値・負の値・知らない綴りは既定のまま)。
    pub fn parse(spec: &str) -> Slo {
        Self::parse_skipping(spec).0
    }

    /// [`Slo::parse`] と同じで、飛ばした項目 (空の項目は除く) も返す (WARN に使う。T.28.31)。
    pub fn parse_skipping(spec: &str) -> (Slo, Vec<&str>) {
        let (mut out, mut skipped) = (DEFAULT_SLO, Vec::new());
        for item in spec.split(',').filter(|i| !i.trim().is_empty()) {
            let Some((k, v)) = item.split_once('=') else {
                skipped.push(item.trim());
                continue;
            };
            let n = v
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n >= 0.0);
            let slot = match k.trim().to_ascii_lowercase().as_str() {
                "connect_p50_ms" => &mut out.connect_p50_ms,
                "connect_p95_ms" => &mut out.connect_p95_ms,
                "error_rate" => &mut out.error_rate,
                "dns_miss_per_connect" => &mut out.dns_miss_per_connect,
                _ => {
                    skipped.push(item.trim());
                    continue;
                }
            };
            match n {
                Some(n) => *slot = n,
                None => skipped.push(item.trim()),
            }
        }
        (out, skipped)
    }

    /// `.env` にそのまま書き戻せる形 (`/config` と `--check` に出るのもこれ)。
    pub fn spec(&self) -> String {
        format!(
            "connect_p50_ms={},connect_p95_ms={},error_rate={},dns_miss_per_connect={}",
            self.connect_p50_ms, self.connect_p95_ms, self.error_rate, self.dns_miss_per_connect
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 書き戻しは既定を既定の綴りで出し、読み直せば同じ値になる。
    #[test]
    fn spec_round_trips_and_shows_the_default() {
        assert_eq!(
            DEFAULT_SLO.spec(),
            "connect_p50_ms=5,connect_p95_ms=50,error_rate=0.005,dns_miss_per_connect=0.2"
        );
        let custom = Slo {
            connect_p50_ms: 10.0,
            connect_p95_ms: 100.0,
            error_rate: 0.01,
            dns_miss_per_connect: 0.5,
        };
        assert_eq!(Slo::parse(&custom.spec()), custom);
        assert_eq!(Slo::default(), DEFAULT_SLO);
    }

    /// 大小は問わず、`inf` と負の値と知らない綴りは既定のまま飛ばす。
    #[test]
    fn parse_skips_what_it_cannot_use() {
        let (slo, skipped) = Slo::parse_skipping("CONNECT_P95_MS=70,error_rate=inf,x=1");
        assert_eq!(slo.connect_p95_ms, 70.0);
        assert_eq!(slo.error_rate, DEFAULT_SLO.error_rate);
        assert_eq!(skipped, vec!["error_rate=inf", "x=1"]);
        assert_eq!(Slo::parse(""), DEFAULT_SLO);
    }
}
