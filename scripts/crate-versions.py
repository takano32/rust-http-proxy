#!/usr/bin/env python3
"""クレートの版の決まりを数える・確かめる・そろえる道具 (T21.1 で作り、T21.6 を経て T26.3 で決まりを置き換えた)。

決まり (2026-10-23、はかせの決定。T21.6 の「版 = 中身を最後に変えたタスクの番号」を置き換えた。
README の「クレート構成」):

  (1) 版は**デプロイの番号** `X.Y.Z`。クレート (`crates/*`) も本体 (`rust-http-proxy`) も
      `proxy-server` も**同じ版**。
  (2) 上げるのは**デプロイの直前に 1 回だけ**: `--deploy 0.26.0`。タスクの作業中は版を触らない
      (デプロイは複数の Phase をまとめてよいので、タスクごとの bump は邪魔になる)。
  (3) タグは本体の版 (`v0.26.0`)。
  (4) 中身が変わったこと自体は `--check` では NG にしない (デプロイ前は普通の状態)。
      代わりに「前回のデプロイから中身が変わったクレート: N 個。次のデプロイで --deploy する」を印字する。

「中身が変わった」= メンバーのディレクトリ (`crates/<名前>/`) の下の差分のうち、`Cargo.toml` の
`[package]` の `version = …` の 1 行を除いたもの。本体の中身は `src/` の下。

`--check` の決まり (**コミットの文面には頼らない**。文面にタスク番号が無いコミットもあるため):
  (a) 版の形が `X.Y.Z` で、**全クレート (と `proxy-server`) の版 = 本体の版**。
  (b) タグの上 (HEAD を指す `v*` のタグがある) なら、本体の版 = タグ。
  (c) 中身の変化は NG にしない。前回のデプロイ (= いまの版を書いたコミット。作業ツリーが版を
      書いたなら HEAD の版を書いたコミット) から中身が変わったクレートを数えて印字するだけ。

使い方 (リポジトリの根から。`-C DIR` で別のリポジトリ):
  scripts/crate-versions.py --list                 クレートごとに 今の版・中身を最後に変えたコミット・
                                                   その版の行を書いたコミット・合うか
  scripts/crate-versions.py --check                決まりに合わないものの名前と理由を出して 1 で抜ける
  scripts/crate-versions.py --deploy 0.26.0        全クレート + 本体 + proxy-server を同じ版にそろえる
                                                   (`Cargo.lock` の該当の行も直す)。デプロイの直前に 1 回

`--task` は廃止した (T26.3)。渡されたら `--deploy` を案内するエラー (終了コード 2) にする。

外部のパッケージは使わない (標準ライブラリだけ)。`release.yml` の ubuntu-22.04 の python3 (3.10) で動くこと
(`tomllib` は 3.11 からなので使わず、`Cargo.toml` は行で読む)。
終了コード: 0 = 合う / 1 = 合わない (`--check`) / 2 = 使い方の誤り・リポジトリの形が想定と違う等。
"""

import argparse
import os
import re
import subprocess
import sys

TAG_RE = re.compile(r"^v(\d+\.\d+\.\d+)$")
# 決まり (a): `X.Y.Z` (デプロイの番号)
VER_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
# `[package]` の版の行 (`version = "0.26.0"`)。行の頭から書いてあるものだけを見る
VERSION_LINE_RE = re.compile(r'^version\s*=\s*"([^"]*)"\s*$')
SERVER = "proxy-server"
ROOT_CONTENT = "src"  # 本体の「中身」
NEXT_VERSION = "X.Y.Z"  # 案内に出すデプロイの版の形


class Error(Exception):
    """使い方の誤り・リポジトリの形が想定と違う (終了コード 2)。"""


def git(repo, *args, check=True):
    out = subprocess.run(["git", "-C", repo] + list(args), stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, universal_newlines=True)
    if check and out.returncode != 0:
        raise Error("git %s: %s" % (" ".join(args), out.stderr.strip()))
    return out.stdout if out.returncode == 0 else None


def ver_key(text):
    m = VER_RE.match(text or "")
    return tuple(int(x) for x in m.groups()) if m else None


# --- Cargo.toml を行で読む ---------------------------------------------------

def package_fields(text):
    """`[package]` の節の `name` と `version` と、版の行の番号 (0 始まり) を返す。"""
    name = version = None
    line_no = None
    section = None
    for i, line in enumerate(text.splitlines()):
        s = line.strip()
        if s.startswith("["):
            section = s
            continue
        if section != "[package]":
            continue
        m = re.match(r'^name\s*=\s*"([^"]*)"', s)
        if m and name is None:
            name = m.group(1)
        m = VERSION_LINE_RE.match(line)
        if m and version is None:
            version, line_no = m.group(1), i
    return name, version, line_no


def without_version_line(text):
    """`[package]` の版の行を除いた中身 (「中身が変わった」を比べるときに使う)。"""
    _, _, line_no = package_fields(text)
    if line_no is None:
        return text
    lines = text.splitlines(True)
    return "".join(lines[:line_no] + lines[line_no + 1:])


def members(root_toml):
    """ルートの `Cargo.toml` の `[workspace]` の `members` (並びのまま)。"""
    m = re.search(r"^\[workspace\][\s\S]*?^members\s*=\s*\[([\s\S]*?)\]", root_toml, re.M)
    if not m:
        raise Error("Cargo.toml に [workspace] の members が無い")
    body = "\n".join(l.split("#", 1)[0] for l in m.group(1).splitlines())
    return re.findall(r'"([^"]+)"', body)


# --- リポジトリを読む ---------------------------------------------------------

class Unit:
    """版を持つ 1 つ (本体かクレート)。toml = 版の行のあるファイル、content = 中身のディレクトリ。"""

    def __init__(self, name, version, toml, content):
        self.name, self.version, self.toml, self.content = name, version, toml, content


class Repo:
    def __init__(self, path):
        top = git(path, "rev-parse", "--show-toplevel")
        self.path = top.strip()
        with open(os.path.join(self.path, "Cargo.toml"), encoding="utf-8") as f:
            root_text = f.read()
        name, version, _ = package_fields(root_text)
        if not name:
            raise Error("ルートの Cargo.toml に [package] の name が無い")
        self.root = Unit(name, version, "Cargo.toml", ROOT_CONTENT)
        self.crates = []
        for member in members(root_text):
            member = member.rstrip("/")
            with open(os.path.join(self.path, member, "Cargo.toml"), encoding="utf-8") as f:
                cname, cversion, _ = package_fields(f.read())
            self.crates.append(Unit(cname, cversion, member + "/Cargo.toml", member))
        self.head = self.rev_of("HEAD")
        self._parents = None
        self._blob_ver = {}
        self._procs = {}

    def units(self):
        return [self.root] + self.crates

    def rev_of(self, rev):
        out = git(self.path, "rev-parse", "--verify", "-q", rev + "^{commit}", check=False)
        return out.strip() if out else None

    def close(self):
        for p in self._procs.values():
            p.stdin.close()
            p.wait()
        self._procs = {}

    # -- コミットのファイルを速く読む (`git cat-file` を 1 本ずつ開けたまま流す) --

    def _cat(self, mode, line):
        p = self._procs.get(mode)
        if p is None:
            p = self._procs[mode] = subprocess.Popen(["git", "-C", self.path, "cat-file", mode],
                                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        p.stdin.write((line + "\n").encode())
        p.stdin.flush()
        return p.stdout.readline().decode().split(), p

    def blob_of(self, rev, path):
        """rev の path の blob の名前 (無ければ None)。"""
        head, _ = self._cat("--batch-check", "%s:%s" % (rev, path))
        return head[0] if len(head) == 3 and head[1] == "blob" else None

    def read_blob(self, blob):
        head, p = self._cat("--batch", blob)
        data = p.stdout.read(int(head[2]) + 1)[:-1]
        return data.decode("utf-8", "replace")

    def show(self, rev, path):
        """rev のファイルの中身 (無ければ None)。rev が None なら作業ツリー。"""
        if rev is None:
            full = os.path.join(self.path, path)
            if not os.path.isfile(full):
                return None
            with open(full, encoding="utf-8", errors="replace") as f:
                return f.read()
        blob = self.blob_of(rev, path)
        return self.read_blob(blob) if blob else None

    def version_at(self, rev, path):
        """rev (None = 作業ツリー) の Cargo.toml の `[package]` の版 (ファイルが無ければ None、行が無ければ "")。"""
        if rev is None:
            text = self.show(None, path)
            return None if text is None else (package_fields(text)[1] or "")
        blob = self.blob_of(rev, path)
        if blob is None:
            return None
        if blob not in self._blob_ver:
            self._blob_ver[blob] = package_fields(self.read_blob(blob))[1] or ""
        return self._blob_ver[blob]

    # -- 歴史 --

    def parents(self, commit):
        if self._parents is None:
            self._parents = {}
            if self.head:
                for line in git(self.path, "rev-list", "--parents", "HEAD").splitlines():
                    ids = line.split()
                    self._parents[ids[0]] = ids[1:]
        return self._parents.get(commit, [])

    def origin_of(self, unit, commit):
        """commit の時点の版の値を書いたコミット。決まり (c) の「書いたコミット」。

        blame と同じく、同じ値を持つ親へ進む。値が親のどれとも違えばそのコミットが書いた。
        マージで両側とも分かれ目から値を変えていたら (衝突を解いた)、そのマージが書いたとみなす
        (片側だけが変えていたなら、変えた側へ進む)。
        """
        val = self.version_at(commit, unit.toml)
        c = commit
        while True:
            ps = self.parents(c)
            if not ps:
                return c
            pvals = [self.version_at(p, unit.toml) for p in ps]
            same = [p for p, v in zip(ps, pvals) if v == val]
            if not same:
                return c
            if len(same) < len(ps):
                base = git(self.path, "merge-base", *ps, check=False)
                bval = self.version_at(base.strip(), unit.toml) if base else None
                if all(v != bval for v in pvals):
                    return c
            c = same[0]

    def version_origin(self, unit):
        """いまの版の行を書いたコミット (作業ツリーが書いていれば None)。"""
        if self.head is None or self.version_at(None, unit.toml) != self.version_at(self.head, unit.toml):
            return None
        return self.origin_of(unit, self.head)

    def content_changed(self, unit, a, b=None):
        """a と b (None = 作業ツリー) の間で unit の中身 (版の行を除く) が変わったか。"""
        if b is None:
            names = git(self.path, "diff", "--name-only", "--no-renames", a, "--", unit.content).split()
            # まだ追跡していないファイルも作業ツリーの中身に数える (.gitignore のものは除く)
            names += git(self.path, "ls-files", "--others", "--exclude-standard", "--", unit.content).split()
        else:
            names = git(self.path, "diff", "--name-only", "--no-renames", a, b, "--", unit.content).split()
        for name in names:
            if name == unit.toml:
                old, new = self.show(a, name), self.show(b, name)
                if old is not None and new is not None and \
                        without_version_line(old) == without_version_line(new):
                    continue  # 版の行だけの差分は「変わった」に数えない
            return True
        return False

    def last_content_commit(self, unit):
        """中身を最後に変えたコミット (表示用。マージは見ない)。作業ツリーで変わっていれば None、見つからなければ ""。"""
        if self.head is None or self.content_changed(unit, self.head):
            return None
        out = git(self.path, "log", "--format=%H", "--no-merges", "--", unit.content)
        for h in out.split():
            ps = self.parents(h)
            if not ps or self.content_changed(unit, ps[0], h):
                return h
        return ""

    def tags_at_head(self):
        if self.head is None:
            return []
        out = git(self.path, "tag", "--points-at", "HEAD", "--list", "v*") or ""
        return [t for t in out.split() if TAG_RE.match(t)]


def deploy_commit(repo):
    """前回のデプロイ = いまの版を書いたコミット。

    作業ツリーが版を書いた (= デプロイの直前でまだコミットしていない) ときは、
    HEAD の版を書いたコミット (= 1 つ前のデプロイ) を返す。
    """
    if repo.head is None:
        return ""
    origin = repo.version_origin(repo.root)
    if origin is None:
        origin = repo.origin_of(repo.root, repo.head)
    return origin or ""


def changed_since_deploy(repo):
    """(前回のデプロイのコミット, そこから中身が変わった Unit の名前) を返す。"""
    base = deploy_commit(repo)
    if not base:
        return base, []
    return base, [u.name for u in repo.units() if repo.content_changed(u, base)]


def change_line(repo):
    """決まり (c) の 1 行。中身が変わったクレートを数えるだけで、NG にはしない。"""
    base, changed = changed_since_deploy(repo)
    crates = [n for n in changed if n != repo.root.name]
    text = "前回のデプロイ (%s) から中身が変わったクレート: %d 個" % (short(base), len(crates))
    if repo.root.name in changed:
        text += " (本体 %s/ も変わった)" % ROOT_CONTENT
    if crates or repo.root.name in changed:
        text += "。次のデプロイで --deploy %s する" % NEXT_VERSION
    return text


def judge(repo):
    """({名前: 版の行を書いたコミット (None = 作業ツリー)}, 合わない理由の並び [(名前, 理由)])。"""
    bad = []
    origins = {}
    # (a) 形と、全クレート = 本体
    for u in repo.units():
        if ver_key(u.version) is None:
            bad.append((u.name, "版 %s が X.Y.Z の形でない (決まり a)" % u.version))
        origins[u.name] = repo.version_origin(u)
    if ver_key(repo.root.version):
        for u in repo.crates:
            if u.version != repo.root.version:
                bad.append((u.name, "版 %s が本体 %s %s と違う (決まり a。デプロイの直前に --deploy でそろえる)"
                            % (u.version, repo.root.name, repo.root.version)))
    # (b) タグの上
    tags = repo.tags_at_head()
    if tags and "v%s" % repo.root.version not in tags:
        bad.append((repo.root.name, "タグ %s の上なのに本体の版が %s (決まり b)"
                    % (" ".join(tags), repo.root.version)))
    return origins, bad


# --- 書き換え ------------------------------------------------------------------

def set_toml_version(path, version):
    with open(path, encoding="utf-8") as f:
        text = f.read()
    _, old, line_no = package_fields(text)
    if line_no is None:
        raise Error("%s に [package] の version の行が無い" % path)
    lines = text.splitlines(True)
    lines[line_no] = VERSION_LINE_RE.sub('version = "%s"' % version, lines[line_no].rstrip("\n")) + "\n"
    with open(path, "w", encoding="utf-8") as f:
        f.write("".join(lines))
    return old


def set_lock_versions(path, versions):
    """`Cargo.lock` の `[[package]]` の `name = "X"` の次の `version = …` の行だけを書き換える。

    版の行しか変えないので、外の依存が無いこのリポジトリでは `cargo metadata` が書き直す結果と同じになる。
    """
    with open(path, encoding="utf-8") as f:
        lines = f.read().splitlines(True)
    done = set()
    for i, line in enumerate(lines[:-1]):
        m = re.match(r'^name = "([^"]+)"$', line.rstrip("\n"))
        if m and m.group(1) in versions and lines[i - 1].strip() == "[[package]]" and \
                lines[i + 1].startswith("version = "):
            lines[i + 1] = 'version = "%s"\n' % versions[m.group(1)]
            done.add(m.group(1))
    missing = sorted(set(versions) - done)
    if missing:
        raise Error("Cargo.lock に無いパッケージ: %s" % " ".join(missing))
    with open(path, "w", encoding="utf-8") as f:
        f.write("".join(lines))


def apply_versions(repo, versions):
    """{名前: 版} を Cargo.toml と Cargo.lock に書く。"""
    paths = {u.name: os.path.join(repo.path, u.toml) for u in repo.units()}
    for name, version in versions.items():
        set_toml_version(paths[name], version)
    lock = os.path.join(repo.path, "Cargo.lock")
    if os.path.exists(lock):
        set_lock_versions(lock, versions)


def do_deploy(repo, version):
    """--deploy: 全クレート + 本体 + proxy-server を同じ版にそろえる。書き換えた {名前: 版} を返す。

    版は下げない (デプロイの番号は進むだけ)。同じ版をもう一度渡しても書き換えない (何度でも回せる)。
    """
    key = ver_key(version)
    if key is None:
        raise Error("デプロイの版は X.Y.Z の形で渡す (例: 0.26.0): %s" % version)
    current = {u.name: u.version for u in repo.units()}
    now = ver_key(repo.root.version)
    if now and key < now:
        raise Error("版は下げない (いまの本体 %s より前のデプロイは指定できない): %s"
                    % (repo.root.version, version))
    versions = {name: version for name, v in current.items() if v != version}
    if versions:
        apply_versions(repo, versions)
    return versions


# --- 出力 ----------------------------------------------------------------------

def short(c):
    return "作業ツリー" if c is None else (c[:7] if c else "-")


def print_list(repo, origins, bad):
    print("クレートの版 (本体 %s %s。決まりは scripts/crate-versions.py の頭)" % (repo.root.name, repo.root.version))
    w = max(len(u.name) for u in repo.units())
    fmt = "%-*s  %-8s  %-7s  %-7s  %-2s  %s"
    print(fmt % (w, "名前", "今の版", "中身", "版の行", "合", "中身を最後に変えたコミット"))
    reasons = dict(bad)
    for u in repo.units():
        c = repo.last_content_commit(u)
        subject = ""
        if c:
            subject = git(repo.path, "log", "-1", "--format=%ad %s", "--date=short", c).strip()
            if len(subject) > 70:
                subject = subject[:69] + "…"
        print(fmt % (w, u.name, u.version, short(c), short(origins.get(u.name, "")),
                     "NG" if u.name in reasons else "ok", subject))
    print(change_line(repo))
    print("合わない: %d" % len(bad))


def main(argv=None):
    ap = argparse.ArgumentParser(description="クレートの版の決まり (T26.3: 版 = デプロイの番号。全クレート = 本体)")
    ap.add_argument("-C", dest="repo", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."),
                    help="リポジトリ (既定はこのスクリプトの 1 つ上)")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--list", action="store_true")
    g.add_argument("--check", action="store_true")
    g.add_argument("--deploy", metavar="VERSION", help="デプロイの版 (0.26.0 など)。全クレート + 本体をそろえる")
    # 廃止 (T26.3)。案内を出すためだけに受ける
    g.add_argument("--task", metavar="TID", help=argparse.SUPPRESS)
    a = ap.parse_args(argv)
    repo = None
    try:
        if a.task:
            raise Error("--task は廃止した (T26.3)。版はデプロイの番号なので、"
                        "デプロイの直前に 1 回だけ --deploy 0.26.0 のようにそろえる")
        repo = Repo(a.repo)
        if a.list:
            origins, bad = judge(repo)
            print_list(repo, origins, bad)
            return 0
        if a.check:
            _, bad = judge(repo)
            for name, why in bad:
                print("NG %s: %s" % (name, why))
            print(change_line(repo))
            if bad:
                print("決まりに合わない: %d" % len(bad))
                return 1
            print("決まりに合う: %d クレート + 本体 (本体 %s)" % (len(repo.crates), repo.root.version))
            return 0
        versions = do_deploy(repo, a.deploy)
        for u in repo.units():
            if u.name in versions:
                print("%s %s -> %s" % (u.name, u.version, versions[u.name]))
        print("そろえた: %d (全クレート + 本体 + %s を %s に)" % (len(versions), SERVER, a.deploy))
        return 0
    except Error as e:
        print("crate-versions: %s" % e, file=sys.stderr)
        return 2
    finally:
        if repo is not None:
            repo.close()


if __name__ == "__main__":
    sys.exit(main())
