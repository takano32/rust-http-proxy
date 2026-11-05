// `scripts/check-dashboard.js` の節: 3. /status を読む関数 (名前解決の KPI と悪いホスト) (T.34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
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
// T.32.17: ミス率は名前解決に失敗した試みのミスを除く (`/history?summary=1&since=restart` の
// `dns_fail_misses ÷ dns_misses` を `/status` のミスから引き、`stale_hits` を当たりに数える)。
// 数字は 0.30.4 の 48 時間の雪像 (`2026-10-31T063104Z`): 今までの 64.5% が 5.1% になる
const realDns = { dns: { hits: 17266, misses: 31358, stale_hits: 12591 } };
const realSum = { dns_misses: 31348, dns_fail_misses: 29742 };
const rdn = api.dnsStats(realDns, realSum);
if (rdn.rate.toFixed(1) !== '5.1') fail('失敗を除いたミス率が 5.1% にならない: ' + rdn.rate);
if (rdn.raw_rate.toFixed(1) !== '64.5') fail('失敗も入れたミス率が 64.5% にならない: ' + rdn.raw_rate);
if (rdn.failed !== 29751) fail('失敗のミスの見積もりが合わない: ' + rdn.failed);
// 要約が無い / `dns_fail_misses` を持たない古い版は今までどおり misses ÷ (hits + misses)
for (const old of [undefined, null, { dns_misses: 31348 }, { dns_misses: 0, dns_fail_misses: 0 }]) {
  const o = api.dnsStats(realDns, old);
  if (o.rate !== o.raw_rate || o.failed !== null) fail('欄の無い版でミス率が変わった: ' + JSON.stringify(old));
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

