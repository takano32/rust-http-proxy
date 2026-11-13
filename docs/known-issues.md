# docs/known-issues.md — 既知の小物

## 既知の小物 (タスク 12〜17 で見つけたもの。旧タスク 14 の表)

| 既知の小物 | 出どころ | 何をするか |
|---|---|---|
| keep-alive の HTTP 接続が `/connections` でバイト数を出さない (宛先は T14.2 (5) / T14.48 で済。`set_bytes` を呼ぶのはトンネルだけ) | T13.4、T17.20 の気づき | 要求ごとに書かない方針とどう両立させるかを先に決める |
| 「束縛してすぐ手放したポート」を死んだ宛先に使うテストが `crates/net-conn/src/net.rs` に 2 か所残っている (`a_deadline_reports_timed_out_not_the_first_refusal` の `closed`、`unseen_hosts…` の後半の `dead`。落ちた記録は無い) | T20.2 の気づき | 落ちたら `tests/proxy_test.rs` の `RefusedPort` (束縛したまま `listen` しない) を `tests/common/` か `net.rs` のテストへ持っていく |
| `crates/metrics-window/src/history.rs` の `summary::of` のコメント「窓は始まりの時刻 (t) で切る」は、5 秒の標本では区間の終わり (取った時刻) | T21.4 の気づき | T22.6 (2) で直した |
| 手でプロキシを起動するとき `HOME` を一時ディレクトリへ向けると、同じシェルの `scripts/mx cargo` が rustup の既定を見失って何も回らない (出力が空) | T21.4 の気づき | 指示文の「$HOME に書かない」の所に「`RUSTUP_HOME` と `CARGO_HOME` は本物のまま」を足す (タスク 22 の指示文から足した) |
| `tests/heavy_test.rs` の `test_integration_only_one_snapshot_is_built_at_a_time` が全体テストの負荷の下で 1 回落ちた (`/snapshot` 4 本を 20 回投げて、1 回も 4 本が同時にならなかった。単独 5 / 5 通過) | タスク 21 の全体チェック | T22.6 (1) で直した (2026-10-17) |
| 本文付きの要求 (POST など) がプールのオリジン接続に乗り、相手がちょうど閉じたときは 502 (送り直すのは本文の無い GET / HEAD だけ。`crates/http/src/http/mod.rs` の `retryable`)。決まりどおりで、実機で出た記録は無い | T18.4 | 実機の `/errors` に POST の 502 が出たら、要求の経路の判断として決める |
| `warm_promote` が次の予定を「上げた時刻 + 45 秒」に置く (答えを引いた時刻ではない。齢 60〜75 秒が期限切れになりうる。実機の `warm_stale` の一部かも) | T17.5 の模型 | T22.4 で数えた: serve-stale のもとではすき間は古い答えになりミスにならない (`warm_stale` 0)。閉じる |
| `tests/stages_test.rs` と `tests/newclient_test.rs` が負荷の下で 1 回ずつ落ちた (`client_read` の 40 ms の窓と、`new_client` の「最初の宛先」が入る前の行を読む) | 波 23a・23b の全体チェック (2026-10-20) | 単独では 2/2 も 5/5 も通り、負荷の無い全体実行は 139 バイナリ全通過。直すなら T17.14 の型で「sleep を伸ばさず値で待つ」に (T23.99 の次のタスクの候補) |
