// `scripts/check-dashboard.js` の節: 5. canary の配列 (2 の続き) (T34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 5. canary の配列 (T14.10、T14.37 で 5 列目)。`/history` の応答に**別の配列**として付く
// (`{"keys":[...],"samples":[[t,dns_ms,connect_ms,"host",ipv6_connect_ms],...]}`)。描くのは
// T14.8 なので、ここでは**出力の形**だけを見る: 列名・行の長さ・型・時刻が古い順であること。
//
// **列は末尾にしか足さない**約束なので、列名は**先頭からの一致**で見る (T14.37 より前の
// 作り置き = 4 列でも落ちない)。行の長さは `keys` の長さと合っていること。
const CANARY_KEYS = [
  't',
  'canary_dns_ms',
  'canary_connect_ms',
  'canary_host',
  'canary_ipv6_connect_ms',
];
function checkCanary(h) {
  const c = h && h.canary;
  if (c === undefined || c === null) return null; // canary の無い版の出力 (飛ばす)
  const want = CANARY_KEYS;
  if (
    !Array.isArray(c.keys) ||
    c.keys.length < 4 ||
    c.keys.length > want.length ||
    c.keys.some((k, i) => k !== want[i])
  ) {
    fail('canary の keys が ' + want.join(',') + ' の先頭からの一致でない: ' + JSON.stringify(c.keys));
  }
  if (!Array.isArray(c.samples)) fail('canary の samples が配列でない');
  let last = 0;
  for (const row of c.samples) {
    if (!Array.isArray(row) || row.length !== c.keys.length) {
      fail('canary の 1 行が ' + c.keys.length + ' 列でない: ' + JSON.stringify(row));
    }
    const [t, dns, conn, host, v6] = row;
    if (typeof t !== 'number' || !(t > 0)) fail('canary の時刻が epoch 秒でない: ' + t);
    if (t < last) fail('canary の標本が古い順になっていない: ' + t + ' < ' + last);
    last = t;
    if (typeof dns !== 'number' || !(dns >= 0)) fail('canary_dns_ms が数でない: ' + dns);
    if (typeof conn !== 'number' || !(conn >= 0)) fail('canary_connect_ms が数でない: ' + conn);
    if (typeof host !== 'string' || !host) fail('canary_host が空: ' + JSON.stringify(host));
    // IPv6 側は**繋がらなければ null** (AAAA が無い / 黒穴 / off)。T14.37
    if (c.keys.length > 4 && v6 !== null && !(typeof v6 === 'number' && v6 >= 0)) {
      fail('canary_ipv6_connect_ms が数でも null でもない: ' + JSON.stringify(v6));
    }
  }
  return c.samples.length;
}
// 実出力に canary があれば読む (無い版の作り置きでも落ちない)
const canaryRows = checkCanary(hist);
// 作り置きに canary が無い版でも検査そのものが動くことを、架空の 2 点で確かめる
// (IPv6 側は 1 点が `null` = 黒穴、1 点が数 = 生きている)
const fakeCanary = {
  canary: {
    keys: CANARY_KEYS,
    samples: [
      [1789251460, 3, 8, 'a.example.net:443', null],
      [1789251520, 4, 9, 'a.example.net:443', 12],
    ],
  },
};
if (checkCanary(fakeCanary) !== 2) fail('canary の 2 点が読めていない');
// T14.37 より前の 4 列の出力も今までどおり読める
if (
  checkCanary({
    canary: {
      keys: CANARY_KEYS.slice(0, 4),
      samples: [[1789251460, 3, 8, 'a.example.net:443']],
    },
  }) !== 1
) {
  fail('4 列 (T14.37 より前) の canary が読めない');
}
// canary が付いても既存の標本の読み方は変わらない (列は 1 つも動かない)
if (api.toSamples(Object.assign({}, hist, fakeCanary)).length !== samples.length) {
  fail('canary を足したら標本の数が変わった');
}

