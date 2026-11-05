// `scripts/check-dashboard.js` の節: 11. 匿名化した実データで 2〜5 と 10 をもう一度 (T.34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 11. 匿名化した実データ (T14.35) で 2〜5 と 10 の読み方をもう一度回す。
// `scripts/testdata/deployed-2026-09-16.anon.json` は**デプロイ先の実出力**を
// `scripts/anonymize-snapshot.py` に通したもの (ホスト名・接続元 IP・User-Agent だけを
// 決定的に置き換え、数字は 1 つも変えていない)。上の作り置きは手元のプロキシと架空の個票なので、
// **本物の分布** (ホスト 817 件、名前解決 90 件、`res=60` は 1,440 標本) で読み方が壊れていないか
// をここで見る。匿名化済みの名前 (`host-0001.example` / `198.51.100.x`) しか入っていないことも
// 一緒に確かめる (元の名前が混ざったらここで気づく)。
function checkAnonymized() {
  const snapPath = path.join(__dirname, 'testdata', 'deployed-2026-09-16.anon.json');
  if (!fs.existsSync(snapPath)) return null;
  const where = path.basename(snapPath);
  const snap = JSON.parse(fs.readFileSync(snapPath, 'utf8'));
  const NAME = '(?:host-\\d{4,}\\.example|other|203\\.0\\.113\\.\\d+|192\\.0\\.2\\.\\d+)';
  const HOSTKEY = new RegExp('^(?:[a-z]+:\\/\\/)?(?:' + NAME + ')(?::\\d+)?$');
  const CLIENT = /^(?:198\.51\.100\.\d+|198\.18\.\d+\.\d+|2001:db8::[0-9a-f]+)(?::\d+)?$/;
  let rows = 0, conns = 0;
  for (const res of Object.keys(snap.history || {})) {
    const h = snap.history[res] || {};
    const ss = api.toSamples(h);
    if (ss.length !== (h.samples || []).length) fail(where + ' の res=' + res + ' の標本の数が合わない');
    if (ss.length === 0) fail(where + ' の res=' + res + ' に標本が無い');
    for (const k of h.keys) {
      if (!(k in ss[0])) fail(where + ' の res=' + res + ' で列 ' + k + ' が読めていない');
    }
    for (const s of ss) {
      if (!s.connects) continue;
      const p50 = api.winQuantile(s.connect_buckets, s.connects, s.connect_ms_max, 0.5, h.bounds_ms);
      const p95 = api.winQuantile(s.connect_buckets, s.connects, s.connect_ms_max, 0.95, h.bounds_ms);
      if (p50 === null) fail(where + ': 件数があるのに p50 が null');
      if (!(p50 <= p95 + 1e-9)) fail(where + ': p50 ' + p50 + ' > p95 ' + p95);
      if (!(p95 <= s.connect_ms_max)) fail(where + ': p95 が窓の最大値を超えた');
      conns += s.connects;
    }
    const tail = ss.slice(Math.max(0, ss.length - 60));
    const merged = api.mergeWindows(ss, 60, 'connect');
    const want = tail.reduce((a, s) => a + s.connects, 0);
    if (merged.count !== want) fail(where + ' の mergeWindows の件数 ' + merged.count + ' != ' + want);
    if (merged.buckets.reduce((a, b) => a + b, 0) !== merged.count) fail(where + ': 区間の合計が件数と合わない');
    if (api.peak(ss, 60, 'active_max', 'active') === null) fail(where + ': 山が読めていない');
    rows += ss.length;
  }
  if (conns === 0) fail(where + ' に CONNECT のある標本が 1 つも無い');
  // `/status` (要求数順・エラー順・名前解決順の 3 枚) と個票
  const st = snap.status || {};
  const sdn = api.dnsStats(st);
  if (!(sdn.rate >= 0 && sdn.rate <= 100)) fail(where + ' の名前解決のミス率が範囲外: ' + sdn.rate);
  if (sdn.lookups !== (st.dns.hits || 0) + (st.dns.misses || 0)) fail(where + ': 解決した回数が合わない');
  const causes = (snap.history && snap.history['60'] && snap.history['60'].causes) || [];
  const bad = api.badHosts([st.hosts, (snap.status_errors || {}).hosts, (snap.status_dns || {}).hosts], causes, 10);
  if (bad.length === 0) fail(where + ': 悪いホストが 1 件も出ない');
  const hostRows = ((snap.hosts || {}).hosts) || [];
  if (hostRows.length === 0) fail(where + ' に /hosts の行が無い');
  for (const h of hostRows.concat(st.hosts || [])) {
    if (!HOSTKEY.test(h.host)) fail(where + ': 匿名化されていないホストがある (' + h.host.length + ' 文字)');
  }
  for (const c of (st.clients || []).concat(((snap.clients || {}).clients) || [])) {
    if (!CLIENT.test(c.client) && c.client !== 'other') {
      fail(where + ': 匿名化されていない接続元がある');
    }
  }
  const errRowsA = api.errorRows(snap.errors, 20);
  const connA = api.connRows(snap.connections, 50);
  for (const r of connA.rows) {
    if (r.target && !HOSTKEY.test(r.target)) fail(where + ': /connections の宛先が匿名化されていない');
    if (!CLIENT.test(r.client)) fail(where + ': /connections の接続元が匿名化されていない');
  }
  // 「調査」ページ (T14.8) の読み方も同じ実データで 1 度通す (この版の出力にあるのは `/history` だけ)
  const insH = (snap.history || {})['60'] || {};
  const insRows = ins.toSamples(insH);
  if (insRows.length !== (insH.samples || []).length) fail(where + ': inspect.html が実データの標本を読めない');
  for (const s2 of insRows) {
    if (!s2.connects) continue;
    const q = ins.winQuantile(s2.connect_buckets, s2.connects, s2.connect_ms_max, 0.5, insH.bounds_ms);
    if (!(q >= 0 && q <= s2.connect_ms_max)) fail(where + ': inspect.html の p50 が範囲外: ' + q);
  }
  // T14.6 / T14.12 / T14.25 の窓は、この版の出力には無い (あれば読む)
  const windows = checkClosed((snap.history || {})['5'] && (snap.history || {})['5'].closed, where) +
    checkTransfer((snap.history || {})['5'] && (snap.history || {})['5'].transfer, where) +
    checkKernelHistory((snap.history || {})['5'] && (snap.history || {})['5'].kernel, where);
  return {
    resolutions: Object.keys(snap.history || {}).length,
    rows,
    conns,
    hosts: hostRows.length,
    inspect: insRows.length,
    bad: bad.length,
    errors: errRowsA.length,
    conns_now: connA.rows.length,
    windows,
  };
}

const anon = checkAnonymized();
console.log(
  anon === null
    ? '(匿名化した実データ (T14.35) は無い: scripts/testdata/deployed-2026-09-16.anon.json)'
    : 'OK: 匿名化した実データ (T14.35) も読めた: /history ' +
        anon.resolutions +
        ' 本 (標本 ' +
        anon.rows +
        '、CONNECT ' +
        anon.conns +
        ' 本)、/hosts ' +
        anon.hosts +
        ' 件、悪いホスト ' +
        anon.bad +
        ' 件、/errors ' +
        anon.errors +
        ' 件、/connections ' +
        anon.conns_now +
        ' 本 (inspect.html でも ' +
        anon.inspect +
        ' 標本)。ホスト名と接続元は全部 匿名化済みの形だった'
);

