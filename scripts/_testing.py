"""`scripts/test_*.py` が共に使う小物 (T.34.135)。

道具はファイル名に `-` を含む (`snapshot-diff.py`) ので `import` では読めない。どの試験も同じ
`importlib` の 4 行と、`main()` の標準出力を文字列で受け取る 4 行を自分の頭に写していたので、
ここに 1 つだけ置く。名前が `test` で始まらないので、`unittest discover` は試験として読まない。
"""

import contextlib
import importlib.util
import io
import os

HERE = os.path.dirname(os.path.abspath(__file__))


def load(name, filename):
    """`scripts/<filename>` を `name` という名前のモジュールとして読む。"""
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, filename))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def run_main(mod, argv):
    """`mod.main(argv)` を回して、標準出力を文字列で受け取る。"""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        mod.main(argv)
    return out.getvalue()
