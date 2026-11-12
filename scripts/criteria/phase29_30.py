# 判定表 (`scripts/snapshot_criteria.py`) の **Phase 29〜30 の行** (T34.129 で割った)。
#
# phase29 で名前解決の失敗の割合で動かない形にした 6 行と、phase30 で使われ方で動かない形にした 2 行と、
# その材料を読む小物 (`/history` の部・`/profile` の窓と役割・名前解決の失敗の数・`dns_slow` の窓・
# 答えのある名前)。並べるのは `RULES` (`criteria/rules.py`)。

from proxydata import CAUSE_NAMES

from .common import MET, MISSED, UNKNOWN, n, part
from .phase14_18 import profile_source
from .phase20_22 import counter_rates, display_only, dns_counter, profile_span
from .phase23_25 import _p23_miss_band_measured
from .phase26_28 import _p27_connect_errors, _p28_cgroup_cpu_per_request


# --- Phase 29 (T32.16・T32.29・T32.30。phase28 の 26 行のうち 5 行を、名前解決の失敗の割合で動かない形にしたもの) ---
#
# T31.99 (0.30.4 の 48 時間) で、名前解決に失敗した要求 (ほとんどが負のキャッシュで即 502) の割合が
# 前 40% → 後 66% に増え、要求あたりの行がその混ざり方で動いた。接続のエラーの行は `/recent` の末尾
# 4.3 時間 (48.3 時間の 9%) だけで読んでいて、起動からの `/history` では逆の結論だった。`dns_slow` の行は
# コードが T22.2 から合計 1,000 ms を条件にしているので構造上落ちず、名前解決の失敗の遅さで鳴っていた。

def history_part(snap, res):
    """雪像の `/history?res=…` の部 (`{"keys", "samples", "causes"}`。無ければ None)。"""
    h = (snap.get("history") or {}).get(str(res))
    if not isinstance(h, dict) or not isinstance(h.get("keys"), list) or not isinstance(h.get("samples"), list):
        return None
    return h


def profile_window(p):
    """`/profile?res=60` の材料の `(最初の t, 最後の t)`。`t` の欄が無ければ None。"""
    keys = p.get("keys") or []
    rows = p.get("samples") or []
    if "t" not in keys or not rows:
        return None
    ti = keys.index("t")
    ts = [r[ti] for r in rows if ti < len(r) and isinstance(r[ti], (int, float))]
    return (min(ts), max(ts)) if ts else None


def _dns_failures_in(h, t0, t1):
    """`/history` の部 1 つの `errors_by_cause.dns` を、窓 `[t0, t1)` に重なる割合で按分して足す (欄が無ければ None)。"""
    if h is None or "errors_by_cause" not in h["keys"]:
        return None
    causes = h.get("causes") or list(CAUSE_NAMES)
    if "dns" not in causes:
        return None
    ei, di = h["keys"].index("errors_by_cause"), causes.index("dns")
    res = h.get("interval_secs") or 3600
    total = 0.0
    for s in h["samples"]:
        if not s or not isinstance(s[0], (int, float)):
            continue
        share = (min(s[0] + res, t1) - max(s[0], t0)) / res
        by = s[ei] if ei < len(s) else None
        if share > 0 and isinstance(by, list) and di < len(by):
            total += (by[di] or 0) * min(share, 1.0)
    return total


def dns_failures_between(snap, t0, t1):
    """窓 `[t0, t1)` の名前解決の失敗 (`/history` の `errors_by_cause.dns`) の数 (無ければ None)。

    `/history?res=60` (1 日) が覆うところは 1 分の標本で、それより古いところは `res=3600` の標本を窓に重なる
    割合で按分して数える (`/profile` の窓は分の単位で時間の境目に揃わず、雪像を撮った時刻によっては端の 1 時間が
    まだ無い)。
    """
    h60, h3600 = history_part(snap, 60), history_part(snap, 3600)
    first60 = next((s[0] for s in (h60 or {}).get("samples") or [] if s and isinstance(s[0], (int, float))), None)
    if h60 is not None and "errors_by_cause" in h60["keys"] and first60 is not None and first60 < t1:
        cut = max(t0, first60)
        recent = _dns_failures_in(h60, cut, t1)
        older = _dns_failures_in(h3600, t0, cut) if cut > t0 else 0.0
        if recent is not None and older is not None:
            return round(recent + older)
    v = _dns_failures_in(h3600, t0, t1)
    return None if v is None else round(v)


def profile_roles_us(p):
    """`/profile` の JSON 1 つの `({役割: cpu_us の和}, 全体の cpu_us の和, 要求の和, 秒)`。欄が無ければ None。"""
    keys = p.get("keys") or []
    roles = p.get("roles") or []
    rows = p.get("samples") or []
    interval = p.get("interval_secs") or 0
    if not rows or not interval or not all(k in keys for k in ("threads", "requests", "cpu_us")):
        return None
    ti, qi, ci = keys.index("threads"), keys.index("requests"), keys.index("cpu_us")
    by = {r: 0 for r in roles}
    cpu = req = 0
    for r in rows:
        threads = r[ti] if ti < len(r) else None
        for i, role in enumerate(roles):
            t = threads[i] if threads and i < len(threads) else None
            if t:
                by[role] += t[0] or 0
        cpu += (r[ci] if ci < len(r) else 0) or 0
        req += (r[qi] if qi < len(r) else 0) or 0
    return by, cpu, req, len(rows) * interval


def _p29_profile_sides(c, th):
    """前と後の `/profile` の材料 (`[(誰, 雪像, p, 出どころ)]`) か、判定できない理由。

    phase21 以降の「参考」(材料が 12 時間に満たない) は使わず、満たなければ「判定できず」にする (T.32.29。
    `--closeout` が前の版の最後の `-profile_res_60.json` (同じ版で 12 時間以上) を前の材料に選ぶ)。
    """
    hours = th["profile_min_hours"]
    out = []
    for who, snap in (("前", c["a"]), ("後", c["b"])):
        p, src = profile_source(snap)
        if not p:
            return None, f"{who}の雪像に `/profile` の材料が無い (`--profile` / `--profile-before`)"
        span = profile_span(snap)
        if span[0] < hours * 3600:
            return None, (f"{who}の `/profile` が {span[0] / 3600:,.1f} 時間 ({n(span[1])} 標本) で {hours:.0f} 時間に"
                          "満たない (`--closeout` は前の版の最後の 12 時間以上の `-profile_res_60.json` を選ぶ)")
        out.append((who, snap, p, src))
    return out, None


def _p29_conn_per_resolved_request(c, th):
    """T.32.29 (b): `conn` 役の CPU ÷ **名前解決に成功した要求**が前の版の `conn_per_resolved_factor` 倍未満。

    名前解決に失敗した要求は負のキャッシュで即 502 になり `conn` の CPU をほとんど使わないので、要求の全数で
    割ると失敗の割合 (T.31.99 で 40% → 66%) で動いた。失敗の数は、`/profile` の窓 (最初と最後の `t`) に入る
    `/history?res=3600` の `errors_by_cause` の `dns`。要求あたり (全数) は表示だけ。
    """
    k = th["conn_per_resolved_factor"]
    label = (f"`conn` 役の CPU ÷ **名前解決に成功した要求**が前の版の {k} 倍未満 (T.32.29。要求の全数で割った値は表示だけ)")
    limit = f"< 前 ×{k}"
    sides, why = _p29_profile_sides(c, th)
    if sides is None:
        return (label, limit, "—", UNKNOWN, why)
    got, srcs = {}, []
    for who, snap, p, src in sides:
        v = profile_roles_us(p)
        win = profile_window(p)
        if v is None or win is None or "conn" not in v[0]:
            return (label, limit, "—", UNKNOWN, f"{who}の `/profile` に `t`・`threads`・`requests`・`cpu_us` か `conn` の役割が無い")
        fails = dns_failures_between(snap, win[0], win[1] + (p.get("interval_secs") or 60))
        if fails is None:
            return (label, limit, "—", UNKNOWN,
                    f"{who}の雪像の `/history?res=3600` に `errors_by_cause` (の `dns`) が無い。名前解決の失敗が数えられない")
        by, _, req, secs = v
        ok = req - fails
        if ok <= 0:
            return (label, limit, "—", UNKNOWN, f"{who}の `/profile` の窓で名前解決に成功した要求が 0 本")
        got[who] = {"us": by["conn"] / ok, "all": by["conn"] / req if req else None, "req": req, "fail": fails}
        srcs.append(f"{who}: {src} {secs / 3600:,.1f} 時間、要求 {n(req)} 本のうち名前解決の失敗 {n(fails)} 本 "
                    f"({fails / req * 100 if req else 0:.0f}%)")
    a, b = got["後"], got["前"]
    r = a["us"] / b["us"] if b["us"] else None
    if r is None:
        return (label, limit, "—", UNKNOWN, "。".join(srcs) + "。前の `conn` 役の CPU が 0")
    shown = (f"**{b['us']:,.0f} → {a['us']:,.0f}** us/名前解決に成功した要求 (**{r:.2f}** 倍)。"
             f"表示だけ: 要求の全数あたり {b['all']:,.0f} → {a['all']:,.0f} us")
    return (label, limit, shown, MET if r < k else MISSED,
            "`/profile` の `conn` の `cpu_us` ÷ (要求 − `/history?res=3600` の `errors_by_cause.dns`)。" + "。".join(srcs))


def _p29_idle_cost(c, th):
    """T.32.29 (a): 暇なときの費用 = 裏の役割 (`conn`・`accept` 以外) の CPU (us/秒) が前の版の `idle_cost_factor` 倍未満。

    裏の役割は要求の量でほとんど動かない (T.31.99 の材料で 890 → 894 us/秒) ので、割合 (phase26 の行) ではなく
    1 秒あたりの量で比べる。`profile-sample`・`cache-probe` の割合は表示だけ。
    """
    k = th["idle_cost_factor"]
    label = (f"暇なときの費用: 裏の役割 (`conn`・`accept` 以外) の CPU が前の版の {k} 倍未満 (us/秒。T.32.29。"
             "`profile-sample`・`cache-probe` の割合は表示だけ)")
    limit = f"< 前 ×{k}"
    sides, why = _p29_profile_sides(c, th)
    if sides is None:
        return (label, limit, "—", UNKNOWN, why)
    got, shares, srcs = {}, {}, []
    for who, snap, p, src in sides:
        v = profile_roles_us(p)
        if v is None or not v[3]:
            return (label, limit, "—", UNKNOWN, f"{who}の `/profile` に `threads`・`requests`・`cpu_us` が無い")
        by, cpu, _, secs = v
        got[who] = (cpu - by.get("conn", 0) - by.get("accept", 0)) / secs
        shares[who] = {role: (by[role] / cpu if cpu and role in by else None) for role in th["idle_roles"]}
        srcs.append(f"{who}: {src} {secs / 3600:,.1f} 時間")
    a, b = got["後"], got["前"]
    if not b:
        return (label, limit, "—", UNKNOWN, "。".join(srcs) + "。前の裏の役割の CPU が 0")
    r = a / b
    parts = []
    for role in th["idle_roles"]:
        sb, sa = shares["前"][role], shares["後"][role]
        if sa is not None and sb is not None:
            parts.append(f"`{role}` {sb * 100:.1f}% → {sa * 100:.1f}%")
    shown = f"**{b:,.0f} → {a:,.0f}** us/秒 (**{r:.2f}** 倍)"
    if parts:
        shown += "。表示だけ: " + "・".join(parts)
    return (label, limit, shown, MET if r < k else MISSED,
            "`/profile` の全体の `cpu_us` − `conn` − `accept` を秒で割ったもの。" + "。".join(srcs))


def history_since_start(snap, res, keys):
    """起動からの `/history?res=…` の標本 (起動を含む区間から) の `keys` の和 (`{名前: 和}`)。欄が無ければ None。

    `errors_by_cause` は原因ごとの和の並び。区間 `[t, t + res)` が起動より後に終わる標本を数える
    (起動の時間の標本は前のプロセスのぶんも含みうるが、T.32.30 の数え方と同じ)。
    """
    h = history_part(snap, res)
    up = snap.get("uptime_secs")
    taken = snap.get("taken_at")
    if h is None or not isinstance(up, (int, float)) or not isinstance(taken, (int, float)):
        return None
    hk = h["keys"]
    if any(k not in hk for k in keys):
        return None
    boot = taken - up
    out = {k: ([0] * len(h.get("causes") or CAUSE_NAMES) if k == "errors_by_cause" else 0) for k in keys}
    hours = 0
    for s in h["samples"]:
        if not s or not isinstance(s[0], (int, float)) or s[0] + res <= boot:
            continue
        hours += 1
        for k in keys:
            v = s[hk.index(k)] if hk.index(k) < len(s) else None
            if k == "errors_by_cause":
                for i, x in enumerate(v or []):
                    if i < len(out[k]):
                        out[k][i] += x or 0
            else:
                out[k] += v or 0
    out["_samples"] = hours
    out["_causes"] = list(h.get("causes") or CAUSE_NAMES)
    return out


def _p29_connect_errors(c, th):
    """T.32.30: 接続のエラーを、**起動からの `/history?res=3600` の 443・80 だけ**で読む。

    率 = (エラー − 名前解決の失敗 − `other_port_errors`) ÷ (確立 + 同じエラー − `other_port_attempts`)。
    phase27 の行は `/recent` の末尾 (48.3 時間のうち 4.3 時間) だけで 0.20 倍の「満たした」だったが、同じ定義を
    起動からの `/history` で数えると 2.1 倍で逆の結論で、中身は 443・80 以外 (メール宛) だった。前の版に
    `other_port_*` の欄が無ければ「判定できず」(理由を書く)。`/recent` の窓の値 (phase27 の行) は表示だけ。
    """
    k = th["connect_error_factor"]
    label = (f"接続のエラー (443・80 宛て、名前解決の失敗を除く) ÷ 試行 が前の版の {k} 倍未満 "
             "(起動からの `/history?res=3600`。T.32.30。`/recent` の窓の値は表示だけ)")
    limit = f"< 前 ×{k}"
    keys = ("errors", "errors_by_cause", "connects", "other_port_errors", "other_port_attempts")
    _, _, recent, _, _ = _p27_connect_errors(c, th)
    got, missing = {}, []
    for who, snap in (("前", c["a"]), ("後", c["b"])):
        v = history_since_start(snap, 3600, keys)
        if v is None:
            base = history_since_start(snap, 3600, keys[:3])
            missing.append(f"{who}の雪像の " + (
                "`/history?res=3600` に `other_port_errors`・`other_port_attempts` の欄が無い (T.30.3 より前の版)"
                if base is not None else "`/history?res=3600` の部か `errors`・`connects` の欄、`uptime_secs` が無い"))
            continue
        causes = v["_causes"]
        dns = v["errors_by_cause"][causes.index("dns")] if "dns" in causes else 0
        errs = v["errors"] - dns
        got[who] = (errs - v["other_port_errors"], v["connects"] + errs - v["other_port_attempts"], v["_samples"])

    def text(who, bold):
        num, den, _ = got[who]
        star = "**" if bold else ""
        return f"{star}{num / den:.5f}{star} ({n(num)} / {n(den)})" if den else "— (試行 0)"
    src = ("`/history?res=3600` の起動からの `errors` − `errors_by_cause.dns` − `other_port_errors` ÷ "
           "(`connects` + 同じエラー − `other_port_attempts`)")
    tail = f"。表示だけ (`/recent` の窓): {recent}"
    if missing:
        shown = (f"後 {text('後', True)}" if "後" in got else "—") + tail
        return (label, limit, shown, UNKNOWN, "。".join(missing) + "。" + src)
    (na, da, ha), (nb, db, hb) = got["後"], got["前"]
    if not da or not db:
        side = "後" if not da else "前"
        return (label, limit, f"{text('前', False)} → {text('後', True)}" + tail, UNKNOWN, f"{side}の 443・80 の試行が 0")
    ra, rb = na / da, nb / db
    shown = f"{text('前', False)} → {text('後', True)}" + tail
    src += f" (前 {hb} 時間・後 {ha} 時間)"
    if rb == 0:
        verdict = MET if ra == 0 else MISSED
    else:
        verdict = MET if ra < rb * k else MISSED
    return (label, limit, shown, verdict, src)


def _p29_dns_slow(c, th):
    """T.32.16: `dns_slow` のうち、**名前解決に成功したミス**で立った件/時。

    `/events` の `dns_slow` (起動から) の 1 件ずつについて、立った時刻までの 5 分の `/history?res=60` の標本で
    名前解決に成功したミス (`dns_misses − dns_fail_misses`) が `dns_min_misses` 回以上で、その待ちの合計
    (`dns_ms_sum − dns_fail_ms_sum`) が `dns_slow_min_sum_ms` 以上なら「成功したミスで立った」と数える。
    件/時が `events_per_hour["dns_slow"]` 以下なら満たした。名前解決の失敗の遅さで立った件は表示だけ。
    `dns_fail_ms_sum` の欄 (T.32.16 で標本に足す) が無い版は「判定できず」。`/history?res=60` (1 日) より古い
    出来事は数えず、その時間も分母から外す。
    """
    lim = th["events_per_hour"]["dns_slow"]
    min_sum, min_n = th["dns_slow_min_sum_ms"], th["dns_min_misses"]
    label = (f"`dns_slow` のうち名前解決に**成功した**ミスで立った件/時が {lim} 以下 (T.32.16。失敗の遅さで立った件は表示だけ)")
    limit = f"≤ {lim} 件/時"
    b = c["b"]
    ev = part(b, "events")
    rows = ev.get("events")
    if not isinstance(rows, list):
        return (label, limit, "—", UNKNOWN, "後の雪像に `/events` の部が無い")
    h = history_part(b, 60)
    need = ("dns_misses", "dns_fail_misses", "dns_ms_sum", "dns_fail_ms_sum")
    if h is None or any(k not in h["keys"] for k in need):
        missing = "部" if h is None else "・".join(f"`{k}`" for k in need if k not in h["keys"])
        return (label, limit, "—", UNKNOWN,
                f"後の雪像の `/history?res=60` に {missing} が無い (`dns_fail_ms_sum` は T.32.16 で足した欄)")
    hk = h["keys"]
    idx = {k: hk.index(k) for k in need}
    samples = [s for s in h["samples"] if s and isinstance(s[0], (int, float))]
    if not samples:
        return (label, limit, "—", UNKNOWN, "後の雪像の `/history?res=60` に標本が無い")
    up, taken = b.get("uptime_secs") or 0, b.get("taken_at") or 0
    start = max(taken - up, samples[0][0] + 300)
    hours = max(taken - start, 0) / 3600.0
    ok = fail = 0
    for e in rows:
        text = e.get("text") or ""
        at = e.get("at") or 0
        if e.get("kind") != "anomaly" or not text.startswith("dns_slow:") or at < start:
            continue
        w = [s for s in samples if at - 300 < s[0] + 60 <= at + 60]
        cnt = sum((s[idx["dns_misses"]] or 0) - (s[idx["dns_fail_misses"]] or 0) for s in w)
        tot = sum((s[idx["dns_ms_sum"]] or 0) - (s[idx["dns_fail_ms_sum"]] or 0) for s in w)
        if cnt >= min_n and tot >= min_sum:
            ok += 1
        else:
            fail += 1
    if not hours:
        return (label, limit, "—", UNKNOWN, "`/history?res=60` の窓と起動からの時間が重ならない")
    rate = ok / hours
    shown = f"**{rate:.2f}** 件/時 ({ok} 件 / {hours:,.1f} 時間)。表示だけ: 名前解決の失敗の遅さで立った {fail} 件"
    src = (f"`/events` の `dns_slow:` (起動から、`/history?res=60` の窓の {hours:,.1f} 時間) を、立つまでの 5 分の "
           f"`dns_misses − dns_fail_misses` ≥ {min_n} 回・`dns_ms_sum − dns_fail_ms_sum` ≥ {min_sum:,} ms で分けた")
    if ev.get("truncated"):
        src += "。**`/events` が切れている** (件数は下限)"
    return (label, limit, shown, MET if rate <= lim else MISSED, src)


_p29_cgroup_cpu_shown = display_only(
    _p28_cgroup_cpu_per_request, lambda th: "cgroup の起動からの CPU (コアの合計と要求 1 本あたり。T.32.29)",
    "要求あたりは名前解決に失敗した要求 (負のキャッシュで即 502) の割合で動く (T.31.99 で 40% → 66%)。"
    "判定は `conn` 役の名前解決に成功した要求あたりと、裏の役割の us/秒の 2 行で見る", was="phase28")


def _p29_miss_band_measured(c, th):
    """T.32.17: 平常時の名前解決のミスの行 (phase23 の行) に、**名前解決の失敗を除いた**回/接続を添える。

    phase23 までの回/接続は名前解決に失敗したミス (負のキャッシュの総当たり) も数えていて、T.31.99 は 0.95 回/接続
    (失敗を除けば 0.048)。`/history` の `dns_fail_misses` の無い版は添えない (今までどおり)。
    """
    label, limit, shown, verdict, src = _p23_miss_band_measured(c, th)
    parts = []
    for key, name in (("after", "平常時"), ("after_all", "バースト込み")):
        v = (c.get(key) or {}).get("dns_ok_per_connect")
        if v is not None:
            parts.append(f"{name} **{v:.3f}**" if key == "after" else f"{name} {v:.3f}")
    if not parts:
        return (label, limit, shown, verdict, src + "。`/history` に `dns_fail_misses` が無い (失敗を除いた率は出せない)")
    return (label, limit, f"{shown}。名前解決の失敗を除くと " + "・".join(parts) + " 回/接続 (T.32.17)",
            verdict, src + "、失敗を除いた率は (`dns_misses` − `dns_fail_misses`) ÷ `connects`")


_p29_miss_band = display_only(
    _p29_miss_band_measured,
    lambda th: "平常時の名前解決のミス (回/接続。名前解決の失敗を除いた率と `/recent` の試行あたりも並べる)",
    "宛先の数で動く使われ方の数字 (T20.99 は宛先 395 名前で 0.12、`cold` 408 回。10-20 は死んだ名前の"
    "総当たりで 0.0850)", was="phase20")


# --- Phase 30 (T33.1・T33.2。phase29 の 26 行のうち、T32.99 で物差しの側が届かなかった 2 行を使われ方で動かない形にしたもの) ---
#
# T32.99 (0.32.39 の 48.3 時間) で phase29 の `dns_slow` の行は 0.63 件/時 (15 件 / 23.9 時間、閾 0.1) の「届かず」。
# 15 件とも初めて引く名前 (ほとんどが 1 つの接続元のメールのホスト) の名前解決そのものが遅い窓で、件/時は
# 初めて引く名前の数 (使われ方) で動く。0.1 件/時は T17〜T23 の静かなころの値。1 名前あたりの引き直しの行は、
# 分母の `dns.entries` が表の上限と総当たりの濃さで 7,511 → 891 に動いて 6.27 倍の「届かず」だった (T33.2)。

def dns_slow_windows(snap, th):
    """`/events` の `dns_slow` が立った窓の、名前解決に**成功した**ミスの回数と待ちの合計 (T.33.1)。

    窓は phase29 の行 (`_p29_dns_slow`) と同じ「立つまでの 5 分の `/history?res=60` の標本」で、起動からの
    出来事のうち `/history?res=60` (1 日) の中のものだけ。重なった窓の標本は 1 回だけ数える。
    `dns_fail_ms_sum` (T.32.16) のある版は、phase29 と同じく成功したミスで立った窓だけを選び、待ちは確か (`exact`)。
    無い版 (0.30.4 まで) は立った窓 (失敗込みの閾で立った) を全部使い、1 分の標本ごとに、失敗の混ざらない 1 分は
    確か、混ざった 1 分は成功したミスの待ちを 0〜その 1 分の合計として**上下限** (`lo`・`hi`) で返す。
    `dns_fail_misses` も無ければ (T.32.17 より前) None。`web_dns_misses`・`web_dns_ms_sum` (T.33.3) のある版は、
    本体の `dns_slow` と同じく 443・80 宛ての成功したミスだけで窓を選び、回数と待ちもそれで数える (`web`)。返すのは
    `{"fired", "hours", "misses", "lo", "hi", "exact", "web", "truncated"}`。
    """
    ev = part(snap, "events")
    rows = ev.get("events")
    h = history_part(snap, 60)
    if not isinstance(rows, list) or h is None:
        return None
    hk = h["keys"]
    if any(k not in hk for k in ("dns_misses", "dns_fail_misses", "dns_ms_sum")):
        return None
    exact = "dns_fail_ms_sum" in hk
    web = exact and "web_dns_misses" in hk and "web_dns_ms_sum" in hk
    idx = {k: hk.index(k) for k in ("dns_misses", "dns_fail_misses", "dns_ms_sum", "dns_fail_ms_sum",
                                    "web_dns_misses", "web_dns_ms_sum") if k in hk}
    samples = [s for s in h["samples"] if s and isinstance(s[0], (int, float))]
    if not samples:
        return None
    up, taken = snap.get("uptime_secs") or 0, snap.get("taken_at") or 0
    start = max(taken - up, samples[0][0] + 300)

    def get(s, k):
        return (s[idx[k]] if idx[k] < len(s) else 0) or 0

    def resolved(s):
        """その 1 分の成功したミスの (回数, 待ち)。T.33.3 の列があれば 443・80 宛てだけ。"""
        if web:
            return get(s, "web_dns_misses"), get(s, "web_dns_ms_sum")
        return get(s, "dns_misses") - get(s, "dns_fail_misses"), get(s, "dns_ms_sum") - get(s, "dns_fail_ms_sum")
    used, fired = set(), 0
    for e in rows:
        text = e.get("text") or ""
        at = e.get("at") or 0
        if e.get("kind") != "anomaly" or not text.startswith("dns_slow:") or at < start:
            continue
        w = [i for i, s in enumerate(samples) if at - 300 < s[0] + 60 <= at + 60]
        if exact:
            cnt = sum(resolved(samples[i])[0] for i in w)
            tot = sum(resolved(samples[i])[1] for i in w)
            if cnt < th["dns_min_misses"] or tot < th["dns_slow_min_sum_ms"]:
                continue
        fired += 1
        used.update(w)
    misses = lo = hi = 0
    for i in used:
        s = samples[i]
        ok = resolved(s)[0] if exact else get(s, "dns_misses") - get(s, "dns_fail_misses")
        if ok <= 0:
            continue
        misses += ok
        if exact:
            v = resolved(s)[1]
            lo += v
            hi += v
        else:
            hi += get(s, "dns_ms_sum")
            if not get(s, "dns_fail_misses"):
                lo += get(s, "dns_ms_sum")
    return {"fired": fired, "hours": max(taken - start, 0) / 3600.0, "misses": misses,
            "lo": lo, "hi": hi, "exact": exact, "web": web, "truncated": bool(ev.get("truncated"))}


def _p30_dns_slow(c, th):
    """T.33.1: `dns_slow` が立った窓の、名前解決に成功したミスの平均が前の版の 1.5 倍未満。

    phase29 の行 (`_p29_dns_slow`) は成功したミスで立った件/時 ≤ 0.1 で、件/時は初めて引く名前の数 (使われ方) で
    動く (T.32.99 は 15 件 / 23.9 時間で 0.63)。平均はリゾルバの距離で決まり、使われ方で動きにくいので判定にし、
    件/時は表示だけにする。前の版に `dns_fail_ms_sum` が無ければ平均は上下限 (`dns_slow_windows`) で、
    上限でも閾を越えれば「届かず」、下限でも越えなければ「満たした」、間なら「判定できず」(上下限を書く)。
    後の期間に成功したミスで立った窓が無ければ満たした。前の版に立った窓が無ければ比べる平均が無いので判定できず。
    """
    k = th["dns_slow_avg_factor"]
    label = (f"`dns_slow` が立った窓の、名前解決に**成功した**ミスの平均が前の版の {k} 倍未満 "
             "(T.33.1。件/時は表示だけ)")
    limit = f"< 前 ×{k} (立った窓の成功したミスの平均)"
    aft, bef = dns_slow_windows(c["b"], th), dns_slow_windows(c["a"], th)
    if aft is None or not aft["exact"]:
        return (label, limit, "—", UNKNOWN,
                "後の雪像に `/events` か `/history?res=60` の `dns_fail_ms_sum` (T.32.16 で足した欄) が無い")
    if not aft["hours"]:
        return (label, limit, "—", UNKNOWN, "`/history?res=60` の窓と起動からの時間が重ならない")

    def rate(v, what):
        return f"{v['fired'] / v['hours']:.2f} 件/時 ({v['fired']} 件 / {v['hours']:,.1f} 時間、{what})"

    def avg(v):
        if not v["misses"]:
            return None
        if v["exact"]:
            return f"{v['lo'] / v['misses']:,.0f} ms ({n(v['misses'])} 回、立った {v['fired']} 件)"
        return (f"{v['lo'] / v['misses']:,.0f}〜{v['hi'] / v['misses']:,.0f} ms ({n(v['misses'])} 回、"
                f"立った {v['fired']} 件。`dns_fail_ms_sum` の無い版なので上下限)")
    src = ("`/events` の `dns_slow:` (起動から、`/history?res=60` の窓の中) が立つまでの 5 分の `/history?res=60` の"
           "標本を重ねずに足し、(`dns_ms_sum − dns_fail_ms_sum`) ÷ (`dns_misses − dns_fail_misses`)。後は phase29 と"
           f"同じく成功したミス {th['dns_min_misses']} 回以上・{th['dns_slow_min_sum_ms']:,} ms 以上で立った窓だけ")
    if aft["web"] or (bef or {}).get("web"):
        src += ("。`web_dns_misses`・`web_dns_ms_sum` (T.33.3) のある版は本体の `dns_slow` と同じく 443・80 宛ての"
                "成功したミスだけで選び、数える (" + "・".join(name for name, v in (("後", aft), ("前", bef))
                                                         if (v or {}).get("web")) + ")")
    if bef is not None and not bef["exact"]:
        src += ("。前は `dns_fail_ms_sum` が無いので立った窓 (失敗込みの閾) を全部使い、失敗の混ざらない 1 分は確か、"
                "混ざった 1 分は成功したミスの待ちを 0〜その 1 分の合計とした上下限")
    if aft["truncated"] or (bef or {}).get("truncated"):
        src += "。**`/events` が切れている** (立った件は下限)"
    rates = "表示だけ: " + rate(aft, "成功したミスで立った")
    if bef is not None and bef["hours"]:
        rates += " (前 " + rate(bef, "成功したミスで立った" if bef["exact"] else "失敗込みの閾で立った") + ")"
    if not aft["fired"]:
        return (label, limit, f"後の期間に成功したミスで立った窓は **0** 件。{rates}", MET, src)
    a = aft["lo"] / aft["misses"]
    shown = f"**{a:,.0f}** ms ({n(aft['misses'])} 回、立った {aft['fired']} 件)"
    if bef is None:
        return (label, limit, shown + "。" + rates, UNKNOWN,
                "前の雪像に `/events` か `/history?res=60` の `dns_misses`・`dns_fail_misses` が無い "
                "(`dns_fail_misses` は T.32.17 で足した欄)。" + src)
    if not bef["misses"]:
        return (label, limit, shown + "。" + rates, UNKNOWN,
                "前の版の `/history?res=60` の窓で立った窓に成功したミスが無い (比べる平均が無い)。" + src)
    lo, hi = bef["lo"] / bef["misses"], bef["hi"] / bef["misses"]
    if bef["exact"]:
        shown += f" (前 {avg(bef)}" + (f"、{a / lo:.2f} 倍" if lo else "") + ")"
        ok = a < lo * k if lo else False
        return (label, limit, shown + "。" + rates, MET if ok else MISSED, src)
    shown += f" (前 {avg(bef)})"
    if lo and a < lo * k:
        verdict = MET
    elif a >= hi * k:
        verdict = MISSED
    else:
        verdict = UNKNOWN
        src = (f"前の平均の上下限 {lo:,.0f}〜{hi:,.0f} ms の {k} 倍 ({lo * k:,.0f}〜{hi * k:,.0f} ms) の間に後の "
               f"{a:,.0f} ms があり、どちらとも言えない。" + src)
    return (label, limit, shown + "。" + rates, verdict, src)


def answered_names(snap):
    """`/dns` の一覧のうち答えのある (`addrs` のある) 名前の数 (T.33.2)。

    一覧が切れていれば (`truncated`、または `shown` < `count`) 数えられないので None。表の大半は答えの無い
    失敗の行 (総当たりの死んだ名前) で、裏の引き直しの対象ではない。
    """
    d = part(snap, "dns")
    rows = d.get("entries")
    if not isinstance(rows, list) or d.get("truncated"):
        return None
    if isinstance(d.get("count"), int) and len(rows) < d["count"]:
        return None
    return sum(1 for e in rows if e.get("addrs") or e.get("addr_count"))


def _p30_refresh_per_name(c, th):
    """T.33.2: 裏の引き直し (回/時) を**答えのある名前 1 つあたり**で前の期間と比べ、1.5 倍未満なら満たす。

    phase25 の行 (`_p25_refresh_per_name`) は `/status` の `dns.entries` (表の名前の数) で割っていて、表の上限
    (T.32.2) や総当たりの濃さで分母が動く (T.32.99 は 7,511 → 891 名前で 0.04 → 0.25 回/時/名前、6.27 倍の
    「届かず」。引き直しの合計は 302.4 → 224.9 回/時に下がっていた)。分母は `/dns` の一覧の `addrs` のある行の数
    (`answered_names`) にする。前か後の一覧が切れていて数えられなければ、**引き直しの合計 (回/時)** が前の期間の
    1.5 倍未満で比べる (理由を書く)。表の名前の数あたりと `expired` + `warm_stale` は表示だけ。
    """
    k = th["refresh_per_name_factor"]
    label = (f"裏の引き直し (回/時) が、答えのある名前 1 つあたりで前の期間の {k} 倍未満 (T.33.2。`/dns` の一覧が"
             "切れていれば引き直しの合計で。表の名前の数あたりと `expired` + `warm_stale` は表示だけ)")
    limit = f"< 前 ×{k} (答えのある名前 1 つあたり、無ければ合計)"
    rb, ra = counter_rates(c, "refreshes")
    src = ("`/status` の `dns.refreshes` を 1 時間あたりにした (前は前の雪像の起動から、後は"
           + ("その間" if dns_counter(c, "refreshes")[2] else "後の雪像の起動から") + ")")
    if rb is None or ra is None:
        return (label, limit, "—", UNKNOWN, "前か後の `/status` に `dns.refreshes` か `uptime_secs` が無い")
    nb, na = answered_names(c["a"]), answered_names(c["b"])
    extra = []
    eb = (part(c["a"], "status").get("dns") or {}).get("entries")
    ea = (part(c["b"], "status").get("dns") or {}).get("entries")
    if eb and ea:
        extra.append(f"表の名前あたり {ra / ea:.2f} 回/時/名前 (前 {rb / eb:.2f}、名前 {n(ea)}、前 {n(eb)})")
    rates = [counter_rates(c, "misses_by_kind", kind) for kind in ("expired", "warm_stale")]
    if not any(b is None or a is None for b, a in rates):
        mb_, ma_ = sum(b for b, _ in rates) * 24, sum(a for _, a in rates) * 24
        extra.append(f"`expired` + `warm_stale` {ma_:,.1f} 回/日 (前 {mb_:,.1f})")
    if nb and na:
        pb, pa = rb / nb, ra / na
        shown = (f"**{pa:.2f}** 回/時/答えのある名前 (前 {pb:.2f}" + (f"、{pa / pb:.2f} 倍" if pb else "") + ")。"
                 f"表示だけ: 合計 {ra:,.1f} 回/時 (前 {rb:,.1f})、答えのある名前 {n(na)} (前 {n(nb)})")
        ok = pa < pb * k if pb else pa == 0
        src += "、`/dns` の一覧 (切れていない) の `addrs` のある行の数で割った"
    else:
        why = []
        for side, snap, v in (("前", c["a"], nb), ("後", c["b"], na)):
            if v is None:
                d = part(snap, "dns")
                why.append(f"{side}の `/dns` の一覧が切れている ({n(len(d['entries']))} / {n(d.get('count'))} 行)"
                           if isinstance(d.get("entries"), list) else f"{side}の雪像に `/dns` の部が無い")
            elif not v:
                why.append(f"{side}の `/dns` の一覧に答えのある名前が無い")
        shown = (f"合計 **{ra:,.1f}** 回/時 (前 {rb:,.1f}" + (f"、{ra / rb:.2f} 倍" if rb else "") + ")")
        ok = ra < rb * k if rb else ra == 0
        src = "、".join(why) + " ので答えのある名前の数が出ない。引き直しの合計で比べた。" + src
        known = [f"{side} {n(v)}" for side, v in (("後", na), ("前", nb)) if v]
        if known:
            extra.append("答えのある名前 " + "・".join(known))
    if extra:
        shown += ("。表示だけ: " if "表示だけ" not in shown else "、") + "、".join(extra)
    return (label, limit, shown, MET if ok else MISSED, src)
