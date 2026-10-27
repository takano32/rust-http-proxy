#!/usr/bin/env python3
# 名前解決の表 (`crates/net-dns/src/dns.rs`) を**再生の模型**で回し、規則を変えたときの
# ミスと引き直しの数を見積もる (T.17.5)。
#
# 材料は雪像 (`/snapshot`) 1 枚:
#   - `/recent` の `at` (開いた時刻) と `target` … **全部そろっている区間** (下の「測る区間」) の到着列
#   - `/connections` の `age_secs` … まだ閉じていない接続 (同じ区間に開いたもの)
#   - `/hosts_series` (上位 16 ホスト × 5 分 × 24 時間) … 測る区間の**前の助走** (warm の状態を作る)
#   - `/dns` の名前ごとの `misses` … 助走に出てこない名前が区間の前から表にあったか (下の「前から居た名前」)
#   - `/history?res=60` の `connects` / `dns_misses` … 同じ区間の実機の値 (突き合わせ用)
#
# **測る区間**: `/recent` は閉じた時刻の新しい順に 256 KiB で切れているので、見えている個票の
# 閉じた時刻の最小 (`c0`) より後に開いた接続は全部そろっている。区間は `[c0, taken_at]`。
# それより前は `/hosts_series` の 5 分ごとの件数から到着を作る (窓の中に等間隔に置く)。
# 助走は上位 16 ホストしか持たないので、**ミスと引き直しは区間の中だけで数える**。
#
# **写した規則** (`dns.rs` の `resolve_host` / `warm_promote` / `warm_next` / `refresh_one`):
#   - TTL 60 秒 (`PROXY_DNS_TTL_SECS`)。表の答えが TTL 以内なら当たり、それ以外はミス (同期で引く)
#   - 当たりでも「1 つ前の使用が TTL 以内 (hot)」かつ齢が 3/4 TTL 以上なら裏で 1 回引き直す
#   - 2 回目の使用で warm に上げる (`promote`)。条件は下の `RULES` (**ここだけを差し替える**)
#   - warm の名前は 3/4 TTL ごとに裏で引き直し、最後の使用から窓 (3,600 秒) を過ぎたら外す
#   - warm は同時に `MAX_WARM` 32 件まで。満杯なら最後の使用がいちばん古いものを外す
#   - 裏の引き直しは**一瞬で終わり、失敗しない**とする (実機の `refresh_failures` は 0、1 回 約 18 ms)
#   - IP リテラルは表を通らない
#
# **serve-stale (T.21.3)**: `--serve-stale SECS` を渡すと、いまの規則に「表の答えが TTL を過ぎていても
# 齢 (答えを引いてからの秒) が `SECS` 未満なら古い答えで当たりとし、裏で 1 回引き直す」を足した規則
# (`stale_<SECS>`) も回す (RFC 8767)。古い答えの当たりはミスに数えず (`stale_hits`)、裏の引き直しは
# `refreshes` (うち `refresh_stale`) に数える。複数回渡せる。
#
# **warm の枠と窓 (T.22.4)**: `--grid` で、serve-stale 21,600 秒 (T.21.3 の既定) を基準にして
# 「warm の枠 16 / 32 / 64」「warm の窓 900 秒」「warm をやめる (窓 0 = 引き直しは serve-stale と先回りだけ)」
# 「`warm_promote` の予定を答えの齢から出す」を並べ、`expired` + `warm_stale` のミス (1 日あたりの増え)、
# 引き直しの回/時、名前ごとの引き直しの最大 (回/時) を出す。区間の前から居た名前は、下限側 (表にあるが
# warm でない) と上限側 (区間の始まりで warm) の 2 通りで置く (模型の warm の件数が実機より少ないため)。
# `--series` は `/hosts_series` (上位 16 名前 × 24 時間) だけを到着列にして回す (長い区間での名前ごとの値)。
#
# **「今の規則」(`current`) は keep-warm を止めた模型 (T.29.17)**: `PROXY_DNS_WARM_SECS=0` (T.22.4。warm に上げない
# = 裏の引き直しは先回りと serve-stale だけ) と serve-stale 21,600 秒 (T.21.3) が 0.22.6 からの既定で、tokyo は
# `warm_secs` 0。T.29.16 までの `current` は warm 3,600 秒・serve-stale なしの模型で、10-27 の雪像に当てると引き直し
# 2,273 回/時 (実機 374 回/時) を出し、道具自身が「模型が実機と合わない」と判定していた。その模型は `keep_warm`
# (と `no_idle`) として残す。**名前解決に失敗した接続** (`/recent` の `error:nxdomain` / `error:dns_temp` /
# `error:dns`) は負のキャッシュ (60 秒。T.27.3) の模型で回す (期限内は当たり、それ以外は `negative` のミス、
# 裏の引き直しは無し)。一致は**その雪像の実機の値**と比べる: ミスは同じ区間の `/history?res=60` の
# `dns_misses`、引き直しは `/status` の `dns.refreshes` ÷ 起動からの時間 (どちらも ±30%)
#
# 使い方: scripts/dns-replay.py SNAP.json [--rule current|keep_warm|no_idle|all] [--serve-stale SECS]... [--no-prime]
#         [--grid] [--series] [--json]
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import heapq
import ipaddress
import json
import sys

# `dns.rs` の既定 (`TTL_SECS` / `WARM` / `MAX_WARM`)
TTL = 60
WARM = 3600
MAX_WARM = 32
# 実機の値 (T.16.99: 平常時のミス 0.06 回/接続、裏の引き直し 436 回/時)。模型が合っているかの物差し。
# T.29.17 からは雪像に実機の値 (`/history` のミス・`/status` の引き直し) があればそちらと比べ、無いときだけこれ
TARGET_MISS_PER_CONNECT = 0.06
TARGET_REFRESH_PER_HOUR = 436.0
# 負のキャッシュの TTL (`PROXY_DNS_NEGATIVE_TTL_SECS` の既定。T.27.3)
NEGATIVE_TTL = 60
# 名前解決に失敗した接続の `/recent` の理由 (T25.0 の内訳と、それより前の綴り)
DNS_FAIL_REASONS = ("error:nxdomain", "error:dns_temp", "error:dns")
# 「合う」と見なす幅 (±30%)
TOLERANCE = 0.30
# `hosts_series` の窓の長さの既定 (応答に `window_secs` が無いとき)
SERIES_WINDOW = 300
MISS_KINDS = ("cold", "expired", "warm_stale", "negative")
# serve-stale を採る目安 (T.21.3): `expired` のミスが半分以下、かつ裏の引き直しの増えが +10% 以内
STALE_EXPIRED_MAX = 0.5
STALE_REFRESH_MAX = 0.10
# T.22.4 の基準 (`dns.rs` の `SERVE_STALE`) と目安: 引き直しを基準の半分以下にして、
# `expired` + `warm_stale` のミスの増えが 1 日 10 回以内
SERVE_STALE = 21600
GRID_REFRESH_MAX = 0.5
GRID_MISSES_PER_DAY = 10.0
# 区間の前から居た名前を置く時刻 (区間の始まりの何秒前に最後に使ったとするか)。窓の外 (T.17.5 の下限側)
PRIME_BEFORE = 2 * WARM
# T.22.4 の案 (名前, 基準からの差分)。最初の行が基準 (いまの既定)
GRID = (
    ("base", {}),
    ("warm16", {"max_warm": 16}),
    ("warm64", {"max_warm": 64}),
    ("window900", {"window": 900}),
    ("no_warm", {"window": 0}),
    ("promote_answer", {"promote_at": "answer"}),
)


def refresh_after(ttl):
    """期限前に引き直し始める齢 (`dns.rs` の `refresh_after` = TTL の 3/4)。"""
    return ttl - ttl / 4


def promote_current(e, idle, window):
    """いまの規則 (`dns.rs` の `promote`): 窓の中で 2 回目の使用なら warm に上げる。"""
    return not e["warm"] and window > 0 and idle < window and e["has_addrs"]


def promote_no_idle(e, idle, window):
    """T.17.5 の案: `idle < window` を外す (前に 1 回でも使って答えを持っていれば上げる)。"""
    return not e["warm"] and window > 0 and e["has_addrs"]


RULES = {"current": promote_current, "no_idle": promote_no_idle}

# `run()` が回す規則 (名前 → (promote の規則, `replay` の設定))。`current` は 0.22.6 からの既定
# (keep-warm を止め、serve-stale 21,600 秒。T.29.17)、`keep_warm` と `no_idle` は T.17.5 の模型
# (warm 3,600 秒・serve-stale なし。T.29.16 まで `current` と呼んでいたもの)
PLANS = {
    "current": (promote_current, {"window": 0, "serve_stale": 21600}),
    "keep_warm": (promote_current, {}),
    "no_idle": (promote_no_idle, {}),
}


def host_of(target):
    """`host:port` / `connect://host:port` / `[v6]:port` から名前を取り出す (小文字)。"""
    t = target.split("://", 1)[-1]
    if t.startswith("["):
        return t[1:t.index("]")].lower() if "]" in t else t.lower()
    if t.count(":") == 1:
        t = t.rsplit(":", 1)[0]
    return t.lower()


def is_ip(name):
    try:
        ipaddress.ip_address(name)
        return True
    except ValueError:
        return False


class Table:
    """`dns.rs` の表と warm の待ち行列の写し。時刻は epoch 秒 (float)。"""

    def __init__(self, rule, ttl=TTL, window=WARM, max_warm=MAX_WARM, serve_stale=0,
                 promote_at="now", refresh_at=None):
        self.rule = rule
        # `warm_promote` の最初の予定: "now" = 上げた時刻 + 3/4 TTL (いまの `dns_warm.rs`)、
        # "answer" = 答えを引いた時刻 + 3/4 TTL (既知の小物の直しの案。T.22.4)
        self.promote_at = promote_at
        # 先回りを始める齢 (秒)。`None` = TTL の 3/4 (いまの `refresh_after`)。
        # **k と TTL のどちらが引き直しの間隔を決めているか**を分けて見るための口 (T.24.1)
        self.k = refresh_at if refresh_at else refresh_after(ttl)
        # 古い答えを返してよい齢の上限 (秒。0 = いまの規則 = 期限切れは同期で引く。T.21.3)
        self.serve_stale = serve_stale
        self.ttl = ttl
        self.window = window
        self.max_warm = max_warm
        self.table = {}
        # warm の待ち行列: 名前 → 予定の時刻。`heap` は遅延削除 (予定が変わった古い組は捨てる)
        self.queue = {}
        self.heap = []
        self.counting = True
        # warm の件数 × 時間を積む時計 (最後に積んだ時刻。`None` = まだ積まない)
        self._warm_t = None
        # 名前ごとの引き直しの回数 (区間の中だけ。名前ごとの最大を出すため。T.22.4)
        self.per_name = {}
        # 負のキャッシュ (名前 → 期限の時刻。T.29.17)
        self.negative = {}
        self.c = self.zero()

    @staticmethod
    def zero():
        return {"lookups": 0, "hits": 0, "misses": 0, "negative_hits": 0,
                "misses_by_kind": {k: 0 for k in MISS_KINDS},
                "refreshes": 0, "refresh_warm": 0, "refresh_ask": 0,
                "stale_hits": 0, "refresh_stale": 0, "stale_hits_warm": 0,
                "stale_age_sum": 0.0, "stale_age_max": 0.0,
                "promotes": 0, "warm_evicted": 0, "warm_max": 0,
                "warm_secs": 0.0}

    def prime(self, name, last_used, warm=False):
        """表に「前から居た」名前を置く (答えは持っているが期限は切れている)。

        `warm` なら、その時刻に warm に上げた名前として置く (上限側。T.22.4)。数えない間に呼ぶ。
        """
        if name not in self.table:
            self.table[name] = {"has_addrs": True, "resolved_at": last_used,
                                "last_used": last_used, "warm": False}
            if warm and self.window > 0:
                self._promote(name, last_used)

    # -- 裏の引き直し (`warm_next` + `refresh_one`)

    def _refresh(self, e, now, kind, name=None):
        e["resolved_at"] = now
        if self.counting:
            self.c["refreshes"] += 1
            self.c[kind] += 1
            if name is not None:
                self.per_name[name] = self.per_name.get(name, 0) + 1

    def advance(self, now):
        """`now` までに期限の来た warm の予定を順に捌く (`dns-refresh` スレッドの写し)。"""
        while self.heap and self.heap[0][0] <= now:
            due, name = heapq.heappop(self.heap)
            if self.queue.get(name) != due:
                continue  # 予定が変わった古い組
            self._tick_warm(due)
            del self.queue[name]
            e = self.table[name]
            if not e["warm"] or due - e["last_used"] >= self.window:
                # 窓の外に出た = 誰も使っていない。ここで止める
                e["warm"] = False
                continue
            self._schedule(name, due + self.k)
            self._refresh(e, due, "refresh_warm", name)
        self._tick_warm(now)

    def _tick_warm(self, now):
        # warm の件数 × 時間 (平均を出すため)
        last = self._warm_t
        if last is not None and self.counting and now > last:
            self.c["warm_secs"] += len(self.queue) * (now - last)
        self._warm_t = now

    def _schedule(self, name, at):
        self.queue[name] = at
        heapq.heappush(self.heap, (at, name))

    def _promote(self, name, now):
        """`warm_promote`: 満杯なら最後の使用がいちばん古い名前を外してから入れる。"""
        self.queue.pop(name, None)
        if len(self.queue) >= self.max_warm:
            victim = min(self.queue, key=lambda k: self.table[k]["last_used"])
            del self.queue[victim]
            self.table[victim]["warm"] = False
            if self.counting:
                self.c["warm_evicted"] += 1
        at = now + self.k
        if self.promote_at == "answer":
            # 答えの齢から出す: 引いた時刻 + 3/4 TTL (過ぎていればすぐ)
            at = max(now, self.table[name]["resolved_at"] + self.k)
        self._schedule(name, at)
        self.table[name]["warm"] = True
        if self.counting:
            self.c["promotes"] += 1
            self.c["warm_max"] = max(self.c["warm_max"], len(self.queue))

    # -- 要求の経路 (`resolve_host`)

    def lookup(self, name, now, fail=False):
        self.advance(now)
        if self.counting:
            self.c["lookups"] += 1
        if fail:
            # 名前解決に失敗した接続 (T.29.17): 負のキャッシュの期限内なら当たり、それ以外はミスで
            # 期限を延ばす。表の答え (正の方) と裏の引き直しには触らない
            if self.negative.get(name, 0) > now:
                if self.counting:
                    self.c["negative_hits"] += 1
            else:
                self._miss("negative")
                self.negative[name] = now + NEGATIVE_TTL
            return
        e = self.table.get(name)
        if e is None:
            self.table[name] = {"has_addrs": True, "resolved_at": now,
                                "last_used": now, "warm": False}
            self._miss("cold")
            return
        was_warm = e["warm"]
        idle = now - e["last_used"]
        hot = idle < self.ttl
        promote = self.rule(e, idle, self.window)
        e["last_used"] = now
        age = now - e["resolved_at"]
        if e["has_addrs"] and age < self.ttl:
            if self.counting:
                self.c["hits"] += 1
            if hot and age >= self.k:
                # 当たりのまま裏で 1 回引き直す (T.13.1)。一瞬で終わるとする
                self._refresh(e, now, "refresh_ask", name)
        elif e["has_addrs"] and age < self.serve_stale:
            # serve-stale (T.21.3): 期限切れでも齢が窓の中なら古い答えで繋ぎに行き (ミスではない)、
            # 裏で 1 回だけ引き直す。引き直しは一瞬で終わるとする (同じ名前を二重に頼む間が無い)
            if self.counting:
                self.c["stale_hits"] += 1
                # warm なのに期限が切れていた (`warm_promote` の予定のすき間など。serve-stale の
                # もとでは `warm_stale` のミスにならず、ここに出る。T.22.4)
                self.c["stale_hits_warm"] += int(was_warm)
                # 古い答えの齢 (warm をやめる案で、どれだけ古い番地に繋ぎに行くかを読む。T.22.4)
                self.c["stale_age_sum"] += age
                self.c["stale_age_max"] = max(self.c["stale_age_max"], age)
            self._refresh(e, now, "refresh_stale", name)
        else:
            self._miss("warm_stale" if was_warm else ("expired" if e["has_addrs"] else "cold"))
            e["resolved_at"] = now
            e["has_addrs"] = True
        if promote:
            self._promote(name, now)

    def _miss(self, kind):
        if self.counting:
            self.c["misses"] += 1
            self.c["misses_by_kind"][kind] += 1


# ------------------------------------------------------------------ 雪像から到着列を作る

def history_rows(snap, res="60"):
    """`/history?res=<res>` の標本を辞書の列にする (`keys` の並びで読む)。"""
    h = (snap.get("history") or {}).get(res) or {}
    keys = h.get("keys") or []
    return [dict(zip(keys, row)) for row in (h.get("samples") or []) if isinstance(row, list)]


def build(snap):
    """雪像から (助走の到着列, 区間の到着列, 区間の始まり, 区間の終わり, 前から居た名前) を作る。"""
    taken = snap.get("taken_at") or 0
    recent = ((snap.get("recent") or {}).get("recent")) or []
    if not recent:
        raise ValueError("the snapshot has no /recent records")
    # 閉じた時刻の最小 = そこから後に開いた接続は全部そろっている
    c0 = min(r["at"] + (r.get("secs") or 0) for r in recent)
    inside = [(r["at"], host_of(r["target"])) for r in recent if r["at"] >= c0]
    for c in ((snap.get("connections") or {}).get("connections")) or []:
        at = taken - (c.get("age_secs") or 0)
        if at >= c0:
            inside.append((at, host_of(c["target"])))
    inside = sorted((t, h) for t, h in inside if not is_ip(h))

    hs = snap.get("hosts_series") or {}
    win = hs.get("window_secs") or SERIES_WINDOW
    t0 = hs.get("t0") or 0
    keys = hs.get("keys") or ["count"]
    ci = keys.index("count") if "count" in keys else 0
    warmup = []
    for s in hs.get("series") or []:
        name = host_of(s.get("host") or "")
        if not name or name == "other" or is_ip(name):
            continue
        for i, row in enumerate(s.get("samples") or []):
            n = row[ci] if isinstance(row, list) else 0
            start = t0 + i * win
            for j in range(n or 0):
                t = start + (j + 0.5) * win / n
                if t < c0:
                    warmup.append((t, name))
    warmup.sort()

    # 区間の前から表に居た名前 (`/dns` の通算のミスが区間の中のミスより多い)。区間の中の
    # ミスは `/recent` の `ms.dns` が 0 でない個票で数える (デプロイ先のミスは 1 回 数 ms 以上)
    seen_before = set()
    in_misses = {}
    for r in recent:
        if r["at"] >= c0 and ((r.get("ms") or {}).get("dns") or 0) > 0:
            h = host_of(r["target"])
            in_misses[h] = in_misses.get(h, 0) + 1
    names_inside = {h for _, h in inside}
    for e in ((snap.get("dns") or {}).get("entries")) or []:
        h = (e.get("host") or "").lower()
        if h in names_inside and (e.get("misses") or 0) > in_misses.get(h, 0):
            seen_before.add(h)
    return warmup, inside, c0, taken, seen_before


def failed_lookups(snap, c0):
    """区間の中で名前解決に失敗した接続 (`/recent` の理由が `DNS_FAIL_REASONS`) の `(at, 名前)` の集合 (T.29.17)。"""
    recent = ((snap.get("recent") or {}).get("recent")) or []
    return {(r["at"], host_of(r["target"])) for r in recent
            if r["at"] >= c0 and r.get("reason") in DNS_FAIL_REASONS}


def actual_refresh_per_hour(snap):
    """実機の裏の引き直し (回/時) = `/status` の `dns.refreshes` ÷ 起動からの時間 (無ければ None)。"""
    d = ((snap.get("status") or {}).get("dns")) or {}
    up = snap.get("uptime_secs") or 0
    if "refreshes" not in d or not up:
        return None
    return d["refreshes"] / (up / 3600.0)


def actual(snap, c0, taken):
    """同じ区間の実機の値 (`/history?res=60`)。"""
    conns = misses = n = 0
    for r in history_rows(snap):
        t = r.get("t") or 0
        if t + 60 > c0 and t <= taken:
            conns += r.get("connects") or 0
            misses += r.get("dns_misses") or 0
            n += 1
    if not n:
        return None
    return {"connects": conns, "dns_misses": misses,
            "miss_per_connect": (misses / conns) if conns else None}


def replay(warmup, inside, c0, taken, rule, seen_before=(), ttl=TTL, window=WARM,
           max_warm=MAX_WARM, serve_stale=0, promote_at="now", prime_warm=False,
           refresh_at=None, fails=frozenset()):
    """助走を数えずに回し、区間 `[c0, taken]` の中だけを数える。

    `fails` は名前解決に失敗した到着の `(時刻, 名前)` の集合 (`failed_lookups`。T.29.17)。負のキャッシュで回す。

    `refresh_at` は先回りの齢 (秒)。`None` = TTL の 3/4 (`refresh_after`)。T.24.1 で
    「k と TTL のどちらが引き直しの間隔を決めているか」を分けて見るために足した。
    """
    tb = Table(rule, ttl, window, max_warm, serve_stale, promote_at, refresh_at)
    tb.counting = False
    for t, h in warmup:
        tb.lookup(h, t)
    tb.advance(c0)
    # 助走に出てこなかったが前から表に居た名前: 最後の使用は分からないので窓の外に置く
    # (いまの規則では上がらず、`idle < window` を外した規則でだけ上がる = 案の効きの上限側)。
    # 置く時刻は窓に依らない (窓を変える案 (T.22.4) で答えの齢が変わらないように)。
    # `prime_warm` なら区間の始まりで warm だったとして置く (T.22.4 の上限側)
    for h in sorted(seen_before):
        if prime_warm:
            tb.prime(h, c0, warm=True)
        else:
            tb.prime(h, c0 - PRIME_BEFORE)
    tb.counting = True
    tb.c["warm_max"] = len(tb.queue)
    tb._warm_t = c0
    for t, h in inside:
        tb.lookup(h, t, (t, h) in fails)
    tb.advance(taken)
    c = tb.c
    hours = (taken - c0) / 3600.0
    c["hours"] = hours
    c["miss_per_lookup"] = (c["misses"] / c["lookups"]) if c["lookups"] else None
    c["refresh_per_hour"] = (c["refreshes"] / hours) if hours > 0 else None
    c["warm_avg"] = (c["warm_secs"] / (taken - c0)) if taken > c0 else None
    c["stale_age_avg"] = (c["stale_age_sum"] / c["stale_hits"]) if c["stale_hits"] else None
    top = max(tb.per_name.values(), default=0)
    c["refresh_name_max"] = top
    c["refresh_name_max_per_hour"] = (top / hours) if hours > 0 else None
    # 名前ごとの引き直しの回数 (区間の中だけ)。T.24.1 で 1 つの名前 (短い TTL の名前) を
    # 観測と突き合わせるために出す
    c["per_name"] = dict(tb.per_name)
    return c


def within(v, target, tol=TOLERANCE):
    return v is not None and abs(v - target) <= tol * target


def stale_verdict(cur, new):
    """serve-stale の規則がいまの規則に対して目安 (T.21.3) を満たすか。"""
    e0, e1 = cur["misses_by_kind"]["expired"], new["misses_by_kind"]["expired"]
    r0, r1 = cur["refreshes"], new["refreshes"]
    expired = (e1 / e0) if e0 else None
    refreshes = ((r1 - r0) / r0) if r0 else None
    ok = (e1 <= STALE_EXPIRED_MAX * e0) and refreshes is not None \
        and refreshes <= STALE_REFRESH_MAX
    return {"expired_ratio": expired, "refreshes": refreshes, "ok": ok}


def run(snap, rules, prime=True, stale=()):
    warmup, inside, c0, taken, seen_before = build(snap)
    fails = failed_lookups(snap, c0)
    real = actual(snap, c0, taken)
    real_refresh = actual_refresh_per_hour(snap)
    if real is not None:
        real["refresh_per_hour"] = real_refresh
    out = {"window": {"from": c0, "to": taken, "hours": (taken - c0) / 3600.0,
                      "lookups": len(inside), "warmup_lookups": len(warmup),
                      "failed_lookups": len(fails),
                      "seen_before": sorted(seen_before) if prime else []},
           "actual": real, "rules": {}}
    for name in rules:
        rule, kw = PLANS[name]
        out["rules"][name] = replay(warmup, inside, c0, taken, rule,
                                    seen_before if prime else (), fails=fails, **kw)
    # serve-stale (T.21.3) は keep-warm の模型の `promote` の上に足す (比べる相手は `keep_warm`)
    for secs in stale:
        out["rules"][f"stale_{secs}"] = replay(warmup, inside, c0, taken, promote_current,
                                               seen_before if prime else (), serve_stale=secs,
                                               fails=fails)
    cur = out["rules"].get("current")
    if cur:
        # 実機の値と比べる (T.29.17)。ミスは同じ区間の回数、引き直しは起動からの回/時。
        # 雪像に無ければ T.16.99 の物差し (ミス/到着と回/時)
        if real is not None and real.get("dns_misses"):
            miss_ok = within(cur["misses"], real["dns_misses"])
            miss_target = {"misses": real["dns_misses"]}
        else:
            miss_ok = within(cur["miss_per_lookup"], TARGET_MISS_PER_CONNECT)
            miss_target = {"miss_per_lookup": TARGET_MISS_PER_CONNECT}
        refresh_target = real_refresh if real_refresh else TARGET_REFRESH_PER_HOUR
        out["fit"] = {
            "miss_per_lookup": miss_ok,
            "refresh_per_hour": within(cur["refresh_per_hour"], refresh_target),
            "target": dict(miss_target, refresh_per_hour=refresh_target),
        }
    base = out["rules"].get("keep_warm")
    if base and "no_idle" in out["rules"]:
        cur = base
        new = out["rules"]["no_idle"]
        out["delta"] = {
            "misses": ((new["misses"] - cur["misses"]) / cur["misses"]) if cur["misses"] else None,
            "refreshes": ((new["refreshes"] - cur["refreshes"]) / cur["refreshes"])
            if cur["refreshes"] else None,
        }
    base = out["rules"].get("keep_warm")
    if base and stale:
        out["stale"] = {str(secs): stale_verdict(base, out["rules"][f"stale_{secs}"])
                        for secs in stale}
        ok = [secs for secs in stale if out["stale"][str(secs)]["ok"]]
        # 既定は目安を満たすいちばん短い窓 (長いほど古い番地に繋ぎに行く見込みが増える)
        out["stale_pick"] = min(ok) if ok else None
    return out


def build_series(snap):
    """`/hosts_series` だけから到着列を作る (上位 16 名前 × 24 時間。T.22.4 の `--series`)。

    5 分の窓の中の到着は等間隔に置く (`build` の助走と同じ)。返すのは (到着列, 始まり, 終わり)。
    """
    hs = snap.get("hosts_series") or {}
    win = hs.get("window_secs") or SERIES_WINDOW
    t0 = hs.get("t0") or 0
    keys = hs.get("keys") or ["count"]
    ci = keys.index("count") if "count" in keys else 0
    out = []
    end = t0
    for s in hs.get("series") or []:
        name = host_of(s.get("host") or "")
        rows = s.get("samples") or []
        end = max(end, t0 + len(rows) * win)
        if not name or name == "other" or is_ip(name):
            continue
        for i, row in enumerate(rows):
            n = row[ci] if isinstance(row, list) else 0
            for j in range(n or 0):
                out.append((t0 + i * win + (j + 0.5) * win / n, name))
    out.sort()
    if not out:
        raise ValueError("the snapshot has no /hosts_series counts")
    return out, t0, end


def grid_case(base, delta):
    """基準の設定 (`base`) に案の差分を重ねた `replay` の引数。"""
    kw = {"window": WARM, "max_warm": MAX_WARM, "serve_stale": SERVE_STALE, "promote_at": "now"}
    kw.update(base)
    kw.update(delta)
    return kw


def grid_verdict(base, new):
    """案が基準に対して目安 (T.22.4) を満たすか。ミスの増えは 1 日あたりに直す。"""
    def bad(c):
        k = c["misses_by_kind"]
        return k["expired"] + k["warm_stale"]
    hours = base["hours"]
    more = bad(new) - bad(base)
    per_day = (more / hours * 24) if hours > 0 else None
    ratio = (new["refreshes"] / base["refreshes"]) if base["refreshes"] else None
    ok = ratio is not None and per_day is not None and ratio <= GRID_REFRESH_MAX \
        and per_day <= GRID_MISSES_PER_DAY
    return {"refresh_ratio": ratio, "misses_more": more, "misses_more_per_day": per_day, "ok": ok}


def run_grid(snap, series=False, base=None):
    """T.22.4 の案を並べる。`series` なら `/hosts_series` だけ、そうでなければ `build` の区間で。

    区間で回すときは、前から居た名前の置き方を下限側 (`lower` = warm でない) と上限側
    (`upper` = 区間の始まりで warm) の 2 通り回す。目安は両方で満たして「満たす」とする。
    """
    base = base or {}
    if series:
        arrivals, t0, end = build_series(snap)
        sides = {"series": ((), arrivals, t0, end, (), False)}
        window = {"from": t0, "to": end, "hours": (end - t0) / 3600.0, "lookups": len(arrivals),
                  "names": len({h for _, h in arrivals})}
        real = None
    else:
        warmup, inside, c0, taken, seen = build(snap)
        sides = {"lower": (warmup, inside, c0, taken, seen, False),
                 "upper": (warmup, inside, c0, taken, seen, True)}
        window = {"from": c0, "to": taken, "hours": (taken - c0) / 3600.0, "lookups": len(inside),
                  "warmup_lookups": len(warmup), "seen_before": len(seen)}
        real = actual_grid(snap, c0, taken)
    out = {"window": window, "actual": real, "sides": {}}
    for side, (w, ins, a, b, seen, pw) in sides.items():
        rows = {}
        for name, delta in GRID:
            rows[name] = replay(w, ins, a, b, promote_current, seen, prime_warm=pw,
                                **grid_case(base, delta))
        verdict = {name: grid_verdict(rows["base"], rows[name]) for name, _ in GRID[1:]}
        out["sides"][side] = {"rules": rows, "verdict": verdict}
    ok = [name for name, _ in GRID[1:]
          if all(v["verdict"][name]["ok"] for v in out["sides"].values())]
    out["pass"] = ok
    return out


def actual_grid(snap, c0, taken):
    """同じ区間の実機の値 (T.22.4 の突き合わせ): ミスの回数と、warm の件数の平均・最大 (`/history?res=60`)。

    実機の引き直しの回数は区間では取れないので、warm の件数の平均 × 3,600 / (3/4 TTL) を
    「warm の引き直しの回/時」の見積もりとして並べる (warm の名前は 3/4 TTL ごとに 1 回引く)。
    """
    a = actual(snap, c0, taken)
    if a is None:
        return None
    warm = [r.get("dns_warm") for r in history_rows(snap)
            if (r.get("t") or 0) + 60 > c0 and (r.get("t") or 0) <= taken
            and r.get("dns_warm") is not None]
    peak = [r.get("dns_warm_max") or r.get("dns_warm") or 0 for r in history_rows(snap)
            if (r.get("t") or 0) + 60 > c0 and (r.get("t") or 0) <= taken]
    if warm:
        a["warm_avg"] = sum(warm) / len(warm)
        a["warm_max"] = max(peak) if peak else None
        a["warm_refresh_per_hour"] = a["warm_avg"] * 3600.0 / refresh_after(TTL)
    d = ((snap.get("status") or {}).get("dns")) or {}
    if d:
        a["misses_by_kind_since_start"] = d.get("misses_by_kind")
        up = snap.get("uptime_secs") or 0
        a["refresh_per_hour_since_start"] = (d.get("refreshes", 0) / (up / 3600.0)) if up else None
        # 模型に依らない上限: warm をやめると、裏の引き直しは名前を引いた回 (当たり・古い答え・
        # ミス) に高々 1 回しか起きない。その和の、いまの引き直しに対する比 (起動から)
        looks = (d.get("hits") or 0) + (d.get("misses") or 0) + (d.get("stale_hits") or 0)
        refr = d.get("refreshes") or 0
        a["no_warm_refresh_bound"] = (looks / refr) if refr else None
    return a


def render_grid(out):
    w = out["window"]
    lines = [f"区間 {w['from']}〜{w['to']} ({w['hours']:.2f} 時間、到着 {w['lookups']} 件)"]
    a = out.get("actual")
    if a:
        lines.append(f"実機 (同じ区間の `/history?res=60`): 確立 {a['connects']} 本、ミス {a['dns_misses']} 回"
                     + (f"、warm 平均 {a['warm_avg']:.1f} / 最大 {a['warm_max']} "
                        f"(warm の引き直しの見積もり {a['warm_refresh_per_hour']:.0f} 回/時)"
                        if "warm_avg" in a else ""))
        if a.get("misses_by_kind_since_start"):
            k = a["misses_by_kind_since_start"]
            rph = a.get("refresh_per_hour_since_start")
            lines.append(f"実機 (起動から): cold {k.get('cold')} / expired {k.get('expired')} / "
                         f"warm_stale {k.get('warm_stale')}、引き直し "
                         + ("—" if rph is None else f"{rph:.0f}") + " 回/時")
        if a.get("no_warm_refresh_bound") is not None:
            lines.append(f"warm をやめたときの引き直しの上限 (名前を引いた回 ÷ いまの引き直し、起動から): "
                         f"{a['no_warm_refresh_bound'] * 100:.0f}%")
    for side, v in out["sides"].items():
        lines.append("")
        lines.append(f"[{side}]")
        lines.append("| 案 | ミス | cold / expired / warm_stale | 古い答え (うち warm) | 古い答えの齢 平均 / 最大 秒 | 引き直し | 回/時 "
                     "| うち warm / 先回り / 古い答え | 名前ごとの最大 回/時 | warm 平均 / 最大 | 追い出し "
                     "| 引き直しの比 | expired+warm_stale の増え (/日) | 目安 |")
        lines.append("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
        for name, c in v["rules"].items():
            k = c["misses_by_kind"]
            sage = "—" if c["stale_age_avg"] is None else f"{c['stale_age_avg']:.0f}"
            vd = v["verdict"].get(name)
            tail = "| — | — | 基準 |" if vd is None else (
                f"| {vd['refresh_ratio'] * 100:.0f}% | {vd['misses_more']:+d} "
                f"({vd['misses_more_per_day']:+.1f}) | {'満たす' if vd['ok'] else '届かない'} |")
            lines.append(
                f"| {name} | {c['misses']} | {k['cold']} / {k['expired']} / {k['warm_stale']} "
                f"| {c['stale_hits']} ({c['stale_hits_warm']}) "
                f"| {sage} / {c['stale_age_max']:.0f} | {c['refreshes']} "
                f"| {c['refresh_per_hour']:.1f} "
                f"| {c['refresh_warm']} / {c['refresh_ask']} / {c['refresh_stale']} "
                f"| {c['refresh_name_max_per_hour']:.1f} | {c['warm_avg']:.2f} / {c['warm_max']} "
                f"| {c['warm_evicted']} {tail}")
    lines.append("")
    lines.append(f"目安 (T.22.4): 引き直しが基準の {GRID_REFRESH_MAX * 100:.0f}% 以下、かつ "
                 f"expired + warm_stale の増えが 1 日 {GRID_MISSES_PER_DAY:.0f} 回以内 (どの置き方でも)")
    note = (" (参考: `/hosts_series` は上位 16 名前を入れ替えながら持つだけで、5 分の中は等間隔に置く)"
            if "series" in out["sides"] else "")
    lines.append("満たす案" + note + ": " + (", ".join(out["pass"]) if out["pass"] else "無し"))
    return "\n".join(lines)


def pct(v):
    return "—" if v is None else f"{v * 100:+.1f}%"


def render(out):
    w = out["window"]
    lines = [f"区間 {w['from']}〜{w['to']} ({w['hours']:.2f} 時間、到着 {w['lookups']} 件、"
             f"助走 {w['warmup_lookups']} 件、前から居た名前 {len(w['seen_before'])} 件)", ""]
    a = out.get("actual")
    if a:
        mp = a["miss_per_connect"]
        lines.append(f"実機 (同じ区間の `/history?res=60`): 確立 {a['connects']} 本、ミス {a['dns_misses']} 回"
                     + (f" = {mp:.3f} 回/接続" if mp is not None else ""))
    tgt = (out.get("fit") or {}).get("target") or {}
    if "misses" in tgt:
        lines.append(f"物差し (T.29.17。この雪像の実機): ミス {tgt['misses']} 回 (同じ区間)、引き直し "
                     f"{tgt['refresh_per_hour']:.0f} 回/時 (`/status` の起動から) (±{TOLERANCE * 100:.0f}%)")
    else:
        lines.append(f"物差し (T.16.99): ミス {TARGET_MISS_PER_CONNECT} 回/接続、引き直し "
                     f"{TARGET_REFRESH_PER_HOUR:.0f} 回/時 (±{TOLERANCE * 100:.0f}%)")
    if w.get("failed_lookups"):
        lines.append(f"名前解決に失敗した到着 {w['failed_lookups']} 件は負のキャッシュ ({NEGATIVE_TTL} 秒) で回す")
    lines.append("")
    lines.append("| 規則 | 到着 | ミス | ミス/到着 | cold / expired / warm_stale / negative | 古い答え | 引き直し | 回/時 "
                 "| うち warm / 先回り / 古い答え | 上げた | warm 平均 / 最大 |")
    lines.append("|---|---|---|---|---|---|---|---|---|---|---|")
    for name, c in out["rules"].items():
        k = c["misses_by_kind"]
        lines.append(
            f"| {name} | {c['lookups']} | {c['misses']} | {c['miss_per_lookup']:.3f} "
            f"| {k['cold']} / {k['expired']} / {k['warm_stale']} / {k['negative']} | {c['stale_hits']} "
            f"| {c['refreshes']} | {c['refresh_per_hour']:.1f} "
            f"| {c['refresh_warm']} / {c['refresh_ask']} / {c['refresh_stale']} "
            f"| {c['promotes']} | {c['warm_avg']:.2f} / {c['warm_max']} |")
    fit = out.get("fit")
    if fit:
        ok = fit["miss_per_lookup"] and fit["refresh_per_hour"]
        lines.append("")
        lines.append(f"いまの規則の一致: ミス {'合う' if fit['miss_per_lookup'] else '合わない'}、"
                     f"引き直し {'合う' if fit['refresh_per_hour'] else '合わない'}"
                     + ("" if ok else " → **模型が実機と合わないので、案の差は読まない**"))
    d = out.get("delta")
    if d:
        lines.append(f"`idle < window` を外すと: ミス {pct(d['misses'])}、引き直し {pct(d['refreshes'])} "
                     "(採る目安: ミス −15% 以上かつ引き直し +10% 以内)")
    for secs, v in (out.get("stale") or {}).items():
        ratio = "—" if v["expired_ratio"] is None else f"{v['expired_ratio'] * 100:.0f}%"
        lines.append(f"serve-stale {secs} 秒: expired はいまの {ratio}、引き直し {pct(v['refreshes'])} → "
                     + ("目安を満たす" if v["ok"] else "目安に届かない")
                     + " (目安: expired が半分以下かつ引き直し +10% 以内)")
    if "stale_pick" in out:
        pick = out["stale_pick"]
        lines.append("目安を満たすいちばん短い窓: " + ("無し" if pick is None else f"{pick} 秒"))
    return "\n".join(lines)


def main(argv=None):
    p = argparse.ArgumentParser(description="replay the DNS table rules over a snapshot")
    p.add_argument("snapshot")
    p.add_argument("--rule", choices=["current", "keep_warm", "no_idle", "all"], default="all")
    p.add_argument("--serve-stale", type=int, action="append", default=[], metavar="SECS",
                   help="also replay serve-stale: answer from an expired entry younger than SECS "
                        "and refresh it in the background (repeatable)")
    p.add_argument("--no-prime", action="store_true",
                   help="do not seed names that were in the table before the window")
    p.add_argument("--grid", action="store_true",
                   help="compare the warm limit / window variants under serve-stale (T.22.4)")
    p.add_argument("--series", action="store_true",
                   help="with --grid, replay only /hosts_series (top hosts over 24 hours)")
    p.add_argument("--json", action="store_true")
    a = p.parse_args(argv)
    with open(a.snapshot, encoding="utf-8") as f:
        snap = json.load(f)
    if a.series and not a.grid:
        print("dns-replay: --series needs --grid", file=sys.stderr)
        return 2
    if a.grid:
        try:
            out = run_grid(snap, series=a.series)
        except ValueError as e:
            print(f"dns-replay: {e}", file=sys.stderr)
            return 2
        print(json.dumps(out, indent=1) if a.json else render_grid(out))
        return 0
    rules = list(PLANS) if a.rule == "all" else [a.rule]
    if a.serve_stale and "keep_warm" not in rules:
        rules.append("keep_warm")  # serve-stale は keep-warm の模型と比べる
    if any(s <= 0 for s in a.serve_stale):
        print("dns-replay: --serve-stale must be positive", file=sys.stderr)
        return 2
    try:
        out = run(snap, rules, prime=not a.no_prime, stale=a.serve_stale)
    except ValueError as e:
        print(f"dns-replay: {e}", file=sys.stderr)
        return 2
    print(json.dumps(out, indent=1) if a.json else render(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
