# 判定表 (`scripts/snapshot_criteria.py`) の **T36 の行** (T36.1)。
#
# T35 の 26 行のうち、heap の行 (9 番目) を、キャッシュの持ち物に加えて**名前解決の表の持ち物**も引いた残りで
# 読む行 (T36.1)。並べるのは `RULES` (`criteria/rules.py`)。**T35 以前の行と文面は 1 つも動かさない**。

from .common import MET, MISSED, UNKNOWN, n, part, stamp
from .phase14_18 import mb, memory_of, uptime_text
from .phase23_25 import _p24_heap
from .t35 import ALLOC_KEY, cache_owned_range


# --- T36.1: heap の行から名前解決の表の持ち物も引く (9 番目の行) ---
#
# T35 の heap の行 (`_t35_heap`) は、0.35.12 の締め (T35.99) でキャッシュがほぼ空なのに +5.7 MB の「届かず」だった。
# 2026-11-12 のスナップショットで見直すと、漏れではなく**上限で止まる増え**だった: 同じ起動の 41.3 時間で +5.8 MB、
# 101.6 時間で +6.6 MB と止まりかけ、増えは名前解決の表の行数 (`/status` の `dns.entries`。上限 4,096 名前。
# `crates/net-dns` の `MAX_ENTRIES`) に沿っていた。表は起動 60 秒の基準のあとで埋まるので、その持ち物も
# キャッシュと同じく増えから引く (本体は変えない。利用者の判断どおり物差しの側で読む)。
#
# 1 名前あたりの大きさは、実バイナリ (0.35.12 の release。名前解決の表は 0.35.14 と同じ) で表を埋めて `/status` の
# `memory` を前後で読んで測った (T36.1): 127.0.0.1 の空いた番号で起こし (`PROXY_CANARY=off`)、先に IP の宛先への
# CONNECT 3,000 本で環 (`/recent`・`/errors`) を埋めてから、名前の違う CONNECT を 1,024 本ずつ 4,096 名前まで流した。
# 名前は tokyo の `/dns` の一覧に似せて 17 文字、3 本に 2 本は答えのある名前 (`getaddrinfo` を差し替えて 2 番地を返す)、
# 1 本は答えの無い名前 (`.invalid`)。`heap_used + mmap − cache_memory − rings_used.total` の増えは 0 → 4,096 名前で
# 4.38 MB = **1 名前あたり 1,069 B** (答えのある名前だけなら 1,060 B、答えの無い名前だけなら 1,137 B)。
# 表の索引 (`HashMap`) が倍々で育つので、途中の 1,024 本ごとの傾きは 77〜2,400 B と揺れる。上限の 4,096 名前で
# 測ったので、tokyo (いつも上限) の読みには合う。上限を越えて 2,048 名前を足しても増えは 1 名前あたり 86 B だった。
# 基準 (起動 60 秒) の時点で表にあった名前の数はスナップショットに無いので、**いまの `dns.entries` を全部引く**
# (基準の時点の名前のぶん、多めに引く側に倒れる。tokyo の起動 60 秒のスナップショットでは 4〜14 名前 = 15 KB 以下)。

# 名前解決の表の 1 名前あたりの持ち物 (バイト。上の計測の 1,069 B を丸めた)
DNS_ENTRY_BYTES = 1070
# `dns.entries` の読めない版で、表の持ち物の上限に使う名前の数 (`crates/net-dns` の `MAX_ENTRIES`)
DNS_MAX_ENTRIES = 4096


def dns_owned_range(st):
    """`/status` の部 `st` から、名前解決の表の持ち物の `(下限, 上限, 説明)` (バイト)。

    `dns.entries` があれば下限 = 上限 = 名前の数 × `DNS_ENTRY_BYTES`。無ければ 0〜上限 4,096 名前ぶんの幅。
    """
    dns = st.get("dns")
    entries = dns.get("entries") if isinstance(dns, dict) else None
    if isinstance(entries, (int, float)):
        v = entries * DNS_ENTRY_BYTES
        return (v, v, f"名前解決の表 {n(entries)} 名前 × {n(DNS_ENTRY_BYTES)} B = {mb(v)} MB")
    top = DNS_MAX_ENTRIES * DNS_ENTRY_BYTES
    return (0, top, f"名前解決の表は `dns.entries` が無いので 0〜{mb(top)} MB (上限 {n(DNS_MAX_ENTRIES)} 名前ぶん)")


def _t36_heap(c, th):
    """T36.1: (g) を、`heap_used + mmap − cache_memory` からキャッシュと名前解決の表の持ち物を引いた残りの、起動 60 秒の基準からの増えで読む。

    キャッシュの持ち物は T35 の行と同じ (`cache_owned_range`)。名前解決の表は上限 4,096 名前で止まる増えなので、
    `dns.entries` × 1 名前あたりの大きさ (`DNS_ENTRY_BYTES`) を引く (`dns_owned_range`)。
    `memory.baseline` が無いスナップショットは `_p24_heap` をそのまま返す (基準を混ぜない。T35 と同じ)。
    """
    b = c["b"]
    mem = memory_of(b)
    base = mem.get("baseline")
    if not isinstance(base, dict) or base.get("bytes") is None:
        return _p24_heap(c, th)
    lim = th["heap_growth_mb"]
    label = (f"`heap_used + mmap − cache_memory` から**キャッシュと名前解決の表の持ち物**を引いた残りの、"
             f"起動から 60 秒の基準からの増えが {lim:.0f} MB 未満 (T36.1)")
    limit = f"< {lim:.0f} MB"
    at = base.get("at")
    taken, up = b.get("taken_at"), b.get("uptime_secs")
    start = (taken - up) if (taken and up is not None) else None
    when = (f"{stamp(at)}" + (f" (起動から {n(int(at - start))} 秒)" if (at and start) else ""))
    shown = "、".join(f"`{k}` {mb(mem.get(k))}"
                     for k in ("heap_used", "mmap", "cache_memory", "heap_free", "rss"))
    shown += f" MB (`heap_free` と `rss` は表示だけ)。基準は {mb(base['bytes'])} MB ({when})"
    missing = [f"`{k}`" for k in ("heap_used", "mmap", "cache_memory") if mem.get(k) is None]
    if missing:
        return (label, limit, shown, UNKNOWN, "、".join(missing) + " が無い (`memory.baseline` はある)")
    if at is not None and start is not None and at < start - 5:
        return (label, limit, shown, UNKNOWN,
                f"`memory.baseline` の `at` ({stamp(at)}) がこの起動の始まり "
                f"({stamp(start)}) より前 = 前の起動が残した基準")
    st = part(b, "status")
    owned = cache_owned_range(st)
    if owned is None and mem["cache_memory"]:
        return (label, limit, shown, UNKNOWN, "`/status` に `cache` の部が無い (キャッシュの件数が読めない)")
    c_lo, c_hi, c_what = owned if owned is not None else (0, 0, "`/status` に `cache` の部が無い (`cache_memory` も 0)")
    d_lo, d_hi, d_what = dns_owned_range(st)
    lo, hi = c_lo + d_lo, c_hi + d_hi
    now = mem["heap_used"] + mem["mmap"] - mem["cache_memory"]
    grown = (now - base["bytes"]) / 1e6
    rest_hi, rest_lo = grown - lo / 1e6, grown - hi / 1e6

    def span(a, z):
        return mb(a) if a == z else f"{mb(a)}〜{mb(z)}"
    src = (f"後のスナップショット (起動から {uptime_text(up)}) の `/status` の `memory.baseline` (起動 60 秒の "
           f"`heap_used + mmap − cache_memory`) との差から、`/status` の `cache` で見積もったキャッシュの持ち物と、"
           f"`dns.entries` で見積もった名前解決の表の持ち物 (1 名前 {n(DNS_ENTRY_BYTES)} B。T36.1 で実バイナリの表を"
           f"埋めて測った。基準の時点の名前も引くので多めに引く側) を引いた: {c_what}。{d_what}")
    parts = f"増え {grown:+.1f} MB − キャッシュ {span(c_lo, c_hi)} MB − 名前解決の表 {span(d_lo, d_hi)} MB"
    if lo == hi:
        return (label, limit, f"残り **{rest_hi:+.1f}** MB ({parts})。" + shown,
                MET if rest_hi < lim else MISSED, src)
    text = f"残り **{rest_lo:+.1f}〜{rest_hi:+.1f}** MB ({parts})。" + shown
    if rest_hi < lim:
        return (label, limit, text, MET, src)
    if rest_lo >= lim:
        return (label, limit, text, MISSED, src)
    return (label, limit, text, UNKNOWN,
            src + f"。**残りの幅が閾 {lim:.0f} MB をまたぐ** (長さの分からない本文の確保の端数か、名前解決の表の"
            f"名前の数がスナップショットに無い。本体が `cache.memory.{ALLOC_KEY}` と `dns.entries` を出せば 1 つに決まる)")
