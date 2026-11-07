# 判定表 (`scripts/snapshot_criteria.py`) の **Phase 31 の行** (T35.9・T35.11)。
#
# phase30 の 26 行のうち、`/events` の anomaly の行 (7 番目) を要約 (`snapshot-summary.py`) と同じ数え方に
# した行 (T35.9) と、heap の行 (9 番目) をキャッシュの持ち物を引いた残りで読む行 (T35.11)。
# 並べるのは `RULES` (`criteria/rules.py`)。**phase30 以前の行と文面は 1 つも動かさない**。

from .common import MET, MISSED, UNKNOWN, anomaly_rates_since_start, n, part, stamp
from .phase14_18 import mb, memory_of, uptime_text
from .phase20_22 import display_only
from .phase23_25 import _p24_heap


# --- T35.9: `/events` の anomaly の件/時を要約と同じ関数で数える (7 番目の行) ---
#
# phase17〜30 の行 (`_p17_events`) は `criteria/phase14_18.py` の `anomaly_rates` で数え、要約の表は
# `snapshot-summary.py` の `anomaly_rates` で数えていた。2 つは起動の時刻の出し方 (`uptime_secs` だけか、
# `status.uptime_secs` を先に見るか)・`:` の無い文面 (外すか `?` に数えるか)・「切れている」の読み方
# (応答の `truncated` だけか、リングが満杯で起動より前の出来事が残っていない時も含めるか) が違い、同じ雪像でも
# 要約の件/時と判定表の行がずれることがあった (T34.5 のレビュー)。利用者の判断 (2026-11-07) で要約の
# 数え方に揃え、T35 からはその 1 つの関数 (`criteria/common.py` の `anomaly_rates_since_start`) を使う。
# 件/時は要約の表と同じく 3 桁 (閾 0.1 件/時の近くで 2 桁に丸めると読み違える)。

def _t35_events_measured(c, th):
    """T35.9: `/events` の anomaly の種類別 件/時 (要約の表と同じ関数・同じ 3 桁)。

    行の形は `_p17_events` と同じ (閾のある種類を先に太字で、前の雪像の件/時を添える)。T35 では
    下の `display_only` で表示だけにする (phase22 から `dns_slow` の判定は別の行に任せている)。
    """
    limits = th["events_per_hour"]
    label = "`/events` の anomaly の種類別 件/時 (" + "、".join(
        f"`{k}` {v} 以下" for k, v in limits.items()) + "。ほかは表示だけ)"
    limit = "、".join(f"`{k}` ≤ {v}" for k, v in limits.items())
    aft, bef = anomaly_rates_since_start(c["b"]), anomaly_rates_since_start(c["a"])
    if aft is None:
        return (label, limit, "—", UNKNOWN, "後の雪像に `/events` の部が無い")
    now = {k: (cnt, rate) for k, cnt, rate in aft["rows"]}
    was = {k: (cnt, rate) for k, cnt, rate in bef["rows"]} if bef is not None else None
    kinds = sorted(set(now) | set(limits), key=lambda k: (k not in limits, -now.get(k, (0, None))[0], k))

    def rate_text(rate):
        return "—" if rate is None else f"{rate:.3f}"
    shown, verdict = [], MET
    for k in kinds:
        cnt, rate = now.get(k, (0, 0.0 if aft["hours"] else None))
        before = ""
        if was is not None:
            before = f"、前 {rate_text(was.get(k, (0, 0.0 if bef['hours'] else None))[1])}"
        star = "**" if k in limits else ""
        shown.append(f"`{k}` {star}{rate_text(rate)}{star} 件/時 ({cnt} 件{before})")
        if k in limits and rate is not None and rate > limits[k]:
            verdict = MISSED
    hours = (f"起動からの {aft['hours']:,.1f} 時間で割った" if aft["hours"]
             else "起動からの時間が分からない (件/時は出さない)")
    src = ("`/events` の `kind == \"anomaly\"` を `text` の先頭で分け (`:` の無い文面は `?`)、`cleared:` を除き、"
           f"起動 (取得 − `status.uptime_secs`) より前の {aft['before']} 件を外し、{hours}"
           " (`snapshot-summary.py` の要約の表と同じ関数。T35.9)")
    if bef is None:
        src += "。前の雪像に `/events` が無い"
    if aft["truncated"]:
        src += "。**`/events` が満杯か切れている** (起動直後のぶんが落ちているかもしれない。件数は下限)"
    return (label, limit, "、".join(shown), verdict, src)


_t35_events = display_only(
    _t35_events_measured,
    lambda th: "`/events` の anomaly の種類別 件/時 (`dns_slow` を含めて表示だけ。要約と同じ数え方)",
    "`dns_slow` の判定は新しい閾の行 (合計 1,000 ms 未満の窓で立った件数) に任せる。T22.2 の模型で tokyo の後は"
    "本当に遅い窓だけで 0.17 件/時あり、件/時の閾は使われ方で落ちる (親の判断 2026-10-17)", was="phase21")


# --- T35.11: heap の行を、キャッシュが実際に持つ分を引いた残りで読む (9 番目の行) ---
#
# phase24〜30 の heap の行 (`_p24_heap`) は `heap_used + mmap − cache_memory` の起動 60 秒の基準からの増えで
# 読む。`cache_memory` はメモリのキャッシュの本文の**長さ** (`Vec::len` の和) と先行確保の合計で、本文の
# **確保の大きさ** (`Vec::capacity`) と索引・鍵は入っていない。0.34.165 の tokyo でキャッシュが初めて使われ
# (メモリ 25 件・`cache_memory` 30.4 MB)、この行は +6.3 MB (閾 5 MB) の「届かず」になった。
#
# 手元で 0.34.165 に本文を保存させて `/status` の `memory` を前後で読んだ結果 (T35.11):
# - `Content-Length` のある本文は `Vec::with_capacity(頭 + 本文)` で受けるので端数は 0。1 件あたり
#   0.5〜0.65 KiB (メモリとディスクの索引・鍵・URL) だけが `cache_memory` の外に出る。ディスクだけの 1 件は 0.3 KiB
# - 長さの分からない (chunked の) 本文は 64 KiB から倍々で育つ `Vec` に受け、縮めずに置くので、確保は
#   64 KiB × 2^k (本文以上の最小)。10 MB の本文で +6.8 MB、3 MB で +1.2 MB、1 KB で +64 KiB が数えられていない
# - mmap の頁の端数 (`PROXY_MALLOC_MMAP_THRESHOLD_KB` 256 以上の本文) は 1 件 4 KiB 未満で、上の 0.5 KiB に紛れる
#
# 雪像には本文ごとの長さも、長さが分かっていたかも無い (`/status` の `cache.memory` は件数と長さの合計だけ)
# ので、端数は**幅**でしか引けない: 1 件の端数は `max(本文, 64 KiB)` 未満なので、合計は 0 以上
# `used_bytes + 件数 × 64 KiB` 未満。残りの上下限が両方とも閾の同じ側なら判定し、またがれば「判定できず」
# (上下限を書く。phase30 の `dns_slow` の行と同じ書き方)。本体が本文の確保の大きさの合計を
# `cache.memory.alloc_bytes` に出すようになれば、端数はその差で 1 つに決まる (下の `ALLOC_KEY`。T35.11 の報告)。

# 本文の確保の大きさの合計を読む欄 (`/status` の `cache.memory` の中。**0.34.165 には無い**)
ALLOC_KEY = "alloc_bytes"
# 長さの分からない本文を受ける `Vec` の最初の確保 (`crates/cache` の `begin_store` の `64 * 1024`)
CHUNKED_FIRST_BYTES = 64 * 1024
# 索引・鍵の 1 件あたりの見積もり (手元の 0.5〜0.65 KiB と 0.3 KiB を切り上げた)。ディスクの件は起動時に
# 読み戻したもの (基準に入っている) も数えるので、多めに引く側に倒れる
INDEX_BYTES_MEMORY = 1024
INDEX_BYTES_DISK = 512


def cache_owned_range(st):
    """`/status` の部 `st` から、キャッシュが `cache_memory` の外に持つ分の `(下限, 上限, 説明)` (バイト)。

    `cache` の部が無ければ None。`cache.memory.alloc_bytes` があれば下限 = 上限。
    """
    cache = st.get("cache")
    if not isinstance(cache, dict):
        return None
    mem = cache.get("memory") if isinstance(cache.get("memory"), dict) else {}
    disk = cache.get("disk") if isinstance(cache.get("disk"), dict) else {}
    m_n, d_n, used = mem.get("entries") or 0, disk.get("entries") or 0, mem.get("used_bytes") or 0
    index = m_n * INDEX_BYTES_MEMORY + d_n * INDEX_BYTES_DISK
    what = (f"メモリ {n(m_n)} 件 (`used_bytes` {mb(used)} MB)・ディスク {n(d_n)} 件、索引の見積もり "
            f"{mb(index)} MB (1 件 {INDEX_BYTES_MEMORY // 1024} KiB・{INDEX_BYTES_DISK / 1024:.1f} KiB)")
    alloc = mem.get(ALLOC_KEY)
    if isinstance(alloc, (int, float)):
        slack = max(alloc - used, 0)
        return (index + slack, index + slack,
                what + f"、本文の確保の端数 {mb(slack)} MB (`{ALLOC_KEY}` {mb(alloc)} − `used_bytes`)")
    top = used + m_n * CHUNKED_FIRST_BYTES
    return (index, index + top,
            what + f"、本文の確保の端数 0〜{mb(top)} MB (`{ALLOC_KEY}` が無いので `used_bytes` + 件数 × 64 KiB まで)")


def _t35_heap(c, th):
    """T35.11: (g) を、`heap_used + mmap − cache_memory` からキャッシュの持ち物を引いた残りの、起動 60 秒の基準からの増えで読む。

    キャッシュがメモリやディスクを使うのは良いことなので、届かずにしない (利用者の判断 2026-11-07)。
    `/status` の `cache_memory` の定義は変えず、物差しの側で、本文の確保の端数と索引の見積もりを引く。
    `memory.baseline` が無い雪像は `_p24_heap` をそのまま返す (基準を混ぜない。phase24 と同じ)。
    """
    b = c["b"]
    mem = memory_of(b)
    base = mem.get("baseline")
    if not isinstance(base, dict) or base.get("bytes") is None:
        return _p24_heap(c, th)
    lim = th["heap_growth_mb"]
    label = (f"`heap_used + mmap − cache_memory` から**キャッシュの持ち物** (本文の確保の端数と索引) を引いた残りの、"
             f"起動から 60 秒の基準からの増えが {lim:.0f} MB 未満 (T35.11)")
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
    owned = cache_owned_range(part(b, "status"))
    if owned is None and mem["cache_memory"]:
        return (label, limit, shown, UNKNOWN, "`/status` に `cache` の部が無い (キャッシュの件数が読めない)")
    lo, hi, what = owned if owned is not None else (0, 0, "`/status` に `cache` の部が無い (`cache_memory` も 0)")
    now = mem["heap_used"] + mem["mmap"] - mem["cache_memory"]
    grown = (now - base["bytes"]) / 1e6
    rest_hi, rest_lo = grown - lo / 1e6, grown - hi / 1e6
    src = (f"後の雪像 (起動から {uptime_text(up)}) の `/status` の `memory.baseline` (起動 60 秒の "
           f"`heap_used + mmap − cache_memory`) との差から、`/status` の `cache` で見積もったキャッシュの持ち物を引いた: "
           + what)
    if lo == hi:
        return (label, limit,
                f"残り **{rest_hi:+.1f}** MB (増え {grown:+.1f} MB − 持ち物 {mb(lo)} MB)。" + shown,
                MET if rest_hi < lim else MISSED, src)
    text = (f"残り **{rest_lo:+.1f}〜{rest_hi:+.1f}** MB (増え {grown:+.1f} MB − 持ち物 {mb(lo)}〜{mb(hi)} MB)。"
            + shown)
    if rest_hi < lim:
        return (label, limit, text, MET, src)
    if rest_lo >= lim:
        return (label, limit, text, MISSED, src)
    return (label, limit, text, UNKNOWN,
            src + f"。**残りの幅が閾 {lim:.0f} MB をまたぐ** (長さの分からない本文の確保の端数が雪像に無い。"
            f"本体が `cache.memory.{ALLOC_KEY}` を出せば 1 つに決まる)")
