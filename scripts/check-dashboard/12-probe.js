// `scripts/check-dashboard.js` の節: 11. 端末から測るページ (probe.html) (T.34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 11. 「端末から測る」ページ (`crates/web/src/probe.html`。T14.33) の関数。
//     ブラウザが無いので、(1) 作り物の数列で `median`、(2) `/snapshot` の中の `/status` の
//     実出力 (5 つ目の引数) で `pick` と `render` を通す。**この 3 つは DOM にも fetch にも
//     触らない**ので、ここで「経由している / していない」の判定まで確かめられる。
const probeHtml = fs.readFileSync(
  path.join(__dirname, '..', 'crates/web/src/probe.html'),
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

