#!/usr/bin/env python3
"""`scripts/ledger-close.py` (台帳の締め) の単体テスト (T32.26)。

    python3 -m unittest discover -s scripts        # リポジトリの根から

(1) 再生: このリポジトリの履歴の台帳 (`3cd4499` → `95c7566` → `75305de`) に道具を当てて、4 ファイル
(`TASKS.md`・`TODO.md`・`tasks/README.md`・`tasks/T31.md`) が同じバイト列になる。75305de は手で書いて
「。 タスク 31」と空白が入ったのと、前置きの相対リンク `tasks/T30.md` が `tasks/` の中から切れていた
(T32.38) ので、その 2 か所は直した形と比べる。履歴の無い浅い clone では飛ばす。
(2) 作り物の小さな台帳で: 新しい `tasks/TNN.md` と索引の行を作る・範囲の印 2 つを広げる・`--finish` の
`--deployed` で「デプロイ先の現在地」の見出しを直す・ほかに開いているタスクがあれば「なし」を書かない・使い方の誤り。
`SyntheticTest` は T37.5 より前の形 (索引と現在地が `TODO.md` の章にある木。(1) の再生もこの形)、
`DocsLayoutTest` は T37.5 からの形 (索引は `docs/task-index.md`、現在地は `docs/status.md`)。
"""

import os
import re
import subprocess
import sys
import tempfile
import unittest

import _testing

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
TOOL = os.path.join(HERE, "ledger-close.py")
FILES = ["TASKS.md", "TODO.md", "tasks/README.md", "tasks/T31.md"]

lc = _testing.load("ledger_close", "ledger-close.py")
# 一時の git リポジトリの作り (本体・クレート・`proxy-server`) は版の道具の試験のものを使う (T36.25)
tcv = _testing.load("test_crate_versions_fixture", "test_crate_versions.py")


def run(*args):
    out = subprocess.run([sys.executable, TOOL] + list(args), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         universal_newlines=True)
    return out.returncode, out.stdout + out.stderr


def git_show(rev, path):
    out = subprocess.run(["git", "-C", ROOT, "show", "%s:%s" % (rev, path)], stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL)
    # 過去のコミットの台帳は番号を `T.31.0` と書いていた。いまの書き方 (`T31.0`) に直して読む。
    text = out.stdout.decode("utf-8") if out.returncode == 0 else None
    return re.sub(r"(?<![A-Za-z0-9_\\])T\.(?=\d)", "T", text) if text is not None else None


def have(rev):
    return subprocess.run(["git", "-C", ROOT, "cat-file", "-e", rev + "^{commit}"],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0


def results_of(text, tid):
    """`tasks/T31.md` の `tid` の「結果」の行 (字下げした続きの行も) を取り出す。"""
    out, cur, on = [], None, False
    for line in text.split("\n"):
        m = re.match(r"^- \[x\] \*\*(T\d+\.\d+) ", line)
        if m:
            cur, on = m.group(1), False
            continue
        if line.startswith("  - 結果") and cur == tid:
            on = True
        if line == "":
            on = False
        if on:
            out.append(line)
    return "\n".join(out) + "\n"


def write(root, rel, text):
    full = os.path.join(root, rel)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "w", encoding="utf-8") as f:
        f.write(text)


def read(root, rel):
    with open(os.path.join(root, rel), encoding="utf-8") as f:
        return f.read()


@unittest.skipUnless(all(have(r) for r in ("3cd4499", "95c7566", "75305de")),
                     "台帳の履歴 (3cd4499・95c7566・75305de) が無い (浅い clone)")
class ReplayTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="test-ledger-close-")
        self.w = self.tmp.name
        for f in FILES[:3]:
            write(self.w, f, git_show("3cd4499", f))
        self.res = {}
        b31, c31 = git_show("95c7566", "tasks/T31.md"), git_show("75305de", "tasks/T31.md")
        for tid, src in (("T31.0", b31), ("T31.1", b31), ("T31.99", c31)):
            path = os.path.join(self.w, "r-%s.txt" % tid)
            with open(path, "w", encoding="utf-8") as f:
                f.write(results_of(src, tid))
            self.res[tid] = path

    def tearDown(self):
        self.tmp.cleanup()

    def same(self, rev, fix=None):
        for f in FILES:
            want = git_show(rev, f)
            if fix:
                want = fix(f, want)
            self.assertEqual(read(self.w, f), want, "%s が %s と違う" % (f, rev))

    def test_replay(self):
        code, out = run("-C", self.w, "--no-check", "--date", "2026-10-30",
                        "--title", "T30.99 で届かなかった 2 行を量に左右されない物差しに",
                        "--index", "T30.99 の CPU と RSS の行を量に左右されない物差しに", "T31.0", self.res["T31.0"])
        self.assertEqual(code, 0, out)
        code, out = run("-C", self.w, "--no-check", "--date", "2026-10-30", "T31.1", self.res["T31.1"])
        self.assertEqual(code, 0, out)
        self.same("95c7566")

        code, out = run("-C", self.w, "--no-check", "--date", "2026-10-31", "T31.99", self.res["T31.99"])
        self.assertEqual(code, 0, out)
        code, out = run("-C", self.w, "--no-check", "--date", "2026-10-31", "--finish", "31",
                        "--note", "T31.99 で 0.30.4 を phase28 で読み直した。デプロイは無し")
        self.assertEqual(code, 0, out)

        def fixed(f, text):
            if f == "TODO.md":  # 75305de の手書きの空白
                return text.replace("(TASKS.md)。 タスク 31 は全部済んだ", "(TASKS.md)。タスク 31 は全部済んだ")
            if f == "tasks/T31.md":  # T32.38: 移した先から辿れるリンク
                return text.replace("[`tasks/T30.md`](tasks/T30.md)", "[`tasks/T30.md`](T30.md)")
            return text

        self.same("75305de", fixed)


TASKS = """# TASKS.md

## いま開いているタスク

### タスク 5 (2026-01-02 に起こした: 作り物の題)

前置きの段落 ([`tasks/T4.md`](tasks/T4.md) と [README](README.md#運用) を指す)。

- [ ] **T5.0 ひとつめ**
  - 目的: [`scripts/x.py`](scripts/x.py) を直す。

- [ ] **T5.99 締める**
  - 前提: なし。

**利用者が決めたもの**: なし。
"""

TODO = """# TODO.md

> | [`tasks/TNN.md`](tasks/README.md) | 済んだタスクのタスク本文 (T15〜T4。1 文字も変えずに移したもの)。索引は `tasks/README.md` |

### デプロイ先の現在地 (2026-01-01、tokyo (`tokyo.example:1`) に 0.4.9 をデプロイして 24.0 時間)

**数字の本文は締めの結果にだけ置く**。いまの数字は [`tasks/T4.md`](tasks/T4.md) の **T4.99 の結果** (要点)。

## 5. 索引

### タスク 4 (2026-01-01 に起こした: 前の題)

**済んだ 1 本の本文は [`tasks/T4.md`](tasks/T4.md) に移した** (2026-01-01。T4.99、1 文字も変えていない)。

### タスク 19 (2026-10-23 に起こした: 別の題)

**済んだ 1 本**。
"""

README = """# tasks/ — 済んだタスクの本文 (タスク 0〜4)

| 番号 | ファイル | 中身 |
|---|---|---|
| T4.99〜T4.99 | [T4.md](T4.md) | 前の題 (2026-01-01) |

- 終わり。
"""


class SyntheticTest(unittest.TestCase):
    """T37.5 より前の形 (索引と「デプロイ先の現在地」が `TODO.md` の章にある木)。過去の台帳を読み直すために残す。"""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="test-ledger-close-")
        self.w = self.tmp.name
        write(self.w, "TASKS.md", TASKS)
        write(self.w, "TODO.md", TODO)
        write(self.w, "tasks/README.md", README)
        write(self.w, "tasks/T4.md", "# タスク 4 — 記録\n")
        self.r = os.path.join(self.w, "r.txt")
        with open(self.r, "w", encoding="utf-8") as f:
            f.write("  - 結果 (2026-01-03、`abc1234`): 直した ([`T4.md`](T4.md))。\n")

    def tearDown(self):
        self.tmp.cleanup()

    def close(self, tid, *extra):
        code, out = run("-C", self.w, "--no-check", "--date", "2026-01-03", tid, self.r, *extra)
        self.assertEqual(code, 0, out)

    def test_new_task_file_index_and_ranges(self):
        self.close("T5.0")
        t5 = read(self.w, "tasks/T5.md")
        self.assertTrue(t5.startswith("# タスク 5 — 記録 (作り物の題)\n\n> `TASKS.md` のタスク 5 のうち、"), t5)
        self.assertIn("- [x] **T5.0 ひとつめ**\n  - 目的: [`scripts/x.py`](../scripts/x.py) を直す。\n"
                      "  - 結果 (2026-01-03、`abc1234`): 直した ([`T4.md`](T4.md))。\n", t5)
        tasks = read(self.w, "TASKS.md")
        self.assertNotIn("T5.0", tasks)
        self.assertNotIn("\n\n\n", tasks)
        self.assertIn("を指す)。\n\n- [ ] **T5.99 締める**", tasks)
        todo = read(self.w, "TODO.md")
        self.assertIn("(T15〜T5。", todo)
        self.assertIn("### タスク 4 (2026-01-01 に起こした: 前の題)\n\n**済んだ 1 本の本文は [`tasks/T4.md`]"
                      "(tasks/T4.md) に移した** (2026-01-01。T4.99、1 文字も変えていない)。\n\n"
                      "### タスク 5 (2026-01-02 に起こした: 作り物の題)\n\n**済んだ 1 本の本文は [`tasks/T5.md`]"
                      "(tasks/T5.md) に移した** (2026-01-02〜03。T5.0、1 文字も変えていない)。"
                      "未着手の残りは [`TASKS.md`](TASKS.md)。\n\n### タスク 19", todo)
        readme = read(self.w, "tasks/README.md")
        self.assertIn("# tasks/ — 済んだタスクの本文 (タスク 0〜5)", readme)
        self.assertIn("| T4.99〜T4.99 | [T4.md](T4.md) | 前の題 (2026-01-01) |\n"
                      "| T5.0〜T5.0 | [T5.md](T5.md) | 作り物の題 (2026-01-03) |\n", readme)

    def test_finish_deployed(self):
        self.close("T5.0")
        code, out = run("-C", self.w, "--no-check", "--date", "2026-01-03", "--finish", "5")
        self.assertEqual(code, 2, out)  # T5.99 が残っている
        self.assertIn("未着手が 1 本", out)
        self.close("T5.99")
        code, out = run("-C", self.w, "--no-check", "--date", "2026-01-04", "--finish", "5",
                        "--deployed", "0.5.3+abcdef0", "--hours", "48.1")
        self.assertEqual(code, 0, out)
        self.assertIn("手で書く", out)
        tasks = read(self.w, "TASKS.md")
        self.assertTrue(tasks.endswith("## いま開いているタスク\n\n"
                                       "なし (タスク 5 は 2026-01-04 に T5.99 で締めた。タスク 6 はまだ起こしていない)。\n"),
                        tasks)
        t5 = read(self.w, "tasks/T5.md")
        self.assertTrue(t5.endswith(
            "\n\n## タスク 5 の前置きと判断の記録 (2026-01-04、全部済んだので TASKS.md から移した。1 文字も変えていない)\n\n"
            "#### タスク 5 (2026-01-02 に起こした: 作り物の題)\n\n"
            "前置きの段落 ([`tasks/T4.md`](T4.md) と [README](../README.md#運用) を指す)。\n\n"
            "**利用者が決めたもの**: なし。\n"), t5)
        todo = read(self.w, "TODO.md")
        self.assertIn("(2026-01-02〜03。T5.0・T5.99、1 文字も変えていない)。未着手の残りは [`TASKS.md`](TASKS.md)。"
                      "タスク 5 は全部済んだ (2026-01-04、T5.99 で 0.5.3 を締めた)。\n", todo)
        self.assertIn("### デプロイ先の現在地 (2026-01-04、tokyo (`tokyo.example:1`) に 0.5.3 をデプロイして 48.1 時間)", todo)
        self.assertIn("いまの数字は [`tasks/T5.md`](tasks/T5.md) の **T5.99 の結果** (要点)", todo)
        # 2 度目は節が無いので誤り
        code, out = run("-C", self.w, "--no-check", "--finish", "5")
        self.assertEqual(code, 2, out)

    def test_finish_keeps_other_open_sections(self):
        write(self.w, "TASKS.md", TASKS.replace("**利用者が決めたもの**: なし。\n",
                                                "**利用者が決めたもの**: なし。\n\n### タスク 6 (2026-01-03 に起こした: 次)\n\n"
                                                "- [ ] **T6.0 次の**\n"))
        self.close("T5.0")
        self.close("T5.99")
        code, out = run("-C", self.w, "--no-check", "--finish", "5")
        self.assertEqual(code, 0, out)
        tasks = read(self.w, "TASKS.md")
        self.assertNotIn("なし (", tasks)
        self.assertIn("## いま開いているタスク\n\n### タスク 6 (2026-01-03 に起こした: 次)\n\n- [ ] **T6.0 次の**\n",
                      tasks)

    def test_usage_errors(self):
        for args in (["T5.7", self.r], ["T.5.0", self.r], ["T5.0"], ["--finish", "5", "T5.0", self.r],
                     ["T5.0", self.r, "--note", "x"], ["--finish", "5", "--hours", "1"]):
            code, out = run("-C", self.w, "--no-check", *args)
            self.assertEqual(code, 2, (args, out))
        self.assertEqual(read(self.w, "TASKS.md"), TASKS)  # 誤りでは何も書き換えない

    def test_check_ledger_runs(self):
        # --no-check を付けなければ最後に check-ledger.py を回す (作り物の台帳は合わないので 1)
        code, out = run("-C", self.w, "--date", "2026-01-03", "T5.0", self.r)
        self.assertEqual(code, 1, out)


TODO_NOW = """# TODO.md

入り口の索引だけ (T37.5 から)。
"""

INDEX_NOW = """# docs/task-index.md — タスクの索引

## タスク 4 (2026-01-01 に起こした: 前の題)

**済んだ 1 本の本文は [`tasks/T4.md`](../tasks/T4.md) に移した** (2026-01-01。T4.99、1 文字も変えていない)。

## タスク 19 (2026-10-23 に起こした: 別の題)

**済んだ 1 本**。
"""

STATUS_NOW = """# docs/status.md — 現在地

## デプロイ先の現在地 (2026-01-01、tokyo (`tokyo.example:1`) に 0.4.9 をデプロイして 24.0 時間)

**数字の本文は締めの結果にだけ置く**。いまの数字は [`tasks/T4.md`](../tasks/T4.md) の **T4.99 の結果** (要点)。
"""


class DocsLayoutTest(unittest.TestCase):
    """T37.5 からの形: 索引は `docs/task-index.md` (`## タスク N (…)`)、現在地は `docs/status.md`。`TODO.md` は触らない。"""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="test-ledger-close-docs-")
        self.w = self.tmp.name
        write(self.w, "TASKS.md", TASKS)
        write(self.w, "TODO.md", TODO_NOW)
        write(self.w, "docs/task-index.md", INDEX_NOW)
        write(self.w, "docs/status.md", STATUS_NOW)
        write(self.w, "tasks/README.md", README)
        write(self.w, "tasks/T4.md", "# タスク 4 — 記録\n")
        self.r = os.path.join(self.w, "r.txt")
        with open(self.r, "w", encoding="utf-8") as f:
            f.write("  - 結果 (2026-01-03、`abc1234`): 直した ([`T4.md`](T4.md))。\n")

    def tearDown(self):
        self.tmp.cleanup()

    def close(self, tid, *extra):
        code, out = run("-C", self.w, "--no-check", "--date", "2026-01-03", tid, self.r, *extra)
        self.assertEqual(code, 0, out)

    def test_index_line_goes_to_the_index_document(self):
        self.close("T5.0")
        index = read(self.w, "docs/task-index.md")
        self.assertIn("## タスク 4 (2026-01-01 に起こした: 前の題)\n\n**済んだ 1 本の本文は [`tasks/T4.md`]"
                      "(../tasks/T4.md) に移した** (2026-01-01。T4.99、1 文字も変えていない)。\n\n"
                      "## タスク 5 (2026-01-02 に起こした: 作り物の題)\n\n**済んだ 1 本の本文は [`tasks/T5.md`]"
                      "(../tasks/T5.md) に移した** (2026-01-02〜03。T5.0、1 文字も変えていない)。"
                      "未着手の残りは [`TASKS.md`](../TASKS.md)。\n\n## タスク 19", index)
        self.assertEqual(read(self.w, "TODO.md"), TODO_NOW)  # 入り口は触らない
        self.assertIn("# tasks/ — 済んだタスクの本文 (タスク 0〜5)", read(self.w, "tasks/README.md"))
        # 2 本目は同じ節の行を書き直す
        self.close("T5.99")
        index = read(self.w, "docs/task-index.md")
        self.assertEqual(index.count("## タスク 5 ("), 1, index)
        self.assertIn("(2026-01-02〜03。T5.0・T5.99、1 文字も変えていない)", index)

    def test_finish_deployed_rewrites_the_status_document(self):
        self.close("T5.0")
        self.close("T5.99")
        code, out = run("-C", self.w, "--no-check", "--date", "2026-01-04", "--finish", "5",
                        "--deployed", "0.5.3+abcdef0", "--hours", "48.1")
        self.assertEqual(code, 0, out)
        self.assertIn("手で書く: docs/status.md「デプロイ先の現在地」", out)
        index = read(self.w, "docs/task-index.md")
        self.assertIn("未着手の残りは [`TASKS.md`](../TASKS.md)。タスク 5 は全部済んだ (2026-01-04、T5.99 で 0.5.3 を締めた)。\n",
                      index)
        status = read(self.w, "docs/status.md")
        self.assertIn("## デプロイ先の現在地 (2026-01-04、tokyo (`tokyo.example:1`) に 0.5.3 をデプロイして 48.1 時間)",
                      status)
        self.assertIn("いまの数字は [`tasks/T5.md`](../tasks/T5.md) の **T5.99 の結果** (要点)", status)
        self.assertEqual(read(self.w, "TODO.md"), TODO_NOW)

    def test_check_ledger_passes_on_the_closed_tree(self):
        """締めた木を check-ledger.py が通す (索引の行のリンクが docs/ から辿れる)。"""
        write(self.w, "TASKS.md", TASKS.replace("**利用者が決めたもの**: なし。\n",
                                                "**利用者が決めたもの**: なし。\n\n### タスク 6 (2026-01-03 に起こした: 次)\n\n"
                                                "- [ ] **T6.0 次の**\n"))
        write(self.w, "tasks/T4.md", "# タスク 4 — 記録\n\n- [x] **T4.99 前の**\n  - 結果: 済み。\n")
        write(self.w, "docs/task-index.md", INDEX_NOW.replace(
            "## タスク 19 (2026-10-23 に起こした: 別の題)\n\n**済んだ 1 本**。\n", ""))
        write(self.w, "README.md", "# README\n")       # 前置きのリンクの先 (作り物)
        write(self.w, "scripts/x.py", "# 作り物\n")
        self.close("T5.0")
        self.close("T5.99")
        code, out = run("-C", self.w, "--date", "2026-01-04", "--finish", "5")
        self.assertEqual(code, 0, out)
        self.assertIn("docs/task-index.md のタスク見出し 2 節", out)
        self.assertNotIn("NG", out)


class VersionCheckTest(unittest.TestCase):
    """台帳を締めるとき、クレートの版の食い違いをその場で知らせる (T36.25)。

    一時の git リポジトリは `test_crate_versions.py` と同じ作り (本体・3 クレート・`proxy-server`)。
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="test-ledger-close-versions-")
        self.r = tcv.Repo(self.tmp.name)
        self.r.git("init", "-q", "-b", "main")
        self.r.set_versions("0.1.1", alpha="0.1.1", beta="0.1.1", gamma="0.1.1", server="0.1.1")
        for c in tcv.CRATES:
            self.r.write("crates/%s/src/lib.rs" % c, "// %s\n" % c)
        self.r.write("src/main.rs", "// main\n")
        self.r.commit("T1.1")

    def tearDown(self):
        self.tmp.cleanup()

    def test_matching_versions_say_nothing(self):
        self.assertEqual(lc.check_versions(self.r.path, "T2.3"), (0, ""))

    def test_a_forgotten_bump_is_reported_with_the_fix_command(self):
        # 小タスク T2.3 を取り込んだが版をそろえていない (beta の中身だけが変わった)
        self.r.write("crates/beta/src/lib.rs", "// beta 2\n")
        self.r.commit("T2.3 の実装 (版は上げていない)")
        code, note = lc.check_versions(self.r.path, "T2.3")
        self.assertEqual(code, 1, note)
        self.assertIn("NG proxy-beta", note)
        self.assertIn("版の決まりに合わない: 1 個", note)
        self.assertIn("scripts/crate-versions.py --task T2.3 --fix", note)
        # 知らせた命令で直れば、もう何も言わない
        code_fix, out = self.r.run("--task", "T2.3", "--fix")
        self.assertEqual(code_fix, 0, out)
        self.assertEqual(lc.check_versions(self.r.path, "T2.3"), (0, ""))

    def test_finish_has_no_number_to_suggest(self):
        self.r.write("crates/beta/src/lib.rs", "// beta 2\n")
        self.r.commit("版を上げ忘れた")
        code, note = lc.check_versions(self.r.path)
        self.assertEqual(code, 1, note)
        self.assertIn("--task <取り込んだ小タスク> --fix", note)

    def test_ledger_only_repository_is_not_checked(self):
        with tempfile.TemporaryDirectory(prefix="test-ledger-close-noversions-") as d:
            self.assertEqual(lc.check_versions(d, "T2.3"), (0, ""))

    def test_a_tool_that_cannot_run_does_not_fail_the_close(self):
        # Cargo.toml はあるが git のリポジトリでない: 版の道具は 2 で終わる
        with tempfile.TemporaryDirectory(prefix="test-ledger-close-nogit-") as d:
            with open(os.path.join(d, "Cargo.toml"), "w", encoding="utf-8") as f:
                f.write(self.r.read("Cargo.toml"))
            code, note = lc.check_versions(d, "T2.3")
            self.assertEqual(code, 0, note)
            self.assertIn("確かめられなかった", note)

    def test_main_returns_1_and_prints_the_note(self):
        # 台帳の作り物 (SyntheticTest と同じ) を置いたうえで版を食い違わせ、--no-check なしで締める
        write(self.r.path, "TASKS.md", TASKS)
        write(self.r.path, "TODO.md", TODO)
        write(self.r.path, "tasks/README.md", README)
        write(self.r.path, "tasks/T4.md", "# タスク 4 — 記録\n")
        self.r.write("crates/beta/src/lib.rs", "// beta 2\n")
        self.r.commit("T5.0 の実装 (版は上げていない)")
        res = os.path.join(self.r.path, "r.txt")
        with open(res, "w", encoding="utf-8") as f:
            f.write("  - 結果 (2026-01-03、`abc1234`): 直した。\n")
        code, out = run("-C", self.r.path, "--date", "2026-01-03", "T5.0", res)
        self.assertEqual(code, 1, out)
        self.assertIn("scripts/crate-versions.py --task T5.0 --fix", out)
        # --no-check なら版も見ない
        self.r.git("checkout", "-q", "--", ".")
        self.r.git("clean", "-qfd")
        with open(res, "w", encoding="utf-8") as f:
            f.write("  - 結果 (2026-01-03、`abc1234`): 直した。\n")
        code, out = run("-C", self.r.path, "--no-check", "--date", "2026-01-03", "T5.0", res)
        self.assertEqual(code, 0, out)
        self.assertNotIn("crate-versions.py --task", out)


class HelpersTest(unittest.TestCase):
    def test_rewrite_links(self):
        f = lc.rewrite_links
        self.assertEqual(f("[a](tasks/T30.md)", "", "tasks"), "[a](T30.md)")
        self.assertEqual(f("[a](TODO.md#5-x)", "", "tasks"), "[a](../TODO.md#5-x)")
        self.assertEqual(f("[a](https://e.example/x) [b](#here)", "", "tasks"), "[a](https://e.example/x) [b](#here)")
        self.assertEqual(f("[a](tasks/T30.md)", "", ""), "[a](tasks/T30.md)")

    def test_date_range(self):
        self.assertEqual(lc.date_range("2026-10-30", "2026-10-30"), "2026-10-30〜")
        self.assertEqual(lc.date_range("2026-10-30", "2026-10-31"), "2026-10-30〜31")
        self.assertEqual(lc.date_range("2026-10-30", "2026-11-02"), "2026-10-30〜11-02")
        self.assertEqual(lc.date_range("2026-12-30", "2027-01-02"), "2026-12-30〜2027-01-02")


if __name__ == "__main__":
    unittest.main()
