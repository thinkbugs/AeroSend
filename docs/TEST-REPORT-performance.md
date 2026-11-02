# 性能优化验证报告 · AirPlay 镜像流畅度 + 屏幕标注

- 日期：2026-11-02
- 被测项目：
  - `D:\codeX\meeting`（HORIZON 会议中枢，AirPlay 镜像接收端；`target/release/horizon-launcher.exe`）
  - `D:\codeX\AeroSend`（AirPlay 发送端与屏幕标注；`src-tauri/target/release` 下的 release 产物）
- 测试方式：
  - 发送端用 `mirror_e2e` 真实走完 pair-verify → FairPlay → `SETUP stream 110` → AES-CTR 镜像流；
  - 接收端用 release 版 HORIZON 主窗真实呈现（WebView2 + WebCodecs），通过 CDP 读取 `receiver_status` 的引擎计数与 `preview` 计数；
  - 标注用 release 版批注浮层，CDP 在真实画布上派发拖拽（1920×1080 @1x）；
  - 接收端 trace 由 `HORIZON_AIRPLAY_TRACE=1` + `HORIZON_AIRPLAY_TRACE_FILE` 采集。

## 1. 结论

用户反馈的“AirPlay 能投但卡”有两个独立根因，均已修复并在真实收发链路上复测：

1. **接收端每 15 秒把镜像会话挂断一次**（控制连接读超时留在重复句柄上，镜像流本身长时间静默）。表现就是画面周期性地“冻住—恢复”，是最主要的卡顿来源。修复后 26 s / 24 s 的会话都由发送端 `TEARDOWN` 正常结束，不再出现 `i/o error` 断连。
2. **发送端在静态画面上每帧重发旧像素**（“首帧后一直用上一帧”的锁存），把带宽和编码器占用花在没有任何变化的画面上，也违背了“静态屏幕不发帧”的按需机制。改为逐 tick 判断“本帧是否真有更新”，静态桌面回到约 2 帧/秒。
3. 屏幕标注卡顿与笔画数量线性相关：旧实现每次 `pointermove` 全画布重绘全部笔画，40 笔时平均帧间隔 38.8 ms（约 25.8 fps）、34 个长任务。改为增量绘制后同条件 16.66 ms（60 fps）、0 个长任务。

| 检查 | 结果 |
| --- | --- |
| `npx tsc --noEmit`（meeting） | 无错误 |
| `npm run build`（meeting） | 通过 |
| `cargo build --release`（meeting） | 通过（含 `custom-protocol`，release 二进制内嵌前端） |
| `cargo build --release --example mirror_e2e`（AeroSend） | 通过 |
| 30 fps 镜像 26 s | 发 647 帧 / 丢 0；预览绘制 26–29 fps，跳过 0，队列 0 |
| 60 fps 目标 24 s | 发 652 帧 / 丢 0；预览绘制 24–28 fps，跳过 2–5/百帧，队列 0 |
| 静态桌面 12 s | 仅 50 帧被采集送入编码器（约 2 帧/秒，时钟等零碎更新） |
| 标注 40 笔 A/B | 38.8 ms → 16.66 ms；长任务 34 → 0 |

## 2. 测试环境

- CPU i9-14900KF，96 GB；显示输出 AMD Radeon R5 220 @1920×1080 60 Hz（另有 GameViewer 虚拟显示适配器）
- Windows 11 26200；WebView2（ANGLE / D3D11）
- 收发同机（127.0.0.1），发送端 `Resolution::Native`、4 Mbps、`EncoderType::Auto`（x264 ultrafast/zerolatency）
- 运动源：窗口化 `ffplay testsrc2` 图案 + 30 Hz 光标轨迹 + 接收端预览自身的回绘

## 3. 根因与修复

### 3.1 接收端 15 秒断流（主因）

[meeting/crates/horizon-cast/src/airplay/session.rs:1057](file:///D:/codeX/meeting/crates/horizon-cast/src/airplay/session.rs#L1057)

RTSP 控制连接最初设了 15 s 读超时用于防半开连接；会话建立后代码清了超时，但读线程持有的是 `try_clone()` 出来的**另一个句柄**，Windows 上超时是句柄级的，清掉主句柄并不生效。镜像会话全程静默，于是每 15 s 触发一次读超时，读线程退出、会话被判失败，发送端重连后再次在 15 s 处断——用户看到的就是周期性冻结。

修复：首个 RTSP 请求成功应答后，同时清除 `self.stream` 与 `self.reader.get_ref()` 的超时。复测中 26 s、24 s 的会话都在发送端主动 `TEARDOWN` 时以 `session … saw 10 request(s)` 结束，复现日志里的 15.069 s `i/o error` 已不再出现。

### 3.2 发送端静态画面重发旧像素

[AeroSend/src-tauri/src/stream/pipeline.rs:105](file:///D:/codeX/AeroSend/src-tauri/src/stream/pipeline.rs#L105)

原实现用 `have_frame` 锁存：首帧之后即使 `AcquireNextFrame` 超时，也把上一帧再送编码器一次，于是静态屏幕仍以目标帧率持续产生完全相同的画面，接收端白白解码、预览白白重绘，发送端也持续占满编码与网络。

修复：每个 tick 以 `capture_frame_into` 的返回值判断本帧是否新鲜，不新鲜直接 `continue`。静态桌面 12 s 只送了 50 帧（任务栏时钟等零碎更新），按需发送恢复。

### 3.3 标注全量重绘

[AeroSend/src/pages/AnnotationOverlay.tsx:355](file:///D:/codeX/AeroSend/src/pages/AnnotationOverlay.tsx#L355)、[:416](file:///D:/codeX/AeroSend/src/pages/AnnotationOverlay.tsx#L416)

旧实现把所有笔画存在数组里，每次 `pointermove` 清空画布并把**全部历史笔画**重画一遍，代价 O(笔画数) 且随会话增长；荧光笔每次用 `globalAlpha` 叠画也放大了填充成本。

改为三层画布按层增量绘制：

- 墨迹层、荧光笔层各自只追加当前段，只有撤销/清空才全量 `redrawAll()`；
- 未完成图形画在独立的 preview 层，用脏矩形只清上一帧图形占的区域，不再整屏清除；
- 荧光笔以不透明色画在独立层，CSS `opacity: 0.35` 提供半透明，省掉 alpha 合成；
- 橡皮擦用 `destination-out` 直接挖除，宽笔迹 ×4；
- 每个 `pointermove` 消费 `getCoalescedEvents()` 的采样点，逐点连线（[AnnotationOverlay.tsx:416](file:///D:/codeX/AeroSend/src/pages/AnnotationOverlay.tsx#L416)）。

### 3.4 预览呈现策略

[meeting/src/components/MirrorPreview.tsx:432](file:///D:/codeX/meeting/src/components/MirrorPreview.tsx#L432)

验证了“最多缓存 3 帧再按节奏呈现”的方案：绘制率只从 30–34 提到 42–43（被 rAF 上限锁住），但到达—上屏从 11–15 ms 涨到 37–46 ms，得不偿失，已回退为“只保留最新帧，rAF 到点就画、旧帧关闭”的低延迟策略。

### 3.5 构建配置

[meeting/src-tauri/Cargo.toml:52](file:///D:/codeX/meeting/src-tauri/Cargo.toml#L52)

`tauri` 的 `custom-protocol` feature 缺失时，`cargo build --release` 出的是“dev”语义二进制，窗口去加载 `devUrl`（本机 1420 端口），于是 release 包里出现“找不到命令 start_discovery”一类假故障。现在 `default = ["custom-protocol"]`，release 二进制内嵌前端（CDP 打开的是 `http://tauri.localhost/`）。

## 4. 实测数据

### 4.1 屏幕标注（release 浮层，CDP 真实拖拽，1920×1080 @1x）

| 条件 | 旧实现 | 新实现 |
| --- | --- | --- |
| 0 笔（基线） | 帧间隔 16.67 ms | 16.67 ms |
| 40 笔 240 个采样点 | 平均 38.8 ms（≈25.8 fps），p95 50.1 ms，max 66.7 ms | 平均 16.66 ms（60 fps），p95 16.7 ms，max 17.5 ms |
| 长任务 | 34 个（最长 58 ms） | 0 |
| 拖拽总耗时 | 9.8 s | 4.0 s |

0 笔时两者一致，说明差异来自“历史笔画参与每帧重绘”，而不是事件分发或其他固定开销；新实现的耗时与笔画数解耦。

### 4.2 AirPlay 镜像 30 fps（26 s，发送端实测）

```
[24.0s] captured 595 sent 594 dropped 0 | 29.0 fps 1.3 Mbps latency 85 ms | 1920x1080
send wire window: 100 frames gap min 14.5 avg 35.6 max 60.1 ms
summary: captured 648 sent 647 dropped 0 bytes 5.4 MB, last fps 26.2, latency 87 ms
```

接收端引擎（每 100 帧窗口）：

```
mirror window: 100 frames 778 KB, gap avg 35.6 ms max 70.2 ms (short 0 long 96), behind max 0.0 ms, hand-off avg 0.02 ms
```

预览：

```
preview window: paint 26–29 fps, skip 0, queue max 0, arrival to paint avg 8.7–14.8 ms max 17.8–19.9 ms
```

要点：发送端 648/647 帧全出、零丢弃；引擎侧“落后”始终为 0，交接平均 0.02 ms；预览在 27 fps 输入下无跳过、队列恒为 0，到达—上屏约 9–15 ms。会话持续 26 s，正常 `TEARDOWN`。

### 4.3 AirPlay 镜像 60 fps 目标（24 s，发送端实测）

```
[23.0s] captured 624 sent 622 dropped 0 | 28.9 fps 1.7 Mbps latency 102 ms | 1920x1080
send wire window: 100 frames gap min 7.7 avg 33.6 max 86.3 ms
summary: captured 653 sent 652 dropped 0 bytes 3.5 MB, last fps 31.4, latency 119 ms
```

接收端引擎：

```
mirror window: 100 frames 514 KB, gap avg 32.7 ms max 75.2 ms (short 3 long 65), behind max 5.2 ms, hand-off avg 0.02 ms
```

预览：

```
preview window: paint 24–28 fps, skip 2–5, queue max 0, arrival to paint avg 6.1–9.6 ms max 14.7–23.7 ms
```

要点：发送端按 60 fps 目标起拍，但本机桌面合成器在测量窗口里只提供约 22–32 个“新鲜帧/秒”，所以实际出流仍是约 27 帧/秒；延迟降到 64–119 ms（30 fps 为 81–86 ms 量级），预览按 rAF 节奏绘制、只有 2–5 帧因“新帧早于上一帧上屏”被合并。零丢弃，会话正常结束。

> 数据修正：早期记录里“60 fps 下发送 59.9–60.1 fps、采集 63.3 fps”是基于每 tick 计数的旧口径（含重发旧帧的 tick），不是真正的交付帧。换成本次“新鲜帧”口径后，本机受桌面合成器更新率限制，无法在本测试台产生 60 帧/秒的真实桌面变化；编解码与网络侧不是瓶颈（编码器上限 107 fps，AES+TCP 0.41 ms/帧，本次零丢弃）。

### 4.4 静态桌面（12 s，30 fps 目标）

```
summary: captured 50 sent 50 dropped 0 bytes 0.5 MB, last fps 1.7, latency 226 ms
```

约 2 帧/秒被采集送入编码器（任务栏时钟与零碎更新），确认“无变化不发帧”的按需机制生效。修复前同样场景会在 12 s 内灌满约 360 帧重复画面。

### 4.5 预览呈现上限（本机 WebView2）

| 场景 | rAF 频率 |
| --- | --- |
| 窗口未遮挡 | 31.8–32.1 Hz |
| 窗口获得焦点 | 40.5–40.9 Hz |
| 小面积运动窗口 | 47.9 Hz |
| 极轻负载偶发 | 55.9 Hz |
| Canvas 2D 绘制提交 | 0.043 ms |
| `getImageData(1×1)`（GPU 回读） | 44.5 ms |

结论：预览的绘制上限由 WebView2 合成器决定，R5 220 上典型 32–48 Hz。60 fps 输入时“多出来的帧”只会被合并，不会提高上屏率；这也是保留“最新帧优先”策略的原因。

## 5. 诚实边界

1. **60 fps 不是本测试台能端到端验证的吞吐**：桌面更新率只有 22–32 次/秒；手机发送真实 60 fps 时，接收链路能零丢弃地接住（引擎交接 0.02 ms、`behind max` ≤5.2 ms），但上屏仍受 WebView2 rAF 上限（32–48 Hz）约束。
2. **同机回环会出现成对到达**：60 fps 目标时引擎 `gap` 出现少量 `short`（<8 ms）与较多 `long`（>25 ms），而发送端线缆节奏是均匀的（min 7.7 / avg 33.6 / max 86.3 ms），配对发生在同进程调度，不是协议或网络问题；真实跨设备不受此影响。
3. **30 fps 建议作为默认**：30 fps 下预览 26–29 fps、跳过 0、延迟 81–86 ms；60 fps 下延迟降到 64–119 ms，但上屏率不变，仅在低延迟场景（演示、批注联动）有意义。
4. **桌面 DDA 只看得到 DWM 合成内容**：Chrome GPU 叠加层（硬件 overlay/MPO）在本次测试里完全不出现在 `AcquireNextFrame` 的新鲜帧里（同屏 `CopyFromScreen` 能看到画面变化）；软件合成或普通窗口（ffplay、WinForms、光标）则正常。这是采集 API 的已知边界，未在本轮修改。
5. 真机 Apple TV / iPad 互操作（任务 #10）仍缺硬件，本轮所有链路验证为 AeroSend ⇄ HORIZON 同机回环。

## 6. 复现命令

```bash
# 接收端（release，内嵌前端；trace 便于看窗口统计）
HORIZON_AIRPLAY_TRACE=1 \
HORIZON_AIRPLAY_TRACE_FILE=C:\\Users\\<user>\\AppData\\Local\\Temp\\recv-trace.log \
WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9333 \
D:/codeX/meeting/target/release/horizon-launcher.exe

# 发送端：127.0.0.1:5000、时长(秒)、目标帧率
cd D:/codeX/AeroSend/src-tauri
cargo build --release --example mirror_e2e
HORIZON_SEND_TRACE=1 target/release/examples/mirror_e2e.exe 127.0.0.1:5000 26 30

# 标注 A/B：release 浮层 + CDP 拖拽
node %TEMP%/cdp-stroke.mjs
```

## 7. 本轮改动文件

- `meeting/crates/horizon-cast/src/airplay/session.rs`：清除镜像会话控制连接读超时（15 s 断流修复）；`MirrorWindow` 增加 `short`/`long` 间隔分桶。
- `meeting/src/components/MirrorPreview.tsx`：保持“最新帧优先”策略（队列方案已回退）。
- `meeting/src-tauri/Cargo.toml`：`default = ["custom-protocol"]`。
- `AeroSend/src-tauri/src/stream/pipeline.rs`：逐 tick 新鲜帧门控，去掉旧帧重发。
- `AeroSend/src-tauri/src/stream/mirror.rs`：`HORIZON_SEND_TRACE` 线缆节奏统计。
- `AeroSend/src/pages/AnnotationOverlay.tsx`：三层画布增量绘制、脏矩形预览、合并指针采样、`destination-out` 橡皮擦、不透明荧光笔层。
- `AeroSend/src/styles/annotation.css`：三层画布的 `pointer-events` 与荧光笔层 `opacity: 0.35`。

## 8. 未覆盖

- 真机 Apple TV / iPad 发送端的 60 fps 实测（需硬件）。
- 长会话（>10 分钟）内存与丢帧曲线；本轮最长 26 s。
- Chrome 硬件叠加层内容的采集（DDA 可见性边界，见 5.4）。
