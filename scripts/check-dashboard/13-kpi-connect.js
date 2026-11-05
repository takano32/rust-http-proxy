// `scripts/check-dashboard.js` の節: 12. KPI「CONNECT 確立 p50」 (T34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
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

