//! player-core 的集成测试：用真实视频样本验证解码链路。
//!
//! 样本：tests/assets/sample.mp4（H.264 480x270 / 10s / 30fps / AAC 单声道 48kHz）。
//! 刻意做小（~250KB）：仓库是公开的，测试素材只需覆盖解码链路，
//! 不需要高分辨率或长时长。参数都取整数，断言才好写。
//! 保留音轨是有意的——将来做 A/V 同步时需要它，去掉就测不了。
//! 运行：cargo test -p player-core --test decode

use player_core::{FfmpegSource, MediaSource};
use std::path::PathBuf;

/// 测试样本： (文件名, 期望宽, 期望高, 期望时长秒)
const SAMPLES: &[(&str, u32, u32, f64)] = &[("sample.mp4", 480, 270, 10.0)];

/// 样本时长（秒），供 seek 等测试推导安全的目标位置。
const SAMPLE_DURATION_SECS: u64 = 10;

fn sample_path(name: &str) -> PathBuf {
    // cargo 集成测试运行时，crate 根目录即当前工作目录。
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/assets")
        .join(name)
}

#[test]
fn open_reports_correct_video_info() {
    for (name, w, h, dur) in SAMPLES {
        let path = sample_path(name);
        assert!(path.exists(), "sample video missing: {}", path.display());

        let src = FfmpegSource::open(&path).expect("open failed");
        let info = src.video_info();

        assert_eq!(info.width, *w, "[{name}] width mismatch");
        assert_eq!(info.height, *h, "[{name}] height mismatch");
        // 时长允许 ±1s 误差（容器/流时长口径差异）。
        assert!(
            (info.duration.as_secs_f64() - dur).abs() < 1.0,
            "[{name}] duration off: {}s",
            info.duration.as_secs_f64()
        );
        assert!(info.fps > 0.0, "[{name}] fps should be > 0");
    }
}

/// 样本必须带音轨。这条测试守的是**素材本身**而非代码：
/// 素材若被重新生成时漏了 `-c:a`，音频链路的测试会静默失去覆盖，
/// 等做 A/V 同步时才发现就晚了。
#[test]
fn sample_has_audio_stream() {
    for (name, _, _, _) in SAMPLES {
        let path = sample_path(name);
        ffmpeg_next::init().expect("ffmpeg init failed");
        let input = ffmpeg_next::format::input(&path).expect("open failed");
        let has_audio = input
            .streams()
            .any(|s| s.parameters().medium() == ffmpeg_next::media::Type::Audio);
        assert!(
            has_audio,
            "[{name}] 样本缺少音轨，请用带 -c:a aac 的命令重新生成"
        );
    }
}

#[test]
fn next_frame_decodes_bgra_with_correct_dimensions() {
    for (name, w, h, _) in SAMPLES {
        let path = sample_path(name);
        let mut src = FfmpegSource::open(&path).expect("open failed");

        let frame = src
            .next_frame()
            .expect("next_frame errored")
            .expect("expected at least one frame");

        assert_eq!(frame.width, *w, "[{name}] width");
        assert_eq!(frame.height, *h, "[{name}] height");
        // BGRA: 每行至少 width*4 字节。
        assert!(
            frame.stride >= frame.width as usize * 4,
            "[{name}] stride too small"
        );
        // 含 stride 填充的总长度。
        assert_eq!(
            frame.data.len(),
            frame.stride * frame.height as usize,
            "[{name}] data len"
        );
        // 首帧不应是全零（否则解码/转换有问题）。
        assert!(
            frame.data.iter().any(|&b| b != 0),
            "[{name}] frame is all zeros"
        );
    }
}

#[test]
fn decode_several_frames_increases_pts() {
    for (name, _, _, _) in SAMPLES {
        let path = sample_path(name);
        let mut src = FfmpegSource::open(&path).expect("open failed");

        let mut prev_pts = None;
        let mut count = 0;
        while let Some(frame) = src.next_frame().expect("decode error") {
            if let Some(prev) = prev_pts {
                // 逐帧 PTS 应不减（允许相等，但不回退）。
                assert!(frame.pts >= prev, "[{name}] PTS went backwards");
            }
            prev_pts = Some(frame.pts);
            count += 1;
            if count >= 30 {
                break; // 只验前 30 帧，避免测试太慢。
            }
        }
        assert!(count >= 30, "[{name}] decoded fewer than 30 frames");
    }
}

#[test]
fn seek_then_decode_returns_frames_near_target() {
    for (name, _, _, _) in SAMPLES {
        let path = sample_path(name);
        let mut src = FfmpegSource::open(&path).expect("open failed");

        // 目标取中点：样本只有 10s，seek 到 10s 就是文件末尾，测不出东西。
        let target = std::time::Duration::from_secs(SAMPLE_DURATION_SECS / 2);
        src.seek(target).expect("seek failed");

        // seek 落到最近的关键帧。样本每 1s 一个关键帧（生成时 -g 30 固定 GOP），
        // 故容差取 1.5s 足够——容差过松会掩盖真实的 seek 缺陷：
        // 最初用 12s 容差时，seek 落回 0s 这种明显错误都能"通过"。
        let frame = src
            .next_frame()
            .expect("decode error")
            .expect("no frame after seek");
        let delta = (frame.pts.as_secs_f64() - target.as_secs_f64()).abs();
        assert!(
            delta < 1.5,
            "[{name}] seek landed too far: {}s (target {}s)",
            frame.pts.as_secs_f64(),
            target.as_secs_f64()
        );
    }
}
#[test]
fn decode_to_end_returns_none_without_panic() {
    for (name, _, _, _) in SAMPLES {
        let path = sample_path(name);
        let mut src = FfmpegSource::open(&path).expect("open failed");

        // 一路解到末尾，确认最终返回 Ok(None) 而非把 EOF 当错误 panic。
        let mut count = 0;
        loop {
            match src.next_frame() {
                Ok(Some(_)) => count += 1,
                Ok(None) => break, // 正常结束
                Err(e) => panic!("[{name}] decode error at frame {count}: {e}"),
            }
            if count > 1_000 {
                panic!("[{name}] decoded too many frames, loop?");
            }
        }
        // 10s @ 30fps ≈ 300 帧。给宽松区间即可，重点是"解完且数量合理"，
        // 而非精确值（末尾可能因 GOP 边界差几帧）。
        assert!(
            (280..=320).contains(&count),
            "[{name}] expected ~300 frames, got {count}"
        );
    }
}
