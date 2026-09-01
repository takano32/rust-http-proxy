# sorahost-http-proxy

`SERVER_PORT` 環境変数でポート番号を指定して起動する、依存クレートゼロ（Rust 標準ライブラリ `std` のみ使用）の軽量 HTTP/HTTPS プロキシサーバーです。

## 特徴

- **依存クレートゼロ**: 外部クレート依存がないため、ビルド負荷が最小限で高速にビルド可能
- **超軽量バイナリ**: リリースビルド時で約 300KB
- **HTTP / HTTPS (CONNECTトンネリング) 対応**
- **環境変数による設定**: `SERVER_PORT` で待受ポートを指定（未指定時は `8080`）

## ビルド

```bash
# 通常ビルド
cargo build

# 最適化リリースビルド
cargo build --release
```

## 起動方法

```bash
# SERVER_PORT を指定して起動
SERVER_PORT=8080 cargo run

# またはリリースバイナリを直接実行
SERVER_PORT=8080 ./target/release/sorahost-http-proxy
```

## 動作確認 (curl)

```bash
# HTTP プロキシ経由のリクエスト
curl -x http://127.0.0.1:8080 http://example.com/ -I

# HTTPS (CONNECT) プロキシ経由のリクエスト
curl -x http://127.0.0.1:8080 https://example.com/ -I
```
