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
//   7. **`/events` の時系列**が読めること (11 種の綴り・新しい順・`?since=` の絞り。T14.11 / T14.23)
//   8. **`/profile` を読む関数** (段階・スレッド・ロック) が実出力と合っていること (T14.3)
//   9. **`/history` の `transfer`** (転送速度と半閉じの分布) が読めること (区間の数・合計と件数の一致。T14.25)
//  10. **「調査」ページ (`inspect.html`) の描画関数**が `/snapshot` の実出力で例外なく通ること
//      (タイムライン・遅い接続・山・接続元・RTT の散布・起動からの窓・出来事の印。T14.8)
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
const api = pick(js, names, 'dashboard.html');

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

// 5. canary の配列 (T14.10)。`/history` の応答に**別の配列**として付く
// (`{"keys":[...],"samples":[[t,dns_ms,connect_ms,"host"],...]}`)。描くのは T14.8 なので、
// ここでは**出力の形**だけを見る: 列名・行の長さ・型・時刻が古い順であること。
function checkCanary(h) {
  const c = h && h.canary;
  if (c === undefined || c === null) return null; // canary の無い版の出力 (飛ばす)
  const want = ['t', 'canary_dns_ms', 'canary_connect_ms', 'canary_host'];
  if (!Array.isArray(c.keys) || c.keys.join(',') !== want.join(',')) {
    fail('canary の keys が ' + want.join(',') + ' でない: ' + JSON.stringify(c.keys));
  }
  if (!Array.isArray(c.samples)) fail('canary の samples が配列でない');
  let last = 0;
  for (const row of c.samples) {
    if (!Array.isArray(row) || row.length !== want.length) {
      fail('canary の 1 行が ' + want.length + ' 列でない: ' + JSON.stringify(row));
    }
    const [t, dns, conn, host] = row;
    if (typeof t !== 'number' || !(t > 0)) fail('canary の時刻が epoch 秒でない: ' + t);
    if (t < last) fail('canary の標本が古い順になっていない: ' + t + ' < ' + last);
    last = t;
    if (typeof dns !== 'number' || !(dns >= 0)) fail('canary_dns_ms が数でない: ' + dns);
    if (typeof conn !== 'number' || !(conn >= 0)) fail('canary_connect_ms が数でない: ' + conn);
    if (typeof host !== 'string' || !host) fail('canary_host が空: ' + JSON.stringify(host));
  }
  return c.samples.length;
}
// 実出力に canary があれば読む (無い版の作り置きでも落ちない)
const canaryRows = checkCanary(hist);
// 作り置きに canary が無い版でも検査そのものが動くことを、架空の 2 点で確かめる
const fakeCanary = {
  canary: {
    keys: ['t', 'canary_dns_ms', 'canary_connect_ms', 'canary_host'],
    samples: [
      [1789251460, 3, 8, 'a.example.net:443'],
      [1789251520, 4, 9, 'a.example.net:443'],
    ],
  },
};
if (checkCanary(fakeCanary) !== 2) fail('canary の 2 点が読めていない');
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

// 7. `/events` の時系列 (T14.11)。11 種で固定なので、綴りが増減したらここで気づく
// (`anomaly` は異常の自動検知が書く 1 件。T14.23)
const EVENT_KINDS = [
  'start', 'reload', 'blocklist', 'ipv6', 'pressure', 'ballast',
  'state_file', 'evict', 'emfile', 'shutdown', 'anomaly',
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
if (eventJson.kinds.length !== 11) fail('種類は 11 種で固定のはず');
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
  keys: ['t', 'tunnels', 'speed_n', 'speed', 'half_close_n', 'half_close', 'bytes_sum', 'relay_ms_sum', 'half_close_ms_sum'],
  speed_bounds_bps: [1024, 4096, 16384, 65536, 262144, 1048576, 4194304, 16777216, 67108864, 268435456, 1073741824, 4294967296],
  half_close_bounds_ms: [1, 4, 16, 64, 256, 1024, 4096, 16384, 65536, 262144, 1048576, 4194304],
  min_bytes: 1024,
  samples: [
    [1789251460, 3, 2, [0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0], 1, [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0], 1258291, 4200, 150],
    [1789251465, 1, 1, [0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0], 0, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 20971520, 2000, 0],
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
if (Buffer.byteLength(insHtml) > 64 * 1024) {
  fail('inspect.html が 64 KiB を超えた: ' + Buffer.byteLength(insHtml) + ' B');
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
    'timeline', 'eventMarks', 'slowRows', 'burstCards', 'clientRows', 'rttScatter', 'sinceStart'],
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
    if (!(p.rtt > 0) || !(p.connect > 0)) fail(where + ': 0 以下の点が入った');
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
    ],
  },
  {}
);
if (scFake.count !== 2 || scFake.skipped !== 3) fail('描く行の選び方が違う: ' + JSON.stringify([scFake.count, scFake.skipped]));
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
const sum = ins.sinceStart(summaryJson, 0, 0);
if (sum.mode !== 'summary') fail('?summary=1 の応答を要約として読めていない');
if (sum.connects !== 4000 || sum.p50 !== 8.3 || sum.p95 !== 80.7) fail('要約の数が読めていない');
if (sum.dns_per_connect !== 0.55 || sum.dns_avg !== 11.5) fail('要約の名前解決が読めていない');
if (sum.active_max !== 218 || sum.errors !== 99) fail('要約の山とエラーが読めていない');
if (sum.to - sum.from !== 3600) fail('要約の期間が読めていない');

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
    ')'
);
