# aa-player · 路线图 / TODO

本文件记录开发进度与待办，方便协作者和未来的自己快速对齐状态。
完成项保留历史痕迹（含 commit 哈希），未完成的按优先级排列。

> 进度与 `git log` 对齐；commit 哈希是写文件时的快照，后续 rebase 会变。

## 已完成

- [x] **骨架与构建** — cargo workspace 结构、`justfile`、依赖接入。
  `packages/player-core`（库：解码 / seek，不依赖 GUI）+ `packages/player-app`（GPUI 界面）。
- [x] **视频解码 + 按 PTS 平稳播放（无音频）** — `MediaSource` trait + `FfmpegSource`：
  解包 → 解码 → BGRA 缩放 → 双缓冲上屏，按 PTS 用 GPUI timer 节流。
  commit `19c4ce3` 之前的多步提交。
- [x] **接入 cpal 音频输出** — `AudioOutput`：采样队列 + 回调填充 +
  主时钟读数（`frames_played` 原子计数）。验证能真实出声。
  commit `19c4ce3`
- [x] **音频解码与重采样** — `AudioDecoder`：`ffmpeg_next::decoder::Audio` +
  swresample 重采样到设备采样率/声道，产出交错 `AudioChunk`。
  踩坑：① packed 多声道必须用 `data(0)` 而非 `plane::<f32>(0)`（单声道测不出）；
  ② `flush` 必须预分配输出帧（用 `delay().output`）。
  验证：录 sink monitor 与 ffmpeg 参考 PCM 互相关 corr=1.0000，10.01s 时长吻合。
  commit `1eadab9`
- [x] **MediaSource 统一产出音视频** — `next_event()` 返回 `MediaEvent::{Video,Audio}`；
  音频为可选 `open_with(path, Option<AudioFormat>)`；保留默认 `next_frame()` 跳过音频。
  commit `c699c26`
- [x] **音视频同时播放 + 音频主时钟** — 解码线程直接把采样推声卡；
  `AudioClockSource`(OnceLock) 把非 `Send` 的声卡时钟交接给渲染侧；
  `PlaybackClock` 支持音频主时钟（落后 >100ms 丢帧 `Drop`，墙钟模式保留 `Resynced`）。
  commit `49eeb04`

## 进行中 / 下一步

- [ ] **#12 音画同步质量验证 + 漂移度量**
  - 在 `stats.rs` 加度量：渲染侧记录每帧显示时 `audio.position() - pts`，汇总 max/p99。
  - 端到端跑真实播放，确认漂移在人耳可感范围（±40ms）内、10s 样本无累积漂移。
  - 注：headless 单测覆盖不到真实声卡同步，需在带音频设备的环境跑。
  - 前几轮验证教训：编译通过不代表行为对；粗糙测量（如录短了误判截断）比系统更会骗人。

## 待办（未排期）

- [ ] **播放控制 UI** — 暂停 / 继续、seek 拖动条、音量。
  - seek 时既要清解码队列、重置视频时间轴，也要重置音频时钟（声卡不能中途倒带，
    需把已推入队列的采样排空或重建流）。
- [ ] **AUR PKGBUILD** — 便于在 Arch 上安装；注意动态链接系统 ffmpeg，
  运行时依赖要在 `depends` 里列全。
- [ ] **音视频首帧对齐** — 当前音频时钟从首个采样开始计、视频从首帧校准原点，
  二者起点可能有小偏差，必要时在 `PlaybackClock` 引入 offset 校正。
- [ ] **真实设备上的 A/V 同步端到端测试** — 考虑用固定测试素材 + 录制，
  把"互相关 corr≈1.0 且漂移 < N ms"做成可重复的检查（参考 `docs/debugging-playback-jank.md`）。

## 关键设计约束（易踩坑，先读）

1. `FfmpegSource` / `AudioOutput` / cpal `Stream` 都**不是 `Send`**：
   必须在解码线程内部 `open`，时钟只把原子计数器克隆出去。
2. 队列满时**不能丢帧**（要重试到送出），否则 PTS 出现空洞、时间轴错乱 → 忽快忽卡。
3. 音频**不走帧 channel**：解码线程直接推声卡，走最短路径；声音断裂比画面卡顿刺耳得多。
4. 落后处理分两种模式：**墙钟**落后可重置原点（`Resynced`）；
   **音频主时钟**落后已无法挽回（声音放出去了），只能让画面丢帧追上去（`Drop`）。
5. 度量看**尾部分布**（p99 / max / on_time_pct），平均值会把偶发卡顿抹平。

详见 [docs/debugging-playback-jank.md](docs/debugging-playback-jank.md)。
