//! 表の 1 領域の「どのスロットに誰を、どんな中身で書いたか」([`Slots`]。T.32.22。T.34.31 で
//! `persist.rs` から移した)。
//!
//! 書き直しの判断 ([`Slots::plan`]) は I/O をしない。`Store::write_table` が計画どおりに `pwrite` し、
//! 書けたかどうかを [`Slots::wrote`] / [`Slots::cleared`] で返す。判断の式と書く順は割る前と同じ。

use crate::metrics::HostStats;

/// 表の 1 領域の「どのスロットに誰を、どんな中身で書いたか」(T.32.22)。
///
/// 60 秒ごとの書き直しは上位 1,000 行を 1 行ずつ `pwrite` していた (1 分に 1,001 回、
/// 1 回の flush で 3.75 ms)。**名前ごとにスロットを固定し、中身の hash が前回と同じ行は
/// 書かない**。並びの順位が入れ替わっても、名前のスロットは動かさない (読み戻しは
/// 並びを見ないので、どのスロットに居てもよい)。上位から外れた名前のスロットは、
/// 新しく入った名前が使い、余ったぶんだけ消す。
#[derive(Default)]
pub(super) struct Slots {
    /// スロット → (名前, 中身の hash)。空なら `None`
    slots: Vec<Option<(String, u64)>>,
    /// 名前 → スロット
    by_name: std::collections::HashMap<String, usize>,
}

/// 書き直しの計画 ([`Slots::plan`] の戻り値)。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Plan {
    /// 書く行 (スロット, 名前, 中身, 中身の hash)。この順に書く
    pub(super) writes: Vec<(usize, String, Vec<u8>, u64)>,
    /// 書き終えたあとに消すか確かめるスロット ([`Slots::stale`])。この順に確かめる
    pub(super) clears: Vec<usize>,
}

/// 書き直しの 1 行: (いまのスロット (無ければ `None`), 名前, 中身, 中身の hash)。
type Changed = (Option<usize>, String, Vec<u8>, u64);

impl Slots {
    /// 読み戻した中身から作る (起動直後の 1 回目の書き直しも、変わった行だけにする)。
    pub(super) fn from_loaded(count: usize, recs: &[(usize, Vec<u8>)]) -> Slots {
        let mut t = Slots {
            slots: vec![None; count],
            by_name: std::collections::HashMap::new(),
        };
        for (idx, payload) in recs {
            if *idx >= count {
                continue;
            }
            let Some((name, _)) = HostStats::decode(payload) else {
                continue;
            };
            // 同じ名前が 2 つあれば (壊れた書き直しの名残)、後のほうは空き扱い
            if t.by_name.contains_key(&name) {
                continue;
            }
            t.by_name.insert(name.clone(), *idx);
            t.slots[*idx] = Some((name, payload_hash(payload)));
        }
        t
    }

    /// 上位 `count` 行のうち、**中身が前回と違う行**をどのスロットに書くかと、消すか確かめる
    /// スロットを決める (I/O はしない)。上位から外れた名前はここで表から外す。
    ///
    /// `visit` は上位の行を順不同で 1 回ずつ渡す。渡された中では中身を組んで hash を比べるだけ。
    pub(super) fn plan(
        &mut self,
        count: usize,
        visit: impl FnOnce(&mut dyn FnMut(&str, &HostStats)),
    ) -> Plan {
        if self.slots.len() != count {
            *self = Slots {
                slots: vec![None; count],
                by_name: std::collections::HashMap::new(),
            };
        }
        let mut seen = vec![false; count];
        let mut changed: Vec<Changed> = Vec::new();
        let mut visited = 0usize;
        visit(&mut |name: &str, s: &HostStats| {
            if visited >= count {
                return;
            }
            visited += 1;
            let payload = s.encode(name);
            let h = payload_hash(&payload);
            match self.by_name.get(name) {
                Some(&i) => {
                    seen[i] = true;
                    if self.slots[i].as_ref().is_none_or(|(_, old)| *old != h) {
                        changed.push((Some(i), name.to_owned(), payload, h));
                    }
                }
                None => changed.push((None, name.to_owned(), payload, h)),
            }
        });
        // 上位から外れた名前のスロットを空ける (新しく入った名前が上書きするか、最後に消す)。
        // 空きは若い番号から使う (どのスロットでも読み戻せるが、ファイルの前に寄せておく)
        let mut free: Vec<usize> = Vec::new();
        for (i, slot) in self.slots.iter().enumerate().rev() {
            match slot {
                None => free.push(i),
                Some((name, _)) if !seen[i] => {
                    self.by_name.remove(name);
                    free.push(i);
                }
                Some(_) => {}
            }
        }
        let mut writes = Vec::with_capacity(changed.len());
        for (at, name, payload, h) in changed {
            let Some(idx) = at.or_else(|| free.pop()) else {
                // 上位 `count` 行に絞ってあるので空きは必ず足りる
                continue;
            };
            writes.push((idx, name, payload, h));
        }
        Plan {
            writes,
            clears: free,
        }
    }

    /// 計画の 1 行を書いた (`h` は書けなかったら 0 = 中身が分からないので次の回に書き直す)。
    pub(super) fn wrote(&mut self, idx: usize, name: String, h: u64) {
        self.by_name.insert(name.clone(), idx);
        self.slots[idx] = Some((name, h));
    }

    /// 外れた名前のスロットで、誰も上書きしなかったものか (消すべきか)。
    pub(super) fn stale(&self, i: usize) -> bool {
        self.slots[i]
            .as_ref()
            .is_some_and(|(n, _)| self.by_name.get(n) != Some(&i))
    }

    /// スロットを消した。
    pub(super) fn cleared(&mut self, i: usize) {
        self.slots[i] = None;
    }
}

/// レコードの中身の hash。読み戻した中身はスロットの大きさまでゼロで埋まっているので、
/// **末尾のゼロを除いて**比べる (書く前の `encode` の結果と同じ値になるように)。
///
/// 1 分に 1,000 行 × 約 600 B を通すので、SipHash (約 340 ns/行) ではなく 8 バイトずつ
/// 混ぜる軽いもの (約 30 ns/行)。比べるのは同じスロットの前回の中身とだけで、外れると
/// 「変わったのに書かない」になるが、64 ビットで偶然に揃う見込みは無視できる
pub(super) fn payload_hash(payload: &[u8]) -> u64 {
    let end = payload.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ end as u64;
    let mut mix = |w: u64| {
        h = (h ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h ^= h >> 29;
    };
    let (words, rest) = payload[..end].as_chunks::<8>();
    for c in words {
        mix(u64::from_le_bytes(*c));
    }
    if !rest.is_empty() {
        let mut w = [0u8; 8];
        w[..rest.len()].copy_from_slice(rest);
        mix(u64::from_le_bytes(w));
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(requests: u64) -> HostStats {
        HostStats {
            requests,
            ..HostStats::default()
        }
    }

    /// 計画を全部「書けた」として当て、消すべきスロットを返す (`Store::write_table` の I/O 抜き)。
    fn apply(t: &mut Slots, p: Plan) -> Vec<usize> {
        for (idx, name, _, h) in p.writes {
            t.wrote(idx, name, h);
        }
        let gone: Vec<usize> = p.clears.into_iter().filter(|&i| t.stale(i)).collect();
        for &i in &gone {
            t.cleared(i);
        }
        gone
    }

    fn rows(t: &mut Slots, count: usize, list: &[(&str, u64)]) -> Plan {
        t.plan(count, |f| {
            for (n, r) in list {
                f(n, &row(*r));
            }
        })
    }

    fn slots_of(p: &Plan) -> Vec<(usize, &str)> {
        p.writes
            .iter()
            .map(|(i, n, _, _)| (*i, n.as_str()))
            .collect()
    }

    /// 1. 空の表には上位を若いスロットから書く。2. 中身が同じなら 1 行も書かない。
    #[test]
    fn an_empty_table_fills_from_slot_zero_and_an_unchanged_one_writes_nothing() {
        let mut t = Slots::default();
        let p = rows(&mut t, 4, &[("a", 1), ("b", 2), ("c", 3)]);
        assert_eq!(slots_of(&p), vec![(0, "a"), (1, "b"), (2, "c")]);
        assert_eq!(apply(&mut t, p), Vec::<usize>::new());
        let p = rows(&mut t, 4, &[("c", 3), ("a", 1), ("b", 2)]);
        assert!(p.writes.is_empty(), "並びが入れ替わっただけでは書かない");
        assert_eq!(p.clears, vec![3], "空きのスロットは確かめるだけ");
        assert_eq!(apply(&mut t, p), Vec::<usize>::new());
    }

    /// 3. 中身が変わった行は、その名前のスロットに書き直す。
    #[test]
    fn a_changed_row_is_rewritten_in_its_own_slot() {
        let mut t = Slots::default();
        let p = rows(&mut t, 3, &[("a", 1), ("b", 2), ("c", 3)]);
        apply(&mut t, p);
        let p = rows(&mut t, 3, &[("a", 1), ("b", 20), ("c", 3)]);
        assert_eq!(slots_of(&p), vec![(1, "b")]);
        assert_eq!(p.writes[0].3, payload_hash(&row(20).encode("b")));
    }

    /// 4. 上位から外れた名前のスロットは、新しく入った名前が上書きする (消さない)。
    #[test]
    fn a_new_name_takes_the_slot_of_the_one_that_dropped_out() {
        let mut t = Slots::default();
        let p = rows(&mut t, 3, &[("a", 1), ("b", 2), ("c", 3)]);
        apply(&mut t, p);
        let p = rows(&mut t, 3, &[("a", 1), ("d", 9), ("c", 3)]);
        assert_eq!(slots_of(&p), vec![(1, "d")]);
        assert!(p.clears.is_empty());
        assert_eq!(apply(&mut t, p), Vec::<usize>::new());
    }

    /// 5. 行が減ったら、余ったスロットを消す。書けなかった行は hash 0 で残り、次の回に書き直す。
    #[test]
    fn fewer_rows_clear_the_rest_and_a_failed_write_is_retried() {
        let mut t = Slots::default();
        let p = rows(&mut t, 3, &[("a", 1), ("b", 2), ("c", 3)]);
        apply(&mut t, p);
        let p = rows(&mut t, 3, &[("b", 2)]);
        assert!(p.writes.is_empty());
        assert_eq!(
            apply(&mut t, p),
            vec![2, 0],
            "消す順は空きの並び (後ろから)"
        );
        // 書けなかった (hash 0) 行は、中身が同じでも次の回にもう一度書く
        let p = rows(&mut t, 3, &[("b", 2), ("e", 5)]);
        assert_eq!(slots_of(&p), vec![(0, "e")]);
        for (idx, name, _, _) in p.writes {
            t.wrote(idx, name, 0);
        }
        let p = rows(&mut t, 3, &[("b", 2), ("e", 5)]);
        assert_eq!(slots_of(&p), vec![(0, "e")]);
    }

    /// 6. 領域の大きさが変わったら表を作り直し、`count` を越えた行は見ない。
    #[test]
    fn a_new_region_size_starts_over_and_rows_past_the_count_are_ignored() {
        let mut t = Slots::default();
        let p = rows(&mut t, 2, &[("a", 1), ("b", 2), ("c", 3)]);
        assert_eq!(slots_of(&p), vec![(0, "a"), (1, "b")]);
        apply(&mut t, p);
        let p = rows(&mut t, 3, &[("a", 1), ("b", 2)]);
        assert_eq!(
            slots_of(&p),
            vec![(0, "a"), (1, "b")],
            "作り直したので全部書く"
        );
    }
}
