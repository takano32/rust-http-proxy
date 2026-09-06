#!/bin/sh
# musl で静的リンクしたバイナリを作る (どのディストリビューションでもそのまま置いて動く)。
#
#   ./scripts/build-static.sh [TARGET]     # 既定は x86_64-unknown-linux-musl
#
# これは「どのディストリビューションにも 1 ファイルで置きたい」ときの選択肢で、既定ではありません。
# 引き換えに失うもの:
#   1. TLS: dlopen が使えないので libssl を実行時に読み込めません。https:// オリジンの取得と
#      キャッシュが無効になります (CONNECT トンネル = ブラウザの HTTPS は影響なし)。
#   2. 名前解決: musl は NSS を通さず /etc/resolv.conf だけを見ます (systemd-resolved / mDNS が効かない)。
#   3. malloc: musl の malloc はマルチスレッドで glibc より遅く、1 要求あたりの確保回数が
#      多いこの実装では効きます。
# 迷ったら glibc のビルド (cargo build --release) か Dockerfile を使ってください。
set -eu

target="${1:-x86_64-unknown-linux-musl}"

rustup target add "$target"
RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target "$target"

bin="target/$target/release/rust-http-proxy"
echo "built $bin"
ls -l "$bin"
file "$bin" 2>/dev/null || true
