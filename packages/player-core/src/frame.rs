//! 解码后的帧与媒体元信息。
//!
//! 关键字段 [`DecodedFrame::stride`]：ffmpeg 解码帧的每行字节数通常 **不等于**
//! `width * 4`（有行对齐填充）。上传 GPU 纹理或复制像素时必须用 stride，
//! 否则画面会斜切。这是抽象层最容易丢失、却最致命的字段。

use std::time::Duration;

/// 解码出来的一帧视频。
///
/// 当前统一转成 **BGRA** 紧密打包（ffmpeg `Pixel::BGRA`），因为 GPUI 的
/// [`gpui::RenderImage`] 接收 BGRA 像素。后续如需保留原始 YUV 平面直传 GPU
/// 做 shader 转换（mpv/VLC 做法），可在此扩展 `format` 字段与多平面数据。
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// 像素数据，长度 `stride * height`（已含行填充）。
    pub data: Vec<u8>,
    /// 单行字节数（>= width * 4），GPU 上传的 `bytes_per_row`。
    pub stride: usize,
    /// 像素宽度。
    pub width: u32,
    /// 像素高度。
    pub height: u32,
    /// 展示时间戳（PTS），相对媒体起点。
    pub pts: Duration,
}

impl DecodedFrame {
    /// 取第 `row` 行（0-based）的像素起点。用于逐行复制/上传，避免被 stride 坑。
    #[inline]
    pub fn row_offset(&self, row: usize) -> usize {
        row * self.stride
    }

    /// 把含行填充的 BGRA 数据紧凑化成 `width*4 * height`，方便直接上传纹理。
    /// 若 stride == width*4 则零拷贝返回引用。
    pub fn to_tight_bgra(&self) -> Vec<u8> {
        let w = self.width as usize * 4;
        if self.stride == w {
            return self.data.clone();
        }
        let mut out = Vec::with_capacity(w * self.height as usize);
        for row in 0..self.height as usize {
            let start = row * self.stride;
            out.extend_from_slice(&self.data[start..start + w]);
        }
        out
    }
}

/// 媒体的视频流元信息。
#[derive(Debug, Clone, Copy)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    /// 总时长；部分流（直播/无索引文件）可能为 0。
    pub duration: Duration,
    /// 平均帧率（fps）；可能为 0（如变帧率视频）。
    pub fps: f64,
}
