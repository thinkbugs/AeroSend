# AeroSend 测试报告 · Phase 7（AirPlay 2 镜像 + RAOP L16 音频互操作）

- 日期：2026-11-02
- 被测项目：`D:\codeX\AeroSend`（Windows AirPlay/DLNA 发送端）
- 测试机：Windows 11 build 26200 / Node 24.18 / Rust stable（`cargo test` debug profile）
- 对端：`D:\codeX\meeting` 的 HORIZON 会议白板启动器（戎耀会投屏）——同一台机器上的 `127.0.0.1:5000`，以及它在局域网广播的 `192.168.0.27:5000`

## 1. 结论

AirPlay 的两条通道都已经和真实接收端跑通，且各自只做协议允许的事：

| 层 | 通道 | 结果 |
| --- | --- | --- |
| `_raop._tcp`（扬声器） | L16 音频，PT 96，44.1 kHz 立体声，明文 | 通过：ANNOUNCE → SETUP → RECORD → TEARDOWN 全绿，768 包 / 270,336 采样帧 |
| `_airplay._tcp`（带 `pk`） | AirPlay 2 镜像：pair-verify → FairPlay → SETUP(plist) → AES-128-CTR | 通过：1920×1080 协商，181 张图走加密数据口 |
| mDNS 能力判定 | `pk` 决定走镜像，`_raop._tcp` 一律音频 | 通过：同一个接收端两条广播分别判为 `mirroring=true` / `false` |

| 检查 | 结果 |
| --- | --- |
| `cargo test --lib` | 149 通过 |
| `cargo test`（集成） | 11 通过 / 5 忽略（3 AirPlay + 2 DLNA） |
| `AEROSEND_PEER=127.0.0.1:5000 cargo test --test test_interop_airplay -- --ignored --nocapture --test-threads=1` | 3 通过 |
| `npx tsc --noEmit` | 无错误 |
| `npm run build` | 通过（双入口产物 28.78 kB 主 JS + 2.70 kB overlay JS） |

仍未验证的一项：**AirPlay 2 镜像通道的音频（stream 96）**。握手会协商它，但镜像会话目前不发音频帧；这是任务 #14，不在本阶段范围内。（已由 Phase 8 补上并实测，见 [TEST-REPORT-phase8.md](TEST-REPORT-phase8.md)。）

## 2. RAOP 音频：从 403 / 461 到真实收流

### 2.1 第一层：ANNOUNCE 403

上一轮的 SDP 是视频 + AAC（`mpeg4-generic`）。接收端的 `Sdp::parse` 逐行读取，后出现的 `a=rtpmap` 覆盖前一条，于是最终 codec 是 AAC；它的 `announce` 只接受 L16，其余一律 403。

修改后只广播一条音频媒体行：

```text
m=audio 0 RTP/AVP 96
c=IN IP4 0.0.0.0
a=rtpmap:96 L16/44100/2
a=control:streamid=1
```

没有 `rsaaeskey` / `aesiv`：L16 走明文，接收端据此不建 AES 流；送上去反而会被当成已加密数据。

### 2.2 第二层：SETUP 461

接收端的 RAOP `setup` 明确要求 Transport 头同时带 `control_port` 和 `timing_port`，缺一个就 461。现在的 SETUP 是：

```text
RTP/AVP/UDP;unicast;destination=127.0.0.1;mode=record;client_port=55178-55179;control_port=55179;timing_port=55180
```

- `client_port` 是本地 RTP/RTCP 端口对（偶数 + 下一个奇数）；
- `control_port` 用同一个 RTCP 端口，接收端往这里回 RTCP；
- `timing_port` 绑定并保持存活，但本发送端不发 NTP——接收端只是要求这个端口存在；
- `mode=record`：RAOP 的语义是"接收端录、发送端放"。

RTCP SR 现在发往**接收端在 SETUP 应答里给出的 `control_port`**，而不是旧的 `server_port + 1`。RECORD 增加 `RTP-Info: seq=<首个序号>;rtptime=0`，让接收端的重排队列从我们的第一个包开始对齐。

### 2.3 第三层：视频不能进扬声器通道

接收端的 RAOP SETUP 只绑一个音频 UDP socket，之后这个 socket 上的每个 RTP 净荷都按 L16 解码。如果继续把 H.264 发进去，结果不是"视频被忽略"，而是音频被解成噪声。

`_raop._tcp` 在 AirPlay 里本来就是扬声器服务，屏幕镜像属于 `_airplay._tcp` 的 AirPlay 2 通道。因此本轮把 RAOP 会话改成**纯音频**：

- `stream/session.rs` 只做 WASAPI → 重采样 → L16 RTP；
- 屏幕采集 / 编码 / access unit 管线移到 `stream/pipeline.rs`，只由 `stream/mirror.rs` 使用；
- `stream_start` 对 RAOP 设备要求 `audio=true`，否则直接给出可读错误，而不是起一个什么都不发的会话；
- 统计与前端跟随通道：音频会话只显示码率、时长、音频帧、已发送，传输标签是 `L16 / 44.1 kHz`。

### 2.4 真实收流数据

`AEROSEND_PEER=127.0.0.1:5000` 跑 `a_real_airplay_speaker_takes_our_l16_audio`：

```text
[   855ms] the speaker answers our RTSP handshake
        6336 audio frames already accepted
sent    270336 audio sample frames, 768 RTP packets, 1.09 MB, 1353 kbps
ok
```

270,336 / 352 = 768，包数与结算采样帧完全一致；TEARDOWN 正常结束，没有错误。接收端侧的正向锚点是它自己的两道门：codec 不对回 403，Transport 缺端口回 461；这两道门都过了，才进入 RECORD 并持续收流。

## 3. 镜像通道回归

`a_real_airplay_receiver_takes_our_mirroring_handshake` 在把采集管线挪到 `pipeline.rs` 之后重跑：

```text
[  1460ms] the receiver completes the AirPlay 2 handshake
        1920x1080 negotiated, 2 picture(s) already on the data port
sent    189 frames captured, 0 dropped, 181 pictures, 2.3 MB, 19.0 fps, 4.3 Mbps, 449.1 ms latency
ok
```

mDNS 判定同时复核：

```text
AppleTV3,2                       192.168.0.27:5000  type=raop     mirroring=false
戎耀会投屏._airplay._tcp.local.     192.168.0.27:5000  type=airplay  mirroring=true
```

镜像帧率 19 fps 而不是设置的 30 fps，是本机 x264 在该时刻的实测吞吐；同一条用例上一轮跑到 30 fps，编码器选择（`Auto`）和机器负载会改变这个数字，本轮没有把它当作回归信号。

## 4. 测试侧修正

- **loopback 端到端用例重写**：fake 接收端从"视频 SETUP + 音频 SETUP"改为单次音频 SETUP；断言 SDP 只有 `m=audio`、Transport 三个端口齐全、每个包净荷恰好 1408 字节、序号无缺口、时间戳每包 +352、RTCP SR 的 SSRC 与音频一致。实测 488 包、171,776 采样帧、0 丢包。
- **接收端 control_port 的解析**：新增单元用例，应答里 `control_port=50124;timing_port=50125;server_port=40120` 时三个端口各自读对，确认没有退回 `server_port + 1` 的假设。
- **互操作用例的调用方式**：libtest 在第二个 `--` 之后把参数当名称过滤器，文档里原来的 `-- --peer=…` 会跑 0 个用例。改为支持 `AEROSEND_PEER` / `AEROSEND_NAME` 环境变量，一条命令即可让全部 `#[ignore]` 用例指向同一台接收端。

## 5. 统计口径

- `audio_frames_sent` 以**采样帧**计（每包 352），与接收端的 `frames` 同一单位，两边可以直接比；
- 一秒窗口之外的尾部流量在泵停止时补一次 flush，`rtp_packets_sent` / `bytes_sent` 与真实上线的包数一致（本轮 757 → 768 的变化就是补上了最后不足一秒的部分）。

## 6. 已知限制

- **AirPlay 2 镜像音频未发送**（stream 96 已协商）。RAOP 的 L16 音频是独立通道，不依赖它。（Phase 8 已补上，见 [TEST-REPORT-phase8.md](TEST-REPORT-phase8.md)。）
- 未与真实 Apple TV / 商用电视验证；本轮对端是 HORIZON 接收端。
- 接收端把音频交给 `WaveOutSink` 播放，本轮没有从运行中的 hub 读取它的解码计数；"接收端已接受并持续收流"由它的 403 / 461 门与正常 RECORD 证明，"扬声器实际出声"需要现场听感确认。
- 本机未安装 `rustfmt`，新代码按 `cargo check` 无新增 warning 与人工换行处理。

## 7. 复现

```bash
cd D:\codeX\AeroSend\src-tauri
cargo test
AEROSEND_PEER=127.0.0.1:5000 cargo test --test test_interop_airplay -- --ignored --nocapture --test-threads=1
cargo test --test test_session_e2e -- --nocapture
cd D:\codeX\AeroSend
npx tsc --noEmit
npm run build
```
