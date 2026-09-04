# aa-player · 路线图 / TODO

本文件记录开发进度与待办，方便协作者和未来的自己快速对齐状态。
完成项保留历史痕迹（含 commit 哈希），未完成的按优先级排列。

> 进度与 `git log` 对齐；commit 哈希是写文件时的快照，后续 rebase 会变。

## 已完成

- [x] **骨架与构建** — cargo workspace 结构、`justfile`、依赖接入。
  `packages/player-core`（库：解码 / seek，不依赖 GUI）+ `packages/aa-player`（GPUI 界面，发布到 AUR）。
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

- [x] **#12 音画同步漂移度量 + 真实设备验证**
  - `stats.rs` 新增音画漂移度量：`record_av_sync(drift_us=音频时钟-帧PTS)`，
    汇总有符号均值 `av_sync_mean_ms`、RMS `av_sync_rms_ms`、最大落后/领先、
    超 ±40ms 阈值占比 `av_sync_bad_pct`（见 `AV_SYNC_TOLERANCE_MS`）。
  - 渲染侧在**真正显示**的帧（非 Drop 丢帧）记录漂移；`view.rs` 每 2s 上报
    "音画同步 / 音画失步"（后者在 >10% 帧超阈值或单帧 >100ms 时触发）。
  - 补 5 个单测：零漂移、偶发尖峰不误报、系统性落后报警、方向符号、极端单帧报警。
  - **真实设备验证通过**（`RUST_LOG=player_app=debug cargo run`，PipeWire sink）：
    全程 `mean_ms≈3` `rms_ms≈4` `max_lag≤12` `bad_pct=0`，漂移稳定 ±10ms 内，
    远优于 ±40ms 可感阈值，无累积漂移。
  - 顺带修了一个**度量误报 bug**：卡顿判据的"准时窗口"原硬编码 28~38ms，
    把正常 30fps 抖动（常到 41ms）算成失准，导致健康播放被天天报成卡顿
    （又是"度量本身在骗人"）。改为 20~42ms（桶 1..=5），`max>66` 兜底真掉帧。

## 进行中 / 下一步

- [ ] **播放控制（进行中）** — 已落地暂停/继续 + seek + 屏上进度条（见下方已完成），
  剩余：
  - **进度条拖动**：目前是点击跳转（`on_mouse_down`），拖动（`on_drag_move`）留待接入。
  - **音量控制**：未做。
  - **seek 重建声卡流的爆音淡入淡出**：当前 seek 重建会有一瞬静音/爆音，可加淡入淡出。

## 已完成

- [x] **播放控制器 v1** — 空格暂停/继续、←/→ 快退快进（5s）、底部进度条 + 时间显示、
  点击进度条跳转；seek 重建声卡流重锚音频时钟（`AudioClockSource` 改为可换柄 +
  generation 换代检测）。
  - **修复一个真 bug**：启动时首窗口曾出现 427ms 卡顿 + 400ms 画面领先音频
    （`音画失步 max_lead=400`）。根因：渲染循环每帧 `PlaybackClock::with_audio`
    重建，把墙钟 `origin` 反复清零，导致音频未出声的启动窗口里画面不受节流提前刷出，
    音频一出声又猛然等 400ms 追赶。改为 `set_audio`（保留 origin）+ 按 generation
    换代才换柄，实测定版后启动窗口干净（首窗 `on_time≥96%`，稳态 `bad_pct=0`）。
  - 真实设备验证（3 次 run）：稳态 `mean≈3ms` `rms≈4ms` `max_lag≤10` `bad_pct=0`，
    无启动卡顿、无丢帧风暴。
  - **后续修复两个 bug**：
    1. 点击进度条映射到整窗宽，导致点窗最右=满进度；改为进度条占满整行宽、
       时间文本独立成行，点击按窗口宽精确映射。
    2. 播放到末尾（EOF）后再拖进度条画面不动：EOF 时解码线程不再退出，而是
       `finished=true` 继续轮询命令；seek 清 draining 后重新出帧。新增
       `seek_after_eof_resumes_playback` 集成测试（需真实音频设备，`#[ignore]`）。

## 已完成

- [x] **音频续杯：根治 FFmpeg 9 下的持续欠载** — ffmpeg-next 8→9 后 h264
  多线程解码让视频帧就绪节奏更碎，单解码线程按显示节奏拉事件 + 视频优先交付，
  音频产出被锁死在贴实时线（0.92x），队列从启动峰值一路衰减到反复欠载。
  三层修复（commit `6e4c134`）：
  1. `MediaSource::try_next_audio`：解码器见底时继续读包喂它，途中视频包
     压缩态暂存有界 `video_backlog`，把解复用位置推到播放位置前方；
  2. controller 投递视频帧前先「续杯」到 AUDIO_BUFFER 水位——水位检查放
     **泵循环条件**而非推送路径（推送带背压睡眠会把主循环拖成声卡节奏，
     实测视频掉到 1 帧/2s）；
  3. FRAME_QUEUE_CAP 3→12：通道容量=解复用领先度上限，3 帧(~100ms)撑不起
     400ms 音频缓冲。
  复测 60.mp4：14s 全程零欠载、队列稳定蓄满 416ms。探针保留：
  「解码线程 2s 时间去向」「next_event 统计」+ pump_bench 隔离基准。

## 已完成

- [x] **桌面集成（AUR 前置）** — 参考 zed 的 bundle-linux：
  - `scripts/gen-icons.mjs`（bun + @resvg/resvg-js）从 logo.svg 生成 hicolor 多尺寸
    PNG（512/256/128/64/48/32），产物在 `resources/icons/hicolor/` 随仓库提交，
    打包无需 node/bun；`just icons` 可重新生成。
  - `resources/aa-player.desktop`（Icon=aa-player、MimeType 常见视频类型，
    已过 `desktop-file-validate`）。
  - `just install [PREFIX]`：本地真实安装（默认 ~/.local），布局与 PKGBUILD 一致
    （bin + share/icons/hicolor + share/applications），装完刷新 desktop/icon 缓存。
  - **依赖升级 ffmpeg-next 8→9**：系统 FFmpeg 升到 9（libavcodec 63）后 8.x 的
    绑定编译失败（AVCodec 公开字段被移除等）；release 全新构建暴露了此问题
    （debug 靠旧缓存一直没触发）。9.0.0 编译通过，测试全绿。

## 待办（未排期）

- [ ] **AUR 上架** — PKGBUILD 双包模板已就绪（`packaging/aur/`，源码包 +
  bin 包，makepkg 校验通过）。流程：打 tag 触发 Release 流水线（v0.1.1 已打）→
  两包 `pkgver` 同步到 0.1.1 并 `updpkgsums` 填校验和 → 发布到 AUR。
  bin 包由 `.github/workflows/release.yml` 在 Arch 容器里构建（与用户系统 ffmpeg 同源）。
  注意：PKGBUILD 里的 `pkgver` 现仍是 0.1.0，改版本号后旧校验和失效，必须重填。
- [ ] **Windows 移植** — gpui Windows 后端 + vcpkg LGPL 共享 ffmpeg +
  exe 资源嵌入（build.rs 已就位）。实验工作流
  `windows-experimental.yml`（手动触发）待跑通；预计要修平台差异 bug
  （音频设备枚举、字体加载、文件对话框等）。
- [ ] **本地交叉编译 Windows exe（cargo-xwin）** — 2026-09-03 POC 结论：
  **暂缓，继续用 GitHub Actions 出 exe**；等官方合并后再切换。POC 进展与阻塞点：
  - 工具链已就绪：`cargo-xwin 0.23.1`（binstall 安装）+ `x86_64-pc-windows-msvc`
    target + clang 22.1.8（clang-cl/llvm-rc/lld-link/libclang 全齐）；
    Windows SDK/CRT 由 xwin 自动下载缓存（~/.cache/cargo-xwin）。
  - ffmpeg 依赖替代方案已验证：BtbN `ffmpeg-n9.0-latest-win64-lgpl-shared`
    （include + MSVC .lib 导入库 + DLL），`FFMPEG_DIR` 指路即可，
    ffmpeg-sys-next 9 的 bindgen 绑定与编译正常——不再依赖 vcpkg。
  - 阻塞 ①（外部）：gpui 的 windows-manifest 资源在交叉编译下 llvm-rc
    找不到 `gpui.manifest.xml`（[zed#62522]），修复 PR
    [zed#62525] 未合并；本地已在 cargo git checkout 打同款 patch 验证可绕过
    （临时，checkout 重建会丢）。
  - 阻塞 ②（自身）——**已修**（commit `33586b9`）：`ffmpeg_log.rs` 的日志回调
    用了 `__va_list_tag`/`vsnprintf`，Windows target 下 bindgen 不生成这两个
    符号。注意这**不是交叉编译的问题**：GitHub Actions 的 MSVC+vcpkg job
    同样挂在它上面（v0.1.1 首次构建即失败）。改为按平台分叉 va_list
    （MSVC 是 `char*`）+ Windows 侧 extern 声明 UCRT 的 `vsnprintf`。
  - 现状：打上阻塞 ① 的临时 patch 后，本地 `cargo xwin build` 能出可用的
    `aa-player.exe`（debug 40MB，正确导入 avcodec/avformat/avutil/avfilter DLL）。
    未做：release 构建、zip 组装（ffmpeg DLL + gpui 需要的 dxcompiler/dxil）。
  - 切换条件：zed#62525 合并进 main 并 bump 我们的 gpui rev，
    届时删掉临时 patch 即可常态使用。

[zed#62522]: https://github.com/zed-industries/zed/issues/62522
[zed#62525]: https://github.com/zed-industries/zed/pull/62525
- [ ] **帧通道容量按分辨率自适应** — FRAME_QUEUE_CAP 现固定 12（1080p 约
  100MB 内存），4K 下偏大；需要把通道创建挪进解码线程按视频尺寸收窄。
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
6. **不要每帧重建 `PlaybackClock`**：音频未出声的启动窗口走墙钟，`origin` 必须在
   首帧定一次。每帧 `with_audio` 重建会把 `origin` 清零 → 画面不受节流提前刷出，
   音频一出声又猛然追赶（实测启动 427ms 卡顿）。换音频时钟柄用 `set_audio`（保留
   origin），并靠 `AudioClockSource` 的 generation 换代才换柄。

详见 [docs/debugging-playback-jank.md](docs/debugging-playback-jank.md)。
