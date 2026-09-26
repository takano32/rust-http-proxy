#!/usr/bin/env python3
"""`scripts/mx` (機械のロック) のテストを unittest から回す (T17.15)。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

本体は `scripts/test_mx.sh` (bash)。mx は `flock` / `setsid` / `pgrep` とプロセスグループで
できているので、試験も bash で書き、ここでは 1 本の `subprocess` で呼んで終了コードだけを見る。
落ちたときは test_mx.sh の出力 (どの項目が NG か) をそのまま失敗の文面に載せる。
"""

import os
import subprocess
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


class MxTest(unittest.TestCase):
    def test_mx(self):
        # 6 項目で 15 秒ほど。他人のベンチが走っている機械では mx の待ちが入るので余裕を持つ
        proc = subprocess.run(
            ["bash", os.path.join(HERE, "test_mx.sh")],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=300,
        )
        self.assertEqual(proc.returncode, 0, proc.stdout)


if __name__ == "__main__":
    unittest.main()
