// `scripts/check-dashboard.js` の節: 4. 個票 (/errors /connections) (T.34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 4. 個票 (`/errors` `/connections`) を読む関数 (T13.4)。実出力の作り置きは無いので、
// 形だけ同じ架空のデータで見る (ホスト名は架空、接続元はドキュメント用の範囲)
const errJson = {
  errors: [
    { at: 1789251465, kind: 'connect', target: 'a.example.net:443', cause: 'dns', dns_ms: 2013, connect_ms: 0, status: 502, client: '198.51.100.7' },
    { at: 1789251400, kind: 'forward', target: 'http://b.example.net:80', cause: 'refused', dns_ms: 0, connect_ms: 1, status: 502, client: '198.51.100.8' },
  ],
  count: 2, kept: 2, capacity: 500, recorded: 2, truncated: false,
};
const errRows = api.errorRows(errJson, 20);
if (errRows.length !== 2) fail('errorRows の件数が合わない: ' + errRows.length);
if (errRows[0].cause !== 'dns' || errRows[0].dns_ms !== 2013) fail('errorRows が読めていない');
if (errRows[0].at <= 0) fail('時刻が epoch 秒で読めていない');
if (api.errorRows(errJson, 1).length !== 1) fail('n で絞れていない');
if (api.errorRows({}, 20).length !== 0) fail('空でも例外なく 0 件のはず');
if (api.errorRows(null, 20).length !== 0) fail('null でも例外なく 0 件のはず');
// 古い出力 (キーが無い) でも落ちない
if (api.errorRows({ errors: [{}] }, 20)[0].cause !== '') fail('無いキーは空文字のはず');
// まとめた行 (T20.3 の `repeats` / `last_at`) は「最近のエラー」の行に ×N と最後の時刻を出す (T21.5)。
// `repeats` の無い版と `repeats` 1 の行は今までどおり (×N も「〜」も出さない)
const repJson = {
  errors: [
    { at: 1789251400, kind: 'connect', target: 'c.example.net:443', cause: 'refused', dns_ms: 0, connect_ms: 1, status: 502, client: '198.51.100.7', repeats: 115, last_at: 1789597000 },
    { at: 1789251300, kind: 'connect', target: 'd.example.net:443', cause: 'timeout', dns_ms: 0, connect_ms: 10000, status: 504, client: '198.51.100.7', repeats: 1, last_at: 1789251300 },
  ],
};
const [rep, one] = api.errorRows(repJson, 20);
if (rep.repeats !== 115 || rep.last_at !== 1789597000) fail('repeats / last_at が読めていない: ' + JSON.stringify(rep));
if (errRows[0].repeats !== 1 || errRows[0].last_at !== null) fail('repeats の無い版は 1 回 / last_at null のはず');
const repHtml = api.errorRowHtml(rep);
if (repHtml.indexOf('×115') < 0) fail('まとめた行に ×N が出ていない: ' + repHtml);
const lastShown = new Date(1789597000 * 1000).toLocaleTimeString();
if (repHtml.indexOf('〜' + lastShown) < 0) fail('まとめた行に最後の時刻が出ていない: ' + repHtml);
if (repHtml.indexOf(new Date(1789251400 * 1000).toLocaleTimeString()) < 0) fail('最初の時刻が消えた');
for (const [what, row] of [['repeats の無い版', errRows[0]], ['repeats 1 の行', one]]) {
  const h = api.errorRowHtml(row);
  if (h.indexOf('×') >= 0 || h.indexOf('〜') >= 0) fail(what + 'に ×N か「〜」が出た: ' + h);
  if ((h.match(/<td/g) || []).length !== 8) fail(what + 'の列が 8 つでない');
}
if ((repHtml.match(/<td/g) || []).length !== 8) fail('まとめた行の列が 8 つでない');

const connJson = {
  connections: [
    { id: 3, client: '198.51.100.7', target: 'a.example.net:443', kind: 'connect', state: 'parked', age_secs: 120, bytes: 4096, fds: 2, rate_bps: 0 },
    { id: 9, client: '198.51.100.9', target: '', kind: 'http', state: 'serving', age_secs: 0, bytes: 0, fds: 1, rate_bps: 0 },
    { id: 5, client: '198.51.100.8', target: 'b.example.net:443', kind: 'connect', state: 'relaying', age_secs: 900, bytes: 1048576, fds: 2, rate_bps: 1048576 },
  ],
  count: 3, shown: 3, truncated: false, lite: false,
};
const conn = api.connRows(connJson, 50);
if (conn.rows.length !== 3) fail('connRows の件数が合わない');
// 長く居る順 (経過の降順)
for (let i = 1; i < conn.rows.length; i++) {
  if (conn.rows[i - 1].age_secs < conn.rows[i].age_secs) fail('connRows の並びが崩れた');
}
if (conn.rows[0].id !== 5) fail('いちばん長く居る接続が先頭でない: ' + conn.rows[0].id);
if (conn.kinds.connect !== 2 || conn.kinds.http !== 1) fail('種類の内訳が合わない');
if (conn.states.parked !== 1 || conn.states.relaying !== 1 || conn.states.serving !== 1) {
  fail('状態の内訳が合わない: ' + JSON.stringify(conn.states));
}
if (conn.bytes !== 4096 + 1048576) fail('転送の合計が合わない: ' + conn.bytes);
if (conn.count !== 3) fail('count が読めていない');
// 直近 5 秒の転送速度 (T14.39)。描くのは画面の仕事なので、ここは行に残ることだけ見る
if (conn.rows[0].rate_bps !== 1048576) fail('rate_bps が行に残っていない: ' + conn.rows[0].rate_bps);
if (api.connRows(connJson, 1).rows.length !== 1) fail('n で絞れていない');
const lite = api.connRows({ connections: [], count: 0, lite: true }, 50);
if (lite.rows.length !== 0 || !lite.lite) fail('lite の空一覧が読めていない');
if (api.connRows({}, 50).rows.length !== 0) fail('空でも例外なく 0 件のはず');
if (api.connRows(null, 50).count !== 0) fail('null でも例外なく 0 件のはず');

