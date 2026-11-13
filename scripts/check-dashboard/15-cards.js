// `scripts/check-dashboard.js` の節: 16. T15.0 の 5 枚のカード (T34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 16. T15.0 の 5 枚のカード。作り物は**インラインの定数**で、5 枚それぞれに
// 「欄がある版」と「**欄が無い古い版**」の 2 通りを通す。
//
// 「古い版」は**引数で渡された実出力から T15.0 の欄を消して**作る。作り置きの実出力
// (`history-res5.json` / `status.json` / `profile-res5.json`) はたしかに T15.0 より前のものだが、
// この確認は `scripts/collect-deployed.sh` からも**デプロイ先の実出力**を引数に呼ばれるので
// (296 行)、引数をそのまま「古い版」に使うと、新しい欄を持つプロキシを相手にした日に必ず落ちる。

// `keys` + 配列の配列 (`/history` も `/profile` も同じ形) から列を落として「その欄を持たない版」を作る
function withoutCols(j, drop) {
  const out = JSON.parse(JSON.stringify(j || {}));
  const keep = [];
  (out.keys || []).forEach((k, i) => {
    if (drop.indexOf(k) < 0) keep.push(i);
  });
  const keys = out.keys || [];
  out.keys = keep.map((i) => keys[i]);
  if (Array.isArray(out.key_kinds)) {
    const kinds = out.key_kinds;
    out.key_kinds = keep.map((i) => kinds[i]);
  }
  out.samples = (out.samples || []).map((row) => keep.map((i) => row[i]));
  return out;
}
// `/status` から T15.0 の欄を消した「古い版」(kernel = 単位 4、dns の 5 つ = 単位 5、wait = 単位 1)
const oldStatus = JSON.parse(JSON.stringify(st));
delete oldStatus.kernel;
for (const k of ['misses_by_kind', 'refresh_failures', 'refresh_ms_sum', 'refresh_ms_max', 'refresh_late']) {
  if (oldStatus.dns) delete oldStatus.dns[k];
}
if (oldStatus.recent_quantiles) delete oldStatus.recent_quantiles.wait;
// `/history` から単位 6 の `wait_*` を消した「古い版」と、その標本
const oldHist = withoutCols(hist, ['waits', 'wait_ms_sum', 'wait_ms_max', 'wait_buckets']);
const oldSamples = api.toSamples(oldHist);
// `/profile` から単位 3 の末尾 2 列を消した「古い版」
const oldProf = api.toProfile(withoutCols(pj, ['threads_top', 'run_delay_us']));

// (a) CPU の絞り。`/history` の `kernel` に T15.0 が `cpu_nr_periods` を**末尾に**足した版
const fakeKernelNew = {
  interval_secs: 5,
  keys: fakeKernel.keys.concat(['cpu_nr_periods']),
  samples: [fakeKernel.samples[0].concat([50]), fakeKernel.samples[1].concat([null])],
};
if (checkKernelHistory(fakeKernelNew, '作り置き (T15.0)') !== 2) fail('新しい kernel を読めていない');
const kernNew = api.toKernel({ kernel: fakeKernelNew });
if (kernNew.length !== 2) fail('/history の kernel を 2 標本で読めていない: ' + kernNew.length);
if (kernNew[0].cpu_nr_periods !== 50) fail('cpu_nr_periods が列名で読めていない');
if (kernNew[1].cpu_nr_periods !== null) fail('読めない環境の null が落ちている');
if (api.toKernel({ kernel: fakeKernel })[0].cpu_nr_periods !== undefined) {
  fail('古い版に無い列が undefined 以外で来た');
}
if (api.toKernel({}).length !== 0) fail('kernel の無い版は 0 件のはず');
if (api.toKernel(null).length !== 0) fail('null でも 0 件のはず');
const cpuStatus = {
  kernel: {
    at: 1789251465,
    cgroup_cpu: {
      nr_throttled: 1200, throttled_usec: 48000000, quota_cores: 2,
      nr_periods: 40000, path: '/sys/fs/cgroup',
      since_start: { nr_periods: 3600, nr_throttled: 90, throttled_usec: 3600000 },
    },
  },
};
const cpu = api.cpuThrottle(cpuStatus, kernNew, 60);
if (!cpu) fail('cgroup_cpu があるのに null');
if (Math.abs(cpu.pct - 3) > 1e-9) fail('絞られた割合が nr_throttled ÷ nr_periods でない: ' + cpu.pct);
if (cpu.quota !== 2 || cpu.path !== '/sys/fs/cgroup') fail('割り当てと道が読めていない');
if (!cpu.since || cpu.since.periods !== 3600) fail('since_start が読めていない');
if (cpu.window.periods !== 50 || cpu.window.throttled !== 0) fail('窓の増分が合わない');
if (cpu.window.pct !== 0) fail('窓の割合が 0 でない: ' + cpu.window.pct);
// 割り当てが無い (`quota_cores` が null) 環境では割合だけ出す
const noQuota = api.cpuThrottle(
  { kernel: { cgroup_cpu: { nr_throttled: 0, throttled_usec: 0, quota_cores: null, nr_periods: 0, path: null, since_start: null } } },
  [], 60
);
if (!noQuota || noQuota.quota !== null || noQuota.pct !== null) fail('割り当ての無い環境が読めていない');
// 古い版 (`/status` に kernel が無い) と cgroup v2 の読めない環境ではカードごと断る
if (api.cpuThrottle(oldStatus, kernNew, 60) !== null) fail('kernel の無い /status で null になっていない');
if (api.cpuThrottle({ kernel: { cgroup_cpu: null } }, [], 60) !== null) fail('cgroup_cpu が null なら null のはず');
// `cgroup_cpu` は在るが `nr_periods` (単位 4 で足した分母) がまだ無い版 = T15.0 より前のデプロイ先。
// 分母 0 の比を見せないために、ここも「記録していません」に落とす
if (api.cpuThrottle({ kernel: { cgroup_cpu: { nr_throttled: 1069258, throttled_usec: 152029542480, quota_cores: 0.5 } } }, [], 60) !== null) {
  fail('nr_periods を持たない古い版で null になっていない');
}
if (api.cpuThrottle(null, null, 60) !== null) fail('null でも例外なく null のはず');
// `kern` が空のとき `ch-cpu` に**長さ 0 の系列**が 2 本渡る (`/history` の `kernel` が無い版、
// `res=3600` = 解像度が 2 つしか無いので `kernel` は null、cgroup が読めない環境)。
// `drawChart` の末尾の目盛りが `hist[hist.length].t` を読んで TypeError を投げると、
// 例外は `pollHistory` の catch が握り潰すので **`redraw` の残りが 5 秒ごとに黙って飛ぶ**。
// ここだけ DOM に触るので、作り物の canvas を前置きにして別に切り出す
const chartApi = pick(js, ['drawChart', 'ago'], 'dashboard.html',
  'var window={devicePixelRatio:1};' +
  'var el={clientWidth:900,clientHeight:300,width:0,height:0,' +
  'getContext:function(){return new Proxy({},{get:function(){return function(){}}})}};' +
  'function $(){return el}var hist=[{t:100},{t:105},{t:110}];\n');
for (const [what, series] of [
  ['長さ 0 の系列 2 本 (kernel を持たない版と res=3600)', [
    { data: [], color: '#8b91a5', dash: [4, 4], width: 1 },
    { data: [], color: '#ff6b6b', fill: 'rgba(255,107,107,.12)' }]],
  ['ふつうの系列', [{ data: [1, null, 3], color: '#5aa9ff', fill: 'rgba(90,169,255,.12)' }]],
]) {
  try {
    chartApi.drawChart('ch-cpu', series, {});
  } catch (e) {
    fail('drawChart が ' + what + ' で落ちた: ' + e.message);
  }
}

// (b) 動かないトンネル。T15.0 の欄は**既定のままなら 1 バイトも出ない**ので、
// 古い版の応答と「何も起きていない応答」は同じ形になる (どちらも 0 件)
const idleConnJson = {
  connections: [
    { id: 11, client: '198.51.100.7', target: 'a.example.net:5228', kind: 'connect', state: 'relaying',
      age_secs: 99000, bytes: 9096, fds: 2, rate_bps: 0, tid: 4821, spins: 1043,
      revents: { client: 'HUP', origin: '' }, half_closed: 'client', half_closed_secs: 98700, idle_secs: 98400 },
    { id: 12, client: '198.51.100.8', target: 'b.example.net:5228', kind: 'connect', state: 'relaying',
      age_secs: 98000, bytes: 9032, fds: 2, rate_bps: 0, tid: 4822, idle_secs: 600 },
    { id: 13, client: '198.51.100.9', target: 'c.example.net:443', kind: 'connect', state: 'relaying',
      age_secs: 30, bytes: 4096, fds: 2, rate_bps: 1024, tid: 4823 },
    { id: 14, client: '198.51.100.9', target: '', kind: 'http', state: 'reading', age_secs: 900, bytes: 0, fds: 1, rate_bps: 0 },
  ],
  count: 4, shown: 4, truncated: false, lite: false,
};
const idle = api.idleTunnels(idleConnJson, 300, 20);
if (idle.count !== 2) fail('300 秒以上動いていないトンネルが 2 本でない: ' + idle.count);
if (idle.rows[0].id !== 11) fail('止まっている長い順でない: ' + idle.rows[0].id);
if (idle.tunnels !== 3) fail('トンネルの本数が合わない: ' + idle.tunnels);
if (idle.spins !== 1043) fail('空回りの合計が合わない: ' + idle.spins);
if (idle.half_closed !== 1) fail('半閉じの本数が合わない: ' + idle.half_closed);
if (idle.quiet !== 1) fail('1 度も空回りしていない本数が合わない: ' + idle.quiet);
if (idle.rows[0].revents.client !== 'HUP') fail('poll の旗が行に残っていない');
if (idle.rows[0].tid !== 4821) fail('tid が行に残っていない');
if (api.idleTunnels(idleConnJson, 100000, 20).count !== 0) fail('秒で絞れていない');
if (api.idleTunnels(idleConnJson, 300, 1).rows.length !== 1) fail('n で絞れていない');
// 古い版 (`idle_secs` を持たない `/connections`) と空と null
const oldIdle = api.idleTunnels(connJson, 300, 20);
if (oldIdle.count !== 0) fail('欄の無い版で 0 件になっていない: ' + oldIdle.count);
if (oldIdle.tunnels !== 2) fail('欄の無い版でもトンネルは数えるはず: ' + oldIdle.tunnels);
if (api.idleTunnels({ connections: [], lite: true }, 300, 20).lite !== true) fail('lite が読めていない');
if (api.idleTunnels({}, 300, 20).count !== 0) fail('空でも例外なく 0 件のはず');
if (api.idleTunnels(null, 300, 20).count !== 0) fail('null でも例外なく 0 件のはず');
// 欄を足しても「いまの接続」の読み方は 1 つも変わらない (末尾に足しただけ)
const bothConns = api.connRows(idleConnJson, 50);
if (bothConns.rows.length !== 4 || bothConns.rows[0].id !== 11) fail('connRows の並びが変わった');
if (bothConns.kinds.connect !== 3 || bothConns.kinds.http !== 1) fail('connRows の内訳が変わった');

// (c) 名前解決の内訳。`misses_by_kind` の 4 つの和は、プロキシ側では `misses` と一致する
// (この確認が突き合わせるのは作り物自身の和。引数の実出力の `misses` とは無関係)
const fakeMissKinds = { cold: 800, expired: 300, warm_stale: 80, negative: 26 };
const fakeMissTotal = 800 + 300 + 80 + 26;
const dnsStatus = {
  dns: Object.assign({}, oldStatus.dns, {
    warm: 12, warm_secs: 900, refreshes: 340, misses: fakeMissTotal,
    misses_by_kind: fakeMissKinds,
    refresh_failures: 4, refresh_ms_sum: 12000.5, refresh_ms_max: 640, refresh_late: 7,
  }),
};
const missKinds = api.dnsMissKinds(dnsStatus);
if (!missKinds) fail('misses_by_kind があるのに null');
if (missKinds.rows.map((r) => r.name).join(',') !== 'cold,expired,warm_stale,negative') {
  fail('ミスの種類の綴りか並びが変わった: ' + missKinds.rows.map((r) => r.name).join(','));
}
if (missKinds.total !== fakeMissTotal) fail('種類別の合計 ' + missKinds.total + ' != ' + fakeMissTotal);
if (missKinds.total !== (dnsStatus.dns.misses || 0)) {
  fail('種類別の合計 ' + missKinds.total + ' != misses ' + dnsStatus.dns.misses);
}
const dn2 = api.dnsStats(dnsStatus);
if (dn2.reffail !== 4 || dn2.reflate !== 7) fail('引き直しの失敗と遅れが読めていない');
if (Math.abs(dn2.refms - 12000.5) > 1e-9 || dn2.refmax !== 640) fail('引き直しの ms が読めていない');
if (dn2.warm !== 12) fail('warm が読めていない (T14.1 の枝が壊れた)');
// 古い版 (`misses_by_kind` も `refresh_*` も無い `/status`)
if (api.dnsMissKinds(oldStatus) !== null) fail('欄の無い版は null のはず');
if (api.dnsStats(oldStatus).reffail !== null) fail('refresh_failures の無い版は null のはず');
if (api.dnsMissKinds({}) !== null || api.dnsMissKinds(null) !== null) fail('空でも null のはず');

// (d) 受付待ち。二峰 (1 ms 未満と 50〜100 ms) の作り物を 12 段の区間で読む
const zeroWin = [0, 0, 0, 0, 0, 0, 0];
const queueBuckets = [80, 0, 0, 0, 0, 0, 20, 0, 0, 0, 0, 0, 0];
const fakeProfile = {
  interval_secs: 5, sample_ms: 1000, sampler: 'on', bounds_ms: hist.bounds_ms,
  stages: pj.stages, roles: pj.roles, states: pj.states, lock_names: pj.lock_names,
  keys: ['t', 'requests', 'cpu_us', 'connect', 'forward', 'threads', 'locks', 'queue', 'threads_top', 'run_delay_us'],
  samples: [
    [1789251460, 100, 4920000,
      [[100, 3000, 95, queueBuckets], 0, 0, 0, 0, 0, 0], zeroWin.slice(0, 6),
      pj.roles.map(() => 0), pj.lock_names.map(() => 0), [0, 0, 0],
      [[4821, 'conn', 1, 4800000, 50], [4822, 'history', 4, 120000, 3]],
      [0, 120000, 0, 0, 3000, 0, 0, 0, 0]],
    [1789251465, 0, 4900000,
      zeroWin, zeroWin.slice(0, 6), pj.roles.map(() => 0), pj.lock_names.map(() => 0), [0, 0, 0],
      [[4821, 'conn', 1, 4900000, 50]], null],
  ],
  locks_total: pj.lock_names.map(() => 0), queue_total: [0, 0, 0], unknown_syscalls: [],
  recent: { secs: 300, requests: 100, cpu_us: 9820000, cpu_per_request_us: 98200 },
  count: 2, shown: 2, truncated: false,
};
const fprof = api.toProfile(fakeProfile);
if (fprof.samples.length !== 2) fail('作り物の /profile の標本が読めていない');
const qs = api.queueSpread(fprof, 60, 'connect');
if (!qs) fail('queue の段があるのに null');
if (qs.rows.length !== hist.bounds_ms.length + 1) fail('区間が bounds_ms + 1 でない: ' + qs.rows.length);
if (qs.total !== 100 || qs.count !== 100) fail('件数と区間の合計が合わない: ' + qs.total + ' / ' + qs.count);
if (qs.rows[0].avg !== 80 || qs.rows[6].avg !== 20) fail('二峰が区間に落ちていない');
if (qs.rows[0].name !== '0–1 ms') fail('区間の名前が違う: ' + qs.rows[0].name);
if (qs.rows[qs.rows.length - 1].name.indexOf('>') !== 0) fail('いちばん上の区間が「より大きい」でない');
if (!(qs.p95 > 50 && qs.p95 <= 95)) fail('p95 が上の峰に来ていない: ' + qs.p95);
if (!(qs.p50 <= 1)) fail('p50 が下の峰に来ていない: ' + qs.p50);
// (c) と (d) の横棒は**件数**を積むので、tooltip の整形は呼ぶ側が渡す (既定は今までどおり ms)。
// `stackHtml` の第 3 引数が効いていないと「cold 800 ms」という嘘の tooltip になる
const qbar = api.stackHtml(qs.rows, qs.total, api.fmtNum);
if (qbar.indexOf('title="0–1 ms 80 (80%)"') < 0) fail('件数の横棒の tooltip が件数になっていない: ' + qbar);
const mkbar = api.stackHtml(missKinds.rows, missKinds.total, api.fmtNum);
if (mkbar.indexOf(' ms') >= 0) fail('件数の横棒の tooltip に ms が入っている: ' + mkbar);
if (mkbar.indexOf('title="cold 800 (66%)"') < 0) fail('ミスの種類の横棒が件数になっていない: ' + mkbar);
// 整形を渡さなければ今までどおり ms (既存の 2 か所はここに乗っている)
if (api.stackHtml(missKinds.rows, missKinds.total).indexOf('title="cold 800 ms (66%)"') < 0) {
  fail('整形を渡さないときは ms のはず: ' + api.stackHtml(missKinds.rows, missKinds.total));
}
if (api.stackHtml(qs.rows, 0) !== '') fail('合計 0 の横棒は空のはず');
const rd = api.runDelay(fprof, 60);
if (!rd.ok) fail('run_delay_us があるのに ok でない');
if (rd.total !== 123000) fail('直近の合計が合わない: ' + rd.total);
if (rd.series.length !== 2 || rd.series[1] !== null) fail('読めない標本が null で来ていない');
if (Math.abs(rd.series[0] - 24600) > 1e-9) fail('us / 秒 になっていない: ' + rd.series[0]);
if (rd.roles[0].role !== 'conn' || rd.roles[0].us !== 120000) fail('役割ごとの合計が合わない');
const tt = api.topThreads(fprof, 60);
if (tt.length !== 2) fail('上位スレッドが 2 本で読めていない: ' + tt.length);
if (tt[0].tid !== 4821 || tt[0].cpu_us !== 9700000) fail('tid で束ねられていない: ' + JSON.stringify(tt[0]));
if (tt[0].windows !== 2) fail('窓をまたいだ数が合わない: ' + tt[0].windows);
if (tt[0].role !== 'conn' || tt[1].role !== 'history') fail('役割が roles の添字で戻っていない');
if (Math.abs(tt[0].cpu_pct - 97) > 1e-9) fail('CPU % が合わない: ' + tt[0].cpu_pct);
// 古い版 (`threads_top` も `run_delay_us` も無い 8 列の `/profile`)
if (api.topThreads(oldProf, n5).length !== 0) fail('列の無い版で 0 件になっていない');
const oldRd = api.runDelay(oldProf, n5);
if (oldRd.ok || oldRd.total !== 0) fail('列の無い版で ok になっている');
if (oldRd.series.some((v) => v !== null)) fail('列の無い版の折れ線が null で切れていない');
if (api.queueSpread(api.toProfile(null), 60, 'connect') !== null) fail('空なら null のはず');
if (api.topThreads(api.toProfile(null), 60).length !== 0) fail('空でも 0 件のはず');
if (api.runDelay(api.toProfile(null), 60).ok) fail('空で ok になっている');
// 既存の読み方 (段階・役割・ロック) は末尾に 2 列足しても変わらない
const fstage = api.stageRows(fprof, 60, 'connect');
if (fstage.rows.length !== 7 || fstage.rows[0].count !== 100) fail('段階の読み方が変わった');
if (api.roleRows(fprof, 60).length !== 0) fail('標本 0 の役割が出ている');

// (e) 利用者が待つ時間。`recent_quantiles.wait` があればそれ、無ければ `wait_*` の補間
const waitStatus = {
  recent_quantiles: {
    connect: { n: 1024, p50: 8.4, p90: 50, p99: 63.4, max: 150, window_secs: 240 },
    forward: { n: 8, p50: 1, p90: 2, p99: 2, max: 2, window_secs: 5 },
    wait: { n: 1024, p50: 9.5, p90: 63, p99: 92, max: 180.5, window_secs: 240 },
  },
};
const wexact = api.waitKpi(waitStatus, null, hist.bounds_ms);
if (!wexact || !wexact.exact) fail('recent_quantiles.wait があるのに補間に落ちている');
if (wexact.p50 !== 9.5) fail('KPI が recent_quantiles.wait.p50 になっていない: ' + wexact.p50);
if (wexact.label.indexOf('1,024') < 0) fail('札が「直近 1,024 本」でない: ' + wexact.label);
for (const part of ['1024 本', 'p90', 'p99', '最大']) {
  if (wexact.detail.indexOf(part) < 0) fail('内訳に ' + part + ' が無い: ' + wexact.detail);
}
// `wait` は `connect` より必ず大きいか等しい (名前解決と queue が入っているぶん)
const wc = waitStatus.recent_quantiles;
for (const q of ['p50', 'p90', 'p99', 'max']) {
  if (!(wc.wait[q] >= wc.connect[q])) fail('wait.' + q + ' が connect を下回った');
}
// `/history` に T15.0 の 4 列を足した版 (**古い版の末尾に**足すだけ。既に持っている
// 実出力を渡されても鍵が重複しないように、土台は列を落とした `oldHist`)
const waitHist = Object.assign({}, oldHist, {
  keys: oldHist.keys.concat(['waits', 'wait_ms_sum', 'wait_ms_max', 'wait_buckets']),
  samples: oldHist.samples.map((row, i) =>
    row.concat(
      i % 2
        ? [4, 260, 95, [2, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0]]
        : [0, 0, 0, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]]
    )
  ),
});
const wsamples = api.toSamples(waitHist);
if (wsamples.length !== oldSamples.length) fail('列を足したら標本の数が変わった');
if (wsamples[0].connect_buckets.length !== oldSamples[0].connect_buckets.length) fail('既存の列がずれた');
const wwin = api.mergeWindows(wsamples, 60, 'wait');
const waitTotal = wsamples.slice(Math.max(0, wsamples.length - 60)).reduce((a, s) => a + (s.waits || 0), 0);
if (wwin.count !== waitTotal) fail('mergeWindows が waits を数えていない: ' + wwin.count + ' != ' + waitTotal);
if (wwin.buckets.reduce((a, b) => a + b, 0) !== waitTotal) fail('wait の区間の合計が件数と合わない');
const wfell = api.waitKpi({}, wwin, hist.bounds_ms);
if (!wfell || wfell.exact) fail('wait_* しか無いのに実測を名乗っている');
if (wfell.label !== '直近 5 分') fail('落ちた先の札が違う: ' + wfell.label);
if (wfell.detail.indexOf('区間の補間') < 0) fail('補間であることが内訳に書かれていない');
if (!(wfell.p50 >= 0 && wfell.p50 <= wwin.max)) fail('落ちた先の p50 が範囲外: ' + wfell.p50);
// 古い版 (`recent_quantiles.wait` も `wait_*` も無い) ではカードを「–」のままにする
if (api.waitKpi(oldStatus, api.mergeWindows(oldSamples, 60, 'wait'), hist.bounds_ms) !== null) {
  fail('wait をどこにも持たない版で null になっていない');
}
if (api.mergeWindows(oldSamples, 60, 'wait').count !== 0) fail('古い標本で waits が数えられている');
if (api.waitKpi(null, null, hist.bounds_ms) !== null) fail('null でも例外なく null のはず');
if (api.waitKpi({ recent_quantiles: { wait: { n: 0 } } }, null, hist.bounds_ms) !== null) {
  fail('n = 0 は無いのと同じ (null) のはず');
}
// 既存の KPI (`connectKpi`) は 1 つも変わらない
if (api.connectKpi(waitStatus, kpiWin, hist.bounds_ms).p50 !== 8.4) fail('connectKpi が変わった');
// T29.18: 近い接続元 (RTT < 100 ms) の待ちは `/recent?rtt_client=lt100` の成功した CONNECT の
// queue + client_read + dns + connect の実測の分位点。`rtt_client` を知らない版 (絞られていない) は null
const nearRows = [
  { kind: 'connect', reason: 'server_eof', ms: { queue: 1, client_read: 0, dns: 0, connect: 11 } },
  { kind: 'connect', reason: 'server_eof', ms: { dns: 4, connect: 12 } },
  { kind: 'connect', reason: 'client_eof', ms: { dns: 0, connect: 30 } },
  { kind: 'connect', reason: 'error:timeout', ms: { dns: 0, connect: 3000 } },
  { kind: 'http', reason: 'client_eof', ms: { dns: 0, connect: 900 } },
];
const near = api.nearWaitKpi({ recent: nearRows, rtt_client: 'lt100' });
if (!near || near.n !== 3) fail('近い接続元の成功した CONNECT 3 本を数えていない: ' + JSON.stringify(near));
if (near.p50 !== 16 || near.p95 !== 30) fail('近い接続元の分位点が違う: ' + JSON.stringify(near));
if (near.label.indexOf('近い接続元 3 本') < 0) fail('札が違う: ' + near.label);
if (api.nearWaitKpi({ recent: nearRows }) !== null) fail('rtt_client を知らない版で null になっていない');
if (api.nearWaitKpi({ recent: [], rtt_client: 'lt100' }) !== null) fail('0 本は null のはず');
if (api.nearWaitKpi(null) !== null) fail('null でも例外なく null のはず');
// T29.19: キャッシュ命中率は母数 (n=) を添え、1 日あたり 100 本未満なら率を出さない (tokyo の 24 時間は 43 / 80)
const fewHits = api.hitKpi({ uptime_secs: 86798, cache: { hits: 43, misses: 37, hits_memory: 24, hits_disk: 19 } });
if (fewHits.pct !== null) fail('1 日 80 本で率を出した: ' + fewHits.pct);
if (fewHits.detail.indexOf('(n=80。1 日 100 本未満なので率は出さない)') < 0) fail('母数が無い: ' + fewHits.detail);
const manyHits = api.hitKpi({ uptime_secs: 86400, cache: { hits: 300, misses: 100 } });
if (Math.abs(manyHits.pct - 75) > 1e-9) fail('1 日 400 本の率が違う: ' + manyHits.pct);
if (manyHits.detail.indexOf('(n=400)') < 0) fail('母数が無い: ' + manyHits.detail);
if (api.hitKpi({}).pct !== null) fail('空の /status で率を出した');

