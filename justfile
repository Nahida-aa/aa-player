# aa-player 的常用命令 —— 沿用 gpui_learn / ocr-lab 的 justfile 风格
#
# 用法：just <recipe>   例如：just dev / just check / just test / just build
# 不带参数时默认执行 `default`。

# 加载项目根目录的 .env（可写 VIDEO=path/to/video.mp4，供 `just video` 用）。
# 见 .gitignore：.env 被忽略，不会把本地视频路径提交到仓库。
set dotenv-load

# 默认 recipe：启动 GUI 应用
default:
    @just --list

# 启动播放器 GUI（首次会从 zed git 源编译 GPUI，较慢）
# 可选传视频路径：`just run path/to/video.mp4`；不传则播放内置样本。
run *video:
    cargo run -- {{video}}

dev *video:
    cargo watch -x run -- {{video}}

debug:
    RUST_LOG=player_app=debug cargo run -- ${VIDEO}

dev-info:
    RUST_LOG=player_app=info cargo run -- ${VIDEO}

# 用环境变量 VIDEO 指定要播放的视频（绝对或相对路径）：
#   VIDEO=/path/to/video.mp4 just video
# 便于 shell 脚本/自动化传大视频测试。
video:
    cargo run -- ${VIDEO}

# 只做类型/编译检查，不产出二进制
check:
    cargo check --workspace

# 跑测试
test:
    cargo test --workspace

# 发布构建
build:
    cargo build --release

# 只检查 app 包（日常起窗口用）
app:
    cargo run -p player-app

# 只检查 core 库包
core:
    cargo check -p player-core

# ---- gpui-video：可复用播放器组件 demo ----
# 用法：
#   just video-demo /path/to/video.mp4   # 第一个参数显式传路径
#   VIDEO=/path/x.mp4 just video-demo    # 或环境变量 VIDEO（默认值 ${VIDEO} 由 shell 展开）
#   都不传 → 播放 player-core 内置样本。
video-demo *video="${VIDEO}":
    cargo run -p gpui-video --example demo -- {{video}}
