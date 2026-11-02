use anyhow::{Context, Result};
use ffmpeg_sidecar::{child::FfmpegChild, command::FfmpegCommand};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{ChildStdin, ChildStdout};
use std::sync::LazyLock;
use tracing::info;

/// ffmpeg is a console program and AeroSend is not, so a raw spawn gets its own
/// console window: starting a cast used to flash one window per probe before the
/// picture even began. `FfmpegCommand` sets this for itself; these are the
/// encoders we ask about directly.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Which H.264 encoders the bundled ffmpeg actually provides.
#[derive(Debug, Clone)]
pub struct EncoderProbe {
    pub ffmpeg: String,
    pub x264: bool,
    pub nvenc: bool,
    pub qsv: bool,
}

/// Asking ffmpeg costs a process spawn, so the answer is kept for the process.
static PROBE: LazyLock<Option<EncoderProbe>> = LazyLock::new(|| probe_encoders().ok());

/// Whether the encoder can actually open, not merely whether it is listed.
///
/// This machine proves the difference: its ffmpeg lists `h264_nvenc` and then
/// fails the first frame with "Cannot load nvcuda.dll". Choosing it from the
/// listing alone hands the user an encoder that kills every session.
fn codec_opens(path: &std::path::Path, codec: &str) -> bool {
    use std::process::Stdio;
    let child = std::process::Command::new(path)
        .args(["-hide_banner", "-loglevel", "error"])
        .args(["-f", "rawvideo", "-pix_fmt", "bgra", "-s", "64x64", "-i", "-"])
        .args(["-c:v", codec, "-frames:v", "1", "-f", "null", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let Ok(mut child) = child else { return false };
    // One blank frame is enough to reach the encoder and see it refuse.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&[0u8; 64 * 64 * 4]);
    }
    let Ok(out) = child.wait_with_output() else {
        return false;
    };
    if out.status.success() {
        return true;
    }
    let text = String::from_utf8_lossy(&out.stderr);
    info!("{codec} cannot encode: {}", text.trim());
    false
}

pub fn probe_encoders() -> Result<EncoderProbe, String> {
    let path = ffmpeg_sidecar::paths::ffmpeg_path();
    let out = std::process::Command::new(&path)
        .args(["-hide_banner", "-encoders"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Could not run {} ({e})", path.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let provides = |name: &str| {
        text.lines()
            .any(|line| line.split_whitespace().any(|token| token == name))
            && codec_opens(path.as_path(), name)
    };
    let probe = EncoderProbe {
        ffmpeg: path.display().to_string(),
        x264: provides("libx264"),
        nvenc: provides("h264_nvenc"),
        qsv: provides("h264_qsv"),
    };
    info!(
        "ffmpeg {}: x264={} nvenc={} qsv={}",
        probe.ffmpeg, probe.x264, probe.nvenc, probe.qsv
    );
    Ok(probe)
}

pub struct VideoEncoder {
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    ffmpeg: Option<FfmpegChild>,
    /// Geometry of the BGRA frames written to stdin.
    input_width: u32,
    input_height: u32,
    /// Geometry of the encoded stream; differs from the input when scaled.
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    encoder_type: EncoderType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EncoderType {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "nvenc")]
    Nvenc,
    #[serde(rename = "qsv")]
    Qsv,
    #[serde(rename = "x264")]
    X264,
}

impl EncoderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EncoderType::Nvenc => "h264_nvenc",
            EncoderType::Qsv => "h264_qsv",
            EncoderType::X264 => "libx264",
            // "Auto" has to mean the GPU when there is one: software x264
            // cannot hold 30fps at a panel's native resolution, and the mirror
            // falls behind by seconds.
            EncoderType::Auto => match PROBE.as_ref() {
                Some(p) if p.nvenc => "h264_nvenc",
                Some(p) if p.qsv => "h264_qsv",
                _ => "libx264",
            },
        }
    }
}

impl VideoEncoder {
    pub fn new(
        input_width: u32,
        input_height: u32,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        encoder: EncoderType,
    ) -> Self {
        Self {
            stdin: None,
            stdout: None,
            ffmpeg: None,
            input_width,
            input_height,
            width,
            height,
            fps,
            bitrate,
            encoder_type: encoder,
        }
    }

    /// Encoded geometry, for the SDP framesize and the stats readout.
    pub fn output_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn start(&mut self) -> Result<()> {
        info!(
            "Starting video encoder: in {}x{} -> out {}x{} @ {}fps, {}kbps, codec={}",
            self.input_width,
            self.input_height,
            self.width,
            self.height,
            self.fps,
            self.bitrate / 1000,
            self.encoder_type.as_str()
        );

        let encoder_name = self.encoder_type.as_str();
        let bitrate_str = format!("{}k", self.bitrate / 1000);
        let input_size = format!("{}x{}", self.input_width, self.input_height);
        let scale_filter = format!("scale={}x{}", self.width, self.height);
        let gop_size = (self.fps * 2).to_string();
        let fps_str = self.fps.to_string();
        // Option names differ per encoder, so the low-latency flags follow the
        // encoder actually resolved rather than what was asked for.
        //
        // libx264 deliberately gets no `-x264-params sliced-threads=0:slices=1`:
        // measured on this machine, forcing single-slice encoding held the first
        // output until 1518ms after the first frame instead of 207ms. One picture
        // may then span several slice NALs, which costs nothing — they share one
        // RTP timestamp, so the access-unit grouper still yields one unit per
        // captured frame.
        let speed_args: &[&str] = match encoder_name {
            "libx264" => &["-preset", "ultrafast", "-tune", "zerolatency"],
            "h264_nvenc" => &["-preset", "p1", "-tune", "ll"],
            "h264_qsv" => &["-preset", "veryfast", "-async_depth", "1"],
            _ => &["-preset", "ultrafast"],
        };
        let scaled: &[&str] = if self.input_width == self.width && self.input_height == self.height {
            &[]
        } else {
            &["-vf", &scale_filter]
        };

        let mut ffmpeg = FfmpegCommand::new()
            .format("rawvideo")
            .pix_fmt("bgra")
            .args(["-s", &input_size])
            .args(["-r", &fps_str])
            .input("-")
            .args(scaled)
            .args(["-c:v", encoder_name])
            .pix_fmt("yuv420p")
            .args(["-b:v", &bitrate_str])
            .args(["-r", &fps_str])
            .args(speed_args)
            .args(["-g", &gop_size])
            .args(["-bf", "0"])
            .format("h264")
            .output("-")
            .spawn()
            .context("Failed to spawn ffmpeg")?;

        let stdin = ffmpeg.take_stdin().context("Failed to take ffmpeg stdin")?;
        self.stdout = Some(
            ffmpeg
                .take_stdout()
                .context("Failed to take ffmpeg stdout")?,
        );
        self.stdin = Some(stdin);
        self.ffmpeg = Some(ffmpeg);

        info!("Video encoder started");
        Ok(())
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    pub fn send_frame(&mut self, frame_data: &[u8]) -> Result<()> {
        let stdin = self.stdin.as_mut().context("Encoder not started")?;
        stdin.write_all(frame_data).context("Failed to write frame")?;
        stdin.flush().context("Failed to flush")?;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.stdin.take();
        self.stdout.take();
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
