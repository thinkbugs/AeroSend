use anyhow::{bail, Context, Result};
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{info, warn};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

const AUDCLNT_SILENT: u32 = 0x2;

#[derive(Debug)]
pub struct PollResult {
    pub pcm: Vec<u8>,
    pub silent_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub is_float: bool,
    pub block_align: u16,
}

pub struct AudioCapture {
    audio_client: Option<IAudioClient>,
    capture_client: Option<IAudioCaptureClient>,
    format: AudioFormat,
    running: Arc<AtomicBool>,
    com_initialized: bool,
}

impl AudioCapture {
    pub fn new() -> Result<Self> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() {
                bail!("CoInitializeEx failed: {:?}", hr);
            }

            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("Failed to create MMDeviceEnumerator")?;

            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("No default render endpoint found")?;

            let audio_client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .context("Failed to activate IAudioClient")?;

            let pwfx: *mut WAVEFORMATEX = audio_client
                .GetMixFormat()
                .context("Failed to get mix format")?;
            if pwfx.is_null() {
                bail!("GetMixFormat returned null");
            }

            let wf = &*pwfx;
            let format = AudioFormat {
                sample_rate: wf.nSamplesPerSec,
                channels: wf.nChannels,
                bits_per_sample: wf.wBitsPerSample,
                is_float: wf.wFormatTag == 0xFFFE || wf.wFormatTag == 3,
                block_align: wf.nBlockAlign,
            };
            info!(
                "Audio mix format: {}Hz {}ch {}bit float={}",
                format.sample_rate, format.channels, format.bits_per_sample, format.is_float
            );

            let init_result = audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                10_000_000,
                0,
                pwfx,
                None,
            );

            CoTaskMemFree(Some(pwfx as *const _));

            init_result.map_err(|e| anyhow::anyhow!("Failed to init audio client: {:?}", e))?;

            let capture_client: IAudioCaptureClient = audio_client
                .GetService()
                .context("Failed to get IAudioCaptureClient")?;

            Ok(Self {
                audio_client: Some(audio_client),
                capture_client: Some(capture_client),
                format,
                running: Arc::new(AtomicBool::new(false)),
                com_initialized: true,
            })
        }
    }

    pub fn running_flag(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    pub fn format(&self) -> &AudioFormat {
        &self.format
    }

    pub fn start(&self) -> Result<()> {
        if let Some(client) = &self.audio_client {
            unsafe {
                client.Start().map_err(|e| anyhow::anyhow!("Start failed: {:?}", e))?;
            }
        }
        self.running.store(true, Ordering::SeqCst);
        info!("WASAPI loopback capture started");
        Ok(())
    }

    pub fn stop(&self) {
        if let Some(client) = &self.audio_client {
            unsafe {
                let _ = client.Stop();
            }
        }
        self.running.store(false, Ordering::SeqCst);
        info!("WASAPI loopback capture stopped");
    }

    /// Returns the loopback PCM for this poll plus how many of those bytes were
    /// driver-flagged silence. Silence is still returned because the encoder
    /// timeline must stay continuous.
    pub fn poll(&self) -> Option<PollResult> {
        let capture = self.capture_client.as_ref()?;
        let mut out = Vec::new();
        let mut silent_bytes = 0usize;
        unsafe {
            let mut packet_count = match capture.GetNextPacketSize() {
                Ok(n) => n,
                Err(_) => return None,
            };

            while packet_count != 0 {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut num_frames: u32 = 0;
                let mut flags: u32 = 0;
                if capture
                    .GetBuffer(&mut data, &mut num_frames, &mut flags, None, None)
                    .is_err()
                {
                    warn!("GetBuffer failed");
                    break;
                }

                if flags & AUDCLNT_SILENT != 0 {
                    silent_bytes += num_frames as usize * self.format.block_align as usize;
                }

                let bytes_len = num_frames as usize * self.format.block_align as usize;
                if bytes_len > 0 {
                    // Silent packets carry no buffer but still occupy the timeline;
                    // emitting zeros keeps the encoder clock continuous.
                    if data.is_null() {
                        out.resize(out.len() + bytes_len, 0);
                    } else {
                        out.extend_from_slice(slice::from_raw_parts(data, bytes_len));
                    }
                }

                let _ = capture.ReleaseBuffer(num_frames);
                packet_count = match capture.GetNextPacketSize() {
                    Ok(n) => n,
                    Err(_) => break,
                };
            }
        }

        if out.is_empty() {
            None
        } else {
            Some(PollResult {
                pcm: out,
                silent_bytes,
            })
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop();
        self.audio_client = None;
        self.capture_client = None;
        if self.com_initialized {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
