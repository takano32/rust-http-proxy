#!/usr/bin/env python3
"""デプロイ先の実データ (`/snapshot`) を匿名化してテストへ持ち込む (TASKS.md T14.35)。

`/snapshot` には**個人の閲覧先** (ホスト名)・接続元 IP・`User-Agent` が並ぶのでリポジトリに
入れられず、道具のテストは毎回架空の fixture を作っている (T13.3 / T14.4 / T14.7)。
**名前と IP と UA だけを決定的に置き換え、数字は 1 つも変えない**ことで、本物の分布
(件数・時間・区間・閉じた理由) を持ったまま fixture にできる。

使い方:
    scripts/anonymize-snapshot.py ~/rust-http-proxy-status/2026-09-16T0106Z-snapshot.json \
                                  -o scripts/testdata/deployed-2026-09-16.anon.json
    # `/snapshot` より前の形 (`collect-deployed.sh` 以前に 1 本ずつ取ったファイル群) からも組める。
    # `-metrics` `-dashboard` のような JSON でないものは黙って飛ばす
    scripts/anonymize-snapshot.py ~/rust-http-proxy-status/2026-09-16T0106Z-* \
                                  -o scripts/testdata/deployed-2026-09-16.anon.json
    scripts/anonymize-snapshot.py a-snapshot.json -o -        # 標準出力へ

置き換えるもの (**決定的**: 同じ入力からは同じ出力になるので、匿名化した 2 枚で差分が取れる):

| 元 | 後 | 備考 |
|---|---|---|
| ホスト名 (`host` / `target` / `canary_host` / `sni`) | `host-0001.g0007.example` | 出現順に採番。`connect://host:443` の scheme と port はそのまま |
| 接続元 IP (`client`) | `198.51.100.1` / `2001:db8::1` | `ip:port` なら port はそのまま |
| 名前解決の答え (`/dns` の `addrs`) | `203.0.113.1` / `2001:db8:1::1` | 宛先が IP リテラルのときも同じ表を使う |
| `User-Agent` (`agents`) | `ua-01` | |
| `/log` の行・`/events` の説明の中のホストと IP | 上と同じ表 | 行の他の語はそのまま |

置き換えないもの: **数字** (件数・ms・区間・閉じた理由・時刻)、`version`、部の名前、
この機械の設定 (`path` のような個人の閲覧先ではないもの)。

**まとめの粒度を残す** (T14.54): ホスト名の `gNNNN` は**まとめの単位 (eTLD+1)** の番号で、
`img.dlsite.jp` と `www.dlsite.jp` は同じ `gNNNN` (= 匿名化後も同じ eTLD+1) に落ちる。
`www.dlsite.com` は別の単位なので別の番号になる。これで匿名化したあとの fixture でも
`scripts/status-diff.py --group domain` / `snapshot-diff.py --group domain` の粒度が読める。

すでに匿名化済みの値 (`host-0001.g0007.example`、**古い形の `host-0001.example` も**、
文書用の IP) はそのまま通す (2 回かけても変わらない)。

依存は Python 3 の標準ライブラリだけ (このリポジトリの方針どおり外部パッケージを使わない)。
"""

import argparse
import ipaddress
import json
import os
import re
import sys
from datetime import datetime, timezone

# まとめの単位 (eTLD+1 の近似) は `scripts/proxydata.py` と同じ 1 関数を使う (T14.54)
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proxydata import etld1  # noqa: E402

# ---------------------------------------------------------------- 置き換えの対象

# 値が `[scheme://]名前[:port]` の欄 (`/hosts` `/status` `/dns` `/errors` `/connections`
# `/recent` `/bursts` `/clients` の宛先、T14.10 の canary、T14.22 の `hosts_series`)
HOST_KEYS = frozenset(("host", "target", "canary_host", "sni"))
# 値が接続元 IP (`ip` か `ip:port`) の欄
CLIENT_KEYS = frozenset(("client",))
# 値が名前解決の答えの一覧 (`/dns` の `addrs`)
ADDR_KEYS = frozenset(("addrs",))
# 値が `User-Agent` の一覧 (T14.7 の接続元の個票)
AGENT_KEYS = frozenset(("agents",))
# 値が文 (中にホストや IP が混ざる。行の他の語は変えない)
TEXT_KEYS = frozenset(("msg", "text", "error", "url", "file"))
# ホスト名でも接続元でもない鍵 (`crates/metrics/src/metrics.rs` の `MAX_HOSTS` / `MAX_CLIENTS` を
# 越えた分をまとめる行の名前)。**置き換えると「その他」の行の意味が消える**ので、そのまま通す
RESERVED_NAMES = frozenset(("other",))

# 置き換え先の形。`gNNNN` は**まとめの単位 (eTLD+1) の番号**で、同じ単位のホストは
# 同じ番号に落ちる (T14.54)。`host-0001.g0007.example` の eTLD+1 は `g0007.example` なので、
# 匿名化した fixture でも `--group domain` が元と同じ粒度でまとまる。
# **古い形 (`host-0001.example`、T14.35 の最初の版) も匿名化済みとして通す** ので、
# すでにある fixture を通しても名前は変わらない。
HOST_FMT = "host-{:04d}.g{:04d}.example"
UA_FMT = "ua-{:02d}"
ANON_HOST_RE = re.compile(r"^host-\d{4,}(?:\.g(\d{4,}))?\.example$")
ANON_HOST_IN_TEXT = re.compile(r"host-\d{4,}(?:\.g\d{4,})?\.example")
ANON_UA_RE = re.compile(r"^ua-\d{2,}$")
# RFC 5737 / RFC 3849 の文書用。足りなくなったら RFC 2544 の試験用 (198.18.0.0/15) へ続ける
CLIENT_V4 = ("198.51.100",) + tuple("198.18.{}".format(i) for i in range(256))
ADDR_V4 = ("203.0.113", "192.0.2") + tuple("198.19.{}".format(i) for i in range(256))
# 置き換え済みの IP かどうかの判定 (2 回かけても変わらないように)
DOC_NETS = tuple(
    ipaddress.ip_network(n)
    for n in ("198.51.100.0/24", "203.0.113.0/24", "192.0.2.0/24", "198.18.0.0/15",
              "2001:db8::/32")
)

# 文の中の見分け (数字を 1 つも変えないように、IP は `ipaddress` で確かめてから置き換える)
TEXT_V6_BRACKET = re.compile(r"\[([0-9A-Fa-f:.]{2,45})\]")
TEXT_V6 = re.compile(r"(?<![\w:.\-])([0-9A-Fa-f]{0,4}(?::[0-9A-Fa-f]{0,4}){2,7})(?![\w:.\-])")
TEXT_V4 = re.compile(r"(?<![\w.\-])(\d{1,3}(?:\.\d{1,3}){3})(?![\d.])")
TEXT_HOST = re.compile(r"(?<![\w.\-])([0-9A-Za-z_\-]+(?:\.[0-9A-Za-z_\-]+)+)(?::(\d{1,5}))?(?![\w.\-])")
TLD_RE = re.compile(r"^[A-Za-z][A-Za-z0-9\-]{1,23}$")
# 文の中の `なにか.なにか` のうち、ホスト名ではないもの (拡張子)。**表に無い**ものだけに効く
# (表にある名前 = どこかの欄でホストとして現れた名前は、拡張子に見えても置き換える)
FILE_SUFFIXES = frozenset("""
json html htm js css md rs py sh txt toml yaml yml lock env log rrd gz zst xz tar zip conf cfg
ini so bin tmp bak old sock pid csv svg png ico wasm map d service state db crt pem key deb rpm
""".split())


class Anonymizer:
    """名前 / IP / UA の表を持ち、**出現順**に採番する。"""

    def __init__(self):
        self.hosts = {}     # 小文字のホスト名 -> host-0001.g0007.example
        self.groups = {}    # まとめの単位 (eTLD+1) -> 7
        self.used_groups = set()  # すでに出した単位の番号 (入力に混ざっていた分も含む)
        self.ips = {}       # 正規化した IP -> 198.51.100.1 / 203.0.113.1
        self.agents = {}    # User-Agent -> ua-01
        self.used = set()   # すでに出した置き換え先 (入力に混ざっていた分も含む)
        self.n_client_ips = 0
        self.n_addr_ips = 0
        self.text_hits = 0  # 文の中で置き換えた回数
        self._next = {}     # (場所, 族) -> 次の番号 (IPv4 と IPv6 は別に数える)
        self._dotless = None

    # ------------------------------------------------------------ 1 つぶん

    def host(self, name):
        """ホスト名 1 つ (scheme も port も付かない裸の名前)。"""
        if not name:
            return name
        if name.lower() in RESERVED_NAMES:
            return name  # 表の上限を越えた分の行 ("other")
        anon = ANON_HOST_RE.match(name)
        if anon:
            self.used.add(name)
            # すでに匿名化済みの名前が持っている単位の番号を押さえる (同じ番号を
            # 別の単位に出さないため)。`host-0001.g0007.example` の単位は `g0007.example`
            if anon.group(1):
                self.groups.setdefault(etld1(name), int(anon.group(1)))
                self.used_groups.add(int(anon.group(1)))
            return name
        try:  # IP リテラルの宛先は IP のまま置き換える (名前に化けさせない)
            ipaddress.ip_address(name)
        except ValueError:
            pass
        else:
            return self.ip(name, "addr")
        low = name.lower()
        got = self.hosts.get(low)
        if got is None:
            got = self._take_host(self.group(low), len(self.hosts) + 1)
            self.hosts[low] = got
            self._dotless = None
        return got

    def group(self, name):
        """**まとめの単位 (eTLD+1) の番号** (T14.54)。同じ単位なら同じ番号。"""
        unit = etld1(name)
        got = self.groups.get(unit)
        if got is None:
            got = len(self.groups) + 1
            while got in self.used_groups:  # 入力に混ざっていた番号は避ける
                got += 1
            self.groups[unit] = got
            self.used_groups.add(got)
        return got

    def ip(self, text, which):
        """IP 1 つ。`which` は最初に見た場所 (`client` / `addr`) で、置き換え先の範囲を決める。"""
        try:
            addr = ipaddress.ip_address(text)
        except ValueError:
            return text  # IP でなければ触らない
        if any(addr in net for net in DOC_NETS if net.version == addr.version):
            self.used.add(text)
            return text  # すでに文書用の範囲 (2 回かけても変わらない)
        key = str(addr)
        got = self.ips.get(key)
        if got is None:
            n = self._next.get((which, addr.version), 0) + 1
            self._next[(which, addr.version)] = n
            if which == "client":
                self.n_client_ips += 1
                got = self._take_ip(addr.version, CLIENT_V4, "2001:db8::{:x}", n)
            else:
                self.n_addr_ips += 1
                got = self._take_ip(addr.version, ADDR_V4, "2001:db8:1::{:x}", n)
            self.ips[key] = got
        return got

    def agent(self, ua):
        """`User-Agent` 1 つ。"""
        if not ua or ANON_UA_RE.match(ua):
            self.used.add(ua)
            return ua
        got = self.agents.get(ua)
        if got is None:
            got = self._take(UA_FMT, len(self.agents) + 1)
            self.agents[ua] = got
        return got

    def _take_host(self, group_n, n):
        """ホスト名 1 つぶんの番号を取る (単位の番号は決まっているので、動かすのは前の数)。"""
        while True:
            got = HOST_FMT.format(n, group_n)
            if got not in self.used:
                self.used.add(got)
                return got
            n += 1

    def _take(self, fmt, n):
        """すでに入力に混ざっていた置き換え先とぶつからない番号を取る。"""
        while True:
            got = fmt.format(n)
            if got not in self.used:
                self.used.add(got)
                return got
            n += 1

    def _take_ip(self, version, nets, v6fmt, n):
        while True:
            if version == 6:
                got = v6fmt.format(n)
            else:
                block, last = divmod(n - 1, 254)
                if block >= len(nets):
                    raise SystemExit("置き換え先の IPv4 が足りない (%d 個目)" % n)
                got = "{}.{}".format(nets[block], last + 1)
            if got not in self.used:
                self.used.add(got)
                return got
            n += 1

    # ------------------------------------------------------------ 欄 1 つぶん

    def target(self, value):
        """`connect://host:443` / `[2001:db8::1]:443` / `host` を、scheme と port を残して置き換える。"""
        if not value:
            return value
        scheme, rest = "", value
        at = value.find("://")
        if at > 0:
            scheme, rest = value[: at + 3], value[at + 3:]
        tail = ""
        slash = rest.find("/")
        if slash >= 0:  # `http://host:80/path` のような形 (パスはそのまま)
            rest, tail = rest[:slash], rest[slash:]
        if rest.startswith("["):
            end = rest.find("]")
            if end > 0:
                return scheme + "[" + self.host(rest[1:end]) + rest[end:] + tail
        head, sep, port = rest.rpartition(":")
        if sep and port.isdigit():
            return scheme + self.host(head) + ":" + port + tail
        return scheme + self.host(rest) + tail

    def client(self, value):
        """接続元 (`ip` か `ip:port`、IPv6 は `[ip]:port`)。"""
        if not value:
            return value
        if value.startswith("["):
            end = value.find("]")
            if end > 0:
                return "[" + self.ip(value[1:end], "client") + value[end:]
        head, sep, port = value.rpartition(":")
        if sep and port.isdigit() and head.count(":") == 0:
            return self.ip(head, "client") + ":" + port
        return self.ip(value, "client")

    # ------------------------------------------------------------ 文 1 つぶん

    def text(self, s):
        """`/log` の行や `/events` の説明。**ホストと IP だけ**を置き換え、他の語と数字は残す。"""
        if not s or ("." not in s and ":" not in s):
            return s
        s = TEXT_V6_BRACKET.sub(lambda m: "[" + self._text_ip(m.group(1)) + "]", s)
        s = TEXT_V6.sub(lambda m: self._text_ip(m.group(1)), s)
        s = TEXT_V4.sub(lambda m: self._text_ip(m.group(1)), s)
        dotless = self._dotless_re()
        if dotless is not None:
            s = dotless.sub(lambda m: self._text_host(m.group(0)), s)
        return TEXT_HOST.sub(self._text_hostport, s)

    def _text_ip(self, text):
        try:
            addr = ipaddress.ip_address(text)
        except ValueError:
            return text  # 時刻 (10:30:00) やバージョンは IP ではないので触らない
        got = self.ips.get(str(addr))
        if got is None:
            got = self.ip(text, "client")  # 文の中の IP は接続元のことが多い
        if got != text:
            self.text_hits += 1
        return got

    def _text_host(self, name):
        got = self.hosts.get(name.lower(), name)
        if got != name:
            self.text_hits += 1
        return got

    def _text_hostport(self, m):
        name, port = m.group(1), m.group(2)
        low = name.lower()
        if low in self.hosts:
            got = self.hosts[low]
        elif self._looks_like_host(low):
            got = self.host(name)
        else:
            got = name
        if got != name:
            self.text_hits += 1
        return got + (":" + port if port else "")

    def _looks_like_host(self, low):
        """表に無い `なにか.なにか` をホスト名と見なすか (拡張子・版・置き換え済みは見なさない)。"""
        if ANON_HOST_RE.match(low) or low in self.used or len(low) > 253:
            return False
        labels = low.split(".")
        if len(labels) < 2 or not all(labels):
            return False
        tld = labels[-1]
        return bool(TLD_RE.match(tld)) and tld not in FILE_SUFFIXES

    def _dotless_re(self):
        """点の無いホスト名 (`localhost` のような) が表にあれば、文の中でも置き換える。"""
        if self._dotless is None:
            names = sorted((k for k in self.hosts if "." not in k), key=len, reverse=True)
            self._dotless = re.compile(
                r"(?<![\w.\-])(?:" + "|".join(re.escape(n) for n in names) + r")(?![\w.\-])",
                re.IGNORECASE,
            ) if names else False
        return self._dotless or None

    # ------------------------------------------------------------ 全体

    def run(self, node):
        """**3 回なめる**: すでに匿名化済みの値を押さえ、欄の名前と IP を採番し、書き換える。

        1 回目は入力に混ざっている `host-0001.example` のような値を押さえるだけ (同じ番号を
        2 回出さないため)。2 回目で欄を採番し、3 回目で文も含めて書き換える。文の中にしか
        出てこない名前は 3 回目で採番されるので、番号は「欄に出た順 → 文に出た順」で決まる
        (同じ入力なら同じ番号)。
        """
        self._walk(node, None, "reserve")
        self._walk(node, None, "learn")
        return self._walk(node, None, "rewrite")

    def _walk(self, node, key, mode):
        if isinstance(node, dict):
            table = self._table(node, mode)
            out = {k: (table if k == "samples" and table is not None else self._walk(v, k, mode))
                   for k, v in node.items()}
            return out if mode == "rewrite" else node
        if isinstance(node, list):
            out = [self._walk(v, key, mode) for v in node]
            return out if mode == "rewrite" else node
        if isinstance(node, str):
            return self._value(node, key, mode)
        return node

    def _table(self, node, mode):
        """`{"keys":[…],"samples":[[…]]}` の表 (T14.10 の canary や T14.22 の時系列)。

        列の名前で見分ける (`canary_host` のように**位置でしか名前の分からない**欄がある)。
        """
        keys, rows = node.get("keys"), node.get("samples")
        if not (isinstance(keys, list) and isinstance(rows, list) and keys
                and all(isinstance(k, str) for k in keys)):
            return None
        cols = [self._column(k) for k in keys]
        if not any(cols):
            return None
        out = []
        for row in rows:
            if not isinstance(row, list):
                out.append(self._walk(row, "samples", mode))
                continue
            new_row = list(row)
            for i, col in enumerate(cols):
                if col and i < len(row) and isinstance(row[i], str):
                    new_row[i] = self._value(row[i], col, mode)
            out.append(new_row)
        return out if mode == "rewrite" else None

    @staticmethod
    def _column(name):
        """表の列の名前を、欄の名前 (`host` / `client`) に読み替える (関係なければ None)。"""
        if name in HOST_KEYS or name.endswith("_host"):
            return "host"
        if name in CLIENT_KEYS or name.endswith("_client"):
            return "client"
        return None

    def _value(self, s, key, mode):
        if mode == "reserve":
            return self._reserve(s, key)
        if key in HOST_KEYS:
            return self.target(s)
        if key in CLIENT_KEYS:
            return self.client(s)
        if key in ADDR_KEYS:
            return self.ip(s, "addr")
        if key in AGENT_KEYS:
            return self.agent(s)
        if key in TEXT_KEYS and mode == "rewrite":
            return self.text(s)
        return s

    def _reserve(self, s, key):
        """入力にすでに混ざっている置き換え先を押さえる (同じ番号を 2 回出さないため)。"""
        if not s:
            return s
        self.used.update(ANON_HOST_IN_TEXT.findall(s))
        if key in AGENT_KEYS:
            if ANON_UA_RE.match(s):
                self.used.add(s)
            return s
        if key in HOST_KEYS or key in CLIENT_KEYS or key in ADDR_KEYS:
            bare = s.split("://", 1)[-1].split("/", 1)[0]
            if bare.startswith("["):
                bare = bare[1:bare.find("]")] if "]" in bare else bare[1:]
            else:
                head, sep, port = bare.rpartition(":")
                if sep and port.isdigit():
                    bare = head
            try:
                addr = ipaddress.ip_address(bare)
            except ValueError:
                return s
            if any(addr in net for net in DOC_NETS if net.version == addr.version):
                self.used.add(str(addr))
        return s

    def summary(self):
        return ("ホスト {} 件 (まとめの単位 {} 件)、接続元 IP {} 件、答えの IP {} 件、"
                "User-Agent {} 件、文の中の置き換え {} か所").format(
            len(self.hosts), len(self.groups), self.n_client_ips, self.n_addr_ips,
            len(self.agents), self.text_hits)


# ---------------------------------------------------------------- 入力を 1 枚にする

# `/snapshot` の部の並び (`crates/endpoints/src/endpoints/recent.rs` の `snapshot()` と同じ)
PART_ORDER = ("status", "status_errors", "status_dns", "history.5", "history.60", "history.3600",
              "dns", "errors", "connections", "recent", "hosts", "hosts_series", "clients",
              "bursts", "profile", "events", "log")
STAMP_RE = re.compile(r"(\d{4})-(\d{2})-(\d{2})T(\d{2})(\d{2})(\d{2})?Z")


def taken_at_from_name(path):
    """`…/2026-09-16T0106Z-status` の `2026-09-16T0106Z` を epoch 秒にする (読めなければ 0)。

    `scripts/snapshot-diff.py` の同名の関数と同じ読み方。
    """
    m = STAMP_RE.search(os.path.basename(path))
    if not m:
        return 0
    y, mo, d, hh, mm, ss = m.groups()
    return int(datetime(int(y), int(mo), int(d), int(hh), int(mm), int(ss or 0),
                        tzinfo=timezone.utc).timestamp())


def classify(name):
    """ファイル名の (時刻より後ろの) 部分を `/snapshot` の部の名前にする (使わないなら None)。

    `scripts/snapshot-diff.py` の `classify()` と同じ見分け方。
    """
    s = STAMP_RE.sub("", os.path.basename(name)).lstrip("-")
    if s.startswith("snapshot"):
        return "snapshot"
    if s.startswith("status_sort_errors"):
        return "status_errors"
    if s.startswith("status_sort_dns"):
        return "status_dns"
    if s.startswith("status_sort_"):
        return None  # slow は `/status` と同じ中身なので読まない
    if s.startswith("status"):
        return "status"
    m = re.match(r"history_res_(\d+)", s)
    if m:
        return "history." + m.group(1)
    for part in ("hosts_series", "hosts", "clients", "dns", "errors", "connections", "recent",
                 "log", "events", "bursts", "profile"):
        if s.startswith(part):
            return part
    return None


def preferred(part, name):
    """同じ部の候補が複数あるときの好み (小さいほうを採る)。`snapshot-diff.py` と同じ。"""
    want = {"hosts": "requests", "clients": "requests", "dns": "misses"}.get(part)
    return 0 if want is None or want in os.path.basename(name) else 1


def read_json(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return None  # `-metrics` `-dashboard` のような JSON でないものは黙って飛ばす


def guess(body):
    """名前で分からないときに、中身から部の名前を当てる (`/status` だけを渡されたとき用)。"""
    if isinstance(body.get("status"), str) and "hosts" in body:
        return "status"
    if isinstance(body.get("samples"), list) and "keys" in body:
        return "history." + str(body.get("interval_secs") or 5)
    for key, part in (("entries", "dns"), ("lines", "log"), ("errors", "errors"),
                      ("connections", "connections"), ("events", "events"),
                      ("bursts", "bursts"), ("clients", "clients"), ("hosts", "hosts")):
        if isinstance(body.get(key), list):
            return part
    return None


def load_inputs(paths):
    """`/snapshot` 1 枚、または個別ファイルの束を `/snapshot` と同じ形にして返す。"""
    found, bodies = {}, {}
    for path in paths:
        body = read_json(path)
        if not isinstance(body, dict):
            continue
        if "parts" in body:  # `/snapshot` そのもの (名前は何でもよい)
            if len(paths) > 1:
                raise SystemExit("`/snapshot` の JSON は 1 枚だけ渡すこと: " + path)
            return body
        part = classify(path) or guess(body)
        if part is None or part == "snapshot":
            continue
        rank = preferred(part.split(".", 1)[0], path)
        if part in found and found[part] <= rank:
            continue
        found[part], bodies[part] = rank, body
    if not bodies:
        raise SystemExit("読める JSON が無い (`/snapshot` か `-status` 等のファイル群を渡すこと)")
    status = bodies.get("status", {})
    snap = {
        "taken_at": max((taken_at_from_name(p) for p in paths), default=0),
        "version": status.get("version", ""),
        "uptime_secs": status.get("uptime_secs", status.get("since_start_secs", 0)),
        "parts": [p for p in PART_ORDER if p in bodies] + sorted(set(bodies) - set(PART_ORDER)),
        "dropped": [],
    }
    for part in snap["parts"]:
        res = part.split(".", 1)
        if res[0] == "history":
            snap.setdefault("history", {})[res[1]] = bodies[part]
        else:
            snap[part] = bodies[part]
    return snap


def main(argv=None):
    p = argparse.ArgumentParser(
        description="デプロイ先の `/snapshot` を、数字を 1 つも変えずに匿名化する (T14.35)")
    p.add_argument("inputs", nargs="+", metavar="IN",
                   help="`/snapshot` の JSON 1 枚、または `-status` `-history_res_5` … のファイル群")
    p.add_argument("-o", "--out", required=True, metavar="OUT",
                   help="書き出し先 (`-` で標準出力)")
    p.add_argument("-q", "--quiet", action="store_true", help="件数を出さない")
    args = p.parse_args(argv)

    snap = load_inputs(args.inputs)
    anon = Anonymizer()
    out = anon.run(snap)
    text = json.dumps(out, ensure_ascii=False, separators=(",", ":")) + "\n"
    if args.out == "-":
        sys.stdout.write(text)
    else:
        with open(args.out, "w", encoding="utf-8") as f:
            f.write(text)
    if not args.quiet:
        print("匿名化: {} ({} B){}".format(
            anon.summary(), len(text.encode("utf-8")),
            "" if args.out == "-" else " -> " + args.out), file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
