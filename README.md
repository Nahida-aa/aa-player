# aa-player

用 Rust + [GPUI](https://www.gpui.rs/) 写的视频播放器。

> 状态：可用早期版。音视频同步播放、暂停/快进快退/进度条 seek、拖动预览、
> 倍速、音量；Linux（Wayland/X11）优先，Windows 移植实验中。

![logo](packages/assets/assets/images/logo.svg)

## 为什么造这个轮子

本机缺一个称手的本地播放器，影响日常开发时看录屏/样本视频的体验。顺便把解码
部分做成独立的库包，将来可以复用到其他需要逐帧取画面的项目里。

## 功能

- 音视频同步播放，音频主时钟调度（PipeWire/PulseAudio/ALSA）
- 播放控制：空格暂停/继续、←/→ 快进快退（5s）、进度条点击跳转与拖动预览
- 倍速播放（0.25x–4x）、静音
- seek 时声卡流重建重锚时钟，拖动中实时出预览帧并静音
- 中英双语界面（跟随系统）
- 任务栏图标 / 启动器集成（Wayland `app_id` 与 X11 `WM_CLASS` 对齐）

## 安装

### Arch Linux

> AUR 包已备好（`packaging/aur/`，实测构建通过），但 AUR 新账号注册
> 目前对所有人关闭（官方清理恶意包中），恢复后即上架。期间用下面任一方式：

**预编译二进制（推荐，免编 GPUI）**——仓库内 PKGBUILD 直接装，
从 GitHub Release 拉取二进制并交给 pacman 管理：

```bash
git clone https://github.com/Nahida-aa/aa-player
cd aa-player/packaging/aur/aa-player-bin
makepkg -si
```

或源码构建：同上但进 `packaging/aur/aa-player/` 目录。

动态链接系统 ffmpeg——ffmpeg 大版本升级后请重编/升级包。

### 本地安装（任意发行版，用户级）

需要 [just](https://github.com/casey/just)：

```bash
just install            # 装到 ~/.local/bin + 图标/桌面文件，刷新缓存
just install /usr       # 或系统级
```

### 从源码构建

依赖系统的 ffmpeg 开发库（**≥ 9.x**）与 ALSA 开发库：

```bash
# Arch
sudo pacman -S ffmpeg alsa-lib clang

# Debian/Ubuntu（注意仓库 ffmpeg 版本可能过旧，见下）
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev \
    libavfilter-dev libavdevice-dev libasound2-dev pkg-config clang
```

> 绑定按 FFmpeg 9 头文件生成（libavcodec 63）。Debian stable 的 ffmpeg
> 通常落后，编译报错先查版本。ALSA 只是接口层：实际跑在 PipeWire/PulseAudio
> 上时由它们的 ALSA 兼容层接管。

```bash
cargo run --release -- path/to/video.mp4
```

### Windows

从 [Release](https://github.com/Nahida-aa/aa-player/releases/latest) 下载
`aa-player-windows-x86_64.zip` 解压即用：exe 静态链接 CRT、ffmpeg 为
vcpkg 的 LGPL 共享 DLL（/MT 自包含），无需安装任何运行库。
平台差异问题见 [TODO.md](TODO.md)。

## 结构

```
packages/
  player-core/   库包：解码、seek，不依赖 GUI
  gpui-video/    可复用播放器组件（解码线程/音频时钟/画面/控制条/统计）
  aa-player/     应用包：GPUI 图形界面（发布到 AUR 的二进制）
packaging/aur/   AUR 双包模板（aa-player 源码构建 / aa-player-bin 预编译）
resources/       图标产物、desktop 文件（打包直接拷贝，无需 node/bun）
.github/workflows/   Release 流水线（tag 触发）与 Windows 实验构建
```

`player-core` 通过 `MediaSource` trait 暴露能力，当前实现基于 ffmpeg
（动态链接系统库）。将来若要换纯 Rust 解码后端，再实现一个 `MediaSource` 即可。

## 开发

用 [just](https://github.com/casey/just) 管理常用命令：

```bash
just          # 运行（等同 just dev）
just check    # cargo check --workspace
just test     # cargo test --workspace
just build    # release 构建
just icons    # 从 logo.svg 重新生成 PNG/ICO 图标（需 bun）
```

### 排查播放卡顿 / 音频欠载

日志用 `tracing`，级别由标准的 `RUST_LOG` 控制：

```bash
RUST_LOG=player_app=debug just dev
```

每 2 秒输出一次播放统计，并直接给出结论：

```
INFO 播放流畅 decoded_fps=30 displayed_fps=30 avg_interval_ms=32
     p99_interval_ms=38 max_interval_ms=36 on_time_pct=100 avg_decode_us=4046
```

判据看**尾部分布**而非平均值——平均值会把偶发卡顿完全抹平。
`max_interval_ms > 66`（掉了整帧）或 `on_time_pct < 90` 时会升级为
`WARN 检测到卡顿`，并附上帧间隔直方图便于定位。

音频侧另有「解码线程 2s 时间去向」（next_event/send_blocked/音频推送与
消费对照）与「next_event 2s 统计」探针，用于定位欠载类问题；
`cargo run -p player-core --example pump_bench` 可单独压测解码吞吐。

## 文档

- [路线图 / TODO](TODO.md) —— 开发进度与待办，协作者与未来的自己用来对齐状态。
- [排查播放「无响应」与「卡顿」的经历](docs/debugging-playback-jank.md)
  —— 四个真实 bug 的症状/真因对照，以及三次「度量本身在骗人」的教训。

## 测试素材

`packages/player-core/tests/assets/sample.mp4` 是刻意做小的样本
（480x270 / 10s / 30fps / 固定 1s GOP / AAC 单声道 / ~250KB），只为覆盖解码链路。

- 参数取整数：断言好写。
- 固定 GOP：seek 测试才能收紧容差。容差过松会掩盖真实缺陷——最初用 12s
  容差时，seek 落回 0s 这种明显错误都能"通过"。
- 保留音轨：将来做 A/V 同步需要。有一条测试专门守着它，防止素材被重新
  生成时漏掉 `-c:a`。

## License

Apache-2.0
