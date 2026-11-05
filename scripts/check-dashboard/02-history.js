// `scripts/check-dashboard.js` の節: 2. /history を読む関数 (切り出し・分位点・合成・山) (T34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 2. `/history` と `/status` と個票を読む関数を取り出して動かす (DOM に触らない 8 つだけ)
const names = [
  'toSamples',
  'winQuantile',
  'mergeWindows',
  'dnsStats',
  'badHosts',
  'peak',
  'errorRows',
  // 「最近のエラー」の 1 行の HTML (T21.5。まとめた行の ×N と最後の時刻)
  'errorRowHtml',
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
  // T29.18: 近い接続元 (RTT < 100 ms) の待ち
  'nearWaitKpi',
  // T29.19: キャッシュ命中率に母数
  'hitKpi',
  // 100% の横棒 1 本を組み立てる側 (T15.0 (14) で tooltip の整形を呼ぶ側に渡せるようにした)
  'stackHtml',
  'esc',
  'fmtMs',
  'fmtMsFine',
  'fmtNum',
  'fmtDur',
  'bootKind',
  // 設定の帯・版の隣のビルド・`/slo` の tooltip (T29.27)
  'settingsLines',
  'versionLabel',
  'sloTitle',
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

