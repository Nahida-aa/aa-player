//! player-core —— 视频播放器的无 GUI 核心。
//!
//! 只放与界面无关的逻辑：媒体加载、解码、帧抽象。GUI（GPUI）放在
//! `player-app` 里，通过依赖本 crate 复用这些能力；未来 ocr-lab 等其它项目
//! 也可以 `cargo add player-core` 在后台驱动视频，而不必引入窗口/渲染栈。
//!
//! 当前范围：单视频流解码 + 逐帧拉取（BGRA）+ 运行时 seek。
//! 待做（下一轮）：播放时钟、音视频同步、音频解码、线程管线。

pub mod audio_decoder;
pub mod audio_output;
pub mod error;
pub mod ffmpeg_log;
pub mod frame;
pub mod media_source;

pub use audio_decoder::{AudioDecoder, AudioInfo};
pub use audio_output::{AudioClock, AudioFormat, AudioOutput};
pub use error::Result;
pub use frame::{AudioChunk, DecodedFrame, VideoInfo};
pub use media_source::{FfmpegSource, MediaEvent, MediaSource, SeekCancelled};
