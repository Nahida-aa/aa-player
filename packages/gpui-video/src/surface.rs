//! 视频画面渲染元素（VideoSurface）。
//!
//! 读取控制器的 `latest_frame`，用 `gpui::img` 上屏，填满给定盒子。

use gpui::{Entity, IntoElement, RenderOnce, Window, div, prelude::*};

use crate::controller::PlayerController;

/// 视频画面元素（一次性）。
///
/// 每帧由父视图重新构建，读取 `PlayerController` 的最新帧渲染并填满父容器。
/// 双缓冲回收（`drop_image`）由拥有它的父视图处理，这里只负责上屏。
#[derive(IntoElement)]
pub struct VideoSurface {
    controller: Entity<PlayerController>,
}

impl VideoSurface {
    /// 绑定一个播放控制器。
    pub fn new(controller: &Entity<PlayerController>) -> Self {
        Self {
            controller: controller.clone(),
        }
    }
}

impl RenderOnce for VideoSurface {
    fn render(self, _window: &mut Window, cx: &mut gpui::App) -> impl IntoElement {
        let Some(frame) = self.controller.read(cx).latest_frame() else {
            // 尚无帧：黑色占位。
            return div().size_full().bg(gpui::rgb(0x000000)).into_any_element();
        };
        gpui::img(frame).size_full().into_any_element()
    }
}
