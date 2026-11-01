use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use crate::airplay::rtp::{RtcpSenderReport, RtpPacketizer};
use crate::capture::encoder::{EncoderType, VideoEncoder};
use crate::capture::screen::{CapturedFrame, ScreenCapture};

pub struct StreamSession {
    running: Arc<AtomicBool>,
    stats: StreamStats,
}

#[derive(Debug, Clone, Default)]
pub struct StreamStats {
    pub frames_captured: u64,
    pub frames_encoded: u64,
    pub bytes_sent: u64,
    pub rtp_packets_sent: u64,
    pub fps: f64,
    pub bitrate_mbps: f64,
    pub latency_ms: f64,
}

impl StreamSession {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            stats: StreamStats::default(),
        }
    }

    pub async fn start(
        &mut self,
        host: &str,
        port: u16,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<()> {
        info!("Starting stream session to {}:{} ({}x{}@{}fps)", host, port, width, height, fps);

        self.running.store(true, Ordering::SeqCst);

        // Step 1: RTSP handshake
        let (video_port, audio_port) = self.rtsp_handshake(host, port, width, height, fps).await?;
        info!("RTSP negotiated: video_port={}, audio_port={}", video_port, audio_port);

        // Step 2: Initialize screen capture
        let capture = ScreenCapture::new().context("Failed to init screen capture")?;
        info!("Screen capture ready: {}x{}", capture.width(), capture.height());

        // Step 3: Initialize encoder
        let mut encoder = VideoEncoder::new(
            capture.width(),
            capture.height(),
            fps,
            bitrate,
            EncoderType::Auto,
        );
        encoder.start().context("Failed to start encoder")?;
        info!("Encoder started");

        // Step 4: Create UDP socket for RTP
        let udp_socket = UdpSocket::bind("0.0.0.0:0").context("Failed to bind UDP socket")?;
        let target_addr = format!("{}:{}", host, video_port);
        udp_socket
            .connect(&target_addr)
            .context("Failed to connect UDP socket")?;
        info!("UDP connected to {}", target_addr);

        // Step 5: Create RTP packetizer
        let mut video_rtp = RtpPacketizer::new(96, 90000);
        let mut rtcp_sr = RtcpSenderReport::new();

        // Step 6: Streaming loop
        let running = self.running.clone();
        let frame_interval = std::time::Duration::from_micros(1_000_000 / fps as u64);
        let mut last_frame_time = Instant::now();
        let mut stats_start = Instant::now();
        let mut frame_count = 0u64;

        info!("Streaming started!");

        while running.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now.duration_since(last_frame_time) < frame_interval {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            last_frame_time = now;

            // Capture frame
            match capture.capture_frame(100) {
                Ok(Some(frame)) => {
                    self.stats.frames_captured += 1;

                    // Encode frame
                    if let Err(e) = encoder.send_frame(&frame.data) {
                        error!("Encoder error: {}", e);
                        continue;
                    }
                    self.stats.frames_encoded += 1;

                    // For now, we treat the entire frame as a single NAL unit
                    // In production, we'd read ffmpeg's stdout for actual H.264 data
                    // and parse NAL units. This is a placeholder that sends raw frame
                    // data wrapped in RTP for pipeline validation.
                    let pts_ms = now.duration_since(stats_start).as_millis() as u64;
                    let rtp_packet = video_rtp.packetize_h264(&frame.data, pts_ms);

                    if let Err(e) = udp_socket.send(&rtp_packet) {
                        warn!("UDP send error: {}", e);
                    } else {
                        self.stats.bytes_sent += rtp_packet.len() as u64;
                        self.stats.rtp_packets_sent += 1;
                    }

                    frame_count += 1;

                    // Update stats every second
                    let elapsed = stats_start.elapsed();
                    if elapsed.as_secs() >= 1 {
                        self.stats.fps = frame_count as f64 / elapsed.as_secs_f64();
                        self.stats.bitrate_mbps =
                            self.stats.bytes_sent as f64 * 8.0 / elapsed.as_secs_f64() / 1_000_000.0;
                        frame_count = 0;
                        self.stats.bytes_sent = 0;
                        stats_start = Instant::now();
                    }
                }
                Ok(None) => {
                    // No frame available (screen unchanged)
                }
                Err(e) => {
                    warn!("Capture error: {}", e);
                }
            }
        }

        // Cleanup
        encoder.stop()?;
        self.rtsp_teardown(host, port).await?;
        info!("Stream session ended. Total frames: {}", self.stats.frames_captured);

        Ok(())
    }

    pub fn stop(&mut self) {
        info!("Stopping stream session");
        self.running.store(false, Ordering::SeqCst);
    }

    pub fn stats(&self) -> &StreamStats {
        &self.stats
    }

    async fn rtsp_handshake(
        &self,
        host: &str,
        port: u16,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<(u16, u16)> {
        let addr = format!("{}:{}", host, port);
        let mut stream = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("Failed to connect to {}", addr))?;

        let mut cseq = 1u32;

        // OPTIONS
        let options_req = format!(
            "OPTIONS * RTSP/1.0\r\nCSeq: {}\r\nApple-Jack-Status: connected; type=analog\r\n\r\n",
            cseq
        );
        stream.write_all(options_req.as_bytes()).await?;
        cseq += 1;
        debug!("RTSP >> OPTIONS");

        let response = Self::read_rtsp_response(&mut stream).await?;
        debug!("RTSP << OPTIONS response: {}", response.status_line);

        // ANNOUNCE with SDP
        let sdp = format!(
            "v=0\r\n\
             o=AirSend 0 0 IN IP4 0.0.0.0\r\n\
             s=Screen Mirror\r\n\
             t=0 0\r\n\
             m=video 0 RTP/AVP 96\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=rtpmap:96 H264/90000\r\n\
             a=fmtp:96 packetization-mode=1;profile-level-id=42001e\r\n\
             a=framerate:{fps}\r\n\
             a=framesize:96 {width}-{height}\r\n",
            fps = fps,
            width = width,
            height = height,
        );

        let announce_req = format!(
            "ANNOUNCE * RTSP/1.0\r\nCSeq: {}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
            cseq,
            sdp.len(),
            sdp
        );
        stream.write_all(announce_req.as_bytes()).await?;
        cseq += 1;
        debug!("RTSP >> ANNOUNCE");

        let _response = Self::read_rtsp_response(&mut stream).await?;
        debug!("RTSP << ANNOUNCE response");

        // SETUP
        let setup_req = format!(
            "SETUP * RTSP/1.0\r\nCSeq: {}\r\nTransport: RTP/AVP/UDP;unicast;client_port={}\r\n\r\n",
            cseq,
            port + 1
        );
        stream.write_all(setup_req.as_bytes()).await?;
        cseq += 1;
        debug!("RTSP >> SETUP");

        let response = Self::read_rtsp_response(&mut stream).await?;
        debug!("RTSP << SETUP response");

        // Extract server port from Transport header
        let video_port = response
            .headers
            .get("transport")
            .or_else(|| response.headers.get("Transport"))
            .and_then(|t| {
                t.split(';')
                    .find(|p| p.trim().starts_with("server_port="))
                    .and_then(|p| p.split('=').nth(1))
                    .and_then(|p| p.trim().split('-').next())
                    .and_then(|p| p.parse().ok())
            })
            .unwrap_or(port + 1);

        // RECORD
        let record_req = format!(
            "RECORD * RTSP/1.0\r\nCSeq: {}\r\nSession: 1\r\nRange: npt=0-\r\nRTP-Info: seq={};rtptime={}\r\n\r\n",
            cseq,
            rand::random::<u16>(),
            chrono::Utc::now().timestamp() as u32,
        );
        stream.write_all(record_req.as_bytes()).await?;
        cseq += 1;
        debug!("RTSP >> RECORD");

        let _response = Self::read_rtsp_response(&mut stream).await?;
        debug!("RTSP << RECORD response");

        info!("RTSP handshake complete");
        Ok((video_port, 0))
    }

    async fn rtsp_teardown(&self, host: &str, port: u16) -> Result<()> {
        let addr = format!("{}:{}", host, port);
        if let Ok(mut stream) = TcpStream::connect(&addr).await {
            let teardown = "TEARDOWN * RTSP/1.0\r\nCSeq: 99\r\nSession: 1\r\n\r\n";
            let _ = stream.write_all(teardown.as_bytes()).await;
            debug!("RTSP >> TEARDOWN");
        }
        Ok(())
    }

    async fn read_rtsp_response(stream: &mut TcpStream) -> Result<RtspRawResponse> {
        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).await?;

        let mut headers = HashMap::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some((key, value)) = trimmed.split_once(':') {
                headers.insert(key.trim().to_lowercase(), value.trim().to_string());
            }
        }

        Ok(RtspRawResponse {
            status_line: status_line.trim().to_string(),
            headers,
        })
    }
}

struct RtspRawResponse {
    status_line: String,
    headers: HashMap<String, String>,
}
