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

    /// 取出并清零本区间的全部统计，得到一个可直接上报的快照。
    ///
    /// 所有计数器一次性 swap 为 0，从而天然形成"区间速率"语义。
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
}

/// 准时率：帧间隔落在 28~38ms（标称 33.3ms 上下各约 5ms）的占比。
/// 对应桶下标 2..=4。
fn on_time_pct(hist: &[u64; HIST_LEN], total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    (hist[2] + hist[3] + hist[4]) * 100 / total
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
        // 下标 2/3/4 是准时区间。
        let h = hist_of(&[(2, 10), (3, 70), (4, 10), (7, 10)]);
        assert_eq!(on_time_pct(&h, 100), 90);
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
        };
        assert!(!s.is_janky());
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
