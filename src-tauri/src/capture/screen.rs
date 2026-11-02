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
    DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};
#[cfg(test)]
use windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR;
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
    /// Desktop coordinates of the output's top-left corner: the pointer arrives
    /// in virtual-desktop coordinates, the frame starts here.
    origin: POINT,
    /// Desktop Duplication keeps the pointer out of the image; it is returned
    /// separately and has to be composited or the receiver never sees a cursor.
    pointer: Pointer,
    shape_buffer: Vec<u8>,
}

#[derive(Default)]
struct Pointer {
    shape: Option<PointerShape>,
    visible: bool,
    position: POINT,
}

struct PointerShape {
    kind: u32,
    width: u32,
    height: u32,
    pitch: u32,
    hotspot: POINT,
    pixels: Vec<u8>,
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
                origin: prepared.origin,
                pointer: Pointer::default(),
                shape_buffer: Vec::new(),
            })
        }
    }

    /// Reads the desktop into `data`, reusing its allocation. Returns `false`
    /// when no new frame was presented within `timeout_ms`; the caller keeps its
    /// previous pixels in that case.
    ///
    /// The caller owns the buffer because 1080p BGRA is 8.3 MB: allocating and
    /// freeing that per frame spent a measurable slice of the frame budget in
    /// the allocator and handed ffmpeg a different address every time.
    pub fn capture_frame_into(&mut self, timeout_ms: u32, data: &mut Vec<u8>) -> Result<bool> {
        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            let hr = self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource);

            if let Err(e) = hr {
                let code = e.code();
                if code == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(false);
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

            // The pointer belongs to the frame that is being held; the shape
            // buffer is only readable until ReleaseFrame.
            self.update_pointer(&frame_info);

            self.device_context
                .CopyResource(&self.staging_texture, &desktop_texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if let Err(e) = self.device_context.Map(
                &self.staging_texture,
                0,
                D3D11_MAP_READ,
                0,
                Some(&mut mapped),
            ) {
                let _ = self.duplication.ReleaseFrame();
                return Err(e).context("Failed to map staging texture");
            }

            let pitch = mapped.RowPitch as usize;
            let row_bytes = self.width as usize * 4;
            data.resize(row_bytes * self.height as usize, 0);

            let src = mapped.pData as *const u8;
            if pitch == row_bytes {
                std::ptr::copy_nonoverlapping(src, data.as_mut_ptr(), row_bytes * self.height as usize);
            } else {
                for row in 0..self.height as usize {
                    std::ptr::copy_nonoverlapping(
                        src.add(row * pitch),
                        data.as_mut_ptr().add(row * row_bytes),
                        row_bytes,
                    );
                }
            }

            self.device_context.Unmap(&self.staging_texture, 0);
            let _ = self.duplication.ReleaseFrame();

            self.draw_pointer(data);
            Ok(true)
        }
    }

    pub fn capture_frame(&mut self, timeout_ms: u32) -> Result<Option<CapturedFrame>> {
        let mut data = Vec::new();
        if !self.capture_frame_into(timeout_ms, &mut data)? {
            return Ok(None);
        }
        Ok(Some(CapturedFrame {
            width: self.width,
            height: self.height,
            data,
            pitch: self.width * 4,
        }))
    }

    unsafe fn update_pointer(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) {
        let position = info.PointerPosition;
        self.pointer.visible = position.Visible.as_bool();
        self.pointer.position = POINT {
            x: position.Position.x - self.origin.x,
            y: position.Position.y - self.origin.y,
        };
        // A zero-length buffer means the shape did not change; the position
        // above still did, and that is the case that matters for motion.
        if info.PointerShapeBufferSize == 0 {
            return;
        }

        let size = info.PointerShapeBufferSize as usize;
        self.shape_buffer.resize(size, 0);
        let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let mut required = 0u32;
        let mut read = self.duplication.GetFramePointerShape(
            size as u32,
            self.shape_buffer.as_mut_ptr().cast(),
            &mut required,
            &mut shape,
        );
        // A driver can report the required size only after refusing the first
        // call, so a short buffer is retried once at the size it named.
        if read.is_err() && required as usize > size {
            self.shape_buffer.resize(required as usize, 0);
            read = self.duplication.GetFramePointerShape(
                required,
                self.shape_buffer.as_mut_ptr().cast(),
                &mut required,
                &mut shape,
            );
        }
        if read.is_err() {
            // Keeping the previous shape is better than drawing a cursor at
            // whatever dimensions the failed call left behind.
            return;
        }

        self.pointer.shape = Some(PointerShape {
            kind: shape.Type,
            width: shape.Width,
            height: shape.Height,
            pitch: shape.Pitch,
            hotspot: shape.HotSpot,
            pixels: self.shape_buffer.clone(),
        });
    }

    fn draw_pointer(&self, data: &mut [u8]) {
        let Some(shape) = self.pointer.shape.as_ref() else {
            return;
        };
        if !self.pointer.visible {
            return;
        }
        composite_pointer(data, self.width, self.height, shape, self.pointer.position);
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

/// Blends the pointer onto a tightly packed BGRA frame.
///
/// Monochrome pointers are two masks applied as the GDI raster operation
/// `dest = (dest AND and) XOR xor`; colour pointers are straight-alpha BGRA;
/// masked colour pointers XOR where alpha is zero and replace everywhere else.
fn composite_pointer(
    data: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    shape: &PointerShape,
    position: POINT,
) {
    let monochrome = shape.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
    // For a monochrome pointer the reported height covers both masks, so the
    // image itself is half as tall.
    let image_height = if monochrome {
        shape.height / 2
    } else {
        shape.height
    };
    if image_height == 0 || shape.width == 0 || frame_width == 0 || frame_height == 0 {
        return;
    }

    let left = position.x - shape.hotspot.x;
    let top = position.y - shape.hotspot.y;
    let x0 = left.max(0);
    let y0 = top.max(0);
    let x1 = left.saturating_add(shape.width as i32).min(frame_width as i32);
    let y1 = top
        .saturating_add(image_height as i32)
        .min(frame_height as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }

    let pitch = shape.pitch as usize;
    for y in y0..y1 {
        let sy = (y - top) as usize;
        for x in x0..x1 {
            let sx = (x - left) as usize;
            let at = ((y as u32 * frame_width + x as u32) * 4) as usize;
            let Some(pixel) = data.get_mut(at..at + 4) else {
                continue;
            };
            if monochrome {
                let byte = sy * pitch + sx / 8;
                let bit = 7 - (sx % 8);
                // Row offsets are in whole mask rows, not in image rows.
                let and = shape.pixels.get(byte).copied().unwrap_or(0);
                let xor = shape
                    .pixels
                    .get((sy + image_height as usize) * pitch + sx / 8)
                    .copied()
                    .unwrap_or(0);
                match ((and >> bit) & 1, (xor >> bit) & 1) {
                    // AND=1 XOR=0: the screen shows through.
                    (1, 0) => {}
                    (1, 1) => {
                        pixel[0] = !pixel[0];
                        pixel[1] = !pixel[1];
                        pixel[2] = !pixel[2];
                    }
                    (0, 0) => {
                        pixel[0] = 0;
                        pixel[1] = 0;
                        pixel[2] = 0;
                    }
                    _ => {
                        pixel[0] = 255;
                        pixel[1] = 255;
                        pixel[2] = 255;
                    }
                }
                pixel[3] = 255;
                continue;
            }

            let src_at = sy * pitch + sx * 4;
            let Some(src) = shape.pixels.get(src_at..src_at + 4) else {
                continue;
            };
            let alpha = src[3] as u32;
            if shape.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
                if alpha == 0 {
                    pixel[0] ^= src[0];
                    pixel[1] ^= src[1];
                    pixel[2] ^= src[2];
                } else {
                    pixel[..3].copy_from_slice(&src[..3]);
                }
            } else if alpha > 0 {
                // DXGI hands back straight, not premultiplied, BGRA.
                for (channel, value) in pixel.iter_mut().take(3).enumerate() {
                    *value = ((src[channel] as u32 * alpha
                        + *value as u32 * (255 - alpha))
                        / 255) as u8;
                }
            } else {
                continue;
            }
            pixel[3] = 255;
        }
    }
}

struct PrimaryOutput {
    adapter: IDXGIAdapter,
    output: IDXGIOutput1,
    name: String,
    rect: RECT,
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
                        rect: desc.DesktopCoordinates,
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
    origin: POINT,
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
                    origin: POINT {
                        x: primary.rect.left,
                        y: primary.rect.top,
                    },
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
    let output = adapter
        .EnumOutputs(0)
        .context("No display outputs found")?;
    let rect = output.GetDesc().map(|desc| desc.DesktopCoordinates).ok();
    let output: IDXGIOutput1 = output.cast().context("Failed to cast to IDXGIOutput1")?;
    let source = "default adapter output 0".to_string();
    let duplication = duplicate(&device, &output)?;
    Ok(Prepared {
        device,
        device_context,
        duplication,
        source,
        origin: rect
            .map(|rect| POINT {
                x: rect.left,
                y: rect.top,
            })
            .unwrap_or(POINT { x: 0, y: 0 }),
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

    fn frame(width: u32, height: u32) -> Vec<u8> {
        let mut data = vec![0u8; (width * height * 4) as usize];
        for pixel in data.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[10, 20, 30, 255]);
        }
        data
    }

    fn pixel(data: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * width + x) * 4) as usize;
        data[at..at + 4].try_into().unwrap()
    }

    /// The monochrome masks are the GDI operation `dest = (dest AND and) XOR
    /// xor`: transparent, inverted, black and white in one pass. Getting the
    /// second mask's row offset wrong would paint the cursor's AND mask twice.
    #[test]
    fn monochrome_pointer_applies_both_masks_per_pixel() {
        let mut data = frame(8, 3);
        let shape = PointerShape {
            kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32,
            width: 8,
            height: 2,
            pitch: 1,
            hotspot: POINT { x: 0, y: 0 },
            // AND: 1010_0000 (transparent, transparent), XOR: 0100_0000.
            pixels: vec![0b1010_0000, 0b0100_0000],
        };
        composite_pointer(&mut data, 8, 3, &shape, POINT { x: 1, y: 1 });

        assert_eq!(pixel(&data, 8, 1, 1), [10, 20, 30, 255], "AND=1 XOR=0");
        assert_eq!(pixel(&data, 8, 2, 1), [255, 255, 255, 255], "AND=0 XOR=1");
        assert_eq!(pixel(&data, 8, 3, 1), [10, 20, 30, 255], "AND=1 XOR=0");
        assert_eq!(pixel(&data, 8, 4, 1), [0, 0, 0, 255], "AND=0 XOR=0");
        assert_eq!(pixel(&data, 8, 0, 0), [10, 20, 30, 255], "outside the cursor");
    }

    #[test]
    fn colour_pointer_blends_with_its_alpha() {
        let mut data = frame(4, 4);
        let shape = PointerShape {
            kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32,
            width: 1,
            height: 1,
            pitch: 4,
            hotspot: POINT { x: 0, y: 0 },
            // Red at 50% over (10, 20, 30) BGRA.
            pixels: vec![0, 0, 255, 128],
        };
        composite_pointer(&mut data, 4, 4, &shape, POINT { x: 2, y: 2 });

        let [b, g, r, a] = pixel(&data, 4, 2, 2);
        assert_eq!(a, 255);
        assert!((b as i32 - 5).abs() <= 1, "blue {b}");
        assert!((g as i32 - 10).abs() <= 1, "green {g}");
        assert!((r as i32 - 142).abs() <= 1, "red {r}");
    }

    #[test]
    fn masked_colour_pointer_xors_where_transparent() {
        let mut data = frame(4, 4);
        let shape = PointerShape {
            kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32,
            width: 1,
            height: 1,
            pitch: 4,
            hotspot: POINT { x: 0, y: 0 },
            pixels: vec![0x01, 0x02, 0x04, 0],
        };
        composite_pointer(&mut data, 4, 4, &shape, POINT { x: 1, y: 1 });

        assert_eq!(pixel(&data, 4, 1, 1), [11, 22, 26, 255]);
    }

    /// The pointer's hotspot is where the click lands, and a cursor at the top
    /// or left edge must be clipped rather than indexed out of the frame.
    #[test]
    fn hotspot_positions_and_edge_clipping_stay_inside_the_frame() {
        let mut data = frame(4, 4);
        let shape = PointerShape {
            kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32,
            width: 2,
            height: 2,
            pitch: 8,
            hotspot: POINT { x: 1, y: 1 },
            pixels: vec![255; 16],
        };
        composite_pointer(&mut data, 4, 4, &shape, POINT { x: 1, y: 1 });
        assert_eq!(pixel(&data, 4, 0, 0)[3], 255, "top-left quadrant is painted");

        let mut edge = frame(4, 4);
        composite_pointer(&mut edge, 4, 4, &shape, POINT { x: 0, y: 0 });
        let painted = edge
            .chunks_exact(4)
            .filter(|pixel| pixel[..3] != [10, 20, 30])
            .count();
        assert_eq!(painted, 1, "only the in-frame quarter of the cursor is drawn");
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
