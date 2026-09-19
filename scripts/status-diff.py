#!/usr/bin/env python3
# デプロイ先の `/status` をホスト別に読む道具 (TASKS.md §1「デプロイ先の測り方」)。
#
# ホスト別統計 (`hosts[]`) は `.rrd` に永続化されて**再起動をまたいで通算される**ので、
# `avg_ms` をそのまま読むと直す前の値が混ざり続ける。だから 2 回取って差分で見る:
# `avg_ms × timed` が合計 ms なので、`(avg2·timed2 − avg1·timed1) / (timed2 − timed1)` が
# **その間だけ**の平均になる。1 つだけ渡したときは通算をそのままの形で出す。
#
# CONNECT (`connect://host:port`) と forward (`http://host:port`) は別の表にする
# (デプロイ先は要求の 98% が CONNECT で、混ぜると forward の 3 ホストが見えない)。
# さらに **AAAA の有無で 2 群に分ける**: Happy Eyeballs の 250 ms (`crates/net-conn/src/net.rs` の
# `STAGGER`) を払うのは AAAA のあるホストだけなので、この 2 群の中央値の差が Phase 12 の主指標。
#
# 使い方:
#   scripts/status-diff.py A.json [B.json] [--aaaa FILE | --no-dns] [--min-timed N] [--top N]
#                          [--sort errors|dns|slow] [--group domain]
#     scripts/status-diff.py <(curl -s http://host:port/status)              # いまの通算
#     curl -s http://host:port/status > a.json; sleep 3600
#     curl -s http://host:port/status > b.json; scripts/status-diff.py a.json b.json
#     scripts/status-diff.py <(curl -s 'http://host:port/status?sort=errors') --sort errors
#
# **`/snapshot` の JSON もそのまま読める** (T14.4)。中の `/hosts` の部分 (最大 1,000 ホスト) を
# 使うので、`scripts/collect-deployed.sh` が保存したファイルをそのまま 1 枚でも 2 枚でも渡せる:
#     scripts/collect-deployed.sh nagoya.sorahost.net:50697        # 1 日 1 回取る
#     scripts/status-diff.py ~/rust-http-proxy-status/*-snapshot.json   # 最初と最後で差分
#
# **`/hosts` の JSON もそのまま読める** (T13.4)。`/status` の `hosts[]` は要求数の上位 50 だけ
# なので、`.rrd` にある全ホスト (最大 1,000) を見たいときはこちら:
#     scripts/status-diff.py <(curl -s 'http://host:port/hosts?limit=1000')
#     curl -s 'http://host:port/hosts?limit=1000' > a.json; sleep 86400
#     curl -s 'http://host:port/hosts?limit=1000' > b.json; scripts/status-diff.py a.json b.json
# `--aaaa` と組で 1,000 ホストの AAAA 別集計が取れる。`/hosts` は 1 件 325 B ほどなので
# 256 KiB に入りきらないと `"truncated": true` を付けて途中で切る (そのときは `--sort` か
# `?limit=` で絞る)。切れていたらこのスクリプトが 1 行警告を出す。
#
# **`--group domain` は eTLD+1 でまとめる** (T14.54)。`/hosts` は `img.dlsite.jp` と
# `www.dlsite.jp` が別の行なので「dlsite 全体で何件か」が読めない。`--group domain` は
# **同じ eTLD+1 のホストを 1 行にまとめる** (`img.dlsite.jp` + `www.dlsite.jp` -> `dlsite.jp`。
# `www.dlsite.com` は eTLD+1 が違うので別の行)。3 ラベルの `co.jp` / `ne.jp` … は
# `scripts/proxydata.py` の短い表で近似する (Public Suffix List は持たない)。
# 要求数・バイト・エラー・名前解決は和、`avg_ms` は計測数 (`timed`) で重みづけ、
# **p50 / p95 は 2 つ以上まとまった行では出ない** (ホスト別の分位点は足せない)。
#
# **`--sort` は 1 枚のときだけ** (`/status?sort=errors|dns|slow` を取った JSON をそのまま読んで、
# 同じ鍵で並べ、名前解決 / 確立の 1 回あたりとエラーの原因の列を足す。T13.3)。
# **差分は要求数順の 2 枚でだけ取る**: `?sort=` が変えるのは「上位 50 をどの鍵で切り出すか」なので、
# 鍵の違う 2 枚を引き算すると、入れ替わったホストの差が「新しく現れた」ように見えてしまう。
#
# **AAAA の判定はリゾルバ次第**なので注意。既定は `socket.getaddrinfo(host, AF_INET6)` だが、
# **この機械のリゾルバは一部のホストで AAAA を落とすことがある** (2026-09-10 の合議のときは
# www.google.com が「無し」と出た。同じ日に引き直したときは 50 ホスト全部が下の表と一致した)。
# **数字を残すときは引いた表を `--aaaa` で渡して固定する**。TASKS.md の表は dns.google の DoH の表:
#   scripts/status-diff.py snap.json --aaaa ~/rust-http-proxy-status/2026-09-10-aaaa-by-dns-google.json
# `--aaaa FILE` は `{"www.dlsite.com": true, "discord.com": false}` 形式の JSON。
# `--no-dns` は判定を省く (群分けをせず、全ホストを 1 つの表に出す)。
#
# **差分の精度に注意**: `/status` の `avg_ms` は小数第 1 位までなので、合計 ms の丸め誤差は
# 最悪 `0.05 × timed`。これが `Δtimed` で割られるため、**Δtimed が小さいと差分は当てにならない**
# (実測: 8 分あけた 2 枚では Δtimed = 2 に対して通算 timed が 3,930 で、誤差は ±98 ms)。
# `Δavg_ms` の右にこの誤差の上限を `±` で出すので、**桁で読める幅になるまで間隔をあける**こと
# (デプロイ先は 0.012 req/s = 80 秒に 1 件なので、1 ホストで数十件貯めるには数時間から 1 日かかる)。
#
# **応答の形の版 (`schema`。T14.49)**: 新しいプロキシの応答は先頭に `"schema":1` を持ちます。
# この道具は版で分岐しますが、**版の無い古い出力 (版 0) も今までどおり読めます**
# (手元に残っている雪像はどれも版の無い形なので、読めなくなると過去の分析をやり直せない)。
# 読んだ版は出力の先頭に `# <ファイル> の形の版 schema=N` として出ます。
#
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import os
import sys

# 読む部分は `scripts/proxydata.py` に置いてある (`snapshot-diff.py` (T14.17) と共有。
# このファイルの隣なので、どこから呼んでも見つかるように 1 行だけ足す)
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proxydata import (  # noqa: E402
    causes_text,
    cell,
    fmt_bytes,
    fmt_ms,
    group_by_domain,
    load,
    median,
    per_conn,
    per_miss,
    resolve_aaaa,
    reqs,
    row_of,
    schema_of,
    up,
    warn_newer,
)


def print_table(title, rows, diff, aaaa_shown, detail=False, group=False):
    print()
    unit = "まとめの単位" if group else "ホスト"
    print(f"== {title} ({len(rows)} {unit}"
          + (f"、{sum(r['hosts'] for r in rows)} ホスト) ==" if group else ") =="))
    if not rows:
        print("  (該当なし)")
        return
    width = min(52, max(len(r["name"]) for r in rows))
    head = f"  {'domain' if group else 'host':<{width}}"
    if group:
        head += f" {'n':>3}"
    head += f" {'AAAA':>4} {'req':>7} {'timed':>7} {'avg_ms':>9}"
    if diff:
        head += f" {'±':>7}"
    head += f" {'p50':>7} {'p95':>7} {'max':>7} {'bytes':>9} {'err':>4}"
    if detail:
        # `--sort` のときだけ足す列 (待ちの内訳とエラーの原因。T13.3)
        head += f" {cell('dns/回', 7)} {cell('dns計', 8)} {cell('接続/回', 8)}  原因"
    print(head)
    for r in rows:
        name = r["name"] if len(r["name"]) <= width else r["name"][: width - 1] + "…"
        aaaa = {True: "有", False: "無", None: "?"}[r["aaaa"]] if aaaa_shown else "-"
        line = f"  {name:<{width}}"
        if group:
            line += f" {r['hosts']:>3}"
        line += (f" {cell(aaaa, 4)} {r['requests']:>7} {r['timed']:>7} "
                 f"{fmt_ms(r['avg_ms']):>9}")
        if diff:
            err = "—" if r["avg_err"] is None else "%.1f" % r["avg_err"]
            line += f" {err:>7}"
        line += (f" {fmt_ms(r['p50_ms']):>7} {fmt_ms(r['p95_ms']):>7} "
                 f"{fmt_ms(r['max_ms']):>7} {fmt_bytes(r['bytes']):>9} {r['errors']:>4}")
        if detail:
            line += (f" {cell(fmt_ms(per_miss(r)), 7)} {fmt_ms(r['dns_ms_sum']):>8} "
                     f"{cell(fmt_ms(per_conn(r)), 8)}  {causes_text(r['errors_by_cause'])}")
        print(line)


def print_groups(title, rows, diff):
    """AAAA の有無で 2 群に分けて中央値を出す (Phase 12 の主指標)。"""
    total_req = sum(r["requests"] for r in rows)
    if not rows:
        return
    print()
    print(f"-- {title}: AAAA の有無で分けた中央値 (要求 {total_req:,}) --")
    label = {True: "AAAA あり", False: "AAAA なし", None: "AAAA 不明"}
    for flag in (True, False, None):
        g = [r for r in rows if r["aaaa"] is flag]
        if not g:
            continue
        req = sum(r["requests"] for r in g)
        share = 100.0 * req / total_req if total_req else 0.0
        avgs = [r["avg_ms"] for r in g if r["avg_ms"] is not None]
        p50s = [r["p50_ms"] for r in g if r["p50_ms"] is not None]
        who = "その間の" if diff else "通算の"
        print(f"  {label[flag]}: {len(g)} ホスト、要求 {req:,} ({share:.1f}%)、"
              f"{who} avg の中央値 {fmt_ms(median(avgs))} ms"
              + ("" if diff else f"、p50 の中央値 {fmt_ms(median(p50s))} ms"))
    if diff:
        print("  (差分では p50 / p95 / max は通算の値。区間の分位点は差し引けない)")


def main(argv=None):
    # 引数を渡せるようにしてあるのは単体テスト (`scripts/test_status_diff.py`) から
    # 呼ぶため (`snapshot-diff.py` の `main(argv=None)` と同じ作法)
    p = argparse.ArgumentParser(
        description="デプロイ先の /status (または /hosts) をホスト別に読む "
                    "(1 つなら通算、2 つなら差分)")
    p.add_argument("files", nargs="+", metavar="STATUS.json",
                   help="/status か /hosts か /snapshot の JSON (1 つか 2 つ)")
    p.add_argument("--aaaa", metavar="FILE", help='{"host": true/false} の JSON で AAAA の有無を与える')
    p.add_argument("--no-dns", action="store_true", help="AAAA を引かない (群分けをしない)")
    p.add_argument("--min-timed", type=int, default=0, metavar="N",
                   help="timed がこの数に満たないホストを表から省く (既定 0)")
    p.add_argument("--top", type=int, default=0, metavar="N", help="表に出すホスト数 (既定は全部)")
    p.add_argument("--group", choices=["host", "domain"], default="host", metavar="KEY",
                   help="行のまとめ方 (既定 host)。domain は eTLD+1 でまとめる "
                        "(img.dlsite.jp と www.dlsite.jp が dlsite.jp の 1 行。T14.54)")
    p.add_argument("--sort", choices=["errors", "dns", "slow"], metavar="KEY",
                   help="1 枚のときの並びを errors|dns|slow にし、内訳の列を足す "
                        "(/status?sort= を取った JSON をそのまま読むため。差分は要求数順の 2 枚で取る)")
    args = p.parse_args(argv)

    if len(args.files) > 2:
        p.error("渡せるのは 1 つか 2 つ")
    if args.sort and len(args.files) == 2:
        # 鍵の違う 2 枚を引き算すると、入れ替わったホストの差が「新しく現れた」ように見える
        p.error("--sort は 1 枚のときだけ (差分は要求数順の 2 枚で取る)")
    snaps = [load(f) for f in args.files]
    diff = len(snaps) == 2
    if diff:
        a, b = snaps
        older = {h["host"]: h for h in a["hosts"]}
        newer = {h["host"]: h for h in b["hosts"]}
        rows = [row_of(k, older.get(k), v) for k, v in newer.items()]
    else:
        a, b = snaps[0], None
        rows = [row_of(h["host"], h, None) for h in a["hosts"]]

    mode, table = "dns", {}
    if args.no_dns:
        mode = "none"
    elif args.aaaa:
        mode, table = "file", load(args.aaaa)
    names = sorted({r["name"] for r in rows})
    aaaa = resolve_aaaa(names, mode, table)
    for r in rows:
        r["aaaa"] = aaaa.get(r["name"])

    # eTLD+1 でまとめる (AAAA を引いたあと。群ごとに「全部あり / 全部なし」を見るため)
    if args.group == "domain":
        rows = group_by_domain(rows)

    src = {"dns": "getaddrinfo", "file": args.aaaa, "none": "引かない"}[mode]
    print(f"# status-diff: {' -> '.join(args.files)}")
    # 応答の形の版 (T14.49)。**版の無い古い出力は「版 0」** として今までどおり読む
    for name, snap in zip(args.files, snaps):
        v = schema_of(snap)
        print(f"# {name} の形の版 schema={v}"
              + ("" if v else " (版を持たない古い出力。推測で読む)"))
        warn_newer(snap, name, out=sys.stdout)
    for name, snap in zip(args.files, snaps):
        if snap.get("snapshot_taken_at"):
            print(f"# {name} は /snapshot (T14.4) の中の /hosts を読んだ "
                  f"(取得 {snap['snapshot_taken_at']}、ホスト {len(snap.get('hosts', []))} 件)")
    if diff:
        d_up = up(b) - up(a)
        note = "" if d_up > 0 else "  **再起動をまたいでいる** (uptime が減った)"
        print(f"# {'差分':<4} uptime {up(a):,} -> {up(b):,} 秒 (Δ{d_up:+,})"
              f"、total_requests {reqs(a):,} -> {reqs(b):,} (起動からの窓){note}")
    else:
        print(f"# {'通算':<4} uptime {up(a):,} 秒、total_requests {reqs(a):,} "
              f"(起動からの窓。hosts[] は .rrd の通算なので窓が違う)")
    # `/hosts` は 256 KiB でバイト数打ち切りをするので、切れていたら言う (T13.4)
    for name, snap in zip(args.files, snaps):
        if snap.get("truncated"):
            print(f"# **注意** {name} は途中で切れている "
                  f"(count {snap.get('count')} のうち shown {snap.get('shown')})。"
                  "?limit= か ?sort= で絞ること")
    print(f"# AAAA の判定: {src} / ホスト {len(names)}")

    shown = [r for r in rows if r["timed"] >= args.min_timed]
    # 並べ替えの鍵は `/status?sort=` と同じ (同点は要求数 → 名前で崩す。T13.3)
    order = {
        "errors": lambda r: (-r["errors"], -r["dns_ms_sum"], -r["requests"], r["name"]),
        "dns": lambda r: (-r["dns_ms_sum"], -r["dns_misses"], -r["requests"], r["name"]),
        "slow": lambda r: (-(r["avg_ms"] or 0.0), -r["requests"], r["name"]),
    }.get(args.sort, lambda r: (-r["requests"], r["name"]))
    shown.sort(key=order)
    if args.top:
        shown = shown[: args.top]
    con = [r for r in shown if r["connect"]]
    fwd = [r for r in shown if not r["connect"]]
    aaaa_shown = mode != "none"
    if args.sort:
        print(f"# 並び  {args.sort} (/status?sort={args.sort} と同じ鍵。"
              "dns/回 = dns_ms_sum ÷ dns_misses、接続/回 = connect_ms_sum ÷ timed)")
    group = args.group == "domain"
    if group:
        print("# まとめ  eTLD+1 (--group domain。n = まとめたホスト数、"
              "avg_ms は timed で重みづけ、p50 / p95 は 2 つ以上では出ない)")
    print_table("CONNECT", con, diff, aaaa_shown, bool(args.sort), group)
    print_table("forward", fwd, diff, aaaa_shown, bool(args.sort), group)
    if aaaa_shown:
        print_groups("CONNECT", con, diff)
        print_groups("forward", fwd, diff)
    if diff:
        print()
        print("注: Δavg_ms の ± は avg_ms の丸め (0.1 ms 刻み) から来る誤差の上限。"
              "Δtimed が小さいと当てにならない。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
