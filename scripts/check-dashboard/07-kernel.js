// `scripts/check-dashboard.js` の節: 6. カーネルと cgroup の窓 (T.34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 6. カーネルと cgroup の窓 (`/history` の `kernel` と `/status` の `kernel`。T14.12)。
//    実出力の作り置きは `kernel` より前の版なので、**あれば読む**形にしてある。
//    形だけ同じ架空のデータでも 1 回通す (読み方が壊れたらここで気づける)
function checkKernelHistory(k, where) {
  if (!k) return 0;
  if (!Array.isArray(k.keys) || !Array.isArray(k.samples)) fail(where + ' の kernel の形が違う');
  for (const want of ['t', 'time_wait', 'psi_cpu_some_avg10', 'listen_overflows']) {
    if (k.keys.indexOf(want) < 0) fail(where + ' の kernel に列 ' + want + ' が無い');
  }
  for (const row of k.samples) {
    if (!Array.isArray(row) || row.length !== k.keys.length) {
      fail(where + ' の kernel の列の数が keys と合わない: ' + JSON.stringify(row));
    }
    for (let i = 0; i < row.length; i++) {
      // 読めない源は null、読めた源は数 (文字列や undefined が混ざったら形が壊れている)
      if (row[i] !== null && typeof row[i] !== 'number') {
        fail(where + ' の kernel の ' + k.keys[i] + ' が数でも null でもない: ' + JSON.stringify(row[i]));
      }
    }
    if (row[0] === null || !(row[0] > 0)) fail(where + ' の kernel の t が時刻でない');
  }
  return k.samples.length;
}

const fakeKernel = {
  interval_secs: 5,
  keys: [
    't', 'listen_overflows', 'listen_drops', 'tcp_timeouts', 'syn_retrans', 'abort_on_timeout',
    'retrans_segs', 'curr_estab', 'sockets_inuse', 'time_wait', 'sockets_alloc', 'sockets_mem',
    'cpu_nr_throttled', 'cpu_throttled_usec',
    'psi_cpu_some_avg10', 'psi_cpu_full_avg10', 'psi_mem_some_avg10', 'psi_mem_full_avg10',
    'psi_io_some_avg10', 'psi_io_full_avg10', 'dns_misses', 'dns_miss_ms', 'state_file_errors',
  ],
  samples: [
    // 読めた環境 (Linux + cgroup v2 + PSI)
    [1789251465, 0, 0, 2, 1, 0, 3, 16, 43, 30912, 50, 0, 0, 0, 43.02, 2.99, 0, 0, 0.13, 0.13, 2, 13, 0],
    // 読めない環境 (cgroup v1 / PSI 無し / `/proc/net` の無いコンテナ)
    [1789251470, null, null, null, null, null, null, null, null, null, null, null, null, null,
      null, null, null, null, null, null, 0, null, null],
  ],
};
if (checkKernelHistory(fakeKernel, '作り置き') !== 2) fail('作り置きの kernel を読めていない');
// 実出力にあれば読む (`res=3600` は `null` = この解像度には窓が無い、も正しい形)
const kernelRows = checkKernelHistory(hist.kernel, path.basename(file));
// `/status` の `kernel` の節 (最新の値と累計)。無い版の出力でも落ちない
if (st.kernel) {
  if (typeof st.kernel.at !== 'number') fail('/status の kernel.at が数でない');
  if (!st.kernel.tcp) fail('/status の kernel に tcp が無い');
  for (const k of ['listen_overflows', 'time_wait']) {
    const v = st.kernel.tcp[k];
    if (v !== null && typeof v !== 'number') fail('/status の kernel.tcp.' + k + ' が数でも null でもない');
  }
  if (!st.kernel.last_5m) fail('/status の kernel に last_5m が無い');
}

