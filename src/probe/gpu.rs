//! GPU sampling (SPEC §8.5, §13.6). Per process: engine time and GPU memory from D3DKMT
//! statistics for a list of PIDs (cost scales with the list, not the machine); engine time for
//! every process from PDH `GPU Engine` (the same kernel counters, read for all). Per device:
//! clocks, power, temperature and PCIe from NVML, loaded at runtime from System32; no NVML, no
//! device fields, never an error.

use super::os::pdh::Query;
use serde::Serialize;

/// One graphics adapter as the OS names it. `luid` matches the `luid_` part of PDH instances.
#[derive(Clone, Debug, Serialize)]
pub struct Adapter {
    pub luid: u64,
    pub name: String,
    #[serde(skip)]
    pub bus: Option<u32>,
    /// Engine type name per node.
    #[serde(skip)]
    pub nodes: Vec<String>,
    /// Per memory segment: true for an aperture segment (system memory: "shared").
    #[serde(skip)]
    pub apertures: Vec<bool>,
}

/// One process on one adapter over one tick: engine time summed over the engines of one type,
/// in % of one engine (above 100 only when the type has several engines).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct EngineUse {
    pub pid: u32,
    /// Index into `Samples::adapters`.
    pub adapter: u16,
    /// Index into `Samples::engines`.
    pub engine: u16,
    pub pct: f32,
}

/// One process's GPU memory on one adapter at the end of a tick, in bytes: what it committed in
/// dedicated (local) and shared (aperture, system memory) segments. Allocations it only opens
/// from another process count in their owner, so sums over processes do not double count
/// (Task Manager's "Dedicated GPU memory" does).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct VramUse {
    pub pid: u32,
    pub adapter: u16,
    pub dedicated: u64,
    pub shared: u64,
}

/// One NVML device.
#[derive(Clone, Debug, Serialize)]
pub struct Device {
    pub name: String,
    /// Index into `Samples::adapters`, matched by PCI bus.
    pub adapter: Option<u16>,
    pub vram_total_mb: Option<f64>,
}

/// One NVML reading. A field is absent when the device or driver does not report it.
/// MB = 2^20 bytes. Utilization is the driver's own average over its sample period (1/6 to 1 s).
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct DeviceSample {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gfx_mhz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_mhz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power_w: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pcie_tx_mbs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pcie_rx_mbs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vram_used_mb: Option<f64>,
}

impl DeviceSample {
    pub fn fields(&self) -> [Option<f64>; 9] {
        [self.gpu_pct, self.mem_pct, self.gfx_mhz, self.mem_mhz, self.power_w, self.temp_c, self.pcie_tx_mbs, self.pcie_rx_mbs, self.vram_used_mb]
    }

    pub fn from_fields(f: [Option<f64>; 9]) -> DeviceSample {
        DeviceSample { gpu_pct: f[0], mem_pct: f[1], gfx_mhz: f[2], mem_mhz: f[3], power_w: f[4], temp_c: f[5], pcie_tx_mbs: f[6], pcie_rx_mbs: f[7], vram_used_mb: f[8] }
    }
}

/// The adapters D3DKMT enumerates, with driver descriptions, nodes and memory segments.
pub fn adapters() -> Vec<Adapter> {
    #[cfg(windows)]
    {
        let mut out = kmt::adapters();
        for i in 1..out.len() {
            let same = out[..i].iter().filter(|a| a.name == out[i].name).count();
            if same > 0 {
                out[i].name = format!("{} #{}", out[i].name, same + 1);
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

fn intern(names: &mut Vec<String>, name: &str) -> u16 {
    match names.iter().position(|n| n == name) {
        Some(i) => i as u16,
        None => {
            names.push(name.to_string());
            (names.len() - 1) as u16
        }
    }
}

/// Per-process engine time for every process (including ones D3DKMT cannot open, like dwm)
/// from the wildcard PDH counter `GPU Engine(*)\Utilization Percentage`. Costs about 0.5 ms
/// per collection on a busy desktop. Not memory: PDH's `Dedicated Usage` also counts
/// allocations opened from other processes (dwm shows more than the adapter holds), and its
/// other counters split dedicated and shared differently on integrated GPUs.
pub struct Engines {
    q: Query,
    util: isize,
    wide: Vec<(Vec<u16>, u16)>,
}

impl Engines {
    pub fn open() -> Option<Engines> {
        let q = Query::open()?;
        Some(Engines { util: q.add(r"\GPU Engine(*)\Utilization Percentage")?, q, wide: Vec::new() })
    }

    /// Collects and reads; zero rows are dropped. Rates cover the time since the last call.
    /// `present` gets every PID with a GPU context (an engine instance), busy or not.
    pub fn read(&mut self, adapters: &mut Vec<Adapter>, names: &mut Vec<String>, gpu: &mut Vec<EngineUse>, present: &mut Vec<u32>) {
        self.q.collect();
        let (wide, q) = (&mut self.wide, &mut self.q);
        q.each(self.util, 0, |inst, v| {
            let Some((pid, luid, rest)) = parse(inst) else { return };
            if present.last() != Some(&pid) {
                present.push(pid);
            }
            if v <= 0.0 {
                return;
            }
            let ty = after(rest, "engtype_").unwrap_or(&[]);
            let engine = match wide.iter().find(|w| w.0.as_slice() == ty) {
                Some(w) => w.1,
                None => {
                    let i = intern(names, &String::from_utf16_lossy(ty));
                    wide.push((ty.to_vec(), i));
                    i
                }
            };
            gpu.push(EngineUse { pid, adapter: adapter_index(adapters, luid), engine, pct: v as f32 });
        });
        present.sort_unstable();
        present.dedup();
        gpu.sort_unstable_by_key(|e| (e.pid, e.adapter, e.engine));
        gpu.dedup_by(|cur, prev| {
            let same = (cur.pid, cur.adapter, cur.engine) == (prev.pid, prev.adapter, prev.engine);
            if same {
                prev.pct += cur.pct;
            }
            same
        });
    }
}

fn adapter_index(adapters: &mut Vec<Adapter>, luid: u64) -> u16 {
    match adapters.iter().position(|a| a.luid == luid) {
        Some(i) => i as u16,
        None => {
            adapters.push(Adapter { luid, name: format!("adapter 0x{luid:x}"), bus: None, nodes: Vec::new(), apertures: Vec::new() });
            (adapters.len() - 1) as u16
        }
    }
}

/// `pid_1234_luid_0x00000000_0x000128E7_phys_0_eng_0_engtype_3D` → (pid, luid, rest).
fn parse(name: &[u16]) -> Option<(u32, u64, &[u16])> {
    let rest = strip(name, "pid_")?;
    let (pid, rest) = number(rest, 10)?;
    let rest = strip(rest, "_luid_0x")?;
    let (hi, rest) = number(rest, 16)?;
    let rest = strip(rest, "_0x")?;
    let (lo, rest) = number(rest, 16)?;
    Some((pid as u32, (hi << 32) | lo, rest))
}

fn strip<'a>(s: &'a [u16], prefix: &str) -> Option<&'a [u16]> {
    let n = prefix.len();
    (s.len() >= n && s.iter().zip(prefix.bytes()).all(|(&a, b)| a == b as u16)).then(|| &s[n..])
}

fn after<'a>(s: &'a [u16], pat: &str) -> Option<&'a [u16]> {
    (0..s.len()).find_map(|i| strip(&s[i..], pat))
}

fn number(s: &[u16], radix: u32) -> Option<(u64, &[u16])> {
    let digit = |c: u16| char::from_u32(c as u32).and_then(|c| c.to_digit(radix));
    let end = s.iter().position(|&c| digit(c).is_none()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((s[..end].iter().fold(0u64, |acc, &c| acc.wrapping_mul(radix as u64) + digit(c).unwrap_or(0) as u64), &s[end..]))
}

#[cfg(windows)]
pub use kmt::Kmt;
#[cfg(windows)]
pub use nvml::Nvml;

#[cfg(not(windows))]
pub struct Kmt {
    pub unreadable: Vec<u32>,
}

#[cfg(not(windows))]
impl Kmt {
    pub fn new(_adapters: &[Adapter], _names: &mut Vec<String>) -> Option<Kmt> {
        None
    }
    pub fn read(&mut self, _pids: &[u32], _dt_ns: u64, _nodes: bool, _gpu: &mut Vec<EngineUse>, _vram: &mut Vec<VramUse>) {}
}

#[cfg(not(windows))]
pub struct Nvml {
    pub devices: Vec<Device>,
}

#[cfg(not(windows))]
impl Nvml {
    pub fn load(_adapters: &[Adapter]) -> Option<Nvml> {
        None
    }
    pub fn sample(&mut self, _t_ns: u64, _pcie: bool, _out: &mut Vec<DeviceSample>) {}
}

#[cfg(windows)]
mod kmt {
    use super::{Adapter, EngineUse, VramUse, intern};
    use std::mem::{size_of, zeroed};
    use windows_sys::Wdk::Graphics::Direct3D::{
        D3DKMT_ADAPTERADDRESS, D3DKMT_ADAPTERINFO, D3DKMT_CLOSEADAPTER, D3DKMT_DRIVER_DESCRIPTION, D3DKMT_ENUMADAPTERS2, D3DKMT_NODEMETADATA, D3DKMT_QUERYADAPTERINFO, D3DKMT_QUERYSTATISTICS,
        D3DKMT_QUERYSTATISTICS_ADAPTER, D3DKMT_QUERYSTATISTICS_PROCESS_NODE, D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT, D3DKMT_QUERYSTATISTICS_QUERY_NODE, D3DKMT_QUERYSTATISTICS_QUERY_SEGMENT,
        D3DKMT_QUERYSTATISTICS_SEGMENT, D3DKMTCloseAdapter, D3DKMTEnumAdapters2, D3DKMTQueryAdapterInfo, D3DKMTQueryStatistics, KMTQAITYPE_ADAPTERADDRESS, KMTQAITYPE_DRIVER_DESCRIPTION,
        KMTQAITYPE_NODEMETADATA,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION};

    fn luid64(l: LUID) -> u64 {
        ((l.HighPart as u32 as u64) << 32) | l.LowPart as u64
    }

    fn luid(v: u64) -> LUID {
        LUID { LowPart: v as u32, HighPart: (v >> 32) as i32 }
    }

    fn query_info<T>(h: u32, kind: i32, out: &mut T) -> bool {
        let mut q = D3DKMT_QUERYADAPTERINFO { hAdapter: h, Type: kind, pPrivateDriverData: out as *mut T as *mut _, PrivateDriverDataSize: size_of::<T>() as u32 };
        unsafe { D3DKMTQueryAdapterInfo(&mut q) >= 0 }
    }

    /// D3DKMTQueryStatistics writes its result into the struct it is given.
    fn stats(st: &mut D3DKMT_QUERYSTATISTICS) -> bool {
        unsafe { D3DKMTQueryStatistics(st as *mut D3DKMT_QUERYSTATISTICS as *const _) >= 0 }
    }

    fn wide_str(w: &[u16]) -> String {
        String::from_utf16_lossy(&w[..w.iter().position(|&c| c == 0).unwrap_or(w.len())]).trim().to_string()
    }

    pub fn adapters() -> Vec<Adapter> {
        unsafe {
            let mut e: D3DKMT_ENUMADAPTERS2 = zeroed();
            if D3DKMTEnumAdapters2(&mut e) < 0 || e.NumAdapters == 0 {
                return Vec::new();
            }
            let mut infos: Vec<D3DKMT_ADAPTERINFO> = vec![zeroed(); e.NumAdapters as usize];
            e.pAdapters = infos.as_mut_ptr();
            if D3DKMTEnumAdapters2(&mut e) < 0 {
                return Vec::new();
            }
            infos.truncate(e.NumAdapters as usize);
            let mut st: Box<D3DKMT_QUERYSTATISTICS> = Box::new(zeroed());
            infos
                .iter()
                .map(|a| {
                    let id = luid64(a.AdapterLuid);
                    let mut desc: Box<D3DKMT_DRIVER_DESCRIPTION> = Box::new(zeroed());
                    let name = if query_info(a.hAdapter, KMTQAITYPE_DRIVER_DESCRIPTION, &mut *desc) { wide_str(&desc.DriverDescription) } else { String::new() };
                    let mut addr: D3DKMT_ADAPTERADDRESS = zeroed();
                    let bus = query_info(a.hAdapter, KMTQAITYPE_ADAPTERADDRESS, &mut addr).then_some(addr.BusNumber);
                    *st = zeroed();
                    st.Type = D3DKMT_QUERYSTATISTICS_ADAPTER;
                    st.AdapterLuid = a.AdapterLuid;
                    let (node_count, segment_count) = if stats(&mut st) { (st.QueryResult.AdapterInformation.NodeCount, st.QueryResult.AdapterInformation.NbSegments) } else { (0, 0) };
                    let nodes = (0..node_count)
                        .map(|n| {
                            let mut md: D3DKMT_NODEMETADATA = zeroed();
                            md.NodeOrdinalAndAdapterIndex = n;
                            if !query_info(a.hAdapter, KMTQAITYPE_NODEMETADATA, &mut md) {
                                return format!("Node {n}");
                            }
                            // Named like PDH's `engtype_`: the engine type, or the driver's
                            // friendly name for engines of type Other.
                            let friendly = wide_str(&{ md.NodeData.FriendlyName });
                            match md.NodeData.EngineType {
                                1 => "3D",
                                2 => "VideoDecode",
                                3 => "VideoEncode",
                                4 => "VideoProcessing",
                                5 => "SceneAssembly",
                                6 => "Copy",
                                7 => "Overlay",
                                8 => "Crypto",
                                _ if !friendly.is_empty() => return friendly,
                                _ => "Other",
                            }
                            .to_string()
                        })
                        .collect();
                    let apertures = (0..segment_count)
                        .map(|s| {
                            *st = zeroed();
                            st.Type = D3DKMT_QUERYSTATISTICS_SEGMENT;
                            st.AdapterLuid = a.AdapterLuid;
                            st.Anonymous.QuerySegment = D3DKMT_QUERYSTATISTICS_QUERY_SEGMENT { SegmentId: s };
                            stats(&mut st) && st.QueryResult.SegmentInformation.Aperture != 0
                        })
                        .collect();
                    D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: a.hAdapter });
                    Adapter { luid: id, name: if name.is_empty() { format!("adapter 0x{id:x}") } else { name }, bus, nodes, apertures }
                })
                .collect()
        }
    }

    /// Engine time and memory for a list of PIDs from D3DKMTQueryStatistics, one call per node
    /// and segment of each adapter a process has opened (needs PROCESS_QUERY_INFORMATION; PIDs
    /// that cannot be opened are listed in `unreadable`). Adapters a process has not opened are
    /// rechecked every 20 reads, and PIDs that join after the first read are born during
    /// sampling: both start from zero, so no engine time is lost.
    pub struct Kmt {
        adapters: Vec<KmtAdapter>,
        procs: Vec<Proc>,
        st: Box<D3DKMT_QUERYSTATISTICS>,
        started: bool,
        pub unreadable: Vec<u32>,
    }

    // The process handles are owned and only used from the thread that holds the Kmt.
    unsafe impl Send for Kmt {}

    struct KmtAdapter {
        luid: LUID,
        index: u16,
        engines: Vec<u16>,
        apertures: Vec<bool>,
    }

    struct Proc {
        pid: u32,
        handle: HANDLE,
        uses: Vec<Use>,
    }

    #[derive(Clone)]
    struct Use {
        open: bool,
        checked: u32,
        running: Vec<i64>,
    }

    impl Kmt {
        pub fn new(adapters: &[Adapter], names: &mut Vec<String>) -> Option<Kmt> {
            let list: Vec<KmtAdapter> = adapters
                .iter()
                .enumerate()
                .filter(|(_, a)| !a.nodes.is_empty())
                .map(|(i, a)| KmtAdapter { luid: luid(a.luid), index: i as u16, engines: a.nodes.iter().map(|n| intern(names, n)).collect(), apertures: a.apertures.clone() })
                .collect();
            (!list.is_empty()).then(|| Kmt { adapters: list, procs: Vec::new(), st: Box::new(unsafe { zeroed() }), started: false, unreadable: Vec::new() })
        }

        /// Reads every PID in `pids` (sorted): memory, and with `nodes` engine time over the
        /// `dt_ns` tick. Handles of PIDs no longer listed are closed.
        pub fn read(&mut self, pids: &[u32], dt_ns: u64, nodes: bool, gpu: &mut Vec<EngineUse>, vram: &mut Vec<VramUse>) {
            self.procs.retain(|p| {
                let keep = pids.binary_search(&p.pid).is_ok();
                if !keep && !p.handle.is_null() {
                    unsafe { CloseHandle(p.handle) };
                }
                keep
            });
            for &pid in pids {
                if self.procs.iter().any(|p| p.pid == pid) {
                    continue;
                }
                let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid) };
                if handle.is_null() && !self.unreadable.contains(&pid) {
                    self.unreadable.push(pid);
                }
                // Processes present at the first read get a baseline (-1); later ones count from zero.
                let base = if self.started { 0 } else { -1 };
                let uses = self.adapters.iter().map(|a| Use { open: true, checked: 0, running: vec![base; a.engines.len()] }).collect();
                self.procs.push(Proc { pid, handle, uses });
            }
            self.started = true;
            let st = &mut *self.st;
            for p in &mut self.procs {
                if p.handle.is_null() {
                    continue;
                }
                for (a, u) in self.adapters.iter().zip(p.uses.iter_mut()) {
                    if !u.open {
                        u.checked += 1;
                        if u.checked < 20 {
                            continue;
                        }
                        u.checked = 0;
                    }
                    let (mut dedicated, mut shared, mut any) = (0u64, 0u64, false);
                    for (s, &aperture) in a.apertures.iter().enumerate() {
                        st.Type = D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT;
                        st.AdapterLuid = a.luid;
                        st.hProcess = p.handle;
                        st.Anonymous.QueryProcessSegment = D3DKMT_QUERYSTATISTICS_QUERY_SEGMENT { SegmentId: s as u32 };
                        if !stats(st) {
                            continue;
                        }
                        any = true;
                        let bytes = unsafe { st.QueryResult.ProcessSegmentInformation.BytesCommitted };
                        if aperture { shared += bytes } else { dedicated += bytes }
                    }
                    if !any && !a.apertures.is_empty() {
                        // Not opened: its counters start at zero when it is.
                        u.open = false;
                        u.running.iter_mut().for_each(|r| *r = 0);
                        continue;
                    }
                    u.open = true;
                    if dedicated + shared > 0 {
                        vram.push(VramUse { pid: p.pid, adapter: a.index, dedicated, shared });
                    }
                    if !nodes {
                        continue;
                    }
                    for (n, &engine) in a.engines.iter().enumerate() {
                        st.Type = D3DKMT_QUERYSTATISTICS_PROCESS_NODE;
                        st.AdapterLuid = a.luid;
                        st.hProcess = p.handle;
                        st.Anonymous.QueryProcessNode = D3DKMT_QUERYSTATISTICS_QUERY_NODE { NodeId: n as u32 };
                        if !stats(st) {
                            continue;
                        }
                        let running = unsafe { st.QueryResult.ProcessNodeInformation.RunningTime };
                        let prev = std::mem::replace(&mut u.running[n], running);
                        if prev >= 0 && running > prev && dt_ns > 0 {
                            // RunningTime is in 100 ns units.
                            let pct = ((running - prev) as f64 * 100.0 / dt_ns as f64 * 100.0) as f32;
                            gpu.push(EngineUse { pid: p.pid, adapter: a.index, engine, pct });
                        }
                    }
                }
            }
            gpu.sort_unstable_by_key(|e| (e.pid, e.adapter, e.engine));
            gpu.dedup_by(|cur, prev| {
                let same = (cur.pid, cur.adapter, cur.engine) == (prev.pid, prev.adapter, prev.engine);
                if same {
                    prev.pct += cur.pct;
                }
                same
            });
        }
    }

    impl Drop for Kmt {
        fn drop(&mut self) {
            for p in &self.procs {
                if !p.handle.is_null() {
                    unsafe { CloseHandle(p.handle) };
                }
            }
        }
    }
}

#[cfg(windows)]
mod nvml {
    use super::{Adapter, Device, DeviceSample};
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

    type Dev = *mut c_void;
    type Ret = i32;
    const MB: f64 = 1024.0 * 1024.0;
    const FI_POWER_INSTANT: u32 = 186;
    const FI_PCIE_TX_BYTES: u32 = 197;
    const FI_PCIE_RX_BYTES: u32 = 198;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Field {
        id: u32,
        scope: u32,
        timestamp: i64,
        latency_us: i64,
        kind: u32,
        ret: Ret,
        value: u64,
    }

    #[repr(C)]
    struct PciInfo {
        bus_id_legacy: [u8; 16],
        domain: u32,
        bus: u32,
        device: u32,
        device_id: u32,
        subsystem_id: u32,
        bus_id: [u8; 32],
    }

    #[derive(Clone, Copy, Default)]
    struct Last {
        pcie_ns: u64,
        pcie: Option<(u64, u64)>,
    }

    /// NVML device readings. Each call is a few µs except the energy counter (~0.5 ms), which
    /// is not read: power is the instantaneous reading, averaged over windows by the caller.
    pub struct Nvml {
        lib: HMODULE,
        shutdown: Option<unsafe extern "C" fn() -> Ret>,
        util: Option<unsafe extern "C" fn(Dev, *mut [u32; 2]) -> Ret>,
        clock: Option<unsafe extern "C" fn(Dev, u32, *mut u32) -> Ret>,
        power: Option<unsafe extern "C" fn(Dev, *mut u32) -> Ret>,
        temp: Option<unsafe extern "C" fn(Dev, u32, *mut u32) -> Ret>,
        mem: Option<unsafe extern "C" fn(Dev, *mut [u64; 3]) -> Ret>,
        fields: Option<unsafe extern "C" fn(Dev, i32, *mut Field) -> Ret>,
        handles: Vec<Dev>,
        last: Vec<Last>,
        pub devices: Vec<Device>,
    }

    // NVML is thread-safe; device handles are opaque tokens.
    unsafe impl Send for Nvml {}

    unsafe fn sym<T: Copy>(lib: HMODULE, name: &[u8]) -> Option<T> {
        let f = unsafe { GetProcAddress(lib, name.as_ptr()) }?;
        Some(unsafe { std::mem::transmute_copy(&f) })
    }

    impl Nvml {
        pub fn load(adapters: &[Adapter]) -> Option<Nvml> {
            let path: Vec<u16> = "nvml.dll".encode_utf16().chain(std::iter::once(0)).collect();
            let lib = unsafe { LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
            if lib.is_null() {
                return None;
            }
            unsafe {
                let init: Option<unsafe extern "C" fn() -> Ret> = sym(lib, b"nvmlInit_v2\0");
                let count: Option<unsafe extern "C" fn(*mut u32) -> Ret> = sym(lib, b"nvmlDeviceGetCount_v2\0");
                let by_index: Option<unsafe extern "C" fn(u32, *mut Dev) -> Ret> = sym(lib, b"nvmlDeviceGetHandleByIndex_v2\0");
                let (Some(init), Some(count), Some(by_index)) = (init, count, by_index) else {
                    FreeLibrary(lib);
                    return None;
                };
                if init() != 0 {
                    FreeLibrary(lib);
                    return None;
                }
                let mut nv = Nvml {
                    lib,
                    shutdown: sym(lib, b"nvmlShutdown\0"),
                    util: sym(lib, b"nvmlDeviceGetUtilizationRates\0"),
                    clock: sym(lib, b"nvmlDeviceGetClockInfo\0"),
                    power: sym(lib, b"nvmlDeviceGetPowerUsage\0"),
                    temp: sym(lib, b"nvmlDeviceGetTemperature\0"),
                    mem: sym(lib, b"nvmlDeviceGetMemoryInfo\0"),
                    fields: sym(lib, b"nvmlDeviceGetFieldValues\0"),
                    handles: Vec::new(),
                    last: Vec::new(),
                    devices: Vec::new(),
                };
                let name_fn: Option<unsafe extern "C" fn(Dev, *mut u8, u32) -> Ret> = sym(lib, b"nvmlDeviceGetName\0");
                let pci_fn: Option<unsafe extern "C" fn(Dev, *mut PciInfo) -> Ret> = sym(lib, b"nvmlDeviceGetPciInfo_v3\0");
                let mut n = 0u32;
                if count(&mut n) != 0 {
                    n = 0;
                }
                for i in 0..n {
                    let mut h: Dev = std::ptr::null_mut();
                    if by_index(i, &mut h) != 0 || h.is_null() {
                        continue;
                    }
                    let mut buf = [0u8; 96];
                    let name = match name_fn {
                        Some(f) if f(h, buf.as_mut_ptr(), buf.len() as u32) == 0 => String::from_utf8_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]).to_string(),
                        _ => format!("NVIDIA GPU {i}"),
                    };
                    let mut pci: PciInfo = std::mem::zeroed();
                    let bus = pci_fn.filter(|f| f(h, &mut pci) == 0).map(|_| pci.bus);
                    let adapter = bus.and_then(|b| adapters.iter().position(|a| a.bus == Some(b))).map(|i| i as u16);
                    let mut m = [0u64; 3];
                    let vram_total_mb = nv.mem.filter(|f| f(h, &mut m) == 0).map(|_| m[0] as f64 / MB);
                    nv.handles.push(h);
                    nv.last.push(Last::default());
                    nv.devices.push(Device { name, adapter, vram_total_mb });
                }
                if nv.handles.is_empty() {
                    return None;
                }
                Some(nv)
            }
        }

        /// Reads every device once. With `pcie`, also the PCIe byte counters; their rates cover
        /// the time since the last such read.
        pub fn sample(&mut self, t_ns: u64, pcie: bool, out: &mut Vec<DeviceSample>) {
            for (i, &h) in self.handles.iter().enumerate() {
                let last = self.last[i];
                let mut s = DeviceSample::default();
                unsafe {
                    let mut u = [0u32; 2];
                    if self.util.is_some_and(|f| f(h, &mut u) == 0) {
                        s.gpu_pct = Some(u[0] as f64);
                        s.mem_pct = Some(u[1] as f64);
                    }
                    let mut v = 0u32;
                    s.gfx_mhz = self.clock.filter(|f| f(h, 0, &mut v) == 0).map(|_| v as f64);
                    s.mem_mhz = self.clock.filter(|f| f(h, 2, &mut v) == 0).map(|_| v as f64);
                    s.temp_c = self.temp.filter(|f| f(h, 0, &mut v) == 0).map(|_| v as f64);
                    let mut m = [0u64; 3];
                    s.vram_used_mb = self.mem.filter(|f| f(h, &mut m) == 0).map(|_| m[2] as f64 / MB);
                    // The PCIe counters cost ~0.1 ms per read; the other fields a few µs.
                    let mut fv = [FI_POWER_INSTANT, FI_PCIE_TX_BYTES, FI_PCIE_RX_BYTES].map(|id| Field { id, ..Field::default() });
                    let n = if pcie { 3 } else { 1 };
                    let ok = self.fields.is_some_and(|f| f(h, n, fv.as_mut_ptr()) == 0);
                    let value = |f: &Field| if ok { counter(f) } else { None };
                    s.power_w = value(&fv[0]).map(|mw| mw as f64 / 1000.0).or_else(|| self.power.filter(|f| f(h, &mut v) == 0).map(|_| v as f64 / 1000.0));
                    if pcie {
                        let now = value(&fv[1]).zip(value(&fv[2]));
                        let dt_s = t_ns.saturating_sub(last.pcie_ns) as f64 / 1e9;
                        if let (Some((tx1, rx1)), Some((tx0, rx0))) = (now, last.pcie)
                            && dt_s > 0.0
                            && tx1 >= tx0
                            && rx1 >= rx0
                        {
                            s.pcie_tx_mbs = Some((tx1 - tx0) as f64 / MB / dt_s);
                            s.pcie_rx_mbs = Some((rx1 - rx0) as f64 / MB / dt_s);
                        }
                        self.last[i] = Last { pcie_ns: t_ns, pcie: now };
                    }
                }
                out.push(s);
            }
        }
    }

    fn counter(f: &Field) -> Option<u64> {
        match (f.ret, f.kind) {
            (0, 1 | 2 | 5 | 6) => Some(f.value & 0xffff_ffff),
            (0, 3 | 4) => Some(f.value),
            _ => None,
        }
    }

    impl Drop for Nvml {
        fn drop(&mut self) {
            unsafe {
                if let Some(f) = self.shutdown {
                    f();
                }
                FreeLibrary(self.lib);
            }
        }
    }
}
