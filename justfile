# aa-player 的常用命令 —— 沿用 gpui_learn / ocr-lab 的 justfile 风格
#
# 用法：just <recipe>   例如：just dev / just check / just test / just build
# 不带参数时默认执行 `default`（即 dev）。

# 默认 recipe：启动 GUI 应用
default: dev

# 启动播放器 GUI（首次会从 zed git 源编译 GPUI，较慢）
dev:
    cargo run

debug:
    RUST_LOG=player_app=debug cargo run

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
