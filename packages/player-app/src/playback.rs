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

/// 送往渲染侧的一帧：图像 + 显示时刻（PTS，微秒）+ 文件总时长（微秒）。
/// `None` 表示流结束（EOF 或出错）。
///
/// 总时长随每帧带上，渲染侧无需自行 open 文件就能画进度条。
/// 它是常量（每帧都相同），但 `Duration` 是 `Copy`、队列又很浅，
/// 顺带捎带的成本可忽略。
pub type FrameMsg = Option<(Arc<RenderImage>, u64, u64)>;

pub type FrameSender = mpsc::Sender<FrameMsg>;
pub type FrameReceiver = mpsc::Receiver<FrameMsg>;

/// 建一条帧通道。
pub fn frame_channel() -> (FrameSender, FrameReceiver) {
    mpsc::channel(FRAME_QUEUE_CAP)
}

/// 播放器控制命令：UI → 解码线程。
///
/// 用 **unbounded** 通道：控制命令不能因背压丢失，也不能让 UI 线程
/// 阻塞等队列（否则拖拽 seek 时界面会卡）。命令量极小，无满队列之虞。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackCommand {
    /// 暂停。声卡冻结、画面停走、source 不再前进。
    Pause,
    /// 恢复。
    Resume,
    /// 跳转到指定时刻。解码线程 seek + 重建声卡流重锚时钟。
    Seek(Duration),
}

pub type CommandSender = mpsc::UnboundedSender<PlaybackCommand>;
pub type CommandReceiver = mpsc::UnboundedReceiver<PlaybackCommand>;

/// 建一条命令通道。
pub fn command_channel() -> (CommandSender, CommandReceiver) {
    mpsc::unbounded()
}

/// 音频主时钟的交接点。
///
/// 声卡只能在解码线程里打开（cpal 的 `Stream` 不是 `Send`），
/// 但渲染 task 需要读它的进度。二者的创建时机也对不上：视图先建起来，
/// 解码线程随后才知道有没有音轨。用一个可后填的槽把这个空档接上。
///
/// 渲染侧在时钟就位前按墙钟走——文件无音轨时它会**永远**是空的，
/// 那也是正确行为。
///
/// 与 `OnceLock` 不同，这里用 `Mutex<Option<_>>`：seek 时会**重建**声卡流
/// （硬件时钟不能倒带，只能重开让 `frames_played` 归零），所以时钟句柄
/// 必须能被替换。
///
/// `generation` 每次 `attach` 递增：渲染侧据此判断「时钟是否换了新柄」。
/// 只在换代时才重建 [`PlaybackClock`]，避免每帧重建把墙钟 `origin` 反复清零
/// ——否则启动时（音频尚未出声，走墙钟）每帧都重置原点，视频会不受节流地
/// 提前刷出，等音频一出声又猛然等 400ms 追赶（实测的启动 427ms 卡顿与
/// 400ms 领先正是这么来的）。
#[derive(Default)]
pub struct AudioClockSource {
    clock: std::sync::Mutex<Option<AudioClock>>,
    generation: std::sync::atomic::AtomicU64,
    /// 最近一次 seek 的目标（微秒）。seek 重建声卡流后音频时钟从 0 重新起算，
    /// 但视频 PTS 是绝对时间（如 5s），两者相差一个目标偏移。渲染侧取
    /// `audio.position() + seek_offset` 作为音频主时钟读数，才能让 seek 后的
    /// 首帧立即显示而不被误判"落后 5 秒"。
    seek_offset_us: std::sync::atomic::AtomicU64,
}

impl AudioClockSource {
    /// 解码线程确认有音轨后（或 seek 重建流后）把时钟交出来。
    pub fn attach(&self, clock: AudioClock) {
        *self.clock.lock().unwrap_or_else(|e| e.into_inner()) = Some(clock);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录最近一次 seek 的目标时间（微秒）。seek 重建声卡流前调用。
    pub fn set_seek_offset(&self, us: u64) {
        self.seek_offset_us.store(us, Ordering::Relaxed);
    }

    /// 取当前音频时钟、代次与 seek 偏移；尚未就位（或无音轨）时时钟为 `None`。
    ///
    /// `generation` 用来自检换柄：渲染侧记住上次用的 `generation`，
    /// 变了才重建时钟，没变则沿用（墙钟 origin 得以保持）。
    pub fn get_with_generation(&self) -> (u64, u64, Option<AudioClock>) {
        let generation = self.generation.load(Ordering::Relaxed);
        let offset = self.seek_offset_us.load(Ordering::Relaxed);
        let clock = self.clock.lock().unwrap_or_else(|e| e.into_inner()).clone();
        (generation, offset, clock)
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
    mut cmd_rx: CommandReceiver,
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
        let mut audio = audio.filter(|_| source.audio_info().is_some());
        if let Some(a) = audio.as_ref() {
            clock_source.attach(a.clock());
            info!("音频主时钟已启用");
        } else {
            info!("无音轨，使用墙钟");
        }

        let duration_us = source.video_info().duration.as_micros() as u64;
        let mut paused = false;
        let mut frame_no: u64 = 0;

        // 每次 seek 都要重建声卡流来重锚时钟；这个 audio 由 `run_one_seek` 移动式持有。
        run_until_eof(
            &mut source,
            &mut tx,
            &running,
            &stats,
            &clock_source,
            &mut cmd_rx,
            &mut audio,
            &mut paused,
            &mut frame_no,
            duration_us,
        );
    });
}

/// 解码直到文件末尾，期间响应暂停/seek 命令。
///
/// `paused` 为真时进入暂停态：不再 `next_event`、不推音频、不发帧，
/// 仅轮询命令直到恢复或 seek。这样暂停期间 source 不前进、声卡冻结，
/// 恢复后位置天然连续。
#[allow(clippy::too_many_arguments)]
fn run_until_eof(
    source: &mut FfmpegSource,
    tx: &mut FrameSender,
    running: &Arc<AtomicBool>,
    stats: &Arc<ProfileStats>,
    clock_source: &Arc<AudioClockSource>,
    cmd_rx: &mut CommandReceiver,
    audio: &mut Option<AudioOutput>,
    paused: &mut bool,
    frame_no: &mut u64,
    duration_us: u64,
) {
    let mut next_frame: Option<(Arc<RenderImage>, u64)> = None;
    // 是否已放完（EOF）。之后线程不退出，继续轮询命令，好让"播完后点进度条"
    // 还能 seek 回中间重新播（见 Err/EOF 分支）。
    let mut finished = false;
    loop {
        if !running.load(Ordering::Relaxed) {
            return;
        }

        // 优先响应命令（尤其 seek/暂停），非阻塞。
        if let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                PlaybackCommand::Pause => {
                    if finished {
                        continue; // 已播完，暂停无意义
                    }
                    if let Some(a) = audio.as_ref() {
                        a.pause();
                    }
                    *paused = true;
                }
                PlaybackCommand::Resume => {
                    if finished {
                        continue;
                    }
                    *paused = false;
                    if let Some(a) = audio.as_ref() {
                        a.resume();
                    }
                }
                PlaybackCommand::Seek(ts) => {
                    if let Err(e) = source.seek(ts) {
                        error!(error = %e, seek_ms = ts.as_millis(), "seek 失败");
                    } else {
                        info!(seek_ms = ts.as_millis(), "seek");
                        // 播完后 seek：seek 会撤销 draining，重新可读，即可继续播放。
                        finished = false;
                        // 告诉渲染侧 seek 目标：重建声卡流后音频从 0 起算，
                        // 需加回这个偏移，否则首帧会被误判"落后 5 秒"而卡住。
                        clock_source.set_seek_offset(ts.as_micros() as u64);
                        seek_rebuild_audio(audio, clock_source);
                    }
                    next_frame = None; // 丢弃 seek 前暂存的帧
                }
            }
            continue;
        }

        if *paused {
            // 暂停态：不推音频、不发帧、不前进。睡一下等命令。
            std::thread::sleep(Duration::from_millis(8));
            continue;
        }

        if finished {
            // 已放完，只等 seek 命令。睡一下免得空转烧 CPU。
            std::thread::sleep(Duration::from_millis(8));
            continue;
        }

        // 有条暂存的待发帧？先发掉（投递会背压），再解下一帧。
        if let Some((render, pts_us)) = next_frame.take() {
            if !send_blocking(tx, (render, pts_us, duration_us), running) {
                return;
            }
            continue;
        }

        let t0 = Instant::now();
        match source.next_event() {
            Ok(Some(MediaEvent::Video(f))) => {
                *frame_no += 1;
                if frame_no.is_multiple_of(DECODE_LOG_EVERY) {
                    debug!(frame = *frame_no, pts_ms = f.pts.as_millis(), "解码进度");
                }
                let decode_us = t0.elapsed().as_micros() as u64;
                let render = decoded_to_render_image(&f);
                // 计数放在这里而非投递成功之后：投递会阻塞重试，
                // 挂在成功分支上会让"队列持续满"表现为 fps=0（曾误判成线程死亡）。
                stats.record_decoded(decode_us);
                let pts_us = f.pts.as_micros() as u64;
                // 交给下一轮发，避免在 seek 后发送 seek 前的帧。
                next_frame = Some((render, pts_us));
            }
            Ok(Some(MediaEvent::Audio(chunk))) => {
                if let Some(a) = audio.as_ref() {
                    // 背压：缓冲够深就等一等，别把整轨解进内存。
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
            }
            Ok(None) => {
                info!(frames = *frame_no, "解码到达文件末尾");
                // 别急着退：声卡缓冲里还有几百毫秒没播完，
                // 此刻 drop AudioOutput 会把声音戛然掐掉。
                if let Some(a) = audio.as_ref() {
                    drain_audio(a, running);
                }
                // 通知渲染侧"放完了"，但**不退出线程**：继续轮询命令，
                // 让播完后还能点进度条 seek 回去重播。seek 会清 draining，
                // 并在这里把 finished 置回 false。
                let _ = tx.try_send(None);
                finished = true;
            }
            Err(e) => {
                error!(error = %e, frames = *frame_no, "解码失败，停止");
                let _ = tx.try_send(None);
                return;
            }
        }
    }
}

/// seek 后重建声卡流，让音频时钟归零。
///
/// 声卡硬件时钟不能倒带：seek 到新位置后，旧 `frames_played` 还在原处，
/// 画面相对旧音频位置会被判定"大幅落后"→ 丢帧风暴。重建流（`AudioOutput::new`）
/// 让计数器归零，再把新时钟句柄交回渲染侧。会有一瞬静音/爆音（可接受）。
fn seek_rebuild_audio(audio: &mut Option<AudioOutput>, clock_source: &Arc<AudioClockSource>) {
    *audio = match AudioOutput::new() {
        Ok(o) => Some(o),
        Err(e) => {
            warn!(error = %e, "seek 后重开音频设备失败，将以无声模式播放");
            None
        }
    };
    if let Some(a) = audio.as_ref() {
        clock_source.attach(a.clock());
    }
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
    item: (Arc<RenderImage>, u64, u64),
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
    /// seek 后音频时钟的偏移：重建声卡流后音频从 0 起算，但视频 PTS 是
    /// 绝对时间，需加回 seek 目标才能对齐（否则首帧被判"落后"而卡住）。
    /// 有效读数为 `audio.position() + audio_offset`。
    audio_offset: Duration,
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
            audio_offset: Duration::ZERO,
            origin: None,
        }
    }

    /// 更换音频主时钟句柄，但**保留墙钟 origin**。
    ///
    /// seek 重建声卡流后会换上全新的时钟；此时若直接重建整个
    /// `PlaybackClock`，墙钟模式的 `origin` 会被清掉。音频出声后 origin 无意义，
    /// 但在"音频已 attach 尚未 start"的启动窗口里，我们仍走墙钟，origin 必须
    /// 只在首帧设置一次——每帧重建正是导致启动画面提前刷出的根因。
    pub fn set_audio(&mut self, audio: AudioClock) {
        self.audio = Some(audio);
    }

    /// 设置音频时钟偏移（seek 目标）。见 [`Self::audio_offset`]。
    pub fn set_audio_offset(&mut self, offset: Duration) {
        self.audio_offset = offset;
    }

    /// 为 PTS 为 `target` 的帧决定何时显示。
    pub fn schedule(&mut self, target: Duration) -> Schedule {
        // 音频时钟在第一个采样播出前恒为 0。这段时间内用它做基准，
        // 会把每一帧都判成"未来"，画面卡在首帧等一个还没开始走的钟。
        // 所以要等它真的动起来。
        if let Some(audio) = self.audio.as_ref()
            && audio.started()
        {
            // 音频主时钟读数 = 硬件进度 + seek 偏移。不加偏移的话，
            // seek 后音频从 0 起算、视频 PTS 却还是 5s，首帧会被判
            // "落后 5 秒"→ Wait(5s)，画面卡住直到音频追上 seek 点。
            let now = audio.position() + self.audio_offset;
            return Self::schedule_against(target, now, DROP_THRESHOLD, |behind| {
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
        let mut clock = audio_clock(fake_clock(0));
        // 音频已出声（started=true）。目标帧在 500ms 之后 → 应等待。
        match clock.schedule(Duration::from_millis(500)) {
            Schedule::Wait(d) => assert!(d > Duration::from_millis(400)),
            _ => panic!("未来的帧应当等待"),
        }
        // 时钟走到帧的位置 → 立即显示。
        assert!(matches!(
            audio_clock(fake_clock(500)).schedule(Duration::from_millis(500)),
            Schedule::Now
        ));
    }

    #[test]
    fn audio_clock_drops_frame_when_far_behind() {
        // 音频已播到 5 秒，目标帧才 100ms：落后远超 100ms 阈值，
        // 且音频没法重置 → 必须丢帧。
        let mut clock = audio_clock(fake_clock(5000));
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
        let mut clock = audio_clock(fake_clock(150));
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
        let mut clock = audio_clock(fake_clock_unstarted(0));
        assert!(matches!(clock.schedule(Duration::ZERO), Schedule::Now));
    }

    /// 构造一个已挂音频时钟的 `PlaybackClock`（测试用）。
    fn audio_clock(audio: AudioClock) -> PlaybackClock {
        let mut clock = PlaybackClock::new();
        clock.set_audio(audio);
        clock
    }

    /// 启动时音频已 attach 但尚未 start，走墙钟；`set_audio` 只换时钟柄、
    /// **不清墙钟 origin**。若每帧重建（`with_audio`），origin 反复清零，
    /// 画面会不受节流提前刷出——这是实测启动 427ms 卡顿的根因。
    #[test]
    fn set_audio_preserves_wallclock_origin() {
        let mut clock = PlaybackClock::new();
        // 首帧（音频未出声，走墙钟）定 origin 并立即显示。
        assert!(matches!(clock.schedule(Duration::ZERO), Schedule::Now));

        // 换上一个"未出声"的音频时钟，模拟启动窗口：应仍走墙钟、保持 origin。
        clock.set_audio(fake_clock_unstarted(0));
        // 假设 0.5s 后来了 pts=500ms 的帧；若 origin 被清，会误判为"未来/立即"，
        // 若 origin 保留，则按墙钟节奏应等待 ~0.5s。
        match clock.schedule(Duration::from_millis(500)) {
            Schedule::Wait(d) => {
                assert!(d > Duration::from_millis(400), "origin 应保留，等待约 0.5s");
            }
            other => panic!("origin 被清掉了，首帧之后本应等待，得到 {other:?}"),
        }
    }

    /// 构造一个"已出声、读数可控"的假音频时钟。
    fn fake_clock(ms: u64) -> AudioClock {
        AudioClock::for_test(Duration::from_millis(ms), true)
    }

    /// 构造一个"未出声、读数可控"的假音频时钟。
    fn fake_clock_unstarted(ms: u64) -> AudioClock {
        AudioClock::for_test(Duration::from_millis(ms), false)
    }

    // ---- AudioClockSource：可换柄 ----

    /// seek 会重建声卡流并重新 `attach` 时钟；旧柄必须能被替换，
    /// 渲染侧重读拿到**新**时钟（这才是 seek 后时间轴对齐的关键）。
    #[test]
    fn clock_source_returns_latest_attached_clock() {
        let src = AudioClockSource::default();
        // 尚未 attach → None。
        let (gen0, offset0, c0) = src.get_with_generation();
        assert!(c0.is_none());
        assert_eq!(gen0, 0);
        assert_eq!(offset0, 0);

        // 第一次 attach：位置 1s，代次 +1。
        src.attach(fake_clock(1000));
        let (gen1, _, c1) = src.get_with_generation();
        let c1 = c1.expect("attach 后应可取到");
        assert_eq!(c1.position(), Duration::from_millis(1000));
        assert_eq!(gen1, 1);

        // 模拟 seek：先记偏移，再重建换时钟，代次再 +1。
        src.set_seek_offset(5_000_000);
        src.attach(fake_clock(0));
        let (gen2, offset2, c2) = src.get_with_generation();
        let c2 = c2.expect("换柄后应可取到新时钟");
        assert_eq!(c2.position(), Duration::ZERO, "应拿到重建后的新时钟");
        assert_eq!(gen2, 2, "每次 attach 代次都要递增，渲染侧据此换柄");
        assert_eq!(offset2, 5_000_000, "seek 偏移应随 seek 更新");
    }

    /// seek 重建声卡流后音频从 0 起算、视频 PTS 仍是绝对时间（如 5s），
    /// 若不加偏移，首帧会被判"落后 5s"→ Wait(5s)，画面卡住。
    /// 加了 `audio_offset` 后，首帧（pts≈5s）应对齐立即显示。
    #[test]
    fn audio_offset_aligns_post_seek_frame() {
        let mut clock = audio_clock(fake_clock(0)); // 音频从 0 起算（重建后）
        // seek 到 5s，设偏移 5s。
        clock.set_audio_offset(Duration::from_secs(5));
        // 首帧 pts=5s，音频位置=0：加了偏移后 now=5s，target=5s → Now。
        assert!(matches!(
            clock.schedule(Duration::from_secs(5)),
            Schedule::Now
        ));
        // 若不加偏移（now=0 < target=5s），会被误判 Wait(5s)——
        // 这里用不设偏移的时钟验证基线行为，确保上面确实靠偏移对齐。
        let mut no_offset = audio_clock(fake_clock(0));
        assert!(matches!(
            no_offset.schedule(Duration::from_secs(5)),
            Schedule::Wait(_)
        ));
    }

    // ---- 端到端：播放到末尾后再 seek 能重新播放 ----

    /// 回归测试：放完后（EOF）点进度条 seek 回去要能重新出帧。
    /// 之前 EOF 时解码线程直接返回、source 被 drop，seek 命令石沉大海，
    /// 画面停在最后一帧不动。现在 EOF 后线程继续轮询命令，seek 清 draining
    /// 重新可读。这个测试需要真实音频设备（本机有），用真实素材驱动整条线程。
    #[test]
    #[ignore = "需要真实音频设备"]
    fn seek_after_eof_resumes_playback() {
        let (tx, mut rx) = frame_channel();
        let (cmd, cmd_rx) = command_channel();
        let running = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(ProfileStats::default());
        let clock_source = Arc::new(AudioClockSource::default());

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../player-core/tests/assets/sample.mp4");
        spawn_decode_thread(
            path,
            tx.clone(),
            running.clone(),
            stats.clone(),
            clock_source.clone(),
            cmd_rx,
        );

        // 等 EOF（None 信号）。解码线程把 10s 音频按真实速度播完才到 EOF，
        // 加上尾部 drain，给足 15s 余量。
        let mut saw_eof = false;
        let mut waited = Duration::ZERO;
        while waited < Duration::from_secs(15) {
            match rx.try_recv() {
                Ok(Some(_)) => continue, // 正常帧
                Ok(None) => {
                    saw_eof = true; // EOF 信号
                    break;
                }
                _ => {
                    std::thread::sleep(Duration::from_millis(20));
                    waited += Duration::from_millis(20);
                }
            }
        }
        assert!(saw_eof, "应在 10s 内读到 EOF");

        // 播完后再 seek 回 2s。
        cmd.unbounded_send(PlaybackCommand::Seek(Duration::from_secs(2))).unwrap();

        // 应重新出帧，且首帧 pts 接近 2s。
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut got_target = false;
        while std::time::Instant::now() < deadline {
            if let Ok(Some((_, pts_us, _))) = rx.try_recv() {
                let pts = Duration::from_micros(pts_us);
                if (pts.as_secs_f64() - 2.0).abs() < 1.0 {
                    got_target = true;
                    break;
                }
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(got_target, "seek 后应在 3s 内收到 pts≈2s 的帧");

        running.store(false, Ordering::Relaxed);
        drop(tx); // 释放 sender，让线程干净退出
    }
}
