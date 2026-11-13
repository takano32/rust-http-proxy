# TODO.md — 認証なし・手軽・最速の HTTP Proxy にするための入り口

この文書は**入り口の索引だけ**を持つ。タスクは台帳に、振る舞いの決まりは仕様に、それ以外は文書に置く
(2026-11-13、T37.5 で章を文書に分けた。利用者の指示「タスクと仕様、それからドキュメントのみで表現しよう」)。

| どこ | 何が書いてあるか |
|---|---|
| [`AGENTS.md`](AGENTS.md) | エージェントが最初に読むもの (言葉の決まり・指し方・進め方の決まり) |
| [`TASKS.md`](TASKS.md) | **未着手のタスクだけ**と、台帳の決まり。済んだらタスクごと `tasks/TNN.md` へ移す (番号は使い回さない) |
| [`tasks/`](tasks/README.md) | 済んだタスクの本文 (`tasks/TNN.md`。1 文字も変えずに移したもの)。どの番号がどのファイルかは `tasks/README.md` |
| [`SPECS.md`](SPECS.md) | **仕様** `S1`〜 (振る舞いの決まり)。大きい仕様は `specs/Sn.md` に分ける。完了の定義は S24、守ること (認証なし・外部クレートなし・ビルドのメモリ) は S25 |
| [`README.md`](README.md) | 使い方の説明 (起動・運用・道具) |
| [`docs/goals.md`](docs/goals.md) | ゴールと前提 (到達・作業の決まり・作業の進め方) |
| [`docs/measuring.md`](docs/measuring.md) | 計測の作法 (速さ・デプロイ先に似せた条件・デプロイ先・ビルドのメモリの測り方) |
| [`docs/status.md`](docs/status.md) | 現在地 (手元の loopback の表と読み方、**デプロイ先の現在地**) |
| [`docs/rejected.md`](docs/rejected.md) | 測って採らなかったもの (同じ道を二度調べないための表) |
| [`docs/handoff.md`](docs/handoff.md) | タスクの形式と渡し方 (タスク 15 の渡し方・実装を Opus に渡すときの決まり) |
| [`docs/known-issues.md`](docs/known-issues.md) | 既知の小物 |
| [`docs/task-index.md`](docs/task-index.md) | 済んだタスクの見出しと索引 (`## タスク N (…)` ごとの「済んだ N 本の本文は …」) と、やったこと |
| [`docs/measurements.md`](docs/measurements.md) | 計測の記録 (時系列) と、デプロイ先の現在地の古い版 |
| [`docs/perf.md`](docs/perf.md) ほか `docs/*.md` | 性能の数字の読み方と道具の説明 (README から分けたもの) |

## 以前の章といまの文書

済んだタスクの記録 (`tasks/TNN.md`) と古いコミットメッセージは、この文書を章の記号で指している (記録は 1 文字も変えない決まり)。
その行き先は次のとおり (この表だけは古い記号をそのまま書く)。

| 以前の章 | いまの置き場 |
|---|---|
| 冒頭の「この文書の形式」 | [`docs/handoff.md`](docs/handoff.md) の「タスクの形式」 |
| 冒頭の雪像の置き場と `mx` の注 | [`docs/measuring.md`](docs/measuring.md) の冒頭 |
| §0 ゴールと前提 | [`docs/goals.md`](docs/goals.md)。「守ること」のうち振る舞いとビルドの約束は仕様 S25 |
| §1 計測の作法 | [`docs/measuring.md`](docs/measuring.md) |
| §2 現在地 (「デプロイ先の現在地」を含む) | [`docs/status.md`](docs/status.md)。「完了の定義」は仕様 S24 |
| §3 やったこと | [`docs/task-index.md`](docs/task-index.md) の「やったこと」 |
| §4 測って採らなかったもの | [`docs/rejected.md`](docs/rejected.md) |
| §5 これから: タスク 15 の渡し方・実装を Opus に渡すときの決まり | [`docs/handoff.md`](docs/handoff.md) |
| §5 これから: 既知の小物 | [`docs/known-issues.md`](docs/known-issues.md) |
| §5 これから: `### タスク N (…)` の索引 | [`docs/task-index.md`](docs/task-index.md) |
| 付録 A 計測の記録 (デプロイ先の現在地の古い版を含む) | [`docs/measurements.md`](docs/measurements.md) |

このファイルは 2026-09-19 まで `TASKS.md` という名前だった (T15.14 で改名)。**2026-10-22 に作った `TASKS.md` は役割が違う** (未着手だけを持つ新しい台帳)。当時の `TASKS.md` はこのファイルのことで、過去のコミットメッセージに出てくる `TASKS.md` はその意味。
