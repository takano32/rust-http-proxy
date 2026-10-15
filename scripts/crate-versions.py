#!/usr/bin/env python3
"""クレートの版の決まりを数える・確かめる・上げる道具 (T21.1)。

決まり (2026-10-15、利用者の決定。README の「クレート構成」):

  (1) 各クレートの `version` は、**そのクレートの中身が最後に変わったリリース (`v*` のタグ) の版**。
  (2) リリースの直前に、前のタグから中身が変わったクレートだけを本体と同じ版に上げる (変わらないものは据え置く)。
  (3) 本体 (`rust-http-proxy`) と `proxy-server` は常に同じ版 (`crates/server/build.rs` が
      `CARGO_PKG_VERSION` から `-V` の版を作るため)。
  (4) 本体の版はタグの名前と同じ (`v0.20.0` ↔ `0.20.0`)。

「中身が変わった」= メンバーのディレクトリ (`crates/<名前>/`) の下の差分のうち、`Cargo.toml` の
`[package]` の `version = …` の 1 行を除いたもの。最初のタグより前の歴史は見ない (最初のタグにあった
クレートは、それより後に変わっていなければ最初のタグの版)。

使い方 (リポジトリの根から。`-C DIR` で別のリポジトリ):
  scripts/crate-versions.py --list  [--since TAG]   クレートごとに 今の版・最後に変わったタグ・決まりから出る版
  scripts/crate-versions.py --check [--since TAG]   決まりに合わないクレートの名前と理由を出して 1 で抜ける
  scripts/crate-versions.py --bump VERSION [--since TAG]
                                                    前のタグから変わったクレートと本体・proxy-server の
                                                    `version` を VERSION にし、`Cargo.lock` の該当の行も直す

`--since` の既定は HEAD から辿れるいちばん新しい `v*` のタグ (`git describe --tags --match 'v*'`)。
タグの上 (HEAD がそのタグ) で回すと、本体の版がタグと同じかも見る (`release.yml` はここで回す)。
「前のタグから変わった」は**作業ツリー**と比べる (コミット前の `--bump` の結果も確かめられるように)。
前のタグから変わったのにまだ上げていないクレートは、リリースの前の普通の状態なので `--check` を落とさない
(「次の --bump で上がる」と出す)。上げたあとは本体と同じ版であること。

外部のパッケージは使わない (標準ライブラリだけ)。`release.yml` の ubuntu-22.04 の python3 (3.10) で動くこと
(`tomllib` は 3.11 からなので使わず、`Cargo.toml` は行で読む)。
終了コード: 0 = 合う / 1 = 合わない (`--check`) / 2 = 使い方の誤り・タグが無い等。
"""

import argparse
import os
import re
import subprocess
import sys

TAG_RE = re.compile(r"^v(\d+)\.(\d+)\.(\d+)$")
VER_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
# `[package]` の版の行 (`version = "0.20.0"`)。行の頭から書いてあるものだけを見る
VERSION_LINE_RE = re.compile(r'^version\s*=\s*"([^"]*)"\s*$')
SERVER = "proxy-server"


class Error(Exception):
    """使い方の誤り・リポジトリの形が想定と違う (終了コード 2)。"""


def git(repo, *args, check=True):
    out = subprocess.run(["git", "-C", repo] + list(args), stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, universal_newlines=True)
    if check and out.returncode != 0:
        raise Error("git %s: %s" % (" ".join(args), out.stderr.strip()))
    return out.stdout if out.returncode == 0 else None


def ver_key(text):
    m = VER_RE.match(text)
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

class Repo:
    def __init__(self, path):
        top = git(path, "rev-parse", "--show-toplevel")
        self.path = top.strip()
        with open(os.path.join(self.path, "Cargo.toml"), encoding="utf-8") as f:
            root_text = f.read()
        self.root_name, self.root_version, _ = package_fields(root_text)
        if not self.root_name:
            raise Error("ルートの Cargo.toml に [package] の name が無い")
        self.crates = []  # (メンバーのパス, 名前, 今の版)
        for path in members(root_text):
            with open(os.path.join(self.path, path, "Cargo.toml"), encoding="utf-8") as f:
                name, version, _ = package_fields(f.read())
            self.crates.append((path.rstrip("/"), name, version))
        self.tags = self._tags()

    def _tags(self):
        """`vX.Y.Z` のタグを版の順に (古い順)。"""
        found = []
        for t in (git(self.path, "tag", "--list", "v*") or "").split():
            m = TAG_RE.match(t)
            if m:
                found.append((tuple(int(x) for x in m.groups()), t))
        return [t for _, t in sorted(found)]

    def default_since(self):
        out = git(self.path, "describe", "--tags", "--abbrev=0", "--match", "v*", "HEAD", check=False)
        if not out or not TAG_RE.match(out.strip()):
            raise Error("HEAD から辿れる v* のタグが無い (--since TAG で渡す)")
        return out.strip()

    def rev_of(self, rev):
        out = git(self.path, "rev-parse", "--verify", "-q", rev + "^{commit}", check=False)
        return out.strip() if out else None

    def show(self, rev, path):
        """rev のファイルの中身 (無ければ None)。rev が None なら作業ツリー。"""
        if rev is None:
            full = os.path.join(self.path, path)
            if not os.path.isfile(full):
                return None
            with open(full, encoding="utf-8", errors="replace") as f:
                return f.read()
        return git(self.path, "show", "%s:%s" % (rev, path), check=False)

    def exists_at(self, rev, member):
        out = git(self.path, "ls-tree", "-d", "--name-only", rev, member, check=False)
        return bool(out and out.strip())

    def owner(self, path):
        """ファイルのパスが属するメンバー (いちばん長く一致するもの)。"""
        best = None
        for member, _, _ in self.crates:
            if path.startswith(member + "/") and (best is None or len(member) > len(best)):
                best = member
        return best

    def changed(self, a, b):
        """a と b (None = 作業ツリー) の間で中身が変わったメンバーの集合。"""
        dirs = [m for m, _, _ in self.crates]
        if b is None:
            names = git(self.path, "diff", "--name-only", "--no-renames", a, "--", *dirs).split()
            # まだ追跡していないファイルも作業ツリーの中身に数える (.gitignore のものは除く)
            names += git(self.path, "ls-files", "--others", "--exclude-standard", "--", *dirs).split()
        else:
            names = git(self.path, "diff", "--name-only", "--no-renames", a, b, "--", *dirs).split()
        out = set()
        for name in names:
            member = self.owner(name)
            if member is None or member in out:
                continue
            if name == member + "/Cargo.toml":
                old, new = self.show(a, name), self.show(b, name)
                if old is not None and new is not None and \
                        without_version_line(old) == without_version_line(new):
                    continue  # 版の行だけの差分は「変わった」に数えない
            out.add(member)
        return out


def analyse(repo, since):
    """クレートごとの (パス, 名前, 今の版, 最後に変わったタグ, 決まりから出る版, 前のタグから変わったか)。"""
    if since not in repo.tags:
        raise Error("タグ %s が無い (v*.*.* の形のタグを渡す)" % since)
    tags = repo.tags[:repo.tags.index(since) + 1]
    last = {}
    for i, tag in enumerate(tags):
        if i == 0:
            for member, _, _ in repo.crates:
                if repo.exists_at(tag, member):
                    last[member] = tag
            continue
        for member in repo.changed(tags[i - 1], tag):
            last[member] = tag
    pending = repo.changed(since, None)
    rows = []
    for member, name, version in repo.crates:
        tag = last.get(member)
        rows.append((member, name, version, tag, tag[1:] if tag else None, member in pending))
    return rows


def judge(repo, since):
    """(rows, 合わない理由の並び [(名前, 理由)])。"""
    rows = analyse(repo, since)
    bad = []
    root_v = repo.root_version
    since_v = since[1:]
    bumped = root_v != since_v  # 本体だけ先に上げてある = --bump のあと、タグを打つ前
    server = [r for r in rows if r[1] == SERVER]
    if server and server[0][2] != root_v:
        bad.append((SERVER, "本体 %s %s と版が違う: %s (決まり 3)" % (repo.root_name, root_v, server[0][2])))
    head = repo.rev_of("HEAD")
    if head and head == repo.rev_of(since) and root_v != since_v:
        bad.append((repo.root_name, "タグ %s の上なのに本体の版が %s (決まり 4)" % (since, root_v)))
    for member, name, version, tag, want, pending in rows:
        if name == SERVER:
            continue
        if pending:
            # 前のタグから変わった: 上げる前 (決まりから出る版のまま) か、上げたあと (本体と同じ版) なら合う。
            # まだどのタグにも無いクレートは、上げる前は何の版でもよい
            if version == root_v and bumped:
                continue
            if want is None or version == want:
                continue
            bad.append((name, "%s から中身が変わったのに版が %s (上げる前は %s、--bump のあとは本体と同じ %s)"
                        % (since, version, want, root_v)))
            continue
        if want is None:
            bad.append((name, "%s までのどのタグにも無い" % since))
        elif version != want:
            bad.append((name, "版が %s (中身が最後に変わったのは %s なので %s)" % (version, tag, want)))
    return rows, bad


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
    """`Cargo.lock` の `[[package]]` の `name = "X"` の次の `version = …` の行だけを書き換える。"""
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
    """{名前: 版} を Cargo.toml と Cargo.lock に書く。本体はルートの Cargo.toml。"""
    paths = {name: os.path.join(repo.path, member, "Cargo.toml") for member, name, _ in repo.crates}
    paths[repo.root_name] = os.path.join(repo.path, "Cargo.toml")
    for name, version in versions.items():
        set_toml_version(paths[name], version)
    lock = os.path.join(repo.path, "Cargo.lock")
    if os.path.exists(lock):
        set_lock_versions(lock, versions)


# --- 出力 ----------------------------------------------------------------------

def print_list(repo, since, rows, bad):
    print("クレートの版 (前のタグ %s、本体 %s %s。決まりは scripts/crate-versions.py の頭)" %
          (since, repo.root_name, repo.root_version))
    w = max(len(r[1]) for r in rows + [("", repo.root_name)])
    print("%-*s  %-9s  %-12s  %-9s  %s" % (w, "クレート", "今の版", "最後に変わった", "決まりの版", "メモ"))
    reasons = dict(bad)
    print("%-*s  %-9s  %-12s  %-9s  %s" % (w, repo.root_name, repo.root_version, "-", "-",
                                         "本体 (決まり 3・4)" + ("  NG" if repo.root_name in reasons else "")))
    for member, name, version, tag, want, pending in rows:
        note = []
        if name == SERVER:
            note.append("本体と同じ版 (決まり 3)")
            want = repo.root_version
        elif pending:
            note.append("%s から変わった (次の --bump で上がる)" % since)
        if name in reasons:
            note.append("NG")
        print("%-*s  %-9s  %-12s  %-9s  %s" % (w, name, version, tag or "-", want or "-", " ".join(note)))
    print("合わない: %d" % len(bad))


def main(argv=None):
    ap = argparse.ArgumentParser(description="クレートの版の決まり (T21.1)")
    ap.add_argument("-C", dest="repo", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."),
                    help="リポジトリ (既定はこのスクリプトの 1 つ上)")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--list", action="store_true")
    g.add_argument("--check", action="store_true")
    g.add_argument("--bump", metavar="VERSION")
    ap.add_argument("--since", metavar="TAG")
    a = ap.parse_args(argv)
    try:
        repo = Repo(a.repo)
        since = a.since or repo.default_since()
        if a.list:
            rows, bad = judge(repo, since)
            print_list(repo, since, rows, bad)
            return 0
        if a.check:
            rows, bad = judge(repo, since)
            for name, why in bad:
                print("NG %s: %s" % (name, why))
            if bad:
                print("決まりに合わないクレート: %d (前のタグ %s)" % (len(bad), since))
                return 1
            print("決まりに合う: %d クレート + 本体 (前のタグ %s、本体 %s)" % (len(rows), since, repo.root_version))
            return 0
        new = a.bump
        if ver_key(new) is None:
            raise Error("版は X.Y.Z の形で渡す: %s" % new)
        if ver_key(new) <= ver_key(since[1:]):
            raise Error("版 %s が前のタグ %s より新しくない" % (new, since))
        rows = analyse(repo, since)
        versions = {repo.root_name: new}
        for member, name, version, tag, want, pending in rows:
            if pending or name == SERVER:
                versions[name] = new
        apply_versions(repo, versions)
        for name in [repo.root_name] + [r[1] for r in rows if r[1] in versions]:
            print("%s -> %s" % (name, new))
        print("上げた: %d (本体と %s を含む。%s から中身が変わったクレート %d)" %
              (len(versions), SERVER, since, len(versions) - 2 if SERVER in versions else len(versions) - 1))
        return 0
    except Error as e:
        print("crate-versions: %s" % e, file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
