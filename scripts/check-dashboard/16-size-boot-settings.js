// `scripts/check-dashboard.js` の節: 17. dashboard.html の大きさ・18. 起動のカード・19. 設定の帯 (T34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 17. `dashboard.html` の大きさ。inspect は Rust 側で 64 KiB を見張っているのに
// dashboard には上限が無かった (T15.0 で足した)。外部ライブラリを読み込まない
// 1 ページという方針を守るための歯止めで、超えたら**中身を削るか上限を上げるか**を先に決める
// T28.25 (2026-10-25) で 80 → 96 KiB に上げた (余白が 1,405 B しか無く、起動のカードを足すと越えた)。
// inspect.html と同じ上限。コメントを削って入れる案は、読み手のための注をなくすので採らなかった
const DASH_MAX = 96 * 1024;

// 18. 起動のカード (T28.25): `/events` から「普通の再起動 / 無言の再起動」を決める `bootKind`
const bootStart = (at, boot) => ({ at, kind: 'start', text: 'version 0.28.0+abc on port 60357 boot ' + boot + ' (profile default)' });
let bk = api.bootKind([bootStart(100, 'aaaa'), { at: 200, kind: 'shutdown', text: 'stop' }, bootStart(230, 'aaaa'),
  { at: 230, kind: 'anomaly', text: '同じ秒の出来事は直前と見ない' }]);
if (bk.kind !== 'graceful' || bk.down !== 30 || bk.version !== '0.28.0+abc' || bk.boot !== 'aaaa') fail('bootKind が普通の再起動を読めない: ' + JSON.stringify(bk));
bk = api.bootKind([bootStart(100, 'aaaa'), { at: 150, kind: 'anomaly', text: 'x' }, bootStart(300, 'bbbb'),
  { at: 310, kind: 'start', text: 'self bench: 自己ベンチの start は起動ではない' }]);
if (bk.kind !== 'silent' || bk.at !== 300) fail('bootKind が無言の再起動を読めない: ' + JSON.stringify(bk));
if (api.bootKind([bootStart(100, 'aaaa')]).kind !== 'edge') fail('bootKind: 窓の先頭の start は edge');
if (api.bootKind([]).kind !== 'none' || api.bootKind(undefined).kind !== 'none') fail('bootKind: 出来事が無いときは none');
// 実データ (匿名化した雪像に /events があれば) でも落ちない
const realEvents = statusFile && fs.existsSync(statusFile) ? (JSON.parse(fs.readFileSync(statusFile, 'utf8')).events || null) : null;
const realKind = realEvents ? api.bootKind(realEvents.events || realEvents).kind : 'none';
console.log('OK: 起動のカード (T28.25) の bootKind: 普通の再起動・無言の再起動・窓の先頭・記録なしを読み分けた (' +
  (realEvents ? '実データ ' + realKind : '実データに /events なし') + ')');

// 19. 設定の帯・版の隣のビルド・`/slo` の読み方 (T29.27)
// (1) `.env` の読めない値と知らない鍵 (T28.6 の `settings.invalid` / `unknown`) を 1 つ 1 行で
const stBad = {
  path: '/home/container/.env', watch: 'inotify', reloads: 0, last_reload: 0, applied: [], restart_required: [], error: null,
  invalid: [{ key: 'PROXY_MAX_CONNS', value: 'lots' }, { key: 'PROXY_SLO', value: '<b>' }],
  unknown: [{ key: 'PROXY_MAX_CONN', suggest: 'PROXY_MAX_CONNS' }, { key: 'FOO_BAR', suggest: null }],
};
const sl = api.settingsLines(stBad);
if (sl.length !== 4) fail('読めない値 2 つと知らない鍵 2 つが 1 つ 1 行で出ていない: ' + JSON.stringify(sl));
if (sl[0].indexOf('読めない値') < 0 || sl[0].indexOf('PROXY_MAX_CONNS=lots') < 0) fail('読めない値の行が無い: ' + sl[0]);
if (sl[1].indexOf('&lt;b&gt;') < 0 || sl[1].indexOf('<b>読めない値') !== 0) fail('読めない値が逃がされていない: ' + sl[1]);
if (sl[2].indexOf('知らない鍵') < 0 || sl[2].indexOf('もしかして <code>PROXY_MAX_CONNS</code>') < 0) fail('知らない鍵の候補が無い: ' + sl[2]);
if (sl[3].indexOf('FOO_BAR') < 0 || sl[3].indexOf('もしかして') >= 0) fail('候補の無い知らない鍵に候補が付いた: ' + sl[3]);
// 正しい `.env` (`[]`) と、欄の無い古い版 (T28.6 より前) では何も出さない (帯は隠れたまま)
if (api.settingsLines(Object.assign({}, stBad, { invalid: [], unknown: [] })).length !== 0) fail('正しい .env で行が出た');
const stOld = { path: '.env', watch: 'poll', reloads: 1, last_reload: 0, applied: [], restart_required: [], error: null };
if (api.settingsLines(stOld).length !== 0 || api.settingsLines(null).length !== 0) fail('欄の無い古い版で行が出た');
// 前からある 2 行 (解釈の失敗・再起動が要る) はそのまま
if (api.settingsLines(Object.assign({}, stOld, { error: 'x', restart_required: ['PROXY_THREADS'] })).length !== 2) fail('解釈の失敗と再起動の行が変わった');
// (3) 版の隣にビルド
if (api.versionLabel({ version: '0.29.35+b2b26c1', build: 'release' }) !== '0.29.35+b2b26c1 (release)') fail('版の隣にビルドが出ない');
if (api.versionLabel({ version: '0.28.30+abc' }) !== '0.28.30+abc') fail('build の無い版で括弧が出た');
if (api.versionLabel({}) !== '') fail('版の無い出力で空にならない');
// (4) `/slo` の `basis` (T28.45) があれば tooltip は閾ごとの読み方。無い版は旧い読み方と断る
const basis = { connect: 'establish_minus_rtt', error_rate: 'resolved_attempts', dns_miss_per_connect: 'excluding_failed_lookups' };
const sloNew = Object.assign({}, sloJson, { basis });
const tNew = api.sloTitle(sloNew);
for (const part of ['読み方 (basis)', '確立 p50 ≤ 10 ms', 'RTT を引いた値', '名前解決に成功した試行あたり', '失敗した問い合わせを除いた']) {
  if (tNew.indexOf(part) < 0) fail('basis のある /slo の tooltip に ' + part + ' が無い: ' + tNew);
}
const tOld = api.sloTitle(sloJson);
if (tOld.indexOf('basis') >= 0 || tOld.indexOf('確立そのもの') < 0) fail('basis の無い /slo の tooltip: ' + tOld);
// 外れの最多が確立の閾なら、内訳にも「確立 − RTT」と添える (basis が無い版は今までどおり)
if (api.sloKpi(sloNew).detail.indexOf('閾 10、確立 − RTT') < 0) fail('basis のある内訳に確立 − RTT が無い: ' + api.sloKpi(sloNew).detail);
if (api.sloKpi(sloJson).detail.indexOf('確立 − RTT') >= 0) fail('basis の無い内訳に確立 − RTT が出た');
console.log('OK: 設定の帯 (読めない値 2・知らない鍵 2 を 1 行ずつ、候補つき。欄の無い版は 0 行)・版の隣のビルド (' +
  api.versionLabel({ version: '0.29.35+b2b26c1', build: 'release' }) + ')・/slo の basis の tooltip (あり / なし) を確かめた (T29.27)');

const dashBytes = Buffer.byteLength(html);
if (dashBytes > DASH_MAX) {
  fail('dashboard.html が ' + DASH_MAX + ' B を超えた: ' + dashBytes + ' B');
}

console.log(
  'OK: T15.0 の 5 枚のカードも通った: (a) CPU の絞り ' +
    cpu.pct.toFixed(2) + '% (窓 ' + cpu.window.periods + ' 周期、割り当て ' + cpu.quota + ' コア)、' +
    '(b) 動かないトンネル ' + idle.count + ' / ' + idle.tunnels + ' 本 (空回り ' + idle.spins + ')、' +
    '(c) ミスの種類 ' + missKinds.rows.map((r) => r.name + ' ' + r.avg).join(' · ') + '、' +
    '(d) 受付待ち ' + qs.count + ' 件 (p50 ' + api.fmtMsFine(qs.p50) + ' / p95 ' + api.fmtMsFine(qs.p95) +
    ' の二峰、上位スレッド ' + tt.length + ' 本、run_delay ' + rd.total + ' us)、' +
    '(e) 利用者が待つ時間 p50 ' + api.fmtMsFine(wexact.p50) + ' (無い版では区間の補間 ' +
    api.fmtMsFine(wfell.p50) + '、どちらも無い版では出さない)。' +
    '欄の無い古い版 (' + path.basename(file) + ' / ' + path.basename(statusFile) + ' / ' +
    path.basename(profFile) + ' から T15.0 の欄を落としたもの) では 5 枚とも「無い」に落ちた。' +
    'dashboard.html は ' + dashBytes + ' B / 上限 ' + DASH_MAX + ' B'
);

