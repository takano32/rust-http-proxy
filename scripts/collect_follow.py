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
    """標準入力の `/snapshots` の一覧を、1 行 1 枚 (`<日付> <バイト数> <置く名前>`) にする。名前は形を確かめてから使う。"""
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
        with open(pick_out, "w", encoding="utf-8") as f:
            f.write("BEFORE=%s\nZERO=%s\nPREMISE=%s\n" % (before["path"] if before else "",
                                                         zero["path"] if zero else "", premise or ""))


def follow_plan(argv):
    """雪像の切れた `recent`・`hosts`・`dns`・`profile` の続きの引き方を 1 部 1 行で出す (T22.5・T27.1)。"""
    # 雪像がその部を組むときの引数 + `offset=` (`crates/endpoints/src/endpoints/recent/snapshot.rs` の `snapshot`)
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
    """追って取った続きの頁を雪像の同じ部に繋いで置き直し、要約の 1 行を出す (T22.5・T27.1・T30.2)。"""
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
    """`--full` で続きを引く部を 1 部 1 行で出す (T15.0 (11))。"""
    # 続きを引ける部と、その URL の形 (雪像がその部を組むときの引数 + `offset=`)
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


def judge_profile(argv):
    """`BEFORE` と同じ版の、12 時間以上ある最後の `-profile_res_60.json` を選ぶ (T32.29)。"""
    before, d = argv[1], argv[2]

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
            try:
                with open(os.path.join(d, name), encoding="utf-8") as f:
                    p = json.load(f)
            except (OSError, ValueError):
                continue
            if len(p.get("samples") or []) * (p.get("interval_secs") or 0) >= 12 * 3600:
                print(os.path.join(d, name))
                break


def dashboard_inputs(argv):
    """雪像から `check-dashboard.js` に渡す `/history?res=5` と `/status` を書き出す。"""
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
    "dashboard-inputs": dashboard_inputs,
}


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv or argv[0] not in COMMANDS:
        sys.exit("usage: collect_follow.py {%s} ARGS..." % ",".join(COMMANDS))
    return COMMANDS[argv[0]](argv)


if __name__ == "__main__":
    sys.exit(main())
