use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, HANDLE, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
    D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ,
    D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTOPRIMARY, GetMonitorInfoW, MONITORINFOEXW};

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub pitch: u32,
}

pub struct ScreenCapture {
    duplication: IDXGIOutputDuplication,
    staging_texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl ScreenCapture {
    pub fn new() -> Result<Self> {
        info!("Initializing DXGI screen capture");

        unsafe {
            // Create D3D11 device
            let mut device = None;
            let mut feature_level = Default::default();
            let feature_levels = [
                windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0,
                windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_1,
                windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_0,
                windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_9_1,
            ];

            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut feature_level),
                None,
            )
            .context("Failed to create D3D11 device")?;

            let device = device.context("D3D11 device is None")?;

            // Get DXGI device
            let dxgi_device: windows::Win32::Graphics::Dxgi::IDXGIDevice =
                device.cast().context("Failed to get DXGI device")?;

            // Get DXGI adapter
            let adapter = dxgi_device
                .GetAdapter()
                .context("Failed to get DXGI adapter")?;

            // Enumerate outputs to find the primary display
            let output = adapter
                .EnumOutputs(0)
                .context("Failed to enumerate outputs")?;

            // Cast to IDXGIOutput1 for DuplicateOutput
            let output1: IDXGIOutput1 = output.cast().context("Failed to cast to IDXGIOutput1")?;

            // Create output duplication
            let duplication = output1
                .DuplicateOutput(&dxgi_device)
                .context("Failed to create output duplication")?;

            // Get description
            let desc: DXGI_OUTDUPL_DESC = duplication.GetDesc();
            let mode_desc = desc.ModeDesc;
            let width = mode_desc.Width;
            let height = mode_desc.Height;

            info!("Screen capture: {}x{}", width, height);

            // Create staging texture for CPU readback
            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: mode_desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0u32,
            };

            let mut staging_texture = None;
            device
                .CreateTexture2D(&tex_desc, None, Some(&mut staging_texture))
                .context("Failed to create staging texture")?;

            let staging_texture = staging_texture.context("Staging texture is None")?;

            Ok(Self {
                duplication,
                staging_texture,
                width,
                height,
            })
        }
    }

    pub fn capture_frame(&self, timeout_ms: u32) -> Result<Option<CapturedFrame>> {
        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            let hr = self.duplication.AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource);

            if let Err(e) = hr {
                let code = e.code();
                if code == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(None);
                }
                if code == DXGI_ERROR_ACCESS_LOST {
                    warn!("Desktop duplication access lost");
                    return Err(anyhow::anyhow!("Access lost - desktop composition changed"));
                }
                return Err(e).context("Failed to acquire frame");
            }

            let resource = resource.context("No resource returned")?;

            let _texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D =
                resource.cast().context("Failed to cast to texture2D")?;

            // Note: In a real implementation, we'd copy the texture to the staging texture first
            // using ID3D11DeviceContext::CopyResource, then map the staging texture.
            // For the initial scaffold, we return the frame dimensions.

            let _ = self.duplication.ReleaseFrame();

            Ok(Some(CapturedFrame {
                width: self.width,
                height: self.height,
                data: Vec::new(), // Will be populated with actual pixel data
                pitch: self.width * 4,
            }))
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

pub fn get_primary_monitor_size() -> Result<(u32, u32)> {
    unsafe {
        let point = POINT { x: 0, y: 0 };
        let hmonitor = MonitorFromPoint(point, MONITOR_DEFAULTTOPRIMARY);
        if hmonitor.is_invalid() {
            return Err(anyhow::anyhow!("No primary monitor found"));
        }

        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

        if !GetMonitorInfoW(hmonitor, &mut info.monitorInfo).as_bool() {
            return Err(anyhow::anyhow!("GetMonitorInfoW failed"));
        }

        let rect = info.monitorInfo.rcMonitor;
        let width = (rect.right - rect.left) as u32;
        let height = (rect.bottom - rect.top) as u32;

        Ok((width, height))
    }
}
