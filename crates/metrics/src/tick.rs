//! 5 秒ごとに標本を取る記録スレッド (`history` スレッド)。
//!
//! **ここに置いてあるのは層の都合**: 1 周の中で状態ファイル ([`crate::persist`]) へ書き、
//! 日次 ([`crate::daily`])・雪像 ([`crate::snapshots`])・異常 ([`crate::anomaly`])・
//! SLO ([`crate::slo`]) を呼ぶ = **いちばん上の層が全部要る**。窓そのもの
//! ([`crate::history`]) は下の層 (`proxy-metrics-core`) にあり、そちらは何も知らない
//! (T14.55 でクレートを割ったときに、この 1 関数だけを上へ出した)。

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::cache::Cache;
use crate::history::{INTERVAL, Sample};
use crate::metrics::Metrics;

/// 定期的に記録するスレッドを起動する。記録先は `metrics.history`、`store` があれば状態ファイルにも。
pub fn spawn(
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    store: Option<Arc<crate::persist::Store>>,
) -> JoinHandle<()> {
    spawn_every(metrics, cache, store, INTERVAL)
}

/// 周期を指定して起こす版 (**結合テスト用**。本番は [`spawn`] = [`INTERVAL`])。
///
/// 山の写真 (T14.6) を撮るのがこのスレッドなので、テストで 5 秒待たずに
/// 「越えた → 1 枚撮れた」を見るための口。
pub fn spawn_every(
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    store: Option<Arc<crate::persist::Store>>,
    interval: Duration,
) -> JoinHandle<()> {
    // 前に転送速度を控えた時刻 (T14.39)。1 回目は `None` = 控えるだけで速さは出さない
    let mut swept: Option<Instant> = None;
    let mut record = move |metrics: &Arc<Metrics>, cache: &Cache| {
        // いまの転送速度 (`/connections` の `rate_bps`。T14.39)。全 slot の `bytes` を
        // 控えて差分 ÷ この周期を書く。**書くのはこのスレッドだけ**で、接続の経路は 0 増
        metrics
            .conns
            .update_rates(swept.map_or(0, |t: Instant| t.elapsed().as_millis() as u64));
        swept = Some(Instant::now());
        // 山の写真と、閉じた接続の分布の窓 (T14.6)。**標本より先に**撮るのは、
        // 越えてから撮るまでを 1 周期より短くするため
        metrics.take_burst_shot();
        // 下の層 (IPv4 優先の切替・圧迫・バラスト) の変わり目を出来事に 1 件 (T14.11)
        crate::events::poll(cache);
        let now = crate::cache::now_epoch();
        metrics.history.closed.roll(now);
        // 速さと半閉じの窓も同じ境目で閉じる (`closed` と時刻で突き合わせる。T14.25)
        metrics.history.transfer.roll(now);
        // ホスト別の時系列の窓送りと上位 16 の入れ替え (T14.22)。**5 分の境目でだけ**動く
        metrics.roll_host_series();
        let sample = Sample::take(metrics, cache);
        // 日付が変わっていたら前日の要約を 1 行残す (T14.20)。書かない設定なら原子の読み 1 回
        crate::daily::tick(metrics, &sample);
        // 同じ境目で前日ぶんの `/snapshot` を 1 ファイル残す (T14.34)。こちらも
        // 書かない設定なら原子の読み 1 回で戻る (組むのは日付が変わったときだけ)
        crate::snapshots::tick(cache, &sample);
        let pushed = metrics.history.push(sample);
        // 積んだあとに、直近 5 分が直近 1 時間の基準値から外れていないかを見る (T14.23)
        crate::anomaly::check(metrics, &sample);
        // 同じ標本を SLO の 4 つの閾に当て、時間ごとの達成率に 1 本足す (`/slo`。T14.50)
        crate::slo::observe(&sample);
        if let Some(st) = &store {
            st.write_samples(&pushed);
            st.write_recent(metrics);
        }
        // 利用者の要求が無い時間帯も待ちを測る (T14.10)。**ここでは測らない**
        // (名前解決と接続は `canary` スレッド 1 本の仕事で、この周期は止めない)
        crate::canary::tick(metrics);
    };
    record(&metrics, &cache);
    thread::Builder::new()
        .name("history".into())
        .spawn(move || {
            loop {
                thread::sleep(interval);
                record(&metrics, &cache);
            }
        })
        .expect("spawn history thread")
}
