#!/usr/bin/env node
// ダッシュボード (`crates/endpoints/src/web/dashboard.html`) の JS を、ブラウザ無しで確かめる。
//
// ブラウザが無い環境でも壊れに気づけるように、見るのは 3 つだけ:
//   1. <script> の中身が構文として通ること (`new Function` = `node --check` と同じ判定)
//   2. **`/history` の配列の配列を読む関数の読み方が、実際の出力と合っていること**
//      (キーの並び・入れ子の配列・区間の分位点。T12.4 (5))
//   3. **`/status` を読む関数が実出力で例外なく描けること** (名前解決の KPI、
//      「悪いホスト」の表、直近の山。T13.3)
//   4. **個票 (`/errors` `/connections`) を読む関数**が、形の違う入力でも落ちないこと
//      (作り置きは架空のホスト名。T13.4)
//   5. **`/history` の `closed` (閉じた接続の分布) と `/bursts` の写真**が読めること
//      (区間の数・合計と件数の一致・並び。T14.6)
//   6. **カーネルと cgroup の窓** (`/history` の `kernel`、`/status` の `kernel`) が読めること (T14.12)
//   7. **`/events` の時系列**が読めること (12 種の綴り・新しい順・`?since=` の絞り。T14.11 / T14.23 / T14.54)
//   8. **`/profile` を読む関数** (段階・スレッド・ロック) が実出力と合っていること (T14.3)
//   9. **`/history` の `transfer`** (転送速度と半閉じの分布) が読めること (区間の数・合計と件数の一致。T14.25。
//      末尾の `stall_client_ms_sum` / `stall_origin_ms_sum` = 中継の詰まりの向きの合計は T14.42)
//  10. **「調査」ページ (`inspect.html`) の描画関数**が `/snapshot` の実出力で例外なく通ること
//      (タイムライン・遅い接続・山・接続元・RTT の散布・起動からの窓・出来事の印。T14.8)
//  11. **「端末から測る」ページ (`probe.html`) の関数** (`median` / `pick` / `render`) が
//      作り物の数列と `/status` の実出力で通り、「経由している / していない」を判定できること (T14.33)
//  12. **KPI「CONNECT 確立 p50」が `/status` の `recent_quantiles` (直近 1,024 本の実測) を
//      使い、無い版の出力では区間の補間に落ちること** (T14.31)
//  13. **KPI「今日の SLO」が `/slo` の応答を読めること** (今日の達成率・外れの最多・
//      直近の外れた時間帯。`/slo` を持たない版では `null` を返してカードごと出さない。T14.50)
//  14. **応答の形の版 (`schema`)** で分岐しても読み方が変わらないこと (T14.49)。
//      **版の無い古い出力 (版 0) も版 1 の出力も同じ関数で同じ結果**になること
//  15. **「調査」ページの「今日」「今週」「出来事と異常」** (`dailyRows` / `weeklyRows` /
//      `eventRows`) が `/daily` (T14.20)・`/snapshots` (T14.34)・`/events` (T14.11 / T14.23) の
//      出力と、**匿名化した実データ**の「無い版」の分岐で通ること (T14.44)
//  16. **T15.0 (14) の 5 枚のカード** (CPU の絞り・動かないトンネル・名前解決の内訳・
//      受付待ち・利用者が待つ時間) が、**欄がある版**と**欄が無い古い版**の両方で通ること。
//      古い版では例外を出さず「無い」と分かる形 (null / 0 件) に落ちること
//  17. **`dashboard.html` の大きさ**が 80 KiB 以下であること (inspect と同じ作法。T15.0 (14))
//
// 使い方: node scripts/check-dashboard.js [/history の実出力.json] [/status の実出力.json]
//                                         [/profile の実出力.json] [/snapshot の実出力.json]
//   引数を省くと下の作り置き (手元のプロキシから取った実出力と、架空のホスト名の見本) を使う。
// 依存なし。Node があるときだけ回す補助的な確認で、`cargo test` の代わりではない。

const fs = require('fs');
const path = require('path');

const html = fs.readFileSync(
  path.join(__dirname, '..', 'crates/endpoints/src/web/dashboard.html'),
  'utf8'
);
const m = html.match(/<script>([\s\S]*?)<\/script>/);
if (!m) fail('dashboard.html に <script> が無い');
const js = m[1];

function fail(msg) {
  console.error('NG: ' + msg);
  process.exit(1);
}

// 1. 構文
try {
  new Function(js);
} catch (e) {
  fail('JS の構文エラー: ' + e.message);
}

// 2. `/history` と `/status` と個票を読む関数を取り出して動かす (DOM に触らない 8 つだけ)
const names = [
  'toSamples',
  'winQuantile',
  'mergeWindows',
  'dnsStats',
  'badHosts',
  'peak',
  'errorRows',
  'connRows',
  // `/profile` を読む側 (T14.3)
  'toProfile',
  'stageRows',
  'longestStage',
  'roleRows',
  'lockRows',
  // KPI「CONNECT 確立 p50」の値を選ぶ側 (T14.31) と、それが使う整形 (どれも DOM に触らない)
  'connectKpi',
  // KPI「今日の SLO」の値を選ぶ側 (T14.50)
  'sloKpi',
  // T15.0 (14) の 5 枚のカードが読む側 (どれも DOM に触らない)
  'toKernel',
  'cpuThrottle',
  'topThreads',
  'runDelay',
  'queueSpread',
  'idleTunnels',
  'dnsMissKinds',
  'waitKpi',
  // 100% の横棒 1 本を組み立てる側 (T15.0 (14) で tooltip の整形を呼ぶ側に渡せるようにした)
  'stackHtml',
  'esc',
  'fmtMs',
  'fmtMsFine',
  'fmtNum',
  'fmtDur',
];
// 名前で関数を切り出して 1 つの object にする (DOM に触らない関数だけ渡すこと)
function pick(source, wanted, where, prelude) {
  let src = prelude || '';
  for (const n of wanted) {
    const at = source.indexOf('function ' + n + '(');
    if (at < 0) fail(n + ' が ' + where + ' に無い (名前を変えたらこの確認も直すこと)');
    // 対応する閉じ括弧まで
    let depth = 0, i = source.indexOf('{', at), end = -1;
    for (; i < source.length; i++) {
      if (source[i] === '{') depth++;
      else if (source[i] === '}' && --depth === 0) { end = i + 1; break; }
    }
    if (end < 0) fail(n + ' の括弧が閉じていない (' + where + ')');
    src += source.slice(at, end) + '\n';
  }
  return new Function(src + 'return {' + wanted.join(',') + '};')();
}
// `stackHtml` が使う色だけは関数ではないので、HTML からそのまま切り出して前置きにする
const stageColors = js.match(/var STAGE_COLORS=\[[^\]]*\];/);
if (!stageColors) fail('dashboard.html に STAGE_COLORS が無い');
const api = pick(js, names, 'dashboard.html', stageColors[0] + '\n');

const file = process.argv[2] || path.join(__dirname, 'testdata', 'history-res5.json');
const hist = JSON.parse(fs.readFileSync(file, 'utf8'));

const samples = api.toSamples(hist);
if (samples.length !== hist.samples.length) fail('標本の数が合わない');
if (samples.length === 0) fail('標本が 0 件 (5 秒以上動かしたプロキシの出力を渡すこと)');
for (const k of hist.keys) {
  if (!(k in samples[0])) fail('列 ' + k + ' が読めていない');
}
if (!Array.isArray(samples[0].connect_buckets)) fail('connect_buckets が入れ子の配列で読めていない');
if (samples[0].connect_buckets.length !== hist.bounds_ms.length + 1) {
  fail('区間の数が bounds_ms + 1 (上限なし) になっていない');
}
if (!Array.isArray(samples[0].errors_by_cause) || samples[0].errors_by_cause.length !== hist.causes.length) {
  fail('errors_by_cause が causes と同じ数で読めていない');
}

// 分位点: 件数のある標本で 0 < p50 <= p95 <= その窓の最大値
let checked = 0;
for (const s of samples) {
  if (!s.connects) continue;
  const p50 = api.winQuantile(s.connect_buckets, s.connects, s.connect_ms_max, 0.5, hist.bounds_ms);
  const p95 = api.winQuantile(s.connect_buckets, s.connects, s.connect_ms_max, 0.95, hist.bounds_ms);
  if (p50 === null) fail('件数があるのに p50 が null');
  if (!(p50 <= p95 + 1e-9)) fail('p50 ' + p50 + ' > p95 ' + p95);
  if (!(p95 <= s.connect_ms_max)) fail('p95 ' + p95 + ' が窓の最大値 ' + s.connect_ms_max + ' を超えた');
  if (!(s.connect_ms_sum / s.connects <= s.connect_ms_max)) fail('平均が最大値を超えた');
  checked++;
}
if (checked === 0) fail('CONNECT のある標本が 1 つも無い (--only connect で回した出力を渡すこと)');
if (api.winQuantile([0, 0], 0, 0, 0.5, hist.bounds_ms) !== null) fail('件数 0 は null のはず');

// 直近 5 分 (60 標本) の合成: 件数と最大値が足し合わせ / 最大になっていること。
// 比べる相手も**同じ直近 60 標本**にする (全標本と比べると、標本が 60 を超える実出力で必ず食い違う。
// デプロイ先の res=60 (1,440 標本) で「29 != 8839」と出た誤検出がそれ)
const merged = api.mergeWindows(samples, 60, 'connect');
const recent = samples.slice(Math.max(0, samples.length - 60));
const total = recent.reduce((a, s) => a + s.connects, 0);
const max = recent.reduce((a, s) => Math.max(a, s.connect_ms_max), 0);
if (merged.count !== total) fail('mergeWindows の件数 ' + merged.count + ' != ' + total);
if (merged.max !== max) fail('mergeWindows の最大値 ' + merged.max + ' != ' + max);
const bucketSum = merged.buckets.reduce((a, b) => a + b, 0);
if (bucketSum !== total) fail('区間の合計 ' + bucketSum + ' != 件数 ' + total);
const kpi = api.winQuantile(merged.buckets, merged.count, merged.max, 0.5, hist.bounds_ms);
if (!(kpi >= 0 && kpi <= merged.max)) fail('KPI の p50 が範囲外: ' + kpi);

// 累計カウンタの列は **0 以上の数**で読めること (ブラウザ側は差分でレートにする)。
// 列がずれると入れ子の配列 (`connect_buckets` / `errors_by_cause`) や undefined が入るので、
// 型で気づける。**単調増加は見ない**: 累計は再起動で 0 に戻るので、複数の起動をまたぐ
// 出力 (`res=3600` は 30 日ぶん) では正しくても減る。
// `evicted_idle` は T14.2 でレコードの余白に足した列なので、**古い出力には無い** (飛ばす)
for (const k of ['requests', 'bytes', 'evicted_idle']) {
  if (!(k in samples[0])) continue;
  for (const s of samples) {
    if (typeof s[k] !== 'number' || !(s[k] >= 0)) {
      fail('累計のはずの ' + k + ' が 0 以上の数で読めていない: ' + JSON.stringify(s[k]));
    }
  }
}

// 直近 60 標本のゲージの山 (「接続中」のカードの「山 (直近 5 分)」。T13.3)
const want = recent.reduce(
  (a, s) => Math.max(a, s.active_max != null ? s.active_max : s.active),
  0
);
const pk = api.peak(samples, 60, 'active_max', 'active');
if (pk !== want) fail('peak の山 ' + pk + ' != ' + want);
if (api.peak([], 60, 'active_max', 'active') !== null) fail('標本 0 は null のはず');

// 3. `/status` を読む関数 (名前解決の KPI と「悪いホスト」の表。T13.3)
const statusFile = process.argv[3] || path.join(__dirname, 'testdata', 'status.json');
const st = JSON.parse(fs.readFileSync(statusFile, 'utf8'));
if (!Array.isArray(st.hosts)) fail(statusFile + ' に hosts[] が無い (/status の出力を渡すこと)');
const dns = st.dns || {};
const dn = api.dnsStats(st);
if (dn.lookups !== (dns.hits || 0) + (dns.misses || 0)) fail('解決した回数が合わない');
if (dn.lookups && !(dn.rate >= 0 && dn.rate <= 100)) fail('ミス率が範囲外: ' + dn.rate);
if (dns.misses && Math.abs(dn.avg - dns.miss_ms_sum / dns.misses) > 0.02) {
  fail('ミス 1 回の平均が miss_ms_sum ÷ misses と合わない: ' + dn.avg);
}
// 無いキーは null (「–」で描く)、あれば数で返る。T13.1 が `refreshes` と
// `negative_ttl_secs` を足すまでは実出力に無いので、両方の枝をここで見る
if (api.dnsStats({}).rate !== null) fail('dns が無いときは null のはず');
if (api.dnsStats({ dns: {} }).refreshes !== null) fail('無いキーは null のはず');
if (api.dnsStats({ dns: { refreshes: 7, negative_ttl_secs: 60 } }).refreshes !== 7) {
  fail('refreshes を読めていない');
}
if (api.dnsStats({ dns: {} }).warm !== null) fail('warm が無ければ null のはず');
if (api.dnsStats({ dns: { warm: 5, warm_secs: 900 } }).warm !== 5) {
  fail('warm を読めていない');
}

const causeNames = hist.causes || [];
const bad = api.badHosts([st.hosts], causeNames, 10);
if (bad.length > 10) fail('上位 10 件を超えた: ' + bad.length);
for (const b of bad) {
  if (typeof b.host !== 'string' || !b.host) fail('host が読めていない');
  if (!(b.errors > 0 || b.dns_sum > 0 || b.dns_avg !== null)) {
    fail('エラーも名前解決も無いホストが表に出た: ' + b.host);
  }
  if (b.dns_avg !== null && !(b.dns_avg >= 0)) fail('名前解決の平均が数でない: ' + b.host);
  if (b.conn_avg !== null && !(b.conn_avg >= 0)) fail('確立の平均が数でない: ' + b.host);
  for (const c of b.causes) {
    if (!causeNames.some((n) => c.indexOf(n + ' ') === 0)) fail('原因の名前が違う: ' + c);
  }
}
for (let i = 1; i < bad.length; i++) {
  const a = bad[i - 1], b = bad[i];
  if (a.errors < b.errors || (a.errors === b.errors && a.dns_sum < b.dns_sum)) {
    fail('並びが崩れた: ' + a.host + ' の次に ' + b.host);
  }
}
// 2 枚 (?sort=errors と ?sort=dns) を混ぜても同じホストは 1 行だけ
if (api.badHosts([st.hosts, st.hosts], causeNames, 10).length !== bad.length) {
  fail('2 枚を混ぜたら重複した');
}
if (api.badHosts([null, undefined], causeNames, 10).length !== 0) fail('空でも例外なく 0 件のはず');

// 5. canary の配列 (T14.10、T14.37 で 5 列目)。`/history` の応答に**別の配列**として付く
// (`{"keys":[...],"samples":[[t,dns_ms,connect_ms,"host",ipv6_connect_ms],...]}`)。描くのは
// T14.8 なので、ここでは**出力の形**だけを見る: 列名・行の長さ・型・時刻が古い順であること。
//
// **列は末尾にしか足さない**約束なので、列名は**先頭からの一致**で見る (T14.37 より前の
// 作り置き = 4 列でも落ちない)。行の長さは `keys` の長さと合っていること。
const CANARY_KEYS = [
  't',
  'canary_dns_ms',
  'canary_connect_ms',
  'canary_host',
  'canary_ipv6_connect_ms',
];
function checkCanary(h) {
  const c = h && h.canary;
  if (c === undefined || c === null) return null; // canary の無い版の出力 (飛ばす)
  const want = CANARY_KEYS;
  if (
    !Array.isArray(c.keys) ||
    c.keys.length < 4 ||
    c.keys.length > want.length ||
    c.keys.some((k, i) => k !== want[i])
  ) {
    fail('canary の keys が ' + want.join(',') + ' の先頭からの一致でない: ' + JSON.stringify(c.keys));
  }
  if (!Array.isArray(c.samples)) fail('canary の samples が配列でない');
  let last = 0;
  for (const row of c.samples) {
    if (!Array.isArray(row) || row.length !== c.keys.length) {
      fail('canary の 1 行が ' + c.keys.length + ' 列でない: ' + JSON.stringify(row));
    }
    const [t, dns, conn, host, v6] = row;
    if (typeof t !== 'number' || !(t > 0)) fail('canary の時刻が epoch 秒でない: ' + t);
    if (t < last) fail('canary の標本が古い順になっていない: ' + t + ' < ' + last);
    last = t;
    if (typeof dns !== 'number' || !(dns >= 0)) fail('canary_dns_ms が数でない: ' + dns);
    if (typeof conn !== 'number' || !(conn >= 0)) fail('canary_connect_ms が数でない: ' + conn);
    if (typeof host !== 'string' || !host) fail('canary_host が空: ' + JSON.stringify(host));
    // IPv6 側は**繋がらなければ null** (AAAA が無い / 黒穴 / off)。T14.37
    if (c.keys.length > 4 && v6 !== null && !(typeof v6 === 'number' && v6 >= 0)) {
      fail('canary_ipv6_connect_ms が数でも null でもない: ' + JSON.stringify(v6));
    }
  }
  return c.samples.length;
}
// 実出力に canary があれば読む (無い版の作り置きでも落ちない)
const canaryRows = checkCanary(hist);
// 作り置きに canary が無い版でも検査そのものが動くことを、架空の 2 点で確かめる
// (IPv6 側は 1 点が `null` = 黒穴、1 点が数 = 生きている)
const fakeCanary = {
  canary: {
    keys: CANARY_KEYS,
    samples: [
      [1789251460, 3, 8, 'a.example.net:443', null],
      [1789251520, 4, 9, 'a.example.net:443', 12],
    ],
  },
};
if (checkCanary(fakeCanary) !== 2) fail('canary の 2 点が読めていない');
// T14.37 より前の 4 列の出力も今までどおり読める
if (
  checkCanary({
    canary: {
      keys: CANARY_KEYS.slice(0, 4),
      samples: [[1789251460, 3, 8, 'a.example.net:443']],
    },
  }) !== 1
) {
  fail('4 列 (T14.37 より前) の canary が読めない');
}
// canary が付いても既存の標本の読み方は変わらない (列は 1 つも動かない)
if (api.toSamples(Object.assign({}, hist, fakeCanary)).length !== samples.length) {
  fail('canary を足したら標本の数が変わった');
}

// 4. 個票 (`/errors` `/connections`) を読む関数 (T13.4)。実出力の作り置きは無いので、
// 形だけ同じ架空のデータで見る (ホスト名は架空、接続元はドキュメント用の範囲)
const errJson = {
  errors: [
    { at: 1789251465, kind: 'connect', target: 'a.example.net:443', cause: 'dns', dns_ms: 2013, connect_ms: 0, status: 502, client: '198.51.100.7' },
    { at: 1789251400, kind: 'forward', target: 'http://b.example.net:80', cause: 'refused', dns_ms: 0, connect_ms: 1, status: 502, client: '198.51.100.8' },
  ],
  count: 2, kept: 2, capacity: 500, recorded: 2, truncated: false,
};
const errRows = api.errorRows(errJson, 20);
if (errRows.length !== 2) fail('errorRows の件数が合わない: ' + errRows.length);
if (errRows[0].cause !== 'dns' || errRows[0].dns_ms !== 2013) fail('errorRows が読めていない');
if (errRows[0].at <= 0) fail('時刻が epoch 秒で読めていない');
if (api.errorRows(errJson, 1).length !== 1) fail('n で絞れていない');
if (api.errorRows({}, 20).length !== 0) fail('空でも例外なく 0 件のはず');
if (api.errorRows(null, 20).length !== 0) fail('null でも例外なく 0 件のはず');
// 古い出力 (キーが無い) でも落ちない
if (api.errorRows({ errors: [{}] }, 20)[0].cause !== '') fail('無いキーは空文字のはず');

const connJson = {
  connections: [
    { id: 3, client: '198.51.100.7', target: 'a.example.net:443', kind: 'connect', state: 'parked', age_secs: 120, bytes: 4096, fds: 2, rate_bps: 0 },
    { id: 9, client: '198.51.100.9', target: '', kind: 'http', state: 'serving', age_secs: 0, bytes: 0, fds: 1, rate_bps: 0 },
    { id: 5, client: '198.51.100.8', target: 'b.example.net:443', kind: 'connect', state: 'relaying', age_secs: 900, bytes: 1048576, fds: 2, rate_bps: 1048576 },
  ],
  count: 3, shown: 3, truncated: false, lite: false,
};
const conn = api.connRows(connJson, 50);
if (conn.rows.length !== 3) fail('connRows の件数が合わない');
// 長く居る順 (経過の降順)
for (let i = 1; i < conn.rows.length; i++) {
  if (conn.rows[i - 1].age_secs < conn.rows[i].age_secs) fail('connRows の並びが崩れた');
}
if (conn.rows[0].id !== 5) fail('いちばん長く居る接続が先頭でない: ' + conn.rows[0].id);
if (conn.kinds.connect !== 2 || conn.kinds.http !== 1) fail('種類の内訳が合わない');
if (conn.states.parked !== 1 || conn.states.relaying !== 1 || conn.states.serving !== 1) {
  fail('状態の内訳が合わない: ' + JSON.stringify(conn.states));
}
if (conn.bytes !== 4096 + 1048576) fail('転送の合計が合わない: ' + conn.bytes);
if (conn.count !== 3) fail('count が読めていない');
// 直近 5 秒の転送速度 (T14.39)。描くのは画面の仕事なので、ここは行に残ることだけ見る
if (conn.rows[0].rate_bps !== 1048576) fail('rate_bps が行に残っていない: ' + conn.rows[0].rate_bps);
if (api.connRows(connJson, 1).rows.length !== 1) fail('n で絞れていない');
const lite = api.connRows({ connections: [], count: 0, lite: true }, 50);
if (lite.rows.length !== 0 || !lite.lite) fail('lite の空一覧が読めていない');
if (api.connRows({}, 50).rows.length !== 0) fail('空でも例外なく 0 件のはず');
if (api.connRows(null, 50).count !== 0) fail('null でも例外なく 0 件のはず');

// 5. `/history` の `closed` (T14.6) と `/bursts` の写真を読む。
// **標本 (`samples`) とは別の配列**なので、ここは既存の読み方に 1 行も触らずに足せる。
// 実出力 (`hist.closed`) があればそれも、無くても下の見本で読み方を確かめる
// (作り置きの `/history` は T14.6 より前に取ったものなので `closed` を持っていない)。
function closedRows(closed) {
  if (!closed || !Array.isArray(closed.samples)) return [];
  const keys = closed.keys || [];
  return closed.samples.map((row) => {
    const o = {};
    keys.forEach((k, i) => {
      o[k] = row[i];
    });
    return o;
  });
}

function checkClosed(closed, where) {
  const rows = closedRows(closed);
  if (rows.length === 0) return 0;
  const reasons = closed.reasons || [];
  const life = closed.life_bounds_secs || [];
  const bytes = closed.byte_bounds || [];
  if (reasons.length !== 8) fail(where + ': 閉じた理由が 8 種でない: ' + reasons.length);
  if (life.length !== 12) fail(where + ': 寿命の区間が 12 段でない: ' + life.length);
  if (bytes.length !== 12) fail(where + ': バイトの区間が 12 段でない: ' + bytes.length);
  for (const r of rows) {
    if (!(r.t > 0)) fail(where + ': 窓の時刻が epoch 秒で読めていない: ' + r.t);
    if (!(r.closed > 0)) fail(where + ': 件数 0 の窓が出ている (残さないはず)');
    if (r.reasons.length !== reasons.length) fail(where + ': reasons の数が合わない');
    if (r.life.length !== life.length + 1) fail(where + ': life が区間 + 1 でない');
    if (r.up.length !== bytes.length + 1 || r.down.length !== bytes.length + 1) {
      fail(where + ': up / down が区間 + 1 でない');
    }
    const sum = (a) => a.reduce((x, y) => x + y, 0);
    for (const k of ['reasons', 'life', 'up', 'down']) {
      if (sum(r[k]) !== r.closed) {
        fail(where + ': ' + k + ' の合計 ' + sum(r[k]) + ' != 件数 ' + r.closed);
      }
    }
    if (!(r.life_secs_sum >= 0) || !(r.parked_secs_sum >= 0) || !(r.parks >= 0)) {
      fail(where + ': 合計が 0 以上の数で読めていない');
    }
    if (!(r.up_bytes >= 0) || !(r.down_bytes >= 0)) fail(where + ': バイトの合計が読めていない');
  }
  // 窓を 1 つに畳む (T14.8 が「直近 1 時間の内訳」を描くときの読み方)
  const merged = rows.reduce((a, r) => {
    a.closed += r.closed;
    r.reasons.forEach((n, i) => (a.reasons[i] += n));
    return a;
  }, { closed: 0, reasons: reasons.map(() => 0) });
  if (merged.reasons.reduce((x, y) => x + y, 0) !== merged.closed) {
    fail(where + ': 畳んだ件数が合わない');
  }
  return rows.length;
}

const closedSample = {
  interval_secs: 5,
  keys: ['t', 'closed', 'reasons', 'life', 'up', 'down', 'life_secs_sum', 'parked_secs_sum', 'parks', 'up_bytes', 'down_bytes'],
  reasons: ['client_eof', 'server_eof', 'idle_timeout', 'keepalive_timeout', 'evicted', 'limit', 'shutdown', 'error'],
  life_bounds_secs: [1, 2, 5, 10, 15, 30, 60, 120, 300, 900, 3600, 21600],
  byte_bounds: [1024, 4096, 16384, 65536, 262144, 1048576, 4194304, 16777216, 67108864, 268435456, 1073741824, 4294967296],
  samples: [
    [1789251460, 3, [2, 0, 0, 1, 0, 0, 0, 0], [0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0], [3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 1, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0], 317, 240, 4, 1200, 900000],
    [1789251465, 1, [0, 0, 1, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0], [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0], 300, 295, 1, 900, 40000],
  ],
  windows: 2,
  capacity: 720,
  recorded: 4,
};
const closedWindows = checkClosed(closedSample, '見本') + checkClosed(hist.closed, path.basename(file));
if (closedWindows < 2) fail('閉じた接続の分布を 1 窓も読めていない');
if (closedRows(null).length !== 0) fail('closed が無くても例外なく 0 件のはず');
if (closedRows({ samples: [] }).length !== 0) fail('空の窓でも 0 件のはず');

// `/bursts` の写真 (T14.6)。実出力の作り置きは無いので形だけ同じ架空のデータで見る
function burstRows(json, n) {
  const shots = (json && json.bursts) || [];
  return shots.slice(0, n).map((b) => ({
    at: b.at || 0,
    seq: b.seq || 0,
    active: b.active || 0,
    max_conns: b.max_conns || 0,
    threshold: b.threshold || 0,
    clients: b.clients || [],
    targets: b.targets || [],
    states: b.states || {},
    kinds: b.kinds || {},
    fds: b.fds || 0,
  }));
}

const burstJson = {
  bursts: [
    {
      at: 1789251465, seq: 2, active: 218, trigger_active: 217, max_conns: 240, threshold: 120,
      clients: [{ client: '198.51.100.7', conns: 210 }, { client: '203.0.113.9', conns: 8 }],
      clients_distinct: 2, clients_other: 0,
      targets: [{ target: 'mtalk.google.com:5228', conns: 120 }, { target: 'a.example.net:443', conns: 98 }],
      targets_distinct: 2, targets_other: 0,
      states: { serving: 2, reading: 1, parked: 200, queued: 0, relaying: 15 },
      kinds: { connect: 215, http: 3 },
      evicted_idle: 12, rejected_overload: 0, threads: 68, fds: 501, max_fds: 1024,
    },
  ],
  count: 1, shown: 1, kept: 1, capacity: 50, recorded: 1,
  threshold: 120, max_conns: 240, active: 3, armed: true, pending: false, truncated: false, lite: false,
};
const shots = burstRows(burstJson, 50);
if (shots.length !== 1) fail('burstRows の件数が合わない');
const shot = shots[0];
if (shot.active !== 218 || shot.threshold !== 120) fail('写真の本数と閾が読めていない');
const stateSum = Object.values(shot.states).reduce((a, b) => a + b, 0);
if (stateSum !== shot.active) fail('状態別の合計 ' + stateSum + ' != ' + shot.active);
const kindSum = Object.values(shot.kinds).reduce((a, b) => a + b, 0);
if (kindSum !== shot.active) fail('種類別の合計 ' + kindSum + ' != ' + shot.active);
const clientSum = shot.clients.reduce((a, c) => a + c.conns, 0);
if (clientSum > shot.active) fail('接続元の合計が本数を超えた');
if (shot.clients[0].conns < shot.clients[1].conns) fail('接続元が多い順でない');
if (shot.targets[0].conns < shot.targets[1].conns) fail('宛先が多い順でない');
if (burstRows(burstJson, 0).length !== 0) fail('n で絞れていない');
if (burstRows({}, 50).length !== 0) fail('空でも例外なく 0 件のはず');
if (burstRows(null, 50).length !== 0) fail('null でも例外なく 0 件のはず');
if (burstRows({ bursts: [{}] }, 50)[0].active !== 0) fail('無いキーは 0 のはず');

// 6. カーネルと cgroup の窓 (`/history` の `kernel` と `/status` の `kernel`。T14.12)。
//    実出力の作り置きは `kernel` より前の版なので、**あれば読む**形にしてある。
//    形だけ同じ架空のデータでも 1 回通す (読み方が壊れたらここで気づける)
function checkKernelHistory(k, where) {
  if (!k) return 0;
  if (!Array.isArray(k.keys) || !Array.isArray(k.samples)) fail(where + ' の kernel の形が違う');
  for (const want of ['t', 'time_wait', 'psi_cpu_some_avg10', 'listen_overflows']) {
    if (k.keys.indexOf(want) < 0) fail(where + ' の kernel に列 ' + want + ' が無い');
  }
  for (const row of k.samples) {
    if (!Array.isArray(row) || row.length !== k.keys.length) {
      fail(where + ' の kernel の列の数が keys と合わない: ' + JSON.stringify(row));
    }
    for (let i = 0; i < row.length; i++) {
      // 読めない源は null、読めた源は数 (文字列や undefined が混ざったら形が壊れている)
      if (row[i] !== null && typeof row[i] !== 'number') {
        fail(where + ' の kernel の ' + k.keys[i] + ' が数でも null でもない: ' + JSON.stringify(row[i]));
      }
    }
    if (row[0] === null || !(row[0] > 0)) fail(where + ' の kernel の t が時刻でない');
  }
  return k.samples.length;
}

const fakeKernel = {
  interval_secs: 5,
  keys: [
    't', 'listen_overflows', 'listen_drops', 'tcp_timeouts', 'syn_retrans', 'abort_on_timeout',
    'retrans_segs', 'curr_estab', 'sockets_inuse', 'time_wait', 'sockets_alloc', 'sockets_mem',
    'cpu_nr_throttled', 'cpu_throttled_usec',
    'psi_cpu_some_avg10', 'psi_cpu_full_avg10', 'psi_mem_some_avg10', 'psi_mem_full_avg10',
    'psi_io_some_avg10', 'psi_io_full_avg10', 'dns_misses', 'dns_miss_ms', 'state_file_errors',
  ],
  samples: [
    // 読めた環境 (Linux + cgroup v2 + PSI)
    [1789251465, 0, 0, 2, 1, 0, 3, 16, 43, 30912, 50, 0, 0, 0, 43.02, 2.99, 0, 0, 0.13, 0.13, 2, 13, 0],
    // 読めない環境 (cgroup v1 / PSI 無し / `/proc/net` の無いコンテナ)
    [1789251470, null, null, null, null, null, null, null, null, null, null, null, null, null,
      null, null, null, null, null, null, 0, null, null],
  ],
};
if (checkKernelHistory(fakeKernel, '作り置き') !== 2) fail('作り置きの kernel を読めていない');
// 実出力にあれば読む (`res=3600` は `null` = この解像度には窓が無い、も正しい形)
const kernelRows = checkKernelHistory(hist.kernel, path.basename(file));
// `/status` の `kernel` の節 (最新の値と累計)。無い版の出力でも落ちない
if (st.kernel) {
  if (typeof st.kernel.at !== 'number') fail('/status の kernel.at が数でない');
  if (!st.kernel.tcp) fail('/status の kernel に tcp が無い');
  for (const k of ['listen_overflows', 'time_wait']) {
    const v = st.kernel.tcp[k];
    if (v !== null && typeof v !== 'number') fail('/status の kernel.tcp.' + k + ' が数でも null でもない');
  }
  if (!st.kernel.last_5m) fail('/status の kernel に last_5m が無い');
}

// 7. `/events` の時系列 (T14.11)。12 種で固定なので、綴りが増減したらここで気づく
// (`anomaly` は異常の自動検知が書く 1 件 (T14.23)、`new_client` は初めて見た接続元 (T14.54))
const EVENT_KINDS = [
  'start', 'reload', 'blocklist', 'ipv6', 'pressure', 'ballast',
  'state_file', 'evict', 'emfile', 'shutdown', 'anomaly', 'new_client',
];

function eventRows(json, since) {
  const events = (json && json.events) || [];
  return events
    .filter((e) => (e.at || 0) >= (since || 0))
    .map((e) => ({ at: e.at || 0, kind: e.kind || '', text: e.text || '' }));
}

const eventJson = {
  events: [
    { at: 1789251470, kind: 'reload', text: 'PROXY_TIMEOUT_SECS 30 \u2192 10' },
    { at: 1789251468, kind: 'ballast', text: 'ballast +256 MiB -> 256 MiB (memory 0 MiB, disk 256 MiB)' },
    { at: 1789251465, kind: 'start', text: 'version 0.1.0+abcdef1 on port 8080 (profile default, cache on, timeout 30s, max conns 240)' },
    { at: 1789251460, kind: 'anomaly', text: 'connect_p95: connect p95 138 ms over 5m is 14.1x the 1h baseline 9.8 ms (120 of 1328 connects)' },
  ],
  count: 4, kept: 4, capacity: 512, recorded: 4, since: 0, kinds: EVENT_KINDS, truncated: false,
};
const evs = eventRows(eventJson, 0);
if (evs.length !== 4) fail('eventRows の件数が合わない');
if (evs.some((e, i) => i > 0 && evs[i - 1].at < e.at)) fail('出来事が新しい順でない');
if (!evs.every((e) => EVENT_KINDS.includes(e.kind))) fail('知らない種類がある');
if (eventJson.kinds.length !== 12) fail('種類は 12 種で固定のはず');
if (eventJson.kinds.join(',') !== EVENT_KINDS.join(',')) fail('種類の綴りか並びが変わった');
if (evs.some((e) => e.text.length > 128)) fail('説明が 128 バイトを超えた');
if (eventRows(eventJson, 1789251468).length !== 2) fail('since で絞れていない');
if (eventRows({}, 0).length !== 0) fail('空でも例外なく 0 件のはず');
if (eventRows(null, 0).length !== 0) fail('null でも例外なく 0 件のはず');
if (eventRows({ events: [{}] }, 0)[0].kind !== '') fail('無いキーは空のはず');

// 8. `/profile` を読む関数 (段階・スレッド・ロック。T14.3)
const profFile = process.argv[4] || path.join(__dirname, 'testdata', 'profile-res5.json');
const pj = JSON.parse(fs.readFileSync(profFile, 'utf8'));
const prof = api.toProfile(pj);
if (prof.off) fail(profFile + ' が lite の出力 (/profile が off)');
if (prof.samples.length !== pj.samples.length) fail('/profile の標本の数が合わない');
if (prof.samples.length === 0) fail('/profile の標本が 0 件 (5 秒以上動かしたプロキシの出力を渡すこと)');
if (prof.names.connect.length !== 7 || prof.names.forward.length !== 6) {
  fail('段階の数が CONNECT 7 / forward 6 になっていない');
}
if (prof.bounds.length !== hist.bounds_ms.length) fail('/profile の区間が /history と違う');
for (const s of prof.samples) {
  if (s.connect.length !== prof.names.connect.length) fail('CONNECT の段階の数が合わない');
  if (s.forward.length !== prof.names.forward.length) fail('forward の段階の数が合わない');
  if (s.threads.length !== prof.roles.length) fail('役割の数が合わない');
  if (s.locks.length !== prof.lockNames.length) fail('ロックの数が合わない');
  if (s.queue.length !== 3) fail('待ち行列は (件数, 合計 ms, 最大 ms) の 3 つ');
  for (const w of s.connect.concat(s.forward)) {
    if (w.count && (!w.buckets || w.buckets.length !== prof.bounds.length + 1)) {
      fail('段階の区間が bounds_ms + 1 (上限なし) になっていない');
    }
    if (w.count && w.sum / w.count > w.max) fail('段階の平均が最大を超えた');
  }
  for (const t of s.threads) {
    if (t.samples && (!t.states || t.states.length !== prof.states.length)) {
      fail('状態の枠が states と同じ数で読めていない');
    }
  }
}
// 段階の合計は、生の JSON を自分で足したものと一致すること
const n5 = Math.max(1, Math.round(300 / prof.interval));
let checkedStages = 0;
for (const kind of ['connect', 'forward']) {
  const rows = api.stageRows(prof, n5, kind);
  const idx = kind === 'connect' ? 3 : 4;
  const from = Math.max(0, pj.samples.length - n5);
  for (let j = 0; j < rows.rows.length; j++) {
    let count = 0, sum = 0;
    for (let k = from; k < pj.samples.length; k++) {
      const w = pj.samples[k][idx][j];
      if (w) { count += w[0]; sum += w[1]; }
    }
    if (rows.rows[j].count !== count) fail(kind + ' の ' + rows.rows[j].name + ' の件数 ' + rows.rows[j].count + ' != ' + count);
    if (rows.rows[j].sum !== sum) fail(kind + ' の ' + rows.rows[j].name + ' の合計が合わない');
    if (count) checkedStages++;
  }
  // 割合の合計は 100% (どの段階も観測されていなければ 0)
  const share = rows.rows.reduce((a, r) => a + r.share, 0);
  if (rows.total > 0 && Math.abs(share - 100) > 0.01) fail(kind + ' の割合の合計が 100% でない: ' + share);
  // 「確立まで」と「その後」に分けて積む (段階の合計は利用者が待った時間ではない)
  if (Math.abs(rows.setupMs + rows.afterMs - rows.total) > 1e-9) fail(kind + ' の切れ目が合わない');
  if (rows.setup.length !== 5) fail(kind + ' の「確立まで」は 5 段のはず');
}
if (checkedStages === 0) fail('段階が 1 つも観測されていない出力 (ベンチを流したプロキシの出力を渡すこと)');
// 先頭の 1 行 (いちばん長い段階) は降順で 3 つまで
const lead = api.longestStage(prof, n5, 'connect');
if (!lead) fail('CONNECT の「いちばん長い段階」が出ない');
if (lead.top.length > 3) fail('上位 3 つを超えた');
for (let i = 1; i < lead.top.length; i++) {
  if (lead.top[i - 1].avg < lead.top[i].avg) fail('いちばん長い段階の並びが降順でない');
}
// 役割: CPU の多い順、状態の割合は 100% 以下
const roles = api.roleRows(prof, n5);
if (!roles.length) fail('役割が 1 つも出ない');
for (let i = 1; i < roles.length; i++) {
  if (roles[i - 1].cpu_us < roles[i].cpu_us) fail('役割が CPU の多い順になっていない');
}
for (const r of roles) {
  if (!(r.cpu_pct >= 0)) fail(r.role + ' の CPU % が数でない');
  const pct = r.top.reduce((a, x) => a + x.pct, 0);
  if (pct > 100.01) fail(r.role + ' の状態の割合が 100% を超えた: ' + pct);
  for (const x of r.top) {
    if (!prof.states.includes(x.name)) fail('知らない状態の名前: ' + x.name);
  }
}
if (!roles.some((r) => r.role === 'conn')) fail('conn 役が出ていない (ベンチを流した出力のはず)');
// ロック: 窓の増分は通算以下
const lk = api.lockRows(prof, n5);
if (lk.rows.length !== prof.lockNames.length) fail('ロックの行が足りない');
for (const r of lk.rows) {
  if (r.window > r.total) fail(r.name + ' の窓 ' + r.window + ' が通算 ' + r.total + ' を超えた');
}
if (lk.queue.waited > lk.queue.total.waited) fail('待ち行列の窓が通算を超えた');
// lite と壊れた入力でも例外を出さない
if (!api.toProfile({ profile: 'off' }).off) fail('lite の出力を off と読めていない');
if (api.toProfile({}).samples.length !== 0) fail('空でも 0 件のはず');
if (api.toProfile(null).samples.length !== 0) fail('null でも 0 件のはず');
if (api.stageRows(api.toProfile(null), 60, 'connect').rows.length !== 0) fail('空でも例外なし');
if (api.longestStage(api.toProfile(null), 60, 'connect') !== null) fail('空なら null のはず');
if (api.roleRows(api.toProfile(null), 60).length !== 0) fail('空でも 0 件のはず');
if (api.lockRows(api.toProfile(null), 60).rows.length !== 0) fail('空でも 0 件のはず');

const connLead = api.longestStage(prof, n5, 'connect');
const fwdLead = api.longestStage(prof, n5, 'forward');

// 9. `/history` の `transfer` (T14.25)。`closed` と同じく**標本とは別の配列**なので、
// ここも既存の読み方に 1 行も触らずに足せる。作り置きの `/history` は T14.25 より前に
// 取ったものなので、実出力にあれば読み、無ければ見本だけで読み方を確かめる。
function transferRows(transfer) {
  if (!transfer || !Array.isArray(transfer.samples)) return [];
  const keys = transfer.keys || [];
  return transfer.samples.map((row) => {
    const o = {};
    keys.forEach((k, i) => {
      o[k] = row[i];
    });
    return o;
  });
}

function checkTransfer(transfer, where) {
  const rows = transferRows(transfer);
  if (rows.length === 0) return 0;
  const speedBounds = transfer.speed_bounds_bps || [];
  const halfBounds = transfer.half_close_bounds_ms || [];
  if (speedBounds.length !== 12) fail(where + ': 速さの区間が 12 段でない: ' + speedBounds.length);
  if (halfBounds.length !== 12) fail(where + ': 半閉じの区間が 12 段でない: ' + halfBounds.length);
  // 等比 (4 倍ずつ) であること。境目が動いたら分布の読み方が変わるのでここで気づく
  for (const b of [speedBounds, halfBounds]) {
    for (let i = 1; i < b.length; i += 1) {
      if (b[i] !== b[i - 1] * 4) fail(where + ': 区間が 4 倍ずつでない: ' + b.join(','));
    }
  }
  if (transfer.min_bytes !== 1024) fail(where + ': 速さを数える下限が 1 KiB でない');
  const sum = (a) => a.reduce((x, y) => x + y, 0);
  for (const r of rows) {
    if (!(r.t > 0)) fail(where + ': 窓の時刻が epoch 秒で読めていない: ' + r.t);
    if (!(r.tunnels > 0)) fail(where + ': 本数 0 の窓が出ている (残さないはず)');
    if (r.speed.length !== speedBounds.length + 1) fail(where + ': speed が区間 + 1 でない');
    if (r.half_close.length !== halfBounds.length + 1) fail(where + ': half_close が区間 + 1 でない');
    if (sum(r.speed) !== r.speed_n) fail(where + ': speed の合計 ' + sum(r.speed) + ' != ' + r.speed_n);
    if (sum(r.half_close) !== r.half_close_n) fail(where + ': half_close の合計が件数と合わない');
    // 速さを数えるのは 1 KiB 以上運んだトンネルだけなので、本数より多くはならない
    if (r.speed_n > r.tunnels) fail(where + ': 速さの件数が本数を超えた');
    if (r.half_close_n > r.tunnels) fail(where + ': 半閉じの件数が本数を超えた');
    if (r.speed_n > 0 && !(r.bytes_sum > 0)) fail(where + ': 運んだバイトの合計が読めていない');
    if (!(r.relay_ms_sum >= 0) || !(r.half_close_ms_sum >= 0)) fail(where + ': 合計が数で読めていない');
    // 中継の詰まりの向き (T14.42)。古い `/history` には無いので「あれば数」だけ見る
    for (const k of ['stall_client_ms_sum', 'stall_origin_ms_sum']) {
      if (r[k] !== undefined && !(r[k] >= 0)) fail(where + ': ' + k + ' が数で読めていない: ' + r[k]);
    }
  }
  // 窓を 1 つに畳む (T14.8 が「直近 1 時間の速さの分布」を描くときの読み方)
  const merged = rows.reduce((a, r) => {
    a.speed_n += r.speed_n;
    r.speed.forEach((n, i) => (a.speed[i] += n));
    return a;
  }, { speed_n: 0, speed: rows[0].speed.map(() => 0) });
  if (sum(merged.speed) !== merged.speed_n) fail(where + ': 畳んだ速さの件数が合わない');
  return rows.length;
}

const transferSample = {
  interval_secs: 5,
  keys: ['t', 'tunnels', 'speed_n', 'speed', 'half_close_n', 'half_close', 'bytes_sum', 'relay_ms_sum', 'half_close_ms_sum', 'stall_client_ms_sum', 'stall_origin_ms_sum'],
  speed_bounds_bps: [1024, 4096, 16384, 65536, 262144, 1048576, 4194304, 16777216, 67108864, 268435456, 1073741824, 4294967296],
  half_close_bounds_ms: [1, 4, 16, 64, 256, 1024, 4096, 16384, 65536, 262144, 1048576, 4194304],
  min_bytes: 1024,
  samples: [
    [1789251460, 3, 2, [0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0], 1, [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0], 1258291, 4200, 150, 1800, 0],
    [1789251465, 1, 1, [0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0], 0, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 20971520, 2000, 0, 0, 0],
  ],
  windows: 2,
  capacity: 720,
  recorded: 4,
};
const transferWindows =
  checkTransfer(transferSample, '見本') + checkTransfer(hist.transfer, path.basename(file));
if (transferWindows < 2) fail('速さと半閉じの分布を 1 窓も読めていない');
if (transferRows(null).length !== 0) fail('transfer が無くても例外なく 0 件のはず');
if (transferRows({ samples: [] }).length !== 0) fail('空の窓でも 0 件のはず');

// 10. 「調査」ページ (`crates/endpoints/src/web/inspect.html`。T14.8) の描画関数。
//    `/dashboard` と同じ作法で **DOM に触らない 9 つ**を名前で抜き出し、
//    (1) 手元のベンチで取った `/snapshot` の実出力、(2) デプロイ先の雪像 (`/status` と
//    `/history`)、(3) まだ実出力に無い欄の架空の見本、の 3 つで通す。
const insHtml = fs.readFileSync(
  path.join(__dirname, '..', 'crates/endpoints/src/web/inspect.html'),
  'utf8'
);
// T14.8 は 64 KiB だったが、T14.44 で「今日」「今週」「出来事と異常」の 3 枚を足したので
// **96 KiB** に上げた (理由は README。外部ライブラリ無しの 1 枚のままであることは変えない)
if (Buffer.byteLength(insHtml) > 96 * 1024) {
  fail('inspect.html が 96 KiB を超えた: ' + Buffer.byteLength(insHtml) + ' B');
}
const insM = insHtml.match(/<script>([\s\S]*?)<\/script>/);
if (!insM) fail('inspect.html に <script> が無い');
const insJs = insM[1];
try {
  new Function(insJs);
} catch (e) {
  fail('inspect.html の JS の構文エラー: ' + e.message);
}
// 色と段階の表は関数の外にあるので、切り出した関数と一緒に渡す (中身は見ない)
const insPrelude = ['REASON_COLORS', 'EVENT_COLORS', 'STAGE_COLORS', 'SLOW_STAGES']
  .map((v) => {
    const at = insJs.indexOf('var ' + v + '=');
    if (at < 0) fail(v + ' が inspect.html に無い');
    const end = insJs.indexOf(';\n', at);
    return insJs.slice(at, end + 1) + '\n';
  })
  .join('');
const ins = pick(
  insJs,
  ['num', 'reasonKey', 'reasonColor', 'weightOf', 'toSamples', 'winQuantile',
    'timeline', 'eventMarks', 'slowRows', 'burstCards', 'clientRows', 'rttScatter', 'sinceStart',
    'seriesLines'],
  'inspect.html',
  insPrelude
);

// `/snapshot` の実出力 (無ければ作り置き)。中身は手元のベンチで作ったものだけ
// (デプロイ先の雪像は**個人の閲覧先**が入るのでリポジトリには置かない)
const snapFile = process.argv[5] || path.join(__dirname, 'testdata', 'snapshot-local.json');
const snap = JSON.parse(fs.readFileSync(snapFile, 'utf8'));
const snapAt = +snap.taken_at || 0;
if (!snapAt) fail(snapFile + ' に taken_at が無い (/snapshot の出力を渡すこと)');

// (a) タイムライン: 窓の内側だけを 0〜1 の比にして、行ごとに束ねる
function checkTimeline(recent, where, opts) {
  const tl = ins.timeline(recent, Object.assign({ now: snapAt, span: 86400 }, opts || {}));
  if (tl.shown + tl.hidden !== tl.count) {
    fail(where + ': 描いた数 + 外した数 != 取れた数 (' + tl.shown + '+' + tl.hidden + '!=' + tl.count + ')');
  }
  let conns = 0;
  for (let i = 0; i < tl.lanes.length; i++) {
    const lane = tl.lanes[i];
    conns += lane.conns;
    if (lane.rows.length !== lane.conns) fail(where + ': 行の本数が合わない: ' + lane.name);
    if (i && tl.lanes[i - 1].conns < lane.conns) fail(where + ': 行が多い順になっていない');
    for (const r of lane.rows) {
      if (!(r.x0 >= 0 && r.x0 <= 1 && r.x1 >= 0 && r.x1 <= 1 && r.x1 >= r.x0)) {
        fail(where + ': 線の座標が 0〜1 に収まっていない: ' + JSON.stringify([r.x0, r.x1]));
      }
      if (!(r.at >= tl.t0 - 1) && !(r.end >= tl.t0)) fail(where + ': 窓の外の接続が入った');
      if (!(r.weight >= 1 && r.weight <= 5)) fail(where + ': 太さが 1〜5 でない: ' + r.weight);
      if (typeof r.color !== 'string' || r.color[0] !== '#') fail(where + ': 色が読めない: ' + r.color);
      if (r.end - r.at !== r.secs) fail(where + ': 寿命と時刻が合わない');
    }
  }
  if (conns + tl.others.conns !== tl.shown) {
    fail(where + ': 行に束ねた本数 ' + conns + ' + 他 ' + tl.others.conns + ' != ' + tl.shown);
  }
  const tally = tl.reasons.reduce((a, r) => a + r.count, 0);
  if (tally !== tl.shown) fail(where + ': 理由の内訳 ' + tally + ' != ' + tl.shown);
  for (let i = 1; i < tl.reasons.length; i++) {
    if (tl.reasons[i - 1].count < tl.reasons[i].count) fail(where + ': 理由が多い順でない');
  }
  return tl;
}

const tlClient = checkTimeline(snap.recent, path.basename(snapFile) + ' の /recent');
const tlTarget = checkTimeline(snap.recent, '宛先で束ねた /recent', { by: 'target' });
if (tlClient.shown !== tlTarget.shown) fail('縦軸を変えたら描いた本数が変わった');
if (tlTarget.by !== 'target') fail('by が読めていない');
// 窓を狭めれば描く本数は減る (増えることはない)
if (ins.timeline(snap.recent, { now: snapAt, span: 60 }).shown > tlClient.shown) {
  fail('窓を狭めたのに本数が増えた');
}
// 空・壊れた入力・知らない綴りでも例外を出さない
if (ins.timeline(null, {}).shown !== 0) fail('null でも 0 本のはず');
if (ins.timeline({}, {}).shown !== 0) fail('空でも 0 本のはず');
if (ins.timeline({ recent: [{}] }, { now: snapAt, span: 3600 }).hidden !== 1) {
  fail('時刻の無い行は外すはず');
}
const odd = ins.timeline(
  { recent: [{ id: 1, at: snapAt - 10, secs: 5, client: '198.51.100.7', reason: 'error:brand_new' }] },
  { now: snapAt, span: 3600 }
);
if (odd.shown !== 1) fail('知らない原因の綴りで落ちた');
if (odd.reasons[0].name !== 'error') fail('error:<原因> は error に畳むはず: ' + odd.reasons[0].name);
if (ins.reasonColor('nonesuch') !== '#8b91a5') fail('知らない理由は既定の色のはず');

// 出来事の印 (T14.11)。**種類が増えても壊れない**ことをここで見る (T14.23 の `anomaly`)
const markJson = {
  events: EVENT_KINDS.concat(['anomaly', 'brand_new_kind']).map((k, i) => ({
    at: snapAt - 60 + i,
    kind: k,
    text: k + ' の説明',
  })),
};
const mk = ins.eventMarks(markJson, tlClient);
if (mk.length !== markJson.events.length) fail('印の数が合わない: ' + mk.length);
for (let i = 1; i < mk.length; i++) if (mk[i - 1].at > mk[i].at) fail('印が古い順になっていない');
for (const m of mk) {
  if (!(m.x >= 0 && m.x <= 1)) fail('印の位置が 0〜1 でない: ' + m.x);
  if (typeof m.color !== 'string' || m.color[0] !== '#') fail('印の色が読めない: ' + m.kind);
}
if (mk[mk.length - 1].known !== false) fail('知らない種類は known:false のはず');
if (!mk.find((m) => m.kind === 'anomaly')) fail('anomaly (T14.23) が落ちた');
if (ins.eventMarks(null, tlClient).length !== 0) fail('events が無くても 0 件のはず');
if (ins.eventMarks(eventJson, { t0: 0, t1: 0 }).length !== 0) fail('窓の外は 0 件のはず');
if (snap.events && ins.eventMarks(snap.events, tlClient).length > (snap.events.events || []).length) {
  fail('実出力の印が件数を超えた');
}

// (b) 遅い接続: 段階の積み上げ (T14.3 の 5 つ)
function checkSlow(recent, where) {
  const rows = ins.slowRows(recent, 50);
  for (const r of rows) {
    if (r.stages.length !== 5) fail(where + ': 段階が 5 つでない: ' + r.stages.length);
    const sum = r.stages.reduce((a, s) => a + s.ms, 0);
    if (sum !== r.total) fail(where + ': 段階の合計 ' + sum + ' != total ' + r.total);
    const pct = r.stages.reduce((a, s) => a + s.pct, 0);
    if (r.total > 0 && Math.abs(pct - 100) > 0.01) fail(where + ': 割合の合計が 100% でない: ' + pct);
    if (r.total === 0 && pct !== 0) fail(where + ': 段階 0 なのに割合が付いた');
    if (r.stages[3].name !== 'connect' || r.stages[3].ms !== r.connect_ms) {
      fail(where + ': connect の段階が読めていない');
    }
    if (r.rtt_origin == null && r.unexplained !== null) fail(where + ': RTT が無いのに差が出た');
    if (r.rtt_origin != null && !(r.unexplained >= 0)) fail(where + ': 差が 0 以上でない');
    if (r.bytes < 0) fail(where + ': バイトが負');
  }
  return rows;
}
const slow = checkSlow(snap.recent, path.basename(snapFile));
if (ins.slowRows(snap.recent, 3).length > 3) fail('n で絞れていない');
if (ins.slowRows(null, 50).length !== 0) fail('null でも 0 件のはず');
if (ins.slowRows({ recent: [{}] }, 50)[0].total !== 0) fail('無いキーは 0 のはず');
// 5 段とも入った 1 本 (実出力では 0 の段階が JSON に出ない = 上の行では見られない)
const fullStages = ins.slowRows(
  {
    recent: [
      {
        id: 7, at: snapAt - 5, client: '198.51.100.7', target: 'a.example.net:443', kind: 'connect',
        secs: 12, up: 1200, down: 3400, reason: 'client_eof', status: 0,
        ms: { dns: 13, connect: 257, first_relay: 4, queue: 2, client_read: 1 },
        rtt_ms: { client: 48.3, origin: 30.1 }, retrans: { client: 0, origin: 2 },
      },
    ],
  },
  50
)[0];
if (fullStages.total !== 13 + 257 + 4 + 2 + 1) fail('段階の合計が合わない: ' + fullStages.total);
if (fullStages.stages.map((s) => s.name).join(',') !== 'queue,client_read,dns,connect,first_relay') {
  fail('段階の並びが T14.3 の 5 つでない');
}
if (Math.round(fullStages.unexplained) !== 227) fail('確立 − RTT が 227 ms でない: ' + fullStages.unexplained);
if (fullStages.retrans !== 2) fail('再送が読めていない');

// (c) 山の写真 (T14.6)
function checkShots(bursts, where) {
  const cards = ins.burstCards(bursts, 50);
  for (const b of cards) {
    if (b.active && b.state_sum !== b.active) fail(where + ': 状態の合計 ' + b.state_sum + ' != ' + b.active);
    if (b.active && b.kind_sum !== b.active) fail(where + ': 種類の合計 ' + b.kind_sum + ' != ' + b.active);
    const cs = b.clients.reduce((a, c) => a + c.conns, 0) + b.clients_other;
    if (cs > b.active) fail(where + ': 接続元の合計が本数を超えた');
    for (let i = 1; i < b.clients.length; i++) {
      if (b.clients[i - 1].conns < b.clients[i].conns) fail(where + ': 接続元が多い順でない');
    }
    for (const s of b.states) if (!(s.pct >= 0 && s.pct <= 100.01)) fail(where + ': 状態の割合が範囲外');
    if (b.max_conns && Math.abs(b.pct - (b.active / b.max_conns) * 100) > 0.01) {
      fail(where + ': 上限に対する割合が合わない');
    }
  }
  return cards;
}
const shotCards = checkShots(snap.bursts, path.basename(snapFile));
checkShots(burstJson, '見本の写真');
if (ins.burstCards(null, 50).length !== 0) fail('null でも 0 枚のはず');
if (ins.burstCards({ bursts: [{}] }, 50)[0].active !== 0) fail('無いキーは 0 のはず');
if (ins.burstCards({ bursts: [{}, {}, {}] }, 2).length !== 2) fail('n で絞れていない');

// (d) 接続元 (T14.7)。RTT は標本が無ければ null (T14.5)
function checkClients(clients, where) {
  const rows = ins.clientRows(clients, 200, 'requests');
  for (let i = 1; i < rows.length; i++) {
    if (rows[i - 1].requests < rows[i].requests) fail(where + ': 要求数の多い順でない');
  }
  for (const c of rows) {
    if (typeof c.client !== 'string') fail(where + ': client が読めていない');
    if (!Array.isArray(c.agents)) fail(where + ': agents が配列でない');
    if (!Array.isArray(c.ports)) fail(where + ': ports が配列でない');
    if (c.rtt !== null && !(c.rtt.samples > 0 && c.rtt.avg >= 0)) fail(where + ': RTT の形が違う');
    // T14.26 の上り / 下り (無い版では 0 で、割合は null)
    if (c.bytes_in + c.bytes_out > 0 && c.down_pct === null) fail(where + ': 下りの割合が出ていない');
    if (c.down_pct !== null && !(c.down_pct >= 0 && c.down_pct <= 100.01)) {
      fail(where + ': 下りの割合が範囲外: ' + c.down_pct);
    }
  }
  return rows;
}
const clientTbl = checkClients(snap.clients, path.basename(snapFile));
if (snap.status && snap.status.clients) checkClients(snap.status, '/status の clients[]');
if (ins.clientRows(null, 50).length !== 0) fail('null でも 0 件のはず');
if (ins.clientRows({ clients: [{}] }, 50)[0].requests !== 0) fail('無いキーは 0 のはず');
const rttClients = {
  clients: [
    { client: '198.51.100.7', requests: 10, bytes: 100, rtt_ms: { avg: 48.3, min: 40, samples: 3 }, retrans: 1 },
    { client: '198.51.100.8', requests: 20, bytes: 200, rtt_ms: null, retrans: 0 },
    { client: '198.51.100.9', requests: 5, bytes: 50, rtt_ms: { avg: 0, min: 0, samples: 0 } },
  ],
};
const rttRows = ins.clientRows(rttClients, 50, 'requests');
if (rttRows[0].client !== '198.51.100.8' || rttRows[0].rtt !== null) fail('rtt_ms:null を読めていない');
if (rttRows[1].rtt.avg !== 48.3 || rttRows[1].rtt.samples !== 3) fail('rtt_ms を読めていない');
if (rttRows[2].rtt !== null) fail('標本 0 は描かない (null) はず');
if (ins.clientRows(rttClients, 50, 'bytes')[0].bytes !== 200) fail('転送量で並べ替えられていない');
const dirRows = ins.clientRows(
  { clients: [{ client: '198.51.100.7', requests: 1, bytes: 1000, bytes_in: 200, bytes_out: 800 }] },
  50
);
if (dirRows[0].down_pct !== 80) fail('下りの割合 (T14.26) が読めていない: ' + dirRows[0].down_pct);
if (ins.clientRows({ clients: [{ client: 'x', bytes: 10 }] }, 50)[0].down_pct !== null) {
  fail('bytes_in / bytes_out の無い版は null のはず');
}
if (ins.clientRows(rttClients, 1, 'requests').length !== 1) fail('n で絞れていない');

// (e) RTT の散布 (T14.5)。`rtt_ms` が null / 標本 0 / 接続を測っていない行は描かない
function checkScatter(hosts, where) {
  const sc = ins.rttScatter(hosts, { max: 200 });
  const rows = (hosts && hosts.hosts) || [];
  if (sc.count + sc.skipped !== rows.length) {
    fail(where + ': 描いた点 + 外した行 != ホスト数 (' + sc.count + '+' + sc.skipped + '!=' + rows.length + ')');
  }
  for (let i = 1; i < sc.points.length; i++) {
    if (sc.points[i - 1].gap < sc.points[i].gap) fail(where + ': 差の大きい順でない');
  }
  for (const p of sc.points) {
    // 接続の平均は 0 ms (loopback) がありうるが、RTT が 0 の行は描かない
    if (!(p.rtt > 0) || !(p.connect >= 0)) fail(where + ': 描けない点が入った');
    if (Math.abs(p.gap - (p.connect - p.rtt)) > 1e-9) fail(where + ': 差が 接続 − RTT でない');
    if (Math.abs(p.ratio - p.connect / p.rtt) > 1e-9) fail(where + ': 倍が 接続 ÷ RTT でない');
    if (p.rtt > sc.maxX || p.connect > sc.maxY) fail(where + ': 最大値が点を含んでいない');
  }
  return sc;
}
const sc = checkScatter(snap.hosts, path.basename(snapFile));
checkScatter(st, 'デプロイ先の /status の hosts[]');
if (ins.rttScatter(null, {}).count !== 0) fail('null でも 0 点のはず');
if (ins.rttScatter({ hosts: [{}] }, {}).skipped !== 1) fail('空の行は外すはず');
const scFake = ins.rttScatter(
  {
    hosts: [
      { host: 'a.example.net:443', requests: 100, timed: 100, connect_ms_sum: 25700, rtt_ms: { avg: 30.1, min: 29, samples: 50 }, retrans: 3 },
      { host: 'b.example.net:443', requests: 50, timed: 50, connect_ms_sum: 1600, rtt_ms: { avg: 30, min: 30, samples: 10 } },
      { host: 'c.example.net:443', requests: 10, timed: 10, connect_ms_sum: 100, rtt_ms: null },
      { host: 'd.example.net:443', requests: 10, timed: 0, connect_ms_sum: 0, rtt_ms: { avg: 5, min: 5, samples: 2 } },
      { host: 'e.example.net:443', requests: 10, timed: 10, connect_ms_sum: 100, rtt_ms: { avg: 0, min: 0, samples: 0 } },
      // 接続の平均が 0 ms (loopback) の行は**描く** (捨てると手元の出力が 1 点も残らない)
      { host: 'f.example.net:443', requests: 10, timed: 10, connect_ms_sum: 0, rtt_ms: { avg: 0.1, min: 0.05, samples: 7 } },
    ],
  },
  {}
);
if (scFake.count !== 3 || scFake.skipped !== 3) fail('描く行の選び方が違う: ' + JSON.stringify([scFake.count, scFake.skipped]));
if (scFake.points[2].host !== 'f.example.net:443' || scFake.points[2].connect !== 0) {
  fail('接続 0 ms の行が最後に来ていない');
}
if (scFake.points[0].host !== 'a.example.net:443') fail('対角線から遠い順でない');
if (Math.round(scFake.points[0].connect) !== 257) fail('接続の平均が connect_ms_sum ÷ timed でない');
if (Math.round(scFake.points[0].gap) !== 227) fail('差が 227 ms でない: ' + scFake.points[0].gap);

// (f) 起動からの窓 (T14.24 の要約があればそれ、無ければ手元で切る)
const snapHist = (snap.history && (snap.history['5'] || snap.history[5])) || hist;
const upt = (snap.status && +snap.status.since_start_secs) || 0;
const cut = ins.sinceStart(snapHist, upt, snapAt);
if (cut.mode !== 'samples') fail('/history の標本を切る側にならなかった');
const allRows = ins.sinceStart(snapHist, 0, snapAt);
if (cut.rows.length + cut.cut !== allRows.rows.length) fail('切った数が合わない');
if (cut.rows.some((s) => s.t < cut.from)) fail('起動より前の標本が残った');
let handConnects = 0;
for (const s of cut.rows) handConnects += s.connects || 0;
if (cut.connects !== handConnects) fail('CONNECT の本数が手で足した値と違う');
if (cut.connects && !(cut.p50 <= cut.p95 + 1e-9)) fail('p50 > p95');
if (cut.connects && !(cut.p95 <= cut.max)) fail('p95 が窓の最大値を超えた');
if (cut.connects && !(cut.avg <= cut.max)) fail('平均が最大値を超えた');
if (cut.dns_misses && !(cut.dns_avg >= 0)) fail('ミス 1 回の平均が読めていない');
// デプロイ先の雪像 (`/history` + `/status`) でも同じ読み方が通ること
const deployedNow = samples.length ? samples[samples.length - 1].t : 0;
const deployed = ins.sinceStart(hist, +st.since_start_secs || 0, deployedNow);
if (deployed.mode !== 'samples') fail('デプロイ先の /history を標本として読めていない');
if (deployed.rows.length > samples.length) fail('切ったのに標本が増えた');
if (deployed.connects > total + 0) {
  // `total` は直近 60 標本ぶんなので、起動からの窓はそれ以上になりうる (ここでは形だけ見る)
}
if (ins.sinceStart(null, 0, 0).samples !== 0) fail('null でも 0 標本のはず');
if (ins.sinceStart({}, 100, 200).rows.length !== 0) fail('空でも 0 件のはず');
// T14.24 の `?summary=1` の応答 (サーバーが畳んだ 1 行) を読む側
const summaryJson = {
  from: snapAt - 3600, to: snapAt, interval_secs: 5, first_t: snapAt - 3590, last_t: snapAt,
  samples: 720, burst_samples: 0, normal_hours_only: false,
  connects: 4000, p50_ms: 8.3, p95_ms: 80.7, avg_ms: 29.3, max_ms: 3000,
  forwards: 120, forward_p50_ms: 1.2, forward_p95_ms: 9.0, forward_avg_ms: 2.0, forward_max_ms: 50,
  dns_misses: 2200, dns_miss_per_connect: 0.55, dns_miss_avg_ms: 11.5,
  errors: 99, errors_by_cause: [80, 0, 0, 10, 9, 0, 0, 0], causes: causeNames, active_max: 218,
};
// (T14.22) ホスト別の折れ線。`/snapshot` の `hosts_series` があればそれで、無ければ見本で。
// `count` の欄は窓ごとの件数なので、**足すと `total` に一致する** (`ms_max` だけは最大)
function checkSeries(j, where) {
  const sl = ins.seriesLines(j, 'count');
  const rows = (j && j.series) || [];
  if (sl.lines.length !== rows.length) fail(where + ': 系列の本数が合わない');
  for (let i = 1; i < sl.lines.length; i++) {
    if (sl.lines[i - 1].total < sl.lines[i].total) fail(where + ': 多い順になっていない');
  }
  for (const l of sl.lines) {
    if (l.values.length > sl.n) fail(where + ': 標本の数が揃っていない');
    const s2 = l.values.reduce((a, b) => a + b, 0);
    if (s2 !== l.total) fail(where + ': 窓の合計 ' + s2 + ' != total ' + l.total + ' (' + l.host + ')');
    for (const v of l.values) if (v > sl.max) fail(where + ': 最大値が点を含んでいない');
    if (typeof l.color !== 'string' || l.color[0] !== '#') fail(where + ': 色が読めない');
  }
  if (sl.t1 - sl.t0 !== Math.max(0, sl.n - 1) * sl.window) {
    fail(where + ': 時刻が t0 + i × window_secs になっていない');
  }
  return sl;
}
const seriesSample = {
  series: [
    { host: 'connect://a.example.net:443', hour_requests: 120, total: [30, 900, 120, 40, 1],
      samples: [[10, 300, 90, 20, 0], [20, 600, 120, 20, 1]] },
    { host: 'http://b.example.net:80', hour_requests: 10, total: [3, 9, 5, 0, 0],
      samples: [[1, 3, 5, 0, 0], [2, 6, 4, 0, 0]] },
  ],
  keys: ['count', 'ms_sum', 'ms_max', 'dns_ms', 'errors'],
  window_secs: 300, samples: 288, slots: 16, t0: 1789251000, tracked: 2, rotations: 0,
  count: 2, shown: 2, host: '', top: 8, truncated: false,
};
const seriesLines = checkSeries(seriesSample, '見本の折れ線');
if (seriesLines.lines[0].host.indexOf('a.example.net') < 0) fail('多い順の先頭が違う');
if (ins.seriesLines(seriesSample, 'ms_sum').lines[0].total !== 900) fail('欄を選べていない');
if (ins.seriesLines(seriesSample, 'nonesuch').key !== 'count') fail('知らない欄は count に倒すはず');
if (snap.hosts_series) checkSeries(snap.hosts_series, path.basename(snapFile) + ' の /hosts/series');
if (ins.seriesLines(null, 'count').lines.length !== 0) fail('null でも 0 本のはず');
if (ins.seriesLines({ series: [{}] }, 'count').lines[0].total !== 0) fail('無いキーは 0 のはず');

const sum = ins.sinceStart(summaryJson, 0, 0);
if (sum.mode !== 'summary') fail('?summary=1 の応答を要約として読めていない');
if (sum.connects !== 4000 || sum.p50 !== 8.3 || sum.p95 !== 80.7) fail('要約の数が読めていない');
if (sum.dns_per_connect !== 0.55 || sum.dns_avg !== 11.5) fail('要約の名前解決が読めていない');
if (sum.active_max !== 218 || sum.errors !== 99) fail('要約の山とエラーが読めていない');
if (sum.to - sum.from !== 3600) fail('要約の期間が読めていない');
// 実出力の要約 (手元のプロキシから取った 1 行。無ければ飛ばす)
const sumFile = path.join(__dirname, 'testdata', 'history-summary.json');
if (fs.existsSync(sumFile)) {
  const real = ins.sinceStart(JSON.parse(fs.readFileSync(sumFile, 'utf8')), 0, 0);
  if (real.mode !== 'summary') fail('実出力の ?summary=1 を要約として読めていない');
  if (!(real.to >= real.from)) fail('要約の期間が逆');
  if (!(real.samples >= 0) || !(real.connects >= 0)) fail('要約の数が読めていない');
  if (real.connects && !(real.p50 <= real.p95 + 1e-9)) fail('要約の p50 > p95');
}
// T14.9 の `persisted` / `restored` (無い版の出力では false / 0)
const restored = ins.timeline({ recent: [], persisted: true, restored: 7, kept: 9 }, { now: snapAt, span: 60 });
if (!restored.persisted || restored.restored !== 7 || restored.kept !== 9) {
  fail('個票の persisted / restored を読めていない');
}

// 11. 「端末から測る」ページ (`crates/endpoints/src/web/probe.html`。T14.33) の関数。
//     ブラウザが無いので、(1) 作り物の数列で `median`、(2) `/snapshot` の中の `/status` の
//     実出力 (5 つ目の引数) で `pick` と `render` を通す。**この 3 つは DOM にも fetch にも
//     触らない**ので、ここで「経由している / していない」の判定まで確かめられる。
const probeHtml = fs.readFileSync(
  path.join(__dirname, '..', 'crates/endpoints/src/web/probe.html'),
  'utf8'
);
if (Buffer.byteLength(probeHtml) > 64 * 1024) {
  fail('probe.html が 64 KiB を超えた: ' + Buffer.byteLength(probeHtml) + ' B');
}
const probeM = probeHtml.match(/<script>([\s\S]*?)<\/script>/);
if (!probeM) fail('probe.html に <script> が無い');
try {
  new Function(probeM[1]);
} catch (e) {
  fail('probe.html の JS の構文エラー: ' + e.message);
}
const prb = pick(probeM[1], ['median', 'pick', 'render'], 'probe.html');

// (1) 作り物の数列。**平均ではなく中央値**なので、1 回目が遅くても効かない
if (prb.median([2, 1, 3]) !== 2) fail('median: 奇数の中央値が違う');
if (prb.median([4, 1, 3, 2]) !== 2.5) fail('median: 偶数は真ん中 2 つの平均のはず');
if (prb.median([9.9, 0.8, 0.9, 1.0, 0.85]) !== 0.9) fail('median: 1 回目が遅い並びで違う');
if (prb.median([]) !== null || prb.median(null) !== null) fail('median: 空は null のはず');
if (prb.median([1, null, 'x', 3, undefined, NaN]) !== 2) fail('median: 数でない要素を落とせていない');
if (prb.median([5]) !== 5) fail('median: 1 件は そのものはず');

// (2) `/status` の実出力 (`/snapshot` の中の 1 枚) で「経由したか」を見る。
// プロキシは**自分宛ての要求を `clients[]` に数えない**ので、増えていなければ「経由していない」
const probeStatus = snap.status || snap;
if (!probeStatus.clients) fail(path.basename(snapFile) + ' に status.clients[] が無い');
const probeNow = +(probeStatus.clients[0] || {}).last_seen || snapAt;
function probeBumped(j, ip, n) {
  const copy = JSON.parse(JSON.stringify(j));
  const row = copy.clients.find((c) => c.client === ip);
  row.requests += n;
  row.last_seen = probeNow;
  return copy;
}
const probeIp = probeStatus.clients[0].client;
const probeSame = prb.pick(probeStatus, probeStatus, { now: probeNow, need: 5 });
if (probeSame.proxied) fail('要求が増えていないのに「経由している」と出た');
if (probeSame.candidates.length !== 0) fail('増えた行が無いのに候補が出た');
const probeGrew = prb.pick(probeStatus, probeBumped(probeStatus, probeIp, 5), { now: probeNow, need: 5 });
if (!probeGrew.proxied || probeGrew.ip !== probeIp || probeGrew.delta !== 5) {
  fail('要求が 5 増えた行を拾えていない: ' + JSON.stringify([probeGrew.proxied, probeGrew.ip, probeGrew.delta]));
}
if (probeGrew.how !== 'delta' || probeGrew.ambiguous) fail('増えた行 1 件を delta で拾えていない');
// 初めての接続元 (前の枚に行が無い) も「経由している」
const probeFresh = prb.pick({ clients: [] }, probeBumped(probeStatus, probeIp, 5), { now: probeNow });
if (!probeFresh.proxied || !probeFresh.first) fail('新しく増えた行を first で拾えていない');
// 自分の IP が分からないとき (増えた行が無い) は「最終が今」の行を見当にする
const probeGuess = prb.pick(probeStatus, probeStatus, { now: probeNow, window: 10 });
if (probeGuess.proxied) fail('見当は「経由している」ではない');
if (probeStatus.clients.length === 1 && (probeGuess.ip !== probeIp || probeGuess.how !== 'recent')) {
  fail('「最終が今」の行を見当にできていない');
}
if (prb.pick(null, null, {}).proxied) fail('空の入力で「経由している」と出た');

// (3) `render`: 測った数列 + 実出力の `/status` と `/clients` を 1 つの形に畳む
const probeSamples = [
  { ms: 3.4, head_ms: 3.0, bytes: 4096, ok: true },
  { ms: 0.9, head_ms: 0.7, bytes: 4096, ok: true },
  { ms: 1.1, head_ms: 0.8, bytes: 4096, ok: true },
  { ms: 1.0, head_ms: 0.8, bytes: 4096, ok: true },
  { ms: 1.2, head_ms: 0.9, bytes: 4096, ok: true },
];
const probeClients = snap.clients && snap.clients.clients ? snap.clients : probeStatus;
const probeVia = prb.render({
  direct: probeSamples,
  probe: { url: 'http://example.com/', samples: [{ ms: 21 }, { ms: 19 }, { ms: 20 }, { ms: 25 }, { ms: 18 }] },
  before: probeStatus,
  after: probeBumped(probeStatus, probeIp, 5),
  clients: probeClients,
  now: probeNow,
});
if (probeVia.direct.median !== 1.1) fail('(1) の中央値が違う: ' + probeVia.direct.median);
if (!(probeVia.direct.median < 10)) fail('(1) が 1 ms 台にならない (loopback の作り物)');
if (probeVia.direct.first !== 3.4 || probeVia.direct.ok !== 5) fail('(1) の 1 回目 / 成功数が違う');
if (probeVia.verdict.key !== 'proxy') fail('経由しているのに ' + probeVia.verdict.key);
if (probeVia.via.ip !== probeIp || !probeVia.via.exact) fail('経由した端末の IP / 回数が合わない');
if (probeVia.probe.median !== 20) fail('(2) の中央値が違う: ' + probeVia.probe.median);
if (!probeVia.me || probeVia.me.client !== probeIp) fail('/clients の自分の行が読めていない');
// `rtt_ms` (T14.5) は標本 0 なら null。実出力にあるならページの往復と並ぶ
const probeRttRow = (probeClients.clients || []).find((c) => c.client === probeIp) || {};
if (probeRttRow.rtt_ms && +probeRttRow.rtt_ms.samples > 0) {
  if (!probeVia.me.rtt || probeVia.me.rtt.avg !== +probeRttRow.rtt_ms.avg) fail('rtt_ms を読めていない');
  const gap = probeVia.direct.median - +probeRttRow.rtt_ms.avg;
  if (Math.abs(probeVia.compare.gap - gap) > 1e-9) fail('往復 − RTT が合わない');
} else if (probeVia.me.rtt !== null) {
  fail('標本 0 の rtt_ms を null にできていない');
}
// プロキシ設定なし: 取れているのに `clients[]` が 1 行も増えない = 「経由していない」
const probeDirect = prb.render({
  direct: probeSamples,
  probe: { url: 'http://example.com/', samples: [{ ms: 120 }, { ms: 95 }, { ms: 99 }] },
  before: probeStatus,
  after: probeStatus,
  clients: probeClients,
  now: probeNow,
});
if (probeDirect.verdict.key !== 'direct') fail('プロキシ設定なしで ' + probeDirect.verdict.key);
if (probeDirect.via.proxied || probeDirect.via.delta !== 0) fail('経由していないのに増分が出た');
// 取得そのものが失敗したら「判定できない」(経由していないと言い切らない)
const probeFailed = prb.render({
  direct: probeSamples,
  probe: { url: 'http://example.com/', samples: [{ ok: false, error: 'Failed to fetch' }] },
  before: probeStatus,
  after: probeStatus,
  clients: probeClients,
  now: probeNow,
});
if (probeFailed.verdict.key !== 'unknown') fail('取得に失敗したのに ' + probeFailed.verdict.key);
if (probeFailed.probe.fail !== 1 || probeFailed.probe.median !== null) fail('失敗した回を数えられていない');
// `--probeLite` (接続元を記録していない) と、まだ測っていない状態でも落ちない
const probeLite = prb.render({ direct: [], probe: { url: '', samples: [] }, before: {}, after: {}, clients: { clients: [] }, now: probeNow });
if (probeLite.verdict.key !== 'none' || probeLite.me !== null || probeLite.direct.median !== null) fail('--probeLite の形で畳めていない');
if (!probeLite.notes.join('').includes('記録していません')) fail('--probeLite の断り書きが無い');
if (prb.render().verdict.key !== 'none') fail('引数なしで落ちた');
const probeKiB = Math.round(Buffer.byteLength(probeHtml) / 1024);

// 12. KPI「CONNECT 確立 p50」(T14.31)。`recent_quantiles` があれば**区間の補間ではなく
// 直近 1,024 本の実測**を使い、無い版の `/status` では今までどおり区間の補間に落ちること。
const kpiWin = api.mergeWindows(samples, 60, 'connect');
const exactStatus = {
  recent_quantiles: {
    connect: { n: 1024, p50: 0.712, p90: 1.204, p99: 3.41, max: 9.876, window_secs: 137 },
    forward: { n: 8, p50: 1.0, p90: 2.0, p99: 2.0, max: 2.0, window_secs: 5 },
  },
};
const exact = api.connectKpi(exactStatus, kpiWin, hist.bounds_ms);
if (!exact || !exact.exact) fail('recent_quantiles があるのに区間の補間に落ちている');
if (exact.p50 !== 0.712) fail('KPI が recent_quantiles.connect.p50 になっていない: ' + exact.p50);
if (exact.label.indexOf('1,024') < 0) fail('札が「直近 1,024 本」になっていない: ' + exact.label);
for (const part of ['1024 本', 'p90', 'p99', '最大']) {
  if (exact.detail.indexOf(part) < 0) fail('内訳に ' + part + ' が無い: ' + exact.detail);
}
// 1 ms 未満を 1 ms 単位に丸めない (これが T14.31 の目的。fmtMs だと 1 ms になる)
if (api.fmtMsFine(exact.p50) === api.fmtMs(exact.p50)) fail('1 ms 未満が丸められている');
// 無い版 (今までの `/status`) では区間の補間に落ちる
const fell = api.connectKpi({}, kpiWin, hist.bounds_ms);
if (!fell || fell.exact) fail('recent_quantiles が無いのに実測を名乗っている');
if (fell.label !== '直近 5 分') fail('落ちた先の札が違う: ' + fell.label);
if (Math.abs(fell.p50 - kpi) > 1e-9) fail('落ちた先が今までの補間と違う: ' + fell.p50 + ' != ' + kpi);
if (fell.detail.indexOf('区間の補間') < 0) fail('補間であることが内訳に書かれていない');
// 標本が 0 本 (件数 0 の窓、n = 0、null) はどれも null
if (api.connectKpi(null, { count: 0, buckets: [], max: 0, sum: 0 }, hist.bounds_ms) !== null) {
  fail('CONNECT が 1 本も無ければ null のはず');
}
if (api.connectKpi({ recent_quantiles: { connect: { n: 0 } } }, null, hist.bounds_ms) !== null) {
  fail('n = 0 は無いのと同じ (null) のはず');
}
// 実出力に `recent_quantiles` があれば、その形も見る (無い版の出力では飛ばす)
let liveKpi = null;
if (st.recent_quantiles) {
  for (const k of ['connect', 'forward']) {
    const q = st.recent_quantiles[k];
    if (!q) fail('recent_quantiles.' + k + ' が無い');
    for (const f of ['n', 'p50', 'p90', 'p99', 'max', 'window_secs']) {
      if (typeof q[f] !== 'number') fail('recent_quantiles.' + k + '.' + f + ' が数でない');
    }
    if (q.n > 1024) fail(k + ': n が 1,024 を超えている: ' + q.n);
    if (!(q.p50 <= q.p90 + 1e-9 && q.p90 <= q.p99 + 1e-9 && q.p99 <= q.max + 1e-9)) {
      fail(k + ': p50 <= p90 <= p99 <= max になっていない');
    }
    if (q.n === 0 && q.max !== 0) fail(k + ': 標本 0 本なのに値がある');
  }
  liveKpi = api.connectKpi(st, kpiWin, hist.bounds_ms);
}

// 13. KPI「今日の SLO」(T14.50)。`/slo` の応答から**今日 (UTC) の達成率**と
// 「いちばん外した閾」「直近の外れた時間帯」が読めること。`/slo` を持たない版
// (この口が 404 の版) では `null` を返し、ダッシュボードはカードごと出さない。
const sloJson = {
  now: 1789171195,
  days: 7,
  from: 1788566400,
  to: 1789171195,
  sample_secs: 5,
  thresholds: { connect_p50_ms: 10, connect_p95_ms: 100, error_rate: 0.005, dns_miss_per_connect: 0.2 },
  names: ['connect_p50_ms', 'connect_p95_ms', 'error_rate', 'dns_miss_per_connect'],
  judged: 17280,
  met: 12240,
  ratio: 0.70833,
  misses: [5040, 5040, 5040, 0],
  first_t: 1789084800,
  last_t: 1789167600,
  hours: 24,
  kept_hours: 744,
  today: { date: '2026-09-11', t: 1789084800, judged: 17280, met: 12240, ratio: 0.70833, misses: [5040, 5040, 5040, 0] },
  daily: [{ date: '2026-09-11', t: 1789084800, judged: 17280, met: 12240, ratio: 0.70833, misses: [5040, 5040, 5040, 0] }],
  breaches: [
    {
      from: 1789146000, to: 1789171200, from_hour: '2026-09-11T17Z', to_hour: '2026-09-12T00Z',
      hours: 7, judged: 5040, met: 0, ratio: 0.0,
      breached: [
        { name: 'connect_p50_ms', samples: 5040, worst: 46.875, threshold: 10 },
        { name: 'connect_p95_ms', samples: 5040, worst: 300, threshold: 100 },
        { name: 'error_rate', samples: 5040, worst: 0.125, threshold: 0.005 },
      ],
    },
  ],
  hourly_keys: ['t', 'judged', 'met', 'misses'],
  hourly: [[1789084800, 720, 720, [0, 0, 0, 0]], [1789146000, 720, 0, [720, 720, 720, 0]]],
  truncated: false,
};
const slo = api.sloKpi(sloJson);
if (!slo) fail('/slo の応答から KPI が読めていない');
if (Math.abs(slo.pct - 70.833) > 0.01) fail('今日の達成率が 70.833% になっていない: ' + slo.pct);
if (slo.date !== '2026-09-11') fail('札が今日 (UTC) の日付になっていない: ' + slo.date);
if (slo.worst !== 'connect_p50_ms') fail('いちばん外した閾が読めていない: ' + slo.worst);
if (slo.breaches !== 1) fail('外れた時間帯の数が合わない: ' + slo.breaches);
for (const part of ['判定 17,280 標本', '2026-09-11T17Z〜2026-09-12T00Z', '直近 7 日']) {
  if (slo.detail.indexOf(part) < 0) fail('内訳に ' + part + ' が無い: ' + slo.detail);
}
// 判定できた標本が 0 本の日は `ratio` が null (「達成率 0%」ではない)
const sloQuiet = api.sloKpi(
  Object.assign({}, sloJson, {
    ratio: null,
    breaches: [],
    today: { date: '2026-09-11', t: 1789084800, judged: 0, met: 0, ratio: null, misses: [0, 0, 0, 0] },
  })
);
if (!sloQuiet || sloQuiet.pct !== null) fail('判定 0 本の日が 0% になっている');
if (sloQuiet.detail.indexOf('まだ判定できた標本がありません') < 0) fail('断り書きが無い: ' + sloQuiet.detail);
// `/slo` を持たない版 (404 / 空) ではカードごと出さない
for (const bad of [null, undefined, {}, { today: null }, { today: { judged: 0 } }]) {
  if (api.sloKpi(bad) !== null) fail('/slo の無い版で null になっていない: ' + JSON.stringify(bad));
}

console.log(
  'OK: dashboard.html の JS は構文が通り、/history ' +
    samples.length +
    ' 標本 (CONNECT のある標本 ' +
    checked +
    ' 件、合計 ' +
    total +
    ' 本) を読めた。直近の p50 = ' +
    kpi.toFixed(2) +
    ' ms、山 = ' +
    pk +
    '。/status (' +
    path.basename(statusFile) +
    ') はホスト ' +
    st.hosts.length +
    ' 件、名前解決のミス率 ' +
    (dn.rate == null ? '–' : dn.rate.toFixed(1) + '%') +
    ' (ミス 1 回 ' +
    (dn.avg == null ? '–' : dn.avg.toFixed(1) + ' ms') +
    ')、悪いホスト ' +
    bad.length +
    ' 件。個票 (/errors ' +
    errRows.length +
    ' 件、/connections ' +
    conn.rows.length +
    ' 本) も読めた。閉じた接続の分布 ' +
    closedWindows +
    ' 窓と、山の写真 ' +
    shots.length +
    ' 枚 (T14.6) も読めた。canary は ' +
    (canaryRows === null ? 'この出力には無い' : canaryRows + ' 点') +
    '。カーネルの窓 (T14.12) は ' +
    (hist.kernel ? kernelRows + ' 標本' : 'この出力には無い') +
    '、/status の kernel は ' +
    (st.kernel ? '読めた' : 'この出力には無い') +
    '。出来事 (T14.11) は ' +
    evs.length +
    ' 件 / ' +
    EVENT_KINDS.length +
    ' 種' +
    '。/profile (' +
    path.basename(profFile) +
    ') は標本 ' +
    prof.samples.length +
    '、sampler ' +
    prof.sampler +
    '、CPU/要求 ' +
    (prof.recent && prof.recent.cpu_per_request_us != null
      ? (+prof.recent.cpu_per_request_us).toFixed(1) + ' us'
      : '–') +
    '、いちばん長い段階 = CONNECT ' +
    (connLead && connLead.top.length ? connLead.top[0].name + ' ' + connLead.top[0].share.toFixed(0) + '%' : '–') +
    ' / forward ' +
    (fwdLead && fwdLead.top.length ? fwdLead.top[0].name + ' ' + fwdLead.top[0].share.toFixed(0) + '%' : '–') +
    '、役割 ' +
    roles.length +
    ' 件' +
    '。速さと半閉じの分布 (T14.25) は ' +
    transferWindows +
    ' 窓' +
    '。調査ページ (inspect.html ' +
    Math.round(Buffer.byteLength(insHtml) / 1024) +
    ' KiB) は ' +
    path.basename(snapFile) +
    ' の個票で通った: タイムライン ' +
    tlClient.shown +
    ' 本 / ' +
    tlClient.lanes.length +
    ' 行 (理由 ' +
    tlClient.reasons.map((r) => r.name + ' ' + r.count).join(' · ') +
    ')、遅い接続 ' +
    slow.length +
    ' 件、山 ' +
    shotCards.length +
    ' 枚、接続元 ' +
    clientTbl.length +
    ' 件、RTT の点 ' +
    sc.count +
    ' 個 (外した行 ' +
    sc.skipped +
    ')、起動からの窓 ' +
    cut.rows.length +
    ' 標本 (外 ' +
    cut.cut +
    '、CONNECT ' +
    cut.connects +
    ' 本' +
    (cut.p50 == null ? '' : '、p50 ' + cut.p50.toFixed(2) + ' ms') +
    ')。デプロイ先の雪像 (' +
    path.basename(statusFile) +
    ' + ' +
    path.basename(file) +
    ') でも通った: RTT の点 ' +
    ins.rttScatter(st, {}).count +
    ' 個 / ホスト ' +
    st.hosts.length +
    ' 件、起動からの窓 ' +
    deployed.rows.length +
    ' / ' +
    samples.length +
    ' 標本' +
    '。端末から測るページ (probe.html ' +
    probeKiB +
    ' KiB) の median / pick / render も通った: (1) の往復 (作り物) の中央値 ' +
    probeVia.direct.median.toFixed(2) +
    ' ms、判定は 経由している (増えた要求 ' +
    probeVia.via.delta +
    ' 回) / 経由していない / 判定できない の 3 通り' +
    (probeVia.me && probeVia.me.rtt
      ? '、カーネルの RTT ' +
        probeVia.me.rtt.avg.toFixed(2) +
        ' ms (標本 ' +
        probeVia.me.rtt.samples +
        ') と並んだ'
      : '、カーネルの RTT はこの出力には無い') +
    '。KPI「CONNECT 確立 p50」は ' +
    (liveKpi && liveKpi.exact
      ? '実出力の recent_quantiles (' + liveKpi.n + ' 本) で ' + api.fmtMsFine(liveKpi.p50)
      : '見本の recent_quantiles で ' + api.fmtMsFine(exact.p50)) +
    '、無い版では区間の補間 ' +
    api.fmtMsFine(fell.p50) +
    ' に落ちた' +
    '。KPI「今日の SLO」(T14.50) は作り物の /slo で ' +
    slo.pct.toFixed(1) +
    '% (外れの最多 ' +
    slo.worst +
    '、外れた時間帯 ' +
    slo.breaches +
    ' 件)、/slo を持たない版では出さない'
);

// 16. T15.0 (14) の 5 枚のカード。作り物は**インラインの定数**で、5 枚それぞれに
// 「欄がある版」と「**欄が無い古い版**」の 2 通りを通す。
//
// 「古い版」は**引数で渡された実出力から T15.0 の欄を消して**作る。作り置きの実出力
// (`history-res5.json` / `status.json` / `profile-res5.json`) はたしかに T15.0 より前のものだが、
// この確認は `scripts/collect-deployed.sh` からも**デプロイ先の実出力**を引数に呼ばれるので
// (296 行)、引数をそのまま「古い版」に使うと、新しい欄を持つプロキシを相手にした日に必ず落ちる。

// `keys` + 配列の配列 (`/history` も `/profile` も同じ形) から列を落として「その欄を持たない版」を作る
function withoutCols(j, drop) {
  const out = JSON.parse(JSON.stringify(j || {}));
  const keep = [];
  (out.keys || []).forEach((k, i) => {
    if (drop.indexOf(k) < 0) keep.push(i);
  });
  const keys = out.keys || [];
  out.keys = keep.map((i) => keys[i]);
  if (Array.isArray(out.key_kinds)) {
    const kinds = out.key_kinds;
    out.key_kinds = keep.map((i) => kinds[i]);
  }
  out.samples = (out.samples || []).map((row) => keep.map((i) => row[i]));
  return out;
}
// `/status` から T15.0 の欄を消した「古い版」(kernel = 単位 4、dns の 5 つ = 単位 5、wait = 単位 1)
const oldStatus = JSON.parse(JSON.stringify(st));
delete oldStatus.kernel;
for (const k of ['misses_by_kind', 'refresh_failures', 'refresh_ms_sum', 'refresh_ms_max', 'refresh_late']) {
  if (oldStatus.dns) delete oldStatus.dns[k];
}
if (oldStatus.recent_quantiles) delete oldStatus.recent_quantiles.wait;
// `/history` から単位 6 の `wait_*` を消した「古い版」と、その標本
const oldHist = withoutCols(hist, ['waits', 'wait_ms_sum', 'wait_ms_max', 'wait_buckets']);
const oldSamples = api.toSamples(oldHist);
// `/profile` から単位 3 の末尾 2 列を消した「古い版」
const oldProf = api.toProfile(withoutCols(pj, ['threads_top', 'run_delay_us']));

// (a) CPU の絞り。`/history` の `kernel` に T15.0 (6) が `cpu_nr_periods` を**末尾に**足した版
const fakeKernelNew = {
  interval_secs: 5,
  keys: fakeKernel.keys.concat(['cpu_nr_periods']),
  samples: [fakeKernel.samples[0].concat([50]), fakeKernel.samples[1].concat([null])],
};
if (checkKernelHistory(fakeKernelNew, '作り置き (T15.0 (6))') !== 2) fail('新しい kernel を読めていない');
const kernNew = api.toKernel({ kernel: fakeKernelNew });
if (kernNew.length !== 2) fail('/history の kernel を 2 標本で読めていない: ' + kernNew.length);
if (kernNew[0].cpu_nr_periods !== 50) fail('cpu_nr_periods が列名で読めていない');
if (kernNew[1].cpu_nr_periods !== null) fail('読めない環境の null が落ちている');
if (api.toKernel({ kernel: fakeKernel })[0].cpu_nr_periods !== undefined) {
  fail('古い版に無い列が undefined 以外で来た');
}
if (api.toKernel({}).length !== 0) fail('kernel の無い版は 0 件のはず');
if (api.toKernel(null).length !== 0) fail('null でも 0 件のはず');
const cpuStatus = {
  kernel: {
    at: 1789251465,
    cgroup_cpu: {
      nr_throttled: 1200, throttled_usec: 48000000, quota_cores: 2,
      nr_periods: 40000, path: '/sys/fs/cgroup',
      since_start: { nr_periods: 3600, nr_throttled: 90, throttled_usec: 3600000 },
    },
  },
};
const cpu = api.cpuThrottle(cpuStatus, kernNew, 60);
if (!cpu) fail('cgroup_cpu があるのに null');
if (Math.abs(cpu.pct - 3) > 1e-9) fail('絞られた割合が nr_throttled ÷ nr_periods でない: ' + cpu.pct);
if (cpu.quota !== 2 || cpu.path !== '/sys/fs/cgroup') fail('割り当てと道が読めていない');
if (!cpu.since || cpu.since.periods !== 3600) fail('since_start が読めていない');
if (cpu.window.periods !== 50 || cpu.window.throttled !== 0) fail('窓の増分が合わない');
if (cpu.window.pct !== 0) fail('窓の割合が 0 でない: ' + cpu.window.pct);
// 割り当てが無い (`quota_cores` が null) 環境では割合だけ出す
const noQuota = api.cpuThrottle(
  { kernel: { cgroup_cpu: { nr_throttled: 0, throttled_usec: 0, quota_cores: null, nr_periods: 0, path: null, since_start: null } } },
  [], 60
);
if (!noQuota || noQuota.quota !== null || noQuota.pct !== null) fail('割り当ての無い環境が読めていない');
// 古い版 (`/status` に kernel が無い) と cgroup v2 の読めない環境ではカードごと断る
if (api.cpuThrottle(oldStatus, kernNew, 60) !== null) fail('kernel の無い /status で null になっていない');
if (api.cpuThrottle({ kernel: { cgroup_cpu: null } }, [], 60) !== null) fail('cgroup_cpu が null なら null のはず');
// `cgroup_cpu` は在るが `nr_periods` (単位 4 で足した分母) がまだ無い版 = T15.0 より前のデプロイ先。
// 分母 0 の比を見せないために、ここも「記録していません」に落とす
if (api.cpuThrottle({ kernel: { cgroup_cpu: { nr_throttled: 1069258, throttled_usec: 152029542480, quota_cores: 0.5 } } }, [], 60) !== null) {
  fail('nr_periods を持たない古い版で null になっていない');
}
if (api.cpuThrottle(null, null, 60) !== null) fail('null でも例外なく null のはず');
// `kern` が空のとき `ch-cpu` に**長さ 0 の系列**が 2 本渡る (`/history` の `kernel` が無い版、
// `res=3600` = 解像度が 2 つしか無いので `kernel` は null、cgroup が読めない環境)。
// `drawChart` の末尾の目盛りが `hist[hist.length].t` を読んで TypeError を投げると、
// 例外は `pollHistory` の catch が握り潰すので **`redraw` の残りが 5 秒ごとに黙って飛ぶ**。
// ここだけ DOM に触るので、作り物の canvas を前置きにして別に切り出す
const chartApi = pick(js, ['drawChart', 'ago'], 'dashboard.html',
  'var window={devicePixelRatio:1};' +
  'var el={clientWidth:900,clientHeight:300,width:0,height:0,' +
  'getContext:function(){return new Proxy({},{get:function(){return function(){}}})}};' +
  'function $(){return el}var hist=[{t:100},{t:105},{t:110}];\n');
for (const [what, series] of [
  ['長さ 0 の系列 2 本 (kernel を持たない版と res=3600)', [
    { data: [], color: '#8b91a5', dash: [4, 4], width: 1 },
    { data: [], color: '#ff6b6b', fill: 'rgba(255,107,107,.12)' }]],
  ['ふつうの系列', [{ data: [1, null, 3], color: '#5aa9ff', fill: 'rgba(90,169,255,.12)' }]],
]) {
  try {
    chartApi.drawChart('ch-cpu', series, {});
  } catch (e) {
    fail('drawChart が ' + what + ' で落ちた: ' + e.message);
  }
}

// (b) 動かないトンネル。T15.0 (4) の欄は**既定のままなら 1 バイトも出ない**ので、
// 古い版の応答と「何も起きていない応答」は同じ形になる (どちらも 0 件)
const idleConnJson = {
  connections: [
    { id: 11, client: '198.51.100.7', target: 'a.example.net:5228', kind: 'connect', state: 'relaying',
      age_secs: 99000, bytes: 9096, fds: 2, rate_bps: 0, tid: 4821, spins: 1043,
      revents: { client: 'HUP', origin: '' }, half_closed: 'client', half_closed_secs: 98700, idle_secs: 98400 },
    { id: 12, client: '198.51.100.8', target: 'b.example.net:5228', kind: 'connect', state: 'relaying',
      age_secs: 98000, bytes: 9032, fds: 2, rate_bps: 0, tid: 4822, idle_secs: 600 },
    { id: 13, client: '198.51.100.9', target: 'c.example.net:443', kind: 'connect', state: 'relaying',
      age_secs: 30, bytes: 4096, fds: 2, rate_bps: 1024, tid: 4823 },
    { id: 14, client: '198.51.100.9', target: '', kind: 'http', state: 'reading', age_secs: 900, bytes: 0, fds: 1, rate_bps: 0 },
  ],
  count: 4, shown: 4, truncated: false, lite: false,
};
const idle = api.idleTunnels(idleConnJson, 300, 20);
if (idle.count !== 2) fail('300 秒以上動いていないトンネルが 2 本でない: ' + idle.count);
if (idle.rows[0].id !== 11) fail('止まっている長い順でない: ' + idle.rows[0].id);
if (idle.tunnels !== 3) fail('トンネルの本数が合わない: ' + idle.tunnels);
if (idle.spins !== 1043) fail('空回りの合計が合わない: ' + idle.spins);
if (idle.half_closed !== 1) fail('半閉じの本数が合わない: ' + idle.half_closed);
if (idle.quiet !== 1) fail('1 度も空回りしていない本数が合わない: ' + idle.quiet);
if (idle.rows[0].revents.client !== 'HUP') fail('poll の旗が行に残っていない');
if (idle.rows[0].tid !== 4821) fail('tid が行に残っていない');
if (api.idleTunnels(idleConnJson, 100000, 20).count !== 0) fail('秒で絞れていない');
if (api.idleTunnels(idleConnJson, 300, 1).rows.length !== 1) fail('n で絞れていない');
// 古い版 (`idle_secs` を持たない `/connections`) と空と null
const oldIdle = api.idleTunnels(connJson, 300, 20);
if (oldIdle.count !== 0) fail('欄の無い版で 0 件になっていない: ' + oldIdle.count);
if (oldIdle.tunnels !== 2) fail('欄の無い版でもトンネルは数えるはず: ' + oldIdle.tunnels);
if (api.idleTunnels({ connections: [], lite: true }, 300, 20).lite !== true) fail('lite が読めていない');
if (api.idleTunnels({}, 300, 20).count !== 0) fail('空でも例外なく 0 件のはず');
if (api.idleTunnels(null, 300, 20).count !== 0) fail('null でも例外なく 0 件のはず');
// 欄を足しても「いまの接続」の読み方は 1 つも変わらない (末尾に足しただけ)
const bothConns = api.connRows(idleConnJson, 50);
if (bothConns.rows.length !== 4 || bothConns.rows[0].id !== 11) fail('connRows の並びが変わった');
if (bothConns.kinds.connect !== 3 || bothConns.kinds.http !== 1) fail('connRows の内訳が変わった');

// (c) 名前解決の内訳。`misses_by_kind` の 4 つの和は、プロキシ側では `misses` と一致する
// (この確認が突き合わせるのは作り物自身の和。引数の実出力の `misses` とは無関係)
const fakeMissKinds = { cold: 800, expired: 300, warm_stale: 80, negative: 26 };
const fakeMissTotal = 800 + 300 + 80 + 26;
const dnsStatus = {
  dns: Object.assign({}, oldStatus.dns, {
    warm: 12, warm_secs: 900, refreshes: 340, misses: fakeMissTotal,
    misses_by_kind: fakeMissKinds,
    refresh_failures: 4, refresh_ms_sum: 12000.5, refresh_ms_max: 640, refresh_late: 7,
  }),
};
const missKinds = api.dnsMissKinds(dnsStatus);
if (!missKinds) fail('misses_by_kind があるのに null');
if (missKinds.rows.map((r) => r.name).join(',') !== 'cold,expired,warm_stale,negative') {
  fail('ミスの種類の綴りか並びが変わった: ' + missKinds.rows.map((r) => r.name).join(','));
}
if (missKinds.total !== fakeMissTotal) fail('種類別の合計 ' + missKinds.total + ' != ' + fakeMissTotal);
if (missKinds.total !== (dnsStatus.dns.misses || 0)) {
  fail('種類別の合計 ' + missKinds.total + ' != misses ' + dnsStatus.dns.misses);
}
const dn2 = api.dnsStats(dnsStatus);
if (dn2.reffail !== 4 || dn2.reflate !== 7) fail('引き直しの失敗と遅れが読めていない');
if (Math.abs(dn2.refms - 12000.5) > 1e-9 || dn2.refmax !== 640) fail('引き直しの ms が読めていない');
if (dn2.warm !== 12) fail('warm が読めていない (T14.1 の枝が壊れた)');
// 古い版 (`misses_by_kind` も `refresh_*` も無い `/status`)
if (api.dnsMissKinds(oldStatus) !== null) fail('欄の無い版は null のはず');
if (api.dnsStats(oldStatus).reffail !== null) fail('refresh_failures の無い版は null のはず');
if (api.dnsMissKinds({}) !== null || api.dnsMissKinds(null) !== null) fail('空でも null のはず');

// (d) 受付待ち。二峰 (1 ms 未満と 50〜100 ms) の作り物を 12 段の区間で読む
const zeroWin = [0, 0, 0, 0, 0, 0, 0];
const queueBuckets = [80, 0, 0, 0, 0, 0, 20, 0, 0, 0, 0, 0, 0];
const fakeProfile = {
  interval_secs: 5, sample_ms: 1000, sampler: 'on', bounds_ms: hist.bounds_ms,
  stages: pj.stages, roles: pj.roles, states: pj.states, lock_names: pj.lock_names,
  keys: ['t', 'requests', 'cpu_us', 'connect', 'forward', 'threads', 'locks', 'queue', 'threads_top', 'run_delay_us'],
  samples: [
    [1789251460, 100, 4920000,
      [[100, 3000, 95, queueBuckets], 0, 0, 0, 0, 0, 0], zeroWin.slice(0, 6),
      pj.roles.map(() => 0), pj.lock_names.map(() => 0), [0, 0, 0],
      [[4821, 'conn', 1, 4800000, 50], [4822, 'history', 4, 120000, 3]],
      [0, 120000, 0, 0, 3000, 0, 0, 0, 0]],
    [1789251465, 0, 4900000,
      zeroWin, zeroWin.slice(0, 6), pj.roles.map(() => 0), pj.lock_names.map(() => 0), [0, 0, 0],
      [[4821, 'conn', 1, 4900000, 50]], null],
  ],
  locks_total: pj.lock_names.map(() => 0), queue_total: [0, 0, 0], unknown_syscalls: [],
  recent: { secs: 300, requests: 100, cpu_us: 9820000, cpu_per_request_us: 98200 },
  count: 2, shown: 2, truncated: false,
};
const fprof = api.toProfile(fakeProfile);
if (fprof.samples.length !== 2) fail('作り物の /profile の標本が読めていない');
const qs = api.queueSpread(fprof, 60, 'connect');
if (!qs) fail('queue の段があるのに null');
if (qs.rows.length !== hist.bounds_ms.length + 1) fail('区間が bounds_ms + 1 でない: ' + qs.rows.length);
if (qs.total !== 100 || qs.count !== 100) fail('件数と区間の合計が合わない: ' + qs.total + ' / ' + qs.count);
if (qs.rows[0].avg !== 80 || qs.rows[6].avg !== 20) fail('二峰が区間に落ちていない');
if (qs.rows[0].name !== '0–1 ms') fail('区間の名前が違う: ' + qs.rows[0].name);
if (qs.rows[qs.rows.length - 1].name.indexOf('>') !== 0) fail('いちばん上の区間が「より大きい」でない');
if (!(qs.p95 > 50 && qs.p95 <= 95)) fail('p95 が上の峰に来ていない: ' + qs.p95);
if (!(qs.p50 <= 1)) fail('p50 が下の峰に来ていない: ' + qs.p50);
// (c) と (d) の横棒は**件数**を積むので、tooltip の整形は呼ぶ側が渡す (既定は今までどおり ms)。
// `stackHtml` の第 3 引数が効いていないと「cold 800 ms」という嘘の tooltip になる
const qbar = api.stackHtml(qs.rows, qs.total, api.fmtNum);
if (qbar.indexOf('title="0–1 ms 80 (80%)"') < 0) fail('件数の横棒の tooltip が件数になっていない: ' + qbar);
const mkbar = api.stackHtml(missKinds.rows, missKinds.total, api.fmtNum);
if (mkbar.indexOf(' ms') >= 0) fail('件数の横棒の tooltip に ms が入っている: ' + mkbar);
if (mkbar.indexOf('title="cold 800 (66%)"') < 0) fail('ミスの種類の横棒が件数になっていない: ' + mkbar);
// 整形を渡さなければ今までどおり ms (既存の 2 か所はここに乗っている)
if (api.stackHtml(missKinds.rows, missKinds.total).indexOf('title="cold 800 ms (66%)"') < 0) {
  fail('整形を渡さないときは ms のはず: ' + api.stackHtml(missKinds.rows, missKinds.total));
}
if (api.stackHtml(qs.rows, 0) !== '') fail('合計 0 の横棒は空のはず');
const rd = api.runDelay(fprof, 60);
if (!rd.ok) fail('run_delay_us があるのに ok でない');
if (rd.total !== 123000) fail('直近の合計が合わない: ' + rd.total);
if (rd.series.length !== 2 || rd.series[1] !== null) fail('読めない標本が null で来ていない');
if (Math.abs(rd.series[0] - 24600) > 1e-9) fail('us / 秒 になっていない: ' + rd.series[0]);
if (rd.roles[0].role !== 'conn' || rd.roles[0].us !== 120000) fail('役割ごとの合計が合わない');
const tt = api.topThreads(fprof, 60);
if (tt.length !== 2) fail('上位スレッドが 2 本で読めていない: ' + tt.length);
if (tt[0].tid !== 4821 || tt[0].cpu_us !== 9700000) fail('tid で束ねられていない: ' + JSON.stringify(tt[0]));
if (tt[0].windows !== 2) fail('窓をまたいだ数が合わない: ' + tt[0].windows);
if (tt[0].role !== 'conn' || tt[1].role !== 'history') fail('役割が roles の添字で戻っていない');
if (Math.abs(tt[0].cpu_pct - 97) > 1e-9) fail('CPU % が合わない: ' + tt[0].cpu_pct);
// 古い版 (`threads_top` も `run_delay_us` も無い 8 列の `/profile`)
if (api.topThreads(oldProf, n5).length !== 0) fail('列の無い版で 0 件になっていない');
const oldRd = api.runDelay(oldProf, n5);
if (oldRd.ok || oldRd.total !== 0) fail('列の無い版で ok になっている');
if (oldRd.series.some((v) => v !== null)) fail('列の無い版の折れ線が null で切れていない');
if (api.queueSpread(api.toProfile(null), 60, 'connect') !== null) fail('空なら null のはず');
if (api.topThreads(api.toProfile(null), 60).length !== 0) fail('空でも 0 件のはず');
if (api.runDelay(api.toProfile(null), 60).ok) fail('空で ok になっている');
// 既存の読み方 (段階・役割・ロック) は末尾に 2 列足しても変わらない
const fstage = api.stageRows(fprof, 60, 'connect');
if (fstage.rows.length !== 7 || fstage.rows[0].count !== 100) fail('段階の読み方が変わった');
if (api.roleRows(fprof, 60).length !== 0) fail('標本 0 の役割が出ている');

// (e) 利用者が待つ時間。`recent_quantiles.wait` があればそれ、無ければ `wait_*` の補間
const waitStatus = {
  recent_quantiles: {
    connect: { n: 1024, p50: 8.4, p90: 50, p99: 63.4, max: 150, window_secs: 240 },
    forward: { n: 8, p50: 1, p90: 2, p99: 2, max: 2, window_secs: 5 },
    wait: { n: 1024, p50: 9.5, p90: 63, p99: 92, max: 180.5, window_secs: 240 },
  },
};
const wexact = api.waitKpi(waitStatus, null, hist.bounds_ms);
if (!wexact || !wexact.exact) fail('recent_quantiles.wait があるのに補間に落ちている');
if (wexact.p50 !== 9.5) fail('KPI が recent_quantiles.wait.p50 になっていない: ' + wexact.p50);
if (wexact.label.indexOf('1,024') < 0) fail('札が「直近 1,024 本」でない: ' + wexact.label);
for (const part of ['1024 本', 'p90', 'p99', '最大']) {
  if (wexact.detail.indexOf(part) < 0) fail('内訳に ' + part + ' が無い: ' + wexact.detail);
}
// `wait` は `connect` より必ず大きいか等しい (名前解決と queue が入っているぶん)
const wc = waitStatus.recent_quantiles;
for (const q of ['p50', 'p90', 'p99', 'max']) {
  if (!(wc.wait[q] >= wc.connect[q])) fail('wait.' + q + ' が connect を下回った');
}
// `/history` に T15.0 (10) の 4 列を足した版 (**古い版の末尾に**足すだけ。既に持っている
// 実出力を渡されても鍵が重複しないように、土台は列を落とした `oldHist`)
const waitHist = Object.assign({}, oldHist, {
  keys: oldHist.keys.concat(['waits', 'wait_ms_sum', 'wait_ms_max', 'wait_buckets']),
  samples: oldHist.samples.map((row, i) =>
    row.concat(
      i % 2
        ? [4, 260, 95, [2, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0]]
        : [0, 0, 0, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]]
    )
  ),
});
const wsamples = api.toSamples(waitHist);
if (wsamples.length !== oldSamples.length) fail('列を足したら標本の数が変わった');
if (wsamples[0].connect_buckets.length !== oldSamples[0].connect_buckets.length) fail('既存の列がずれた');
const wwin = api.mergeWindows(wsamples, 60, 'wait');
const waitTotal = wsamples.slice(Math.max(0, wsamples.length - 60)).reduce((a, s) => a + (s.waits || 0), 0);
if (wwin.count !== waitTotal) fail('mergeWindows が waits を数えていない: ' + wwin.count + ' != ' + waitTotal);
if (wwin.buckets.reduce((a, b) => a + b, 0) !== waitTotal) fail('wait の区間の合計が件数と合わない');
const wfell = api.waitKpi({}, wwin, hist.bounds_ms);
if (!wfell || wfell.exact) fail('wait_* しか無いのに実測を名乗っている');
if (wfell.label !== '直近 5 分') fail('落ちた先の札が違う: ' + wfell.label);
if (wfell.detail.indexOf('区間の補間') < 0) fail('補間であることが内訳に書かれていない');
if (!(wfell.p50 >= 0 && wfell.p50 <= wwin.max)) fail('落ちた先の p50 が範囲外: ' + wfell.p50);
// 古い版 (`recent_quantiles.wait` も `wait_*` も無い) ではカードを「–」のままにする
if (api.waitKpi(oldStatus, api.mergeWindows(oldSamples, 60, 'wait'), hist.bounds_ms) !== null) {
  fail('wait をどこにも持たない版で null になっていない');
}
if (api.mergeWindows(oldSamples, 60, 'wait').count !== 0) fail('古い標本で waits が数えられている');
if (api.waitKpi(null, null, hist.bounds_ms) !== null) fail('null でも例外なく null のはず');
if (api.waitKpi({ recent_quantiles: { wait: { n: 0 } } }, null, hist.bounds_ms) !== null) {
  fail('n = 0 は無いのと同じ (null) のはず');
}
// 既存の KPI (`connectKpi`) は 1 つも変わらない
if (api.connectKpi(waitStatus, kpiWin, hist.bounds_ms).p50 !== 8.4) fail('connectKpi が変わった');

// 17. `dashboard.html` の大きさ。inspect は Rust 側で 64 KiB を見張っているのに
// dashboard には上限が無かった (T15.0 (14) で足した)。外部ライブラリを読み込まない
// 1 ページという方針を守るための歯止めで、超えたら**中身を削るか上限を上げるか**を先に決める
const DASH_MAX = 80 * 1024;
const dashBytes = Buffer.byteLength(html);
if (dashBytes > DASH_MAX) {
  fail('dashboard.html が ' + DASH_MAX + ' B を超えた: ' + dashBytes + ' B');
}

console.log(
  'OK: T15.0 (14) の 5 枚のカードも通った: (a) CPU の絞り ' +
    cpu.pct.toFixed(2) + '% (窓 ' + cpu.window.periods + ' 周期、割り当て ' + cpu.quota + ' コア)、' +
    '(b) 動かないトンネル ' + idle.count + ' / ' + idle.tunnels + ' 本 (空回り ' + idle.spins + ')、' +
    '(c) ミスの種類 ' + missKinds.rows.map((r) => r.name + ' ' + r.avg).join(' · ') + '、' +
    '(d) 受付待ち ' + qs.count + ' 件 (p50 ' + api.fmtMsFine(qs.p50) + ' / p95 ' + api.fmtMsFine(qs.p95) +
    ' の二峰、上位スレッド ' + tt.length + ' 本、run_delay ' + rd.total + ' us)、' +
    '(e) 利用者が待つ時間 p50 ' + api.fmtMsFine(wexact.p50) + ' (無い版では区間の補間 ' +
    api.fmtMsFine(wfell.p50) + '、どちらも無い版では出さない)。' +
    '欄の無い古い版 (' + path.basename(file) + ' / ' + path.basename(statusFile) + ' / ' +
    path.basename(profFile) + ' から T15.0 の欄を落としたもの) では 5 枚とも「無い」に落ちた。' +
    'dashboard.html は ' + dashBytes + ' B / 上限 ' + DASH_MAX + ' B'
);

// 11. 匿名化した実データ (T14.35) で 2〜5 と 10 の読み方をもう一度回す。
// `scripts/testdata/deployed-2026-09-16.anon.json` は**デプロイ先の実出力**を
// `scripts/anonymize-snapshot.py` に通したもの (ホスト名・接続元 IP・User-Agent だけを
// 決定的に置き換え、数字は 1 つも変えていない)。上の作り置きは手元のプロキシと架空の個票なので、
// **本物の分布** (ホスト 817 件、名前解決 90 件、`res=60` は 1,440 標本) で読み方が壊れていないか
// をここで見る。匿名化済みの名前 (`host-0001.example` / `198.51.100.x`) しか入っていないことも
// 一緒に確かめる (元の名前が混ざったらここで気づく)。
function checkAnonymized() {
  const snapPath = path.join(__dirname, 'testdata', 'deployed-2026-09-16.anon.json');
  if (!fs.existsSync(snapPath)) return null;
  const where = path.basename(snapPath);
  const snap = JSON.parse(fs.readFileSync(snapPath, 'utf8'));
  const NAME = '(?:host-\\d{4,}\\.example|other|203\\.0\\.113\\.\\d+|192\\.0\\.2\\.\\d+)';
  const HOSTKEY = new RegExp('^(?:[a-z]+:\\/\\/)?(?:' + NAME + ')(?::\\d+)?$');
  const CLIENT = /^(?:198\.51\.100\.\d+|198\.18\.\d+\.\d+|2001:db8::[0-9a-f]+)(?::\d+)?$/;
  let rows = 0, conns = 0;
  for (const res of Object.keys(snap.history || {})) {
    const h = snap.history[res] || {};
    const ss = api.toSamples(h);
    if (ss.length !== (h.samples || []).length) fail(where + ' の res=' + res + ' の標本の数が合わない');
    if (ss.length === 0) fail(where + ' の res=' + res + ' に標本が無い');
    for (const k of h.keys) {
      if (!(k in ss[0])) fail(where + ' の res=' + res + ' で列 ' + k + ' が読めていない');
    }
    for (const s of ss) {
      if (!s.connects) continue;
      const p50 = api.winQuantile(s.connect_buckets, s.connects, s.connect_ms_max, 0.5, h.bounds_ms);
      const p95 = api.winQuantile(s.connect_buckets, s.connects, s.connect_ms_max, 0.95, h.bounds_ms);
      if (p50 === null) fail(where + ': 件数があるのに p50 が null');
      if (!(p50 <= p95 + 1e-9)) fail(where + ': p50 ' + p50 + ' > p95 ' + p95);
      if (!(p95 <= s.connect_ms_max)) fail(where + ': p95 が窓の最大値を超えた');
      conns += s.connects;
    }
    const tail = ss.slice(Math.max(0, ss.length - 60));
    const merged = api.mergeWindows(ss, 60, 'connect');
    const want = tail.reduce((a, s) => a + s.connects, 0);
    if (merged.count !== want) fail(where + ' の mergeWindows の件数 ' + merged.count + ' != ' + want);
    if (merged.buckets.reduce((a, b) => a + b, 0) !== merged.count) fail(where + ': 区間の合計が件数と合わない');
    if (api.peak(ss, 60, 'active_max', 'active') === null) fail(where + ': 山が読めていない');
    rows += ss.length;
  }
  if (conns === 0) fail(where + ' に CONNECT のある標本が 1 つも無い');
  // `/status` (要求数順・エラー順・名前解決順の 3 枚) と個票
  const st = snap.status || {};
  const sdn = api.dnsStats(st);
  if (!(sdn.rate >= 0 && sdn.rate <= 100)) fail(where + ' の名前解決のミス率が範囲外: ' + sdn.rate);
  if (sdn.lookups !== (st.dns.hits || 0) + (st.dns.misses || 0)) fail(where + ': 解決した回数が合わない');
  const causes = (snap.history && snap.history['60'] && snap.history['60'].causes) || [];
  const bad = api.badHosts([st.hosts, (snap.status_errors || {}).hosts, (snap.status_dns || {}).hosts], causes, 10);
  if (bad.length === 0) fail(where + ': 悪いホストが 1 件も出ない');
  const hostRows = ((snap.hosts || {}).hosts) || [];
  if (hostRows.length === 0) fail(where + ' に /hosts の行が無い');
  for (const h of hostRows.concat(st.hosts || [])) {
    if (!HOSTKEY.test(h.host)) fail(where + ': 匿名化されていないホストがある (' + h.host.length + ' 文字)');
  }
  for (const c of (st.clients || []).concat(((snap.clients || {}).clients) || [])) {
    if (!CLIENT.test(c.client) && c.client !== 'other') {
      fail(where + ': 匿名化されていない接続元がある');
    }
  }
  const errRowsA = api.errorRows(snap.errors, 20);
  const connA = api.connRows(snap.connections, 50);
  for (const r of connA.rows) {
    if (r.target && !HOSTKEY.test(r.target)) fail(where + ': /connections の宛先が匿名化されていない');
    if (!CLIENT.test(r.client)) fail(where + ': /connections の接続元が匿名化されていない');
  }
  // 「調査」ページ (T14.8) の読み方も同じ実データで 1 度通す (この版の出力にあるのは `/history` だけ)
  const insH = (snap.history || {})['60'] || {};
  const insRows = ins.toSamples(insH);
  if (insRows.length !== (insH.samples || []).length) fail(where + ': inspect.html が実データの標本を読めない');
  for (const s2 of insRows) {
    if (!s2.connects) continue;
    const q = ins.winQuantile(s2.connect_buckets, s2.connects, s2.connect_ms_max, 0.5, insH.bounds_ms);
    if (!(q >= 0 && q <= s2.connect_ms_max)) fail(where + ': inspect.html の p50 が範囲外: ' + q);
  }
  // T14.6 / T14.12 / T14.25 の窓は、この版の出力には無い (あれば読む)
  const windows = checkClosed((snap.history || {})['5'] && (snap.history || {})['5'].closed, where) +
    checkTransfer((snap.history || {})['5'] && (snap.history || {})['5'].transfer, where) +
    checkKernelHistory((snap.history || {})['5'] && (snap.history || {})['5'].kernel, where);
  return {
    resolutions: Object.keys(snap.history || {}).length,
    rows,
    conns,
    hosts: hostRows.length,
    inspect: insRows.length,
    bad: bad.length,
    errors: errRowsA.length,
    conns_now: connA.rows.length,
    windows,
  };
}

const anon = checkAnonymized();
console.log(
  anon === null
    ? '(匿名化した実データ (T14.35) は無い: scripts/testdata/deployed-2026-09-16.anon.json)'
    : 'OK: 匿名化した実データ (T14.35) も読めた: /history ' +
        anon.resolutions +
        ' 本 (標本 ' +
        anon.rows +
        '、CONNECT ' +
        anon.conns +
        ' 本)、/hosts ' +
        anon.hosts +
        ' 件、悪いホスト ' +
        anon.bad +
        ' 件、/errors ' +
        anon.errors +
        ' 件、/connections ' +
        anon.conns_now +
        ' 本 (inspect.html でも ' +
        anon.inspect +
        ' 標本)。ホスト名と接続元は全部 匿名化済みの形だった'
);

// 14. **応答の形の版 (`schema`。T14.49)**。
// プロキシの応答は先頭に `"schema":1` を持つようになった。ここで見るのは 2 つ:
//   (a) **版の無い古い出力 (版 0) が今までどおり読めること** — 手元に残っている雪像
//       (`scripts/testdata/snapshot-local.json`、匿名化した実データ) はどれも版を
//       持たない形なので、そちらが読めなくなったら過去のデータを描けない
//   (b) **版 1 の出力も同じ関数で読めること** — 版を足しただけで形は変わっていないので、
//       上の (a) と**同じ結果**にならなければならない (版の分岐が読み方を変えていない証拠)
// 版 1 の入力は、版の無い雪像に `schema` を足して作る (新しいプロキシの出力と同じ形)。
const SCHEMA = 1;

/** この JSON の形の版 (`schema` が無い古い出力は 0 = 推測で読む)。 */
function schemaOf(x) {
  const v = x && typeof x === 'object' ? x.schema : undefined;
  return Number.isInteger(v) ? v : 0;
}

/** 版の無い雪像を版 1 の形にする (外側と、object の部の全部に `schema` を足す)。 */
function withSchema(snapshot) {
  const out = { schema: SCHEMA };
  for (const k of Object.keys(snapshot)) {
    const v = snapshot[k];
    if (k === 'history' && v && typeof v === 'object') {
      out.history = {};
      for (const res of Object.keys(v)) {
        out.history[res] = v[res] && typeof v[res] === 'object' ? Object.assign({ schema: SCHEMA }, v[res]) : v[res];
      }
    } else if (v && typeof v === 'object' && !Array.isArray(v)) {
      out[k] = Object.assign({ schema: SCHEMA }, v);
    } else {
      out[k] = v;
    }
  }
  return out;
}

/** 版に関わらず同じ読み方で出せる数 (この並びが版 0 と版 1 で一致すること)。 */
function digest(s) {
  const h = (s.history || {})['5'] || (s.history || {})['60'] || {};
  const rows = api.toSamples(h);
  const st = s.status || {};
  return JSON.stringify({
    samples: rows.length,
    keys: (h.keys || []).length,
    connects: rows.reduce((a, r) => a + (r.connects || 0), 0),
    dns: st.dns ? api.dnsStats(st).lookups : null,
    hosts: (st.hosts || []).length,
    errors: api.errorRows(s.errors, 20).length,
    conns: api.connRows(s.connections, 50).rows.length,
    timeline: ins.timeline(s.recent, { now: +s.taken_at || 0, span: 86400 }).shown,
  });
}

const versioned = [];
for (const name of ['snapshot-local.json', 'deployed-2026-09-16.anon.json']) {
  const p = path.join(__dirname, 'testdata', name);
  if (!fs.existsSync(p)) continue;
  const old = JSON.parse(fs.readFileSync(p, 'utf8'));
  // (a) 置いてある fixture は版を持たない古い出力
  if (schemaOf(old) !== 0) fail(name + ' は版の無い古い出力のはず (版 ' + schemaOf(old) + ')');
  const before = digest(old);
  // (b) 版 1 にしても同じ読み方で同じ結果
  const now = withSchema(old);
  if (schemaOf(now) !== SCHEMA) fail(name + ': 版 1 にできていない');
  if (schemaOf(now.status) !== SCHEMA) fail(name + ': 部に版が付いていない');
  if (digest(now) !== before) fail(name + ': 版 1 にしたら読めた中身が変わった');
  versioned.push(name + ' (版 0 → 1 で一致)');
}
// 知らない鍵 (先頭の `schema`) が混ざっても、列を名前で引く読み方は影響を受けない
const keyed = api.toSamples(Object.assign({ schema: 99 }, JSON.parse(fs.readFileSync(file, 'utf8'))));
if (keyed.length === 0) fail('版だけを足した /history が読めない');
if (schemaOf({ schema: true }) !== 0 || schemaOf({ schema: '1' }) !== 0) {
  fail('schemaOf: 整数でない版を 0 にできていない');
}
console.log(
  versioned.length === 0
    ? '(版 (schema) を確かめる雪像が無い)'
    : 'OK: 応答の形の版 (schema。T14.49) で分岐しても読み方が変わらない: ' + versioned.join('、') +
        '。版の無い古い出力は版 0 として今までどおり読める'
);
// 15. 「調査」ページの「今日」「今週」「出来事と異常」(T14.44)。
//     `dailyRows` (`/daily`。T14.20)・`weeklyRows` (`/daily` の 7 行をブラウザの中で足す +
//     `/snapshots` の一覧。T14.40 / T14.34)・`eventRows` (`/events` の表。T14.11 / T14.23) を
//     3 つの入力で回す: (1) `/daily` と `/snapshots` の作り置き (9 日ぶん = 週の窓より 2 日多い)、
//     (2) **`snapshot-local.json` の `events`** (手元のプロキシの本物の出力。`anomaly` 入り)、
//     (3) **匿名化した実データ** (T14.35。この版の雪像には `/daily` も `/events` も無いので
//     **「無い版」の分岐**を実データで通し、`/history?res=3600` を UTC の日で束ねて
//     「週の足し算」を本物の分布で確かめる)。
const insNew = pick(insJs, ['num', 'dailyRows', 'weeklyRows', 'eventRows'], 'inspect.html', insPrelude);
const DAY_SECS = 86400;
const dayName = (t) => new Date(t * 1000).toISOString().slice(0, 10);

// (1) `/daily` の作り置き。**T14.20 が書く 1 行の形そのまま** (欄が増えたらここも増やす)
const dayFrom = Math.floor(snapAt / DAY_SECS) * DAY_SECS - 8 * DAY_SECS;
const dailyLines = [];
for (let i = 0; i < 9; i++) {
  const t = dayFrom + i * DAY_SECS;
  const connects = 1000 + i * 100;
  const misses = 500 + i * 10;
  dailyLines.push({
    day: dayName(t), t,
    // 4 日目だけ半日しか動いていない日 (「見ていた 50%」の行)
    secs: i === 3 ? DAY_SECS / 2 : DAY_SECS, samples: i === 3 ? 8640 : 17280,
    requests: 10000 + i * 1000, bytes: 1000000000 + i, connects,
    connect_p50_ms: 8 + i * 0.1, connect_p95_ms: 80 + i,
    dns_misses: misses, dns_per_connect: misses / connects, dns_miss_ms: 10 + i,
    errors: i === 1 ? 85 : 0, bursts: i === 1 ? 4 : 0, active_max: i === 1 ? 218 : 20,
    evicted_idle: 0, rss_max: 20000000 + i, rss_avg: 18000000 + i, version: '0.1.0+abc1234',
  });
}
const dailyJson = {
  days: dailyLines, count: 9, shown: 9, bytes: 3024, max_bytes: 2 * 1024 * 1024,
  max_line: 512, path: '/home/u/.rust-http-proxy.daily.jsonl', truncated: false,
};
// `/snapshots` の作り置き (窓の 7 日のうち 5 日ぶんがディスクにある)
const snapsJson = {
  files: dailyLines.slice(2, 7).map((d) => ({ date: d.day, bytes: 17871 + d.t % 7, t: d.t + DAY_SECS })),
  count: 5, shown: 5, bytes: 89355, days: 30, max_bytes: 4 * 1024 * 1024,
  dir: '/home/u/.rust-http-proxy/snapshots', truncated: false,
};

const dr = insNew.dailyRows(dailyJson, 14);
if (dr.rows.length !== 9) fail('/daily の行が読めていない: ' + dr.rows.length);
if (!dr.available) fail('/daily の口があるのに available:false');
if (dr.rows[0].day !== dailyLines[8].day) fail('/daily が新しい順でない: ' + dr.rows[0].day);
for (let i = 1; i < dr.rows.length; i++) {
  if (dr.rows[i - 1].t <= dr.rows[i].t) fail('/daily が新しい順でない');
}
if (dr.rows[0].coverage !== 100) fail('丸 1 日の行が 100% でない: ' + dr.rows[0].coverage);
const halfDay = dr.rows.filter((r) => r.day === dailyLines[3].day)[0];
if (Math.abs(halfDay.coverage - 50) > 1e-9) fail('見ていた割合が secs ÷ 86400 でない: ' + halfDay.coverage);
if (dr.rows[0].error_rate !== 0) fail('エラー 0 の日の率が 0 でない');
if (insNew.dailyRows(dailyJson, 3).rows.length !== 3) fail('/daily を n で絞れていない');
if (insNew.dailyRows(null, 7).available !== false) fail('口の無い版 (404) は available:false のはず');
if (insNew.dailyRows(null, 7).rows.length !== 0) fail('口の無い版でも 0 行のはず');
if (insNew.dailyRows({ days: [], count: 0 }, 7).available !== true) {
  fail('口はあるが 1 行も書いていない版は available:true のはず');
}
if (insNew.dailyRows({ days: [{}] }, 7).rows[0].requests !== 0) fail('無いキーは 0 のはず');
// `dns_per_connect` を書いていない行 (手で作った `/daily`) はミス ÷ CONNECT を自分で出す
const calc = insNew.dailyRows({ days: [{ day: '2026-09-16', t: 1, connects: 200, dns_misses: 50 }] }, 7).rows[0];
if (calc.dns_per_connect !== 0.25) fail('ミス率を自分で出せていない: ' + calc.dns_per_connect);
if (insNew.dailyRows({ days: [{ day: 'x', t: 1 }] }, 7).rows[0].dns_per_connect !== null) {
  fail('CONNECT 0 の日はミス率を出さない (null) はず');
}

// 「今週」: `/daily` の 7 行を足したもの。**足せるものだけ足す** (分位点は足さない)
const wk = insNew.weeklyRows(dailyJson, snapsJson, 7);
const want7 = dailyLines.slice(2);
const sum7 = (k) => want7.reduce((a, d) => a + d[k], 0);
if (wk.rows.length !== 7) fail('週が 7 行でない: ' + wk.rows.length);
if (wk.rows[0].day !== want7[0].day) fail('週が古い順でない: ' + wk.rows[0].day);
for (let i = 1; i < wk.rows.length; i++) if (wk.rows[i - 1].t >= wk.rows[i].t) fail('週が古い順でない');
for (const k of ['requests', 'bytes', 'connects', 'errors', 'dns_misses', 'bursts', 'secs', 'samples']) {
  if (wk.total[k] !== sum7(k)) fail('週の ' + k + ' が 7 日の和でない: ' + wk.total[k] + ' != ' + sum7(k));
}
if (wk.total.active_max !== Math.max.apply(null, want7.map((d) => d.active_max))) {
  fail('週の山は日ごとの最大のはず: ' + wk.total.active_max);
}
if (Math.abs(wk.total.dns_per_connect - sum7('dns_misses') / sum7('connects')) > 1e-12) {
  fail('週のミス率がミス ÷ CONNECT でない: ' + wk.total.dns_per_connect);
}
const msWant = want7.reduce((a, d) => a + d.dns_miss_ms * d.dns_misses, 0) / sum7('dns_misses');
if (Math.abs(wk.total.dns_miss_ms - msWant) > 1e-9) {
  fail('ミス 1 回はミスの数で重みを付けた平均のはず: ' + wk.total.dns_miss_ms + ' != ' + msWant);
}
if (Math.abs(wk.total.error_rate - (sum7('errors') / sum7('requests')) * 100) > 1e-12) {
  fail('週のエラー率が合わない: ' + wk.total.error_rate);
}
// **週の p50 / p95 は出さない** (区間ヒストグラムが `/daily` に無いので足せない。T14.40 と同じ)
if (wk.total.p50 !== null || wk.total.p95 !== null) fail('週の分位点を出してしまっている');
if (wk.p50_lo !== Math.min.apply(null, want7.map((d) => d.connect_p50_ms))) fail('日ごとの p50 の下が違う');
if (wk.p95_hi !== Math.max.apply(null, want7.map((d) => d.connect_p95_ms))) fail('日ごとの p95 の上が違う');
if (wk.from !== want7[0].day || wk.to !== want7[6].day) fail('週の期間が違う: ' + wk.from + ' → ' + wk.to);
if (wk.missing !== 0 || wk.gaps !== 0) fail('7 日そろっているのに足りない日が出た');
// `/snapshots` の一覧と日を突き合わせる (この週は 5 枚)
if (wk.in_week !== 5) fail('この週の雪像が 5 枚でない: ' + wk.in_week);
if (!wk.rows[0].snapshot || wk.rows[0].snapshot.date !== wk.rows[0].day) fail('雪像が日に結び付いていない');
if (wk.rows[6].snapshot !== null) fail('雪像の無い日に雪像が付いた');
if (wk.snapshot_days !== 30 || !wk.has_snapshots) fail('/snapshots の残す日数が読めていない');
// 足りない日・抜けた日・無い版
const thin = insNew.weeklyRows({ days: dailyLines.slice(0, 3) }, null, 7);
if (thin.rows.length !== 3 || thin.missing !== 4) fail('足りない日が 4 日でない: ' + thin.missing);
if (thin.has_snapshots || thin.snapshots !== 0) fail('/snapshots の無い版で枚数が出た');
const gappy = insNew.weeklyRows({ days: [dailyLines[0], dailyLines[2], dailyLines[3]] }, null, 7);
if (gappy.gaps !== 1) fail('間で抜けた 1 日を数えていない: ' + gappy.gaps);
const noDaily = insNew.weeklyRows(null, null, 7);
if (noDaily.available || noDaily.rows.length !== 0 || noDaily.total.requests !== 0) {
  fail('/daily の無い版で 0 にならない');
}
if (noDaily.total.dns_per_connect !== null || noDaily.p50_lo !== null) fail('0 日なのに値が出た');

// (2) 「出来事と異常」。立った異常と `cleared:` を対にする (T14.23)
const evAt = snapAt - 3600;
const evList = [
  { at: evAt + 900, kind: 'anomaly', text: 'cleared: dns_slow after 6m (dns miss 12 ms avg over 5m; 3 misses)' },
  { at: evAt + 600, kind: 'anomaly', text: 'errors: 6 errors in 5m (threshold 5): dns 6' },
  { at: evAt + 300, kind: 'anomaly', text: 'dns_slow: dns miss 693 ms avg over 5m (threshold 100 ms; 1 misses, 693 ms total)' },
  { at: evAt + 120, kind: 'brand_new_kind', text: '知らない綴りの出来事' },
  { at: evAt, kind: 'start', text: 'version 0.1.0 on port 18099' },
];
const er = insNew.eventRows(
  { events: evList, count: 5, kept: 5, capacity: 512, recorded: 5, persisted: true, restored: 2, truncated: false },
  200
);
if (er.rows.length !== 5) fail('出来事の件数が合わない: ' + er.rows.length);
if (!er.available) fail('/events があるのに available:false');
for (let i = 1; i < er.rows.length; i++) if (er.rows[i - 1].at < er.rows[i].at) fail('出来事が新しい順でない');
const fired = er.rows.filter((r) => r.subject === 'dns_slow' && r.anomaly)[0];
const clr = er.rows[0];
if (!fired) fail('立った異常 (dns_slow) が読めていない');
if (!clr.cleared || clr.subject !== 'dns_slow') fail('cleared: の種類が読めていない: ' + clr.subject);
if (clr.pair_at !== fired.at || fired.pair_at !== clr.at) fail('cleared: が対になっていない');
if (fired.secs !== 600 || clr.secs !== 600) fail('続いた長さが 600 秒でない: ' + fired.secs);
if (fired.open) fail('解除された異常が「続いている」ままになっている');
if (clr.color === fired.color) fail('anomaly と cleared: の色が同じ');
const still = er.rows.filter((r) => r.subject === 'errors')[0];
if (!still.open || still.secs !== null) fail('解除の無い異常は続いている扱いのはず');
if (er.anomalies !== 2 || er.cleared !== 1) fail('異常 / 解除の数が合わない: ' + er.anomalies + '/' + er.cleared);
if (er.open !== 1 || er.open_kinds.join() !== 'errors') fail('続いている異常が 1 件 (errors) でない');
const unknownEv = er.rows.filter((r) => r.kind === 'brand_new_kind')[0];
if (unknownEv.known !== false || unknownEv.color !== '#8b91a5') fail('知らない綴りは既定の色のはず');
if (unknownEv.anomaly || unknownEv.subject !== '') fail('anomaly でない行に種類が付いた');
if (er.kinds.reduce((a, k) => a + k.count, 0) !== er.rows.length) fail('種類の内訳が件数と合わない');
// 既知の綴り (T14.11 の 10 種 + T14.23 の `anomaly` + T14.54 の `new_client`) は
// どれも「知っている」側に入る
const allKinds = insNew.eventRows({ events: EVENT_KINDS.map((k, i) => ({ at: evAt + i, kind: k, text: k })) }, 200);
if (allKinds.kinds.length !== EVENT_KINDS.length) fail(EVENT_KINDS.length + ' 種が読めていない: ' + allKinds.kinds.length);
for (const r of allKinds.rows) if (!r.known) fail('既知の綴りのはずが知らない綴りになった: ' + r.kind);
if (insNew.eventRows(null, 200).available !== false) fail('/events の無い版は available:false のはず');
if (insNew.eventRows(null, 200).rows.length !== 0) fail('null でも 0 件のはず');
if (insNew.eventRows({ events: [] }, 200).available !== true) fail('口はあるが 0 件の版は available:true');
if (insNew.eventRows({ events: [{}] }, 200).rows[0].at !== 0) fail('無いキーは 0 のはず');
if (insNew.eventRows({ events: evList }, 2).rows.length !== 2) fail('出来事を n で絞れていない');
// **手元のプロキシの本物の出力** (`snapshot-local.json` の `events`。anomaly 入り)
const localEv = insNew.eventRows(snap.events, 200);
if (!localEv.available) fail(path.basename(snapFile) + ' の /events が読めていない');
if (localEv.rows.length !== ((snap.events || {}).events || []).length) fail('本物の出来事の件数が合わない');
for (let i = 1; i < localEv.rows.length; i++) {
  if (localEv.rows[i - 1].at < localEv.rows[i].at) fail('本物の出来事が新しい順でない');
}
for (const r of localEv.rows) {
  if (typeof r.color !== 'string' || r.color[0] !== '#') fail('本物の出来事の色が読めない: ' + r.kind);
  if (r.anomaly && !r.subject) fail('本物の異常の種類が読めていない: ' + r.text);
  if (r.anomaly && !r.open && !r.pair_at) fail('対になっていないのに続いていない扱い');
}
if (localEv.anomalies !== localEv.rows.filter((r) => r.anomaly).length) fail('本物の異常の数が合わない');

// (3) 匿名化した実データ (T14.35)。この版の雪像には `/daily` も `/events` も `/snapshots` も
//     無いので、まず**「無い版」の分岐**を実データで通し、そのあと `/history?res=3600` を
//     UTC の日で束ねて `/daily` の形に組み直し、**週の足し算**を本物の分布で確かめる。
function checkDailyWeekly() {
  const snapPath = path.join(__dirname, 'testdata', 'deployed-2026-09-16.anon.json');
  if (!fs.existsSync(snapPath)) return null;
  const where = path.basename(snapPath);
  const a = JSON.parse(fs.readFileSync(snapPath, 'utf8'));
  if (insNew.dailyRows(a.daily || null, 14).available) fail(where + ': /daily の無い雪像で available になった');
  if (insNew.eventRows(a.events || null, 200).available) fail(where + ': /events の無い雪像で available になった');
  const empty = insNew.weeklyRows(a.daily || null, a.snapshots || null, 7);
  if (empty.rows.length !== 0 || empty.total.requests !== 0 || empty.available) {
    fail(where + ': /daily の無い雪像で週が 0 にならない');
  }
  // 実データの標本を UTC の日で束ねて `/daily` の 1 行にする (区間の値はそのまま足せる)
  const h = (a.history || {})['3600'] || {};
  const byDay = {};
  for (const s of ins.toSamples(h)) {
    const t = +s.t || 0;
    if (!t) continue;
    const start = Math.floor(t / DAY_SECS) * DAY_SECS;
    const d = byDay[start] || (byDay[start] = {
      day: dayName(start), t: start, secs: 0, samples: 0, requests: 0, bytes: 0, connects: 0,
      connect_p50_ms: 0, connect_p95_ms: 0, dns_misses: 0, dns_ms_sum: 0, dns_miss_ms: 0,
      errors: 0, bursts: 0, active_max: 0, evicted_idle: 0, rss_max: 0, rss_avg: 0, version: a.version || '',
    });
    d.samples++;
    d.secs += +h.interval_secs || 3600;
    d.requests += +s.requests || 0;
    d.bytes += +s.bytes || 0;
    d.connects += +s.connects || 0;
    d.errors += +s.errors || 0;
    d.dns_misses += +s.dns_misses || 0;
    d.dns_ms_sum += +s.dns_ms_sum || 0;
    d.active_max = Math.max(d.active_max, +s.active_max || 0);
    d.rss_max = Math.max(d.rss_max, +s.rss || 0);
  }
  const lines = Object.keys(byDay).sort((x, y) => x - y).map((k) => {
    const d = byDay[k];
    d.dns_miss_ms = d.dns_misses ? d.dns_ms_sum / d.dns_misses : 0;
    d.dns_per_connect = d.connects ? d.dns_misses / d.connects : 0;
    return d;
  });
  if (lines.length === 0) fail(where + ': 日で束ねられなかった');
  const w = insNew.weeklyRows({ days: lines, count: lines.length }, null, 7);
  const hand = (k) => lines.slice(Math.max(0, lines.length - 7)).reduce((x, d) => x + d[k], 0);
  for (const k of ['requests', 'bytes', 'connects', 'errors', 'dns_misses']) {
    if (w.total[k] !== hand(k)) fail(where + ': 週の ' + k + ' が手で足した値と違う: ' + w.total[k] + ' != ' + hand(k));
  }
  if (w.rows.length !== Math.min(7, lines.length)) fail(where + ': 週の行数が合わない');
  const msAll = lines.slice(-7).reduce((x, d) => x + d.dns_ms_sum, 0) / hand('dns_misses');
  if (Math.abs(w.total.dns_miss_ms - msAll) > 1e-6) fail(where + ': 週のミス 1 回が合わない: ' + w.total.dns_miss_ms);
  // **T14.40 (`weekly-report.py`) が同じ実データから出した週の数字と一致すること**
  // (fixture を再デプロイ後の雪像に差し替えたら、この 3 つの数も一緒に直す)
  if (w.total.requests !== 522251) fail(where + ': 週の要求数が T14.40 の 522,251 と違う: ' + w.total.requests);
  if (w.total.errors !== 101) fail(where + ': 週のエラーが T14.40 の 101 と違う: ' + w.total.errors);
  if (w.total.active_max !== 218) fail(where + ': 週の最大同時が T14.40 の 218 と違う: ' + w.total.active_max);
  return { days: w.rows.length, requests: w.total.requests, errors: w.total.errors,
    connects: w.total.connects, misses: w.total.dns_misses, active_max: w.total.active_max,
    from: w.from, to: w.to };
}
const anonWeek = checkDailyWeekly();

console.log(
  'OK: 調査ページの「今日」「今週」「出来事と異常」(T14.44) も通った: /daily ' +
    dr.rows.length +
    ' 日 (新しい順、最後の日 ' +
    dr.rows[0].day +
    ')、週は ' +
    wk.rows.length +
    ' 日ぶんを足して 要求 ' +
    wk.total.requests +
    '・エラー ' +
    wk.total.errors +
    '・ミス率 ' +
    wk.total.dns_per_connect.toFixed(3) +
    '・最大同時 ' +
    wk.total.active_max +
    ' (p50 / p95 は日ごとに ' +
    wk.p50_lo.toFixed(1) +
    '〜' +
    wk.p95_hi.toFixed(1) +
    ' ms、週では出さない)、雪像 ' +
    wk.in_week +
    ' / ' +
    wk.rows.length +
    ' 日。出来事は 作り置き ' +
    er.rows.length +
    ' 件 (異常 ' +
    er.anomalies +
    '・解除 ' +
    er.cleared +
    '・続いている ' +
    er.open +
    ') と ' +
    path.basename(snapFile) +
    ' の本物 ' +
    localEv.rows.length +
    ' 件 (異常 ' +
    localEv.anomalies +
    ')。' +
    (anonWeek === null
      ? '匿名化した実データは無い'
      : '匿名化した実データ (T14.35) は /daily も /events も無い版として通り、' +
        '/history?res=3600 を日で束ねた ' +
        anonWeek.days +
        ' 日 (' +
        anonWeek.from +
        ' → ' +
        anonWeek.to +
        ') で 要求 ' +
        anonWeek.requests +
        '・CONNECT ' +
        anonWeek.connects +
        '・ミス ' +
        anonWeek.misses +
        '・エラー ' +
        anonWeek.errors +
        '・最大同時 ' +
        anonWeek.active_max +
        ' = T14.40 の週の数字と一致した')
);

// (4) 煙試験: **作り物の DOM と fetch で `inspect.html` の `<script>` を丸ごと動かす**。
//     T14.8 はこれをスクラッチでやってリポジトリに入れていなかったので、ここに畳んだ。
//     描画関数 (DOM に触らない側) は上で見たので、ここで見るのは**描く側**:
//     `renderDaily` / `renderWeekly` / `renderEvents` を含めた 1 枚が、
//     (a) 全部の口がある版と (b) `/daily` `/events` `/snapshots` `/hosts/series` が 404 の版
//     (古い版と `--lite`) の**どちらでも例外を出さない**こと。ページは自分の `catch` で
//     例外を「取得失敗」に変えてしまうので、**`lastok` の文字**で成否を見る。
function smokeInspect(route) {
  const els = {};
  const $ = (id) => els[id] || (els[id] = {
    id, textContent: '', innerHTML: '', hidden: false, value: '', checked: false,
    className: '', style: {}, clientWidth: 900, clientHeight: 300, width: 900, height: 300,
    tBodies: [{ innerHTML: '' }], addEventListener() {},
    getContext: () => {
      const noop = () => {};
      return { setTransform: noop, clearRect: noop, beginPath: noop, moveTo: noop, lineTo: noop,
        stroke: noop, fill: noop, fillText: noop, arc: noop, save: noop, restore: noop,
        translate: noop, rotate: noop, closePath: noop, setLineDash: noop };
    },
  });
  const asked = [];
  const sandbox = {
    document: { getElementById: $, body: {} },
    window: { devicePixelRatio: 1, addEventListener() {} },
    getComputedStyle: () => ({ getPropertyValue: () => 'monospace' }),
    fetch: (u) => {
      asked.push(u);
      const d = route(u);
      return d === undefined
        ? Promise.reject(new Error('HTTP 404'))
        : Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(d) });
    },
    setInterval: () => 0,
  };
  const names = Object.keys(sandbox);
  new Function(...names, insJs)(...names.map((n) => sandbox[n]));
  return { $, asked, rows: (id) => (($(id).tBodies[0].innerHTML || '').match(/<tr/g) || []).length };
}
const withAll = smokeInspect((u) => {
  if (u.indexOf('/status') === 0) return snap.status;
  if (u.indexOf('/recent') === 0) return snap.recent;
  if (u.indexOf('/events') === 0) return snap.events;
  if (u.indexOf('/history') === 0) return (snap.history || {})['5'];
  if (u.indexOf('/bursts') === 0) return snap.bursts;
  if (u.indexOf('/clients') === 0) return snap.clients;
  if (u.indexOf('/hosts/series') === 0) return snap.hosts_series;
  if (u.indexOf('/hosts') === 0) return snap.hosts;
  if (u.indexOf('/daily') === 0) return dailyJson;
  if (u.indexOf('/snapshots') === 0) return snapsJson;
  return undefined;
});
// 古い版と `--lite`: `/daily` `/events` `/snapshots` `/hosts/series` は 404、個票は空
const withNone = smokeInspect((u) => {
  if (u.indexOf('/status') === 0) return snap.status;
  if (u.indexOf('/recent') === 0) return { recent: [], count: 0, lite: true };
  if (u.indexOf('/history') === 0) return (snap.history || {})['5'];
  if (u.indexOf('/bursts') === 0) return { bursts: [], lite: true };
  if (u.indexOf('/clients') === 0) return { clients: [], lite: true };
  if (u.indexOf('/hosts') === 0) return { hosts: [] };
  return undefined;
});
process.on('unhandledRejection', (e) => fail('煙試験で拾われない例外: ' + (e && e.message)));
setTimeout(() => {
  for (const [name, r] of [['全部の口がある版', withAll], ['/daily も /events も無い版', withNone]]) {
    const last = r.$('lastok').textContent;
    if (last.indexOf('取得失敗') === 0) fail('煙試験 (' + name + ') で例外: ' + last);
    if (last.indexOf('更新') !== 0) fail('煙試験 (' + name + ') が最後まで動いていない: ' + last);
    for (const id of ['dailyhint', 'weekhint', 'evhint', 'tlhint']) {
      if (!r.$(id).textContent) fail('煙試験 (' + name + '): ' + id + ' が空のまま');
    }
  }
  // 全部ある版: 3 枚に行が出て、`/daily` と `/snapshots` を実際に引いている
  if (withAll.rows('daily') !== 9) fail('煙試験: 「今日」の行が 9 でない: ' + withAll.rows('daily'));
  if (withAll.rows('weekly') !== 7) fail('煙試験: 「今週」の行が 7 でない: ' + withAll.rows('weekly'));
  if (withAll.rows('events') !== ((snap.events || {}).events || []).length) {
    fail('煙試験: 「出来事と異常」の行が実出力の件数と違う: ' + withAll.rows('events'));
  }
  if (!withAll.asked.some((u) => u.indexOf('/daily') === 0)) fail('煙試験: /daily を引いていない');
  if (!withAll.asked.some((u) => u.indexOf('/snapshots') === 0)) fail('煙試験: /snapshots を引いていない');
  if ((withAll.$('slow').tBodies[0].innerHTML.match(/\/explain\?host=/g) || []).length === 0) {
    fail('煙試験: 遅い接続の宛先が /explain へのリンクになっていない');
  }
  // 無い版: 3 枚とも空のまま「記録していません」と断る (`--lite` でもページは開ける)
  for (const id of ['daily', 'weekly', 'events']) {
    if (withNone.rows(id) !== 0) fail('煙試験: 無い版で ' + id + ' に行が出た');
  }
  for (const id of ['dailyhint', 'weekhint', 'evhint']) {
    if (withNone.$(id).textContent.indexOf('記録していません') < 0) {
      fail('煙試験: 無い版の ' + id + ' が「記録していません」と言っていない');
    }
  }
  console.log(
    'OK: 調査ページの煙試験 (作り物の DOM と fetch で <script> を丸ごと) も通った: ' +
      '全部の口がある版は ' + withAll.asked.length + ' 本引いて 今日 ' + withAll.rows('daily') +
      ' 行 / 今週 ' + withAll.rows('weekly') + ' 行 / 出来事 ' + withAll.rows('events') +
      ' 行 / 遅い接続 ' + withAll.rows('slow') + ' 行を描き、' +
      '/daily も /events も無い版 (古い版と --lite) では ' + withNone.asked.length +
      ' 本引いて 3 枚とも「記録していません」と断った'
  );
}, 0);
