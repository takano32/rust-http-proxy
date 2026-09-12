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
//
// 使い方: node scripts/check-dashboard.js [/history の実出力.json] [/status の実出力.json]
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
];
let src = '';
for (const n of names) {
  const at = js.indexOf('function ' + n + '(');
  if (at < 0) fail(n + ' が dashboard.html に無い (名前を変えたらこの確認も直すこと)');
  // 対応する閉じ括弧まで
  let depth = 0, i = js.indexOf('{', at), end = -1;
  for (; i < js.length; i++) {
    if (js[i] === '{') depth++;
    else if (js[i] === '}' && --depth === 0) { end = i + 1; break; }
  }
  if (end < 0) fail(n + ' の括弧が閉じていない');
  src += js.slice(at, end) + '\n';
}
const api = new Function(src + 'return {' + names.join(',') + '};')();

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
    { id: 3, client: '198.51.100.7', target: 'a.example.net:443', kind: 'connect', state: 'parked', age_secs: 120, bytes: 4096, fds: 2 },
    { id: 9, client: '198.51.100.9', target: '', kind: 'http', state: 'serving', age_secs: 0, bytes: 0, fds: 1 },
    { id: 5, client: '198.51.100.8', target: 'b.example.net:443', kind: 'connect', state: 'relaying', age_secs: 900, bytes: 1048576, fds: 2 },
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
if (api.connRows(connJson, 1).rows.length !== 1) fail('n で絞れていない');
const lite = api.connRows({ connections: [], count: 0, lite: true }, 50);
if (lite.rows.length !== 0 || !lite.lite) fail('lite の空一覧が読めていない');
if (api.connRows({}, 50).rows.length !== 0) fail('空でも例外なく 0 件のはず');
if (api.connRows(null, 50).count !== 0) fail('null でも例外なく 0 件のはず');

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
    ' 本) も読めた'
);
