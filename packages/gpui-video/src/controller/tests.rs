use super::*;
use std::time::Instant;

/// player-core 的内置样本视频（含音轨）。
fn sample_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../player-core/tests/assets/sample.mp4")
}

/// 轮询读一帧，直到拿到一帧或超时。返回是否拿到。
fn try_recv_frame(rx: &mut mpsc::Receiver<FrameMsg>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match rx.try_recv() {
            Ok(Some(_)) => return true,
            _ => {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// 打开控制器，消费一批帧，触发一次拖动 seek（preview + release），
/// 验证 seek 后**解码线程不卡死、持续产帧**（对齐之前"拖动后画面/进度条
/// 不动、声音继续"的 bug——seek 后解码必须继续，不能停）。
#[test]
fn seek_preview_release_keeps_frames_flowing() {
    let (mut controller, mut rx) = PlayerController::open(sample_path());

    // 1) 消费若干帧（解码线程在跑）。
    let mut got = 0;
    while got < 30 {
        if try_recv_frame(&mut rx, Duration::from_secs(5)) {
            got += 1;
        } else {
            break;
        }
    }
    assert!(got >= 30, "应解出至少 30 帧，实际 {got}");

    // 2) 拖动 seek（preview 拖动中 + release 提交）。
    controller.seek_preview(Duration::from_secs(2));
    controller.seek_release(Duration::from_secs(2));

    // 3) seek 后继续产帧（解码不卡死）。
    let mut got_after = 0;
    while got_after < 10 {
        if try_recv_frame(&mut rx, Duration::from_secs(5)) {
            got_after += 1;
        } else {
            break;
        }
    }
    assert!(
        got_after >= 10,
        "seek 后应持续产帧（解码不卡死），实际 {got_after}"
    );
}

/// 模拟**快速拖动**：短时间内快速连续发几十次 seek_preview（不同目标，
/// 模拟鼠标快速来回拖），最后 seek_release。验证 seek 覆盖合并 + 抢占
/// seek 下解码线程不卡死、seek 后持续产帧（对齐之前"快速拖动画面卡住"）。
#[test]
fn rapid_drag_does_not_stall_decode() {
    let (mut controller, mut rx) = PlayerController::open(sample_path());

    // 1) 消费若干帧（解码线程在跑）。
    let mut got = 0;
    while got < 20 {
        if try_recv_frame(&mut rx, Duration::from_secs(5)) {
            got += 1;
        } else {
            break;
        }
    }
    assert!(got >= 20, "应解出至少 20 帧，实际 {got}");

    // 2) 快速拖动：1 秒内快速发 50 次 seek_preview，目标在 0..2.5s 之间递增
    //    （模拟快速向右拖），紧接着 seek_release 提交到最终位置。
    for i in 0..50 {
        let t = Duration::from_millis((i * 50) as u64);
        controller.seek_preview(t);
        // 不 sleep：命令在 unbounded channel 堆积，由解码线程覆盖合并，
        // 模拟"拖动比解码线程处理还快"的最坏情况。
    }
    controller.seek_release(Duration::from_millis(2500));

    // 3) 快速拖动结束后应持续产帧（解码线程合并 seek 后不卡死）。
    let mut got_after = 0;
    while got_after < 10 {
        if try_recv_frame(&mut rx, Duration::from_secs(10)) {
            got_after += 1;
        } else {
            break;
        }
    }
    assert!(
        got_after >= 10,
        "快速拖动后应持续产帧（解码不卡死），实际 {got_after}"
    );
}

/// 回归测试：**按住 thumb 不松手、不滑动**（进入拖动预览但不再发新命令）。
///
/// 修复前，Preview 模式下解码线程会继续往解码，预览帧以解码速度一帧帧快进，
/// 画面看起来在自动快播（无声音）。修复后：预览解出目标帧后应**定格**，
/// 不继续产帧（画面停住），直到下一个命令（新 Preview 或 Release 的 Commit）。
#[test]
fn preview_holds_still_freezes_not_fast_forward() {
    let (mut controller, mut rx) = PlayerController::open(sample_path());

    // 1) 消费若干帧（解码线程在跑）。
    let mut got = 0;
    while got < 30 {
        if try_recv_frame(&mut rx, Duration::from_secs(5)) {
            got += 1;
        } else {
            break;
        }
    }
    assert!(got >= 30, "应解出至少 30 帧，实际 {got}");

    // 2) 进入拖动预览：拖动开始（静音）+ 一个 Preview seek 到 4s，然后**停住**，
    //    模拟"点住 thumb 不松手不滑动"。
    controller.mute_audio();
    controller.seek_preview(Duration::from_secs(4));

    // 3) 等预览目标帧送达（画面应该跳过去）。
    assert!(
        try_recv_frame(&mut rx, Duration::from_secs(5)),
        "Preview 应解出目标帧"
    );
    // 再消费掉紧随的 1~2 帧（seek 落点可能先送关键帧前的帧）。
    while got < 34 {
        if try_recv_frame(&mut rx, Duration::from_millis(200)) {
            got += 1;
        } else {
            break;
        }
    }

    // 4) **关键**：此后不再发任何命令（继续"按住不松"），短窗口内不应再有
    //    新帧 —— 画面必须定格，否则就是快进 bug。
    let mut leaked = 0;
    let window = Duration::from_millis(300);
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        if try_recv_frame(&mut rx, Duration::from_millis(50)) {
            leaked += 1;
        }
    }
    assert!(
        leaked == 0,
        "按住不滑动时预览应定格（不应再产帧/快进），300ms 内泄漏了 {leaked} 帧"
    );

    // 5) 松开（Commit）：恢复正常播放，应重新持续产帧。
    controller.seek_release(Duration::from_secs(4));
    let mut got_after = 0;
    while got_after < 5 {
        if try_recv_frame(&mut rx, Duration::from_secs(5)) {
            got_after += 1;
        } else {
            break;
        }
    }
    assert!(got_after >= 5, "松开后应恢复产帧，实际 {got_after}");
}
