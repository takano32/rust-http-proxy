//! 土台の層。標準ライブラリだけで書かれた、上の層に依存しない部品を集める。
//!
//! ここに置くものの条件は「ソケットも HTTP も知らないこと」。ログ、設定ファイルの
//! 読み取り、時刻、システム情報、シグナル、固定長リングバッファまで。
//! ソケットと HTTP の素の部品は [`proxy_net`](../proxy_net/index.html) にある。
//!
//! **なぜクレートを分けるか**: 1 クレートが大きいと `rustc` が全部を一度に抱えるため、
//! ビルドの最大 RSS がそのまま増える (実測: 18,276 行 1 クレートで 330 MB)。
//! 動作環境の上限は 200 MB なので、層ごとに別クレートにして 1 プロセスあたりの
//! 使用量を下げている。外部クレートは 1 つも増やしていない。

pub mod cli;
pub mod clock;
pub mod envfile;
pub mod httpdate;
pub mod json;
pub mod log;
pub mod rrd;
pub mod signal;
pub mod sync;
#[cfg(target_os = "linux")]
pub mod sys;
pub mod sysinfo;
pub mod workers;
