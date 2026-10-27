#!/bin/bash
# デプロイ先の様子を **1 回で取って 1 枚に読む** (TODO.md T.14.4)。
#
# `/snapshot` (T.14.4) を保存し、前回の雪像との差分 (`snapshot-diff.py` と `status-diff.py`)、
# ダッシュボードの読み方の確認 (`check-dashboard.js`)、手元から見た待ち (`probe-deployed.sh`) を
# 続けて回して、**Markdown 1 枚**を標準出力に出す。T.14.0 で 17 本の URL を手で叩いていた作業の代わり。
# **前回の雪像があれば `snapshot-diff.py` (T.14.17) を呼ぶ** (再起動で切った平常時の前後・ホスト別・
# 接続元別・名前解決・エラー・バースト)。**完了の定義に対する判定表は要約のいちばん最後**に置く。
# 判定表の相手は既定では前回の雪像で、`BEFORE=<再デプロイ直前の雪像>` を渡すとそちらになる (T.18.2)。
#
# 使い方:
#   scripts/collect-deployed.sh HOST:PORT [DIR]
#     例: scripts/collect-deployed.sh tokyo.sorahost.net:60357
#         scripts/collect-deployed.sh 127.0.0.1:8080 /tmp/snaps      # 手元のプロキシで試す
#   保存先の既定はリポジトリの `status/` (`<UTC 時刻>-snapshot.json`、秒まで)。
#   同じ時刻の名前で、要約 (`-collect.md`)・道具の文句 (`-collect.err`、空なら残さない)・雪像に入らない口
#   (`-daily.json` `-healthz.json` `-slo.json` `-config.json` `-profile_res_60.json`) も隣に置く。
#   `-profile_res_60.json` は 1 枚 (256 KiB) で切れるので、`--full` が無くても `offset=` で追って
#   24 時間ぶんを 1 つに繋ぐ (T17.0b。`samples` を連結して `truncated` は `false`)。
#   **雪像の `recent` と `hosts` も、切れていれば既定で `offset=` で追って雪像の同じ部に繋ぐ** (T.22.5。
#   T.27.1 から `dns` と `profile` も。形は `/snapshot` のまま。上限は部ごとに `MAX_PAGES` 枚、続きを引く時間の合計 `FOLLOW_SECS` 秒で、
#   越えたら止めて要約に書く)。`FULL=0` か `--no-follow` で T.22.5 より前のとおり追わない。
#   **`status/` は .gitignore 済みでコミットしない** (個票には接続元 IP と宛先ホストが並ぶため)。
#   テストへ持ち込めるように、**匿名化した写し** (`-snapshot.anon.json`、`-daily.anon.json`、
#   `-profile_res_60.anon.json`) も隣に置く (T.17.16。`anonymize-snapshot.py --side` で同じ表を使う)。
#   `$HOME` には書かない (2026-09-26、利用者の決定。前の既定は ~/rust-http-proxy-status/)。
#
#   scripts/collect-deployed.sh --from-server HOST:PORT [DIR]
#     プロキシ自身が 1 日 1 回書いている日次の snapshot (T.14.34) のうち、**手元に無い日付だけ**を
#     `/snapshots` の一覧から取り寄せる (取りに行くのは `/snapshots/<date>` で、中身は
#     `/snapshot` そのもの)。名前は今の流儀に合わせて `<日付>T000000Z-snapshot.json`
#     (日付は**その 1 日を写したもの**。実際に撮られたのは翌日 00:00 UTC の直後)。
#     回し忘れた日の個票がこれで埋まる。
#     **起動と停止の 1 枚** (T.28.20。プロキシが停止シグナルで状態ファイルを書く前と、起動から 60 秒で
#     残す `<UTC 時刻>-shutdown` / `-start`。一覧の `moments`) も手元に無いものだけ取り寄せて
#     `<UTC 時刻>-shutdown-snapshot.json` / `-start-snapshot.json` に置き、**最後の停止とその後の
#     最初の起動**を `BEFORE=… ZERO=…` の形で 1 行出す (締めの「前」と「0 時間」にそのまま渡せる)。
#     T.29.26 から**版で選ぶ**: 前 = いまの版と系列 (`0.<Phase>`) の違う版の最後の停止 (同じ系列の途中の
#     デプロイは飛ばす)、0 時間 = その後のいまの版の最初の起動。候補は全部、選んだ / 外した理由つきで並べる。
#
#   scripts/collect-deployed.sh --closeout HOST:PORT [DIR]
#     締めを 1 コマンドで (T.29.26): `--from-server` で取り寄せ → 上の決まりで「前」と「0 時間」を選ぶ →
#     それを `BEFORE` / `ZERO` にして普段の 1 回 (本番) → 要約の頭に**前提の 1 行** (版・ビルド・起動時刻・
#     `boot.id`・前の版の停止時刻) と取り寄せ・選択の箇条。`BEFORE` / `ZERO` を渡せばそちらを使う。
#
#   scripts/collect-deployed.sh --full HOST:PORT [DIR]
#     雪像で `truncated` が立った部 (`recent` / `hosts` / `profile`) の**続き**を `offset=` で
#     追って `<UTC 時刻>-page<何枚目>-<部>.json` に落とす (T.15.0 (11))。雪像 1 枚には `/recent` は
#     2,000 件中 615 件、`/hosts` は 1,000 件中 639 件、`/profile?res=5` は 720 標本中 456 (T.15.0 の実測) しか
#     入らないので、全部を残したい日だけ付ける。`recent` と `hosts` は T.22.5 から、`profile` は T.27.1 から
#     既定で雪像に繋ぐので、別ファイルに落ちるのはそのあとも切れている部だけ (上限で止めた残り)。
#     `--full --no-follow` で T.22.5 より前の `--full` (3 つとも別ファイル) になる。
#     `snapshot-summary.py` に渡す雪像の形は変わらない (落とすのは別ファイル)。
#     **名前が `page<N>-<部>` の順なのはわざと**: `<部>-<N>` にすると
#     `snapshot-diff.py --from-files` と `anonymize-snapshot.py` の `classify()` が
#     「`<UTC 時刻>-` の後ろが部の名前で始まれば 1 口 1 ファイル」と見分けるので、
#     続きの 1 枚を部そのものと取り違える (`page2-hosts.json` はどちらも読み飛ばす)。
#     `--from-server` とは併用できない (あちらは保存済みの雪像を取り寄せるだけ)。
#
# 環境変数:
#   VIA (無指定)        … デプロイ先への curl を全部 `-x "$VIA" -p` (CONNECT のトンネル) で通す (T.27.2)。
#                         クラウドのセッションのエージェントのプロキシは平文の HTTP を通さず CONNECT だけを
#                         通すので、`VIA=$HTTPS_PROXY` で回す。**`VIA` があるときの `PROBE` の既定は 0**
#                         (探りはデプロイ先をプロキシとして使う要求で、トンネルの中では測れない)
#   PROBE (既定 1)      … 0 で `probe-deployed.sh` を飛ばす (デプロイ先へ本物の要求を
#                         15 本送るので、何度も回すときは 0 にする)
#   DASHBOARD (既定 1)  … 0 で `check-dashboard.js` を飛ばす (Node が無ければ自動で飛ばす)
#   DIFF (既定 1)       … 0 で前回との差分を飛ばす
#   CRITERIA (既定 phase26) … 判定表に使う完了の定義 (`phase25` `phase24` `phase23` `phase22` `phase21` `phase20` `phase18` `phase17`
#                         `phase15` `phase14` も残してある)。phase20 は phase18 の物差しを直したもの (T.20.1)、
#                         phase21 は phase20 のミス・conn 役・IPv6 の探りの 3 行の読み方を直したもの (T.21.2)、
#                         phase22 は phase21 の 10 行 + 22a の 4 行で、平常時を「300 本と期間の中央値の
#                         3 倍の大きい方」未満で切るもの (T.22.1)、phase23 は phase22 の 14 行のうち接続の
#                         エラーの行を「名前解決に成功した試行あたり」(`/recent` から `dns` の失敗を外した
#                         分母) に差し替え、heap の行に `/events` の `start` の時刻を添え、無言の再起動の
#                         行を足した 16 行 (T.23.1)、phase24 は phase23 の 16 行を持ち越し、heap の行を
#                         「起動から 60 秒の基準 (`/status` の `memory.baseline`) からの増え」に、無言の
#                         再起動の行を「こちらが原因 (同じ `boot` のまま起動し直した回) が 0 回」に
#                         読み替えたもの (T.24.2)、phase25 は phase24 の 17 行を持ち越し、接続のエラーの行に
#                         試行 1,000 本の下限、引き直しの行を「1 名前あたり」、無言の再起動の行を「前の雪像より
#                         後の `start` だけ」に読み替えたもの (T.27.0)、phase26 は phase25 の 17 行に、T.28.99 で
#                         手で読んだ 408・`/log` の error・`/errors` の `local`・3 秒で切った確立・
#                         `rejected_requests` と、T.29.99 の RSS・CPU・`/log` の 502 の 9 行を足したもの (T.29.10)。
#                         `off` で判定表を出さない
#   BEFORE (無指定)     … **判定表の相手**にする雪像 (再デプロイの直前に撮った「前」。T.18.2)。
#                         無ければ今までどおり直前の 1 枚 (再デプロイのあとは 0 時間の雪像になるので、
#                         T.17.99 では手で `snapshot-diff.py` を回した)。隣に同じ時刻の
#                         `-profile_res_60.json` があれば `--profile-before` で渡す。
#                         「2. 前回との差分」とホスト別の相手は変わらない (直前の 1 枚のまま)
#   ZERO (無指定)       … 0 時間の雪像 (再デプロイの直後に撮った 1 枚)。`snapshot-diff.py --zero` に
#                         渡す (phase25 / phase24 / phase22 / phase21 / phase20 / phase18 の `heap_used + mmap` の増えの行と、
#                         phase22 / phase21 の IPv6 の探りの行。phase24 は `memory.baseline` が無い雪像のときだけ
#                         こちらで読む。無ければ「判定できず」)。
#                         BEFORE も ZERO も、相対パスはリポジトリの根から (`DIR` と同じ)
#   MAX_TIME (既定 30)  … `/snapshot` を取る上限 (秒)。4 MiB まであるので長めに
#   MAX_PAGES (既定 8)  … `--full` が 1 つの部について追う続きの枚数の上限。`/profile?res=60` と
#                         既定で追う `recent`・`hosts`・`dns`・`profile` を繋ぐ枚数の上限 (1 枚目を含む) にも使う
#                         (2,000 件の `recent` は 1 枚 576 件の今の tokyo で 4 枚)
#   FOLLOW_SECS (既定 60) … 既定で追う `recent`・`hosts`・`dns`・`profile` の続きを引く時間の合計の上限 (秒)。
#                         越えたら次の 1 枚を引かずに止める (T.22.5)
#   FULL (無指定)       … `0` で `--no-follow` と同じ (切れた部をどれも追わない)、`1` で `--full` と同じ
#   AAAA (無指定)       … `status-diff.py --aaaa FILE` に渡す表 (数字を残すときは固定する。§1)
#   ANON (既定 1)       … 0 で匿名化した写し (`<時刻>-snapshot.anon.json` ほか) を置かない
#
# 出口: 雪像が取れなければ 1 (それ以外は、途中の道具が失敗しても 1 枚は出す)。
set -u
# 要約をファイルにも残すため、自分をもう 1 回呼ぶ (下の「1. 取る」の手前)。`cd` の前に絶対パスにしておく
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
ORIG_ARGS=("$@")
cd "$(dirname "$0")/.." || exit 1
FROM_SERVER=0
CLOSEOUT=0
# 切れた部の追い方は 3 段 (T.22.5): `FOLLOW=1` (既定) は `recent`・`hosts`・`dns`・`profile` を追って雪像に繋ぐ、
# `FULL=1` (`--full`) はそのうえで残りの部 (`profile` ほか) も別ファイルに落とす、
# `FULL=0` (`--no-follow`) は T.22.5 より前の既定 (どれも追わない) に戻す
FOLLOW=1
FULL_ENV=${FULL:-}
FULL=0
case "$FULL_ENV" in
  0) FOLLOW=0 ;;
  1) FULL=1 ;;
esac
while :; do
  case "${1:-}" in
    --from-server)
      FROM_SERVER=1
      shift
      ;;
    --closeout)
      CLOSEOUT=1
      shift
      ;;
    --full)
      FULL=1
      shift
      ;;
    --no-follow)
      FOLLOW=0
      shift
      ;;
    *) break ;;
  esac
done
PROXY=${1:-}
if [ -z "$PROXY" ]; then
  # **2 つの旗は排他** (`--from-server` は保存済みの雪像を取り寄せるだけで、続きを引く相手が居ない)
  echo "usage: $0 [--full] [--no-follow] HOST:PORT [DIR]   (例: $0 tokyo.sorahost.net:60357)" >&2
  echo "       $0 --from-server HOST:PORT [DIR]   (回し忘れた日を取り寄せる。--full は効きません)" >&2
  echo "       $0 --closeout HOST:PORT [DIR]      (締め: 取り寄せ → 前と 0 時間を版で選ぶ → 本番 → 要約の頭に前提)" >&2
  exit 2
fi
# `http://host:port` と書かれても `host:port` として扱う (URL でも通るように)
PROXY=${PROXY#http://}
PROXY=${PROXY%/}
# 相対パスはリポジトリの根から (上の `cd`)
DIR=${2:-status}
VIA=${VIA:-}
# `VIA` (T.27.2) のときは探りを既定で飛ばす (トンネルの中ではデプロイ先をプロキシとして使えない)
if [ -n "$VIA" ]; then
  PROBE=${PROBE:-0}
else
  PROBE=${PROBE:-1}
fi
DASHBOARD=${DASHBOARD:-1}
DIFF=${DIFF:-1}
CRITERIA=${CRITERIA:-phase26}
# 判定表の相手 (「前」の雪像) と 0 時間の雪像 (T.18.2)。どちらも無指定なら今までどおり
JUDGE_BEFORE=${BEFORE:-}
ZERO=${ZERO:-}
MAX_TIME=${MAX_TIME:-30}
MAX_PAGES=${MAX_PAGES:-8}
FOLLOW_SECS=${FOLLOW_SECS:-60}
AAAA=${AAAA:-}
ANON=${ANON:-1}

mkdir -p "$DIR" || exit 1

# デプロイ先への curl は全部ここを通る (T.27.2)。`VIA` があれば CONNECT のトンネル (`-p`) を張る。
# `-p` が無いと curl は `GET http://…` を絶対形式でプロキシに送り、CONNECT だけを通すプロキシに断られる
curl() {
  if [ -n "$VIA" ]; then
    command curl -x "$VIA" -p "$@"
  else
    command curl "$@"
  fi
}

# いまの時刻 (ミリ秒)。取得にかかった時間を要約に書く (T.22.5)。bash 5 の `EPOCHREALTIME` が
# 無ければ `date` (小数点が `,` の地域設定でも読めるように、区切りは数字以外を全部落とす)
now_ms() {
  local t=${EPOCHREALTIME:-}
  if [ -n "$t" ]; then
    t=${t//[!0-9]/}
    echo $((10#$t / 1000))
  else
    echo $(($(date +%s%N) / 1000000))
  fi
}

# --- 0. 保存済みの日次 snapshot を取り寄せる (--from-server。T.14.34) ----------
# プロキシが `$HOME/.rust-http-proxy/snapshots/` に 1 日 1 ファイル書いているので、
# 手元に無い日付だけを取って `$DIR` に置く (`/snapshots` の一覧 → `/snapshots/<date>`)。
# --- 締めを 1 コマンドで (--closeout。T.29.26) ----------------------------------
# 取り寄せ (`--from-server`) → 「前」と「0 時間」を版で選ぶ (下の 0. の終わり) → 本番 (`BEFORE` / `ZERO` を
# 付けた普段の 1 回) → 要約の頭に**前提の 1 行** (版・ビルド・起動時刻・`boot.id`・前の版の停止時刻) を出す。
# 前は 2 枚を選び、前提を手で打っていた (T.28.99)。`BEFORE` / `ZERO` を自分で渡したときはそちらを使う。
# いまの版は `/status` の `version` (読めなければ手元の最後の起動の版)
if [ "$CLOSEOUT" = 1 ]; then
  if [ "$FROM_SERVER" = 1 ]; then
    echo "--closeout は --from-server を含みます (2 つ並べない)" >&2
    exit 2
  fi
  CO=$(mktemp -d) || exit 1
  # shellcheck disable=SC2064  # いまの $CO で張る
  trap "rm -rf '$CO'" EXIT
  CUR=$(curl -s --max-time "$MAX_TIME" "http://$PROXY/status" | python3 -c '
import json, sys
try:
    v = json.load(sys.stdin).get("version")
except (ValueError, AttributeError):
    v = None
print(v if isinstance(v, str) else "")
' 2>/dev/null) || CUR=
  CURRENT_VERSION=$CUR CLOSEOUT_PICK="$CO/pick" "$SELF" --from-server "$PROXY" "$DIR" >"$CO/fetch.md"
  fetch_rc=$?
  touch "$CO/pick"
  pick_before=$(sed -n 's/^BEFORE=//p' "$CO/pick")
  pick_zero=$(sed -n 's/^ZERO=//p' "$CO/pick")
  premise=$(sed -n 's/^PREMISE=//p' "$CO/pick")
  {
    printf '**締めの前提** (`--closeout`): %s\n\n' "${premise:-組めなかった (起動と停止の 1 枚が手元に無い)}"
    # 取り寄せの要約の見出し (1 行目と空行) は落とし、箇条だけを普段の要約の頭の箇条に並べる
    tail -n +3 "$CO/fetch.md"
    [ "$fetch_rc" = 0 ] || printf -- '- **取り寄せが exit %s で終わった** (上の「取れなかった」を見る)\n' "$fetch_rc"
    [ -n "$JUDGE_BEFORE" ] && printf -- '- `BEFORE` は渡されたもの (`%s`) を使う (選んだのは `%s`)\n' "$JUDGE_BEFORE" "${pick_before:-無し}"
    [ -n "$ZERO" ] && printf -- '- `ZERO` は渡されたもの (`%s`) を使う (選んだのは `%s`)\n' "$ZERO" "${pick_zero:-無し}"
  } >"$CO/head.md"
  args=()
  [ "$FULL" = 1 ] && args+=(--full)
  [ "$FOLLOW" = 0 ] && args+=(--no-follow)
  BEFORE=${JUDGE_BEFORE:-$pick_before} ZERO=${ZERO:-$pick_zero} CLOSEOUT_HEAD="$CO/head.md" \
    "$SELF" "${args[@]}" "$PROXY" "$DIR"
  exit $?
fi

if [ "$FROM_SERVER" = 1 ]; then
  # `--full` はこの枝では効かない (取り寄せるのは保存済みの雪像そのもので、
  # `offset=` で続きを引く相手が居ない)。黙って無視しないで 1 行断る
  if [ "$FULL" = 1 ]; then
    echo "note: --full は --from-server では効きません (保存済みの雪像をそのまま取り寄せます)" >&2
  fi
  LIST=$(curl -s --max-time "$MAX_TIME" "http://$PROXY/snapshots") || LIST=
  if [ -z "$LIST" ]; then
    echo "failed to fetch http://$PROXY/snapshots" >&2
    exit 1
  fi
  # 1 行 1 枚 (`<日付> <バイト数> <置く名前>`)。日次は `<日付>T000000Z-snapshot.json`、起動と停止の
  # 1 枚 (T.28.20。`moments` が無い古い版のプロキシでは 0 枚) は `<名前>-snapshot.json`。
  # **名前は形を確かめてから使う** (一覧に何が書かれていても `$DIR` の外へは置かない)
  DAYS=$(printf '%s' "$LIST" | python3 -c '
import json, re, sys
try:
    d = json.load(sys.stdin)
except ValueError:
    sys.exit("not JSON")
for f in d.get("files") or []:
    day = str(f.get("date"))
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}", day):
        print(day, f.get("bytes", 0), day + "T000000Z-snapshot.json")
for m in d.get("moments") or []:
    name = str(m.get("name"))
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{6}Z-(start|shutdown)", name):
        print(name, m.get("bytes", 0), name + "-snapshot.json")
') || exit 1
  printf '# rust-http-proxy — 日次と起動・停止の snapshot を取り寄せる (%s)\n\n' "$PROXY"
  printf -- '- 保存先: `%s`\n' "$DIR"
  got=0
  skipped=0
  failed=0
  fetched=()
  while read -r day bytes file; do
    [ -n "$day" ] || continue
    out="$DIR/$file"
    if [ -f "$out" ]; then
      skipped=$((skipped + 1))
      continue
    fi
    if curl -s --max-time "$MAX_TIME" "http://$PROXY/snapshots/$day" -o "$out.part" &&
      python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$out.part" 2>/dev/null; then
      mv "$out.part" "$out"
      got=$((got + 1))
      fetched+=("$out")
      printf -- '- 取り寄せた: `%s` (%s B、サーバー側 %s B)\n' "$out" "$(wc -c <"$out" | tr -d ' ')" "$bytes"
    else
      rm -f "$out.part"
      failed=$((failed + 1))
      printf -- '- **取れなかった**: %s\n' "$day"
    fi
  done <<EOF
$DAYS
EOF
  printf -- '- 取り寄せ %d 件、手元にあった %d 件、失敗 %d 件\n' "$got" "$skipped" "$failed"
  # 取り寄せた雪像の大きさと上限 (`limit_bytes`) の割合・部の `truncated`。90% 以上は警告 (T.29.16。
  # 日次の 1 枚が上限を越えると、プロキシは `recent` を 256 KiB に戻して黙って T.28.18 の形に戻る)
  if [ "${#fetched[@]}" -gt 0 ]; then
    printf -- '- 大きさ (上限の 90%% 以上は警告。T.29.16):\n'
    python3 scripts/snapshot-summary.py --size-check "${fetched[@]}" | sed 's/^/  /'
  fi
  # 締めの「前」と「0 時間」を**版で**選ぶ (T.28.20 は時刻だけで「最後の停止」と「その後の最初の起動」を
  # 選んでいて、T.28.99 では 0.28.39 を 53 分挟んだので「前」が 0.28.39 の停止になり、手で差し替えた。T.29.26)。
  # 各 1 枚の `status.version` (と `build`・`boot.id`) を読み、
  #   いまの版 = `CURRENT_VERSION` (`--closeout` が `/status` から渡す) か、無ければ手元の最後の起動の版
  #   前      = **いまの版と系列 (`0.<Phase>`) の違う版**の最後の停止 (同じ系列の途中のデプロイは飛ばす。
  #             系列の違う停止が無ければ、版の違う最後の停止)
  #   0 時間  = 前より後の、**いまの版** (`+<コミット>` は見ない) の最初の起動
  # 候補は全部、選んだ / 外した理由つきで並べる。どの 1 枚にも版が無ければ (T.28.20 より前の形) 今までどおり
  # 時刻だけで選ぶ。名前の先頭 18 文字 (`YYYY-MM-DDTHHMMSSZ`) は時刻なので、文字列の並びがそのまま時刻の並び。
  # `CLOSEOUT_PICK` があれば、選んだ 2 枚を `BEFORE=…` / `ZERO=…` の 2 行で、前提の 1 行を `PREMISE=…` で書く
  CURRENT_VERSION=${CURRENT_VERSION:-} CLOSEOUT_PICK=${CLOSEOUT_PICK:-} python3 - "$DIR" <<'PICK'
import json, os, re, sys, time

d = sys.argv[1]
cur = os.environ.get("CURRENT_VERSION") or None
pick_out = os.environ.get("CLOSEOUT_PICK") or None
pat = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{6}Z)-(start|shutdown)-snapshot\.json$")


def get(doc, *keys):
    for k in keys:
        doc = doc.get(k) if isinstance(doc, dict) else None
    return doc


ms = []
for name in sorted(os.listdir(d)):
    m = pat.match(name)
    if not m:
        continue
    path = os.path.join(d, name)
    try:
        with open(path, encoding="utf-8") as f:
            doc = json.load(f)
    except (OSError, ValueError):
        doc = {}
    ver = get(doc, "status", "version") or get(doc, "version")
    ms.append({"path": path, "at": m.group(1), "kind": m.group(2),
               "ver": ver if isinstance(ver, str) and ver else None,
               "build": get(doc, "status", "build"), "boot": get(doc, "status", "boot", "id"),
               "pid": get(doc, "status", "boot", "pid"), "taken": get(doc, "taken_at"),
               "up": get(doc, "status", "uptime_secs")})


def base(v):
    return v.split("+", 1)[0].split(" ", 1)[0] if v else None


def series(v):
    b = base(v)
    return ".".join(b.split(".")[:2]) if b else None


def when(m):
    return "%s-%s-%sT%s:%s:%sZ" % (m["at"][:4], m["at"][5:7], m["at"][8:10], m["at"][11:13], m["at"][13:15], m["at"][15:17])


def started(m):
    if isinstance(m["taken"], (int, float)) and isinstance(m["up"], (int, float)):
        return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(m["taken"] - m["up"]))
    return when(m) + " の 60 秒ほど前"


why = {}
before = zero = None
if not any(m["ver"] for m in ms):
    # 版の読めない古い形: 最後の停止と、その後の最初の起動 (T.28.20 のまま)
    stops = [m for m in ms if m["kind"] == "shutdown"]
    before = stops[-1] if stops else None
    zero = next((m for m in ms if m["kind"] == "start" and (before is None or m["at"] > before["at"])), None) \
        if before else None
    how = "時刻だけで選んだ (版の読める 1 枚が無い)"
else:
    if cur is None:
        cur = next((m["ver"] for m in reversed(ms) if m["kind"] == "start" and m["ver"]), None)
        src = "手元の最後の起動の版"
    else:
        src = "`/status`"
    how = "いまの版 `%s` (%s) で選んだ" % (cur, src)
    stops = [m for m in ms if m["kind"] == "shutdown" and m["ver"]]
    other = [m for m in stops if series(m["ver"]) != series(cur)]
    if other:
        before = other[-1]
    else:
        diff = [m for m in stops if base(m["ver"]) != base(cur)]
        before = diff[-1] if diff else None
        if before:
            how += "。系列 %s の違う停止が無いので、版の違う最後の停止" % series(cur)
    zero = next((m for m in ms if m["kind"] == "start" and base(m["ver"]) == base(cur)
                 and (before is None or m["at"] > before["at"])), None)
    for m in ms:
        if m is before or m is zero:
            continue
        if not m["ver"]:
            why[id(m)] = "版が読めない"
        elif m["kind"] == "shutdown":
            if base(m["ver"]) == base(cur):
                why[id(m)] = "いまの版の停止"
            elif series(m["ver"]) == series(cur):
                why[id(m)] = "いまと同じ系列 %s の途中の版" % series(cur)
            else:
                why[id(m)] = "前に選んだ停止より古い"
        elif base(m["ver"]) != base(cur):
            why[id(m)] = "いまの版でない起動"
        elif before is not None and m["at"] < before["at"]:
            why[id(m)] = "前に選んだ停止より古い"
        else:
            why[id(m)] = "いまの版の 2 回目以降の起動"

if ms and any(m["ver"] for m in ms):
    print("- 締めの候補 (起動と停止の 1 枚、古い順。%s):" % how)
    for m in ms:
        tag = "**前 (BEFORE)**" if m is before else "**0 時間 (ZERO)**" if m is zero else "外した: " + why.get(id(m), "")
        print("  - `%s` %s `%s`%s → %s" % (os.path.basename(m["path"]), m["kind"], m["ver"] or "?",
              " boot.id `%s`" % m["boot"] if m["boot"] else "", tag))
if before and zero:
    print("- 締めの判定に渡せる 2 枚 (前と 0 時間): `BEFORE=%s ZERO=%s`" % (before["path"], zero["path"]))
elif before:
    print("- 締めの判定に渡せる「前」(その後のいまの版の起動の 1 枚はまだ無い): `BEFORE=%s`" % before["path"])
elif zero:
    print("- 締めの判定に渡せる「0 時間」(前の版の停止の 1 枚が無い): `ZERO=%s`" % zero["path"])
if pick_out:
    premise = None
    if zero:
        premise = "版 `%s`%s・起動 %s%s%s" % (
            zero["ver"] or "?", " (`%s`)" % zero["build"] if zero["build"] else "", started(zero),
            " (`boot.id` `%s`" % zero["boot"] if zero["boot"] else "",
            (", pid %s)" % zero["pid"] if zero["pid"] is not None else ")") if zero["boot"] else "")
    if before:
        premise = (premise + "・" if premise else "") + "前の版 `%s` の停止 %s" % (before["ver"] or "?", when(before))
    with open(pick_out, "w", encoding="utf-8") as f:
        f.write("BEFORE=%s\nZERO=%s\nPREMISE=%s\n" % (before["path"] if before else "",
                                                     zero["path"] if zero else "", premise or ""))
PICK
  [ "$failed" = 0 ] || exit 1
  exit 0
fi

STAMP=${COLLECT_STAMP:-$(date -u +%Y-%m-%dT%H%M%SZ)}
# **要約 (標準出力) を `<時刻>-collect.md`、途中の道具の文句 (標準エラー) を `<時刻>-collect.err` に残す**。
# 前は呼ぶ側がリダイレクトしていて、忘れると要約が端末にしか残らなかった。
# 自分を `COLLECT_STAMP` 付きでもう 1 回呼び、標準出力は `tee` で端末にも出す
# (標準エラーはファイルに溜めて、最後に端末へまとめて出す。空なら消す)
if [ -z "${COLLECT_STAMP:-}" ]; then
  MD="$DIR/$STAMP-collect.md"
  ERR="$DIR/$STAMP-collect.err"
  COLLECT_STAMP=$STAMP "$SELF" "${ORIG_ARGS[@]}" 2>"$ERR" | tee "$MD"
  rc=${PIPESTATUS[0]}
  if [ -s "$ERR" ]; then
    cat "$ERR" >&2
  else
    rm -f "$ERR"
  fi
  echo "要約: $MD" >&2
  exit "$rc"
fi
OUT="$DIR/$STAMP-snapshot.json"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# --- 1. 取る -----------------------------------------------------------------
# **雪像の前に `/status` を 1 本** (T.15.0 (15))。要約が使うのは **RSS だけ**で、
# `/snapshot` は 17 部・最大 4 MiB を 1 つの文字列に組むので、**雪像を配ること自体が
# RSS を約 1.0 MB 押し上げる**。ほかの通算は 1 秒差で意味が変わらないのでそのまま雪像を使う。
# 取れなくても続ける (そのときは雪像の中の `/status` の RSS になる)。
# (変数の名前は `STATUS_BEFORE`。`BEFORE` は判定表の相手を渡す環境変数。T.18.2)
STATUS_BEFORE="$work/status-before.json"
if ! curl -s --max-time "$MAX_TIME" "http://$PROXY/status" -o "$STATUS_BEFORE" ||
  ! python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$STATUS_BEFORE" 2>/dev/null; then
  rm -f "$STATUS_BEFORE"
  STATUS_BEFORE=
fi
SNAP_START=$(now_ms)
if ! curl -s --max-time "$MAX_TIME" "http://$PROXY/snapshot" -o "$OUT"; then
  echo "failed to fetch http://$PROXY/snapshot" >&2
  rm -f "$OUT"
  exit 1
fi
if ! python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$OUT" 2>/dev/null; then
  echo "http://$PROXY/snapshot did not return JSON (kept at $OUT for inspection)" >&2
  exit 1
fi
SNAP_MS=$(($(now_ms) - SNAP_START))

# --- 1-follow. 切れた `recent`・`hosts`・`dns`・`profile` の続きを既定で追って雪像に繋ぐ (T.22.5・T.27.1) ---
# 量が 12 倍になった tokyo (T.21.99) では、雪像の `recent` は 2,000 件中 576 件 (33 分ぶん)、
# `hosts` は 1,001 件中 574 件で 256 KiB に切れる。**`--full` を付けなくても**、この 2 つの部 (T.27.1 からは
# `dns` (T.25.99 で 811 件中 678 件) と `profile` (720 標本中 348) も) は
# `next_offset` が `null` になるまで `offset=` で追い、**雪像の同じ部に行を足して** `/snapshot` の形の
# まま置き直す (`/profile?res=60` を繋ぐ T17.0b と同じ流儀。鍵は足さない・消さない)。
# 読む道具 (`snapshot-summary.py`・`snapshot-diff.py`・`status-diff.py`・`anonymize-snapshot.py`) は
# 隣のファイルを知らないので、繋いでおけばそのまま全部を読む。
# **`/recent?since=<前の雪像の取得時刻>` にしないのはわざと**: `since` は「**開いた**時刻がこれ以降」で
# 絞るので、前の雪像より前に開いて後に閉じた長生きのトンネルが前の雪像にも今回にも入らない。
# それにリングは 2,000 件 (`MAX_RECENT`) しか持たないので、`since` を付けても取れる窓は広がらない。
# 頁の間に新しく閉じた接続があると並びがずれて同じ行が 2 枚に出る (欠けはしない) ので `id` で、
# `hosts` は `host` で重ねを落とす (`hosts` は要求数の順なので、頁の間に順位が入れ替わった
# ホストは出ないことがある。`--full` の別ファイルでも同じ)。
# 上限は部ごとの枚数 `MAX_PAGES` (1 枚目を含む) と、続きを引く時間の合計 `FOLLOW_SECS`。
# 越えたら止めて、繋いだぶんの `truncated` と `next_offset` は残りを指したままにし、要約に書く
FOLLOW_NOTE=
if [ "$FOLLOW" = 1 ]; then
  FOLLOW_START=$(now_ms)
  # 1 行は `+ <部> <URL の形> <次の offset> <1 枚目の件数>` か `- <部> <引けない理由>`
  FOLLOW_PLAN=$(python3 - "$OUT" <<'PLANPY'
import json, sys

# 雪像がその部を組むときの引数 + `offset=` (`crates/endpoints/src/endpoints/recent/snapshot.rs` の `snapshot`)
# `dns` と `profile` は T.27.1 で足した (`/dns` の `offset=` は 0.27 から。0.25.4 には無い)
URL = {"recent": "/recent?n=2000&offset=", "hosts": "/hosts?limit=1000&offset=",
       "dns": "/dns?sort=age&limit=4096&offset=", "profile": "/profile?res=5&offset="}
# 部の中で行の並ぶ鍵 (`/dns` は `entries`、`/profile` は `samples`)
ITEMS = {"recent": "recent", "hosts": "hosts", "dns": "entries", "profile": "samples"}
d = json.load(open(sys.argv[1]))
for name, tmpl in URL.items():
    p = d.get(name)
    if not isinstance(p, dict) or not p.get("truncated"):
        continue
    if p.get("next_offset") is None:
        print("-", name, "この版のプロキシに offset= がありません")
        continue
    print("+", name, tmpl, p["next_offset"], len(p.get(ITEMS[name]) or []))
PLANPY
) || FOLLOW_PLAN=
  JOIN_ARGS=()
  while read -r kind part tmpl off _; do
    [ -n "${kind:-}" ] || continue
    if [ "$kind" = - ]; then
      JOIN_ARGS+=("@$part:old")
      continue
    fi
    page=2
    why="done"
    files=()
    while [ "${off:-null}" != null ]; do
      if [ "$page" -gt "$MAX_PAGES" ]; then
        why=max_pages
        break
      fi
      if [ $(($(now_ms) - FOLLOW_START)) -ge $((FOLLOW_SECS * 1000)) ]; then
        why="time"
        break
      fi
      f="$work/follow-$part-page$page.json"
      ok=0
      for _ in 1 2 3; do
        # 重い口が塞がっていると `{"error":"busy"}` (503) も JSON で返るので、それは取れていない扱い
        if curl -s --max-time "$MAX_TIME" "http://$PROXY$tmpl$off" -o "$f" &&
          python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$f" 2>/dev/null &&
          ! grep -q '"error":"busy"' "$f"; then
          ok=1
          break
        fi
        sleep 1 # 重い口は同時 1 本 (T.14.51)。`Retry-After: 1` に従う
      done
      if [ "$ok" != 1 ]; then
        echo "failed to fetch http://$PROXY$tmpl$off" >&2
        why=failed
        break
      fi
      files+=("$f")
      off=$(python3 -c 'import json, sys
v = json.load(open(sys.argv[1])).get("next_offset")
print("null" if v is None else v)' "$f" 2>/dev/null) || off=null
      page=$((page + 1))
    done
    JOIN_ARGS+=("@$part:$why" "${files[@]}")
  done <<EOF
$FOLLOW_PLAN
EOF
  FOLLOW_MS=$(($(now_ms) - FOLLOW_START))
  if [ "${#JOIN_ARGS[@]}" -gt 0 ]; then
    FOLLOW_NOTE=$(python3 - "$OUT" "$FOLLOW_MS" "$SNAP_MS" "$MAX_PAGES" "$FOLLOW_SECS" "${JOIN_ARGS[@]}" <<'JOINPY'
import json, os, sys

out, follow_ms, snap_ms, max_pages, follow_secs = sys.argv[1:6]
# `@<部>:<止めた理由>` のあとに、その部の続きの頁のファイルが並ぶ
plan, cur = [], None
for a in sys.argv[6:]:
    if a.startswith("@"):
        name, why = a[1:].split(":", 1)
        cur = (name, why, [])
        plan.append(cur)
    else:
        cur[2].append(a)
d = json.load(open(out))
KEY = {"recent": "id", "hosts": "host", "dns": "host", "profile": 0}
ITEMS = {"recent": "recent", "hosts": "hosts", "dns": "entries", "profile": "samples"}


def key(name, r):
    # `profile` の標本は配列で、先頭が `t` (`keys` の 0 番目)。ほかは辞書の鍵
    if name == "profile":
        return r[0] if isinstance(r, list) and r else None
    return r.get(KEY[name]) if isinstance(r, dict) else None


notes = []
for name, why, files in plan:
    p = d.get(name)
    if why == "old":
        notes.append(f"`{name}` は切れていますが、この版のプロキシに `offset=` がありません (1 枚ぶんだけ)")
        continue
    rows = p.get(ITEMS[name]) or []
    first = len(rows)
    seen = {key(name, r) for r in rows}
    rest = p.get("next_offset")
    dup = 0
    pages = []
    for f in files:
        page = json.load(open(f))
        got = []
        for r in page.get(ITEMS[name]) or []:
            k = key(name, r)
            if k is not None and k in seen:
                dup += 1
                continue
            seen.add(k)
            got.append(r)
        pages.append(got)
        rest = page.get("next_offset")
    # `profile` の `offset` は新しい順に飛ばす本数で、各頁の中は古い順 = 後の頁ほど古いので**前に**繋ぐ
    # (`/profile?res=60` を繋ぐ T17.0b と同じ)。ほかは後ろに足す
    if name == "profile":
        rows = [r for got in reversed(pages) for r in got] + rows
    else:
        rows = rows + [r for got in pages for r in got]
    # 形は `/snapshot` の部のまま (鍵の順も変えない)。件数の欄だけ繋いだあとの値にする。
    # `recent` の `count` は「返した件数」(`shown` と同じ値) で、`hosts`・`dns`・`profile` の `count` は全件数
    p[ITEMS[name]] = rows
    p["shown"] = len(rows)
    if name == "recent":
        p["count"] = len(rows)
    # 最後の頁に続きが無ければ切れていない。止めたときは残りを指したまま
    p["truncated"] = rest is not None
    p["next_offset"] = rest
    total = p.get("matched") if name == "recent" else p.get("count")
    line = (f"`{name}` {len(files) + 1} 枚で {first:,} → {len(rows):,} 件"
            + (f" (全 {total:,} 件)" if isinstance(total, int) else "")
            + (f"、頁の重なり {dup} 件を落とした" if dup else ""))
    if why == "max_pages":
        line += f"。**打ち切った**: `MAX_PAGES={max_pages}` で止めた (残りは offset={rest} から)"
    elif why == "time":
        line += (f"。**打ち切った**: 続きを引く時間が `FOLLOW_SECS={follow_secs}` 秒を越えた"
                 f" (残りは offset={rest} から)")
    elif why == "failed":
        line += f"。**打ち切った**: 続きが取れなかった (残りは offset={rest} から)"
    notes.append(line)
tmp = out + ".part"
with open(tmp, "w") as f:
    json.dump(d, f, ensure_ascii=False, separators=(",", ":"))
os.replace(tmp, out)
print("、".join(notes)
      + f" (続き {int(follow_ms) / 1000:.1f} 秒。`/snapshot` 自体は {int(snap_ms) / 1000:.1f} 秒)")
JOINPY
) || FOLLOW_NOTE='**繋げなかった** (雪像は 1 枚目のまま)'
  fi
fi

# --- 1a. 雪像に入らない口 ---------------------------------------------------------
# `/daily` `/healthz` `/slo` `/config` `/profile?res=60` は `/snapshot` の部に無いので、
# T.15.99 までは親が手で取っていた。同じ時刻の名前 (`<時刻>-<口>.json`) で隣に置く。
# `/profile` は重い口 (同時 1 本) なので、JSON が返らなければ 1 秒おいて 3 回まで試す。
# 取れなくても雪像は出す (要約に「取れなかった」と書く)
EXTRA_GOT=
EXTRA_FAILED=
for spec in daily:/daily healthz:/healthz slo:/slo config:/config 'profile_res_60:/profile?res=60'; do
  name=${spec%%:*}
  path=${spec#*:}
  f="$DIR/$STAMP-$name.json"
  ok=0
  for _ in 1 2 3; do
    # 重い口が塞がっていると `{"error":"busy"}` (503) も JSON で返るので、それは取れていない扱い
    if curl -s --max-time "$MAX_TIME" "http://$PROXY$path" -o "$f" &&
      python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$f" 2>/dev/null &&
      ! grep -q '"error":"busy"' "$f"; then
      ok=1
      break
    fi
    sleep 1
  done
  if [ "$ok" = 1 ]; then
    EXTRA_GOT="$EXTRA_GOT $name"
  else
    rm -f "$f"
    EXTRA_FAILED="$EXTRA_FAILED $name"
    echo "failed to fetch http://$PROXY$path" >&2
  fi
done
DAILY="$DIR/$STAMP-daily.json"

# `/profile?res=60` の 1 枚は 256 KiB で切れ、24 時間 (1,440 標本) のうち約 5 時間 (308 標本) しか
# 入らない (T.16.99)。**`--full` を付けなくても** `next_offset` が `null` になるまで `offset=` で追い、
# 1 つの JSON に繋いで同じ名前 (`<時刻>-profile_res_60.json`) に置き直す (T17.0b)。1 日 1 回の口で、
# 重い口は同時 1 本 (T.14.51) なので順に引く。枚数の上限は `--full` と同じ `MAX_PAGES` (1 枚目を含む)。
# `offset` は**新しい順**に飛ばす本数で、各頁の中は古い順なので、後の頁ほど古い = 前に繋ぐ。
# 頁の間に標本が 1 本増えると境目の 1 本が 2 枚に出るので、`t` で重ねを落とす。
# 途中で取れなくなったら取れたぶんだけ繋ぎ、`truncated` と `next_offset` は残りを指したままにする
PROFILE_NOTE=
PROFILE60="$DIR/$STAMP-profile_res_60.json"
if [ -f "$PROFILE60" ]; then
  pages=("$PROFILE60")
  off=$(python3 -c 'import json, sys
v = json.load(open(sys.argv[1])).get("next_offset")
print("null" if v is None else v)' "$PROFILE60" 2>/dev/null) || off=null
  page=2
  while [ "${off:-null}" != null ] && [ "$page" -le "$MAX_PAGES" ]; do
    pf="$work/profile_res_60-page$page.json"
    ok=0
    for _ in 1 2 3; do
      if curl -s --max-time "$MAX_TIME" "http://$PROXY/profile?res=60&offset=$off" -o "$pf" &&
        python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$pf" 2>/dev/null &&
        ! grep -q '"error":"busy"' "$pf"; then
        ok=1
        break
      fi
      sleep 1 # 重い口は同時 1 本 (T.14.51)。`Retry-After: 1` に従う
    done
    if [ "$ok" != 1 ]; then
      echo "failed to fetch http://$PROXY/profile?res=60&offset=$off" >&2
      break
    fi
    pages+=("$pf")
    off=$(python3 -c 'import json, sys
v = json.load(open(sys.argv[1])).get("next_offset")
print("null" if v is None else v)' "$pf" 2>/dev/null) || off=null
    page=$((page + 1))
  done
  if [ "${#pages[@]}" -gt 1 ]; then
    PROFILE_NOTE=$(python3 - "$PROFILE60" "${pages[@]}" <<'JOINPY'
import json, os, sys

out, paths = sys.argv[1], sys.argv[2:]
docs = [json.load(open(p)) for p in paths]
# 1 枚目 (いちばん新しい頁) の形 (`keys` / `roles` ほか) を土台に、`samples` だけ差し替える
base = docs[0]
seen = set()
merged = []
# 後の頁ほど古い。古い頁から順に足し、`t` (標本の先頭) が同じものは 2 本目を捨てる
for d in reversed(docs):
    for s in d.get("samples") or []:
        t = s[0] if isinstance(s, list) and s else None
        if t is not None and t in seen:
            continue
        seen.add(t)
        merged.append(s)
rest = docs[-1].get("next_offset")
base["samples"] = merged
base["shown"] = len(merged)
# 最後の頁に続きが無ければ切れていない。取れなくなって止めたときは残りを指したまま
base["truncated"] = rest is not None
base["next_offset"] = rest
tmp = out + ".part"
with open(tmp, "w") as f:
    json.dump(base, f, ensure_ascii=False, separators=(",", ":"))
os.replace(tmp, out)
print(f"{len(docs)} 枚を繋いだ ({len(merged)} / {base.get('count')} 標本、"
      f"truncated={'true' if base['truncated'] else 'false'})")
JOINPY
) || PROFILE_NOTE='繋げなかった (1 枚目のまま)'
  fi
fi
[ -f "$DAILY" ] || DAILY=

# --- 1c. 匿名化した写しを隣に置く (T.17.16) ---------------------------------------
# 雪像は `status/` にしか置けない (閲覧先と接続元が並ぶ) ので、テストへ持ち込めるように
# `anonymize-snapshot.py` で名前・IP・UA だけを置き換えた写しを `<時刻>-snapshot.anon.json` に置く。
# `/daily` と (繋いだあとの) `/profile?res=60` も `--side` で**同じ表**に通して `<時刻>-<口>.anon.json` に。
# 失敗しても雪像と要約は出す (写しを消して 1 行知らせるだけ)
ANON_NOTE=
if [ "$ANON" = 1 ]; then
  ANON_OUT="$DIR/$STAMP-snapshot.anon.json"
  ANON_SIDES=()
  for name in daily profile_res_60; do
    [ -f "$DIR/$STAMP-$name.json" ] && ANON_SIDES+=(--side "$DIR/$STAMP-$name.json")
  done
  if python3 scripts/anonymize-snapshot.py -q "$OUT" -o "$ANON_OUT" "${ANON_SIDES[@]}"; then
    ANON_NOTE="\`$ANON_OUT\`"
    [ "${#ANON_SIDES[@]}" -gt 0 ] && ANON_NOTE="$ANON_NOTE (隣の口 $((${#ANON_SIDES[@]} / 2)) つも同じ表で)"
  else
    rm -f "$ANON_OUT"
    echo "anonymize-snapshot.py failed (no .anon.json this time)" >&2
    ANON_NOTE='**作れなかった**'
  fi
fi

# 前回の雪像 (名前が UTC 時刻なので、名前順の 1 つ前が前回)
# shellcheck disable=SC2010  # 名前は自分で付けた UTC 時刻 (英数字と - だけ) なので ls | grep で足りる
PREV=$(ls -1 "$DIR"/*-snapshot.json 2>/dev/null | grep -vF "$OUT" | tail -1)

printf '# rust-http-proxy — %s (%s)\n\n' "$PROXY" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
# `--closeout` (T.29.26) から呼ばれたときは、前提の 1 行と取り寄せ・選択の箇条を頭に置く
[ -n "${CLOSEOUT_HEAD:-}" ] && [ -f "$CLOSEOUT_HEAD" ] && cat "$CLOSEOUT_HEAD"
printf -- '- 雪像: `%s` (%s B)\n' "$OUT" "$(wc -c <"$OUT" | tr -d ' ')"
# shellcheck disable=SC2086  # 先頭の空白を落とすため、わざと語に割る
[ -n "$EXTRA_GOT" ] && printf -- '- 雪像に入らない口: `%s-{%s}.json`\n' "$DIR/$STAMP" "$(echo $EXTRA_GOT | tr ' ' ',')"
[ -n "$EXTRA_FAILED" ] && printf -- '- **取れなかった口**:%s\n' "$EXTRA_FAILED"
[ -n "$FOLLOW_NOTE" ] && printf -- '- 切れた部の続き (既定で追う。T.22.5): %s\n' "$FOLLOW_NOTE"
[ "$FOLLOW" = 1 ] || printf -- '- 切れた部の続き: 追っていない (`FULL=0` か `--no-follow`)\n'
[ -n "$PROFILE_NOTE" ] && printf -- '- `/profile?res=60` は `offset=` で追って %s\n' "$PROFILE_NOTE"
[ -n "$ANON_NOTE" ] && printf -- '- 匿名化した写し: %s\n' "$ANON_NOTE"
[ -n "$PREV" ] && printf -- '- 前回: `%s`\n' "$PREV"
# 判定表の相手と 0 時間の雪像 (T.18.2)。読めないものは 1 行断って、渡されなかったことにする
if [ -n "$JUDGE_BEFORE" ]; then
  if [ -f "$JUDGE_BEFORE" ]; then
    printf -- '- 判定表の相手 (`BEFORE`): `%s`\n' "$JUDGE_BEFORE"
  else
    printf -- '- **`BEFORE` の雪像が無い**: `%s` (判定表の相手は前回の雪像のまま)\n' "$JUDGE_BEFORE"
    echo "BEFORE=$JUDGE_BEFORE is not a file (judging against the previous snapshot)" >&2
    JUDGE_BEFORE=
  fi
fi
if [ -n "$ZERO" ]; then
  if [ -f "$ZERO" ]; then
    printf -- '- 0 時間の雪像 (`ZERO`): `%s`\n' "$ZERO"
  else
    printf -- '- **`ZERO` の雪像が無い**: `%s`\n' "$ZERO"
    echo "ZERO=$ZERO is not a file (the heap row cannot be judged)" >&2
    ZERO=
  fi
fi

# --- 1b. 切れた部の続きを取る (--full。T.15.0 (11)) -----------------------------
# 雪像 1 枚は部ごとに 256 KiB で切れる (`truncated`)。`offset=` を持つ 3 つの部だけ、
# `next_offset` が `null` になるまで**追加の URL で**引いて別ファイルに落とす。
# `recent` と `hosts` は上の 1-follow (T.22.5) で繋いだあとなので、ここに来るのはまだ切れている
# ときだけ (続きは繋いだ部の `next_offset` から引く)
# 続きはどれも重い口 (同時 1 本) なので順に引き、断られたら `Retry-After: 1` に従う。
if [ "$FULL" = 1 ]; then
  printf -- '- `--full`: 切れた部の続きを `offset=` で追います (重い口は同時 1 本なので順に引きます)\n'
  PAGES=$(python3 - "$OUT" <<'PAGESPY'
import json, sys

# 続きを引ける部と、その URL の形 (雪像がその部を組むときの引数 + `offset=`)
URL = {
    "recent": "/recent?n=2000&offset=",
    "hosts": "/hosts?limit=1000&offset=",
    "profile": "/profile?res=5&offset=",
}
try:
    d = json.load(open(sys.argv[1]))
except ValueError:
    sys.exit(0)
for name, tmpl in URL.items():
    p = d.get(name)
    if not isinstance(p, dict) or not p.get("truncated"):
        continue
    nxt = p.get("next_offset")
    if nxt is None:                      # `offset=` を知らない古いプロキシ
        print("-", name, "この版のプロキシに offset= がありません")
        continue
    print("+", name, tmpl, nxt)
# 切れているのに続きを引けない部は名前だけ知らせる
for name, p in sorted(d.items()):
    if name in URL or not isinstance(p, dict):
        continue
    if p.get("truncated"):
        print("-", name, "この部に offset= はありません (1 枚ぶんだけです)")
PAGESPY
) || PAGES=
  # 1 行は `+ <部> <URL の形> <次の offset>` か `- <部> <引けない理由>`
  while read -r kind part rest; do
    [ -n "${kind:-}" ] || continue
    if [ "$kind" = - ]; then
      printf -- '  - **%s** は切れていますが続きを引けません (%s)\n' "$part" "$rest"
      continue
    fi
    off=${rest##* }    # 最後の語が次の offset
    rest=${rest% *}    # その手前までが URL の形
    page=2
    while [ "${off:-null}" != null ] && [ "$page" -le "$MAX_PAGES" ]; do
      # 名前は `page<N>-<部>` の順 (`<部>-<N>` だと `snapshot-diff.py --from-files` と
      # `anonymize-snapshot.py` の `classify()` が「後ろが部の名前で始まれば 1 口 1 ファイル」と
      # 見分けるので、続きの 1 枚を部そのものと取り違える)
      f="$DIR/$STAMP-page$page-$part.json"
      ok=0
      for try in 1 2 3; do
        if curl -s --max-time "$MAX_TIME" "http://$PROXY$rest$off" -o "$f"; then
          if grep -q '"error":"busy"' "$f" 2>/dev/null; then
            sleep 1 # 重い口は同時 1 本 (T.14.51)。`Retry-After: 1` に従う
            continue
          fi
          ok=1
          break
        fi
        sleep 1
      done
      if [ "$ok" != 1 ]; then
        printf -- '  - **取れなかった**: %s の %s 枚目 (%s 回試した)\n' "$part" "$page" "$try"
        rm -f "$f"
        break
      fi
      next=$(python3 -c 'import json, sys
try:
    v = json.load(open(sys.argv[1])).get("next_offset")
except Exception:
    v = None
print("null" if v is None else v)' "$f" 2>/dev/null) || next=null
      printf -- '  - `%s` (%s B、offset=%s、次は %s)\n' \
        "$f" "$(wc -c <"$f" | tr -d ' ')" "$off" "$next"
      off=$next
      page=$((page + 1))
    done
    if [ "${off:-null}" != null ]; then
      printf -- '  - **%s はまだ続きがあります** (offset=%s。MAX_PAGES=%s で止めました)\n' \
        "$part" "$off" "$MAX_PAGES"
    fi
  done <<EOF
$PAGES
EOF
fi
printf '\n'

# --- 2. 要点 ------------------------------------------------------------------
printf '## 1. 要点\n\n'
python3 scripts/snapshot-summary.py "$OUT" ${PREV:+--prev "$PREV"} \
  ${STATUS_BEFORE:+--status-before "$STATUS_BEFORE"} || echo '(要点を組めなかった)'
printf '\n'

# --- 3. 前回との差分 (snapshot-diff.py) ---------------------------------------
# 判定表 (`## 9.`) だけは要約のいちばん最後に回すので、ここではその手前までを出す
DIFFMD=
CRIT=
PROFILE_AFTER=
[ -f "$PROFILE60" ] && PROFILE_AFTER=$PROFILE60
PROFILE_BEFORE=
[ -n "$PREV" ] && [ -f "${PREV%-snapshot.json}-profile_res_60.json" ] &&
  PROFILE_BEFORE=${PREV%-snapshot.json}-profile_res_60.json
[ "$CRITERIA" = off ] || CRIT="--criteria $CRITERIA"
# phase18 の 2 行の材料 (T.18.2): `ZERO` があれば `--zero` で、日次の雪像の表は保存先 (`--from-server`
# が `<日付>T000000Z-snapshot.json` を置く所) を `--daily-snapshots` で渡す。**phase18 と、同じ材料を
# 使う phase20 (T.20.1)・phase21 (T.21.2)・phase22 (T.22.1)・phase23 (T.23.1)・phase24 (T.24.2)・phase25 (T.27.0)・phase26 (T.29.10) のときだけ**
# (phase24 と phase25 は `memory.baseline` が無い雪像のときだけ 0 時間の雪像で読むので、渡しておく)
# (古い定義の判定表は今までと同じ命令で出す)
P18=()
if [ "$CRITERIA" = phase18 ] || [ "$CRITERIA" = phase20 ] || [ "$CRITERIA" = phase21 ] ||
  [ "$CRITERIA" = phase22 ] || [ "$CRITERIA" = phase23 ] || [ "$CRITERIA" = phase24 ] ||
  [ "$CRITERIA" = phase25 ] || [ "$CRITERIA" = phase26 ]; then
  [ -n "$ZERO" ] && P18+=(--zero "$ZERO")
  P18+=(--daily-snapshots "$DIR")
fi
# `BEFORE` があるときは判定表だけ相手が違うので、下の差分には判定表を付けず、もう 1 回回す
DIFF_CRIT=$CRIT
DIFF_P18=("${P18[@]}")
if [ -n "$JUDGE_BEFORE" ] || [ -z "$CRIT" ]; then
  DIFF_CRIT=
  DIFF_P18=()
fi
if [ "$DIFF" = 1 ] && [ -n "$PREV" ]; then
  DIFFMD=$work/snapshot-diff.md
  # 判定表の無い差分 (`BEFORE` があるとき・`CRITERIA=off`) も、平常時は「300 本と中央値 × 3 の大きい方」で切る
  # (T.29.13。サーバーの `normal_hours_only=1` と同じ規則。300 本だけだと tokyo の後の 23 時間が全部バーストだった)
  MEDIAN_K=()
  [ -n "$DIFF_CRIT" ] || MEDIAN_K=(--burst-median-k 3)
  # shellcheck disable=SC2086  # $DIFF_CRIT は 2 語に分けたい
  # `/daily` は雪像に無いので、取れていれば判定表のミスの行に日ごとの幅を並べる (T.15.15 (2))
  # `/profile?res=60` (上で繋いだ 24 時間ぶん) は phase17 の conn 役の CPU/要求 の材料 (T17.0a)。
  # 前回の雪像の隣に同じ時刻の `-profile_res_60.json` があれば `--profile-before` で前にも渡す
  # (`--from-server` で取り寄せた日の雪像には無いので、そのときは雪像の `/profile` の部で参考になる)
  python3 scripts/snapshot-diff.py "$PREV" "$OUT" ${AAAA:+--aaaa "$AAAA"} $DIFF_CRIT "${MEDIAN_K[@]}" \
    ${DIFF_CRIT:+${DAILY:+--daily "$DAILY"}} \
    ${DIFF_CRIT:+${PROFILE_AFTER:+--profile "$PROFILE_AFTER"}} \
    ${DIFF_CRIT:+${PROFILE_BEFORE:+--profile-before "$PROFILE_BEFORE"}} \
    "${DIFF_P18[@]}" \
    >"$DIFFMD" 2>&1 || echo '(snapshot-diff.py が失敗した)' >>"$DIFFMD"
fi
# 判定表の相手が `BEFORE` のとき (T.18.2。T.17.99 (f) で手で回していた 1 回)。前の `/profile?res=60` は
# `BEFORE` の隣の同じ時刻のもの。ここで要るのは `## 9.` より後ろだけで、AAAA の有無はホスト別の表
# (`## 3.`) にしか出ないので、`AAAA` の表が無ければ名前は引かない (`--no-dns`)
CRITMD=$DIFFMD
if [ "$DIFF" = 1 ] && [ -n "$JUDGE_BEFORE" ] && [ -n "$CRIT" ]; then
  CRITMD=$work/snapshot-criteria.md
  JUDGE_PROFILE=
  [ -f "${JUDGE_BEFORE%-snapshot.json}-profile_res_60.json" ] &&
    [ "${JUDGE_BEFORE%-snapshot.json}" != "$JUDGE_BEFORE" ] &&
    JUDGE_PROFILE=${JUDGE_BEFORE%-snapshot.json}-profile_res_60.json
  if [ -n "$AAAA" ]; then
    JUDGE_AAAA=(--aaaa "$AAAA")
  else
    JUDGE_AAAA=(--no-dns)
  fi
  # shellcheck disable=SC2086  # $CRIT は 2 語に分けたい
  python3 scripts/snapshot-diff.py "$JUDGE_BEFORE" "$OUT" "${JUDGE_AAAA[@]}" $CRIT \
    ${DAILY:+--daily "$DAILY"} \
    ${PROFILE_AFTER:+--profile "$PROFILE_AFTER"} \
    ${JUDGE_PROFILE:+--profile-before "$JUDGE_PROFILE"} \
    "${P18[@]}" \
    >"$CRITMD" 2>&1 || echo '(snapshot-diff.py が失敗した)' >>"$CRITMD"
fi
printf '## 2. 前回との差分 (snapshot-diff.py)\n\n'
if [ -n "$DIFFMD" ]; then
  # 見出しは 1 段下げる (この文書の `## 1.` 〜と番号がぶつからないように)
  sed -e '/^## 9\. /,$d' -e 's/^#\{1,2\} /### /' "$DIFFMD"
else
  echo '(前回の雪像が無いか DIFF=0)'
fi
printf '\n'

# --- 4. ホスト別 (status-diff.py) ---------------------------------------------
printf '## 3. ホスト別 (status-diff.py)\n\n```\n'
if [ "$DIFF" = 1 ] && [ -n "$PREV" ]; then
  scripts/status-diff.py "$PREV" "$OUT" ${AAAA:+--aaaa "$AAAA"} --min-timed 1 --top 40 2>&1 ||
    echo '(status-diff.py が失敗した)'
else
  scripts/status-diff.py "$OUT" ${AAAA:+--aaaa "$AAAA"} --sort slow --top 40 2>&1 ||
    echo '(status-diff.py が失敗した)'
fi
printf '```\n\n'

# --- 5. ダッシュボードの読み方 (check-dashboard.js) ---------------------------
printf '## 4. ダッシュボード (check-dashboard.js)\n\n```\n'
if [ "$DASHBOARD" = 1 ] && command -v node >/dev/null 2>&1; then
  python3 - "$OUT" "$work/history.json" "$work/status.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
json.dump(d.get("history", {}).get("5") or {}, open(sys.argv[2], "w"))
json.dump(d.get("status") or {}, open(sys.argv[3], "w"))
PY
  node scripts/check-dashboard.js "$work/history.json" "$work/status.json" 2>&1 ||
    echo '(check-dashboard.js が失敗した)'
else
  echo '(飛ばした: node が無いか DASHBOARD=0)'
fi
printf '```\n\n'

# --- 6. 手元から見た待ち (probe-deployed.sh) ----------------------------------
printf '## 5. 手元から見た待ち (probe-deployed.sh)\n\n```\n'
if [ "$PROBE" = 1 ]; then
  scripts/probe-deployed.sh "$PROXY" 2>&1 || echo '(probe-deployed.sh が失敗した)'
else
  echo '(飛ばした: PROBE=0)'
fi
printf '```\n\n'

# --- 7. 完了の定義に対する判定 (要約の末尾) --------------------------------
printf '## 6. 完了の定義に対する判定'
[ "$CRITERIA" = off ] || printf ' (snapshot-diff.py --criteria %s)' "$CRITERIA"
printf '\n'
if [ -n "$CRITMD" ] && grep -q '^## 9\. ' "$CRITMD"; then
  # 相手が `BEFORE` のときは、どの 1 枚と比べた表かを先に書く (上の差分とは相手が違う)
  [ "$CRITMD" = "$DIFFMD" ] || printf '\n相手は `BEFORE` の雪像 `%s` です (上の差分の相手は前回の雪像)。\n' "$JUDGE_BEFORE"
  sed -n '/^## 9\. /,$p' "$CRITMD" | sed '1d'   # 見出しは上で出している
elif [ "$CRITERIA" = off ]; then
  printf '\n(CRITERIA=off なので出していない)\n'
elif [ -n "$CRITMD" ] && [ "$CRITMD" != "$DIFFMD" ]; then
  # `BEFORE` との判定が表まで届かなかった (道具の文句をそのまま出す)
  printf '\n(`BEFORE` の雪像との判定表を組めなかった)\n\n```\n'
  cat "$CRITMD"
  printf '```\n'
else
  printf '\n(前回の雪像が無いか DIFF=0 なので判定できない)\n'
fi
