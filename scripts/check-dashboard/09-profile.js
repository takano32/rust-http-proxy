// `scripts/check-dashboard.js` の節: 8. /profile を読む関数 (T34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 8. `/profile` を読む関数 (段階・スレッド・ロック。T14.3)
const profFile = process.argv[4] || path.join(__dirname, 'testdata', 'profile-res5.json');
const pj = JSON.parse(fs.readFileSync(profFile, 'utf8'));
const prof = api.toProfile(pj);
if (prof.off) fail(profFile + ' が lite の出力 (/profile が off)');
if (prof.samples.length !== pj.samples.length) fail('/profile の標本の数が合わない');
if (prof.samples.length === 0) fail('/profile の標本が 0 件 (5 秒以上動かしたプロキシの出力を渡すこと)');
if (prof.names.connect.length !== 7 || prof.names.forward.length !== 6) {
  fail('段階の数が CONNECT 7 / forward 6 になっていない');
}
if (prof.bounds.length !== hist.bounds_ms.length) fail('/profile の区間が /history と違う');
for (const s of prof.samples) {
  if (s.connect.length !== prof.names.connect.length) fail('CONNECT の段階の数が合わない');
  if (s.forward.length !== prof.names.forward.length) fail('forward の段階の数が合わない');
  if (s.threads.length !== prof.roles.length) fail('役割の数が合わない');
  if (s.locks.length !== prof.lockNames.length) fail('ロックの数が合わない');
  if (s.queue.length !== 3) fail('待ち行列は (件数, 合計 ms, 最大 ms) の 3 つ');
  for (const w of s.connect.concat(s.forward)) {
    if (w.count && (!w.buckets || w.buckets.length !== prof.bounds.length + 1)) {
      fail('段階の区間が bounds_ms + 1 (上限なし) になっていない');
    }
    if (w.count && w.sum / w.count > w.max) fail('段階の平均が最大を超えた');
  }
  for (const t of s.threads) {
    if (t.samples && (!t.states || t.states.length !== prof.states.length)) {
      fail('状態の枠が states と同じ数で読めていない');
    }
  }
}
// 段階の合計は、生の JSON を自分で足したものと一致すること
const n5 = Math.max(1, Math.round(300 / prof.interval));
let checkedStages = 0;
for (const kind of ['connect', 'forward']) {
  const rows = api.stageRows(prof, n5, kind);
  const idx = kind === 'connect' ? 3 : 4;
  const from = Math.max(0, pj.samples.length - n5);
  for (let j = 0; j < rows.rows.length; j++) {
    let count = 0, sum = 0;
    for (let k = from; k < pj.samples.length; k++) {
      const w = pj.samples[k][idx][j];
      if (w) { count += w[0]; sum += w[1]; }
    }
    if (rows.rows[j].count !== count) fail(kind + ' の ' + rows.rows[j].name + ' の件数 ' + rows.rows[j].count + ' != ' + count);
    if (rows.rows[j].sum !== sum) fail(kind + ' の ' + rows.rows[j].name + ' の合計が合わない');
    if (count) checkedStages++;
  }
  // 割合の合計は 100% (どの段階も観測されていなければ 0)
  const share = rows.rows.reduce((a, r) => a + r.share, 0);
  if (rows.total > 0 && Math.abs(share - 100) > 0.01) fail(kind + ' の割合の合計が 100% でない: ' + share);
  // 「確立まで」と「その後」に分けて積む (段階の合計は利用者が待った時間ではない)
  if (Math.abs(rows.setupMs + rows.afterMs - rows.total) > 1e-9) fail(kind + ' の切れ目が合わない');
  if (rows.setup.length !== 5) fail(kind + ' の「確立まで」は 5 段のはず');
}
if (checkedStages === 0) fail('段階が 1 つも観測されていない出力 (ベンチを流したプロキシの出力を渡すこと)');
// 先頭の 1 行 (いちばん長い段階) は降順で 3 つまで
const lead = api.longestStage(prof, n5, 'connect');
if (!lead) fail('CONNECT の「いちばん長い段階」が出ない');
if (lead.top.length > 3) fail('上位 3 つを超えた');
for (let i = 1; i < lead.top.length; i++) {
  if (lead.top[i - 1].avg < lead.top[i].avg) fail('いちばん長い段階の並びが降順でない');
}
// 役割: CPU の多い順、状態の割合は 100% 以下
const roles = api.roleRows(prof, n5);
if (!roles.length) fail('役割が 1 つも出ない');
for (let i = 1; i < roles.length; i++) {
  if (roles[i - 1].cpu_us < roles[i].cpu_us) fail('役割が CPU の多い順になっていない');
}
for (const r of roles) {
  if (!(r.cpu_pct >= 0)) fail(r.role + ' の CPU % が数でない');
  const pct = r.top.reduce((a, x) => a + x.pct, 0);
  if (pct > 100.01) fail(r.role + ' の状態の割合が 100% を超えた: ' + pct);
  for (const x of r.top) {
    if (!prof.states.includes(x.name)) fail('知らない状態の名前: ' + x.name);
  }
}
if (!roles.some((r) => r.role === 'conn')) fail('conn 役が出ていない (ベンチを流した出力のはず)');
// ロック: 窓の増分は通算以下
const lk = api.lockRows(prof, n5);
if (lk.rows.length !== prof.lockNames.length) fail('ロックの行が足りない');
for (const r of lk.rows) {
  if (r.window > r.total) fail(r.name + ' の窓 ' + r.window + ' が通算 ' + r.total + ' を超えた');
}
if (lk.queue.waited > lk.queue.total.waited) fail('待ち行列の窓が通算を超えた');
// lite と壊れた入力でも例外を出さない
if (!api.toProfile({ profile: 'off' }).off) fail('lite の出力を off と読めていない');
if (api.toProfile({}).samples.length !== 0) fail('空でも 0 件のはず');
if (api.toProfile(null).samples.length !== 0) fail('null でも 0 件のはず');
if (api.stageRows(api.toProfile(null), 60, 'connect').rows.length !== 0) fail('空でも例外なし');
if (api.longestStage(api.toProfile(null), 60, 'connect') !== null) fail('空なら null のはず');
if (api.roleRows(api.toProfile(null), 60).length !== 0) fail('空でも 0 件のはず');
if (api.lockRows(api.toProfile(null), 60).rows.length !== 0) fail('空でも 0 件のはず');

const connLead = api.longestStage(prof, n5, 'connect');
const fwdLead = api.longestStage(prof, n5, 'forward');

