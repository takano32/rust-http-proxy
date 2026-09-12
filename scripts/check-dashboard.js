#!/usr/bin/env node
// ダッシュボード (`crates/endpoints/src/web/dashboard.html`) の JS を、ブラウザ無しで確かめる。
//
// ブラウザが無い環境でも壊れに気づけるように、見るのは 2 つだけ:
//   1. <script> の中身が構文として通ること (`new Function` = `node --check` と同じ判定)
//   2. **`/history` の配列の配列を読む関数の読み方が、実際の出力と合っていること**
//      (キーの並び・入れ子の配列・区間の分位点。T12.4 (5))
//
// 使い方: node scripts/check-dashboard.js [/history の実出力.json]
//   引数を省くと下の作り置き (手元のプロキシから取った実出力) を使う。
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

// 2. `/history` を読む関数を取り出して動かす (DOM に触らない 3 つだけ)
const names = ['toSamples', 'winQuantile', 'mergeWindows'];
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

console.log(
  'OK: dashboard.html の JS は構文が通り、/history ' +
    samples.length +
    ' 標本 (CONNECT のある標本 ' +
    checked +
    ' 件、合計 ' +
    total +
    ' 本) を読めた。直近の p50 = ' +
    kpi.toFixed(2) +
    ' ms'
);
