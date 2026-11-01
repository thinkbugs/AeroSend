use anyhow::{Context, Result};
use ffmpeg_sidecar::{child::FfmpegChild, command::FfmpegCommand};
use std::io::Write;
use std::process::ChildStdin;
use tracing::info;

pub struct VideoEncoder {
    stdin: Option<ChildStdin>,
    ffmpeg: Option<FfmpegChild>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    encoder_type: EncoderType,
}

#[derive(Debug, Clone, Copy)]
pub enum EncoderType {
    Auto,
    Nvenc,
    Qsv,
    X264,
}

impl EncoderType {
    pub fn as_str(&self) -> &str {
        match self {
            EncoderType::Auto => "libx264",
            EncoderType::Nvenc => "h264_nvenc",
            EncoderType::Qsv => "h264_qsv",
            EncoderType::X264 => "libx264",
        }
    }
}

impl VideoEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, encoder: EncoderType) -> Self {
        Self {
            stdin: None,
            ffmpeg: None,
            width,
            height,
            fps,
            bitrate,
            encoder_type: encoder,
        }
    }

    pub fn start(&mut self) -> Result<()> {
        info!(
            "Starting video encoder: {}x{} @ {}fps, {}kbps, codec={}",
            self.width,
            self.height,
            self.fps,
            self.bitrate / 1000,
            self.encoder_type.as_str()
        );

        let encoder_name = self.encoder_type.as_str();
        let bitrate_str = format!("{}k", self.bitrate / 1000);
        let size_str = format!("{}x{}", self.width, self.height);
        let gop_size = (self.fps * 2).to_string();

        let mut ffmpeg = FfmpegCommand::new()
            .format("rawvideo")
            .pix_fmt("bgra")
            .args(["-s", &size_str])
            .args(["-r", &self.fps.to_string()])
            .input("-")
            .args(["-c:v", encoder_name])
            .pix_fmt("yuv420p")
            .args(["-b:v", &bitrate_str])
            .args(["-r", &self.fps.to_string()])
            .args(["-preset", "ultrafast"])
            .args(["-tune", "zerolatency"])
            .args(["-g", &gop_size])
            .args(["-bf", "0"])
            .format("h264")
            .output("-")
            .spawn()
            .context("Failed to spawn ffmpeg")?;

        let stdin = ffmpeg.take_stdin().context("Failed to take ffmpeg stdin")?;
        self.stdin = Some(stdin);
        self.ffmpeg = Some(ffmpeg);

        info!("Video encoder started");
        Ok(())
    }

    pub fn send_frame(&mut self, frame_data: &[u8]) -> Result<()> {
        let stdin = self.stdin.as_mut().context("Encoder not started")?;
        stdin.write_all(frame_data).context("Failed to write frame")?;
        stdin.flush().context("Failed to flush")?;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.stdin.take();
        if let Some(mut ffmpeg) = self.ffmpeg.take() {
            let _ = ffmpeg.quit();
            let _ = ffmpeg.wait();
        }
        info!("Video encoder stopped");
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.ffmpeg.is_some()
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
