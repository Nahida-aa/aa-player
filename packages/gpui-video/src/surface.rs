//! 视频画面渲染元素（VideoSurface）。
//!
//! 读取控制器的 `latest_frame`，用 `gpui::img` 上屏。
//! 默认填满父容器（视频 letterbox 保持宽高比）；可设 `max_size` 等比上限，
//! 让组件按视频原始比例缩放显示（最大边不超上限）。

use gpui::{Entity, IntoElement, Pixels, RenderOnce, Window, div, img, prelude::*, px};

use crate::controller::PlayerController;

/// 视频画面元素（一次性）。
///
/// 每帧由父视图重新构建，读取 `PlayerController` 的最新帧渲染。
/// 双缓冲回收（`drop_image`）由拥有它的父视图处理，这里只负责上屏。
#[derive(IntoElement)]
pub struct VideoSurface {
    controller: Entity<PlayerController>,
    /// 等比上限：设了则组件按视频比例缩放到最大边 ≤ 上限；未设则填满父容器。
    max_size: Option<Pixels>,
}

impl VideoSurface {
    /// 绑定一个播放控制器。
    pub fn new(controller: &Entity<PlayerController>) -> Self {
        Self {
            controller: controller.clone(),
            max_size: None,
        }
    }

    /// 设置等比上限：组件按视频原始宽高比缩放，最大边不超过 `max`。
    /// 未设则填满父容器（letterbox）。
    pub fn max_size(mut self, max: Pixels) -> Self {
        self.max_size = Some(max);
        self
    }
}

impl RenderOnce for VideoSurface {
    fn render(self, _window: &mut Window, cx: &mut gpui::App) -> impl IntoElement {
        let Some(frame) = self.controller.read(cx).latest_frame() else {
            // 尚无帧：黑色占位。
            return div().size_full().bg(gpui::rgb(0x000000)).into_any_element();
        };
        let img = img(frame);
        // 若设了等比上限且已知视频分辨率，按比例缩放；否则填满容器。
        match (self.max_size, self.controller.read(cx).video_size()) {
            (Some(max), (w, h)) if w > 0 && h > 0 => {
                let scale = (max.as_f32() / w.max(h) as f32).min(1.0);
                let w = (w as f32 * scale).max(1.0);
                let h = (h as f32 * scale).max(1.0);
                img.w(px(w)).h(px(h)).into_any_element()
            }
            _ => img.size_full().into_any_element(),
        }
    }
}
