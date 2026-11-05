// `scripts/check-dashboard.js` の節: 10. 調査ページ (inspect.html) の描画関数 (T34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 10. 「調査」ページ (`crates/web/src/inspect.html`。T14.8) の描画関数。
//    `/dashboard` と同じ作法で **DOM に触らない 9 つ**を名前で抜き出し、
//    (1) 手元のベンチで取った `/snapshot` の実出力、(2) デプロイ先の雪像 (`/status` と
//    `/history`)、(3) まだ実出力に無い欄の架空の見本、の 3 つで通す。
const insHtml = fs.readFileSync(
  path.join(__dirname, '..', 'crates/web/src/inspect.html'),
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

