"""`scripts/check-position-refs.py` の試験 (T37.9)。

`findings()` に作り物の文を渡して、落とす 5 種類 (`§`・付録・章の番号・札つきのタスク・記号の検査名) と、
落とさないもの (外の規格の節・記号そのものを名前で書いたもの・Rust の見出しに見える行) を見る。
除外表は作り物に差し替えて、ファイルごと・見出しの範囲・その行だけ・ファイルの終わりまでの 4 つの粒と、
何も外していない除外の行が NG になることを、一時のリポジトリで見る。
"""

import contextlib
import io
import os
import subprocess
import tempfile
import unittest

import _testing

cpr = _testing.load("check_position_refs", "check-position-refs.py")


def kinds(path, text):
    return [(n, k) for n, k, _hit, _line in cpr.findings(path, text)]


class Kinds(unittest.TestCase):
    def setUp(self):
        self.saved = cpr.SKIP
        cpr.SKIP = []

    def tearDown(self):
        cpr.SKIP = self.saved

    def test_section_sign_is_dropped(self):
        self.assertEqual(kinds("docs/a.md", "`TODO.md §0` を見る\n§2 の表"), [(1, "§"), (2, "§")])

    def test_external_rfc_sections_are_kept(self):
        text = ("RFC 9110 §7.6.3 は要求と応答の両方\n"
                "(RFC 9111 §3.2・§4.3.4。T28.2)\n"
                "`Age` は end-to-end、RFC 9110 §7.6・9111 §4.2.3\n"
                "RFC 9110 §7.6.2・§9.3.7・§9.3.8")
        self.assertEqual(kinds("crates/x/src/a.rs", text), [])

    def test_rfc_then_own_section_is_still_dropped(self):
        # RFC の節のあとに、離れて書いた自分の文書の § は残らない
        self.assertEqual(kinds("README.md", "RFC 9112 §6.1 のとおり。詳しくは §5 を見る"), [(1, "§")])

    def test_naming_the_symbol_itself_is_kept(self):
        self.assertEqual(kinds("TASKS.md", "外の規格を除いて、`§` と章の番号の参照を落とす"), [])

    def test_appendix(self):
        self.assertEqual(kinds("docs/a.md", "付録 A の表\n付録の見本"), [(1, "付録")])

    def test_numbered_heading_in_documents(self):
        text = "## 3. 1 時間後\n### 0.5 置く\n## 3,585 行の中身\n### 24 時間後\n```\n## 2. 出力の見本\n```"
        self.assertEqual(kinds("README.md", text), [(1, "章の番号")])

    def test_numbered_heading_is_not_seen_outside_documents(self):
        # 道具の出力の見出し (`p("## 1. 再起動")`) や shell のコメントは文書の見出しではない
        self.assertEqual(kinds("scripts/a.sh", "## 1. 再起動"), [])

    def test_numbered_item_reference(self):
        self.assertEqual(kinds("docs/a.md", "「エージェントが守ること」の 2. (全部通す) は古い"), [(1, "章の番号")])

    def test_labelled_task_reference(self):
        text = "(T15.0 (11)。\nT28.10 (b) から\nT18.0 の (g) と\nT15.0 は (1) 新しい"
        self.assertEqual(kinds("crates/x/src/a.rs", text),
                         [(1, "札つきのタスク"), (2, "札つきのタスク"), (3, "札つきのタスク")])

    def test_check_named_by_symbol(self):
        text = ("`check-docs.sh` の (b) が\n"
                "check-ledger.py の (3)\n"
                "(決まり c。--task で上げる)\n"
                "決まり [上げ忘れ] で落ちる\n"
                "`check-docs.sh` の環境変数の検査\n"
                "`check-dashboard.js` の 11 番目の検査が見る\n"
                "`stat` の 2 番目の項目 (外の形式の位置は落とさない)")
        self.assertEqual(kinds("scripts/a.py", text),
                         [(1, "記号の検査名"), (2, "記号の検査名"), (3, "記号の検査名"), (6, "記号の検査名")])

    def test_inline_enumeration_is_kept(self):
        # その場の列挙 (どこからも指されていない札) は落とさない
        self.assertEqual(kinds("docs/a.md", "理由は (a) 宛先を IP で書いている、(b) 設定の間違い"), [])


class Skip(unittest.TestCase):
    def setUp(self):
        self.saved = cpr.SKIP

    def tearDown(self):
        cpr.SKIP = self.saved

    def test_four_spans(self):
        cpr.SKIP = [
            ("tasks/T*.md", None, None, "記録"),
            ("TODO.md", None, "## 対応表", "表"),
            ("docs/m.md", None, "以降 ## 古い版", "移した記録"),
            ("scripts/c.py", "§", "(§2。T22.1)", "凍結"),
        ]
        self.assertEqual(kinds("tasks/T14.md", "§2 の表"), [])
        todo = "## 対応表\n| §0 | goals |\n### 小見出し §1\n## 次\n§2 は落ちる"
        self.assertEqual(kinds("TODO.md", todo), [(5, "§")])
        m = "## 新しい\n§1\n## 古い版\n§2\n## もっと古い版\n§3"
        self.assertEqual(kinds("docs/m.md", m), [(2, "§")])
        c = '"(§2。T22.1)"\n"T18.0 の (g)"\n"§3"'
        self.assertEqual(kinds("scripts/c.py", c), [(2, "札つきのタスク"), (3, "§")])

    def test_used_records_which_entry_skipped(self):
        cpr.SKIP = [("a.md", None, None, "x"), ("b.md", None, None, "y")]
        used = set()
        cpr.findings("a.md", "§1", used)
        self.assertEqual(used, {0})


@unittest.skipIf(subprocess.run(["git", "--version"], capture_output=True).returncode != 0, "git が無い")
class Main(unittest.TestCase):
    def setUp(self):
        self.saved = cpr.SKIP
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        subprocess.run(["git", "init", "-q", self.root], check=True)

    def tearDown(self):
        cpr.SKIP = self.saved
        self.tmp.cleanup()

    def write(self, name, text):
        path = os.path.join(self.root, name)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
        subprocess.run(["git", "-C", self.root, "add", name], check=True)

    def run_main(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = cpr.main(["check-position-refs.py", self.root])
        return code, buf.getvalue()

    def test_clean_tree_passes(self):
        cpr.SKIP = [("tasks/T*.md", None, None, "記録")]
        self.write("README.md", "RFC 9110 §7.6.3。`docs/perf.md` の「運用」\n")
        self.write("tasks/T1.md", "§2 の表\n")
        code, out = self.run_main()
        self.assertEqual(code, 0, out)
        self.assertIn("[位置で指す識別] 2 ファイルを見た (除外 1)", out)
        self.assertIn("    除外: tasks/T*.md — 記録", out)

    def test_untracked_files_are_not_read(self):
        cpr.SKIP = []
        self.write("README.md", "ok\n")
        with open(os.path.join(self.root, "notes.md"), "w", encoding="utf-8") as f:
            f.write("§2\n")
        self.assertEqual(self.run_main()[0], 0)

    def test_finding_is_ng_with_place(self):
        cpr.SKIP = []
        self.write("docs/a.md", "前置き\n詳しくは §5 を見る\n")
        code, out = self.run_main()
        self.assertEqual(code, 1)
        self.assertIn("  NG [位置で指す識別]: docs/a.md:2 の [§] 「§」: 詳しくは §5 を見る", out)

    def test_unused_skip_entry_is_ng(self):
        cpr.SKIP = [("docs/gone.md", None, None, "もう無い")]
        self.write("README.md", "ok\n")
        code, out = self.run_main()
        self.assertEqual(code, 1)
        self.assertIn("除外表の「docs/gone.md」は何も外していない", out)


if __name__ == "__main__":
    unittest.main()
