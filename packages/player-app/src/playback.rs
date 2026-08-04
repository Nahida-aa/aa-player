//! 播放管线：解码线程 + 按 PTS 调度的显示节拍。
//!
//! 采用双时钟模型（参考 OBS：解码节拍 ≠ 渲染节拍）：
//!   - 独立 OS 线程做同步解码，经**有界** channel 投递，形成背压；
//!   - GPUI 后台 async task 收帧，按 PTS 用 timer 精确调度，绝不阻塞 executor。
//!
//! 这样重的解算在专用线程，渲染循环（vsync）不被拖慢。
//!
//! 踩过的坑见 `docs/debugging-playback-jank.md`，其中两个直接塑造了本模块：
//! 队列满时**不能丢帧**（要重试到送出），落后时**必须重置时间轴原点**。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use gpui::RenderImage;
use player_core::{FfmpegSource, MediaSource};
use tracing::{debug, error, info, warn};

use crate::render_image::decoded_to_render_image;
use crate::stats::ProfileStats;

/// 帧队列容量。刻意很浅：渲染侧按 PTS 主动等待，队列**本来就该几乎总是满的**，
/// 这正是我们要的背压（解码不跑在渲染前面太多）。队列深了只会增加延迟。
const FRAME_QUEUE_CAP: usize = 3;

/// 落后多久就重置时间轴原点。超过此阈值说明不是抖动而是真掉队，
/// 继续按原原点追赶只会让画面一次性冲刷完再干等。
const RESYNC_THRESHOLD: Duration = Duration::from_millis(200);

/// 投递队列满时的退避间隔。
const SEND_BACKOFF: Duration = Duration::from_millis(2);

/// 每隔多少帧打一条解码进度日志。逐帧日志在 30fps 下会因终端 IO
/// 反过来拖慢 worker，污染我们要测的东西。
const DECODE_LOG_EVERY: u64 = 60;

/// 送往渲染侧的一帧：图像 + 显示时刻（PTS，微秒）。
/// `None` 表示流结束（EOF 或出错）。
pub type FrameMsg = Option<(Arc<RenderImage>, u64)>;

pub type FrameSender = mpsc::Sender<FrameMsg>;
pub type FrameReceiver = mpsc::Receiver<FrameMsg>;

/// 建一条帧通道。
pub fn frame_channel() -> (FrameSender, FrameReceiver) {
    mpsc::channel(FRAME_QUEUE_CAP)
}

/// 在独立 OS 线程里解码 `path`，把帧投递到 `tx`。
///
/// `running` 置 false 时线程退出（窗口关闭）。
///
/// 之所以传 `PathBuf` 而不是已打开的 source：`FfmpegSource` 内部的 ffmpeg
/// 类型不实现 `Send`，不能跨线程移动，因此必须**在线程内部** open。
pub fn spawn_decode_thread(
    path: PathBuf,
    mut tx: FrameSender,
    running: Arc<AtomicBool>,
    stats: Arc<ProfileStats>,
) {
    std::thread::spawn(move || {
        let mut source = match FfmpegSource::open(&path) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, path = %path.display(), "打开视频失败");
                let _ = tx.try_send(None);
                return;
            }
        };

        let mut frame_no: u64 = 0;
        while running.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            let frame = match source.next_frame() {
                Ok(Some(f)) => {
                    frame_no += 1;
                    if frame_no % DECODE_LOG_EVERY == 0 {
                        debug!(frame = frame_no, pts_ms = f.pts.as_millis(), "解码进度");
                    }
                    f
                }
                Ok(None) => {
                    info!(frames = frame_no, "解码到达文件末尾");
                    let _ = tx.try_send(None);
                    return;
                }
                Err(e) => {
                    error!(error = %e, frames = frame_no, "解码失败，停止");
                    let _ = tx.try_send(None);
                    return;
                }
            };

            let decode_us = t0.elapsed().as_micros() as u64;
            let render = decoded_to_render_image(&frame);
            // 计数放在这里而非投递成功之后：投递会阻塞重试，
            // 挂在成功分支上会让"队列持续满"表现为 fps=0（曾误判成线程死亡）。
            stats.record_decoded(decode_us);

            let pts_us = frame.pts.as_micros() as u64;
            if !send_blocking(&mut tx, (render, pts_us), &running) {
                return; // 接收端关闭或被要求停止
            }
        }
        debug!(frames = frame_no, "解码线程正常退出（窗口关闭）");
    });
}

/// 把一帧送进队列，满则退避重试直到成功。
///
/// 返回 `false` 表示应当结束线程（接收端已关闭，或 `running` 被置 false）。
///
/// **不能丢帧**：丢帧会让渲染侧收到的 PTS 出现空洞，时间轴对不上，
/// 表现为忽快忽卡。早期版本"重试一次仍满就丢弃"正是卡顿的根源。
fn send_blocking(
    tx: &mut FrameSender,
    item: (Arc<RenderImage>, u64),
    running: &AtomicBool,
) -> bool {
    let mut pending = Some(item);
    while running.load(Ordering::Relaxed) {
        match tx.try_send(pending) {
            Ok(()) => return true,
            Err(e) if e.is_full() => {
                pending = e.into_inner();
                std::thread::sleep(SEND_BACKOFF);
            }
            Err(_) => return false, // 接收端已关闭
        }
    }
    false
}

/// PTS 时间轴：把帧的 PTS 映射到墙钟时刻。
///
/// 不变式：`该帧应显示的墙钟时刻 == origin + pts`。
pub struct PlaybackClock {
    /// 时间轴原点。首帧到达时校准（首帧 PTS 未必为 0，故要减去它）。
    origin: Option<Instant>,
}

/// 时钟对某一帧给出的调度决定。
pub enum Schedule {
    /// 还没到点，等待这么久再显示。
    Wait(Duration),
    /// 已到点，立即显示。
    Now,
    /// 落后太多，已重置原点；立即显示并附带落后量（用于告警）。
    Resynced { behind: Duration },
}

impl PlaybackClock {
    pub fn new() -> Self {
        Self { origin: None }
    }

    /// 为 PTS 为 `target` 的帧决定何时显示。
    pub fn schedule(&mut self, target: Duration) -> Schedule {
        let origin = *self.origin.get_or_insert_with(|| Instant::now() - target);
        let elapsed = origin.elapsed();

        if target > elapsed {
            Schedule::Wait(target - elapsed)
        } else {
            let behind = elapsed - target;
            if behind > RESYNC_THRESHOLD {
                // 重置原点，以当前帧为新起点继续。否则原点永远偏早，
                // 之后每帧都判定"迟到"从而不再等待，画面会一次性冲刷完
                // 再干等 —— 正是忽快忽卡的成因。
                self.origin = Some(Instant::now() - target);
                Schedule::Resynced { behind }
            } else {
                Schedule::Now
            }
        }
    }
}

impl Default for PlaybackClock {
    fn default() -> Self {
        Self::new()
    }
}

/// 记录一次重同步。稳态播放**不该**出现它；若持续刷屏，
/// 说明解码或渲染真的跟不上，要查根因而不是靠重置掩盖。
pub fn log_resync(behind: Duration, pts: Duration) {
    warn!(
        behind_ms = behind.as_millis(),
        pts_ms = pts.as_millis(),
        "播放落后，重置时间轴原点"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_waits_for_future_frame() {
        let mut clock = PlaybackClock::new();
        // 首帧校准原点后立即显示。
        assert!(matches!(clock.schedule(Duration::ZERO), Schedule::Now));

        // 下一帧在 1 秒后，应要求等待接近 1 秒。
        match clock.schedule(Duration::from_secs(1)) {
            Schedule::Wait(d) => {
                assert!(
                    d > Duration::from_millis(900) && d <= Duration::from_secs(1),
                    "期望等待约 1s，实际 {d:?}"
                );
            }
            _ => panic!("未来的帧应当等待"),
        }
    }

    #[test]
    fn clock_calibrates_origin_from_nonzero_first_pts() {
        let mut clock = PlaybackClock::new();
        // 首帧 PTS 不为 0 时，原点要减去它，否则会误判为落后 5 秒。
        let first = clock.schedule(Duration::from_secs(5));
        assert!(
            matches!(first, Schedule::Now),
            "首帧无论 PTS 多少都应立即显示，不该触发重同步"
        );
    }

    #[test]
    fn clock_resyncs_when_far_behind() {
        let mut clock = PlaybackClock::new();
        clock.schedule(Duration::ZERO);
        // 伪造"原点在很久以前"：把原点手动往前挪，模拟播放严重落后。
        clock.origin = Some(Instant::now() - Duration::from_secs(10));

        match clock.schedule(Duration::from_millis(100)) {
            Schedule::Resynced { behind } => {
                assert!(behind > RESYNC_THRESHOLD);
            }
            _ => panic!("落后超阈值应触发重同步"),
        }

        // 重同步后，同一帧不应再被判为落后。
        assert!(matches!(
            clock.schedule(Duration::from_millis(100)),
            Schedule::Now
        ));
    }

    #[test]
    fn clock_tolerates_small_lag_without_resync() {
        let mut clock = PlaybackClock::new();
        clock.schedule(Duration::ZERO);
        // 落后 50ms（< 200ms 阈值）：属正常抖动，直接显示而不重置原点。
        clock.origin = Some(Instant::now() - Duration::from_millis(50));
        assert!(matches!(clock.schedule(Duration::ZERO), Schedule::Now));
    }
}
