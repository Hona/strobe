//! GPU sampling (SPEC §8.5, §13.6): engine time and memory per process from PDH (`GPU Engine`,
//! `GPU Process Memory`), adapter names from D3DKMT, and device clocks, power, temperature and
//! PCIe from NVML, loaded at runtime from System32. No NVML, no device fields; never an error.

use super::os::pdh::Query;
use serde::Serialize;

const MB: f64 = 1024.0 * 1024.0;

/// One graphics adapter as the OS names it. `luid` matches the `luid_` part of PDH instances.
#[derive(Clone, Debug, Serialize)]
pub struct Adapter {
    pub luid: u64,
    pub name: String,
    #[serde(skip)]
    pub bus: Option<u32>,
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

/// One process's GPU memory on one adapter at the end of a tick, in bytes.
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
/// MB = 2^20 bytes. Power is the energy counter's average over the tick when available.
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
        DeviceSample {
            gpu_pct: f[0],
            mem_pct: f[1],
            gfx_mhz: f[2],
            mem_mhz: f[3],
            power_w: f[4],
            temp_c: f[5],
            pcie_tx_mbs: f[6],
            pcie_rx_mbs: f[7],
            vram_used_mb: f[8],
        }
    }
}

/// The adapters D3DKMT enumerates, with their driver descriptions.
pub fn adapters() -> Vec<Adapter> {
    #[cfg(windows)]
    {
        let mut out = d3dkmt_adapters();
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

#[cfg(windows)]
fn d3dkmt_adapters() -> Vec<Adapter> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Wdk::Graphics::Direct3D::{
        D3DKMT_ADAPTERADDRESS, D3DKMT_ADAPTERINFO, D3DKMT_CLOSEADAPTER, D3DKMT_DRIVER_DESCRIPTION, D3DKMT_ENUMADAPTERS2,
        D3DKMT_QUERYADAPTERINFO, D3DKMTCloseAdapter, D3DKMTEnumAdapters2, D3DKMTQueryAdapterInfo, KMTQAITYPE_ADAPTERADDRESS,
        KMTQAITYPE_DRIVER_DESCRIPTION,
    };
    fn query<T>(h: u32, kind: i32, out: &mut T) -> bool {
        let mut q = D3DKMT_QUERYADAPTERINFO { hAdapter: h, Type: kind, pPrivateDriverData: out as *mut T as *mut _, PrivateDriverDataSize: size_of::<T>() as u32 };
        unsafe { D3DKMTQueryAdapterInfo(&mut q) >= 0 }
    }
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
        infos
            .iter()
            .map(|a| {
                let luid = ((a.AdapterLuid.HighPart as u32 as u64) << 32) | a.AdapterLuid.LowPart as u64;
                let mut desc: Box<D3DKMT_DRIVER_DESCRIPTION> = Box::new(zeroed());
                let name = if query(a.hAdapter, KMTQAITYPE_DRIVER_DESCRIPTION, &mut *desc) {
                    let w = &desc.DriverDescription;
                    String::from_utf16_lossy(&w[..w.iter().position(|&c| c == 0).unwrap_or(w.len())]).trim().to_string()
                } else {
                    String::new()
                };
                let mut addr: D3DKMT_ADAPTERADDRESS = zeroed();
                let bus = query(a.hAdapter, KMTQAITYPE_ADAPTERADDRESS, &mut addr).then_some(addr.BusNumber);
                D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: a.hAdapter });
                Adapter { luid, name: if name.is_empty() { format!("adapter 0x{luid:x}") } else { name }, bus }
            })
            .collect()
    }
}

/// Per-process engine time and memory from three wildcard PDH counters in the shared query.
pub struct Engines {
    util: isize,
    dedicated: isize,
    shared: isize,
    pub names: Vec<String>,
    wide: Vec<Vec<u16>>,
}

impl Engines {
    pub fn add(q: &Query) -> Option<Engines> {
        Some(Engines {
            util: q.add(r"\GPU Engine(*)\Utilization Percentage")?,
            dedicated: q.add(r"\GPU Process Memory(*)\Dedicated Usage")?,
            shared: q.add(r"\GPU Process Memory(*)\Shared Usage")?,
            names: Vec::new(),
            wide: Vec::new(),
        })
    }

    /// Reads the last collection for the PIDs `keep` accepts; zero rows are dropped.
    pub fn read(&mut self, q: &mut Query, keep: &dyn Fn(u32) -> bool, adapters: &mut Vec<Adapter>, gpu: &mut Vec<EngineUse>, vram: &mut Vec<VramUse>) {
        let (names, wide) = (&mut self.names, &mut self.wide);
        q.each(self.util, 0, |inst, v| {
            if v <= 0.0 {
                return;
            }
            let Some((pid, luid, rest)) = parse(inst) else { return };
            if !keep(pid) {
                return;
            }
            let ty = after(rest, "engtype_").unwrap_or(&[]);
            let engine = match wide.iter().position(|w| w.as_slice() == ty) {
                Some(i) => i,
                None => {
                    wide.push(ty.to_vec());
                    names.push(String::from_utf16_lossy(ty));
                    wide.len() - 1
                }
            };
            gpu.push(EngineUse { pid, adapter: adapter_index(adapters, luid), engine: engine as u16, pct: v as f32 });
        });
        gpu.sort_unstable_by_key(|e| (e.pid, e.adapter, e.engine));
        gpu.dedup_by(|cur, prev| {
            let same = (cur.pid, cur.adapter, cur.engine) == (prev.pid, prev.adapter, prev.engine);
            if same {
                prev.pct += cur.pct;
            }
            same
        });
        for (counter, dedicated) in [(self.dedicated, true), (self.shared, false)] {
            q.each(counter, super::os::pdh::NOSCALE, |inst, v| {
                if v <= 0.0 {
                    return;
                }
                let Some((pid, luid, _)) = parse(inst) else { return };
                if !keep(pid) {
                    return;
                }
                let bytes = v as u64;
                let (d, s) = if dedicated { (bytes, 0) } else { (0, bytes) };
                vram.push(VramUse { pid, adapter: adapter_index(adapters, luid), dedicated: d, shared: s });
            });
        }
        vram.sort_unstable_by_key(|m| (m.pid, m.adapter));
        vram.dedup_by(|cur, prev| {
            let same = (cur.pid, cur.adapter) == (prev.pid, prev.adapter);
            if same {
                prev.dedicated += cur.dedicated;
                prev.shared += cur.shared;
            }
            same
        });
    }
}

fn adapter_index(adapters: &mut Vec<Adapter>, luid: u64) -> u16 {
    match adapters.iter().position(|a| a.luid == luid) {
        Some(i) => i as u16,
        None => {
            adapters.push(Adapter { luid, name: format!("adapter 0x{luid:x}"), bus: None });
            (adapters.len() - 1) as u16
        }
    }
}

/// `pid_1234_luid_0x00000000_0x000128E7_phys_0[_eng_0_engtype_3D]` → (pid, luid, rest).
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
    let end = s.iter().position(|&c| char::from_u32(c as u32).and_then(|c| c.to_digit(radix)).is_none()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let v = s[..end].iter().fold(0u64, |acc, &c| acc.wrapping_mul(radix as u64) + char::from_u32(c as u32).and_then(|c| c.to_digit(radix)).unwrap_or(0) as u64);
    Some((v, &s[end..]))
}

#[cfg(windows)]
pub use nvml::Nvml;

#[cfg(not(windows))]
pub struct Nvml {
    pub devices: Vec<Device>,
}

#[cfg(not(windows))]
impl Nvml {
    pub fn load(_adapters: &[Adapter]) -> Option<Nvml> {
        None
    }
    pub fn sample(&mut self, _t_ns: u64, _out: &mut Vec<DeviceSample>) {}
}

#[cfg(windows)]
mod nvml {
    use super::{Adapter, Device, DeviceSample, MB};
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

    type Dev = *mut c_void;
    type Ret = i32;
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
        t_ns: u64,
        energy_mj: Option<u64>,
        pcie: Option<(u64, u64)>,
    }

    pub struct Nvml {
        lib: HMODULE,
        shutdown: Option<unsafe extern "C" fn() -> Ret>,
        util: Option<unsafe extern "C" fn(Dev, *mut [u32; 2]) -> Ret>,
        clock: Option<unsafe extern "C" fn(Dev, u32, *mut u32) -> Ret>,
        power: Option<unsafe extern "C" fn(Dev, *mut u32) -> Ret>,
        energy: Option<unsafe extern "C" fn(Dev, *mut u64) -> Ret>,
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
                    energy: sym(lib, b"nvmlDeviceGetTotalEnergyConsumption\0"),
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
                        Some(f) if f(h, buf.as_mut_ptr(), buf.len() as u32) == 0 => {
                            String::from_utf8_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]).to_string()
                        }
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

        /// Reads every device once; rates (power from energy, PCIe) cover the time since the last call.
        pub fn sample(&mut self, t_ns: u64, out: &mut Vec<DeviceSample>) {
            for (i, &h) in self.handles.iter().enumerate() {
                let last = self.last[i];
                let dt_s = t_ns.saturating_sub(last.t_ns) as f64 / 1e9;
                let fresh = last.t_ns > 0 && dt_s > 0.0;
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
                    let mut e = 0u64;
                    let energy = self.energy.filter(|f| f(h, &mut e) == 0).map(|_| e);
                    s.power_w = match (energy, last.energy_mj) {
                        (Some(e1), Some(e0)) if fresh && e1 >= e0 => Some((e1 - e0) as f64 / 1000.0 / dt_s),
                        _ => self.power.filter(|f| f(h, &mut v) == 0).map(|_| v as f64 / 1000.0),
                    };
                    let mut fv = [Field { id: FI_PCIE_TX_BYTES, ..Default::default() }, Field { id: FI_PCIE_RX_BYTES, ..Default::default() }];
                    let pcie = self.fields.filter(|f| f(h, 2, fv.as_mut_ptr()) == 0).and_then(|_| Some((counter(&fv[0])?, counter(&fv[1])?)));
                    if let (Some((tx1, rx1)), Some((tx0, rx0))) = (pcie, last.pcie)
                        && fresh
                        && tx1 >= tx0
                        && rx1 >= rx0
                    {
                        s.pcie_tx_mbs = Some((tx1 - tx0) as f64 / MB / dt_s);
                        s.pcie_rx_mbs = Some((rx1 - rx0) as f64 / MB / dt_s);
                    }
                    self.last[i] = Last { t_ns, energy_mj: energy, pcie };
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
