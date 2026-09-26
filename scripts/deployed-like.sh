#!/bin/bash
# デプロイ先 (Pterodactyl コンテナ) に似せた条件で <コマンド> を回す (T14.16)。
#
# 手元の loopback とデプロイ先の一番の違いは **IPv6 が黙って落ちること**で、
# Phase 12 で見つけた 250 ms (Happy Eyeballs の Connection Attempt Delay = `STAGGER`) は
# その条件でしか出ない。ベンチの `--only connect` は宛先が IP リテラルなので候補が 1 つ
# (`crates/net-conn/src/net.rs` の `addrs.len() == 1` の短絡) になり、**Happy Eyeballs 本体を
# 一度も通っていなかった** (T10.1 が「無罪」と結論した理由)。この 2 つを同時に埋めるのが
# このスクリプトと `--only connect-multi` で、中で回せば手元で 250 ms が再現できる。
#
# 作るもの (root は要らない。ユーザー名前空間 = `unshare -r`):
#
#   1. **ネット名前空間** (`-n`): lo だけ。外へは出られないので、オリジンはベンチの内蔵のものだけ
#   2. **IPv6 の既定経路を `dev lo`** に置く: 出た SYN は lo を回って戻り、自分宛てではないので
#      黙って捨てられる → `connect` は**約 1 秒ハングしてから失敗**する。
#      これはデプロイ先の実測 (「IPv6 リテラル宛ては約 1.0 秒で 502」。§5 Phase 12 の合議) と同じ姿。
#      **`blackhole` の経路ではこうならない**: この機械では `connect` がその場で `EINVAL` を返し
#      (実測 0.000 秒)、プロキシは待たずに IPv4 へ移るので 250 ms が出ない
#      (デプロイ先も即 `ENETUNREACH` ではない、というのが Phase 12 の前提だった)
#   3. **マウント名前空間** (`-m`): `/etc/hosts` を「元の内容 + `multi.test` の 2 行」に差し替える。
#      `2001:db8::1` (上の黒穴) と `127.0.0.1` (生きている) を持つので、プロキシの `getaddrinfo` は
#      **AAAA と A の 2 候補**を返す (この機械の `nsswitch.conf` は `files` を見る。確認済み)
#   4. `ulimit -n` (既定 1024) と、使えれば `systemd-run --user --scope -p MemoryMax=…`
#      (既定 256M。`scripts/cgroup-run.sh` と同じ作法)。デプロイ先は `ulimit -n` 1024 /
#      cgroup 256 MiB で、`max_conns` はここから 240 に決まる
#   5. **cgroup 名前空間** (`-C`) + `mount -t cgroup2` で `/sys/fs/cgroup` を貼り直す:
#      これをしないとプロキシ自身が上限を読めない (この機械では scope の cgroup が
#      自分の cgroup 名前空間の外にできる。`scripts/cgroup-run.sh` の頭に同じ話がある)
#
# 使い方:
#   scripts/deployed-like.sh [--memory 256M] [--nofile 1024] [--hosts-from FILE] -- <コマンド...>
#
#   scripts/deployed-like.sh -- scripts/cpu-per-request.sh --only connect-multi --seconds 5
#   scripts/deployed-like.sh --memory off -- ./target/release/rust-http-proxy --lite -p 18080
#   scripts/deployed-like.sh --hosts-from scripts/testdata/replay-burst.json -- \
#     ./target/release/bench --proxy 127.0.0.1:18080 --only replay \
#       --replay-file scripts/testdata/replay-burst.json --speed 10
#
#   --memory SIZE   cgroup の MemoryMax (既定 256M。`off` / `0` で付けない)
#   --nofile N      名前空間の中の `ulimit -n` (既定 1024)
#   --hosts-from F  **個票 (`/recent` / `/snapshot` / `/connections` の JSON) に出てくる宛先の
#                   ホスト名を全部 `127.0.0.1` に向ける** (T14.29 の `bench --only replay` 用。
#                   何度でも渡せる)。`"target":"host:port"` を拾い、名前 (IP リテラルでない
#                   もの) だけを **A レコードとして**足す。**AAAA は足さない** — IPv6 の黒穴を
#                   見るのは `multi.test` の役目で、再生に混ぜるとバーストの形ではなく
#                   Happy Eyeballs を測ることになるため。
#                   **足した名前は名前空間の中の `/etc/hosts` にしか出ない** (元の
#                   `/etc/hosts` も、このリポジトリも書き換えない)
#
# **名前空間が作れない機械では「使えない」と印字して終了コード 2** で終わる
# (中のコマンドは走らせない)。中で走るコマンドには `RHP_DEPLOYED_LIKE=1` が見える。
#
# ベンチもプロキシも**同じ名前空間の中**で回す (ネット名前空間はプロセス単位なので、
# 外からは中の 127.0.0.1 に届かない)。`taskset` は名前空間の中でも効くので、
# `scripts/cpu-per-request.sh` の固定はそのまま使える。
set -u
cd "$(dirname "$0")/.." || exit 1

# ベンチの `--only connect-multi` が使う名前 (`crates/bench/src/main.rs` の `MULTI_HOST` と同じ)
MULTI_HOST=multi.test
MEMORY=256M
NOFILE=1024
HOSTS_FROM=()
while [ $# -gt 0 ]; do
  case "$1" in
    --memory) MEMORY=$2; shift 2 ;;
    --nofile) NOFILE=$2; shift 2 ;;
    --hosts-from) HOSTS_FROM+=("$2"); shift 2 ;;
    --) shift; break ;;
    -h|--help) sed -n '2,56p' "$0"; exit 0 ;;
    *) echo "deployed-like: unknown option: $1" >&2; exit 2 ;;
  esac
done
[ $# -gt 0 ] || { echo "usage: scripts/deployed-like.sh [--memory 256M] [--nofile 1024] [--hosts-from FILE] -- <command...>" >&2; exit 2; }

# 入れ子で呼ばれたら (`--deployed-like` を渡した `cpu-per-request.sh` が中でまた呼ぶ等)
# もう一度作らずにそのまま走らせる。
if [ "${RHP_DEPLOYED_LIKE:-0}" = 1 ]; then
  exec "$@"
fi

if ! command -v unshare >/dev/null 2>&1 ||
   ! unshare -rmnC true >/dev/null 2>&1; then
  echo "deployed-like: この機械では使えません (root 不要のユーザー名前空間 + ネット名前空間が作れない)。" >&2
  echo "  unshare -rmnC true が通るか確かめてください (util-linux の unshare が要ります)。" >&2
  exit 2
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# 元の /etc/hosts に 2 行足したものを名前空間の中で bind mount する
# (元の行を消さないので、localhost も自分のホスト名も今までどおり引ける)
{ cat /etc/hosts; printf '2001:db8::1 %s\n127.0.0.1 %s\n' "$MULTI_HOST" "$MULTI_HOST"; } >"$work/hosts"

# --hosts-from: 個票に出てくる宛先の名前を全部 127.0.0.1 へ (T14.29 の再生用)。
# **A だけ**足す (AAAA を足すと再生が Happy Eyeballs の測定になってしまう)。
REPLAY_NAMES=0
for f in ${HOSTS_FROM[@]+"${HOSTS_FROM[@]}"}; do
  [ -r "$f" ] || { echo "deployed-like: --hosts-from $f が読めません" >&2; exit 2; }
done
if [ -n "${HOSTS_FROM[*]+x}" ] && [ ${#HOSTS_FROM[@]} -gt 0 ]; then
  # `"target":"host:port"` の host だけを拾う。scheme (`connect://`) とポートを落とし、
  # IP リテラル (v4 / v6) と空の宛先は足さない (名前解決を通らないので要らない)
  names=$(
    grep -ho '"target":"[^"]*"' ${HOSTS_FROM[@]+"${HOSTS_FROM[@]}"} |
      sed -e 's/^"target":"//' -e 's/"$//' -e 's#^[a-z][a-z0-9+.-]*://##' -e 's/:[0-9]*$//' |
      grep -Ev '^(\[|$)' |
      grep -E '^[A-Za-z0-9_.-]+$' |
      grep -Ev '^([0-9]{1,3}\.){3}[0-9]{1,3}$' |
      LC_ALL=C sort -u
  )
  if [ -n "$names" ]; then
    while IFS= read -r n; do
      printf '127.0.0.1 %s\n' "$n" >>"$work/hosts"
      REPLAY_NAMES=$((REPLAY_NAMES + 1))
    done <<<"$names"
  fi
  if [ "$REPLAY_NAMES" -eq 0 ]; then
    echo "deployed-like: --hosts-from に名前の宛先がありません (\"target\":\"host:port\" を探します)" >&2
    exit 2
  fi
fi

cat >"$work/inner.sh" <<'INNER'
set -u
hosts=$1; nofile=$2; multi=$3; shift 3
# lo だけの網。IPv6 の既定経路は lo へ (出た SYN は戻って捨てられる = 黙って落ちる)
ip link set lo up || { echo "deployed-like: ip link set lo up に失敗" >&2; exit 2; }
ip -6 route add default dev lo || { echo "deployed-like: IPv6 の既定経路を置けません" >&2; exit 2; }
# プロキシ自身に cgroup の上限を見せる (`crates/sysinfo` は /proc/self/cgroup の道をたどる)
mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null ||
  echo "deployed-like: /sys/fs/cgroup を貼り直せません (上限は効きますが中からは読めません)" >&2
mount --bind "$hosts" /etc/hosts || { echo "deployed-like: /etc/hosts を差し替えられません" >&2; exit 2; }
ulimit -n "$nofile" || true
# 名前が引けることをここで 1 度だけ確かめる (中のコマンドが謎の失敗をしないように)
getent ahosts "$multi" >/dev/null || { echo "deployed-like: $multi が引けません" >&2; exit 2; }
exec "$@"
INNER

scope=()
if [ "$MEMORY" != off ] && [ "$MEMORY" != 0 ]; then
  if command -v systemd-run >/dev/null 2>&1 &&
     systemd-run --user --scope --quiet -p MemoryMax=64M -p MemorySwapMax=0 /bin/true >/dev/null 2>&1; then
    # ユーザーの scope は自分自身として走るので --uid/--gid は要らない
    # (`scripts/build-memory.sh` の detect_scope の user の枝と同じ)。
    scope=(systemd-run --user --scope --quiet --unit="rhp-t1416-$$.scope"
           -p "MemoryMax=$MEMORY" -p MemorySwapMax=0)
  else
    echo "deployed-like: systemd-run --user --scope が使えないので、メモリ上限は付けません"
    MEMORY="(none)"
  fi
else
  MEMORY="(none)"
fi

note=""
[ "$REPLAY_NAMES" -gt 0 ] && note=" + 再生の名前 $REPLAY_NAMES 件 (A だけ)"
echo "deployed-like: netns (lo only) | IPv6 default dev lo (SYN は黙って落ちる) |" \
  "/etc/hosts + $MULTI_HOST (2001:db8::1 + 127.0.0.1)$note | ulimit -n $NOFILE | MemoryMax=$MEMORY"
RHP_DEPLOYED_LIKE=1 ${scope[@]+"${scope[@]}"} \
  unshare -rmnC bash "$work/inner.sh" "$work/hosts" "$NOFILE" "$MULTI_HOST" "$@"
