//! 解码线程时间去向探针（诊断工具，debug 日志可长期保留）。
//! 在解码线程内就地汇总，每 2s 由任一事件触发上报一次：
//! 各阶段累计耗时占比 + 音频入队深度分布，用于定位「音频欠载」时
//! 时间到底花在 next_event（解码慢）还是 send_blocking（渲染背压）。
use super::Duration;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

struct Acc {
    next_us: AtomicU64,
    send_us: AtomicU64,
    audio_pushed_ms10: AtomicU64, // 累计推送音频时长×10（毫秒定点数）
    queued_max_ms: AtomicU64,
    played_ms: AtomicU64,         // 声卡累计消费（绝对值，报告算增量）
    dropped: AtomicU64,
    pump_chunks: AtomicU64,       // 续杯泵抽到的音频块数
    arm_chunks: AtomicU64,        // next_event 正常路径交付的音频块数
    events: AtomicU64,
    t_last: AtomicU64, // 上次上报时刻（相对进程锚点的毫秒数）
    last_played: AtomicU64,
}
static ACC: Acc = Acc {
    next_us: AtomicU64::new(0),
    send_us: AtomicU64::new(0),
    audio_pushed_ms10: AtomicU64::new(0),
    queued_max_ms: AtomicU64::new(0),
    played_ms: AtomicU64::new(0),
    dropped: AtomicU64::new(0),
    pump_chunks: AtomicU64::new(0),
    arm_chunks: AtomicU64::new(0),
    events: AtomicU64::new(0),
    t_last: AtomicU64::new(0),
    last_played: AtomicU64::new(0),
};

const REPORT_INTERVAL_MS: u64 = 2000;

fn anchor() -> Instant {
    static A: OnceLock<Instant> = OnceLock::new();
    *A.get_or_init(Instant::now)
}

fn maybe_report() {
    let now_ms = anchor().elapsed().as_millis() as u64;
    let last = ACC.t_last.load(Ordering::Relaxed);
    if now_ms.wrapping_sub(last) < REPORT_INTERVAL_MS {
        return;
    }
    if ACC
        .t_last
        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return; // 别的调用已触发上报
    }
    let next = ACC.next_us.swap(0, Ordering::Relaxed);
    let send = ACC.send_us.swap(0, Ordering::Relaxed);
    let pushed = ACC.audio_pushed_ms10.swap(0, Ordering::Relaxed);
    let qmax = ACC.queued_max_ms.swap(0, Ordering::Relaxed);
    let played = ACC.played_ms.load(Ordering::Relaxed);
    let dropped = ACC.dropped.swap(0, Ordering::Relaxed);
    let pump_chunks = ACC.pump_chunks.swap(0, Ordering::Relaxed);
    let arm_chunks = ACC.arm_chunks.swap(0, Ordering::Relaxed);
    let n = ACC.events.load(Ordering::Relaxed);
    // 消费增量 = 本次读数 - 上次读数（绝对计数）。
    let last_played = ACC.last_played.swap(played, Ordering::Relaxed);
    let d_played = played.saturating_sub(last_played);
    tracing::info!(
        next_event_ms = next / 1000,
        send_blocked_ms = send / 1000,
        audio_pushed_ms = pushed / 10,
        card_consumed_ms = d_played,
        audio_queued_peak_ms = qmax,
        pump_chunks,
        arm_chunks,
        dropped,
        events = n,
        "解码线程 2s 时间去向"
    );
}

pub fn next_event_done(d: Duration) {
    ACC.next_us.fetch_add(d.as_micros() as u64, Ordering::Relaxed);
    ACC.events.fetch_add(1, Ordering::Relaxed);
    maybe_report();
}

pub fn send_blocked(d: Duration) {
    ACC.send_us.fetch_add(d.as_micros() as u64, Ordering::Relaxed);
    maybe_report();
}

/// 每次音频 push 调用：记录本 chunk 时长与当前队列深度峰值。
pub fn audio_pushed(
    sample_count: usize,
    channels: u16,
    sample_rate: u32,
    queued: std::time::Duration,
) {
    let ch = channels.max(1) as f64;
    let ms10 = (sample_count as f64 / ch / sample_rate as f64 * 1000.0 * 10.0) as u64;
    ACC.audio_pushed_ms10.fetch_add(ms10, Ordering::Relaxed);
    ACC.queued_max_ms
        .fetch_max(queued.as_millis() as u64, Ordering::Relaxed);
    maybe_report();
}

/// 声卡侧消费进度（position 增量在报告里算差值）。
pub fn audio_chunk(played: Duration, _queued: Duration) {
    ACC.played_ms
        .store(played.as_millis() as u64, Ordering::Relaxed);
}

/// 记录音频块被丢弃的路径（seek 过滤 / scrub 静音）。
/// 丢弃原因进 debug 日志：丢弃量异常暴涨（如整轨被丢）时可直接定位闸门。
pub fn audio_dropped(why: &'static str) {
    ACC.dropped.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(why, "音频块被门闸丢弃");
}

pub fn pump_chunk() {
    ACC.pump_chunks.fetch_add(1, Ordering::Relaxed);
}

pub fn arm_chunk() {
    ACC.arm_chunks.fetch_add(1, Ordering::Relaxed);
}
