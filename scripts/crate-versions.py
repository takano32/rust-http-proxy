#!/usr/bin/env python3
"""クレートの版の決まりを数える・確かめる・上げる道具 (T.21.1 で作り、T.21.6 で決まりを置き換えた)。

決まり (2026-10-15、利用者の提案を親が細部まで決めた。README の「クレート構成」):

  (1) 各クレートの `version` は、**そのクレートの中身を最後に変えたタスクの番号** `0.<Phase>.<番号>`
      (T.21.3 → `0.21.3`、T.17.11 → `0.17.11`。英字の付くタスク (T17.0a) は英字を落とす)。
  (2) タスクの中で上げる: 取り込む前に `--task T.21.3` を回すと、基準 (`--since REF`。既定は `main` との
      分かれ目 = `git merge-base HEAD main`) から中身が変わったクレートの版を `0.21.3` にする。
  (3) 本体 (`rust-http-proxy`) と `proxy-server` は、そのビルドに入っている最後のタスク = 全クレートの版と、
      本体 (`src/`) を最後に変えたタスクの番号のうち最大。`proxy-server` は本体と同じ
      (`crates/server/build.rs` が `CARGO_PKG_VERSION` から `-V` の版を作るため)。
  (4) タグは本体の版 (`v0.21.4` など)。

「中身が変わった」= メンバーのディレクトリ (`crates/<名前>/`) の下の差分のうち、`Cargo.toml` の
`[package]` の `version = …` の 1 行を除いたもの。本体の中身は `src/` の下と、**ルートの `Cargo.toml`**
(`[package]` の版の行と `[workspace]` の `members = [...]` を除く。`[profile.*]` の `codegen-units` を
変えてもビルドは変わるのに、前は数えずに「決まりに合う」と出た。T.29.28)。

`--check` の決まり (**コミットの文面には頼らない**。文面にタスク番号が無いコミットも、複数の番号が並ぶものもあるため):
  (a) 版の形が `0.P.T`。
  (b) `proxy-server` = 本体 = 全クレートの版の最大以上。
  (c) 上げ忘れ: 各クレート (と本体) について、いまの `version` の行を書いたコミットより後に中身が変わっていたら落とす。
      「いまの行を書いたコミット」は blame と同じく、同じ値を持つ親へ辿って探す。ただしマージで両側とも
      分かれ目から版の行を変えていた (= 衝突を解いた) ときは、そのマージが書いたとみなす。
      作業ツリーの版の行が HEAD と違えば作業ツリーが書いた (コミット前の `--task` を確かめられるように)。
      「後に中身が変わった」= そのコミットの中身と作業ツリーの中身 (版の行を除く) が違う。
      本体と proxy-server は全部の最大なので、番号の小さいタスクが後から入っても版が変わらない。この 2 つは
      どれかのクレートの版の行を書いたコミット (= `--task` を回した) より後に中身が変わっていなければよい (T.28.0)。
      同じタスクの中で 2 回目の `--task` を回したとき、もうその番号のクレートも同じで、同じ版の行を
      書いたコミットより後に中身が変わっていなければよい (T.28.27)。
  (d) タグの上 (HEAD を指す `v*` のタグがある) なら、本体の版 = タグ。
  (e) `Cargo.lock` の各メンバー (本体と全クレート) の版が、その `Cargo.toml` の版と同じ。`Cargo.toml` を
      手で上げて `Cargo.lock` をそのままにしても、CI は `--locked` なしでビルドするので気づかない (T.29.28)。
      `Cargo.lock` が無いリポジトリでは見ない。

使い方 (リポジトリの根から。`-C DIR` で別のリポジトリ):
  scripts/crate-versions.py --list                 クレートごとに 今の版・中身を最後に変えたコミット・
                                                   その版の行を書いたコミット・合うか
  scripts/crate-versions.py --check                決まりに合わないものの名前と理由を出して 1 で抜ける
  scripts/crate-versions.py --task TID [--since REF]
                                                   REF から中身が変わったクレートの版を TID の番号にし、
                                                   本体と proxy-server を全部の最大にする (`Cargo.lock` の該当の行も直す)

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
# 決まり (a): `0.<Phase>.<番号>`
VER_RE = re.compile(r"^0\.(\d+)\.(\d+)$")
# タスクの番号 (`T.21.3`、`T17.0a`。英字は落とす)
TASK_RE = re.compile(r"^T\.(\d+)\.(\d+)[a-z]?$")
# `[package]` の版の行 (`version = "0.20.0"`)。行の頭から書いてあるものだけを見る
VERSION_LINE_RE = re.compile(r'^version\s*=\s*"([^"]*)"\s*$')
SERVER = "proxy-server"
ROOT_CONTENT = ("src", "Cargo.toml")  # 本体の「中身」(ルートの Cargo.toml は T.29.28 から)


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


def task_version(tid):
    """`T.21.3` → `0.21.3` (英字は落とす)。形が違えば None。"""
    m = TASK_RE.match(tid)
    return "0.%d.%d" % (int(m.group(1)), int(m.group(2))) if m else None


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


def without_members(text):
    """`[workspace]` の `members = [...]` (複数行も) を除いた中身 (本体の中身を比べるときに使う。T.29.28)。"""
    out = []
    section = None
    skipping = False
    for line in text.splitlines(True):
        s = line.strip()
        if skipping:
            if "]" in s.split("#", 1)[0]:
                skipping = False
            continue
        if s.startswith("["):
            section = s
        elif section == "[workspace]" and re.match(r"^members\s*=\s*\[", s):
            skipping = "]" not in s.split("#", 1)[0].split("[", 1)[1]
            continue
        out.append(line)
    return "".join(out)


def lock_versions(text):
    """`Cargo.lock` の `[[package]]` のうち `source` の無いもの (= このワークスペースのメンバー) の {名前: 版}。"""
    found = {}
    for block in re.split(r"^\[\[package\]\]\s*$", text, flags=re.M)[1:]:
        name = re.search(r'^name = "([^"]+)"', block, re.M)
        version = re.search(r'^version = "([^"]+)"', block, re.M)
        if name and version and not re.search(r"^source = ", block, re.M):
            found[name.group(1)] = version.group(1)
    return found


def members(root_toml):
    """ルートの `Cargo.toml` の `[workspace]` の `members` (並びのまま)。"""
    m = re.search(r"^\[workspace\][\s\S]*?^members\s*=\s*\[([\s\S]*?)\]", root_toml, re.M)
    if not m:
        raise Error("Cargo.toml に [workspace] の members が無い")
    body = "\n".join(l.split("#", 1)[0] for l in m.group(1).splitlines())
    return re.findall(r'"([^"]+)"', body)


# --- リポジトリを読む ---------------------------------------------------------

class Unit:
    """版を持つ 1 つ (本体かクレート)。toml = 版の行のあるファイル、content = 中身のパスの並び
    (クレートはディレクトリ 1 つ、本体は `src` とルートの `Cargo.toml`)。root = 本体か。"""

    def __init__(self, name, version, toml, content, root=False):
        self.name, self.version, self.toml = name, version, toml
        self.content = (content,) if isinstance(content, str) else tuple(content)
        self.root = root

    def label(self):
        """理由の文面に出す中身の名前 (`crates/alpha/`、`src/・Cargo.toml`)。"""
        return "・".join(c if c.endswith(".toml") else c + "/" for c in self.content)

    def normalize(self, text):
        """版を持つファイルの中身から「中身が変わった」に数えない行を除く。"""
        text = without_version_line(text)
        return without_members(text) if self.root else text


class Repo:
    def __init__(self, path):
        top = git(path, "rev-parse", "--show-toplevel")
        self.path = top.strip()
        with open(os.path.join(self.path, "Cargo.toml"), encoding="utf-8") as f:
            root_text = f.read()
        name, version, _ = package_fields(root_text)
        if not name:
            raise Error("ルートの Cargo.toml に [package] の name が無い")
        self.root = Unit(name, version, "Cargo.toml", ROOT_CONTENT, root=True)
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

    def version_origin(self, unit):
        """いまの版の行を書いたコミット (作業ツリーなら None)。決まり (c) の「書いたコミット」。

        blame と同じく、同じ値を持つ親へ進む。値が親のどれとも違えばそのコミットが書いた。
        マージで両側とも分かれ目から値を変えていたら (衝突を解いた)、そのマージが書いたとみなす
        (片側だけが変えていたなら、変えた側へ進む = 変えていない側の中身の変更は「後」になる)。
        """
        if self.head is None or self.version_at(None, unit.toml) != self.version_at(self.head, unit.toml):
            return None
        c = self.head
        while True:
            val = self.version_at(c, unit.toml)
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

    def content_changed(self, unit, a, b=None):
        """a と b (None = 作業ツリー) の間で unit の中身 (版の行を除く) が変わったか。"""
        if b is None:
            names = git(self.path, "diff", "--name-only", "--no-renames", a, "--", *unit.content).split()
            # まだ追跡していないファイルも作業ツリーの中身に数える (.gitignore のものは除く)
            names += git(self.path, "ls-files", "--others", "--exclude-standard", "--", *unit.content).split()
        else:
            names = git(self.path, "diff", "--name-only", "--no-renames", a, b, "--", *unit.content).split()
        for name in names:
            if name == unit.toml:
                old, new = self.show(a, name), self.show(b, name)
                if old is not None and new is not None and unit.normalize(old) == unit.normalize(new):
                    continue  # 版の行 (本体は members も) だけの差分は「変わった」に数えない
            return True
        return False

    def last_content_commit(self, unit):
        """中身を最後に変えたコミット (表示用。マージは見ない)。作業ツリーで変わっていれば None、見つからなければ ""。"""
        if self.head is None or self.content_changed(unit, self.head):
            return None
        out = git(self.path, "log", "--format=%H", "--no-merges", "--", *unit.content)
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


def judge(repo):
    """({名前: 版の行を書いたコミット (None = 作業ツリー)}, 合わない理由の並び [(名前, 理由)])。"""
    bad = []
    origins = {}
    server = [u for u in repo.crates if u.name == SERVER]
    # (a) 形
    for u in repo.units():
        if ver_key(u.version) is None:
            bad.append((u.name, "版 %s が 0.<Phase>.<番号> の形でない (決まり a)" % u.version))
    # (b) proxy-server = 本体 = 全クレートの最大以上
    if server and server[0].version != repo.root.version:
        bad.append((SERVER, "本体 %s %s と版が違う: %s (決まり b)"
                    % (repo.root.name, repo.root.version, server[0].version)))
    root_k = ver_key(repo.root.version)
    keyed = [(ver_key(u.version), u) for u in repo.crates if ver_key(u.version)]
    if root_k and keyed:
        top_k, top = max(keyed, key=lambda x: x[0])
        if top_k > root_k:
            bad.append((repo.root.name, "本体の版 %s が %s の %s より小さい (決まり b。--task で上げる)"
                        % (repo.root.version, top.name, top.version)))
    # (c) 上げ忘れ
    for u in repo.units():
        origins[u.name] = repo.version_origin(u)
    for u in repo.units():
        origin = origins[u.name]
        if origin is not None and repo.content_changed(u, origin):
            # 版の行を書き直せない場合が 2 つある。どちらも、後で `--task` を回したコミット (= ほかの
            # クレートの版の行を書いた) より後に中身が変わっていなければよい:
            # - 本体と proxy-server は全部の最大なので、番号の小さいタスクが後から入ると版が変わらない (T.28.0)
            # - 同じタスクの中で 2 回目の `--task` を回すと、もうその番号になっているクレートは版が
            #   変わらない (T.28.27)。このときは同じ版の行を書いたコミットだけを見る
            whole = u.name in (repo.root.name, SERVER)
            origin = next((origins[v.name] for v in repo.units()
                           if (whole or v.version == u.version)
                           and (origins[v.name] is None or not repo.content_changed(u, origins[v.name]))),
                          origin)
        if origin is not None and repo.content_changed(u, origin):
            bad.append((u.name, "版 %s の行を書いた %s より後に中身 (%s) が変わった (決まり c。--task で上げる)"
                        % (u.version, origin[:7], u.label())))
    # (d) タグの上
    tags = repo.tags_at_head()
    if tags and "v%s" % repo.root.version not in tags:
        bad.append((repo.root.name, "タグ %s の上なのに本体の版が %s (決まり d)" % (" ".join(tags), repo.root.version)))
    # (e) Cargo.lock の版が Cargo.toml と同じ (T.29.28)
    lock = repo.show(None, "Cargo.lock")
    if lock is not None:
        locked = lock_versions(lock)
        for u in repo.units():
            if u.name not in locked:
                bad.append((u.name, "Cargo.lock に無い (決まり e。cargo build か --task で作り直す)"))
            elif locked[u.name] != u.version:
                bad.append((u.name, "Cargo.toml の版 %s と Cargo.lock の版 %s が違う (決まり e。"
                                    "cargo build か --task で揃える)" % (u.version, locked[u.name])))
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
    """{名前: 版} を Cargo.toml と Cargo.lock に書く。"""
    paths = {u.name: os.path.join(repo.path, u.toml) for u in repo.units()}
    for name, version in versions.items():
        set_toml_version(paths[name], version)
    lock = os.path.join(repo.path, "Cargo.lock")
    if os.path.exists(lock):
        set_lock_versions(lock, versions)


def bump_task(repo, tid, since):
    """--task: since から中身が変わったクレートを tid の版にし、本体と proxy-server を全部の最大にする。

    (基準, 版, 書き換えた {名前: 版}) を返す。本体の版は下げない (いまの本体の版も最大の候補に入れる)。
    """
    new = task_version(tid)
    if new is None:
        raise Error("タスクの番号は T<Phase>.<番号> の形で渡す (例: T.21.3、T17.0a): %s" % tid)
    if since is None:
        base = git(repo.path, "merge-base", "HEAD", "main", check=False)
        if not base:
            raise Error("main との分かれ目が無い (--since REF で基準を渡す)")
        since = base.strip()
    elif repo.rev_of(since) is None:
        raise Error("基準 %s が無い" % since)
    current = {u.name: u.version for u in repo.units()}
    versions = {u.name: new for u in repo.crates if repo.content_changed(u, since)}
    keys = [ver_key(versions.get(name, v)) for name, v in current.items()]
    if repo.content_changed(repo.root, since):
        keys.append(ver_key(new))
    keys = [k for k in keys if k]
    top = "0.%d.%d" % max(keys)
    versions[repo.root.name] = top
    if SERVER in current:
        versions[SERVER] = top
    versions = {k: v for k, v in versions.items() if v != current[k]}
    if versions:
        apply_versions(repo, versions)
    return since, new, versions


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
    print("合わない: %d" % len(bad))


def main(argv=None):
    ap = argparse.ArgumentParser(description="クレートの版の決まり (T.21.6: 版 = 中身を最後に変えたタスクの番号)")
    ap.add_argument("-C", dest="repo", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."),
                    help="リポジトリ (既定はこのスクリプトの 1 つ上)")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--list", action="store_true")
    g.add_argument("--check", action="store_true")
    g.add_argument("--task", metavar="TID", help="タスクの番号 (T.21.3 など)")
    ap.add_argument("--since", metavar="REF", help="--task の基準 (既定は git merge-base HEAD main)")
    a = ap.parse_args(argv)
    repo = None
    try:
        if a.since and not a.task:
            raise Error("--since は --task と一緒に使う")
        repo = Repo(a.repo)
        if a.list:
            origins, bad = judge(repo)
            print_list(repo, origins, bad)
            return 0
        if a.check:
            _, bad = judge(repo)
            for name, why in bad:
                print("NG %s: %s" % (name, why))
            if bad:
                print("決まりに合わない: %d" % len(bad))
                return 1
            print("決まりに合う: %d クレート + 本体 (本体 %s)" % (len(repo.crates), repo.root.version))
            return 0
        since, new, versions = bump_task(repo, a.task, a.since)
        for u in repo.units():
            if u.name in versions:
                print("%s %s -> %s" % (u.name, u.version, versions[u.name]))
        print("上げた: %d (%s から中身が変わったものを %s に。本体と %s は全部の最大)"
              % (len(versions), since[:12], new, SERVER))
        return 0
    except Error as e:
        print("crate-versions: %s" % e, file=sys.stderr)
        return 2
    finally:
        if repo is not None:
            repo.close()


if __name__ == "__main__":
    sys.exit(main())
