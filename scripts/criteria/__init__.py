# `scripts/snapshot_criteria.py` (判定表) の中身を割って置く所 (T.34.124〜T.34.129)。
#
# 入口は `snapshot_criteria.py` のまま (README がファイル名で指し、`snapshot-diff.py`・
# `snapshot-summary.py`・試験が約 60 の名前を名前で取り込んでいる)。そちらが全部を同じ名前で
# 取り込み直すので、ここのモジュールを直接 import する必要は無い。依存は標準ライブラリと
# `scripts/proxydata.py` だけ (`scripts/` が `sys.path` にあること。`snapshot_criteria.py` が入れる)。
