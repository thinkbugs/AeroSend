use anyhow::{Context, Result};
use ffmpeg_sidecar::child::FfmpegChild;
use ffmpeg_sidecar::command::FfmpegCommand;
use std::io::Write;
use std::process::{ChildStdin, ChildStdout};
use tracing::info;

use super::audio::AudioFormat;

pub struct AudioEncoder {
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    ffmpeg: Option<FfmpegChild>,
    format: AudioFormat,
    bitrate: u32,
}

impl AudioEncoder {
    pub fn new(format: AudioFormat, bitrate: u32) -> Self {
        Self {
            stdin: None,
            stdout: None,
            ffmpeg: None,
            format,
            bitrate,
        }
    }

    pub fn start(&mut self) -> Result<()> {
        info!(
            "Starting audio encoder: {}Hz {}ch, {}kbps -> AAC",
            self.format.sample_rate, self.format.channels, self.bitrate / 1000
        );

        let in_fmt = if self.format.is_float { "f32le" } else { "s16le" };
        let ch_str = self.format.channels.to_string();
        let sr_str = self.format.sample_rate.to_string();
        let bitrate_str = format!("{}k", self.bitrate / 1000);

        let mut ffmpeg = FfmpegCommand::new()
            .args(["-f", in_fmt])
            .args(["-ar", &sr_str])
            .args(["-ac", &ch_str])
            .input("-")
            .args(["-c:a", "aac"])
            .args(["-b:a", &bitrate_str])
            .args(["-ac", "2"])
            .args(["-ar", "44100"])
            .args(["-f", "adts"])
            .output("-")
            .spawn()
            .context("Failed to spawn ffmpeg audio encoder")?;

        let stdin = ffmpeg.take_stdin().context("Failed to take ffmpeg stdin")?;
        self.stdout = Some(
            ffmpeg
                .take_stdout()
                .context("Failed to take ffmpeg stdout")?,
        );
        self.stdin = Some(stdin);
        self.ffmpeg = Some(ffmpeg);
        Ok(())
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    pub fn send_pcm(&mut self, pcm: &[u8]) -> Result<()> {
        let stdin = self.stdin.as_mut().context("Audio encoder not started")?;
        stdin.write_all(pcm).context("Failed to write PCM to ffmpeg")?;
        stdin.flush().context("Failed to flush audio encoder")?;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.stdin.take();
        self.stdout.take();
        if let Some(mut ffmpeg) = self.ffmpeg.take() {
            let _ = ffmpeg.quit();
            let _ = ffmpeg.wait();
        }
        info!("Audio encoder stopped");
        Ok(())
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
