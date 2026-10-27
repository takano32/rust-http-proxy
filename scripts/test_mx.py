#!/usr/bin/env python3
"""`scripts/mx` (機械のロック) のテストを unittest から回す (T.17.15)。

    python3 -m unittest discover -s scripts        # リポジトリの根から
    cd scripts && python3 -m unittest              # scripts の中から

本体は `scripts/test_mx.sh` (bash)。mx は `flock` / `setsid` / `pgrep` とプロセスグループで
できているので、試験も bash で書き、ここでは `subprocess` で呼んで終了コードだけを見る。
落ちたときは test_mx.sh の出力 (どの項目が NG か) をそのまま失敗の文面に載せる。
mx の待つ相手は test_mx.sh が `MX_OTHERS` で自分の偽物だけにする (機械に本物のプロキシが 1 本
動いているだけで discover が 7 分半たっても終わらなかった。T.29.31)。
項目 7 (起動の直後の TERM。T.18.3) は別の 1 本にしてある (偽物のベンチを起動しないので、
これだけを繰り返し回せる: `python3 -m unittest scripts/test_mx.py -k term`)。
"""

import os
import subprocess
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


class MxTest(unittest.TestCase):
    def run_items(self, *items):
        proc = subprocess.run(
            ["bash", os.path.join(HERE, "test_mx.sh"), *items],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            # 待ちは test_mx.sh が `MX_WAIT=20` に縮めてある。それでも止まれば 120 秒で打ち切る (T.29.31)
            timeout=120,
        )
        self.assertEqual(proc.returncode, 0, proc.stdout)

    def test_mx(self):
        # 7 項目で 10 秒ほど (T.28.29 で `MX_POLL=0.1` にして縮めた)。8 (待つ相手の型) は T.29.31。
        # 他人のプロキシが走っていても待たない (`MX_OTHERS` が自分の偽物だけを見る)
        self.run_items("1", "2", "3", "4", "5", "6", "8")

    def test_term_right_after_launch_leaves_no_child(self):
        # 起動の直後 (mx が `CHILD=$!` を実行する前、子が `setsid` を呼ぶ前) の TERM で子が残らない。4 秒ほど
        self.run_items("7")


if __name__ == "__main__":
    unittest.main()
