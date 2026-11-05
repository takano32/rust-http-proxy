// `scripts/check-dashboard.js` の節: 7. /events の時系列 (T.34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 7. `/events` の時系列 (T14.11)。12 種で固定なので、綴りが増減したらここで気づく
// (`anomaly` は異常の自動検知が書く 1 件 (T14.23)、`new_client` は初めて見た接続元 (T14.54))
const EVENT_KINDS = [
  'start', 'reload', 'blocklist', 'ipv6', 'pressure', 'ballast',
  'state_file', 'evict', 'emfile', 'shutdown', 'anomaly', 'new_client',
];

function eventRows(json, since) {
  const events = (json && json.events) || [];
  return events
    .filter((e) => (e.at || 0) >= (since || 0))
    .map((e) => ({ at: e.at || 0, kind: e.kind || '', text: e.text || '' }));
}

const eventJson = {
  events: [
    { at: 1789251470, kind: 'reload', text: 'PROXY_TIMEOUT_SECS 30 \u2192 10' },
    { at: 1789251468, kind: 'ballast', text: 'ballast +256 MiB -> 256 MiB (memory 0 MiB, disk 256 MiB)' },
    { at: 1789251465, kind: 'start', text: 'version 0.1.0+abcdef1 on port 8080 (profile default, cache on, timeout 30s, max conns 240)' },
    { at: 1789251460, kind: 'anomaly', text: 'connect_p95: connect p95 138 ms over 5m is 14.1x the 1h baseline 9.8 ms (120 of 1328 connects)' },
  ],
  count: 4, kept: 4, capacity: 512, recorded: 4, since: 0, kinds: EVENT_KINDS, truncated: false,
};
const evs = eventRows(eventJson, 0);
if (evs.length !== 4) fail('eventRows の件数が合わない');
if (evs.some((e, i) => i > 0 && evs[i - 1].at < e.at)) fail('出来事が新しい順でない');
if (!evs.every((e) => EVENT_KINDS.includes(e.kind))) fail('知らない種類がある');
if (eventJson.kinds.length !== 12) fail('種類は 12 種で固定のはず');
if (eventJson.kinds.join(',') !== EVENT_KINDS.join(',')) fail('種類の綴りか並びが変わった');
if (evs.some((e) => e.text.length > 128)) fail('説明が 128 バイトを超えた');
if (eventRows(eventJson, 1789251468).length !== 2) fail('since で絞れていない');
if (eventRows({}, 0).length !== 0) fail('空でも例外なく 0 件のはず');
if (eventRows(null, 0).length !== 0) fail('null でも例外なく 0 件のはず');
if (eventRows({ events: [{}] }, 0)[0].kind !== '') fail('無いキーは空のはず');

