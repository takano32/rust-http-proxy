#!/bin/bash
# リリースビルド中の rustc の最大 RSS をクレートごとに測り、上限を超えたら失敗する。
#
# 動作環境 (Pterodactyl のコンテナ) はメモリが小さく、超えると rustc が OOM killer に
# SIGKILL されてビルドが落ちる。1 クレートが大きいほど rustc がまとめて抱えるので、
# 層ごとにクレートを分けてある。この見張りが無いと、行が増えたときに気付けない。
#
# 使い方: scripts/build-memory.sh [上限 MB]  (既定 200)
set -u
LIMIT_MB="${1:-200}"
ROWS=$(mktemp)
trap 'rm -f "$ROWS"' EXIT

(
  while true; do
    for p in $(pgrep -x rustc 2>/dev/null); do
      rss=$(awk '/VmRSS/{print $2}' "/proc/$p/status" 2>/dev/null) || continue
      name=$(tr '\0' '\n' < "/proc/$p/cmdline" 2>/dev/null | grep -A1 -x -- '--crate-name' | tail -1)
      [ -n "$rss" ] && [ -n "$name" ] && echo "$rss $name" >> "$ROWS"
    done
    sleep 0.03
  done
) &
SAMPLER=$!

cargo build --release
RC=$?
sleep 0.3
kill "$SAMPLER" 2>/dev/null

echo
echo "クレートごとの rustc 最大 RSS (上限 ${LIMIT_MB} MB):"
sort -k2,2 -k1,1rn "$ROWS" | awk -v limit="$LIMIT_MB" '
  !seen[$2]++ {
    mb = $1 / 1024
    printf "  %-22s %7.1f MB%s\n", $2, mb, (mb > limit ? "  ← 上限超え" : "")
    if (mb > limit) over = 1
    if (mb > max) { max = mb; who = $2 }
  }
  END {
    printf "\n最大: %s %.1f MB\n", who, max
    exit over ? 1 : 0
  }
'
OVER=$?
[ "$RC" -ne 0 ] && exit "$RC"
exit "$OVER"
