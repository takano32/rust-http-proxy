// `scripts/check-dashboard.js` の節: 1. 構文 (T.34.140 で割った。単体では動かさない。
// 前の節の `const` を使うので、`check-dashboard.js` が名前の順につないで 1 つの本文として回す)
// 1. 構文
try {
  new Function(js);
} catch (e) {
  fail('JS の構文エラー: ' + e.message);
}

