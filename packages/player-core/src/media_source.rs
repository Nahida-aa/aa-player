//! 媒体源抽象与 ffmpeg 实现。
//!
//! [`MediaSource`] 是 player-core 对外的核心边界：调用方（player-app / ocr-lab）
//! 只依赖这个 trait，不关心底层是 ffmpeg、Symphonia 还是别的。未来要换纯 Rust
//! 解码后端，只需再实现一个 `MediaSource` 即可。
//!
//! [`FfmpegSource`] 是当前基于 `ffmpeg-next`（动态链接系统 ffmpeg）的实现，
//! 提供音视频逐单元拉取（视频 BGRA / 音频交错 f32）+ 运行时 seek 的能力。
//!
//! 音视频从**同一个** [`MediaSource::next_event`] 出口交付（见 [`MediaEvent`]），
//! 因为容器里本就只有一条 packet 流；分成两个方法必然要缓冲另一路，
//! 而那个缓冲要么无界（吃光内存）要么会死锁。

use std::path::Path;
use std::time::Duration;

use ffmpeg_next::{
    Error, Packet, Rational,
    format::Pixel,
    software::scaling::{context::Context as Scaler, flag::Flags},
    util::frame::Video,
};

use crate::audio_decoder::{AudioDecoder, AudioInfo};
use crate::audio_output::AudioFormat;
use crate::error::Result;
use crate::frame::{AudioChunk, DecodedFrame, VideoInfo};

/// 从媒体里解出来的一个单元。
///
/// 音频和视频**共享同一条 demux 流**，谁的包先到就先产出谁，
/// 因此调用方拿到的顺序天然接近二者的 PTS 顺序。
///
/// 为什么不是 `next_video()` / `next_audio()` 两个方法：容器里只有一条
/// packet 流，分开拉就必须把另一路的包缓冲起来。而缓冲无上界时，
/// 「只拉音频」会把整条视频攒进内存；有上界时又会死锁（缓冲满了，
/// 但调用方偏偏还在要另一路）。统一出口把这个两难消掉了。
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// 一帧视频。
    Video(DecodedFrame),
    /// 一块音频（已重采样到打开时指定的设备格式）。
    Audio(AudioChunk),
}

/// 媒体源：一个可打开、可逐单元解码、可 seek 的媒体。
pub trait MediaSource {
    /// 打开一个本地文件，只解视频。
    fn open(path: &Path) -> Result<Self>
    where
        Self: Sized;

    /// 打开并同时解码音频，重采样到 `audio` 指定的设备格式。
    ///
    /// 传 `None` 等价于 [`open`](Self::open)。文件本身没有音轨时不算错误，
    /// 只是不会产出 [`MediaEvent::Audio`]。
    fn open_with(path: &Path, audio: Option<AudioFormat>) -> Result<Self>
    where
        Self: Sized;

    /// 视频流元信息（宽高、时长、帧率）。
    fn video_info(&self) -> VideoInfo;

    /// 音频流元信息；没有音轨或未开启音频解码时为 `None`。
    fn audio_info(&self) -> Option<AudioInfo>;

    /// 拉取下一个单元。到文件末尾返回 `Ok(None)`。
    fn next_event(&mut self) -> Result<Option<MediaEvent>>;

    /// 拉取下一帧视频，丢弃途中遇到的音频。
    ///
    /// 给纯视频场景（逐帧分析、缩略图）的便利方法。开了音频解码时用它
    /// 会把声音悄悄丢掉，所以要播放请用 [`next_event`](Self::next_event)。
    fn next_frame(&mut self) -> Result<Option<DecodedFrame>> {
        loop {
            match self.next_event()? {
                Some(MediaEvent::Video(f)) => return Ok(Some(f)),
                Some(MediaEvent::Audio(_)) => continue,
                None => return Ok(None),
            }
        }
    }

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
    /// 音频解码器；未开启音频或文件无音轨时为 `None`。
    audio: Option<AudioTrack>,
    /// 已 send_eof、正在把解码器内部缓冲吐干净。
    draining: bool,
}

/// 音频那一路的状态。
struct AudioTrack {
    index: usize,
    decoder: AudioDecoder,
    /// 解码器已 drain 完，接下来该把重采样器里的残留也吐出来。
    flushing_resampler: bool,
}

impl MediaSource for FfmpegSource {
    fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, None)
    }

    fn open_with(path: &Path, audio: Option<AudioFormat>) -> Result<Self> {
        ffmpeg_next::init()?;

        if !path.exists() {
            return Err(anyhow::anyhow!("source not found: {}", path.display()));
        }

        Self::from_input_with(ffmpeg_next::format::input(&path)?, audio)
    }

    fn video_info(&self) -> VideoInfo {
        VideoInfo {
            width: self.width,
            height: self.height,
            duration: self.duration,
            fps: self.fps,
        }
    }

    fn audio_info(&self) -> Option<AudioInfo> {
        self.audio.as_ref().map(|a| a.decoder.info())
    }

    fn next_event(&mut self) -> Result<Option<MediaEvent>> {
        // ffmpeg 官方 demux/decode 状态机：
        //   - receive_frame 返回 EAGAIN  ⇒ 解码器要更多 packet，继续送
        //   - send_packet  返回 EAGAIN  ⇒ 解码器输入缓冲满，先 receive 排空（暂存 packet 重试）
        //   - receive_frame 返回 Eof    ⇒ 仅当已 send_eof（draining）后才表示真正结束
        //
        // 两路解码器共用这一套流程：读到的 packet 按 stream index 分发，
        // 每轮先看看有没有现成的解码结果可以交付。
        loop {
            // 1) 优先把已解出的东西交出去，不必等下一个 packet。
            if let Some(ev) = self.take_ready()? {
                return Ok(Some(ev));
            }

            if self.draining {
                // 两个解码器都吐干净了，还要收走重采样器尾部的残留。
                if let Some(a) = self.audio.as_mut()
                    && a.flushing_resampler
                    && let Some(c) = a.decoder.flush_resampler()?
                {
                    return Ok(Some(MediaEvent::Audio(c)));
                }
                return Ok(None); // 真正结束
            }

            // 2) 喂一个 packet 进去。
            let packet = match self.pending.take() {
                Some(p) => Some(p),
                None => self.read_packet()?,
            };

            match packet {
                Some(packet) => self.dispatch(packet)?,
                None => {
                    // 文件读完：两路一起进入 draining。
                    // draining 期间重复 send_eof 会返回 Eof，是正常信号，忽略。
                    let _ = self.decoder.send_eof();
                    if let Some(a) = self.audio.as_mut() {
                        a.decoder.send_eof();
                    }
                    self.draining = true;
                }
            }
        }
    }

    fn seek(&mut self, ts: Duration) -> Result<()> {
        let ts_us = ts.as_micros() as i64;
        // avformat_seek_file：stream_index=-1 表示 ts 单位是微秒（AV_TIME_BASE）。
        self.input.seek(ts_us, ..ts_us)?;
        // seek 后必须 flush 解码器，丢弃残留守帧，否则花屏。
        self.decoder.flush();
        if let Some(a) = self.audio.as_mut() {
            a.decoder.flush();
            a.flushing_resampler = false;
        }
        // 暂存的 packet 属于 seek 前的位置，留着会在新位置插进一段旧内容。
        self.pending = None;
        // seek 到文件中间后又能继续读了，draining 状态必须撤销，
        // 否则播到过结尾再 seek 回去会立刻又报 EOF。
        self.draining = false;
        Ok(())
    }
}

impl FfmpegSource {
    /// 从一个已打开的输入上下文构造。
    ///
    /// 与 [`MediaSource::open`] 的区别只在于「怎么拿到 `Input`」：
    /// `open` 负责本地路径校验，这里则接受任何来源（含测试里构造的流），
    /// 好处是解码逻辑可以脱离文件系统单独测试。
    pub(crate) fn from_input_with(
        input: ffmpeg_next::format::context::Input,
        audio_format: Option<AudioFormat>,
    ) -> Result<Self> {
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
        let duration = Duration::from_secs_f64(
            stream.duration().unsigned_abs() as f64 * f64::from(time_base.numerator())
                / f64::from(time_base.denominator()),
        );
        let fps = {
            let r = stream.avg_frame_rate();
            if r.denominator() != 0 {
                r.numerator() as f64 / r.denominator() as f64
            } else {
                0.0
            }
        };

        // 音频是可选的：没开启、或文件本身无音轨，都只是"没有声音"，
        // 不该让打开失败——纯视频素材是常见输入。
        let audio = match audio_format {
            Some(fmt) => match input.streams().best(ffmpeg_next::media::Type::Audio) {
                Some(s) => {
                    let index = s.index();
                    Some(AudioTrack {
                        index,
                        decoder: AudioDecoder::new(&s, fmt)?,
                        flushing_resampler: false,
                    })
                }
                None => None,
            },
            None => None,
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
            audio,
            draining: false,
        })
    }

    /// 看看两个解码器里有没有已经解好、可以直接交付的东西。
    ///
    /// 视频优先只是个任意选择：同一轮里两者都有产出的情况很少，
    /// 且调用方本来就要按 PTS 自行调度，谁先出来不影响正确性。
    fn take_ready(&mut self) -> Result<Option<MediaEvent>> {
        match self.decoder.receive_frame(&mut self.raw_frame) {
            Ok(()) => return Ok(Some(MediaEvent::Video(self.frame_to_decoded()?))),
            Err(Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => {}
            // Eof 只说明**这一路**吐完了，另一路可能还有货，所以不能就此返回 None。
            Err(Error::Eof) => {}
            Err(e) => return Err(e.into()),
        }

        if let Some(a) = self.audio.as_mut() {
            match a.decoder.receive()? {
                Some(c) => return Ok(Some(MediaEvent::Audio(c))),
                // receive 把 EAGAIN 和 Eof 都映射成 None。draining 阶段的 None
                // 就意味着解码器空了，该轮到重采样器交尾巴了。
                None if self.draining => a.flushing_resampler = true,
                None => {}
            }
        }

        Ok(None)
    }

    /// 把一个 packet 送进它所属的解码器。
    ///
    /// 输入满（EAGAIN）时暂存到 `pending`，下一轮先 receive 排空再重试
    /// ——**不能丢弃**，丢一个音频包就是一段静音，丢一个视频包会花屏到下个关键帧。
    fn dispatch(&mut self, packet: Packet) -> Result<()> {
        let index = packet.stream();

        if index == self.stream_index {
            match self.decoder.send_packet(&packet) {
                Ok(()) => {}
                Err(Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => {
                    self.pending = Some(packet);
                }
                Err(e) => return Err(e.into()),
            }
            return Ok(());
        }

        if let Some(a) = self.audio.as_mut()
            && index == a.index
            && !a.decoder.send(&packet)?
        {
            self.pending = Some(packet);
        }
        // 其余流（字幕等）无人认领，丢弃。
        Ok(())
    }

    /// 读出下一个 packet（不分流）；返回 `Ok(None)` 表示文件真正读完。
    ///
    /// 分流交给 [`dispatch`](Self::dispatch)：在这里过滤会让「跳过的包」和
    /// 「没有的包」混在一起，音频那一路就永远等不到自己的数据。
    ///
    /// 这里刻意不用 `input.packets()` 迭代器：它把「真 EOF」和「I/O 错误」
    /// 都折叠成 `None`（见 rust-ffmpeg `PacketIter::next`），调用方无从区分，
    /// 于是读盘出错会被静默当成「正常播完」。直接驱动 [`Packet::read`]
    /// 才能拿到原始错误码，从而分三类处理：
    ///   - `Eof`         → 真正结束
    ///   - `InvalidData` → 单个坏包，解复用器可以重新同步，跳过继续
    ///   - 其它          → 终止性错误（I/O 失败、读取被取消），向上报错
    ///
    /// 注意终止性错误**不能重试**：它会被记录进 `AVIOContext->pb->error`，
    /// 之后每次 `av_read_frame` 都返回同一个错误。实测旧写法遇到连接中断时
    /// 会 98% CPU 空转且永不退出——回归测试见本文件底部的
    /// `read_error_surfaces_instead_of_looking_like_eof`。
    fn read_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            let mut packet = Packet::empty();
            match packet.read(&mut self.input) {
                Ok(()) => return Ok(Some(packet)),
                Err(Error::Eof) => return Ok(None),
                // 坏包：解复用器能越过 AVERROR_INVALIDDATA 重新同步，跳过即可。
                Err(Error::InvalidData) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// 把当前 raw_frame 转 BGRA 后拷成 [`DecodedFrame`]，并换算 PTS 为 Duration。
    fn frame_to_decoded(&mut self) -> Result<DecodedFrame> {
        // 原始帧 → BGRA（scaler 在 rgba_frame 为空时自动 alloc）。
        self.scaler.run(&self.raw_frame, &mut self.rgba_frame)?;

        let width = self.width;
        let height = self.height;
        let stride = self.rgba_frame.stride(0);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 把测试素材重封装成 MPEG-TS 字节流（`-c copy`，不重新编码）。
    ///
    /// 为什么必须是 TS：mp4 的 `moov` 在文件尾，数据截断后**打开阶段**就失败，
    /// 根本走不到解码循环，也就测不到 `next_frame` 的错误处理。
    /// TS 是流式容器，头部信息在最前面，残缺数据照样能开。
    ///
    /// 不把 .ts 存进仓库：它只服务于这一条测试，且能从 sample.mp4 无损推导。
    /// ffmpeg 命令不可用时返回 None，让测试跳过而不是误报失败。
    fn sample_as_mpegts() -> Option<Vec<u8>> {
        let sample =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/assets/sample.mp4");
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(sample)
            .args(["-c", "copy", "-f", "mpegts", "pipe:1"])
            .output()
            .ok()?;
        (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
    }

    /// 读取中断必须报错，不能伪装成「正常播完」。
    ///
    /// 背景：旧实现用 `input.packets()` 迭代器，而 rust-ffmpeg 的
    /// `PacketIter::next` 把**所有**错误都折叠成 `None`，于是网络中断/读盘失败
    /// 和真 EOF 长得一模一样。后果不止是误报播完：`next_frame` 收到 `None` 会
    /// 去 `send_eof` 进入 draining，而终止性错误已被记进 `AVIOContext->pb->error`
    /// 且是**粘性**的，之后每次 `av_read_frame` 都返回它，外层循环便一直空转
    /// ——实测跑满 98% CPU 且永不退出。
    ///
    /// 怎么造出这个错误：本地文件造不出来。截断文件、FIFO 关闭写端、concat 分片
    /// 被截短，在操作系统看来**都是干净的 EOF**——OS 无法表达「本该还有数据」。
    /// 只有声明了预期长度的协议才能发现缺斤少两，故这里起一个假 HTTP 服务：
    /// 响应头写完整的 `Content-Length`，实际只发 1/4 就断开。
    ///
    /// 注意这里走 [`FfmpegSource::from_input_with`] 而非 `open`：`open` 只接受
    /// 存在的本地路径，这是有意的约束，不该为了测试去放宽它。
    #[test]
    fn read_error_surfaces_instead_of_looking_like_eof() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let Some(data) = sample_as_mpegts() else {
            eprintln!("跳过：ffmpeg 命令不可用，无法生成 TS");
            return;
        };
        let total = data.len();
        // 只发一小部分就断开，确保失败发生在「读到一半」而非一开始。
        let cut = total / 4;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            // 只服务一个连接就退出。用 incoming() 的第一个即可。
            if let Some(Ok(mut stream)) = listener.incoming().next() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n\
                     Content-Type: video/mp2t\r\nAccept-Ranges: none\r\n\r\n"
                );
                if stream.write_all(hdr.as_bytes()).is_ok() {
                    let _ = stream.write_all(&data[..cut]);
                    // 承诺了 total 字节却只发 cut 就关闭：http 协议层会发现
                    // 「还差一大截」，报连接中断而非当成干净 EOF。
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        });

        let url = format!("http://{addr}/sample.ts");

        // 解码放子线程 + 超时：错误被吞掉时的症状是**死循环空转**，
        // 直接在测试线程跑会把 CI 挂死。看门狗把「卡死」转成明确的失败信息。
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let verdict = (|| {
                ffmpeg_next::init().map_err(|e| format!("ffmpeg init 失败: {e}"))?;
                let input = ffmpeg_next::format::input(&url)
                    .map_err(|e| format!("截断的 TS 应该能正常打开，却失败了: {e}"))?;
                let mut src = FfmpegSource::from_input_with(input, None)
                    .map_err(|e| format!("构造失败: {e}"))?;

                let mut count = 0u32;
                loop {
                    match src.next_frame() {
                        Ok(Some(_)) => count += 1,
                        Ok(None) => {
                            return Err(format!(
                                "读取被中断却报告成正常 EOF（{count} 帧后），错误被吞了"
                            ));
                        }
                        Err(_) => return Ok(count), // 期望路径：错误如实上报
                    }
                    if count > 1_000 {
                        return Err("解出的帧数远超样本总量，疑似死循环".into());
                    }
                }
            })();
            let _ = tx.send(verdict);
        });

        let verdict = rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| Err("解码线程 30s 未返回：错误很可能被吞掉后陷入空转".into()));
        let _ = server.join();

        match verdict {
            Ok(count) => assert!(count < 300, "连接中途就断了，不该解出完整的 {count} 帧"),
            Err(msg) => panic!("{msg}"),
        }
    }
}
