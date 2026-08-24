//! 功能级冒烟测试：驱动完整控制器（解码线程 + 音频设备 + 时钟），
//! 守护用户可感知的行为不变量。区别于 src 内白盒单元测试：
//! 这里只走公开 API（黑盒），允许整体搬迁、重排而不破坏封装。
//!
//! 测试素材（按优先级）：
//! 1. 仓库根目录 `.env` 的 `VIDEO=`（与 justfile 惯例同款，本地大视频
//!    不进仓库；改这一个文件即可让全部冒烟跑在真实素材上）
//! 2. 环境变量 `AA_PLAYER_SMOKE_VIDEO`（CI/临时覆盖）
//! 3. player-core 自带样本（10s，含音轨）
//! 绝对时间目标在素材较短时由解码侧 seek_clamped 兜底；
//! 「向过去跳转」场景要求素材 ≥1s，过短自动跳过。
//!
//! 日志：init_tracing 装订阅者后，解码线程的 WARN（欠载/流错误）
//! 才能浮出——红绿之外还要看病情。

use futures::channel::mpsc;
use gpui_video::controller::FrameMsg;
use gpui_video::PlayerController;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 测试进程安装一次 tracing 订阅者：没有它，WARN 会被静默丢弃，
/// 冒烟测试就成了"只看红绿不看病情"。RUST_LOG 可调级别，默认 warn。
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

/// 冒烟素材路径（优先级见模块文档）：`.env` 的 VIDEO >
/// `AA_PLAYER_SMOKE_VIDEO` > 内置样本。
///
/// 手写解析而非引 dotenv 依赖：只读一个键，不值得为此加 dev-dep。
fn sample_path() -> PathBuf {
    let p = dotenv_video()
        .or_else(|| std::env::var_os("AA_PLAYER_SMOKE_VIDEO").map(PathBuf::from))
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../player-core/tests/assets/sample.mp4")
        });
    assert!(
        p.exists(),
        "冒烟素材不存在：{}（可在仓库根目录 .env 写 VIDEO=/path/to/video.mp4）",
        p.display()
    );
    p
}

/// 读仓库根 `.env` 的 `VIDEO=` 键（存在且非空才生效）。
fn dotenv_video() -> Option<PathBuf> {
    // CARGO_MANIFEST_DIR = packages/gpui-video，仓库根在两级之上。
    let env_file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.env")
        .canonicalize()
        .ok()?;
    let content = std::fs::read_to_string(env_file).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("VIDEO=") {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    None
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

/// 从帧流里读出素材时长（FrameMsg 第 3 元素）。
///
/// 黑盒测试**不能信 `controller.duration()`**：它由控制器的帧消费路径
/// （真实 UI 的渲染循环）更新，这里直接抽通道原始帧，控制器侧恒为 0。
fn observed_duration(rx: &mut mpsc::Receiver<FrameMsg>) -> Duration {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match rx.try_recv() {
            Ok(Some((_, _, dur_us, _, _))) => {
                return Duration::from_micros(dur_us);
            }
            _ => {
                if Instant::now() >= deadline {
                    panic!("5s 内没收到任何帧，无法得知素材时长");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// 排干当前积压的帧（不等待）。
fn drain_frames(rx: &mut mpsc::Receiver<FrameMsg>) -> u32 {
    let mut n = 0;
    while rx.try_recv().map(|f| f.is_some()).unwrap_or(false) {
        n += 1;
    }
    n
}

/// 消费至少 `min` 帧（解码线程在跑的基本前提）。
fn consume_frames(rx: &mut mpsc::Receiver<FrameMsg>, min: usize, timeout: Duration) -> usize {
    let mut got = drain_frames(rx) as usize;
    let deadline = Instant::now() + timeout;
    while got < min && Instant::now() < deadline {
        if try_recv_frame(rx, Duration::from_millis(100)) {
            got += 1;
        }
    }
    got
}

/// 验证 seek 后**解码线程不卡死、持续产帧**（对齐之前"拖动后画面/进度条
/// 不动、声音继续"的 bug——seek 后解码必须继续，不能停）。
#[test]
fn seek_preview_release_keeps_frames_flowing() {
    init_tracing();
    let (mut controller, mut rx) = PlayerController::open(sample_path());

    // 1) 消费若干帧（顺带让时长信息随首帧送达）。
    let got = consume_frames(&mut rx, 30, Duration::from_secs(5));
    assert!(got >= 30, "应解出至少 30 帧，实际 {got}");

    // 2) 拖动 seek（preview 拖动中 + release 提交）。中间 sleep 模拟
    //    **真人按压窗**（~120ms）：此前背靠背发送会被解码线程合并成
    //    一次，测不到真实点击的断续窗口——设备在预览清队后会饿十几个
    //    回调，宽限逻辑必须全程吞掉（否则每次点击都误报欠载 WARN）。
    controller.seek_preview(Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(120));
    controller.seek_release(Duration::from_secs(2));

    // 3) seek 后继续产帧（解码不卡死）。
    let got_after = consume_frames(&mut rx, 10, Duration::from_secs(5));
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
    let got = consume_frames(&mut rx, 20, Duration::from_secs(5));
    assert!(got >= 20, "应解出至少 20 帧，实际 {got}");

    // 2) 快速拖动：50 次 seek_preview 目标递增（模拟快速向右拖），紧接着
    //    seek_release 提交到最终位置。目标上限取素材可容纳的范围
    //    （自定义短素材时由解码侧 seek_clamped 再兜一层底）。
    let max_t = observed_duration(&mut rx).min(Duration::from_millis(2500));
    for i in 0..50 {
        let t = max_t.mul_f64(i as f64 / 50.0);
        controller.seek_preview(t);
        // 不 sleep：命令在 unbounded channel 堆积，由解码线程覆盖合并，
        // 模拟"拖动比解码线程处理还快"的最坏情况。
    }
    controller.seek_release(max_t);

    // 3) 快速拖动结束后应持续产帧（解码线程合并 seek 后不卡死）。
    let got_after = consume_frames(&mut rx, 10, Duration::from_secs(10));
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
    let got = consume_frames(&mut rx, 30, Duration::from_secs(5));
    assert!(got >= 30, "应解出至少 30 帧，实际 {got}");

    // 2) 进入拖动预览：拖动开始（静音）+ 一个 Preview seek 到目标位置，然后
    //    **停住**，模拟"点住 thumb 不松手不滑动"。目标取素材中段。
    let target = (observed_duration(&mut rx).min(Duration::from_secs(4)) / 2)
        .max(Duration::from_secs(1));
    controller.mute_audio();
    controller.seek_preview(target);

    // 3) 等预览目标帧送达（画面应该跳过去）。
    assert!(
        try_recv_frame(&mut rx, Duration::from_secs(5)),
        "Preview 应解出目标帧"
    );

    // 4) **关键**：不再发任何命令（继续"按住不松"）。预览允许越过目标再快进
    //    PREVIEW_CREEP(350ms) 的帧——这是拖动顺滑感的来源——然后必须定格。
    //    断言两点：(a) 快进有界：泄漏帧的 pts 不超过目标 + 余量 + 容差；
    //    (b) 最终定格：静默窗口内不再有新帧（否则就是无限快播 bug）。
    let mut leaked = 0u32;
    let mut last_pts_us = 0u64;
    let mut quiet = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(Some((_, pts_us, _, _, _))) => {
                leaked += 1;
                last_pts_us = last_pts_us.max(pts_us);
            }
            _ => {
                if try_recv_frame(&mut rx, Duration::from_millis(400)) {
                    continue;
                }
                quiet = true;
                break;
            }
        }
    }
    assert!(quiet, "预览必须最终定格，不应无限快进");
    let overshoot_ms = last_pts_us.saturating_sub(target.as_micros() as u64) / 1000;
    assert!(
        overshoot_ms <= 600,
        "按住不动时预览快进应有界（≤目标+350ms+容差），实际越过目标 {overshoot_ms}ms"
    );
    assert!(leaked > 0 || target.as_millis() < 100, "应观察到有界的预览流动");

    // 5) 松开（Commit）：恢复正常播放，应重新持续产帧。
    controller.seek_release(target);
    let got_after = consume_frames(&mut rx, 5, Duration::from_secs(5));
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
    let got = consume_frames(&mut rx, 20, Duration::from_secs(5));
    assert!(got >= 20, "应解出至少 20 帧，实际 {got}");

    // 暂停：先排干命令生效前在途的最后几帧，然后确认通道静默。
    controller.pause();
    loop {
        let drained = drain_frames(&mut rx);
        if drained == 0 && !try_recv_frame(&mut rx, Duration::from_millis(250)) {
            break;
        }
    }
    assert!(
        !try_recv_frame(&mut rx, Duration::from_millis(600)),
        "暂停生效后不应继续产帧"
    );

    // 恢复：重新流动。
    controller.play();
    let got_after = consume_frames(&mut rx, 5, Duration::from_secs(5));
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

    // 播一会儿，让位置离开文件头部（向后跳才成立）。素材过短则没有
    // 「过去」可跳，跳过本场景。时长从帧流读（黑盒不信 controller.duration，
    // 它由真实 UI 的帧消费路径更新，这里恒为 0）。
    let got = consume_frames(&mut rx, 30, Duration::from_secs(5));
    assert!(got >= 30, "应解出至少 30 帧，实际 {got}");
    if observed_duration(&mut rx) < Duration::from_secs(1) {
        eprintln!("素材不足 1s，无法构造向后跳转，跳过");
        return;
    }

    // 暂停并确认静默。
    controller.pause();
    loop {
        if drain_frames(&mut rx) == 0 && !try_recv_frame(&mut rx, Duration::from_millis(250)) {
            break;
        }
    }
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
    // 边观察边消费视频帧：帧通道容量只有 FRAME_QUEUE_CAP=12，测试没有
    // 渲染循环兜底，不消费会让解码线程阻塞在 send_blocking 上。
    controller.play();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut progressed = false;
    let mut frames_seen = 0u32;
    while Instant::now() < deadline {
        frames_seen += drain_frames(&mut rx);
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
        "恢复播放后音频时钟应前进到 ≥150ms（泵停投 + 起播协议回归点；期间消费了 {frames_seen} 帧）"
    );
}
