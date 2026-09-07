//! いちばん下の道具立て。ロック、壁時計、JSON の組み立て、`.env` の読み取り、ログ、
//! HTTP 日付、コマンドライン引数。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod cli;
pub mod clock;
pub mod envfile;
pub mod httpdate;
pub mod json;
pub mod log;
pub mod sync;
