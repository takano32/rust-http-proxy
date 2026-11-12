# 判定表 (`scripts/snapshot_criteria.py`) の **T36 の行** (T36.1・T36.2・T36.3)。
#
# T35 の 26 行のうち、heap の行 (9 番目) を、キャッシュの持ち物に加えて**名前解決の表の持ち物**も引いた残りで
# 読む行 (T36.1) と、前の期間が 0 件だと読めなかった `dns_slow` (11 番目) と接続のエラー (13 番目) を下限つきで
# 読む行 (T36.3) と、CPU の 2 行 (5・25 番目) を**機械の速さの物差しで割ってから**比べる行と、物差しそのものの
# 前後を出す表示だけの行 (27 番目。T36.2)。並べるのは `RULES` (`criteria/rules.py`)。
# **T35 以前の行と文面は 1 つも動かさない**。

from .common import MET, MISSED, SHOWN, UNKNOWN, n, part, stamp
from .phase14_18 import mb, memory_of, uptime_text
from .phase23_25 import _p24_heap
from .phase29_30 import (
    CONN_PER_RESOLVED_SRC, IDLE_COST_SRC, _p29_connect_errors, _p29_profile_sides, _p30_dns_slow,
    conn_per_resolved_sides, dns_slow_windows, history_since_start, idle_cost_sides, idle_role_shares,
)
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


# --- T36.3: 前の期間が 0 件のときの「前の何倍未満」を、下限つきで読む (11・13 番目の行) ---
#
# 前の版と比べる行は、前が 0 件だと読めない。T34.999 の 2 枚 (0.33.3 → 0.34.165) では:
# - 13 番目 (接続のエラー ÷ 試行) は前が 0 / 9,981 で、後の 13 / 45,577 (0.00029) が 1 件でも「届かず」になった
#   (`_p29_connect_errors` は前が 0 なら後も 0 のときだけ満たす)
# - 11 番目 (`dns_slow` が立った窓の成功したミスの平均) は、前の版に立った窓が無く、比べる平均が無いので「判定できず」
# どちらも、前の何倍かではなく**絶対の値**で読めば判定できる。T36 では、率の行は後が下限未満なら満たしたとし、
# `dns_slow` は前に立った窓が無ければ後の平均を絶対値の閾で読む。T35 以前の行は変えない。

# 接続のエラーの率の下限 (後がこれ未満なら、前の何倍かに依らず満たした。TASKS.md の T36.3 の「たとえば 0.001」)。
# tokyo の締めの後の値は 0.00010〜0.00029 (T34.999・T35.99)
CONNECT_ERROR_FLOOR = 0.001
# 前の版に立った窓が無いときの、`dns_slow` が立った窓の成功したミスの平均の閾 (ms)。tokyo の `dns_fail_ms_sum` の
# ある版のスナップショット (0.32.39〜0.35.14、2026-11-08 まで) で、立った窓の平均は 144〜334 ms だった。その最大の
# 1.5 倍 (= 前の版と比べる行の倍率) を丸めた
DNS_SLOW_AVG_FLOOR_MS = 500
# `_p30_dns_slow` が前の版に立った窓が無いときに出どころの頭に付ける文 (T36 ではこれを外して書き直す)
NO_BEFORE = "前の版の `/history?res=60` の窓で立った窓に成功したミスが無い (比べる平均が無い)。"


def resolved_error_rate(snap):
    """起動からの `/history?res=3600` の、443・80 宛ての接続のエラー (名前解決の失敗を除く) ÷ 試行。

    数え方は `_p29_connect_errors` と同じ。欄が無いか試行が 0 なら None。
    """
    keys = ("errors", "errors_by_cause", "connects", "other_port_errors", "other_port_attempts")
    v = history_since_start(snap, 3600, keys)
    if v is None:
        return None
    causes = v["_causes"]
    dns = v["errors_by_cause"][causes.index("dns")] if "dns" in causes else 0
    errs = v["errors"] - dns
    num, den = errs - v["other_port_errors"], v["connects"] + errs - v["other_port_attempts"]
    return (num / den) if den else None


def _t36_connect_errors(c, th):
    """T36.3: 接続のエラー (443・80) の行を、前の版の 1.5 倍未満**か、後が下限 0.001 未満**で読む。

    実測と出どころは `_p29_connect_errors` と同じ。前が 0 件だと後が 1 件でも「届かず」になっていたので、
    後の率が `CONNECT_ERROR_FLOOR` 未満なら満たしたとする (「届かず」だけを読み替える。判定できずは変えない)。
    """
    label, limit, shown, verdict, src = _p29_connect_errors(c, th)
    floor = CONNECT_ERROR_FLOOR
    label = label.replace("倍未満 (", f"倍未満か、後が {floor} 未満 (T36.3。", 1)
    limit = f"{limit} か < {floor}"
    if verdict == MISSED:
        rate = resolved_error_rate(c["b"])
        if rate is not None and rate < floor:
            return (label, limit, shown, MET,
                    src + f"。後の {rate:.5f} が下限 {floor} 未満なので、前の何倍かに依らず満たした (T36.3)")
    return (label, limit, shown, verdict, src)


def _t36_dns_slow(c, th):
    """T36.3: `dns_slow` の行を、前の版に立った窓が無ければ**後の平均の絶対値** (500 ms 未満) で読む。

    実測と出どころは `_p30_dns_slow` と同じ。前の版の窓に成功したミスで立った窓が無いと比べる平均が無く
    「判定できず」になっていたので、そのときだけ後の平均を `DNS_SLOW_AVG_FLOOR_MS` と比べる。
    """
    label, limit, shown, verdict, src = _p30_dns_slow(c, th)
    floor = DNS_SLOW_AVG_FLOOR_MS
    label = label.replace("(T.33.1。", f"(前の版に立った窓が無ければ {floor} ms 未満。T36.3。", 1)
    limit = f"{limit}。前に無ければ < {floor} ms"
    if verdict != UNKNOWN:
        return (label, limit, shown, verdict, src)
    aft, bef = dns_slow_windows(c["b"], th), dns_slow_windows(c["a"], th)
    if (aft is None or not aft["exact"] or not aft["hours"] or not aft["fired"] or not aft["misses"]
            or bef is None or bef["misses"]):
        return (label, limit, shown, verdict, src)
    a = aft["lo"] / aft["misses"]
    return (label, limit, shown, MET if a < floor else MISSED,
            f"前の版に成功したミスで立った窓が無いので、後の平均 {a:,.0f} ms を絶対値の閾 {floor} ms と比べた "
            f"(T36.3。tokyo の 2026-11-08 までのスナップショットの平均 144〜334 ms の最大の 1.5 倍)。" + src.replace(NO_BEFORE, "", 1))


# --- T36.2: CPU の 2 行を、機械の速さの物差しで割ってから比べる (5・25 番目の行と、表示だけの 27 番目の行) ---
#
# T36.0 で、毎秒決まった仕事しかしない役割の CPU まで 0.34.165 → 0.35.12 で 1.4〜1.7 倍になっていた。機械の側で
# 同じ仕事の値段が変わると、CPU の 2 行は、コードが同じでも届かずになる。そこで前と後のそれぞれを、同じ期間の
# **決まった仕事の値段**で割ってから比べる。
#
# 物差しは **`history` 役の CPU ÷ その役の標本の数** (us/秒。標本は `profile-sample` が毎秒 1 回取るので、
# 標本の数 = スレッドが生きていた秒)。`history` は 5 秒に 1 回、窓を閉じて標本を積む決まった仕事をする
# (`crates/metrics/src/tick.rs`)。もう 1 つの候補だった `profile-sample` 役の CPU ÷ 読んだスレッドの標本の数
# (全役割の `threads` の標本の和) は、暇な秒には全スレッドを読まずに前の状態を積む (T32.37) ので、1 標本の値段が
# 要求の量で動く。tokyo の 3 枚 (0.34.165 の締め・0.35.12 の締め・2026-11-12) の `/profile` を 1 時間ずつ割って
# 比べると、時間ごとの揺れ (変動係数) は `history` が 0.086・0.018・0.029、`profile-sample` が 0.180・0.053・0.050、
# 時間ごとの要求の数との相関は `history` が +0.65・+0.30・+0.50、`profile-sample` が +0.81・+0.84・+0.72 だった。
# `profile-sample` の値は表示だけの行に参考で並べる。

# 物差しに使う役割 (毎回決まった仕事をする裏の役割)
MACHINE_ROLE = "history"
# 参考に並べる役割 (毎秒、生きているスレッドの数だけ `/proc` を読む)
SAMPLER_ROLE = "profile-sample"


def machine_cost(p):
    """`/profile` の JSON 1 つから、機械の速さの物差しを読む (T36.2)。

    返すのは `{"us": history の us/秒, "cpu": history の cpu_us の和, "samples": history の標本の和,
    "sampler": profile-sample の us ÷ 全役割の標本 (読めなければ None)}`。`history` の役割か標本が無ければ None。
    役割ごとの `threads` は `[cpu_us, 標本の数, [状態ごとの数…]]` (標本 0 の役割は `0`)。
    """
    keys = p.get("keys") or []
    roles = p.get("roles") or []
    rows = p.get("samples") or []
    if "threads" not in keys or MACHINE_ROLE not in roles:
        return None
    ti = keys.index("threads")
    cpu = [0] * len(roles)
    samples = [0] * len(roles)
    for r in rows:
        threads = r[ti] if ti < len(r) else None
        for i in range(len(roles)):
            t = threads[i] if isinstance(threads, list) and i < len(threads) else None
            if isinstance(t, list) and len(t) >= 2:
                cpu[i] += t[0] or 0
                samples[i] += t[1] or 0
    hi = roles.index(MACHINE_ROLE)
    if not samples[hi]:
        return None
    total = sum(samples)
    si = roles.index(SAMPLER_ROLE) if SAMPLER_ROLE in roles else None
    sampler = cpu[si] / total if si is not None and total else None
    return {"us": cpu[hi] / samples[hi], "cpu": cpu[hi], "samples": samples[hi], "sampler": sampler}


def machine_sides(c, th):
    """前と後の物差し (`{誰: machine_cost}`, None) か (None, 判定できない理由)。材料は CPU の 2 行と同じ `/profile`。"""
    sides, why = _p29_profile_sides(c, th)
    if sides is None:
        return None, why
    got = {}
    for who, _, p, _ in sides:
        m = machine_cost(p)
        if m is None or not m["us"]:
            return None, f"{who}の `/profile` に `{MACHINE_ROLE}` 役の CPU か標本が無い (機械の速さの物差しが読めない)"
        got[who] = m
    return got, None


def machine_text(m):
    """物差しの前後の比の文 (出どころの欄の末尾に付ける)。"""
    b, a = m["前"]["us"], m["後"]["us"]
    return (f"機械の速さの物差し (`{MACHINE_ROLE}` 役の CPU ÷ その役の標本の数) は前 {b:,.1f} → 後 {a:,.1f} us/秒 "
            f"({a / b:.2f} 倍)。後の値に前 ÷ 後の物差しを掛けて、前の機械の値段に直してから比べた (T36.2)")


def _t36_conn_per_resolved_request(c, th):
    """T36.2: `conn` 役の CPU ÷ 名前解決に成功した要求を、**機械の速さの物差しで割ってから**前の版と比べる。

    材料は phase29 の行 (`conn_per_resolved_sides`) と同じ。後の値に `前の物差し ÷ 後の物差し` を掛けて
    前の機械の値段に直し、その比が `conn_per_resolved_factor` 倍未満なら満たした。割る前の値は表示だけ。
    """
    k = th["conn_per_resolved_factor"]
    label = (f"`conn` 役の CPU ÷ **名前解決に成功した要求**を**機械の速さの物差しで割って**、前の版の {k} 倍未満 "
             f"(T36.2。物差しは `{MACHINE_ROLE}` 役の us/秒。割る前の値と要求の全数あたりは表示だけ)")
    limit = f"< 前 ×{k} (物差しで割って)"
    got, srcs, why = conn_per_resolved_sides(c, th)
    if got is None:
        return (label, limit, "—", UNKNOWN, why)
    m, why = machine_sides(c, th)
    if m is None:
        return (label, limit, "—", UNKNOWN, why)
    a, b = got["後"], got["前"]
    if not b["us"]:
        return (label, limit, "—", UNKNOWN, "。".join(srcs) + "。前の `conn` 役の CPU が 0")
    scale = m["前"]["us"] / m["後"]["us"]
    fixed = a["us"] * scale
    r, raw = fixed / b["us"], a["us"] / b["us"]
    shown = (f"**{b['us']:,.0f} → {fixed:,.0f}** us/名前解決に成功した要求 (前の機械の値段に直した。**{r:.2f}** 倍)。"
             f"表示だけ: 割る前 {b['us']:,.0f} → {a['us']:,.0f} us ({raw:.2f} 倍)、"
             f"要求の全数あたり {b['all']:,.0f} → {a['all']:,.0f} us")
    return (label, limit, shown, MET if r < k else MISSED,
            CONN_PER_RESOLVED_SRC + "。".join(srcs) + "。" + machine_text(m))


def _t36_idle_cost(c, th):
    """T36.2: 暇なときの費用 (裏の役割の us/秒) を、**機械の速さの物差しで割ってから**前の版と比べる。

    材料は phase29 の行 (`idle_cost_sides`) と同じ。後の値を前の機械の値段に直して、その比が `idle_cost_factor`
    倍未満なら満たした。割る前の値と `profile-sample`・`cache-probe` の割合は表示だけ。
    """
    k = th["idle_cost_factor"]
    label = (f"暇なときの費用: 裏の役割 (`conn`・`accept` 以外) の CPU を**機械の速さの物差しで割って**、前の版の {k} 倍未満 "
             f"(us/秒。T36.2。物差しは `{MACHINE_ROLE}` 役の us/秒。割る前の値と `profile-sample`・`cache-probe` の割合は表示だけ)")
    limit = f"< 前 ×{k} (物差しで割って)"
    got, shares, srcs, why = idle_cost_sides(c, th)
    if got is None:
        return (label, limit, "—", UNKNOWN, why)
    m, why = machine_sides(c, th)
    if m is None:
        return (label, limit, "—", UNKNOWN, why)
    a, b = got["後"], got["前"]
    if not b:
        return (label, limit, "—", UNKNOWN, "。".join(srcs) + "。前の裏の役割の CPU が 0")
    fixed = a * m["前"]["us"] / m["後"]["us"]
    r = fixed / b
    shown = (f"**{b:,.0f} → {fixed:,.0f}** us/秒 (前の機械の値段に直した。**{r:.2f}** 倍)。"
             f"表示だけ: 割る前 {b:,.0f} → {a:,.0f} us/秒 ({a / b:.2f} 倍)")
    parts = idle_role_shares(th, shares)
    if parts:
        shown += "・" + "・".join(parts)
    return (label, limit, shown, MET if r < k else MISSED, IDLE_COST_SRC + "。".join(srcs) + "。" + machine_text(m))


def _t36_machine_speed(c, th):
    """T36.2 (表示だけ): 機械の速さの物差し (`history` 役の CPU ÷ その役の標本の数) の前後の比。

    5 番目と 25 番目の行は、これで前と後を割ってから比べる。参考に `profile-sample` 役の CPU ÷ 読んだスレッドの
    標本の数 (全役割の標本の和) も並べる (暇な秒を読み飛ばすので要求の量で動き、物差しには使わない)。
    """
    label = (f"機械の速さの物差し: `{MACHINE_ROLE}` 役の CPU ÷ その役の標本の数 (us/秒) の前後の比 "
             "(T36.2。`conn` 役の CPU と暇なときの費用の行は、前と後をこれで割ってから比べる)")
    limit = "—"
    m, why = machine_sides(c, th)
    if m is None:
        return (label, limit, "—", SHOWN, why)
    b, a = m["前"], m["後"]
    shown = f"**{b['us']:,.1f} → {a['us']:,.1f}** us/秒 (**{a['us'] / b['us']:.2f}** 倍)"
    if b["sampler"] and a["sampler"]:
        shown += (f"。参考: `{SAMPLER_ROLE}` 役の CPU ÷ 読んだスレッドの標本 {b['sampler']:,.2f} → {a['sampler']:,.2f} us "
                  f"({a['sampler'] / b['sampler']:.2f} 倍)")
    src = "。".join(f"{who}: `{MACHINE_ROLE}` 役の `cpu_us` {v['cpu']:,.0f} us ÷ 標本 {v['samples']:,.0f}"
                   for who, v in (("前", b), ("後", a)))
    return (label, limit, shown, SHOWN,
            src + f"。**閾を置かない**: 機械の側の値段の変わりを見るための行。`{MACHINE_ROLE}` は 5 秒に 1 回の決まった"
            f"仕事をする。`{SAMPLER_ROLE}` は暇な秒に全スレッドを読まない (T32.37) ので、1 標本の値段が要求の量で動く")
