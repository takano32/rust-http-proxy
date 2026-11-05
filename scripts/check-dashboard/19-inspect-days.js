// `scripts/check-dashboard.js` の節: 15. 調査ページの今日・今週・出来事と異常 (と煙試験) (T.34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 15. 「調査」ページの「今日」「今週」「出来事と異常」(T14.44)。
//     `dailyRows` (`/daily`。T14.20)・`weeklyRows` (`/daily` の 7 行をブラウザの中で足す +
//     `/snapshots` の一覧。T14.40 / T14.34)・`eventRows` (`/events` の表。T14.11 / T14.23) を
//     3 つの入力で回す: (1) `/daily` と `/snapshots` の作り置き (9 日ぶん = 週の窓より 2 日多い)、
//     (2) **`snapshot-local.json` の `events`** (手元のプロキシの本物の出力。`anomaly` 入り)、
//     (3) **匿名化した実データ** (T14.35。この版の雪像には `/daily` も `/events` も無いので
//     **「無い版」の分岐**を実データで通し、`/history?res=3600` を UTC の日で束ねて
//     「週の足し算」を本物の分布で確かめる)。
const insNew = pick(insJs, ['num', 'dailyRows', 'weeklyRows', 'eventRows'], 'inspect.html', insPrelude);
const DAY_SECS = 86400;
const dayName = (t) => new Date(t * 1000).toISOString().slice(0, 10);

// (1) `/daily` の作り置き。**T14.20 が書く 1 行の形そのまま** (欄が増えたらここも増やす)
const dayFrom = Math.floor(snapAt / DAY_SECS) * DAY_SECS - 8 * DAY_SECS;
const dailyLines = [];
for (let i = 0; i < 9; i++) {
  const t = dayFrom + i * DAY_SECS;
  const connects = 1000 + i * 100;
  const misses = 500 + i * 10;
  dailyLines.push({
    day: dayName(t), t,
    // 4 日目だけ半日しか動いていない日 (「見ていた 50%」の行)
    secs: i === 3 ? DAY_SECS / 2 : DAY_SECS, samples: i === 3 ? 8640 : 17280,
    requests: 10000 + i * 1000, bytes: 1000000000 + i, connects,
    connect_p50_ms: 8 + i * 0.1, connect_p95_ms: 80 + i,
    dns_misses: misses, dns_per_connect: misses / connects, dns_miss_ms: 10 + i,
    errors: i === 1 ? 85 : 0, bursts: i === 1 ? 4 : 0, active_max: i === 1 ? 218 : 20,
    evicted_idle: 0, rss_max: 20000000 + i, rss_avg: 18000000 + i, version: '0.1.0+abc1234',
  });
}
const dailyJson = {
  days: dailyLines, count: 9, shown: 9, bytes: 3024, max_bytes: 2 * 1024 * 1024,
  max_line: 512, path: '/home/u/.rust-http-proxy.daily.jsonl', truncated: false,
};
// `/snapshots` の作り置き (窓の 7 日のうち 5 日ぶんがディスクにある)
const snapsJson = {
  files: dailyLines.slice(2, 7).map((d) => ({ date: d.day, bytes: 17871 + d.t % 7, t: d.t + DAY_SECS })),
  count: 5, shown: 5, bytes: 89355, days: 30, max_bytes: 4 * 1024 * 1024,
  dir: '/home/u/.rust-http-proxy/snapshots', truncated: false,
};

const dr = insNew.dailyRows(dailyJson, 14);
if (dr.rows.length !== 9) fail('/daily の行が読めていない: ' + dr.rows.length);
if (!dr.available) fail('/daily の口があるのに available:false');
if (dr.rows[0].day !== dailyLines[8].day) fail('/daily が新しい順でない: ' + dr.rows[0].day);
for (let i = 1; i < dr.rows.length; i++) {
  if (dr.rows[i - 1].t <= dr.rows[i].t) fail('/daily が新しい順でない');
}
if (dr.rows[0].coverage !== 100) fail('丸 1 日の行が 100% でない: ' + dr.rows[0].coverage);
const halfDay = dr.rows.filter((r) => r.day === dailyLines[3].day)[0];
if (Math.abs(halfDay.coverage - 50) > 1e-9) fail('見ていた割合が secs ÷ 86400 でない: ' + halfDay.coverage);
if (dr.rows[0].error_rate !== 0) fail('エラー 0 の日の率が 0 でない');
if (insNew.dailyRows(dailyJson, 3).rows.length !== 3) fail('/daily を n で絞れていない');
if (insNew.dailyRows(null, 7).available !== false) fail('口の無い版 (404) は available:false のはず');
if (insNew.dailyRows(null, 7).rows.length !== 0) fail('口の無い版でも 0 行のはず');
if (insNew.dailyRows({ days: [], count: 0 }, 7).available !== true) {
  fail('口はあるが 1 行も書いていない版は available:true のはず');
}
if (insNew.dailyRows({ days: [{}] }, 7).rows[0].requests !== 0) fail('無いキーは 0 のはず');
// `dns_per_connect` を書いていない行 (手で作った `/daily`) はミス ÷ CONNECT を自分で出す
const calc = insNew.dailyRows({ days: [{ day: '2026-09-16', t: 1, connects: 200, dns_misses: 50 }] }, 7).rows[0];
if (calc.dns_per_connect !== 0.25) fail('ミス率を自分で出せていない: ' + calc.dns_per_connect);
if (insNew.dailyRows({ days: [{ day: 'x', t: 1 }] }, 7).rows[0].dns_per_connect !== null) {
  fail('CONNECT 0 の日はミス率を出さない (null) はず');
}
// T.32.17: 失敗を除いた率の欄 (`dns_resolved_per_connect`)。無ければ `dns_fail_misses` から出し、
// どちらも無い古い行は null (表は今までの `dns_per_connect` を出す)
const res1 = insNew.dailyRows({ days: [{ day: 'a', t: 1, connects: 200, dns_misses: 50,
  dns_per_connect: 0.25, dns_resolved_per_connect: 0.04 }] }, 7).rows[0];
if (res1.dns_resolved_per_connect !== 0.04 || res1.dns_per_connect !== 0.25) {
  fail('/daily の dns_resolved_per_connect を読めていない');
}
const res2 = insNew.dailyRows({ days: [{ day: 'b', t: 1, connects: 200, dns_misses: 50, dns_fail_misses: 40 }] }, 7).rows[0];
if (res2.dns_resolved_per_connect !== 0.05) fail('dns_fail_misses から率を出せていない: ' + res2.dns_resolved_per_connect);
if (calc.dns_resolved_per_connect !== null) fail('欄の無い古い行は null のはず');

// 「今週」: `/daily` の 7 行を足したもの。**足せるものだけ足す** (分位点は足さない)
const wk = insNew.weeklyRows(dailyJson, snapsJson, 7);
const want7 = dailyLines.slice(2);
const sum7 = (k) => want7.reduce((a, d) => a + d[k], 0);
if (wk.rows.length !== 7) fail('週が 7 行でない: ' + wk.rows.length);
if (wk.rows[0].day !== want7[0].day) fail('週が古い順でない: ' + wk.rows[0].day);
for (let i = 1; i < wk.rows.length; i++) if (wk.rows[i - 1].t >= wk.rows[i].t) fail('週が古い順でない');
for (const k of ['requests', 'bytes', 'connects', 'errors', 'dns_misses', 'bursts', 'secs', 'samples']) {
  if (wk.total[k] !== sum7(k)) fail('週の ' + k + ' が 7 日の和でない: ' + wk.total[k] + ' != ' + sum7(k));
}
if (wk.total.active_max !== Math.max.apply(null, want7.map((d) => d.active_max))) {
  fail('週の山は日ごとの最大のはず: ' + wk.total.active_max);
}
if (Math.abs(wk.total.dns_per_connect - sum7('dns_misses') / sum7('connects')) > 1e-12) {
  fail('週のミス率がミス ÷ CONNECT でない: ' + wk.total.dns_per_connect);
}
const msWant = want7.reduce((a, d) => a + d.dns_miss_ms * d.dns_misses, 0) / sum7('dns_misses');
if (Math.abs(wk.total.dns_miss_ms - msWant) > 1e-9) {
  fail('ミス 1 回はミスの数で重みを付けた平均のはず: ' + wk.total.dns_miss_ms + ' != ' + msWant);
}
if (Math.abs(wk.total.error_rate - (sum7('errors') / sum7('requests')) * 100) > 1e-12) {
  fail('週のエラー率が合わない: ' + wk.total.error_rate);
}
// **週の p50 / p95 は出さない** (区間ヒストグラムが `/daily` に無いので足せない。T14.40 と同じ)
if (wk.total.p50 !== null || wk.total.p95 !== null) fail('週の分位点を出してしまっている');
if (wk.p50_lo !== Math.min.apply(null, want7.map((d) => d.connect_p50_ms))) fail('日ごとの p50 の下が違う');
if (wk.p95_hi !== Math.max.apply(null, want7.map((d) => d.connect_p95_ms))) fail('日ごとの p95 の上が違う');
if (wk.from !== want7[0].day || wk.to !== want7[6].day) fail('週の期間が違う: ' + wk.from + ' → ' + wk.to);
if (wk.missing !== 0 || wk.gaps !== 0) fail('7 日そろっているのに足りない日が出た');
// `/snapshots` の一覧と日を突き合わせる (この週は 5 枚)
if (wk.in_week !== 5) fail('この週の雪像が 5 枚でない: ' + wk.in_week);
if (!wk.rows[0].snapshot || wk.rows[0].snapshot.date !== wk.rows[0].day) fail('雪像が日に結び付いていない');
if (wk.rows[6].snapshot !== null) fail('雪像の無い日に雪像が付いた');
if (wk.snapshot_days !== 30 || !wk.has_snapshots) fail('/snapshots の残す日数が読めていない');
// 足りない日・抜けた日・無い版
const thin = insNew.weeklyRows({ days: dailyLines.slice(0, 3) }, null, 7);
if (thin.rows.length !== 3 || thin.missing !== 4) fail('足りない日が 4 日でない: ' + thin.missing);
if (thin.has_snapshots || thin.snapshots !== 0) fail('/snapshots の無い版で枚数が出た');
const gappy = insNew.weeklyRows({ days: [dailyLines[0], dailyLines[2], dailyLines[3]] }, null, 7);
if (gappy.gaps !== 1) fail('間で抜けた 1 日を数えていない: ' + gappy.gaps);
const noDaily = insNew.weeklyRows(null, null, 7);
if (noDaily.available || noDaily.rows.length !== 0 || noDaily.total.requests !== 0) {
  fail('/daily の無い版で 0 にならない');
}
if (noDaily.total.dns_per_connect !== null || noDaily.p50_lo !== null) fail('0 日なのに値が出た');

// (2) 「出来事と異常」。立った異常と `cleared:` を対にする (T14.23)
const evAt = snapAt - 3600;
const evList = [
  { at: evAt + 900, kind: 'anomaly', text: 'cleared: dns_slow after 6m (dns miss 12 ms avg over 5m; 3 misses)' },
  { at: evAt + 600, kind: 'anomaly', text: 'errors: 6 errors in 5m (threshold 5): dns 6' },
  { at: evAt + 300, kind: 'anomaly', text: 'dns_slow: dns miss 693 ms avg over 5m (threshold 100 ms; 1 misses, 693 ms total)' },
  { at: evAt + 120, kind: 'brand_new_kind', text: '知らない綴りの出来事' },
  { at: evAt, kind: 'start', text: 'version 0.1.0 on port 18099' },
];
const er = insNew.eventRows(
  { events: evList, count: 5, kept: 5, capacity: 512, recorded: 5, persisted: true, restored: 2, truncated: false },
  200
);
if (er.rows.length !== 5) fail('出来事の件数が合わない: ' + er.rows.length);
if (!er.available) fail('/events があるのに available:false');
for (let i = 1; i < er.rows.length; i++) if (er.rows[i - 1].at < er.rows[i].at) fail('出来事が新しい順でない');
const fired = er.rows.filter((r) => r.subject === 'dns_slow' && r.anomaly)[0];
const clr = er.rows[0];
if (!fired) fail('立った異常 (dns_slow) が読めていない');
if (!clr.cleared || clr.subject !== 'dns_slow') fail('cleared: の種類が読めていない: ' + clr.subject);
if (clr.pair_at !== fired.at || fired.pair_at !== clr.at) fail('cleared: が対になっていない');
if (fired.secs !== 600 || clr.secs !== 600) fail('続いた長さが 600 秒でない: ' + fired.secs);
if (fired.open) fail('解除された異常が「続いている」ままになっている');
if (clr.color === fired.color) fail('anomaly と cleared: の色が同じ');
const still = er.rows.filter((r) => r.subject === 'errors')[0];
if (!still.open || still.secs !== null) fail('解除の無い異常は続いている扱いのはず');
if (er.anomalies !== 2 || er.cleared !== 1) fail('異常 / 解除の数が合わない: ' + er.anomalies + '/' + er.cleared);
if (er.open !== 1 || er.open_kinds.join() !== 'errors') fail('続いている異常が 1 件 (errors) でない');
const unknownEv = er.rows.filter((r) => r.kind === 'brand_new_kind')[0];
if (unknownEv.known !== false || unknownEv.color !== '#8b91a5') fail('知らない綴りは既定の色のはず');
if (unknownEv.anomaly || unknownEv.subject !== '') fail('anomaly でない行に種類が付いた');
if (er.kinds.reduce((a, k) => a + k.count, 0) !== er.rows.length) fail('種類の内訳が件数と合わない');
// 既知の綴り (T14.11 の 10 種 + T14.23 の `anomaly` + T14.54 の `new_client`) は
// どれも「知っている」側に入る
const allKinds = insNew.eventRows({ events: EVENT_KINDS.map((k, i) => ({ at: evAt + i, kind: k, text: k })) }, 200);
if (allKinds.kinds.length !== EVENT_KINDS.length) fail(EVENT_KINDS.length + ' 種が読めていない: ' + allKinds.kinds.length);
for (const r of allKinds.rows) if (!r.known) fail('既知の綴りのはずが知らない綴りになった: ' + r.kind);
if (insNew.eventRows(null, 200).available !== false) fail('/events の無い版は available:false のはず');
if (insNew.eventRows(null, 200).rows.length !== 0) fail('null でも 0 件のはず');
if (insNew.eventRows({ events: [] }, 200).available !== true) fail('口はあるが 0 件の版は available:true');
if (insNew.eventRows({ events: [{}] }, 200).rows[0].at !== 0) fail('無いキーは 0 のはず');
if (insNew.eventRows({ events: evList }, 2).rows.length !== 2) fail('出来事を n で絞れていない');
// **手元のプロキシの本物の出力** (`snapshot-local.json` の `events`。anomaly 入り)
const localEv = insNew.eventRows(snap.events, 200);
if (!localEv.available) fail(path.basename(snapFile) + ' の /events が読めていない');
if (localEv.rows.length !== ((snap.events || {}).events || []).length) fail('本物の出来事の件数が合わない');
for (let i = 1; i < localEv.rows.length; i++) {
  if (localEv.rows[i - 1].at < localEv.rows[i].at) fail('本物の出来事が新しい順でない');
}
for (const r of localEv.rows) {
  if (typeof r.color !== 'string' || r.color[0] !== '#') fail('本物の出来事の色が読めない: ' + r.kind);
  if (r.anomaly && !r.subject) fail('本物の異常の種類が読めていない: ' + r.text);
  if (r.anomaly && !r.open && !r.pair_at) fail('対になっていないのに続いていない扱い');
}
if (localEv.anomalies !== localEv.rows.filter((r) => r.anomaly).length) fail('本物の異常の数が合わない');

// (3) 匿名化した実データ (T14.35)。この版の雪像には `/daily` も `/events` も `/snapshots` も
//     無いので、まず**「無い版」の分岐**を実データで通し、そのあと `/history?res=3600` を
//     UTC の日で束ねて `/daily` の形に組み直し、**週の足し算**を本物の分布で確かめる。
function checkDailyWeekly() {
  const snapPath = path.join(__dirname, 'testdata', 'deployed-2026-09-16.anon.json');
  if (!fs.existsSync(snapPath)) return null;
  const where = path.basename(snapPath);
  const a = JSON.parse(fs.readFileSync(snapPath, 'utf8'));
  if (insNew.dailyRows(a.daily || null, 14).available) fail(where + ': /daily の無い雪像で available になった');
  if (insNew.eventRows(a.events || null, 200).available) fail(where + ': /events の無い雪像で available になった');
  const empty = insNew.weeklyRows(a.daily || null, a.snapshots || null, 7);
  if (empty.rows.length !== 0 || empty.total.requests !== 0 || empty.available) {
    fail(where + ': /daily の無い雪像で週が 0 にならない');
  }
  // 実データの標本を UTC の日で束ねて `/daily` の 1 行にする (区間の値はそのまま足せる)
  const h = (a.history || {})['3600'] || {};
  const byDay = {};
  for (const s of ins.toSamples(h)) {
    const t = +s.t || 0;
    if (!t) continue;
    const start = Math.floor(t / DAY_SECS) * DAY_SECS;
    const d = byDay[start] || (byDay[start] = {
      day: dayName(start), t: start, secs: 0, samples: 0, requests: 0, bytes: 0, connects: 0,
      connect_p50_ms: 0, connect_p95_ms: 0, dns_misses: 0, dns_ms_sum: 0, dns_miss_ms: 0,
      errors: 0, bursts: 0, active_max: 0, evicted_idle: 0, rss_max: 0, rss_avg: 0, version: a.version || '',
    });
    d.samples++;
    d.secs += +h.interval_secs || 3600;
    d.requests += +s.requests || 0;
    d.bytes += +s.bytes || 0;
    d.connects += +s.connects || 0;
    d.errors += +s.errors || 0;
    d.dns_misses += +s.dns_misses || 0;
    d.dns_ms_sum += +s.dns_ms_sum || 0;
    d.active_max = Math.max(d.active_max, +s.active_max || 0);
    d.rss_max = Math.max(d.rss_max, +s.rss || 0);
  }
  const lines = Object.keys(byDay).sort((x, y) => x - y).map((k) => {
    const d = byDay[k];
    d.dns_miss_ms = d.dns_misses ? d.dns_ms_sum / d.dns_misses : 0;
    d.dns_per_connect = d.connects ? d.dns_misses / d.connects : 0;
    return d;
  });
  if (lines.length === 0) fail(where + ': 日で束ねられなかった');
  const w = insNew.weeklyRows({ days: lines, count: lines.length }, null, 7);
  const hand = (k) => lines.slice(Math.max(0, lines.length - 7)).reduce((x, d) => x + d[k], 0);
  for (const k of ['requests', 'bytes', 'connects', 'errors', 'dns_misses']) {
    if (w.total[k] !== hand(k)) fail(where + ': 週の ' + k + ' が手で足した値と違う: ' + w.total[k] + ' != ' + hand(k));
  }
  if (w.rows.length !== Math.min(7, lines.length)) fail(where + ': 週の行数が合わない');
  const msAll = lines.slice(-7).reduce((x, d) => x + d.dns_ms_sum, 0) / hand('dns_misses');
  if (Math.abs(w.total.dns_miss_ms - msAll) > 1e-6) fail(where + ': 週のミス 1 回が合わない: ' + w.total.dns_miss_ms);
  // **T14.40 (`weekly-report.py`) が同じ実データから出した週の数字と一致すること**
  // (fixture を再デプロイ後の雪像に差し替えたら、この 3 つの数も一緒に直す)
  if (w.total.requests !== 522251) fail(where + ': 週の要求数が T14.40 の 522,251 と違う: ' + w.total.requests);
  if (w.total.errors !== 101) fail(where + ': 週のエラーが T14.40 の 101 と違う: ' + w.total.errors);
  if (w.total.active_max !== 218) fail(where + ': 週の最大同時が T14.40 の 218 と違う: ' + w.total.active_max);
  return { days: w.rows.length, requests: w.total.requests, errors: w.total.errors,
    connects: w.total.connects, misses: w.total.dns_misses, active_max: w.total.active_max,
    from: w.from, to: w.to };
}
const anonWeek = checkDailyWeekly();

console.log(
  'OK: 調査ページの「今日」「今週」「出来事と異常」(T14.44) も通った: /daily ' +
    dr.rows.length +
    ' 日 (新しい順、最後の日 ' +
    dr.rows[0].day +
    ')、週は ' +
    wk.rows.length +
    ' 日ぶんを足して 要求 ' +
    wk.total.requests +
    '・エラー ' +
    wk.total.errors +
    '・ミス率 ' +
    wk.total.dns_per_connect.toFixed(3) +
    '・最大同時 ' +
    wk.total.active_max +
    ' (p50 / p95 は日ごとに ' +
    wk.p50_lo.toFixed(1) +
    '〜' +
    wk.p95_hi.toFixed(1) +
    ' ms、週では出さない)、雪像 ' +
    wk.in_week +
    ' / ' +
    wk.rows.length +
    ' 日。出来事は 作り置き ' +
    er.rows.length +
    ' 件 (異常 ' +
    er.anomalies +
    '・解除 ' +
    er.cleared +
    '・続いている ' +
    er.open +
    ') と ' +
    path.basename(snapFile) +
    ' の本物 ' +
    localEv.rows.length +
    ' 件 (異常 ' +
    localEv.anomalies +
    ')。' +
    (anonWeek === null
      ? '匿名化した実データは無い'
      : '匿名化した実データ (T14.35) は /daily も /events も無い版として通り、' +
        '/history?res=3600 を日で束ねた ' +
        anonWeek.days +
        ' 日 (' +
        anonWeek.from +
        ' → ' +
        anonWeek.to +
        ') で 要求 ' +
        anonWeek.requests +
        '・CONNECT ' +
        anonWeek.connects +
        '・ミス ' +
        anonWeek.misses +
        '・エラー ' +
        anonWeek.errors +
        '・最大同時 ' +
        anonWeek.active_max +
        ' = T14.40 の週の数字と一致した')
);

// (4) 煙試験: **作り物の DOM と fetch で `inspect.html` の `<script>` を丸ごと動かす**。
//     T14.8 はこれをスクラッチでやってリポジトリに入れていなかったので、ここに畳んだ。
//     描画関数 (DOM に触らない側) は上で見たので、ここで見るのは**描く側**:
//     `renderDaily` / `renderWeekly` / `renderEvents` を含めた 1 枚が、
//     (a) 全部の口がある版と (b) `/daily` `/events` `/snapshots` `/hosts/series` が 404 の版
//     (古い版と `--lite`) の**どちらでも例外を出さない**こと。ページは自分の `catch` で
//     例外を「取得失敗」に変えてしまうので、**`lastok` の文字**で成否を見る。
function smokeInspect(route) {
  const els = {};
  const $ = (id) => els[id] || (els[id] = {
    id, textContent: '', innerHTML: '', hidden: false, value: '', checked: false,
    className: '', style: {}, clientWidth: 900, clientHeight: 300, width: 900, height: 300,
    tBodies: [{ innerHTML: '' }], addEventListener() {},
    getContext: () => {
      const noop = () => {};
      return { setTransform: noop, clearRect: noop, beginPath: noop, moveTo: noop, lineTo: noop,
        stroke: noop, fill: noop, fillText: noop, arc: noop, save: noop, restore: noop,
        translate: noop, rotate: noop, closePath: noop, setLineDash: noop };
    },
  });
  const asked = [];
  const sandbox = {
    document: { getElementById: $, body: {} },
    window: { devicePixelRatio: 1, addEventListener() {} },
    getComputedStyle: () => ({ getPropertyValue: () => 'monospace' }),
    fetch: (u) => {
      asked.push(u);
      const d = route(u);
      return d === undefined
        ? Promise.reject(new Error('HTTP 404'))
        : Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(d) });
    },
    setInterval: () => 0,
  };
  const names = Object.keys(sandbox);
  new Function(...names, insJs)(...names.map((n) => sandbox[n]));
  return { $, asked, rows: (id) => (($(id).tBodies[0].innerHTML || '').match(/<tr/g) || []).length };
}
const withAll = smokeInspect((u) => {
  if (u.indexOf('/status') === 0) return snap.status;
  if (u.indexOf('/recent') === 0) return snap.recent;
  if (u.indexOf('/events') === 0) return snap.events;
  if (u.indexOf('/history') === 0) return (snap.history || {})['5'];
  if (u.indexOf('/bursts') === 0) return snap.bursts;
  if (u.indexOf('/clients') === 0) return snap.clients;
  if (u.indexOf('/hosts/series') === 0) return snap.hosts_series;
  if (u.indexOf('/hosts') === 0) return snap.hosts;
  if (u.indexOf('/daily') === 0) return dailyJson;
  if (u.indexOf('/snapshots') === 0) return snapsJson;
  return undefined;
});
// 古い版と `--lite`: `/daily` `/events` `/snapshots` `/hosts/series` は 404、個票は空
const withNone = smokeInspect((u) => {
  if (u.indexOf('/status') === 0) return snap.status;
  if (u.indexOf('/recent') === 0) return { recent: [], count: 0, lite: true };
  if (u.indexOf('/history') === 0) return (snap.history || {})['5'];
  if (u.indexOf('/bursts') === 0) return { bursts: [], lite: true };
  if (u.indexOf('/clients') === 0) return { clients: [], lite: true };
  if (u.indexOf('/hosts') === 0) return { hosts: [] };
  return undefined;
});
process.on('unhandledRejection', (e) => fail('煙試験で拾われない例外: ' + (e && e.message)));
setTimeout(() => {
  for (const [name, r] of [['全部の口がある版', withAll], ['/daily も /events も無い版', withNone]]) {
    const last = r.$('lastok').textContent;
    if (last.indexOf('取得失敗') === 0) fail('煙試験 (' + name + ') で例外: ' + last);
    if (last.indexOf('更新') !== 0) fail('煙試験 (' + name + ') が最後まで動いていない: ' + last);
    for (const id of ['dailyhint', 'weekhint', 'evhint', 'tlhint']) {
      if (!r.$(id).textContent) fail('煙試験 (' + name + '): ' + id + ' が空のまま');
    }
  }
  // 全部ある版: 3 枚に行が出て、`/daily` と `/snapshots` を実際に引いている
  if (withAll.rows('daily') !== 9) fail('煙試験: 「今日」の行が 9 でない: ' + withAll.rows('daily'));
  if (withAll.rows('weekly') !== 7) fail('煙試験: 「今週」の行が 7 でない: ' + withAll.rows('weekly'));
  if (withAll.rows('events') !== ((snap.events || {}).events || []).length) {
    fail('煙試験: 「出来事と異常」の行が実出力の件数と違う: ' + withAll.rows('events'));
  }
  if (!withAll.asked.some((u) => u.indexOf('/daily') === 0)) fail('煙試験: /daily を引いていない');
  if (!withAll.asked.some((u) => u.indexOf('/snapshots') === 0)) fail('煙試験: /snapshots を引いていない');
  if ((withAll.$('slow').tBodies[0].innerHTML.match(/\/explain\?host=/g) || []).length === 0) {
    fail('煙試験: 遅い接続の宛先が /explain へのリンクになっていない');
  }
  // 無い版: 3 枚とも空のまま「記録していません」と断る (`--lite` でもページは開ける)
  for (const id of ['daily', 'weekly', 'events']) {
    if (withNone.rows(id) !== 0) fail('煙試験: 無い版で ' + id + ' に行が出た');
  }
  for (const id of ['dailyhint', 'weekhint', 'evhint']) {
    if (withNone.$(id).textContent.indexOf('記録していません') < 0) {
      fail('煙試験: 無い版の ' + id + ' が「記録していません」と言っていない');
    }
  }
  console.log(
    'OK: 調査ページの煙試験 (作り物の DOM と fetch で <script> を丸ごと) も通った: ' +
      '全部の口がある版は ' + withAll.asked.length + ' 本引いて 今日 ' + withAll.rows('daily') +
      ' 行 / 今週 ' + withAll.rows('weekly') + ' 行 / 出来事 ' + withAll.rows('events') +
      ' 行 / 遅い接続 ' + withAll.rows('slow') + ' 行を描き、' +
      '/daily も /events も無い版 (古い版と --lite) では ' + withNone.asked.length +
      ' 本引いて 3 枚とも「記録していません」と断った'
  );
}, 0);
