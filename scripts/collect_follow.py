#!/usr/bin/env python3
"""`scripts/collect-deployed.sh` が使う Python の部 (T34.139 で埋め込みから出した)。

    python3 scripts/collect_follow.py <小命令> 引数…

前は `collect-deployed.sh` の中に `python3 - <<'PICK'` などの形で 10 本埋め込んでいた (いちばん大きい
締めの選び方が 120 行)。中身は 1 文字も変えずに小命令ごとの関数にしただけで、引数の並びも前と同じ
(`argv[1]` が 1 つ目。`argv[0]` は小命令の名前)。印字と終了コードも前と同じ。
単体では叩かない (入口は `collect-deployed.sh`)。依存は Python 3 の標準ライブラリだけ。
"""

import json
import os
import re
import sys
import time


def status_version(argv):
    """標準入力の `/status` から `version` を 1 行で出す (読めなければ空行)。`--closeout` のいまの版。"""
    try:
        v = json.load(sys.stdin).get("version")
    except (ValueError, AttributeError):
        v = None
    print(v if isinstance(v, str) else "")


def snapshot_list(argv):
    """標準入力の `/snapshots` の一覧を、1 行 1 枚 (`<日付> <バイト数> <置く名前>`) にする。名前は形を確かめてから使う。

    停止の 1 枚の隣の `/profile?res=60` (`<時刻>-shutdown-profile`。T40.0) は `<時刻>-shutdown-profile_res_60.json` に置く
    (停止の 1 枚 `<時刻>-shutdown-snapshot.json` の `-snapshot.json` を `-profile_res_60.json` にした名前で、
    普段の 1 回が隣に置く `/profile?res=60` と同じ流儀)。
    """
    try:
        d = json.load(sys.stdin)
    except ValueError:
        sys.exit("not JSON")
    for f in d.get("files") or []:
        day = str(f.get("date"))
        if re.fullmatch(r"\d{4}-\d{2}-\d{2}", day):
            print(day, f.get("bytes", 0), day + "T000000Z-snapshot.json")
    for m in d.get("moments") or []:
        name = str(m.get("name"))
        if re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{6}Z-(start|shutdown)", name):
            print(name, m.get("bytes", 0), name + "-snapshot.json")
        elif re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{6}Z-shutdown-profile", name):
            print(name, m.get("bytes", 0), name + "_res_60.json")


def pick(argv):
    """`DIR` の起動と停止の 1 枚から、締めの「前」と「0 時間」を版で選んで箇条で出す (T29.26)。"""
    d = argv[1]
    cur = os.environ.get("CURRENT_VERSION") or None
    pick_out = os.environ.get("CLOSEOUT_PICK") or None
    pat = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{6}Z)-(start|shutdown)-snapshot\.json$")

    def get(doc, *keys):
        for k in keys:
            doc = doc.get(k) if isinstance(doc, dict) else None
        return doc

    ms = []
    for name in sorted(os.listdir(d)):
        m = pat.match(name)
        if not m:
            continue
        path = os.path.join(d, name)
        try:
            with open(path, encoding="utf-8") as f:
                doc = json.load(f)
        except (OSError, ValueError):
            doc = {}
        ver = get(doc, "status", "version") or get(doc, "version")
        ms.append({"path": path, "at": m.group(1), "kind": m.group(2),
                   "ver": ver if isinstance(ver, str) and ver else None,
                   "build": get(doc, "status", "build"), "boot": get(doc, "status", "boot", "id"),
                   "pid": get(doc, "status", "boot", "pid"), "taken": get(doc, "taken_at"),
                   "up": get(doc, "status", "uptime_secs")})

    def base(v):
        return v.split("+", 1)[0].split(" ", 1)[0] if v else None

    def series(v):
        b = base(v)
        return ".".join(b.split(".")[:2]) if b else None

    def when(m):
        return "%s-%s-%sT%s:%s:%sZ" % (m["at"][:4], m["at"][5:7], m["at"][8:10], m["at"][11:13], m["at"][13:15], m["at"][15:17])

    def started(m):
        if isinstance(m["taken"], (int, float)) and isinstance(m["up"], (int, float)):
            return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(m["taken"] - m["up"]))
        return when(m) + " の 60 秒ほど前"

    why = {}
    before = zero = None
    if not any(m["ver"] for m in ms):
        # 版の読めない古い形: 最後の停止と、その後の最初の起動 (T28.20 のまま)
        stops = [m for m in ms if m["kind"] == "shutdown"]
        before = stops[-1] if stops else None
        zero = next((m for m in ms if m["kind"] == "start" and (before is None or m["at"] > before["at"])), None) \
            if before else None
        how = "時刻だけで選んだ (版の読める 1 枚が無い)"
    else:
        if cur is None:
            cur = next((m["ver"] for m in reversed(ms) if m["kind"] == "start" and m["ver"]), None)
            src = "手元の最後の起動の版"
        else:
            src = "`/status`"
        how = "いまの版 `%s` (%s) で選んだ" % (cur, src)
        stops = [m for m in ms if m["kind"] == "shutdown" and m["ver"]]
        other = [m for m in stops if series(m["ver"]) != series(cur)]
        if other:
            before = other[-1]
        else:
            diff = [m for m in stops if base(m["ver"]) != base(cur)]
            before = diff[-1] if diff else None
            if before:
                how += "。系列 %s の違う停止が無いので、版の違う最後の停止" % series(cur)
        zero = next((m for m in ms if m["kind"] == "start" and base(m["ver"]) == base(cur)
                     and (before is None or m["at"] > before["at"])), None)
        for m in ms:
            if m is before or m is zero:
                continue
            if not m["ver"]:
                why[id(m)] = "版が読めない"
            elif m["kind"] == "shutdown":
                if base(m["ver"]) == base(cur):
                    why[id(m)] = "いまの版の停止"
                elif series(m["ver"]) == series(cur):
                    why[id(m)] = "いまと同じ系列 %s の途中の版" % series(cur)
                else:
                    why[id(m)] = "前に選んだ停止より古い"
            elif base(m["ver"]) != base(cur):
                why[id(m)] = "いまの版でない起動"
            elif before is not None and m["at"] < before["at"]:
                why[id(m)] = "前に選んだ停止より古い"
            else:
                why[id(m)] = "いまの版の 2 回目以降の起動"

    if ms and any(m["ver"] for m in ms):
        print("- 締めの候補 (起動と停止の 1 枚、古い順。%s):" % how)
        for m in ms:
            tag = "**前 (BEFORE)**" if m is before else "**0 時間 (ZERO)**" if m is zero else "外した: " + why.get(id(m), "")
            print("  - `%s` %s `%s`%s → %s" % (os.path.basename(m["path"]), m["kind"], m["ver"] or "?",
                  " boot.id `%s`" % m["boot"] if m["boot"] else "", tag))
    if before:
        # 前の CPU の材料 (`--profile-before`) の選び方 (T40.0)。本番の 1 回 (`judge-profile`) も同じ関数で選ぶ。
        # `PROFILE_BEFORE` で手で渡されていれば、どちらもそれを使う (T40.1)
        print("- 前の CPU の材料 (`--profile-before`): " + profile_before_note(before["path"], d))
    if before and zero:
        print("- 締めの判定に渡せる 2 枚 (前と 0 時間): `BEFORE=%s ZERO=%s`" % (before["path"], zero["path"]))
    elif before:
        print("- 締めの判定に渡せる「前」(その後のいまの版の起動の 1 枚はまだ無い): `BEFORE=%s`" % before["path"])
    elif zero:
        print("- 締めの判定に渡せる「0 時間」(前の版の停止の 1 枚が無い): `ZERO=%s`" % zero["path"])
    if pick_out:
        premise = None
        if zero:
            premise = "版 `%s`%s・起動 %s%s%s" % (
                zero["ver"] or "?", " (`%s`)" % zero["build"] if zero["build"] else "", started(zero),
                " (`boot.id` `%s`" % zero["boot"] if zero["boot"] else "",
                (", pid %s)" % zero["pid"] if zero["pid"] is not None else ")") if zero["boot"] else "")
        if before:
            premise = (premise + "・" if premise else "") + "前の版 `%s` の停止 %s" % (before["ver"] or "?", when(before))
        if zero:
            premise += "・" + later_period(ms, zero, started)
        with open(pick_out, "w", encoding="utf-8") as f:
            f.write("BEFORE=%s\nZERO=%s\nPREMISE=%s\n" % (before["path"] if before else "",
                                                         zero["path"] if zero else "", premise or ""))


def restart_kind(prev, m):
    """起動の 1 枚 `m` が、その 1 つ前の 1 枚 `prev` から見てどんな再起動か (T39.7)。

    `boot.id` は OS の起動ごとの id なので、変わればホストの再起動、同じならプロセスだけの再起動。
    どちらかに `boot.id` が無ければ分からない。前が停止の 1 枚でなければ、停止の 1 枚を書かずに止まった。
    """
    if m["boot"] and prev and prev["boot"]:
        if m["boot"] != prev["boot"]:
            kind = "ホストの再起動 (`boot.id` が `%s` に変わった" % m["boot"]
        else:
            kind = "プロセスだけの再起動 (`boot.id` は同じ `%s`" % m["boot"]
    else:
        kind = "再起動 (`boot.id` が読めず、ホストかプロセスだけかは分からない"
    if m["pid"] is not None:
        kind += "、pid %s" % m["pid"]
    if prev is None or prev["kind"] != "shutdown":
        kind += "。停止の 1 枚なし"
    return kind + ")"


def later_period(ms, zero, started):
    """締めの前提に足す「窓の途中の起動」と「後の期間の始まり」(T39.7)。

    0.38.10 の締め (T38.99) は前提に最初の起動しか書かず、11-20 18:54Z のホストの再起動と、後の期間の材料
    (`/profile`・`/recent`・起動からの数) がその後の 25.3 時間だけなことを手で足した。0 時間の起動より後の
    `start` の 1 枚を全部、ホストの再起動かプロセスだけかを添えて並べ、後の期間は最後の起動から数える。
    `CURRENT_STATUS` (`--closeout` が最初に取った `/status`) があれば、その `uptime_secs` で長さを書き、
    `boot` が最後の起動の 1 枚と違えば (起動の 1 枚の無い再起動) そう断る。
    """
    later = [m for m in ms if m["kind"] == "start" and m["at"] > zero["at"]]
    parts = []
    if later:
        items = []
        for m in later:
            i = ms.index(m)
            prev = ms[i - 1] if i > 0 else None
            ver = " 版 `%s`" % m["ver"] if m["ver"] and m["ver"] != zero["ver"] else ""
            items.append("%s%s に%s" % (started(m), ver, restart_kind(prev, m)))
        parts.append("**窓の途中の起動 %d 回**: %s" % (len(later), "、".join(items)))
    else:
        parts.append("窓の途中の起動なし")
    last = later[-1] if later else zero
    status = None
    path = os.environ.get("CURRENT_STATUS") or None
    if path:
        try:
            with open(path, encoding="utf-8") as f:
                status = json.load(f)
        except (OSError, ValueError):
            status = None
    if not isinstance(status, dict):
        status = {}
    up = status.get("uptime_secs")
    boot = status.get("boot") if isinstance(status.get("boot"), dict) else {}
    hours = " %.1f 時間" % (up / 3600) if isinstance(up, (int, float)) and not isinstance(up, bool) else ""
    origin = "0 時間の起動" if last is zero else "最後の起動"
    if boot.get("id") and last["boot"] and (boot.get("id") != last["boot"] or
                                            (boot.get("pid") is not None and last["pid"] is not None
                                             and boot.get("pid") != last["pid"])):
        parts.append("後の期間 (`/profile`・`/recent`・起動からの数) は `/status` の起動から%s "
                     "(`boot.id` `%s`, pid %s。手元の最後の起動の 1 枚と違う: 起動の 1 枚の無い再起動がある)"
                     % (hours, boot.get("id"), boot.get("pid")))
    else:
        parts.append("後の期間 (`/profile`・`/recent`・起動からの数) は %s %s から%s"
                     % (origin, started(last), hours))
    return "・".join(parts)


def follow_plan(argv):
    """スナップショットの切れた `recent`・`hosts`・`dns`・`profile` の続きの引き方を 1 部 1 行で出す (T22.5・T27.1)。"""
    # スナップショットがその部を組むときの引数 + `offset=` (`crates/endpoints/src/endpoints/recent/snapshot.rs` の `snapshot`)
    # `dns` と `profile` は T27.1 で足した (`/dns` の `offset=` は 0.27 から。0.25.4 には無い)
    URL = {"recent": "/recent?n=2000&offset=", "hosts": "/hosts?limit=1000&offset=",
           "dns": "/dns?sort=age&limit=4096&offset=", "profile": "/profile?res=5&offset="}
    # 部の中で行の並ぶ鍵 (`/dns` は `entries`、`/profile` は `samples`)
    ITEMS = {"recent": "recent", "hosts": "hosts", "dns": "entries", "profile": "samples"}
    d = json.load(open(argv[1]))
    for name, tmpl in URL.items():
        p = d.get(name)
        if not isinstance(p, dict) or not p.get("truncated"):
            continue
        if p.get("next_offset") is None:
            print("-", name, "この版のプロキシに offset= がありません")
            continue
        print("+", name, tmpl, p["next_offset"], len(p.get(ITEMS[name]) or []))


def next_offset(argv):
    """JSON の `next_offset` を出す (無ければ `null`)。続きの頁を引く 4 か所が使う。"""
    v = json.load(open(argv[1])).get("next_offset")
    print("null" if v is None else v)


def follow_join(argv):
    """追って取った続きの頁をスナップショットの同じ部に繋いで置き直し、要約の 1 行を出す (T22.5・T27.1・T30.2)。"""
    out, follow_ms, snap_ms, max_pages, follow_secs = argv[1:6]
    # `@<部>:<止めた理由>` のあとに、その部の続きの頁のファイルが並ぶ
    plan, cur = [], None
    for a in argv[6:]:
        if a.startswith("@"):
            name, why = a[1:].split(":", 1)
            cur = (name, why, [])
            plan.append(cur)
        else:
            cur[2].append(a)
    served_bytes = os.path.getsize(out)
    d = json.load(open(out))
    KEY = {"recent": "id", "hosts": "host", "dns": "host", "profile": 0}
    ITEMS = {"recent": "recent", "hosts": "hosts", "dns": "entries", "profile": "samples"}

    def key(name, r):
        # `profile` の標本は配列で、先頭が `t` (`keys` の 0 番目)。ほかは辞書の鍵
        if name == "profile":
            return r[0] if isinstance(r, list) and r else None
        return r.get(KEY[name]) if isinstance(r, dict) else None

    notes = []
    for name, why, files in plan:
        p = d.get(name)
        if why == "old":
            notes.append(f"`{name}` は切れていますが、この版のプロキシに `offset=` がありません (1 枚ぶんだけ)")
            continue
        rows = p.get(ITEMS[name]) or []
        first = len(rows)
        seen = {key(name, r) for r in rows}
        rest = p.get("next_offset")
        dup = 0
        pages = []
        for f in files:
            page = json.load(open(f))
            got = []
            for r in page.get(ITEMS[name]) or []:
                k = key(name, r)
                if k is not None and k in seen:
                    dup += 1
                    continue
                seen.add(k)
                got.append(r)
            pages.append(got)
            rest = page.get("next_offset")
        # `profile` の `offset` は新しい順に飛ばす本数で、各頁の中は古い順 = 後の頁ほど古いので**前に**繋ぐ
        # (`/profile?res=60` を繋ぐ T17.0b と同じ)。ほかは後ろに足す
        if name == "profile":
            rows = [r for got in reversed(pages) for r in got] + rows
        else:
            rows = rows + [r for got in pages for r in got]
        # 形は `/snapshot` の部のまま (鍵の順も変えない)。件数の欄だけ繋いだあとの値にする。
        # `recent` の `count` は「返した件数」(`shown` と同じ値) で、`hosts`・`dns`・`profile` の `count` は全件数
        p[ITEMS[name]] = rows
        p["shown"] = len(rows)
        if name == "recent":
            p["count"] = len(rows)
        # 最後の頁に続きが無ければ切れていない。止めたときは残りを指したまま
        p["truncated"] = rest is not None
        p["next_offset"] = rest
        total = p.get("matched") if name == "recent" else p.get("count")
        line = (f"`{name}` {len(files) + 1} 枚で {first:,} → {len(rows):,} 件"
                + (f" (全 {total:,} 件)" if isinstance(total, int) else "")
                + (f"、頁の重なり {dup} 件を落とした" if dup else ""))
        if why == "max_pages":
            line += f"。**打ち切った**: `MAX_PAGES={max_pages}` で止めた (残りは offset={rest} から)"
        elif why == "time":
            line += (f"。**打ち切った**: 続きを引く時間が `FOLLOW_SECS={follow_secs}` 秒を越えた"
                     f" (残りは offset={rest} から)")
        elif why == "failed":
            line += f"。**打ち切った**: 続きが取れなかった (残りは offset={rest} から)"
        notes.append(line)
    # 道具が繋いだ印 (T30.2): プロキシが返した 1 枚の大きさと、繋いだ部。要約の大きさの警告は
    # 繋いだあとの大きさではなく `served_bytes` で判定する (繋げば上限を越えるのは当たり前)
    d["joined"] = {"parts": [name for name, _, _ in plan], "served_bytes": served_bytes}
    tmp = out + ".part"
    with open(tmp, "w") as f:
        json.dump(d, f, ensure_ascii=False, separators=(",", ":"))
    os.replace(tmp, out)
    print("、".join(notes)
          + f" (続き {int(follow_ms) / 1000:.1f} 秒。`/snapshot` 自体は {int(snap_ms) / 1000:.1f} 秒)")


def profile_join(argv):
    """`/profile?res=60` の頁を 1 つに繋いで置き直し、要約の 1 行を出す (T17.0b)。"""
    out, paths = argv[1], argv[2:]
    docs = [json.load(open(p)) for p in paths]
    # 1 枚目 (いちばん新しい頁) の形 (`keys` / `roles` ほか) を土台に、`samples` だけ差し替える
    base = docs[0]
    seen = set()
    merged = []
    # 後の頁ほど古い。古い頁から順に足し、`t` (標本の先頭) が同じものは 2 本目を捨てる
    for d in reversed(docs):
        for s in d.get("samples") or []:
            t = s[0] if isinstance(s, list) and s else None
            if t is not None and t in seen:
                continue
            seen.add(t)
            merged.append(s)
    rest = docs[-1].get("next_offset")
    base["samples"] = merged
    base["shown"] = len(merged)
    # 最後の頁に続きが無ければ切れていない。取れなくなって止めたときは残りを指したまま
    base["truncated"] = rest is not None
    base["next_offset"] = rest
    tmp = out + ".part"
    with open(tmp, "w") as f:
        json.dump(base, f, ensure_ascii=False, separators=(",", ":"))
    os.replace(tmp, out)
    print(f"{len(docs)} 枚を繋いだ ({len(merged)} / {base.get('count')} 標本、"
          f"truncated={'true' if base['truncated'] else 'false'})")


def full_plan(argv):
    """`--full` で続きを引く部を 1 部 1 行で出す (T15.0)。"""
    # 続きを引ける部と、その URL の形 (スナップショットがその部を組むときの引数 + `offset=`)
    URL = {
        "recent": "/recent?n=2000&offset=",
        "hosts": "/hosts?limit=1000&offset=",
        "profile": "/profile?res=5&offset=",
    }
    try:
        d = json.load(open(argv[1]))
    except ValueError:
        sys.exit(0)
    for name, tmpl in URL.items():
        p = d.get(name)
        if not isinstance(p, dict) or not p.get("truncated"):
            continue
        nxt = p.get("next_offset")
        if nxt is None:                      # `offset=` を知らない古いプロキシ
            print("-", name, "この版のプロキシに offset= がありません")
            continue
        print("+", name, tmpl, nxt)
    # 切れているのに続きを引けない部は名前だけ知らせる
    for name, p in sorted(d.items()):
        if name in URL or not isinstance(p, dict):
            continue
        if p.get("truncated"):
            print("-", name, "この部に offset= はありません (1 枚ぶんだけです)")


def profile_hours(path):
    """`/profile?res=60` の 1 枚の長さ (時間。標本の数 × `interval_secs`)。読めなければ None。"""
    try:
        with open(path, encoding="utf-8") as f:
            p = json.load(f)
    except (OSError, ValueError):
        return None
    if not isinstance(p, dict):
        return None
    return len(p.get("samples") or []) * (p.get("interval_secs") or 0) / 3600


def local_long_profile(before, d):
    """`BEFORE` と同じ版の、12 時間以上ある最後の `-profile_res_60.json` (T32.29)。無ければ None。"""

    def version(path):
        try:
            with open(path, encoding="utf-8") as f:
                doc = json.load(f)
        except (OSError, ValueError):
            return None
        v = (doc.get("status") or {}).get("version") or doc.get("version")
        return v.split("+", 1)[0] if isinstance(v, str) and v else None

    want = version(before)
    if want:
        for name in sorted(os.listdir(d), reverse=True):
            m = re.match(r"^(\d{4}-\d{2}-\d{2}T\d{6}Z)-profile_res_60\.json$", name)
            if not m or version(os.path.join(d, m.group(1) + "-snapshot.json")) != want:
                continue
            h = profile_hours(os.path.join(d, name))
            if h is not None and h >= 12:
                return os.path.join(d, name)
    return None


def choose_profile_before(before, d):
    """締めの「前」(停止の 1 枚) に合わせる `--profile-before` と、選び方の説明 (T40.0)。

    1. 前の停止の隣の `/profile?res=60` (`<時刻>-shutdown-profile_res_60.json`。プロキシが止めるときに書いたもの)
       が 12 時間以上あればそれ
    2. 無いか短ければ、手元の前の版の 12 時間以上の最後の `-profile_res_60.json` (T32.29)
    3. それも無ければ、短くても停止の隣のもの
    どれも無ければ (None, 説明)。
    `PROFILE_BEFORE` で手で渡されたものがあれば、呼ぶ側がこの選び方より先にそれを使う (T40.1。取り寄せの箇条は
    `profile_before_note`、本番の 1 回は `collect-deployed.sh` の判定表の節)。
    """
    side = None
    m = re.match(r"^(.*)-shutdown-snapshot\.json$", before)
    if m and os.path.isfile(m.group(1) + "-shutdown-profile_res_60.json"):
        side = m.group(1) + "-shutdown-profile_res_60.json"
    side_h = profile_hours(side) if side else None
    if side and side_h is not None and side_h >= 12:
        return side, "前の停止の隣の `/profile?res=60` `%s` (%.1f 時間。T40.0) を選んだ" % (side, side_h)
    local = local_long_profile(before, d)
    if local:
        if side:
            why = "前の停止の隣の `/profile?res=60` は %.1f 時間しか無い" % (side_h or 0)
        else:
            why = "前の停止の隣の `/profile?res=60` は無い"
        return local, "手元の前の版の 12 時間以上の最後の `%s` (%.1f 時間。T32.29) を選んだ (%s)" % (
            local, profile_hours(local), why)
    if side:
        return side, ("前の停止の隣の `/profile?res=60` `%s` を選んだ (%.1f 時間で 12 時間に満たないが、"
                      "手元にも前の版の 12 時間以上の `-profile_res_60.json` が無い)" % (side, side_h or 0))
    return None, ("無い (前の停止の隣の `/profile?res=60` も、手元の前の版の 12 時間以上の "
                  "`-profile_res_60.json` も無い。判定表は `--profile-before` 無しで読む。"
                  "別の置き場に取ってあれば `PROFILE_BEFORE=<ファイル>` で手で渡せる)")


def judge_profile(argv):
    """`BEFORE` に合わせる `--profile-before` を選んで 1 行で出す (T32.29。T40.0 から停止の隣のものを先に見る)。"""
    path, _ = choose_profile_before(argv[1], argv[2])
    if path:
        print(path)


def given_profile(path):
    """手で渡された「前」の `/profile?res=60` (`PROFILE_BEFORE`。T40.1) が使えるかと、その中身か使えない理由。

    返すのは `(使えるか, 説明)`。使えるときの説明は長さ・標本の数・標本の時刻の端 (渡されたファイルには版も
    名前の時刻も無いので、どの期間のものかはここで確かめる)。12 時間に満たなければそう添える (判定表の CPU の行は
    前も後も 12 時間以上の材料が要る)。使えないときの説明は理由 (ファイルが無い・JSON として読めない・`/profile` の
    応答の形でない・標本が無い)。呼ぶ側が説明をそのまま出す (黙って無視しない)。
    """
    if not os.path.isfile(path):
        return False, "ファイルが無い"
    try:
        with open(path, encoding="utf-8") as f:
            p = json.load(f)
    except (OSError, ValueError):
        return False, "JSON として読めない"
    rows = p.get("samples") if isinstance(p, dict) else None
    interval = p.get("interval_secs") if isinstance(p, dict) else None
    if (not isinstance(rows, list) or not isinstance(interval, (int, float)) or isinstance(interval, bool)
            or interval <= 0):
        return False, "`/profile` の応答の形でない (`samples` と `interval_secs` が要る)"
    if not rows:
        return False, "標本が 1 本も無い"
    hours = len(rows) * interval / 3600
    text = "%.1f 時間、%s 標本" % (hours, format(len(rows), ","))
    keys = p.get("keys") or []
    ti = keys.index("t") if "t" in keys else None
    ends = [r[ti] for r in (rows[0], rows[-1])
            if ti is not None and isinstance(r, list) and ti < len(r) and isinstance(r[ti], (int, float))]
    if len(ends) == 2:
        text += "、標本は %s 〜 %s" % tuple(time.strftime("%Y-%m-%d %H:%MZ", time.gmtime(t)) for t in ends)
    if hours < 12:
        text += "。12 時間に満たない"
    return True, text


def profile_before_note(before, d):
    """取り寄せの箇条の「前の CPU の材料」の文 (T40.0)。`PROFILE_BEFORE` で手で渡されていれば、それを先に書く (T40.1)。

    手で渡されたものが使えれば「手で渡された」と、道具が選ぶはずだったもの (比べられるように) を、使えなければ理由と、
    渡されなかったときと同じ選び方を書く。本番の 1 回 (`collect-deployed.sh` の判定表の節) も同じ決まりで使う。
    """
    path, how = choose_profile_before(before, d)
    given = os.environ.get("PROFILE_BEFORE") or None
    if not given:
        return how
    ok, text = given_profile(given)
    if ok:
        return "手で渡された `%s` (%s。`PROFILE_BEFORE`。T40.1) を使う (道具が選ぶのは %s)" % (
            given, text, "`%s`" % path if path else "無し")
    return "**`PROFILE_BEFORE` で手で渡された `%s` は使えない** (%s)。渡されなかったときと同じに選ぶ: %s" % (
        given, text, how)


def given_profile_cmd(argv):
    """手で渡された「前」の `/profile?res=60` の中身か、使えない理由を 1 行で出す。使えなければ exit 1 (T40.1)。"""
    ok, text = given_profile(argv[1])
    print(text)
    return 0 if ok else 1


def dashboard_inputs(argv):
    """スナップショットから `check-dashboard.js` に渡す `/history?res=5` と `/status` を書き出す。"""
    d = json.load(open(argv[1]))
    json.dump(d.get("history", {}).get("5") or {}, open(argv[2], "w"))
    json.dump(d.get("status") or {}, open(argv[3], "w"))


COMMANDS = {
    "status-version": status_version,
    "snapshot-list": snapshot_list,
    "pick": pick,
    "follow-plan": follow_plan,
    "next-offset": next_offset,
    "follow-join": follow_join,
    "profile-join": profile_join,
    "full-plan": full_plan,
    "judge-profile": judge_profile,
    "given-profile": given_profile_cmd,
    "dashboard-inputs": dashboard_inputs,
}


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv or argv[0] not in COMMANDS:
        sys.exit("usage: collect_follow.py {%s} ARGS..." % ",".join(COMMANDS))
    return COMMANDS[argv[0]](argv)


if __name__ == "__main__":
    sys.exit(main())
