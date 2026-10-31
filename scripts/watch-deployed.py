#!/usr/bin/env python3
"""デプロイ先の最小の見張り (T.28.26)。`.github/workflows/watch.yml` が回す (cron は毎時だが、実際の間隔は 2〜7.5 時間)。

読むのは `/healthz` と `/events?n=200` と `/status?hosts=0` の 3 つだけ (どれも読む口)。次のどれかで exit 1:

  - `/healthz` が 200 でない、または `ok` が真でない (落ちている検査の名前を出す)
  - **前回の見張りから** (`--since`。無ければ直近 `--window` 分、既定 480 分) に**無言の再起動** = 直前の
    出来事が `shutdown` でない `start` がある (T.32.25。cron は `17 * * * *` でも GitHub の schedule は
    121.2 時間で 23 回 (中央値 5.5 時間・最大 7.5 時間) しか回らず、75 分の窓では約 8 割の期間の再起動が
    見えなかった。workflow は前回の成功した実行の時刻を `--since` に渡す)
    (`scripts/snapshot_criteria.py` の `silent_restarts` と同じ見方。ホストの再起動 (`boot` が変わった) も
    数える。こちらが原因かどうかは締めで切り分ける)
  - 3 つのどれかが読めない (届かない・JSON でない)

**出すのは結論・版・起動からの時間・落ちた検査の名前だけ**。このリポジトリは public で、Actions の実行ログは
誰でも読めるので、個票・接続元・宛先・出来事の文面は出さない。

使い方: scripts/watch-deployed.py HOST:PORT [--since 時刻] [--window MIN] [--timeout SEC]
  `--since` は前回の見張りの時刻 (ISO 8601 の `2026-10-31T02:20:00Z` か epoch 秒)。そこから 15 分さかのぼって
  探す (前回の見張りが読んだ直後の再起動を落とさない)。読めなければ `--window` に戻る
依存は Python 3 の標準ライブラリだけ。
"""

import argparse
import json
import sys
import time
from datetime import datetime
import urllib.error
import urllib.request


def fetch(base, path, timeout):
    """`http://HOST:PORT<path>` を引いて (状態コード, JSON) を返す。JSON でなければ None。"""
    req = urllib.request.Request(base + path, headers={"User-Agent": "rust-http-proxy-watch"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            code, body = r.status, r.read()
    except urllib.error.HTTPError as e:
        code, body = e.code, e.read()
    try:
        return code, json.loads(body)
    except ValueError:
        return code, None


def silent_starts(events, since):
    """`since` (epoch 秒) より後の `start` のうち、直前の出来事が `shutdown` でないものの時刻。"""
    marks = sorted((e for e in events if isinstance(e, dict) and e.get("at")), key=lambda e: e["at"])
    out = []
    for i, e in enumerate(marks):
        if e.get("kind") != "start" or not (e.get("text") or "").startswith("version ") or e["at"] <= since:
            continue
        earlier = [m for m in marks[:i] if m["at"] < e["at"]]
        if earlier and earlier[-1].get("kind") != "shutdown":
            out.append(e["at"])
    return out


def judge(healthz, events, status, now, window_min):
    """3 つの JSON から (問題の一覧, 版, 起動からの秒) を返す (`main` と試験が使う)。"""
    problems = []
    code, h = healthz
    if code != 200 or not isinstance(h, dict) or h.get("ok") is not True:
        failed = sorted(k for k, v in ((h or {}).get("checks") or {}).items()
                        if isinstance(v, dict) and v.get("ok") is False)
        problems.append("/healthz が ok でない (状態 %s%s)" % (code, "、落ちた検査: " + ", ".join(failed) if failed else ""))
    code, ev = events
    if code != 200 or not isinstance(ev, dict) or not isinstance(ev.get("events"), list):
        problems.append("/events が読めない (状態 %s)" % code)
    else:
        silent = silent_starts(ev["events"], now - window_min * 60)
        if silent:
            problems.append("直近 %d 分に無言の再起動が %d 回 (%s)" % (
                window_min, len(silent),
                ", ".join(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t)) for t in silent)))
    code, st = status
    version = uptime = None
    if code != 200 or not isinstance(st, dict):
        problems.append("/status が読めない (状態 %s)" % code)
    else:
        version, uptime = st.get("version"), st.get("uptime_secs")
    return problems, version, uptime


# `--since` から、さらにさかのぼる分 (前回の見張りが読んだ直後の再起動を、次の窓から落とさない)
SINCE_OVERLAP_MIN = 15
# `--since` が無いときの窓 (分)。GitHub の schedule の実際の間隔の最大 7.5 時間より長く (T.32.25)
DEFAULT_WINDOW_MIN = 480
# `--since` から出した窓の下限 (分。T.28.26 の 75 分)
MIN_WINDOW_MIN = 75


def parse_since(text):
    """`--since` の値 (ISO 8601 か epoch 秒) を epoch 秒にする。読めなければ None。"""
    if not text:
        return None
    text = text.strip()
    try:
        return float(text)
    except ValueError:
        pass
    try:
        return datetime.fromisoformat(text.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def window_minutes(since, now, fallback):
    """`since` (epoch 秒か None) から窓の分を出す。前回より前にさかのぼる重なりを足し、`MIN_WINDOW_MIN` より短くはしない。

    `since` が無い・未来なら `fallback`。
    """
    if since is None or since > now:
        return fallback, False
    return max(int((now - since) / 60) + 1 + SINCE_OVERLAP_MIN, MIN_WINDOW_MIN), True


def main(argv=None):
    ap = argparse.ArgumentParser(description="デプロイ先の最小の見張り (読む口だけ)")
    ap.add_argument("target", help="HOST:PORT")
    ap.add_argument("--since", help="前回の見張りの時刻 (ISO 8601 か epoch 秒)。そこから 15 分前までさかのぼって探す")
    ap.add_argument("--window", type=int, default=DEFAULT_WINDOW_MIN,
                    help="`--since` が無い・読めないときの窓 (分。既定 %d)" % DEFAULT_WINDOW_MIN)
    ap.add_argument("--timeout", type=float, default=20.0)
    a = ap.parse_args(argv)
    now = time.time()
    since = parse_since(a.since)
    window, from_since = window_minutes(since, now, a.window)
    if a.since and since is None:
        print("note: --since が読めない (%s) ので直近 %d 分を見る" % (a.since, window))
    base = "http://" + a.target.removeprefix("http://").rstrip("/")
    got = []
    for path in ("/healthz", "/events?n=200", "/status?hosts=0"):
        try:
            got.append(fetch(base, path, a.timeout))
        except (OSError, urllib.error.URLError) as e:
            got.append((None, None))
            print("届かない: %s (%s)" % (path, type(e).__name__))
    problems, version, uptime = judge(*got, now=now, window_min=window)
    up = ("%.1f 時間" % (uptime / 3600.0)) if isinstance(uptime, (int, float)) else "?"
    print("版 %s / 起動から %s" % (version or "?", up))
    for p in problems:
        print("NG: " + p)
    if not problems:
        print("OK: /healthz は ok、直近 %d 分%sに無言の再起動なし"
              % (window, " (前回の見張りから)" if from_since else ""))
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
