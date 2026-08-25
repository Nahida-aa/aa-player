//! seek 后音频收敛帧丢弃（vlc 同款）的行为守护。
//!
//! AAC（尤其 HE-AACv2 SBR/PS）中途进入的前几帧是劣化输出，`seek()` 后
//! 前 [`AUDIO_CONVERGENCE_FRAMES`] 个解码帧应被静默丢弃。
//!
//! 注意语义：丢弃的是**进入点**的收敛帧，不是「目标线之前」的帧——
//! demux seek 的落点在目标附近（可能略早），跨线块照常保留（不留空洞）。
//! 因此可观测契约是「预算被 seek 重置、被解码消费」，而非「首块 pts
//! 必然 ≥ 目标」。

use std::path::PathBuf;
use std::time::Duration;

use player_core::media_source::{AUDIO_CONVERGENCE_FRAMES, FfmpegSource};
use player_core::{AudioFormat, MediaEvent, MediaSource};

fn sample_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/assets/sample.mp4")
}

fn device_format() -> AudioFormat {
    AudioFormat {
        sample_rate: 48_000,
        channels: 2,
    }
}

#[test]
fn seek_resets_budget_and_decoding_consumes_it() {
    let mut src = FfmpegSource::open_with(&sample_path(), Some(device_format())).expect("open");

    // 顺序解几拍（消耗掉任何状态），再 seek：预算应重置为满额。
    for _ in 0..30 {
        if src.next_event().expect("next_event").is_none() {
            break;
        }
    }
    src.seek(Duration::from_secs(2)).expect("seek");
    assert_eq!(
        src.audio_convergence_budget(),
        AUDIO_CONVERGENCE_FRAMES,
        "seek 应重置收敛帧预算"
    );

    // 驱动解码：预算应被前几个音频帧消费到 0，之后保持 0（正常播放
    // 零开销），且音频流连续不挖洞（PTS 单调）。
    let mut audio_pts = Vec::new();
    let mut consumed_seen = false;
    for _ in 0..500 {
        match src.next_event().expect("next_event") {
            Some(MediaEvent::Audio(c)) => {
                audio_pts.push(c.pts);
                let budget = src.audio_convergence_budget();
                if !consumed_seen && budget == 0 {
                    consumed_seen = true;
                    assert!(
                        audio_pts.len() <= AUDIO_CONVERGENCE_FRAMES as usize + 1,
                        "预算应在最初几帧内耗尽，实际已交付 {} 块",
                        audio_pts.len()
                    );
                }
            }
            Some(MediaEvent::Video(_)) => {}
            None => break,
        }
        if audio_pts.len() >= 8 {
            break;
        }
    }
    assert!(consumed_seen, "seek 后应观察到预算被消费");
    assert_eq!(src.audio_convergence_budget(), 0);
    for w in audio_pts.windows(2) {
        assert!(w[1] > w[0], "音频 PTS 应单调递增（丢弃不挖洞）");
    }
}

#[test]
fn arm_discards_does_not_reset_budget() {
    // commit 复用预览 seek 路径：arm_discards 只挂丢弃线，**不**重置预算
    // ——预览那次 seek 已经丢过收敛帧，再丢就是凭空挖洞。
    let mut src = FfmpegSource::open_with(&sample_path(), Some(device_format())).expect("open");
    src.seek(Duration::from_secs(2)).expect("seek");
    // 预算消费到 0
    while src.audio_convergence_budget() > 0 {
        match src.next_event().expect("next_event") {
            Some(_) => {}
            None => break,
        }
    }
    src.arm_discards(Duration::from_secs(3));
    assert_eq!(
        src.audio_convergence_budget(),
        0,
        "arm_discards 不应重置预算"
    );
}

#[test]
fn linear_play_from_zero_is_not_affected_by_convergence_drop() {
    // 预算初始为 0：顺序播放不应丢任何帧——首块起点仍在开头附近。
    let mut src = FfmpegSource::open_with(&sample_path(), Some(device_format())).expect("open");
    let mut first_audio: Option<Duration> = None;
    for _ in 0..200 {
        match src.next_event().expect("next_event") {
            Some(MediaEvent::Audio(c)) => {
                first_audio = Some(c.pts);
                break;
            }
            Some(MediaEvent::Video(_)) => {}
            None => break,
        }
    }
    let pts = first_audio.expect("顺序播放应有音频");
    assert!(
        pts < Duration::from_millis(500),
        "顺序播放的首块音频应在文件开头附近，实际 {pts:?}"
    );
}
