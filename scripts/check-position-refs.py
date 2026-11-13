#!/usr/bin/env python3
"""このリポジトリの中を位置や記号で指す識別が、新しく書いた文に戻っていないかを見る (T37.9)。

利用者の指示 (2026-11-13): 「パラグラフとかよくわからない識別は一切使わずに、タスクと仕様、それからドキュメントのみで
表現しよう」。指すときは、タスク (`T37.9`)・仕様 (`S7`)・文書の名前 (`docs/perf.md`。中の場所は見出しの名前) だけを
使う (`AGENTS.md` の「指し方」)。`scripts/check-docs.sh` の [位置で指す識別] として毎回回す。落とすのは次の 5 種類:

  [§]          … `§` (外の規格の節 `RFC 9110 §7.6.3`・`RFC 9110 §7.6・9111 §4.2.3` のように RFC の番号に続くものと、
                  記号そのものを名前で書いた `` `§` `` は除く)
  [付録]       … `付録 A` のような付録の記号
  [章の番号]   … Markdown の見出しの頭の番号 (`## 3. 1 時間後`)、と「X」の 2. のような番号での参照
  [札つきのタスク] … `T15.0 (11)`・`T28.10 (b)`・`T18.0 の (g)` のような、タスクの中の札を指す参照
                  (指すのは `T15.0` だけにするか、中身を言葉で書く)
  [記号の検査名] … `check-docs.sh の (b)`・`check-ledger.py の (3)`・`決まり (c)`・`check-dashboard.js の 11 番目の検査` のような、検査を記号や番号で呼ぶ参照

見るのは git が持っているファイルのうち、文書 (`*.md`)・Rust (`*.rs`)・道具 (`scripts/` の `*.py`・`*.sh`・`*.js`・`mx`)・
CI (`.github/workflows/*.yml`)・画面 (`crates/web/src/*.html`)。コードの文字列もコメントも同じに見る。

**意図的に残すものは下の除外表 (`SKIP`) に理由つきで持つ** (黙って無視しない)。除外は 3 つの粒で書く:
ファイルごと (`tasks/TNN.md` など)、見出しから次の同じ段の見出しまで (`TODO.md` の対応表など) か
ファイルの終わりまで (`docs/measurements.md` の移した記録)、
その文字列を含む行だけ (凍結した判定表の文面など)。どれにも当たらない識別が 1 つでもあるか、
何も外していない除外の行があれば (直したあとに残った古い行)、理由を印字して exit 1。
使うのは標準ライブラリだけ。どのファイルも読むだけ。

使い方: scripts/check-position-refs.py [リポジトリの根]   (引数なしならこのファイルの 2 つ上)
"""

import fnmatch
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

# 見るファイル (git ls-files の名前に当てる)
GLOBS = ["*.md", "*.rs", "scripts/*.py", "scripts/*/*.py", "scripts/*.sh", "scripts/*.js",
         "scripts/*/*.js", "scripts/mx", ".github/workflows/*.yml", "crates/web/src/*.html"]

# 外の規格の節: RFC の番号に続く § と、そこから・や、で続く節 (`RFC 9111 §3.2・§4.3.4`、`RFC 9110 §7.6・9111 §4.2.3`)
RFC = re.compile(r"RFC ?\d+ ?§[\d.]*\d(?:(?:・|、|,\s*|\s*/\s*| )(?:(?:RFC ?)?\d{3,4} ?)?§[\d.]*\d)*")
KINDS = [
    ("§", re.compile(r"§")),
    ("付録", re.compile(r"付録 ?[A-Z]\b")),
    ("章の番号", re.compile(r"^#{1,6} \d+(?:\.\d+)*\.\s|」の ?\d+\.(?:\s|・|$|\()")),
    ("札つきのタスク", re.compile(r"T\d+\.\d+[a-z]? ?(?:の ?)?\((?:\d{1,2}|[a-z])\)")),
    ("記号の検査名", re.compile(r"(?:check-docs\.sh|check-ledger\.py)`?(?: の)? ?\((?:[a-k]|\d)\)|決まり ?\(?[a-e]\)?(?![\w\]])"
                                r"|\d+ ?番目の(?:検査|節|章|手順)")),
]

# 除外表: (ファイルの glob, 種類 (None は全部), 範囲, 理由)。
# 範囲は None (ファイルごと)、"## 見出し" (その見出しから次の同じ段か上の段の見出しまで)、
# "以降 ## 見出し" (その見出しからファイルの終わりまで)、それ以外の文字列 (それを含む行だけ)。
SKIP = [
    ("tasks/T*.md", None, None,
     "済んだタスクの記録。1 文字も変えない決まり (`AGENTS.md` の「言葉の決まり」)"),
    ("docs/measurements.md", None, "以降 ## デプロイ先の現在地の古い版",
     "`TODO.md` の現在地から 1 文字も変えずに移した記録と、古い書き方「§2 の…」の行き先の説明"),
    ("docs/status.md", "§", "済んだタスクの記録に「§2 の」と書いてあっても",
     "済んだタスクの記録の古い書き方がどこを指すかの説明 (`docs/measurements.md` を指す)"),
    ("TODO.md", None, "## 以前の章といまの文書",
     "古い章の記号の行き先の対応表 (この表だけは古い記号をそのまま書く)"),
    ("AGENTS.md", None, "| `§2`・`(g)`・`付録 A`・章の番号など",
     "言葉の決まりの表。使わない識別そのものを書いている"),
    ("scripts/check-position-refs.py", None, None, "この検査そのもの (落とす形を書いている)"),
    ("scripts/test_check_position_refs.py", None, None, "この検査の試験 (落とす形の見本を書いている)"),
    ("scripts/test_check_ledger.py", None, None,
     "T37.5 より前の `TODO.md` の形 (`## 4. …`・`## 付録 A.`・「§4 の見本」) を読めることの試験の見本"),
    ("scripts/snapshot_criteria.py", "§", "(§2。T22.1)",
     "凍結した判定表 phase22 の INTRO の文面 (過去の判定を同じ命令で出し直すため。§2 は `snapshot-diff.py` の `## 2.`)"),
    ("scripts/snapshot_criteria.py", "札つきのタスク", "T18.0 の (g) と (h)",
     "凍結した判定表 phase18 の INTRO の文面 (過去の判定を同じ命令で出し直すため)"),
    ("scripts/snapshot-diff.py", "§", "再起動で切った平常時の前後は §2 を見ること。",
     "phase30 までの判定で出し直す `snapshot-diff.py` の出力の文面 (凍結。§2 は同じ出力の `## 2.`)"),
    ("crates/web/src/*.html", "札つきのタスク", None,
     "画面の HTML はコメントも応答の本文に入る (振る舞いを変えないので、`T15.0 (n)` のコメントを残した。T37.8)"),
]


def tracked(root):
    """git が持っているファイルのうち、`GLOBS` に当たるもの (並びは名前の順)。"""
    out = subprocess.run(["git", "-C", root, "ls-files", "-z"], capture_output=True, check=True).stdout
    names = [n.decode("utf-8") for n in out.split(b"\0") if n]
    return sorted(n for n in names if any(fnmatch.fnmatchcase(n, g) for g in GLOBS))


def heading_level(line):
    m = re.match(r"^(#{1,6}) ", line)
    return len(m.group(1)) if m else 0


def skip_spans(path, lines, kind):
    """`path` の `kind` に効く除外表の行を [(除外表の番号, 外す行の番号 (0 起算) の集合か、ファイルごとなら None)] で返す。"""
    out = []
    for n, (glob, k, span, _why) in enumerate(SKIP):
        if not fnmatch.fnmatchcase(path, glob) or (k is not None and k != kind):
            continue
        if span is None:
            out.append((n, None))
        elif span.startswith("以降 "):
            head = span[len("以降 "):]
            start = next((i for i, line in enumerate(lines) if line.startswith(head)), len(lines))
            out.append((n, set(range(start, len(lines)))))
        elif span.startswith("#"):
            lvl, inside, rows = heading_level(span), False, set()
            for i, line in enumerate(lines):
                h = heading_level(line)
                if line.startswith(span):
                    inside = True
                elif inside and h and h <= lvl:
                    inside = False
                if inside:
                    rows.add(i)
            out.append((n, rows))
        else:
            out.append((n, {i for i, line in enumerate(lines) if span in line}))
    return out


def findings(path, text, used=None):
    """`path` の中の識別を [(行番号 (1 起算), 種類, 当たった文字列, 行)] で返す。

    除外表で外したものは返さず、外した除外表の番号を `used` (集合) に足す。"""
    lines = text.split("\n")
    found = []
    for kind, pat in KINDS:
        spans = skip_spans(path, lines, kind)
        fence = False
        for i, line in enumerate(lines):
            if path.endswith(".md") and line.startswith("```"):
                fence = not fence
            body = line
            if kind == "§":
                body = RFC.sub("", body).replace("`§`", "")
            if kind == "章の番号" and (fence or not path.endswith(".md")):
                body = re.sub(r"^#{1,6} ", "", body)    # 見出しの形は文書の、囲みの外だけで見る
            for m in pat.finditer(body):
                by = next((n for n, rows in spans if rows is None or i in rows), None)
                if by is None:
                    found.append((i + 1, kind, m.group(0).strip(), line.strip()))
                elif used is not None:
                    used.add(by)
    return sorted(found)


def main(argv):
    root = os.path.abspath(argv[1]) if len(argv) > 1 else ROOT
    files = tracked(root)
    ng, used = 0, set()
    for path in files:
        try:
            text = open(os.path.join(root, path), encoding="utf-8").read()
        except (OSError, UnicodeDecodeError):
            continue
        for line_no, kind, hit, line in findings(path, text, used):
            ng += 1
            print("  NG [位置で指す識別]: %s:%d の [%s] 「%s」: %s" % (path, line_no, kind, hit, line[:120]))
    print("[位置で指す識別] %d ファイルを見た (除外 %d)" % (len(files), len(SKIP)))
    for n, (glob, kind, span, why) in enumerate(SKIP):
        where = glob + (" の「%s」" % span if span else "")
        print("    除外: %s%s — %s" % (where, " [%s]" % kind if kind else "", why))
        if n not in used:
            # 何も外していない除外は、直したあとに残った古い行 (黙って広い穴にしない)
            ng += 1
            print("  NG [位置で指す識別]: 除外表の「%s」は何も外していない (要らなくなったら消す)" % where)
    if ng:
        print("  NG [位置で指す識別]: %d 件。タスク・仕様・文書の名前 (見出しの名前) で言い換えるか、理由を除外表に書くこと" % ng)
    return 1 if ng else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
