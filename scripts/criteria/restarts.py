# 判定表 (`scripts/snapshot_criteria.py`) の**再起動**の部 (T.34.124 で割った)。
#
# 2 枚の雪像の間の再起動 (`restart_info`。差分の §1) と、`/events` の `start` から無言の再起動を
# 数える部 (`silent_restarts`。判定表の phase23 以降の行と `snapshot-summary.py` が使う。T23.2)。

import re

from .common import part, stamp


# ---------------------------------------------------------------- (1) 再起動

def restart_info(a, b):
    ta, tb = a["taken_at"], b["taken_at"]
    ua, ub = a["uptime_secs"], b["uptime_secs"]
    wall = (tb - ta) if (ta and tb) else None
    reasons = []
    if a["version"] and b["version"] and a["version"] != b["version"]:
        reasons.append(f"版が変わった (`{a['version']}` → `{b['version']}`)")
    if ub < ua:
        reasons.append(f"`uptime_secs` が減った ({ua:,} → {ub:,} 秒)")
    elif wall is not None and wall > 0:
        # 時計のずれと取得の間の分を見込んで、窓の 1% (最低 60 秒) は許す
        slack = max(60, wall // 100)
        if (ub - ua) + slack < wall:
            reasons.append(f"`uptime_secs` の伸び {ub - ua:,} 秒が窓 {wall:,} 秒に足りない "
                           f"(差 {wall - (ub - ua):,} 秒)")
    restarted = bool(reasons)
    started = (tb - ub) if tb else None
    return {
        "restarted": restarted,
        "reasons": reasons,
        "wall_secs": wall,
        "uptime": [ua, ub],
        "since_start": [a["since_start_secs"], b["since_start_secs"]],
        "version": [a["version"], b["version"]],
        "taken_at": [ta, tb],
        "started_at": started,
        # 前後を切る境目: 再起動があればその時刻、無ければ A の取得時刻
        "boundary": (started if restarted else ta) or 0,
    }


# ---------------------------------------------------------------- (23) 無言の再起動

# `start` の文面 (`crates/metrics-recent/src/events.rs` の "version {} on port {} …") から版を抜く
START_VERSION = re.compile(r"^version\s+(\S+)")

# `start` の文面に載っている `boot` の印 (T24.0)。`crates/run/src/lib.rs` が
# `/proc/sys/kernel/random/boot_id` の**先頭 12 文字**を "… on port 60357 boot 7aa666f1-264 (profile …)" と書く
# (1 件 128 B の枠に収めるため。全体は `/status` の `boot.id`)。
# 手で組んだ雪像や将来の版が全体 (36 文字) を書いても読めるように、6〜36 文字を受ける。
START_BOOT = re.compile(r"\bboot\s+([0-9a-fA-F]{6,36})\b")


def start_version(e):
    """`start` の 1 行から版を抜く (文面に無ければ空文字。`snapshot-anonymize` の `9****28` もそのまま)。"""
    m = START_VERSION.match((e or {}).get("text") or "")
    return m.group(1) if m else ""


def start_boot(e):
    """`start` の 1 行から `boot` (ホストの起動ごとに変わる印) を抜く。

    T24.0 より前の版が書いた行には無いので `None` (その行は「同じホストか」を判定できない)。
    先頭 12 文字だけでも**同じ起動かどうか**は分かる (36 文字の UUID の先頭が 32 ビット以上ある)。
    """
    m = START_BOOT.search((e or {}).get("text") or "")
    return m.group(1) if m else None


def silent_restarts(snap):
    """`/events` の `start` のうち、直前に `shutdown` が無い起動 (= 無言の再起動) を返す (T23.2)。

    2026-10-17 22:14:52Z の再起動は `shutdown` の行を残しておらず、heap の行が黙って
    「判定できず」になった (T22.99)。`/events` は 24 時間より長く残るので、`start` の直前が
    `shutdown` かどうかで見分けられる。**気づけないまま 24 時間を捨てるのを防ぐ計器**。

    見方 (ここが本体。要点と判定表の両方がこの 1 つを使う):
      - **起動の行は文面が `version …` で始まる `start` だけ**。`start` の種類は自己ベンチの要約にも
        使われていて (`PROXY_SELF_BENCH=on` で起動の 3 秒後に 1 件)、それを起動と数えると
        「直前に `shutdown` が無い」と誤って言う。数えなかった件数は `other_starts` に残す。
      - `at` が**その `start` より前**の最後の出来事が `shutdown` なら、普通の再起動
        (同じ秒の出来事は「直前」と見ない。起動の瞬間に `anomaly` が同じ `at` で載る)。
      - 直前の出来事が `shutdown` でなければ**無言の再起動**。前の `start` からの時間
        (`prev_hours`) を添える。前の起動の後に `shutdown` が残っていれば `downtime` (止まっていた秒数)、
        残っていなければ None (落とされたので分からない)。
      - **状態ファイルから読み戻した古い `start` (`restored`) も、同じ規則で見る**。読み戻した
        `shutdown` があるので、版を上げただけの再起動は無言にならない。逆に、窓の**先頭**の `start`
        (それより前の出来事が 1 つも無い) や、窓が起動の途中から始まっている `start` は
        「無言だった」とは言えないので `edge` に分ける (数えない)。
        2026-10-20 の雪像では、10-08 09:24:20Z の `start` (読み戻しの先頭) がここに入る。

    返すのは次の辞書。`/events` の部が無ければ None (呼ぶ側が「判定できず」と書く)。
      `starts` は起動の `start` の行の数 (自己ベンチの `start` は `other_starts`)、
      `silent` / `graceful` / `edge` はそれぞれの内訳、`host` / `process` / `boot_unknown` は
      **無言の再起動の切り分け** (T24.0。下の 3 つ)、
      `hours` は起動からの時間、`kept` / `count` / `capacity` / `restored` / `truncated` は `/events` の欄。

    **ホストの再起動か、プロセスだけの再起動か** (T24.0): 無言の再起動の 1 件ずつについて、
    その `start` の文面の `boot` (T24.0 から載る) と**1 つ前の起動の `boot`** を比べる。
      - `host` … `boot` が変わっている = **ホスト (カーネル) が起動し直した** (2026-10-21 の 2 回。
        利用者に聞いてホストの管理者によるものと分かった)。**こちらでは防げない**。
      - `process` … `boot` が同じ = **プロセスだけが起動し直した** (落ちた・外から止められた)。
        これが「こちらが原因の無言の再起動」で、0 回であるべきもの。
      - `boot_unknown` … どちらかの行に `boot` が無い (T24.0 より前の版が書いた行)。
        判定できないので `host` / `process` のどちらにも数えない。
    """
    ev = part(snap, "events")
    rows = ev.get("events")
    if not isinstance(rows, list):
        return None
    marks = sorted((e for e in rows if isinstance(e, dict) and e.get("at")), key=lambda e: e["at"])
    up = snap.get("uptime_secs") or 0
    out = {
        "starts": 0, "silent": [], "graceful": [], "edge": [], "other_starts": 0,
        "hours": (up / 3600.0) if up else None,
        "count": ev.get("count"), "kept": ev.get("kept", len(rows)),
        "capacity": ev.get("capacity"), "restored": ev.get("restored"),
        "truncated": bool(ev.get("truncated")),
        "oldest": marks[0]["at"] if marks else None,
        "started_at": ((snap.get("taken_at") or 0) - up) if (snap.get("taken_at") and up) else None,
        # 無言の再起動の切り分け (T24.0。上を参照)
        "host": [], "process": [], "boot_unknown": 0,
    }
    prev = None            # 直前に見た起動の `start` (無言かどうかを言うには、これが要る)
    prev_boot = None       # その `start` の `boot` (1 つ前の起動のホスト)
    for e in marks:
        if e.get("kind") != "start":
            continue
        # **起動の `start` は文面が `version …` で始まる行だけ**。`start` の種類は自己ベンチの
        # 要約にも使われており (`PROXY_SELF_BENCH=on` で起動の 3 秒後に 1 件)、それを起動と
        # 数えると「直前に `shutdown` が無い」と誤って言う
        if not START_VERSION.match(e.get("text") or ""):
            out["other_starts"] += 1
            continue
        out["starts"] += 1
        at = e["at"]
        boot = start_boot(e)
        item = {"at": at, "version": start_version(e), "boot": boot}
        # **同じ秒の出来事は「直前」と見ない** (`at` が strictly 小さいものだけ)
        earlier = [m for m in marks if m["at"] < at]
        if not earlier:
            out["edge"].append(dict(item, why="窓の先頭"))
        elif earlier[-1].get("kind") == "shutdown":
            out["graceful"].append(dict(item, downtime=at - earlier[-1]["at"]))
        elif prev is None:
            # 窓が起動の途中から始まっている (`start` が読み戻しの先頭より前にある)
            out["edge"].append(dict(item, why="窓が起動の途中から始まっている"))
        else:
            shut = [m for m in earlier if m.get("kind") == "shutdown"]
            shut = shut[-1] if shut else None
            item = dict(
                item, prev_at=prev["at"], prev_version=start_version(prev),
                prev_hours=(at - prev["at"]) / 3600.0,
                shutdown_at=(shut["at"] if shut else None),
                # 前の起動より**後**に `shutdown` が残っているときだけ「止まっていた秒数」が言える
                downtime=((at - shut["at"]) if (shut and shut["at"] > prev["at"]) else None),
                # 1 つ前の起動の `boot` (T24.0)。無ければ「同じホストか」を言えない
                prev_boot=prev_boot,
            )
            if boot and prev_boot:
                (out["process"] if boot == prev_boot else out["host"]).append(item)
            else:
                out["boot_unknown"] += 1
            out["silent"].append(item)
        prev = e
        prev_boot = boot
    return out


def boot_split_text(r):
    """無言の再起動のうち、**ホストの再起動**と**プロセスだけの再起動**の内訳を 1 文にする (T24.0)。

    **`boot` を持つ行が 1 つも無ければ何も足さない** (T24.0 より前の版が書いた `start` だけの
    雪像)。過去の phase の出力を 1 文字も変えないための線で、`boot_unknown` だけの雪像
    (切り分けられる回が 1 つも無い) でも文は変えず、判定表の「根拠」に件数を書く。
    """
    host = len(r.get("host") or [])
    proc = len(r.get("process") or [])
    unknown = r.get("boot_unknown") or 0
    if not (host or proc):
        return ""
    text = f"。うちホストの再起動 (boot が変わった回) {host} 回"
    text += f"、プロセスだけの再起動 {proc} 回"
    if unknown:
        text += f"、`boot` の無い `start` {unknown} 件は判定できない"
    return text


def silent_restart_text(r, bold=True):
    """`silent_restarts()` の結果を 1 文にする (要点の 1 行と判定表の「実測」で同じ文を使う)。"""
    if r is None:
        return "判定できず (`/events` の部が無い)"
    if not r["starts"]:
        return "判定できず (`/events` に `start` の行が無い)"
    star = "**" if bold else ""

    def one(s):
        t = f"{star}{stamp(s['at'])}{star}"
        if s.get("version"):
            t += f" 版 `{s['version']}`"
        t += f"、前の起動は {s['prev_hours']:,.1f} 時間"
        if s.get("downtime") is not None:
            t += f" (止まっていた {s['downtime']:,} 秒)"
        return t

    if r["silent"]:
        text = f"{len(r['silent'])} 回 (" + "、".join(one(s) for s in r["silent"]) + ")"
        # 無言の再起動があるときは、窓の先頭の `start` が別に残っていることだけ添える
        if r["edge"]:
            text += f"。ほかに窓の先頭などの `start` {len(r['edge'])} 件は判定できない"
        # ホストの再起動かプロセスだけの再起動か (T24.0。`boot` の無い雪像では何も足さない)
        text += boot_split_text(r)
        return text
    if not r["graceful"]:
        # 直前を見られる `start` が 1 つも無い。**0 件とは書かない** (見ていないだけ)
        return f"判定できず (`start` {r['starts']} 件とも窓の先頭で、直前に出来事が無い)"
    # 何を見て 0 と言っているかを添える (窓の先頭の `start` は判定できない)
    basis = [f"直前が `shutdown` の起動 {len(r['graceful'])} 件"]
    if r["edge"]:
        basis.append(f"窓の先頭など {len(r['edge'])} 件は判定できない")
    return "0 件 (" + "、".join(basis) + ")"
