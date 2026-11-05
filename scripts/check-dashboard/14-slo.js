// `scripts/check-dashboard.js` の節: 13. KPI「今日の SLO」 (T34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 13. KPI「今日の SLO」(T14.50)。`/slo` の応答から**今日 (UTC) の達成率**と
// 「いちばん外した閾」「直近の外れた時間帯」が読めること。`/slo` を持たない版
// (この口が 404 の版) では `null` を返し、ダッシュボードはカードごと出さない。
const sloJson = {
  now: 1789171195,
  days: 7,
  from: 1788566400,
  to: 1789171195,
  sample_secs: 5,
  thresholds: { connect_p50_ms: 10, connect_p95_ms: 100, error_rate: 0.005, dns_miss_per_connect: 0.2 },
  names: ['connect_p50_ms', 'connect_p95_ms', 'error_rate', 'dns_miss_per_connect'],
  judged: 17280,
  met: 12240,
  ratio: 0.70833,
  misses: [5040, 5040, 5040, 0],
  first_t: 1789084800,
  last_t: 1789167600,
  hours: 24,
  kept_hours: 744,
  today: { date: '2026-09-11', t: 1789084800, judged: 17280, met: 12240, ratio: 0.70833, misses: [5040, 5040, 5040, 0] },
  daily: [{ date: '2026-09-11', t: 1789084800, judged: 17280, met: 12240, ratio: 0.70833, misses: [5040, 5040, 5040, 0] }],
  breaches: [
    {
      from: 1789146000, to: 1789171200, from_hour: '2026-09-11T17Z', to_hour: '2026-09-12T00Z',
      hours: 7, judged: 5040, met: 0, ratio: 0.0,
      breached: [
        { name: 'connect_p50_ms', samples: 5040, worst: 46.875, threshold: 10 },
        { name: 'connect_p95_ms', samples: 5040, worst: 300, threshold: 100 },
        { name: 'error_rate', samples: 5040, worst: 0.125, threshold: 0.005 },
      ],
    },
  ],
  hourly_keys: ['t', 'judged', 'met', 'misses'],
  hourly: [[1789084800, 720, 720, [0, 0, 0, 0]], [1789146000, 720, 0, [720, 720, 720, 0]]],
  truncated: false,
};
const slo = api.sloKpi(sloJson);
if (!slo) fail('/slo の応答から KPI が読めていない');
if (Math.abs(slo.pct - 70.833) > 0.01) fail('今日の達成率が 70.833% になっていない: ' + slo.pct);
if (slo.date !== '2026-09-11') fail('札が今日 (UTC) の日付になっていない: ' + slo.date);
if (slo.worst !== 'connect_p50_ms') fail('いちばん外した閾が読めていない: ' + slo.worst);
if (slo.breaches !== 1) fail('外れた時間帯の数が合わない: ' + slo.breaches);
for (const part of ['判定 17,280 標本', '2026-09-11T17Z〜2026-09-12T00Z', '直近 7 日']) {
  if (slo.detail.indexOf(part) < 0) fail('内訳に ' + part + ' が無い: ' + slo.detail);
}
// T29.33 より前の `/slo` (`rate_window_secs` も `rates` も無い) は今までの説明のまま
if (api.sloTitle(sloJson).indexOf('4 つの閾を 5 秒の標本ごと') < 0) fail('古い /slo の説明が変わった: ' + api.sloTitle(sloJson));
if (slo.detail.indexOf('1 時間の合計') >= 0) fail('古い /slo に 1 時間の合計を出した: ' + slo.detail);
// T29.33 の `/slo`: 比の 2 つは 1 時間の合計で判定し、今日の分子 / 分母と外した時間の数を出す
const sloHourly = api.sloKpi(
  Object.assign({}, sloJson, {
    rate_window_secs: 3600,
    today: Object.assign({}, sloJson.today, {
      rates: {
        error_rate: { num: 10, den: 2010, value: 0.00498, hours_met: 1, hours_missed: 1 },
        dns_miss_per_connect: { num: 0, den: 0, value: null, hours_met: 0, hours_missed: 0 },
      },
    }),
  })
);
if (sloHourly.detail.indexOf('error_rate 10 / 2,010 (1 時間の合計で外れ 1 時間)') < 0)
  fail('1 時間の合計と分母が内訳に無い: ' + sloHourly.detail);
if (sloHourly.detail.indexOf('dns_miss_per_connect') >= 0) fail('分母 0 の比を出した: ' + sloHourly.detail);
const sloHourlyTitle = api.sloTitle(
  Object.assign({}, sloJson, {
    rate_window_secs: 3600,
    basis: { connect: 'connect_minus_rtt', error_rate: 'resolved_attempts_hourly_sum', dns_miss_per_connect: 'excluding_failed_lookups_hourly_sum' },
  })
);
if (sloHourlyTitle.indexOf('1 時間の合計で判定') < 0) fail('1 時間の合計の説明が無い: ' + sloHourlyTitle);
if (sloHourlyTitle.indexOf('名前解決に成功した試行あたり (1 時間の合計)') < 0) fail('basis の _hourly_sum が読めていない: ' + sloHourlyTitle);
if (sloHourlyTitle.indexOf('名前解決を除いた確立') < 0) fail('basis の connect_minus_rtt が読めていない: ' + sloHourlyTitle);
if (sloHourly.detail.indexOf('443・80') >= 0) fail('T30.3 より前の basis で 443・80 と書いた: ' + sloHourly.detail);
// T30.3: `error_rate` は 443・80 宛てだけ (`basis.error_rate` が `resolved_web_attempts_hourly_sum`)。tooltip と内訳に書く
const webBasis = { connect: 'connect_minus_rtt', error_rate: 'resolved_web_attempts_hourly_sum', dns_miss_per_connect: 'excluding_failed_lookups_hourly_sum' };
const sloWeb = Object.assign({}, sloJson, { rate_window_secs: 3600, basis: webBasis });
const sloWebTitle = api.sloTitle(sloWeb);
if (sloWebTitle.indexOf('名前解決に成功した 443・80 宛ての試行あたり') < 0 || sloWebTitle.indexOf('(1 時間の合計)') < 0)
  fail('basis の resolved_web_attempts_hourly_sum が読めていない: ' + sloWebTitle);
const sloWebKpi = api.sloKpi(
  Object.assign({}, sloWeb, {
    today: Object.assign({}, sloJson.today, {
      rates: {
        error_rate: { num: 4, den: 1004, value: 0.00398, hours_met: 1, hours_missed: 0 },
        dns_miss_per_connect: { num: 3, den: 1000, value: 0.003, hours_met: 1, hours_missed: 0 },
      },
    }),
  })
);
if (sloWebKpi.detail.indexOf('error_rate (443・80 だけ) 4 / 1,004 (1 時間の合計で外れ 0 時間)') < 0)
  fail('443・80 だけの error_rate が内訳に無い: ' + sloWebKpi.detail);
if (sloWebKpi.detail.indexOf('dns_miss_per_connect 3 / 1,000') < 0) fail('dns_miss_per_connect に印が付いた: ' + sloWebKpi.detail);
if (sloWebKpi.detail.indexOf('外した') >= 0) fail('excluded の無い版で外した宛先を書いた: ' + sloWebKpi.detail);
// T30.4: 1 つの宛先に偏ったエラーを外す (`basis.error_rate` の末尾が `_excluding_top_destination`)。
// tooltip に外し方、内訳に外した時間の数とエラーの件数 (`today.rates.error_rate.excluded`)
const topBasis = Object.assign({}, webBasis, { error_rate: 'resolved_web_attempts_hourly_sum_excluding_top_destination' });
const sloTop = Object.assign({}, sloJson, { rate_window_secs: 3600, basis: topBasis });
const sloTopTitle = api.sloTitle(sloTop);
if (
  sloTopTitle.indexOf('名前解決に成功した 443・80 宛ての試行あたり') < 0 ||
  sloTopTitle.indexOf('半分以上が 1 つの宛先なら、その宛先を外す') < 0
)
  fail('basis の _excluding_top_destination が読めていない: ' + sloTopTitle);
const sloTopKpi = api.sloKpi(
  Object.assign({}, sloTop, {
    today: Object.assign({}, sloJson.today, {
      rates: {
        error_rate: {
          num: 1, den: 401, value: 0.00249, hours_met: 1, hours_missed: 0,
          excluded: { hours: 1, errors: 4, attempts: 4 },
        },
        dns_miss_per_connect: { num: 0, den: 400, value: 0, hours_met: 1, hours_missed: 0 },
      },
    }),
  })
);
if (sloTopKpi.detail.indexOf('error_rate (443・80 だけ) 1 / 401 (1 時間の合計で外れ 0 時間、偏った宛先を外した 1 時間・エラー 4 件)') < 0)
  fail('外した宛先が内訳に無い: ' + sloTopKpi.detail);
// 判定できた標本が 0 本の日は `ratio` が null (「達成率 0%」ではない)
const sloQuiet = api.sloKpi(
  Object.assign({}, sloJson, {
    ratio: null,
    breaches: [],
    today: { date: '2026-09-11', t: 1789084800, judged: 0, met: 0, ratio: null, misses: [0, 0, 0, 0] },
  })
);
if (!sloQuiet || sloQuiet.pct !== null) fail('判定 0 本の日が 0% になっている');
if (sloQuiet.detail.indexOf('まだ判定できた標本がありません') < 0) fail('断り書きが無い: ' + sloQuiet.detail);
// `/slo` を持たない版 (404 / 空) ではカードごと出さない
for (const bad of [null, undefined, {}, { today: null }, { today: { judged: 0 } }]) {
  if (api.sloKpi(bad) !== null) fail('/slo の無い版で null になっていない: ' + JSON.stringify(bad));
}

console.log(
  'OK: dashboard.html の JS は構文が通り、/history ' +
    samples.length +
    ' 標本 (CONNECT のある標本 ' +
    checked +
    ' 件、合計 ' +
    total +
    ' 本) を読めた。直近の p50 = ' +
    kpi.toFixed(2) +
    ' ms、山 = ' +
    pk +
    '。/status (' +
    path.basename(statusFile) +
    ') はホスト ' +
    st.hosts.length +
    ' 件、名前解決のミス率 ' +
    (dn.rate == null ? '–' : dn.rate.toFixed(1) + '%') +
    ' (ミス 1 回 ' +
    (dn.avg == null ? '–' : dn.avg.toFixed(1) + ' ms') +
    ')、悪いホスト ' +
    bad.length +
    ' 件。個票 (/errors ' +
    errRows.length +
    ' 件。まとめた行は ×' +
    rep.repeats +
    ' と最後の時刻を出す (T21.5)、/connections ' +
    conn.rows.length +
    ' 本) も読めた。閉じた接続の分布 ' +
    closedWindows +
    ' 窓と、山の写真 ' +
    shots.length +
    ' 枚 (T14.6) も読めた。canary は ' +
    (canaryRows === null ? 'この出力には無い' : canaryRows + ' 点') +
    '。カーネルの窓 (T14.12) は ' +
    (hist.kernel ? kernelRows + ' 標本' : 'この出力には無い') +
    '、/status の kernel は ' +
    (st.kernel ? '読めた' : 'この出力には無い') +
    '。出来事 (T14.11) は ' +
    evs.length +
    ' 件 / ' +
    EVENT_KINDS.length +
    ' 種' +
    '。/profile (' +
    path.basename(profFile) +
    ') は標本 ' +
    prof.samples.length +
    '、sampler ' +
    prof.sampler +
    '、CPU/要求 ' +
    (prof.recent && prof.recent.cpu_per_request_us != null
      ? (+prof.recent.cpu_per_request_us).toFixed(1) + ' us'
      : '–') +
    '、いちばん長い段階 = CONNECT ' +
    (connLead && connLead.top.length ? connLead.top[0].name + ' ' + connLead.top[0].share.toFixed(0) + '%' : '–') +
    ' / forward ' +
    (fwdLead && fwdLead.top.length ? fwdLead.top[0].name + ' ' + fwdLead.top[0].share.toFixed(0) + '%' : '–') +
    '、役割 ' +
    roles.length +
    ' 件' +
    '。速さと半閉じの分布 (T14.25) は ' +
    transferWindows +
    ' 窓' +
    '。調査ページ (inspect.html ' +
    Math.round(Buffer.byteLength(insHtml) / 1024) +
    ' KiB) は ' +
    path.basename(snapFile) +
    ' の個票で通った: タイムライン ' +
    tlClient.shown +
    ' 本 / ' +
    tlClient.lanes.length +
    ' 行 (理由 ' +
    tlClient.reasons.map((r) => r.name + ' ' + r.count).join(' · ') +
    ')、遅い接続 ' +
    slow.length +
    ' 件、山 ' +
    shotCards.length +
    ' 枚、接続元 ' +
    clientTbl.length +
    ' 件、RTT の点 ' +
    sc.count +
    ' 個 (外した行 ' +
    sc.skipped +
    ')、起動からの窓 ' +
    cut.rows.length +
    ' 標本 (外 ' +
    cut.cut +
    '、CONNECT ' +
    cut.connects +
    ' 本' +
    (cut.p50 == null ? '' : '、p50 ' + cut.p50.toFixed(2) + ' ms') +
    ')。デプロイ先の雪像 (' +
    path.basename(statusFile) +
    ' + ' +
    path.basename(file) +
    ') でも通った: RTT の点 ' +
    ins.rttScatter(st, {}).count +
    ' 個 / ホスト ' +
    st.hosts.length +
    ' 件、起動からの窓 ' +
    deployed.rows.length +
    ' / ' +
    samples.length +
    ' 標本' +
    '。端末から測るページ (probe.html ' +
    probeKiB +
    ' KiB) の median / pick / render も通った: (1) の往復 (作り物) の中央値 ' +
    probeVia.direct.median.toFixed(2) +
    ' ms、判定は 経由している (増えた要求 ' +
    probeVia.via.delta +
    ' 回) / 経由していない / 判定できない の 3 通り' +
    (probeVia.me && probeVia.me.rtt
      ? '、カーネルの RTT ' +
        probeVia.me.rtt.avg.toFixed(2) +
        ' ms (標本 ' +
        probeVia.me.rtt.samples +
        ') と並んだ'
      : '、カーネルの RTT はこの出力には無い') +
    '。KPI「CONNECT 確立 p50」は ' +
    (liveKpi && liveKpi.exact
      ? '実出力の recent_quantiles (' + liveKpi.n + ' 本) で ' + api.fmtMsFine(liveKpi.p50)
      : '見本の recent_quantiles で ' + api.fmtMsFine(exact.p50)) +
    '、無い版では区間の補間 ' +
    api.fmtMsFine(fell.p50) +
    ' に落ちた' +
    '。KPI「今日の SLO」(T14.50) は作り物の /slo で ' +
    slo.pct.toFixed(1) +
    '% (外れの最多 ' +
    slo.worst +
    '、外れた時間帯 ' +
    slo.breaches +
    ' 件)、/slo を持たない版では出さない'
);

