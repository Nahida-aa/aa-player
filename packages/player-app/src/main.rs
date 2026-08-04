//! player-app —— aa-player 的 GPUI 图形界面入口。
//!
//! 最小可玩播放器：后台解码线程持续从 player-core 拉帧，转成 GPUI 的
//! [`gpui::RenderImage`] 后通知窗口重绘。渲染模式照抄 zed 的
//! `remote_video_track_view.rs`：双缓冲 + `drop_image` 回收纹理，避免
//! sprite atlas 泄漏。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{
    App, AppContext, Bounds, Context, EventEmitter, IntoElement, Render, RenderImage, Styled, Task,
    Window, WindowBounds, WindowOptions, div, size,
};
use gpui_platform::application;
use image::{Frame, RgbaImage};
use player_core::{DecodedFrame, FfmpegSource, MediaSource};
use tracing::{debug, error, info, warn};

/// 帧间隔直方图的桶上界（毫秒）。以 33.3ms(30fps) 为基准划分：
/// 卡顿的定义是"帧间隔显著偏离标称值"，所以桶要密集分布在 33 附近，
/// 而不是等宽——等宽桶会把 33 和 40 归到一起，恰好看不见我们要找的抖动。
const INTERVAL_BUCKETS_MS: [u64; 9] = [20, 28, 32, 35, 38, 42, 50, 66, 100];

/// 播放性能计数器。仅当 debug 级别开启时才记录（见 `profiling`），
/// 关闭时完全不调用，无原子操作开销。
#[derive(Default)]
struct ProfileStats {
    /// worker 成功送出的帧数（解码产出）。
    decoded: AtomicU64,
    /// 渲染端实际显示的帧数（update 次数）。
    displayed: AtomicU64,
    /// 解码+像素转换总耗时（微秒）。
    decode_total_us: AtomicU64,
    /// 解码+转换的样本数。
    decode_count: AtomicU64,
    /// 两次显示之间的最大间隔（毫秒），反映最坏抖动。
    max_interval_ms: AtomicU64,
    /// 帧间隔直方图，桶边界见 [`INTERVAL_BUCKETS_MS`]，末桶为溢出桶。
    /// 有了分布才能算 p99——平均值会把偶发卡顿完全抹平。
    interval_hist: [AtomicU64; INTERVAL_BUCKETS_MS.len() + 1],
    /// 帧间隔总和（毫秒），配合 displayed 算平均。
    interval_total_ms: AtomicU64,
    /// 上次显示时刻，用于算间隔。
    last_display: Mutex<Option<Instant>>,
}

impl ProfileStats {
    fn record_decoded(&self, us: u64) {
        self.decoded.fetch_add(1, Ordering::Relaxed);
        self.decode_total_us.fetch_add(us, Ordering::Relaxed);
        self.decode_count.fetch_add(1, Ordering::Relaxed);
    }

    fn record_displayed(&self) {
        self.displayed.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut lg = self.last_display.lock().unwrap();
        if let Some(prev) = *lg {
            // 用微秒算再转毫秒：直接 as_millis 会把 33.9ms 截断成 33，
            // 累积起来足以让平均值失真。
            let us = now.duration_since(prev).as_micros() as u64;
            let ms = us / 1000;
            self.interval_total_ms.fetch_add(ms, Ordering::Relaxed);
            self.max_interval_ms.fetch_max(ms, Ordering::Relaxed);

            let idx = INTERVAL_BUCKETS_MS
                .iter()
                .position(|&b| ms < b)
                .unwrap_or(INTERVAL_BUCKETS_MS.len());
            self.interval_hist[idx].fetch_add(1, Ordering::Relaxed);
        }
        *lg = Some(now);
    }

    /// 取出并清零本区间的直方图，返回 (各桶计数, 总数)。
    fn take_hist(&self) -> ([u64; INTERVAL_BUCKETS_MS.len() + 1], u64) {
        let mut out = [0u64; INTERVAL_BUCKETS_MS.len() + 1];
        let mut total = 0;
        for (i, slot) in self.interval_hist.iter().enumerate() {
            out[i] = slot.swap(0, Ordering::Relaxed);
            total += out[i];
        }
        (out, total)
    }
}

/// 从直方图估算分位数上界（毫秒）。返回该分位落入的桶上界，
/// 溢出桶返回 u64::MAX 的替代值 999。直方图只能给出桶粒度的估计，
/// 但判断"有没有卡顿"够用——我们关心的是量级而非精确值。
fn percentile(hist: &[u64], total: u64, p: f64) -> u64 {
    if total == 0 {
        return 0;
    }
    let target = (total as f64 * p).ceil() as u64;
    let mut cum = 0;
    for (i, &c) in hist.iter().enumerate() {
        cum += c;
        if cum >= target {
            return INTERVAL_BUCKETS_MS.get(i).copied().unwrap_or(999);
        }
    }
    999
}

/// 把 player-core 解码出的 BGRA 帧转成 GPUI 可渲染的 RenderImage。
///
/// GPUI 的 RenderImage 内部按 **BGRA** 解释字节（见 zed crates/gpui/src/assets.rs），
/// 与 ffmpeg Pixel::BGRA 一致。RgbaImage 仅作容器，字节序保持 BGRA 不动。
fn decoded_to_render_image(frame: &DecodedFrame) -> Arc<RenderImage> {
    // 紧密打包（去掉 ffmpeg 的行 stride 填充），长度 = w*h*4。
    let tight = frame.to_tight_bgra();
    let img = RgbaImage::from_raw(frame.width, frame.height, tight)
        .expect("frame byte length mismatch");
    Arc::new(RenderImage::new(smallvec::SmallVec::from_elem(Frame::new(img), 1)))
}

/// 播放器视图：持有一帧最新的解码画面。
struct PlayerView {
    /// 解码线程推来、待渲染的最新帧。
    latest_frame: Option<Arc<RenderImage>>,
    /// 双缓冲：当前已渲染的帧，用于下一帧渲染时回收旧纹理。
    current_rendered: Option<Arc<RenderImage>>,
    previous_rendered: Option<Arc<RenderImage>>,
    /// 后台渲染任务句柄（持有以保活）。
    _render_task: Task<()>,
}

/// 解码线程结束（EOF）时发出，便于 UI 提示。
#[derive(Debug)]
struct PlaybackEnded;

impl EventEmitter<PlaybackEnded> for PlayerView {}

impl PlayerView {
    fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 解码/播放管线（参考 OBS 双时钟模型：解码节拍 ≠ 渲染节拍）：
        //   - 独立 OS 线程做同步解码，按 PTS 软时钟节流产出，经有界 channel 发送；
        //   - GPUI 后台 async task 只负责收帧 + 触发重绘，绝不阻塞 executor。
        // 这样重的解算在专用线程，渲染循环（vsync）不被拖慢，避免卡顿。
        //
        // 注意：FfmpegSource 内部持有的 ffmpeg 类型不实现 Send，不能跨线程移动，
        // 因此只把 PathBuf（Send）传进 worker 线程，在**线程内部** open 出 source。
        let (mut tx, mut rx) = mpsc::channel::<Option<(Arc<RenderImage>, u64)>>(3);
        let running = Arc::new(AtomicBool::new(true));
        // 性能计数器。仅当 debug 级别开启时才计数+打印（`RUST_LOG=player_app=debug`），
        // 避免常态下为统计付出原子操作与定时任务开销。
        let profile = Arc::new(ProfileStats::default());
        let profiling = tracing::enabled!(tracing::Level::DEBUG);

        // 关窗时停止解码线程。
        let running_on_release = running.clone();
        cx.on_release(move |_, _cx| {
            running_on_release.store(false, Ordering::Relaxed);
        })
        .detach();

        // 解码线程。
        let running_decode = running.clone();
        let profile_decode = profile.clone();
        std::thread::spawn(move || {
            // source 在本线程内创建（不跨线程），规避 Send 限制。
            let mut source = match FfmpegSource::open(&path) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, path = %path.display(), "打开视频失败");
                    let _ = tx.try_send(None);
                    return;
                }
            };
            let mut frame_no: u64 = 0;
            while running_decode.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                let frame = match source.next_frame() {
                    Ok(Some(f)) => {
                        frame_no += 1;
                        // 每 60 帧打一次即可：逐帧日志在 30fps 下会因终端 IO
                        // 反过来拖慢 worker，污染我们要测的东西。
                        if frame_no % 60 == 0 {
                            debug!(frame = frame_no, pts_ms = f.pts.as_millis(), "解码进度");
                        }
                        f
                    }
                    Ok(None) => {
                        info!(frames = frame_no, "解码到达文件末尾");
                        let _ = tx.try_send(None);
                        return;
                    }
                    Err(e) => {
                        error!(error = %e, frames = frame_no, "解码失败，停止");
                        let _ = tx.try_send(None);
                        return;
                    }
                };
                let decode_us = t0.elapsed().as_micros() as u64;
                let render = decoded_to_render_image(&frame);
                if profiling {
                    profile_decode.record_decoded(decode_us);
                }
                // 尽快投递，把"按 PTS 精确节流"交给渲染端用 GPUI timer 调度
                // （worker 线程的 thread::sleep 精度差，会让显示节奏抖动 → 卡顿）。
                //
                // 队列满是**常态**：渲染端按 PTS 主动等待，容量 3 的队列几乎总是满的，
                // 这正是我们要的背压（解码不跑在渲染前面太多）。因此这里必须**一直重试
                // 到送进去为止**，而不是重试一次就丢帧 —— 丢帧会让渲染端拿到的 PTS
                // 出现空洞，时间轴对不上，表现为忽快忽卡。
                let mut item = Some((render, frame.pts.as_micros() as u64));
                while running_decode.load(Ordering::Relaxed) {
                    match tx.try_send(item) {
                        Ok(()) => break,
                        Err(e) if e.is_full() => {
                            item = e.into_inner();
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        // 接收端已关闭（窗口关了）：退出线程。
                        Err(_) => return,
                    }
                }
            }
            debug!(frames = frame_no, "解码线程正常退出（窗口关闭）");
        });

        // 渲染 task：异步收帧，按 PTS 用 GPUI timer 精确节流显示，绝不阻塞 executor。
        let profile_render = profile.clone();
        let _render_task = cx.spawn_in(window, async move |this, cx| {
            // play_start 是 PTS 时间轴原点：wall_clock = play_start + pts。
            // 首帧到达时校准（首帧 pts 未必为 0，故减去它）。
            let mut play_start: Option<Instant> = None;
            while let Some(item) = rx.next().await {
                match item {
                    Some((render, pts_us)) => {
                        let target = Duration::from_micros(pts_us);
                        let start = *play_start.get_or_insert_with(|| Instant::now() - target);
                        let elapsed = start.elapsed();
                        if target > elapsed {
                            // 还没到点：用 GPUI timer 精确等待（对齐事件循环，
                            // 比 worker 线程的 thread::sleep 更平滑）。
                            cx.background_executor().timer(target - elapsed).await;
                        } else if elapsed - target > Duration::from_millis(200) {
                            // 已落后超过 200ms（启动抖动 / 系统卡顿导致）：重置时间轴原点，
                            // 以当前帧为新起点继续。否则原点永远偏早，之后每帧都判定"迟到"
                            // 从而不再等待，画面会一次性冲刷完再干等 —— 正是忽快忽卡的成因。
                            //
                            // 这条 warn 是有意为之的信号：稳态播放**不该**出现它。
                            // 若持续刷屏，说明解码或渲染真的跟不上，需要查根因而非靠重置掩盖。
                            warn!(
                                behind_ms = (elapsed - target).as_millis(),
                                pts_ms = target.as_millis(),
                                "播放落后，重置时间轴原点"
                            );
                            play_start = Some(Instant::now() - target);
                        }
                        this.update(cx, |this, cx| {
                            this.latest_frame = Some(render);
                            cx.notify();
                        })
                        .ok();
                        if profiling {
                            profile_render.record_displayed();
                        }
                    }
                    None => break, // EOF
                }
            }
            this.update(cx, |_, cx| cx.emit(PlaybackEnded)).ok();
        });

        // 性能打印任务：每 2 秒输出一次区间统计（计数器 swap 为 0 实现区间速率）。
        if profiling {
            let profile_print = profile.clone();
            cx.spawn_in(window, async move |_, _cx| loop {
                _cx.background_executor()
                    .timer(Duration::from_secs(2))
                    .await;
                let decoded = profile_print.decoded.swap(0, Ordering::Relaxed);
                let displayed = profile_print.displayed.swap(0, Ordering::Relaxed);
                let dt = profile_print.decode_total_us.swap(0, Ordering::Relaxed);
                let dc = profile_print.decode_count.swap(0, Ordering::Relaxed);
                let max_i = profile_print.max_interval_ms.swap(0, Ordering::Relaxed);
                let avg_decode = if dc > 0 { dt / dc } else { 0 };
                let (hist, total) = profile_print.take_hist();
                let sum_ms = profile_print.interval_total_ms.swap(0, Ordering::Relaxed);
                let avg_interval = if total > 0 { sum_ms / total } else { 0 };
                let p99 = percentile(&hist, total, 0.99);

                // 准时率：落在 28~38ms（标称 33.3ms 上下各约 5ms）的帧占比，
                // 对应桶下标 2..=4，越接近 100% 越稳。
                let on_time_pct = if total > 0 {
                    (hist[2] + hist[3] + hist[4]) * 100 / total
                } else {
                    0
                };
                // 卡顿判据：只看平均值会被抹平，必须看尾部。
                //   - max > 66ms      ：至少掉了一整帧（2×33.3ms），必然可感
                //   - on_time < 90%   ：超过一成的帧偏离标称节奏，整体不稳
                // 阈值按**人的感知**定，不按理论完美定：实测单帧晚 6ms（39ms）属于
                // timer 精度与合成器节拍的正常抖动，肉眼不可感，不该报警。否则告警
                // 天天响，真出问题时反而被忽略。
                let janky = max_i > 66 || on_time_pct < 90;

                // 注意：decoded 与 displayed 之差**不是**丢帧率——两者天然相差队列
                // 深度(3)与统计窗口边界。真正的丢帧现在已不存在(worker 会重试到送出
                // 为止)，所以这里只报原始速率，不再算那个会骗人的 drop_rate。
                // (旧实现里 decoded 归零时 drop_rate 恒为 0%，正是它掩盖了真实的丢帧。)
                if janky {
                    warn!(
                        decoded_fps = decoded / 2,
                        displayed_fps = displayed / 2,
                        avg_interval_ms = avg_interval,
                        p99_interval_ms = p99,
                        max_interval_ms = max_i,
                        on_time_pct,
                        avg_decode_us = avg_decode,
                        hist = ?hist,
                        "检测到卡顿"
                    );
                } else {
                    info!(
                        decoded_fps = decoded / 2,
                        displayed_fps = displayed / 2,
                        avg_interval_ms = avg_interval,
                        p99_interval_ms = p99,
                        max_interval_ms = max_i,
                        on_time_pct,
                        avg_decode_us = avg_decode,
                        "播放流畅"
                    );
                }
            })
            .detach();
        }

        Self {
            latest_frame: None,
            current_rendered: None,
            previous_rendered: None,
            _render_task,
        }
    }
}

impl Render for PlayerView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // 双缓冲回收：把上一帧的纹理 drop 掉，防止 sprite atlas 无限增长。
        if let Some(current) = self.current_rendered.take() {
            if let Some(prev) = self.previous_rendered.take() {
                if prev.id != current.id {
                    let _ = window.drop_image(prev);
                }
            }
            self.previous_rendered = Some(current);
        }

        let Some(frame) = self.latest_frame.clone() else {
            return div().size_full().into_any_element();
        };
        self.current_rendered = Some(frame.clone());

        gpui::img(frame)
            .size_full()
            .into_any_element()
    }
}

/// 初始化日志订阅者。
///
/// 级别用标准的 `RUST_LOG` 控制，默认 `info`（只出错误与关键状态）。
/// 排查播放性能时用：`RUST_LOG=player_app=debug just dev`
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        // 播放器日志关心"何时"（帧到达节奏），所以保留时间戳；
        // 线程名能区分 worker 线程与 GPUI executor，排查并发问题必需。
        .with_thread_names(true)
        .with_target(false)
        .init();
}

fn main() {
    init_tracing();

    // 样本视频：packages/player-core/tests/assets/sample.mp4
    let path: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../player-core/tests/assets/sample.mp4");

    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(1280.0.into(), 720.0.into()), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| PlayerView::new(path.clone(), window, cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
