//! 音频输出设备（cpal）。
//!
//! 职责只有两件事：
//!   1. 把 PCM 采样送到声卡；
//!   2. 报告**设备已消费了多少采样** —— 这是后续做音频主时钟的基础。
//!
//! 为什么需要第 2 点：音视频同步要用音频当主时钟（人耳对声音断裂远比
//! 眼睛对丢帧敏感），而声卡以固定采样率消费数据，它天然比 `Instant::now()`
//! 更稳。拿到「已消费采样数 / 采样率」就等于拿到了播放进度。
//!
//! 这也是选 cpal 而非 rodio 的原因：rodio 把这层信息藏起来了。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::error::Result;

/// 音频输出的设备参数。解码侧需要按这个重采样。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// 待播放采样的共享缓冲。
///
/// cpal 的回调运行在**实时音频线程**上：不能在里面分配、加锁过久或阻塞，
/// 否则会产生爆音（xrun）。这里用一个简单的 `Mutex<VecDeque>`——
/// 临界区只有 memcpy，实测足够；真要极致可换无锁环形缓冲。
type SampleQueue = Arc<Mutex<std::collections::VecDeque<f32>>>;

/// 一个已启动的音频输出流。
///
/// `Drop` 时自动停止播放。
pub struct AudioOutput {
    /// 持有以保活：drop 掉流就停止播放。
    _stream: cpal::Stream,
    format: AudioFormat,
    queue: SampleQueue,
    /// 设备累计消费的**帧数**（1 帧 = 每声道各 1 个采样）。
    frames_played: Arc<AtomicU64>,
    /// 缓冲空了却仍在被索取（欠载）。说明解码跟不上。
    underrun: Arc<AtomicBool>,
    /// 是否已经播出过第一个采样。见 [`AudioOutput::started`]。
    started: StartedFlag,
    /// 是否处于暂停态。暂停时声卡设备冻结，时钟也随之停走，
    /// 恢复后位置天然连续——这是 seek/暂停能对齐画面的基础。
    paused: Arc<AtomicBool>,
    /// 输出音量增益（0.0=静音，1.0=正常）。回调里乘到每个采样上，
    /// 实现持久静音（与拖动时的临时 `pause()` 静音正交）。
    /// 用 `AtomicU32` 存 `f32` bits（稳定 Rust 无 `AtomicF32`）。
    volume: Arc<AtomicU32>,
    /// 解码侧累计推入的**帧数**。与 [`Self::frames_played`](Self::position)
    /// 对照即可算出净产出速率，用于诊断欠载是产不足还是消费异常。
    pushed_frames: Arc<AtomicU64>,
    /// 断续宽限：seek/清队后到新数据到达之间，回调必然短暂断供——这是
    /// 预期行为而非解码跟不上。置位时下一次欠载被吞掉（对齐 vlc 的
    /// discontinuity 处理：设备不断重建，断供由核心层消化）。
    grace: Arc<AtomicBool>,
}

/// 播放是否真正开始过（收到过非空采样）。
/// 用于把「启动时队列还没填上」与「播着播着断供」区分开。
type StartedFlag = Arc<AtomicBool>;

/// 一次回调之后，时钟该前进多少帧、要不要报欠载。
///
/// 单独抽出来是为了能脱离声卡测：这段逻辑决定了主时钟准不准，
/// 而它的错误（时钟偷跑、欠载误报）在真实设备上极难复现和观察。
///
/// - `requested` / `filled`：本次回调索取的、以及队列真正供上的采样数。
/// - `started`：此前是否已经播出过采样。
///
/// 返回 `(时钟前进的采样数, 是否已开播, 是否欠载)`。
#[inline]
fn account(requested: usize, filled: usize, started: bool) -> (usize, bool, bool) {
    let started = started || filled > 0;
    // 开播前的空转不计时（否则 position 变成"流创建至今的墙钟"）；
    // 开播后按 requested 计（补的静音也占用了真实播放时间）。
    let advance = if started { requested } else { 0 };
    let underrun = started && filled < requested;
    (advance, started, underrun)
}

/// f32(-1.0..=1.0) → i16。超范围先钳位，否则回绕会变成刺耳爆音。
#[inline]
fn f32_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// f32(-1.0..=1.0) → u16。u16 是**无符号**格式，静音在中点而非 0，
/// 故需先映射到 0.0..=1.0 再放大——直接乘会把负半周全削掉。
#[inline]
fn f32_to_u16(v: f32) -> u16 {
    ((v.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32) as u16
}

impl AudioOutput {
    /// 打开默认输出设备并开始播放（初始为静音，因为队列是空的）。
    pub fn new() -> Result<Self> {
        Self::create(true)
    }

    /// 打开默认输出设备，但**不启动播放**（样本推入队列后堆积，静音）。
    ///
    /// seek 重建声卡流时用：先建好、喂样本，等首个视频帧就绪再 [`start`](Self::start)
    /// 同步开播，避免音频在视频准备好之前就提前跑出去（这正是向后 seek 卡顿的
    /// 根源之一——音频冲在前，画面迟迟追不上）。
    pub fn new_paused() -> Result<Self> {
        Self::create(false)
    }

    fn create(play: bool) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("没有可用的音频输出设备"))?;
        let config = device.default_output_config()?;

        // cpal 0.18 的 SampleRate 就是 u32 别名（不再是 newtype）。
        let format = AudioFormat {
            sample_rate: config.sample_rate(),
            channels: config.channels(),
        };

        let queue: SampleQueue = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let frames_played = Arc::new(AtomicU64::new(0));
        let underrun = Arc::new(AtomicBool::new(false));

        let started: StartedFlag = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let pushed_frames = Arc::new(AtomicU64::new(0));
        let grace = Arc::new(AtomicBool::new(false));

        let stream = Self::build_stream(
            &device,
            &config,
            queue.clone(),
            frames_played.clone(),
            underrun.clone(),
            started.clone(),
            format.channels,
            volume.clone(),
            grace.clone(),
        )?;
        if play {
            stream.play()?;
        }

        Ok(Self {
            _stream: stream,
            format,
            queue,
            frames_played,
            underrun,
            started,
            paused,
            volume,
            pushed_frames,
            grace,
        })
    }

    /// 开始播放。用于 [`new_paused`](Self::new_paused) 建好后、时机到了再开播。
    pub fn start(&self) {
        self._stream.play().ok();
        self.paused.store(false, Ordering::Relaxed);
    }

    /// 按设备的采样格式建流。
    ///
    /// 设备可能要 f32 / i16 / u16，解码侧统一产出 f32，这里负责末端转换。
    fn build_stream(
        device: &cpal::Device,
        config: &cpal::SupportedStreamConfig,
        queue: SampleQueue,
        frames_played: Arc<AtomicU64>,
        underrun: Arc<AtomicBool>,
        started: StartedFlag,
        channels: u16,
        volume: Arc<AtomicU32>,
        grace: Arc<AtomicBool>,
    ) -> Result<cpal::Stream> {
        let cfg = config.config();
        let err_fn = |e| tracing::error!(error = %e, "音频流错误");

        // 回调：从队列取采样填进设备缓冲，不足部分补静音。
        macro_rules! make_stream {
            ($sample:ty, $convert:expr) => {{
                let queue = queue.clone();
                let frames_played = frames_played.clone();
                let underrun = underrun.clone();
                let started = started.clone();
                let volume = volume.clone();
                device.build_output_stream(
                    cfg.clone(),
                    move |out: &mut [$sample], _: &cpal::OutputCallbackInfo| {
                        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                        let mut filled = 0usize;
                        let gain = f32::from_bits(volume.load(Ordering::Relaxed));
                        for slot in out.iter_mut() {
                            match q.pop_front() {
                                Some(v) => {
                                    *slot = $convert(v * gain);
                                    filled += 1;
                                }
                                // 队列空：补静音而不是留脏数据（否则是刺耳噪声）。
                                None => *slot = $convert(0.0f32),
                            }
                        }
                        drop(q);

                        let (advance, now_started, starved) =
                            account(out.len(), filled, started.load(Ordering::Relaxed));
                        started.store(now_started, Ordering::Relaxed);
                        if starved {
                            // 断续宽限：清队（seek/拖动静音）后的第一次断供是
                            // 预期的——吞掉，别让欠载告警误报。真正的解码
                            // 跟不上会连续断供，第二次起照常上报。
                            if !grace.swap(false, Ordering::Relaxed) {
                                underrun.store(true, Ordering::Relaxed);
                            }
                        }
                        frames_played.fetch_add(
                            (advance / channels as usize) as u64,
                            Ordering::Relaxed,
                        );
                    },
                    err_fn,
                    None,
                )?
            }};
        }

        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => make_stream!(f32, |v: f32| v),
            cpal::SampleFormat::I16 => make_stream!(i16, f32_to_i16),
            cpal::SampleFormat::U16 => make_stream!(u16, f32_to_u16),
            other => return Err(anyhow::anyhow!("不支持的采样格式: {other:?}")),
        };

        Ok(stream)
    }

    /// 设备的采样率与声道数。解码侧要按它重采样。
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// 追加交错排列的 f32 采样（长度应为 `channels` 的整数倍）。
    pub fn push_samples(&self, samples: &[f32]) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.extend(samples.iter().copied());
        drop(q);
        self.pushed_frames
            .fetch_add((samples.len() / self.format.channels.max(1) as usize) as u64, Ordering::Relaxed);
    }

    /// 解码侧累计推入的播放时长。
    pub fn pushed_position(&self) -> Duration {
        let frames = self.pushed_frames.load(Ordering::Relaxed);
        Duration::from_secs_f64(frames as f64 / self.format.sample_rate as f64)
    }

    /// 清空待播放队列（并丢弃）。用于拖动静音时把已推入但未播出的采样丢掉，
    /// 否则声卡 pause 前队列里已有的音频会继续播完（拖动中听到旧声音）。
    ///
    /// 同时置断续宽限：清队到新数据到达之间回调必然断供，属预期，
    /// 不该触发欠载告警。
    pub fn clear(&self) {
        self.grace.store(true, Ordering::Relaxed);
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.clear();
    }

    /// 标记一次**时间轴断续**（seek 跳变）。与 [`clear`](Self::clear) 的
    /// 区别：只吞欠载误报，不动队列——seek 后旧位置采样由调用方决定
    /// 丢弃时机，断续本身只改变「断供是否算故障」的语义。
    pub fn mark_discontinuity(&self) {
        self.grace.store(true, Ordering::Relaxed);
    }

    /// 队列中尚未播放的帧数。用于背压：太多就别再解了。
    pub fn queued_frames(&self) -> usize {
        let q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.len() / self.format.channels.max(1) as usize
    }

    /// 队列中尚未播放的时长。比帧数更适合写背压阈值——
    /// 阈值用「毫秒」表达，换了采样率也不用重算。
    pub fn queued_duration(&self) -> Duration {
        Duration::from_secs_f64(self.queued_frames() as f64 / self.format.sample_rate as f64)
    }

    /// 取一个可跨线程共享的时钟句柄。
    ///
    /// [`AudioOutput`] 本身不是 `Send`（cpal 的 `Stream` 不是），
    /// 但读时钟只需要那个原子计数器。渲染线程要做音视频同步，
    /// 就得能读到它，于是把这一小块单独递出去。
    pub fn clock(&self) -> AudioClock {
        AudioClock {
            frames_played: self.frames_played.clone(),
            started: self.started.clone(),
            sample_rate: self.format.sample_rate,
        }
    }

    /// 设备累计已消费的播放时长 —— **音频主时钟的读数**。
    ///
    /// 从第一个采样真正被播出时开始计时；在那之前恒为 0，
    /// 这样调用方推数据前的任意长等待都不会污染时钟。
    ///
    /// 注意这是「已交给设备」的量，和真正从扬声器出声之间还差一个
    /// 硬件缓冲延迟。做精确同步时需要减去 `output_latency`，
    /// 这一步留到接入同步时再处理。
    pub fn position(&self) -> Duration {
        let frames = self.frames_played.load(Ordering::Relaxed);
        Duration::from_secs_f64(frames as f64 / self.format.sample_rate as f64)
    }

    /// 第一个采样是否已经播出。`position()` 在此之前恒为 0。
    pub fn started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// 取出并清除「发生过欠载」的标记。
    pub fn take_underrun(&self) -> bool {
        self.underrun.swap(false, Ordering::Relaxed)
    }

    /// 暂停播放。声卡设备冻结，时钟（`frames_played`）随之停走，
    /// 已缓冲但未播出的采样保留，恢复后从停点继续。
    ///
    /// 暂停不影响队列内容：解码侧应同时停止推送，否则暂停期间
    /// 队列持续累积、恢复后反而超前播放。
    pub fn pause(&self) {
        self._stream.pause().ok();
        self.paused.store(true, Ordering::Relaxed);
    }

    /// 恢复播放（见 [`pause`](Self::pause)）。
    pub fn resume(&self) {
        self._stream.play().ok();
        self.paused.store(false, Ordering::Relaxed);
    }

    /// 当前是否处于暂停态。
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// 设置输出音量增益（0.0=静音，1.0=正常）。用于在回调里乘到每个采样上，
    /// 实现持久静音——与拖动时的临时 `pause()` 静音互不干扰。
    pub fn set_volume(&self, v: f32) {
        self.volume
            .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// 当前输出音量增益。
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }
}

/// 音频主时钟的只读句柄，可跨线程共享。
///
/// 只借走原子计数器，因此和 [`AudioOutput`] 不同，它是 `Send + Sync`。
#[derive(Clone)]
pub struct AudioClock {
    frames_played: Arc<AtomicU64>,
    started: StartedFlag,
    sample_rate: u32,
}

impl AudioClock {
    /// 当前播放进度。见 [`AudioOutput::position`]。
    pub fn position(&self) -> Duration {
        let frames = self.frames_played.load(Ordering::Relaxed);
        Duration::from_secs_f64(frames as f64 / self.sample_rate as f64)
    }

    /// 是否已经开始出声。没开始时 [`position`](Self::position) 恒为 0，
    /// 此时不能拿它当时钟用——否则画面会一直停在第一帧等音频。
    pub fn started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// 仅供测试：用给定的「已播放时长」和「是否开播」造一个假时钟。
    ///
    /// 真实时钟要从声卡的原子计数器读数，脱离设备没法造；
    /// 而 `PlaybackClock` 的音频主时钟路径只依赖 `position()` / `started()`，
    /// 这两个量用假值就能把调度逻辑测全。`player-app` 的测试也需要它，
    /// 故不限定 `cfg(test)`。
    #[doc(hidden)]
    pub fn for_test(position: Duration, started: bool) -> Self {
        let sample_rate = 48_000;
        let frames = (position.as_secs_f64() * sample_rate as f64).round() as u64;
        Self {
            frames_played: Arc::new(AtomicU64::new(frames)),
            started: Arc::new(AtomicBool::new(started)),
            sample_rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 采样格式转换是纯函数，可以脱离声卡测。
    // 值得单独测的原因：转换写错（少个钳位、u16 忘了偏移）在正弦波
    // beep 里未必听得出来，放到真实音频上却是持续的失真或爆音。

    #[test]
    fn i16_conversion_maps_full_range() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(-1.0), -i16::MAX);
    }

    #[test]
    fn i16_conversion_clamps_instead_of_wrapping() {
        // 不钳位的话整数回绕会把过冲变成反相的最大值——爆音。
        assert_eq!(f32_to_i16(5.0), i16::MAX);
        assert_eq!(f32_to_i16(-5.0), -i16::MAX);
    }

    #[test]
    fn u16_conversion_centers_silence_at_midpoint() {
        // u16 无符号：静音是中点，不是 0。
        let mid = f32_to_u16(0.0);
        assert!(
            (mid as i32 - 32767).abs() <= 1,
            "静音应落在中点附近，实际 {mid}"
        );
        assert_eq!(f32_to_u16(1.0), u16::MAX);
        assert_eq!(f32_to_u16(-1.0), 0);
    }

    /// 开播前的空转不该计入时钟。
    ///
    /// 这条守的是「position() 是播放进度，而不是流创建至今的墙钟」。
    /// 早先的实现把开播前的空回调也记进去，于是调用方晚 1 秒推数据，
    /// 时钟就凭空多出 1 秒。实测正是它让播完时时钟显示 10.34s
    /// 而实际只送入了 10.01s——用这种时钟做同步，画面永远追不上。
    #[test]
    fn clock_does_not_run_before_playback_starts() {
        let (advance, started, underrun) = account(512, 0, false);
        assert_eq!(advance, 0, "还没出声，时钟不该走");
        assert!(!started);
        assert!(!underrun, "启动时队列本来就是空的，不算故障");
    }

    #[test]
    fn clock_counts_requested_not_filled_once_running() {
        // 断供时补的静音同样占用了真实播放时间。若只记 filled，
        // 时钟会走慢，视频便会跟着一起卡住，而不是丢帧追上去。
        let (advance, started, underrun) = account(512, 128, true);
        assert_eq!(advance, 512, "应按索取量计时，而非实际填充量");
        assert!(started);
        assert!(underrun, "播着播着供不上，这才是真欠载");
    }

    #[test]
    fn first_nonempty_callback_starts_the_clock() {
        // 第一次拿到数据的这次回调，本身就该计入。
        let (advance, started, underrun) = account(512, 512, false);
        assert_eq!(advance, 512);
        assert!(started);
        assert!(!underrun);
    }

    #[test]
    fn u16_conversion_preserves_negative_half() {
        // 负半周必须落在中点**以下**。若漏了 *0.5+0.5 的映射，
        // 负值会被 clamp 成 0，整个下半波被削平。
        assert!(f32_to_u16(-0.5) < f32_to_u16(0.0));
        assert!(f32_to_u16(0.5) > f32_to_u16(0.0));
    }
}
