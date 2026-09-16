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
//   6. **カーネルと cgroup の窓** (`/history` の `kernel` と `/status` の `kernel`。T14.12)
//   7. **`/profile` を読む関数** (段階・スレッド・ロック) が実出力と合っていること (T14.3)
//
// 使い方: node scripts/check-dashboard.js [/history の実出力.json] [/status の実出力.json] [/profile の実出力.json]
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

// 7. `/profile` を読む関数 (段階・スレッド・ロック。T14.3)
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
    ' 件'
);
