#!/bin/bash
# `cargo build --release` が決められたメモリで通ることを確かめる。
#
# 動作環境 (Pterodactyl のコンテナ) はメモリが小さく、超えると rustc が OOM killer に
# SIGKILL されてビルドが落ちる。rustc はクレート単位で全部を一度に抱えるので、
# 行が増えるとそのまま効いてくる。層ごとにクレートを分けてあるのはそのため。
#
# **RSS を測るだけでは機械をまたいだ判定にならない。** メモリ圧がかかっていないと
# アロケータが解放済みのページを持ち続けるので、同じビルドでも余裕のある機械ほど
# 大きく出る (実測: aarch64 の手元 194 MB に対し、GitHub の runner では 334 MB)。
# そこで、cgroup を作れる環境では**実際にその上限の中でビルドして通るか**を見る。
# 作れない環境では RSS を出すだけにして、判定はしない。
#
# 使い方:
#   scripts/build-memory.sh [上限 MB]            上限の中で通るか (既定 200)
#   scripts/build-memory.sh --find 200 250 300   通る最小の上限を探す (調べるとき用)
set -u
MODE=gate
if [ "${1:-}" = "--find" ]; then MODE=find; shift; fi
LIMIT_MB="${1:-200}"
LADDER="${*:-200}"

run_in_cgroup() {
  command -v systemd-run >/dev/null 2>&1 || return 2
  systemctl is-system-running >/dev/null 2>&1 || return 2
  # systemd-run は -E を当てる前に実行ファイルを探すので、絶対パスで渡す
  local cargo_bin
  cargo_bin=$(command -v cargo) || return 2
  local sudo=""
  [ "$(id -u)" -ne 0 ] && sudo="sudo -n"
  $sudo systemd-run --scope \
      --uid="$(id -u)" --gid="$(id -g)" \
      -p "MemoryMax=${LIMIT_MB}M" -p MemorySwapMax=0 \
      -E "PATH=$PATH" -E "HOME=$HOME" -E "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}" \
      -E "RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}" \
      "$cargo_bin" build --release -j 1
}

report_rss() {
  local rows
  rows=$(mktemp)
  (
    while true; do
      for p in $(pgrep -x rustc 2>/dev/null); do
        rss=$(awk '/VmRSS/{print $2}' "/proc/$p/status" 2>/dev/null) || continue
        name=$(tr '\0' '\n' < "/proc/$p/cmdline" 2>/dev/null | grep -A1 -x -- '--crate-name' | tail -1)
        [ -n "$rss" ] && [ -n "$name" ] && echo "$rss $name" >> "$rows"
      done
      sleep 0.03
    done
  ) &
  local sampler=$!
  cargo build --release
  local rc=$?
  sleep 0.3
  kill "$sampler" 2>/dev/null
  echo
  echo "クレートごとの rustc 最大 RSS (参考値。メモリ圧のかかり方で上下する):"
  sort -k2,2 -k1,1rn "$rows" | awk '!seen[$2]++ {printf "  %-22s %7.1f MB\n", $2, $1/1024}'
  rm -f "$rows"
  return $rc
}

if [ "$MODE" = find ]; then
  for LIMIT_MB in $LADDER; do
    echo "=== 上限 ${LIMIT_MB} MB を試す ==="
    cargo clean -q
    run_in_cgroup
    rc=$?
    if [ $rc -eq 0 ]; then
      echo "通る最小の上限: ${LIMIT_MB} MB"
      exit 0
    fi
    if [ $rc -eq 2 ]; then
      echo "cgroup を作れないので調べられません"
      exit 2
    fi
    echo "  ${LIMIT_MB} MB では通らなかった (exit=$rc)"
  done
  echo "どの上限でも通らなかった"
  exit 1
fi

echo "上限 ${LIMIT_MB} MB でリリースビルドを試します (-j 1)"
run_in_cgroup
case $? in
  0)
    echo "OK: ${LIMIT_MB} MB の中でビルドが通りました"
    exit 0
    ;;
  2)
    echo "cgroup を作れないので判定はしません (参考の RSS だけ出します)"
    report_rss
    exit $?
    ;;
  *)
    echo "NG: ${LIMIT_MB} MB の中でビルドが通りませんでした (rustc が OOM killer に落とされたか、ビルド自体の失敗)"
    echo "    どのクレートが大きいかは、余裕のある機械で下の参考値を見てください"
    exit 1
    ;;
esac
