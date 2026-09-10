# 2 段ビルド: rust でビルドして、バイナリと libssl だけの小さな実行イメージに置く。
#
# glibc のままなのは、musl 静的リンクにすると (1) forward が実測で 1/3 になり、
# (2) dlopen が使えず TLS = https:// オリジンの取得とキャッシュが無効になり、
# (3) 名前解決が NSS を通らず /etc/resolv.conf だけになるためです (README の「配布」を参照)。
FROM rust:1.96 AS build
WORKDIR /src
COPY Cargo.toml ./
# build.rs は版の文字列を作るだけ。.git は入れないので `0.1.0+unknown` になる
COPY build.rs ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
# libssl3 は https:// オリジンの取得に実行時 dlopen で使う。ca-certificates は証明書検証用
RUN apt-get update \
 && apt-get install -y --no-install-recommends libssl3 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/rust-http-proxy /usr/local/bin/rust-http-proxy
ENV SERVER_PORT=8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/rust-http-proxy"]
