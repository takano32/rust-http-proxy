#!/bin/bash
# デプロイ先の様子を **1 回で取って 1 枚に読む** (TASKS.md T14.4)。
#
# `/snapshot` (T14.4) を保存し、前回の雪像との差分 (`status-diff.py`)、ダッシュボードの
# 読み方の確認 (`check-dashboard.js`)、手元から見た待ち (`probe-deployed.sh`) を続けて回して、
# **Markdown 1 枚**を標準出力に出す。T14.0 で 17 本の URL を手で叩いていた作業の代わり。
#
# 使い方:
#   scripts/collect-deployed.sh HOST:PORT [DIR]
#     例: scripts/collect-deployed.sh nagoya.sorahost.net:50697
#         scripts/collect-deployed.sh 127.0.0.1:8080 /tmp/snaps      # 手元のプロキシで試す
#   保存先の既定は ~/rust-http-proxy-status/ (`<UTC 時刻>-snapshot.json`、秒まで)。
#   **リポジトリには保存しない** (個票には接続元 IP と宛先ホストが並ぶため)。
#
# 環境変数:
#   PROBE (既定 1)      … 0 で `probe-deployed.sh` を飛ばす (デプロイ先へ本物の要求を
#                         15 本送るので、何度も回すときは 0 にする)
#   DASHBOARD (既定 1)  … 0 で `check-dashboard.js` を飛ばす (Node が無ければ自動で飛ばす)
#   DIFF (既定 1)       … 0 で前回との差分を飛ばす
#   MAX_TIME (既定 30)  … `/snapshot` を取る上限 (秒)。4 MiB まであるので長めに
#   AAAA (無指定)       … `status-diff.py --aaaa FILE` に渡す表 (数字を残すときは固定する。§1)
#
# 出口: 雪像が取れなければ 1 (それ以外は、途中の道具が失敗しても 1 枚は出す)。
set -u
cd "$(dirname "$0")/.."
PROXY=${1:-}
if [ -z "$PROXY" ]; then
  echo "usage: $0 HOST:PORT [DIR]   (例: $0 nagoya.sorahost.net:50697)" >&2
  exit 2
fi
DIR=${2:-$HOME/rust-http-proxy-status}
PROBE=${PROBE:-1}
DASHBOARD=${DASHBOARD:-1}
DIFF=${DIFF:-1}
MAX_TIME=${MAX_TIME:-30}
AAAA=${AAAA:-}

mkdir -p "$DIR" || exit 1
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

# --- 3. ホスト別 (status-diff.py) ---------------------------------------------
printf '## 2. ホスト別 (status-diff.py)\n\n```\n'
if [ "$DIFF" = 1 ] && [ -n "$PREV" ]; then
  scripts/status-diff.py "$PREV" "$OUT" ${AAAA:+--aaaa "$AAAA"} --min-timed 1 --top 40 2>&1 ||
    echo '(status-diff.py が失敗した)'
else
  scripts/status-diff.py "$OUT" ${AAAA:+--aaaa "$AAAA"} --sort slow --top 40 2>&1 ||
    echo '(status-diff.py が失敗した)'
fi
printf '```\n\n'

# --- 4. ダッシュボードの読み方 (check-dashboard.js) ---------------------------
printf '## 3. ダッシュボード (check-dashboard.js)\n\n```\n'
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

# --- 5. 手元から見た待ち (probe-deployed.sh) ----------------------------------
printf '## 4. 手元から見た待ち (probe-deployed.sh)\n\n```\n'
if [ "$PROBE" = 1 ]; then
  scripts/probe-deployed.sh "$PROXY" 2>&1 || echo '(probe-deployed.sh が失敗した)'
else
  echo '(飛ばした: PROBE=0)'
fi
printf '```\n'
