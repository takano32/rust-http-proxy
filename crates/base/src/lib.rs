//! いちばん下の道具立て。ロック、壁時計、JSON の組み立て、`.env` の読み取り、ログ、
//! HTTP 日付、コマンドライン引数、タイムアウトの約束事 (`0` = 無期限)、起動ごとの `Via` の印、
//! 記録を止める / 接続元をハッシュにする旗 (`PROXY_RECORDS`。T14.41)。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 180 MB)。**外部クレートは 1 つも使っていない。**

pub mod ascii;
pub mod cli;
pub mod clock;
pub mod envfile;
pub mod httpdate;
pub mod json;
pub mod log;
pub mod records;
pub mod sync;
pub mod timeout;
pub mod via;
