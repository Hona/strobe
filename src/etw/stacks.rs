//! Native CPU samples from the kernel's sampled profile (attribution blocks only, SPEC rule 3): a
//! system logger session with the profile interrupt at 5 kHz (200 µs, as for JavaScript, §8.3),
//! a stack walk on every sample, image loads (with the rundown of images already loaded), and
//! process starts so a child of the target counts from its first sample.
//!
//! Needs SeSystemProfilePrivilege, which only administrators hold, so it runs in the helper (§13.4).
//! Samples of other processes are dropped as they arrive; the idle loop never leaves the kernel.

use super::session::{self, Handler, Running};
use super::{Deep, Module};
use crate::model::{Error, Result};
use std::collections::{HashMap, HashSet};
use windows_sys::Win32::System::Diagnostics::Etw::*;
use windows_sys::core::GUID;

const PERFINFO: &str = "ce1dbfb4-137e-4da6-87b0-3f59aa102cbc";
const STACKWALK: &str = "def2fe46-7bd6-4b80-bd94-f57fe20d0ce3";
const IMAGE: &str = "2cb15d1d-5fc1-11d2-abe1-00a0c911f518";
const PROCESS: &str = "3d6fa8d0-fe05-11d0-9dda-00c04fd7ba7c";
const FLAG_PROCESS: u32 = 0x1;
const FLAG_IMAGE_LOAD: u32 = 0x4;
const FLAG_PROFILE: u32 = 0x0100_0000;
const SAMPLED_PROFILE: u8 = 46;
/// 200 µs in the profile interval's 100 ns units.
pub const INTERVAL_100NS: u32 = 2000;
/// Windows' own sampled-profile interval (1 ms), for when the current one cannot be read.
const DEFAULT_100NS: u32 = 10_000;

/// A sampled-stack session. Dropped without `stop` (an error, a client gone), its session stops and
/// the profile interval goes back all the same: fields drop in order, the session before the interval.
pub struct Capture {
    running: Running<Deep>,
    _raised: Raised,
    t0: u64,
}

/// The system-wide sampled-profile interval, raised for a capture and put back when this drops.
struct Raised(u32);

impl Raised {
    fn to(v: u32) -> Result<Raised> {
        let old = interval().unwrap_or(DEFAULT_100NS);
        set_interval(v)?;
        Ok(Raised(old))
    }
}

/// Puts the interval back only while it is still strobe's and no other strobe samples at it (that
/// capture's weights assume 200 µs; it puts it back when it ends). An interval read while another
/// strobe had raised it goes back to Windows' own.
impl Drop for Raised {
    fn drop(&mut self) {
        if interval() == Some(INTERVAL_100NS) && !session::sessions().iter().any(|n| n.starts_with("strobe-stacks-")) {
            let _ = set_interval(if self.0 == INTERVAL_100NS { DEFAULT_100NS } else { self.0 });
        }
    }
}

impl Capture {
    /// Samples the process tree under `root` (children started meanwhile included).
    pub fn start(root: u32) -> Result<Capture> {
        enable_privilege("SeSystemProfilePrivilege")?;
        // A helper that died while sampling left its session running and the interval raised: the
        // session is stopped and the interval put back first, so it is not taken for the system's.
        session::reap();
        let raised = Raised::to(INTERVAL_100NS)?;
        let pids: HashSet<u32> = crate::host::tree_pids(root).into_iter().collect();
        if pids.is_empty() {
            return Err(Error::msg(format!("process {root} is not running")));
        }
        let cfg = session::Config { kind: "stacks", kernel: FLAG_PROCESS | FLAG_IMAGE_LOAD | FLAG_PROFILE, providers: Vec::new(), buffer_kb: 1024, buffers: (64, 512), flush_ms: 0 };
        let t0 = crate::host::now_ns();
        let setup = |h: CONTROLTRACE_HANDLE| -> Result<()> {
            let ids = [CLASSIC_EVENT_ID { EventGuid: session::guid(PERFINFO), Type: SAMPLED_PROFILE, Reserved: [0; 7] }];
            let rc = unsafe { TraceSetInformation(h, TraceStackTracingInfo, ids.as_ptr() as *const _, std::mem::size_of_val(&ids) as u32) };
            if rc != 0 {
                return Err(Error::msg(format!("stack walking: {}", session::win_error(rc))));
            }
            Ok(())
        };
        let running = Running::start(cfg, setup, move || Sampler::new(pids))?;
        Ok(Capture { running, _raised: raised, t0 })
    }

    pub fn stop(self) -> Result<Deep> {
        let t1 = crate::host::now_ns();
        let Capture { running, _raised: raised, t0 } = self;
        let result = running.stop();
        drop(raised);
        let (mut deep, stats) = result?;
        deep.t0_ns = t0;
        deep.t1_ns = t1;
        deep.events_lost = stats.events_lost + stats.buffers_lost;
        Ok(deep)
    }
}

/// After a dead strobe's sampled-stack session was stopped and none remains (`session::reap`): the
/// interval it raised goes back to Windows' own, unless something else has changed it since.
pub fn reset_interval() {
    if interval() == Some(INTERVAL_100NS) {
        let _ = set_interval(DEFAULT_100NS);
    }
}

/// The system's sampled-profile interval (100 ns units).
fn interval() -> Option<u32> {
    let mut v = TRACE_PROFILE_INTERVAL { Source: 0, Interval: 0 };
    let mut len = 0u32;
    let rc = unsafe { TraceQueryInformation(CONTROLTRACE_HANDLE { Value: 0 }, TraceSampledProfileIntervalInfo, &mut v as *mut _ as *mut _, std::mem::size_of_val(&v) as u32, &mut len) };
    (rc == 0 && v.Interval > 0).then_some(v.Interval)
}

fn set_interval(v: u32) -> Result<()> {
    let i = TRACE_PROFILE_INTERVAL { Source: 0, Interval: v };
    let rc = unsafe { TraceSetInformation(CONTROLTRACE_HANDLE { Value: 0 }, TraceSampledProfileIntervalInfo, &i as *const _ as *const _, std::mem::size_of_val(&i) as u32) };
    if rc != 0 {
        return Err(Error::msg(format!("set the profile interval: {}", session::win_error(rc))));
    }
    Ok(())
}

pub fn enable_privilege(name: &str) -> Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LUID};
    use windows_sys::Win32::Security::{AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
            return Err(Error::msg("open this process's token"));
        }
        let mut luid = LUID { LowPart: 0, HighPart: 0 };
        let ok = LookupPrivilegeValueW(std::ptr::null(), w.as_ptr(), &mut luid) != 0 && {
            let tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
            AdjustTokenPrivileges(token, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut()) != 0 && GetLastError() == 0
        };
        CloseHandle(token);
        if !ok {
            return Err(Error::msg(format!("{name} is not held: sampled stacks need an administrator token (the deep-mode helper)")));
        }
    }
    Ok(())
}

/// The session's consumer: keeps the samples of `pids` (and their children) and the images they use.
pub struct Sampler {
    guids: [GUID; 3],
    pids: HashSet<u32>,
    /// (stack walk timestamp, thread) → sample index: a sample taken in kernel mode gets its user
    /// part in a second stack walk event when the thread returns to user mode.
    pending: HashMap<(u64, u32), usize>,
    samples: Vec<(u64, u32, u32, u16, Vec<u64>)>,
    images: Vec<Module>,
    seen: HashSet<(u32, u64)>,
    freq: u64,
}

const KERNEL: u64 = 0xFFFF_8000_0000_0000;

impl Sampler {
    pub fn new(pids: HashSet<u32>) -> Sampler {
        Sampler {
            guids: [session::guid(STACKWALK), session::guid(IMAGE), session::guid(PROCESS)],
            pids,
            pending: HashMap::new(),
            samples: Vec::with_capacity(1 << 16),
            images: Vec::new(),
            seen: HashSet::new(),
            freq: session::qpc_freq(),
        }
    }
}

fn u32_at(d: &[u8], at: usize) -> u32 {
    d.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap_or_default()))
}

fn u64_at(d: &[u8], at: usize) -> u64 {
    d.get(at..at + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}

impl Handler for Sampler {
    type Out = Deep;

    fn event(&mut self, e: &EVENT_RECORD) {
        let g = &e.EventHeader.ProviderId;
        let op = e.EventHeader.EventDescriptor.Opcode;
        let d = session::payload(e);
        if session::same(g, &self.guids[0]) && op == 32 {
            let (ts, pid, tid) = (u64_at(d, 0), u32_at(d, 8), u32_at(d, 12));
            if !self.pids.contains(&pid) {
                return;
            }
            let frames: Vec<u64> = d[16.min(d.len())..].as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect();
            match self.pending.get(&(ts, tid)) {
                Some(&i) => {
                    let s = &mut self.samples[i].4;
                    // Kernel frames are innermost: they go first whichever event came first.
                    if frames.first().is_some_and(|f| *f >= KERNEL) && s.first().is_some_and(|f| *f < KERNEL) {
                        let mut merged = frames;
                        merged.append(s);
                        *s = merged;
                    } else {
                        s.extend(frames);
                    }
                }
                None => {
                    let cpu = unsafe { e.BufferContext.Anonymous.ProcessorIndex };
                    self.pending.insert((ts, tid), self.samples.len());
                    self.samples.push((ts, pid, tid, cpu, frames));
                }
            }
        } else if session::same(g, &self.guids[1]) && matches!(op, 10 | 3 | 4) {
            let (base, size, pid) = (u64_at(d, 0), u64_at(d, 8), u32_at(d, 16));
            if (pid != 0 && !self.pids.contains(&pid)) || !self.seen.insert((pid, base)) {
                return;
            }
            let name: Vec<u16> = d.get(56..).unwrap_or(&[]).as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|c| *c != 0).collect();
            self.images.push(Module { pid, base, size, path: String::from_utf16_lossy(&name) });
        } else if session::same(g, &self.guids[2]) && matches!(op, 1 | 3) {
            let (pid, parent) = (u32_at(d, 8), u32_at(d, 12));
            if self.pids.contains(&parent) {
                self.pids.insert(pid);
            }
        }
    }

    fn finish(self) -> Deep {
        let mut index: HashMap<Vec<u64>, u32> = HashMap::new();
        let mut stacks = Vec::new();
        let mut samples = Vec::with_capacity(self.samples.len());
        for (ts, pid, tid, cpu, frames) in self.samples {
            let next = stacks.len() as u32;
            let at = *index.entry(frames.clone()).or_insert_with(|| {
                stacks.push(frames);
                next
            });
            samples.push((session::ns(ts, self.freq), pid, tid, cpu, at));
        }
        let devices = dos_devices();
        let mut modules: Vec<Module> = self.images.into_iter().map(|m| Module { path: dos_path(&m.path, &devices), ..m }).collect();
        if !modules.iter().any(|m| m.pid == 0) {
            modules.extend(drivers(&devices));
        }
        let mut pids: Vec<u32> = self.pids.into_iter().collect();
        pids.sort_unstable();
        Deep { interval_us: INTERVAL_100NS as f64 / 10.0, modules, stacks, samples, pids, ..Deep::default() }
    }
}

/// The kernel's images when the session's rundown named none (pid 0), from the loaded driver list.
fn drivers(devices: &[(String, String)]) -> Vec<Module> {
    use windows_sys::Win32::System::ProcessStatus::{K32EnumDeviceDrivers, K32GetDeviceDriverFileNameW};
    let mut bases = vec![std::ptr::null_mut(); 1024];
    let mut need = 0u32;
    if unsafe { K32EnumDeviceDrivers(bases.as_mut_ptr(), (bases.len() * 8) as u32, &mut need) } == 0 {
        return Vec::new();
    }
    bases
        .iter()
        .take((need as usize / 8).min(bases.len()))
        .filter(|b| !b.is_null())
        .filter_map(|&b| {
            let mut name = vec![0u16; 1024];
            let n = unsafe { K32GetDeviceDriverFileNameW(b, name.as_mut_ptr(), name.len() as u32) };
            let path = dos_path(&String::from_utf16_lossy(&name[..n as usize]), devices);
            let size = crate::symbol::pdb::image_size(std::path::Path::new(&path))?;
            Some(Module { pid: 0, base: b as u64, size: size as u64, path })
        })
        .collect()
}

/// `\Device\HarddiskVolume3` → `C:` for every drive letter.
fn dos_devices() -> Vec<(String, String)> {
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
    let mut out = Vec::new();
    for letter in b'A'..=b'Z' {
        let drive = format!("{}:", letter as char);
        let w: Vec<u16> = drive.encode_utf16().chain(std::iter::once(0)).collect();
        let mut buf = vec![0u16; 1024];
        let n = unsafe { QueryDosDeviceW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if n > 0 {
            let end = buf.iter().position(|c| *c == 0).unwrap_or(0);
            out.push((String::from_utf16_lossy(&buf[..end]), drive));
        }
    }
    out
}

/// A kernel image path as a path this user can open.
pub fn dos_path(path: &str, devices: &[(String, String)]) -> String {
    let lower = path.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("\\systemroot\\") {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        return format!("{root}\\{}", &path[path.len() - rest.len()..]);
    }
    if let Some(rest) = path.strip_prefix("\\??\\") {
        return rest.to_string();
    }
    for (dev, drive) in devices {
        if lower.starts_with(&dev.to_ascii_lowercase()) && path[dev.len()..].starts_with('\\') {
            return format!("{drive}{}", &path[dev.len()..]);
        }
    }
    if path.starts_with('\\') && !path.starts_with("\\\\") {
        // `\Windows\System32\…` (no device): the system drive.
        let root = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        return format!("{root}{path}");
    }
    path.to_string()
}
