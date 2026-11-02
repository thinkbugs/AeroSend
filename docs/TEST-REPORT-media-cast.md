# AeroSend「媒体投屏」：图片与音乐投屏根因定位与修复测试报告

- 日期：2026-11-03
- 需求：左侧菜单「DLNA 投屏」改名为「媒体投屏」，并且真正支持视频、图片、音乐（原来只能投视频）
- 被测构建（发送端）：`D:\codeX\AeroSend\src-tauri\target\release\aerosend.exe`，14 751 744 B，2026-11-03 01:22
- 被测构建（接收端）：`D:\codeX\meeting\target\release\horizon-launcher.exe`，6 751 232 B，2026-11-03 01:34
- 机器：i9-14900KF / RTX 5080 / 96 GB / Win11 26200，局域网地址 192.168.0.27
- 测试素材：`C:\Users\Public\mediacast\photo.jpg`（23 356 B，JPEG 800×450）、`song.mp3`（32 617 B，4 s）、`clip.mp4`（47 458 B，3 s，h264+aac）

## 1. 结论

1. 菜单与页面标题已在真实渲染出来的窗口里读出来确认为「媒体投屏」，侧边栏三项为 `["设备","媒体投屏","设置"]`。
2. 图片、音乐、视频三类现在都能投出去，并且**接收端把它们认成各自正确的类型**：中枢队列里分别是 `kind:"image"` / `"audio"` / `"video"`，分别渲染成照片舞台、音乐播放器、视频播放器。
3. 投出去的地址现在自带扩展名（`/m/3994d2f3.jpg`），这是让只看 URL 的接收端能分类的前提；DIDL 元数据也补成了规范的 `object.item.imageItem.photo` / `audioItem.musicTrack`。
4. 投照片时中枢确实从 AeroSend 的 HTTP 服务器把整张图拉走了：发送端字节计数从修复前的 `0 B 已从本机送出` 变成 `23.4 kB 已从本机送出`，中枢 `<img>` 实测 `naturalWidth 800 × naturalHeight 450, complete:true`。
5. 图片显示不出来这一步的根因不在投屏链路，而在**中枢自身的 CSP**：`img-src 'self' data: blob:`，而页面源是 `http://tauri.localhost`，所以任何局域网 `http://ip:port` 图片都被拦。这一条同时也影响中枢自己媒体库里的图片，已一并修好。
6. 单测：AeroSend `cargo test --workspace` 退出码 0，**178 passed / 0 failed / 5 ignored**（其中 lib 165 个，改动前 161 个，净增 4 个）；中枢 `cargo test --workspace` 退出码 0，**272 passed / 0 failed / 6 ignored**。`npx tsc --noEmit` 干净。

## 2. 根因：四个独立环节叠加，缺一个都投不成图片/音乐

原来「只能投视频」不是一处判断写死了视频，而是四件事叠在一起，任何一件都足以让图片/音乐失败。逐条列证据：

| 环节 | 原状态 | 证据 |
| --- | --- | --- |
| 文件选择器 | 只列视频扩展名，图片/音乐根本选不中 | 修复前 `commands/cast.rs` 只有一组视频 filter |
| 投出去的 URL | `/m/{id}`，无扩展名 → 只看地址的接收端只能默认成视频 | 中枢 `queue.rs:70` `MediaKind::from_uri`（文档即写明「DLNA 控制器只给这个」）默认落到 `Video` |
| DIDL 元数据 | 发出 `object.item.image`、`object.item.audioItem`，不是规范叶子类 | 修复前 `avt.rs`；规范类应为 `imageItem.photo` / `audioItem.musicTrack` |
| 接收端 CSP | 图片拿到了、拉到了，但 DOM 里渲染不出来 | 中枢 `tauri.conf.json` `img-src 'self' data: blob:`；页面内探针实测远程图片 `BLOCKED/ERROR`、`data:` 图片 `OK 2x2`、`location.origin = http://tauri.localhost` |

排除顺序也是按上表反向做的：先用 `curl` 证明发送端 GET 返回 `200 image/jpeg 23356 B`、range 请求返回 `206`，再用 `ffprobe` 直接解析投出去的 URL 得到 `h264+aac, duration 3.000, 45 frames`，确认发送端字节没问题，才把矛头指向接收端 CSP。

## 3. 改动清单

### 发送端 AeroSend

- [src/components/Sidebar.tsx:78](file:///D:/codeX/AeroSend/src/components/Sidebar.tsx#L78) — 「DLNA 投屏」→「媒体投屏」。
- [src/pages/CastView.tsx:207](file:///D:/codeX/AeroSend/src/pages/CastView.tsx#L207) — 页面标题与说明改为「把本地的视频、图片或音乐交给局域网内的渲染器播放……」；空态提示、按钮文案「选择媒体文件投屏…」、hint 全部改为三类媒体。
- [src/pages/CastView.tsx:201](file:///D:/codeX/AeroSend/src/pages/CastView.tsx#L201)、[:457](file:///D:/codeX/AeroSend/src/pages/CastView.tsx#L457)、[:470](file:///D:/codeX/AeroSend/src/pages/CastView.tsx#L470) — 从当前投出的地址推出类型 `mediaKindOf()`，面板加类型徽标（视频/音乐/图片）；图片不再显示进度条，改为「图片没有播放进度；渲染器显示期间可停止或改投其他文件」——一个永远停在 0:00 的进度条看起来像播放器坏了。
- [src-tauri/src/commands/cast.rs:60](file:///D:/codeX/AeroSend/src-tauri/src/commands/cast.rs#L60) — 原生文件选择器加「音乐」「图片」两组 filter（音乐 mp3/m4a/aac/flac/wav/ogg/oga/opus/wma，图片 jpg/jpeg/png/gif/webp/bmp），保留「所有文件」。
- [src-tauri/src/upnp/media_server.rs:180](file:///D:/codeX/AeroSend/src-tauri/src/upnp/media_server.rs#L180) 与 [:199](file:///D:/codeX/AeroSend/src-tauri/src/upnp/media_server.rs#L199) — `url_for()` 在 id 后追加清洗过的扩展名（只留 ascii 字母数字、最多 8 位，空则不加）；[:264](file:///D:/codeX/AeroSend/src-tauri/src/upnp/media_server.rs#L264) 路由先把 id 按 `.` 截断再查表（id 本身是十六进制，不含点，所以不引入新的路径穿越面）。MIME 表补 webp/bmp/avif/ogg/oga/wma/opus。
- [src-tauri/src/upnp/avt.rs:345](file:///D:/codeX/AeroSend/src-tauri/src/upnp/avt.rs#L345) — 新增 `upnp_class_for(mime)`：audio → `audioItem.musicTrack`，image → `imageItem.photo`，其余 → `videoItem`。
- [src-tauri/src/upnp/caster.rs:704](file:///D:/codeX/AeroSend/src-tauri/src/upnp/caster.rs#L704) — `served_id()` 不再用 `ends_with("/m/{id}")` 匹配，改为切出 `/m/` 之后再按 `? # .` 取第一段，这样带扩展名、带设备自加 query 的 URL 仍能对上已注册文件（字节数和 MIME 的权威来源仍是注册文件，不是 URL）。
- [src-tauri/src/upnp/mod.rs:1](file:///D:/codeX/AeroSend/src-tauri/src/upnp/mod.rs#L1) — 模块说明改为「视频、照片或歌曲」。

### 接收端 HORIZON 会议中枢

- [src-tauri/tauri.conf.json:34](file:///D:/codeX/meeting/src-tauri/tauri.conf.json#L34)、[:35](file:///D:/codeX/meeting/src-tauri/tauri.conf.json#L35) — `csp` 与 `devCsp` 的 `img-src` 放开为 `'self' data: blob: http: https:`。`media-src` 刻意未动：视频/音频是经原生 MediaEngine 子窗口上屏的，不走 CSP；中枢的音乐舞台是纯 DOM 装饰（`DlnaStage.tsx:504` 没有 `<audio>`），全中枢唯一以远程 URL 作 `<img>` 的地方是 `DlnaStage.tsx:612` 的照片，另一处 `FileTransferPanel.tsx:313` 是 `data:` 二维码，本来就不受影响。
- 未改动项（记录为已知边界）：中枢 `crates/horizon-cast/src/dlna.rs:729` 的 `SetAVTransportURI` 仍只读 `CurrentURI`、忽略 `CurrentURIMetaData`，中枢的分类依旧靠地址扩展名。因为发送端现在把扩展名放进地址里，两边口径一致。

## 4. 单元与集成测试

| 命令 | 结果 |
| --- | --- |
| AeroSend `cargo test --workspace` | 退出码 0：lib **165 passed / 0 failed**；集成套件 1 + 2 + 1 + 4 + 1 + 4 = **13 passed / 0 failed**；5 ignored |
| AeroSend `npx tsc --noEmit` | 退出码 0 |
| 中枢 `cargo test --workspace` | 退出码 0：`horizon-cast` **217 passed / 0 failed / 6 ignored**；`horizon-launcher` **55 passed / 0 failed** |

新增用例（都断言的是「接收端会读到的那串字节」，不是内部函数返回值）：

- `upnp::media_server::a_photo_and_a_song_are_advertised_by_their_extension_and_still_served`
- `upnp::media_server::an_extension_that_cannot_live_in_a_url_is_cleaned_rather_than_encoded`
- `upnp::caster::a_served_id_survives_the_extension_and_a_query_a_device_adds`
- `upnp::avt::a_photo_and_a_song_announce_the_class_their_player_expects`
- `tests/test_upnp_e2e.rs::a_photo_and_a_song_cast_as_themselves_and_are_pulled_back_whole` — 走完整 e2e：`cast_file` 投 `Holiday.JPG` 与 `encore.flac`，断言 URL 以 `.jpg`/`.flac` 结尾、DIDL 里类与 `protocolInfo` 正确，并让假 DMR 发三次 range 请求，断言都是 `206` 且字节与原文件逐字节相同。

### 关掉修复必红（证明被测代码真的在链路上）

把 `avt.rs` 的 `upnp_class_for` 与 `media_server.rs` 里 `url_for` 的扩展名后缀还原成修复前：

- lib 测试 `upnp::` 子集 **73 passed / 6 failed**；
- e2e **1 failed**，`tests\test_upnp_e2e.rs:696` 断言失败；
- 恢复修复后全部回到绿色。

## 5. 端到端实测（真实安装包二进制 + CDP，非源码 grep）

驱动脚本 `C:\Users\Public\mediacast\verify-media-cast.ps1`，输出日志 `run2-out.log`（01:25，中枢为修复前二进制）与 `run3-out.log`（01:35，中枢为修复后二进制）。两轮发送端用的是同一个 `aerosend.exe`（01:22），中枢源码在 00:37→01:34 两次构建之间只有 `tauri.conf.json` 的 img-src 变了（其余源文件 mtime 均早于 00:37），因此这两轮构成**单变量 A/B**。

| 测点 | 修复前（run2） | 修复后（run3） |
| --- | --- | --- |
| 侧边栏（真实 DOM） | `["设备","媒体投屏","设置"]` | 同，`h1` = 媒体投屏 |
| 发现渲染器 | HubDMR `uuid:c1202adc-…` @ `http://192.168.0.27:8290`，`castable:true` | 同 |
| 投照片 | URL `…/m/3994d2f3.jpg`，中枢队列 `kind:"image"`，中枢 `<img>` `w:0 h:0`，发送端 `0 B 已从本机送出` | 同一 URL，中枢 `<img>` **`w:800 h:450 complete:true`**，发送端 **`23.4 kB 已从本机送出`** |
| 发送端面板 | 徽标「图片」、无进度条 + 无进度提示 | 同（`slider:false`） |
| 投歌曲 | `…/m/9b47013e.mp3`，`kind:"audio"`，`.dlna-music` 标题正确，引擎读到时长 4.097 s | 同 |
| 投视频 | `…/m/79e91acb.mp4`，`kind:"video"`，`.dlna-video__playing` true，时长 3 s | 同 |
| 注册表回执 | `get_offered_files` 报 `image/jpeg` / `audio/mpeg` / `video/mp4`，URL 均带扩展名 | 同 |

构建产物已 stat 确认落盘：`aerosend.exe` 14 751 744 B @01:22；`bundle\nsis\AeroSend_0.1.0_x64-setup.exe` 31 087 774 B @01:22（29.65 MiB）；`horizon-launcher.exe` 6 751 232 B @01:34；`bundle\nsis\HORIZON Meeting_0.5.1_x64-setup.exe` 4.44 MiB @01:34。

## 6. 已知边界与未验证项

1. **中枢的原生播放在本环境不走带**：投过去的 clip 与中枢自己媒体库里的 `哪吒入心之途.mp4` 一样，能读到时长（3 s / 346 s）但 `phase` 停在 `stopped`、position 卡在 0.1333 / 0.1。这是本测试机的接收端引擎限制（与本次改动无关，媒体库路径同样复现），因此「持续播放/拖动进度即时生效」在这台机器上无法证明。
2. **没有真实电视/音箱在网**：DIDL 类名、`protocolInfo`、range 拉流这些是对着断言精确字符串的假 DMR 与本机中枢验的；真机是否接受图片/音乐类仍未验证。
3. **发送端不做 sink 协议门控**：`caster.rs:321` 的 `castable` 只表示「有 AVTransport 服务」，不看设备 `GetProtocolInfo` 里允许哪些 MIME。只支持 `video/mp4` 的电视收到图片可能直接拒绝。
4. **中枢 `SetAVTransportURI` 忽略 `CurrentURIMetaData`**（`dlna.rs:729`）：分类只看地址。别的控制器投一个不带扩展名的 URL 给中枢，仍会被认成视频。
5. **中枢 MIME 表少 aac/mka/oga/avif/heic**（`dlna.rs:1651`）：这些会广播成通配符 `*`；`MediaKind::from_uri` 认它们，所以类型仍正确，只是 `protocolInfo` 不精确。
6. 真实无线环境下的空闲摘流（NAT/AP）与中枢 60 s 镜像静默上限仍未调参。
7. OEM 构建不受影响已核实：`scripts/build-oem.cjs:205-232` 是读取现有 `tauri.conf.json` 后逐字段改写（不是模板覆盖），img-src 会带进各客户包。
8. 新安装包尚未覆盖安装到用户正在使用的 AeroSend（等用户确认后再做）。

## 7. 复现步骤

1. 准备三个素材文件（见文首），路径用正斜杠——它们会进 JS 字符串字面量，反斜杠会被当转义吃掉。
2. 中枢以 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9334` 启动，发送端用 `9333`，然后调用 `dlna_start({name:'HubDMR'})`。
3. 跑脚本：`powershell -NoProfile -ExecutionPolicy Bypass -File "C:/Users/Public/mediacast/verify-media-cast.ps1"`。
4. 导航项按序号点（`[...document.querySelectorAll('.nav-item')][1]`），中文文案经控制台代码页匹配不可靠。
5. 想复现「关掉修复必红」：还原 `avt.rs::upnp_class_for` 与 `media_server.rs::url_for` 的后缀拼接，跑 `cargo test --lib upnp` 与 `cargo test --test test_upnp_e2e`。
6. 想复现 CSP 拦截：把中枢 `tauri.conf.json` 的 `img-src` 改回 `'self' data: blob:`、重新构建，照片舞台 `naturalWidth` 归 0、发送端字节计数归 `0 B`。
