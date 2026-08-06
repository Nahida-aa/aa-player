//! 音频解码 + 重采样。
//!
//! 输入是容器里的音频 packet，输出是**能直接喂给声卡**的 [`AudioChunk`]：
//! 采样率、声道数、采样格式都已经对齐 [`AudioFormat`]。
//!
//! 为什么解码器不能直接输出可用数据：
//!   - AAC 解码出来是 `fltp`（平面 f32），每个声道一块连续内存；
//!     cpal 要的是交错的 `[L, R, L, R, ...]`。
//!   - 媒体的采样率是编码时定的（这里 48kHz），声卡的采样率是驱动定的
//!     （可能 44.1kHz）。两者不一致时若不重采样，音调会整体偏移。
//!   - 声道数也常常对不上：单声道素材 + 立体声设备，需要上混。
//!
//! 这三件事 swresample 一次性做完，所以这里只维护一个 `Resampler`。

use std::time::Duration;

use ffmpeg_next::{
    ChannelLayout, Error, Packet, Rational, format::Sample as SampleFormat,
    software::resampling::Context as Resampler, util::frame::Audio,
};

use crate::audio_output::AudioFormat;
use crate::error::Result;
use crate::frame::AudioChunk;

/// 音频流的元信息。
#[derive(Debug, Clone, Copy)]
pub struct AudioInfo {
    /// 源采样率（重采样前）。
    pub sample_rate: u32,
    /// 源声道数（重采样前）。
    pub channels: u16,
    /// 总时长；部分流可能为 0。
    pub duration: Duration,
}

/// 解码一路音频流并重采样到目标设备格式。
pub struct AudioDecoder {
    decoder: ffmpeg_next::decoder::Audio,
    resampler: Resampler,
    /// 流的时间基，用于把 PTS 换算成 Duration。
    time_base: Rational,
    /// 目标格式（= 声卡格式）。
    target: AudioFormat,
    info: AudioInfo,
    /// 播放速度倍率（1.0=原速，>1 快进，<1 慢放）。通过改变重采样**输出**采样率
    /// 实现：声卡按固定设备率消费，产出采样数 = 输入 × (设备率/output_rate)，
    /// 故 output_rate = 设备率/speed 时真实播放时长 = 原时长/speed。
    /// 音频主时钟用设备率读数，不受 output_rate 影响，故视频帧调度自动同步。
    speed: f64,
    /// 当前重采样输出采样率（= 设备率/speed）。缓存以便 seek 后 flush 重建。
    output_rate: u32,
    /// 复用：解码器输出帧。
    raw: Audio,
    /// 复用：重采样输出帧。
    resampled: Audio,
    /// 上一块音频的结束时间。用于给缺失 PTS 的帧补时间戳。
    next_pts: Duration,
}

impl AudioDecoder {
    /// 从流参数构造，输出对齐 `target`。
    pub fn new(stream: &ffmpeg_next::format::stream::Stream, target: AudioFormat) -> Result<Self> {
        let time_base = stream.time_base();
        let ctx = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())?;
        let decoder = ctx.decoder().audio()?;

        // 解码器有可能不填 channel_layout（只给了 channels），
        // 这时按声道数取默认布局，否则 swresample 会拒绝初始化。
        let src_layout = if decoder.channel_layout().is_empty() {
            ChannelLayout::default(decoder.channels() as i32)
        } else {
            decoder.channel_layout()
        };
        let dst_layout = ChannelLayout::default(target.channels as i32);

        // speed=1 时 output_rate = 设备率；倍速经 set_speed 改变。
        let output_rate = target.sample_rate;
        let resampler = Self::build_resampler(
            decoder.format(),
            src_layout,
            decoder.rate(),
            dst_layout,
            output_rate,
        )?;

        let duration = Duration::from_secs_f64(
            stream.duration().unsigned_abs() as f64 * f64::from(time_base.numerator())
                / f64::from(time_base.denominator()),
        );

        let info = AudioInfo {
            sample_rate: decoder.rate(),
            channels: decoder.channels(),
            duration,
        };

        Ok(Self {
            decoder,
            resampler,
            time_base,
            target,
            info,
            speed: 1.0,
            output_rate,
            raw: Audio::empty(),
            resampled: Audio::empty(),
            next_pts: Duration::ZERO,
        })
    }

    /// 构造一个重采样器：输入为源格式/率，输出为交错的 f32，目标设备格式，
    /// 输出采样率为 `output_rate`（= 设备率/speed）。抽出来给 `new`/`set_speed`/
    /// `flush` 共用，三处都要按当前 speed 重建重采样器。
    fn build_resampler(
        in_format: ffmpeg_next::format::Sample,
        in_layout: ChannelLayout,
        in_rate: u32,
        out_layout: ChannelLayout,
        out_sample_rate: u32,
    ) -> Result<Resampler> {
        Ok(Resampler::get(
            in_format,
            in_layout,
            in_rate,
            // 统一输出交错 f32：cpal 的缓冲就是交错的，
            // 末端要转 i16/u16 时再由 AudioOutput 负责。
            SampleFormat::F32(ffmpeg_next::format::sample::Type::Packed),
            out_layout,
            out_sample_rate,
        )?)
    }

    /// 当前播放速度倍率。
    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// 设置播放速度倍率（clamp 到 [0.25, 4.0]）。改变重采样输出率，使真实
    /// 播放时长 = 原时长/speed，音频主时钟仍按设备率读数，视频自动同步。
    pub fn set_speed(&mut self, speed: f64) {
        let speed = speed.clamp(0.25, 4.0);
        if (speed - self.speed).abs() < f64::EPSILON {
            return;
        }
        self.speed = speed;
        // output_rate = 设备率 / speed（speed 越大产出越少采样 → 播得越快）。
        self.output_rate = (self.target.sample_rate as f64 / speed) as u32;
        if let Ok(r) = Self::build_resampler(
            self.decoder.format(),
            if self.decoder.channel_layout().is_empty() {
                ChannelLayout::default(self.decoder.channels() as i32)
            } else {
                self.decoder.channel_layout()
            },
            self.decoder.rate(),
            ChannelLayout::default(self.target.channels as i32),
            self.output_rate,
        ) {
            self.resampler = r;
        }
    }

    /// 源音频流的元信息。
    pub fn info(&self) -> AudioInfo {
        self.info
    }

    /// 输出格式（= 声卡格式）。
    pub fn format(&self) -> AudioFormat {
        self.target
    }

    /// 送一个 packet 进解码器。返回 `false` 表示解码器输入缓冲满，
    /// 调用方需要先 [`receive`](Self::receive) 排空再重试同一个 packet。
    pub fn send(&mut self, packet: &Packet) -> Result<bool> {
        match self.decoder.send_packet(packet) {
            Ok(()) => Ok(true),
            Err(Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// 告知解码器输入已结束，进入 draining，把内部缓冲的帧吐完。
    pub fn send_eof(&mut self) {
        // draining 期间重复调用会返回 Eof，是正常信号，忽略。
        let _ = self.decoder.send_eof();
    }

    /// 取一块解码 + 重采样后的音频。
    ///
    /// - `Ok(Some(chunk))`：拿到数据
    /// - `Ok(None)`：解码器暂时没有输出（需要更多 packet，或已 drain 完）
    ///
    /// 这里把 EAGAIN 和 EOF 都映射成 `None`，是因为音频侧的「还要不要继续」
    /// 由 demux 循环统一判断（它才知道文件读没读完）；解码器自己分不清。
    pub fn receive(&mut self) -> Result<Option<AudioChunk>> {
        match self.decoder.receive_frame(&mut self.raw) {
            Ok(()) => {}
            Err(Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => return Ok(None),
            Err(Error::Eof) => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        let pts = match self.raw.timestamp() {
            Some(ts) => Duration::from_secs_f64(
                ts as f64 * f64::from(self.time_base.numerator())
                    / f64::from(self.time_base.denominator()),
            ),
            // 少数容器不给音频帧打 PTS。用「上一块的结束时间」续上，
            // 比塞 ZERO 好：塞 ZERO 会让同步逻辑以为音频一直卡在开头。
            None => self.next_pts,
        };

        // resampled 复用同一个 frame：必须清空 samples 计数，
        // 否则 swr_convert_frame 会以为输出缓冲只剩上次的大小。
        self.resampled = Audio::empty();
        self.resampler.run(&self.raw, &mut self.resampled)?;

        let chunk = self.interleaved_chunk(pts);
        self.next_pts = pts + chunk.duration();
        Ok(Some(chunk))
    }

    /// 把 swresample 内部残留的采样吐出来（在 [`send_eof`](Self::send_eof)
    /// 且解码器已 drain 完之后调用）。
    ///
    /// 重采样器为了做插值会攒住尾部若干采样。不 flush 的话，
    /// 每次播放都会丢掉结尾几毫秒——单次听不出来，但 seek 频繁时会累积成断续。
    pub fn flush_resampler(&mut self) -> Result<Option<AudioChunk>> {
        // 与 `run` 不同，`flush` **不会**替我们分配输出帧：它把空帧的
        // 「0 采样、无格式」当成输出参数变了，直接报 `Output changed`。
        // 所以这里必须先按目标格式开好缓冲。
        let remaining = self.resampler.delay().map_or(0, |d| d.output as usize);
        if remaining == 0 {
            return Ok(None);
        }
        self.resampled = Audio::new(
            SampleFormat::F32(ffmpeg_next::format::sample::Type::Packed),
            remaining,
            ChannelLayout::default(self.target.channels as i32),
        );
        self.resampler.flush(&mut self.resampled)?;
        if self.resampled.samples() == 0 {
            return Ok(None);
        }
        let chunk = self.interleaved_chunk(self.next_pts);
        self.next_pts += chunk.duration();
        Ok(Some(chunk))
    }

    /// seek 之后丢弃解码器与重采样器里的残留数据。
    pub fn flush(&mut self) {
        self.decoder.flush();
        // 重采样器没有 reset API；重建一个是最干净的做法。
        // 残留采样若不清掉，seek 后开头会混入上一个位置的声音。
        // 用当前 speed 对应的 output_rate 重建，否则 seek 后变速会失效。
        if let Ok(r) = Self::build_resampler(
            self.decoder.format(),
            if self.decoder.channel_layout().is_empty() {
                ChannelLayout::default(self.decoder.channels() as i32)
            } else {
                self.decoder.channel_layout()
            },
            self.decoder.rate(),
            ChannelLayout::default(self.target.channels as i32),
            self.output_rate,
        ) {
            self.resampler = r;
        }
        self.next_pts = Duration::ZERO;
    }

    /// 从重采样输出帧里取出交错 f32。
    ///
    /// 输出格式已指定为 Packed，所有声道都在 plane 0，按 `[L,R,L,R,...]` 排列。
    ///
    /// 这里**不能**用 `plane::<f32>(0)`：它返回的切片长度是 `samples()`，
    /// 也就是**帧数**，而 packed 布局下 plane 0 里其实有 `帧数 × 声道数` 个值。
    /// 用它取立体声数据只会拿到前一半，听感是音频被截断到一半时长。
    /// 走 `data(0)`（按 linesize 取字节）再重解释成 f32 才是完整的。
    fn interleaved_chunk(&self, pts: Duration) -> AudioChunk {
        let channels = self.target.channels as usize;
        let wanted = self.resampled.samples() * channels;

        let bytes = self.resampled.data(0);
        // linesize 含 32 字节对齐的尾部填充，实际有效值只有 wanted 个。
        let avail = bytes.len() / std::mem::size_of::<f32>();
        // 缓冲比预期小说明上面的排布假设错了。静默截断只会让声音短一截、
        // 难以察觉；这里宁可留个明确的警告。
        if avail < wanted {
            tracing::warn!(wanted, avail, "重采样输出缓冲小于预期，音频会被截断");
        }
        let take = wanted.min(avail);

        let mut samples = vec![0.0f32; take];
        // SAFETY: ffmpeg 保证 data(0) 起始按 f32 对齐（AVFrame 缓冲对齐 ≥32 字节），
        // 且 take*4 ≤ bytes.len()。用 copy 而非 transmute 切片，避免对齐假设外溢。
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const f32, samples.as_mut_ptr(), take);
        }

        AudioChunk {
            samples,
            channels: self.target.channels,
            sample_rate: self.output_rate,
            pts,
        }
    }
}
