"""`scripts/check-words.py` の試験 (T39.10)。

`findings()` に作り物の文を渡して、使わない言葉 (「雪像」) を落とすことと、除外表の 3 つの粒 (ファイルごと・
Python の文字列だけ・その行だけ) を見る。Python の文字列の粒は、docstring とコメントは外さない。
何も外していない除外の行が NG になることは、一時のリポジトリで見る。
"""

import contextlib
import io
import os
import subprocess
import tempfile
import unittest

import _testing

cw = _testing.load("check_words", "check-words.py")


def hits(path, text):
    return [n for n, _old, _line in cw.findings(path, text)]


class Findings(unittest.TestCase):
    def setUp(self):
        self.saved = cw.SKIP
        cw.SKIP = []

    def tearDown(self):
        cw.SKIP = self.saved

    def test_the_word_is_dropped_anywhere(self):
        self.assertEqual(hits("scripts/a.sh", "# 雪像を撮る\necho スナップショット\nprintf '雪像'\n"), [1, 3])

    def test_the_new_word_is_kept(self):
        self.assertEqual(hits("README.md", "スナップショットを 1 枚撮る\n"), [])

    def test_a_whole_file_is_skipped(self):
        cw.SKIP = [("scripts/a.py", None, "理由")]
        used = set()
        self.assertEqual(cw.findings("scripts/a.py", "x = '雪像'\n", used), [])
        self.assertEqual(used, {0})

    def test_strings_are_skipped_but_comments_and_docstrings_are_not(self):
        cw.SKIP = [("scripts/criteria/*.py", cw.STRINGS, "凍結")]
        text = ('"""雪像の読み方。"""\n'
                "\n"
                "def f(c):\n"
                '    """後の雪像から読む。"""\n'
                "    # 雪像に無ければ判定できず\n"
                '    return ("後の雪像に `/recent` の部が無い",\n'
                '            f"前の雪像 {c}"\n'
                '            "の続き 雪像")\n')
        self.assertEqual(hits("scripts/criteria/phase99.py", text), [1, 4, 5])

    def test_a_line_is_skipped_by_its_text(self):
        cw.SKIP = [("scripts/t.py", "**日次の雪像**", "凍結")]
        self.assertEqual(hits("scripts/t.py", "a('**日次の雪像**')\nb('雪像')\n"), [2])


class Main(unittest.TestCase):
    def setUp(self):
        self.saved = cw.SKIP

    def tearDown(self):
        cw.SKIP = self.saved

    def repo(self, files):
        d = tempfile.mkdtemp(prefix="t3910-")
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", d]))
        for name, text in files.items():
            os.makedirs(os.path.join(d, os.path.dirname(name)), exist_ok=True)
            with open(os.path.join(d, name), "w", encoding="utf-8") as f:
                f.write(text)
        subprocess.run(["git", "-C", d, "init", "-q"], check=True)
        subprocess.run(["git", "-C", d, "add", "-A"], check=True)
        return d

    def run_main(self, d):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = cw.main(["check-words.py", d])
        return rc, out.getvalue()

    def test_clean_repository_passes(self):
        cw.SKIP = [("scripts/a.py", cw.STRINGS, "凍結")]
        d = self.repo({"scripts/a.py": "x = '雪像'\n", "README.md": "スナップショット\n", "docs/b.md": "雪像\n"})
        rc, out = self.run_main(d)
        self.assertEqual(rc, 0, out)
        self.assertIn("[言葉の決まり] 2 ファイルを見た (除外 1)", out)

    def test_a_word_and_an_unused_skip_fail(self):
        cw.SKIP = [("scripts/gone.py", None, "古い行")]
        d = self.repo({"scripts/a.sh": "echo 雪像\n"})
        rc, out = self.run_main(d)
        self.assertEqual(rc, 1, out)
        self.assertIn("NG [言葉の決まり]: scripts/a.sh:1 に「雪像」(「スナップショット」と書く)", out)
        self.assertIn("除外表の「scripts/gone.py」は何も外していない", out)


if __name__ == "__main__":
    unittest.main()
