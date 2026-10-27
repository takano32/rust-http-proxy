#!/bin/bash
# コミットの前の確認を**全部走らせて**、落ちたものを最後にまとめて出す (T.29.29)。
#
# 確認の手順を `cargo fmt --check && cargo clippy … && …` とつなぐと、1 つ落ちた所で止まって残りが
# 見えない。T.28.48 では fmt で止まったのに気づかず次の手順へ進み、fmt に落ちるコミットを push した
# (直後に `b56160a` で直した)。ここでは**短絡せず**全部を回し、失敗を集めて最後に一覧と exit 1。
#
# 回すもの (この順。名前は `--only` / `--skip` に渡す名前):
#   ・fmt       cargo fmt --all --check
#   ・clippy    cargo clippy --workspace --all-targets -- -D warnings      (CI の check と同じ)
#   ・docs      scripts/check-docs.sh                                      (最後の行が「差分 0 件。」)
#   ・dashboard node scripts/check-dashboard.js
#   ・unittest  python3 -m unittest discover -s scripts
#   ・versions  python3 scripts/crate-versions.py --check
#   ・shellcheck shellcheck scripts/*.sh scripts/mx                        (CI と同じ引数)
# `cargo test` は入れない (手元で数分かかり、この環境だけで落ちる既知のものがある。別に回す)。
# 命令が入っていない (node・shellcheck が無い機械) ものは**飛ばした**と出し、失敗には数えない。
#
# 使い方: scripts/check-all.sh [--only 名前,名前] [--skip 名前,名前] [-v]
#   -v で通ったものの出力も出す (既定は落ちたものの出力の末尾 30 行だけ)。
#   cargo の並列は `CARGO_BUILD_JOBS` で (例: `CARGO_BUILD_JOBS=2 scripts/check-all.sh`)。
# 出口: 0 = 全部通った / 1 = 1 つでも落ちた / 2 = 使い方の誤り。
set -u
cd "$(dirname "$0")/.." || exit 1

ALL="fmt clippy docs dashboard unittest versions shellcheck"
ONLY=
SKIP=
VERBOSE=0
usage() {
  echo "usage: $0 [--only NAME,NAME] [--skip NAME,NAME] [-v]   (名前: ${ALL// /, })" >&2
  exit 2
}
while [ $# -gt 0 ]; do
  case "$1" in
  --only)
    [ $# -ge 2 ] || usage
    ONLY=" ${2//,/ } "
    shift 2
    ;;
  --skip)
    [ $# -ge 2 ] || usage
    SKIP=" ${2//,/ } "
    shift 2
    ;;
  -v) VERBOSE=1 && shift ;;
  *) usage ;;
  esac
done
for n in $ONLY $SKIP; do
  case " $ALL " in *" $n "*) ;; *)
    echo "check-all: 知らない名前: $n" >&2
    usage
    ;;
  esac
done

W=$(mktemp -d) || exit 1
trap 'rm -rf "$W"' EXIT

FAILED=()
SKIPPED=()
PASSED=()

# step 名前 必要な命令 命令...
step() {
  local name=$1 need=$2 t0 rc secs
  shift 2
  if [ -n "$ONLY" ]; then case "$ONLY" in *" $name "*) ;; *) return 0 ;; esac fi
  case "$SKIP" in *" $name "*) return 0 ;; esac
  if ! command -v "$need" >/dev/null 2>&1; then
    echo "--  $name: 飛ばした ($need が無い)"
    SKIPPED+=("$name")
    return 0
  fi
  t0=$SECONDS
  "$@" >"$W/$name.log" 2>&1
  rc=$?
  secs=$((SECONDS - t0))
  if [ "$rc" = 0 ]; then
    echo "ok  $name (${secs} 秒)"
    PASSED+=("$name")
    [ "$VERBOSE" = 1 ] && sed 's/^/    /' "$W/$name.log"
  else
    echo "NG  $name (exit $rc、${secs} 秒): $*"
    tail -n 30 "$W/$name.log" | sed 's/^/    /'
    FAILED+=("$name")
  fi
  return 0
}

# check-docs.sh は差分があれば 1 で抜けるが、最後の行も見る (「差分 0 件。」でなければ落とす)
# shellcheck disable=SC2317,SC2329  # step から名前で呼ぶ (shellcheck には呼び出しが見えない)
check_docs() {
  local out rc
  out=$(scripts/check-docs.sh 2>&1)
  rc=$?
  printf '%s\n' "$out"
  [ "$rc" = 0 ] || return "$rc"
  printf '%s\n' "$out" | tail -n 1 | grep -q '差分 0 件。'
}

step fmt cargo cargo fmt --all --check
step clippy cargo cargo clippy -q --workspace --all-targets -- -D warnings
step docs bash check_docs
step dashboard node node scripts/check-dashboard.js
step unittest python3 python3 -m unittest discover -s scripts
step versions python3 python3 scripts/crate-versions.py --check
step shellcheck shellcheck shellcheck scripts/*.sh scripts/mx

echo
if [ ${#FAILED[@]} -eq 0 ]; then
  echo "check-all: 全部通った (${#PASSED[@]} 本${SKIPPED[*]:+、飛ばした: ${SKIPPED[*]}})"
  exit 0
fi
echo "check-all: 落ちた ${#FAILED[@]} 本: ${FAILED[*]} (通った ${#PASSED[@]} 本${SKIPPED[*]:+、飛ばした: ${SKIPPED[*]}})"
exit 1
