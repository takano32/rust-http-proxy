#!/usr/bin/env python3
"""台帳 (`TASKS.md` の未着手 + `docs/task-index.md` の索引 + `tasks/TNN.md`) を機械で見る (T24.10 / T24.13).

2026-10-22 に済んだタスク 15〜23 の本文を `tasks/TNN.md` へ移し (T24.9)、同じ日に
**未着手のタスクを `TASKS.md` に集めた** (T24.13)。**移し忘れ・番号の重複・リンク切れ**は
目で見つからないので、`scripts/check-docs.sh` の (g) として毎回見る。見るのは 4 つと、
そこから出る足場 1 つ:

  (1) 移し忘れ … `TASKS.md` に `- [x]` が 1 本も無いこと (済んだら `tasks/TNN.md` へ移す)。
      移したタスク (`tasks/TNN.md` がある番号) のタスクが `TASKS.md` に残っていたら NG。
      索引 (`docs/task-index.md`) は見出しと索引だけなので、索引にタスクの行が 1 本でもあったら NG。
  (2) リンク … `tasks/TNN.md` と索引の `## タスク NN (…)` の見出しが 1 対 1 (両方向)。
      リンクの番号が見出しの番号と違う・指す先が無い・どの見出しからも指されていない、が NG。
  (3) 番号 … タスクの番号 (`T24.10`) が台帳全体 (`TASKS.md` + `tasks/`) で 1 回だけ。**同じ番号が
      2 度出るのは、片方が `結果` だけの追記** (本文を持たない行) のときだけ許す。番号のタスクが
      置き場所と合うこと (`TASKS.md` のタスク 24 の節なら `T24.x`、`tasks/T17.md` なら `T17.x`)。
  (4) 1 行目 … `tasks/TNN.md` の 1 行目が `# タスク NN — …`。
  (足) 見出しの「済んだ N 本の本文は …」の N が、`tasks/TNN.md` のタスクの本数と合うこと
      (T24.9 が番号ごとに書いたので、片方だけ足したときにここで出る)。
  (5) 索引と範囲 (T28.21) … `tasks/README.md` の表に全部の `TNN.md` の行がある、その見出しの
      「タスク 0〜N」と、`TODO.md` に「T15〜TN」の印があればその N が `tasks/` のいちばん大きい番号。
  (6) 済んだ節 (T28.21) … `TASKS.md` の `### タスク N (…)` の節が未着手を 1 本以上持つ
      (全部済んだら節ごと消す。2026-10-25 に済んだ節が 5 つ残っていた)。
  (7) 現在地の版 (T28.21) … `docs/status.md` の `## デプロイ先の現在地 (…)` の見出しの版が、
      `tasks/*.md` の「デプロイ済み」の行に出るいちばん新しい版 (`X.Y.Z+hash`) と同じ
      (締めのたびに現在地を書き直すのが 2 回続けて抜けたため)。
  (8) 参照 (T28.21) … `TODO.md`・`docs/*.md`・`tasks/*.md` の「未着手の Tn.m は …」が、`TASKS.md` に
      実在する未着手のタスクを指す (「」で引いた文は数えない)。
  (9) 相対リンク (T32.38) … `TASKS.md`・`TODO.md`・`docs/*.md`・`tasks/*.md` の `[…](相対パス)` の指す先のファイルが、
      **そのファイルの置き場所から辿って**ある (コードの囲みの中と `http:` などの URL は見ない)。
      `TASKS.md` から `tasks/` へ移すときに `tasks/T29.md` のまま移して切れたのが 2 本あった。

**意図的に外してあるものは下の除外表 (`SKIP_FILES`) に理由つきで持つ** (黙って無視しない)。
差分が 1 件でもあれば理由を印字して exit 1。使うのは標準ライブラリだけ (外部のパッケージは使わない)。
`TODO.md`・`docs/*.md`・`tasks/*.md` は**読むだけ** (この道具は 1 文字も書き換えない)。

**置き場は T37.5 で動いた**: 索引は `TODO.md` の章 (`## 5. …` の中の `### タスク NN (…)`) から `docs/task-index.md`
(`## タスク NN (…)`) へ、現在地は `TODO.md` の章から `docs/status.md` へ移した。`docs/task-index.md` が無い木
(T37.5 より前のコミット) では、今までどおり `TODO.md` の章を読む (過去の台帳を読み直す試験のため)。

使い方: scripts/check-ledger.py [リポジトリの根]   (引数なしならこのファイルの 2 つ上)
"""

import os
import re
import sys
import types

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

TODO = "TODO.md"
TASKS_MD = "TASKS.md"
TASKS = "tasks"
INDEX = "docs/task-index.md"    # 済んだタスクの見出しと索引 (T37.5 で TODO.md から移した)
STATUS = "docs/status.md"       # 「デプロイ先の現在地」の見出し (同じく T37.5)
DOCS = "docs"

# 除外表: `tasks/` の中でタスクの本文ではないもの (黙って無視しない)
SKIP_FILES = {
    "README.md": "索引 (`tasks/` の引き方と、どの番号がどのファイルかの表)。タスクの本文ではない",
}

# 索引のタスクの見出しは `## タスク NN (日付…` (`TASKS.md` と古い `TODO.md` は `###`) の形だけを台帳の節として扱う
# (`### タスク 15 の渡し方 (…)` のような指示文の節と分けるため、`(` までを形で縛る)。
PHASE_SECTION = re.compile(r"^#{2,3} タスク (\d+) \(")
LINK = re.compile(r"tasks/T(\d+)\.md")
LINK_COUNT = re.compile(r"済んだ (\d+) 本の本文は")
TASK_LINE = re.compile(r"^- \[([ x])\] \*\*(T(\d+)\.(\d+)[A-Za-z]?)\b(.*)$")
FILE_HEAD = re.compile(r"^# タスク (\d+) — ")
FILE_NAME = re.compile(r"^T(\d+)\.md$")
# 本文の箇条書きの名前 (`  - 結果 (2026-10-22、`abc1234`): …` の `結果`)。括弧やコロンは付けない。
BULLET = re.compile(r"^\s+- ([^\s(:：]{1,12})")
# `結果` だけの行は「あとから足した追記」で、タスクの本文ではない (番号の重複で数えない)
NOTE_ONLY = {"結果"}
MISSING_HEAD = "`# タスク NN — …`"
# (5)〜(8) の形 (T28.21)
README_HEAD = re.compile(r"^# tasks/ — 済んだタスクの本文 \(タスク 0〜(\d+)\)", re.M)
TODO_RANGE = re.compile(r"済んだタスクのタスク本文 \(T15〜T(\d+)。")
CURRENT_HEAD = re.compile(r"^#{2,3} デプロイ先の現在地 \((.*)\)\s*$", re.M)
VERSION = re.compile(r"(\d+)\.(\d+)\.(\d+)\+[0-9a-f]{7}")
HEAD_VERSION = re.compile(r"(\d+)\.(\d+)\.(\d+)")
OPEN_REF = re.compile(r"未着手の (T\d+\.\d+) は")
MD_LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")


class Task:
    """タスクの行 1 つ (`- [x] **T24.10 題名**` と、その下の箇条書きの名前)。"""

    __slots__ = ("where", "line", "status", "ident", "phase", "title", "labels")

    def __init__(self, where, line, status, ident, phase, title, labels):
        self.where = where
        self.line = line
        self.status = status
        self.ident = ident
        self.phase = phase
        self.title = title
        self.labels = labels

    @property
    def done(self):
        return self.status == "x"

    @property
    def has_body(self):
        """本文を持つか (`結果` 以外の箇条書きがあるか)。"""
        return bool(self.labels - NOTE_ONLY)

    def at(self):
        return "%s:%d" % (self.where, self.line)


class Ledger:
    """数えた結果 (印字する側が使う)。"""

    def __init__(self):
        self.sections = 0     # 索引の `## タスク NN (…)` の数
        self.files = 0        # tasks/TNN.md の数
        self.filed = 0        # tasks/ 側のタスクの本数
        self.inline = 0       # 索引に残るタスクの本数
        self.inline_done = 0  # そのうち `- [x]`
        self.open = 0         # TASKS.md の未着手のタスクの本数
        self.open_done = 0    # そのうち `- [x]` (0 であるべき)
        self.skipped = []     # (ファイル名, 理由)
        self.notes = []       # 気づき (NG ではない)
        self.ngs = []         # NG の文面
        self.index = INDEX    # 索引の置き場 (印字用。古い木では `TODO.md のタスクの索引`)


def read_lines(path):
    with open(path, encoding="utf-8") as f:
        return f.read().splitlines()


def section5(lines):
    """`## 5. …` から次の `## ` までを (0 起算の行番号, 行) の列で返す。"""
    start = None
    for i, line in enumerate(lines):
        if line.startswith("## 5. "):
            start = i
            break
    if start is None:
        return []
    out = []
    for i in range(start + 1, len(lines)):
        if lines[i].startswith("## "):
            break
        out.append((i, lines[i]))
    return out


def phase_sections(block):
    """索引を `## タスク NN (…)` の見出しごとに割る。[(番号, 見出しの行番号, 節の行)]"""
    sections = []
    cur = None
    for i, line in block:
        m = PHASE_SECTION.match(line)
        if m:
            cur = (int(m.group(1)), i, [])
            sections.append(cur)
        elif line.startswith("#"):
            cur = None
        elif cur is not None:
            cur[2].append((i, line))
    return sections


def tasks_in(lines, where):
    """行の列からタスクの行を拾う (`where` は印字用の置き場所)。"""
    out = []
    for i, (_, line) in enumerate(lines):
        m = TASK_LINE.match(line)
        if not m:
            continue
        # 本文は次のタスクの行か見出しかコメントの行 (`<!-- … -->`) まで
        body = []
        for _, nxt in lines[i + 1:]:
            if TASK_LINE.match(nxt) or nxt.startswith("#") or nxt.lstrip().startswith("<!--"):
                break
            body.append(nxt)
        labels = set()
        for text in body:
            hit = BULLET.match(text)
            if hit:
                labels.add(hit.group(1))
        out.append(Task(where=where, line=lines[i][0] + 1, status=m.group(1), ident=m.group(2),
                        phase=int(m.group(3)), title=m.group(4).strip().strip("*").strip(),
                        labels=labels))
    return out


def collect(root):
    """台帳を読んで `Ledger` を返す (読み取りだけ)。

    読む (`read_ledger`)・移し忘れとリンク (`check_moves_and_links`)・番号の重複と本数
    (`check_numbers`)・索引と範囲と参照先 (`check_index_and_refs`) の 4 段 (T34.137 で割った)。
    NG と注の並びは割る前と同じ (段を同じ順に回す)。
    """
    led = Ledger()
    t = read_ledger(led, root)
    check_moves_and_links(led, t)
    check_numbers(led, t)
    check_index_and_refs(led, t)
    return led


def read_ledger(led, root):
    """`tasks/TNN.md`・索引 (`docs/task-index.md`)・`TASKS.md` を読み、形の NG を `led` に足す。

    返すのは後の段が読む材料 (`types.SimpleNamespace`)。`docs/task-index.md` が無い古い木では
    `TODO.md` の `## 5. …` の章を索引として読み、現在地も `TODO.md` から読む。
    """
    t = types.SimpleNamespace(root=root)
    t.todo = todo = read_lines(os.path.join(root, TODO))
    if os.path.isfile(os.path.join(root, INDEX)):
        block = list(enumerate(read_lines(os.path.join(root, INDEX))))
        led.index = INDEX
    else:
        block = section5(todo)
        led.index = "%s のタスクの索引" % TODO
    t.status_name = STATUS if os.path.isfile(os.path.join(root, STATUS)) else TODO
    t.status = read_lines(os.path.join(root, t.status_name)) if t.status_name != TODO else todo
    sections = phase_sections(block)
    led.sections = len(sections)
    t.bodies = {num: body for num, _, body in sections}

    # --- tasks/ のファイル ---------------------------------------------------
    t.tdir = tdir = os.path.join(root, TASKS)
    t.names = names = {}
    for name in sorted(os.listdir(tdir)):
        if not name.endswith(".md"):
            continue
        m = FILE_NAME.match(name)
        if m:
            names[int(m.group(1))] = name
        elif name in SKIP_FILES:
            led.skipped.append((name, SKIP_FILES[name]))
        else:
            led.ngs.append("tasks/%s が台帳の型 `TNN.md` に合わない (索引なら除外表に理由を書く)" % name)
    led.files = len(names)

    t.filed = filed = {}
    for num, name in sorted(names.items()):
        lines = read_lines(os.path.join(tdir, name))
        head = lines[0] if lines else ""
        m = FILE_HEAD.match(head)
        if not m:
            led.ngs.append("tasks/%s の 1 行目が %s でない: %s" % (name, MISSING_HEAD, head or "(空)"))
        elif int(m.group(1)) != num:
            led.ngs.append("tasks/%s の 1 行目がタスク %s になっている (ファイル名は %d)"
                           % (name, m.group(1), num))
        filed[num] = tasks_in(list(enumerate(lines)), "tasks/" + name)
        led.filed += len(filed[num])
        for task in filed[num]:
            if task.phase != num:
                led.ngs.append("%s の番号 %s がタスク %d のファイルにある"
                               % (task.at(), task.ident, num))

    # --- 索引の節 -------------------------------------------------------------
    t.inline = inline = {}
    t.links = links = {}
    for num, _, body in sections:
        inline[num] = tasks_in(body, "%s タスク %d" % (led.index, num))
        led.inline += len(inline[num])
        led.inline_done += sum(1 for task in inline[num] if task.done)
        # 同じ行に `[`tasks/TNN.md`](tasks/TNN.md)` と 2 度出るので畳む
        links[num] = sorted({int(x) for x in LINK.findall("\n".join(text for _, text in body))})
        for task in inline[num]:
            if task.phase != num:
                led.ngs.append("%s の番号 %s がタスク %d の節にある"
                               % (task.at(), task.ident, num))

    # --- (1) の材料: `TASKS.md` (未着手) を先に見る。無い木 (古い形) では索引の行を未着手として見る。
    t.has_tasks_md = os.path.isfile(os.path.join(root, TASKS_MD))
    t.open_tasks = open_tasks = {}
    if t.has_tasks_md:
        tpath = os.path.join(root, TASKS_MD)
        for num, _, body in phase_sections(list(enumerate(read_lines(tpath)))):
            open_tasks[num] = tasks_in(body, "%s タスク %d" % (TASKS_MD, num))
            led.open += len(open_tasks[num])
            led.open_done += sum(1 for task in open_tasks[num] if task.done)
            for task in open_tasks[num]:
                if task.phase != num:
                    led.ngs.append("%s の番号 %s がタスク %d の節にある"
                                   % (task.at(), task.ident, num))
    return t


def check_moves_and_links(led, t):
    """(1) 移し忘れと (2) リンク (両方向)。"""
    names, filed, inline, links, open_tasks = t.names, t.filed, t.inline, t.links, t.open_tasks
    # --- (1) 移し忘れ ---------------------------------------------------------
    if t.has_tasks_md:
        for num in sorted(open_tasks):
            done = [task for task in open_tasks[num] if task.done]
            if done:
                led.ngs.append("%s のタスク %d に [x] が %d 本ある (済んだタスクは tasks/T%d.md へ移す)"
                               % (TASKS_MD, num, len(done), num))
            if num in names:
                # 同じタスクに「済み (tasks/TNN.md)」と「未着手 (TASKS.md)」が並ぶのは
                # 正しい形 (タスクの途中で締めの 1 本だけ残る)。印字だけする。
                led.notes.append("タスク %d は tasks/%s (済み %d 本) と %s (未着手 %d 本) に分かれている"
                                 % (num, names[num], len(filed.get(num, [])), TASKS_MD,
                                    len(open_tasks[num])))
    else:
        led.notes.append("%s が無い木 (未着手のタスクは %s にある形として見る)" % (TASKS_MD, led.index))

    for num in sorted(inline):
        if not inline[num]:
            continue
        done = sum(1 for task in inline[num] if task.done)
        if num in names:
            led.ngs.append("タスク %d は tasks/%s へ移したのに %s にタスクの行が %d 本 (うち [x] %d 本) 残っている"
                           % (num, names[num], led.index, len(inline[num]), done))
        elif done == len(inline[num]):
            led.ngs.append("タスク %d のタスク %d 本が全部 [x] なのに %s に残っている (tasks/T%d.md へ移す)"
                           % (num, len(inline[num]), led.index, num))
    rest = sorted(num for num in inline if inline[num] and num not in names)
    if rest:
        led.notes.append("まだ tasks/ へ移していないタスク: " + " / ".join(
            "タスク %d (タスク %d 本、[x] %d 本)"
            % (num, len(inline[num]), sum(1 for task in inline[num] if task.done)) for num in rest))

    # --- (2) リンク (両方向) --------------------------------------------------
    for num in sorted(links):
        for linked in links[num]:
            if linked != num:
                led.ngs.append("%s のタスク %d の見出しが tasks/T%d.md を指している" % (led.index, num, linked))
            if linked not in names:
                led.ngs.append("%s のタスク %d の見出しが指す tasks/T%d.md が無い" % (led.index, num, linked))
        if num in names and num not in links[num]:
            led.ngs.append("tasks/%s が %s のタスク %d の見出しからリンクされていない"
                           % (names[num], led.index, num))
    for num in sorted(names):
        if num not in links:
            led.ngs.append("tasks/%s を指す「タスク %d (…)」の見出しが %s に無い"
                           % (names[num], num, led.index))


def check_numbers(led, t):
    """(3) 番号の重複と、(足) 見出しの「済んだ N 本」。"""
    names, filed, inline, open_tasks = t.names, t.filed, t.inline, t.open_tasks
    # --- (3) 番号の重複 -------------------------------------------------------
    all_tasks = [task for num in sorted(inline) for task in inline[num]]
    all_tasks += [task for num in sorted(open_tasks) for task in open_tasks[num]]
    all_tasks += [task for num in sorted(filed) for task in filed[num]]
    seen = {}
    for task in all_tasks:
        seen.setdefault(task.ident, []).append(task)
    for ident in sorted(seen):
        tasks = seen[ident]
        with_body = [task for task in tasks if task.has_body]
        if len(with_body) > 1:
            led.ngs.append("番号 %s が本文つきで %d 回出る (%s)"
                           % (ident, len(with_body), ", ".join(task.at() for task in with_body)))
        elif not with_body and len(tasks) > 1:
            led.ngs.append("番号 %s が %d 回出る (どれも本文を持たない。%s)"
                           % (ident, len(tasks), ", ".join(task.at() for task in tasks)))
        elif len(tasks) > 1:
            led.notes.append("同じ番号が %d 回出る (%s)。1 行は `結果` だけの追記なので重複と数えない"
                             % (len(tasks), ", ".join(task.at() for task in tasks)))

    # --- (足) 見出しの「済んだ N 本」 ------------------------------------------
    for num in sorted(names):
        text = "\n".join(line for _, line in t.bodies.get(num, []))
        m = LINK_COUNT.search(text)
        if not m:
            led.ngs.append("%s のタスク %d の見出しに「済んだ N 本の本文は …」の行が無い" % (led.index, num))
        elif int(m.group(1)) != len(filed.get(num, [])):
            led.ngs.append("%s のタスク %d の「済んだ %s 本」が tasks/%s の %d 本と合わない"
                           % (led.index, num, m.group(1), names[num], len(filed.get(num, []))))


def check_index_and_refs(led, t):
    """(5) 索引と範囲・(6) 済んだ節の残り・(7) 現在地の版・(8) 「未着手の Tn.m」の参照先・(9) 相対リンク。"""
    root, tdir, todo, names, open_tasks = t.root, t.tdir, t.todo, t.names, t.open_tasks
    # --- (5) 索引と範囲 (T28.21) ------------------------------------------------
    top = max(names) if names else None
    readme = os.path.join(tdir, "README.md")
    if top is not None and os.path.isfile(readme):
        text = "\n".join(read_lines(readme))
        for num, name in sorted(names.items()):
            if "](%s)" % name not in text:
                led.ngs.append("tasks/README.md の表に tasks/%s の行が無い" % name)
        m = README_HEAD.search(text)
        if m and int(m.group(1)) != top:
            led.ngs.append("tasks/README.md の見出しが「タスク 0〜%s」(いちばん大きいのは %d)"
                           % (m.group(1), top))
    todo_text = "\n".join(todo)
    m = TODO_RANGE.search(todo_text)
    if m and top is not None and int(m.group(1)) != top:
        led.ngs.append("TODO.md 冒頭の表が「T15〜T%s」(いちばん大きいのは %d)" % (m.group(1), top))

    # --- (6) 済んだ節が TASKS.md に残っていない (T28.21) -------------------------
    for num in sorted(open_tasks):
        if not open_tasks[num]:
            led.ngs.append("%s のタスク %d の節に未着手が 0 本 (全部済んだら節ごと消す)" % (TASKS_MD, num))

    # --- (7) 現在地 (`docs/status.md`。古い木では `TODO.md`) の版 (T28.21) ---------------
    deployed = None
    for name in names.values():
        for line in read_lines(os.path.join(tdir, name)):
            if "デプロイ済み" not in line:
                continue
            for v in VERSION.findall(line):
                v = tuple(int(x) for x in v)
                deployed = v if deployed is None or v > deployed else deployed
    m = CURRENT_HEAD.search("\n".join(t.status))
    if m and deployed is not None:
        hv = HEAD_VERSION.findall(m.group(1))
        have = tuple(int(x) for x in hv[-1]) if hv else None
        if have != deployed:
            led.ngs.append("%s の「デプロイ先の現在地」の版が %s (tasks/ のいちばん新しい"
                           "「デプロイ済み」は %s)"
                           % (t.status_name, ".".join(map(str, have)) if have else "(無い)",
                              ".".join(map(str, deployed))))

    # --- (8) 「未着手の Tn.m は …」の参照先 (T28.21) -------------------------------
    open_ids = {task.ident for num in open_tasks for task in open_tasks[num] if not task.done}
    sources = [(TODO, todo)] + doc_sources(root) + [("tasks/" + name, read_lines(os.path.join(tdir, name)))
                                                    for name in names.values()]
    for where, lines in sources:
        for i, line in enumerate(lines):
            for m in OPEN_REF.finditer(line):
                if m.start() > 0 and line[m.start() - 1] == "「":
                    continue
                if m.group(1) not in open_ids:
                    led.ngs.append("%s:%d の「未着手の %s」が %s に無い" % (where, i + 1, m.group(1), TASKS_MD))

    # --- (9) 相対リンクが置き場所から辿れる (T32.38) ---------------------------------
    sources = [(TODO, todo)] + doc_sources(root) + [("tasks/" + name, read_lines(os.path.join(tdir, name)))
                                                    for name in sorted(os.listdir(tdir)) if name.endswith(".md")]
    if os.path.exists(os.path.join(root, TASKS_MD)):
        sources.insert(0, (TASKS_MD, read_lines(os.path.join(root, TASKS_MD))))
    for where, lines in sources:
        for i, target in relative_links(lines):
            path = target.partition("#")[0]
            full = os.path.normpath(os.path.join(root, os.path.dirname(where), path))
            if not os.path.exists(full):
                base = os.path.dirname(where)
                led.ngs.append("%s:%d のリンク %s の指す先が無い (%s辿る。移したなら移した先から辿れる形に)"
                               % (where, i + 1, target, base + "/ から" if base else "根から"))


def doc_sources(root):
    """`docs/*.md` を [(置き場, 行の列)] で返す (T37.5 で `TODO.md` の章を移した先。無い木では空)。"""
    ddir = os.path.join(root, DOCS)
    if not os.path.isdir(ddir):
        return []
    return [(DOCS + "/" + name, read_lines(os.path.join(ddir, name)))
            for name in sorted(os.listdir(ddir)) if name.endswith(".md")]


def relative_links(lines):
    """[(行番号 0 始まり, 相対リンクの先)]。コードの囲みの中・URL・ページ内 (`#…`) は外す。"""
    out = []
    fence = False
    for i, line in enumerate(lines):
        if line.startswith("```"):
            fence = not fence
            continue
        if fence:
            continue
        # インラインのコード (`[x](y)` と書いた例) は外す。リンクの文字が `…` でも `[](先)` として残る
        for m in MD_LINK.finditer(re.sub(r"`[^`]*`", "", line)):
            target = m.group(1)
            if re.match(r"^[a-z][a-z0-9+.-]*:", target) or target.startswith(("#", "/")):
                continue
            out.append((i, target))
    return out


def main(argv):
    root = argv[1] if len(argv) > 1 else ROOT
    try:
        led = collect(root)
    except OSError as exc:
        print("  NG (g): 台帳が読めない: %s" % exc)
        return 1
    print("(g) 台帳: %s の未着手 %d 本 (うち [x] %d 本) / %s のタスク見出し %d 節 / "
          "tasks/TNN.md %d 本 (タスク %d 本) / 索引に残るタスク %d 本"
          % (TASKS_MD, led.open, led.open_done, led.index, led.sections, led.files, led.filed, led.inline))
    for message in led.ngs:
        print("  NG (g): " + message)
    for name, why in led.skipped:
        print("    除外: tasks/%s — %s" % (name, why))
    for note in led.notes:
        print("    注: " + note)
    return 1 if led.ngs else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
