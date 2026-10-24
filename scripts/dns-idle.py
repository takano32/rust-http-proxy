#!/usr/bin/env python3
# 雪像から **名前ごとの引き直しの回数と、実際の使用回数** を並べ、
# 「1 時間に 1 回も使われていないのに引き直した回」の割合を出す (T.24.1 の段 1)。
#
# 材料は雪像 (`/snapshot`) の 3 か所:
#   - `/status` の `dns.refreshes` … 起動からの引き直しの**合計** (表が切れていても完全)
#   - `/dns` の名前ごとの `refreshes`・`age_secs`・`ttl_left`・`idle_secs`・`misses`
#     (`/dns` は 256 KiB で切れることがある。切れていれば `truncated` が立つ)
#   - `/hosts` の名前ごとの `requests`・`last_seen`
#
# **窓の取り方で意味が変わる**ので、2 通りを別々に出す:
#   - 雪像 2 枚以上 (`pair`): 同じプロセスの 2 枚の差 (`Δ引き直し` と `Δ要求`)。
#     `/hosts` の `requests` は状態ファイルから復元した分を含む通算の値だが、**差を取れば
#     復元した分は消える**ので、この窓だけが「その間に何回使われて何回引き直したか」を正確に言える。
#     `Δ要求` が 0 の名前の `Δ引き直し` が「使われていないのに引き直した回」。
#   - 雪像 1 枚 (`since_start`): 起動から (`uptime_secs`) の平均。`/hosts` の `requests` は
#     復元した分を含むので**通算のまま出し、回/時には直さない** (起動の長さで割ると嘘になる。
#     2026-10-22 の `api.ipinfo.io` の 13,793 は 14 日ぶんの通算で、この起動の中の使用は
#     `/hosts_series` の 24 時間で約 2,800 = 117 回/時)。使われたかどうかは `/dns` の
#     `idle_secs` (このプロセスの中の最後の使用からの秒) で読む。
#
# **`idle_secs` の読み方**: 引き直しは「当たりの先回り」(齢が 3/4 TTL 以上の当たり) と
# 「古い答えを返したとき」(serve-stale) のどちらも**要求の経路**で起き、keep-warm の
# タイマーだけが要求なしで起きる。つまり **`idle_secs` より新しい引き直しは無い**。
# だから `idle_secs > 3600` の名前は、この 1 時間に 1 回も引き直していない。
#
# **`ttl_secs` (60) と `k` (45 秒) のどちらが効いているか** は `--model` で
# `scripts/dns-replay.py` の `Table` をそのまま回して確かめる (同じ雪像の `/recent` の到着列)。
# `k` は `refresh_after(ttl)` = TTL の 3/4 = 45 秒 (期限の手前で裏で引き直す齢)。
# k=45 と k=60 (= TTL) の 2 通りを回し、名前ごとの引き直しの回/時を観測と並べる。
# 模型の内訳 (`refresh_warm` / `refresh_ask` / `refresh_stale`) は、
# **タイマーで起きた引き直し (= 使われていない引き直し)** の割合そのもの。
#
# 使い方: scripts/dns-idle.py SNAP.json [SNAP2.json …] [--model] [--top 10] [--json]
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import importlib.util
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
# 「1 時間に 1 回も使われていない」の窓 (秒)
IDLE_SECS = 3600
# 段 2 に進む目安 (使われていない名前の引き直しが全体の 8 割以上)
IDLE_SHARE = 0.80
# 同じプロセスの 2 枚と見なす `uptime_secs` の伸びと `taken_at` の伸びのずれ (秒)
SAME_PROCESS_SLACK = 5
# 表に出す名前の数 (既定)
TOP = 10
# 模型で「名前ごとの値が読める」と見なす到着の数 (これ未満は 1 回の当たり外れで決まる)
MODEL_MIN_ARRIVALS = 24
# 模型の判定に使う名前の数 (引き直しの多い順)
MODEL_TOP = 3


def _load_replay():
    """`scripts/dns-replay.py` を読み込む (模型を 1 つに保つため、名前の切り出しも借りる)。"""
    path = os.path.join(HERE, "dns-replay.py")
    spec = importlib.util.spec_from_file_location("dns_replay", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


dr = _load_replay()
host_of = dr.host_of
SERVE_STALE = dr.SERVE_STALE


# ------------------------------------------------------------------ 雪像を読む

def taken_at(snap):
    return snap.get("taken_at") or 0


def uptime(snap):
    return snap.get("uptime_secs") or 0


def dns_meta(snap):
    return (snap.get("status") or {}).get("dns") or {}


def dns_entries(snap):
    """`/dns` の名前ごとの行 (名前は小文字)。"""
    out = {}
    for e in ((snap.get("dns") or {}).get("entries")) or []:
        name = (e.get("host") or "").lower()
        if name:
            out[name] = e
    return out


def dns_truncated(snap):
    return bool((snap.get("dns") or {}).get("truncated"))


def hosts_truncated(snap):
    return bool((snap.get("hosts") or {}).get("truncated"))


def requests_by_name(snap):
    """`/hosts` の `requests`・`last_seen` を名前ごとにまとめる (港・scheme を跨いで足す)。

    IP リテラルと `other` は名前解決を通らないので落とす。
    """
    out = {}
    for h in ((snap.get("hosts") or {}).get("hosts")) or []:
        name = host_of(h.get("host") or "")
        if not name or name == "other" or dr.is_ip(name):
            continue
        row = out.setdefault(name, {"requests": 0, "last_seen": None})
        row["requests"] += h.get("requests") or 0
        seen = h.get("last_seen")
        if seen is not None and (row["last_seen"] is None or seen > row["last_seen"]):
            row["last_seen"] = seen
    return out


def empty_row(name):
    return {"name": name, "refreshes": 0, "requests": None, "idle_secs": None,
            "age_secs": None, "ttl_left": None, "refresh_per_hour": None,
            "requests_per_hour": None, "refreshes_per_request": None}


# ------------------------------------------------------------------ 窓 (pair / since_start)

def same_process(a, b):
    """2 枚が同じプロセスのものか。(bool, 理由) を返す。"""
    if taken_at(b) <= taken_at(a):
        return False, "the second snapshot is not newer"
    if b.get("version") != a.get("version"):
        return False, "the version changed (restart)"
    du = uptime(b) - uptime(a)
    dt = taken_at(b) - taken_at(a)
    if du < 0:
        return False, "the uptime went backwards (restart)"
    if abs(du - dt) > SAME_PROCESS_SLACK:
        return False, "the uptime moved %.0f s but the clock moved %.0f s (restart)" % (du, dt)
    return True, ""


def pair_window(a, b):
    """同じプロセスの 2 枚から、名前ごとの `Δ引き直し` と `Δ要求` を作る。"""
    hours = (taken_at(b) - taken_at(a)) / 3600.0
    ja, jb = dns_entries(a), dns_entries(b)
    qa, qb = requests_by_name(a), requests_by_name(b)
    total = (dns_meta(b).get("refreshes") or 0) - (dns_meta(a).get("refreshes") or 0)
    rows = []
    for name in sorted(set(ja) & set(jb)):
        refr = (jb[name].get("refreshes") or 0) - (ja[name].get("refreshes") or 0)
        if refr < 0:
            # 表の行が作り直された (TTL で消えてから引き直した)。差は読めない
            continue
        row = empty_row(name)
        row["refreshes"] = refr
        row["idle_secs"] = jb[name].get("idle_secs")
        row["age_secs"] = jb[name].get("age_secs")
        row["ttl_left"] = jb[name].get("ttl_left")
        # `Δ要求` は両方の雪像に `/hosts` の行があるときだけ読める (`/hosts` は 1,000 件で切れる)
        if name in qa and name in qb:
            row["requests"] = qb[name]["requests"] - qa[name]["requests"]
        if hours > 0:
            row["refresh_per_hour"] = refr / hours
            if row["requests"] is not None:
                row["requests_per_hour"] = row["requests"] / hours
                if row["requests"]:
                    row["refreshes_per_request"] = refr / row["requests"]
        rows.append(row)
    judged = [r for r in rows if r["requests"] is not None]
    unused = sum(r["refreshes"] for r in judged if r["requests"] == 0)
    refr_judged = sum(r["refreshes"] for r in judged)
    return {
        "kind": "pair",
        "from": taken_at(a), "to": taken_at(b), "hours": hours,
        "from_path": a.get("_path"), "to_path": b.get("_path"),
        "dns_truncated": dns_truncated(a) or dns_truncated(b),
        "hosts_truncated": hosts_truncated(a) or hosts_truncated(b),
        "refreshes_total": total,
        "refreshes_judged": refr_judged,
        "coverage": (refr_judged / total) if total > 0 else None,
        "names": len(rows),
        "unused": unused,
        "unused_share": (unused / refr_judged) if refr_judged else None,
        "unused_share_total": (unused / total) if total else None,
        "rows": rows,
    }


def since_start_window(snap):
    """雪像 1 枚から、起動からの平均を出す (要求は通算のまま。`requests_per_hour` は出さない)。"""
    hours = uptime(snap) / 3600.0
    rows = []
    for name, e in dns_entries(snap).items():
        row = empty_row(name)
        row["refreshes"] = e.get("refreshes") or 0
        row["idle_secs"] = e.get("idle_secs")
        row["age_secs"] = e.get("age_secs")
        row["ttl_left"] = e.get("ttl_left")
        if hours > 0:
            row["refresh_per_hour"] = row["refreshes"] / hours
        rows.append(row)
    req = requests_by_name(snap)
    for row in rows:
        r = req.get(row["name"])
        if r is not None:
            # **通算のまま**。起動の長さで割ると復元した分だけ水増しになる
            row["requests"] = r["requests"]
            # 絶対の時刻。復元した分を含むので表には出さない (`--json` でだけ読める)
            row["last_seen"] = r["last_seen"]
    meta = dns_meta(snap)
    warm = meta.get("warm") or 0
    ttl = meta.get("ttl_secs") or dr.TTL
    k = dr.refresh_after(ttl)
    return {
        "kind": "since_start",
        "from": taken_at(snap) - uptime(snap), "to": taken_at(snap), "hours": hours,
        "from_path": snap.get("_path"), "to_path": snap.get("_path"),
        "dns_truncated": dns_truncated(snap),
        "hosts_truncated": hosts_truncated(snap),
        "refreshes_total": meta.get("refreshes") or 0,
        "refreshes_judged": sum(r["refreshes"] for r in rows),
        "coverage": None,
        "names": len(rows),
        "warm": warm,
        "warm_secs": meta.get("warm_secs"),
        "ttl_secs": ttl,
        "k_secs": k,
        "unused": None,
        "unused_share": None,
        "rows": rows,
    }


def windows(snaps):
    """雪像の並びから窓を作る (隣り合う 2 枚が同じプロセスなら pair、そうでなければ since_start)。"""
    out = []
    for a, b in zip(snaps, snaps[1:]):
        ok, why = same_process(a, b)
        win = pair_window(a, b) if ok else since_start_window(b)
        if ok and win["refreshes_total"] < 0:
            # 起動は変わっていないのに引き直しの合計が減った = 数え直された。差は読めない
            win = since_start_window(b)
            win["restart_reason"] = "the refresh counter went backwards (restart)"
        elif not ok:
            win["restart_reason"] = why
        out.append(win)
    if len(snaps) == 1:
        out.append(since_start_window(snaps[0]))
    elif snaps:
        # 最後の 1 枚も単独で置く (起動からの平均は、その起動の全部を写している)。
        # ただし直前の窓が「起動が変わった」でその 1 枚から始まっているなら、同じ窓が 2 度になる
        last = snaps[-1]
        if not out or out[-1].get("to") != taken_at(last) or out[-1]["kind"] == "pair":
            out.append(since_start_window(last))
    return out


def verdict(win, model_share=None, model_warm=None):
    """窓 1 つぶんの判定 (段 2 に進むか)。読めない窓は `None`。

    「使われていない引き直し」の割合は 3 通りでしか読めない。強い順に:
      1. `timer` … 模型の `refresh_warm` (タイマーで起きた引き直し) の割合。**これが本当の値**
      2. `delta` … 同じプロセスの 2 枚の差で `Δ要求` が 0 の名前の `Δ引き直し`。**下限**
         (窓の中で 1 回でも使われた名前の、使われていない引き直しは数えられない)
      3. `none`  … keep-warm が 0 なら、タイマーの引き直しは構造上 0 回 (これは正確)
    """
    total = win.get("refreshes_total") or 0
    if win["kind"] == "pair":
        if win["unused_share"] is None or total <= 0:
            return None
        if (win["coverage"] or 0) < 0.5:
            # 表が切れていて、引き直しの半分以上が名前ごとに読めない
            return None
        return {"share": win["unused_share"], "basis": "delta",
                "go": win["unused_share"] >= IDLE_SHARE}
    # since_start
    if model_share is not None and (model_warm or 0) > 0:
        return {"share": model_share, "basis": "timer", "go": model_share >= IDLE_SHARE}
    if win.get("warm_secs") in (0, "0", None) and not win.get("warm"):
        # keep-warm が 0: 引き直しは全部、要求の経路 (使われた名前) で起きる
        return {"share": 0.0, "basis": "none", "go": False}
    # warm が動いている版で雪像 1 枚: タイマーの回数は今の件数しか分からない
    return None


# ------------------------------------------------------------------ 模型 (dns-replay.py の Table)

def model(snap, names=()):
    """同じ雪像の `/recent` の到着列で、k を変えて引き直しを数える。

    `k` は `refresh_after(ttl)` = 3/4 TTL (いまの規則) と `ttl` (= TTL まで待つ) の 2 通り。
    名前ごとの回/時と、引き直しの内訳 (warm = タイマー / 当たりの先回り / 古い答え) を返す。
    """
    meta = dns_meta(snap)
    ttl = int(meta.get("ttl_secs") or dr.TTL)
    warm = int(meta.get("warm_secs") or 0)
    k = int(dr.refresh_after(ttl))
    warmup, inside, c0, taken, seen = dr.build(snap)
    hours = (taken - c0) / 3600.0
    arrivals = {}
    for _, h in inside:
        arrivals[h] = arrivals.get(h, 0) + 1
    out = {"window": {"from": c0, "to": taken, "hours": hours, "arrivals": len(inside),
                      "ttl_secs": ttl, "k_secs": k, "warm_secs": warm,
                      "serve_stale_secs": int(SERVE_STALE)},
           "rules": {}}
    for label, ks in (("k", k), ("ttl", ttl)):
        c = dr.replay(warmup, inside, c0, taken, dr.promote_current, seen,
                      ttl=ttl, window=warm, serve_stale=SERVE_STALE, refresh_at=ks)
        per = c.get("per_name") or {}
        names_out = {}
        for name, n in per.items():
            names_out[name] = {"refreshes": n, "per_hour": n / hours if hours else None,
                               "arrivals": arrivals.get(name, 0)}
        out["rules"][label] = {
            "refresh_at": ks, "refreshes": c["refreshes"],
            "per_hour": c["refresh_per_hour"],
            "warm": c.get("refresh_warm"), "ask": c.get("refresh_ask"),
            "stale": c.get("refresh_stale"),
            "warm_share": (c.get("refresh_warm") / c["refreshes"]) if c["refreshes"] else None,
            "names": names_out,
        }
    out["compare"] = model_compare(out, names)
    return out


def model_compare(out, names=()):
    """観測 (`/dns` の回/時) と模型 (k / ttl) を名前ごとに並べ、どちらが効いているかを読む。

    比べるのは **到着が `MODEL_MIN_ARRIVALS` 以上ある名前のうち、模型の引き直しが多い上位
    `MODEL_TOP` 件**。到着の少ない名前は 1 回の当たり外れで決まるので入れない。
    """
    hours = out["window"]["hours"]
    obs = {n: v for n, v in (names or ())}
    if not hours or not obs:
        return None
    got = out["rules"]["k"]["names"]
    picked = [n for n, v in sorted(got.items(), key=lambda kv: -kv[1]["refreshes"])
              if v["arrivals"] >= MODEL_MIN_ARRIVALS and n in obs][:MODEL_TOP]
    if not picked:
        return None
    rows = []
    for n in picked:
        rows.append({"name": n,
                     "observed_per_hour": obs[n],
                     "k_per_hour": got[n]["per_hour"],
                     "ttl_per_hour": out["rules"]["ttl"]["names"][n]["per_hour"],
                     "arrivals_per_hour": got[n]["arrivals"] / hours,
                     "interval_observed": (3600.0 / obs[n]) if obs[n] else None,
                     "interval_k": (3600.0 / got[n]["per_hour"]) if got[n]["per_hour"] else None,
                     "interval_ttl": (3600.0 / out["rules"]["ttl"]["names"][n]["per_hour"])
                     if out["rules"]["ttl"]["names"][n]["per_hour"] else None})
    so = sum(r["observed_per_hour"] for r in rows)
    sk = sum(r["k_per_hour"] for r in rows)
    st = sum(r["ttl_per_hour"] for r in rows)
    if out["window"]["warm_secs"]:
        # keep-warm が動いていると、引き直しはタイマー (45 秒ごと) が決める。
        # 使われ方から出る k と ttl の差は、タイマーの回数に埋もれて読めない
        pick = "warm"
    else:
        pick = "k" if abs(sk - so) < abs(st - so) else "ttl"
    return {"names": rows, "observed_sum": so, "k_sum": sk, "ttl_sum": st, "pick": pick,
            "k_error": (sk - so), "ttl_error": (st - so)}


# ------------------------------------------------------------------ 出す

def fmt(v, n=1):
    return "—" if v is None or v is True or v is False else ("%%.%df" % n) % v


def signed(v):
    """差を符号つきで書く (−0.0 を出さない)。"""
    return "%+.1f" % (v if abs(v) >= 0.05 else 0.0)


def pct(v):
    return "—" if v is None else "%.1f%%" % (v * 100)


def stamp(t):
    """epoch 秒を UTC の ISO 8601 にする (雪像の名前と同じ形)。"""
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t or 0))


def render(out):
    lines = []
    for s in out["snapshots"]:
        lines.append("雪像 %s: %s (起動から %.2f 時間、版 %s、`/dns` %s 件%s、`/hosts`%s)"
                     % (s["path"], stamp(s["taken_at"]), s["uptime_secs"] / 3600.0,
                        s["version"], s["dns_entries"],
                        " (切れている)" if s["dns_truncated"] else "",
                        " (切れている)" if s["hosts_truncated"] else ""))
    for w in out["windows"]:
        lines.append("")
        if w.get("restart_reason"):
            lines.append("(起動が変わったので前の雪像とは比べない: %s)" % w["restart_reason"])
        if w["kind"] == "pair":
            lines.append("## 窓 %s → %s (%.2f 時間、同じプロセス)%s"
                         % (w["from_path"], w["to_path"], w["hours"],
                            "" if not w["dns_truncated"] else " — `/dns` が切れている"))
            lines.append("引き直し %d 回 (%.1f 回/時)、名前ごとに読めた分 %d 回 (被覆 %s)"
                         % (w["refreshes_total"],
                            w["refreshes_total"] / w["hours"] if w["hours"] else 0,
                            w["refreshes_judged"], pct(w["coverage"])))
            lines.append("| 名前 | 引き直し | 回/時 | 要求 | 回/時 | 引き直し/要求 | "
                         "idle 秒 (窓の終わり) |")
            lines.append("|---|---|---|---|---|---|---|")
            for r in top_rows(w["rows"], out["top"]):
                lines.append("| %s | %d | %s | %s | %s | %s | %s |"
                             % (r["name"], r["refreshes"], fmt(r["refresh_per_hour"]),
                                "—" if r["requests"] is None else r["requests"],
                                fmt(r["requests_per_hour"]), fmt(r["refreshes_per_request"], 2),
                                r["idle_secs"]))
            if w["unused_share"] is None:
                lines.append("使われていない引き直し: 判定できない (差が読める名前が無い)")
            else:
                lines.append("**使われていない引き直し** (`Δ要求` が 0 の名前): %d / %d 回 = **%s** "
                             "(窓の全引き直し %d 回に対しては %s)"
                             % (w["unused"], w["refreshes_judged"], pct(w["unused_share"]),
                                w["refreshes_total"], pct(w["unused_share_total"])))
        else:
            lines.append("## 起動から %s (%.2f 時間、雪像 1 枚)%s"
                         % (w["to_path"], w["hours"],
                            "" if not w["dns_truncated"] else " — `/dns` が切れている"))
            lines.append("引き直し %d 回 (%.1f 回/時)、表の名前 %d 件、warm %s 件 "
                         "(keep-warm の窓 %s 秒、TTL %s 秒、k %s 秒)"
                         % (w["refreshes_total"],
                            w["refreshes_total"] / w["hours"] if w["hours"] else 0,
                            w["names"], w["warm"], fmt(w["warm_secs"], 0), w["ttl_secs"],
                            int(w["k_secs"])))
            lines.append("| 名前 | 引き直し | 回/時 | 要求 (通算) | idle 秒 |")
            lines.append("|---|---|---|---|---|")
            for r in top_rows(w["rows"], out["top"]):
                lines.append("| %s | %d | %s | %s | %s |"
                             % (r["name"], r["refreshes"], fmt(r["refresh_per_hour"]),
                                "—" if r["requests"] is None else format(r["requests"], ","),
                                r["idle_secs"]))
            lines.append("要求は `/hosts` の**通算** (状態ファイルから復元した分を含む) なので "
                         "回/時には直さない。この窓の中の使用は `/dns` の `idle_secs` "
                         "(この起動の中の最後の使用からの秒) で読む")
            if w["warm_secs"] in (0, "0"):
                lines.append("keep-warm が 0 なので、タイマーで起きる引き直しは 0 回"
                             " (引き直しは全部、要求の経路 = 使われた名前)")
    m = out.get("model")
    if m:
        w = m["window"]
        lines.append("")
        lines.append("## 模型 (`scripts/dns-replay.py` の `Table`、窓 %.2f 時間、到着 %d 件、"
                     "TTL %d 秒 / warm %d 秒 / serve-stale %d 秒)"
                     % (w["hours"], w["arrivals"], w["ttl_secs"], w["warm_secs"],
                        w["serve_stale_secs"]))
        lines.append("| 規則 | 引き直し | 回/時 | うち warm (タイマー) / 先回り / 古い答え | "
                     "タイマーの割合 |")
        lines.append("|---|---|---|---|---|")
        for label in ("k", "ttl"):
            r = m["rules"][label]
            name = "k=%d (いまの規則)" % r["refresh_at"] if label == "k" else \
                "ttl=%d (先回りなし)" % r["refresh_at"]
            lines.append("| %s | %d | %.1f | %d / %d / %d | %s |"
                         % (name, r["refreshes"], r["per_hour"], r["warm"],
                            r["ask"], r["stale"], pct(r["warm_share"])))
        c = m.get("compare")
        if c:
            lines.append("")
            lines.append("| 名前 | 観測 回/時 | 模型 k=%d | 模型 ttl=%d | 到着 回/時 | "
                         "間隔 観測 / k / ttl 秒 |"
                         % (m["rules"]["k"]["refresh_at"], m["rules"]["ttl"]["refresh_at"]))
            lines.append("|---|---|---|---|---|---|")
            for r in c["names"]:
                lines.append("| %s | %.1f | %.1f | %.1f | %.1f | %.0f / %.0f / %.0f |"
                             % (r["name"], r["observed_per_hour"], r["k_per_hour"],
                                r["ttl_per_hour"], r["arrivals_per_hour"],
                                r["interval_observed"] or 0, r["interval_k"] or 0,
                                r["interval_ttl"] or 0))
            lines.append("上位 %d 名前の合計: 観測 %.1f 回/時、模型 k=%d は %.1f 回/時 (%s)、"
                         "ttl=%d は %.1f 回/時 (%s) → **%s が効いている**"
                         % (len(c["names"]), c["observed_sum"],
                            m["rules"]["k"]["refresh_at"], c["k_sum"], signed(c["k_error"]),
                            m["rules"]["ttl"]["refresh_at"], c["ttl_sum"],
                            signed(c["ttl_error"]),
                            {"k": "k (3/4 TTL の先回り)", "ttl": "TTL",
                             "warm": "keep-warm (タイマー)"}[c["pick"]]))
            if c["pick"] == "warm":
                lines.append("(keep-warm が動いているので、引き直しの間隔はタイマーの 45 秒が決める。"
                             "k と ttl の差はタイマーの回数に埋もれて読めない)")
    v = out.get("verdict") or {}
    lines.append("")
    lines.append("## 判定 (T.24.1 の目安: 使われていない名前の引き直しが全体の %d%% 以上なら段 2)"
                 % int(IDLE_SHARE * 100))
    newest = None
    for w in out["windows"]:
        d = v.get(str(w["from"]) + "-" + str(w["to"]))
        head = ("%s → %s" % (w["from_path"], w["to_path"])) if w["kind"] == "pair" \
            else "%s (起動から %.2f 時間)" % (w["to_path"], w["hours"])
        if d is None:
            lines.append("- %s: 判定できない" % head)
            continue
        basis = {"timer": "模型のタイマーの割合", "delta": "Δ要求が 0 の名前 (下限)",
                 "none": "keep-warm が 0 なので構造上 0"}[d["basis"]]
        lines.append("- %s: %s (%s) → %s"
                     % (head, pct(d["share"]), basis,
                        "**段 2 に進む**" if d["go"] else "段 2 は要らない"))
        newest = (head, d)
    if newest:
        lines.append("")
        lines.append("**段 2 の判断** (いちばん新しい、判定できる窓 = %s): %s → %s"
                     % (newest[0], pct(newest[1]["share"]),
                        "段 2 に進む" if newest[1]["go"] else "段 2 は要らない"))
    return "\n".join(lines)


def top_rows(rows, n):
    return sorted(rows, key=lambda r: (-(r["refreshes"] or 0), r["name"]))[:n]


def run(paths, with_model=False, top=TOP):
    snaps = []
    for p in paths:
        with open(p, encoding="utf-8") as f:
            snap = json.load(f)
        snap["_path"] = os.path.basename(p)
        snaps.append(snap)
    snaps.sort(key=taken_at)
    out = {"snapshots": [{"path": s["_path"], "taken_at": taken_at(s),
                          "uptime_secs": uptime(s), "version": s.get("version"),
                          "dns_entries": (s.get("dns") or {}).get("count"),
                          "dns_truncated": dns_truncated(s),
                          "hosts_truncated": hosts_truncated(s),
                          "warm_secs": dns_meta(s).get("warm_secs"),
                          "ttl_secs": dns_meta(s).get("ttl_secs"),
                          "refreshes": dns_meta(s).get("refreshes")} for s in snaps],
           "windows": windows(snaps), "top": top}
    out["verdict"] = {}
    model_share = model_warm = None
    if with_model:
        last = snaps[-1]
        # 観測は `/dns` の名前ごとの回/時 (起動からの平均)
        hours = uptime(last) / 3600.0
        obs = {n: ((e.get("refreshes") or 0) / hours) for n, e in dns_entries(last).items()
               if hours > 0 and (e.get("refreshes") or 0) > 0}
        out["model"] = model(last, sorted(obs.items(), key=lambda kv: -kv[1]))
        # 模型の窓 (直近) は、いちばん新しい雪像の起動についての値
        model_share = out["model"]["rules"]["k"]["warm_share"]
        model_warm = out["model"]["window"]["warm_secs"]
    for w in out["windows"]:
        key = str(w["from"]) + "-" + str(w["to"])
        # 模型の値は、いちばん新しい雪像の起動についてだけ使う
        newest = w["to"] == taken_at(snaps[-1]) and w.get("to_path") == snaps[-1]["_path"]
        out["verdict"][key] = verdict(w, model_share if newest else None,
                                      model_warm if newest else None)
    return out


def main(argv=None):
    p = argparse.ArgumentParser(
        description="list refreshes vs uses per DNS name and count refreshes of unused names")
    p.add_argument("snapshot", nargs="+")
    p.add_argument("--model", action="store_true",
                   help="also replay the refresh rule (k = 3/4 TTL vs TTL) over /recent")
    p.add_argument("--top", type=int, default=TOP, help="rows per table (default %d)" % TOP)
    p.add_argument("--json", action="store_true")
    a = p.parse_args(argv)
    if a.top <= 0:
        print("dns-idle: --top must be positive", file=sys.stderr)
        return 2
    try:
        out = run(a.snapshot, with_model=a.model, top=a.top)
    except (ValueError, OSError) as e:
        print("dns-idle: %s" % e, file=sys.stderr)
        return 2
    print(json.dumps(out, indent=1, ensure_ascii=False) if a.json else render(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
