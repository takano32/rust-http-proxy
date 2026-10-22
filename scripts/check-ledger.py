#!/usr/bin/env python3
"""台帳 (`TASKS.md` の未着手 + `TODO.md` の §5 の索引 + `tasks/phaseNN.md`) を機械で見る (T24.10 / T24.13).

2026-10-22 に済んだ Phase 15〜23 の本文を `tasks/phaseNN.md` へ移し (T24.9)、同じ日に
**未着手のタスクを `TASKS.md` に集めた** (T24.13)。**移し忘れ・番号の重複・リンク切れ**は
目で見つからないので、`scripts/check-docs.sh` の (g) として毎回見る。見るのは 4 つと、
そこから出る足場 1 つ:

  (1) 移し忘れ … `TASKS.md` に `- [x]` が 1 本も無いこと (済んだら `tasks/phaseNN.md` へ移す)。
      移した Phase (`tasks/phaseNN.md` がある番号) のタスクが `TASKS.md` に残っていたら NG。
      `TODO.md` の §5 は索引だけなので、§5 にタスクの行が 1 本でもあったら NG。
  (2) リンク … `tasks/phaseNN.md` と §5 の `### Phase NN (…)` の見出しが 1 対 1 (両方向)。
      リンクの番号が見出しの番号と違う・指す先が無い・どの見出しからも指されていない、が NG。
  (3) 番号 … タスクの番号 (`T24.10`) が台帳全体 (`TASKS.md` + `tasks/`) で 1 回だけ。**同じ番号が
      2 度出るのは、片方が `結果` だけの追記** (本文を持たない行) のときだけ許す。番号の Phase が
      置き場所と合うこと (`TASKS.md` の Phase 24 の節なら `T24.x`、`tasks/phase17.md` なら `T17.x`)。
  (4) 1 行目 … `tasks/phaseNN.md` の 1 行目が `# Phase NN — …`。
  (足) 見出しの「済んだ N 本の本文は …」の N が、`tasks/phaseNN.md` のタスクの本数と合うこと
      (T24.9 が番号ごとに書いたので、片方だけ足したときにここで出る)。

**意図的に外してあるものは下の除外表 (`SKIP_FILES`) に理由つきで持つ** (黙って無視しない)。
差分が 1 件でもあれば理由を印字して exit 1。使うのは標準ライブラリだけ (外部のパッケージは使わない)。
`TODO.md` と `tasks/*.md` は**読むだけ** (この道具は 1 文字も書き換えない)。

使い方: scripts/check-ledger.py [リポジトリの根]   (引数なしならこのファイルの 2 つ上)
"""

import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

TODO = "TODO.md"
TASKS_MD = "TASKS.md"
TASKS = "tasks"

# 除外表: `tasks/` の中で Phase の本文ではないもの (黙って無視しない)
SKIP_FILES = {
    "README.md": "索引 (`tasks/` の引き方と、どの番号がどのファイルかの表)。Phase の本文ではない",
}

# §5 の Phase の見出しは `### Phase NN (日付…` の形だけを台帳の節として扱う
# (`### Phase 15 の渡し方 (…)` のような指示文の節と分けるため、`(` までを形で縛る)。
PHASE_SECTION = re.compile(r"^### Phase (\d+) \(")
LINK = re.compile(r"tasks/phase(\d+)\.md")
LINK_COUNT = re.compile(r"済んだ (\d+) 本の本文は")
TASK_LINE = re.compile(r"^- \[([ x])\] \*\*(T(\d+)\.(\d+)[A-Za-z]?)\b(.*)$")
FILE_HEAD = re.compile(r"^# Phase (\d+) — ")
FILE_NAME = re.compile(r"^phase(\d+)\.md$")
# 本文の箇条書きの名前 (`  - 結果 (2026-10-22、`abc1234`): …` の `結果`)。括弧やコロンは付けない。
BULLET = re.compile(r"^\s+- ([^\s(:：]{1,12})")
# `結果` だけの行は「あとから足した追記」で、タスクの本文ではない (番号の重複で数えない)
NOTE_ONLY = {"結果"}
MISSING_HEAD = "`# Phase NN — …`"


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
        self.sections = 0     # §5 の `### Phase NN (…)` の数
        self.files = 0        # tasks/phaseNN.md の数
        self.filed = 0        # tasks/ 側のタスクの本数
        self.inline = 0       # §5 に残るタスクの本数
        self.inline_done = 0  # そのうち `- [x]`
        self.open = 0         # TASKS.md の未着手のタスクの本数
        self.open_done = 0    # そのうち `- [x]` (0 であるべき)
        self.skipped = []     # (ファイル名, 理由)
        self.notes = []       # 気づき (NG ではない)
        self.ngs = []         # NG の文面


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
    """§5 を `### Phase NN (…)` の見出しごとに割る。[(番号, 見出しの行番号, 節の行)]"""
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
    """台帳を読んで `Ledger` を返す (読み取りだけ)。"""
    led = Ledger()
    todo = read_lines(os.path.join(root, TODO))
    sections = phase_sections(section5(todo))
    led.sections = len(sections)
    bodies = {num: body for num, _, body in sections}

    # --- tasks/ のファイル ---------------------------------------------------
    tdir = os.path.join(root, TASKS)
    names = {}
    for name in sorted(os.listdir(tdir)):
        if not name.endswith(".md"):
            continue
        m = FILE_NAME.match(name)
        if m:
            names[int(m.group(1))] = name
        elif name in SKIP_FILES:
            led.skipped.append((name, SKIP_FILES[name]))
        else:
            led.ngs.append("tasks/%s が台帳の型 `phaseNN.md` に合わない (索引なら除外表に理由を書く)" % name)
    led.files = len(names)

    filed = {}
    for num, name in sorted(names.items()):
        lines = read_lines(os.path.join(tdir, name))
        head = lines[0] if lines else ""
        m = FILE_HEAD.match(head)
        if not m:
            led.ngs.append("tasks/%s の 1 行目が %s でない: %s" % (name, MISSING_HEAD, head or "(空)"))
        elif int(m.group(1)) != num:
            led.ngs.append("tasks/%s の 1 行目が Phase %s になっている (ファイル名は %d)"
                           % (name, m.group(1), num))
        filed[num] = tasks_in(list(enumerate(lines)), "tasks/" + name)
        led.filed += len(filed[num])
        for task in filed[num]:
            if task.phase != num:
                led.ngs.append("%s の番号 %s が Phase %d のファイルにある"
                               % (task.at(), task.ident, num))

    # --- §5 の節 -------------------------------------------------------------
    inline = {}
    links = {}
    for num, _, body in sections:
        inline[num] = tasks_in(body, "%s §5 Phase %d" % (TODO, num))
        led.inline += len(inline[num])
        led.inline_done += sum(1 for task in inline[num] if task.done)
        # 同じ行に `[`tasks/phaseNN.md`](tasks/phaseNN.md)` と 2 度出るので畳む
        links[num] = sorted({int(x) for x in LINK.findall("\n".join(text for _, text in body))})
        for task in inline[num]:
            if task.phase != num:
                led.ngs.append("%s の番号 %s が Phase %d の節にある"
                               % (task.at(), task.ident, num))

    # --- (1) 移し忘れ ---------------------------------------------------------
    # `TASKS.md` (未着手) を先に見る。無い木 (古い形) では §5 の行を未着手として見る。
    tpath = os.path.join(root, TASKS_MD)
    open_tasks = {}
    if os.path.isfile(tpath):
        for num, _, body in phase_sections(list(enumerate(read_lines(tpath)))):
            open_tasks[num] = tasks_in(body, "%s Phase %d" % (TASKS_MD, num))
            led.open += len(open_tasks[num])
            led.open_done += sum(1 for task in open_tasks[num] if task.done)
            for task in open_tasks[num]:
                if task.phase != num:
                    led.ngs.append("%s の番号 %s が Phase %d の節にある"
                                   % (task.at(), task.ident, num))
        for num in sorted(open_tasks):
            done = [task for task in open_tasks[num] if task.done]
            if done:
                led.ngs.append("%s の Phase %d に [x] が %d 本ある (済んだタスクは tasks/phase%d.md へ移す)"
                               % (TASKS_MD, num, len(done), num))
            if num in names:
                # 同じ Phase に「済み (tasks/phaseNN.md)」と「未着手 (TASKS.md)」が並ぶのは
                # 正しい形 (Phase の途中で締めの 1 本だけ残る)。印字だけする。
                led.notes.append("Phase %d は tasks/%s (済み %d 本) と %s (未着手 %d 本) に分かれている"
                                 % (num, names[num], len(filed.get(num, [])), TASKS_MD,
                                    len(open_tasks[num])))
    else:
        led.notes.append("%s が無い木 (未着手のタスクは TODO.md の §5 にある形として見る)" % TASKS_MD)

    for num in sorted(inline):
        if not inline[num]:
            continue
        done = sum(1 for task in inline[num] if task.done)
        if num in names:
            led.ngs.append("Phase %d は tasks/%s へ移したのに §5 にタスクの行が %d 本 (うち [x] %d 本) 残っている"
                           % (num, names[num], len(inline[num]), done))
        elif done == len(inline[num]):
            led.ngs.append("Phase %d のタスク %d 本が全部 [x] なのに §5 に残っている (tasks/phase%d.md へ移す)"
                           % (num, len(inline[num]), num))
    rest = sorted(num for num in inline if inline[num] and num not in names)
    if rest:
        led.notes.append("まだ tasks/ へ移していない Phase: " + " / ".join(
            "Phase %d (タスク %d 本、[x] %d 本)"
            % (num, len(inline[num]), sum(1 for task in inline[num] if task.done)) for num in rest))

    # --- (2) リンク (両方向) --------------------------------------------------
    for num in sorted(links):
        for linked in links[num]:
            if linked != num:
                led.ngs.append("§5 の Phase %d の見出しが tasks/phase%d.md を指している" % (num, linked))
            if linked not in names:
                led.ngs.append("§5 の Phase %d の見出しが指す tasks/phase%d.md が無い" % (num, linked))
        if num in names and num not in links[num]:
            led.ngs.append("tasks/%s が §5 の Phase %d の見出しからリンクされていない"
                           % (names[num], num))
    for num in sorted(names):
        if num not in links:
            led.ngs.append("tasks/%s を指す `### Phase %d (…)` の見出しが §5 に無い"
                           % (names[num], num))

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
        text = "\n".join(line for _, line in bodies.get(num, []))
        m = LINK_COUNT.search(text)
        if not m:
            led.ngs.append("§5 の Phase %d の見出しに「済んだ N 本の本文は …」の行が無い" % num)
        elif int(m.group(1)) != len(filed.get(num, [])):
            led.ngs.append("§5 の Phase %d の「済んだ %s 本」が tasks/%s の %d 本と合わない"
                           % (num, m.group(1), names[num], len(filed.get(num, []))))

    return led


def main(argv):
    root = argv[1] if len(argv) > 1 else ROOT
    try:
        led = collect(root)
    except OSError as exc:
        print("  NG (g): 台帳が読めない: %s" % exc)
        return 1
    print("(g) 台帳: %s の未着手 %d 本 (うち [x] %d 本) / TODO.md §5 の Phase 見出し %d 節 / "
          "tasks/phaseNN.md %d 本 (タスク %d 本) / §5 に残るタスク %d 本"
          % (TASKS_MD, led.open, led.open_done, led.sections, led.files, led.filed, led.inline))
    for message in led.ngs:
        print("  NG (g): " + message)
    for name, why in led.skipped:
        print("    除外: tasks/%s — %s" % (name, why))
    for note in led.notes:
        print("    注: " + note)
    return 1 if led.ngs else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
