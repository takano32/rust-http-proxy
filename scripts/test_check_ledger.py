#!/usr/bin/env python3
"""`scripts/check-ledger.py` (台帳の 1 対 1) の単体テスト (T24.10)。

    python3 -m unittest discover -s scripts        # リポジトリの根から

一時のディレクトリに `TODO.md` と `tasks/phaseNN.md` の見本を置き、`check-ledger.py` の
`collect()` / `main()` をそのまま呼ぶ。**本物の台帳は読まない** (中身が進んでもテストが揺れない)。
見るのは、通る形 1 つと、規則ごとにわざと壊した形:

  (1) 全部 `- [x]` の Phase が §5 に残る / 移した Phase にタスクの行が残る
  (2) リンクが無い / 指す先が無い / 見出しが無い / 番号が違う
  (3) 番号が本文つきで 2 回出る (片方が `結果` だけの追記なら通る) / 番号と置き場所が違う
  (4) 1 行目が `# Phase NN — …` でない
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
    lines = ["### Phase %d (2026-10-22 に決めた: 見本)" % num, ""]
    if link or moved_line:
        n = len(blocks) if count is None else count
        if moved_line is None:
            moved_line = ("**済んだ %d 本の本文は [`tasks/phase%d.md`](tasks/phase%d.md) に移した**"
                          " (2026-10-22、T24.9)。" % (n, num, num))
        lines += [moved_line, ""]
    for block in blocks:
        lines += block
    return lines


def todo_md(sections, extra=None):
    """§5 の外にも `- [x]` を置いた `TODO.md` (§5 だけを見ていることの確認)。"""
    lines = ["# 台帳の見本", "", "## 4. 測って採らなかったもの", "",
             "- [x] **T1.1 §4 の見本 (台帳のタスクではない)**", "",
             "## 5. これから (未着手)", ""]
    for section in sections:
        lines += section + [""]
    lines += ["## 付録 A. 計測の記録 (時系列)", "", "- [x] **T0.1 付録の見本**", ""]
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
             phase(24, [task("T24.0"), task("T24.1", status=" ", body=())])],
            {"phase23.md": ["# Phase 23 — 見本", ""] + task("T23.0") + task("T23.1")})
        self.assertEqual(code, 0, out)
        self.assertNotIn("NG", out)
        self.assertIn("tasks/phaseNN.md 1 本 (タスク 2 本)", out)
        self.assertIn("§5 に残るタスク 2 本", out)

    def test_instruction_heading_is_not_a_phase(self):
        """`### Phase 15 の渡し方 (…)` のような指示文の節は台帳の節として数えない。"""
        code, out = self.build(
            [["### Phase 15 の渡し方 (2026-09-18 版。指示文の型)", ""] + task("T15.0")],
            {})
        self.assertEqual(code, 0, out)
        self.assertIn("Phase 見出し 0 節", out)

    def test_result_only_note_is_not_a_duplicate(self):
        code, out = self.build(
            [phase(24, [task("T24.1"), task("T24.1", body=("結果",)),
                        task("T24.2", status=" ", body=())])],
            {})
        self.assertEqual(code, 0, out)
        self.assertNotIn("NG", out)
        self.assertIn("注:", out)
        self.assertIn("`結果` だけの追記", out)

    def test_index_file_is_skipped_with_reason(self):
        code, out = self.build(
            [phase(24, [task("T24.0", status=" ", body=())])],
            {"README.md": ["# tasks/ — 索引", ""]})
        self.assertEqual(code, 0, out)
        self.assertIn("除外: tasks/README.md", out)


class Move(LedgerCase):
    def test_all_done_phase_must_move(self):
        code, out = self.build([phase(24, [task("T24.0"), task("T24.1")])], {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "全部 [x] なのに §5 に残っている")

    def test_moved_phase_must_not_keep_rows(self):
        code, out = self.build(
            [phase(23, [task("T23.0")], link=True)],
            {"phase23.md": ["# Phase 23 — 見本", ""] + task("T23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "移したのに §5 にタスクの行が 1 本")


class Link(LedgerCase):
    def test_missing_link(self):
        code, out = self.build([], {"phase23.md": ["# Phase 23 — 見本", ""] + task("T23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "tasks/phase23.md を指す `### Phase 23 (…)` の見出しが §5 に無い")

    def test_phase_file_not_linked_from_its_heading(self):
        code, out = self.build(
            [phase(23, [], link=False)],
            {"phase23.md": ["# Phase 23 — 見本", ""] + task("T23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "が §5 の Phase 23 の見出しからリンクされていない")

    def test_link_to_missing_file(self):
        code, out = self.build([phase(21, [], link=True)], {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "見出しが指す tasks/phase21.md が無い")

    def test_link_number_mismatch(self):
        moved = "**済んだ 1 本の本文は [`tasks/phase22.md`](tasks/phase22.md) に移した** (2026-10-22、T24.9)。"
        code, out = self.build(
            [phase(23, [], moved_line=moved)],
            {"phase22.md": ["# Phase 22 — 見本", ""] + task("T22.0"),
             "phase23.md": ["# Phase 23 — 見本", ""]})
        self.assertEqual(code, 1)
        self.assert_ng(out, "§5 の Phase 23 の見出しが tasks/phase22.md を指している")

    def test_unknown_file_in_tasks(self):
        code, out = self.build(
            [phase(24, [task("T24.0", status=" ", body=())])],
            {"notes.md": ["# 覚え書き", ""]})
        self.assertEqual(code, 1)
        self.assert_ng(out, "tasks/notes.md が台帳の型 `phaseNN.md` に合わない")


class Numbers(LedgerCase):
    def test_duplicate_with_two_bodies(self):
        code, out = self.build(
            [phase(24, [task("T24.1"), task("T24.1", title="別のタスク"),
                        task("T24.2", status=" ", body=())])],
            {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T24.1 が本文つきで 2 回出る")

    def test_duplicate_without_body(self):
        code, out = self.build(
            [phase(24, [task("T24.1", body=("結果",)), task("T24.1", body=("結果",)),
                        task("T24.2", status=" ", body=())])],
            {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "どれも本文を持たない")

    def test_number_in_wrong_phase(self):
        code, out = self.build(
            [phase(24, [task("T23.5", status=" ", body=()), task("T24.0", status=" ", body=())])],
            {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T23.5 が Phase 24 の節にある")

    def test_number_in_wrong_file(self):
        code, out = self.build(
            [phase(23, [], link=True, count=2)],
            {"phase23.md": ["# Phase 23 — 見本", ""] + task("T23.0") + task("T22.1")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T22.1 が Phase 23 のファイルにある")

    def test_number_of_two_files(self):
        """同じ番号が §5 と tasks/ の両方に出る (移し忘れの取り残し) も重複。"""
        code, out = self.build(
            [phase(23, [], link=True),
             phase(24, [task("T23.0"), task("T24.0", status=" ", body=())])],
            {"phase23.md": ["# Phase 23 — 見本", ""] + task("T23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T23.0 が本文つきで 2 回出る")


class Shape(LedgerCase):
    def test_first_line_without_dash(self):
        code, out = self.build(
            [phase(23, [], link=True)],
            {"phase23.md": ["# Phase 23 見本", ""] + task("T23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "tasks/phase23.md の 1 行目が `# Phase NN — …` でない")

    def test_first_line_wrong_number(self):
        code, out = self.build(
            [phase(23, [], link=True)],
            {"phase23.md": ["# Phase 22 — 見本", ""] + task("T23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "1 行目が Phase 22 になっている")

    def test_count_mismatch(self):
        code, out = self.build(
            [phase(23, [], link=True, count=5)],
            {"phase23.md": ["# Phase 23 — 見本", ""] + task("T23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "「済んだ 5 本」が tasks/phase23.md の 1 本と合わない")

    def test_count_line_missing(self):
        code, out = self.build(
            [phase(23, [], moved_line="(移した)")],
            {"phase23.md": ["# Phase 23 — 見本", ""] + task("T23.0")})
        self.assertEqual(code, 1)
        self.assert_ng(out, "「済んだ N 本の本文は …」の行が無い")


class OpenTasks(LedgerCase):
    """`TASKS.md` (未着手) の見方 (T24.13)。`build` に `open_tasks` を渡すと `TASKS.md` を置く。"""

    def build_open(self, tasks_md, sections, files):
        self.write("TASKS.md", tasks_md)
        return self.build(sections, files)

    def moved(self):
        return {"phase23.md": ["# Phase 23 — 見本", ""] + task("T23.0") + task("T23.1")}

    def test_open_task_with_result_block_passes(self):
        """済みが `tasks/` に、未着手が `TASKS.md` にある形は通る (注記だけ)。"""
        md = (["# TASKS.md — 未着手のタスク", "", "### Phase 23 (2026-10-22 に決めた: 見本)", ""]
              + task("T23.99", status=" ", body=("目的", "受け入れ基準")))
        code, out = self.build_open(md, [phase(23, [], link=True, count=2)], self.moved())
        self.assertEqual(code, 0, out)
        self.assertIn("TASKS.md の未着手 1 本 (うち [x] 0 本)", out)
        self.assertIn("Phase 23 は tasks/phase23.md (済み 2 本) と TASKS.md (未着手 1 本)", out)

    def test_done_task_left_in_tasks_md(self):
        """未着手のファイルに `- [x]` が残っていたら NG (移し忘れ)。"""
        md = (["# TASKS.md — 未着手のタスク", "", "### Phase 24 (2026-10-22 に決めた: 見本)", ""]
              + task("T24.99", status="x"))
        code, out = self.build_open(md, [], {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "TASKS.md の Phase 24 に [x] が 1 本ある")

    def test_number_in_wrong_phase(self):
        md = (["# TASKS.md — 未着手のタスク", "", "### Phase 24 (2026-10-22 に決めた: 見本)", ""]
              + task("T23.99", status=" "))
        code, out = self.build_open(md, [], {})
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T23.99 が Phase 24 の節にある")

    def test_same_number_in_both_places(self):
        """同じ番号が `TASKS.md` と `tasks/phaseNN.md` に本文つきで出たら NG。"""
        md = (["# TASKS.md — 未着手のタスク", "", "### Phase 23 (2026-10-22 に決めた: 見本)", ""]
              + task("T23.0", status=" "))
        code, out = self.build_open(md, [phase(23, [], link=True, count=2)], self.moved())
        self.assertEqual(code, 1)
        self.assert_ng(out, "番号 T23.0 が本文つきで 2 回出る")


class Parse(unittest.TestCase):
    def test_section5_stops_at_next_heading(self):
        lines = ["## 4. 前", "## 5. これから", "本文", "## 付録 A.", "外"]
        got = [text for _, text in cl.section5(lines)]
        self.assertEqual(got, ["本文"])

    def test_task_line_rejects_non_tasks(self):
        got = cl.tasks_in([(0, "- [x] **T24.10 台帳の検査**"),
                           (1, "- [x] T24.11 題名が太字でない"),
                           (2, "  - [x] **T24.12 入れ子**")], "x")
        self.assertEqual([t.ident for t in got], ["T24.10"])

    def test_body_labels(self):
        got = cl.tasks_in([(0, "- [x] **T1.1 本文あり**"), (1, "  - 目的: a"), (2, "  - 結果 (2026-01-01): b"),
                           (3, "- [x] **T1.2 追記だけ**"), (4, "  - 結果 (2026-01-01): b")], "x")
        self.assertTrue(got[0].has_body)
        self.assertFalse(got[1].has_body)

    def test_ident_with_letter(self):
        got = cl.tasks_in([(0, "- [x] **T17.0a 英字つき**")], "x")
        self.assertEqual([t.ident for t in got], ["T17.0a"])

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
        self.assertTrue(re.search(r"tasks/phaseNN\.md \d+ 本", buf.getvalue()))


if __name__ == "__main__":
    unittest.main()
