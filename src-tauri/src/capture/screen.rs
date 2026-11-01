use anyhow::{Context, Result};
use tracing::{info, warn};
use windows::core::Interface;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11_USAGE_STAGING,
    D3D11_CPU_ACCESS_READ, D3D11_TEXTURE2D_DESC, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    ID3D11Device, ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFOEXW, MONITOR_DEFAULTTOPRIMARY,
};

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub pitch: u32,
}

pub struct ScreenCapture {
    duplication: IDXGIOutputDuplication,
    #[allow(dead_code)]
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,
    staging_texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl ScreenCapture {
    pub fn new() -> Result<Self> {
        info!("Initializing DXGI Desktop Duplication");

        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut feature_level = Default::default();
            let mut context: Option<ID3D11DeviceContext> = None;

            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )
            .context("Failed to create D3D11 device")?;

            let device = device.context("D3D11 device is None")?;
            let device_context = context.context("D3D11 device context is None")?;

            let dxgi_device: windows::Win32::Graphics::Dxgi::IDXGIDevice =
                device.cast().context("Failed to get DXGI device")?;

            let adapter = dxgi_device.GetAdapter().context("Failed to get DXGI adapter")?;
            let output = adapter.EnumOutputs(0).context("No display outputs found")?;
            let output1: IDXGIOutput1 = output.cast().context("Failed to cast to IDXGIOutput1")?;

            let duplication = output1
                .DuplicateOutput(&dxgi_device)
                .context("Failed to create output duplication")?;

            let desc: DXGI_OUTDUPL_DESC = duplication.GetDesc();
            let width = desc.ModeDesc.Width;
            let height = desc.ModeDesc.Height;
            info!("Screen capture initialized: {}x{}", width, height);

            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };

            let mut staging_texture = None;
            device
                .CreateTexture2D(&tex_desc, None, Some(&mut staging_texture))
                .context("Failed to create staging texture")?;
            let staging_texture = staging_texture.context("Staging texture is None")?;

            Ok(Self {
                duplication,
                device,
                device_context,
                staging_texture,
                width,
                height,
            })
        }
    }

    pub fn capture_frame(&self, timeout_ms: u32) -> Result<Option<CapturedFrame>> {
        unsafe {
            let mut frame_info = Default::default();
            let mut resource: Option<IDXGIResource> = None;

            let hr = self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource);

            if let Err(e) = hr {
                let code = e.code();
                if code == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(None);
                }
                if code == DXGI_ERROR_ACCESS_LOST {
                    warn!("Desktop duplication access lost");
                    return Err(anyhow::anyhow!("Access lost"));
                }
                return Err(e).context("AcquireNextFrame failed");
            }

            let resource = resource.context("No resource returned")?;
            let desktop_texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D =
                resource.cast().context("Resource is not a Texture2D")?;

            self.device_context
                .CopyResource(&self.staging_texture, &desktop_texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.device_context
                .Map(&self.staging_texture, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Failed to map staging texture")?;

            let pitch = mapped.RowPitch;
            let row_bytes = self.width * 4;
            let mut data = Vec::with_capacity((row_bytes * self.height) as usize);

            let src = mapped.pData as *const u8;
            for row in 0..self.height {
                let src_row = src.add((row * pitch) as usize);
                data.extend_from_slice(std::slice::from_raw_parts(src_row, row_bytes as usize));
            }

            self.device_context.Unmap(&self.staging_texture, 0);
            let _ = self.duplication.ReleaseFrame();

            Ok(Some(CapturedFrame {
                width: self.width,
                height: self.height,
                data,
                pitch: row_bytes,
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
        Ok((
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        ))
    }
}
