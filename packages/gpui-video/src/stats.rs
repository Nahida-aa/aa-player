//! 播放性能统计。
//!
//! 判定"卡不卡"看的是**尾部分布**而非平均值——平均值会把偶发卡顿完全抹平，
//! 这正是早期排查卡顿时长期没能定位问题的原因（见
//! `docs/debugging-playback-jank.md`）。因此这里维护帧间隔直方图，
//! 由此导出 p99 与准时率。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// 帧间隔直方图的桶上界（毫秒）。以 33.3ms(30fps) 为基准划分：
/// 卡顿的定义是"帧间隔显著偏离标称值"，所以桶要密集分布在 33 附近，
/// 而不是等宽——等宽桶会把 33 和 40 归到一起，恰好看不见我们要找的抖动。
pub const INTERVAL_BUCKETS_MS: [u64; 9] = [20, 28, 32, 35, 38, 42, 50, 66, 100];

/// 直方图桶数（含末尾的溢出桶）。
pub const HIST_LEN: usize = INTERVAL_BUCKETS_MS.len() + 1;

/// 音画同步可接受的最大漂移（毫秒）。
///
/// 人耳对声画错位的可感阈值约 ±40ms（口语对白约 45ms，乐句更严）。
/// 超过这个量就该预警，但单次偶发（一次调度抖动）不必报警——
/// 所以真正看的是**超出占比**而非单帧值。
pub const AV_SYNC_TOLERANCE_MS: i64 = 40;

/// 播放性能计数器。仅当 debug 级别开启时才记录，
/// 关闭时完全不调用，无原子操作开销。
#[derive(Default)]
pub struct ProfileStats {
    /// worker 成功送出的帧数（解码产出）。
    decoded: AtomicU64,
    /// 渲染端实际显示的帧数（update 次数）。
    displayed: AtomicU64,
    /// 解码+像素转换总耗时（微秒）。
    decode_total_us: AtomicU64,
    /// 解码+转换的样本数。
    decode_count: AtomicU64,
    /// 两次显示之间的最大间隔（毫秒），反映最坏抖动。
    max_interval_ms: AtomicU64,
    /// 帧间隔直方图，桶边界见 [`INTERVAL_BUCKETS_MS`]，末桶为溢出桶。
    /// 有了分布才能算 p99——平均值会把偶发卡顿完全抹平。
    interval_hist: [AtomicU64; HIST_LEN],
    /// 帧间隔总和（毫秒），配合 displayed 算平均。
    interval_total_ms: AtomicU64,
    /// 上次显示时刻，用于算间隔。
    last_display: Mutex<Option<Instant>>,

    /// 音画同步漂移累计（微秒）。`drift = 音频主时钟读数 - 帧 PTS`：
    /// 正值表示画面落后音频（lag），负值表示画面领先音频（lead）。
    /// 仅在音频主时钟模式下、显示实际帧时累计；丢帧不计入。
    av_sync_sum_us: AtomicU64,
    /// 漂移平方累计（微秒²），用于算 RMS——比峰值更能反映整体稳定性，
    /// 单次抖动不会像 max 那样把整体画像带偏。
    av_sync_sum_sq_us: AtomicU64,
    /// 样本数（已显示且带音频时钟的帧数）。
    av_sync_count: AtomicU64,
    /// 最大落后量（drift>0，微秒）。
    av_sync_max_lag_us: AtomicU64,
    /// 最大领先量（|drift|<0，微秒）。
    av_sync_max_lead_us: AtomicU64,
    /// 超出 [`AV_SYNC_TOLERANCE_MS`] 的帧数。
    av_sync_out_of_range: AtomicU64,
}

impl ProfileStats {
    /// 记录一帧解码完成。
    ///
    /// 注意：必须在解码**真正完成时**调用，不能挂在"投递成功"的分支上。
    /// 早期版本把它放进 `try_send` 的 else 分支，导致队列满时计数归零，
    /// 统计显示 fps=0，被误判成解码线程死亡。
    pub fn record_decoded(&self, us: u64) {
        self.decoded.fetch_add(1, Ordering::Relaxed);
        self.decode_total_us.fetch_add(us, Ordering::Relaxed);
        self.decode_count.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录一帧上屏，并累计帧间隔分布。
    pub fn record_displayed(&self) {
        self.displayed.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut lg = self.last_display.lock().unwrap();
        if let Some(prev) = *lg {
            // 用微秒算再转毫秒：直接 as_millis 会把 33.9ms 截断成 33，
            // 累积起来足以让平均值失真。
            let us = now.duration_since(prev).as_micros() as u64;
            let ms = us / 1000;
            self.interval_total_ms.fetch_add(ms, Ordering::Relaxed);
            self.max_interval_ms.fetch_max(ms, Ordering::Relaxed);

            let idx = INTERVAL_BUCKETS_MS
                .iter()
                .position(|&b| ms < b)
                .unwrap_or(INTERVAL_BUCKETS_MS.len());
            self.interval_hist[idx].fetch_add(1, Ordering::Relaxed);
        }
        *lg = Some(now);
    }

    /// 记录一次音画同步漂移（微秒）。
    ///
    /// `drift_us = 音频主时钟读数(us) - 帧 PTS(us)`。
    /// 正值 = 画面落后音频（lag），负值 = 画面领先（lead）。
    ///
    /// 只统计**真正显示出来**的帧——丢帧(`Schedule::Drop`)不计入，
    /// 否则"为了追赶而主动丢帧"会被算成漂移，反而掩盖同步质量。
    pub fn record_av_sync(&self, drift_us: i64) {
        self.av_sync_count.fetch_add(1, Ordering::Relaxed);
        // 平方与绝对值用无符号累计；符号方向另由两个 max 字段区分。
        let a = drift_us.unsigned_abs();
        self.av_sync_sum_us
            .fetch_add(a, Ordering::Relaxed);
        // 平方：drift 量级几十 ms 内，u64 不会溢出。
        self.av_sync_sum_sq_us
            .fetch_add(a.wrapping_mul(a), Ordering::Relaxed);
        if drift_us > 0 {
            self.av_sync_max_lag_us.fetch_max(a, Ordering::Relaxed);
        } else {
            self.av_sync_max_lead_us.fetch_max(a, Ordering::Relaxed);
        }
        if (drift_us / 1000).abs() > AV_SYNC_TOLERANCE_MS {
            self.av_sync_out_of_range.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 取出并清零本区间的全部统计，得到一个可直接上报的快照。
    ///
    /// 所有计数器一次性 swap 为 0，从而天然形成"区间速率"语义。
    // 与 player-app 保持一致的除法写法（避免除零），clippy 建议的
    // checked_div 在此语义下更啰嗦，故放行。
    #[allow(clippy::manual_checked_ops)]
    pub fn take_snapshot(&self, window_secs: u64) -> Snapshot {
        let mut hist = [0u64; HIST_LEN];
        let mut total = 0;
        for (i, slot) in self.interval_hist.iter().enumerate() {
            hist[i] = slot.swap(0, Ordering::Relaxed);
            total += hist[i];
        }

        let decoded = self.decoded.swap(0, Ordering::Relaxed);
        let displayed = self.displayed.swap(0, Ordering::Relaxed);
        let dt = self.decode_total_us.swap(0, Ordering::Relaxed);
        let dc = self.decode_count.swap(0, Ordering::Relaxed);
        let max_interval_ms = self.max_interval_ms.swap(0, Ordering::Relaxed);
        let sum_ms = self.interval_total_ms.swap(0, Ordering::Relaxed);

        let av_count = self.av_sync_count.swap(0, Ordering::Relaxed);
        let av_sum = self.av_sync_sum_us.swap(0, Ordering::Relaxed);
        let av_sum_sq = self.av_sync_sum_sq_us.swap(0, Ordering::Relaxed);
        let av_max_lag = self.av_sync_max_lag_us.swap(0, Ordering::Relaxed);
        let av_max_lead = self.av_sync_max_lead_us.swap(0, Ordering::Relaxed);
        let av_bad = self.av_sync_out_of_range.swap(0, Ordering::Relaxed);

        // RMS 漂移（毫秒）：sqrt(Σdrift² / n) / 1000。
        let av_rms_ms = if av_count > 0 {
            let mean_sq_us = av_sum_sq as f64 / av_count as f64;
            (mean_sq_us.sqrt() / 1000.0) as u64
        } else {
            0
        };
        // 有符号均值漂移（毫秒）：正负揭示系统性偏移方向——
        // 正值=画面持续落后音频，负值=持续领先。RMS 把符号抹掉了，看不到。
        let av_mean_ms = if av_count > 0 {
            av_sum as i64 / av_count as i64 / 1000
        } else {
            0
        };
        let av_bad_pct = if av_count > 0 {
            av_bad * 100 / av_count
        } else {
            0
        };

        let w = window_secs.max(1);
        Snapshot {
            decoded_fps: decoded / w,
            displayed_fps: displayed / w,
            avg_decode_us: if dc > 0 { dt / dc } else { 0 },
            avg_interval_ms: if total > 0 { sum_ms / total } else { 0 },
            p99_interval_ms: percentile(&hist, total, 0.99),
            max_interval_ms,
            on_time_pct: on_time_pct(&hist, total),
            hist,
            av_sync_mean_ms: av_mean_ms,
            av_sync_rms_ms: av_rms_ms,
            av_sync_max_lag_ms: av_max_lag / 1000,
            av_sync_max_lead_ms: av_max_lead / 1000,
            av_sync_bad_pct: av_bad_pct,
        }
    }
}

/// 一个统计区间的快照。
pub struct Snapshot {
    pub decoded_fps: u64,
    pub displayed_fps: u64,
    pub avg_decode_us: u64,
    pub avg_interval_ms: u64,
    pub p99_interval_ms: u64,
    pub max_interval_ms: u64,
    /// 帧间隔落在标称值附近（28~38ms）的占比，越接近 100 越稳。
    pub on_time_pct: u64,
    pub hist: [u64; HIST_LEN],
    /// 音画同步有符号均值漂移（毫秒）。> 0 画面持续落后音频，< 0 持续领先。
    /// 反映系统性偏移；抖动（正负相抵）会让它接近 0 而 RMS 仍大。
    pub av_sync_mean_ms: i64,
    /// 音画同步 RMS 漂移（毫秒）。综合正负方向的稳定度度量，
    /// 比单帧峰值更能反映"整体偏不偏"。
    pub av_sync_rms_ms: u64,
    /// 画面落后音频的最大量（毫秒，drift>0）。
    pub av_sync_max_lag_ms: u64,
    /// 画面领先音频的最大量（毫秒，drift<0）。
    pub av_sync_max_lead_ms: u64,
    /// 超出 [`AV_SYNC_TOLERANCE_MS`] 的帧占比（%）。偶发抖动不算问题，
    /// 真正要看的是这个占比有没有系统性地高。
    pub av_sync_bad_pct: u64,
}

impl Snapshot {
    /// 是否判定为卡顿。
    ///
    /// 判据按**人的感知**定，不按理论完美定：
    ///   - `max > 66ms`    ：至少掉了一整帧（2×33.3ms），必然可感
    ///   - `on_time < 90%` ：超过一成的帧偏离标称节奏，整体不稳
    ///
    /// 实测单帧晚 6ms（39ms）属于 timer 精度与合成器节拍的正常抖动，
    /// 肉眼不可感，不该报警——否则告警天天响，真出问题时反而被忽略。
    pub fn is_janky(&self) -> bool {
        self.max_interval_ms > 66 || self.on_time_pct < 90
    }

    /// 音画是否系统性失步。判据按**感知**定，不按理论完美定：
    /// 单次偶尔超 ±40ms 不可感（一次调度抖动），但若超过一成的帧都超阈值，
    /// 说明同步机制本身有漂移，应预警。同时极端单帧（>100ms）也直接判失步——
    /// 那已经是肉眼可见的对嘴型错位。
    pub fn is_av_out_of_sync(&self) -> bool {
        self.av_sync_bad_pct > 10 || self.av_sync_max_lag_ms > 100 || self.av_sync_max_lead_ms > 100
    }
}

/// 准时率：帧间隔落在「标称值上下约 25%」区间的占比。
///
/// 30fps 标称 33.3ms，加上合成器节拍与 timer 精度的正常抖动，
/// 真实间隔稳定在 20~42ms 都算准（1.26× 标称以内仍不可感）。
/// 对应桶下标 1..=5（桶边界见 [`INTERVAL_BUCKETS_MS`]：20~28 / 28~32 /
/// 32~35 / 35~38 / 38~42）。
///
/// 为什么不是更窄的 ±5ms：实测窗口显示正常播放的 `max_interval_ms`
/// 也常到 41ms，那是一次无害的调度抖动，不该被算成失准——否则会
/// 把健康播放天天报成卡顿，真掉帧时反而被淹没（见
/// `docs/debugging-playback-jank.md` 的"度量本身在骗人"）。真正的卡顿
/// 靠 `max > 66`（掉整帧）兜底，不靠准时率。
fn on_time_pct(hist: &[u64; HIST_LEN], total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    (hist[1] + hist[2] + hist[3] + hist[4] + hist[5]) * 100 / total
}

/// 从直方图估算分位数上界（毫秒）。返回该分位落入的桶上界，
/// 溢出桶返回 999。直方图只能给出桶粒度的估计，
/// 但判断"有没有卡顿"够用——我们关心的是量级而非精确值。
fn percentile(hist: &[u64], total: u64, p: f64) -> u64 {
    if total == 0 {
        return 0;
    }
    let target = (total as f64 * p).ceil() as u64;
    let mut cum = 0;
    for (i, &c) in hist.iter().enumerate() {
        cum += c;
        if cum >= target {
            return INTERVAL_BUCKETS_MS.get(i).copied().unwrap_or(999);
        }
    }
    999
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist_of(pairs: &[(usize, u64)]) -> [u64; HIST_LEN] {
        let mut h = [0u64; HIST_LEN];
        for &(i, c) in pairs {
            h[i] = c;
        }
        h
    }

    #[test]
    fn percentile_returns_bucket_upper_bound() {
        // 全部落在下标 3（32~35ms 桶）。
        let h = hist_of(&[(3, 100)]);
        assert_eq!(percentile(&h, 100, 0.99), 35);
    }

    /// 回归测试：p99 判据曾因桶太粗而永远不触发。
    /// 这里确认长尾能被正确识别到高位桶，而非被前面的大桶吞掉。
    #[test]
    fn percentile_detects_tail() {
        // 99 帧正常(下标3)，1 帧落到溢出桶。
        let h = hist_of(&[(3, 99), (HIST_LEN - 1, 1)]);
        assert_eq!(percentile(&h, 100, 0.99), 35, "p99 不该被单个离群值拉走");
        assert_eq!(percentile(&h, 100, 1.0), 999, "p100 应落到溢出桶");
    }

    #[test]
    fn percentile_handles_empty() {
        assert_eq!(percentile(&[0u64; HIST_LEN], 0, 0.99), 0);
    }

    #[test]
    fn on_time_counts_nominal_buckets() {
        // 准时窗口 = 桶 1..=5（20~42ms）。用一次真实稳态播放的分布：
        // 帧间隔集中在 28~42ms，外加 1 帧落在 42~50ms（桶6）作为轻微抖动。
        // 下标 1/2/3/4/5 共 58 帧，总 59 帧 → 准时率 98%。
        let h = hist_of(&[(1, 6), (2, 15), (3, 14), (4, 16), (5, 7), (6, 1)]);
        assert_eq!(on_time_pct(&h, 59), 98, "正常抖动应判为 98% 准时");
    }

    /// 回归测试：早先的准时窗口只覆盖 28~38ms，把正常 30fps 抖动
    /// （常到 41ms）算成失准，于是健康播放被天天报成卡顿。
    /// 这里确认该稳态分布**不**触发 `is_janky`。
    #[test]
    fn steady_state_playback_is_not_janky() {
        let s = Snapshot {
            decoded_fps: 30,
            displayed_fps: 30,
            avg_decode_us: 600,
            avg_interval_ms: 32,
            p99_interval_ms: 42,
            max_interval_ms: 41,
            on_time_pct: 98,
            hist: hist_of(&[(1, 6), (2, 15), (3, 14), (4, 16), (5, 7)]),
            av_sync_mean_ms: 3,
            av_sync_rms_ms: 4,
            av_sync_max_lag_ms: 9,
            av_sync_max_lead_ms: 8,
            av_sync_bad_pct: 0,
        };
        assert!(!s.is_janky(), "稳态播放（max=41ms）不该判为卡顿");
    }

    #[test]
    fn jank_detected_by_dropped_frame() {
        let mut s = Snapshot {
            decoded_fps: 30,
            displayed_fps: 30,
            avg_decode_us: 4000,
            avg_interval_ms: 33,
            p99_interval_ms: 38,
            max_interval_ms: 36,
            on_time_pct: 100,
            hist: [0; HIST_LEN],
            av_sync_mean_ms: 0,
            av_sync_rms_ms: 0,
            av_sync_max_lag_ms: 0,
            av_sync_max_lead_ms: 0,
            av_sync_bad_pct: 0,
        };
        assert!(!s.is_janky(), "健康数据不该判为卡顿");

        // 掉了一整帧。
        s.max_interval_ms = 70;
        assert!(s.is_janky());
    }

    #[test]
    fn jank_detected_by_low_on_time_rate() {
        let s = Snapshot {
            decoded_fps: 30,
            displayed_fps: 30,
            avg_decode_us: 4000,
            avg_interval_ms: 33,
            p99_interval_ms: 42,
            max_interval_ms: 50,
            on_time_pct: 80,
            hist: [0; HIST_LEN],
            av_sync_mean_ms: 0,
            av_sync_rms_ms: 0,
            av_sync_max_lag_ms: 0,
            av_sync_max_lead_ms: 0,
            av_sync_bad_pct: 0,
        };
        assert!(s.is_janky(), "准时率低于 90% 应判为卡顿");
    }

    /// 单帧晚 6ms 是正常调度抖动，不该报警（否则告警疲劳）。
    #[test]
    fn minor_jitter_is_not_jank() {
        let s = Snapshot {
            decoded_fps: 30,
            displayed_fps: 30,
            avg_decode_us: 4000,
            avg_interval_ms: 32,
            p99_interval_ms: 42,
            max_interval_ms: 39,
            on_time_pct: 98,
            hist: [0; HIST_LEN],
            av_sync_mean_ms: 0,
            av_sync_rms_ms: 0,
            av_sync_max_lag_ms: 0,
            av_sync_max_lead_ms: 0,
            av_sync_bad_pct: 0,
        };
        assert!(!s.is_janky());
    }

    // ---- 音画同步度量 ----

    /// 校准：零漂移应判为同步，各项指标为 0。
    #[test]
    fn av_sync_zero_when_no_drift() {
        let stats = ProfileStats::default();
        for _ in 0..100 {
            stats.record_av_sync(0);
        }
        let snap = stats.take_snapshot(2);
        assert_eq!(snap.av_sync_mean_ms, 0);
        assert_eq!(snap.av_sync_rms_ms, 0);
        assert_eq!(snap.av_sync_bad_pct, 0);
        assert!(!snap.is_av_out_of_sync());
    }

    /// 偶发大抖动不报警：单帧超阈值但占比低，属正常调度抖动。
    #[test]
    fn av_sync_tolerates_occasional_spike() {
        let stats = ProfileStats::default();
        // 100 帧里只有 1 帧飘到 60ms（<10% 阈值），其余都贴着 0。
        for _ in 0..99 {
            stats.record_av_sync(1_000); // 1ms
        }
        stats.record_av_sync(60_000); // 60ms 单次尖峰
        let snap = stats.take_snapshot(2);
        assert_eq!(snap.av_sync_bad_pct, 1, "仅 1/100 超阈值");
        assert!(!snap.is_av_out_of_sync(), "偶发尖峰不该判失步");
    }

    /// 系统性失步：超过一成的帧持续落后，应报警。
    #[test]
    fn av_sync_flags_systematic_lag() {
        let stats = ProfileStats::default();
        for _ in 0..50 {
            stats.record_av_sync(50_000); // 50ms 落后，超 ±40ms
        }
        let snap = stats.take_snapshot(2);
        assert!(snap.av_sync_bad_pct > 10);
        assert!(snap.is_av_out_of_sync());
    }

    /// 符号方向：持续落后应体现在 mean > 0 且 max_lag 涨、max_lead 为 0。
    #[test]
    fn av_sync_reports_lag_direction() {
        let stats = ProfileStats::default();
        for _ in 0..10 {
            stats.record_av_sync(20_000); // 画面落后音频 20ms
        }
        let snap = stats.take_snapshot(2);
        assert!(snap.av_sync_mean_ms > 0, "均值应为正（落后）");
        assert!(snap.av_sync_max_lag_ms >= 20);
        assert_eq!(snap.av_sync_max_lead_ms, 0);
    }

    /// 单帧极端错位（>100ms）即使占比低也直接判失步——
    /// 那是肉眼可见的对嘴型错位，不能容忍。
    #[test]
    fn av_sync_flags_extreme_single_frame() {
        let stats = ProfileStats::default();
        stats.record_av_sync(150_000); // 150ms
        let snap = stats.take_snapshot(2);
        assert!(snap.is_av_out_of_sync());
    }

    #[test]
    fn snapshot_computes_rates_over_window() {
        let stats = ProfileStats::default();
        for _ in 0..60 {
            stats.record_decoded(5_000);
        }
        let snap = stats.take_snapshot(2);
        assert_eq!(snap.decoded_fps, 30, "60 帧 / 2 秒 = 30fps");
        assert_eq!(snap.avg_decode_us, 5_000);

        // 快照后计数器应清零。
        let snap2 = stats.take_snapshot(2);
        assert_eq!(snap2.decoded_fps, 0);
    }
}
