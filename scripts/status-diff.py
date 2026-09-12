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
# さらに **AAAA の有無で 2 群に分ける**: Happy Eyeballs の 250 ms (`crates/net/src/net.rs` の
# `STAGGER`) を払うのは AAAA のあるホストだけなので、この 2 群の中央値の差が Phase 12 の主指標。
#
# 使い方:
#   scripts/status-diff.py A.json [B.json] [--aaaa FILE | --no-dns] [--min-timed N] [--top N]
#                          [--sort errors|dns|slow]
#     scripts/status-diff.py <(curl -s http://host:port/status)              # いまの通算
#     curl -s http://host:port/status > a.json; sleep 3600
#     curl -s http://host:port/status > b.json; scripts/status-diff.py a.json b.json
#     scripts/status-diff.py <(curl -s 'http://host:port/status?sort=errors') --sort errors
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
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import json
import socket
import statistics
import sys
import unicodedata
from concurrent.futures import ThreadPoolExecutor

CONNECT = "connect://"
# `/status` の `errors_by_cause` の並び (`crates/metrics/src/metrics.rs` の ERR_CAUSE_NAMES)
CAUSE_NAMES = ["dns", "refused", "unreachable", "timeout", "reset", "tls", "loop", "other"]


def host_name(key):
    """`connect://www.example.com:443` -> `www.example.com`"""
    rest = key.split("://", 1)[-1]
    if rest.startswith("["):  # IPv6 リテラル
        return rest.split("]", 1)[0][1:]
    return rest.rsplit(":", 1)[0] if ":" in rest else rest


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def has_aaaa(name):
    try:
        return bool(socket.getaddrinfo(name, None, socket.AF_INET6))
    except OSError:
        return False


def resolve_aaaa(names, mode, table):
    """ホスト名 -> True / False / None (不明)"""
    if mode == "none":
        return {n: None for n in names}
    if mode == "file":
        return {n: table.get(n) for n in names}
    with ThreadPoolExecutor(max_workers=16) as pool:
        return dict(zip(names, pool.map(has_aaaa, names)))


def row_of(key, a, b):
    """1 ホストぶんの値。b が None なら通算、あれば差分。"""
    zero = {"requests": 0, "timed": 0, "avg_ms": 0.0, "bytes": 0, "hits": 0,
            "misses": 0, "errors": 0, "blocked": 0}
    if b is None:
        cur, old = a, zero
    else:
        cur, old = b, (a or zero)
    d_timed = cur["timed"] - old["timed"]
    ms_sum = cur["avg_ms"] * cur["timed"] - old["avg_ms"] * old["timed"]
    if d_timed > 0:
        avg = ms_sum / d_timed
        # avg_ms は小数第 1 位までなので、合計 ms の丸め誤差は最悪 0.05 × timed。
        err = 0.05 * (cur["timed"] + old["timed"]) / d_timed
    else:
        avg, err = None, None
    # 待ちの内訳 (T12.4 (2) より前の古い雪像には無いので、無ければ 0)
    def d(name):
        return cur.get(name, 0) - old.get(name, 0)
    causes = [x - y for x, y in zip(cur.get("errors_by_cause", [0] * len(CAUSE_NAMES)),
                                    old.get("errors_by_cause", [0] * len(CAUSE_NAMES)))]
    return {
        "key": key,
        "name": host_name(key),
        "connect": key.startswith(CONNECT),
        "requests": cur["requests"] - old["requests"],
        "timed": d_timed,
        "avg_ms": avg,
        "avg_err": err,
        "bytes": cur["bytes"] - old["bytes"],
        "errors": cur["errors"] - old["errors"],
        "p50_ms": cur.get("p50_ms"),
        "p95_ms": cur.get("p95_ms"),
        "max_ms": cur.get("max_ms"),
        "dns_ms_sum": d("dns_ms_sum"),
        "dns_misses": d("dns_misses"),
        "connect_ms_sum": d("connect_ms_sum"),
        "errors_by_cause": causes,
    }


def causes_text(counts):
    """`[80, 0, ...]` -> `dns 80`。全部 0 なら `—`。"""
    shown = [f"{CAUSE_NAMES[i]} {n}" for i, n in enumerate(counts) if n]
    return " ".join(shown) if shown else "—"


def per_miss(row):
    """名前解決のミス 1 回あたりの ms (ミスが無ければ None)。"""
    return row["dns_ms_sum"] / row["dns_misses"] if row["dns_misses"] else None


def per_conn(row):
    """確立 1 回あたりの ms (計った要求が無ければ None)。"""
    return row["connect_ms_sum"] / row["timed"] if row["timed"] else None


def fmt_bytes(n):
    if n < 0:
        return "-" + fmt_bytes(-n)
    for unit, div in (("GB", 1 << 30), ("MB", 1 << 20), ("kB", 1 << 10)):
        if n >= div:
            return f"{n / div:.1f} {unit}"
    return f"{n} B"


def cell(s, width):
    """全角を 2 桁と数えて右詰めする (AAAA の欄の 有 / 無 用)。"""
    shown = sum(2 if unicodedata.east_asian_width(c) in "WF" else 1 for c in s)
    return " " * max(0, width - shown) + s


def fmt_ms(v):
    return "—" if v is None else f"{v:.1f}"


def median(values):
    return statistics.median(values) if values else None


def print_table(title, rows, diff, aaaa_shown, detail=False):
    print()
    print(f"== {title} ({len(rows)} ホスト) ==")
    if not rows:
        print("  (該当なし)")
        return
    width = min(52, max(len(r["name"]) for r in rows))
    head = f"  {'host':<{width}} {'AAAA':>4} {'req':>7} {'timed':>7} {'avg_ms':>9}"
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
        line = (f"  {name:<{width}} {cell(aaaa, 4)} {r['requests']:>7} {r['timed']:>7} "
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


def main():
    p = argparse.ArgumentParser(
        description="デプロイ先の /status をホスト別に読む (1 つなら通算、2 つなら差分)")
    p.add_argument("files", nargs="+", metavar="STATUS.json", help="/status の JSON (1 つか 2 つ)")
    p.add_argument("--aaaa", metavar="FILE", help='{"host": true/false} の JSON で AAAA の有無を与える')
    p.add_argument("--no-dns", action="store_true", help="AAAA を引かない (群分けをしない)")
    p.add_argument("--min-timed", type=int, default=0, metavar="N",
                   help="timed がこの数に満たないホストを表から省く (既定 0)")
    p.add_argument("--top", type=int, default=0, metavar="N", help="表に出すホスト数 (既定は全部)")
    p.add_argument("--sort", choices=["errors", "dns", "slow"], metavar="KEY",
                   help="1 枚のときの並びを errors|dns|slow にし、内訳の列を足す "
                        "(/status?sort= を取った JSON をそのまま読むため。差分は要求数順の 2 枚で取る)")
    args = p.parse_args()

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

    src = {"dns": "getaddrinfo", "file": args.aaaa, "none": "引かない"}[mode]
    print(f"# status-diff: {' -> '.join(args.files)}")
    if diff:
        d_up = b["uptime_secs"] - a["uptime_secs"]
        note = "" if d_up > 0 else "  **再起動をまたいでいる** (uptime が減った)"
        print(f"# {'差分':<4} uptime {a['uptime_secs']:,} -> {b['uptime_secs']:,} 秒 (Δ{d_up:+,})"
              f"、total_requests {a['total_requests']:,} -> {b['total_requests']:,} (起動からの窓){note}")
    else:
        print(f"# {'通算':<4} uptime {a['uptime_secs']:,} 秒、total_requests {a['total_requests']:,} "
              f"(起動からの窓。hosts[] は .rrd の通算なので窓が違う)")
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
    print_table("CONNECT", con, diff, aaaa_shown, bool(args.sort))
    print_table("forward", fwd, diff, aaaa_shown, bool(args.sort))
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
