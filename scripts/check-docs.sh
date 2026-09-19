#!/bin/bash
# README・`/` の案内・`/snapshot`・画面の関数名が、**コードと同じ集合**かどうかを機械で見る (T14.56)。
#
# 今日の版はエンドポイントが 10 本以上増え、環境変数も増えた。増やしたのは別々のエージェントで、
# **コードにあるのに文書に無い / 文書にあるのにコードに無い**が目で見つからなくなっている。
# 突き合わせるのは 5 つ:
#
#   (a) エンドポイント … `crates/endpoints/src/endpoints/mod.rs` の `path == "/…"`
#                        vs README の「動作確認 (curl)」の一覧 vs `/` の案内 (`endpoint_list`)
#   (b) 環境変数     … コードの `"PROXY_…"` / `"SERVER_…"` vs README の「環境変数」の表の鍵の欄
#   (c) `/snapshot`  … `recent::snapshot` の `parts` vs 個票のエンドポイント
#   (d) 画面の関数   … `check-dashboard.js` が `pick()` で切り出す名前 vs HTML の `function` と、呼び出し
#   (e) クレート     … README の「クレート構成」の表 vs `crates/*/Cargo.toml` の `[package] name`
#
# **意図的に外してあるものは下の除外表に理由つきで持つ** (黙って無視しない)。
# 差分が 1 件でもあれば理由を印字して exit 1。
#
# 使う道具は bash + grep / sed / awk / python3 の標準ライブラリだけ (外部依存なし)。
# 使い方: scripts/check-docs.sh   (引数なし。CI の `check` ジョブが毎回回す)
set -u
cd "$(dirname "$0")/.." || exit 1

W=$(mktemp -d) || exit 1
trap 'rm -rf "$W"' EXIT
NG=0
MOD=crates/endpoints/src/endpoints/mod.rs
REC=crates/endpoints/src/endpoints/recent.rs

# 2 つの一覧 (整列済み) を突き合わせ、片側にしか無いものを印字する。
#   diffset <見出し> <左の名前> <左のファイル> <右の名前> <右のファイル>
diffset() {
  local title=$1 ln=$2 lf=$3 rn=$4 rf=$5 only_l only_r
  only_l=$(comm -23 "$lf" "$rf" | tr '\n' ' ')
  only_r=$(comm -13 "$lf" "$rf" | tr '\n' ' ')
  if [ -n "$only_l$only_r" ]; then
    NG=1
    [ -n "$only_l" ] && echo "  NG $title: $ln にあって $rn に無い: $only_l"
    [ -n "$only_r" ] && echo "  NG $title: $rn にあって $ln に無い: $only_r"
  fi
}

echo "rust-http-proxy — 文書とコードの整合 (scripts/check-docs.sh)"
echo

# --- (a) エンドポイント -------------------------------------------------------
# コード側は `path == "/…"` と `path.strip_prefix("/snapshots/")` の全部。
# 別名 (末尾の `/`、`/dashboard/inspect`、`/probe`) は代表の綴りに寄せてから比べる。
norm_ep() {
  sed -e 's|\(.\)/$|\1|' \
      -e 's|^/dashboard/inspect$|/inspect|' \
      -e 's|^/probe$|/probe.html|' \
      -e 's|^\(/snapshots\)/..*|\1/<date>|' |
    grep -v '^/$' | grep -v '^$' | LC_ALL=C sort -u
}
{
  grep -oE 'path == "[^"]*"' "$MOD" | sed 's/.*"\(.*\)"/\1/'
  grep -oE 'path\.strip_prefix\("[^"]*"\)' "$MOD" | sed 's/.*"\(.*\)".*/\1<date>/'
} | norm_ep >"$W/ep-code"

# `/` の案内 (`fn endpoint_list`)。1 行の先頭の `/…` がその行のパス。
sed -n '/^fn endpoint_list/,/^}/p' "$MOD" |
  awk '/\\x20 \/|"  \// { if (match($0, /\/[A-Za-z0-9<>_.\/-]+/)) print substr($0, RSTART, RLENGTH) }' |
  sed 's|^/snapshots/<YYYY-MM-DD>$|/snapshots/<date>|' | norm_ep >"$W/ep-guide"

# README 側は「動作確認 (curl)」の節の URL (コメント行の URL も拾う)。
# **この節が README のエンドポイント一覧**で、他の節の言及は説明であって一覧ではない。
sed -n '/^## 動作確認 (curl)/,/^```$/p' README.md |
  grep -oE 'http://127\.0\.0\.1:8080/[A-Za-z0-9<>?=&|,.:/_%$()-]*' |
  sed -e 's|http://127.0.0.1:8080||' -e 's/[?].*//' | norm_ep >"$W/ep-readme"

printf '(a) エンドポイント: %s の path == "/…" %s 件 / README「動作確認 (curl)」 %s 件 / `/` の案内 %s 件\n' \
  "$MOD" "$(wc -l <"$W/ep-code")" "$(wc -l <"$W/ep-readme")" "$(wc -l <"$W/ep-guide")"
diffset "(a)" "コード" "$W/ep-code" "README" "$W/ep-readme"
diffset '(a)' 'コード' "$W/ep-code" '`/` の案内' "$W/ep-guide"
echo "    除外: /  — 案内そのもの (自分を一覧に載せない)"
echo "    別名: /inspect=/dashboard/inspect, /probe.html=/probe, 末尾の / は同じ口"

# --- (b) 環境変数 -------------------------------------------------------------
# コード側は `crates/` と `src/` の `"PROXY_…"` / `"SERVER_…"` の文字列すべて
# (`config.rs` の `src.mark()` と各クレートの `envfile::var()` はここに入る)。
cat >"$W/env-skip" <<'EOF'
PROXY_VERSION	build.rs が作るビルド時の値 (`env!`)。実行時に読む設定ではない
PROXY_NO_SUCH_KEY_T1415	「設定されていない鍵」を確かめるテスト専用の名前
EOF
grep -rhoE '"(PROXY|SERVER)_[A-Z0-9]+[A-Z0-9_]*"' --include='*.rs' crates/ src/ |
  tr -d '"' | LC_ALL=C sort -u >"$W/env-all"
cut -f1 "$W/env-skip" | LC_ALL=C sort -u >"$W/env-skip-names"
comm -23 "$W/env-all" "$W/env-skip-names" >"$W/env-code"

sed -n '/^## 環境変数/,/^## /p' README.md |
  sed -n 's/^| \(`[^|]*`[^|]*\) |.*/\1/p' |
  grep -oE '`(PROXY|SERVER)_[A-Z0-9_]+`' | tr -d '`' | LC_ALL=C sort -u >"$W/env-readme"

printf '(b) 環境変数: コードの文字列 %s 件 (除外 %s) / README の表の鍵 %s 件\n' \
  "$(wc -l <"$W/env-code")" "$(wc -l <"$W/env-skip-names")" "$(wc -l <"$W/env-readme")"
diffset "(b)" "コード" "$W/env-code" "README の表" "$W/env-readme"
sed 's/^/    除外: /;s/\t/ — /' "$W/env-skip"

# --- (c) `/snapshot` の parts -------------------------------------------------
# `parts` の名前を口の綴りに戻す: `status_errors` / `status_dns` は `/status` の並べ替え、
# `history.5` などは `/history` の解像度、`hosts_series` は `/hosts/series`。
sed -n '/let mut part: Vec<(&.static str, String)> = vec!\[/,/^    \];/p' "$REC" |
  grep -oE '^ *\(?"[a-z0-9_.]+",' | sed 's/[^"]*"\([^"]*\)".*/\1/' >"$W/parts-raw"
sed -e 's/^status_.*$/status/' -e 's/^history\..*$/history/' -e 's|^hosts_series$|hosts/series|' \
  "$W/parts-raw" | sed 's|^|/|' | LC_ALL=C sort -u >"$W/parts-ep"

# **`/snapshot` に入れていない口は、ここに理由を書いて外す** (書かずに外すと気づけない)。
cat >"$W/snap-skip" <<'EOF'
/dashboard	ブラウザで開く画面 (HTML)
/inspect	ブラウザで開く画面 (HTML)
/probe.html	ブラウザで開く画面 (HTML)
/proxy.pac	ブラウザの自動設定スクリプト (JSON ではない)
/metrics	Prometheus 形式。数字は `/status` と同じもの
/healthz	いまの生死の判定。元の数字は `/status` に入っている
/config	効いている設定。`/status` の `settings` に同じものが入る
/purge	キャッシュを消す操作の口 (写真ではない)
/lookup	1 URL を指定して引く口 (`?url=` が要る)
/blocklist	1 ホストを指定して引く / 変える口 (`?host=` が要る)
/explain	1 相手を指定して組む口 (`?host=` / `?client=` が要る。中身は `/hosts` `/dns` `/recent` の組み直し)
/snapshot	自分自身
/snapshots	ディスクに残した過去の雪像の一覧 (雪像の中に雪像は入れない)
/snapshots/<date>	同上 (1 日ぶんの中身)
/readers	`/status` の末尾の `readers` に上位 20 が入る (T14.53)
/trace	`PROXY_TRACE_CLIENT` を設定したときだけ中身があり、**URL のパスが入る** (T14.27)。誰でも取れる雪像には入れない
/slo	`/history` の標本から判定し直せる (T14.50)
/daily	日ごとの要約はファイル (`~/.rust-http-proxy.daily.jsonl`) に永久に残る。雪像は「そのときの様子」を撮るもの (T14.20 の申し送りは `parts` に足すこと。足すときは `scripts/anonymize-snapshot.py` の PART_ORDER と `snapshot-diff.py` も一緒に直す)
EOF
cut -f1 "$W/snap-skip" | LC_ALL=C sort -u >"$W/snap-skip-names"
LC_ALL=C sort -u "$W/ep-code" | comm -23 - "$W/snap-skip-names" >"$W/snap-want"

printf '(c) `/snapshot` の parts: %s 部 = %s 口 / 雪像に入るべき口 %s (除外 %s)\n' \
  "$(wc -l <"$W/parts-raw")" "$(wc -l <"$W/parts-ep")" "$(wc -l <"$W/snap-want")" "$(wc -l <"$W/snap-skip-names")"
diffset "(c)" "parts" "$W/parts-ep" "個票の口" "$W/snap-want"
sed 's/^/    除外: /;s/\t/ — /' "$W/snap-skip"

# --- (d) 画面の関数名 ---------------------------------------------------------
# `check-dashboard.js` は HTML の `<script>` から名前で関数を切り出して 1 つの object にする。
# 見るのは 3 つ: (1) 載せた名前が HTML にあること、(2) 呼んでいる名前を載せてあること、
# (3) 載せたのに誰も (テストも、切り出した他の関数も) 呼ばない名前が残っていないこと。
python3 - <<'PY' >"$W/api-out"
import re

src = open('scripts/check-dashboard.js', encoding='utf-8').read()
consts = dict(re.findall(r"const\s+(\w+)\s*=\s*\[([^\]]*)\];", src))
pages = {}


def body_of(js, name):
    """`function name(` から対応する `}` まで (check-dashboard.js の pick() と同じ切り方)。"""
    at = js.find('function ' + name + '(')
    if at < 0:
        return ''
    depth, i = 0, js.index('{', at)
    for j in range(i, len(js)):
        if js[j] == '{':
            depth += 1
        elif js[j] == '}':
            depth -= 1
            if depth == 0:
                return js[at:j + 1]
    return ''


total = 0
for m in re.finditer(
        r"const\s+(\w+)\s*=\s*pick\(\s*[^,]+,\s*(\[[^\]]*\]|\w+)\s*,\s*'([^']+)'", src):
    var, arr, html = m.groups()
    listed = re.findall(r"'([A-Za-z_]\w*)'", arr[1:-1] if arr.startswith('[') else consts.get(arr, ''))
    if html not in pages:
        page = open('crates/web/src/' + html, encoding='utf-8').read()
        pages[html] = re.search(r"<script>([\s\S]*?)</script>", page).group(1)
    js = pages[html]
    bodies = {n: body_of(js, n) for n in listed}
    called = set(re.findall(r"\b" + var + r"\.([A-Za-z_]\w*)", src))
    # 切り出した関数どうしの呼び出し (`timeline` の中の `num()` など) も「呼ばれている」
    inner = {o for n in listed for o in listed if o != n and re.search(r"\b" + o + r"\s*\(", bodies[n])}
    total += len(listed)
    print('    %s (%s): %d 個' % (var, html, len(listed)))
    for kind, names in (
            ('HTML に無い', [n for n in listed if not bodies[n]]),
            ('呼んでいるが載せていない', sorted(called - set(listed))),
            ('載せたが誰も呼ばない', sorted(set(listed) - called - inner))):
        if names:
            print('  NG (d): %s の %s: %s' % (var, kind, ' '.join(names)))
print('TOTAL %d' % total)
PY
printf '(d) 画面の関数: check-dashboard.js が切り出す %s 個\n' "$(sed -n 's/^TOTAL //p' "$W/api-out")"
grep -v '^TOTAL ' "$W/api-out"
grep -q '^  NG ' "$W/api-out" && NG=1

# --- (e) クレートの表 ---------------------------------------------------------
awk '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^name = /{gsub(/name = "|"/,"");print}' \
  crates/*/Cargo.toml Cargo.toml | LC_ALL=C sort -u >"$W/crate-code"
sed -n '/^## クレート構成/,/^## /p' README.md |
  sed -n 's/^| `\([a-z0-9-]*\)` |.*/\1/p' | LC_ALL=C sort -u >"$W/crate-readme"
printf '(e) クレート: Cargo.toml %s 個 / README の表 %s 行\n' \
  "$(wc -l <"$W/crate-code")" "$(wc -l <"$W/crate-readme")"
diffset "(e)" "Cargo.toml" "$W/crate-code" "README の表" "$W/crate-readme"
# 表の前の文の「(N + 本体)」も表と合っていること (本体 = rust-http-proxy の 1 行を引く)
SAID=$(sed -n 's/.*分けてあります (\([0-9]*\) + 本体).*/\1/p' README.md)
HAVE=$(($(wc -l <"$W/crate-code") - 1))
if [ "$SAID" != "$HAVE" ]; then
  NG=1
  echo "  NG (e): README の「($SAID + 本体)」が表の $HAVE 個と合わない"
fi

echo
if [ "$NG" = 0 ]; then
  echo "5 つを突き合わせて 差分 0 件。"
  exit 0
fi
echo "差分あり。README / \`/\` の案内 / \`/snapshot\` / 環境変数の表 を直すか、理由を除外表に書くこと。"
exit 1
