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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
}

/// 播放是否真正开始过（收到过非空采样）。
/// 用于把「启动时队列还没填上」与「播着播着断供」区分开。
type StartedFlag = Arc<AtomicBool>;

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

        let stream = Self::build_stream(
            &device,
            &config,
            queue.clone(),
            frames_played.clone(),
            underrun.clone(),
            Arc::new(AtomicBool::new(false)),
            format.channels,
        )?;
        stream.play()?;

        Ok(Self {
            _stream: stream,
            format,
            queue,
            frames_played,
            underrun,
        })
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
                device.build_output_stream(
                    cfg.clone(),
                    move |out: &mut [$sample], _: &cpal::OutputCallbackInfo| {
                        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                        let mut filled = 0usize;
                        for slot in out.iter_mut() {
                            match q.pop_front() {
                                Some(v) => {
                                    *slot = $convert(v);
                                    filled += 1;
                                }
                                // 队列空：补静音而不是留脏数据（否则是刺耳噪声）。
                                None => *slot = $convert(0.0f32),
                            }
                        }
                        drop(q);

                        // 只有「曾经出过声、之后又断供」才算欠载。
                        // 启动瞬间队列必然是空的（流先跑起来、采样后到），
                        // 那不是故障；把它算进去会让每次启动都误报一次，
                        // 于是这个信号就再也没人信了。
                        if filled > 0 {
                            started.store(true, Ordering::Relaxed);
                        }
                        if filled < out.len() && started.load(Ordering::Relaxed) {
                            underrun.store(true, Ordering::Relaxed);
                        }
                        // 记账用**请求量**而非实际填充量：设备的时间照走，
                        // 补的静音也占用了真实播放时间。用 filled 会让时钟变慢。
                        frames_played
                            .fetch_add((out.len() / channels as usize) as u64, Ordering::Relaxed);
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
    }

    /// 队列中尚未播放的帧数。用于背压：太多就别再解了。
    pub fn queued_frames(&self) -> usize {
        let q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.len() / self.format.channels.max(1) as usize
    }

    /// 设备累计已消费的播放时长 —— **音频主时钟的读数**。
    ///
    /// 注意这是「已交给设备」的量，和真正从扬声器出声之间还差一个
    /// 硬件缓冲延迟。做精确同步时需要减去 `output_latency`，
    /// 这一步留到接入同步时再处理。
    pub fn position(&self) -> Duration {
        let frames = self.frames_played.load(Ordering::Relaxed);
        Duration::from_secs_f64(frames as f64 / self.format.sample_rate as f64)
    }

    /// 取出并清除「发生过欠载」的标记。
    pub fn take_underrun(&self) -> bool {
        self.underrun.swap(false, Ordering::Relaxed)
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

    #[test]
    fn u16_conversion_preserves_negative_half() {
        // 负半周必须落在中点**以下**。若漏了 *0.5+0.5 的映射，
        // 负值会被 clamp 成 0，整个下半波被削平。
        assert!(f32_to_u16(-0.5) < f32_to_u16(0.0));
        assert!(f32_to_u16(0.5) > f32_to_u16(0.0));
    }
}
