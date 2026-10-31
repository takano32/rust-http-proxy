#!/usr/bin/env python3
"""台帳の締め (`TASKS.md` の済んだ小タスクを `tasks/TNN.md` へ移す) を道具にしたもの (T.32.26)。

前はセッションの作業場に写し (`close_task29.py`〜`close_task31.py`) を置き、番号・題・日付・前のタスクの
正規表現を毎回書き換えていた。写しが直さないもの (`TODO.md` 冒頭の「T.15〜T.N」、`tasks/README.md` の
「タスク 0〜N」、全部済んだときの前置きの移動、§5 の「タスク N は全部済んだ」) は毎回手で直していた。

使い方 (リポジトリの根から。`-C DIR` で別の根):

  scripts/ledger-close.py T.32.5 結果.txt [--title 題] [--index 索引の題] [--date YYYY-MM-DD]
      `TASKS.md` の `- [ ] **T.32.5 …` とその下の字下げした行を `[x]` にし、`結果.txt` の行 (`  - 結果 (…): …`
      の形で書いておく) を足して `tasks/T32.md` の末尾へ移す。`tasks/T32.md` が無ければ 1 行目と前書きを作る
      (`--title` は 1 行目の題。省くと `TASKS.md` の `### タスク 32 (… に起こした: 題)` の題)。
      `TODO.md` の §5 の「済んだ N 本の本文は …」の行 (無ければタスク 31 の節の後ろに節ごと)、`tasks/README.md` の
      表の行 (`--index` は新しく作るときの説明。省くと `--title` と同じ)、範囲の印 2 つ (`TODO.md` 冒頭の
      「T.15〜T.N」と `tasks/README.md` の 1 行目の「タスク 0〜N」) を直す。

  scripts/ledger-close.py --finish 32 [--deployed X.Y.Z[+hash]] [--hours H] [--note 文] [--date YYYY-MM-DD]
      小タスクが全部済んだ (`TASKS.md` の節に `- [ ]` が残っていない) タスクを締める: 節の前置き (見出しと
      小タスク以外の段落) を `tasks/T32.md` の末尾の「## タスク 32 の前置きと判断の記録 (…)」へ移し
      (見出しは `####` に下げる)、`TASKS.md` から節を消し (ほかに開いているタスクが無ければ「なし (…)。」)、
      §5 の行の末尾に「タスク 32 は全部済んだ (日付、文)。」を足す (`--note` を省くと「T.32.99 で締めた」、
      `--deployed` なら「T.32.99 で X.Y.Z を締めた」)。`--deployed` を渡すと `TODO.md` §2 の
      「### デプロイ先の現在地 (…)」の見出しの日付・版 (と `--hours` の時間) と、次の段落の参照先
      (`tasks/TNN.md` の **T.N.99 の結果**) を直す (要点の数字は手で書く。最後に書き直す場所を出す)。

**移すときは本文を 1 文字も変えない。ただし相対リンクだけは、移した先 (`tasks/`) から辿れる形に書き換える**
(T.32.38。`TASKS.md` の `[x](tasks/T30.md)` は `tasks/T30.md` の中では `[x](T30.md)`。リンクの文字は変えない)。
`結果.txt` は移した先に置く文として書く (書き換えない)。

最後に `scripts/check-ledger.py` を回し、差分があれば 1 で抜ける (`--no-check` で回さない)。
終了コード: 0 = 締めた / 1 = check-ledger が差分を出した / 2 = 使い方の誤り・台帳の形が想定と違う。
外部のパッケージは使わない (標準ライブラリだけ)。
"""

import argparse
import datetime
import os
import posixpath
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

TID = re.compile(r"^T\.(\d+)\.(\d+)$")
SECTION = re.compile(r"^### タスク (\d+) \((\d{4}-\d{2}-\d{2}) に起こした: (.*)\)$")
LINK = re.compile(r"(\]\()([^)\s]+)(\))")
TODO_RANGE = re.compile(r"(済んだタスクのタスク本文 \(T\.15〜T\.)(\d+)(。)")
README_HEAD = re.compile(r"^(# tasks/ — 済んだタスクの本文 \(タスク 0〜)(\d+)(\))", re.M)
CURRENT_HEAD = re.compile(r"^### デプロイ先の現在地 \((\d{4}-\d{2}-\d{2})、(.*) に (\d+\.\d+\.\d+) をデプロイして "
                          r"([\d.]+) 時間\)$", re.M)
CURRENT_REF = re.compile(r"\[`tasks/T(\d+)\.md`\]\(tasks/T\d+\.md\) の \*\*T\.\d+\.99 の結果\*\*")


class Error(Exception):
    pass


def read(root, rel):
    with open(os.path.join(root, rel), encoding="utf-8") as f:
        return f.read()


def write(root, rel, text):
    with open(os.path.join(root, rel), "w", encoding="utf-8") as f:
        f.write(text)


def rewrite_links(text, src_dir, dst_dir):
    """`src_dir` (根からの相対。根は "") に置いてあった本文の相対リンクを、`dst_dir` から辿れる形にする (T.32.38)。"""
    if src_dir == dst_dir:
        return text

    def one(m):
        target = m.group(2)
        if re.match(r"^[a-z][a-z0-9+.-]*:", target) or target.startswith(("#", "/")):
            return m.group(0)
        path, sep, frag = target.partition("#")
        if not path:
            return m.group(0)
        full = posixpath.normpath(posixpath.join(src_dir, path))
        new = posixpath.relpath(full, dst_dir or ".")
        return m.group(1) + new + sep + frag + m.group(3)

    return LINK.sub(one, text)


def date_range(start, end):
    """「2026-10-30〜」(同じ日) / 「2026-10-30〜31」(同じ月) / 「2026-10-30〜11-02」/ 年をまたげば全部。"""
    if end <= start:
        return start + "〜"
    if end[:7] == start[:7]:
        return "%s〜%s" % (start, end[8:])
    if end[:4] == start[:4]:
        return "%s〜%s" % (start, end[5:])
    return "%s〜%s" % (start, end)


def section_of(lines, n):
    """`TASKS.md` の `### タスク n (…)` の節: (頭の行番号, 終わりの行番号 (次の見出し), 見出しの match)。"""
    for i, line in enumerate(lines):
        m = SECTION.match(line)
        if m and int(m.group(1)) == n:
            j = i + 1
            while j < len(lines) and not lines[j].startswith(("### ", "## ")):
                j += 1
            return i, j, m
    return None


def minor(tid):
    return int(tid.split(".")[2])


def done_ids(text, n):
    return sorted(set(re.findall(r"^- \[x\] \*\*(T\.%d\.\d+)" % n, text, re.M)), key=minor)


def todo_line(n, done, span):
    return ("**済んだ %d 本の本文は [`tasks/T%d.md`](tasks/T%d.md) に移した** (%s。%s、1 文字も変えていない)。"
            "未着手の残りは [`TASKS.md`](TASKS.md)。" % (len(done), n, n, span, "・".join(done)))


def bump_ranges(root, n):
    """範囲の印 2 つ (`TODO.md` 冒頭の「T.15〜T.N」と `tasks/README.md` の「タスク 0〜N」) を N まで広げる。"""
    o = read(root, "TODO.md")
    o2 = TODO_RANGE.sub(lambda m: m.group(1) + str(max(n, int(m.group(2)))) + m.group(3), o, count=1)
    if o2 != o:
        write(root, "TODO.md", o2)
    r = read(root, "tasks/README.md")
    r2 = README_HEAD.sub(lambda m: m.group(1) + str(max(n, int(m.group(2)))) + m.group(3), r, count=1)
    if r2 != r:
        write(root, "tasks/README.md", r2)


def close(root, tid, result, title=None, index=None, date=None):
    """1 本の小タスクを移す。移した先の相対パスを返す。"""
    m = TID.match(tid)
    if not m:
        raise Error("小タスクの番号は T.<タスク>.<番号> の形で渡す (例: T.32.5): %s" % tid)
    n = int(m.group(1))
    date = date or datetime.date.today().isoformat()
    t = read(root, "TASKS.md").split("\n")
    sec = section_of(t, n)
    if sec is None:
        raise Error("TASKS.md に「### タスク %d (YYYY-MM-DD に起こした: …)」の節が無い" % n)
    head_line, started, sec_title = sec[2].group(0), sec[2].group(2), sec[2].group(3)
    i = next((k for k, line in enumerate(t) if line.startswith("- [ ] **%s " % tid)), None)
    if i is None:
        raise Error("TASKS.md に未着手の「- [ ] **%s …」が無い" % tid)
    if not (sec[0] < i < sec[1]):
        raise Error("%s がタスク %d の節の外にある" % (tid, n))
    j = i + 1
    while j < len(t) and t[j].startswith("  "):
        j += 1
    block = t[i:j]
    block[0] = block[0].replace("- [ ] ", "- [x] ", 1)
    block = rewrite_links("\n".join(block), "", "tasks").split("\n")
    block += result.rstrip("\n").split("\n")
    del t[i:j]
    # 抜いたところの空行が 2 つ続いたら 1 つに (後ろに何か続くときだけ)
    if 0 < i < len(t) - 1 and t[i - 1] == "" and t[i] == "":
        del t[i]
    write(root, "TASKS.md", "\n".join(t))

    rel = "tasks/T%d.md" % n
    path = os.path.join(root, rel)
    if not os.path.exists(path):
        write(root, rel, "# タスク %d — 記録 (%s)\n\n> `TASKS.md` のタスク %d のうち、済んだものをここへ移した (%s)。"
                         "**本文は 1 文字も変えていない** (見出しの `[ ]` を `[x]` にし、`結果` を足しただけ)。\n"
              % (n, title or sec_title, n, date))
        index = index or title or sec_title
    u = read(root, rel).rstrip("\n") + "\n\n" + "\n".join(block) + "\n"
    write(root, rel, u)
    done = done_ids(u, n)

    # TODO.md §5 の行
    o = read(root, "TODO.md")
    line = todo_line(n, done, date_range(started, date))
    if head_line in o.split("\n"):
        o = re.sub(re.escape(head_line) + r"\n\n[^\n]*\n", lambda _: head_line + "\n\n" + line + "\n", o, count=1)
    else:
        prev = re.search(r"^### タスク %d \([^\n]*\n\n[^\n]*\n" % (n - 1), o, re.M)
        if prev is None:
            raise Error("TODO.md の §5 にタスク %d の節も、その前に置くタスク %d の節も無い" % (n, n - 1))
        o = o[:prev.end()] + "\n" + head_line + "\n\n" + line + "\n" + o[prev.end():]
    write(root, "TODO.md", o)

    # tasks/README.md の表の行
    r = read(root, "tasks/README.md")
    span = "%s〜%s" % (done[0], done[-1])
    row = re.compile(r"^\| T\.%d\.[^|]*\| \[T%d\.md\]\(T%d\.md\) \|" % (n, n, n), re.M)
    if row.search(r):
        r = row.sub(lambda _: "| %s | [T%d.md](T%d.md) |" % (span, n, n), r, count=1)
    else:
        if index is None:
            raise Error("tasks/README.md にタスク %d の行が無い (--index で説明を渡す)" % n)
        prev = re.search(r"^\| T\.%d\.[^\n]*$" % (n - 1), r, re.M)
        if prev is None:
            raise Error("tasks/README.md にタスク %d の行の前に置くタスク %d の行が無い" % (n, n - 1))
        new_row = "| %s | [T%d.md](T%d.md) | %s (%s) |" % (span, n, n, index, date)
        r = r[:prev.end()] + "\n" + new_row + r[prev.end():]
    write(root, "tasks/README.md", r)
    bump_ranges(root, n)
    return rel


def finish(root, n, deployed=None, hours=None, note=None, date=None, next_task=None):
    """全部済んだタスク n を締める。書き直しが手で要る場所の並びを返す。"""
    date = date or datetime.date.today().isoformat()
    t = read(root, "TASKS.md").split("\n")
    sec = section_of(t, n)
    if sec is None:
        raise Error("TASKS.md に「### タスク %d (…)」の節が無い (もう締めたか、番号の誤り)" % n)
    i, j, m = sec
    body = t[i:j]
    left = [line for line in body if line.startswith("- [ ] ")]
    if left:
        raise Error("タスク %d に未着手が %d 本残っている (先に移す): %s" % (n, len(left), left[0][:60]))
    head_line = m.group(0)
    rel = "tasks/T%d.md" % n
    if not os.path.exists(os.path.join(root, rel)):
        raise Error("%s が無い (小タスクを 1 本も移していない)" % rel)
    done = done_ids(read(root, rel), n)
    last = done[-1] if done else "T.%d.99" % n

    # 前置きを tasks/TNN.md の末尾へ (見出しは #### に下げる。相対リンクは移した先から辿れる形に)
    pre = list(body)
    while pre and pre[-1] == "":
        pre.pop()
    pre[0] = "#" + pre[0]
    text = rewrite_links("\n".join(pre), "", "tasks")
    u = read(root, rel).rstrip("\n")
    u += ("\n\n## タスク %d の前置きと判断の記録 (%s、全部済んだので TASKS.md から移した。1 文字も変えていない)\n\n"
          % (n, date)) + text + "\n"
    write(root, rel, u)

    # TASKS.md から節を消す。ほかに開いているタスクが無ければ「なし (…)。」
    del t[i:j]
    others = any(SECTION.match(line) for line in t)
    if not others:
        nxt = next_task if next_task is not None else n + 1
        t.insert(i, "なし (タスク %d は %s に %s で締めた。タスク %d はまだ起こしていない)。" % (n, date, last, nxt))
        if i + 1 >= len(t) or t[i + 1] != "":
            t.insert(i + 1, "")
    else:
        while i < len(t) and i > 0 and t[i - 1] == "" and t[i] == "":
            del t[i]
    write(root, "TASKS.md", "\n".join(t))

    # TODO.md §5 の行の末尾に「タスク N は全部済んだ」
    o = read(root, "TODO.md")
    if note is None:
        version = deployed.split("+")[0] if deployed else None
        note = "%s で %s を締めた" % (last, version) if version else "%s で締めた" % last
    mark = "タスク %d は全部済んだ (%s、%s)。" % (n, date, note)
    pat = re.compile(re.escape(head_line) + r"\n\n([^\n]*)\n")
    hit = pat.search(o)
    if hit is None:
        raise Error("TODO.md の §5 に「%s」の節が無い" % head_line)
    if "タスク %d は全部済んだ" % n not in hit.group(1):
        o = o[:hit.end(1)] + mark + o[hit.end(1):]
    todo_hand = []
    if deployed:
        version = deployed.split("+")[0]
        cur = CURRENT_HEAD.search(o)
        if cur is None:
            raise Error("TODO.md に「### デプロイ先の現在地 (日付、… に X.Y.Z をデプロイして H 時間)」が無い")
        new_head = "### デプロイ先の現在地 (%s、%s に %s をデプロイして %s 時間)" % (
            date, cur.group(2), version, hours if hours is not None else cur.group(4))
        o = o[:cur.start()] + new_head + o[cur.end():]
        ref = CURRENT_REF.search(o, cur.start())
        if ref is not None:
            o = o[:ref.start()] + ("[`tasks/T%d.md`](tasks/T%d.md) の **T.%d.99 の結果**" % (n, n, n)) + o[ref.end():]
        todo_hand.append("TODO.md §2「デプロイ先の現在地」の段落の雪像の名前・判定・要点 (見出しと参照先は直した)")
    write(root, "TODO.md", o)
    bump_ranges(root, n)
    return todo_hand


def check(root):
    tool = os.path.join(HERE, "check-ledger.py")
    out = subprocess.run([sys.executable, tool, root], stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                         universal_newlines=True)
    sys.stdout.write(out.stdout)
    return out.returncode


def main(argv=None):
    ap = argparse.ArgumentParser(description="台帳の締め (TASKS.md の済んだ小タスクを tasks/TNN.md へ移す。T.32.26)")
    ap.add_argument("-C", dest="root", default=os.path.dirname(HERE), help="リポジトリの根 (既定はこの 1 つ上)")
    ap.add_argument("tid", nargs="?", help="移す小タスク (T.32.5 など)")
    ap.add_argument("result", nargs="?", help="足す「結果」の行を書いたファイル")
    ap.add_argument("--title", help="tasks/TNN.md を新しく作るときの 1 行目の題")
    ap.add_argument("--index", help="tasks/README.md の表に新しく足す行の説明 (省くと --title)")
    ap.add_argument("--finish", type=int, metavar="N", help="全部済んだタスク N を締める")
    ap.add_argument("--deployed", metavar="X.Y.Z[+hash]", help="--finish: デプロイした版 (§2 の見出しを直す)")
    ap.add_argument("--hours", help="--finish --deployed: §2 の見出しの時間")
    ap.add_argument("--note", help="--finish: §5 の「全部済んだ (日付、…)」の文")
    ap.add_argument("--next", type=int, dest="next_task", help="--finish: 「タスク M はまだ起こしていない」の M")
    ap.add_argument("--date", help="書く日付 (既定は今日)")
    ap.add_argument("--no-check", action="store_true", help="最後に check-ledger.py を回さない")
    a = ap.parse_args(argv)
    try:
        if a.date and not re.match(r"^\d{4}-\d{2}-\d{2}$", a.date):
            raise Error("--date は YYYY-MM-DD: %s" % a.date)
        if a.finish is not None:
            if a.tid or a.result or a.title or a.index:
                raise Error("--finish は小タスクの番号・結果・--title・--index と一緒に使わない")
            if a.hours and not a.deployed:
                raise Error("--hours は --deployed と一緒に使う")
            hand = finish(a.root, a.finish, a.deployed, a.hours, a.note, a.date, a.next_task)
            print("締めた: タスク %d (前置きを tasks/T%d.md へ移した)" % (a.finish, a.finish))
            for h in hand:
                print("手で書く: %s" % h)
        else:
            if not (a.tid and a.result):
                raise Error("小タスクの番号と結果のファイルを渡す (か --finish N)")
            if a.deployed or a.hours or a.note or a.next_task is not None:
                raise Error("--deployed・--hours・--note・--next は --finish と一緒に使う")
            with open(a.result, encoding="utf-8") as f:
                result = f.read()
            if not result.strip():
                raise Error("結果のファイルが空: %s" % a.result)
            rel = close(a.root, a.tid, result, a.title, a.index, a.date)
            print("移した: %s → %s" % (a.tid, rel))
    except (Error, OSError) as e:
        print("ledger-close: %s" % e, file=sys.stderr)
        return 2
    if a.no_check:
        return 0
    return 1 if check(a.root) else 0


if __name__ == "__main__":
    sys.exit(main())
