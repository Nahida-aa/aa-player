//! [`PlayerController`] 的控制命令方法（impl 白盒拆分文件）。
//!
//! 类型与字段定义在父模块，本文件只放「改变状态 / 下发命令」的那一半
//! impl。子模块天然可见父级私有字段，拆分不需要任何可见性调整。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use super::{FrameMsg, PlayerCommand, PlayerController, SeekStep};
use crate::i18n::{Lang, StrKey};

/// 两次预览 seek 的最小间隔（≈22 次/s）。见 [`PlayerController::seek_preview`]。
const PREVIEW_MIN_INTERVAL: Duration = Duration::from_millis(45);

impl PlayerController {
    /// 切换静音（持久）。同时下发命令让解码线程调音量增益。
    pub fn toggle_mute(&mut self) {
        self.set_muted(!self.muted);
    }

    /// 设置静音（持久）。`muted=true` 增益置 0，否则恢复 1。
    pub fn set_muted(&mut self, muted: bool) {
        if self.muted == muted {
            return;
        }
        self.muted = muted;
        let _ = self.cmd.unbounded_send(PlayerCommand::SetMuted(muted));
    }

    /// 「更多」菜单是否展开。
    pub fn is_menu_open(&self) -> bool {
        self.menu_open
    }

    /// 切换「更多」菜单展开状态。
    pub fn toggle_menu(&mut self) {
        self.menu_open = !self.menu_open;
    }

    /// 关闭「更多」菜单（点击外部时调用）。
    pub fn close_menu(&mut self) {
        self.menu_open = false;
    }

    /// 播放速度档位（点击倍速菜单项时循环切换）。
    pub const SPEED_STEPS: &'static [f64] = &[1.0, 1.25, 1.5, 2.0, 0.5];

    /// 当前播放速度倍率。
    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// 设置播放速度倍率（clamp 到合理范围后下发解码线程）。
    pub fn set_speed(&mut self, speed: f64) {
        let speed = speed.clamp(0.25, 4.0);
        if (speed - self.speed).abs() < f64::EPSILON {
            return;
        }
        self.speed = speed;
        let _ = self.cmd.unbounded_send(PlayerCommand::SetSpeed(speed));
    }

    /// 循环切换到下一个速度档位（点击「倍速」菜单项时调用）。
    pub fn cycle_speed(&mut self) {
        let idx = Self::SPEED_STEPS
            .iter()
            .position(|&s| (s - self.speed).abs() < f64::EPSILON)
            .unwrap_or(0);
        let next = Self::SPEED_STEPS[(idx + 1) % Self::SPEED_STEPS.len()];
        self.set_speed(next);
    }

    /// 快进/快退步长档位（点击更多菜单里的「步长」项时循环切换）。
    ///
    /// 含「1 帧」（按当前 fps 换算）、「1ms」「100ms」等细粒度，以及常用的秒级档。
    pub const SEEK_STEP_OPTIONS: &'static [SeekStep] = &[
        SeekStep::Frames(1),
        SeekStep::Duration(Duration::from_millis(1)),
        SeekStep::Duration(Duration::from_millis(100)),
        SeekStep::Duration(Duration::from_secs(5)),
        SeekStep::Duration(Duration::from_secs(10)),
        SeekStep::Duration(Duration::from_secs(30)),
    ];

    /// 当前快进/快退步长。
    pub fn seek_step(&self) -> SeekStep {
        self.seek_step
    }

    /// 当前步长解析成实际跳转时长（Frames 按 `fps` 换算；fps≤0 时 fallback 30）。
    pub fn seek_step_duration(&self) -> Duration {
        self.seek_step.resolve(self.fps())
    }

    /// 设置快进/快退步长（UI 直接指定，如自定义输入）。
    pub fn set_seek_step(&mut self, step: SeekStep) {
        self.seek_step = step;
    }

    /// 按当前步长向前跳一步（控制条快进按钮 / 键盘右方向键调用）。
    pub fn seek_forward_step(&mut self) {
        self.seek_forward(self.seek_step_duration());
    }

    /// 按当前步长向后跳一步（控制条快退按钮 / 键盘左方向键调用）。
    pub fn seek_backward_step(&mut self) {
        self.seek_backward(self.seek_step_duration());
    }

    /// 循环切换到下一个步长档位（点击「步长」菜单项时调用）。
    pub fn cycle_seek_step(&mut self) {
        let idx = Self::SEEK_STEP_OPTIONS
            .iter()
            .position(|&s| s == self.seek_step)
            .unwrap_or(0);
        self.seek_step = Self::SEEK_STEP_OPTIONS[(idx + 1) % Self::SEEK_STEP_OPTIONS.len()];
    }

    // ----- i18n -----

    /// 当前语言。
    pub fn lang(&self) -> Lang {
        self.i18n.lang()
    }

    /// 取键的译文（按当前语言）。
    pub fn t(&self, key: StrKey) -> &'static str {
        self.i18n.get(key)
    }

    /// 循环切换语言（点击「语言」菜单项时调用）。
    pub fn cycle_lang(&mut self) {
        self.i18n.cycle();
    }

    /// 「info」信息面板是否展开。
    pub fn is_info_open(&self) -> bool {
        self.info_open
    }

    /// 切换「info」信息面板（点更多菜单里的 info 项时调用）。
    pub fn toggle_info(&mut self) {
        self.info_open = !self.info_open;
    }

    /// 关闭「info」信息面板（点击外部时调用）。
    pub fn close_info(&mut self) {
        self.info_open = false;
    }

    // ----- 控制 -----

    pub fn play(&mut self) {
        if self.paused {
            self.paused = false;
            let _ = self.cmd.unbounded_send(PlayerCommand::Resume);
        }
    }

    pub fn pause(&mut self) {
        if !self.paused {
            self.paused = true;
            let _ = self.cmd.unbounded_send(PlayerCommand::Pause);
        }
    }

    pub fn toggle(&mut self) {
        if self.paused {
            self.play();
        } else {
            self.pause();
        }
    }

    /// 跳到指定时刻（正式，松开/点击）。同步更新本地 position（预测）。
    ///
    /// position 在这里**立即预测成 target**：seek 有几十毫秒固有延迟（ffmpeg seek +
    /// 重建声卡流），若等真实解码帧慢慢爬过来，进度条/画面会在 seek 期间停在旧值，
    /// 看起来"卡住一下"。先预测让进度条立刻跳到目标（反馈跟手）。
    ///
    /// 预测的 position 被 seek 前投递进帧通道的旧帧覆盖（thumb 闪回）的问题，由
    /// 渲染循环在 seek 时（检测到音频时钟换代）丢弃在途旧帧来兜底。
    pub fn seek_to(&mut self, target: Duration) {
        let target = self.clamp_target(target);
        self.position = target;
        self.dragging = false;
        // 每发一次正式 seek 就推进 seek 代次：seek 前在途的旧帧（代次更小）会在
        // consume_frame 被丢弃，不覆盖这里预测的 position（避免进度条/thumb 闪回）。
        self.seek_gen += 1;
        let _ = self.cmd.unbounded_send(PlayerCommand::SeekCommit(target, self.seek_gen));
    }

    /// 拖动开始：静音（停声卡 + 清队列）。与 Preview 解耦，拖动开始时发一次。
    pub fn mute_audio(&mut self) {
        let _ = self.cmd.unbounded_send(PlayerCommand::MuteAudio);
    }

    /// 拖动中预览 seek：本地位置跟手 + 节流下发。
    ///
    /// **节流**：鼠标移动事件 60+/s，若每次都 demux seek，音频被切成
    /// ~16ms 碎片且每次进入点都带 AAC 收敛噪声——拖动电音的直接来源。
    /// 这里限制真实 seek 频率 ≤ `PREVIEW_MIN_INTERVAL`（~22/s），窗口内
    /// 的移动只更新 UI position（跟手不变），解码侧停在上一预览位置。
    /// 松手的 [`seek_release`](Self::seek_release) 永远全量执行，最终
    /// 落点不受节流影响。
    /// **不置取消标志**——chromium 的 ffmpeg_glue 从不装 interrupt_callback
    /// （取消发生在读调用方层面），中途掐断 avio 会把内部缓冲留在半包
    /// 状态，反复吐同一个"损坏"包。本地文件 avformat_seek_file 本就毫秒级。
    pub fn seek_preview(&mut self, target: Duration) {
        let target = self.clamp_target(target);
        self.position = target;
        self.dragging = true;
        self.seek_gen += 1;
        let now = std::time::Instant::now();
        if let Some(t) = self.last_preview_sent
            && now.duration_since(t) < PREVIEW_MIN_INTERVAL
        {
            return;
        }
        self.last_preview_sent = Some(now);
        let _ = self
            .cmd
            .unbounded_send(PlayerCommand::SeekPreview(target, self.seek_gen));
    }

    /// 结束拖动：发正式 seek，清拖动态。
    pub fn seek_release(&mut self, target: Duration) {
        self.dragging = false;
        self.seek_to(target);
    }

    /// 目标夹到 [0, duration]。**时长未知（0）时不夹**——直接钳会把一切
    /// seek 打回起点（首帧未消费前的启动窗口、以及绕过控制器消费帧的
    /// 测试环境都会踩中）；解码侧 seek_clamped 拿真实时长兜底。
    fn clamp_target(&self, target: Duration) -> Duration {
        if self.duration == Duration::ZERO {
            target
        } else {
            target.min(self.duration)
        }
    }

    /// 相对当前位置 seek（快进/快退）。`delta_ns` 为相对偏移（纳秒），正向前、
    /// 负向后，按 `[0, duration]` 夹紧后发正式 seek（预测 position + 推进 seek
    /// 代次），不进拖动预览态（快进快退是一次性跳转）。
    pub fn seek_relative(&mut self, delta_ns: i64) {
        if delta_ns >= 0 {
            self.seek_forward(Duration::from_nanos(delta_ns as u64));
        } else {
            self.seek_backward(Duration::from_nanos((-delta_ns) as u64));
        }
    }

    /// 相对当前位置向前 seek（夹到 `[0, duration]`）。
    pub fn seek_forward(&mut self, delta: Duration) {
        let target = (self.position() + delta).min(self.duration);
        self.seek_to(target);
    }

    /// 相对当前位置向后 seek（夹到 `[0, duration]`）。
    pub fn seek_backward(&mut self, delta: Duration) {
        let target = self.position().saturating_sub(delta);
        self.seek_to(target);
    }

    /// 渲染循环消费一帧：更新 position/duration/latest_frame。
    pub fn consume_frame(&mut self, item: FrameMsg, cx: &mut gpui::Context<Self>) {
        let Some((render, pts_us, duration_us, _preview, frame_gen)) = item else {
            // EOF：进度条拉满，但解码线程仍活着等 seek 命令。
            if self.duration != Duration::ZERO {
                self.position = self.duration;
            }
            cx.notify();
            return;
        };
        self.duration = Duration::from_micros(duration_us);
        // seek 后在途的旧帧（所属 seek 代次 < 当前代次）会先于真实目标帧到达。
        // 若用它们的 pts 更新 position，会把 `seek_to` 预测的目标覆盖回 seek 前
        // 的位置 —— 连续按方向键时 thumb 闪回"原点"。丢弃这些旧帧的 position。
        // （画面仍可显示——渲染循环已用时钟丢弃了大多数旧帧；这里保证 position
        //   不被旧帧污染，而 `latest_frame` 照常更新。）
        let stale = frame_gen < self.seek_gen;
        if !self.dragging && !stale {
            self.position = Duration::from_micros(pts_us);
        }
        self.latest_frame = Some(render);
        cx.notify();
    }

    /// 取消标志的 clone（供 seek 抢占；解码线程持有另一份）。
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel_seek.clone()
    }
}
