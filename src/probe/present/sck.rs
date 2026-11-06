//! What reached the screen of a macOS app's window, from outside (SPEC §7.2): a ScreenCaptureKit
//! stream of each of the app's windows, alone on its display (`SCContentFilter` of the display with
//! only that window, its source rectangle the window's frame). Each frame is stamped with the time it
//! was displayed (`SCStreamFrameInfoDisplayTime`, Mach absolute time: the host clock) and says which
//! parts changed (`SCStreamFrameInfoDirtyRects`). A frame that changed the app's own content is a
//! present of the app shown at that time.
//!
//! The window server redraws every window's frame by itself: on macOS 27 an idle window's rounded
//! corners and title-bar buttons were dirty 15 to 60 times a second with the app drawing nothing, a
//! desktop-independent window stream reported the whole window changed at every display refresh,
//! and a display-region stream reported damage where another app's window changed under the
//! window. So strobe streams the display region and counts a frame only when pixels of the window
//! changed in a dirty rectangle outside its corners and title-bar buttons.
//!
//! What it cannot see: when the app handed a frame over (no begin), more than one frame per display
//! refresh (the window server composites at most one), what an app draws inside those corners and
//! buttons, and anything without the Screen Recording permission. The stream asks for frames at half
//! the window's size in points: the window server scales each into that, a cost in its process and
//! strobe's, not the app's.
//!
//! Written against the Objective-C runtime and the block ABI directly (no C build, no bindings).

#![allow(non_upper_case_globals)]

use super::{Live, Present, Presents, Unshared};
use crate::host::macos::{CFTypeRef, CGRect, Cf, cf_array, cf_f64, cf_get, cf_i64, cf_rect};
use crate::model::{Error, Result};
use std::collections::HashMap;
use std::ffi::{CString, c_char, c_void};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) type Id = *mut c_void;
pub(crate) type Sel = *mut c_void;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    pub(crate) fn objc_msgSend();
    pub(crate) fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra: usize) -> Id;
    pub(crate) fn objc_registerClassPair(cls: Id);
    pub(crate) fn class_addMethod(cls: Id, sel: Sel, imp: *const c_void, types: *const c_char) -> u8;
    pub(crate) fn class_addIvar(cls: Id, name: *const c_char, size: usize, align: u8, types: *const c_char) -> u8;
    pub(crate) fn class_addProtocol(cls: Id, proto: Id) -> u8;
    pub(crate) fn class_getInstanceVariable(cls: Id, name: *const c_char) -> Id;
    pub(crate) fn objc_getProtocol(name: *const c_char) -> Id;
    pub(crate) fn object_getIvar(obj: Id, ivar: Id) -> Id;
    pub(crate) fn object_setIvar(obj: Id, ivar: Id, value: Id);
    pub(crate) fn objc_retain(obj: Id) -> Id;
    pub(crate) fn objc_release(obj: Id);
    pub(crate) fn objc_autoreleasePoolPush() -> *mut c_void;
    pub(crate) fn objc_autoreleasePoolPop(pool: *mut c_void);
}

unsafe extern "C" {
    static _NSConcreteStackBlock: c_void;
    pub(crate) fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    pub(crate) fn dispatch_sync_f(queue: *mut c_void, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
    pub(crate) fn dispatch_release(object: *mut c_void);
}

#[link(name = "ScreenCaptureKit", kind = "framework")]
unsafe extern "C" {
    pub(crate) static SCStreamFrameInfoStatus: CFTypeRef;
    pub(crate) static SCStreamFrameInfoDisplayTime: CFTypeRef;
    static SCStreamFrameInfoDirtyRects: CFTypeRef;
}

#[cfg(target_arch = "x86_64")]
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_msgSend_stret();
}

/// Points of the window server's own drawing on every window, from its corners: their rounded
/// edges redraw on their own (25 × 25 points each on macOS 27).
const CORNER_PT: f64 = 26.0;
/// The title bar's close, minimize and zoom buttons (macOS 27: three 22-point squares from 5 points
/// in, the top 27 points), which redraw on their own too.
const BUTTONS_PT: (f64, f64) = (80.0, 32.0);

/// A frame's dirty rectangles (output pixels of a `w` × `h` frame, `scale` pixels a point) that may
/// hold the app's own drawing: those outside the corners and the title-bar buttons.
fn app_rects(rects: &[CGRect], w: f64, h: f64, scale: f64) -> Vec<CGRect> {
    let (c, bx, by) = (CORNER_PT * scale, BUTTONS_PT.0 * scale, BUTTONS_PT.1 * scale);
    rects
        .iter()
        .filter(|r| {
            let (x0, y0, x1, y1) = (r.origin.x, r.origin.y, r.origin.x + r.size.width, r.origin.y + r.size.height);
            let corner = (x1 <= c || x0 >= w - c) && (y1 <= c || y0 >= h - c);
            let buttons = x1 <= bx && y1 <= by;
            r.size.width * r.size.height > 0.0 && !corner && !buttons
        })
        .copied()
        .collect()
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn CMSampleBufferGetSampleAttachmentsArray(sbuf: *mut c_void, create: u8) -> CFTypeRef;
    pub(crate) fn CMSampleBufferGetImageBuffer(sbuf: *mut c_void) -> *mut c_void;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferLockBaseAddress(buf: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buf: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buf: *mut c_void) -> *const u8;
    fn CVPixelBufferGetBytesPerRow(buf: *mut c_void) -> usize;
    fn CVPixelBufferGetWidth(buf: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(buf: *mut c_void) -> usize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

/// 32-bit BGRA, the frames strobe asks for. A 4:2:0 YCbCr format or a quarter-size frame read
/// fewer bytes, but on air (macOS 27) both missed 8 to 15% of the frames of a small overlay that
/// animates faint level changes: they rounded away.
const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// Rows compared in a first pass over a frame's rectangles: every 16th, then the rows between. A
/// frame that changed is usually told after a few rows of each rectangle rather than a whole
/// rectangle (on air a sixth of the bytes, 17% less CPU), and a frame that did not reads every row
/// once either way.
const ROW_STRIDE: usize = 16;

/// A frame's pixels, locked for reading: the base address, bytes per row, width and height.
#[derive(Clone, Copy)]
struct Pixels {
    base: *const u8,
    stride: usize,
    w: usize,
    h: usize,
}

impl Pixels {
    /// Locks `image` for reading; None (left unlocked) when it has no readable BGRA pixels.
    fn lock(image: *mut c_void) -> Option<Pixels> {
        if image.is_null() || unsafe { CVPixelBufferLockBaseAddress(image, 1) } != 0 {
            return None;
        }
        let p = unsafe { Pixels { base: CVPixelBufferGetBaseAddress(image), stride: CVPixelBufferGetBytesPerRow(image), w: CVPixelBufferGetWidth(image), h: CVPixelBufferGetHeight(image) } };
        if p.base.is_null() || p.stride < p.w * 4 {
            unsafe { CVPixelBufferUnlockBaseAddress(image, 1) };
            return None;
        }
        Some(p)
    }

    fn row(&self, y: usize, x0: usize, x1: usize) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base.add(y * self.stride + x0 * 4), (x1 - x0) * 4) }
    }
}

/// Whether any pixel of `rects` differs between two frames of one size; stops at the first.
fn differs(now: &Pixels, was: &Pixels, rects: &[CGRect]) -> bool {
    let (w, h) = (now.w, now.h);
    let boxes: Vec<(usize, usize, usize, usize)> = rects
        .iter()
        .map(|r| {
            let x0 = (r.origin.x.floor().max(0.0) as usize).min(w);
            let x1 = ((r.origin.x + r.size.width).ceil().max(0.0) as usize).min(w);
            let y0 = (r.origin.y.floor().max(0.0) as usize).min(h);
            let y1 = ((r.origin.y + r.size.height).ceil().max(0.0) as usize).min(h);
            (x0, x1, y0, y1)
        })
        .collect();
    (0..ROW_STRIDE).any(|phase| boxes.iter().any(|&(x0, x1, y0, y1)| (y0 + phase..y1).step_by(ROW_STRIDE).any(|y| now.row(y, x0, x1) != was.row(y, x0, x1))))
}

/// The last frame whose pixels count, kept as the frame itself (retained and locked for reading)
/// rather than copied: a frame is compared with it and replaces it only when it changed.
#[derive(Default)]
struct Prev {
    image: usize,
    pixels: Option<Pixels>,
}

// SAFETY: the frame is only read, under the sink's lock, and released once.
unsafe impl Send for Prev {}

impl Prev {
    fn keep(&mut self, image: *mut c_void, pixels: Pixels) {
        self.release();
        unsafe { CFRetain(image as CFTypeRef) };
        (self.image, self.pixels) = (image as usize, Some(pixels));
    }

    fn release(&mut self) {
        if self.image != 0 {
            unsafe {
                CVPixelBufferUnlockBaseAddress(self.image as *mut c_void, 1);
                CFRelease(self.image as CFTypeRef);
            }
        }
        // Field by field: assigning a whole `Prev` would drop the old one, which releases again.
        (self.image, self.pixels) = (0, None);
    }
}

impl Drop for Prev {
    fn drop(&mut self) {
        self.release();
    }
}

/// Whether the pixels of `rects` in this frame differ from the last counted frame's (`prev`). The
/// window server reports display damage in the window's rectangle even where only another app's
/// window changed under it, and redraws a window's edges with the same pixels: only a changed pixel
/// is a new frame of the app's. A frame with no such rectangle is not read at all (most of an idle
/// window's frames: its corners and title-bar buttons; locking one cost 40 to 80 µs on air). The
/// first readable frame is the window as it was. None when the frame has no readable pixels.
fn pixels_changed(image: *mut c_void, rects: &[CGRect], prev: &mut Prev) -> Option<bool> {
    if rects.is_empty() && prev.pixels.is_some() {
        return Some(false);
    }
    let now = Pixels::lock(image)?;
    let same_size = prev.pixels.is_some_and(|p| (p.w, p.h) == (now.w, now.h));
    let changed = same_size && prev.pixels.is_some_and(|was| differs(&now, &was, rects));
    if changed || !same_size {
        prev.keep(image, now);
    } else {
        unsafe { CVPixelBufferUnlockBaseAddress(image, 1) };
    }
    Some(changed)
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn CGPreflightScreenCaptureAccess() -> u8;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct CMTime {
    pub(crate) value: i64,
    pub(crate) timescale: i32,
    pub(crate) flags: u32,
    pub(crate) epoch: i64,
}

pub(crate) fn sel(name: &str) -> Sel {
    let c = CString::new(name).unwrap_or_default();
    unsafe { sel_registerName(c.as_ptr()) }
}

pub(crate) fn class(name: &str) -> Id {
    let c = CString::new(name).unwrap_or_default();
    unsafe { objc_getClass(c.as_ptr()) }
}

/// `objc_msgSend` with a given signature.
macro_rules! send {
    ($ret:ty; $obj:expr, $sel:expr $(, $t:ty = $a:expr)*) => {{
        let f: unsafe extern "C" fn($crate::probe::present::sck::Id, $crate::probe::present::sck::Sel $(, $t)*) -> $ret =
            unsafe { std::mem::transmute($crate::probe::present::sck::objc_msgSend as *const ()) };
        unsafe { f($obj, $crate::probe::present::sck::sel($sel) $(, $a)*) }
    }};
}
pub(crate) use send;

/// A method that returns a `CGRect` (`frame`): in registers on arm64, through memory on x86_64.
pub(crate) fn rect_of(obj: Id, name: &str) -> CGRect {
    #[cfg(target_arch = "x86_64")]
    {
        let f: unsafe extern "C" fn(*mut CGRect, Id, Sel) = unsafe { std::mem::transmute(objc_msgSend_stret as *const ()) };
        let mut r = CGRect::default();
        unsafe { f(&mut r, obj, sel(name)) };
        r
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        send!(CGRect; obj, name)
    }
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
static DESCRIPTOR_NONE: Descriptor = Descriptor { reserved: 0, size: std::mem::size_of::<Block>() as u64, copy: copy_ctx, dispose: dispose_ctx, signature: c"v8@?0".as_ptr() };

/// A completion block as the compiler makes one: it starts on the "stack" (strobe's own allocation),
/// and a framework that keeps it `Block_copy`s it to the heap, where the blocks runtime counts its
/// references and calls `dispose` after the last one, possibly long after the handler ran (the XPC
/// reply that carried it is freed later, on another queue). Its context, an `Arc`, is shared by every
/// copy, so it lives until the framework's copy is gone.
#[repr(C)]
pub(crate) struct Block {
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

/// A completion handler without arguments.
extern "C" fn completed_none(b: *mut Block) {
    completed(b, std::ptr::null_mut(), std::ptr::null_mut())
}

/// What a completion handler takes: (object, error), an error, or nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Handler {
    ObjectError,
    Error,
    Nothing,
}

/// Calls `f` with a completion block and waits up to `wait` for it: the handler's (object, error),
/// both retained (the caller releases them); `error_only` for a handler of just an error. The
/// framework keeps its own copy of the block for as long as it wants (`Block`), so neither a late
/// call after a timeout nor the framework releasing its copy after this returns touches freed memory.
pub(crate) fn with_completion(wait: Duration, error_only: bool, f: impl FnOnce(*mut Block)) -> Option<(Id, Id)> {
    with_handler(wait, if error_only { Handler::Error } else { Handler::ObjectError }, f)
}

/// `with_completion` for a handler of any of the shapes `Handler` names.
pub(crate) fn with_handler(wait: Duration, handler: Handler, f: impl FnOnce(*mut Block)) -> Option<(Id, Id)> {
    let done: Arc<Done> = Arc::new((Mutex::new(None), Condvar::new()));
    let (invoke, descriptor) = match handler {
        Handler::Error => (completed_error as *const c_void, &DESCRIPTOR_ERROR),
        Handler::ObjectError => (completed as *const c_void, &DESCRIPTOR),
        Handler::Nothing => (completed_none as *const c_void, &DESCRIPTOR_NONE),
    };
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

pub(crate) fn description(e: Id) -> String {
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
    /// The window's frame when its stream started (the source rectangle follows a moved window).
    rect: Option<(i32, i32, i32, i32)>,
}

/// Per stream, what its output object writes into.
struct Sink {
    pid: u32,
    window: u32,
    shared: Arc<Mutex<Shared>>,
    live: Arc<Live>,
    /// The frames' size in pixels and pixels per point, for their dirty rectangles.
    size: (f64, f64),
    scale: f64,
    /// The last counted frame (`pixels_changed`).
    prev: Mutex<Prev>,
    /// The first complete frame is the window as it was when the stream started, not a new one.
    seen_first: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Shared {
    presents: Vec<Present>,
    events: u64,
    consumer_ns: u64,
}

/// The app's windows it excludes from screen capture, looked for every 500 ms on a thread of its own
/// (and at each `track`), so a block knows when one was on screen whether or not anything tracks the
/// app meanwhile, and one shown and closed during setup is not counted against the measured runs.
#[derive(Default)]
struct Watch {
    pids: Vec<u32>,
    /// Each window's span: first and last seen on screen (each within 500 ms of when it was).
    seen: HashMap<u32, Unshared>,
    /// When the windows were last looked at (by the watcher or `track`).
    looked: Option<Instant>,
    stop: bool,
}

/// How often the app's windows are looked at for excluded ones.
const LOOK: Duration = Duration::from_millis(500);

/// One on-screen window of the app as CoreGraphics lists it: id, owner, frame (points, as
/// `TopWindow::rect`), and whether it lets other apps capture it (`kCGWindowSharingState` not 0).
struct OnScreen {
    id: u32,
    pid: u32,
    rect: (i32, i32, i32, i32),
    shared: bool,
}

/// The app's normal on-screen windows, from one window list read: what `track` needs to tell new,
/// moved and excluded windows apart.
fn on_screen(pids: &[u32]) -> Vec<OnScreen> {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative: u32) -> CFTypeRef;
    }
    // kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements
    let Some(list) = Cf::new(unsafe { CGWindowListCopyWindowInfo(1 | 16, 0) }) else { return Vec::new() };
    cf_array(list.0)
        .into_iter()
        .filter_map(|d| {
            let pid = cf_i64(cf_get(d, "kCGWindowOwnerPID"))? as u32;
            let layer = cf_i64(cf_get(d, "kCGWindowLayer")).unwrap_or(0);
            if !pids.contains(&pid) || !(0..1000).contains(&layer) || cf_f64(cf_get(d, "kCGWindowAlpha")).unwrap_or(1.0) <= 0.0 {
                return None;
            }
            let b = cf_rect(cf_get(d, "kCGWindowBounds"))?;
            // A window under 32 points a side (a tooltip, a badge) holds no reaction worth a stream: one
            // opened mid-block for a browser's tooltip pushed the consumer past its budget.
            if b.size.width < 32.0 || b.size.height < 32.0 {
                return None;
            }
            let rect = (b.origin.x as i32, b.origin.y as i32, b.size.width as i32, b.size.height as i32);
            Some(OnScreen { id: cf_i64(cf_get(d, "kCGWindowNumber"))? as u32, pid, rect, shared: cf_i64(cf_get(d, "kCGWindowSharingState")).unwrap_or(1) != 0 })
        })
        .collect()
}

impl Watch {
    /// Notes which of the app's windows on screen (`on`) it excludes from capture, and returns them.
    fn note(&mut self, on: &[OnScreen]) -> Vec<u32> {
        let now = crate::host::now_ns();
        self.looked = Some(Instant::now());
        let unshared: Vec<&OnScreen> = on.iter().filter(|w| !w.shared).collect();
        for w in &unshared {
            self.seen.entry(w.id).or_insert(Unshared { pid: w.pid, window: w.id as u64, first_ns: now, last_ns: now }).last_ns = now;
        }
        unshared.into_iter().map(|w| w.id).collect()
    }

    fn look(&mut self) -> Vec<u32> {
        let on = on_screen(&self.pids);
        self.note(&on)
    }
}

type Watcher = Arc<(Mutex<Watch>, std::sync::Condvar)>;

/// Looks at the app's windows every `LOOK`, unless `track` just did.
fn watch_unshared(w: Watcher) {
    let (state, cv) = &*w;
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    while !s.stop {
        let since = s.looked.map_or(LOOK, |t| t.elapsed());
        let wait = if since >= LOOK {
            s.look();
            LOOK
        } else {
            LOOK - since
        };
        s = cv.wait_timeout(s, wait).unwrap_or_else(|e| e.into_inner()).0;
    }
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
    /// Windows of the app it excludes from screen capture, which get no stream.
    watch: Watcher,
    watcher: Option<std::thread::JoinHandle<()>>,
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
    let rects = unsafe { cf_dict_value(info, SCStreamFrameInfoDirtyRects) };
    let dirty: Vec<CGRect> = cf_array(rects).into_iter().filter_map(cf_rect).collect();
    let mut sh = sink.shared.lock().unwrap_or_else(|e| e.into_inner());
    sh.events += 1;
    // SCFrameStatusComplete with the app's own content changed. The first frame that draws anything
    // is the window as it was (a stream's first frame may be empty, then the whole window).
    let drew = rects.is_null() || dirty.iter().any(|r| r.size.width * r.size.height > 0.0);
    let first = status == Some(0) && drew && !sink.seen_first.swap(true, std::sync::atomic::Ordering::Relaxed);
    let mut changed = false;
    if status == Some(0) {
        let own = if rects.is_null() { vec![CGRect { origin: Default::default(), size: crate::host::macos::CGSize { width: sink.size.0, height: sink.size.1 } }] } else { app_rects(&dirty, sink.size.0, sink.size.1, sink.scale) };
        let mut prev = sink.prev.lock().unwrap_or_else(|e| e.into_inner());
        // The first frame only fills `prev`; without readable pixels, a dirty rectangle of the app's counts.
        changed = pixels_changed(unsafe { CMSampleBufferGetImageBuffer(sbuf) }, &own, &mut prev).unwrap_or(!own.is_empty()) && !first;
    }
    if changed
        && let Some(t) = shown
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

pub(crate) unsafe fn cf_dict_value(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef {
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
        let watch: Watcher = Arc::default();
        let watcher = {
            let w = watch.clone();
            std::thread::Builder::new().name("strobe-unshared".into()).spawn(move || watch_unshared(w)).ok()
        };
        Ok(Capture {
            shared: Arc::new(Mutex::new(Shared::default())),
            live: Arc::new(Live::default()),
            streams: HashMap::new(),
            pids: Vec::new(),
            tracked: None,
            queue,
            t0: crate::host::now_ns(),
            failed: None,
            watch,
            watcher,
        })
    }

    /// Streams each on-screen window of `pids` (read again at most every 500 ms), and returns once a
    /// new window's stream delivers its first frame, so frames after this are seen.
    pub fn track(&mut self, pids: &[u32]) {
        if self.pids == pids && self.tracked.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            return;
        }
        self.pids = pids.to_vec();
        self.tracked = Some(Instant::now());
        let pool = unsafe { objc_autoreleasePoolPush() };
        // One read of the window list tells new, moved and excluded windows apart.
        let on = on_screen(pids);
        // A window the app excludes from screen capture gets no stream (one would hold none of its
        // pixels, and cost strobe a stream per refresh): `Presents::unshared` says it was there.
        let unshared = {
            let mut w = self.watch.0.lock().unwrap_or_else(|e| e.into_inner());
            w.pids = pids.to_vec();
            w.note(&on)
        };
        // A window that moved or resized gets a new stream: its source rectangle is the old frame.
        let moved: Vec<u32> = self.streams.iter().filter(|(id, s)| unshared.contains(id) || on.iter().any(|w| w.id == **id && Some(w.rect) != s.rect)).map(|(id, _)| *id).collect();
        for id in moved {
            if let Some(s) = self.streams.remove(&id) {
                self.drop_stream(s);
            }
        }
        let ids: Vec<u32> = on.iter().map(|w| w.id).filter(|id| !self.streams.contains_key(id) && !unshared.contains(id)).collect();
        if ids.is_empty() {
            unsafe { objc_autoreleasePoolPop(pool) };
            return;
        }
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
        let displays: Id = send!(Id; content, "displays");
        let displays: Vec<(Id, CGRect)> = (0..send!(usize; displays, "count"))
            .map(|k| {
                let d: Id = send!(Id; displays, "objectAtIndex:", usize = k);
                (d, rect_of(d, "frame"))
            })
            .collect();
        let windows: Id = send!(Id; content, "windows");
        let n: usize = send!(usize; windows, "count");
        for k in 0..n {
            let w: Id = send!(Id; windows, "objectAtIndex:", usize = k);
            let id: u32 = send!(u32; w, "windowID");
            if !ids.contains(&id) {
                continue;
            }
            let state = crate::host::window_state(id as isize);
            let frame = rect_of(w, "frame");
            // The display the window's centre is on (global points, origin at the main display's top left).
            let (cx, cy) = (frame.origin.x + frame.size.width / 2.0, frame.origin.y + frame.size.height / 2.0);
            let on = displays.iter().find(|(_, r)| cx >= r.origin.x && cx < r.origin.x + r.size.width && cy >= r.origin.y && cy < r.origin.y + r.size.height).or(displays.first());
            let Some(&(display, dframe)) = on else { continue };
            let source = CGRect { origin: crate::host::macos::CGPoint { x: frame.origin.x - dframe.origin.x, y: frame.origin.y - dframe.origin.y }, size: frame.size };
            if let Err(e) = self.stream(w, display, source, id, state.as_ref().map_or(0, |s| s.pid), state.map(|s| s.rect)) {
                self.failed = Some(e.message);
            }
        }
        unsafe { objc_release(content) };
        Ok(())
    }

    /// A stream of `window` alone on `display`, of its frame `source` (points on that display).
    fn stream(&mut self, window: Id, display: Id, source: CGRect, id: u32, pid: u32, rect: Option<(i32, i32, i32, i32)>) -> Result<()> {
        let only: Id = send!(Id; class("NSArray"), "arrayWithObject:", Id = window);
        let filter: Id = send!(Id; class("SCContentFilter"), "alloc");
        let filter: Id = send!(Id; filter, "initWithDisplay:includingWindows:", Id = display, Id = only);
        let config: Id = send!(Id; class("SCStreamConfiguration"), "alloc");
        let config: Id = send!(Id; config, "init");
        // Half the window's size in points, at least 32 pixels a side.
        let scale = 0.5;
        let (w, h) = ((source.size.width * scale).round().max(32.0), (source.size.height * scale).round().max(32.0));
        send!((); config, "setSourceRect:", CGRect = source);
        send!((); config, "setWidth:", usize = w as usize);
        send!((); config, "setHeight:", usize = h as usize);
        send!((); config, "setPixelFormat:", u32 = BGRA);
        send!((); config, "setShowsCursor:", i8 = 0);
        // Strobe keeps one frame (the last that counted) besides the one it reads.
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
        let sink = Box::new(Sink { pid, window: id, shared: self.shared.clone(), live: self.live.clone(), size: (w, h), scale: w / source.size.width.max(1.0), prev: Mutex::new(Prev::default()), seen_first: false.into() });
        unsafe { object_setIvar(output, class_getInstanceVariable(output_class(), c"sink".as_ptr()), Box::into_raw(sink) as Id) };
        let mut err: Id = std::ptr::null_mut();
        let added: i8 = send!(i8; stream, "addStreamOutput:type:sampleHandlerQueue:error:", Id = output, isize = 0, *mut c_void = self.queue, *mut Id = &mut err);
        if added == 0 {
            let why = description(err);
            self.drop_stream(Stream { stream, output, rect });
            return Err(Error::msg(format!("ScreenCaptureKit would not take strobe's frame output: {why}")));
        }
        let started = with_completion(Duration::from_secs(5), true, |b| send!((); stream, "startCaptureWithCompletionHandler:", *mut Block = b));
        match started {
            Some((_, e)) if e.is_null() => {}
            Some((_, e)) => {
                let why = description(e);
                unsafe { objc_release(e) };
                self.drop_stream(Stream { stream, output, rect });
                return Err(Error::msg(format!("ScreenCaptureKit did not start: {why}")));
            }
            None => {
                // It may still start: stopped and drained like any other, so it captures nothing on.
                self.drop_stream(Stream { stream, output, rect });
                return Err(Error::msg("ScreenCaptureKit did not start the window's stream in 5 s"));
            }
        }
        // The first frame is the window as it is: once it came, every later frame is new content.
        let sink = unsafe { object_getIvar(output, class_getInstanceVariable(output_class(), c"sink".as_ptr())) } as *const Sink;
        let until = Instant::now() + Duration::from_secs(1);
        while Instant::now() < until && !unsafe { &*sink }.seen_first.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.streams.insert(id, Stream { stream, output, rect });
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
        let mut unshared: Vec<Unshared> = {
            let mut w = self.watch.0.lock().unwrap_or_else(|e| e.into_inner());
            w.look();
            w.seen.drain().map(|(_, u)| u).collect()
        };
        self.end_watch();
        unshared.sort_by_key(|u| (u.first_ns, u.window));
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
            unshared,
        })
    }
}

/// After `stop`, or instead of it (a failed block): every stream stopped and drained, then the queue
/// they delivered on released (each drain waited on it, so nothing still runs there).
impl Drop for Capture {
    fn drop(&mut self) {
        self.end_watch();
        let pool = unsafe { objc_autoreleasePoolPush() };
        for (_, s) in std::mem::take(&mut self.streams) {
            self.drop_stream(s);
        }
        unsafe {
            objc_autoreleasePoolPop(pool);
            if !self.queue.is_null() {
                dispatch_release(self.queue);
            }
        }
    }
}

impl Capture {
    /// Stops the excluded-window watcher and waits for it.
    fn end_watch(&mut self) {
        self.watch.0.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.watch.1.notify_all();
        if let Some(t) = self.watcher.take() {
            let _ = t.join();
        }
    }
}
