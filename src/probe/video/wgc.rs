//! A window's video on Windows (SPEC §8.8, kind `screen`): Windows.Graphics.Capture (Windows 10
//! 1903+) gives each frame the compositor made of the window, stamped on the QPC clock
//! (`SystemRelativeTime`, the host clock), without the cursor and, on Windows 11, without the yellow
//! border. On adais (Windows 11) frames kept coming with the session locked: the compositor still
//! draws a window's redirection surface then.
//!
//! The GPU's video processor scales each frame to at most 1280 pixels on its long edge and converts it
//! to NV12; Media Foundation's sink writer encodes H.264 into MP4 on the GPU's encoder when it has one
//! (`MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` with strobe's Direct3D device), else in software. At most
//! 60 frames a second: a frame within 1/60 s of the last one written is held and written 1/60 s
//! after it (so up to that late), unless a newer one replaces it.
//!
//! Direct3D 11 and Media Foundation are loaded when a video starts (`LoadLibraryW`): strobe does not
//! import them, so no other command maps them.

use super::{Kind, Video};
use crate::model::{Error, Result};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows_core::{GUID, HRESULT, IInspectable, Interface, PCWSTR};

/// The least time between two frames written.
const INTERVAL_NS: u64 = 1_000_000_000 / 60;

type Raw = unsafe extern "system" fn() -> isize;
type CreateDevice = unsafe extern "system" fn(*mut c_void, i32, *mut c_void, u32, *const i32, u32, u32, *mut *mut c_void, *mut i32, *mut *mut c_void) -> HRESULT;
type WrapDevice = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
type Startup = unsafe extern "system" fn(u32, u32) -> HRESULT;
type Shutdown = unsafe extern "system" fn() -> HRESULT;
type Out = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type OutSized = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
type SizedOut = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
type Manager = unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> HRESULT;
type SinkWriter = unsafe extern "system" fn(PCWSTR, *mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;

/// The functions strobe calls in d3d11.dll, mfplat.dll and mfreadwrite.dll.
struct Lib {
    create_device: CreateDevice,
    wrap_device: WrapDevice,
    startup: Startup,
    shutdown: Shutdown,
    attributes: OutSized,
    media_type: Out,
    buffer: SizedOut,
    sample: Out,
    manager: Manager,
    sink_writer: SinkWriter,
}

fn proc(dll: &str, name: &str) -> Result<Raw> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    let wide: Vec<u16> = dll.encode_utf16().chain([0]).collect();
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        return Err(super::refused(format!("{dll} is not on this Windows"), "record on Windows 10 1903 or later with the Media Feature Pack (Windows N editions lack it)"));
    }
    let cname = std::ffi::CString::new(name).unwrap_or_default();
    unsafe { GetProcAddress(module, cname.as_ptr() as *const u8) }.ok_or_else(|| super::refused(format!("{dll} has no {name}"), "update Windows"))
}

impl Lib {
    fn load() -> Result<Lib> {
        // SAFETY: each pointer is the named export, whose signature the type spells out.
        unsafe {
            Ok(Lib {
                create_device: std::mem::transmute::<Raw, CreateDevice>(proc("d3d11.dll", "D3D11CreateDevice")?),
                wrap_device: std::mem::transmute::<Raw, WrapDevice>(proc("d3d11.dll", "CreateDirect3D11DeviceFromDXGIDevice")?),
                startup: std::mem::transmute::<Raw, Startup>(proc("mfplat.dll", "MFStartup")?),
                shutdown: std::mem::transmute::<Raw, Shutdown>(proc("mfplat.dll", "MFShutdown")?),
                attributes: std::mem::transmute::<Raw, OutSized>(proc("mfplat.dll", "MFCreateAttributes")?),
                media_type: std::mem::transmute::<Raw, Out>(proc("mfplat.dll", "MFCreateMediaType")?),
                buffer: std::mem::transmute::<Raw, SizedOut>(proc("mfplat.dll", "MFCreateMemoryBuffer")?),
                sample: std::mem::transmute::<Raw, Out>(proc("mfplat.dll", "MFCreateSample")?),
                manager: std::mem::transmute::<Raw, Manager>(proc("mfplat.dll", "MFCreateDXGIDeviceManager")?),
                sink_writer: std::mem::transmute::<Raw, SinkWriter>(proc("mfreadwrite.dll", "MFCreateSinkWriterFromURL")?),
            })
        }
    }

    fn attributes(&self, n: u32) -> windows_core::Result<IMFAttributes> {
        let mut p = std::ptr::null_mut();
        unsafe { (self.attributes)(&mut p, n).ok()? };
        Ok(unsafe { IMFAttributes::from_raw(p) })
    }

    fn media_type(&self) -> windows_core::Result<IMFMediaType> {
        let mut p = std::ptr::null_mut();
        unsafe { (self.media_type)(&mut p).ok()? };
        Ok(unsafe { IMFMediaType::from_raw(p) })
    }
}

/// What the frame pool's handler tells the encoder thread.
#[derive(Default)]
struct Signal {
    arrived: u64,
    stop: bool,
    end_ns: u64,
}

type Shared = Arc<(Mutex<Signal>, Condvar)>;

pub struct Capture {
    shared: Shared,
    thread: Option<std::thread::JoinHandle<Result<Video>>>,
}

impl Capture {
    pub fn start(window: &crate::host::TopWindow, base: &Path) -> Result<Capture> {
        let shared: Shared = Arc::default();
        let (tx, rx) = std::sync::mpsc::channel::<Result<()>>();
        let (hwnd, s, base) = (window.hwnd, shared.clone(), base.to_path_buf());
        let thread = std::thread::Builder::new()
            .name("strobe-video".into())
            .spawn(move || {
                let r = run(hwnd, &s, &base, &tx);
                if let Err(e) = &r {
                    let _ = tx.send(Err(e.clone()));
                }
                r
            })
            .map_err(|e| Error::msg(format!("start the video thread: {e}")))?;
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(())) => Ok(Capture { shared, thread: Some(thread) }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(Error::msg("Windows.Graphics.Capture did not start within 20 s")),
        }
    }

    pub fn stop(mut self) -> Result<Video> {
        {
            let mut s = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
            s.stop = true;
            s.end_ns = crate::host::now_ns();
        }
        self.shared.1.notify_all();
        let t = self.thread.take().ok_or_else(|| Error::msg("the video stopped twice"))?;
        crate::probe::join_within(t, Duration::from_secs(60)).ok_or_else(|| Error::msg("the video's encoder did not finish within 60 s"))?
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.0.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.shared.1.notify_all();
    }
}

fn win(e: windows_core::Error, what: &str) -> Error {
    Error::msg(format!("{what}: {}", e.message()))
}

/// The GPU's video processor for one input size: the captured frame (copied into `input`, a texture
/// strobe owns) scaled into `output` (NV12, the video's size), letterboxed.
struct Processor {
    vp: ID3D11VideoProcessor,
    input: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
    size: (u32, u32),
}

fn texture(device: &ID3D11Device, w: u32, h: u32, format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT, bind: u32, staging: bool) -> windows_core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: if staging { D3D11_USAGE_STAGING } else { D3D11_USAGE_DEFAULT },
        BindFlags: bind,
        CPUAccessFlags: if staging { D3D11_CPU_ACCESS_READ.0 as u32 } else { 0 },
        MiscFlags: 0,
    };
    let mut t = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut t))? };
    t.ok_or_else(|| windows_core::Error::from_hresult(HRESULT(-2147467259)))
}

impl Processor {
    fn new(device: &ID3D11Device, vdev: &ID3D11VideoDevice, vctx: &ID3D11VideoContext, output: &ID3D11Texture2D, size: (u32, u32), out: (u32, u32)) -> windows_core::Result<Processor> {
        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: size.0,
            InputHeight: size.1,
            OutputFrameRate: rate,
            OutputWidth: out.0,
            OutputHeight: out.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        unsafe {
            let e = vdev.CreateVideoProcessorEnumerator(&desc)?;
            let vp = vdev.CreateVideoProcessor(&e, 0)?;
            let input = texture(device, size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM, (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32, false)?;
            let iv = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut input_view = None;
            vdev.CreateVideoProcessorInputView(&input, &e, &iv, Some(&mut input_view))?;
            let ov = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut output_view = None;
            vdev.CreateVideoProcessorOutputView(output, &e, &ov, Some(&mut output_view))?;
            // Full-range RGB in, BT.709 studio-range YCbCr out (what the MP4 says it holds).
            vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
            vctx.VideoProcessorSetOutputColorSpace(&vp, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) });
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            // The window's frame inside the video, its aspect kept.
            let scale = (out.0 as f64 / size.0 as f64).min(out.1 as f64 / size.1 as f64);
            let (w, h) = ((size.0 as f64 * scale).round() as i32, (size.1 as f64 * scale).round() as i32);
            let dest = RECT { left: 0, top: 0, right: w.min(out.0 as i32), bottom: h.min(out.1 as i32) };
            vctx.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&RECT { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 }));
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&dest));
            vctx.VideoProcessorSetOutputTargetRect(&vp, true, Some(&RECT { left: 0, top: 0, right: out.0 as i32, bottom: out.1 as i32 }));
            Ok(Processor {
                vp,
                input,
                input_view: input_view.ok_or_else(|| windows_core::Error::from_hresult(HRESULT(-2147467259)))?,
                output_view: output_view.ok_or_else(|| windows_core::Error::from_hresult(HRESULT(-2147467259)))?,
                size,
            })
        }
    }

    fn blit(&self, ctx: &ID3D11DeviceContext, vctx: &ID3D11VideoContext, frame: &ID3D11Texture2D) -> windows_core::Result<()> {
        unsafe {
            ctx.CopyResource(&self.input, frame);
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM { Enable: true.into(), pInputSurface: std::mem::ManuallyDrop::new(Some(self.input_view.clone())), ..Default::default() };
            let r = vctx.VideoProcessorBlt(&self.vp, &self.output_view, 0, std::slice::from_ref(&stream));
            std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
            r
        }
    }
}

/// The sink writer and its one H.264 stream.
struct Writer {
    writer: IMFSinkWriter,
    stream: u32,
    hardware: bool,
}

fn writer(lib: &Lib, path: &Path, size: (u32, u32), manager: Option<&IMFDXGIDeviceManager>) -> windows_core::Result<Writer> {
    unsafe {
        let attrs = lib.attributes(4)?;
        attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        if let Some(m) = manager {
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, m)?;
        }
        let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain([0]).collect();
        let mut p = std::ptr::null_mut();
        (lib.sink_writer)(PCWSTR(wide.as_ptr()), std::ptr::null_mut(), attrs.as_raw(), &mut p).ok()?;
        let writer = IMFSinkWriter::from_raw(p);
        let frame = ((size.0 as u64) << 32) | size.1 as u64;
        let common = |t: &IMFMediaType| -> windows_core::Result<()> {
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, frame)?;
            t.SetUINT64(&MF_MT_FRAME_RATE, (60u64 << 32) | 1)?;
            t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
        };
        let out = lib.media_type()?;
        common(&out)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        // About 4 bits a pixel per second at 1280 × 800: text stays sharp when it scrolls.
        out.SetUINT32(&MF_MT_AVG_BITRATE, (size.0 * size.1 * 4).max(500_000))?;
        let stream = writer.AddStream(&out)?;
        let input = lib.media_type()?;
        common(&input)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        writer.SetInputMediaType(stream, &input, None::<&IMFAttributes>)?;
        writer.BeginWriting()?;
        let hardware = manager.is_some() && encoder_is_hardware(&writer, stream);
        Ok(Writer { writer, stream, hardware })
    }
}

/// Whether the stream's encoder transform is a hardware one (it has a hardware URL).
fn encoder_is_hardware(writer: &IMFSinkWriter, stream: u32) -> bool {
    let Ok(ex) = writer.cast::<IMFSinkWriterEx>() else { return false };
    (0..4).any(|i| {
        let mut category = GUID::zeroed();
        let mut transform: Option<IMFTransform> = None;
        unsafe { ex.GetTransformForStream(stream, i, Some(&mut category), &mut transform) }.is_ok()
            && category == MFT_CATEGORY_VIDEO_ENCODER
            && transform.and_then(|t| unsafe { t.GetAttributes() }.ok()).is_some_and(|a| unsafe { a.GetStringLength(&MFT_ENUM_HARDWARE_URL_Attribute) }.is_ok())
    })
}

fn run(hwnd: isize, shared: &Shared, base: &Path, started: &std::sync::mpsc::Sender<Result<()>>) -> Result<Video> {
    let cpu0 = super::ThreadCpu::start();
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        return Err(super::refused("Windows.Graphics.Capture is not available here", "record on Windows 10 1903 or later"));
    }
    let lib = Lib::load()?;
    unsafe { (lib.startup)(MF_VERSION, MFSTARTUP_FULL).ok().map_err(|e| win(e, "Media Foundation"))? };
    let r = capture(&lib, hwnd, shared, base, started, cpu0);
    unsafe {
        let _ = (lib.shutdown)();
    }
    r
}

fn capture(lib: &Lib, hwnd: isize, shared: &Shared, base: &Path, started: &std::sync::mpsc::Sender<Result<()>>, cpu0: super::ThreadCpu) -> Result<Video> {
    // The GPU: a hardware device with BGRA and video support, guarded for Media Foundation's threads.
    let (device, ctx) = unsafe {
        let (mut d, mut c) = (std::ptr::null_mut(), std::ptr::null_mut());
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 | D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0;
        (lib.create_device)(
            std::ptr::null_mut(),
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE.0,
            std::ptr::null_mut(),
            flags,
            std::ptr::null(),
            0,
            D3D11_SDK_VERSION,
            &mut d,
            std::ptr::null_mut(),
            &mut c,
        )
        .ok()
        .map_err(|e| super::refused(format!("no Direct3D 11 device with video support: {}", e.message()), "record on a machine with a GPU driver"))?;
        (ID3D11Device::from_raw(d), ID3D11DeviceContext::from_raw(c))
    };
    let e = |what: &'static str| move |x: windows_core::Error| win(x, what);
    if let Ok(m) = ctx.cast::<ID3D11Multithread>() {
        unsafe {
            let _ = m.SetMultithreadProtected(true);
        }
    }
    let vdev: ID3D11VideoDevice = device.cast().map_err(e("the GPU's video processor"))?;
    let vctx: ID3D11VideoContext = ctx.cast().map_err(e("the GPU's video processor"))?;
    let dxgi: IDXGIDevice = device.cast().map_err(e("DXGI"))?;
    let d3d: IDirect3DDevice = unsafe {
        let mut p = std::ptr::null_mut();
        (lib.wrap_device)(dxgi.as_raw(), &mut p).ok().map_err(e("the WinRT Direct3D device"))?;
        IInspectable::from_raw(p).cast().map_err(e("the WinRT Direct3D device"))?
    };
    let interop = windows_core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(e("Windows.Graphics.Capture"))?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(HWND(hwnd as *mut c_void)) }
        .map_err(|x| super::refused(format!("Windows.Graphics.Capture cannot capture the window: {}", x.message()), "a window that is shown (not minimized), of a process in this session"))?;
    let first = item.Size().map_err(e("the window's size"))?;
    let mut size = (first.Width.max(2) as u32, first.Height.max(2) as u32);
    let out = super::fit(size.0, size.1);
    let output = texture(&device, out.0, out.1, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0 as u32, false).map_err(e("the video's frame"))?;
    let staging = texture(&device, out.0, out.1, DXGI_FORMAT_NV12, 0, true).map_err(e("the video's frame"))?;
    let mut processor = Processor::new(&device, &vdev, &vctx, &output, size, out).map_err(e("the GPU's video processor"))?;
    let path: PathBuf = base.with_extension("mp4");
    // The GPU's encoder through strobe's device, else Media Foundation's software one.
    let manager = unsafe {
        let (mut token, mut p) = (0u32, std::ptr::null_mut());
        (lib.manager)(&mut token, &mut p).ok().ok().map(|_| IMFDXGIDeviceManager::from_raw(p)).filter(|m| m.ResetDevice(&device, token).is_ok())
    };
    let w = match manager.as_ref().map(|m| writer(lib, &path, out, Some(m))) {
        Some(Ok(w)) => w,
        _ => writer(lib, &path, out, None).map_err(e("Media Foundation's H.264 encoder"))?,
    };
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, first).map_err(e("the capture's frame pool"))?;
    let signal = shared.clone();
    pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_, _| {
        signal.0.lock().unwrap_or_else(|e| e.into_inner()).arrived += 1;
        signal.1.notify_all();
        Ok(())
    }))
    .map_err(e("the capture's frame events"))?;
    let session = pool.CreateCaptureSession(&item).map_err(e("the capture session"))?;
    let _ = session.SetIsCursorCaptureEnabled(false);
    // Windows 11; Windows 10 always draws the border.
    let _ = session.SetIsBorderRequired(false);
    // Media time 0: the capture's start. The first frame (the window as it was) stands from there.
    let t0 = crate::host::now_ns();
    session.StartCapture().map_err(e("the capture session"))?;
    let _ = started.send(Ok(()));
    // The capture's own CPU from here: setting up the GPU and the encoder is once per video.
    let setup_ms = cpu0.ms();
    let cpu0 = super::ThreadCpu::start();

    let mut last: Option<u64> = None;
    // The newest frame in `output` not written yet, with its time.
    let mut pending: Option<u64> = None;
    let (mut frames, mut source_frames) = (0u64, 0u64);
    let w = std::cell::RefCell::new(Some(w));
    let write = |at: u64, frames: &mut u64| -> Result<()> {
        let at = if *frames == 0 { t0 } else { at.max(t0) };
        unsafe {
            ctx.CopyResource(&staging, &output);
            // Waits for the GPU's copy by sleeping, not in the driver (which may spin a core).
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            let mut tries = 0;
            loop {
                match ctx.Map(&staging, 0, D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32, Some(&mut m)) {
                    Ok(()) => break,
                    Err(x) if x.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING && tries < 1000 => {
                        tries += 1;
                        std::thread::sleep(Duration::from_micros(500));
                    }
                    Err(x) => return Err(win(x, "the video's frame")),
                }
            }
            let (wpx, h) = (out.0 as usize, out.1 as usize);
            let len = wpx * h * 3 / 2;
            let filled = (|| -> windows_core::Result<IMFSample> {
                let mut p = std::ptr::null_mut();
                (lib.buffer)(len as u32, &mut p).ok()?;
                let buffer = IMFMediaBuffer::from_raw(p);
                let mut dst = std::ptr::null_mut();
                buffer.Lock(&mut dst, None, None)?;
                let src = m.pData as *const u8;
                let pitch = m.RowPitch as usize;
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(src.add(y * pitch), dst.add(y * wpx), wpx);
                }
                // NV12's chroma plane follows the luma plane's rows, at the same pitch.
                for y in 0..h / 2 {
                    std::ptr::copy_nonoverlapping(src.add(pitch * h + y * pitch), dst.add(wpx * h + y * wpx), wpx);
                }
                buffer.Unlock()?;
                buffer.SetCurrentLength(len as u32)?;
                let mut s = std::ptr::null_mut();
                (lib.sample)(&mut s).ok()?;
                let sample = IMFSample::from_raw(s);
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(((at - t0) / 100) as i64)?;
                sample.SetSampleDuration((INTERVAL_NS / 100) as i64)?;
                Ok(sample)
            })();
            ctx.Unmap(&staging, 0);
            let sample = filled.map_err(e("the video's frame"))?;
            let sent = w.borrow().as_ref().map(|w| (w.writer.WriteSample(w.stream, &sample), w.hardware));
            match sent {
                Some((Ok(()), _)) => {}
                // A GPU encoder that took the stream but fails its first frame: Media Foundation's
                // software one from the start, over the same file (the first writer let go of it).
                Some((Err(_), true)) if *frames == 0 => {
                    drop(w.borrow_mut().take());
                    let soft = writer(lib, &path, out, None).map_err(e("Media Foundation's H.264 encoder"))?;
                    soft.writer.WriteSample(soft.stream, &sample).map_err(e("Media Foundation's H.264 encoder"))?;
                    *w.borrow_mut() = Some(soft);
                }
                Some((Err(x), _)) => return Err(win(x, "Media Foundation's H.264 encoder")),
                None => return Err(Error::msg("Media Foundation's H.264 encoder is gone")),
            }
        }
        *frames += 1;
        Ok(())
    };
    let end_ns = loop {
        let (stop, end_ns) = {
            let mut s = shared.0.lock().unwrap_or_else(|e| e.into_inner());
            let due = pending.and(last).map(|l| l + INTERVAL_NS);
            let now = crate::host::now_ns();
            if s.arrived == 0 && !s.stop && due.is_none_or(|d| d > now) {
                let wait = due.map_or(Duration::from_millis(250), |d| Duration::from_nanos(d.saturating_sub(now)));
                s = shared.1.wait_timeout(s, wait).unwrap_or_else(|e| e.into_inner()).0;
            }
            s.arrived = 0;
            (s.stop, s.end_ns)
        };
        while let Ok(frame) = pool.TryGetNextFrame() {
            source_frames += 1;
            let at = frame.SystemRelativeTime().map(|t| t.Duration as u64 * 100).unwrap_or_else(|_| crate::host::now_ns());
            let content = frame.ContentSize().unwrap_or(first);
            let got = (content.Width.max(2) as u32, content.Height.max(2) as u32);
            let tex: Option<ID3D11Texture2D> = frame.Surface().ok().and_then(|s| s.cast::<IDirect3DDxgiInterfaceAccess>().ok()).and_then(|a| unsafe { a.GetInterface() }.ok());
            if got != size {
                // A resized window: the processor and then the pool follow it (the pool only once the
                // processor for the new size exists), the video keeps its size.
                if let Ok(p) = Processor::new(&device, &vdev, &vctx, &output, got, out)
                    && pool.Recreate(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, SizeInt32 { Width: got.0 as i32, Height: got.1 as i32 }).is_ok()
                {
                    processor = p;
                    size = got;
                }
            }
            let fits = tex.as_ref().is_some_and(|t| {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                unsafe { t.GetDesc(&mut d) };
                (d.Width, d.Height) == processor.size
            });
            if let (Some(t), true) = (&tex, fits)
                && processor.blit(&ctx, &vctx, t).is_ok()
            {
                pending = Some(at);
            }
            let _ = frame.Close();
        }
        let now = crate::host::now_ns();
        if let Some(at) = pending {
            match last.map(|l| l + INTERVAL_NS) {
                None => {
                    write(at, &mut frames)?;
                    (last, pending) = (Some(at), None);
                }
                Some(due) if at >= due => {
                    write(at, &mut frames)?;
                    (last, pending) = (Some(at), None);
                }
                Some(due) if now >= due || stop => {
                    write(due, &mut frames)?;
                    (last, pending) = (Some(due), None);
                }
                Some(_) => {}
            }
        }
        if stop {
            break if end_ns > 0 { end_ns } else { now };
        }
    };
    let _ = session.Close();
    let _ = pool.Close();
    if frames == 0 {
        return Err(super::refused("Windows.Graphics.Capture delivered no frame of the window", "make sure the window is shown (not minimized) while the video records"));
    }
    // The last frame again at the end, so the video lasts until the recording stopped.
    let t1 = end_ns.max(last.unwrap_or(t0));
    if last.is_some_and(|l| t1 > l + INTERVAL_NS) {
        let mut n = frames;
        write(t1, &mut n)?;
    }
    let w = w.into_inner().ok_or_else(|| Error::msg("Media Foundation's H.264 encoder is gone"))?;
    unsafe { w.writer.Finalize() }.map_err(e("Media Foundation's H.264 encoder"))?;
    Ok(Video {
        kind: Kind::Screen,
        path,
        ext: "mp4",
        width: out.0,
        height: out.1,
        t0_ns: t0,
        t1_ns: t1,
        frames,
        source_frames,
        fps: None,
        source: "Windows.Graphics.Capture".into(),
        encoder: format!("Media Foundation H.264 ({})", if w.hardware { "the GPU's encoder" } else { "software" }),
        cpu_ms: cpu0.ms(),
        setup_cpu_ms: setup_ms,
        encoder_cpu_ms: None,
        precision_ms: INTERVAL_NS as f64 / 1e6,
    })
}
