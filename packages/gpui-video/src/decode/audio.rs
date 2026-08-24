//! 声卡侧音频投递：续杯泵与 next_event 共用的入队语义。
//!
//! 从父模块拆出。核心是 [`deliver_audio`]：seek 过滤、scrub 静音、背压、
//! 欠载检测四件事的唯一实现，保证「正常路径」与「视频投递前的续杯泵」
//! 两条路径语义完全一致。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use player_core::{AudioChunk, AudioOutput};

use crate::controller::AudioClockSource;

use super::probe;

/// 声卡队列里最多缓冲多少音频。超过就先别解，形成背压。
pub(super) const AUDIO_BUFFER: Duration = Duration::from_millis(400);
/// seek 重建声卡流后，至少缓冲这么多音频才允许 `start()`。
const AUDIO_START_MIN: Duration = Duration::from_millis(80);
/// 音频缓冲满时的退避间隔。
const AUDIO_BACKOFF: Duration = Duration::from_millis(5);
/// 音频缓冲满足条件（≥ AUDIO_START_MIN）就 `start()`。
pub(super) fn try_start_audio(audio: &Option<AudioOutput>, start_audio: &mut bool) {
    if !*start_audio {
        return;
    }
    let Some(a) = audio.as_ref() else { return };
    if a.queued_duration() >= AUDIO_START_MIN {
        *start_audio = false;
        a.start();
    }
}
/// 声卡硬件时钟不能倒带：seek 后重建流（计数器归零、先不启动），
/// 等缓冲填够再 start，再把新时钟句柄交回渲染侧。
pub(super) fn seek_rebuild_audio(audio: &mut Option<AudioOutput>, clock_source: &Arc<AudioClockSource>) {
    *audio = match AudioOutput::new_paused() {
        Ok(o) => Some(o),
        Err(e) => {
            tracing::warn!(?e, "seek 后重开音频设备失败，将以无声模式播放");
            None
        }
    };
    if let Some(a) = audio.as_ref() {
        clock_source.attach(a.clock());
    }
}
/// 等声卡把缓冲里剩下的采样播完（结尾不掐音）。
pub(super) fn drain_audio(audio: &AudioOutput, running: &AtomicBool) {
    if audio.is_paused() {
        audio.start();
    }
    while running.load(Ordering::Relaxed) && audio.queued_frames() > 0 {
        std::thread::sleep(AUDIO_BACKOFF);
    }
}
/// 把一条音频事件按当前模式入队。
///
/// 「next_event 的 Audio 分支」与「视频投递前的音频续杯泵」共用，
/// 保证两条路径的语义完全一致：
/// - seek 后丢弃目标前音频（避免旧位置声音/时钟超前）；
/// - 暂停中 scrub 只解视频预览帧，不推音频（声卡已 pause 冻结，推入只会堆积）；
/// - 背压：缓冲够深就等，别把整轨解进内存。仅 arm 路径（`backpressure=true`）
///   启用——泵路径靠调用方的循环条件控水位，睡在这里会把主循环拖成
///   声卡节奏（实测视频掉到 1 帧/2s）；seek 后音频是暂停态或拖动预览，
///   队列不被消费，此时也不背压；
/// - 欠载检测：声卡回调发现队列空会置位，这里取走并告警。
#[allow(clippy::too_many_arguments)]
pub(super) fn deliver_audio(
    chunk: AudioChunk,
    audio: &Option<AudioOutput>,
    audio_seek_target: &mut Option<Duration>,
    scrub_paused: bool,
    start_audio: bool,
    previewing: bool,
    backpressure: bool,
    running: &AtomicBool,
) {
    if let Some(target) = *audio_seek_target {
        if chunk.pts < target {
            probe::audio_dropped("seek_target");
            return;
        }
        *audio_seek_target = None;
    }
    if scrub_paused {
        probe::audio_dropped("scrub_paused");
        return;
    }
    let Some(a) = audio else { return };
    if backpressure && !start_audio && !previewing {
        while running.load(Ordering::Relaxed) && a.queued_duration() > AUDIO_BUFFER {
            std::thread::sleep(AUDIO_BACKOFF);
        }
    }
    a.push_samples(&chunk.samples);
    probe::audio_chunk(a.position(), a.queued_duration());
    let fmt = a.format();
    probe::audio_pushed(
        chunk.samples.len(),
        fmt.channels,
        fmt.sample_rate,
        a.queued_duration(),
    );
    if a.take_underrun() {
        tracing::warn!(
            queued_ms = a.queued_duration().as_millis() as u64,
            "音频欠载：解码跟不上声卡消费"
        );
    }
}
