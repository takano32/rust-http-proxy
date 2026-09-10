#!/bin/bash
# デプロイ先のプロキシを手元から測る (TASKS.md §1「デプロイ先の測り方」)。
#
# `curl -w` の `time_connect` / `time_appconnect` / `time_starttransfer` を 3 回ずつ取る。
# **見るのは `time_appconnect − time_connect`** で、これが「CONNECT の確立 + TLS 握手」
# (手元からプロキシまでの TCP は `time_connect` に入っているので引き算で落ちる)。
# CONNECT は AAAA のあるホストと無いホストを 1 つずつ叩く: **Happy Eyeballs の 250 ms
# (`crates/net/src/net.rs` の `STAGGER`) を払うのは AAAA のあるホストだけ**なので、
# この 2 行の差が Phase 12 で削ろうとしている待ちそのものになる。
#
# 使い方:
#   scripts/probe-deployed.sh HOST:PORT
#     例: scripts/probe-deployed.sh nagoya.sorahost.net:50697
#
# 環境変数:
#   REPEAT (既定 3)          … 1 経路あたりの回数
#   AAAA_HOST (既定 www.dlsite.com)   … AAAA のあるホスト (https)
#   NO_AAAA_HOST (既定 discord.com)   … AAAA の無いホスト (https)
#   MAX_TIME (既定 3)        … 1 本あたりの上限 (秒)
#   BUDGET (既定 28)         … 全体の予算 (秒)。**30 秒以内に終わること**が受け入れ基準なので、
#                              予算を使い切った行は測らずに `skip` と出す
#
# **デプロイ先は利用者の本番**なので、確認に使う要求は必要最小限にする
# (このスクリプトの 1 回分 = 5 経路 × 3 回 = 15 本。実測 8〜12 秒)。
# 本文は捨てる (`-o /dev/null`) が **GET は本当に送っている**ので、何度も回さないこと。
# `GET /` は「プロキシの URL をブラウザで開いた」ときの動き (自分宛てのオリジン形式)。
# 2026-09-10 時点では **502** が返る (自分の公開アドレスへ転送しに行って届かない。T12.3 で直す)。
#
# **forward の行 (`http://example.com/`) はキャッシュ HIT のことがある**ので、そのままでは
# オリジンまでの待ちを測っていない (2026-09-10 の実測は `X-Cache: HIT from rust-http-proxy (memory)`、
# `Age: 6239` で、AAAA のあるホストなのに 250 ms を払っていない = 出て行っていない)。
# オリジンまで行かせたいときは `-H 'Cache-Control: no-cache'` を足すか、`curl -D -` で `X-Cache` を見る。
#
# ホスト別の統計は `/status` に出るので、効きを見るときは `scripts/status-diff.py` を組で使う
# (このスクリプトは「手元から見た待ち時間」、status-diff.py は「プロキシから見た待ち時間」)。

set -u
PROXY=${1:-}
if [ -z "$PROXY" ]; then
  echo "usage: $0 HOST:PORT   (例: $0 nagoya.sorahost.net:50697)" >&2
  exit 2
fi
REPEAT=${REPEAT:-3}
AAAA_HOST=${AAAA_HOST:-www.dlsite.com}
NO_AAAA_HOST=${NO_AAAA_HOST:-discord.com}
MAX_TIME=${MAX_TIME:-3}
BUDGET=${BUDGET:-28}
W='%{http_code} %{time_connect} %{time_appconnect} %{time_starttransfer} %{time_total} %{size_download}'
START=$(date +%s)

printf '# probe-deployed: %s  (%s runs each, %ss per request, %ss budget)\n' \
  "$PROXY" "$REPEAT" "$MAX_TIME" "$BUDGET"
printf '# AAAA: %s (has AAAA) / %s (no AAAA)\n' "$AAAA_HOST" "$NO_AAAA_HOST"
printf '# times are seconds. app-conn = appconnect - connect = CONNECT setup + TLS handshake\n\n'
printf '%-32s %3s %5s %8s %11s %13s %9s %9s\n' \
  route '#' code connect appconnect starttransfer total app-conn

# probe LABEL curl-args...
probe() {
  local label=$1; shift
  local i out code tc ta ts tt sz diff
  for i in $(seq 1 "$REPEAT"); do
    if [ $(( $(date +%s) - START )) -ge "$BUDGET" ]; then
      printf '%-32s %3s %5s %8s %11s %13s %9s %9s\n' "$label" "$i" skip - - - - -
      continue
    fi
    out=$(curl -s -o /dev/null --max-time "$MAX_TIME" -w "$W" "$@" 2>/dev/null)
    if [ -z "$out" ]; then
      printf '%-32s %3s %5s %8s %11s %13s %9s %9s\n' "$label" "$i" fail - - - - -
      continue
    fi
    read -r code tc ta ts tt sz <<<"$out"
    # TLS を張らない経路は appconnect が 0 なので引き算しない
    diff=$(awk -v a="$ta" -v c="$tc" 'BEGIN { if (a + 0 == 0) print "-"; else printf "%.3f", a - c }')
    printf '%-32s %3s %5s %8s %11s %13s %9s %9s\n' "$label" "$i" "$code" "$tc" "$ta" "$ts" "$tt" "$diff"
  done
}

# 1. プロキシ自身のエンドポイント (オリジン形式。プロキシは自分で応答する)
probe "GET /status" "http://$PROXY/status"
# 2. 自分宛てのオリジン形式 (ブラウザでプロキシの URL を開いた形。T12.3 で 200 にする)
probe "GET / (self-addressed)" "http://$PROXY/"
# 3. forward (http。オリジンプールと名前解決の経路)
probe "http://example.com/" -x "http://$PROXY" "http://example.com/"
# 4. CONNECT (AAAA あり = 250 ms を払う側)
probe "https://$AAAA_HOST (AAAA)" -x "http://$PROXY" "https://$AAAA_HOST/"
# 5. CONNECT (AAAA なし = 払わない側)
probe "https://$NO_AAAA_HOST (no AAAA)" -x "http://$PROXY" "https://$NO_AAAA_HOST/"

printf '\n# elapsed %s s\n' "$(( $(date +%s) - START ))"
