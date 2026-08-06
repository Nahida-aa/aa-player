//! aa-player —— aa-player 的 GPUI 图形界面入口。
//!
//! 本二进制只做窗口/资源/命令行装配，真正的播放（解码线程、音频主时钟、
//! 视频上屏、控制条）全部交给可复用组件 `gpui_video::Player`。组件内部已自带
//! 解码/渲染性能统计与卡顿日志，这里不再单独实现。

use std::path::PathBuf;

use std::sync::Arc;

use clap::Parser;
use gpui::{
    App, AppContext, Bounds, FocusHandle, Focusable, MouseButton, Render, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgba, size, svg, white,
};
use gpui_platform::application;
use tracing::info;

use assets::Assets;
use gpui_video::{Player, TimeFormat};

/// 播放器的命令行参数。
#[derive(Parser)]
#[command(name = "aa-player", about = "用 Rust + GPUI 写的视频播放器")]
struct Cli {
    /// 要播放的视频路径（绝对或相对路径）。
    /// 不传则在应用内点击选择视频文件（系统文件选择器）。
    #[arg(value_name = "VIDEO")]
    video: Option<PathBuf>,
}

/// 从内嵌资源加载窗口图标（X11）。gpui 仅 X11 支持 `WindowOptions.icon`，
/// 且要栅格图（`RgbaImage`），故用 Gemini 生成的 PNG 孪生文件，而非矢量 logo。
/// Wayland 下的图标由 `.desktop` 文件提供，这里设了也不影响。
fn load_window_icon(cx: &App) -> Option<Arc<image::RgbaImage>> {
    let bytes = cx.asset_source().load("images/Gemini_Generated_Image_1kfs201kfs201kfs.png").ok().flatten()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    Some(Arc::new(img))
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
    info!(has_cli_path = cli.video.is_some(), "启动");

    application()
        // 提供内嵌资源（图标/字体），供组件的 svg().path("icons/…") 与文本渲染。
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            // 加载内嵌字体，保证时间文本等正常渲染。
            let _ = Assets.load_fonts(cx);

            let bounds = Bounds::centered(None, size(1280.0.into(), 720.0.into()), cx);
            // 窗口图标（X11 用栅格 PNG；Wayland 由 .desktop 提供）。每个窗口创建时都设。
            let window_icon = load_window_icon(cx);

            match cli.video {
                // 传了路径：直接打开播放器（相对路径相对当前工作目录解析）。
                Some(raw) => {
                    let path = if raw.is_absolute() {
                        raw
                    } else {
                        std::env::current_dir()
                            .map(|cwd| cwd.join(&raw))
                            .unwrap_or(raw)
                    };
                    info!(path = %path.display(), "播放");
                    let window_handle = cx
                        .open_window(
                            WindowOptions {
                                window_bounds: Some(WindowBounds::Windowed(bounds)),
                                icon: window_icon.clone(),
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
                }
                // 没传路径：显示欢迎层，点击任意处用系统选择器选视频。
                None => {
                    cx.open_window(
                        WindowOptions {
                            window_bounds: Some(WindowBounds::Windowed(bounds)),
                            icon: window_icon.clone(),
                            ..Default::default()
                        },
                        |_window, cx| cx.new(|cx| Launcher::new(cx)),
                    )
                    .unwrap();
                }
            }

            cx.activate(true);
        });
}

/// 无参启动时的欢迎层：点击任意处弹出系统文件选择器，选中的视频再交给 `Player`。
struct Launcher {
    focus_handle: FocusHandle,
}

impl Focusable for Launcher {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Launcher {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Render for Launcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgba(0x000000ff))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .track_focus(&self.focus_handle)
            .text_color(white())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, window, cx| {
                    // 异步开系统文件选择器，避免阻塞 GPUI executor。
                    cx.spawn_in(window, async move |_this, cx| {
                        let picked = rfd::AsyncFileDialog::new()
                            .set_title("选择视频文件")
                            .pick_file()
                            .await;
                        if let Some(file) = picked {
                            // 选完即用该路径替换根视图为播放器，并订阅 EOF。
                            let path = file.path().to_path_buf();
                            let player = cx
                                .replace_root_view(|window, cx| {
                                    Player::new(path, window, cx)
                                        .time_format(TimeFormat::FrameMillis)
                                })
                                .expect("替换根视图为 Player 失败");
                            cx.subscribe(&player, |_player, _event, _cx| {
                                info!("播放结束（EOF）");
                            })
                            .detach();
                        }
                    })
                    .detach();
                    let _ = &this;
                }),
            )
            .child(
                svg()
                    .path("images/logo.svg")
                    .w(px(160.0))
                    .h(px(160.0)),
            )
            .child(
                div()
                    .text_xl()
                    .mt(px(16.0))
                    .child("点击任意位置选择视频文件"),
            )
    }
}
