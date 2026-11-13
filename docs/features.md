# 特徴 (詳しい説明) の移り先

この文書の本文 (README の「特徴」の詳しい説明。T29.32 で README から分けたもの) は、T37.4 で仕様 [`SPECS.md`](../SPECS.md) に移した。
済んだタスクの記録 (`tasks/TNN.md`) がこの文書を指しているので、置き場の案内だけを残す。

| 移す前の項目 | 仕様 |
|---|---|
| 認証不要・依存クレートゼロ・超軽量バイナリ・HTTP / HTTPS 対応・RFC 7230 / RFC 9110 準拠 | S1 |
| keep-alive と接続プール・IPv4 / IPv6 デュアルスタック・タイムアウト制御 | S2 |
| ACL / ホストフィルタリング・ドメインのブロックリスト・`/proxy.pac`・`/blocklist` | S4 |
| 同時ミスの合流・Range / HEAD・HTTPS のオリジンもキャッシュ・2 段キャッシュ・入場制御 & ネガティブキャッシュ・`PURGE` と `/lookup` | S5 ([`specs/S5.md`](../specs/S5.md)) |
| DNS キャッシュ | S6 |
| `.env` の自動再読込 | S7 ([`specs/S7.md`](../specs/S7.md)) |
| 詳細なアクセスログ | S9 |
| 統計と履歴の永続化・日次の snapshot・個票の永続化・状態ファイルの版 | S10 |
| `/`・応答の形の版 `schema`・`/config`・RSS の内訳・直近の窓の正確な分位点・起動直後の自己ベンチ・いまの転送速度 | S11 ([`specs/S11.md`](../specs/S11.md)) |
| `/dashboard`・`/inspect`・`/probe.html` | S12 ([`specs/S12.md`](../specs/S12.md)) |
| `/healthz`・`capabilities` | S13 |
| 接続元別の統計・個票 (`/errors`・`/clients`)・`/readers`・閉じた接続の個票・カーネルの RTT と再送・CONNECT の SNI・確立時の SYN の再送・中継の詰まりの向き・山の写真・接続元 1 つの追跡・再起動をまたぐか・`PROXY_RECORDS` | S14 ([`specs/S14.md`](../specs/S14.md)) |
| 起きたことの時系列・異常の自動検知・初めて見た接続元 | S15 |
| 1 要求で全部取る (`/snapshot`)・日次の snapshot (30 日ぶん) | S16 |
| canary・canary の IPv6 側 | S17 |
| カーネルと cgroup の窓 | S18 |
| ホスト別の時系列・1 相手の説明 | S19 |
| 日次の要約・SLO の達成率 | S20 ([`specs/S20.md`](../specs/S20.md)) |
| `/history` | S21 ([`specs/S21.md`](../specs/S21.md)) |
| `/profile` | S22 |
