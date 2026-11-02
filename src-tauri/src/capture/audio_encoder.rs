use anyhow::{Context, Result};
use ffmpeg_sidecar::child::FfmpegChild;
use ffmpeg_sidecar::command::FfmpegCommand;
use std::io::Write;
use std::process::{ChildStdin, ChildStdout};
use tracing::info;

use super::audio::AudioFormat;

/// What the converter writes on its stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioOutput {
    /// Big-endian 16-bit PCM for RAOP's L16 payloads.
    L16,
    /// ADTS-framed AAC-LC for the AirPlay 2 mirroring audio stream. ADTS is
    /// what makes the stream self-delimiting: every frame carries its own
    /// length, so the sender can cut the encoder's byte stream into packets.
    AacLc { bitrate: u32 },
}

pub struct AudioEncoder {
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    ffmpeg: Option<FfmpegChild>,
    format: AudioFormat,
    output: AudioOutput,
}

impl AudioEncoder {
    pub fn new(format: AudioFormat) -> Self {
        Self::with_output(format, AudioOutput::L16)
    }

    /// AAC-LC at 44.1 kHz stereo. This is the closest format the ffmpeg build
    /// on Windows can produce to what Apple's senders put on stream 96: they
    /// use AAC-ELD, which needs libfdk_aac and is not in the stock build.
    pub fn aac_lc(format: AudioFormat, bitrate: u32) -> Self {
        Self::with_output(format, AudioOutput::AacLc { bitrate })
    }

    fn with_output(format: AudioFormat, output: AudioOutput) -> Self {
        Self {
            stdin: None,
            stdout: None,
            ffmpeg: None,
            format,
            output,
        }
    }

    pub fn start(&mut self) -> Result<()> {
        let in_fmt = if self.format.is_float { "f32le" } else { "s16le" };
        let ch_str = self.format.channels.to_string();
        let sr_str = self.format.sample_rate.to_string();

        info!(
            "Starting audio resample: {}Hz {}ch -> 44100Hz 2ch {}",
            self.format.sample_rate,
            self.format.channels,
            match self.output {
                AudioOutput::L16 => "s16be (L16)".to_string(),
                AudioOutput::AacLc { bitrate } => format!("AAC-LC {} kbps", bitrate / 1000),
            }
        );

        // ffmpeg is used for the two conversions the buffer cannot do itself:
        // sample rate and channel layout.
        let mut command = FfmpegCommand::new();
        command
            .args(["-f", in_fmt])
            .args(["-ar", &sr_str])
            .args(["-ac", &ch_str])
            .input("-");
        match self.output {
            AudioOutput::L16 => {
                command
                    .args(["-ac", "2"])
                    .args(["-ar", "44100"])
                    .args(["-f", "s16be"]);
            }
            AudioOutput::AacLc { bitrate } => {
                command
                    .args(["-c:a", "aac"])
                    .args(["-profile:a", "aac_low"])
                    .args(["-b:a", &bitrate.to_string()])
                    .args(["-ac", "2"])
                    .args(["-ar", "44100"])
                    .args(["-f", "adts"]);
            }
        }
        let mut ffmpeg = command
            .output("-")
            .spawn()
            .context("Failed to spawn ffmpeg audio converter")?;

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
