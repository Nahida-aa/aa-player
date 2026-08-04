//! player-core 的错误类型。
//!
//! 库包内部用 [`anyhow::Error`] 承载错误（ffmpeg-next 的错误可直接 `?` 透传），
//! 对外不强制引入额外错误类型，方便调用方（player-app / ocr-lab）自行处理。

pub type Result<T> = anyhow::Result<T>;
