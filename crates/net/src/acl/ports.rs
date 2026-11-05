//! 許可するポートの集合 (`PROXY_CONNECT_PORTS`)。T.34.65 で `acl.rs` から移した (本体は変えていない)。

/// 許可するポートの集合 (`PROXY_CONNECT_PORTS`)。空なら制限なし。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortSet {
    ranges: Vec<(u16, u16)>,
}

impl PortSet {
    /// `443,80,8080-8099` 形式を読む。空文字なら制限なし。書式が違う項目は無視する。
    pub fn parse(spec: &str) -> PortSet {
        let mut ranges = Vec::new();
        for item in spec.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            match item.split_once('-') {
                Some((lo, hi)) => {
                    if let (Ok(lo), Ok(hi)) = (lo.trim().parse::<u16>(), hi.trim().parse::<u16>()) {
                        ranges.push((lo.min(hi), lo.max(hi)));
                    }
                }
                None => {
                    if let Ok(p) = item.parse::<u16>() {
                        ranges.push((p, p));
                    }
                }
            }
        }
        PortSet { ranges }
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// 読んだ結果を `443,8080-8099` の形に書き戻す (空なら `""` = 制限なし)。
    /// `/config` が**効いている値**を出すために使う (読めなかった項目は落ちている。T14.15)。
    pub fn spec(&self) -> String {
        self.ranges
            .iter()
            .map(|(lo, hi)| {
                if lo == hi {
                    lo.to_string()
                } else {
                    format!("{}-{}", lo, hi)
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// 制限が無ければ常に true。
    pub fn allows(&self, port: u16) -> bool {
        self.ranges.is_empty()
            || self
                .ranges
                .iter()
                .any(|(lo, hi)| port >= *lo && port <= *hi)
    }
}
