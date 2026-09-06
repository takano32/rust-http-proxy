#!/bin/sh
# musl で静的リンクしたバイナリを作る (どのディストリビューションでもそのまま置いて動く)。
#
#   ./scripts/build-static.sh [TARGET]     # 既定は x86_64-unknown-linux-musl
#
# 注意: musl の静的リンクでは dlopen が使えないので TLS (libssl) を実行時に読み込めません。
# 起動ログに "TLS: unavailable in static build" が出ます。https:// オリジンのキャッシュだけが
# 無効になり、CONNECT トンネル (ブラウザの HTTPS) は影響を受けません。
set -eu

target="${1:-x86_64-unknown-linux-musl}"

rustup target add "$target"
RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target "$target"

bin="target/$target/release/rust-http-proxy"
echo "built $bin"
ls -l "$bin"
file "$bin" 2>/dev/null || true
