#!/usr/bin/env python3
# 名前解決の表 (`crates/net-dns/src/dns.rs`) を**再生の模型**で回し、規則を変えたときの
# ミスと引き直しの数を見積もる (T17.5)。
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
# 使い方: scripts/dns-replay.py SNAP.json [--rule current|no_idle|all] [--no-prime] [--json]
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
# 実機の値 (T16.99: 平常時のミス 0.06 回/接続、裏の引き直し 436 回/時)。模型が合っているかの物差し
TARGET_MISS_PER_CONNECT = 0.06
TARGET_REFRESH_PER_HOUR = 436.0
# 「合う」と見なす幅 (±30%)
TOLERANCE = 0.30
# `hosts_series` の窓の長さの既定 (応答に `window_secs` が無いとき)
SERIES_WINDOW = 300
MISS_KINDS = ("cold", "expired", "warm_stale")


def refresh_after(ttl):
    """期限前に引き直し始める齢 (`dns.rs` の `refresh_after` = TTL の 3/4)。"""
    return ttl - ttl / 4


def promote_current(e, idle, window):
    """いまの規則 (`dns.rs` の `promote`): 窓の中で 2 回目の使用なら warm に上げる。"""
    return not e["warm"] and window > 0 and idle < window and e["has_addrs"]


def promote_no_idle(e, idle, window):
    """T17.5 の案: `idle < window` を外す (前に 1 回でも使って答えを持っていれば上げる)。"""
    return not e["warm"] and window > 0 and e["has_addrs"]


RULES = {"current": promote_current, "no_idle": promote_no_idle}


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

    def __init__(self, rule, ttl=TTL, window=WARM, max_warm=MAX_WARM):
        self.rule = rule
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
        self.c = self.zero()

    @staticmethod
    def zero():
        return {"lookups": 0, "hits": 0, "misses": 0,
                "misses_by_kind": {k: 0 for k in MISS_KINDS},
                "refreshes": 0, "refresh_warm": 0, "refresh_ask": 0,
                "promotes": 0, "warm_evicted": 0, "warm_max": 0,
                "warm_secs": 0.0}

    def prime(self, name, last_used):
        """表に「前から居た」名前を置く (答えは持っているが期限は切れている)。"""
        if name not in self.table:
            self.table[name] = {"has_addrs": True, "resolved_at": last_used,
                                "last_used": last_used, "warm": False}

    # -- 裏の引き直し (`warm_next` + `refresh_one`)

    def _refresh(self, e, now, kind):
        e["resolved_at"] = now
        if self.counting:
            self.c["refreshes"] += 1
            self.c[kind] += 1

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
            self._schedule(name, due + refresh_after(self.ttl))
            self._refresh(e, due, "refresh_warm")
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
        self._schedule(name, now + refresh_after(self.ttl))
        self.table[name]["warm"] = True
        if self.counting:
            self.c["promotes"] += 1
            self.c["warm_max"] = max(self.c["warm_max"], len(self.queue))

    # -- 要求の経路 (`resolve_host`)

    def lookup(self, name, now):
        self.advance(now)
        if self.counting:
            self.c["lookups"] += 1
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
            if hot and age >= refresh_after(self.ttl):
                # 当たりのまま裏で 1 回引き直す (T13.1)。一瞬で終わるとする
                self._refresh(e, now, "refresh_ask")
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
           max_warm=MAX_WARM):
    """助走を数えずに回し、区間 `[c0, taken]` の中だけを数える。"""
    tb = Table(rule, ttl, window, max_warm)
    tb.counting = False
    for t, h in warmup:
        tb.lookup(h, t)
    tb.advance(c0)
    # 助走に出てこなかったが前から表に居た名前: 最後の使用は分からないので窓の外に置く
    # (いまの規則では上がらず、`idle < window` を外した規則でだけ上がる = 案の効きの上限側)
    for h in sorted(seen_before):
        tb.prime(h, c0 - 2 * window)
    tb.counting = True
    tb._warm_t = c0
    for t, h in inside:
        tb.lookup(h, t)
    tb.advance(taken)
    c = tb.c
    hours = (taken - c0) / 3600.0
    c["hours"] = hours
    c["miss_per_lookup"] = (c["misses"] / c["lookups"]) if c["lookups"] else None
    c["refresh_per_hour"] = (c["refreshes"] / hours) if hours > 0 else None
    c["warm_avg"] = (c["warm_secs"] / (taken - c0)) if taken > c0 else None
    return c


def within(v, target, tol=TOLERANCE):
    return v is not None and abs(v - target) <= tol * target


def run(snap, rules, prime=True):
    warmup, inside, c0, taken, seen_before = build(snap)
    out = {"window": {"from": c0, "to": taken, "hours": (taken - c0) / 3600.0,
                      "lookups": len(inside), "warmup_lookups": len(warmup),
                      "seen_before": sorted(seen_before) if prime else []},
           "actual": actual(snap, c0, taken), "rules": {}}
    for name in rules:
        out["rules"][name] = replay(warmup, inside, c0, taken, RULES[name],
                                    seen_before if prime else ())
    cur = out["rules"].get("current")
    if cur:
        out["fit"] = {
            "miss_per_lookup": within(cur["miss_per_lookup"], TARGET_MISS_PER_CONNECT),
            "refresh_per_hour": within(cur["refresh_per_hour"], TARGET_REFRESH_PER_HOUR),
        }
    if cur and "no_idle" in out["rules"]:
        new = out["rules"]["no_idle"]
        out["delta"] = {
            "misses": ((new["misses"] - cur["misses"]) / cur["misses"]) if cur["misses"] else None,
            "refreshes": ((new["refreshes"] - cur["refreshes"]) / cur["refreshes"])
            if cur["refreshes"] else None,
        }
    return out


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
    lines.append(f"物差し (T16.99): ミス {TARGET_MISS_PER_CONNECT} 回/接続、引き直し "
                 f"{TARGET_REFRESH_PER_HOUR:.0f} 回/時 (±{TOLERANCE * 100:.0f}%)")
    lines.append("")
    lines.append("| 規則 | 到着 | ミス | ミス/到着 | cold / expired / warm_stale | 引き直し | 回/時 "
                 "| うち warm / 先回り | 上げた | warm 平均 / 最大 |")
    lines.append("|---|---|---|---|---|---|---|---|---|---|")
    for name, c in out["rules"].items():
        k = c["misses_by_kind"]
        lines.append(
            f"| {name} | {c['lookups']} | {c['misses']} | {c['miss_per_lookup']:.3f} "
            f"| {k['cold']} / {k['expired']} / {k['warm_stale']} | {c['refreshes']} "
            f"| {c['refresh_per_hour']:.1f} | {c['refresh_warm']} / {c['refresh_ask']} "
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
    return "\n".join(lines)


def main(argv=None):
    p = argparse.ArgumentParser(description="replay the DNS table rules over a snapshot")
    p.add_argument("snapshot")
    p.add_argument("--rule", choices=["current", "no_idle", "all"], default="all")
    p.add_argument("--no-prime", action="store_true",
                   help="do not seed names that were in the table before the window")
    p.add_argument("--json", action="store_true")
    a = p.parse_args(argv)
    with open(a.snapshot, encoding="utf-8") as f:
        snap = json.load(f)
    rules = list(RULES) if a.rule == "all" else [a.rule]
    try:
        out = run(snap, rules, prime=not a.no_prime)
    except ValueError as e:
        print(f"dns-replay: {e}", file=sys.stderr)
        return 2
    print(json.dumps(out, indent=1) if a.json else render(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
