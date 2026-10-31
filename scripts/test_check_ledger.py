#!/usr/bin/env python3
"""`scripts/check-ledger.py` (台帳の 1 対 1) の単体テスト (T.24.10)。

    python3 -m unittest discover -s scripts        # リポジトリの根から

一時のディレクトリに `TODO.md` と `tasks/TNN.md` の見本を置き、`check-ledger.py` の
`collect()` / `main()` をそのまま呼ぶ。**本物の台帳は読まない** (中身が進んでもテストが揺れない)。
見るのは、通る形 1 つと、規則ごとにわざと壊した形:

  (1) 全部 `- [x]` のタスクが §5 に残る / 移したタスクにタスクの行が残る
  (2) リンクが無い / 指す先が無い / 見出しが無い / 番号が違う
  (3) 番号が本文つきで 2 回出る (片方が `結果` だけの追記なら通る) / 番号と置き場所が違う
  (4) 1 行目が `# タスク NN — …` でない
  (足) 「済んだ N 本」がファイルの本数と合わない
"""

import contextlib
import importlib.util
import io
import os
import re
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def _load(name, filename):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


cl = _load("check_ledger", "check-ledger.py")

BODY = ("目的", "変更箇所", "やること", "受け入れ基準")


def task(ident, status="x", body=BODY, title="見本のタスク"):
    """タスクの行 1 つ (と本文)。"""
    lines = ["- [%s] **%s %s**" % (status, ident, title)]
    for label in body:
        lines.append("  - %s: 見本。" % label)
    lines.append("")
    return lines


def phase(num, blocks, link=False, count=None, moved_line=None):
    """§5 の節 1 つ。`blocks` は `task()` の列。"""
    lines = ["### タスク %d (2026-10-22 に決めた: 見本)" % num, ""]
    if link or moved_line:
        n = len(blocks) if count is None else count
        if moved_line is None:
            moved_line = ("**済んだ %d 本の本文は [`tasks/T%d.md`](tasks/T%d.md) に移した**"
                          " (2026-10-22、T.24.9)。" % (n, num, num))
        lines += [moved_line, ""]
    for block in blocks:
        lines += block
    return lines


def todo_md(sections, extra=None):
    """§5 の外にも `- [x]` を置いた `TODO.md` (§5 だけを見ていることの確認)。"""
    lines = ["# 台帳の見本", "", "## 4. 測って採らなかったもの", "",
             "- [x] **T.1.1 §4 の見本 (台帳のタスクではない)**", "",
             "## 5. これから (未着手)", ""]
    for section in sections:
        lines += section + [""]
    lines += ["## 付録 A. 計測の記録 (時系列)", "", "- [x] **T.0.1 付録の見本**", ""]
    if extra:
        lines += extra
    return lines


class LedgerCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = self._tmp.name
        os.makedirs(os.path.join(self.root, "tasks"))

    def write(self, rel, lines):
        with open(os.path.join(self.root, rel), "w", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")

    def build(self, sections, files):
        """`TODO.md` と `tasks/*.md` を置いて、道具を回した (終了コード, 出力) を返す。"""
        self.write("TODO.md", todo_md(sections))
        for name, lines in files.items():
            self.write(os.path.join("tasks", name), lines)
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = cl.main(["check-ledger.py", self.root])
        return code, buf.getvalue()

    def assert_ng(self, out, needle):
        self.assertIn("NG (g)", out)
        self.assertIn(needle, out)


class Pass(LedgerCase):
    def test_clean_ledger_passes(self):
        code, out = self.build(
            [phase(23, [], link=True, count=2),
             phase(24, [task("T.24.0"), task("T.24.1", status=" ", body=())])],
            {"T23.md": ["# タスク 23 — 見本", ""] + task("T.23.0") + task("T.23.1")})
        self.assertEqual(code, 0, out)
        self.assertNotIn("NG", out)
        self.assertIn("tasks/TNN.md 1 本 (タスク 2 本)", out)
        self.assertIn("§5 に残るタスク 2 本", out)

    def test_instruction_heading_is_not_a_phase(self):
        """`### タスク 15 の渡し方 (…)` のような指示文の節は台帳の節として数えない。"""
        code, out = self.build(
            [["### タスク 15 の渡し方 (2026-09-18 版。指示文の型)", ""] + task("T.15.0")],
            {})
        self.assertEqual(code, 0, out)
        self.assertIn("タスク見出し 0 節", out)

    def test_result_only_note_is_not_a_duplicate(self):
        code, out = self.build(
            [phase(24, [task("T.24.1"), task("T.24.1", body=("結果",)),
                        task("T.24.2", status=" ", body=())])],
            {})
        self.assertEqual(code, 0, out)
        self.assertNotIn("NG", out)
        self.assertIn("注:", out)
        self.assertIn("`結果` だけの追記", out)

    def test_index_file_is_skipped_with_reason(self):
        code, out = self.build(
            [phase(24, [task("T.24.0", status=" ", body=())])],
            {"README.md": ["# tasks/ — 索引", ""]})
        self.assertEqual(code, 0, out)
        self.assertIn("除外: tasks/README.md", out)


class Move(LedgerCase):
    def test_all_done_phase_must_move(self):
        code, out = self.build([phase(24, [task("T.24.0"), task("T.24.1")])], {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "全部 [x] なのに §5 に残っている")

    def test_moved_phase_must_not_keep_rows(self):
        code, out = self.build(
            [phase(23, [task("T.23.0")], link=True)],
            {"T23.md": ["# タスク 23 — 見本", ""] + task("T.23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "移したのに §5 にタスクの行が 1 本")


class Link(LedgerCase):
    def test_missing_link(self):
        code, out = self.build([], {"T23.md": ["# タスク 23 — 見本", ""] + task("T.23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "tasks/T23.md を指す `### タスク 23 (…)` の見出しが §5 に無い")

    def test_phase_file_not_linked_from_its_heading(self):
        code, out = self.build(
            [phase(23, [], link=False)],
            {"T23.md": ["# タスク 23 — 見本", ""] + task("T.23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "が §5 のタスク 23 の見出しからリンクされていない")

    def test_link_to_missing_file(self):
        code, out = self.build([phase(21, [], link=True)], {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "見出しが指す tasks/T21.md が無い")

    def test_link_number_mismatch(self):
        moved = "**済んだ 1 本の本文は [`tasks/T22.md`](tasks/T22.md) に移した** (2026-10-22、T.24.9)。"
        code, out = self.build(
            [phase(23, [], moved_line=moved)],
            {"T22.md": ["# タスク 22 — 見本", ""] + task("T.22.0"),
             "T23.md": ["# タスク 23 — 見本", ""]})
        self.assertEqual(code, 1)
        self.assert_ng(out, "§5 のタスク 23 の見出しが tasks/T22.md を指している")

    def test_unknown_file_in_tasks(self):
        code, out = self.build(
            [phase(24, [task("T.24.0", status=" ", body=())])],
            {"notes.md": ["# 覚え書き", ""]})
        self.assertEqual(code, 1)
        self.assert_ng(out, "tasks/notes.md が台帳の型 `TNN.md` に合わない")


class Numbers(LedgerCase):
    def test_duplicate_with_two_bodies(self):
        code, out = self.build(
            [phase(24, [task("T.24.1"), task("T.24.1", title="別のタスク"),
                        task("T.24.2", status=" ", body=())])],
            {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T.24.1 が本文つきで 2 回出る")

    def test_duplicate_without_body(self):
        code, out = self.build(
            [phase(24, [task("T.24.1", body=("結果",)), task("T.24.1", body=("結果",)),
                        task("T.24.2", status=" ", body=())])],
            {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "どれも本文を持たない")

    def test_number_in_wrong_phase(self):
        code, out = self.build(
            [phase(24, [task("T.23.5", status=" ", body=()), task("T.24.0", status=" ", body=())])],
            {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T.23.5 がタスク 24 の節にある")

    def test_number_in_wrong_file(self):
        code, out = self.build(
            [phase(23, [], link=True, count=2)],
            {"T23.md": ["# タスク 23 — 見本", ""] + task("T.23.0") + task("T.22.1")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T.22.1 がタスク 23 のファイルにある")

    def test_number_of_two_files(self):
        """同じ番号が §5 と tasks/ の両方に出る (移し忘れの取り残し) も重複。"""
        code, out = self.build(
            [phase(23, [], link=True),
             phase(24, [task("T.23.0"), task("T.24.0", status=" ", body=())])],
            {"T23.md": ["# タスク 23 — 見本", ""] + task("T.23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T.23.0 が本文つきで 2 回出る")


class Shape(LedgerCase):
    def test_first_line_without_dash(self):
        code, out = self.build(
            [phase(23, [], link=True)],
            {"T23.md": ["# タスク 23 見本", ""] + task("T.23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "tasks/T23.md の 1 行目が `# タスク NN — …` でない")

    def test_first_line_wrong_number(self):
        code, out = self.build(
            [phase(23, [], link=True)],
            {"T23.md": ["# タスク 22 — 見本", ""] + task("T.23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "1 行目がタスク 22 になっている")

    def test_count_mismatch(self):
        code, out = self.build(
            [phase(23, [], link=True, count=5)],
            {"T23.md": ["# タスク 23 — 見本", ""] + task("T.23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "「済んだ 5 本」が tasks/T23.md の 1 本と合わない")

    def test_count_line_missing(self):
        code, out = self.build(
            [phase(23, [], moved_line="(移した)")],
            {"T23.md": ["# タスク 23 — 見本", ""] + task("T.23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "「済んだ N 本の本文は …」の行が無い")


class OpenTasks(LedgerCase):
    """`TASKS.md` (未着手) の見方 (T.24.13)。`build` に `open_tasks` を渡すと `TASKS.md` を置く。"""

    def build_open(self, tasks_md, sections, files):
        self.write("TASKS.md", tasks_md)
        return self.build(sections, files)

    def moved(self):
        return {"T23.md": ["# タスク 23 — 見本", ""] + task("T.23.0") + task("T.23.1")}

    def test_open_task_with_result_block_passes(self):
        """済みが `tasks/` に、未着手が `TASKS.md` にある形は通る (注記だけ)。"""
        md = (["# TASKS.md — 未着手のタスク", "", "### タスク 23 (2026-10-22 に決めた: 見本)", ""]
              + task("T.23.99", status=" ", body=("目的", "受け入れ基準")))
        code, out = self.build_open(md, [phase(23, [], link=True, count=2)], self.moved())
        self.assertEqual(code, 0, out)
        self.assertIn("TASKS.md の未着手 1 本 (うち [x] 0 本)", out)
        self.assertIn("タスク 23 は tasks/T23.md (済み 2 本) と TASKS.md (未着手 1 本)", out)

    def test_done_task_left_in_tasks_md(self):
        """未着手のファイルに `- [x]` が残っていたら NG (移し忘れ)。"""
        md = (["# TASKS.md — 未着手のタスク", "", "### タスク 24 (2026-10-22 に決めた: 見本)", ""]
              + task("T.24.99", status="x"))
        code, out = self.build_open(md, [], {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "TASKS.md のタスク 24 に [x] が 1 本ある")

    def test_number_in_wrong_phase(self):
        md = (["# TASKS.md — 未着手のタスク", "", "### タスク 24 (2026-10-22 に決めた: 見本)", ""]
              + task("T.23.99", status=" "))
        code, out = self.build_open(md, [], {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T.23.99 がタスク 24 の節にある")

    def test_same_number_in_both_places(self):
        """同じ番号が `TASKS.md` と `tasks/TNN.md` に本文つきで出たら NG。"""
        md = (["# TASKS.md — 未着手のタスク", "", "### タスク 23 (2026-10-22 に決めた: 見本)", ""]
              + task("T.23.0", status=" "))
        code, out = self.build_open(md, [phase(23, [], link=True, count=2)], self.moved())
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T.23.0 が本文つきで 2 回出る")


class Parse(unittest.TestCase):
    def test_section5_stops_at_next_heading(self):
        lines = ["## 4. 前", "## 5. これから", "本文", "## 付録 A.", "外"]
        got = [text for _, text in cl.section5(lines)]
        self.assertEqual(got, ["本文"])

    def test_task_line_rejects_non_tasks(self):
        got = cl.tasks_in([(0, "- [x] **T.24.10 台帳の検査**"),
                           (1, "- [x] T.24.11 題名が太字でない"),
                           (2, "  - [x] **T.24.12 入れ子**")], "x")
        self.assertEqual([t.ident for t in got], ["T.24.10"])

    def test_body_labels(self):
        got = cl.tasks_in([(0, "- [x] **T.1.1 本文あり**"), (1, "  - 目的: a"), (2, "  - 結果 (2026-01-01): b"),
                           (3, "- [x] **T.1.2 追記だけ**"), (4, "  - 結果 (2026-01-01): b")], "x")
        self.assertTrue(got[0].has_body)
        self.assertFalse(got[1].has_body)

    def test_ident_with_letter(self):
        got = cl.tasks_in([(0, "- [x] **T.17.0a 英字つき**")], "x")
        self.assertEqual([t.ident for t in got], ["T.17.0a"])

    def test_missing_todo(self):
        with tempfile.TemporaryDirectory() as tmp:
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                code = cl.main(["check-ledger.py", tmp])
        self.assertEqual(code, 1)
        self.assertIn("台帳が読めない", buf.getvalue())


class RealLedger(unittest.TestCase):
    def test_repository_ledger_passes(self):
        """本物の台帳も通ること (道具を足したときに、いまの木で 1 回は確かめる)。"""
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = cl.main(["check-ledger.py"])
        self.assertEqual(code, 0, buf.getvalue())
        self.assertTrue(re.search(r"tasks/TNN\.md \d+ 本", buf.getvalue()))


class Ledger28(LedgerCase):
    """T.28.21 で足した (5)〜(8): 索引と範囲・済んだ節・現在地の版・未着手の参照。"""

    def base(self, readme=None, tasks_md=None, todo_extra=None, t2_lines=None):
        sections = [phase(1, [], link=True, count=1), phase(2, [], link=True, count=1)]
        self.write("TODO.md", ["# 台帳の見本", "",
                               "> | `tasks/TNN.md` | 済んだタスクのタスク本文 (T.15〜T.2。1 文字も変えずに移したもの) |", "",
                               "## 2. 現在地", "",
                               "### デプロイ先の現在地 (2026-10-25、tokyo に 0.2.1 をデプロイして 24 時間)", "",
                               "## 5. これから (未着手)", ""]
                   + [line for sec in sections for line in sec + [""]] + (todo_extra or []))
        self.write("tasks/T1.md", ["# タスク 1 — 見本", ""] + task("T.1.1"))
        self.write("tasks/T2.md", ["# タスク 2 — 見本", ""] + task("T.2.1")
                   + (t2_lines if t2_lines is not None else ["  - 前提: **デプロイ済み** (2026-10-25)、版 `0.2.1+abcdef0`。", ""]))
        self.write("tasks/README.md", readme or ["# tasks/ — 済んだタスクの本文 (タスク 0〜2)", "",
                                                  "| T.1.1 | [T1.md](T1.md) | 見本 |", "| T.2.1 | [T2.md](T2.md) | 見本 |"])
        if tasks_md is not None:
            self.write("TASKS.md", tasks_md)
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = cl.main(["check-ledger.py", self.root])
        return code, buf.getvalue()

    def test_clean_tree_passes(self):
        code, out = self.base(tasks_md=["# TASKS", "", "### タスク 3 (2026-10-25 に起こした: 見本)", ""]
                              + task("T.3.1", status=" "))
        self.assertEqual(code, 0, out)

    def test_readme_missing_a_row(self):
        code, out = self.base(readme=["# tasks/ — 済んだタスクの本文 (タスク 0〜2)", "", "| T.1.1 | [T1.md](T1.md) | 見本 |"])
        self.assert_ng(out, "tasks/README.md の表に tasks/T2.md の行が無い")

    def test_ranges_must_reach_the_top(self):
        code, out = self.base(readme=["# tasks/ — 済んだタスクの本文 (タスク 0〜1)", "",
                                      "| T.1.1 | [T1.md](T1.md) | 見本 |", "| T.2.1 | [T2.md](T2.md) | 見本 |"])
        self.assert_ng(out, "「タスク 0〜1」(いちばん大きいのは 2)")
        self.write("tasks/T3.md", ["# タスク 3 — 見本", ""] + task("T.3.1"))
        code, out = self.base(todo_extra=["### タスク 3 (2026-10-25: 見本)", "",
                                          "**済んだ 1 本の本文は [`tasks/T3.md`](tasks/T3.md) に移した**", ""])
        self.assert_ng(out, "TODO.md 冒頭の表が「T.15〜T.2」(いちばん大きいのは 3)")

    def test_finished_section_left_in_tasks_md(self):
        code, out = self.base(tasks_md=["# TASKS", "", "### タスク 3 (2026-10-25 に起こした: 見本)", "",
                                        "全部済んだ。", ""])
        self.assert_ng(out, "TASKS.md のタスク 3 の節に未着手が 0 本")

    def test_current_heading_must_match_the_latest_deploy(self):
        code, out = self.base(t2_lines=["  - 前提: **デプロイ済み** (2026-10-26)、版 `0.2.5+1234567`。", ""])
        self.assert_ng(out, "§2 の「デプロイ先の現在地」の版が 0.2.1 (tasks/ のいちばん新しい「デプロイ済み」は 0.2.5)")

    def test_open_reference_must_exist(self):
        code, out = self.base(todo_extra=["締めの T.2.99 は未着手の T.2.99 は `TASKS.md` にある。", "",
                                          "引用の「未着手の T.9.9 は TASKS.md」は数えない。"])
        self.assert_ng(out, "「未着手の T.2.99」が TASKS.md に無い")
        self.assertNotIn("T.9.9", out)

    def test_relative_links_from_where_they_live(self):
        # T.32.38: TASKS.md から tasks/ へ移したときに `tasks/T1.md` のまま残すと、tasks/ の中からは切れる
        code, out = self.base(t2_lines=["  - 結果: 数字は [`tasks/T1.md`](tasks/T1.md) の T.1.1 の結果。", ""])
        self.assert_ng(out, "tasks/T2.md:9 のリンク tasks/T1.md の指す先が無い (tasks/ から辿る")
        # 移した先から辿れる形なら通る (ページ内・URL・コードの中の例は見ない)
        code, out = self.base(t2_lines=["  - 結果: 数字は [`tasks/T1.md`](T1.md#t11) と [根](../TODO.md)、"
                                        "[外](https://example.invalid/x)、[ここ](#here)、例 `[x](nope.md)`。",
                                        "  - 前提: **デプロイ済み** (2026-10-25)、版 `0.2.1+abcdef0`。", ""])
        self.assertEqual(code, 0, out)
        # TASKS.md と TODO.md は根から辿る
        code, out = self.base(tasks_md=["# TASKS", "", "[索引](tasks/README.md) と [無い](tasks/T9.md)", "",
                                        "### タスク 3 (2026-10-25 に起こした: 見本)", ""] + task("T.3.1", status=" "),
                              todo_extra=["[切れ](docs/none.md)", ""])
        self.assert_ng(out, "TASKS.md:3 のリンク tasks/T9.md の指す先が無い (根から辿る")
        self.assert_ng(out, "TODO.md:")
        self.assertIn("docs/none.md", out)
        self.assertNotIn("tasks/README.md の指す先", out)


if __name__ == "__main__":
    unittest.main()
