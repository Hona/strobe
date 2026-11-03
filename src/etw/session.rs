//! A real-time ETW session: start it, enable providers, consume its events on one thread, stop it.
//! Plus `Schema`, which reads named fields of manifest events through TDH's layout, cached per
//! event, the way PresentMon reads them (offsets depend on the event's version and pointer size).

use crate::model::{Error, Result};
use std::collections::HashMap;
use std::mem::size_of;
use windows_sys::Win32::System::Diagnostics::Etw::*;
use windows_sys::core::GUID;

const REAL_TIME: u32 = 0x100;
const SYSTEM_LOGGER: u32 = 0x0200_0000;
const TRACED_GUID: u32 = 0x0002_0000;
const PROCESS_REAL_TIME: u32 = 0x100;
const PROCESS_RAW_TIMESTAMP: u32 = 0x1000;
const PROCESS_EVENT_RECORD: u32 = 0x1000_0000;
const CONTROL_QUERY: u32 = 0;
const CONTROL_STOP: u32 = 1;
const ENABLE: u32 = 1;
const FILTER_EVENT_ID: u32 = 0x8000_0200;
const IGNORE_KEYWORD_0: u32 = 0x10;
const ALREADY_EXISTS: u32 = 183;
const NAME_BYTES: usize = 1024;

pub fn guid(text: &str) -> GUID {
    let h: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let n = |a: usize, b: usize| u64::from_str_radix(&h[a..b], 16).unwrap_or(0);
    let mut d4 = [0u8; 8];
    for (i, b) in d4.iter_mut().enumerate() {
        *b = n(16 + i * 2, 18 + i * 2) as u8;
    }
    GUID { data1: n(0, 8) as u32, data2: n(8, 12) as u16, data3: n(12, 16) as u16, data4: d4 }
}

pub fn same(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

/// One provider to enable: events by ID (filtered in the provider, so others cost nothing), with
/// the keywords and level those events need.
pub struct Provider {
    pub guid: GUID,
    pub level: u8,
    pub any: u64,
    pub all: u64,
    pub ids: Vec<u16>,
}

pub trait Handler {
    type Out: Send + 'static;
    fn event(&mut self, e: &EVENT_RECORD);
    fn finish(self) -> Self::Out;
}

/// A running session; `stop` ends it and returns what its handler made of the events.
pub struct Running<O> {
    handle: CONTROLTRACE_HANDLE,
    name: Vec<u16>,
    thread: Option<std::thread::JoinHandle<(O, u64)>>,
    tsc0: Option<u64>,
    ns0: u64,
    pub events: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub events_lost: u64,
    pub buffers_lost: u64,
    /// The consumer thread's CPU while it ran (cycle-exact), when the TSC rate is known.
    pub consumer_cpu_ms: Option<f64>,
    pub events: u64,
}

fn props(name: &[u16]) -> Vec<u64> {
    let size = size_of::<EVENT_TRACE_PROPERTIES>() + NAME_BYTES * 2;
    let mut buf = vec![0u64; size.div_ceil(8)];
    let p = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
    unsafe {
        (*p).Wnode.BufferSize = size as u32;
        (*p).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        (*p).LogFileNameOffset = 0;
        let at = (buf.as_mut_ptr() as *mut u8).add(size_of::<EVENT_TRACE_PROPERTIES>()) as *mut u16;
        std::ptr::copy_nonoverlapping(name.as_ptr(), at, name.len().min(NAME_BYTES / 2));
    }
    buf
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Stops a session by name (one a crashed strobe left behind).
pub fn stop_named(name: &str) -> bool {
    let w = wide(name);
    let mut p = props(&w);
    unsafe { ControlTraceW(CONTROLTRACE_HANDLE { Value: 0 }, w.as_ptr(), p.as_mut_ptr() as *mut _, CONTROL_STOP) == 0 }
}

/// Names of every running session.
pub fn sessions() -> Vec<String> {
    const N: usize = 64;
    let mut bufs: Vec<Vec<u64>> = (0..N).map(|_| props(&[0])).collect();
    // QueryAllTraces fills the names; the log file name offset must point at room too.
    for b in &mut bufs {
        let size = size_of::<EVENT_TRACE_PROPERTIES>() + NAME_BYTES * 2;
        b.resize((size + NAME_BYTES * 2).div_ceil(8), 0);
        let p = b.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        unsafe {
            (*p).Wnode.BufferSize = (size + NAME_BYTES * 2) as u32;
            (*p).LogFileNameOffset = size as u32;
        }
    }
    let mut ptrs: Vec<*mut EVENT_TRACE_PROPERTIES> = bufs.iter_mut().map(|b| b.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES).collect();
    let mut count = 0u32;
    if unsafe { QueryAllTracesW(ptrs.as_mut_ptr(), N as u32, &mut count) } != 0 {
        return Vec::new();
    }
    ptrs.iter()
        .take(count as usize)
        .map(|&p| unsafe {
            let at = (p as *const u8).add((*p).LoggerNameOffset as usize) as *const u16;
            let mut n = 0;
            while n < NAME_BYTES / 2 && *at.add(n) != 0 {
                n += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(at, n))
        })
        .collect()
}

/// Stops `strobe-<kind>-<pid>` sessions whose process is gone (a crash or a kill mid-recording).
pub fn reap() {
    for name in sessions() {
        let Some(pid) = name.strip_prefix("strobe-").and_then(|r| r.rsplit('-').next()).and_then(|p| p.parse::<u32>().ok()) else { continue };
        if crate::host::win::exe_of(pid).is_none() {
            stop_named(&name);
        }
    }
}

pub struct Config {
    /// `strobe-<kind>-<pid>`.
    pub kind: &'static str,
    /// Kernel flags (`EVENT_TRACE_FLAG_*`): a system logger session when nonzero.
    pub kernel: u32,
    pub providers: Vec<Provider>,
    pub buffer_kb: u32,
    pub buffers: (u32, u32),
}

impl<O: Send + 'static> Running<O> {
    /// Starts the session, connects the consumer (a thread that owns the handler `make` builds),
    /// calls `setup` with the session handle (stack walking, the profile interval), then enables
    /// the providers.
    pub fn start<H, F>(cfg: Config, setup: impl FnOnce(CONTROLTRACE_HANDLE) -> Result<()>, make: F) -> Result<Running<O>>
    where
        H: Handler<Out = O>,
        F: FnOnce() -> H + Send + 'static,
    {
        reap();
        let name_text = format!("strobe-{}-{}", cfg.kind, std::process::id());
        let name = wide(&name_text);
        let mut buf = props(&name);
        let p = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        unsafe {
            (*p).Wnode.ClientContext = 1; // QPC
            (*p).Wnode.Flags = TRACED_GUID;
            (*p).LogFileMode = REAL_TIME | if cfg.kernel != 0 { SYSTEM_LOGGER } else { 0 };
            (*p).EnableFlags = cfg.kernel;
            (*p).BufferSize = cfg.buffer_kb;
            (*p).MinimumBuffers = cfg.buffers.0;
            (*p).MaximumBuffers = cfg.buffers.1;
            (*p).FlushTimer = 1;
        }
        let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
        let mut rc = unsafe { StartTraceW(&mut handle, name.as_ptr(), p) };
        if rc == ALREADY_EXISTS {
            stop_named(&name_text);
            buf = props(&name);
            let p = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
            unsafe {
                (*p).Wnode.ClientContext = 1;
                (*p).Wnode.Flags = TRACED_GUID;
                (*p).LogFileMode = REAL_TIME | if cfg.kernel != 0 { SYSTEM_LOGGER } else { 0 };
                (*p).EnableFlags = cfg.kernel;
                (*p).BufferSize = cfg.buffer_kb;
                (*p).MinimumBuffers = cfg.buffers.0;
                (*p).MaximumBuffers = cfg.buffers.1;
                (*p).FlushTimer = 1;
            }
            rc = unsafe { StartTraceW(&mut handle, name.as_ptr(), p) };
        }
        if rc != 0 {
            return Err(Error::msg(format!("start the ETW session: {}", win_error(rc))));
        }
        let events = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (ready, ready_rx) = std::sync::mpsc::channel::<std::result::Result<(), u32>>();
        let consumer_name = name.clone();
        let counter = events.clone();
        let thread = std::thread::Builder::new()
            .name("strobe-etw".into())
            .spawn(move || {
                let k0 = crate::host::thread_cycles();
                struct Ctx<H> {
                    h: H,
                    n: std::sync::Arc<std::sync::atomic::AtomicU64>,
                }
                unsafe extern "system" fn callback<H: Handler>(e: *mut EVENT_RECORD) {
                    unsafe {
                        let ctx = &mut *((*e).UserContext as *mut Ctx<H>);
                        ctx.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        ctx.h.event(&*e);
                    }
                }
                let mut ctx = Box::new(Ctx { h: make(), n: counter });
                let mut log: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
                let mut lname = consumer_name;
                log.LoggerName = lname.as_mut_ptr();
                log.Anonymous1.ProcessTraceMode = PROCESS_REAL_TIME | PROCESS_EVENT_RECORD | PROCESS_RAW_TIMESTAMP;
                log.Anonymous2.EventRecordCallback = Some(callback::<H>);
                log.Context = &mut *ctx as *mut Ctx<H> as *mut _;
                let trace = unsafe { OpenTraceW(&mut log) };
                if trace.Value == u64::MAX {
                    let _ = ready.send(Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32));
                    return (ctx.h.finish(), 0);
                }
                let _ = ready.send(Ok(()));
                unsafe {
                    ProcessTrace(&trace, 1, std::ptr::null(), std::ptr::null());
                    CloseTrace(trace);
                }
                let spent = crate::host::thread_cycles().saturating_sub(k0);
                (ctx.h.finish(), spent)
            })
            .map_err(|e| Error::msg(format!("spawn the ETW consumer: {e}")))?;
        let mut running = Running { handle, name, thread: Some(thread), tsc0: crate::host::tsc(), ns0: crate::host::now_ns(), events };
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(rc)) => {
                running.stop_session();
                return Err(Error::msg(format!("open the ETW session: {}", win_error(rc))));
            }
            Err(_) => {
                running.stop_session();
                return Err(Error::msg("the ETW consumer stopped"));
            }
        }
        if let Err(e) = setup(handle) {
            running.stop_session();
            return Err(e);
        }
        for pr in &cfg.providers {
            let rc = enable(handle, pr);
            if rc != 0 {
                running.stop_session();
                return Err(Error::msg(format!("enable an ETW provider ({:08x}): {}", pr.guid.data1, win_error(rc))));
            }
        }
        Ok(running)
    }

    fn stop_session(&mut self) -> Stats {
        let mut p = props(&self.name);
        let ptr = p.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        unsafe {
            ControlTraceW(self.handle, std::ptr::null(), ptr, CONTROL_QUERY);
        }
        let lost = unsafe { ((*ptr).EventsLost as u64, ((*ptr).LogBuffersLost + (*ptr).RealTimeBuffersLost) as u64) };
        let mut p = props(&self.name);
        unsafe {
            ControlTraceW(self.handle, std::ptr::null(), p.as_mut_ptr() as *mut _, CONTROL_STOP);
        }
        Stats { events_lost: lost.0, buffers_lost: lost.1, ..Stats::default() }
    }

    /// Stops the session; the consumer delivers what the buffers still hold, then returns.
    pub fn stop(mut self) -> Result<(O, Stats)> {
        let mut stats = self.stop_session();
        let (out, cycles) = self.thread.take().and_then(|t| t.join().ok()).ok_or_else(|| Error::msg("the ETW consumer panicked"))?;
        let hz = self.tsc0.zip(crate::host::tsc()).map(|(a, b)| (b - a) as f64 / ((crate::host::now_ns() - self.ns0) as f64 / 1e9)).filter(|hz| *hz > 0.0);
        stats.consumer_cpu_ms = hz.filter(|_| cycles > 0).map(|hz| cycles as f64 / hz * 1e3);
        stats.events = self.events.load(std::sync::atomic::Ordering::Relaxed);
        Ok((out, stats))
    }
}

impl<O> Drop for Running<O> {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let mut p = props(&self.name);
            unsafe {
                ControlTraceW(self.handle, std::ptr::null(), p.as_mut_ptr() as *mut _, CONTROL_STOP);
            }
        }
    }
}

fn enable(handle: CONTROLTRACE_HANDLE, pr: &Provider) -> u32 {
    // EVENT_FILTER_EVENT_ID: FilterIn (u8), Reserved (u8), Count (u16), Events[Count] (u16).
    let mut filter: Vec<u16> = vec![1, 0];
    filter[0] = 1; // FilterIn = TRUE, Reserved = 0 (little-endian u8 pair)
    filter[1] = pr.ids.len() as u16;
    filter.extend(&pr.ids);
    let mut desc = EVENT_FILTER_DESCRIPTOR { Ptr: filter.as_ptr() as u64, Size: (filter.len() * 2) as u32, Type: FILTER_EVENT_ID };
    let params = ENABLE_TRACE_PARAMETERS {
        Version: 2,
        EnableProperty: IGNORE_KEYWORD_0,
        ControlFlags: 0,
        SourceId: GUID { data1: 0, data2: 0, data3: 0, data4: [0; 8] },
        EnableFilterDesc: &mut desc,
        FilterDescCount: if pr.ids.is_empty() { 0 } else { 1 },
    };
    unsafe { EnableTraceEx2(handle, &pr.guid, ENABLE, pr.level, pr.any, pr.all, 0, &params) }
}

pub fn win_error(rc: u32) -> String {
    match rc {
        5 => "access denied (needs admin or Performance Log Users)".into(),
        1314 => "a privilege is missing (sampled profiles need administrators)".into(),
        183 => "a session of that name already runs".into(),
        _ => format!("error {rc}: {}", std::io::Error::from_raw_os_error(rc as i32)),
    }
}

/// Field layouts of manifest events, from TDH once per (provider, id, version).
#[derive(Default)]
pub struct Schema {
    cache: HashMap<(u128, u16, u8), Option<Vec<Prop>>>,
    /// (provider, id, version, call site, 64-bit) → field offsets, for fixed layouts.
    fixed: HashMap<(u128, u16, u8, usize, bool), Vec<Field>>,
}

struct Prop {
    name: String,
    flags: i32,
    in_type: u16,
    length: u16,
    count: u16,
    members: (u16, u16),
}

/// Where one named field sits in an event's payload.
#[derive(Clone, Copy, Debug, Default)]
pub struct Field {
    pub off: usize,
    pub size: usize,
    pub count: usize,
    pub found: bool,
}

fn key(g: &GUID) -> u128 {
    (g.data1 as u128) << 96 | (g.data2 as u128) << 80 | (g.data3 as u128) << 64 | u64::from_be_bytes(g.data4) as u128
}

impl Schema {
    fn layout(&mut self, e: &EVENT_RECORD) -> Option<&Vec<Prop>> {
        let h = &e.EventHeader;
        let k = (key(&h.ProviderId), h.EventDescriptor.Id, h.EventDescriptor.Version);
        self.cache
            .entry(k)
            .or_insert_with(|| unsafe {
                let mut size = 0u32;
                TdhGetEventInformation(e, 0, std::ptr::null(), std::ptr::null_mut(), &mut size);
                if size == 0 {
                    return None;
                }
                let mut buf = vec![0u64; (size as usize).div_ceil(8)];
                let tei = buf.as_mut_ptr() as *mut TRACE_EVENT_INFO;
                if TdhGetEventInformation(e, 0, std::ptr::null(), tei, &mut size) != 0 {
                    return None;
                }
                let base = tei as *const u8;
                let arr = std::ptr::addr_of!((*tei).EventPropertyInfoArray) as *const EVENT_PROPERTY_INFO;
                let n = (*tei).PropertyCount as usize;
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    let p = &*arr.add(i);
                    let at = base.add(p.NameOffset as usize) as *const u16;
                    let mut len = 0;
                    while *at.add(len) != 0 {
                        len += 1;
                    }
                    let name = String::from_utf16_lossy(std::slice::from_raw_parts(at, len));
                    let st = p.Flags & PropertyStruct != 0;
                    out.push(Prop {
                        name,
                        flags: p.Flags,
                        in_type: if st { 0 } else { p.Anonymous1.nonStructType.InType },
                        length: p.Anonymous3.length,
                        count: p.Anonymous2.count,
                        members: if st { (p.Anonymous1.structType.StructStartIndex, p.Anonymous1.structType.NumOfStructMembers) } else { (0, 0) },
                    });
                }
                // Only top-level properties are walked; struct members follow them.
                out.truncate(n.max((*tei).TopLevelPropertyCount as usize));
                let top = (*tei).TopLevelPropertyCount as usize;
                out.iter_mut().skip(top).for_each(|p| p.name.insert(0, '\u{1}'));
                Some(out)
            })
            .as_ref()
    }

    /// Finds `names` in the event's payload. Missing names (another version) stay `found: false`.
    /// Where every field up to the last one asked for has a fixed size, the offsets are cached per
    /// event and call site, so most events cost a hash lookup.
    pub fn fields<const N: usize>(&mut self, e: &EVENT_RECORD, names: [&str; N]) -> [Field; N] {
        let h = &e.EventHeader;
        let ptr64 = h.Flags as u32 & EVENT_HEADER_FLAG_64_BIT_HEADER != 0;
        let site = (key(&h.ProviderId), h.EventDescriptor.Id, h.EventDescriptor.Version, names.first().map_or(0, |n| n.as_ptr() as usize), ptr64);
        if let Some(f) = self.fixed.get(&site)
            && let Ok(out) = <[Field; N]>::try_from(f.as_slice())
        {
            return out;
        }
        let mut out = [Field::default(); N];
        let data = payload(e);
        let Some(props) = self.layout(e) else { return out };
        let mut off = 0usize;
        let mut values: Vec<u64> = Vec::with_capacity(props.len());
        let (mut fixed, mut left) = (true, N);
        for p in props.iter().take_while(|p| !p.name.starts_with('\u{1}')) {
            if left == 0 {
                break;
            }
            fixed &= p.flags & (PropertyParamCount | PropertyParamLength | PropertyStruct) == 0
                && !matches!(p.in_type as i32, TDH_INTYPE_UNICODESTRING | TDH_INTYPE_ANSISTRING | TDH_INTYPE_SID | TDH_INTYPE_WBEMSID)
                && (p.length > 0 || matches!(p.in_type as i32, TDH_INTYPE_POINTER | TDH_INTYPE_SIZET));
            let (size, count) = prop_size(props, p, data, off, ptr64, &values);
            let value = match size {
                1 => data.get(off).map(|b| *b as u64),
                2 => data.get(off..off + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u64),
                4 => data.get(off..off + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()) as u64),
                8 => data.get(off..off + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default())),
                _ => None,
            };
            values.push(value.unwrap_or(0));
            if let Some(i) = names.iter().position(|n| *n == p.name) {
                out[i] = Field { off, size, count, found: off + size * count.max(1) <= data.len() || count == 0 };
                left -= 1;
            }
            off += size * count;
        }
        if fixed && out.iter().all(|f| f.found) {
            self.fixed.insert(site, out.to_vec());
        }
        out
    }
}

/// Size of one element and the element count of property `p` at `off`.
fn prop_size(props: &[Prop], p: &Prop, data: &[u8], off: usize, ptr64: bool, values: &[u64]) -> (usize, usize) {
    let count = if p.flags & PropertyParamCount != 0 { values.get(p.count as usize).copied().unwrap_or(0) as usize } else { (p.count as usize).max(1) };
    if p.flags & PropertyStruct != 0 {
        let (start, n) = p.members;
        let size = (start..start + n).filter_map(|i| props.get(i as usize)).map(|m| m.length as usize * (m.count as usize).max(1)).sum();
        return (size, count);
    }
    let length = if p.flags & PropertyParamLength != 0 { values.get(p.length as usize).copied().unwrap_or(0) as usize } else { p.length as usize };
    let size = match p.in_type as i32 {
        TDH_INTYPE_UNICODESTRING => {
            if length > 0 {
                length * 2
            } else {
                let mut n = 0;
                while off + n + 1 < data.len() && (data[off + n] != 0 || data[off + n + 1] != 0) {
                    n += 2;
                }
                (n + 2).min(data.len().saturating_sub(off))
            }
        }
        TDH_INTYPE_ANSISTRING => {
            if length > 0 {
                length
            } else {
                data.get(off..).and_then(|d| d.iter().position(|b| *b == 0)).map_or(data.len().saturating_sub(off), |n| n + 1)
            }
        }
        TDH_INTYPE_POINTER | TDH_INTYPE_SIZET => {
            if ptr64 {
                8
            } else {
                4
            }
        }
        TDH_INTYPE_SID => data.get(off + 1).map_or(0, |n| 8 + 4 * *n as usize),
        TDH_INTYPE_WBEMSID => {
            let ptr = if ptr64 { 16 } else { 8 };
            data.get(off + ptr + 1).map_or(0, |n| ptr + 8 + 4 * *n as usize)
        }
        _ => length,
    };
    (size, count)
}

/// Reads a field as an unsigned integer of its own size (4 or 8 bytes; pointers by the event's width).
pub fn uint(e: &EVENT_RECORD, f: &Field) -> u64 {
    uint_at(e, f, 0)
}

/// Element `i` of an array field.
pub fn uint_at(e: &EVENT_RECORD, f: &Field, i: usize) -> u64 {
    if !f.found {
        return 0;
    }
    let data = payload(e);
    let at = f.off + i * f.size;
    match f.size {
        1 => data.get(at).map_or(0, |b| *b as u64),
        2 => data.get(at..at + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as u64),
        4 => data.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap_or_default()) as u64),
        8 => data.get(at..at + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or_default())),
        _ => 0,
    }
}

/// Raw payload bytes of an event (kernel events have fixed layouts and no manifest).
pub fn payload(e: &EVENT_RECORD) -> &[u8] {
    if e.UserData.is_null() {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(e.UserData as *const u8, e.UserDataLength as usize) }
}

pub fn qpc_freq() -> u64 {
    let mut f = 0i64;
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut f) };
    f.max(1) as u64
}

/// The raw QPC count now, the clock of event time stamps (`ClientContext = 1`, raw time stamps).
pub fn qpc_now() -> u64 {
    let mut c = 0i64;
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut c) };
    c.max(0) as u64
}

/// A raw QPC count in nanoseconds, the way `host::now_ns` converts it.
pub fn ns(qpc: u64, freq: u64) -> u64 {
    ((qpc as u128) * 1_000_000_000 / freq as u128) as u64
}
