//! 播放管线：解码线程 + 按 PTS 调度的显示节拍。
//!
//! 采用双时钟模型（参考 OBS：解码节拍 ≠ 渲染节拍）：
//!   - 独立 OS 线程做同步解码，经**有界** channel 投递，形成背压；
//!   - GPUI 后台 async task 收帧，按 PTS 用 timer 精确调度，绝不阻塞 executor。
//!
//! 这样重的解算在专用线程，渲染循环（vsync）不被拖慢。
//!
//! 音频**不走这条 channel**：解码线程直接把采样推给声卡。
//! 渲染 task 会按 PTS 睡到下一帧的时刻，音频若也从那里推就会周期性断供。
//! 声音断裂比画面卡顿刺耳得多，所以音频要走最短的路径。
//!
//! 有声音时，[`PlaybackClock`] 以**声卡的播放进度**为准（音频主时钟）：
//! 声卡以固定采样率消费数据，比 `Instant::now()` 稳；且人耳对声音断裂
//! 远比眼睛对丢帧敏感，该迁就的是画面。无音轨时退回墙钟。
//!
//! 踩过的坑见 `docs/debugging-playback-jank.md`，其中两个直接塑造了本模块：
//! 队列满时**不能丢帧**（要重试到送出），落后时**必须重置时间轴原点**。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use gpui::RenderImage;
use player_core::{AudioClock, AudioOutput, FfmpegSource, MediaEvent, MediaSource};
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

/// 音频主时钟的交接点。
///
/// 声卡只能在解码线程里打开（cpal 的 `Stream` 不是 `Send`），
/// 但渲染 task 需要读它的进度。二者的创建时机也对不上：视图先建起来，
/// 解码线程随后才知道有没有音轨。用一个可后填的槽把这个空档接上。
///
/// 渲染侧在时钟就位前按墙钟走——文件无音轨时它会**永远**是空的，
/// 那也是正确行为。
#[derive(Default)]
pub struct AudioClockSource {
    clock: std::sync::OnceLock<AudioClock>,
}

impl AudioClockSource {
    /// 解码线程确认有音轨后调用，把时钟交出来。
    pub fn attach(&self, output: &AudioOutput) {
        let _ = self.clock.set(output.clock());
    }

    /// 取当前的音频时钟；尚未就位（或无音轨）时为 `None`。
    pub fn get(&self) -> Option<&AudioClock> {
        self.clock.get()
    }
}

/// 声卡队列里最多缓冲多少音频。超过就先别解，形成背压。
///
/// 比视频队列（3 帧 ≈ 100ms）深不少：欠载一次就是一声爆音，
/// 而多缓冲一点音频的代价只是 seek 响应慢那么一档。
const AUDIO_BUFFER: Duration = Duration::from_millis(400);

/// 音频缓冲满时的退避间隔。
const AUDIO_BACKOFF: Duration = Duration::from_millis(5);

/// 在独立 OS 线程里解码 `path`，把帧投递到 `tx`，把音频直接推给声卡。
///
/// `running` 置 false 时线程退出（窗口关闭）。
///
/// 之所以传 `PathBuf` 而不是已打开的 source：`FfmpegSource` 内部的 ffmpeg
/// 类型不实现 `Send`，不能跨线程移动，因此必须**在线程内部** open。
/// `AudioOutput` 同理（cpal 的 Stream 不是 Send），所以它也在这里创建，
/// 再把只读的时钟句柄交回给渲染侧。
pub fn spawn_decode_thread(
    path: PathBuf,
    mut tx: FrameSender,
    running: Arc<AtomicBool>,
    stats: Arc<ProfileStats>,
    clock_source: Arc<AudioClockSource>,
) {
    std::thread::spawn(move || {
        // 声卡打不开不该让整个播放失败——没有声音总比放不了强。
        let audio = match AudioOutput::new() {
            Ok(o) => Some(o),
            Err(e) => {
                warn!(error = %e, "打开音频设备失败，将以无声模式播放");
                None
            }
        };
        let audio_format = audio.as_ref().map(|a| a.format());

        let mut source = match FfmpegSource::open_with(&path, audio_format) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, path = %path.display(), "打开视频失败");
                let _ = tx.try_send(None);
                return;
            }
        };

        // 只有确实有音轨时才让时钟切到音频主时钟。
        // 设备开了但文件是纯视频的话，声卡永远不会推进，
        // 拿它当主时钟会让画面彻底不动。
        let audio = audio.filter(|_| source.audio_info().is_some());
        if let Some(a) = audio.as_ref() {
            clock_source.attach(a);
            info!("音频主时钟已启用");
        } else {
            info!("无音轨，使用墙钟");
        }

        let mut frame_no: u64 = 0;
        while running.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            let frame = match source.next_event() {
                Ok(Some(MediaEvent::Video(f))) => {
                    frame_no += 1;
                    if frame_no.is_multiple_of(DECODE_LOG_EVERY) {
                        debug!(frame = frame_no, pts_ms = f.pts.as_millis(), "解码进度");
                    }
                    f
                }
                Ok(Some(MediaEvent::Audio(chunk))) => {
                    if let Some(a) = audio.as_ref() {
                        // 背压：缓冲够深就等一等，别把整轨解进内存。
                        // 这里可以安心 sleep——音频缓冲里还有几百毫秒垫着。
                        while running.load(Ordering::Relaxed)
                            && a.queued_duration() > AUDIO_BUFFER
                        {
                            std::thread::sleep(AUDIO_BACKOFF);
                        }
                        a.push_samples(&chunk.samples);
                        if a.take_underrun() {
                            warn!("音频欠载：解码跟不上声卡消费");
                        }
                    }
                    continue;
                }
                Ok(None) => {
                    info!(frames = frame_no, "解码到达文件末尾");
                    // 别急着退：声卡缓冲里还有几百毫秒没播完，
                    // 此刻返回会把 AudioOutput drop 掉，声音戛然而止。
                    if let Some(a) = audio.as_ref() {
                        drain_audio(a, &running);
                    }
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

/// 等声卡把缓冲里剩下的采样播完。
///
/// 解码线程一返回，`AudioOutput` 就被 drop、流随之停止。
/// 不等的话结尾几百毫秒会被直接掐掉。
fn drain_audio(audio: &AudioOutput, running: &AtomicBool) {
    while running.load(Ordering::Relaxed) && audio.queued_frames() > 0 {
        std::thread::sleep(AUDIO_BACKOFF);
    }
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

/// 落后多久就直接丢帧。音频主时钟下，落后是没法靠"重置"抹平的
/// ——声音已经放出去了，只能让画面追上去。
const DROP_THRESHOLD: Duration = Duration::from_millis(100);

/// PTS 时间轴：把帧的 PTS 映射到「现在该不该显示」。
///
/// 两种模式：
///   - **音频主时钟**（有音轨）：以声卡的播放进度为准。声卡按固定采样率
///     消费数据，比 `Instant` 稳；且人耳对声音断裂远比眼睛对丢帧敏感，
///     所以该迁就的是画面。落后太多就丢帧，不能反过来把音频拽慢。
///   - **墙钟**（无音轨）：退回 `origin + pts` 的老模型，落后太多则重置原点。
///
/// 之所以不是两个类型：调用方（渲染 task）不该关心当前是哪种模式，
/// 而且有无音轨要等解码线程打开文件后才知道，编译期分不开。
pub struct PlaybackClock {
    /// 音频主时钟；`None` 或尚未出声时退回墙钟。
    audio: Option<AudioClock>,
    /// 墙钟模式的时间轴原点。首帧到达时校准（首帧 PTS 未必为 0，故要减去它）。
    origin: Option<Instant>,
}

/// 时钟对某一帧给出的调度决定。
#[derive(Debug, PartialEq, Eq)]
pub enum Schedule {
    /// 还没到点，等待这么久再显示。
    Wait(Duration),
    /// 已到点，立即显示。
    Now,
    /// 落后太多，跳过这一帧（音频主时钟模式）。
    Drop { behind: Duration },
    /// 落后太多，已重置原点；立即显示并附带落后量（墙钟模式）。
    Resynced { behind: Duration },
}

impl PlaybackClock {
    /// 纯墙钟时钟（无音轨时用）。
    pub fn new() -> Self {
        Self {
            audio: None,
            origin: None,
        }
    }

    /// 以音频为主时钟。
    pub fn with_audio(audio: AudioClock) -> Self {
        Self {
            audio: Some(audio),
            origin: None,
        }
    }

    /// 为 PTS 为 `target` 的帧决定何时显示。
    pub fn schedule(&mut self, target: Duration) -> Schedule {
        // 音频时钟在第一个采样播出前恒为 0。这段时间内用它做基准，
        // 会把每一帧都判成"未来"，画面卡在首帧等一个还没开始走的钟。
        // 所以要等它真的动起来。
        if let Some(audio) = self.audio.as_ref()
            && audio.started()
        {
            return Self::schedule_against(target, audio.position(), DROP_THRESHOLD, |behind| {
                Schedule::Drop { behind }
            });
        }

        let origin = *self.origin.get_or_insert_with(|| Instant::now() - target);
        let elapsed = origin.elapsed();
        Self::schedule_against(target, elapsed, RESYNC_THRESHOLD, |behind| {
            // 重置原点，以当前帧为新起点继续。否则原点永远偏早，
            // 之后每帧都判定"迟到"从而不再等待，画面会一次性冲刷完
            // 再干等 —— 正是忽快忽卡的成因。
            self.origin = Some(Instant::now() - target);
            Schedule::Resynced { behind }
        })
    }

    /// 把 `target` 和当前时钟读数 `now` 一比，给出决定。
    ///
    /// 两种模式只有「落后超阈值时怎么办」不同，其余完全一致，
    /// 所以差异用一个回调传进来。
    fn schedule_against(
        target: Duration,
        now: Duration,
        threshold: Duration,
        on_behind: impl FnOnce(Duration) -> Schedule,
    ) -> Schedule {
        if target > now {
            Schedule::Wait(target - now)
        } else {
            let behind = now - target;
            if behind > threshold {
                on_behind(behind)
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

    // ---- 音频主时钟路径 ----
    // 用假的 `AudioClock`：它的 `position()` 直接由我们控制，跳过声卡。

    #[test]
    fn audio_clock_waits_for_future_frame() {
        let mut clock = PlaybackClock::with_audio(fake_clock(0));
        // 音频已出声（started=true）。目标帧在 500ms 之后 → 应等待。
        match clock.schedule(Duration::from_millis(500)) {
            Schedule::Wait(d) => assert!(d > Duration::from_millis(400)),
            _ => panic!("未来的帧应当等待"),
        }
        // 时钟走到帧的位置 → 立即显示。
        assert!(matches!(
            PlaybackClock::with_audio(fake_clock(500)).schedule(Duration::from_millis(500)),
            Schedule::Now
        ));
    }

    #[test]
    fn audio_clock_drops_frame_when_far_behind() {
        // 音频已播到 5 秒，目标帧才 100ms：落后远超 100ms 阈值，
        // 且音频没法重置 → 必须丢帧。
        let mut clock = PlaybackClock::with_audio(fake_clock(5000));
        match clock.schedule(Duration::from_millis(100)) {
            Schedule::Drop { behind } => {
                assert!(behind > DROP_THRESHOLD, "落后 {behind:?} 应超过丢帧阈值");
            }
            other => panic!("音频主时钟下大幅落后应丢帧，得到 {other:?}"),
        }
    }

    #[test]
    fn audio_clock_shows_frame_within_drop_tolerance() {
        // 音频播到 150ms，目标帧在 100ms：落后 50ms（< 100ms 阈值），
        // 属抖动范围，直接显示而不丢帧。
        let mut clock = PlaybackClock::with_audio(fake_clock(150));
        assert!(matches!(
            clock.schedule(Duration::from_millis(100)),
            Schedule::Now
        ));
    }

    #[test]
    fn audio_clock_ignored_before_it_starts() {
        // 声卡还没播出第一个采样时 position()==0 但 started()==false，
        // 不能拿它当基准（会让每帧都判成"未来"卡在首帧）。
        // 此时应退回墙钟：首帧立即显示。
        let mut clock = PlaybackClock::with_audio(fake_clock_unstarted(0));
        assert!(matches!(clock.schedule(Duration::ZERO), Schedule::Now));
    }

    /// 构造一个"已出声、读数可控"的假音频时钟。
    fn fake_clock(ms: u64) -> AudioClock {
        AudioClock::for_test(Duration::from_millis(ms), true)
    }

    /// 构造一个"未出声、读数可控"的假音频时钟。
    fn fake_clock_unstarted(ms: u64) -> AudioClock {
        AudioClock::for_test(Duration::from_millis(ms), false)
    }
}
