//! player-app —— aa-player 的 GPUI 图形界面入口。
//!
//! 模块划分：
//!   - [`playback`]     ：解码线程与 PTS 时间轴（播放管线）
//!   - [`view`]         ：GPUI 视图，负责上屏
//!   - [`stats`]        ：性能统计与卡顿判定
//!   - [`render_image`] ：解码帧 → GPUI 纹理

mod playback;
mod render_image;
mod stats;
mod view;

use std::path::PathBuf;

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, size};
use gpui_platform::application;

use crate::view::PlayerView;

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
    let path: PathBuf =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../player-core/tests/assets/sample.mp4");

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
