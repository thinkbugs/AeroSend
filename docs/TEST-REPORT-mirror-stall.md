# AeroSend 投屏中断（os error 10053）根因定位与修复测试报告

- 日期：2026-11-02
- 现象：AeroSend 投屏过程中界面出现红色错误条
  `mirror video write failed: 你的主机中的软件中止了一个已建立的连接。(os error 10053)`，会话断开。
- 被测构建：安装版 0.1.0（修复前基线）与 `D:\codeX\AeroSend\src-tauri\target\release\aerosend.exe`（修复后）
- 接收端：本机会议中枢 `D:\codeX\meeting\target\release\horizon-launcher.exe`，AirPlay 接收名「Hub测试机」，
  设备 id `192.168.0.27:5000:airplay`（RTSP 5000）
- 机器：i9-14900KF / RTX 5080 / 96 GB / Win11 26200

## 1. 结论

镜像写入线程原本把编码帧直接交给**阻塞 TCP socket**。接收端一旦停止读取（进程被挂起、网络中断、
接收端被调度饿死），socket 发送缓冲填满，`write` 卡在内核里，整条采集→编码链路随之冻结；本地积压
靠丢帧计数被掩盖，界面 `error` 始终为空。连接一直挂着直到 Windows 协议栈自己放弃，
报 `WSAECONNABORTED (10053)`，会话以一条看不出原因的错误结束。

修复把写侧改成**非阻塞排队 + 水位丢弃 + 停滞判死**：socket 永不阻塞采集线程；积压到 2 MB
就拒绝新帧（计入新增的「网络丢帧」指标）；连续 15 秒没有任何字节被 socket 接收则用中文
明确报「接收端 15 秒未接收任何数据，网络已中断或接收端已关闭」并干净结束会话。

> 本文档现在覆盖两次不同的“中途断掉”：§1–§7 是写侧阻塞导致协议栈中止连接（10053），§8 是开始投屏时弹出
> 终端窗口，§9 是接收端把还在收帧的镜像连接拆掉导致的 `the mirroring socket refused data`。

## 2. 根因证据（同机、同注入手法的前后对比）

注入手法：会话跑到 ~30 fps 稳定后，用 `NtSuspendProcess` 挂起接收端进程（socket 保持打开但不再读取），
一段时间后 `NtResumeProcess` 恢复。采样：`D:\codeX\meeting\.probe\sample-stall.mjs` 通过 WebView2 CDP
（发送端 9333 / 中枢 9334）每 0.5 s 读取 `get_stream_stats` 与 `receiver_status`，落到
`C:\Users\Public\cast10053\stall-sample.jsonl`。鼠标移动脚本 `jiggle130.ps1` 提供画面内容，
否则静止画面不产帧。

### 2.1 修复前（安装版 0.1.0，`C:\Users\Public\cast10053\sample.jsonl`）

| 观察项 | 数据 |
|---|---|
| 挂起时刻 | t ≈ 29.2 s |
| 采集计数 | 冻结在 `captured=8824`，220 s 后仅到 8958（≈0.06 fps） |
| 发送字节增量 | 挂起期间 `dBytes=0`（线程卡在 `write` 里） |
| 本地丢帧 | `framesDropped` 0 → 1830（积压的表现形式被记成“丢帧”，与网络无关） |
| 界面延迟读数 | 冻结在 114 ms，随后跳到 1048 ms 一档 |
| `error` | **全程 null**，用户看不到任何原因 |
| 恢复后 | 0.5 s 内最大 1 026 585 字节的突发（把挂起期间的历史画面整段补发） |
| 会话结束 | t ≈ 251 s，协议栈主动中止连接 → `os error 10053` |

采集冻结 + 字节零增长 + 无错误，这三件事同时成立只能是写线程阻塞在内核；`10053` 是**本端**栈的错误码
（对端 RST 会是 `10054`），印证“对端不再取数据、本端超时放弃”，而不是“对端关闭”。

### 2.2 修复后（`target/release`，`stall-sample.jsonl` / `stall-run*.log`）

| 场景 | 停滞时长 | 结果 |
|---|---|---|
| A | 8 s | 会话存活。`framesStalled=0`、`framesDropped=0`、30 fps 不间断——socket 缓冲与 2 MB 队列把停滞整段吸收，用户无感 |
| C | 14 s | 会话存活。水位生效，`framesStalled` 0 → **183**（≈停滞吸收容量用尽后的 6 s × 30 fps），`framesDropped` 仍为 0，采集与编码不受影响，恢复后接收端继续正常出帧 |
| B | 30 s | 会话在停滞约 **23 s** 时报 `mirror video write failed: 接收端 15 秒未接收任何数据，网络已中断或接收端已关闭` 并结束。**未出现 `10053`** |

场景 B 的 23 s = socket/对端缓冲把队列填满所需时间（本机回环 ≈8 s）+ `MAX_STALL` 15 s，
这与设计一致：判死从「最后一个字节被 socket 接收」起算，而不是从对端停止读取起算。

### 2.3 产品界面复核（不读日志，只读 DOM）

会话通过点击设备卡片（走 `connectToDevice` → `stream_start` 的正常路径）建立，再次挂起接收端 18 s，
在挂起期间与恢复之后分别读取渲染出来的 `.stat-item` 与 `.error-bar`：

| 时刻 | 界面读数 |
|---|---|
| 停滞中 | `帧率 30.0`、`丢帧 0`、`网络丢帧 179`，错误条为空 |
| 恢复后 | 错误条 `mirror video write failed: 接收端 15 秒未接收任何数据，网络已中断或接收端已关闭` |

停滞期间帧率仍报 30、「丢帧」仍为 0 而「网络丢帧」上涨，正是修复想要的形状：采集与编码不再被
socket 拖住，被拒的帧单独归因到网络侧。另一次 12 s 挂起在界面上完全无感（`网络丢帧 0`、无错误条），
说明短时抖动已经不会再打断投屏。

注意：画面静止时编码器只出 ~1 fps，此时停滞连队列都填不满，界面上不会出现「网络丢帧」；
上表的数据是在有鼠标移动（内容持续变化）的条件下采集的。

## 3. 改动清单

| 文件 | 改动 |
|---|---|
| `src-tauri/src/airplay/mirror.rs` | `MirrorWriter` 重写为排队写：`set_nonblocking`、`pump()`（一次把 socket 肯收的字节全部交出，绝不阻塞）、`queued()`/`stalled_for()` 停滞时钟、`MAX_QUEUED_BYTES = 2 MiB` 水位；`write_video` 返回 `Result<bool>`，**在过密码之前**先判水位 |
| `src-tauri/src/stream/mirror.rs` | `MAX_STALL = 15 s`；发送循环每轮先 `pump()` 再查停滞；`Ok(false)` 计 `frames_stalled` 并丢弃该帧（首个拒绝打一条 warn，说明已排多少字节）；会话开始时对 RTSP 控制连接发一次 keepalive |
| `src-tauri/src/airplay/control.rs` | 新增 `Control::keepalive()`：只写不读同步等待，避免在-paced 路径上放置阻塞读 |
| `src-tauri/src/stream/session.rs`、`src-tauri/src/commands/stream.rs` | 新增 `frames_stalled` 统计并透传到前端 |
| `src/types.ts`、`src/pages/StreamView.tsx` | 界面新增「网络丢帧」读数，与本地「丢帧」区分：前者说明网络/接收端落后，后者说明本机编码积压 |
| `src-tauri/src/capture/encoder.rs` | 两处裸 `Command`（`-encoders` 探测与逐编码器试开）加 `.creation_flags(CREATE_NO_WINDOW)`，开始投屏时不再弹控制台窗口（详见 §8） |
| 会议中枢 `src-tauri/src/firewall.rs` | `netsh ... show rule` 同样加 `CREATE_NO_WINDOW`，SnapDrop 面板检测防火墙时不再闪窗 |

为什么水位检查必须在 AES 之前：镜像连接只有一条 AES-128-CTR keystream，跨帧连续推进
（`airplay/mirror.rs` 顶部注释）。先加密再丢弃会让接收端从下一帧起全部解密成噪声，
代价比丢一帧大得多。

## 4. 回归测试

```
cargo test --lib                     → 159 passed; 0 failed
cargo test --lib mirror::            → 11 passed（含新增用例）
npx tsc --noEmit                       → 无输出（通过）
```

新增用例 `airplay::mirror::tests::a_refused_frame_leaves_the_keystream_aligned`：
真实 loopback 连接 + 后台读取线程，先送一帧并排空，再把队列伪造到水位（回环 socket 愿意吞多少字节
由操作系统决定，不适合用来断言水位），确认被拒的帧**不计数、不计字节、不入队**，随后清空积压再发该帧；
最后逐帧解析线上数据，用独立派生的 `AesCtr` 按“只见过被接受帧”的顺序解密比对。
若被拒帧曾推进过 counter 或打乱过成帧，比对必然错位。

## 5. 已知边界与未验证项

1. **真实无线路径未测。** 本轮全部为同机回环：另一台接收端（192.168.0.67）端口被过滤，也没有真机 Apple
   设备。回环能吸收 ~8 s 停滞，真实 WLAN 更小，因此场景 C 的“14 s 才丢帧”在真实网络上会更早进入
   网络丢帧；不变的是采集不阻塞、超限即诚实报错。
2. **接收端拆流风险（原第 5 节，已于 2026-11-03 修复）。** 会议中枢的会话线程阻塞在 RTSP 控制连接的读上，
   读一旦结束就从 `crates/horizon-cast/src/airplay/session.rs`（`self.mirror = None`）拆掉**正在直播**的镜像
   连接；发送端又只在会话开始发一次 keepalive，控制连接整场静默。修复见 §9：发送端改为每 5 s 保活，
   接收端在镜像连接自己结束之前不拆流。同机 12 min 空闲控制连接观测未复现回收（`break1/long-sample.log`
   跑到 742 s 仍 `err:null`），所以这条路径的单测与真机长会话是分别取证的两件事。
3. 停滞期间发送端 `latency` 读数会出现 1 s 一档的跳变：它是采集→最后一个包出去的时间，
   静止画面（无内容变化）本身也会推高它，不能单独作为网络质量指标；判断网络问题请看「网络丢帧」。

## 6. 交付

NSIS 安装包由 `npm run tauri build` 重新产出，资源映射仍把 `ffmpeg.exe` 放在主程序同目录：
`D:\codeX\AeroSend\src-tauri\target\release\bundle\nsis\AeroSend_0.1.0_x64-setup.exe`
（29.67 MiB / 31 107 459 字节，11-03 00:39，含 §8 的窗口修复与 §9 的保活/宽限修复）
（安装后需重新安装以覆盖 `%LOCALAPPDATA%\AeroSend\aerosend.exe`）。

## 7. 复现步骤

```
# 1. 起中枢接收端 + 修复后的 AeroSend（两个 WebView2 分别监听 9334 / 9333）
powershell -File C:\Users\Public\launch-fixed.ps1

# 2. 中枢开启 AirPlay 接收（设备名会广播给发送端）
node .probe/evalp.mjs 9334 'window.__TAURI_INTERNALS__.invoke("airplay_start",{name:"Hub测试机"})...'

# 3. 画面内容源（静止画面不产帧，测不出停滞）
powershell -File C:\Users\Public\jiggle130.ps1

# 4. 采样会话两端
node .probe/sample-stall.mjs 130

# 5. 注入停滞（可配时长）
powershell -File C:\Users\Public\stall-once.ps1 -HubPid <horizon-launcher pid> -StallSeconds 18 -HoldSeconds 25
```

数据落在 `C:\Users\Public\cast10053\`：`stall-sample.jsonl`（修复后）、`sample.jsonl`（修复前基线）、
`stall-run*.log`、`send-stderr.log`（`HORIZON_SEND_TRACE=1` 的线上帧间隔）、`recv-trace.log`（中枢 RTSP/镜像跟踪）。
挂起与恢复用 `NtSuspendProcess` / `NtResumeProcess`（`C:\Users\Public\pause.ps1`、`stall-*.ps1` 内联），
接收端进程保持存活但完全不读取 socket，这就是触发原 `10053` 的条件。

本轮为测试关闭了我此前启动的两个带 CDP 的实例（`aerosend.exe`、`horizon-launcher.exe`）与鼠标移动脚本；
用户自己安装的正式版 AeroSend 未改动，仍是修复前构建，需要用上面新产出的安装包覆盖安装。

## 8. 附带修复：开始投屏时弹出多个终端窗口（2026-11-02 23:10 复测）

### 8.1 现象与根因

用户报「开始投屏时，会弹出多个终端窗口」。根因在编码能力探测，不在推流：
`capture/encoder.rs` 用两次裸 `std::process::Command` 直接问 ffmpeg——
一次 `-hide_banner -encoders` 列编码器，一次对每个候选编码器（`libx264`/`h264_nvenc`/`h264_qsv`）
真喂一帧看它能否打开。AeroSend 是 GUI 子系统进程，本身没有控制台可继承，
于是 Windows 为每个被拉起的 `ffmpeg.exe`（控制台子系统程序）新建一个控制台窗口；
`EncoderType::Auto` 在**每次投屏第一次建编码器时**触发 `PROBE`，界面尚未出画就先后闪出这几个窗口。

真正推流的两个 ffmpeg（视频编码器、音频重采样）走 `FfmpegCommand`，该 crate 自带
`CREATE_NO_WINDOW`，所以它们从来不弹——这也解释了为什么只在“开始的一瞬”弹几个就消失。

### 8.2 改法

`capture/encoder.rs`：加 `const CREATE_NO_WINDOW: u32 = 0x0800_0000`，对两处裸 `Command` 调用
`.creation_flags(CREATE_NO_WINDOW)`。行为不变（探测仍照常进行，编码器选择结果一致），
只是不再分配控制台窗口。会议中枢侧同类问题一并处理：`src-tauri/src/firewall.rs` 的
`netsh advfirewall ... show rule` 同样是 GUI 进程拉控制台程序，也已加上同一标志，
中枢 release 已重新链接（`D:\codeX\meeting\target\release\horizon-launcher.exe`，23:19，6 751 232 字节）。

### 8.3 同机前后对比（一次真实投屏，非源码推断）

测量手法：`C:\Users\Public\win-poll.ps1` 以 20 ms 轮询顶层可见窗口，按「窗口属主是
conhost/OpenConsole/WindowsTerminal/cmd 且标题含 ffmpeg」判定控制台窗口，同时用
`Win32_Process` 记录每一个被拉起的 `ffmpeg.exe` 及其命令行作为正锚；
`C:\Users\Public\measure-cast.ps1` 启动被测构建、经 CDP 调 `stream_start` 打到中枢镜像设备
`192.168.0.27:5000:airplay`。日志在 `C:\Users\Public\winmeas\`。

| 构建 | 探测是否真的跑完 | 采集到的 ffmpeg 进程 | 控制台窗口 |
|---|---|---|---|
| 安装版 21:32（修复前） | `x264=true nvenc=false qsv=false`（23:10:33.142） | 6：`-encoders`、`libx264`、`h264_nvenc`、`h264_qsv` 探测 + 视频编码器 + 音频编码器 | **有**：23:10:33.124 `pid=44528 name=WindowsTerminal class=CASCADIA_HOSTING_WINDOW_CLASS title=...\AeroSend\ffmpeg.exe` |
| `target/release`（修复后） | `x264=true nvenc=false qsv=false`（23:03:26.964） | ≥4：同一批探测 + 视频编码器 + 音频编码器（两条极短命的探测进程命令行来不及读） | **无**（`windows-fixed.log` = `none`） |

两侧探测都完整跑完（同一行 `x264/nvenc/qsv` 结果、同一编码器 `libx264`），
差别只在窗口：修复前那一刻确实出现控制台窗口，修复后同一条路径零窗口。
本机默认终端是 Windows Terminal，四个控制台被折进同一个 hosting 窗口，故窗口级去重后计到 1 个；
按进程数看，用户看到的「多个」就是这 4 次探测各要一个控制台。

镜像链路本身未受影响，修复后一次会话读数：`fps=30`、`framesStalled=0`、`error=null`。

### 8.4 复现/复测

```
# 起中枢接收端后：
powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\Public\measure-cast.ps1 -Tag fixed
# 修复前基线用安装版可执行文件：
powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\Public\measure-before.ps1
# 结果看 C:\Users\Public\winmeas\windows-<tag>.log（窗口）与同目录 .spawns.log（ffmpeg 进程锚点）
```

安装包已用含本修复的构建重新产出（见 §6），覆盖安装后开始投屏不再出现终端窗口。

### 8.5 中枢侧回归（同轮）

```
cargo check --workspace --all-targets   → 通过
cargo test --workspace                  → 271 passed; 0 failed（horizon-cast 216 + horizon-launcher 55）
```

顺带修掉一处与本轮无关、但会让 `cargo test --workspace` 直接编译失败的旧问题：
`crates/horizon-cast/examples/engine_probe.rs` 里 `WNDCLASSW.lpfnWndProc` 直接传 `DefWindowProcW`、
`hInstance` 直接用 `GetModuleHandleW(None)?`，在当前 `windows` 版本下分别要求
`unsafe extern "system" fn` 与 `HINSTANCE`。改为一个转发的 `wndproc` 并对句柄做 `.into()`。
另需更正：本轮早些时候口头报过中枢「842 passed」，那不是本次可复算的数字，实际以上面的 271 为准
（此前也没有任何文档写下 842，仅出现在对话里）。


## 9. 中途断流「the mirroring socket refused data」根因与修复（2026-11-03 00:39 复测）

### 9.1 现象与根因

用户截图是发送端错误条：`mirror video write failed: the mirroring socket refused data`。这句话只可能来自
`src-tauri/src/airplay/mirror.rs:283`——`pump()` 把 `WouldBlock`（背压，丢最新帧继续）和硬写错误分开，硬错误才
走这一支。也就是说：**接收端已经关闭了那条一秒钟前还在收帧的镜像数据连接**，不是网络慢，也不是编码器出问题。

接收端唯一会主动关掉「正在直播」的镜像连接的路径在会议中枢：会话线程阻塞在 RTSP 控制连接的读上，读一旦结束
（EOF 或出错）就走到 `self.mirror = None`，把镜像监听连同它手上的 socket 一起拆掉。

而镜像会话期间控制连接是完全静默的：发送端只在 RECORD 之后发一次 OPTIONS，之后再不碰它。静默连接正是
NAT / 防火墙 / AP 空闲回收的对象（不同设备的空闲超时从 30 s 到 10 min 都有）。回收之后控制连接报错 → 中枢拆
镜像 → 发送端下一帧写失败，用户看到的就正好是“投屏一会又报错断掉了”。

同机取证（`C:\Users\Public\break1\long-sample.log`）：控制连接静默 742 s（12 min）两端都健康，`err:null`、
`stalled:0` ——本机的本地路径不做空闲跟踪，所以这一条在本机复现不出来；真实 WLAN 路径仍未测（见 9.5）。

另外，UI 只显示 anyhow 最外层文本，真实 socket 错误码（10053 / 10054）被吞掉。已在
`src-tauri/src/stream/mirror.rs:322`、`:343`、`:355` 三处改成 `{e:#}`，以后现场报错能直接看到是哪一种。

### 9.2 改动（两端互补）

发送端——控制连接自己有心跳，不再依赖画面是否在动：

- `src-tauri/src/airplay/control.rs:36` `KEEPALIVE_INTERVAL = 5 s`；`:107` `Pacer::beat()` 发一条 OPTIONS
  **并把响应读走**（只写不读，接收端的应答会在发送端接收缓冲里越积越多，长会话最终把窗口压死）；
  `:178` `Control::pacer()` 交出读写两半；CSeq 改成 `Arc<AtomicU32>`，保活与握手共用一条计数器，不断号也不重号。
- `src-tauri/src/stream/mirror.rs:251` `spawn_keepalive()`：独立线程，每 5 s 一跳。**没有挂在 send_frames 上**
  ——静止画面根本不出帧，跟着帧走的保活恰好会在最该跳的时候静默。发送端 `TEARDOWN` 之前先 join 该线程，
  避免同一 socket 上两个读者把应答读成半截。

接收端——控制连接没了不等于发送端走了：

- `crates/horizon-cast/src/airplay/mirror.rs:469` `connection_open()`（accept 循环进入连接时置真、`serve`
  返回时置假），`:486` `wait_for_connection_to_end()`：连接开着就每 250 ms 看一次帧计数，帧在涨就一直等，
  60 s 完全没有帧才收（那是网络静默死亡、连 FIN/RST 都没发的情形）。
- `crates/horizon-cast/src/airplay/session.rs:1041` `wait_for_mirror_to_end()`，在控制读的**两个出口**都调用：
  `:1066`（读报错、走 `return Err`）与 `:1176`（EOF / `TEARDOWN` 之后走 break）。原先只有 break 这条路，
  而 AP 回收多数给的是 RST，正好落在被跳过的那条上。

### 9.3 单元证据

新测试 `a_lost_control_connection_does_not_hang_up_a_live_mirror`（中枢 session.rs）：完整握手 → RECORD →
codec config → 一个加密 IDR 到达 sink；然后 `shutdown(Both)` 打断**控制**连接，镜像连接原样保留；紧接着的
第二帧仍然到达 sink（断言 `pts_us=1_235_500_000`、非关键帧、`data=[0,0,0,1,0x41,9,8,7]`，即这确实是第二帧
而不是第一帧的残留），并且 `in_session` 仍为真；只有关掉镜像连接本身会话才结束，`report.mirror_frames == 2`。

**证明这条测试真的在测这条路径**：把 `wait_for_mirror_to_end()` 临时改成空实现重跑，该测试立刻红
（session.rs:3371 的 sink recv 超时），同时原握手测试仍绿。恢复实现后两者都绿。

发送端 `src-tauri/src/airplay/control.rs` 新增两条：`a_pacer_beats_the_control_connection_and_keeps_counting`
用一个会应答的假接收端记录它看到的 CSeq，OPTIONS → beat → beat → TEARDOWN 恰好得到 `1,2,3,4`（不断号、
不重号，且保活的应答确实被读走）；`a_beat_fails_when_the_receiver_has_closed` 让假接收端 accept 后立刻关闭，
beat() 必须返回 Err——只写不读的保活在这里会假报成功。

全量回归：发送端 `cargo test --lib` **161 passed / 0 failed**（原 159 + 新 2）；中枢
`cargo test --workspace` **272 passed / 0 failed / 6 ignored**（lib 217 + 集成 55，原 271 + 新 1）。
顺带把中枢里那条 350 行的镜像握手测试抽成 `a_mirroring_sender()` 构建函数，两条测试共用，断言一条没少。

### 9.4 两端真机构建实测（00:41–00:46，新产物）

- 会话 311.9 s（5.2 min）全程无错误：采样到 `{"t":271,"fps":0,"stalled":0,"err":null}`；发送端结束统计
  `3755 frames captured, 3755 pictures sent, 0 stalled at the socket`；`stopped answering` / `refused data`
  各 **0** 次。
- 控制连接不再静默：中枢 trace 共 74 条请求，其中 **63** 条是 `<- OPTIONS / RTSP/1.0` 保活（另外 11 条是 10 条
  握手请求加最后的 `TEARDOWN`）。整场 CSeq 1…74 连续不重号。保活第一跳在 RECORD 之后 1.313 s，最后一跳在
  311.335 s，62 个间隔平均 5.000 s、最小 5.000 s、最大 5.001 s。jiggle 停止后 fps 归 0，保活照跳不误——
  这就是“不依赖帧到达”的正向锚点。
- 停止投屏没有被宽限拖住：00:46:37.330 触发 `stream_stop` → trace 311.761 `mirror data connection from
  192.168.0.27:52361 ended` → 311.942 `TEARDOWN` → 311.948 会话结束，**505 ms**。主动停止时发送端先关数据连接
  再发 TEARDOWN，所以宽限立刻返回，不会退化成等满 60 s。

```
# 复现本轮实测
powershell -File C:\Users\Public\keepalive-check.ps1 -MaxSeconds 240   # 起两端、投屏、采样、统计保活间隔
powershell -File C:\Users\Public\break2\stop-check.ps1                  # 停止并测接收端察觉耗时
# 单测
cd D:\codeX\meeting && cargo test --workspace
cd D:\codeX\AeroSend\src-tauri && cargo test --lib
```

数据在 `C:\Users\Public\break2\`（`recv-trace.log`、`sample.log`、`send-stdout.log`、`stop-output.log`）与
`C:\Users\Public\break1\long-sample.log`（同机 12 min 静默控制连接基线）。
本轮为测试启动的两个带 CDP 的实例（`aerosend.exe`、`horizon-launcher.exe`）与鼠标移动脚本已关闭；
用户自己安装的正式版 AeroSend 未改动，需要用 §6 的安装包覆盖安装。

### 9.5 仍未验证

1. 真实无线链路下的空闲回收：同机路径 12 min 不回收，本轮无法在本机制造 RST，靠单测覆盖代码路径。
2. iPhone / Mac 真机的长会话表现（仍受任务 #10 的硬件限制）。
3. 60 s 静默上限是设计选择，未在真实投屏中调过：若接收端在长时间静止画面后仍会结束会话，先改这一个常量。
4. 安装包（31 107 459 字节，00:39）含 §8 与 §9 的修复，尚未覆盖安装到本机正式环境。
