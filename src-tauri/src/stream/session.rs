//! Live RAOP audio session: WASAPI loopback → s16be → L16 RTP.
//!
//! `_raop._tcp` is AirPlay's speaker service. Its ANNOUNCE accepts raw L16 PCM
//! and its SETUP names one audio socket, so this session carries audio and
//! nothing else; screen mirroring is a different service and lives in
//! [`crate::stream::mirror`].
//!
//! The receiver acts on the port SETUP answers with, not on the `m=audio` port
//! in the SDP, and it requires the sender to name its control and timing ports
//! or it refuses the transport outright.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::airplay::rtp::{RtcpSenderReport, RtpPacketizer};
use crate::capture::audio::AudioCapture;
use crate::capture::audio_encoder::AudioEncoder;
use crate::capture::encoder::EncoderType;

/// RAOP announces L16 as payload type 96; the payload type is not what tells
/// it apart from other media, the socket and the SDP do.
pub const AUDIO_PAYLOAD_TYPE: u8 = 96;
pub const AUDIO_CLOCK: u32 = 44_100;
/// Frames per RAOP audio packet. A receiver that is sent no `fmtp` still
/// assumes 352, so the packets and that default have to agree.
const L16_FRAMES_PER_PACKET: usize = 352;
const L16_CHANNELS: usize = 2;
/// 16-bit samples, two channels, 352 frames.
const L16_PACKET_BYTES: usize = L16_FRAMES_PER_PACKET * L16_CHANNELS * 2;

/// Encoded height the user asks for, resolved against the real screen.
///
/// RAOP is audio only, so this session ignores it; the mirroring session does
/// not, and the setting is shared by both.
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
    /// Pictures refused by the mirroring socket because the receiver had not
    /// taken what was already queued. A different failure from `frames_dropped`,
    /// which is local: this one says the network or the receiver is behind.
    pub frames_stalled: u64,
    pub access_units_sent: u64,
    /// Audio sample frames sent: 352 per RAOP L16 packet, 1024 per mirrored
    /// AAC-LC packet.
    pub audio_frames_sent: u64,
    /// Audio payload bytes, kept apart from `bytes_sent` so the video bitrate
    /// readout is not diluted by the audio stream.
    pub audio_bytes_sent: u64,
    pub bytes_sent: u64,
    pub rtp_packets_sent: u64,
    pub fps: f64,
    pub bitrate_mbps: f64,
    /// Capture-to-last-packet delay on the video path.
    pub latency_ms: f64,
    /// Geometry actually captured, e.g. `1920x1080`; empty for audio sessions.
    pub resolution: String,
    /// Why the session ended, when it ended badly.
    pub error: Option<String>,
}

pub struct StreamSession {
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<StreamStats>>,
}

/// The sender's RTP and RTCP sockets plus a timing port the receiver insists on
/// being named even though this sender never runs a timing exchange.
struct Endpoint {
    rtp: UdpSocket,
    rtcp: UdpSocket,
    _timing: UdpSocket,
    rtp_port: u16,
    timing_port: u16,
}

struct Handshake {
    /// Where the receiver wants RTP audio.
    server_port: u16,
    /// Where the receiver wants our RTCP sender reports; RAOP does not have to
    /// use `server_port + 1`, so the reply names it.
    control_port: u16,
    session_id: String,
    /// The RTSP connection stays open for the whole session.
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
        let timing = UdpSocket::bind("0.0.0.0:0");
        if let (Ok(rtp), Ok(rtcp), Ok(timing)) = (rtp, rtcp, timing) {
            let timing_port = timing
                .local_addr()
                .context("Failed to read the timing port")?
                .port();
            return Ok(Endpoint {
                rtp,
                rtcp,
                _timing: timing,
                rtp_port: port,
                timing_port,
            });
        }
        port = match port.checked_add(2) {
            // Wrap back into the dynamic range if we run off the end.
            Some(p) if p > 49150 => 49152,
            Some(p) => p,
            None => bail!("ran out of even RTP ports"),
        };
    }
}

impl Default for StreamSession {
    fn default() -> Self {
        Self::new()
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
        info!("Stopping RAOP session");
        self.running.store(false, Ordering::SeqCst);
    }

    /// Blocks for the lifetime of the session; run it on a blocking task.
    pub fn start(&self, host: &str, port: u16, settings: &StreamSettings) -> Result<()> {
        let result = self.run(host, port, settings);
        if let Err(e) = &result {
            let text = format!("{e:#}");
            error!("RAOP session failed: {text}");
            self.stats.lock().unwrap().error = Some(text);
        }
        self.running.store(false, Ordering::SeqCst);
        result
    }

    fn run(&self, host: &str, port: u16, settings: &StreamSettings) -> Result<()> {
        self.running.store(true, Ordering::SeqCst);
        *self.stats.lock().unwrap() = StreamStats::default();

        if !settings.audio {
            bail!(
                "RAOP is the speaker transport and carries audio only; \
                 enable system audio in Settings to cast to this device"
            );
        }

        info!("Starting RAOP audio to {host}:{port}");
        let started = Instant::now();
        let endpoint = bind_endpoint()?;
        let rtp = RtpPacketizer::new(AUDIO_PAYLOAD_TYPE, AUDIO_CLOCK);
        let handshake = rtsp_handshake(host, port, &endpoint, rtp.sequence_number())
            .with_context(|| format!("RTSP handshake with {host}:{port} failed"))?;
        info!(
            "RAOP negotiated: audio port {}, control port {}, session {}",
            handshake.server_port, handshake.control_port, handshake.session_id
        );

        // An unconnected UDP socket cannot `send`; the negotiated receiver
        // ports only exist after the handshake, so the targets are set here.
        let audio_target = format!("{host}:{}", handshake.server_port);
        endpoint
            .rtp
            .connect(&audio_target)
            .with_context(|| format!("Failed to connect audio RTP to {audio_target}"))?;
        let control_target = format!("{host}:{}", handshake.control_port);
        endpoint
            .rtcp
            .connect(&control_target)
            .with_context(|| format!("Failed to connect audio RTCP to {control_target}"))?;

        let audio = spawn_audio(endpoint, rtp, &self.running, self.stats.clone());

        // The audio pump is the whole session: if it dies, there is nothing
        // left to send, so its exit has to end the session too.
        while self.running.load(Ordering::SeqCst) && !audio.is_finished() {
            std::thread::sleep(Duration::from_millis(100));
        }
        self.running.store(false, Ordering::SeqCst);

        let outcome = match audio.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => bail!("audio pump panicked"),
        };
        if let Err(e) = handshake.teardown() {
            warn!("TEARDOWN failed: {e}");
        }
        let stats = self.stats.lock().unwrap().clone();
        info!(
            "RAOP session ended after {:.1}s: {} audio frames, {:.1} MB",
            started.elapsed().as_secs_f64(),
            stats.audio_frames_sent,
            stats.bytes_sent as f64 / 1_000_000.0
        );
        outcome
    }
}

/// Runs the whole audio path on one thread: WASAPI's COM interfaces are not
/// `Send`, so capture and its resampler are created and used here.
fn spawn_audio(
    endpoint: Endpoint,
    mut rtp: RtpPacketizer,
    running: &Arc<AtomicBool>,
    stats: Arc<Mutex<StreamStats>>,
) -> JoinHandle<Result<()>> {
    let running = running.clone();

    std::thread::Builder::new()
        .name("raop-audio".to_string())
        .spawn(move || -> Result<()> {
            let capture =
                AudioCapture::new().context("system audio capture is unavailable")?;
            let format = capture.format().clone();
            let mut encoder = AudioEncoder::new(format.clone());
            encoder
                .start()
                .context("the audio resampler would not start")?;
            let mut stdout = encoder
                .take_stdout()
                .context("the audio resampler has no stdout")?;
            capture
                .start()
                .context("system audio capture would not start")?;
            info!(
                "Audio path live: {}Hz {}ch -> L16 44100Hz 2ch",
                format.sample_rate, format.channels
            );

            // L16 has no framing, so the converter's byte stream is cut into
            // whole 352-frame packets here. A partial frame at the end of a
            // session is dropped rather than padded with audio that was never
            // captured.
            let (frame_tx, frame_rx) = mpsc::sync_channel::<Vec<u8>>(16);
            let reader_running = running.clone();
            let reader = std::thread::spawn(move || {
                let mut buffer: Vec<u8> = Vec::with_capacity(L16_PACKET_BYTES * 2);
                let mut chunk = [0u8; 8 * 1024];
                while reader_running.load(Ordering::SeqCst) {
                    match stdout.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buffer.extend_from_slice(&chunk[..n]);
                            while buffer.len() >= L16_PACKET_BYTES {
                                let frame = buffer.drain(..L16_PACKET_BYTES).collect();
                                if frame_tx.send(frame).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            });

            let mut sr = RtcpSenderReport::new(rtp.ssrc());
            let mut samples: u64 = 0;
            let mut window = RateWindow::start();
            let mut poll_gap = Duration::from_millis(5);
            let mut outcome = Ok(());

            while running.load(Ordering::SeqCst) {
                if let Some(chunk) = capture.poll() {
                    if encoder.send_pcm(&chunk.pcm).is_err() {
                        outcome = Err(anyhow::anyhow!("the audio resampler stdin closed"));
                        break;
                    }
                    // Pace the next poll to about half a buffer so WASAPI never
                    // backs up while the pipe reader drains asynchronously.
                    let frames = chunk.pcm.len() / format.block_align as usize;
                    poll_gap = Duration::from_micros(
                        (frames as u64 * 1_000_000 / format.sample_rate as u64 / 2)
                            .max(1_000),
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

                // L16 advances by exactly the samples one packet carries.
                let timestamp = samples as u32;
                samples += L16_FRAMES_PER_PACKET as u64;

                let packet = rtp.packetize_pcm(&frame, timestamp);
                sr.observe_packet(timestamp, (packet.len() - 12) as u32);
                if let Err(e) = endpoint.rtp.send(&packet) {
                    outcome = Err(anyhow::anyhow!("audio RTP send failed: {e}"));
                    break;
                }
                sr.mark_clock();
                if let Err(e) = endpoint.rtcp.send(&sr.serialize()) {
                    debug!("Audio RTCP send failed: {e}");
                }

                window.on_frame(packet.len());
                // Count sample frames, not packets: this is the same unit a
                // receiver reports, so the two sides can be compared directly.
                stats.lock().unwrap().audio_frames_sent += L16_FRAMES_PER_PACKET as u64;
                window.publish(&stats);
            }

            capture.stop();
            let _ = encoder.stop();
            let _ = reader.join();
            // The last partial second is still real traffic; publishing it keeps
            // the packet and byte totals equal to what left the socket.
            window.flush(&stats);
            info!("Audio pump stopped");
            outcome
        })
        .expect("failed to spawn the RAOP audio thread")
}

/// Tracks a one-second measurement window and publishes it into shared stats.
struct RateWindow {
    since: Instant,
    packets: u64,
    bytes: u64,
}

impl RateWindow {
    fn start() -> Self {
        Self {
            since: Instant::now(),
            packets: 0,
            bytes: 0,
        }
    }

    fn on_frame(&mut self, bytes: usize) {
        self.packets += 1;
        self.bytes += bytes as u64;
    }

    /// Publishes once at least a second has passed.
    fn publish(&mut self, stats: &Arc<Mutex<StreamStats>>) {
        let secs = self.since.elapsed().as_secs_f64();
        if secs < 1.0 {
            return;
        }
        self.flush_for(secs, stats);
    }

    /// Publishes whatever is left when the pump stops.
    fn flush(&mut self, stats: &Arc<Mutex<StreamStats>>) {
        let secs = self.since.elapsed().as_secs_f64();
        if secs > 0.0 {
            self.flush_for(secs, stats);
        }
    }

    fn flush_for(&mut self, secs: f64, stats: &Arc<Mutex<StreamStats>>) {
        let mbps = self.bytes as f64 * 8.0 / secs / 1_000_000.0;
        {
            let mut s = stats.lock().unwrap();
            s.fps = self.packets as f64 / secs;
            s.bitrate_mbps = mbps;
            s.rtp_packets_sent += self.packets;
            s.bytes_sent += self.bytes;
        }
        self.since = Instant::now();
        self.packets = 0;
        self.bytes = 0;
    }
}

struct RtspRawResponse {
    status_line: String,
    headers: HashMap<String, String>,
}

fn rtsp_handshake(
    host: &str,
    port: u16,
    endpoint: &Endpoint,
    initial_seq: u16,
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

    request(
        &mut stream,
        &mut cseq,
        "ANNOUNCE * RTSP/1.0",
        &[("Content-Type", "application/sdp"), ("Server", "AeroSend/1.0")],
        &build_sdp(),
    )
    .context("ANNOUNCE rejected")?;

    // `control_port` and `timing_port` are not optional to every receiver: a
    // RAOP SETUP without them is answered 461 even though the audio port is
    // spelled out. The timing socket is bound and kept alive but never spoken
    // to, because this sender derives its clock from WASAPI, not from NTP.
    let transport = build_transport(host, endpoint);

    let setup = request(
        &mut stream,
        &mut cseq,
        "SETUP * RTSP/1.0",
        &[("Transport", &transport)],
        "",
    )
    .context("audio SETUP rejected")?;

    let session_id = setup
        .headers
        .get("session")
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .unwrap_or_else(|| "1".to_string());

    // The receiver aligns its reorder buffer to the sequence number we start
    // with; without this it treats the first packets as arriving mid-stream.
    let rtp_info = format!("seq={initial_seq};rtptime=0");
    request(
        &mut stream,
        &mut cseq,
        "RECORD * RTSP/1.0",
        &[
            ("Session", &session_id),
            ("Range", "npt=0.000-"),
            ("RTP-Info", &rtp_info),
        ],
        "",
    )
    .context("RECORD rejected")?;

    let server_port = server_port(&setup).unwrap_or(40000);
    Ok(Handshake {
        server_port,
        control_port: transport_port(&setup, "control_port").unwrap_or(server_port + 1),
        session_id,
        control: stream,
        cseq,
    })
}

fn server_port(resp: &RtspRawResponse) -> Option<u16> {
    transport_port(resp, "server_port")
}

/// `RTP/AVP/UDP;unicast;destination=…;mode=record;client_port=…-…;…`
///
/// `mode=record` is what a sender of an RAOP stream asks for: the receiver
/// records, the sender plays. `client_port` is our RTP/RTCP pair; receivers
/// that insist on a control and timing port read them from the same header.
fn build_transport(host: &str, endpoint: &Endpoint) -> String {
    format!(
        "RTP/AVP/UDP;unicast;destination={host};mode=record;\
         client_port={rtp}-{rtcp};control_port={rtcp};timing_port={timing}",
        rtp = endpoint.rtp_port,
        rtcp = endpoint.rtp_port + 1,
        timing = endpoint.timing_port,
    )
}

fn transport_port(resp: &RtspRawResponse, name: &str) -> Option<u16> {
    let prefix = format!("{name}=");
    resp.headers
        .get("transport")?
        .split(';')
        .find(|part| part.trim().starts_with(&prefix))?
        .split('=')
        .nth(1)?
        .trim()
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// An audio-only SDP: RAOP's `m=audio` line with L16 at 44.1kHz stereo.
///
/// L16 is raw big-endian PCM and the only codec every RAOP receiver must
/// decode. No `rsaaeskey`/`aesiv`, so the payloads travel in the clear.
fn build_sdp() -> String {
    format!(
        "v=0\r\n\
         o=AeroSend 0 0 IN IP4 0.0.0.0\r\n\
         s=AeroSend\r\n\
         t=0 0\r\n\
         m=audio 0 RTP/AVP {AUDIO_PAYLOAD_TYPE}\r\n\
         c=IN IP4 0.0.0.0\r\n\
         a=rtpmap:{AUDIO_PAYLOAD_TYPE} L16/44100/2\r\n\
         a=control:streamid=1\r\n"
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The receiver decodes nothing but L16, and it takes the codec from the
    /// audio media line. Announcing AAC is what turned every session into a 403.
    #[test]
    fn the_audio_line_announces_l16_as_plain_pcm() {
        let sdp = build_sdp();
        assert!(sdp.contains("m=audio 0 RTP/AVP 96\r\n"), "{sdp}");
        assert!(sdp.contains("a=rtpmap:96 L16/44100/2\r\n"), "{sdp}");
        assert!(!sdp.contains("m=video"), "{sdp}");
        assert!(!sdp.contains("mpeg4-generic"), "{sdp}");
        // No rsaaeskey/aesiv: L16 is sent in the clear, and a receiver that
        // sees keys would try to decrypt packets that are not encrypted.
        assert!(!sdp.contains("rsaaeskey") && !sdp.contains("aesiv"), "{sdp}");
    }

    /// One packet carries 352 frames of stereo 16-bit PCM; the RTP clock has to
    /// advance by that many samples or the receiver's playout drifts.
    #[test]
    fn an_l16_packet_is_352_stereo_frames() {
        assert_eq!(L16_PACKET_BYTES, 1408);
        assert_eq!(L16_PACKET_BYTES % (L16_CHANNELS * 2), 0);
    }

    #[test]
    fn the_setup_names_control_and_timing_ports() {
        let endpoint = bind_endpoint().expect("local ports");
        let transport = build_transport("127.0.0.1", &endpoint);
        assert!(
            transport.contains(&format!(
                "client_port={}-{}",
                endpoint.rtp_port,
                endpoint.rtp_port + 1
            )),
            "{transport}"
        );
        assert!(
            transport.contains(&format!("control_port={}", endpoint.rtp_port + 1)),
            "{transport}"
        );
        assert!(
            transport.contains(&format!("timing_port={}", endpoint.timing_port)),
            "{transport}"
        );
        assert!(transport.contains("mode=record"), "{transport}");
    }

    /// RAOP replies name a control port that need not be `server_port + 1`.
    #[test]
    fn the_receiver_control_port_is_read_from_the_reply() {
        let mut headers = HashMap::new();
        headers.insert(
            "transport".to_string(),
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=50124;timing_port=50125;server_port=40120"
                .to_string(),
        );
        let resp = RtspRawResponse {
            status_line: "RTSP/1.0 200 OK".to_string(),
            headers,
        };
        assert_eq!(server_port(&resp), Some(40120));
        assert_eq!(transport_port(&resp, "control_port"), Some(50124));
        assert_eq!(transport_port(&resp, "timing_port"), Some(50125));
    }
}
