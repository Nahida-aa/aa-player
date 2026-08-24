use super::*;
use std::time::Instant;

/// 测试进程安装一次 tracing 订阅者：没有它，解码线程/音频回调里的
/// WARN（欠载、流错误）会被静默丢弃，冒烟测试就成了"只看红绿不看病情"。
/// RUST_LOG 可调级别，默认 warn 起步（info 太吵）。
fn init_tracing() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_test_writer()
            .init();
    });
}

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
    init_tracing();
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
    init_tracing();
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
    init_tracing();
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

/// 冒烟：暂停应停止产帧，恢复后应继续（解码侧语义；渲染侧的暂停闸门在
/// GPUI 异步任务里，无法无头测试，这里守住通道层不变量：Pause 生效后
/// 通道必须归于静默，Resume 后重新流动）。
#[test]
fn pause_stops_frame_flow_then_resume_resumes() {
    init_tracing();
    let (mut controller, mut rx) = PlayerController::open(sample_path());

    // 播一会儿。
    let mut got = 0;
    while got < 20 {
        if try_recv_frame(&mut rx, Duration::from_secs(5)) {
            got += 1;
        } else {
            break;
        }
    }
    assert!(got >= 20, "应解出至少 20 帧，实际 {got}");

    // 暂停：先排干命令生效前在途的最后几帧，然后确认通道静默。
    controller.pause();
    while try_recv_frame(&mut rx, Duration::from_millis(250)) {}
    assert!(
        !try_recv_frame(&mut rx, Duration::from_millis(600)),
        "暂停生效后不应继续产帧"
    );

    // 恢复：重新流动。
    controller.play();
    let mut got_after = 0;
    while got_after < 5 {
        if try_recv_frame(&mut rx, Duration::from_secs(5)) {
            got_after += 1;
        } else {
            break;
        }
    }
    assert!(got_after >= 5, "恢复后应继续产帧，实际 {got_after}");
}

/// 回归：**暂停中向过去跳转、再恢复播放，音频时钟必须重新走起来**。
///
/// 病灶有两层（缺一不可，都由本测试守住）：
/// 1. 泵在门闸丢弃期失控：暂停 scrub 期间每块音频入队前就被丢，
///    队列读数恒 0，泵的 `queued < AUDIO_BUFFER` 条件永不满足——
///    以解码速度把整条剩余音轨拉完丢光直到 EOF（10s 样本实测 469 块）。
///    恢复播放时解码器已见底，静音到文件尾。修法：门闸丢弃期停泵。
/// 2. Resume 绕过起播协议：暂停中 commit 的流是 new_paused 建的
///    （未开播、时钟零），Resume 原来只调 resume() 解冻设备，
///    start_audio 停留在 false，永远等不到 try_start_audio 放行。
///    修法：Resume 重置 scrub_paused 并置 start_audio（拖动预览除外）。
#[test]
fn paused_seek_backward_then_play_keeps_audio_clock_running() {
    init_tracing();
    let (mut controller, mut rx) = PlayerController::open(sample_path());

    // 无声卡环境（容器/无设备机器）：没有音频时钟可断言，跳过。
    // 注意竞态：解码线程要解出第一块音频才 attach 时钟，open 后立刻查
    // 必然 None——必须轮询等 attach，否则测试会静默空跑成假绿。
    let mut attached = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if controller.clock.get_with_generation().0 > 0 {
            attached = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(attached, "音频时钟未 attach：无声卡或解码线程卡住");

    // 播一会儿，让位置离开文件头部（向后跳才成立）。
    let mut got = 0;
    while got < 30 {
        if try_recv_frame(&mut rx, Duration::from_secs(5)) {
            got += 1;
        } else {
            break;
        }
    }
    assert!(got >= 30, "应解出至少 30 帧，实际 {got}");

    // 暂停并确认静默。
    controller.pause();
    while try_recv_frame(&mut rx, Duration::from_millis(250)) {}
    assert!(
        !try_recv_frame(&mut rx, Duration::from_millis(400)),
        "暂停生效后不应继续产帧"
    );

    // 暂停中向后跳（对齐用户操作：点击进度条 = 直接 SeekCommit）。
    let gen_before_seek = controller.clock.get_with_generation().0;
    controller.seek_to(Duration::from_millis(200));
    // commit 的 scrub 目标帧应送达（本测试读通道本身，不经过渲染闸门）。
    assert!(
        try_recv_frame(&mut rx, Duration::from_secs(5)),
        "暂停中 commit 应解出目标帧"
    );

    // 恢复播放：音频时钟应换代（重建流）并持续前进到可感知量。
    controller.play();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut progressed = false;
    let mut frames_seen = 0u32;
    while Instant::now() < deadline {
        // 边观察边消费视频帧：帧通道若被塞满，解码线程会阻塞在
        // send_blocking 上，音频投递随之停摆（测试环境没有渲染循环兜底）。
        while rx.try_recv().map(|f| f.is_some()).unwrap_or(false) {
            frames_seen += 1;
        }
        let (generation, _, audio) = controller.clock.get_with_generation();
        assert!(generation > gen_before_seek, "seek 后音频时钟应换代");
        if let Some(a) = audio.as_ref() {
            if a.position() >= Duration::from_millis(150) {
                progressed = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        progressed,
        "恢复播放后音频时钟应前进到 ≥150ms（起播协议收编 + 泵停投回归点；期间消费了 {frames_seen} 帧）"
    );
}
