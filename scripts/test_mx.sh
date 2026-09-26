#!/bin/bash
# `scripts/mx` (機械のロック) のテスト (T17.15)。
#
# 見るのは 6 つ:
#   1. 終了コード      … 命令の終了コードをそのまま返す (引数なしは 2)
#   2. 残った子の片付け … 命令が終わったあと (正常終了) と、mx が TERM を受けたとき (143) に、
#                         命令が裏に残した子がグループごと止まる
#   3. 占有の待ち      … `MX_SLOTS=1` が走っている間は、`MX_SLOTS=1` も `MX_SLOTS=2` も待つ
#   4. 2 口の並列      … `MX_SLOTS=2` なら 2 本は同時に走り、3 本目は口が空くまで待つ
#   5. 他人のベンチ待ち … 命令の先頭が `target/release/rust-http-proxy` のプロセス (ここでは
#                         その名前で起動した `sleep` の偽物) が居なくなるまで待ってから走る
#   6. `MX_WAIT` で 75  … 待ちきれなければ命令を走らせずに 75 で抜ける
#
# **口のファイルは一時ディレクトリ** (`MX_DIR`) に置く。既定の `MX_DIR` は同じ機械で並列に
# 走っている本物の mx が使っているので、テストが触ると他人の口を取ってしまう。
# ただし 5 と 6 の偽物のベンチは機械全体の `pgrep` に見えるので、同じ機械の他人の mx も
# 数秒だけ待つ (偽物は数秒で止める)。
#
# 使い方: scripts/test_mx.sh   (引数なし。`python3 -m unittest discover -s scripts` からも
# `scripts/test_mx.py` 経由で回る)。全部通れば 0、1 つでも落ちれば 1。
set -u
cd "$(dirname "$0")/.." || exit 1
MX=$PWD/scripts/mx

W=$(mktemp -d -t mx-test.XXXXXX) || exit 1
# 呼び出し元の設定を持ち込まない (既定の値を見るテストがある)
unset MX_SLOTS MX_WAIT MX_QUIET
export MX_DIR="$W/mx"
FAKE="$W/target/release/rust-http-proxy"

# 自分が起動したものだけを覚えておき、最後に止める
PIDS=()
# shellcheck disable=SC2317,SC2329  # trap から呼ぶ (shellcheck には呼び出しが見えない)
finish() {
  local p
  for p in "${PIDS[@]}"; do kill -KILL "$p" 2>/dev/null; done
  rm -rf "$W"
}
trap finish EXIT

NG=0
ok() { echo "  ok   $*"; }
ng() {
  echo "  NG   $*"
  NG=1
}

# ファイルができるまで待つ (最大 $2 秒)
wait_file() {
  local i
  for ((i = 0; i < $2 * 10; i++)); do
    [ -e "$1" ] && return 0
    sleep 0.1
  done
  return 1
}

# PID が居なくなるまで待つ (最大 $2 秒。0 なら今だけ見る)。ゾンビは `/proc/<pid>/stat` の 3 つめが Z
gone() {
  local i st
  for ((i = 0; i <= $2 * 10; i++)); do
    st=$(awk '{print $3}' "/proc/$1/stat" 2>/dev/null)
    if [ -z "$st" ] || [ "$st" = Z ]; then return 0; fi
    [ "$i" -lt $(($2 * 10)) ] && sleep 0.1
  done
  return 1
}

# mx と同じ探し方で、偽物のベンチが見えるまで待つ (最大 $1 秒)
wait_fake_visible() {
  local i
  for ((i = 0; i < $1 * 10; i++)); do
    pgrep -f "^$FAKE( |\$)" >/dev/null && return 0
    sleep 0.1
  done
  return 1
}

if pgrep -f '^[^ ]*target/release/rust-http-proxy( |$)' >/dev/null; then
  echo "注意: 他人のベンチ (target/release/rust-http-proxy) が走っている。mx の待ちが入る" >&2
fi

# --- 1. 終了コード --------------------------------------------------------------
echo "1. 終了コード"
"$MX" sh -c 'exit 7'
rc=$?
if [ "$rc" = 7 ]; then ok "命令の 7 をそのまま返す"; else ng "exit 7 が $rc になった"; fi
"$MX" true
rc=$?
if [ "$rc" = 0 ]; then ok "命令の 0 をそのまま返す"; else ng "true が $rc になった"; fi
"$MX" 2>/dev/null
rc=$?
if [ "$rc" = 2 ]; then ok "引数なしは 2"; else ng "引数なしが $rc になった"; fi

# --- 2. 残った子の片付け --------------------------------------------------------
echo "2. 残った子の片付け"
# 正常終了: 命令 (bash) は裏に sleep を残して先に終わる
"$MX" bash -c 'sleep 300 & echo $! >"$1"' _ "$W/child1"
rc=$?
c1=$(cat "$W/child1" 2>/dev/null)
PIDS+=("$c1")
if [ "$rc" = 0 ] && [ -n "$c1" ] && gone "$c1" 5; then
  ok "正常終了のあと、裏に残った子 ($c1) が止まる"
else
  ng "正常終了のあと、裏の子 ($c1) が残った (rc=$rc)"
fi

# TERM: 命令が子を待っている最中に mx を止める
"$MX" bash -c 'sleep 300 & echo $! >"$1"; wait' _ "$W/child2" &
m=$!
PIDS+=("$m")
if wait_file "$W/child2" 10; then
  c2=$(cat "$W/child2")
  PIDS+=("$c2")
  # mx は命令を裏で起動してから trap を張る。その間に TERM が届くと片付けが走らないので、
  # 子が書いたのを見てから少し置く
  sleep 0.5
  kill -TERM "$m"
  wait "$m"
  rc=$?
  if [ "$rc" = 143 ] && gone "$c2" 5; then
    ok "TERM で 143、裏の子 ($c2) も止まる"
  else
    ng "TERM のあと rc=$rc、裏の子 ($c2) は $(gone "$c2" 0 && echo 止まった || echo 残った)"
  fi
else
  ng "TERM の試験の命令が起動しなかった"
fi

# --- 3. 占有の待ち --------------------------------------------------------------
echo "3. 占有の待ち (MX_SLOTS=1)"
: >"$W/log3"
MX_SLOTS=1 "$MX" bash -c 'echo A-start >>"$1"; sleep 2; echo A-end >>"$1"' _ "$W/log3" &
a=$!
PIDS+=("$a")
for ((i = 0; i < 100; i++)); do
  grep -q A-start "$W/log3" && break
  sleep 0.1
done
MX_SLOTS=1 "$MX" bash -c 'echo B1 >>"$1"' _ "$W/log3" 2>"$W/err3b1" &
b1=$!
MX_SLOTS=2 "$MX" bash -c 'echo B2 >>"$1"' _ "$W/log3" 2>"$W/err3b2" &
b2=$!
PIDS+=("$b1" "$b2")
wait "$a" "$b1" "$b2"
# A-start と A-end が続けて並び、B1 / B2 はそのあと (B1 と B2 の順は問わない)
order=$(tr '\n' ' ' <"$W/log3")
case "$order" in
"A-start A-end B1 B2 " | "A-start A-end B2 B1 ")
  ok "占有の間は MX_SLOTS=1 も 2 も待ち、終わってから走る ($order)"
  ;;
*) ng "走った順が違う: $order" ;;
esac
if grep -q '機械の占有を待っています' "$W/err3b1"; then
  ok "MX_SLOTS=1 の側は「占有を待っています」と言う"
else
  ng "MX_SLOTS=1 の待ちの知らせが無い: $(cat "$W/err3b1")"
fi
if grep -q '空いている口を待っています' "$W/err3b2"; then
  ok "MX_SLOTS=2 の側は「空いている口を待っています」と言う"
else
  ng "MX_SLOTS=2 の待ちの知らせが無い: $(cat "$W/err3b2")"
fi

# --- 4. 2 口の並列 --------------------------------------------------------------
echo "4. 2 口の並列 (MX_SLOTS=2)"
# 2 本は相手が起動したのを見てから、release ができるまで口を持ち続ける
hold='touch "$1"; for ((i = 0; i < 100; i++)); do [ -e "$2" ] && [ -e "$3" ] && exit 0; sleep 0.1; done; exit 1'
MX_SLOTS=2 "$MX" bash -c "$hold" _ "$W/p4a" "$W/p4b" "$W/release4" &
p4a=$!
MX_SLOTS=2 "$MX" bash -c "$hold" _ "$W/p4b" "$W/p4a" "$W/release4" &
p4b=$!
PIDS+=("$p4a" "$p4b")
if wait_file "$W/p4a" 10 && wait_file "$W/p4b" 10; then
  ok "2 本が同時に走っている"
  MX_SLOTS=2 "$MX" touch "$W/p4c" 2>"$W/err4c" &
  p4c=$!
  PIDS+=("$p4c")
  sleep 1.5
  if [ -e "$W/p4c" ]; then
    ng "口が 2 つとも埋まっているのに 3 本目が走った"
  else
    ok "3 本目は口が空くのを待つ"
  fi
  touch "$W/release4"
  wait "$p4c"
  rc=$?
  if [ "$rc" = 0 ] && [ -e "$W/p4c" ]; then
    ok "口が空いたら 3 本目が走る"
  else
    ng "3 本目が走らなかった (rc=$rc)"
  fi
else
  touch "$W/release4"
  ng "2 本が同時に走らなかった"
fi
wait "$p4a"
ra=$?
wait "$p4b"
rb=$?
if [ "$ra" != 0 ] || [ "$rb" != 0 ]; then
  ng "並列の 2 本の終了コードが 0 でない ($ra, $rb)"
fi

# --- 5. 他人のベンチ待ち --------------------------------------------------------
echo "5. 他人のベンチ待ち"
# 命令の先頭 (argv[0]) だけを target/release/rust-http-proxy にした sleep を偽物にする
# (sleep へのシンボリックリンクは、uutils の coreutils が argv[0] の名前で断るので使わない)
(exec -a "$FAKE" sleep 3) &
f=$!
PIDS+=("$f")
if wait_fake_visible 5; then
  t0=$(date +%s%N)
  MX_WAIT=30 "$MX" bash -c 'echo ran >"$1"' _ "$W/ran5" 2>"$W/err5"
  rc=$?
  ms=$((($(date +%s%N) - t0) / 1000000))
  if [ "$rc" = 0 ] && [ -e "$W/ran5" ] && gone "$f" 0 && [ "$ms" -ge 1000 ]; then
    ok "偽物のベンチが終わるのを待ってから走る (${ms} ms 待った)"
  else
    ng "他人のベンチを待たなかった (rc=$rc、${ms} ms)"
  fi
  if grep -q '他人のベンチ' "$W/err5"; then
    ok "「他人のベンチ … を待っています」と言う"
  else
    ng "他人のベンチの知らせが無い: $(cat "$W/err5")"
  fi
else
  ng "偽物のベンチが pgrep に見えない"
fi
kill -KILL "$f" 2>/dev/null
wait "$f" 2>/dev/null

# --- 6. MX_WAIT で 75 -----------------------------------------------------------
echo "6. MX_WAIT で 75"
(exec -a "$FAKE" sleep 30) &
f=$!
PIDS+=("$f")
if wait_fake_visible 5; then
  MX_WAIT=1 "$MX" touch "$W/ran6" 2>"$W/err6"
  rc=$?
  # 偽物はすぐ止める (同じ機械の他人の mx も待たせているので)
  kill -KILL "$f" 2>/dev/null
  wait "$f" 2>/dev/null
  if [ "$rc" = 75 ] && [ ! -e "$W/ran6" ]; then
    ok "MX_WAIT=1 秒で待ちきれず、命令を走らせずに 75"
  else
    ng "MX_WAIT を過ぎても 75 にならない (rc=$rc、命令は $([ -e "$W/ran6" ] && echo 走った || echo 走らなかった))"
  fi
  if grep -q '1 秒待っても終わらなかった' "$W/err6"; then
    ok "待ちきれなかったことを言う"
  else
    ng "待ちきれなかった知らせが無い: $(cat "$W/err6")"
  fi
else
  kill -KILL "$f" 2>/dev/null
  ng "偽物のベンチが pgrep に見えない"
fi

if [ "$NG" = 0 ]; then
  echo "test_mx: all ok"
else
  echo "test_mx: FAILED"
fi
exit "$NG"
