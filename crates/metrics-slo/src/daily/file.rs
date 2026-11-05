//! ファイルの扱い: 追記・切り詰め・読み戻し。

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use super::{MAX_BYTES, MAX_LINE, SECS_PER_DAY};

/// 1 行追記し、上限を越えたら古い行から捨てる。
pub(super) fn append(path: &Path, line: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    f.write_all(buf.as_bytes())?;
    f.flush()?;
    let len = f.metadata()?.len();
    drop(f);
    if len > MAX_BYTES {
        trim(path)?;
    }
    Ok(())
}

/// 上限を越えたぶんを古い行から捨てる (一時ファイルへ書いて差し替える)。
fn trim(path: &Path) -> std::io::Result<()> {
    let lines = read_lines(path);
    let mut total: u64 = lines.iter().map(|l| l.len() as u64 + 1).sum();
    let mut from = 0usize;
    while from < lines.len() && total > MAX_BYTES {
        total -= lines[from].len() as u64 + 1;
        from += 1;
    }
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        for l in &lines[from..] {
            f.write_all(l.as_bytes())?;
            f.write_all(b"\n")?;
        }
        f.flush()?;
    }
    fs::rename(&tmp, path)
}

/// 行として読めるか (外から手を入れられても壊れた JSON を返さないための歯止め)。
pub(super) fn is_line(l: &str) -> bool {
    l.len() <= MAX_LINE * 2
        && l.starts_with('{')
        && l.ends_with('}')
        && !l.chars().any(|c| (c as u32) < 0x20)
}

/// ファイルの行 (古い順)。読めない行は落とす。
pub(super) fn read_lines(path: &Path) -> Vec<String> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    // 外から大きくされても効かないよう、上限の 2 倍までしか見ない
    let cap = (MAX_BYTES * 2) as usize;
    let bytes = if bytes.len() > cap {
        &bytes[bytes.len() - cap..]
    } else {
        &bytes[..]
    };
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| is_line(l))
        .collect()
}

/// `"key":value` の value を切り出す (この形式の行だけを相手にする小さな読み取り)。
pub(super) fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{}\":", key);
    let rest = &line[line.find(&pat)? + pat.len()..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim().trim_matches('"'))
}

/// ファイルの最後の行の日 (無ければ 0)。
pub(super) fn last_day(path: &Path) -> u64 {
    read_lines(path)
        .last()
        .and_then(|l| field(l, "t"))
        .and_then(|v| v.parse::<u64>().ok())
        .map(|t| t / SECS_PER_DAY)
        .unwrap_or(0)
}

/// 新しい方から `n` 行まで、改行込みで `budget` バイトに収まるだけ取る (並びは古い順。T.34.46)。
///
/// 返すのは (取った行, 件数か大きさで古い行を落としたか)。
pub(super) fn newest_within(all: Vec<String>, n: usize, budget: usize) -> (Vec<String>, bool) {
    let count = all.len();
    let take = n.min(count);
    let mut lines = Vec::with_capacity(take);
    let mut used = 0usize;
    let mut truncated = count > take;
    for l in all.into_iter().rev().take(take) {
        if used + l.len() + 1 > budget {
            truncated = true;
            break;
        }
        used += l.len() + 1;
        lines.push(l);
    }
    lines.reverse();
    (lines, truncated)
}
