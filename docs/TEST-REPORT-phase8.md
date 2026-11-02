# AeroSend 测试报告 · Phase 8（AirPlay 2 镜像音频，stream 96）

- 日期：2026-11-02
- 被测项目：`D:\codeX\AeroSend`（Windows AirPlay/DLNA 发送端）
- 测试机：Windows 11 build 26200 / Node 24.18 / Rust stable（`cargo test` debug profile）
- 对端：`D:\codeX\meeting` 的 HORIZON 会议白板启动器（戎耀会投屏），`127.0.0.1:5000`，停在 AirPlay 镜像页

## 1. 结论

镜像会话现在不只发视频，也真的在给它自己协商出来的 stream 96 发音频：

| 层 | 结果 |
| --- | --- |
| 握手 | SETUP 同时请求 stream 110（视频）与 stream 96（镜像音频），stream 96 带 `ct=4` / `spf=1024` / `controlPort` / `usingScreen`；接收端接受后进入 RECORD |
| 采集与编码 | WASAPI loopback → ffmpeg AAC-LC 44.1 kHz 立体声 → ADTS 去头 → 每 1024 采样一个裸 access unit |
| 封包 | RTP（PT 96，44.1 kHz 时钟，音频包不带 marker），净荷 AES-128-CBC，每包从 `eiv` 重置，只加密整块，尾块与 12 字节 RTP 头保持明文 |
| 同步 | 独立 control socket 每 3 s 发 20 字节 RTP↔NTP 映射包（首包 `0x90`，其后 `0x80`；`0xd4` 标识同步包） |
| 实测 | 镜像 181 张图 + **267 个音频包 / 273,408 采样帧 / 0.10 MB**，TEARDOWN 正常结束 |

| 检查 | 结果 |
| --- | --- |
| `cargo test` | 166 通过 / 5 忽略（3 AirPlay + 2 DLNA） |
| `cargo test --test test_mirror_audio -- --nocapture` | 通过：真实 ffmpeg 编出 ADTS→去头→RTP→CBC 全链路 |
| `AEROSEND_PEER=127.0.0.1:5000 cargo test --test test_interop_airplay -- --ignored --nocapture --test-threads=1` | 3 通过 |
| `npx tsc --noEmit` / `npm run build` | 无错误 / 通过 |

**能证明什么**：接收端按我们通告的格式收下了 stream 96，音视频同会话、同一次 RECORD，采样帧数与包数严格成 1024 倍关系，控制口同步包按节奏发出。

**不能证明什么**：HORIZON hub 的 `ensure_mirror_audio()` 只绑两个 UDP socket 然后丢弃收到的包（它没有镜像音频解码器），所以本轮没有"接收端真的解出声音"的锚点；也没有 Apple TV 在场。详见第 6 节。

## 2. 为什么协商 AAC-LC 而不是 AAC-ELD

Apple 原厂发送端在 stream 96 上用的是 AAC-ELD（`ct=8`，`spf=480`，SETUP 里 `audioFormat=0x01000000`）。本机的 ffmpeg 9.0.1（gyan.dev essentials）没有 AAC-ELD 编码器——编码需要 libfdk_aac，官方构建未包含，喂 `-profile:a aac_eld` 会直接报 `Profile not supported!`。

在"能真的编码出声音"和"格式与 Apple 一致"之间，本轮选了前者：

- `ct=4` 是 AAC-LC，`spf=1024`；
- 接收端按自己通告/协商到的 `ct` 分派解码器（UxPlay 系就是这样），所以这条流对它们有效；
- 对固定只解 AAC-ELD 的 Apple 硬件，这条流的可解性未验证。

`audioFormat` 字段被有意省略：本发送端唯一见过的取值就是 AAC-ELD 的位标志，抄一个与 `ct=4` 矛盾的字段不如不写。

## 3. 封包格式

### 3.1 SETUP

```text
SETUP rtsp://<host>/stream RTSP/1.0
...
Content-Type: application/x-apple-binary-plist

plist: streams = (
  { type = 96; ct = 4; spf = 1024; controlPort = <本地 UDP 端口>; usingScreen = true; }
)
```

`controlPort` 是发送端真正绑定并监听的 socket，不是占位值。接收端拒绝 stream 96 时只记一条 `aerosend: mirrored audio was not negotiated: …`，视频继续；不会把整个镜像会话拖垮。

### 3.2 音频数据

```text
 0        1        2 3       4..8          8..12       12..
 0x80     0x60     seq       rtp ts        ssrc         AES-128-CBC(payload)
```

- `0x60`：PT 96，marker 清（marker 是视频 access unit 的概念，UxPlay 发音频时也不置位）；
- `rtp ts` 每包 +1024，起点 0；
- 净荷是去掉 ADTS 头的裸 AAC access unit；
- CBC 的 IV 每包都回到 SETUP 应答里的 `eiv`；`floor(len/16)*16` 字节参与加密，尾部不足一块的字节原样发出（与 RAOP 的 KDF/加密习惯一致）。

### 3.3 同步包

UxPlay 系的接收端在没有收到 RTP↔NTP 映射前不会把音频出队，所以 control socket 每 3 s 发一次：

```text
 0     1     2 3    4..8         8..16        16..20
 0x90  0xd4  00 04  rtp sync     ntp (BE64)   rtp sync + 7497
```

首包 `0x90`，其后 `0x80`；最后 4 字节是 0.17 s 之后的 RTP 时间（44.1 kHz × 0.17 ≈ 7497），即 Apple 对 AAC 流的提前量。

## 4. 真实运行数据

### 4.1 镜像 + 音频

`AEROSEND_PEER=127.0.0.1:5000` 跑 `a_real_airplay_receiver_takes_our_mirroring_handshake`，用例自身用 `ffplay` 播 20 s 440 Hz 正弦（WASAPI loopback 只镜像正在播放的声音，空闲机器采不到东西）：

```text
[  1611ms] the receiver completes the AirPlay 2 handshake
        1920x1080 negotiated, 4 picture(s) already on the data port
[     0ms] mirrored audio reaches the receiver's ports
[     0ms] the mirroring session ends
sent    190 frames captured, 0 dropped, 181 pictures, 2.2 MB, 19.7 fps, 4.6 Mbps, 442.1 ms latency
audio   273408 sample frames, 267 RTP packets, 0.10 MB
ok
```

273,408 / 1024 = 267，包数与采样帧完全一致；音频码率 0.10 MB / 约 6.2 s ≈ 130 kbps，与 128 kbps 编码设置吻合（净荷之外的 RTP/IP 头也在统计里）。

### 4.2 RAOP 回归

同一次构建、同一台接收端，`a_real_airplay_speaker_takes_our_l16_audio` 仍然全绿：

```text
[   301ms] the speaker answers our RTSP handshake
[     0ms] the session ends
sent    270336 audio sample frames, 768 RTP packets, 1.09 MB, 1134 kbps
ok
```

270,336 / 352 = 768，Phase 7 的 L16 路径没有被 mirror audio 的改动碰到。

## 5. 单测覆盖

- `airplay/aes.rs`
  - CBC 加密对 NIST SP 800-38A F.2.1 向量；
  - 每包从 IV 重置、只处理整块、尾块原样保留；
  - 短 key/IV 在 CBC 路径上同样被拒。
- `airplay/rtp.rs`：`packetize_audio` 可显式清 marker（音频 `0x60`），PCM 路径行为不变。
- `stream/mirror_audio.rs`
  - 同步包字段与首包标记；
  - RTP 时钟回绕时同步包的最后 4 字节按 32 位回绕。
- `tests/test_mirror_audio.rs`（真实 ffmpeg，非 fake）
  - 2 s 440 Hz 立体声 f32le → AAC-LC，ADTS 解出约 86 帧（44100 × 2 / 1024）；
  - 帧头 `0x80 0x60`、时间戳每帧 +1024、序号连续；
  - 密文至少一块与明文不同，且尾块/不足一块的帧保持明文。
- `tests/test_interop_airplay.rs`：镜像用例把 `audio` 打开，先等第一个视频 access unit，再等 `audio_frames_sent > 0`，最后断言其为 1024 的整数倍。

## 6. 已知限制与未证明项

- **编码格式不是 Apple 原厂的 AAC-ELD**：本机 ffmpeg 没有 AAC-ELD 编码器（需 libfdk_aac），因此以 AAC-LC 协商。按协商值分派的接收端可解；Apple 硬件发送端固定 AAC-ELD，本轮没有 Apple TV / 真电视验证。
- **接收端解码未证明**：HORIZON hub 收下 stream 96 后丢弃，不做解码与播放。本轮能证明的是"接收端按通告格式接受并持续收流"，不是"扬声器出声"。要拿到解码锚点，需要一个会真解码镜像音频的实现（例如 UxPlay 系接收端或 Apple TV）。
- **UDP 丢包不重传**：发送端不处理接收端的 retransmit 请求，丢包直接成为音频空洞。
- **A/V 同步未调**：只依赖周期性同步包提供 RTP↔NTP 映射，没有用视频 PTS 校准音频时间轴，也没有实测唇音同步误差。
- **音频帧率不跟随内容**：AAC 编码持续产生帧（静音也编），不像视频那样按内容自适应；带宽占用稳定在 128 kbps 量级。
- 本机未安装 `rustfmt`，新代码以 `cargo check` 无新增 warning 与人工换行为准。

## 7. 复现

```bash
# 先把 HORIZON 启动器打开并停在 AirPlay 镜像页
cd D:\codeX\AeroSend\src-tauri
cargo test
cargo test --test test_mirror_audio -- --nocapture
AEROSEND_PEER=127.0.0.1:5000 \
  cargo test --test test_interop_airplay -- --ignored --nocapture --test-threads=1
cd D:\codeX\AeroSend
npx tsc --noEmit
npm run build
```

两条互操作用例都会自己起 `ffplay` 声源；如果机器上没有 `ffplay`，用例会打印提示，音频断言会如实失败而不是跳过。
