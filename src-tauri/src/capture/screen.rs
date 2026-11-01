use anyhow::{Context, Result};
use tracing::{info, warn};
use windows::core::Interface;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11_USAGE_STAGING,
    D3D11_CPU_ACCESS_READ, D3D11_TEXTURE2D_DESC, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    ID3D11Device, ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory1, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
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
            let prepared = build_duplication()?;
            info!("Duplicating desktop from {}", prepared.source);

            let device = prepared.device;
            let device_context = prepared.device_context;
            let duplication = prepared.duplication;

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

struct PrimaryOutput {
    adapter: IDXGIAdapter,
    output: IDXGIOutput1,
    name: String,
}

/// A second GPU or a virtual display driver is not guaranteed to be the one the
/// desktop is scanned out from, and `D3D11CreateDevice` picks an adapter we do not
/// control. Duplicating an output that is attached but not hosting the desktop hands
/// back black frames without an error, so the primary monitor's desktop rectangle
/// picks both the adapter and the output.
fn primary_output() -> Option<PrimaryOutput> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let want = primary_monitor_rect().ok()?;

        for adapter_index in 0.. {
            let Ok(adapter) = factory.EnumAdapters(adapter_index) else {
                break;
            };
            for output_index in 0.. {
                let Ok(output) = adapter.EnumOutputs(output_index) else {
                    break;
                };
                let Ok(desc) = output.GetDesc() else {
                    continue;
                };
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let have = desc.DesktopCoordinates;
                let matches = have.left == want.left
                    && have.top == want.top
                    && have.right == want.right
                    && have.bottom == want.bottom;
                if matches {
                    let name = utf16_field(&desc.DeviceName);
                    let output: IDXGIOutput1 = output.cast().ok()?;
                    return Some(PrimaryOutput {
                        adapter,
                        output,
                        name,
                    });
                }
            }
        }
        None
    }
}

fn utf16_field(raw: &[u16]) -> String {
    let end = raw.iter().position(|c| *c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}

unsafe fn create_device(
    adapter: Option<&IDXGIAdapter>,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    // The driver type has to be UNKNOWN when the adapter is named; HARDWARE with a
    // non-null adapter is rejected outright.
    let driver_type = match adapter {
        Some(_) => D3D_DRIVER_TYPE_UNKNOWN,
        None => D3D_DRIVER_TYPE_HARDWARE,
    };

    let mut device: Option<ID3D11Device> = None;
    let mut feature_level = Default::default();
    let mut context: Option<ID3D11DeviceContext> = None;

    D3D11CreateDevice(
        adapter,
        driver_type,
        None,
        D3D11_CREATE_DEVICE_FLAG(0),
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        Some(&mut feature_level),
        Some(&mut context),
    )
    .context("Failed to create D3D11 device")?;

    Ok((
        device.context("D3D11 device is None")?,
        context.context("D3D11 device context is None")?,
    ))
}

unsafe fn duplicate(
    device: &ID3D11Device,
    output: &IDXGIOutput1,
) -> Result<IDXGIOutputDuplication> {
    let dxgi_device: IDXGIDevice = device.cast().context("Failed to get DXGI device")?;
    output
        .DuplicateOutput(&dxgi_device)
        .context("Failed to create output duplication")
}

struct Prepared {
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    source: String,
}

/// A D3D device created on one adapter cannot duplicate an output owned by another,
/// so each candidate builds its own device.
unsafe fn build_duplication() -> Result<Prepared> {
    if let Some(primary) = primary_output() {
        let (device, device_context) = create_device(Some(&primary.adapter))?;
        match duplicate(&device, &primary.output) {
            Ok(duplication) => {
                return Ok(Prepared {
                    device,
                    device_context,
                    duplication,
                    source: primary.name,
                })
            }
            Err(e) => warn!(
                "Duplicating {} failed ({e}); falling back to the default adapter",
                primary.name
            ),
        }
    } else {
        warn!("No DXGI output matches the primary monitor; falling back to the default adapter");
    }

    let (device, device_context) = create_device(None)?;
    let dxgi_device: IDXGIDevice = device.cast().context("Failed to get DXGI device")?;
    let adapter = dxgi_device.GetAdapter().context("Failed to get DXGI adapter")?;
    let output: IDXGIOutput1 = adapter
        .EnumOutputs(0)
        .context("No display outputs found")?
        .cast()
        .context("Failed to cast to IDXGIOutput1")?;
    let source = "default adapter output 0".to_string();
    let duplication = duplicate(&device, &output)?;
    Ok(Prepared {
        device,
        device_context,
        duplication,
        source,
    })
}

fn primary_monitor_rect() -> Result<RECT> {
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

        Ok(info.monitorInfo.rcMonitor)
    }
}

pub fn get_primary_monitor_size() -> Result<(u32, u32)> {
    let rect = primary_monitor_rect()?;
    Ok((
        (rect.right - rect.left) as u32,
        (rect.bottom - rect.top) as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_stop_at_the_nul() {
        let mut raw = vec![0u16; 32];
        for (slot, unit) in raw.iter_mut().zip("AMD Radeon R5 220".encode_utf16()) {
            *slot = unit;
        }
        assert_eq!(utf16_field(&raw), "AMD Radeon R5 220");
    }

    /// The selection has to land on the output whose desktop rectangle *is* the
    /// primary monitor's, not on whichever adapter DXGI happened to list first --
    /// listing order is what made a second GPU able to mirror a black desktop.
    #[test]
    fn the_output_chosen_is_one_of_the_ones_holding_the_desktop() {
        let want = primary_monitor_rect().expect("an interactive desktop");
        assert!(want.right > want.left && want.bottom > want.top);

        let mut holders: Vec<String> = Vec::new();
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("DXGI factory");
            for adapter_index in 0.. {
                let Ok(adapter) = factory.EnumAdapters(adapter_index) else {
                    break;
                };
                for output_index in 0.. {
                    let Ok(output) = adapter.EnumOutputs(output_index) else {
                        break;
                    };
                    let Ok(desc) = output.GetDesc() else {
                        continue;
                    };
                    if desc.AttachedToDesktop.as_bool() && desc.DesktopCoordinates == want {
                        holders.push(utf16_field(&desc.DeviceName));
                    }
                }
            }
        }
        assert!(
            !holders.is_empty(),
            "no DXGI output claims the primary monitor rect {:?}",
            want
        );

        let chosen = primary_output().expect("the primary output should resolve");
        assert!(
            holders.contains(&chosen.name),
            "chose {chose:?} but the desktop is on {holders:?}",
            chose = chosen.name
        );
    }
}
