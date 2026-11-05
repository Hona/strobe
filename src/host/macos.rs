//! macOS calls behind `host`: libproc, Mach, sysctl, CoreGraphics, IOKit and the accessibility API,
//! declared here by hand (no C build). Nothing spawns.
//!
//! - CPU: `proc_pid_rusage` user + system time (Mach time, charged at every context switch), so the
//!   "cycle" counter of `ProcTimes::cycles` is CPU nanoseconds and `host::tsc` is `now_ns`.
//! - Wakes: `proc_taskinfo.pti_csw`, a process's context switches (live and exited threads). macOS
//!   keeps no per-thread count a user can read: per-thread rows carry CPU and names, not wakes.
//! - No CPU affinity API exists (Apple Silicon ignores affinity tags): targets run unpinned.
//! - Windows from `CGWindowListCopyWindowInfo` (no permission for bounds, owner and on-screen state);
//!   moving another app's window needs the Accessibility permission; reading its pixels needs Screen
//!   Recording. Neither is asked for: without them strobe says what it cannot do.

#![allow(non_upper_case_globals, non_snake_case, clippy::upper_case_acronyms)]

use super::{ProcInfo, ProcTimes, ThreadInfo, Topology};
use std::collections::HashMap;
use std::ffi::{CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub type CFTypeRef = *const c_void;
type CFIndex = isize;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFTypeRef;
    fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> u8;
    fn CFNumberGetValue(n: CFTypeRef, kind: CFIndex, out: *mut c_void) -> u8;
    fn CFBooleanGetValue(b: CFTypeRef) -> u8;
    fn CFArrayGetCount(a: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(a: CFTypeRef, i: CFIndex) -> CFTypeRef;
    fn CFDictionaryGetValue(d: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
    fn CFDataGetLength(d: CFTypeRef) -> CFIndex;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative: u32) -> CFTypeRef;
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    fn CGRectMakeWithDictionaryRepresentation(dict: CFTypeRef, rect: *mut CGRect) -> u8;
    fn CGSessionCopyCurrentDictionary() -> CFTypeRef;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayIsAsleep(display: u32) -> u32;
    fn CGGetDisplaysWithPoint(p: CGPoint, max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGGetDisplaysWithRect(r: CGRect, max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayCopyDisplayMode(display: u32) -> CFTypeRef;
    fn CGDisplayModeGetRefreshRate(mode: CFTypeRef) -> f64;
    fn CGDisplayModeGetPixelWidth(mode: CFTypeRef) -> usize;
    fn CGDisplayModeRelease(mode: CFTypeRef);
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayIsBuiltin(display: u32) -> u32;
    fn CGPreflightScreenCaptureAccess() -> u8;
    fn CGImageGetWidth(img: CFTypeRef) -> usize;
    fn CGImageGetHeight(img: CFTypeRef) -> usize;
    fn CGImageGetBytesPerRow(img: CFTypeRef) -> usize;
    fn CGImageGetBitsPerPixel(img: CFTypeRef) -> usize;
    fn CGImageGetDataProvider(img: CFTypeRef) -> CFTypeRef;
    fn CGDataProviderCopyData(p: CFTypeRef) -> CFTypeRef;
    fn CGImageRelease(img: CFTypeRef);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSGetProvidingPowerSourceType(blob: CFTypeRef) -> CFTypeRef;
    fn IOPMAssertionCreateWithName(kind: CFTypeRef, level: u32, name: CFTypeRef, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(el: CFTypeRef, attr: CFTypeRef, out: *mut CFTypeRef) -> i32;
    fn AXUIElementSetAttributeValue(el: CFTypeRef, attr: CFTypeRef, value: CFTypeRef) -> i32;
    fn AXValueCreate(kind: u32, ptr: *const c_void) -> CFTypeRef;
    fn AXValueGetValue(v: CFTypeRef, kind: u32, ptr: *mut c_void) -> u8;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
}

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut Timebase) -> i32;
    fn mach_absolute_time() -> u64;
}

// NSProcessInfo lives in Foundation; linking it makes the class available to `objc_getClass`.
#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}

const UTF8: u32 = 0x0800_0100;

/// A CoreFoundation object this code owns (released on drop).
pub struct Cf(pub CFTypeRef);

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

impl Cf {
    pub fn new(p: CFTypeRef) -> Option<Cf> {
        (!p.is_null()).then_some(Cf(p))
    }
}

pub(crate) fn cfstr(s: &str) -> Cf {
    let c = CString::new(s).unwrap_or_default();
    Cf(unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) })
}

/// Borrowed CoreFoundation values read as Rust ones.
pub(crate) fn cf_string(v: CFTypeRef) -> Option<String> {
    if v.is_null() || unsafe { CFGetTypeID(v) != CFStringGetTypeID() } {
        return None;
    }
    let mut buf = vec![0 as c_char; 4096];
    (unsafe { CFStringGetCString(v, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8) } != 0).then(|| unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().to_string())
}

pub(crate) fn cf_i64(v: CFTypeRef) -> Option<i64> {
    if v.is_null() {
        return None;
    }
    if unsafe { CFGetTypeID(v) == CFBooleanGetTypeID() } {
        return Some(unsafe { CFBooleanGetValue(v) } as i64);
    }
    if unsafe { CFGetTypeID(v) != CFNumberGetTypeID() } {
        return None;
    }
    let mut out = 0i64;
    (unsafe { CFNumberGetValue(v, 4, &mut out as *mut i64 as *mut c_void) } != 0).then_some(out)
}

pub(crate) fn cf_f64(v: CFTypeRef) -> Option<f64> {
    if v.is_null() || unsafe { CFGetTypeID(v) != CFNumberGetTypeID() } {
        return None;
    }
    let mut out = 0f64;
    (unsafe { CFNumberGetValue(v, 13, &mut out as *mut f64 as *mut c_void) } != 0).then_some(out)
}

pub(crate) fn cf_get(dict: CFTypeRef, key: &str) -> CFTypeRef {
    if dict.is_null() || unsafe { CFGetTypeID(dict) != CFDictionaryGetTypeID() } {
        return std::ptr::null();
    }
    let k = cfstr(key);
    unsafe { CFDictionaryGetValue(dict, k.0) }
}

/// A rectangle from its dictionary form (`CGRectCreateDictionaryRepresentation`).
pub(crate) fn cf_rect(v: CFTypeRef) -> Option<CGRect> {
    if v.is_null() || unsafe { CFGetTypeID(v) != CFDictionaryGetTypeID() } {
        return None;
    }
    let mut r = CGRect::default();
    (unsafe { CGRectMakeWithDictionaryRepresentation(v, &mut r) } != 0).then_some(r)
}

pub(crate) fn cf_array(v: CFTypeRef) -> Vec<CFTypeRef> {
    if v.is_null() || unsafe { CFGetTypeID(v) != CFArrayGetTypeID() } {
        return Vec::new();
    }
    (0..unsafe { CFArrayGetCount(v) }).map(|i| unsafe { CFArrayGetValueAtIndex(v, i) }).collect()
}

fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut t = Timebase { numer: 0, denom: 0 };
        unsafe { mach_timebase_info(&mut t) };
        (t.numer.max(1) as u64, t.denom.max(1) as u64)
    })
}

/// Mach absolute time to ns.
pub fn mach_ns(t: u64) -> u64 {
    let (n, d) = timebase();
    ((t as u128) * n as u128 / d as u128) as u64
}

/// `mach_absolute_time` in ns: the clock Metal's presented times, CoreAnimation and CVDisplayLink use.
pub fn now_ns() -> u64 {
    mach_ns(unsafe { mach_absolute_time() })
}

pub fn thread_cycles() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn sysctl_string(name: &str) -> Option<String> {
    let c = CString::new(name).ok()?;
    let mut len = 0usize;
    unsafe { libc::sysctlbyname(c.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    (unsafe { libc::sysctlbyname(c.as_ptr(), buf.as_mut_ptr() as *mut c_void, &mut len, std::ptr::null_mut(), 0) } == 0).then(|| String::from_utf8_lossy(&buf[..len]).trim_end_matches('\0').trim().to_string())
}

fn sysctl_i64(name: &str) -> Option<i64> {
    let c = CString::new(name).ok()?;
    let mut buf = [0u8; 8];
    let mut len = 8usize;
    if unsafe { libc::sysctlbyname(c.as_ptr(), buf.as_mut_ptr() as *mut c_void, &mut len, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    Some(match len {
        4 => i32::from_le_bytes(buf[..4].try_into().ok()?) as i64,
        8 => i64::from_le_bytes(buf),
        _ => return None,
    })
}

fn all_pids() -> Vec<u32> {
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0i32; n as usize + 64];
    let got = unsafe { libc::proc_listallpids(buf.as_mut_ptr() as *mut c_void, (buf.len() * 4) as i32) };
    buf.truncate(got.max(0) as usize);
    buf.into_iter().filter(|&p| p > 0).map(|p| p as u32).collect()
}

struct Bsd {
    pid: u32,
    ppid: u32,
    name: String,
    /// Start time in µs since the epoch: a parent's children start no earlier.
    start: u64,
}

fn bsd(pid: u32) -> Option<Bsd> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    if unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut c_void, size) } != size {
        return None;
    }
    let text = |b: &[c_char]| unsafe { std::ffi::CStr::from_ptr(b.as_ptr()) }.to_string_lossy().to_string();
    let name = Some(text(&info.pbi_name)).filter(|n| !n.is_empty()).unwrap_or_else(|| text(&info.pbi_comm));
    Some(Bsd { pid, ppid: info.pbi_ppid, name, start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec })
}

/// (CPU ns, physical footprint, resident, disk bytes read, written), from `proc_pid_rusage` (this
/// user's processes). The footprint is what Activity Monitor calls a process's memory: its dirty
/// private pages, compressed or swapped ones and its GPU (IOKit) allocations.
fn rusage(pid: u32) -> Option<(u64, u64, u64, u64, u64)> {
    let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    if unsafe { libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V4, &mut ri as *mut _ as *mut libc::rusage_info_t) } != 0 {
        return None;
    }
    Some((mach_ns(ri.ri_user_time + ri.ri_system_time), ri.ri_phys_footprint, ri.ri_resident_size, ri.ri_diskio_bytesread, ri.ri_diskio_byteswritten))
}

fn tree_of(all: &[Bsd], root: u32) -> Vec<usize> {
    let Some(r) = all.iter().position(|p| p.pid == root) else { return Vec::new() };
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, p) in all.iter().enumerate() {
        if p.ppid != p.pid {
            children.entry(p.ppid).or_default().push(i);
        }
    }
    let mut out = vec![r];
    let mut stack = vec![r];
    while let Some(i) = stack.pop() {
        for &j in children.get(&all[i].pid).map(Vec::as_slice).unwrap_or(&[]) {
            if all[j].start >= all[i].start && out.len() <= all.len() {
                out.push(j);
                stack.push(j);
            }
        }
    }
    out
}

fn all_bsd() -> Vec<Bsd> {
    all_pids().into_iter().filter_map(bsd).collect()
}

pub fn process_tree(root: u32) -> Vec<ProcTimes> {
    let all = all_bsd();
    tree_of(&all, root)
        .into_iter()
        .map(|i| {
            let b = &all[i];
            let (cpu, footprint, resident, _, _) = rusage(b.pid).unwrap_or_default();
            ProcTimes { pid: b.pid, parent: b.ppid, name: b.name.clone(), cpu_ns: cpu, cycles: cpu, private_bytes: footprint, working_set: resident }
        })
        .collect()
}

pub fn tree_pids(root: u32) -> Vec<u32> {
    let all = all_bsd();
    tree_of(&all, root).into_iter().map(|i| all[i].pid).collect()
}

pub fn all_processes() -> Vec<ProcInfo> {
    all_bsd()
        .into_iter()
        .map(|b| {
            let (cpu, footprint, _, _, _) = rusage(b.pid).unwrap_or_default();
            ProcInfo { pid: b.pid, parent: b.ppid, name: b.name, cpu_ns: cpu, cycles: cpu, private_bytes: footprint, threads: task_info(b.pid).map(|t| t.pti_threadnum as u32).unwrap_or(0) }
        })
        .collect()
}

fn task_info(pid: u32) -> Option<libc::proc_taskinfo> {
    let mut t: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
    (unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDTASKINFO, 0, &mut t as *mut _ as *mut c_void, size) } == size).then_some(t)
}

const PROC_PIDLISTTHREADIDS: i32 = 28;
const PROC_PIDTHREADID64INFO: i32 = 15;

fn thread_ids(pid: u32) -> Vec<u64> {
    let mut buf = vec![0u64; 512];
    loop {
        let bytes = (buf.len() * 8) as i32;
        let n = unsafe { libc::proc_pidinfo(pid as i32, PROC_PIDLISTTHREADIDS, 0, buf.as_mut_ptr() as *mut c_void, bytes) };
        if n <= 0 {
            return Vec::new();
        }
        if n < bytes || buf.len() >= 65536 {
            buf.truncate(n as usize / 8);
            return buf;
        }
        buf.resize(buf.len() * 2, 0);
    }
}

fn thread_info(pid: u32, tid: u64) -> Option<libc::proc_threadinfo> {
    let mut t: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_threadinfo>() as i32;
    (unsafe { libc::proc_pidinfo(pid as i32, PROC_PIDTHREADID64INFO, tid, &mut t as *mut _ as *mut c_void, size) } == size).then_some(t)
}

/// Thread ids are 64-bit on macOS and need their process: the last `threads` call's map, for
/// `thread_cycles_of` and `thread_name`.
static THREADS: Mutex<Option<HashMap<u32, (u32, u64)>>> = Mutex::new(None);

/// Per process: one row with its context switches (`tid` 0, the only wake count macOS gives), then
/// every thread with its CPU time and no wakes (`switches` 0). IO is disk bytes (`ri_diskio_*`):
/// macOS counts no cached reads or writes per process.
pub fn threads(pids: &[u32]) -> (Vec<ThreadInfo>, Vec<(u32, u64, u64)>) {
    let (mut out, mut io) = (Vec::new(), Vec::new());
    let mut map = HashMap::new();
    for &pid in pids {
        if let Some((_, _, _, r, w)) = rusage(pid) {
            io.push((pid, r, w));
        }
        let Some(task) = task_info(pid) else { continue };
        out.push(ThreadInfo { pid, tid: 0, switches: task.pti_csw as u32, cpu_ns: 0 });
        for id in thread_ids(pid) {
            let Some(t) = thread_info(pid, id) else { continue };
            map.insert(id as u32, (pid, id));
            out.push(ThreadInfo { pid, tid: id as u32, switches: 0, cpu_ns: t.pth_user_time + t.pth_system_time });
        }
    }
    *THREADS.lock().unwrap_or_else(|e| e.into_inner()) = Some(map);
    out.sort_by_key(|t| (t.pid, t.tid));
    (out, io)
}

fn known_thread(tid: u32) -> Option<(u32, u64)> {
    THREADS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.get(&tid).copied()
}

/// A thread's CPU ns (µs resolution), for a thread the last `threads` call listed.
pub fn thread_cycles_of(tid: u32) -> Option<u64> {
    let (pid, id) = known_thread(tid)?;
    thread_info(pid, id).map(|t| t.pth_user_time + t.pth_system_time)
}

pub fn thread_name(tid: u32) -> Option<String> {
    if tid == 0 {
        return Some("(whole process: macOS counts wakes per process)".into());
    }
    let (pid, id) = known_thread(tid)?;
    let t = thread_info(pid, id)?;
    let name = unsafe { std::ffi::CStr::from_ptr(t.pth_name.as_ptr()) }.to_string_lossy().to_string();
    (!name.is_empty()).then_some(name)
}

const PROCESSOR_CPU_LOAD_INFO: i32 = 2;

/// Per logical CPU, idle time so far in ns (`host_processor_info` ticks, 10 ms each).
pub fn idle_cycles(out: &mut Vec<u64>) -> bool {
    out.clear();
    let mut count = 0u32;
    let mut info: *mut i32 = std::ptr::null_mut();
    let mut n_info = 0u32;
    #[allow(deprecated)]
    let host = unsafe { libc::mach_host_self() };
    if unsafe { libc::host_processor_info(host, PROCESSOR_CPU_LOAD_INFO, &mut count, &mut info as *mut *mut i32 as *mut _, &mut n_info) } != 0 || info.is_null() {
        return false;
    }
    let ticks = unsafe { std::slice::from_raw_parts(info, n_info as usize) };
    let hz = (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as u64;
    for c in 0..count as usize {
        // CPU_STATE_USER, SYSTEM, IDLE, NICE
        out.push(ticks.get(c * 4 + 2).copied().unwrap_or(0) as u32 as u64 * 1_000_000_000 / hz);
    }
    #[allow(deprecated)]
    unsafe {
        libc::vm_deallocate(libc::mach_task_self(), info as libc::vm_address_t, (n_info as usize * 4) as libc::vm_size_t)
    };
    !out.is_empty()
}

pub fn system_busy(ms: u64) -> f64 {
    let read = || -> Option<(u64, u64)> {
        let mut v = Vec::new();
        idle_cycles(&mut v).then(|| (v.iter().sum(), now_ns() * v.len() as u64))
    };
    let Some((i0, t0)) = read() else { return 0.0 };
    std::thread::sleep(std::time::Duration::from_millis(ms));
    let Some((i1, t1)) = read() else { return 0.0 };
    let total = t1.saturating_sub(t0) as f64;
    if total <= 0.0 { 0.0 } else { (1.0 - i1.saturating_sub(i0) as f64 / total).clamp(0.0, 1.0) }
}

/// Apple Silicon has no per-cluster L3 to pin to: no cache groups. The core kinds are in `cpu_name`.
pub fn topology() -> Topology {
    Topology { logical: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1), groups: Vec::new() }
}

pub fn os_name() -> String {
    format!("macOS {} ({})", sysctl_string("kern.osproductversion").unwrap_or_default(), sysctl_string("kern.osversion").unwrap_or_default())
}

pub fn cpu_name() -> String {
    let brand = sysctl_string("machdep.cpu.brand_string").unwrap_or_default();
    let levels = sysctl_i64("hw.nperflevels").unwrap_or(1);
    if levels < 2 {
        return brand;
    }
    let kinds: Vec<String> = (0..levels).filter_map(|l| Some(format!("{}{}", sysctl_i64(&format!("hw.perflevel{l}.logicalcpu"))?, if l == 0 { "P" } else { "E" }))).collect();
    format!("{brand} · {}", kinds.join("+"))
}

fn msg_send_isize(class: &str, getter: &str, sel: &str) -> Option<isize> {
    let (c, g, s) = (CString::new(class).ok()?, CString::new(getter).ok()?, CString::new(sel).ok()?);
    unsafe {
        let cls = objc_getClass(c.as_ptr());
        if cls.is_null() {
            return None;
        }
        let get: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
        let obj = get(cls, sel_registerName(g.as_ptr()));
        if obj.is_null() {
            return None;
        }
        let call: extern "C" fn(*mut c_void, *mut c_void) -> isize = std::mem::transmute(objc_msgSend as *const ());
        Some(call(obj, sel_registerName(s.as_ptr())))
    }
}

/// `NSProcessInfo.thermalState`: nominal, fair, serious, critical.
pub fn thermal_state() -> Option<&'static str> {
    msg_send_isize("NSProcessInfo", "processInfo", "thermalState").map(|t| match t {
        0 => "nominal",
        1 => "fair",
        2 => "serious",
        _ => "critical",
    })
}

pub fn low_power_mode() -> Option<bool> {
    msg_send_isize("NSProcessInfo", "processInfo", "isLowPowerModeEnabled").map(|v| v & 0xff != 0)
}

/// macOS has no power plans: Low Power Mode and the thermal state stand in for one.
pub fn power_plan() -> String {
    let mut s = match low_power_mode() {
        Some(true) => "Low Power Mode".to_string(),
        Some(false) => "automatic (Low Power Mode off)".to_string(),
        None => String::new(),
    };
    if let Some(t) = thermal_state().filter(|t| *t != "nominal") {
        s += &format!(" · thermal {t}");
    }
    s
}

pub fn on_battery() -> bool {
    let Some(blob) = Cf::new(unsafe { IOPSCopyPowerSourcesInfo() }) else { return false };
    cf_string(unsafe { IOPSGetProvidingPowerSourceType(blob.0) }).is_some_and(|s| s == "Battery Power")
}

pub fn virtual_machine() -> Option<String> {
    (sysctl_i64("kern.hv_vmm_present") == Some(1)).then(|| sysctl_string("hw.model").unwrap_or_else(|| "virtual machine".into()))
}

/// The assertions `keep_awake` holds: (system, display).
static ASSERTIONS: Mutex<(u32, u32)> = Mutex::new((0, 0));

/// Keeps the machine from idle sleep when `system`, and the display on as well when `display`, through
/// power assertions (released when false), as `caffeinate` does.
pub fn keep_awake(system: bool, display: bool) {
    let mut held = ASSERTIONS.lock().unwrap_or_else(|e| e.into_inner());
    let set = |slot: &mut u32, want: bool, kind: &str| {
        if want && *slot == 0 {
            let mut id = 0u32;
            if unsafe { IOPMAssertionCreateWithName(cfstr(kind).0, 255, cfstr("strobe: a measurement is running").0, &mut id) } == 0 {
                *slot = id;
            }
        } else if !want && *slot != 0 {
            unsafe { IOPMAssertionRelease(*slot) };
            *slot = 0;
        }
    };
    let (mut s, mut d) = *held;
    set(&mut s, system, "PreventUserIdleSystemSleep");
    set(&mut d, display, "PreventUserIdleDisplaySleep");
    *held = (s, d);
}

pub fn exe_of(pid: u32) -> Option<PathBuf> {
    let mut buf = vec![0u8; 4096];
    let n = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    (n > 0).then(|| PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).to_string()))
}

/// A process's environment (`NAME=value`), from `KERN_PROCARGS2`: argc, the executable path, NUL
/// padding, argc arguments, then the environment up to an empty string, then the kernel's own
/// strings (applev), which are left out. Arguments are never read as environment. None for a
/// process of another user. One buffer of `KERN_ARGMAX` bytes per thread, reused.
pub fn env_of(pid: u32) -> Option<Vec<String>> {
    thread_local! {
        static BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        if buf.is_empty() {
            let mut max: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>();
            let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
            if unsafe { libc::sysctl(mib.as_mut_ptr(), 2, &mut max as *mut _ as *mut c_void, &mut len, std::ptr::null_mut(), 0) } != 0 || max <= 0 {
                return None;
            }
            buf.resize(max as usize, 0);
        }
        let mut len = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr() as *mut c_void, &mut len, std::ptr::null_mut(), 0) } != 0 || len < 4 {
            return None;
        }
        Some(procargs_env(&buf[..len]))
    })
}

/// The environment strings of a `KERN_PROCARGS2` buffer (see `env_of`).
fn procargs_env(b: &[u8]) -> Vec<String> {
    let argc = i32::from_ne_bytes([b[0], b[1], b[2], b[3]]).max(0) as usize;
    let mut at = 4;
    // The executable path, then the padding after it.
    at += b[at..].iter().position(|c| *c == 0).unwrap_or(b.len() - at);
    at += b[at..].iter().position(|c| *c != 0).unwrap_or(b.len() - at);
    let next = |at: &mut usize| -> Option<&[u8]> {
        if *at >= b.len() {
            return None;
        }
        let end = *at + b[*at..].iter().position(|c| *c == 0).unwrap_or(b.len() - *at);
        let s = &b[*at..end];
        *at = end + 1;
        Some(s)
    };
    for _ in 0..argc {
        if next(&mut at).is_none() {
            return Vec::new();
        }
    }
    let mut env = Vec::new();
    while let Some(s) = next(&mut at).filter(|s| !s.is_empty()) {
        env.push(String::from_utf8_lossy(s).to_string());
    }
    env
}

/// When a process started (µs since the epoch), which with its pid names it uniquely: a reused pid
/// starts later.
pub fn start_time(pid: u32) -> Option<u64> {
    bsd(pid).map(|b| b.start)
}

pub fn running(exe: &Path) -> Vec<u32> {
    let want = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    all_pids().into_iter().filter(|&p| exe_of(p).is_some_and(|e| e == want)).collect()
}

pub fn running_under(dir: &Path) -> Vec<u32> {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    all_pids().into_iter().filter(|&p| p != std::process::id() && exe_of(p).is_some_and(|e| e.starts_with(&dir))).collect()
}

/// The console session shows the lock screen (`CGSSessionScreenIsLocked`).
pub fn session_locked() -> bool {
    let Some(d) = Cf::new(unsafe { CGSessionCopyCurrentDictionary() }) else { return false };
    cf_i64(cf_get(d.0, "CGSSessionScreenIsLocked")) == Some(1)
}

/// The main display is on (not asleep).
pub fn display_on() -> bool {
    unsafe { CGDisplayIsAsleep(CGMainDisplayID()) == 0 }
}

/// The display at a point (global display coordinates, points), or the only one.
fn display_at(x: f64, y: f64) -> Option<u32> {
    let mut ids = [0u32; 8];
    let mut n = 0u32;
    if unsafe { CGGetDisplaysWithPoint(CGPoint { x, y }, 8, ids.as_mut_ptr(), &mut n) } == 0 && n > 0 {
        return Some(ids[0]);
    }
    unsafe { CGGetActiveDisplayList(8, ids.as_mut_ptr(), &mut n) };
    (n == 1).then_some(ids[0])
}

pub fn refresh_at(x: i32, y: i32) -> Option<u32> {
    display_hz(display_at(x as f64, y as f64)?).map(|hz| hz.round() as u32).filter(|&hz| hz > 1)
}

pub fn display_hz(id: u32) -> Option<f64> {
    let mode = unsafe { CGDisplayCopyDisplayMode(id) };
    if mode.is_null() {
        return None;
    }
    let hz = unsafe { CGDisplayModeGetRefreshRate(mode) };
    unsafe { CGDisplayModeRelease(mode) };
    (hz > 1.0).then_some(hz)
}

/// The display a window shows on (the one holding most of it): its id and refresh.
pub fn display_of(id: isize) -> Option<super::DisplayPath> {
    let w = windows(LIST_INCLUDING, id as u32).into_iter().find(|w| w.id == id as u32)?;
    let mut ids = [0u32; 8];
    let mut n = 0u32;
    unsafe { CGGetDisplaysWithRect(w.bounds, 8, ids.as_mut_ptr(), &mut n) };
    let display = ids[..n as usize]
        .iter()
        .copied()
        .max_by(|a, b| overlap(unsafe { CGDisplayBounds(*a) }, w.bounds).total_cmp(&overlap(unsafe { CGDisplayBounds(*b) }, w.bounds)))
        .or_else(|| display_at(w.bounds.origin.x, w.bounds.origin.y))?;
    let builtin = unsafe { CGDisplayIsBuiltin(display) } != 0;
    Some(super::DisplayPath { device: format!("display {display}{}", if builtin { " (built-in)" } else { "" }), target: display, adapter_luid: 0, hz: display_hz(display).unwrap_or(0.0) })
}

fn overlap(a: CGRect, b: CGRect) -> f64 {
    let w = (a.origin.x + a.size.width).min(b.origin.x + b.size.width) - a.origin.x.max(b.origin.x);
    let h = (a.origin.y + a.size.height).min(b.origin.y + b.size.height) - a.origin.y.max(b.origin.y);
    w.max(0.0) * h.max(0.0)
}

/// Pixels per point of the display at a point.
fn scale_at(p: CGPoint) -> f64 {
    let Some(id) = display_at(p.x, p.y).or_else(|| Some(unsafe { CGMainDisplayID() })) else { return 1.0 };
    let bounds = unsafe { CGDisplayBounds(id) };
    let mode = unsafe { CGDisplayCopyDisplayMode(id) };
    if mode.is_null() || bounds.size.width <= 0.0 {
        return 1.0;
    }
    let px = unsafe { CGDisplayModeGetPixelWidth(mode) } as f64;
    unsafe { CGDisplayModeRelease(mode) };
    (px / bounds.size.width).max(1.0)
}

/// Every active display: (id, refresh, built in).
pub fn displays() -> Vec<(u32, f64, bool)> {
    let mut ids = [0u32; 16];
    let mut n = 0u32;
    unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n) };
    ids[..n as usize].iter().map(|&id| (id, display_hz(id).unwrap_or(0.0), unsafe { CGDisplayIsBuiltin(id) } != 0)).collect()
}

/// One CGWindowList entry.
struct Win {
    id: u32,
    pid: u32,
    layer: i64,
    bounds: CGRect,
    onscreen: bool,
    alpha: f64,
    owner: String,
    /// `kCGWindowSharingState`: 0 when the app excludes the window from screen capture
    /// (`NSWindowSharingNone`), 1 or 2 when other apps may read it.
    sharing: i64,
}

const LIST_ALL: u32 = 0;
const LIST_ON_SCREEN: u32 = 1;
const LIST_INCLUDING: u32 = 8;
const EXCLUDE_DESKTOP: u32 = 16;

fn windows(option: u32, relative: u32) -> Vec<Win> {
    let Some(list) = Cf::new(unsafe { CGWindowListCopyWindowInfo(option, relative) }) else { return Vec::new() };
    cf_array(list.0)
        .into_iter()
        .filter_map(|d| {
            let mut bounds = CGRect::default();
            unsafe { CGRectMakeWithDictionaryRepresentation(cf_get(d, "kCGWindowBounds"), &mut bounds) };
            Some(Win {
                id: cf_i64(cf_get(d, "kCGWindowNumber"))? as u32,
                pid: cf_i64(cf_get(d, "kCGWindowOwnerPID"))? as u32,
                layer: cf_i64(cf_get(d, "kCGWindowLayer")).unwrap_or(0),
                bounds,
                onscreen: cf_i64(cf_get(d, "kCGWindowIsOnscreen")) == Some(1),
                alpha: cf_f64(cf_get(d, "kCGWindowAlpha")).unwrap_or(1.0),
                owner: cf_string(cf_get(d, "kCGWindowOwnerName")).unwrap_or_default(),
                sharing: cf_i64(cf_get(d, "kCGWindowSharingState")).unwrap_or(1),
            })
        })
        .collect()
}

/// The share of `w` that other apps' normal windows in front of it cover (a 16×16 grid of points).
fn covered(w: &Win) -> f64 {
    let front: Vec<Win> = windows(LIST_ON_SCREEN | EXCLUDE_DESKTOP, 0).into_iter().take_while(|o| o.id != w.id).filter(|o| o.layer == 0 && o.alpha > 0.0 && o.pid != w.pid).collect();
    if front.is_empty() {
        return 0.0;
    }
    let b = w.bounds;
    let mut hit = 0;
    for i in 0..16 {
        for j in 0..16 {
            let (x, y) = (b.origin.x + b.size.width * (i as f64 + 0.5) / 16.0, b.origin.y + b.size.height * (j as f64 + 0.5) / 16.0);
            if front.iter().any(|o| x >= o.bounds.origin.x && x < o.bounds.origin.x + o.bounds.size.width && y >= o.bounds.origin.y && y < o.bounds.origin.y + o.bounds.size.height) {
                hit += 1;
            }
        }
    }
    hit as f64 / 256.0
}

/// The apps whose normal windows in front of window `id` overlap it, front first (kCGWindowOwnerName
/// needs no permission): what keeps a covered window covered.
pub fn covering(id: isize) -> Vec<String> {
    let all = windows(LIST_ON_SCREEN | EXCLUDE_DESKTOP, 0);
    let Some(w) = all.iter().find(|w| w.id == id as u32) else { return Vec::new() };
    let b = w.bounds;
    let mut out: Vec<String> = Vec::new();
    for o in all.iter().take_while(|o| o.id != w.id).filter(|o| o.layer == 0 && o.alpha > 0.0 && o.pid != w.pid) {
        let ob = o.bounds;
        let overlaps = ob.origin.x < b.origin.x + b.size.width && b.origin.x < ob.origin.x + ob.size.width && ob.origin.y < b.origin.y + b.size.height && b.origin.y < ob.origin.y + ob.size.height;
        let name = if o.owner.is_empty() { format!("process {}", o.pid) } else { o.owner.clone() };
        if overlaps && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Rects are in points (global display coordinates), not pixels; `dpi` is 96 × the backing scale of
/// the display the window is on. The client size is in physical pixels, as on the other OSes (it was
/// points, so a 980 × 760 point window on a Retina display read as 490 × 380 once divided by its
/// scale), and is the whole frame: CoreGraphics does not separate the title bar.
fn top_window(w: &Win) -> super::TopWindow {
    let b = w.bounds;
    let scale = scale_at(CGPoint { x: b.origin.x + b.size.width / 2.0, y: b.origin.y + b.size.height / 2.0 });
    super::TopWindow {
        hwnd: w.id as isize,
        pid: w.pid,
        rect: (b.origin.x as i32, b.origin.y as i32, b.size.width as i32, b.size.height as i32),
        client: ((b.size.width * scale).round() as i32, (b.size.height * scale).round() as i32),
        visible: w.onscreen && w.alpha > 0.0,
        // Without the Accessibility permission a minimized window and one on another Space look alike: off screen.
        iconic: !w.onscreen,
        // Fully hidden behind other apps' windows: a person cannot see it, and the app may stop drawing.
        cloaked: w.onscreen && covered(w) >= 0.99,
        dpi: (96.0 * scale).round() as u32,
    }
}

/// The largest normal window of `pids` (over 100×100 points), on screen first.
pub fn main_window(pids: &[u32]) -> Option<super::TopWindow> {
    main_win(pids).as_ref().map(top_window)
}

fn main_win(pids: &[u32]) -> Option<Win> {
    windows(LIST_ALL | EXCLUDE_DESKTOP, 0)
        .into_iter()
        .filter(|w| pids.contains(&w.pid) && w.layer == 0 && w.bounds.size.width * w.bounds.size.height > 10_000.0)
        .max_by(|a, b| (a.onscreen, a.bounds.size.width * a.bounds.size.height).partial_cmp(&(b.onscreen, b.bounds.size.width * b.bounds.size.height)).unwrap_or(std::cmp::Ordering::Equal))
}

/// Seconds since the last keyboard, mouse or trackpad input from the hardware. The HID system's state, not
/// the session's: the clicks strobe posts to the session count there, and must not read as a person.
pub fn user_idle_s() -> f64 {
    unsafe { CGEventSourceSecondsSinceLastEventType(1, u32::MAX) }
}

/// How long nobody may have touched this Mac before strobe brings a covered app to the front.
pub const IDLE_BEFORE_ACTIVATE_S: f64 = 60.0;

pub fn window_state(id: isize) -> Option<super::TopWindow> {
    windows(LIST_INCLUDING, id as u32).into_iter().find(|w| w.id == id as u32).map(|w| top_window(&w))
}

/// Accessibility permission: needed to move another app's window or read its elements.
pub fn accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

/// The app macOS asks permissions of for strobe: the ancestor launchd started (Terminal, an agent's
/// app, a remote shell's daemon), as its bundle when it is in one. macOS attributes a process's
/// Accessibility and Screen Recording to that responsible process, not to strobe itself.
pub fn responsible_app() -> String {
    let all = all_bsd();
    let mut pid = std::process::id();
    for _ in 0..64 {
        let Some(p) = all.iter().find(|b| b.pid == pid) else { break };
        if p.ppid <= 1 {
            break;
        }
        pid = p.ppid;
    }
    let exe = exe_of(pid).map(|p| p.display().to_string()).unwrap_or_else(|| format!("process {pid}"));
    match exe.find(".app/") {
        Some(i) => exe[..i + 4].to_string(),
        None => exe,
    }
}

/// Why a process (0: strobe itself) runs at background priority, if it does (`taskpolicy -b`, or a
/// parent that set it): what strobe launches inherits it, and macOS then keeps it on the efficiency
/// cores with its GPU and IO throttled.
pub fn background_qos(pid: u32) -> Option<String> {
    // `getpriority` answers for strobe itself; for another process it read 0 under `taskpolicy -b`
    // (air, macOS 27), while its base priority was the background band's 4 (`ps -o pri`).
    let bg = unsafe { libc::getpriority(libc::PRIO_DARWIN_PROCESS, pid as libc::id_t) } > 0 || (pid != 0 && task_info(pid).is_some_and(|t| (1..=4).contains(&t.pti_priority)));
    bg.then(|| "background priority (taskpolicy -b, or a parent's)".to_string())
}

/// Whether macOS coalesces strobe's timers: a launchd job of ProcessType Standard or Background (a
/// remote shell's or an agent's daemon, when its plist sets no ProcessType) and everything it starts
/// wake from a 16 ms sleep 20 to 60 ms late (air, macOS 27; ProcessType Interactive: 1 to 4 ms), with
/// no thread QoS, role or activity that strobe may set changing it. What strobe launches inherits
/// it. Returns the median wake delay (ms) of a few short sleeps and the job's program, when late.
pub fn timer_coalescing() -> Option<(f64, String)> {
    let mut late: Vec<f64> = (0..7)
        .map(|_| {
            let t = std::time::Instant::now();
            std::thread::sleep(std::time::Duration::from_millis(5));
            t.elapsed().as_secs_f64() * 1e3 - 5.0
        })
        .collect();
    late.sort_by(f64::total_cmp);
    let median = late[late.len() / 2];
    (median > 5.0).then(|| (median, responsible_app()))
}

fn major_version() -> u32 {
    sysctl_string("kern.osproductversion").and_then(|v| v.split('.').next().and_then(|m| m.trim().parse().ok())).unwrap_or(0)
}

/// Where System Settings lists the accessibility permission, with the URL that opens it: macOS 27
/// renamed the pane (the permission and its URL kept their names).
pub fn accessibility_pane() -> String {
    let name = if major_version() >= 27 { "Device Control and Data Access" } else { "Accessibility" };
    format!("System Settings › Privacy & Security › {name} (open \"x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility\")")
}

/// Where System Settings lists the Screen Recording permission, with the URL that opens it.
pub fn screen_recording_pane() -> &'static str {
    "System Settings › Privacy & Security › Screen & System Audio Recording (open \"x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture\")"
}

/// Who to grant Screen Recording: the responsible app, and strobe itself by path when it runs
/// under a daemon or agent (macOS has been seen to pass Accessibility on to such a child but to
/// check Screen Recording against the child's own binary).
pub fn screen_recording_grantee() -> String {
    let app = responsible_app();
    let own = own_exe();
    match tcc_split() {
        Some(_) => format!("strobe itself at {own} (macOS passes Device Control on from {app} to what it starts, but checks Screen Recording against strobe's own binary)"),
        None if own.is_empty() || app == own => app,
        None => format!("{app} (the app strobe runs under), or strobe itself at {own} if granting that app does not help"),
    }
}

/// strobe's own executable, as System Settings lists it (symlinks resolved).
pub fn own_exe() -> String {
    std::env::current_exe().ok().map(|p| std::fs::canonicalize(&p).unwrap_or(p).display().to_string()).unwrap_or_default()
}

/// The inherited-versus-own split: strobe runs under another process (a daemon such as a remote
/// shell's, an agent's app), Device Control came with it, and Screen Recording did not. On macOS 27 a
/// launchd daemon's child is AX-trusted through its parent, while ScreenCaptureKit answers -3801
/// ("user declined TCCs") until strobe's own binary is granted. Returns the parent it inherits from.
pub fn tcc_split() -> Option<String> {
    let app = responsible_app();
    (app != own_exe() && accessibility_trusted() && !screen_capture_allowed()).then_some(app)
}

/// Screen Recording permission: needed to read another app's pixels.
pub fn screen_capture_allowed() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() != 0 }
}

/// Moves (and with `client`, sizes) the app's window that matches `w`'s frame, through the
/// accessibility API (Accessibility permission). macOS lets no app keep another's window on top
/// without activating it: strobe checks occlusion instead (`cloaked`).
pub fn place_window(w: &super::TopWindow, x: i32, y: i32, client: Option<(u32, u32)>) {
    if !accessibility_trusted() {
        return;
    }
    let Some(app) = Cf::new(unsafe { AXUIElementCreateApplication(w.pid as i32) }) else { return };
    let mut list: CFTypeRef = std::ptr::null();
    if unsafe { AXUIElementCopyAttributeValue(app.0, cfstr("AXWindows").0, &mut list) } != 0 {
        return;
    }
    let list = Cf(list);
    for win in cf_array(list.0) {
        let (mut pos, mut size) = (CGPoint::default(), CGSize::default());
        let mut v: CFTypeRef = std::ptr::null();
        if unsafe { AXUIElementCopyAttributeValue(win, cfstr("AXPosition").0, &mut v) } == 0 {
            let v = Cf(v);
            unsafe { AXValueGetValue(v.0, 1, &mut pos as *mut _ as *mut c_void) };
        }
        let mut v: CFTypeRef = std::ptr::null();
        if unsafe { AXUIElementCopyAttributeValue(win, cfstr("AXSize").0, &mut v) } == 0 {
            let v = Cf(v);
            unsafe { AXValueGetValue(v.0, 2, &mut size as *mut _ as *mut c_void) };
        }
        if (pos.x as i32, pos.y as i32, size.width as i32, size.height as i32) != w.rect {
            continue;
        }
        let p = CGPoint { x: x as f64, y: y as f64 };
        let pv = Cf(unsafe { AXValueCreate(1, &p as *const _ as *const c_void) });
        unsafe { AXUIElementSetAttributeValue(win, cfstr("AXPosition").0, pv.0) };
        if let Some((cw, ch)) = client {
            let s = CGSize { width: cw as f64, height: ch as f64 };
            let sv = Cf(unsafe { AXValueCreate(2, &s as *const _ as *const c_void) });
            unsafe { AXUIElementSetAttributeValue(win, cfstr("AXSize").0, sv.0) };
        }
        return;
    }
}

/// The largest window of `pids` shrunk to 16×16 averaged BGRA cells, from `CGWindowListCreateImage`
/// (looked up at run time: newer SDKs mark it obsolete). None without Screen Recording permission:
/// macOS then returns the desktop instead of the window, which is not what a person sees of it.
pub fn window_pixels(pids: &[u32]) -> Option<Vec<u8>> {
    type CreateImage = unsafe extern "C" fn(CGRect, u32, u32, u32) -> CFTypeRef;
    if !screen_capture_allowed() {
        return None;
    }
    let w = main_window(pids)?;
    let sym = CString::new("CGWindowListCreateImage").ok()?;
    let f = unsafe { libc::dlsym(libc::RTLD_DEFAULT, sym.as_ptr()) };
    if f.is_null() {
        return None;
    }
    let create: CreateImage = unsafe { std::mem::transmute(f) };
    // CGRectNull, this window only, its own bounds at nominal resolution (one pixel per point).
    let null = CGRect { origin: CGPoint { x: f64::INFINITY, y: f64::INFINITY }, size: CGSize::default() };
    let img = unsafe { create(null, LIST_INCLUDING, w.hwnd as u32, 1 | 16) };
    if img.is_null() {
        return None;
    }
    let (width, height, stride, bpp) = unsafe { (CGImageGetWidth(img), CGImageGetHeight(img), CGImageGetBytesPerRow(img), CGImageGetBitsPerPixel(img)) };
    let data = Cf::new(unsafe { CGDataProviderCopyData(CGImageGetDataProvider(img)) });
    unsafe { CGImageRelease(img) };
    let data = data?;
    if bpp != 32 || width < 16 || height < 16 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(data.0), CFDataGetLength(data.0) as usize) };
    Some(super::grid16(bytes, width, height, stride))
}

/// The on-screen windows of `pids`, front to back: id, title (CoreGraphics gives titles only with
/// Screen Recording), click-through (unknown here: false).
pub fn top_windows(pids: &[u32]) -> Vec<(isize, String, bool)> {
    let Some(list) = Cf::new(unsafe { CGWindowListCopyWindowInfo(LIST_ON_SCREEN | EXCLUDE_DESKTOP, 0) }) else { return Vec::new() };
    cf_array(list.0)
        .into_iter()
        .filter_map(|d| {
            let pid = cf_i64(cf_get(d, "kCGWindowOwnerPID"))? as u32;
            let layer = cf_i64(cf_get(d, "kCGWindowLayer")).unwrap_or(0);
            (pids.contains(&pid) && (0..1000).contains(&layer) && cf_f64(cf_get(d, "kCGWindowAlpha")).unwrap_or(1.0) > 0.0)
                .then(|| Some((cf_i64(cf_get(d, "kCGWindowNumber"))? as isize, cf_string(cf_get(d, "kCGWindowName")).unwrap_or_default(), false)))?
        })
        .collect()
}

/// The on-screen windows of `pids` (as `top_windows` lists them) that the app excludes from screen
/// capture (`NSWindowSharingNone`): a ScreenCaptureKit stream of one never holds its content, so
/// nothing it draws can be seen.
pub fn unshared_windows(pids: &[u32]) -> Vec<(u32, u32)> {
    windows(LIST_ON_SCREEN | EXCLUDE_DESKTOP, 0).into_iter().filter(|w| pids.contains(&w.pid) && (0..1000).contains(&w.layer) && w.alpha > 0.0 && w.sharing == 0).map(|w| (w.id, w.pid)).collect()
}

/// The frontmost on-screen window of `pids` holding a point (points, global display space).
pub fn window_at(pids: &[u32], x: f64, y: f64) -> Option<u32> {
    windows(LIST_ON_SCREEN | EXCLUDE_DESKTOP, 0)
        .into_iter()
        .find(|w| pids.contains(&w.pid) && w.alpha > 0.0 && x >= w.bounds.origin.x && y >= w.bounds.origin.y && x < w.bounds.origin.x + w.bounds.size.width && y < w.bounds.origin.y + w.bounds.size.height)
        .map(|w| w.id)
}

/// The process's main thread (the lowest thread id: ids only grow), where AppKit runs the UI.
pub fn main_thread(pid: u32) -> Option<u32> {
    let id = thread_ids(pid).into_iter().min()?;
    THREADS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(id as u32, (pid, id));
    Some(id as u32)
}

/// A window's pixels at the display's resolution (`CGWindowListCreateImage`, Screen Recording), as RGBA.
pub fn window_image(id: isize) -> Option<crate::png::Image> {
    type CreateImage = unsafe extern "C" fn(CGRect, u32, u32, u32) -> CFTypeRef;
    if !screen_capture_allowed() {
        return None;
    }
    let sym = CString::new("CGWindowListCreateImage").ok()?;
    let f = unsafe { libc::dlsym(libc::RTLD_DEFAULT, sym.as_ptr()) };
    if f.is_null() {
        return None;
    }
    let create: CreateImage = unsafe { std::mem::transmute(f) };
    let null = CGRect { origin: CGPoint { x: f64::INFINITY, y: f64::INFINITY }, size: CGSize::default() };
    // This window only, its own bounds without the shadow, at the best resolution.
    let img = unsafe { create(null, LIST_INCLUDING, id as u32, 1 | 8) };
    if img.is_null() {
        return None;
    }
    let (width, height, stride, bpp) = unsafe { (CGImageGetWidth(img), CGImageGetHeight(img), CGImageGetBytesPerRow(img), CGImageGetBitsPerPixel(img)) };
    let data = Cf::new(unsafe { CGDataProviderCopyData(CGImageGetDataProvider(img)) });
    unsafe { CGImageRelease(img) };
    let data = data?;
    if bpp != 32 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(data.0), CFDataGetLength(data.0) as usize) };
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for px in bytes.get(y * stride..y * stride + width * 4)?.chunks(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    Some(crate::png::Image { width, height, rgba })
}

/// macOS lets no app put another app's window on top without activating it, which takes the keyboard
/// focus. So strobe makes the app the active one in every block (`check_visible` requires it), and
/// only once nobody has used this Mac for a minute: a person at the keyboard keeps their focus, and
/// the block waits for them. Activating every time, not only when other windows cover the app's,
/// keeps every block in one state: an app's key window can cost several times an inactive one's
/// (a Rune window at rest: 3× its CPU). Returns 1 when the app is active.
pub fn raise_windows(pids: &[u32]) -> usize {
    let Some(w) = main_win(pids) else { return 0 };
    if !w.onscreen {
        return 0;
    }
    if crate::target::a11y::ax::is_front(w.pid) {
        return 1;
    }
    if user_idle_s() < IDLE_BEFORE_ACTIVATE_S || !accessibility_trusted() {
        return 0;
    }
    usize::from(crate::target::a11y::ax::activate(w.pid).is_ok())
}