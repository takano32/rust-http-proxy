#!/usr/bin/env python3
"""コードの環境変数とエンドポイントが、どれかの仕様に載っているかを見る (T38.5)。

`scripts/check-docs.sh` の [仕様に載っているか] として毎回回す。[環境変数] の検査が集めたコードの
`"PROXY_…"` / `"SERVER_…"` の名前と、[エンドポイント] の検査が集めた経路表 (`dispatch.rs` の match の腕。
別名は代表の綴りに寄せたもの) を受け取り、どの名前も仕様 (`SPECS.md` と `specs/*.md`) のどこかに
出てくるかを突き合わせる。[環境変数] は仕様 S7 の表の鍵だけを見るので、こちらは**ほかの仕様の本文も含めて**
どこかで振る舞いが決めてあるかを見る。

名前の当て方:
  環境変数     … 前後が英大文字・数字・`_` でないところに、その名前がそのまま出てくる
                 (`PROXY_DNS_TTL_SECS` は `PROXY_DNS_TTL_SECS_X` の中では数えない)
  エンドポイント … 前が英数字・`_`・`.`・`-` でなく、後ろが英数字・`_`・`-` でも `/` + 英字でもないところに
                 そのパスが出てくる (`/hosts` は `/hosts/series` の中では数えない。`/hosts?limit=` は数える)。
                 `/snapshots/<date>` は `/snapshots/<` で始まる書き方 (`/snapshots/<YYYY-MM-DD>` など) を数える

**意図して外すものは下の除外表 (`SKIP`) に理由つきで持つ** (黙って無視しない)。仕様に出てこない名前が 1 つでも
あるか、外す必要の無くなった除外の行があれば (仕様に書いたあとに残った古い行)、理由を印字して exit 1。
使うのは標準ライブラリだけ。どのファイルも読むだけ。

使い方: scripts/check-spec-names.py <環境変数の一覧> <エンドポイントの一覧> [リポジトリの根]
        (一覧は 1 行 1 名前。`check-docs.sh` が作る。根を省けばこのファイルの 2 つ上)
"""

import glob
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

# (名前, 理由)。ここに書いた名前は、仕様に出てこなくても NG にしない。
# `/` (案内) は [エンドポイント] の集め方が先に外している (`norm_ep`)。
SKIP = []


def spec_text(root):
    """仕様の全文 (`SPECS.md` と `specs/*.md`)。"""
    paths = [os.path.join(root, "SPECS.md")] + sorted(glob.glob(os.path.join(root, "specs", "*.md")))
    return "\n".join(open(p, encoding="utf-8").read() for p in paths if os.path.exists(p))


def env_found(name, text):
    return re.search(r"(?<![A-Z0-9_])" + re.escape(name) + r"(?![A-Z0-9_])", text) is not None


def endpoint_found(path, text):
    if path.endswith("/<date>"):
        return re.search(r"(?<![\w.-])" + re.escape(path[: -len("date>")]), text) is not None
    return re.search(r"(?<![\w.-])" + re.escape(path) + r"(?![\w-]|/[A-Za-z])", text) is not None


def check(envs, endpoints, text, skip=None):
    """(見た名前の数, 仕様に無い名前, 使っていない除外) を返す。"""
    skip = dict(SKIP if skip is None else skip)
    missing, used = [], set()
    for kind, names, found in (("環境変数", envs, env_found), ("エンドポイント", endpoints, endpoint_found)):
        for name in names:
            if name in skip:
                used.add(name)
                continue
            if not found(name, text):
                missing.append((kind, name))
    unused = sorted(n for n in skip if n not in used)
    return len(envs) + len(endpoints), missing, unused


def read_list(path):
    return [line.strip() for line in open(path, encoding="utf-8") if line.strip()]


def main(argv):
    if len(argv) < 3:
        print(__doc__.strip().split("\n")[-2])
        return 2
    root = argv[3] if len(argv) > 3 else ROOT
    envs, endpoints = read_list(argv[1]), read_list(argv[2])
    total, missing, unused = check(envs, endpoints, spec_text(root))
    print("[仕様に載っているか] 環境変数 %d 件・エンドポイント %d 件を SPECS.md・specs/*.md で探した (除外 %d)"
          % (len(envs), len(endpoints), len(SKIP)))
    for name, why in SKIP:
        print("    除外: %s — %s" % (name, why))
    for kind, name in missing:
        print("  NG [仕様に載っているか]: %s %s がどの仕様にも出てこない (振る舞いを仕様に書くか、理由を除外表に書く)"
              % (kind, name))
    for name in unused:
        print("  NG [仕様に載っているか]: 除外表の %s は外す必要が無い (コードに無いか、仕様に出てくる)" % name)
    return 1 if (missing or unused) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
