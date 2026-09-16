#!/bin/bash
# デプロイ先の様子を **1 回で取って 1 枚に読む** (TASKS.md T14.4)。
#
# `/snapshot` (T14.4) を保存し、前回の雪像との差分 (`snapshot-diff.py` と `status-diff.py`)、
# ダッシュボードの読み方の確認 (`check-dashboard.js`)、手元から見た待ち (`probe-deployed.sh`) を
# 続けて回して、**Markdown 1 枚**を標準出力に出す。T14.0 で 17 本の URL を手で叩いていた作業の代わり。
# **前回の雪像があれば `snapshot-diff.py` (T14.17) を呼ぶ** (再起動で切った平常時の前後・ホスト別・
# 接続元別・名前解決・エラー・バースト)。**完了の定義に対する判定表は要約のいちばん最後**に置く。
#
# 使い方:
#   scripts/collect-deployed.sh HOST:PORT [DIR]
#     例: scripts/collect-deployed.sh nagoya.sorahost.net:50697
#         scripts/collect-deployed.sh 127.0.0.1:8080 /tmp/snaps      # 手元のプロキシで試す
#   保存先の既定は ~/rust-http-proxy-status/ (`<UTC 時刻>-snapshot.json`、秒まで)。
#   **リポジトリには保存しない** (個票には接続元 IP と宛先ホストが並ぶため)。
#
#   scripts/collect-deployed.sh --from-server HOST:PORT [DIR]
#     プロキシ自身が 1 日 1 回書いている日次の snapshot (T14.34) のうち、**手元に無い日付だけ**を
#     `/snapshots` の一覧から取り寄せる (取りに行くのは `/snapshots/<date>` で、中身は
#     `/snapshot` そのもの)。名前は今の流儀に合わせて `<日付>T000000Z-snapshot.json`
#     (日付は**その 1 日を写したもの**。実際に撮られたのは翌日 00:00 UTC の直後)。
#     回し忘れた日の個票がこれで埋まる。
#
# 環境変数:
#   PROBE (既定 1)      … 0 で `probe-deployed.sh` を飛ばす (デプロイ先へ本物の要求を
#                         15 本送るので、何度も回すときは 0 にする)
#   DASHBOARD (既定 1)  … 0 で `check-dashboard.js` を飛ばす (Node が無ければ自動で飛ばす)
#   DIFF (既定 1)       … 0 で前回との差分を飛ばす
#   CRITERIA (既定 phase14) … 判定表に使う完了の定義。`off` で判定表を出さない
#   MAX_TIME (既定 30)  … `/snapshot` を取る上限 (秒)。4 MiB まであるので長めに
#   AAAA (無指定)       … `status-diff.py --aaaa FILE` に渡す表 (数字を残すときは固定する。§1)
#
# 出口: 雪像が取れなければ 1 (それ以外は、途中の道具が失敗しても 1 枚は出す)。
set -u
cd "$(dirname "$0")/.."
FROM_SERVER=0
if [ "${1:-}" = --from-server ]; then
  FROM_SERVER=1
  shift
fi
PROXY=${1:-}
if [ -z "$PROXY" ]; then
  echo "usage: $0 [--from-server] HOST:PORT [DIR]   (例: $0 nagoya.sorahost.net:50697)" >&2
  exit 2
fi
# `http://host:port` と書かれても `host:port` として扱う (URL でも通るように)
PROXY=${PROXY#http://}
PROXY=${PROXY%/}
DIR=${2:-$HOME/rust-http-proxy-status}
PROBE=${PROBE:-1}
DASHBOARD=${DASHBOARD:-1}
DIFF=${DIFF:-1}
CRITERIA=${CRITERIA:-phase14}
MAX_TIME=${MAX_TIME:-30}
AAAA=${AAAA:-}

mkdir -p "$DIR" || exit 1

# --- 0. 保存済みの日次 snapshot を取り寄せる (--from-server。T14.34) ----------
# プロキシが `$HOME/.rust-http-proxy/snapshots/` に 1 日 1 ファイル書いているので、
# 手元に無い日付だけを取って `$DIR` に置く (`/snapshots` の一覧 → `/snapshots/<date>`)。
if [ "$FROM_SERVER" = 1 ]; then
  LIST=$(curl -s --max-time "$MAX_TIME" "http://$PROXY/snapshots") || LIST=
  if [ -z "$LIST" ]; then
    echo "failed to fetch http://$PROXY/snapshots" >&2
    exit 1
  fi
  DAYS=$(printf '%s' "$LIST" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except ValueError:
    sys.exit("not JSON")
for f in d.get("files") or []:
    print(f.get("date"), f.get("bytes", 0))
') || exit 1
  printf '# rust-http-proxy — 日次の snapshot を取り寄せる (%s)\n\n' "$PROXY"
  printf -- '- 保存先: `%s`\n' "$DIR"
  got=0
  skipped=0
  failed=0
  while read -r day bytes; do
    [ -n "$day" ] || continue
    out="$DIR/${day}T000000Z-snapshot.json"
    if [ -f "$out" ]; then
      skipped=$((skipped + 1))
      continue
    fi
    if curl -s --max-time "$MAX_TIME" "http://$PROXY/snapshots/$day" -o "$out.part" &&
      python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$out.part" 2>/dev/null; then
      mv "$out.part" "$out"
      got=$((got + 1))
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
  [ "$failed" = 0 ] || exit 1
  exit 0
fi

STAMP=$(date -u +%Y-%m-%dT%H%M%SZ)
OUT="$DIR/$STAMP-snapshot.json"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# --- 1. 取る -----------------------------------------------------------------
if ! curl -s --max-time "$MAX_TIME" "http://$PROXY/snapshot" -o "$OUT"; then
  echo "failed to fetch http://$PROXY/snapshot" >&2
  rm -f "$OUT"
  exit 1
fi
if ! python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$OUT" 2>/dev/null; then
  echo "http://$PROXY/snapshot did not return JSON (kept at $OUT for inspection)" >&2
  exit 1
fi

# 前回の雪像 (名前が UTC 時刻なので、名前順の 1 つ前が前回)
PREV=$(ls -1 "$DIR"/*-snapshot.json 2>/dev/null | grep -vF "$OUT" | tail -1)

printf '# rust-http-proxy — %s (%s)\n\n' "$PROXY" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
printf -- '- 雪像: `%s` (%s B)\n' "$OUT" "$(wc -c <"$OUT" | tr -d ' ')"
[ -n "$PREV" ] && printf -- '- 前回: `%s`\n' "$PREV"
printf '\n'

# --- 2. 要点 ------------------------------------------------------------------
printf '## 1. 要点\n\n'
python3 scripts/snapshot-summary.py "$OUT" ${PREV:+--prev "$PREV"} || echo '(要点を組めなかった)'
printf '\n'

# --- 3. 前回との差分 (snapshot-diff.py) ---------------------------------------
# 判定表 (`## 9.`) だけは要約のいちばん最後に回すので、ここではその手前までを出す
DIFFMD=
CRIT=
[ "$CRITERIA" = off ] || CRIT="--criteria $CRITERIA"
if [ "$DIFF" = 1 ] && [ -n "$PREV" ]; then
  DIFFMD=$work/snapshot-diff.md
  # shellcheck disable=SC2086  # $CRIT は 2 語に分けたい
  python3 scripts/snapshot-diff.py "$PREV" "$OUT" ${AAAA:+--aaaa "$AAAA"} $CRIT \
    >"$DIFFMD" 2>&1 || echo '(snapshot-diff.py が失敗した)' >>"$DIFFMD"
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
if [ -n "$DIFFMD" ] && grep -q '^## 9\. ' "$DIFFMD"; then
  sed -n '/^## 9\. /,$p' "$DIFFMD" | sed '1d'   # 見出しは上で出している
elif [ "$CRITERIA" = off ]; then
  printf '\n(CRITERIA=off なので出していない)\n'
else
  printf '\n(前回の雪像が無いか DIFF=0 なので判定できない)\n'
fi
