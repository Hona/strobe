//! What reached the screen of a macOS app's window, from outside (SPEC §7.2): a ScreenCaptureKit
//! stream of each of the app's windows on its own (`SCContentFilter`'s desktop-independent window),
//! which delivers a frame only when the window server composited new content of that window, stamped
//! with the time that frame was displayed (`SCStreamFrameInfoDisplayTime`, Mach absolute time: the
//! host clock). Each such frame is a present of the app shown at that time.
//!
//! What it cannot see: when the app handed a frame over (no begin), more than one frame per display
//! refresh (the window server composites at most one), and anything without the Screen Recording
//! permission. The stream asks for 64×64-pixel frames: the window server scales each new frame of the
//! window into that, a cost in its process and strobe's, not the app's.
//!
//! Written against the Objective-C runtime and the block ABI directly (no C build, no bindings).

#![allow(non_upper_case_globals)]

use super::{Live, Present, Presents};
use crate::host::macos::{CFTypeRef, cf_array, cf_i64};
use crate::model::{Error, Result};
use std::collections::HashMap;
use std::ffi::{CString, c_char, c_void};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

type Id = *mut c_void;
type Sel = *mut c_void;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra: usize) -> Id;
    fn objc_registerClassPair(cls: Id);
    fn class_addMethod(cls: Id, sel: Sel, imp: *const c_void, types: *const c_char) -> u8;
    fn class_addIvar(cls: Id, name: *const c_char, size: usize, align: u8, types: *const c_char) -> u8;
    fn class_addProtocol(cls: Id, proto: Id) -> u8;
    fn class_getInstanceVariable(cls: Id, name: *const c_char) -> Id;
    fn objc_getProtocol(name: *const c_char) -> Id;
    fn object_getIvar(obj: Id, ivar: Id) -> Id;
    fn object_setIvar(obj: Id, ivar: Id, value: Id);
    fn objc_retain(obj: Id) -> Id;
    fn objc_release(obj: Id);
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

unsafe extern "C" {
    static _NSConcreteStackBlock: c_void;
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_sync_f(queue: *mut c_void, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
}

#[link(name = "ScreenCaptureKit", kind = "framework")]
unsafe extern "C" {
    static SCStreamFrameInfoStatus: CFTypeRef;
    static SCStreamFrameInfoDisplayTime: CFTypeRef;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetSampleAttachmentsArray(sbuf: *mut c_void, create: u8) -> CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> u8;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

fn sel(name: &str) -> Sel {
    let c = CString::new(name).unwrap_or_default();
    unsafe { sel_registerName(c.as_ptr()) }
}

fn class(name: &str) -> Id {
    let c = CString::new(name).unwrap_or_default();
    unsafe { objc_getClass(c.as_ptr()) }
}

/// `objc_msgSend` with a given signature.
macro_rules! send {
    ($ret:ty; $obj:expr, $sel:expr $(, $t:ty = $a:expr)*) => {{
        let f: unsafe extern "C" fn(Id, Sel $(, $t)*) -> $ret = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
        unsafe { f($obj, sel($sel) $(, $a)*) }
    }};
}

#[repr(C)]
struct Descriptor {
    reserved: u64,
    size: u64,
    copy: extern "C" fn(*mut Block, *const Block),
    dispose: extern "C" fn(*const Block),
    signature: *const c_char,
}

// SAFETY: only static data, read by the blocks runtime.
unsafe impl Sync for Descriptor {}

static DESCRIPTOR: Descriptor = Descriptor { reserved: 0, size: std::mem::size_of::<Block>() as u64, copy: copy_ctx, dispose: dispose_ctx, signature: c"v24@?0@8@16".as_ptr() };
static DESCRIPTOR_ERROR: Descriptor = Descriptor { reserved: 0, size: std::mem::size_of::<Block>() as u64, copy: copy_ctx, dispose: dispose_ctx, signature: c"v16@?0@8".as_ptr() };

/// A completion block as the compiler makes one: it starts on the "stack" (strobe's own allocation),
/// and a framework that keeps it `Block_copy`s it to the heap, where the blocks runtime counts its
/// references and calls `dispose` after the last one, possibly long after the handler ran (the XPC
/// reply that carried it is freed later, on another queue). Its context, an `Arc`, is shared by every
/// copy, so it lives until the framework's copy is gone.
#[repr(C)]
struct Block {
    isa: *const c_void,
    flags: i32,
    reserved: i32,
    invoke: *const c_void,
    descriptor: *const Descriptor,
    ctx: *const c_void,
}

const BLOCK_HAS_COPY_DISPOSE: i32 = 1 << 25;
const BLOCK_HAS_SIGNATURE: i32 = 1 << 30;

/// The blocks runtime copied `src` into `dst` (bytes already moved): the copy holds the context too.
extern "C" fn copy_ctx(_dst: *mut Block, src: *const Block) {
    unsafe { Arc::increment_strong_count((*src).ctx as *const Done) };
}

/// The last reference to a heap copy went away.
extern "C" fn dispose_ctx(b: *const Block) {
    unsafe { Arc::decrement_strong_count((*b).ctx as *const Done) };
}

/// What a completion handler hands back: its two object arguments (retained), once it ran.
type Done = (Mutex<Option<(usize, usize)>>, Condvar);

extern "C" fn completed(b: *mut Block, a: Id, e: Id) {
    unsafe {
        let done = &*((*b).ctx as *const Done);
        if !a.is_null() {
            objc_retain(a);
        }
        if !e.is_null() {
            objc_retain(e);
        }
        *done.0.lock().unwrap_or_else(|x| x.into_inner()) = Some((a as usize, e as usize));
        done.1.notify_all();
    }
}

/// A completion handler whose only argument is the error.
extern "C" fn completed_error(b: *mut Block, e: Id) {
    completed(b, std::ptr::null_mut(), e)
}

/// Calls `f` with a completion block and waits up to `wait` for it: the handler's (object, error),
/// both retained (the caller releases them); `error_only` for a handler of just an error. The
/// framework keeps its own copy of the block for as long as it wants (`Block`), so neither a late
/// call after a timeout nor the framework releasing its copy after this returns touches freed memory.
fn with_completion(wait: Duration, error_only: bool, f: impl FnOnce(*mut Block)) -> Option<(Id, Id)> {
    let done: Arc<Done> = Arc::new((Mutex::new(None), Condvar::new()));
    let (invoke, descriptor) = if error_only { (completed_error as *const c_void, &DESCRIPTOR_ERROR) } else { (completed as *const c_void, &DESCRIPTOR) };
    let mut block = Block { isa: unsafe { &_NSConcreteStackBlock }, flags: BLOCK_HAS_COPY_DISPOSE | BLOCK_HAS_SIGNATURE, reserved: 0, invoke, descriptor, ctx: Arc::as_ptr(&done) as *const c_void };
    // The method returns before the handler runs; a framework that calls it later copied the block.
    f(&raw mut block);
    let deadline = Instant::now() + wait;
    let mut got = done.0.lock().unwrap_or_else(|e| e.into_inner());
    while got.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        got = done.1.wait_timeout(got, left).unwrap_or_else(|e| e.into_inner()).0;
    }
    got.take().map(|(a, e)| (a as Id, e as Id))
}

fn description(e: Id) -> String {
    if e.is_null() {
        return String::new();
    }
    let s: Id = send!(Id; e, "localizedDescription");
    let p: *const c_char = send!(*const c_char; s, "UTF8String");
    if p.is_null() { String::new() } else { unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().to_string() }
}

/// One window's stream and what its frames are counted against.
struct Stream {
    stream: Id,
    output: Id,
}

/// Per stream, what its output object writes into.
struct Sink {
    pid: u32,
    window: u32,
    shared: Arc<Mutex<Shared>>,
    live: Arc<Live>,
    /// The first complete frame is the window as it was when the stream started, not a new one.
    seen_first: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Shared {
    presents: Vec<Present>,
    events: u64,
    consumer_ns: u64,
}

pub struct Capture {
    shared: Arc<Mutex<Shared>>,
    pub live: Arc<Live>,
    streams: HashMap<u32, Stream>,
    pids: Vec<u32>,
    tracked: Option<Instant>,
    queue: *mut c_void,
    t0: u64,
    /// Why a window has no stream, when one failed to start.
    failed: Option<String>,
}

/// `stream:didOutputSampleBuffer:ofType:` of strobe's output class.
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
    let attachments = unsafe { CMSampleBufferGetSampleAttachmentsArray(sbuf, 0) };
    let Some(&info) = cf_array(attachments).first() else { return };
    let status = cf_i64(unsafe { cf_dict_value(info, SCStreamFrameInfoStatus) });
    let shown = cf_i64(unsafe { cf_dict_value(info, SCStreamFrameInfoDisplayTime) });
    let mut sh = sink.shared.lock().unwrap_or_else(|e| e.into_inner());
    sh.events += 1;
    // SCFrameStatusComplete: new content of the window.
    if status == Some(0)
        && let Some(t) = shown
        && sink.seen_first.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        let at = crate::host::macos::mach_ns(t as u64);
        sh.presents.push(Present {
            pid: sink.pid,
            tid: 0,
            start_ns: at,
            stop_ns: 0,
            ready_ns: 0,
            screen_ns: at,
            swapchain: sink.window as u64,
            sync_interval: 1,
            mode: "ScreenCaptureKit".into(),
            state: "displayed".into(),
        });
        sink.live.push(sink.pid, at, at);
    }
    sh.consumer_ns += crate::host::thread_cycles().saturating_sub(cpu0);
}

unsafe fn cf_dict_value(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef {
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDictionaryGetValue(d: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    }
    if dict.is_null() || key.is_null() {
        return std::ptr::null();
    }
    unsafe { CFDictionaryGetValue(dict, key) }
}

/// strobe's SCStreamOutput class, made once.
fn output_class() -> Id {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CLASS.get_or_init(|| unsafe {
        let existing = class("StrobeFrameOutput");
        if !existing.is_null() {
            return existing as usize;
        }
        let cls = objc_allocateClassPair(class("NSObject"), c"StrobeFrameOutput".as_ptr(), 0);
        class_addIvar(cls, c"sink".as_ptr(), std::mem::size_of::<usize>(), 3, c"^v".as_ptr());
        class_addMethod(cls, sel("stream:didOutputSampleBuffer:ofType:"), on_frame as *const c_void, c"v@:@^vq".as_ptr());
        let proto = objc_getProtocol(c"SCStreamOutput".as_ptr());
        if !proto.is_null() {
            class_addProtocol(cls, proto);
        }
        objc_registerClassPair(cls);
        cls as usize
    }) as Id
}

impl Capture {
    pub fn start() -> Result<Capture> {
        if unsafe { CGPreflightScreenCaptureAccess() } == 0 {
            let who = crate::host::macos::screen_recording_grantee();
            return Err(Error::msg(format!(
                "frames on screen come from ScreenCaptureKit, which needs the Screen Recording permission for {who}: {}",
                crate::host::macos::screen_recording_pane()
            )));
        }
        let queue = unsafe { dispatch_queue_create(c"strobe.frames".as_ptr(), std::ptr::null()) };
        Ok(Capture { shared: Arc::new(Mutex::new(Shared::default())), live: Arc::new(Live::default()), streams: HashMap::new(), pids: Vec::new(), tracked: None, queue, t0: crate::host::now_ns(), failed: None })
    }

    /// Streams each on-screen window of `pids` (read again at most every 500 ms), and returns once a
    /// new window's stream delivers its first frame, so frames after this are seen.
    pub fn track(&mut self, pids: &[u32]) {
        if self.pids == pids && self.tracked.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            return;
        }
        self.pids = pids.to_vec();
        self.tracked = Some(Instant::now());
        let ids: Vec<u32> = crate::host::top_windows(pids).into_iter().map(|w| w.0 as u32).filter(|id| !self.streams.contains_key(id)).collect();
        if ids.is_empty() {
            return;
        }
        let pool = unsafe { objc_autoreleasePoolPush() };
        if let Err(e) = self.open(&ids) {
            self.failed = Some(e.message);
        }
        unsafe { objc_autoreleasePoolPop(pool) };
    }

    fn open(&mut self, ids: &[u32]) -> Result<()> {
        let content = with_completion(Duration::from_secs(5), false, |b| {
            send!((); class("SCShareableContent"), "getShareableContentExcludingDesktopWindows:onScreenWindowsOnly:completionHandler:", i8 = 1, i8 = 1, *mut Block = b)
        });
        let Some((content, err)) = content else { return Err(Error::msg("ScreenCaptureKit did not list the windows in 5 s")) };
        if content.is_null() {
            let why = description(err);
            unsafe { objc_release(err) };
            return Err(Error::msg(format!("ScreenCaptureKit: {why}")));
        }
        let windows: Id = send!(Id; content, "windows");
        let n: usize = send!(usize; windows, "count");
        for k in 0..n {
            let w: Id = send!(Id; windows, "objectAtIndex:", usize = k);
            let id: u32 = send!(u32; w, "windowID");
            if !ids.contains(&id) {
                continue;
            }
            let pid = crate::host::window_state(id as isize).map_or(0, |s| s.pid);
            if let Err(e) = self.stream(w, id, pid) {
                self.failed = Some(e.message);
            }
        }
        unsafe { objc_release(content) };
        Ok(())
    }

    fn stream(&mut self, window: Id, id: u32, pid: u32) -> Result<()> {
        let filter: Id = send!(Id; class("SCContentFilter"), "alloc");
        let filter: Id = send!(Id; filter, "initWithDesktopIndependentWindow:", Id = window);
        let config: Id = send!(Id; class("SCStreamConfiguration"), "alloc");
        let config: Id = send!(Id; config, "init");
        send!((); config, "setWidth:", usize = 64);
        send!((); config, "setHeight:", usize = 64);
        send!((); config, "setShowsCursor:", i8 = 0);
        send!((); config, "setQueueDepth:", isize = 8);
        // As often as the window changes (the window server composites at most once a refresh).
        send!((); config, "setMinimumFrameInterval:", CMTime = CMTime { value: 1, timescale: 1000, flags: 1, epoch: 0 });
        let stream: Id = send!(Id; class("SCStream"), "alloc");
        let stream: Id = send!(Id; stream, "initWithFilter:configuration:delegate:", Id = filter, Id = config, Id = std::ptr::null_mut());
        unsafe {
            objc_release(filter);
            objc_release(config);
        }
        if stream.is_null() {
            return Err(Error::msg("ScreenCaptureKit made no stream for the window"));
        }
        let output: Id = send!(Id; output_class(), "new");
        let sink = Box::new(Sink { pid, window: id, shared: self.shared.clone(), live: self.live.clone(), seen_first: false.into() });
        unsafe { object_setIvar(output, class_getInstanceVariable(output_class(), c"sink".as_ptr()), Box::into_raw(sink) as Id) };
        let mut err: Id = std::ptr::null_mut();
        let added: i8 = send!(i8; stream, "addStreamOutput:type:sampleHandlerQueue:error:", Id = output, isize = 0, *mut c_void = self.queue, *mut Id = &mut err);
        if added == 0 {
            let why = description(err);
            self.drop_stream(Stream { stream, output });
            return Err(Error::msg(format!("ScreenCaptureKit would not take strobe's frame output: {why}")));
        }
        let started = with_completion(Duration::from_secs(5), true, |b| send!((); stream, "startCaptureWithCompletionHandler:", *mut Block = b));
        match started {
            Some((_, e)) if e.is_null() => {}
            Some((_, e)) => {
                let why = description(e);
                unsafe { objc_release(e) };
                self.drop_stream(Stream { stream, output });
                return Err(Error::msg(format!("ScreenCaptureKit did not start: {why}")));
            }
            None => return Err(Error::msg("ScreenCaptureKit did not start the window's stream in 5 s")),
        }
        // The first frame is the window as it is: once it came, every later frame is new content.
        let sink = unsafe { object_getIvar(output, class_getInstanceVariable(output_class(), c"sink".as_ptr())) } as *const Sink;
        let until = Instant::now() + Duration::from_secs(1);
        while Instant::now() < until && !unsafe { &*sink }.seen_first.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.streams.insert(id, Stream { stream, output });
        Ok(())
    }

    fn drop_stream(&self, s: Stream) {
        let _ = with_completion(Duration::from_secs(2), true, |b| send!((); s.stream, "stopCaptureWithCompletionHandler:", *mut Block = b));
        unsafe {
            let ivar = class_getInstanceVariable(output_class(), c"sink".as_ptr());
            let sink = object_getIvar(s.output, ivar) as *mut Sink;
            object_setIvar(s.output, ivar, std::ptr::null_mut());
            // The queue may still run a callback that read the sink before it was cleared: the queue
            // is serial, so once an empty call queued now has run, every earlier one has.
            extern "C" fn nothing(_: *mut c_void) {}
            dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing);
            if !sink.is_null() {
                drop(Box::from_raw(sink));
            }
            objc_release(s.stream);
            objc_release(s.output);
        }
    }

    pub fn stop(mut self) -> Result<Presents> {
        let pool = unsafe { objc_autoreleasePoolPush() };
        for (_, s) in std::mem::take(&mut self.streams) {
            self.drop_stream(s);
        }
        unsafe { objc_autoreleasePoolPop(pool) };
        let sh = std::mem::take(&mut *self.shared.lock().unwrap_or_else(|e| e.into_inner()));
        if sh.events == 0
            && let Some(why) = &self.failed
        {
            return Err(Error::msg(why.clone()));
        }
        Ok(Presents {
            presents: sh.presents,
            vsyncs: Vec::new(),
            events: sh.events,
            events_lost: 0,
            consumer_cpu_ms: Some(sh.consumer_ns as f64 / 1e6),
            t0_ns: self.t0,
            t1_ns: crate::host::now_ns(),
            no_begin: true,
            only: None,
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        for (_, s) in std::mem::take(&mut self.streams) {
            self.drop_stream(s);
        }
    }
}
