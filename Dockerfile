# 2 段ビルド: rust でビルドして、バイナリだけを scratch に置く。
FROM rust:1.96 AS build
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
RUN rustup target add x86_64-unknown-linux-musl \
 && RUSTFLAGS="-C target-feature=+crt-static" \
    cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/rust-http-proxy /rust-http-proxy
# 静的リンクなので libssl を dlopen できません (https:// オリジンのキャッシュのみ無効。
# CONNECT トンネルは影響なし)。
ENV SERVER_PORT=8080
EXPOSE 8080
ENTRYPOINT ["/rust-http-proxy"]
