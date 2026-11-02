//! Live AirPlay 2 mirroring session: capture → encode → AVCC → the mirror socket.
//!
//! This shares the capture and encoder pumps with [`crate::stream::pipeline`]
//! and parts company with them right after the encoder. Mirroring has no RTP: the
//! handshake names a TCP port, and every coded picture goes down it as
//! [`MirrorWriter`]'s 128-byte header plus an AES-128-CTR encrypted AVCC payload,
//! one keystream for the life of the connection.
//!
//! With audio enabled the session also runs [`crate::stream::mirror_audio`] on
//! the stream 96 ports the receiver named. Its failure is reported but does not
//! end the picture.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::airplay::control::{Pacer, KEEPALIVE_INTERVAL};
use crate::airplay::h264::Nal;
use crate::airplay::handshake::MirrorSession;
use crate::airplay::mirror::{ntp_now, MirrorWriter};
use crate::capture::encoder::VideoEncoder;
use crate::capture::screen::ScreenCapture;
use crate::stream::pipeline::{read_access_units, run_capture_loop, AccessUnit, CaptureClock};
use crate::stream::session::{StreamSettings, StreamStats};

/// How long the connection may go without accepting a single byte before the
/// session is called dead.
///
/// Below this the sender rides the stall out by dropping pictures; above it
/// there is no receiver left to ride out anything for, and the honest thing is
/// to say so instead of letting the stack abort the connection.
const MAX_STALL: Duration = Duration::from_secs(15);

/// Shares the shape of [`crate::stream::session::StreamSession`] so a caller can
/// drive either transport with the same running flag and stats.
pub struct MirrorStream {
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<StreamStats>>,
}

impl Default for MirrorStream {
    fn default() -> Self {
        Self::new()
    }
}

impl MirrorStream {
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
        self.running.store(false, Ordering::SeqCst);
    }

    /// Blocks for the lifetime of the session; run it on a blocking task.
    pub fn start(&self, host: &str, port: u16, settings: &StreamSettings) -> Result<()> {
        let result = self.run(host, port, settings);
        if let Err(e) = &result {
            let text = format!("{e:#}");
            warn!("Mirroring session failed: {text}");
            self.stats.lock().unwrap().error = Some(text);
        }
        self.running.store(false, Ordering::SeqCst);
        result
    }

    fn run(&self, host: &str, port: u16, settings: &StreamSettings) -> Result<()> {
        self.running.store(true, Ordering::SeqCst);
        *self.stats.lock().unwrap() = StreamStats::default();

        // The receiver needs to be told where our retransmission port is before
        // it answers SETUP stream 96, so the socket exists first.
        let audio_control = if settings.audio {
            Some(UdpSocket::bind("0.0.0.0:0").context("Failed to bind the mirror audio port")?)
        } else {
            None
        };
        let audio_control_port = match &audio_control {
            Some(socket) => Some(socket.local_addr()?.port()),
            None => None,
        };

        let mut session = MirrorSession::open(host, port, audio_control_port)
            .with_context(|| format!("AirPlay 2 handshake with {host}:{port}"))?;
        info!("Mirroring to {:?} on port {}", session.name, session.data_port);

        // Capture first: the encoder geometry has to match the screen that is
        // actually being read.
        let mut capture = ScreenCapture::new().context("Failed to init screen capture")?;
        let (screen_w, screen_h) = (capture.width(), capture.height());
        let (mut width, mut height) = settings.resolution.target(screen_w, screen_h);
        let mut fps = settings.fps;
        if let Some(display) = session.display {
            // Encoding past the geometry the receiver advertised costs bandwidth
            // the receiver will only scale away.
            width = width.min(display.width);
            height = height.min(display.height);
            fps = fps.min(display.max_fps);
        }
        // h264 needs even edges, and a receiver that asked for 1080p should not
        // be sent 1081 lines.
        width -= width % 2;
        height -= height % 2;
        info!("Mirroring {screen_w}x{screen_h} as {width}x{height} at {fps}fps");
        self.stats.lock().unwrap().resolution = format!("{width}x{height}");

        let addr = session.mirror_addr();
        let writer = MirrorWriter::connect(
            &addr,
            &session.aes_key,
            &session.shared_secret,
            &session.connection_id.to_string(),
        )
        .with_context(|| format!("mirror data connection to {addr}"))?;

        let mut encoder = VideoEncoder::new(
            screen_w,
            screen_h,
            width,
            height,
            fps,
            settings.bitrate,
            settings.encoder,
        );
        encoder.start().context("Failed to start video encoder")?;
        let encoder_stdout = encoder
            .take_stdout()
            .context("Video encoder has no stdout")?;

        let running = self.running.clone();
        let stats = self.stats.clone();
        let clock: CaptureClock = Arc::new(Mutex::new(VecDeque::new()));
        let (au_tx, au_rx) = mpsc::sync_channel::<AccessUnit>(4);

        let reader = {
            let running = running.clone();
            let clock = clock.clone();
            std::thread::spawn(move || {
                read_access_units(encoder_stdout, &running, &au_tx, &clock);
                info!("Video encoder reader exited");
            })
        };
        let sender = {
            let running = running.clone();
            let stats = stats.clone();
            std::thread::spawn(move || {
                send_frames(
                    writer,
                    &au_rx,
                    &running,
                    &stats,
                    (screen_w, screen_h),
                    (width, height),
                );
            })
        };

        // Streaming the screen without its sound is the worse failure, so an
        // audio problem leaves the pictures alone and only surfaces in the log
        // and in a stalled audio counter.
        let audio = match (audio_control, session.audio_ports) {
            (Some(control), Some(ports)) => {
                info!("Mirror audio negotiated: ports {ports:?}");
                Some(crate::stream::mirror_audio::spawn(
                    host.to_string(),
                    ports,
                    control,
                    session.aes_key,
                    session.eiv,
                    running.clone(),
                    stats.clone(),
                ))
            }
            (Some(_), None) => {
                warn!("The receiver did not negotiate mirrored audio; continuing with video only");
                None
            }
            _ => None,
        };

        // A mirroring session puts no traffic on the receiver's control
        // connection, and a connection the network sees as idle is a connection
        // an access point or a firewall is free to reap. When the receiver loses
        // it, it ends the session and closes the mirror socket that is still
        // carrying frames — which the user sees as a picture that stopped on its
        // own. So the control connection asks a question of its own accord for
        // as long as the session runs.
        let keepalive = spawn_keepalive(
            session.control.pacer().context("mirror control keepalive")?,
            &running,
        )?;
        run_capture_loop(&mut capture, &mut encoder, &clock, &running, &stats, fps);
        info!("Capture pump stopped; tearing down mirroring");

        // Closing stdin makes ffmpeg flush and exit, which ends the reader and
        // through it the writer; the session is only torn down once both are done.
        encoder.stop().ok();
        self.running.store(false, Ordering::SeqCst);
        let _ = reader.join();
        let _ = sender.join();
        // Joined before `TEARDOWN` because it owns a read half of the same
        // socket: two readers would leave a half-read reply in front of the
        // answer this session's last request is about to wait for.
        let _ = keepalive.join();
        if let Some(audio) = audio {
            match audio.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Mirror audio ended: {e:#}"),
                Err(_) => warn!("Mirror audio thread panicked"),
            }
        }

        session.teardown();
        let s = stats.lock().unwrap();
        info!(
            "Mirroring session ended: {} frames captured, {} pictures sent, {} stalled at the \
             socket, {} audio frames, {:.2} MB audio",
            s.frames_captured,
            s.access_units_sent,
            s.frames_stalled,
            s.audio_frames_sent,
            s.audio_bytes_sent as f64 / 1_000_000.0
        );
        Ok(())
    }
}

/// Beats the control connection until the session stops.
///
/// On its own thread rather than inside [`send_frames`]: a still screen makes the
/// encoder produce nothing, and a keepalive that rides on frame arrival would go
/// quiet during exactly the pause that a network reads as an idle connection.
fn spawn_keepalive(
    mut pacer: Pacer,
    running: &Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let flag = Arc::clone(running);
    std::thread::Builder::new()
        .name("mirror-keepalive".to_string())
        .spawn(move || {
            // A first beat straight away, so a receiver already gone is in the
            // log at the start of the session rather than five seconds into it.
            let mut due = Instant::now();
            let mut broken = false;
            while flag.load(Ordering::SeqCst) {
                let now = Instant::now();
                if now < due {
                    std::thread::sleep((due - now).min(Duration::from_millis(200)));
                    continue;
                }
                due = Instant::now() + KEEPALIVE_INTERVAL;
                if let Err(e) = pacer.beat() {
                    // One line, then silence: a socket that will not answer is
                    // not going to start, and a warning every five seconds would
                    // bury the log entry that says why the session ended.
                    if !broken {
                        broken = true;
                        warn!("The mirroring control connection stopped answering: {e:#}");
                    }
                } else {
                    broken = false;
                }
            }
        })
        .context("start the mirror keepalive thread")
}

/// Writes coded pictures to one mirroring connection until the session ends.
fn send_frames(
    mut writer: MirrorWriter,
    au_rx: &Receiver<AccessUnit>,
    running: &AtomicBool,
    stats: &Arc<Mutex<StreamStats>>,
    source: (u32, u32),
    output: (u32, u32),
) {
    let mut codec: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut window_start = Instant::now();
    let mut window_units = 0u64;
    let mut window_bytes = 0u64;
    // `HORIZON_SEND_TRACE=1` prints the interval between pictures leaving the
    // socket. Even pacing is what a display can show evenly, and this is the
    // only place in the sender that can see it: not the capture tick, not the
    // encoder's opinion of itself.
    let trace = std::env::var_os("HORIZON_SEND_TRACE").is_some();
    let mut last_wire = Instant::now();
    let mut wire_count = 0u64;
    let mut wire_sum_ms = 0.0f64;
    let mut wire_min_ms = f64::MAX;
    let mut wire_max_ms = 0.0f64;

    while running.load(Ordering::SeqCst) {
        let au = match au_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(au) => au,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // The stall clock is read on every pass, including the passes that
        // encode nothing. A receiver that stopped taking data used to be
        // discovered by the local stack giving up on the connection, which
        // surfaced as `os error 10053` and ended the session.
        if let Err(e) = writer.pump() {
            fail(stats, "mirror video write", &format!("{e:#}"));
            return;
        }
        if writer.stalled_for() >= MAX_STALL {
            fail(
                stats,
                "mirror video write",
                &format!(
                    "接收端 {} 秒未接收任何数据，网络已中断或接收端已关闭",
                    MAX_STALL.as_secs()
                ),
            );
            return;
        }

        let (sps, pps, picture) = split_parameter_sets(&au.nalus);
        if let (Some(sps), Some(pps)) = (sps, pps) {
            if codec.as_ref() != Some(&(sps.clone(), pps.clone())) {
                // The receiver rebuilds every key frame from this, so parameter
                // sets the encoder changed mid-session have to be re-announced.
                if let Err(e) = writer.write_codec_config(&sps, &pps, source, output) {
                    fail(stats, "codec config", &format!("{e:#}"));
                    return;
                }
                codec = Some((sps, pps));
            }
        }
        if picture.is_empty() {
            continue;
        }

        match writer.write_video(&picture, ntp_now()) {
            Err(e) => {
                fail(stats, "mirror video write", &format!("{e:#}"));
                return;
            }
            // The socket is full: this picture is dropped rather than queued
            // behind the backlog, because the next IDR arrives within a couple
            // of seconds and a short run of artifacts beats a dead session.
            Ok(false) => {
                let mut s = stats.lock().unwrap();
                s.frames_stalled += 1;
                if s.frames_stalled == 1 {
                    warn!(
                        "The receiver is not taking data; {} bytes queued, dropping new frames",
                        writer.queued()
                    );
                }
                continue;
            }
            Ok(true) => {}
        }

        let mut s = stats.lock().unwrap();
        s.latency_ms = Instant::now()
            .duration_since(au.captured_at)
            .as_secs_f64()
            * 1000.0;
        s.access_units_sent += 1;
        window_units += 1;
        // The writer counts everything it has put on the socket, headers
        // included, so the window sees the difference since the last picture.
        let delta = writer.bytes - s.bytes_sent;
        s.bytes_sent = writer.bytes;
        window_bytes += delta;
        if trace {
            let gap_ms = last_wire.elapsed().as_secs_f64() * 1000.0;
            last_wire = Instant::now();
            wire_count += 1;
            wire_sum_ms += gap_ms;
            wire_min_ms = wire_min_ms.min(gap_ms);
            wire_max_ms = wire_max_ms.max(gap_ms);
            if wire_count % 100 == 0 {
                eprintln!(
                    "send wire window: {} frames gap min {:.1} avg {:.1} max {:.1} ms",
                    wire_count,
                    wire_min_ms,
                    wire_sum_ms / wire_count as f64,
                    wire_max_ms
                );
                wire_min_ms = f64::MAX;
                wire_max_ms = 0.0;
                wire_sum_ms = 0.0;
                wire_count = 0;
            }
        }
        if window_start.elapsed() >= Duration::from_secs(1) {
            let secs = window_start.elapsed().as_secs_f64();
            s.fps = window_units as f64 / secs;
            s.bitrate_mbps = window_bytes as f64 * 8.0 / secs / 1_000_000.0;
            window_start = Instant::now();
            window_units = 0;
            window_bytes = 0;
        }
    }
}

/// A closed mirror socket ends the session; a receiver cannot be fed in silence.
fn fail(stats: &Arc<Mutex<StreamStats>>, what: &str, why: &str) {
    let text = format!("{what} failed: {why}");
    warn!("{text}");
    stats.lock().unwrap().error = Some(text);
}

/// Splits a coded picture into its parameter sets and the slices that follow them.
fn split_parameter_sets(nalus: &[Nal]) -> (Option<Vec<u8>>, Option<Vec<u8>>, Vec<Nal>) {
    let mut sps = None;
    let mut pps = None;
    let mut picture = Vec::new();
    for nal in nalus {
        match nal.nal_type() {
            crate::airplay::h264::NalType::Sps => sps = Some(nal.data.clone()),
            crate::airplay::h264::NalType::Pps => pps = Some(nal.data.clone()),
            _ => picture.push(nal.clone()),
        }
    }
    (sps, pps, picture)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::h264::NalType;

    fn nal(header: u8, rest: &[u8]) -> Nal {
        let mut data = vec![header];
        data.extend_from_slice(rest);
        Nal { data }
    }

    #[test]
    fn parameter_sets_leave_the_picture() {
        let nalus = vec![nal(0x67, &[1, 2]), nal(0x68, &[3]), nal(0x65, &[4, 5, 6])];
        let (sps, pps, picture) = split_parameter_sets(&nalus);
        assert_eq!(sps.unwrap(), vec![0x67, 1, 2]);
        assert_eq!(pps.unwrap(), vec![0x68, 3]);
        // The receiver prepends the parameter sets itself, so a picture that kept
        // them would double them up in front of every key frame.
        assert_eq!(picture.len(), 1);
        assert_eq!(picture[0].nal_type(), NalType::SliceIdr);
    }

    #[test]
    fn a_picture_without_parameter_sets_is_left_alone() {
        let nalus = vec![nal(0x41, &[9])];
        let (sps, pps, picture) = split_parameter_sets(&nalus);
        assert!(sps.is_none() && pps.is_none());
        assert_eq!(picture.len(), 1);
    }
}
