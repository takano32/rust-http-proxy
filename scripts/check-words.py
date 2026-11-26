#!/usr/bin/env python3
"""使わないと決めた言葉が、道具と README に戻っていないかを見る (T39.10)。

利用者の決めた呼び方 (`AGENTS.md` の「言葉の決まり」): 「雪像」ではなく「スナップショット」(2026-11-12)。
T39.10 で、道具 (`scripts/` の `*.py`・`*.sh`・`*.js`・`mx`)・CI (`.github/workflows/*.yml`)・`.gitignore`・README の
出力の文面とコメントを置き換えた (利用者がまとめての置き換えを頼んだ、2026-11-22)。`scripts/check-docs.sh` の
[言葉の決まり] として毎回回す。

見るのはその範囲だけ。文書 (`docs/`)・本体 (Rust と `crates/web/src/*.html`) は T39.10 では置き換えていない
(`AGENTS.md` の「当てはめ方」: 既にある文をまとめて置き換えるのは、利用者が頼んだときだけ)。
済んだタスクの記録 (`tasks/TNN.md`) は 1 文字も変えない決まりなので、もとから見ない。

**意図的に残すものは下の除外表 (`SKIP`) に理由つきで持つ** (黙って無視しない)。除外は 3 つの粒で書く:
ファイルごと、Python の文字列 (`"文字列"`。docstring とコメントは見る。凍結した判定表の行と出力の文面)、
その文字列を含む行だけ。どれにも当たらない言葉が 1 つでもあるか、何も外していない除外の行があれば
(直したあとに残った古い行)、理由を印字して exit 1。使うのは標準ライブラリだけ。どのファイルも読むだけ。

使い方: scripts/check-words.py [リポジトリの根]   (引数なしならこのファイルの 2 つ上)
"""

import fnmatch
import io
import os
import subprocess
import sys
import tokenize

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

# 見るファイル (git ls-files の名前に当てる)
GLOBS = ["scripts/*.py", "scripts/*/*.py", "scripts/*.sh", "scripts/*.js", "scripts/*/*.js", "scripts/mx",
         ".github/workflows/*.yml", ".gitignore", "README.md",
         # T39.12 で本体・試験・仕様・docs にも広げた
         "crates/*/src/*.rs", "crates/*/src/*/*.rs", "crates/*/src/*/*/*.rs", "crates/*/src/*/*/*/*.rs",
         "crates/*/src/*.html", "crates/*/build.rs", "src/*.rs", "src/*/*.rs", "tests/*.rs", "tests/*/*.rs",
         "SPECS.md", "specs/*.md", "docs/*.md", "TODO.md", "TASKS.md", "AGENTS.md"]

# (使わない, 使う)
WORDS = [("雪像", "スナップショット")]

STRINGS = "文字列"

# 除外表: (ファイルの glob, 範囲, 理由)。範囲は None (ファイルごと)、STRINGS (Python の文字列のうち docstring で
# ないもの)、それ以外の文字列 (それを含む行だけ)。
FROZEN = "過去の判定を同じ命令で出し直すために凍結した文面 (`AGENTS.md` の「当てはめ方」)"
SKIP = [
    ("scripts/check-words.py", None, "この検査そのもの (使わない言葉を書いている)"),
    (".github/workflows/*.yml", None,
     "コメントだけ。このセッションの push の鍵は workflow のファイルを書き換えられない (GitHub が `workflow` の権限の無い鍵を断る。T39.10)。"
     "GitHub のツール (API) での書き込みも同じ権限が無くて断られた (T40.3)。"
     "利用者が手元から置き換えたら、この行を外す"),
    ("docs/measurements.md", None,
     "「デプロイ先の現在地の古い版」以下は、`TODO.md` の現在地から 1 文字も変えずに移した昔の記録 (T39.12 で残した 44 行はどれもこの節)"),
    ("AGENTS.md", "| 雪像 | スナップショット |", "言葉の決まりの表そのもの (使わない言葉を書いている)"),
    ("scripts/test_check_words.py", None, "この検査の試験 (使わない言葉の見本を書いている)"),
    ("scripts/criteria/phase*.py", STRINGS, "判定表 phase14〜phase30 の行の文面。" + FROZEN),
    ("scripts/criteria/t3[5-8].py", STRINGS, "判定表 T35〜T38 の行の文面。" + FROZEN),
    ("scripts/snapshot_criteria.py", STRINGS, "判定表 phase14〜phase30・T35〜T38 の INTRO の文面。" + FROZEN),
    ("scripts/snapshot-diff.py", STRINGS,
     "phase30 までの判定で出し直す `snapshot-diff.py` の出力の文面 (`--help` は置き換えた)。" + FROZEN),
    ("scripts/test_snapshot_criteria*.py", STRINGS, "凍結した判定表の行の文面を確かめる試験の期待値"),
    ("scripts/test_collect_deployed.py", "の 0 時間の雪像からの増えが 5 MB 未満",
     "要約に入る判定表 phase20 の行の文面を確かめる試験の期待値 (凍結)"),
    ("scripts/test_collect_deployed.py", "`--zero` (0 時間の雪像) が渡されていない",
     "要約に入る判定表 phase20 の行の文面を確かめる試験の期待値 (凍結)"),
    ("scripts/test_collect_deployed.py", "**日次の雪像**",
     "要約に入る `snapshot-diff.py` の日次の表の文面を確かめる試験の期待値 (凍結)"),
    ("scripts/test_snapshot_diff.py", "後の雪像に `/recent` の部が無い",
     "`snapshot-diff.py` の出力の文面を確かめる試験の期待値 (凍結)"),
    ("scripts/test_snapshot_summary.py", "(雪像には 3 ",
     "要約に入る `snapshot-diff.py` の `/errors` の行の文面を確かめる試験の期待値 (凍結)"),
]


def tracked(root):
    """git が持っているファイルのうち、`GLOBS` に当たるもの (並びは名前の順)。"""
    out = subprocess.run(["git", "-C", root, "ls-files", "-z"], capture_output=True, check=True).stdout
    names = [n.decode("utf-8") for n in out.split(b"\0") if n]
    return sorted(n for n in names if any(fnmatch.fnmatchcase(n, g) for g in GLOBS))


def string_lines(text):
    """Python の文字列 (docstring を除く) が載っている行の番号 (0 起算) の集合。読めなければ None。"""
    try:
        toks = list(tokenize.generate_tokens(io.StringIO(text).readline))
    except (tokenize.TokenError, SyntaxError, IndentationError):
        return None
    rows, prev = set(), None
    for i, t in enumerate(toks):
        if t.type == tokenize.STRING or t.type == getattr(tokenize, "FSTRING_MIDDLE", -1):
            nxt = next((u for u in toks[i + 1:] if u.type not in (tokenize.NL, tokenize.COMMENT)), None)
            doc = (t.type == tokenize.STRING and
                   (prev is None or prev.type in (tokenize.NEWLINE, tokenize.INDENT, tokenize.DEDENT,
                                                  tokenize.ENCODING)) and
                   nxt is not None and nxt.type in (tokenize.NEWLINE, tokenize.ENDMARKER))
            if not doc:
                rows.update(range(t.start[0] - 1, t.end[0]))
        if t.type not in (tokenize.NL, tokenize.COMMENT):
            prev = t
    return rows


def skip_spans(path, lines, text):
    """`path` に効く除外表の行を [(除外表の番号, 外す行の番号の集合か、ファイルごとなら None)] で返す。"""
    out = []
    for n, (glob, span, _why) in enumerate(SKIP):
        if not fnmatch.fnmatchcase(path, glob):
            continue
        if span is None:
            out.append((n, None))
        elif span == STRINGS:
            out.append((n, string_lines(text) or set()))
        else:
            out.append((n, {i for i, line in enumerate(lines) if span in line}))
    return out


def findings(path, text, used=None):
    """`path` の中の使わない言葉を [(行番号 (1 起算), 言葉, 行)] で返す。

    除外表で外したものは返さず、外した除外表の番号を `used` (集合) に足す。"""
    lines = text.split("\n")
    if not any(old in text for old, _new in WORDS):
        return []
    spans = skip_spans(path, lines, text)
    found = []
    for i, line in enumerate(lines):
        for old, _new in WORDS:
            if old not in line:
                continue
            by = next((n for n, rows in spans if rows is None or i in rows), None)
            if by is None:
                found.append((i + 1, old, line.strip()))
            elif used is not None:
                used.add(by)
    return found


def main(argv):
    root = os.path.abspath(argv[1]) if len(argv) > 1 else ROOT
    files = tracked(root)
    ng, used = 0, set()
    new = dict(WORDS)
    for path in files:
        try:
            with open(os.path.join(root, path), encoding="utf-8") as f:
                text = f.read()
        except (OSError, UnicodeDecodeError):
            continue
        for line_no, old, line in findings(path, text, used):
            ng += 1
            print("  NG [言葉の決まり]: %s:%d に「%s」(「%s」と書く): %s" % (path, line_no, old, new[old], line[:120]))
    print("[言葉の決まり] %d ファイルを見た (除外 %d)" % (len(files), len(SKIP)))
    for n, (glob, span, why) in enumerate(SKIP):
        where = glob + (" の「%s」" % span if span else "")
        print("    除外: %s — %s" % (where, why))
        if n not in used:
            ng += 1
            print("  NG [言葉の決まり]: 除外表の「%s」は何も外していない (要らなくなったら消す)" % where)
    if ng:
        print("  NG [言葉の決まり]: %d 件。`AGENTS.md` の「言葉の決まり」の言葉に直すか、理由を除外表に書くこと" % ng)
    return 1 if ng else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
