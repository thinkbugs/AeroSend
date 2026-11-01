//! Live AirPlay mirroring session: capture → encode → packetize → RTP.
//!
//! Video and audio run as independent pumps that share one RTSP session. Both
//! derive RTP timestamps from the same monotonic clock started at `RECORD`, so
//! the receiver can pair them through their RTCP sender reports.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::airplay::h264::{AccessUnitReader, Nal};
use crate::airplay::rtp::{RtcpSenderReport, RtpPacketizer};
use crate::capture::adts::{AdtsParser, AdtsFrame};
use crate::capture::audio::AudioCapture;
use crate::capture::audio_encoder::AudioEncoder;
use crate::capture::encoder::{EncoderType, VideoEncoder};
use crate::capture::screen::ScreenCapture;

pub const VIDEO_PAYLOAD_TYPE: u8 = 96;
pub const AUDIO_PAYLOAD_TYPE: u8 = 106;
pub const VIDEO_CLOCK: u32 = 90_000;
pub const AUDIO_CLOCK: u32 = 44_100;
const AUDIO_BITRATE: u32 = 128_000;
/// Frames allowed between capture and encoder output: the pictures the encoder
/// still holds plus the one the access-unit grouper keeps back. Below the
/// encoder's own fill the gate would starve it and nothing would ever come out;
/// above it the viewer is watching history. Six frames is ~200ms at 30fps.
const MAX_BACKLOG_FRAMES: usize = 6;

/// Encoded height the user asks for, resolved against the real screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    #[serde(rename = "native")]
    Native,
    #[serde(rename = "1080p")]
    Height1080,
    #[serde(rename = "720p")]
    Height720,
    #[serde(rename = "480p")]
    Height480,
}

impl Default for Resolution {
    fn default() -> Self {
        Resolution::Native
    }
}

impl Resolution {
    /// Geometry to encode: the target height, aspect preserved, never
    /// upscaled, and both dimensions even because yuv420p rejects odd ones.
    pub fn target(&self, screen_width: u32, screen_height: u32) -> (u32, u32) {
        let wanted = match self {
            Resolution::Native => screen_height,
            Resolution::Height1080 => 1080,
            Resolution::Height720 => 720,
            Resolution::Height480 => 480,
        };
        let height = wanted.min(screen_height).max(2) & !1;
        let width = (screen_width * height / screen_height).max(2) & !1;
        (width, height)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StreamSettings {
    pub fps: u32,
    /// Target video bitrate in bits per second.
    pub bitrate: u32,
    pub audio: bool,
    pub resolution: Resolution,
    pub encoder: EncoderType,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            fps: 30,
            bitrate: 4_000_000,
            audio: true,
            resolution: Resolution::default(),
            encoder: EncoderType::Auto,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StreamStats {
    pub frames_captured: u64,
    /// Capture ticks skipped because the encoder had not drained its backlog.
    pub frames_dropped: u64,
    pub access_units_sent: u64,
    pub audio_frames_sent: u64,
    pub bytes_sent: u64,
    pub rtp_packets_sent: u64,
    pub fps: f64,
    pub bitrate_mbps: f64,
    /// Capture-to-last-packet delay on the video path.
    pub latency_ms: f64,
    /// Geometry actually captured, e.g. `1920x1080`.
    pub resolution: String,
    /// Why the session ended, when it ended badly.
    pub error: Option<String>,
}

pub struct StreamSession {
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<StreamStats>>,
}

/// One media stream's RTP data and RTCP control sockets.
struct Endpoint {
    rtp: UdpSocket,
    rtcp: UdpSocket,
    rtp_port: u16,
}

struct Handshake {
    video_server_port: u16,
    audio_server_port: u16,
    session_id: String,
    /// The RTSP control connection stays open for the whole session.
    control: TcpStream,
    cseq: u32,
}

impl Handshake {
    /// TEARDOWN belongs on the connection the session was announced on; a fresh
    /// TCP connection carries no session state and most receivers ignore it.
    fn teardown(mut self) -> Result<()> {
        request(
            &mut self.control,
            &mut self.cseq,
            "TEARDOWN * RTSP/1.0",
            &[("Session", &self.session_id.clone())],
            "",
        )
        .context("TEARDOWN rejected")?;
        Ok(())
    }
}

/// RTP needs an even data port with the next odd port reserved for RTCP.
fn bind_endpoint() -> Result<Endpoint> {
    let probe = UdpSocket::bind("0.0.0.0:0").context("Failed to probe a local port")?;
    let mut port = probe.local_addr()?.port();
    drop(probe);
    if port % 2 == 1 {
        port = port + 1;
    }

    loop {
        let rtp = UdpSocket::bind(("0.0.0.0", port));
        let rtcp = UdpSocket::bind(("0.0.0.0", port + 1));
        if let (Ok(rtp), Ok(rtcp)) = (rtp, rtcp) {
            return Ok(Endpoint { rtp, rtcp, rtp_port: port });
        }
        port = match port.checked_add(2) {
            // Wrap back into the dynamic range if we run off the end.
            Some(p) if p > 49150 => 49152,
            Some(p) => p,
            None => bail!("ran out of even RTP ports"),
        };
    }
}

impl StreamSession {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(Mutex::new(StreamStats::default())),
        }
    }

    pub fn running_flag(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    pub fn stats(&self) -> StreamStats {
        self.stats.lock().unwrap().clone()
    }

    pub fn stop(&self) {
        info!("Stopping stream session");
        self.running.store(false, Ordering::SeqCst);
    }

    /// Blocks for the lifetime of the session; run it on a blocking task.
    pub fn start(&self, host: &str, port: u16, settings: &StreamSettings) -> Result<()> {
        let result = self.run(host, port, settings);
        if let Err(e) = &result {
            let text = format!("{e:#}");
            error!("Stream session failed: {text}");
            self.stats.lock().unwrap().error = Some(text);
        }
        self.running.store(false, Ordering::SeqCst);
        result
    }

    fn run(&self, host: &str, port: u16, settings: &StreamSettings) -> Result<()> {
        let StreamSettings {
            fps,
            bitrate,
            audio,
            resolution,
            encoder,
            ..
        } = *settings;
        info!("Starting stream to {host}:{port} at {fps}fps / {bitrate}bps (audio={audio})");
        self.running.store(true, Ordering::SeqCst);
        *self.stats.lock().unwrap() = StreamStats::default();
        let started = Instant::now();

        // Capture first: the SDP must advertise the geometry actually sent,
        // so the negotiated framesize comes from DXGI rather than the caller.
        let capture = ScreenCapture::new().context("Failed to init screen capture")?;
        let (screen_w, screen_h) = (capture.width(), capture.height());
        let (width, height) = resolution.target(screen_w, screen_h);
        info!("Screen capture {screen_w}x{screen_h}, encoding {width}x{height}");
        self.stats.lock().unwrap().resolution = format!("{width}x{height}");

        let video = bind_endpoint()?;
        let audio = if audio { Some(bind_endpoint()?) } else { None };

        let handshake = rtsp_handshake(host, port, &video, audio.as_ref(), width, height, fps)
            .with_context(|| format!("RTSP handshake with {host}:{port} failed"))?;
        info!(
            "RTSP negotiated: video port {}, audio port {}, session {}",
            handshake.video_server_port, handshake.audio_server_port, handshake.session_id
        );

        // An unconnected UDP socket cannot `send`; the negotiated receiver
        // ports only exist after the handshake, so the targets are set here.
        let video_target = format!("{host}:{}", handshake.video_server_port);
        video
            .rtp
            .connect(&video_target)
            .with_context(|| format!("Failed to connect video RTP to {video_target}"))?;
        let _ = video
            .rtcp
            .connect(format!("{host}:{}", handshake.video_server_port + 1));

        let mut encoder =
            VideoEncoder::new(screen_w, screen_h, width, height, fps, bitrate, encoder);
        encoder.start().context("Failed to start video encoder")?;
        let encoder_stdout = encoder
            .take_stdout()
            .context("Video encoder has no stdout")?;

        let running = self.running.clone();
        let stats = self.stats.clone();
        let clock: CaptureClock = Arc::new(Mutex::new(VecDeque::new()));

        let (au_tx, au_rx) = mpsc::sync_channel::<AccessUnit>(4);
        let mut threads = Vec::new();

        threads.push({
            let running = running.clone();
            let clock = clock.clone();
            std::thread::spawn(move || {
                read_access_units(encoder_stdout, &running, &au_tx, &clock);
                info!("Video encoder reader exited");
            })
        });

        threads.push({
            let running = running.clone();
            let stats = stats.clone();
            std::thread::spawn(move || {
                send_video(video, &au_rx, &running, &stats, started);
            })
        });

        let audio_handle =
            audio.map(|endpoint| spawn_audio(endpoint, host, handshake.audio_server_port, &running, stats.clone()));

        run_capture_loop(&capture, &mut encoder, &clock, &running, &stats, fps);
        info!("Capture pump stopped; tearing down");

        // Closing stdin makes ffmpeg flush its pipeline and exit, which in turn
        // ends the reader thread; that must happen before signalling shutdown.
        encoder.stop()?;
        self.running.store(false, Ordering::SeqCst);
        for handle in threads.drain(..) {
            let _ = handle.join();
        }
        if let Some(handle) = audio_handle {
            let _ = handle.join();
        }

        if let Err(e) = handshake.teardown() {
            warn!("TEARDOWN failed: {e}");
        }
        // One guard for both readings: two `lock()` calls in the same statement
        // keep the first alive while asking for the second, and this mutex is
        // not reentrant.
        let (frames, access_units) = {
            let s = stats.lock().unwrap();
            (s.frames_captured, s.access_units_sent)
        };
        info!("Stream session ended: {frames} frames, {access_units} access units");
        Ok(())
    }
}

/// An access unit plus the wall-clock time its frame left the screen capture.
struct AccessUnit {
    nalus: Vec<Nal>,
    captured_at: Instant,
}

/// Capture instants of fed frames, consumed in encoder output order. The
/// encoder preserves frame order and runs at zerolatency, so one queued
/// instant corresponds to one emitted access unit.
type CaptureClock = Arc<Mutex<VecDeque<Instant>>>;

/// Reads ffmpeg stdout, splits Annex-B, and emits grouped access units.
fn read_access_units(
    mut stdout: impl Read,
    running: &AtomicBool,
    tx: &SyncSender<AccessUnit>,
    clock: &CaptureClock,
) {
    let mut reader = AccessUnitReader::new();
    let mut chunk = [0u8; 64 * 1024];

    // Each emitted picture consumes one capture instant: the clock is fed by
    // the capture loop in the same order the encoder produces pictures.
    let emit = |nalus: Vec<Nal>| -> bool {
        let captured_at = clock
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(Instant::now);
        tx.send(AccessUnit {
            nalus,
            captured_at,
        })
        .is_ok()
    };

    while running.load(Ordering::SeqCst) {
        match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for nalus in reader.push(&chunk[..n]) {
                    if !emit(nalus) {
                        return;
                    }
                }
            }
        }
    }

    for nalus in reader.finish() {
        if !emit(nalus) {
            return;
        }
    }
}

fn run_capture_loop(
    capture: &ScreenCapture,
    encoder: &mut VideoEncoder,
    clock: &CaptureClock,
    running: &AtomicBool,
    stats: &Arc<Mutex<StreamStats>>,
    fps: u32,
) {
    let frame_interval = Duration::from_micros(1_000_000 / fps as u64);
    let mut next_tick = Instant::now();
    // Last pixels out of the capture; reused when the screen has not changed,
    // so every tick still costs exactly one encoded picture.
    let mut buffer: Vec<u8> = Vec::new();
    let mut fed = 0u64;
    let mut dropped = 0u64;

    while running.load(Ordering::SeqCst) {
        let due = next_tick;
        next_tick = next_tick + frame_interval;
        if next_tick > Instant::now() {
            std::thread::sleep(next_tick.saturating_duration_since(Instant::now()));
        }

        let changed = match capture.capture_frame(0) {
            Ok(Some(frame)) => {
                buffer = frame.data;
                true
            }
            Ok(None) => false,
            Err(e) => {
                warn!("Capture error: {e}");
                false
            }
        };
        if !changed && buffer.is_empty() {
            // Nothing has reached us yet; there is no picture to encode.
            continue;
        }
        // Frames handed to the encoder leave the pipeline one access unit at a
        // time, so the queue length is its backlog. Past a few frames the viewer
        // is watching history, so the newest pixels replace what is not encoded
        // yet instead of queueing behind it.
        if clock.lock().unwrap().len() >= MAX_BACKLOG_FRAMES {
            dropped += 1;
            stats.lock().unwrap().frames_dropped = dropped;
            continue;
        }
        if encoder.send_frame(&buffer).is_err() {
            warn!("Video encoder stdin closed after {fed} frames");
            return;
        }
        // Exactly one clock instant per fed frame keeps the latency measure
        // aligned with the access units the reader pops.
        clock.lock().unwrap().push_back(due);
        fed += 1;
        stats.lock().unwrap().frames_captured = fed;
    }
    info!("Capture pump fed {fed} frames, dropped {dropped}");
}

fn send_video(
    endpoint: Endpoint,
    au_rx: &mpsc::Receiver<AccessUnit>,
    running: &AtomicBool,
    stats: &Arc<Mutex<StreamStats>>,
    started: Instant,
) {
    let mut rtp = RtpPacketizer::new(VIDEO_PAYLOAD_TYPE, VIDEO_CLOCK);
    let mut sr = RtcpSenderReport::new(rtp.ssrc());
    let mut window = RateWindow::start();

    while running.load(Ordering::SeqCst) {
        let au = match au_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(au) => au,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        let timestamp = rtp.clock_for_ms(started.elapsed().as_millis() as u64);
        let packets = rtp.packetize_h264(&au.nalus, timestamp);
        let payload_bytes: usize = packets.iter().map(|p| p.len() - 12).sum();

        for pkt in &packets {
            sr.observe_packet(timestamp, (pkt.len() - 12) as u32);
            if let Err(e) = endpoint.rtp.send(pkt) {
                warn!("Video RTP send failed: {e}");
                return;
            }
        }
        sr.mark_clock();
        if let Err(e) = endpoint.rtcp.send(&sr.serialize()) {
            debug!("Video RTCP send failed: {e}");
        }

        window.on_access_unit(packets.len(), payload_bytes, au.captured_at);
        {
            let mut s = stats.lock().unwrap();
            s.access_units_sent += 1;
        }
        window.publish(stats);
    }
}

/// Tracks a one-second measurement window and publishes it into shared stats.
struct RateWindow {
    since: Instant,
    units: u64,
    packets: u64,
    bytes: u64,
    latency_ms: f64,
}

impl RateWindow {
    fn start() -> Self {
        Self {
            since: Instant::now(),
            units: 0,
            packets: 0,
            bytes: 0,
            latency_ms: 0.0,
        }
    }

    fn on_access_unit(&mut self, packets: usize, bytes: usize, captured_at: Instant) {
        self.units += 1;
        self.packets += packets as u64;
        self.bytes += bytes as u64;
        self.latency_ms = Instant::now()
            .duration_since(captured_at)
            .as_secs_f64()
            * 1000.0;
    }

    fn on_frame(&mut self, bytes: usize) {
        self.units += 1;
        self.packets += 1;
        self.bytes += bytes as u64;
    }

    /// Publishes once at least a second has passed, returning the measured rate.
    fn publish(&mut self, stats: &Arc<Mutex<StreamStats>>) -> Option<(f64, f64)> {
        let secs = self.since.elapsed().as_secs_f64();
        if secs < 1.0 {
            return None;
        }
        let fps = self.units as f64 / secs;
        let mbps = self.bytes as f64 * 8.0 / secs / 1_000_000.0;

        let mut s = stats.lock().unwrap();
        s.fps = fps;
        s.bitrate_mbps = mbps;
        s.rtp_packets_sent += self.packets;
        s.bytes_sent += self.bytes;
        if self.latency_ms > 0.0 {
            s.latency_ms = self.latency_ms;
        }
        drop(s);

        self.since = Instant::now();
        self.units = 0;
        self.packets = 0;
        self.bytes = 0;
        Some((fps, mbps))
    }
}

/// Runs the whole audio path on one thread.
///
/// WASAPI's COM interfaces are not `Send`, so `AudioCapture` and the encoder
/// that consumes it have to be created and used inside this thread; only the
/// decoded AAC frames cross back out as owned `Vec<u8>`.
fn spawn_audio(
    endpoint: Endpoint,
    host: &str,
    server_port: u16,
    running: &Arc<AtomicBool>,
    stats: Arc<Mutex<StreamStats>>,
) -> JoinHandle<()> {
    let host = host.to_string();
    let running = running.clone();

    std::thread::spawn(move || {
        let capture = match AudioCapture::new() {
            Ok(c) => c,
            Err(e) => {
                warn!("Audio path unavailable: {e:#}");
                return;
            }
        };
        let format = capture.format().clone();
        let mut encoder = AudioEncoder::new(format.clone(), AUDIO_BITRATE);
        if let Err(e) = encoder.start() {
            warn!("Audio path unavailable: {e:#}");
            return;
        }
        let mut stdout = match encoder.take_stdout() {
            Some(s) => s,
            None => {
                warn!("Audio path unavailable: AAC encoder has no stdout");
                return;
            }
        };

        let target = format!("{host}:{server_port}");
        if let Err(e) = endpoint.rtp.connect(&target) {
            warn!("Audio path unavailable: failed to connect {target}: {e}");
            return;
        }
        let _ = endpoint.rtcp.connect(format!("{host}:{}", server_port + 1));
        if let Err(e) = capture.start() {
            warn!("Audio path unavailable: {e:#}");
            return;
        }
        info!(
            "Audio path live: {}Hz {}ch -> {target}",
            format.sample_rate, format.channels
        );

        // AAC frames decoded by ffmpeg leave through a channel so reading the
        // pipe cannot block the WASAPI polling loop.
        let (frame_tx, frame_rx) = mpsc::sync_channel::<AdtsFrame>(16);
        let reader_running = running.clone();
        std::thread::spawn(move || {
            let mut parser = AdtsParser::default();
            let mut buf = [0u8; 8 * 1024];
            while reader_running.load(Ordering::SeqCst) {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        parser.push(&buf[..n]);
                        while let Some(frame) = parser.next_frame() {
                            if frame_tx.send(frame).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });

        let mut rtp = RtpPacketizer::new(AUDIO_PAYLOAD_TYPE, AUDIO_CLOCK);
        let mut sr = RtcpSenderReport::new(rtp.ssrc());
        let mut samples: u64 = 0;
        let mut window = RateWindow::start();
        let mut poll_gap = Duration::from_millis(5);

        while running.load(Ordering::SeqCst) {
            if let Some(chunk) = capture.poll() {
                if encoder.send_pcm(&chunk.pcm).is_err() {
                    warn!("AAC encoder stdin closed");
                    break;
                }
                // Pace the next poll to about half a buffer so WASAPI never
                // backs up while the pipe reader drains asynchronously.
                let frames = chunk.pcm.len() / format.block_align as usize;
                poll_gap = Duration::from_micros(
                    (frames as u64 * 1_000_000 / format.sample_rate as u64 / 2).max(1_000),
                );
            }

            let frame = match frame_rx.try_recv() {
                Ok(f) => f,
                Err(mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(poll_gap);
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
            };

            // AAC-hbr advances by exactly one frame of samples per access unit.
            let timestamp = samples as u32;
            samples += frame.samples as u64;

            let packet = rtp.packetize_aac(&frame.data, timestamp);
            sr.observe_packet(timestamp, (packet.len() - 12) as u32);
            if let Err(e) = endpoint.rtp.send(&packet) {
                warn!("Audio RTP send failed: {e}");
                break;
            }
            sr.mark_clock();
            let _ = endpoint.rtcp.send(&sr.serialize());

            window.on_frame(packet.len());
            stats.lock().unwrap().audio_frames_sent += 1;
            window.publish(&stats);
        }

        capture.stop();
        let _ = encoder.stop();
        info!("Audio pump stopped");
    })
}
struct RtspRawResponse {
    status_line: String,
    headers: HashMap<String, String>,
}

fn rtsp_handshake(
    host: &str,
    port: u16,
    video: &Endpoint,
    audio: Option<&Endpoint>,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Handshake> {
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).with_context(|| format!("connect {addr}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("Failed to set RTSP read timeout")?;
    let mut cseq = 0u32;

    request(
        &mut stream,
        &mut cseq,
        "OPTIONS * RTSP/1.0",
        &[("Apple-Jack-Status", "connected; type=analog")],
        "",
    )
    .context("OPTIONS rejected")?;

    let sdp = build_sdp(width, height, fps, audio.is_some());
    request(
        &mut stream,
        &mut cseq,
        "ANNOUNCE * RTSP/1.0",
        &[("Content-Type", "application/sdp"), ("Server", "AeroSend/1.0")],
        &sdp,
    )
    .context("ANNOUNCE rejected")?;

    let transport = |e: &Endpoint| {
        format!(
            "RTP/AVP/UDP;unicast;destination={host};mode=play;client_port={}-{}",
            e.rtp_port,
            e.rtp_port + 1
        )
    };

    let video_setup = request(
        &mut stream,
        &mut cseq,
        "SETUP * RTSP/1.0",
        &[("Transport", &transport(video))],
        "",
    )
    .context("video SETUP rejected")?;

    let audio_setup = match audio {
        Some(e) => Some(
            request(
                &mut stream,
                &mut cseq,
                "SETUP * RTSP/1.0",
                &[("Transport", &transport(e))],
                "",
            )
            .context("audio SETUP rejected")?,
        ),
        None => None,
    };

    let session_id = video_setup
        .headers
        .get("session")
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .unwrap_or_else(|| "1".to_string());

    request(
        &mut stream,
        &mut cseq,
        "RECORD * RTSP/1.0",
        &[("Session", &session_id), ("Range", "npt=0.000-")],
        "",
    )
    .context("RECORD rejected")?;

    Ok(Handshake {
        video_server_port: server_port(&video_setup).unwrap_or(7100),
        audio_server_port: audio_setup
            .as_ref()
            .and_then(server_port)
            .unwrap_or(40000),
        session_id,
        control: stream,
        cseq,
    })
}

fn server_port(resp: &RtspRawResponse) -> Option<u16> {
    resp.headers
        .get("transport")?
        .split(';')
        .find(|p| p.trim().starts_with("server_port="))?
        .split('=')
        .nth(1)?
        .trim()
        .split('-')
        .next()?
        .parse()
        .ok()
}

fn build_sdp(width: u32, height: u32, fps: u32, audio_enabled: bool) -> String {
    let mut sdp = format!(
        "v=0\r\n\
         o=AeroSend 0 0 IN IP4 0.0.0.0\r\n\
         s=AeroSend\r\n\
         t=0 0\r\n\
         m=video 0 RTP/AVP {VIDEO_PAYLOAD_TYPE}\r\n\
         c=IN IP4 0.0.0.0\r\n\
         a=rtpmap:{VIDEO_PAYLOAD_TYPE} H264/90000\r\n\
         a=fmtp:{VIDEO_PAYLOAD_TYPE} packetization-mode=1;profile-level-id=640028\r\n\
         a=framerate:{fps}\r\n\
         a=framesize:{VIDEO_PAYLOAD_TYPE} {width}-{height}\r\n"
    );
    if audio_enabled {
        // AAC-LC at 44.1kHz stereo: AudioSpecificConfig 0x1210.
        sdp.push_str(
            "m=audio 0 RTP/AVP 106\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=rtpmap:106 mpeg4-generic/44100/2\r\n\
             a=fmtp:106 profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1210\r\n\
             a=control:streamid=1\r\n",
        );
    }
    sdp
}

fn request(
    stream: &mut TcpStream,
    cseq: &mut u32,
    line: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<RtspRawResponse> {
    *cseq += 1;
    let mut msg = format!("{line}\r\nCSeq: {cseq}\r\n");
    for (k, v) in headers {
        msg.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        msg.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    msg.push_str("\r\n");
    msg.push_str(body);

    debug!("RTSP >> {line}");
    stream
        .write_all(msg.as_bytes())
        .with_context(|| format!("Failed to send {line}"))?;
    stream.flush()?;

    let resp = read_response(stream)?;
    debug!("RTSP << {}", resp.status_line);
    if !resp.status_line.contains(" 200") {
        bail!("{} -> {}", line, resp.status_line);
    }
    Ok(resp)
}

fn read_response(stream: &mut TcpStream) -> Result<RtspRawResponse> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        buf.push(byte[0]);
        if buf.len() >= 4 && buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            bail!("RTSP response headers exceeded 64 KiB");
        }
    }

    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    Ok(RtspRawResponse {
        status_line,
        headers,
    })
}

