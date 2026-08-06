# aa-player

用 Rust + [GPUI](https://www.gpui.rs/) 写的视频播放器。

> 状态：早期开发中。目前能按 PTS 平稳播放视频（无音频、无播放控制）。

## 为什么造这个轮子

本机缺一个称手的本地播放器，影响日常开发时看录屏/样本视频的体验。顺便把解码
部分做成独立的库包，将来可以复用到其他需要逐帧取画面的项目里。

## 结构

```
packages/
  player-core/   库包：解码、seek，不依赖 GUI
  aa-player/     应用包：GPUI 图形界面（发布到 AUR 的二进制）
```

`player-core` 通过 `MediaSource` trait 暴露能力，当前实现基于 ffmpeg
（动态链接系统库）。将来若要换纯 Rust 解码后端，再实现一个 `MediaSource` 即可。

## 依赖

需要系统的 ffmpeg 开发库（≥ 7.x）与 ALSA 开发库（音频输出，cpal 依赖）：

```bash
# Arch
sudo pacman -S ffmpeg alsa-lib

# Debian/Ubuntu
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev libavdevice-dev libasound2-dev
```

> ALSA 只是**接口层**：实际跑在 PipeWire/PulseAudio 上时，
> 由它们提供的 ALSA 兼容层接管，无需额外配置。

## 开发

用 [just](https://github.com/casey/just) 管理常用命令：

```bash
just          # 运行（等同 just dev）
just check    # cargo check --workspace
just test     # cargo test --workspace
just build    # release 构建
```

### 排查播放卡顿

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
