# AeroSend 测试报告 · Phase 6（批注 / 标注 + 采集链路多显卡加固）

- 日期：2026-11-02
- 被测项目：`D:\codeX\AeroSend`（Windows AirPlay/DLNA 发送端，独立于 `D:\codeX\meeting` 接收端）
- 测试机（本阶段实测，与 Phase 4/5 报告头的机器**不是同一台**）：
  `Intel(R) Xeon(R) CPU E5-2680 v2 @ 2.80GHz`（10 核 20 线程）/ 31.9 GB / Windows 11 教育版 build 26200 /
  GPU：`GameViewer Virtual Display Adapter`（driver 15.6.5.199）+ `AMD Radeon R5 220`（2 GB，driver 15.201.1151.1008）/
  Node 24.18 / Rust stable（`cargo test` debug profile，本机未装 `rustfmt`）
- 对端：本阶段是窗口层与采集层，不引入新对端；协议链路沿用 Phase 4/5 的 HORIZON 接收端与 fake 接收端

## 1. 结论

批注 / 标注功能已落地并在**真实渲染窗口**内逐项验证通过；同时把验证过程中暴露出的一个采集链路隐患（多显卡下可能镜像到错误的输出）修掉并加了回归测试。

| 层次 | 用例 / 项 | 结果 |
| --- | --- | --- |
| 单元（`cargo test --lib`） | 101 | 通过（本阶段新增 2 条采集选择用例） |
| 集成 · fake 对端（capture / pipeline / audio / session / upnp） | 11 | 通过 |
| 互操作 · HORIZON 真机渲染器（`#[ignore]`） | 2 | 通过（需先启动对端，见 Phase 4/5 报告） |
| 真实窗口 UI 走查（CDP 驱动 WebView2，两个文档） | 1 轮完整流程 | 通过 |
| `npx tsc --noEmit` | — | 无错误（exit 0） |
| `npm run build` | 双入口产物 | 通过，样式表完全隔离 |

未通过 / 无法在本机验证的一项：**"墨迹随镜像到达接收端"这一条在本机无法证明**，原因是控制台处于锁屏状态，两种屏幕读取 API 都读不到任何窗口内容（第 8 节给出三组对照实测数据，不是推测）。

## 2. 窗口层：独立窗口 + 样式隔离

`src-tauri/src/annotation/mod.rs` 用 `WebviewWindowBuilder` 建第二个窗口（label `annotate`，加载 `annotate.html`）：`decorations(false)` + `transparent(true)` + `shadow(false)` + `always_on_top(true)` + `skip_taskbar(true)` + `fullscreen(true)`。

- 选 `fullscreen` 而不是"取主屏尺寸放在 (0,0)"：builder 用逻辑像素、采集用物理像素，150% 缩放的屏上两者不等，让窗口管理器盖住整屏才能让墨迹和 duplicator 抓到的画面重合。
- 关闭即销毁，不留墨迹：批注是当场的手势，上周画到一半的线不该出现在本周的 PPT 上。

Vite 双入口实测产物（本轮 `npm run build`）：

```text
dist/index.html                      0.47 kB
dist/annotate.html                   0.49 kB
dist/assets/annotate-CoS3qFyQ.css    2.02 kB
dist/assets/main-BAT5Th4t.css       18.65 kB
dist/assets/annotate-eM2unA_f.js     7.27 kB
dist/assets/main-_N9R12JS.js        28.56 kB
✓ 42 modules transformed. built in 1.42s
```

两个文档、两张互不包含的样式表：overlay 只拿到 2.02 kB，侧栏的 18.65 kB 不会漏进透明全屏层（这是本阶段特意做的隔离，透明窗口的 `body`/`html` 必须保持无背景）。CDP 实测 overlay 的 `document.body` / `documentElement` 背景均为透明，窗口全屏且 topmost 位于主输出上。

## 3. 绘画语义：逐个工具的像素级验证

工具栏是 7 个工具（`pen / highlighter / line / arrow / rect / ellipse / eraser`）× 6 色（`#ff3b30 #ff9f0a #ffd60a #34c759 #0a84ff #ffffff`）× 3 档笔宽（`3 / 6 / 12`），外加 undo / clear / lock / PNG / close。

判定方法不是"看截图像不像"，而是把 canvas 的 `toDataURL` 解码回像素，按工具断言：

| 断言 | 实测 |
| --- | --- |
| pen 落一条线 → 画布出现该色且不透明 | 通过，`#ff3b30` 精确匹配、alpha=255 |
| highlighter 半透明加宽 | 通过，alpha 实测 **89**（`globalAlpha 0.35` 叠加白底所得），笔宽 ×2.5 且 `butt` 端帽 |
| rect / ellipse 只描边 | 通过，内部区域采样为全透明 |
| arrow 头部尺寸 `max(14, width*3)` | 通过，12 档笔宽下头部 36 px |
| eraser 只擦掉笔迹本身 | 通过，`destination-out` 擦除后像素计数**精确减少**为笔迹覆盖量，周围墨迹不动 |
| undo / clear | 通过，undo 弹掉最后一条并重绘，clear 后画布非透明像素为 0 |
| 新开的 overlay 从零开始 | 通过，非透明像素计数 0 |

其中 eraser 一项最初报"橡皮反而增加像素、颜色还滞后"，实测定位为**测试脚本自身缺陷**：React 18 批处理下，用 `.click()` 选工具后立刻同步派发 `pointerdown`，状态还没 flush，笔还停在上一档。加 80 ms settle 后复测，全部通过——不是产品缺陷，故未改产品代码。

截图：`docs/shots/annotation-overlay.png`（工具栏 + 红椭圆 + 蓝箭头 + 黄荧光带 + 绿勾）、`docs/shots/annotation-sidebar.png`（侧栏 ANNOTATION 段展开态）。

## 4. PNG 导出走的是原生对话框

`save_annotation_png`（`src-tauri/src/commands/annotate.rs`）：base64 解码 → `spawn_blocking` → `rfd::FileDialog::new().set_title("Save the annotation as a PNG").set_file_name("aerosend-annotation.png").add_filter("PNG image", &["png"]).save_file()` → 写盘。**没有**自绘文件选择器。

验证方式是让真实保存框完成一次：CDP 触发 Save → `EnumWindows`/`EnumChildWindows` 找到 `#32770` 对话框 → 往文件名 Edit 写 `WM_SETTEXT` → 对默认按钮 `BM_CLICK`。

- 往返一致性：画布导出的 PNG 里非透明像素 **27382**，落盘后重新解码仍是 **27382**，颜色仍为 `#ff3b30`。即对话框与写盘没有改变任何一个墨迹像素。
- 踩到的坑值得记录：**保存框的文件名输入框控件 id 是 1001**，打开框才是 1148；按 1148 找会得到"NO-EDIT"的假阴性。另外一次误操作堆了 3 个保存框（含直接 `__TAURI_INTERNALS__.invoke` 探针触发的那次），全部用 `BM_CLICK` id=2 取消后只留一个重测，才拿到确定性数字。

## 5. 锁定、快捷键与生命周期

| 项 | 实测 |
| --- | --- |
| Lock（`set_annotation_passthrough`） | 通过，窗口样式位同时置上 `WS_EX_TRANSPARENT` + `WS_EX_LAYERED`；解除后 `WS_EX_TRANSPARENT` 消失 |
| Esc 两段式 | 通过：锁定时第一下 Esc 只解锁、窗口仍在；第二下关闭；未锁定时一下关闭 |
| `l` / `Ctrl+Z` / `c` | 通过，分别对应锁定 / 撤销 / 清空 |
| 鼠标穿透期间仍可退出 | 通过（穿透后窗口收不到 click，Esc 与侧栏按钮是留出的两条退路） |
| 在 overlay 外面把窗口关掉 | 通过，侧栏按钮回到"Open overlay"（见第 6 节的同步修复） |

## 6. 本轮发现并修掉的产品缺陷：两个窗口各自记状态

**现象**：overlay 工具栏点 Lock 之后，侧栏的 "Lock pointer" 仍显示旧状态；反向（侧栏改、overlay 显示）同样会不一致。两个窗口各自持有 `locked` 的 React state，谁都不知道对方改过。

**修法**：按本项目既有约定——全项目一律轮询、没有 `app.emit` / `listen`——让两边都以后端为准：

- `src/App.tsx`：只在 overlay 打开时轮询 `get_annotation_state`（1.5 s），关闭时把定时器收掉；
- `src/pages/AnnotationOverlay.tsx`：对称地轮询同一命令，只同步 `locked`。

**复测**：live 双向（overlay→侧栏、侧栏→overlay）各一次，状态在一轮内一致；轮询失败时静默重试，不打断绘制。

## 7. 采集链路加固：D3D 设备的适配器不再交给系统挑

`ScreenCapture::new()` 原来是 `D3D11CreateDevice(None, HARDWARE, …)` → `GetAdapter()` → `EnumOutputs(0)`。这条路径把"镜像哪个输出"交给了 DXGI 的枚举顺序：**双显卡（真卡 + 虚拟显示驱动）的机器不保证正在扫描出桌面的是 D3D 设备自己那张卡**，而 `DuplicateOutput` 对一个"已挂载但不承载桌面"的输出**不会报错，只给全黑帧**——表现为"投屏黑了，但日志一切正常"。

现在（`src-tauri/src/capture/screen.rs`）：

1. `primary_monitor_rect()` 拿主屏桌面矩形（`MonitorFromPoint(0,0, MONITOR_DEFAULTTOPRIMARY)` + `GetMonitorInfoW`）；
2. `primary_output()` 走 `CreateDXGIFactory1` → 逐 adapter → 逐 output，只接受 `AttachedToDesktop` 且 `DesktopCoordinates` **等于**主屏矩形的输出，并带着它自己的 adapter 返回；
3. `create_device(Some(&adapter))`：指定 adapter 时 driver type 必须是 `D3D_DRIVER_TYPE_UNKNOWN`（`HARDWARE` + 非空 adapter 会被直接拒绝）；
4. 找不到匹配输出、或该输出 `DuplicateOutput` 失败（虚拟卡 / WARP 属合法失败场景）时 `warn!` 并退回原来的默认 adapter + output 0 行为；
5. `info!("Duplicating desktop from {}", source)` 把实际使用的输出名写进日志——排查"镜像全黑"时先看这一行。

本机 DXGI 实测枚举表（scratch 探针，跑完即删）：

```text
primary monitor rect: (0,0)-(1920,1080) 1920x1080
adapter[0] AMD Radeon R5 220 vendor=0x1002 device=0x68f9
  output[0] \\.\DISPLAY1 attached=true rect=(0,0)-(1920,1080) EXACT-MATCH
adapter[1] AMD Radeon R5 220 vendor=0x1002 device=0x68f9            ← 没有任何输出
adapter[2] Microsoft Basic Render Driver vendor=0x1414 device=0x008c
```

**如实记录：这台机器上新旧两条路径选到的是同一个输出。** `\\.\DISPLAY1` 本来就挂在 adapter[0]/index 0，因此修复不改变本机结果——它是给"adapter[0] 是一张不承载桌面的卡"那种机器（真实双卡、装了虚拟显示驱动的机器）拆掉隐患，不是本机某个故障的解药。系统另有一块 `GameViewer Virtual Display Adapter`（WMI 可见、DXGI 未列出输出），正是这类拓扑会踩坑的形状。

回归测试（`src/capture/screen.rs` 内 `#[cfg(test)]`，本机实跑通过）：

- `device_names_stops_at_the_nul`：`utf16_field` 在第一个 NUL 截断；
- `the_output_chosen_is_one_of_the_ones_holding_the_desktop`：独立枚举一遍所有 adapter/output，收集"已挂载且矩形等于主屏矩形"的输出名集合，断言 `primary_output()` 选中的名字在这个集合里——**这是那条负面结论（旧代码可能选错）的正面锚点**，并且断言主屏矩形非退化，无桌面环境会直接失败而不是假通过。

## 8. 环境限制（本轮实测，非推测）：控制台锁屏 → 屏幕读取为空

Phase 4/5 报告已经记下"测试期间控制台处于锁屏状态"。本阶段把它量到了可以直接引用的程度：三组对照，同一个时刻、同一块屏幕。

对照实验：起一个真实的 1920×1080 无边框纯红 topmost 窗口（WinForms，`visible=True`），然后同时用三种读取方式采样。

| 读取方式 | 结果 |
| --- | --- |
| `PrintWindow(PW_RENDERFULLCONTENT)` 该窗口 | **red = 129600/129600 采样点（100.0%）**，`PrintWindow=True` |
| GDI 屏幕 DC `CopyFromScreen` 同一矩形 | red = **0**（0.0%） |
| 生产采集路径 DXGI Desktop Duplication | `First frame: 1920x1080, 8294400 bytes, pitch=7680` → `Non-zero pixels: 0/8294400 (0.0%)`；3.10 s 内 2 帧、0.7 fps、5.5 MB/s、16.6 MB |

会话侧实测：`SESSIONNAME=Console`、进程 `sessionId=1`、`qwinsta` 显示 console 会话"运行中"，但 `GetForegroundWindow` 拿到的是 `Windows.UI.Core.CoreWindow`「Windows 默认锁屏界面」。即：**桌面层还在，但屏幕内容不是用户桌面**，任何窗口——包括批注 overlay 和它自己的墨迹——都不可能被屏幕读取抓到。

因此：

- 已证实的是"墨迹在 overlay 自己的窗口表面里"（`PrintWindow` 采到红通道 23251 个像素，第 3/4 节的 PNG 往返同样是 27382 像素级一致）；
- **未能**证实的是"墨迹经编码 → RTP → 接收端解码后仍然在画面上"。这一条不是逻辑缺口，只是本机给不出屏幕像素。它需要在**解锁的控制台**（或另一台真机）上复跑一次：

```bash
cd D:\codeX\AeroSend\src-tauri
cargo test --test test_capture -- --nocapture    # 解锁控制台后 Non-zero pixels 不应为 0%
# 再手工确认：打开 overlay 画一笔 → 开始镜像 → 接收端画面里有墨迹
```

其它限制：

- **未与真实 Apple TV / 商用电视验证**（沿用 Phase 4/5 的结论，本阶段不涉及新协议面）。
- 单元测试跑在 debug profile；发布前建议补一轮 `--release`。
- 本机未安装 `rustfmt`（`rustfmt.exe is not installed for the toolchain 'stable-x86_64-pc-windows-msvc'`），新代码按 `cargo check` 无 warning 与人工对齐换行处理，未跑 `cargo fmt`。

## 9. 本轮发现并修掉的问题

| # | 现象 | 根因 | 处理 |
| --- | --- | --- | --- |
| 1 | overlay 里点 Lock，侧栏状态不同步（反向同样） | 两个窗口各存一份 React state | 双方都轮询 `get_annotation_state`，以后端为准（项目原本就是轮询制）；双向 live 复测通过 |
| 2 | `DuplicateOutput` 可能镜像到不承载桌面的输出，全黑且无错误 | 原实现把 adapter/output 的选择交给 DXGI 枚举顺序（`GetAdapter()` + `EnumOutputs(0)`） | 主屏桌面矩形决定 adapter+output，指定 adapter 时改用 `D3D_DRIVER_TYPE_UNKNOWN`，保留回退并 `warn!`；新增 2 条回归用例 |
| 3 | 疑似"橡皮增加像素 / 颜色滞后" | 测试脚本缺陷：React 18 批处理下 `.click()` 后同步派发 `pointerdown`，工具还没切换 | 选择后加 80 ms settle；产品代码未改 |
| 4 | 保存对话框探测报"NO-EDIT (id 1148)" | 保存框的文件名 Edit 是 **id 1001**，1148 是打开框 | 先试 1148 再试 1001；顺带清掉误堆的 3 个对话框，只留一个重测 |
| 5 | 窗口标题/类名只返回 1 个字符，`#32770` 匹配不到 | PowerShell `Add-Type` 的 `DllImport` 默认 `CharSet.Ansi` | 显式 `CharSet = CharSet.Unicode`；这条曾把对话框排查整体带偏 |
| 6 | overlay 在扫描中途消失 | 改了 `src-tauri/examples/overlay_probe.rs` 触发 `tauri dev` 重建并重启应用 | scratch 探针移出仓库并删除；结论固化为规则：**窗口扫描进行中不得编辑 `src-tauri/**`** |
| 7 | `cargo test` 报 `failed to remove target\debug\aerosend.exe (os error 5)`，杀掉 `aerosend` 又复活 | `node → cargo → cargo → aerosend` 这条 watcher 链会重启应用 | 一次性杀掉链上 4 个 PID，之后全套干净通过 |
| 8 | `SetThreadExecutionState` 报 `EntryPointNotFoundException`；本轮 `GetProcessWindowStation` 同样踩反 | 前者在 kernel32，后者在 user32 | 各按真实导出模块声明；顺带记录：本轮三次窗口站探针都返回 `gle=87`，**窗口站不是"锁屏"这一结论的必要证据，已按"未查明"处理**，根因记录以第 8 节的三组对照实测为准 |
| 9 | 探针脚本写 `redhwnd.txt` 报 `DirectoryNotFoundException` | PowerShell 5.1 按 ANSI 读无 BOM 的 `.ps1`，脚本里的中文 `$HOME` 路径字面量被解坏 | 脚本内不写中文字面量，改用 `$env:TEMP` + `Join-Path` |

## 10. 复现全套

```bash
cd D:\codeX\AeroSend\src-tauri
cargo test                                            # 101 单测 + 11 集测通过 / 2 忽略
cargo test --lib -- capture::screen                   # 采集输出选择的两条用例
cargo test --test test_capture -- --nocapture          # 采集实测：本机 Non-zero 0.0%（见第 8 节）
cd D:\codeX\AeroSend
npx tsc --noEmit                                      # 前端类型，exit 0
npm run build                                         # 双入口：index.html + annotate.html，两张独立样式表
cargo test --test test_interop_hub_dmr -- --ignored --nocapture --test-threads=1   # 需先起 HORIZON 对端
```

窗口层的 CDP 走查脚本（`cdp.mjs` / `shot.mjs` / `expr*.mjs` / `redwin.ps1` / `redcheck.ps1` 等）是一次性量具，留在 `%TEMP%\aerosend-phase6` 且不进仓库；其中可复用的部分（对话框控件 id、`CharSet.Unicode`、CDP target 必须精确匹配 URL 以免 `annotate.html` 被 `localhost:1420/` 误命中）已写进第 4、8、9 节。
