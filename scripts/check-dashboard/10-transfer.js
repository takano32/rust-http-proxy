// `scripts/check-dashboard.js` の節: 9. /history の transfer (T34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 9. `/history` の `transfer` (T14.25)。`closed` と同じく**標本とは別の配列**なので、
// ここも既存の読み方に 1 行も触らずに足せる。作り置きの `/history` は T14.25 より前に
// 取ったものなので、実出力にあれば読み、無ければ見本だけで読み方を確かめる。
function transferRows(transfer) {
  if (!transfer || !Array.isArray(transfer.samples)) return [];
  const keys = transfer.keys || [];
  return transfer.samples.map((row) => {
    const o = {};
    keys.forEach((k, i) => {
      o[k] = row[i];
    });
    return o;
  });
}

function checkTransfer(transfer, where) {
  const rows = transferRows(transfer);
  if (rows.length === 0) return 0;
  const speedBounds = transfer.speed_bounds_bps || [];
  const halfBounds = transfer.half_close_bounds_ms || [];
  if (speedBounds.length !== 12) fail(where + ': 速さの区間が 12 段でない: ' + speedBounds.length);
  if (halfBounds.length !== 12) fail(where + ': 半閉じの区間が 12 段でない: ' + halfBounds.length);
  // 等比 (4 倍ずつ) であること。境目が動いたら分布の読み方が変わるのでここで気づく
  for (const b of [speedBounds, halfBounds]) {
    for (let i = 1; i < b.length; i += 1) {
      if (b[i] !== b[i - 1] * 4) fail(where + ': 区間が 4 倍ずつでない: ' + b.join(','));
    }
  }
  if (transfer.min_bytes !== 1024) fail(where + ': 速さを数える下限が 1 KiB でない');
  const sum = (a) => a.reduce((x, y) => x + y, 0);
  for (const r of rows) {
    if (!(r.t > 0)) fail(where + ': 窓の時刻が epoch 秒で読めていない: ' + r.t);
    if (!(r.tunnels > 0)) fail(where + ': 本数 0 の窓が出ている (残さないはず)');
    if (r.speed.length !== speedBounds.length + 1) fail(where + ': speed が区間 + 1 でない');
    if (r.half_close.length !== halfBounds.length + 1) fail(where + ': half_close が区間 + 1 でない');
    if (sum(r.speed) !== r.speed_n) fail(where + ': speed の合計 ' + sum(r.speed) + ' != ' + r.speed_n);
    if (sum(r.half_close) !== r.half_close_n) fail(where + ': half_close の合計が件数と合わない');
    // 速さを数えるのは 1 KiB 以上運んだトンネルだけなので、本数より多くはならない
    if (r.speed_n > r.tunnels) fail(where + ': 速さの件数が本数を超えた');
    if (r.half_close_n > r.tunnels) fail(where + ': 半閉じの件数が本数を超えた');
    if (r.speed_n > 0 && !(r.bytes_sum > 0)) fail(where + ': 運んだバイトの合計が読めていない');
    if (!(r.relay_ms_sum >= 0) || !(r.half_close_ms_sum >= 0)) fail(where + ': 合計が数で読めていない');
    // 中継の詰まりの向き (T14.42)。古い `/history` には無いので「あれば数」だけ見る
    for (const k of ['stall_client_ms_sum', 'stall_origin_ms_sum']) {
      if (r[k] !== undefined && !(r[k] >= 0)) fail(where + ': ' + k + ' が数で読めていない: ' + r[k]);
    }
  }
  // 窓を 1 つに畳む (T14.8 が「直近 1 時間の速さの分布」を描くときの読み方)
  const merged = rows.reduce((a, r) => {
    a.speed_n += r.speed_n;
    r.speed.forEach((n, i) => (a.speed[i] += n));
    return a;
  }, { speed_n: 0, speed: rows[0].speed.map(() => 0) });
  if (sum(merged.speed) !== merged.speed_n) fail(where + ': 畳んだ速さの件数が合わない');
  return rows.length;
}

const transferSample = {
  interval_secs: 5,
  keys: ['t', 'tunnels', 'speed_n', 'speed', 'half_close_n', 'half_close', 'bytes_sum', 'relay_ms_sum', 'half_close_ms_sum', 'stall_client_ms_sum', 'stall_origin_ms_sum'],
  speed_bounds_bps: [1024, 4096, 16384, 65536, 262144, 1048576, 4194304, 16777216, 67108864, 268435456, 1073741824, 4294967296],
  half_close_bounds_ms: [1, 4, 16, 64, 256, 1024, 4096, 16384, 65536, 262144, 1048576, 4194304],
  min_bytes: 1024,
  samples: [
    [1789251460, 3, 2, [0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0], 1, [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0], 1258291, 4200, 150, 1800, 0],
    [1789251465, 1, 1, [0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0], 0, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 20971520, 2000, 0, 0, 0],
  ],
  windows: 2,
  capacity: 720,
  recorded: 4,
};
const transferWindows =
  checkTransfer(transferSample, '見本') + checkTransfer(hist.transfer, path.basename(file));
if (transferWindows < 2) fail('速さと半閉じの分布を 1 窓も読めていない');
if (transferRows(null).length !== 0) fail('transfer が無くても例外なく 0 件のはず');
if (transferRows({ samples: [] }).length !== 0) fail('空の窓でも 0 件のはず');

