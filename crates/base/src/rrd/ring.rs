//! 環状の領域: 時刻付きレコードを順に書き、いっぱいになったら最古を上書きする。
//! 読み込み時はレコードの時刻からカーソルを復元する。

use std::io;

use super::{Dec, Region, Rrd};

pub struct Ring {
    region: Region,
    /// 次に書く位置
    next: usize,
}

impl Ring {
    /// 領域を読み、時刻順のペイロード列と、その続きから書けるカーソルを作る。
    /// ペイロードの先頭 8 バイトが時刻 (epoch 秒) であること。
    pub fn load(rrd: &Rrd, region: Region) -> io::Result<(Ring, Vec<Vec<u8>>)> {
        let mut recs: Vec<(u64, usize, Vec<u8>)> = rrd
            .read_all(region)?
            .into_iter()
            .map(|(idx, p)| (Dec(&p).u64(), idx, p))
            .filter(|(t, _, _)| *t > 0)
            .collect();
        recs.sort_by_key(|(t, idx, _)| (*t, *idx));
        let next = recs
            .last()
            .map(|(_, idx, _)| (idx + 1) % region.count)
            .unwrap_or(0);
        Ok((
            Ring { region, next },
            recs.into_iter().map(|(_, _, p)| p).collect(),
        ))
    }

    pub fn push(&mut self, rrd: &Rrd, payload: &[u8]) -> io::Result<()> {
        rrd.write(self.region, self.next, payload)?;
        self.next = (self.next + 1) % self.region.count;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rrd::Enc;

    #[test]
    fn wraps_and_restores_in_time_order() {
        let path = std::env::temp_dir().join(format!("shp-ring-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (rrd, _) = Rrd::open(&path).unwrap();
        let region = rrd.layout.history_hour; // 720 本
        let (mut ring, got) = Ring::load(&rrd, region).unwrap();
        assert!(got.is_empty());
        for t in 1..=800u64 {
            ring.push(&rrd, &Enc::new().u64(t).u64(t * 10).0).unwrap();
        }
        let (mut ring, got) = Ring::load(&rrd, region).unwrap();
        assert_eq!(got.len(), 720);
        assert_eq!(Dec(&got[0]).u64(), 81, "oldest surviving");
        assert_eq!(Dec(&got[719]).u64(), 800);
        ring.push(&rrd, &Enc::new().u64(801).0).unwrap();
        let (_, got) = Ring::load(&rrd, region).unwrap();
        assert_eq!(Dec(&got[719]).u64(), 801);
        assert_eq!(Dec(&got[0]).u64(), 82);
        let _ = std::fs::remove_file(&path);
    }
}
