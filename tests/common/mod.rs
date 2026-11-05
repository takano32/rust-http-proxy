//! 結合テストの共通の道具立て (モックのオリジン、テスト用プロキシの起動、応答の読み取り)。
//!
//! `tests/*.rs` はそれぞれ別のテストバイナリなので、使わない道具が出るのは普通。
#![allow(dead_code)]
// 下の `pub use` の glob も、そのバイナリで使わなければ「使っていない」と言われるので黙らせる
#![allow(unused_imports)]

mod capture;
mod client;
mod counters;
pub mod json;
mod net;
mod origin;
mod process;
mod proxy;
mod scratch;
mod wait;

pub use capture::*;
pub use client::*;
pub use counters::*;
pub use net::*;
pub use origin::*;
pub use process::*;
pub use proxy::*;
pub use scratch::*;
pub use wait::*;
