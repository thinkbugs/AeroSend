# AeroSend 测试报告 · Phase 4（音频投射）+ Phase 5（DLNA 视频流推送）

- 日期：2026-11-02
- 被测项目：`D:\codeX\AeroSend`（Windows AirPlay/DLNA 发送端，独立于 `D:\codeX\meeting` 接收端）
- 测试机：i9-14900KF / RTX 5080 / 96 GB / Windows 11 26200 / Node 24.18 / Rust stable（`cargo test` debug profile）
- 对端：HORIZON 会议白板启动器的 DLNA 渲染器（`dlna-probe`，独立代码库 + 独立解码器），以及项目内置的 fake DMR / fake AirPlay 接收端

## 1. 结论

Phase 4、Phase 5 的链路已打透并验证通过：**110 个自动化用例全绿**，另有 **2 个跨实现互操作用例**在拉起对端后全绿，DLNA 页面在**真实应用窗口**内完成一次从原生文件对话框到播放的完整走查。

| 层次 | 用例 | 结果 |
| --- | --- | --- |
| 单元（`cargo test --lib`） | 99 | 通过 |
| 集成 · fake 对端（capture / pipeline / audio / session / upnp） | 11 | 通过 |
| 互操作 · HORIZON 真机渲染器（`#[ignore]`） | 2 | 通过（需先启动对端） |
| 真实窗口 UI 走查（CDP 驱动 WebView2） | 1 轮完整流程 | 通过 |
| `npx tsc --noEmit` | — | 无错误 |

## 2. 音频链路（Phase 4）

`test_session_e2e` 在 127.0.0.1 上起一个 fake AirPlay 接收端，跑完整会话（OPTIONS→ANNOUNCE→SETUP→RECORD→TEARDOWN），4 秒实采。本轮输出：

```text
session stats: 120 frames captured, 0 dropped, 119 units sent, 167 audio frames,
               30.0 fps, 1.2 Mbps, 79.9 ms latency, 1920x1080
video: 2176 packets -> 119 access units, 119 RTCP reports
audio: 167 packets, 167 RTCP reports
```

断言覆盖：SDP 同时声明 `m=video` 与 `m=audio`；RTP 版本/载荷类型/单 SSRC；每包 ≤ 12+1400（MTU 预算）；每个 AU 恰好一个 marker 且至少一个 slice；视频与音频时间戳单调递增；RTCP SR（PT 200）存在；音频帧为 44.1 kHz AAC、时间戳按 1024 步进推进。

**环境依赖（本轮实测确认，不是推测）**：WASAPI loopback 只镜像渲染设备上正在播放的声音，空闲机器上采集结果恒为 0 包。因此 `test_audio.rs` 与 `test_session_e2e.rs` 现在自带声源（`ffplay` 播 440 Hz 正弦），并由 fixture 负责回收进程。修复前该用例在小憩的机器上会假失败。

## 3. DLNA 推送（Phase 5）

### 3.1 集成层（fake DMR，`test_upnp_e2e.rs`，3 例）

覆盖 SSDP M-SEARCH→响应、设备描述 XML 解析、AVTransport/RenderingControl/ConnectionManager 三类 SOAP 调用、DIDL-Lite 组装、以及**接收端真的按 HTTP Range 拉字节**：

- `a_local_file_casts_and_the_renderer_pulls_the_bytes_it_asked_for`：按 `res size` 分段拉取，逐字节比对，含 206/`Content-Range`/416 行为、时钟推进、Seek、音量。
- `casting_a_url_the_renderer_reaches_itself_needs_no_local_file`：URL 直投不经过本地媒体服务器。
- `a_device_that_refuses_a_cast_reports_its_own_reason`：SOAP Fault 原文回传 UI，不被吞掉。

### 3.2 互操作层（HORIZON 真机渲染器，`test_interop_hub_dmr.rs`，2 例，`#[ignore]`）

对端是另一个代码库、另一个解码器，因此这一层证明的是"协议真的对"，而不是"我们的两边自洽"。本轮输出：

```text
peer    HORIZON Cast Receiver  http://127.0.0.1:8290/ctl/AVTransport
offered aerosend-interop-46496.wav (352844 bytes) as http://127.0.0.1:61335/m/f2b8d6a3
served  352985 bytes so far
clock   1.00s of 4.00s as the hub measures it
sinks   13 protocol entries
interop ok: hub 127.0.0.1:8290 served, decoded, driven and stopped

offered aerosend-interop.mp4 (166110 bytes) as http://127.0.0.1:61282/m/3c9af818 — mime video/mp4
[  1623ms] the hub to start decoding
clock   1.00s of 6.00s as the hub measures it
interop ok: hub 127.0.0.1:8290 decoded and drove an MP4
```

断言要点：时长由**对端自己解析**得到（WAV 4.00 s / MP4 6.00 s，非我们写入的值）；Pause 后 1.5 s 内时钟冻结；Seek(2 s) 对端跟上；`SetVolume` 由 `GetVolume` 读回；Stop 后双方状态一致；MP4 用例要求发送字节 ≥ 文件字节。

复现命令：

```bash
# 终端 A：D:\codeX\meeting
cargo run --release --bin dlna-probe -- --seconds=240 --port=8290 --headless
# 终端 B：D:\codeX\AeroSend\src-tauri
cargo test --test test_interop_hub_dmr -- --ignored --nocapture --test-threads=1
```

### 3.3 真实窗口 UI 走查

通过 WebView2 的 `--remote-debugging-port=9333` 用 CDP 驱动**实际渲染的窗口**（不是 jsdom，也不是源码 grep），逐项验证控件→命令→设备状态：

| 步骤 | 结果 |
| --- | --- |
| 侧栏进入 DLNA Cast | 通过 |
| 扫描 / 按地址添加（`127.0.0.1:8290`） | 通过，列表出现 `HORIZON Cast Receiver` + `AVTransport` 徽标 |
| 选择渲染器 | 通过，`Cast a file…` 解除禁用 |
| `Cast URL`（局域网 fixture 的 6 s 片段） | 通过，状态 `PLAYING`，设备时钟 0:01→0:06 |
| Pause / Play | 通过，`PAUSED_PLAYBACK` ↔ `PLAYING` |
| 音量 | 通过，显示设备自报的 30% |
| Stop | 通过，回到 "Idle. Choose a file or paste a stream URL." |
| Forget | 通过，回到空态并给出按地址添加的引导 |
| **原生文件对话框 → `cast_file`** | 通过，见下 |

原生选择文件这一段（本轮补齐）：

```text
nav -> cast page
scan clicked: true
renderer -> HORIZON Cast Receiver http://127.0.0.1:8290 AVTransport
renderer selected
picker asked for
driver: ["dialog 0x410714","name edit 0x361336 open button 0x261982",
         "field now: [C:\\Users\\…\\Temp\\aerosend-interop.mp4]","clicked open",
         "dialog still alive=False"]
state: PLAYING
title : aerosend-interop.mp4PLAYING
clock : 0:00 / 0:00 / 30%
clock+2.5s: 0:02 / 0:06 / 30%
served: 702.4 kB served from this PC · http://127.0.0.1:63118/m/3c9af818
offered: Being served aerosend-interop.mp4 video/mp4 · 166.1 kB · http://127.0.0.1:63118/m/3c9af818
error : null
```

即：点击 `Cast a file…` 弹出的是 Windows 原生选择框（标题为 `pick_media_file` 设置的文案），选定文件后 `cast_file` 自动接管，媒体服务器开始按 Range 供字节，对端解码并把播放状态推回本界面。截图见 `docs/shots/dlna-cast-playing.png` 与 `docs/shots/dlna-cast-page.png`。

## 4. 本轮发现并修掉的问题

| # | 现象 | 根因 | 处理 |
| --- | --- | --- | --- |
| 1 | 互操作用例里 `GetProtocolInfo` 返回 0 条 | 该动作属于 **ConnectionManager**，我们把它 POST 到了 AVTransport 的 URL；测试又用 `unwrap_or_default()` 把设备拒绝吞成了"空列表" | 参数改名 `cm_control_url` 并写明原因，测试改为取 ConnectionManager 的 control URL 且 `.expect(...)` → 13 条 |
| 2 | 音频用例偶发"0 packets" | 不是回归：WASAPI loopback 在渲染设备空闲时本就无声 | 用例自带 `ffplay` 声源并负责回收 |
| 3 | `NotCastable` 报错里是内部 id，用户看不懂 | 错误信息直接带了 renderer id | 改为携带设备名 `r.name` |
| 4 | `npm run tauri dev` 启动即退出（`EBUSY … aerosend.exe`） | vite 监听了 `src-tauri/target`，Cargo 正在重写 exe | `server.watch.ignored` 排除 `src-tauri/target/**` 与 `dist/**` |
| 5 | Cast 页地址行溢出卡片 | input 的 placeholder 给了它 min-content 宽度，`flex:1` 压不下去 | `.cast-add-row .form-input { min-width: 0 }` |
| 6 | WAV 用例期望 `audio/x-wav` | 我们的映射是 `audio/wav`（class 判定按 `audio/` 前缀，仍为 `audioItem`） | 修正测试期望 |

## 5. 环境限制与未覆盖项（如实记录）

- **未与真实 Apple TV / 商用电视验证**。AirPlay 侧对端是 HORIZON 接收端与 fake 接收端，DLNA 侧对端是 HORIZON 渲染器；跨厂商互操作仍需一台真实设备在场。
- **测试期间控制台处于锁屏状态**（实测：`LogonUI.exe` 在跑，`SetForegroundWindow` 被拒，前台窗口为"Windows 默认锁屏界面"）。因此 `SendKeys` 这类依赖前台的注入不可用；本轮改用 `EnumChildWindows` 定位对话框的 `Edit`(id 1148) 与 `Button`(id 1)，用 `WM_SETTEXT` + `BM_CLICK` 完成选择——这仍然走的是原生对话框自己的消息循环，不是绕过它。
- **"served bytes" 是累计发送量**：包含对端为探测时长、seek、重播发起的多次拉取，因此可能大于文件本身大小（本轮 702.4 kB / 166.1 kB）。文案已写明 "served from this PC"。
- 锁屏状态无法采集"人眼看着电视画面"的这一路，音画同步的主观评价留待真机复测。
- 单元测试跑在 debug profile；发布前建议补一轮 `--release`。

## 6. 复现全套

```bash
cd D:\codeX\AeroSend\src-tauri
cargo test                                            # 110 通过 / 2 忽略
cargo test --test test_interop_hub_dmr -- --ignored --nocapture --test-threads=1   # 需先起对端
cd D:\codeX\AeroSend && npx tsc --noEmit              # 前端类型
```
