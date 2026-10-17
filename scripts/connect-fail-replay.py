#!/usr/bin/env python3
# 「繋がらなかった宛先を N 秒覚えて、その間の同じ宛先への要求は繋ぎに行かずに 502 を返す」
# 規則を雪像 1 枚の上で回し、**早く断れた件数と縮む待ち**、**誤って断ったはずの件数**を数える (T22.3 段 1)。
#
# 材料は雪像 (`/snapshot`) 1 枚:
#   - `/log` の `502 Bad Gateway: connect <host:port> failed: <理由>` の行 … 失敗 1 回ずつの時刻
#     (`/errors` の 1 行は同じ失敗をまとめたもの (T20.3) で間の時刻が無いので、時刻はこちらから取る。
#     CONNECT (`crates/tunnel`) も forward (`crates/http`) も同じ文面で書く)
#   - `/errors` の `cause`・`dns_ms`・`connect_ms`・`repeats` … 1 回の失敗で待たせた時間と、行の数の突き合わせ
#   - `/recent` (閉じた接続の個票) … 成功の時刻 (秒まで)。256 KiB で切れているので窓は短い
#   - `/hosts_series` (上位 16 ホスト × 5 分) … 成功の 5 分ごとの件数 (`count` − `errors`)
#   - `/hosts` の `requests` − `errors` … 宛先ごとの成功の総数 (時刻は無い)
#
# **写した規則** (段 2 で入れるならこの形):
#   - 覚えるのは `timeout` / `refused` / `unreachable` の失敗だけ (宛先 = `host:port`。CONNECT と forward で共通)
#   - 失敗が終わった時刻 `f` から N 秒の間 (`f < 着いた時刻 <= f + N`) に着いた同じ宛先への要求は、
#     繋ぎに行かずに 502 を返す。**早く断った要求では時刻を延ばさない** (N 秒ごとに 1 本は本当に繋ぎに行く)
#   - 成功したら忘れる (ただし覚えている間に着いた要求は断るので、成功しない)
#
# **数え方**:
#   - (a) 早く断れた件数と縮む待ち: 失敗の時刻だけで決まる (覚えている間に着いた成功は断られて
#     覚えを消さず、覚えが切れたあとの成功は消すものが無いので、成功の位置は (a) を動かさない)。
#     失敗の行がそろっていれば (`/log` が切れていない) **ちょうどの値**。縮む待ちは、断った要求が
#     本当に払った `dns_ms` + `connect_ms` の合計
#   - (b) 誤って断ったはずの件数 (= 覚えている間に着いた成功): 時刻の分かり方で下限と上限を書く。
#     下限 = `/recent` の成功 (秒まで分かる) のうち覚えている間に着いたもの。
#     上限 = 下限 + `/hosts_series` の 5 分枠のうち覚えている間と重なる枠の成功 + 時刻の分からない成功
#     (`/hosts` の総数から上の 2 つを引いた残り。覚えている間が「時刻の分かる区間」の外にもあるときだけ足す)。
#     `/hosts` が 256 KiB で切れていてその宛先が載っていなければ、載っている中で最も少ない `requests` を
#     上限の代わりに使う (並びは `requests` の多い順なので、載らなかった宛先はそれ以下)
#
# **同じ起動の雪像を `--with` で足すと**、成功の時刻の手がかり (`/recent` と `/hosts_series`) だけをそこから足す
# (日次の `/snapshots/<date>` など。起動の時刻 `taken_at - uptime_secs` が 5 秒以内のものだけ使い、ほかは黙って捨てる)。
# 失敗の列と `/hosts` の総数は主の 1 枚からだけ取る。
#
# 使い方: scripts/connect-fail-replay.py SNAP.json... [--with SNAP]... [--secs N]... [--anon] [--top K] [--json]
# 依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。

import argparse
import json
import re
import sys

# 段 1 の N の既定 (TODO.md の T22.3)
DEFAULT_SECS = (30, 60, 300)
# 覚える原因 (`crates/metrics-types` の `ERR_CAUSE_NAMES` の綴り)
CAUSES = ("timeout", "refused", "unreachable")
# 目安 (T22.3): (b) が 0、かつ (a) で縮む待ちが 1 日 5 分以上
TARGET_SAVED_SECS_PER_DAY = 300.0

LOG_RE = re.compile(r"^502 Bad Gateway: connect (\S+) failed: (.*)$")


def cause_of_message(msg):
    """`io::Error` の文面から原因を当てる (`ErrCause::from_io` の写し。覚える 3 つ以外は `None`)。"""
    m = msg.lower()
    if "timed out" in m:
        return "timeout"
    if "refused" in m:
        return "refused"
    if "unreachable" in m or "no route to host" in m or "cannot assign requested address" in m:
        return "unreachable"
    return None


def target_key(target):
    """`/errors` の `target` (`http://h:80` / `h:443`) と `/log` の `h:443` を同じ鍵 `h:port` にそろえる。"""
    t = target.split("://", 1)[-1].rstrip("/")
    return t.lower()


def part(snap, name, key):
    v = snap.get(name)
    if isinstance(v, dict) and isinstance(v.get(key), list):
        return v, v[key]
    return {}, []


def run_start(snap):
    return int(snap.get("taken_at", 0)) - int(snap.get("uptime_secs", 0))


def failures(snap):
    """この起動の間の失敗 1 回ずつ: `{target: [(done, wait_s, cause), ...]}` と、覆う秒数。

    時刻は `/log` から取る (この起動より前の行 = 状態ファイルから読み戻した行は捨てる)。
    待ちは同じ宛先・同じ原因の `/errors` の行 (`at` <= 時刻 <= `last_at`) の `dns_ms` + `connect_ms`。
    """
    start = run_start(snap)
    taken = int(snap.get("taken_at", 0))
    logm, lines = part(snap, "log", "lines")
    _, rows = part(snap, "errors", "errors")
    first = min((int(x.get("at", 0)) for x in lines), default=taken)
    # 覆う区間: ログが切れていなければ起動から、切れていれば残っている最古の行から
    lo = max(start, first) if logm.get("recorded", 0) > logm.get("count", 0) else start
    by_row = {}
    for r in rows:
        if r.get("cause") not in CAUSES:
            continue
        k = (target_key(r.get("target", "")), r["cause"])
        by_row.setdefault(k, []).append(r)
    out = {}
    for x in lines:
        m = LOG_RE.match(x.get("msg", ""))
        if not m:
            continue
        cause = cause_of_message(m.group(2))
        done = int(x.get("at", 0))
        if cause is None or done < lo:
            continue
        tgt = target_key(m.group(1))
        cands = by_row.get((tgt, cause), [])
        row = next((r for r in cands
                    if int(r["at"]) <= done <= int(r.get("last_at", r["at"]))), None)
        if row is None and cands:
            row = min(cands, key=lambda r: abs(int(r["at"]) - done))
        wait = (row.get("dns_ms", 0) + row.get("connect_ms", 0)) / 1000.0 if row else 0.0
        out.setdefault(tgt, []).append((done, wait, cause))
    for v in out.values():
        v.sort()
    return out, lo, taken


def rows_count(snap, lo):
    """`/errors` の行から数えた、覆う区間の失敗の回数 (`/log` との突き合わせ用)。"""
    _, rows = part(snap, "errors", "errors")
    c = {}
    for r in rows:
        if r.get("cause") in CAUSES and int(r.get("at", 0)) >= lo:
            k = target_key(r.get("target", ""))
            c[k] = c.get(k, 0) + int(r.get("repeats", 1))
    return c


# 同じ起動の雪像と見なす `taken_at - uptime_secs` のずれ (秒。丸めで 1 秒ずれる)
SAME_RUN_SLACK = 5


def same_run(a, b):
    return abs(run_start(a) - run_start(b)) <= SAME_RUN_SLACK


class Evidence:
    """成功の時刻の手がかりを、同じ起動の雪像 (主の 1 枚 + `--with` の何枚か) から集めたもの。

    - `/recent` の成功は `id` で重ねずに集め、窓 ([最古の `at`, 取った時刻]) を区間として持つ
    - `/hosts_series` の 5 分枠は枠の始まりの時刻で重ねずに集め (大きい方)、数えている区間を持つ
    - `/hosts` の総数は主の 1 枚から (いちばん新しいので、いちばん大きい)
    """

    def __init__(self, main, extra=()):
        self.main = main
        snaps = [main] + [x for x in extra if same_run(main, x)]
        self.used = len(snaps)
        start = run_start(main)
        self.exact = {}   # 宛先 → {id: at}
        self.rec_cov = []  # [(lo, hi)]
        self.slots = {}   # 宛先 → {枠の始まり: (終わり, 成功の数)}
        self.series_cov = {}  # 宛先 → [(lo, hi)]
        for sn in snaps:
            taken = int(sn.get("taken_at", 0))
            _, rec = part(sn, "recent", "recent")
            if rec:
                self.rec_cov.append((max(start, min(int(x["at"]) for x in rec)), taken))
            for x in rec:
                if str(x.get("reason", "")).startswith("error") or int(x.get("at", 0)) < start:
                    continue
                tgt = target_key(x.get("target", ""))
                self.exact.setdefault(tgt, {})[x.get("id", (x["at"], x.get("client")))] = int(x["at"])
            hs = sn.get("hosts_series") or {}
            w = int(hs.get("window_secs", 300) or 300)
            t0 = hs.get("t0")
            if t0 is None or not isinstance(hs.get("series"), list):
                continue
            for se in hs["series"]:
                if not isinstance(se.get("samples"), list):
                    continue
                tgt = target_key(se.get("host", ""))
                idx = [k for k, r in enumerate(se["samples"]) if r and r[0]]
                if not idx:
                    continue
                # 上位 16 に入った枠より前は数えていない (0 が並ぶ)。入った枠そのものも途中からなので、
                # 数えている区間はその次の枠から
                lo = t0 + (idx[0] + 1) * w
                self.series_cov.setdefault(tgt, []).append((lo, t0 + len(se["samples"]) * w))
                d = self.slots.setdefault(tgt, {})
                for k in idx:
                    a = t0 + k * w
                    if a < lo:
                        continue
                    r = se["samples"][k]
                    n = r[0] - (r[4] if len(r) > 4 else 0)
                    if n > 0:
                        d[a] = (a + w, max(n, d.get(a, (0, 0))[1]))

    def total(self, tgt):
        """宛先の成功の総数の上限 (`/hosts` の `requests` − `errors`。CONNECT と forward の和)。"""
        hm, hosts = part(self.main, "hosts", "hosts")
        rows = [h for h in hosts if target_key(h.get("host", "")) == tgt]
        if rows:
            return sum(max(0, int(h.get("requests", 0)) - int(h.get("errors", 0))) for h in rows)
        if hm.get("truncated") and hosts:
            # 載らなかった宛先は、載っている中で最も少ない `requests` 以下 (CONNECT と forward の 2 つ)
            return 2 * min(int(h.get("requests", 0)) for h in hosts)
        return 0


def covered_by(a, b, cov):
    """区間 (a, b] が `cov` の区間の和に入りきるか。"""
    t = a
    for lo, hi in sorted(cov):
        if lo <= t < hi:
            t = hi
        if t >= b:
            return True
    return t >= b


def replay(fails, secs):
    """失敗の列に規則を当てる: (断った [(着いた時刻, 待ち)], 覚えている区間 [(f, f+N)])。"""
    rejected = []
    windows = []
    mem = None
    for done, wait, _cause in fails:
        arrived = done - wait
        if mem is not None and mem < arrived <= mem + secs:
            rejected.append((arrived, wait))
            continue
        mem = done if mem is None else max(mem, done)
        windows.append((done, done + secs))
    return rejected, windows


def inside(t, windows):
    return any(a < t <= b for a, b in windows)


def count_target(ev, tgt, fails, secs):
    rejected, windows = replay(fails, secs) if secs > 0 else ([], [])
    exact = sorted(ev.exact.get(tgt, {}).values())
    slots = sorted((a, b, n) for a, (b, n) in ev.slots.get(tgt, {}).items())
    scov = ev.series_cov.get(tgt, [])
    # 5 分枠で数えている所の成功は枠の方で数える (同じ成功を 2 度数えない)
    loose = [t for t in exact if not any(lo <= t < hi for lo, hi in scov)]
    b_lo = sum(1 for t in exact if inside(t, windows))
    b_slot = sum(n for a, b, n in slots if any(x < b and a < y for x, y in windows))
    total = ev.total(tgt)
    unknown = max(0, total - len(loose) - sum(n for _a, _b, n in slots))
    # 時刻の分からない成功は、覚えている間のどれかが「時刻の分かる区間」の外にあるときだけ上限に足す
    known = ev.rec_cov + scov
    outside = any(not covered_by(a, b, known) for a, b in windows)
    b_hi = max(b_lo, sum(1 for t in loose if inside(t, windows)) + b_slot) + (unknown if outside else 0)
    return {
        "target": tgt,
        "failures": len(fails),
        "causes": sorted({c for _d, _w, c in fails}),
        "early": len(rejected),
        "saved_secs": round(sum(w for _t, w in rejected), 3),
        "wrong_lo": b_lo,
        "wrong_hi": b_hi,
        "successes_total": total,
        "successes_unknown": unknown,
    }


def analyze(snap, secs_list=DEFAULT_SECS, extra=()):
    fails, lo, taken = failures(snap)
    ev = Evidence(snap, extra)
    covered = max(1, taken - lo)
    check = rows_count(snap, lo)
    mismatch = {t: (len(fails.get(t, [])), n) for t, n in check.items()
                if len(fails.get(t, [])) != n}
    res = {"taken_at": taken, "covered_secs": covered, "snapshots_used": ev.used,
           "targets": len(fails),
           "failures": sum(len(v) for v in fails.values()),
           "log_vs_errors_mismatch": mismatch, "by_secs": []}
    for secs in secs_list:
        per = [count_target(ev, t, v, secs) for t, v in fails.items()]
        per.sort(key=lambda r: (-r["saved_secs"], r["target"]))
        early = sum(r["early"] for r in per)
        saved = sum(r["saved_secs"] for r in per)
        res["by_secs"].append({
            "secs": secs,
            "early": early,
            "saved_secs": round(saved, 3),
            "saved_secs_per_day": round(saved * 86400.0 / covered, 1),
            "wrong_lo": sum(r["wrong_lo"] for r in per),
            "wrong_hi": sum(r["wrong_hi"] for r in per),
            "meets": (sum(r["wrong_hi"] for r in per) == 0
                      and saved * 86400.0 / covered >= TARGET_SAVED_SECS_PER_DAY),
            "per_target": per,
        })
    return res


def anon_names(res):
    """宛先を「宛先 A」「宛先 B」… に置き換える表 (縮む待ちの多い順。報告に貼るとき用)。"""
    order = []
    for b in res["by_secs"]:
        for r in b["per_target"]:
            if r["target"] not in order:
                order.append(r["target"])
    names = {}
    for i, t in enumerate(order):
        s = ""
        n = i
        while True:
            s = chr(ord("A") + n % 26) + s
            n = n // 26 - 1
            if n < 0:
                break
        names[t] = "宛先 " + s
    return names


def render(res, label, top=5, anon=False, out=sys.stdout):
    names = anon_names(res) if anon else {}
    nm = (lambda t: names.get(t, t)) if anon else (lambda t: t)
    hours = res["covered_secs"] / 3600.0
    print(f"## {label} (覆う区間 {hours:.1f} 時間、失敗 {res['failures']} 回 / 宛先 {res['targets']} 個、"
          f"成功の時刻は同じ起動の雪像 {res['snapshots_used']} 枚から)", file=out)
    if res["log_vs_errors_mismatch"]:
        print("注意: /log と /errors の回数が合わない宛先: "
              + ", ".join(f"{nm(t)} log {a} / errors {b}"
                          for t, (a, b) in sorted(res["log_vs_errors_mismatch"].items())), file=out)
    print("", file=out)
    print("| N 秒 | (a) 早く断れた件数 | (a) 縮む待ち (秒) | 1 日あたり (分) | (b) 誤って断った (下限〜上限) | 目安 |", file=out)
    print("|---:|---:|---:|---:|---:|---|", file=out)
    for b in res["by_secs"]:
        print(f"| {b['secs']} | {b['early']} | {b['saved_secs']:.1f} | {b['saved_secs_per_day'] / 60:.2f} "
              f"| {b['wrong_lo']}〜{b['wrong_hi']} | {'満たす' if b['meets'] else '届かず'} |", file=out)
    for b in res["by_secs"]:
        rows = [r for r in b["per_target"] if r["early"] or r["wrong_hi"]][:top]
        if not rows:
            continue
        print("", file=out)
        print(f"N = {b['secs']} 秒の内訳 (縮む待ちの多い順に {len(rows)} 個):", file=out)
        for r in rows:
            print(f"  - {nm(r['target'])}: 失敗 {r['failures']} 回 ({'/'.join(r['causes'])})、早く断れた {r['early']}、"
                  f"縮む待ち {r['saved_secs']:.1f} 秒、誤って断った {r['wrong_lo']}〜{r['wrong_hi']} "
                  f"(成功の総数 {r['successes_total']}、うち時刻の分からないもの {r['successes_unknown']})", file=out)
    print("", file=out)


def main(argv=None):
    p = argparse.ArgumentParser(description="replay a remember-failed-targets rule over a snapshot")
    p.add_argument("snapshot", nargs="+")
    p.add_argument("--secs", type=int, action="append", default=[], metavar="N",
                   help="how long to remember a failed target (repeatable; default 30, 60, 300)")
    p.add_argument("--with", dest="extra", action="append", default=[], metavar="SNAP",
                   help="another snapshot of the same run, used only for success timestamps (repeatable)")
    p.add_argument("--anon", action="store_true", help="print targets as 宛先 A, B, ...")
    p.add_argument("--top", type=int, default=5)
    p.add_argument("--json", action="store_true")
    a = p.parse_args(argv)
    secs = tuple(a.secs) or DEFAULT_SECS
    extra = []
    for path in a.extra:
        with open(path, encoding="utf-8") as f:
            extra.append(json.load(f))
    allres = {}
    for path in a.snapshot:
        with open(path, encoding="utf-8") as f:
            snap = json.load(f)
        res = analyze(snap, secs, extra)
        allres[path] = res
        if not a.json:
            render(res, path, top=a.top, anon=a.anon)
    if a.json:
        json.dump(allres, sys.stdout, ensure_ascii=False, indent=1)
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
