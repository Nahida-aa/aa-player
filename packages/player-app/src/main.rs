//! player-app —— aa-player 的 GPUI 图形界面入口。
//!
//! 本二进制只做窗口/资源/命令行装配，真正的播放（解码线程、音频主时钟、
//! 视频上屏、控制条）全部交给可复用组件 `gpui_video::Player`。组件内部已自带
//! 解码/渲染性能统计与卡顿日志，这里不再单独实现。

use std::path::PathBuf;

use clap::Parser;
use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, size};
use gpui_platform::application;
use tracing::info;

use assets::Assets;
use gpui_video::{Player, TimeFormat};

/// 播放器的命令行参数。
#[derive(Parser)]
#[command(name = "aa-player", about = "用 Rust + GPUI 写的视频播放器")]
struct Cli {
    /// 要播放的视频路径（绝对或相对路径）。
    /// 不传时播放内置样本 `player-core/tests/assets/sample.mp4`。
    #[arg(value_name = "VIDEO")]
    video: Option<PathBuf>,
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

    let cli = Cli::parse();
    // 解析视频路径：优先命令行参数；未传时用内置样本。
    let path: PathBuf = match cli.video {
        Some(v) => v,
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../player-core/tests/assets/sample.mp4"),
    };
    // 绝对路径直接用；相对路径相对当前工作目录解析（clap 已按字面保留）。
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    };
    info!(path = %path.display(), "播放");

    application()
        // 提供内嵌资源（图标/字体），供组件的 svg().path("icons/…") 与文本渲染。
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            // 加载内嵌字体，保证时间文本等正常渲染。
            let _ = Assets.load_fonts(cx);

            let bounds = Bounds::centered(None, size(1280.0.into(), 720.0.into()), cx);
            let window_handle = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        ..Default::default()
                    },
                    |window, cx| {
                    cx.new(|cx| {
                        Player::new(path.clone(), window, cx)
                            .time_format(TimeFormat::FrameMillis)
                    })
                },
                )
                .unwrap();
            // 订阅播放结束（EOF）事件，便于外部感知（如自动下一集/提示）。
            let player = window_handle.entity(cx).unwrap();
            cx.subscribe(&player, |_player, _event, _cx| {
                info!("播放结束（EOF）");
            })
            .detach();
            cx.activate(true);
        });
}
