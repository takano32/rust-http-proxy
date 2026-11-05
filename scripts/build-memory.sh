#!/bin/bash
# `cargo build --release` が決められたメモリで通ることを確かめる。
#
# 動作環境 (Pterodactyl のコンテナ) はメモリが小さく、超えると rustc が OOM killer に
# SIGKILL されてビルドが落ちる。rustc はクレート単位で全部を一度に抱えるので、
# 行が増えるとそのまま効いてくる。層ごとにクレートを分けてあるのはそのため。
#
# **VmRSS を測るだけでは上限との比較にならない。** rustc の VmRSS のうち 110〜122 MB は
# `librustc_driver-*.so` (111 MB) をマップしたファイル由来のページで、これはページキャッシュに
# 載っていれば cgroup には課金されない (実測: 上限 110 MB の cgroup の中で通ったビルドでも、
# VmRSS は 215 MB と出る)。cgroup が必ず抱えるのは **RssAnon** の方で、こちらは 36〜86 MB。
# だから RSS の絶対値を上限と比べてはいけない。**同じ機械の中でクレートの順位を見るためだけに使う。**
# そこで、cgroup を作れる環境では**実際にその上限の中でビルドして通るか**を見る。
# システムの systemd (CI) が無くても、**ユーザーの systemd** (`systemd-run --user --scope`) が
# あれば同じ判定ができる (手元の機械はこちら。PID 1 が systemd でなくてもユーザーの
# インスタンスは動いていて、上限も効く)。どちらも無いときだけ RSS を出すだけにして判定しない。
#
# **上限を決めているクレート** (T17.11 = T15.12 段 8 の後の実測 2026-09-26、全クレート `codegen-units = 4`。RssAnon の最大。
# 手元 aarch64、rustc 1.98.1。通る最小は 90 MB: 85 以下は毎回落ち、90 は 4 回とも通った):
#   proxy-endpoints 82.5 MB > proxy-server 81.5 > proxy-metrics-recent 76.9 > proxy-http 74.7
#   > proxy-config 73.0 > proxy-net-dns 72.2 > proxy-metrics-core 71.8 > proxy-metrics-types 71.1
#   > proxy-metrics-watch 71.0 > … > proxy-metrics-profile 69.2 > proxy-metrics-window 67.7 > proxy-metrics-slo 63.3 > …
#   (段 8 の前は proxy-metrics-watch 95.4 > proxy-metrics-window 91.1 がいちばん上で、通る最小は 105 MB。
#    段 8 で watch から SLO・日次・雪像を proxy-metrics-slo へ、window から窓の土台と段階の標本を
#    proxy-metrics-profile へ割った。段 7 の前 = 要求の経路のクレートが `codegen-units = 1` だった頃は
#    proxy-metrics-window 107.8 MB がいちばん上で、通る最小は 120〜125 MB)
# 行数の順ではない。効くのは「自分の行数 + 依存から単相化されてくる量」。
# 上限を下げたいならこの上位から割ること (T14.55 で `proxy-metrics` 1 つ 272 MB を 6 つに割った)。
#
# 使い方:
#   scripts/build-memory.sh [上限 MB]            上限の中で通るか (既定 120)
#   scripts/build-memory.sh --find 85 90 95 100  通る最小の上限を探す (調べるとき用)
set -u
MODE=gate
if [ "${1:-}" = "--find" ]; then MODE="find"; shift; fi
LIMIT_MB="${1:-120}"
LADDER="${*:-120}"

# どの systemd で scope を作れるかを 1 度だけ調べ、`SCOPE_KIND` に覚える。
# 判定は「実際に小さい scope を 1 つ作ってみる」で行う。`systemctl is-system-running` は
# 状態が degraded なだけでも 1 を返すので使えない。
SCOPE_KIND=""
detect_scope() {
  [ -n "$SCOPE_KIND" ] && return 0
  command -v systemd-run >/dev/null 2>&1 || { SCOPE_KIND=none; return 0; }
  local sudo=""
  [ "$(id -u)" -ne 0 ] && sudo="sudo -n"
  if $sudo systemd-run --scope --quiet -p MemoryMax=64M /bin/true >/dev/null 2>&1; then
    SCOPE_KIND=system   # CI やふつうの Linux
  elif systemd-run --user --scope --quiet -p MemoryMax=64M -p MemorySwapMax=0 /bin/true >/dev/null 2>&1; then
    SCOPE_KIND=user     # 手元 (PID 1 が systemd でない。ユーザーの systemd だけ動いている)
  else
    SCOPE_KIND=none
  fi
}

run_in_cgroup() {
  detect_scope
  [ "$SCOPE_KIND" = none ] && return 2
  # systemd-run は -E を当てる前に実行ファイルを探すので、絶対パスで渡す
  local cargo_bin
  cargo_bin=$(command -v cargo) || return 2
  # ユーザーの scope は自分自身として走るので --uid/--gid は付けない
  # (付けると systemd に拒まれる)。システムの scope は root が作るので要る。
  local scope=(--user --scope)
  local sudo=""
  if [ "$SCOPE_KIND" = system ]; then
    [ "$(id -u)" -ne 0 ] && sudo="sudo -n"
    scope=(--scope --uid="$(id -u)" --gid="$(id -g)")
  fi
  $sudo systemd-run "${scope[@]}" \
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
        # RssAnon (cgroup が必ず抱える分) と VmRSS (ファイル由来のページを含む) の両方。
        # 並べ替えの鍵は RssAnon。VmRSS は 110 MB 前後が librustc_driver のマップで、
        # クレートの大きさとはほとんど関係しない。
        read -r anon rss < <(awk '/^RssAnon:/{a=$2} /^VmRSS:/{r=$2} END{print a+0, r+0}' \
          "/proc/$p/status" 2>/dev/null)
        [ "${anon:-0}" -eq 0 ] && continue
        name=$(tr '\0' '\n' < "/proc/$p/cmdline" 2>/dev/null | grep -A1 -x -- '--crate-name' | tail -1)
        [ -n "$name" ] && echo "$anon $rss $name" >> "$rows"
      done
      sleep 0.02
    done
  ) &
  local sampler=$!
  cargo build --release
  local rc=$?
  sleep 0.3
  kill "$sampler" 2>/dev/null
  echo
  echo "クレートごとの rustc の最大メモリ (RssAnon 順。**上限と直接比べてはいけない**。"
  echo "同じ機械の中でどのクレートが大きいかを見るためのもの):"
  printf "  %-22s %9s %9s\n" "クレート" "RssAnon" "VmRSS"
  sort -k3,3 -k1,1rn "$rows" \
    | awk '!seen[$3]++ {printf "  %-22s %6.1f MB %6.1f MB\n", $3, $1/1024, $2/1024}' \
    | sort -k2,2rn
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

echo "上限 ${LIMIT_MB} MB でリリースビルドを試します (-j 1。.cargo/config.toml と同じ)"
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
    echo "    落ちるのは上限を決めているクレート (2026-09-26 の実測では proxy-endpoints か proxy-server のあたり) のところ。"
    echo "    順位は cgroup を作れない機械で参考値 (RssAnon) を出すと見られます"
    exit 1
    ;;
esac
