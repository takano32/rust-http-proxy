//! HTTP 本文の枠組み (framing): 解読と再エンコード、`Range` の解釈。
//!
//! オリジンからの応答は必ず解読 (chunked を外す) してから扱う。クライアントへは
//! 自前で枠を付け直し (Content-Length / 再 chunk / close)、キャッシュには生の本文だけを置く。
//!
//! - `framing`: 枠組みの決め方 ([`Framing`]。応答の先頭の 1 回の走査 [`Framing::scan_response_head`])
//! - `reader`: 枠組みに従って本文を解読する [`BodyReader`] と、読み捨て・chunk の書き出し
//! - `range`: `Range` 要求の解釈 ([`RangeSpec`])

mod framing;
mod range;
mod reader;

#[cfg(test)]
mod of_response_head_tests;
#[cfg(test)]
mod tests;

pub use framing::{BadLength, Framing, ResponseScan};
pub use range::{RangeSpec, parse_range};
pub use reader::{
    BodyReader, MAX_CHUNK_SIZE_LINE, MAX_TRAILER_BYTES, MAX_TRAILER_LINE, drain, write_chunk,
    write_last_chunk,
};
