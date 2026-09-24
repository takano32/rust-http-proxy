#!/usr/bin/env python3
# `/snapshot` (T14.4) を 1 枚の Markdown の「要点」に畳む (`scripts/collect-deployed.sh` が呼ぶ)。
#
# 見るのは T14.0 の分析で実際に要った順: 全体 → CPU → 名前解決 → エラー → 閉じた接続 → いまの接続。
# **`--prev` を渡すと通算の指標は差分**になる (`/status` の数はどれも起動からの通算なので、
# そのまま読むと「いつからの値か」が混ざる)。
#
# **T15.0 (15) で足した欄** (どれも「無ければ出さない」ので、古い雪像もそのまま読める):
#   - CPU の表 … `/profile` の CPU (割り当てに対する使用率)・`/status` の `kernel.cgroup_cpu`
#     (絞られた周期の割合)・`threads_top` (上位スレッド)・`run_delay_us` (走れずに待った時間)
#   - 名前解決 … `misses_by_kind` (ミスの種類別) と引き直しの失敗・遅れ・最大 ms
#   - 待ち … `/history` の `wait` (利用者が待つ時間 = `queue + client_read + dns + connect`)
#   - いまの接続 … **動かないトンネル** (`idle_secs` ≥ 300 秒) の `spins` / `revents` / 半閉じ
#
# 使い方: scripts/snapshot-summary.py SNAP.json [--prev PREV.json] [--status-before STATUS.json]
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import json
import statistics
import sys
from datetime import datetime, timezone

# `/status` の `errors_by_cause` の並び (`crates/metrics/src/metrics.rs` の ERR_CAUSE_NAMES)
CAUSE_NAMES = ["dns", "refused", "unreachable", "timeout", "reset", "tls", "loop", "other"]
# 名前解決のミスの種類 (`crates/net-dns/src/dns.rs` の `misses_by_kind_json`。この順で和が `misses`)
MISS_KINDS = ["cold", "expired", "warm_stale", "negative"]
# 「動かないトンネル」と見なす秒 (T15.0 (14) の画面と同じ固定の閾。見出しに書く)
IDLE_TUNNEL_SECS = 300


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def num(v, unit=""):
    if v is None:
        return "—"
    if isinstance(v, float):
        return f"{v:,.1f}{unit}"
    return f"{v:,}{unit}"


def stamp(t):
    """epoch 秒を UTC の読める形に (`scripts/snapshot-diff.py` の `stamp()` と同じ綴り)。

    同じ Markdown の中で `1789601514` と `2026-09-16 23:31:54Z` が混ざると、
    読み手がどの時間帯の話か計算しないと分からない。
    """
    if not t:
        return "—"
    return datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")


def fmt_bytes(n):
    """バイト数を読める形に (`scripts/proxydata.py` の `fmt_bytes()` と同じ単位)。

    **割るのは 1,024 なので単位は GiB / MiB / KiB** (画面の `fmtBytes` と同じ)。
    """
    if n is None:
        return "—"
    for unit, div in (("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)):
        if abs(n) >= div:
            return f"{n / div:.1f} {unit}"
    return f"{n} B"


def quantile(buckets, count, top, p, bounds):
    """`/history` の対数バケツから分位点を出す (dashboard.html の `winQuantile` と同じ)。"""
    if not count or not buckets:
        return None
    rank = max(1.0, p * count)
    seen = 0
    for i, n in enumerate(buckets):
        if not n:
            continue
        if seen + n >= rank:
            lo = 0 if i == 0 else bounds[i - 1]
            hi = bounds[i] if i < len(bounds) else max(top, lo)
            return min(lo + (hi - lo) * ((rank - seen) / n), top)
        seen += n
    return top


# 分布の列に対する「件数」の列 (`wait` は T15.0 (10) で足した `waits`)
COUNT_KEY = {"connect": "connects", "forward": "forwards", "wait": "waits"}


def merge(history, prefix, last=None):
    """履歴の標本を足し合わせて (件数, バケツ, 最大, 合計 ms) にする。`last` で直近 N 標本だけ。"""
    if not history:
        return 0, None, 0, 0.0
    keys = history.get("keys") or []
    rows = history.get("samples") or []
    if last:
        rows = rows[-last:]
    idx = {k: i for i, k in enumerate(keys)}
    need = [prefix + "_buckets", prefix + "_ms_max", prefix + "_ms_sum", COUNT_KEY[prefix]]
    if any(k not in idx for k in need):
        return 0, None, 0, 0.0
    count, buckets, top, total = 0, None, 0, 0.0
    for r in rows:
        b = r[idx[prefix + "_buckets"]]
        if not b:
            continue
        if buckets is None:
            buckets = [0] * len(b)
        for j, v in enumerate(b):
            buckets[j] += v
        count += r[idx[need[3]]] or 0
        total += r[idx[prefix + "_ms_sum"]] or 0
        top = max(top, r[idx[prefix + "_ms_max"]] or 0)
    return count, buckets, top, total


def latency_line(label, history, prefix, last=None):
    count, buckets, top, total = merge(history, prefix, last)
    if not count:
        # **「0 本」と「その列が無い」と「部ごと入っていない」は 3 つとも別**。
        # `wait` は T15.0 より前の雪像に無く、0 本と書くと「誰も待っていない」と
        # 読まれる。`history.5` は `/snapshot` が大きいときに落とす 3 つのうちの 1 つ
        # (`crates/endpoints/src/endpoints/recent.rs` の `DROP_ORDER`) なので、
        # 落ちたときに「その列は無い」と書くと「この版は測っていない」と読まれる (T15.0 (15))
        keys = history.get("keys") or []
        if not keys:
            return f"| {label} | (この部は雪像に入っていない) | — | — | — |"
        if prefix + "_buckets" not in keys:
            return f"| {label} | (この雪像にその列は無い) | — | — | — |"
        return f"| {label} | 0 本 | — | — | — |"
    bounds = history.get("bounds_ms") or []
    p50 = quantile(buckets, count, top, 0.5, bounds)
    p95 = quantile(buckets, count, top, 0.95, bounds)
    return (f"| {label} | {count:,} 本 | {total / count:.1f} ms | "
            f"{num(p50)} / {num(p95)} ms | {num(top)} ms |")


def tally(rows, key):
    out = {}
    for r in rows:
        out[r.get(key)] = out.get(r.get(key), 0) + 1
    return dict(sorted(out.items(), key=lambda kv: -kv[1]))


def host_errors(snap):
    """`/hosts` を**ホストの鍵で引ける形**にする (エラー件数と原因別、切れているか)。

    `/status` にエラーの合計が無いのでホスト別を足すしかないが、**表ごと足して引く**と
    `/hosts` が 256 KiB で切れているときに「後の表から消えたホスト」のぶんが負の数になる。
    """
    h = part(snap, "hosts")
    rows = h.get("hosts") or []
    out = {}
    for r in rows:
        causes = [0] * len(CAUSE_NAMES)
        for i, v in enumerate(r.get("errors_by_cause") or []):
            if i < len(causes):
                causes[i] = v
        out[r.get("host")] = (r.get("errors", 0), causes)
    return {"rows": out, "truncated": bool(h.get("truncated")),
            "count": h.get("count"), "shown": h.get("shown") or len(rows)}


def error_delta(cur, prev):
    """前後の `/hosts` を**ホストの鍵で突き合わせて**引く (`--prev` が無ければ後の合計)。

    2026-09-18 の実測 (`2026-09-18T053213Z-snapshot.json` と 2 日前の雪像): 表ごとの
    合計は 52 → 48 で **−4 件**、鍵で突き合わせると **+4 件** (dns 1・refused 1・timeout 2)
    で `/errors` の個票 4 件と一致した。差は「後の表から消えた 8 ホスト (エラー 8 件)」で、
    どちらの `/hosts` も `truncated` (1,000 件中 643 / 639 件) なので下位は簡単に出入りする。
    ホスト 1 件の通算が**減っている**ときは `.rrd` の作り直しなので引き算しない。
    """
    now = host_errors(cur)
    zero = (0, [0] * len(CAUSE_NAMES))
    if prev is None:
        causes = [sum(c[i] for _, c in now["rows"].values()) for i in range(len(CAUSE_NAMES))]
        return {"total": sum(e for e, _ in now["rows"].values()), "causes": causes,
                "back": 0, "gone": 0, "gone_errors": 0, "truncated": now["truncated"],
                "shown": [now["shown"]], "count": [now["count"]], "windowed": False}
    old = host_errors(prev)
    total, causes, back = 0, [0] * len(CAUSE_NAMES), 0
    for key, (errs, cs) in now["rows"].items():
        oe, oc = old["rows"].get(key, zero)
        if errs < oe:
            back += 1
            continue
        total += errs - oe
        for i in range(len(CAUSE_NAMES)):
            causes[i] += cs[i] - oc[i]
    gone = [k for k in old["rows"] if k not in now["rows"]]
    return {"total": total, "causes": causes, "back": back, "gone": len(gone),
            "gone_errors": sum(old["rows"][k][0] for k in gone),
            "truncated": now["truncated"] or old["truncated"],
            "shown": [old["shown"], now["shown"]], "count": [old["count"], now["count"]],
            "windowed": True}


def user_kernel(prof, cg):
    """CPU のユーザー空間とカーネル側 (T16.0)。どの源も無ければ None (行ごと出さない)。

    コンテナは `kernel.cgroup_cpu.since_start` の `user_usec` / `system_usec` (起動から)、
    プロセスは `/profile` の `cpu_user_us` (カーネル側は `cpu_us − cpu_user_us`)、
    役割は `user_us` の多い順に上位 3 つを「ユーザー / カーネル」で。
    """
    parts = []
    since = (cg or {}).get("since_start") or {}
    su, ss = since.get("user_usec"), since.get("system_usec")
    if su is not None and ss is not None and su + ss > 0:
        parts.append(f"コンテナ (起動から) {su / 1e6:,.1f} / {ss / 1e6:,.1f} 秒"
                     f" (ユーザー **{su / (su + ss) * 100:.1f}%**)")
    if prof and prof.get("cpu_user_us") is not None and prof["cpu_us"] > 0:
        u, c = prof["cpu_user_us"], prof["cpu_us"]
        parts.append(f"プロセス {u / 1000:,.0f} / {max(0, c - u) / 1000:,.0f} ms"
                     f" (ユーザー **{u / c * 100:.1f}%**)")
    if prof and prof.get("role_user_us"):
        rows = sorted(zip(prof["roles"], prof["role_user_us"], prof["role_cpu_us"]),
                      key=lambda x: (-x[1], -x[2]))
        shown = "、".join(f"{r} {u / 1000:,.0f} / {max(0, c - u) / 1000:,.0f} ms"
                         for r, u, c in rows[:3] if u or c)
        if shown:
            parts.append(f"役割の上位 {shown}")
    return "。".join(parts) or None


def profile_totals(snap):
    """`/profile` の標本を足して (窓の秒数・CPU・役割ごとの CPU・上位スレッド・遅れ) にする。

    1 標本は `[t, requests, cpu_us, [connect...], [forward...], [roles...], [locks...],
    [queue...], threads_top, run_delay_us, user_us, cpu_user_us]` で、**位置ではなく `keys` の名前で引く**
    (`crates/endpoints/src/endpoints/profile.rs` の `keys`)。役割の枠は標本 0 なら `0` 1 文字、
    上位スレッドは 1 本も無い窓なら `0`、`schedstat` の無いカーネルでは `run_delay_us` が `null`。
    `user_us` (役割ごと) と `cpu_user_us` (プロセス全体) は CPU のうちユーザー空間 (T16.0)。
    無い版では `role_user_us` / `cpu_user_us` が `None`。
    部が無い版では `None` を返す (呼ぶ側が表ごと出さない)。
    """
    p = part(snap, "profile")
    rows = p.get("samples") or []
    keys = p.get("keys") or []
    if not rows or "cpu_us" not in keys:
        return None
    idx = {k: i for i, k in enumerate(keys)}
    roles = p.get("roles") or []
    out = {"samples": len(rows), "interval_secs": p.get("interval_secs") or 0,
           "roles": roles, "cpu_us": 0, "role_cpu_us": [0] * len(roles),
           "run_delay_us": None, "top": [],
           # T16.0 (無い版では None のまま)
           "cpu_user_us": None, "role_user_us": None}

    def col(row, name):
        i = idx.get(name)
        return row[i] if i is not None and i < len(row) else None

    top = {}
    for r in rows:
        out["cpu_us"] += col(r, "cpu_us") or 0
        user = col(r, "cpu_user_us")
        if user is not None:
            out["cpu_user_us"] = (out["cpu_user_us"] or 0) + user
        role_user = col(r, "user_us")
        if role_user:
            if out["role_user_us"] is None:
                out["role_user_us"] = [0] * len(roles)
            for i, v in enumerate(role_user):
                if i < len(out["role_user_us"]):
                    out["role_user_us"][i] += v or 0
        for i, t in enumerate(col(r, "threads") or []):
            if t and i < len(out["role_cpu_us"]):
                out["role_cpu_us"][i] += t[0] or 0
        delay = col(r, "run_delay_us")
        if delay:
            if out["run_delay_us"] is None:
                out["run_delay_us"] = [0] * len(roles)
            for i, v in enumerate(delay):
                if i < len(out["run_delay_us"]):
                    out["run_delay_us"][i] += v or 0
        for t in col(r, "threads_top") or []:
            tid, comm, role, cpu_us, running = (list(t) + [0] * 5)[:5]
            cur = top.setdefault(tid, {"comm": comm, "role": role, "cpu_us": 0, "running": 0})
            cur["comm"], cur["role"] = comm, role
            cur["cpu_us"] += cpu_us or 0
            cur["running"] += running or 0
    out["secs"] = out["samples"] * out["interval_secs"]
    out["top"] = sorted(({"tid": k, **v} for k, v in top.items()), key=lambda x: -x["cpu_us"])
    return out


def cores(cpu_us, secs):
    """CPU の us と窓の秒から「何コアぶん」か (窓が 0 秒なら None)。"""
    return (cpu_us / 1e6 / secs) if secs else None


def idle_tunnels(conns, edge=IDLE_TUNNEL_SECS):
    """**1 バイトも動いていないトンネル** (`idle_secs` ≥ `edge`) を長い順に (T15.0 (4))。

    `idle_secs` は CONNECT の行にしか出ず、既定値 (0) の行では**欄ごと出ない**ので
    「無ければ 0」と読む (`crates/metrics-recent/src/recent.rs` の `Slot::to_json`)。
    """
    rows = [c for c in conns if (c.get("idle_secs") or 0) >= edge]
    return sorted(rows, key=lambda c: -(c.get("idle_secs") or 0))


def conn_evidence(c):
    """`/connections` の 1 行のうち T15.0 (4) が足した証拠だけを 1 行に (無い欄は飛ばす)。"""
    out = [f"idle {num(c.get('idle_secs'))} 秒", f"齢 {num(c.get('age_secs'))} 秒",
           fmt_bytes(c.get("bytes")), f"{num(c.get('rate_bps'))} bps"]
    if c.get("tid"):
        out.append(f"tid {c['tid']}")
    if c.get("spins"):
        out.append(f"spins {num(c['spins'])}")
    if c.get("half_closed"):
        out.append(f"半閉じ {c['half_closed']} {num(c.get('half_closed_secs'))} 秒")
    # 旗は**立っている側だけ**書く (`{"client":"HUP","origin":""}` の空の側を出すと読みにくい)
    rev = c.get("revents") or {}
    flags = [f"{side}=`{rev[side]}`" for side in ("client", "origin") if rev.get(side)]
    if flags:
        out.append("revents " + " ".join(flags))
    return " / ".join(out)


def part(snap, name):
    """雪像の部を取り出す (`parts` の `history.5` のような入れ子の名前も辿る)。"""
    v = snap
    for key in name.split("."):
        v = v.get(key) if isinstance(v, dict) else None
    return v if isinstance(v, dict) else {}


def restart_between(a, b, wall):
    """2 枚の間に再起動があったか (`snapshot-diff.py` の `restart_info()` と同じ見方)。

    `uptime_secs` の大小だけを見ると、**再起動の直後に取った雪像**が前のときに見落とす
    (2026-09-16 の 0 時間の雪像は `uptime_secs` 61 秒で、後の 131,881 秒より小さい)。
    版の違いと「`uptime_secs` の伸びが窓に足りない」も再起動として数える。
    """
    va, vb = a.get("version"), b.get("version")
    if va and vb and va != vb:
        return True
    ua = (part(a, "status").get("uptime_secs") or 0)
    ub = (part(b, "status").get("uptime_secs") or 0)
    if ub < ua:
        return True
    # 時計のずれと取得の間の分を見込んで、窓の 1% (最低 60 秒) は許す
    return bool(wall and wall > 0 and (ub - ua) + max(60, wall // 100) < wall)


def truncated_parts(snap):
    """**応答が 256 KiB で切れている部**を (名前, 出た件数, 全件) で並べる。

    雪像の top-level の `dropped` は「4 MiB を越えたので**部ごと**落とした」の意味で
    (`crates/endpoints/src/endpoints/recent.rs`)、部の中の打ち切りは載らない。
    2026-09-18 の雪像は `dropped` が空のまま `recent` / `hosts` / `profile` の 3 つが
    切れていて、要約からは「CPU は 1 日ぶんでなく 456 標本ぶん」が読めなかった。
    """
    out = []
    for name in snap.get("parts") or []:
        v = part(snap, name)
        if v.get("truncated"):
            out.append((name, v.get("shown"), v.get("count")))
    return out


def main(argv=None):
    p = argparse.ArgumentParser(description="/snapshot を 1 枚の要点に畳む")
    p.add_argument("snapshot")
    p.add_argument("--prev", metavar="PREV.json", help="前回の雪像 (通算の指標を差分にする)")
    # 雪像を配ること自体が RSS を約 1.0 MB 押し上げるので、RSS だけは雪像の**前**に
    # 取った `/status` を使う (T15.0 (15)。ほかの通算は 1 秒差で意味が変わらない)
    p.add_argument("--status-before", metavar="STATUS.json",
                   help="雪像の前に取った /status (**RSS だけ**こちらの値を使う)")
    args = p.parse_args(argv)
    d = load(args.snapshot)
    prev = load(args.prev) if args.prev else None
    before = load(args.status_before) if args.status_before else None
    if "parts" not in d:
        print("(これは /snapshot の JSON ではない)")
        return 1

    st = part(d, "status")
    pst = part(prev, "status") if prev else {}
    window, restarted = "", False
    if prev:
        dt = (d.get("taken_at") or 0) - (prev.get("taken_at") or 0)
        restarted = restart_between(prev, d, dt)
        window = (f" (前回からの窓 {dt / 3600:.1f} 時間"
                  + ("、**再起動をまたいでいる**" if restarted else "") + ")")

    def delta(path, default=0):
        """`status` の通算から前回を引く (`--prev` が無ければそのまま)。

        **再起動をまたいだら引き算しない**。`/status` の通算は再起動で 0 に戻るので、
        前の雪像 (別のプロセス) の値を引くとその分だけ足りなくなる
        (2026-09-16 の 2 枚では要求 3,312 − 11 = 3,301 と、前の版の 11 件を引いていた)。
        `snapshot-diff.py` の §1 と同じく「起動から」の値として読む。
        """
        cur, old = st, pst
        for k in path[:-1]:
            cur, old = cur.get(k, {}) or {}, (old.get(k, {}) or {})
        c = cur.get(path[-1], default)
        if not prev or restarted:
            return c
        o = old.get(path[-1], default)
        if isinstance(c, (int, float)) and isinstance(o, (int, float)) and c >= o:
            return c - o
        return c

    print(f"- 版 `{d.get('version')}` / 起動から {(st.get('uptime_secs') or 0) / 3600:.1f} 時間"
          f" / 取得 {stamp(d.get('taken_at'))} (`{d.get('taken_at')}`){window}")
    if d.get("dropped"):
        print(f"- **4 MiB を越えたので落とした部分**: {', '.join(d['dropped'])}")
    cut = truncated_parts(d)
    if cut:
        print("- **応答が 256 KiB で切れている部**: "
              + "、".join(f"`{name}` ({num(shown)}"
                         + (f"/{num(count)}" if count and count != shown else "") + " 件)"
                         for name, shown, count in cut)
              + " (top-level の `dropped` は**部ごと**落としたもの。部の中の打ち切りは"
                "その部の `truncated` を見る)")
    print()

    reqs = delta(["total_requests"])
    # 通算を引き算した窓は「取得から取得まで」、引かないなら「起動から」
    secs = ((d.get("taken_at", 0) - prev.get("taken_at", 0)) if (prev and not restarted)
            else (st.get("uptime_secs") or 0))
    if restarted:
        print("**再起動をまたいでいるので、`/status` の通算 (要求・転送・名前解決・上限で断った) は"
              "「起動から」の値** (引き算すると前の版のぶんを引いてしまう)。")
        print()
    print("| 全体 | 値 |")
    print("|---|---|")
    print(f"| 要求 | {num(reqs)} ({reqs / secs:.3f} /s) |" if secs else f"| 要求 | {num(reqs)} |")
    print(f"| 転送 | {fmt_bytes(delta(['bytes_forwarded']))} |")
    print(f"| いまの接続 | {num(st.get('active_connections'))} / 上限 {num(st.get('max_conns'))}"
          f" (預かり所 {num(st.get('parked_connections'))}、うちトンネル {num(st.get('parked_tunnels'))}) |")
    print(f"| スレッド / fd | {num(st.get('live_threads'))} / {num(st.get('max_threads'))}"
          f" ・ {num(st.get('fds'))} / {num(st.get('max_fds'))} |")
    print(f"| 上限で断った / 追い出した | {num(delta(['rejected_overload']))}"
          f" / {num(delta(['evicted_idle']))} |")
    cache = st.get("cache") or {}
    sysinfo = cache.get("system") or {}
    print(f"| キャッシュ | 命中率 {num(cache.get('hit_ratio'))} "
          f"(使用 {fmt_bytes((cache.get('memory') or {}).get('used_bytes'))}) |")
    rss, rss_note = sysinfo.get("process_rss_bytes"), ""
    if before:
        early = ((before.get("cache") or {}).get("system") or {}).get("process_rss_bytes")
        if early is not None:
            rss, rss_note = early, " (**RSS は雪像の前に取った `/status` の値**)"
    print(f"| RSS | {fmt_bytes(rss)}{rss_note} |")
    print()

    # CPU (割り当てに対する使用率・絞られた周期の割合・上位スレッド・走れずに待った時間)。
    # **どちらの源も無い版では表ごと出さない** (T15.0 (15))
    prof = profile_totals(d)
    cg = (st.get("kernel") or {}).get("cgroup_cpu") or {}
    if prof or cg:
        print("| CPU | 値 |")
        print("|---|---|")
        if prof:
            used = cores(prof["cpu_us"], prof["secs"])
            quota = cg.get("quota_cores")
            share = (f" / 割り当て {num(quota)} コア の **{used / quota * 100:.1f}%**"
                     if quota and used is not None else " (cgroup の割り当ては無い)")
            # **コアは 3 桁**。1 桁だと 0.006 コアも 0.04 コアも「0.0」に潰れる
            print(f"| 使用 | {'—' if used is None else f'{used:.3f}'} コア{share}"
                  f" (`/profile` {prof['samples']} 標本 × {prof['interval_secs']} 秒) |")
        split = user_kernel(prof, cg)
        if split:
            print(f"| ユーザー / カーネル | {split} |")
        periods = cg.get("nr_periods")
        if periods:
            since = cg.get("since_start") or {}
            sp, sth = since.get("nr_periods"), since.get("nr_throttled")
            extra = (f"、起動から {sth / sp * 100:.1f}% ({num(sth)}/{num(sp)})"
                     if sp else "")
            print(f"| 絞られた周期 | **{cg.get('nr_throttled', 0) / periods * 100:.1f}%** "
                  f"({num(cg.get('nr_throttled'))}/{num(periods)}){extra} |")
        elif cg:
            print(f"| 絞られた周期 | 絞られ {num(cg.get('nr_throttled'))} 回 "
                  "(**この版に分母 `nr_periods` が無い**ので割合は出せない) |")
        if prof and prof["top"]:
            roles = prof["roles"]
            shown = "、".join(
                f"`{t['comm']}` (tid {t['tid']}"
                + (f"、{roles[t['role']]}" if 0 <= t["role"] < len(roles) else "")
                + f"、{t['cpu_us'] / 1000:,.0f} ms、走行 {num(t['running'])} 標本)"
                for t in prof["top"][:3])
            print(f"| 上位スレッド | {shown} |")
        if prof and prof["run_delay_us"]:
            pairs = sorted(zip(prof["roles"], prof["run_delay_us"]), key=lambda kv: -kv[1])
            shown = "、".join(f"{r} {us / 1000:,.1f} ms" for r, us in pairs[:3] if us) or "—"
            print(f"| 走れずに待った (`run_delay_us`) | {shown} |")
        print()

    dns = st.get("dns") or {}
    pdns = (pst.get("dns") or {}) if (prev and not restarted) else {}
    misses = dns.get("misses", 0) - pdns.get("misses", 0)
    ms_sum = dns.get("miss_ms_sum", 0.0) - pdns.get("miss_ms_sum", 0.0)
    print("| 名前解決 | 値 |")
    print("|---|---|")
    # 完了の定義の閾が 0.15 なので、2 桁だと 0.146 が「0.15」に丸まって判定を読み違える
    print(f"| ミス | {num(misses)} ({misses / reqs:.3f} /要求) |" if reqs
          else f"| ミス | {num(misses)} |")
    print(f"| ミス 1 回 | {ms_sum / misses:.1f} ms |" if misses else "| ミス 1 回 | — |")
    print(f"| 表 / warm / 引き直し | {num(dns.get('entries'))} / {num(dns.get('warm'))}"
          f" / {num(dns.get('refreshes', 0) - pdns.get('refreshes', 0))} |")
    print(f"| 負のキャッシュ命中 / 古い答えで代用 | {num(dns.get('negative_hits'))}"
          f" / {num(dns.get('stale_served'))} |")
    # ミスの種類別と引き直しの様子 (T15.0 (7))。**どちらも起動からの通算**なので
    # `--prev` でも引き算しない (`expired` が主なら窓、`warm_stale` なら引き直しの詰まり)
    kinds = dns.get("misses_by_kind")
    if kinds:
        print("| ミスの種類別 (起動から) | "
              + " / ".join(f"{k} {num(kinds.get(k, 0))}" for k in MISS_KINDS) + " |")
    if any(k in dns for k in ("refresh_failures", "refresh_late", "refresh_ms_max")):
        print(f"| 引き直しの失敗 / 遅れ / 最大 (起動から) | "
              f"{num(dns.get('refresh_failures'))} 回 / {num(dns.get('refresh_late'))} 回"
              f" / {num(dns.get('refresh_ms_max'))} ms |")
    print()

    hist = (d.get("history") or {})
    print("| 待ち (履歴の全標本) | 本数 | 平均 | p50 / p95 | 最大 |")
    print("|---|---|---|---|---|")
    # `wait` は**利用者が待つ時間** (`queue + client_read + dns + connect`。T15.0 (2))。
    # `connect` は要求行を読んだ後からしか測らないので、次の完了の定義の基準線はこちら
    for label, prefix, res, last in (("CONNECT 確立 (直近 1 時間)", "connect", "5", None),
                                     ("CONNECT 確立 (直近 1 日)", "connect", "60", None),
                                     ("CONNECT 確立 (通算)", "connect", "3600", None),
                                     ("利用者が待つ `wait` (直近 1 日)", "wait", "60", None),
                                     ("forward 初バイト (直近 1 時間)", "forward", "5", None),
                                     ("forward 初バイト (通算)", "forward", "3600", None)):
        print(latency_line(label, hist.get(res) or {}, prefix, last))
    print()

    # エラーの合計は `/status` に無いので、`/hosts` (全ホスト) から出す。
    # **表ごとの合計は引き算しない** (切れた表どうしだと負になる。上の `error_delta` を見る)
    errors = part(d, "errors").get("errors") or []
    ed = error_delta(d, prev)
    where = "`/hosts` をホストの鍵で突き合わせた差分" if ed["windowed"] else "`/hosts` の合計"
    if ed["back"]:
        print(f"- エラー: **引き算できない** ({ed['back']} ホストで `/hosts` の通算が減っている"
              f" — `.rrd` が作り直された疑い。`scripts/snapshot-diff.py` の §7 を見ること)"
              f" / 個票 {len(errors)} 件")
    else:
        shown = " ".join(f"{CAUSE_NAMES[i]} {n}" for i, n in enumerate(ed["causes"]) if n) or "—"
        print(f"- エラー: {num(ed['total'])} 件 ({where}) / 原因 {shown}"
              f" / 個票 {len(errors)} 件")
    if ed["windowed"] and ed["gone"]:
        print(f"  - 後の `/hosts` から消えたホスト {ed['gone']} 件 (通算のエラー"
              f" {ed['gone_errors']} 件) は差分から外した")
    if ed["truncated"]:
        pairs = "、".join(f"{num(s)}/{num(c)} 件" for s, c in zip(ed["shown"], ed["count"]))
        print(f"  - `/hosts` は **256 KiB で切れている** ({pairs}) ので、"
              "下位のホストは前後で出入りする (合計どうしを引くと負になる)")
    for e in errors[:5]:
        print(f"  - `{stamp(e.get('at'))}` {e.get('kind')} {e.get('target')} → {e.get('status')}"
              f" ({e.get('cause')}、dns {e.get('dns_ms')} ms / connect {e.get('connect_ms')} ms、"
              f"from {e.get('client')})")
    print()

    rec = part(d, "recent")
    rows = rec.get("recent") or []
    print(f"- 閉じた接続 (`/recent`): 通算 {num(rec.get('recorded'))} 本 / 覚えている {len(rows)} 本"
          + ("" if not rec.get("truncated") else " (**応答は 256 KiB で切れている**)"))
    if rows:
        # **そろっている窓は「いちばん早く閉じた接続」から取得まで** (`at` の最小からではない)。
        # 応答が切れているとき残っているのは「最後に閉じた N 本」なので、`at` の最小で窓を
        # 切ると、その窓の中に**開始して窓の中で閉じなかった**接続が抜けたまま「全部」に見える。
        ends = [r.get("at", 0) + r.get("secs", 0) for r in rows if r.get("at")]
        if ends:
            print(f"  - そろっている窓: {stamp(min(ends))} → {stamp(d.get('taken_at'))}"
                  " (`min(at + secs)` から取得まで)"
                  + ("。**これより前に閉じた接続は応答から落ちている** "
                     "(`/recent?since=` を付けて取り直せば切れない)"
                     if rec.get("truncated") else ""))
        print(f"  - 閉じた理由: " + ", ".join(f"{k} {v}" for k, v in tally(rows, "reason").items()))
        print(f"  - 種類: " + ", ".join(f"{k} {v}" for k, v in tally(rows, "kind").items()))
        secs_all = [r.get("secs", 0) for r in rows]
        print(f"  - 寿命: 中央値 {statistics.median(secs_all):.0f} 秒 / 最大 {max(secs_all):,} 秒")
        up = sum(r.get("up", 0) for r in rows)
        down = sum(r.get("down", 0) for r in rows)
        print(f"  - バイト: 上り {fmt_bytes(up)} / 下り {fmt_bytes(down)}")
        top_clients = tally(rows, "client")
        print("  - 接続元: " + ", ".join(f"{k} {v}" for k, v in list(top_clients.items())[:5]))
        slow = sorted(rows, key=lambda r: -(r.get("ms", {}).get("connect", 0)))[:5]
        print("  - 確立の遅かった 5 本:")
        for r in slow:
            ms = r.get("ms", {})
            print(f"    - {r.get('target')} connect {ms.get('connect', 0)} ms"
                  f" / dns {ms.get('dns', 0)} ms ({r.get('reason')}、{r.get('secs')} 秒、"
                  f"{fmt_bytes(r.get('up', 0) + r.get('down', 0))})")
    print()

    conns = part(d, "connections").get("connections") or []
    print(f"- いまの接続 (`/connections`): {len(conns)} 本"
          + ((" / " + ", ".join(f"{k} {v}" for k, v in tally(conns, "state").items())) if conns else ""))
    # 動かないトンネル (T15.0 (4) の証拠つき)。**閾は固定の 300 秒**なので見出しに書く
    stuck = idle_tunnels(conns)
    if stuck:
        spins = sum(c.get("spins") or 0 for c in stuck)
        half = sum(1 for c in stuck if c.get("half_closed"))
        print(f"  - **動かないトンネル (`idle_secs` ≥ {IDLE_TUNNEL_SECS} 秒)**: {len(stuck)} 本"
              f" (半閉じ {half} 本、`spins` の合計 {num(spins)})")
        for c in stuck[:5]:
            print(f"    - `{c.get('target')}` {conn_evidence(c)}")
    log = part(d, "log").get("lines") or []
    print(f"- 警告と失敗 (`/log`): {len(log)} 行"
          + ((" / 直近 `" + (log[0].get("msg") or "")[:120] + "`") if log else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
