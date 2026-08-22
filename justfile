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

# 生成应用图标 PNG（hicolor 多尺寸，产物随仓库提交）。
# 需要 bun：cd scripts && bun install（仅首次）
icons:
    cd scripts && bun gen-icons.mjs

# 本地真实安装（默认用户级 ~/.local，无需 root；可传 PREFIX 覆盖，如 `just install /usr`）。
# 布局与 AUR PKGBUILD 保持一致：bin + share/icons/hicolor + share/applications。
# 安装副本里 Exec/TryExec 改写为绝对路径——用户级安装时 ~/.local/bin 往往不在
# GUI 会话 PATH 里，TryExec 解析失败会导致启动器隐藏该应用。
install prefix="$HOME/.local":
    cargo build --release -p aa-player
    install -Dm755 target/release/aa-player {{prefix}}/bin/aa-player
    cp -r resources/icons/hicolor/. {{prefix}}/share/icons/hicolor/
    sed "s|Exec=aa-player|Exec={{prefix}}/bin/aa-player|; s|TryExec=aa-player|TryExec={{prefix}}/bin/aa-player|" \
        resources/aa-player.desktop > {{prefix}}/share/applications/aa-player.desktop
    -update-desktop-database {{prefix}}/share/applications
    -gtk-update-icon-cache -q -t -f {{prefix}}/share/icons/hicolor
    -kbuildsycoca6

# 只检查 app 包（日常起窗口用）
app:
    cargo run -p aa-player

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

# 打开真实窗口 + 自动拖动（肉眼观察画面/进度条跟着自动 seek 动）
video-auto-drag *video="${VIDEO}":
    cargo run -p gpui-video --example auto_drag -- {{video}}
