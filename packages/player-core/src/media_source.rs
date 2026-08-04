//! 媒体源抽象与 ffmpeg 实现。
//!
//! [`MediaSource`] 是 player-core 对外的核心边界：调用方（player-app / ocr-lab）
//! 只依赖这个 trait，不关心底层是 ffmpeg、Symphonia 还是别的。未来要换纯 Rust
//! 解码后端，只需再实现一个 `MediaSource` 即可。
//!
//! [`FfmpegSource`] 是当前基于 `ffmpeg-next`（动态链接系统 ffmpeg）的实现，
//! 提供逐帧拉取（BGRA）+ 运行时 seek 的能力。

use std::path::Path;
use std::time::Duration;

use ffmpeg_next::{
    format::Pixel,
    software::scaling::{context::Context as Scaler, flag::Flags},
    util::frame::Video,
    Error, Packet, Rational,
};

use crate::error::Result;
use crate::frame::{DecodedFrame, VideoInfo};

/// 媒体源：一个可打开、可逐帧解码、可 seek 的视频。
pub trait MediaSource {
    /// 打开一个本地文件或 URL。
    fn open(path: &Path) -> Result<Self>
    where
        Self: Sized;

    /// 视频流元信息（宽高、时长、帧率）。
    fn video_info(&self) -> VideoInfo;

    /// 拉取下一帧。到文件末尾返回 `Ok(None)`。
    fn next_frame(&mut self) -> Result<Option<DecodedFrame>>;

    /// 跳转到指定时间点（最近关键帧）。已解码的残帧需注意在调用方处理。
    fn seek(&mut self, ts: Duration) -> Result<()>;
}

/// 基于 `ffmpeg-next` 的媒体源实现。
pub struct FfmpegSource {
    input: ffmpeg_next::format::context::Input,
    /// 视频流在输入里的下标。
    stream_index: usize,
    /// 视频流的时间基（PTS 单位 → 秒的换算分母）。
    time_base: Rational,
    /// 视频解码器。
    decoder: ffmpeg_next::decoder::Video,
    /// YUV/原生格式 → BGRA 的缩放/色彩转换上下文。
    scaler: Scaler,
    /// 复用：解码器产出的原始帧（scaler 的输入）。
    raw_frame: Video,
    /// 复用：scaler 输出的 BGRA 帧（GPU 上传源）。
    rgba_frame: Video,
    /// 上次 send_packet 因解码器输入缓冲满(EAGAIN)而未送出的 packet，暂存待重试。
    pending: Option<Packet>,
    width: u32,
    height: u32,
    duration: Duration,
    fps: f64,
}

impl MediaSource for FfmpegSource {
    fn open(path: &Path) -> Result<Self> {
        ffmpeg_next::init()?;

        if !path.exists() {
            return Err(anyhow::anyhow!("source not found: {}", path.display()));
        }

        let input = ffmpeg_next::format::input(&path)?;

        // 找最佳视频流。
        let stream = input
            .streams()
            .best(ffmpeg_next::media::Type::Video)
            .ok_or_else(|| anyhow::anyhow!("no video stream in source"))?;
        let stream_index = stream.index();
        let time_base = stream.time_base();

        let ctx = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())?;
        let decoder = ctx.decoder().video()?;

        let (width, height) = (decoder.width(), decoder.height());
        let scaler = Scaler::get(
            decoder.format(),
            width,
            height,
            Pixel::BGRA,
            width,
            height,
            Flags::BILINEAR,
        )?;

        // 时长：优先流时长，回退到容器时长（均基于 time_base，转秒）。
        let duration = Duration::from_secs_f64(stream.duration().unsigned_abs() as f64
            * f64::from(time_base.numerator())
            / f64::from(time_base.denominator()));
        let fps = {
            let r = stream.avg_frame_rate();
            if r.denominator() != 0 {
                r.numerator() as f64 / r.denominator() as f64
            } else {
                0.0
            }
        };

        Ok(Self {
            input,
            stream_index,
            time_base,
            decoder,
            scaler,
            raw_frame: Video::empty(),
            rgba_frame: Video::empty(),
            pending: None,
            width,
            height,
            duration,
            fps,
        })
    }

    fn video_info(&self) -> VideoInfo {
        VideoInfo {
            width: self.width,
            height: self.height,
            duration: self.duration,
            fps: self.fps,
        }
    }

    fn next_frame(&mut self) -> Result<Option<DecodedFrame>> {
        // ffmpeg 官方 demux/decode 状态机：
        //   - receive_frame 返回 EAGAIN  ⇒ 解码器要更多 packet，继续送
        //   - send_packet  返回 EAGAIN  ⇒ 解码器输入缓冲满，先 receive 排空（暂存 packet 重试）
        //   - receive_frame 返回 Eof    ⇒ 仅当已 send_eof（draining）后才表示真正结束
        let mut eof_sent = false;
        loop {
            // 1) 确保解码器有 packet 可吃，或已进入 draining 模式。
            if !eof_sent {
                let packet = match self.pending.take() {
                    Some(p) => p,
                    None => match self.input.packets().next() {
                        Some((stream, packet)) if stream.index() == self.stream_index => packet,
                        Some(_) => continue, // 非视频流，跳过
                        None => {
                            // 文件读完：进入 draining。draining 重复调用会返回 Eof，
                            // 这是正常终止信号，忽略错误。
                            let _ = self.decoder.send_eof();
                            eof_sent = true;
                            // 直接进入收帧阶段。
                            match self.decoder.receive_frame(&mut self.raw_frame) {
                                Ok(()) => return Ok(Some(self.frame_to_decoded()?)),
                                Err(Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => {
                                    continue
                                }
                                Err(Error::Eof) => return Ok(None),
                                Err(e) => return Err(e.into()),
                            }
                        }
                    },
                };
                match self.decoder.send_packet(&packet) {
                    Ok(()) => {}
                    Err(Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => {
                        // 解码器输入满：暂存，先去 receive 排空，下轮重试。
                        self.pending = Some(packet);
                    }
                    Err(e) => return Err(e.into()),
                }
            }

            // 2) 收一帧。
            match self.decoder.receive_frame(&mut self.raw_frame) {
                Ok(()) => return Ok(Some(self.frame_to_decoded()?)),
                Err(Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => continue,
                Err(Error::Eof) => {
                    if eof_sent {
                        return Ok(None); // 真正结束
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn seek(&mut self, ts: Duration) -> Result<()> {
        let ts_us = ts.as_micros() as i64;
        // avformat_seek_file：stream_index=-1 表示 ts 单位是微秒（AV_TIME_BASE）。
        self.input.seek(ts_us, ..ts_us)?;
        // seek 后必须 flush 解码器，丢弃残留守帧，否则花屏。
        self.decoder.flush();
        Ok(())
    }
}

impl FfmpegSource {
    /// 把当前 raw_frame 转 BGRA 后拷成 [`DecodedFrame`]，并换算 PTS 为 Duration。
    fn frame_to_decoded(&mut self) -> Result<DecodedFrame> {
        // 原始帧 → BGRA（scaler 在 rgba_frame 为空时自动 alloc）。
        self.scaler.run(&self.raw_frame, &mut self.rgba_frame)?;

        let width = self.width;
        let height = self.height;
        let stride = self.rgba_frame.stride(0) as usize;
        // 注意：PTS 必须取自解码出的原始帧 raw_frame，而非 scaler 输出帧 rgba_frame。
        // swscale 生成的输出帧不带时间戳，rgba_frame.timestamp() 恒为 None。
        let pts = match self.raw_frame.timestamp() {
            Some(ts) => Duration::from_secs_f64(
                ts as f64 * f64::from(self.time_base.numerator())
                    / f64::from(self.time_base.denominator()),
            ),
            None => Duration::ZERO,
        };

        // 复制像素（含 stride 填充）。GPU 上传时由调用方决定要不要紧凑化。
        let data = self.rgba_frame.data(0).to_vec();

        Ok(DecodedFrame {
            data,
            stride,
            width,
            height,
            pts,
        })
    }
}
