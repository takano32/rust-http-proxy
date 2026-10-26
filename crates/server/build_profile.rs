//! ビルドの種類 (cargo のプロファイル名) を `OUT_DIR` から読む (T.28.42)。
//!
//! `build.rs` と `src/lib.rs` の試験の両方が `#[path]` で読む (ビルドスクリプトは
//! 単体の試験を持てないので、関数だけをこのファイルに分けた)。
//!
//! cargo がビルドスクリプトに渡す `PROFILE` は `debug` か `release` の 2 つだけで、
//! `[profile.dist]` (`inherits = "release"`) で作っても `release` になる。そこで
//! **`OUT_DIR` の道のり**から取る: 形は `<target>/[<triple>/]<プロファイルの置き場>/build/<pkg>-<hash>/out`
//! で、置き場の名前は `dev` / `test` が `debug`、それ以外はプロファイル名そのもの
//! (`release`・`dist`)。形が違えば (cargo が置き場を変えた) `PROFILE` に倒す。

/// `OUT_DIR` からプロファイルの置き場の名前を取る。読めなければ `profile_env` (`PROFILE`)、
/// それも空なら `unknown`。
pub fn profile_from_out_dir(out_dir: &str, profile_env: &str) -> String {
    let path = std::path::Path::new(out_dir);
    let mut up = path.ancestors();
    let out = up.next().and_then(|p| p.file_name());
    let _pkg = up.next();
    let build = up.next().and_then(|p| p.file_name());
    let dir = up
        .next()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str());
    match (out, build, dir) {
        (Some(o), Some(b), Some(d)) if o == "out" && b == "build" && !d.is_empty() => d.to_string(),
        _ if !profile_env.is_empty() => profile_env.to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::profile_from_out_dir;

    /// release と dist で値が変わる (cargo の `PROFILE` はどちらも `release`)
    #[test]
    fn release_and_dist_are_told_apart() {
        assert_eq!(
            profile_from_out_dir(
                "/src/target/release/build/proxy-server-0123abcd/out",
                "release"
            ),
            "release"
        );
        assert_eq!(
            profile_from_out_dir(
                "/src/target/dist/build/proxy-server-0123abcd/out",
                "release"
            ),
            "dist"
        );
        // `--target` を付けたときは三つ組が 1 段挟まる
        assert_eq!(
            profile_from_out_dir(
                "/src/target/aarch64-unknown-linux-gnu/dist/build/proxy-server-0123abcd/out",
                "release"
            ),
            "dist"
        );
        // `dev` / `test` の置き場は `debug`
        assert_eq!(
            profile_from_out_dir("/src/target/debug/build/proxy-server-0123abcd/out", "debug"),
            "debug"
        );
        // 形が違えば `PROFILE`、それも無ければ `unknown`
        assert_eq!(
            profile_from_out_dir("/somewhere/else", "release"),
            "release"
        );
        assert_eq!(profile_from_out_dir("", ""), "unknown");
    }
}
