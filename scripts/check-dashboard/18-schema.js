// `scripts/check-dashboard.js` の節: 14. 応答の形の版 (schema) (T.34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
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
