//! A window's video on macOS (SPEC §8.8, kind `screen`): a ScreenCaptureKit stream of the window
//! alone (`initWithDesktopIndependentWindow:`, which follows it when it moves), 4:2:0 frames at the
//! window's pixel size (at most 1280 on the long edge), at most 60 a second, each stamped with the
//! time it was displayed (`SCStreamFrameInfoDisplayTime`, Mach absolute time: the host clock).
//! AVAssetWriter encodes H.264 into MP4 on the media engine (VideoToolbox, in its own process) with
//! each frame at its display time.
//!
//! AVFoundation is loaded when a video starts (`dlopen`): strobe does not link it, so no other command
//! maps it. Written against the Objective-C runtime through `present::sck`'s bridge.

use super::{Kind, Video};
use crate::host::macos::cf_array;
use crate::model::{Error, Result};
use crate::probe::present::sck::{
    self, CMSampleBufferGetImageBuffer, CMSampleBufferGetSampleAttachmentsArray, CMTime, Handler, Id, SCStreamFrameInfoDisplayTime, SCStreamFrameInfoStatus, Sel, class, class_addIvar,
    class_addMethod, class_addProtocol, class_getInstanceVariable, description, dispatch_queue_create, dispatch_release, dispatch_sync_f, objc_allocateClassPair, objc_autoreleasePoolPop,
    objc_autoreleasePoolPush, objc_getProtocol, objc_registerClassPair, objc_release, objc_retain, object_getIvar, object_setIvar, send, with_handler,
};
use std::ffi::{CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const YUV420V: u32 = u32::from_be_bytes(*b"420v");

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

/// AVFoundation, loaded once.
fn avfoundation() -> Result<*mut c_void> {
    static HANDLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let h = *HANDLE.get_or_init(|| unsafe { dlopen(c"/System/Library/Frameworks/AVFoundation.framework/AVFoundation".as_ptr(), 1) } as usize);
    if h == 0 { Err(super::refused("AVFoundation did not load", "record on macOS 13 or later")) } else { Ok(h as *mut c_void) }
}

/// One of AVFoundation's `NSString *const` keys.
fn key(name: &str) -> Result<Id> {
    let c = CString::new(name).unwrap_or_default();
    let p = unsafe { dlsym(avfoundation()?, c.as_ptr()) } as *const Id;
    if p.is_null() { Err(Error::msg(format!("AVFoundation has no {name}"))) } else { Ok(unsafe { *p }) }
}

fn string(s: &str) -> Id {
    let c = CString::new(s).unwrap_or_default();
    send!(Id; class("NSString"), "stringWithUTF8String:", *const c_char = c.as_ptr())
}

fn number(n: isize) -> Id {
    send!(Id; class("NSNumber"), "numberWithInteger:", isize = n)
}

fn dict(keys: &[Id], values: &[Id]) -> Id {
    send!(Id; class("NSDictionary"), "dictionaryWithObjects:forKeys:count:", *const Id = values.as_ptr(), *const Id = keys.as_ptr(), usize = keys.len())
}

fn ns(t: i64) -> CMTime {
    CMTime { value: t, timescale: 1_000_000_000, flags: 1, epoch: 0 }
}

/// What the stream's output object writes into.
struct Sink {
    writer: Id,
    input: Id,
    adaptor: Id,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The capture's start (media time 0), and whether the writer's session began.
    t0: Option<u64>,
    started: bool,
    last: u64,
    frames: u64,
    source: u64,
    cpu_ns: u64,
}

// SAFETY: the writer's objects are used under the state's lock, on the stream's serial queue, and
// after it drained.
unsafe impl Send for Sink {}
unsafe impl Sync for Sink {}

/// `stream:didOutputSampleBuffer:ofType:` of strobe's video output class.
extern "C" fn on_frame(this: Id, _cmd: Sel, _stream: Id, sbuf: *mut c_void, kind: isize) {
    let cpu0 = crate::host::thread_cycles();
    if kind != 0 || sbuf.is_null() {
        return;
    }
    let sink = unsafe { object_getIvar(this, class_getInstanceVariable(output_class(), c"sink".as_ptr())) } as *const Sink;
    if sink.is_null() {
        return;
    }
    let sink = unsafe { &*sink };
    let Some(&info) = cf_array(unsafe { CMSampleBufferGetSampleAttachmentsArray(sbuf, 0) }).first() else { return };
    let status = crate::host::macos::cf_i64(unsafe { sck::cf_dict_value(info, SCStreamFrameInfoStatus) });
    let shown = crate::host::macos::cf_i64(unsafe { sck::cf_dict_value(info, SCStreamFrameInfoDisplayTime) });
    let image = unsafe { CMSampleBufferGetImageBuffer(sbuf) };
    let mut st = sink.state.lock().unwrap_or_else(|e| e.into_inner());
    // SCFrameStatusComplete: a frame with new content (an idle window sends none).
    if let (Some(0), Some(t), false) = (status, shown, image.is_null()) {
        st.source += 1;
        let at = crate::host::macos::mach_ns(t as u64);
        // Media time 0 is the capture's start: the first frame (the window as it was) stands from there.
        let t0 = st.t0.unwrap_or(at);
        let first = st.frames == 0;
        if first && !st.started {
            send!((); sink.writer, "startSessionAtSourceTime:", CMTime = ns(0));
            st.started = true;
        }
        let pts = if first { 0 } else { at.saturating_sub(t0) as i64 };
        let ready = send!(i8; sink.input, "isReadyForMoreMediaData") != 0;
        if (first || at > st.last) && ready && send!(i8; sink.adaptor, "appendPixelBuffer:withPresentationTime:", *mut c_void = image, CMTime = ns(pts)) != 0 {
            st.frames += 1;
            st.last = at.max(t0);
        }
    }
    st.cpu_ns += crate::host::thread_cycles().saturating_sub(cpu0);
}

/// strobe's video SCStreamOutput class, made once.
fn output_class() -> Id {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CLASS.get_or_init(|| unsafe {
        let existing = class("StrobeVideoOutput");
        if !existing.is_null() {
            return existing as usize;
        }
        let cls = objc_allocateClassPair(class("NSObject"), c"StrobeVideoOutput".as_ptr(), 0);
        class_addIvar(cls, c"sink".as_ptr(), std::mem::size_of::<usize>(), 3, c"^v".as_ptr());
        class_addMethod(cls, sck::sel("stream:didOutputSampleBuffer:ofType:"), on_frame as *const c_void, c"v@:@^vq".as_ptr());
        let proto = objc_getProtocol(c"SCStreamOutput".as_ptr());
        if !proto.is_null() {
            class_addProtocol(cls, proto);
        }
        objc_registerClassPair(cls);
        cls as usize
    }) as Id
}

pub struct Capture {
    stream: Id,
    output: Id,
    queue: *mut c_void,
    path: PathBuf,
    size: (u32, u32),
    cpu: super::ThreadCpu,
    setup_ms: Option<f64>,
}

// SAFETY: the stream and output are used from the thread that owns the capture only.
unsafe impl Send for Capture {}

/// The error an `NSError **` out-argument holds, released.
fn taken(err: Id) -> String {
    let why = description(err);
    if !err.is_null() {
        unsafe { objc_release(err) };
    }
    why
}

impl Capture {
    pub fn start(window: &crate::host::TopWindow, base: &Path) -> Result<Capture> {
        let cpu = super::ThreadCpu::start();
        if unsafe { sck::CGPreflightScreenCaptureAccess() } == 0 {
            let who = crate::host::macos::screen_recording_grantee();
            return Err(super::refused(format!("a window's video comes from ScreenCaptureKit, which needs the Screen Recording permission for {who}"), crate::host::macos::screen_recording_pane()));
        }
        avfoundation()?;
        let pool = unsafe { objc_autoreleasePoolPush() };
        let r = Capture::open(window, base, cpu);
        unsafe { objc_autoreleasePoolPop(pool) };
        r
    }

    fn open(window: &crate::host::TopWindow, base: &Path, cpu: super::ThreadCpu) -> Result<Capture> {
        let content = sck::with_completion(
            Duration::from_secs(5),
            false,
            |b| send!((); class("SCShareableContent"), "getShareableContentExcludingDesktopWindows:onScreenWindowsOnly:completionHandler:", i8 = 1, i8 = 1, *mut sck::Block = b),
        );
        let Some((content, err)) = content else { return Err(Error::msg("ScreenCaptureKit did not list the windows in 5 s")) };
        if content.is_null() {
            return Err(super::refused(format!("ScreenCaptureKit: {}", taken(err)), "check the Screen Recording permission (strobe status)"));
        }
        let windows: Id = send!(Id; content, "windows");
        let found = (0..send!(usize; windows, "count")).map(|k| send!(Id; windows, "objectAtIndex:", usize = k)).find(|w| send!(u32; *w, "windowID") == window.hwnd as u32);
        let Some(w) = found else {
            unsafe { objc_release(content) };
            return Err(super::refused("ScreenCaptureKit does not list the target's window on screen", "make sure the window is shown (not minimized, on this Space) while the video records"));
        };
        let frame = sck::rect_of(w, "frame");
        let scale = window.scale().max(1.0);
        let out = super::fit((frame.size.width * scale).round() as u32, (frame.size.height * scale).round() as u32);
        let filter: Id = send!(Id; class("SCContentFilter"), "alloc");
        let filter: Id = send!(Id; filter, "initWithDesktopIndependentWindow:", Id = w);
        unsafe { objc_release(content) };
        let config: Id = send!(Id; class("SCStreamConfiguration"), "alloc");
        let config: Id = send!(Id; config, "init");
        send!((); config, "setWidth:", usize = out.0 as usize);
        send!((); config, "setHeight:", usize = out.1 as usize);
        send!((); config, "setPixelFormat:", u32 = YUV420V);
        send!((); config, "setShowsCursor:", i8 = 0);
        send!((); config, "setQueueDepth:", isize = 6);
        send!((); config, "setMinimumFrameInterval:", CMTime = CMTime { value: 1, timescale: 60, flags: 1, epoch: 0 });
        // macOS 14: the window scaled into the frame with its aspect kept.
        for setter in ["setScalesToFit:", "setPreservesAspectRatio:"] {
            if send!(i8; config, "respondsToSelector:", Sel = sck::sel(setter)) != 0 {
                send!((); config, setter, i8 = 1);
            }
        }
        // The writer: H.264 in MP4 at about 4 bits a pixel per second, the index at the front.
        let path = base.with_extension("mp4");
        let url: Id = send!(Id; class("NSURL"), "fileURLWithPath:", Id = string(&path.to_string_lossy()));
        let mut err: Id = std::ptr::null_mut();
        let writer: Id = send!(Id; class("AVAssetWriter"), "assetWriterWithURL:fileType:error:", Id = url, Id = key("AVFileTypeMPEG4")?, *mut Id = &mut err);
        if writer.is_null() {
            unsafe {
                objc_release(filter);
                objc_release(config);
            }
            return Err(Error::msg(format!("AVAssetWriter: {}", taken(err))));
        }
        send!((); writer, "setShouldOptimizeForNetworkUse:", i8 = 1);
        let props = dict(
            &[key("AVVideoAverageBitRateKey")?, key("AVVideoExpectedSourceFrameRateKey")?, key("AVVideoMaxKeyFrameIntervalKey")?],
            &[number((out.0 * out.1 * 4).max(500_000) as isize), number(60), number(120)],
        );
        let settings = dict(
            &[key("AVVideoCodecKey")?, key("AVVideoWidthKey")?, key("AVVideoHeightKey")?, key("AVVideoCompressionPropertiesKey")?],
            &[key("AVVideoCodecTypeH264")?, number(out.0 as isize), number(out.1 as isize), props],
        );
        let input: Id = send!(Id; class("AVAssetWriterInput"), "assetWriterInputWithMediaType:outputSettings:", Id = key("AVMediaTypeVideo")?, Id = settings);
        send!((); input, "setExpectsMediaDataInRealTime:", i8 = 1);
        let added = send!(i8; writer, "canAddInput:", Id = input) != 0;
        if added {
            send!((); writer, "addInput:", Id = input);
        }
        let adaptor: Id =
            send!(Id; class("AVAssetWriterInputPixelBufferAdaptor"), "assetWriterInputPixelBufferAdaptorWithAssetWriterInput:sourcePixelBufferAttributes:", Id = input, Id = std::ptr::null_mut());
        if !added || adaptor.is_null() || send!(i8; writer, "startWriting") == 0 {
            let why = description(send!(Id; writer, "error"));
            unsafe {
                objc_release(filter);
                objc_release(config);
            }
            return Err(Error::msg(format!("AVAssetWriter did not start: {why}")));
        }
        let (writer, input, adaptor) = unsafe { (objc_retain(writer), objc_retain(input), objc_retain(adaptor)) };
        let stream: Id = send!(Id; class("SCStream"), "alloc");
        let stream: Id = send!(Id; stream, "initWithFilter:configuration:delegate:", Id = filter, Id = config, Id = std::ptr::null_mut());
        unsafe {
            objc_release(filter);
            objc_release(config);
        }
        if stream.is_null() {
            send!((); writer, "cancelWriting");
            unsafe {
                objc_release(adaptor);
                objc_release(input);
                objc_release(writer);
            }
            return Err(Error::msg("ScreenCaptureKit made no stream for the window"));
        }
        let output: Id = send!(Id; output_class(), "new");
        let sink = Box::new(Sink { writer, input, adaptor, state: Mutex::new(State { t0: Some(crate::host::now_ns()), ..State::default() }) });
        unsafe { object_setIvar(output, class_getInstanceVariable(output_class(), c"sink".as_ptr()), Box::into_raw(sink) as Id) };
        let queue = unsafe { dispatch_queue_create(c"strobe.video".as_ptr(), std::ptr::null()) };
        let mut c = Capture { stream, output, queue, path, size: out, cpu, setup_ms: None };
        let mut err: Id = std::ptr::null_mut();
        if send!(i8; stream, "addStreamOutput:type:sampleHandlerQueue:error:", Id = output, isize = 0, *mut c_void = queue, *mut Id = &mut err) == 0 {
            return Err(Error::msg(format!("ScreenCaptureKit would not take strobe's video output: {}", taken(err))));
        }
        match with_handler(Duration::from_secs(5), Handler::Error, |b| send!((); stream, "startCaptureWithCompletionHandler:", *mut sck::Block = b)) {
            Some((_, e)) if e.is_null() => {}
            Some((_, e)) => return Err(super::refused(format!("ScreenCaptureKit did not start the window's video: {}", taken(e)), "check the Screen Recording permission (strobe status)")),
            None => return Err(Error::msg("ScreenCaptureKit did not start the window's video in 5 s")),
        }
        c.setup_ms = c.cpu.ms();
        c.cpu = super::ThreadCpu::start();
        Ok(c)
    }

    /// Stops the stream, drains its queue and takes the sink back (None: already taken).
    fn end(&mut self) -> Option<Box<Sink>> {
        if self.stream.is_null() {
            return None;
        }
        let _ = with_handler(Duration::from_secs(2), Handler::Error, |b| send!((); self.stream, "stopCaptureWithCompletionHandler:", *mut sck::Block = b));
        unsafe {
            let ivar = class_getInstanceVariable(output_class(), c"sink".as_ptr());
            let sink = object_getIvar(self.output, ivar) as *mut Sink;
            object_setIvar(self.output, ivar, std::ptr::null_mut());
            // The queue is serial: once an empty call queued now has run, every frame before it has.
            extern "C" fn nothing(_: *mut c_void) {}
            dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing);
            objc_release(self.stream);
            objc_release(self.output);
            dispatch_release(self.queue);
            self.stream = std::ptr::null_mut();
            (!sink.is_null()).then(|| Box::from_raw(sink))
        }
    }

    pub fn stop(mut self) -> Result<Video> {
        let t1 = crate::host::now_ns();
        let sink = self.end().ok_or_else(|| Error::msg("the video stopped twice"))?;
        let st = std::mem::take(&mut *sink.state.lock().unwrap_or_else(|e| e.into_inner()));
        let release = |s: &Sink| unsafe {
            objc_release(s.adaptor);
            objc_release(s.input);
            objc_release(s.writer);
        };
        let t0 = st.t0.unwrap_or(t1);
        if st.frames == 0 {
            send!((); sink.writer, "cancelWriting");
            release(&sink);
            let _ = std::fs::remove_file(&self.path);
            return Err(super::refused(
                "ScreenCaptureKit delivered no frame of the window (it shows nothing new while hidden, on another Space, or on a locked or sleeping display)",
                "keep the window on screen while the video records",
            ));
        }
        // The video lasts until the recording stopped, its last frame held; then the input ends.
        send!((); sink.writer, "endSessionAtSourceTime:", CMTime = ns(t1.max(st.last).saturating_sub(t0) as i64));
        send!((); sink.input, "markAsFinished");
        let done = with_handler(Duration::from_secs(30), Handler::Nothing, |b| send!((); sink.writer, "finishWritingWithCompletionHandler:", *mut sck::Block = b));
        let status = send!(isize; sink.writer, "status");
        let why = description(send!(Id; sink.writer, "error"));
        release(&sink);
        // AVAssetWriterStatusCompleted.
        if done.is_none() || status != 2 {
            return Err(Error::msg(format!("AVAssetWriter did not finish the video{}", if why.is_empty() { String::new() } else { format!(": {why}") })));
        }
        Ok(Video {
            kind: Kind::Screen,
            path: self.path.clone(),
            ext: "mp4",
            width: self.size.0,
            height: self.size.1,
            t0_ns: t0,
            t1_ns: t1.max(st.last),
            frames: st.frames,
            source_frames: st.source,
            fps: None,
            source: "ScreenCaptureKit".into(),
            encoder: "AVAssetWriter H.264 (VideoToolbox)".into(),
            // The stream's callbacks on strobe's queue; the scaling is the window server's, the encoding VideoToolbox's.
            cpu_ms: Some(st.cpu_ns as f64 / 1e6),
            setup_cpu_ms: self.setup_ms,
            encoder_cpu_ms: None,
            precision_ms: 0.0,
        })
    }
}

/// A capture dropped without `stop` (a failed block) stops its stream and abandons its file.
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(sink) = self.end() {
            send!((); sink.writer, "cancelWriting");
            unsafe {
                objc_release(sink.adaptor);
                objc_release(sink.input);
                objc_release(sink.writer);
            }
        }
    }
}
