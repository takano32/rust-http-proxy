// `scripts/check-dashboard.js` の節: 5. /history の closed と /bursts の写真 (T34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
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

