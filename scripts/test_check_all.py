#!/usr/bin/env python3
"""`scripts/check-all.sh` (短絡しない一括確認。T.29.29) のテスト。

    python3 -m unittest discover -s scripts        # リポジトリの根から

このリポジトリを一時のディレクトリへ `git clone --depth 1` し (HEAD の中身。`check-all.sh` だけは作業ツリーのものを
置く)、**2 つを同時に壊して**どちらの名前も出ることを見る: 本体の `src/main.rs` の書式 (fmt) と、
クレートの版を本体より大きくする (versions。決まり b)。`&&` でつないでいれば fmt で止まって versions は
見えなかった。間に挟んだ docs (壊していない) も回ることで「止まらずに続けた」を確かめる。
clippy・unittest は回さない (遅いのと、unittest の中から unittest を回すことになるため)。
`cargo` (rustfmt) か `git` が無い機械では飛ばす。
"""

import os
import re
import shutil
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
ENV = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")


@unittest.skipUnless(shutil.which("git") and shutil.which("cargo"), "git と cargo (rustfmt) が要る")
class CheckAllTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix="test-check-all-")
        cls.repo = os.path.join(cls.tmp.name, "repo")
        # 歴史は 1 つだけ (crate-versions.py は HEAD を「版の行を書いた」とみなすので、壊したものだけが落ちる)。
        # 元が浅いクローンでも読めるように file:// で取る (ただのパスだと浅い歴史の穴を辿って落ちる)
        subprocess.run(["git", "clone", "-q", "--depth", "1", "file://" + ROOT, cls.repo], env=ENV, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        shutil.copy2(os.path.join(HERE, "check-all.sh"), os.path.join(cls.repo, "scripts", "check-all.sh"))

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def run_check(self, *args):
        out = subprocess.run(["bash", os.path.join(self.repo, "scripts", "check-all.sh")] + list(args),
                             env=ENV, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                             universal_newlines=True, timeout=300)
        return out.returncode, out.stdout

    def edit(self, rel, fn):
        path = os.path.join(self.repo, rel)
        with open(path, encoding="utf-8") as f:
            text = f.read()
        with open(path, "w", encoding="utf-8") as f:
            f.write(fn(text))
        return text

    def test_two_breakages_are_both_named(self):
        main_rs = self.edit("src/main.rs", lambda t: t + "\nfn   badly_formatted( ){}\n")
        toml = self.edit("crates/base/Cargo.toml",
                         lambda t: re.sub(r'(?m)^version = "[^"]*"$', 'version = "0.999.0"', t, count=1))
        try:
            code, out = self.run_check("--only", "fmt,docs,versions")
            self.assertEqual(code, 1, out)
            self.assertRegex(out, r"(?m)^NG  fmt ")
            self.assertRegex(out, r"(?m)^NG  versions ")
            self.assertIn("proxy-base", out)  # 落ちた理由 (どのクレートか) も出る
            # 壊していない docs は fmt のあとも回っている (短絡していない)
            self.assertRegex(out, r"(?m)^ok  docs ")
            self.assertIn("check-all: 落ちた 2 本: fmt versions", out)
        finally:
            self.edit("src/main.rs", lambda _: main_rs)
            self.edit("crates/base/Cargo.toml", lambda _: toml)
        # 戻せば通る
        code, out = self.run_check("--only", "fmt,versions")
        self.assertEqual(code, 0, out)
        self.assertIn("check-all: 全部通った (2 本)", out)

    def test_usage(self):
        code, out = self.run_check("--only", "nope")
        self.assertEqual(code, 2, out)
        self.assertIn("知らない名前: nope", out)


if __name__ == "__main__":
    unittest.main()
