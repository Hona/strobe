//! Win32 calls behind `host`. Each function reads an OS API directly; nothing spawns.

use super::{CacheGroup, ProcTimes, Topology};
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE, LocalFree, POINT};
use windows_sys::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW,
    MonitorFromPoint,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, PowerGetActiveScheme, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, RelationCache, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessAffinityMask, GetProcessTimes, GetSystemTimes, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, SetProcessAffinityMask,
};

pub fn qpc_ns() -> u64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    });
    let mut c = 0i64;
    unsafe { QueryPerformanceCounter(&mut c) };
    ((c as i128) * 1_000_000_000 / freq as i128) as u64
}

fn ft(t: &FILETIME) -> u64 {
    ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) * 100
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn process_tree(root: u32) -> Vec<ProcTimes> {
    let mut all: Vec<(u32, u32, String)> = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut e: PROCESSENTRY32W = zeroed();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                all.push((e.th32ProcessID, e.th32ParentProcessID, wide_to_string(&e.szExeFile)));
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, p) in all.iter().enumerate() {
        children.entry(p.1).or_default().push(i);
    }
    let mut out = Vec::new();
    let mut stack: Vec<u32> = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(p) = all.iter().find(|p| p.0 == pid) {
            let (cpu_ns, private_bytes) = proc_times(pid);
            out.push(ProcTimes { pid, parent: p.1, name: p.2.clone(), cpu_ns, private_bytes });
        }
        for &i in children.get(&pid).map(Vec::as_slice).unwrap_or(&[]) {
            // A reused parent pid can point at an older, unrelated process; children are always newer.
            stack.push(all[i].0);
        }
    }
    out
}

fn proc_times(pid: u32) -> (u64, u64) {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return (0, 0);
        }
        let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = (zeroed(), zeroed(), zeroed(), zeroed());
        let cpu = if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0 { ft(&k) + ft(&u) } else { 0 };
        let mut m: PROCESS_MEMORY_COUNTERS_EX = zeroed();
        m.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let private = if K32GetProcessMemoryInfo(h, &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb) != 0 {
            m.PrivateUsage as u64
        } else {
            0
        };
        CloseHandle(h);
        (cpu, private)
    }
}

pub fn topology() -> Topology {
    let logical = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let mut groups = Vec::new();
    unsafe {
        let mut len = 0u32;
        GetLogicalProcessorInformationEx(RelationCache, std::ptr::null_mut(), &mut len);
        if len == 0 {
            return Topology { logical, groups };
        }
        let mut buf = vec![0u8; len as usize];
        if GetLogicalProcessorInformationEx(RelationCache, buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, &mut len) == 0 {
            return Topology { logical, groups };
        }
        let mut off = 0usize;
        while off < len as usize {
            let info = &*(buf.as_ptr().add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            if info.Relationship == RelationCache {
                let cache = &info.Anonymous.Cache;
                if cache.Level == 3 {
                    let mask = cache.Anonymous.GroupMask.Mask as u64;
                    groups.push(CacheGroup { mask, cpus: mask.count_ones(), l3_mb: cache.CacheSize / (1024 * 1024) });
                }
            }
            if info.Size == 0 {
                break;
            }
            off += info.Size as usize;
        }
    }
    Topology { logical, groups }
}

pub fn set_affinity(mask: u64) -> Option<u64> {
    unsafe {
        let h = GetCurrentProcess();
        let (mut proc_mask, mut sys_mask) = (0usize, 0usize);
        if GetProcessAffinityMask(h, &mut proc_mask, &mut sys_mask) == 0 {
            return None;
        }
        let target = if mask == 0 { sys_mask } else { mask as usize & sys_mask };
        if target == 0 || SetProcessAffinityMask(h, target) == 0 {
            return None;
        }
        Some(proc_mask as u64)
    }
}

fn reg_string(key: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = vec![0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut size)
    };
    (rc == 0).then(|| wide_to_string(&buf).trim().to_string())
}

pub fn os_name() -> String {
    let key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build = reg_string(key, "CurrentBuild").unwrap_or_default();
    let version = reg_string(key, "DisplayVersion").unwrap_or_default();
    let name = if build.parse::<u32>().unwrap_or(0) >= 22000 { "Windows 11" } else { "Windows 10" };
    format!("{name} {build}{}", if version.is_empty() { String::new() } else { format!(" ({version})") })
}

pub fn cpu_name() -> String {
    reg_string(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", "ProcessorNameString").unwrap_or_default()
}

pub fn power_plan() -> String {
    unsafe {
        let mut guid = std::ptr::null_mut();
        if PowerGetActiveScheme(std::ptr::null_mut(), &mut guid) != 0 || guid.is_null() {
            return String::new();
        }
        let g = *guid;
        LocalFree(guid as *mut _);
        let text = format!("{:08x}-{:04x}-{:04x}", g.data1, g.data2, g.data3);
        match text.as_str() {
            "381b4222-f694-41f0" => "Balanced".into(),
            "8c5e7fda-e8bf-4a96" => "High performance".into(),
            "a1841308-3541-4fab" => "Power saver".into(),
            "e9a42b02-d5df-448d" => "Ultimate performance".into(),
            _ => format!("custom {text}"),
        }
    }
}

pub fn on_battery() -> bool {
    unsafe {
        let mut s: SYSTEM_POWER_STATUS = zeroed();
        GetSystemPowerStatus(&mut s) != 0 && s.ACLineStatus == 0
    }
}

pub fn system_busy(ms: u64) -> f64 {
    let read = || unsafe {
        let (mut i, mut k, mut u): (FILETIME, FILETIME, FILETIME) = (zeroed(), zeroed(), zeroed());
        GetSystemTimes(&mut i, &mut k, &mut u);
        (ft(&i), ft(&k) + ft(&u))
    };
    let (i0, t0) = read();
    std::thread::sleep(std::time::Duration::from_millis(ms));
    let (i1, t1) = read();
    let total = t1.saturating_sub(t0) as f64;
    if total <= 0.0 { 0.0 } else { 1.0 - (i1.saturating_sub(i0) as f64 / total) }
}

/// Puts every visible top-level window of these processes on top without activating it, so the
/// target is never occluded (an occluded Chromium window stops rendering).
pub fn raise_windows(pids: &[u32]) -> usize {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowThreadProcessId, HWND_TOPMOST, IsWindowVisible, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowPos,
    };
    struct State<'a> {
        pids: &'a [u32],
        found: Vec<HWND>,
    }
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> i32 {
        let state = unsafe { &mut *(lparam as *mut State) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if state.pids.contains(&pid) && unsafe { IsWindowVisible(hwnd) } != 0 && unsafe { GetWindowTextLengthW(hwnd) } > 0 {
            state.found.push(hwnd);
        }
        1
    }
    let mut state = State { pids, found: Vec::new() };
    unsafe {
        EnumWindows(Some(each), &mut state as *mut State as LPARAM);
        for &h in &state.found {
            SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
    }
    state.found.len()
}

pub fn refresh_at(x: i32, y: i32) -> Option<u32> {
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFOEXW = zeroed();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(mon, &mut info as *mut _ as *mut _) == 0 {
            return None;
        }
        let mut mode: DEVMODEW = zeroed();
        mode.dmSize = size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(info.szDevice.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) == 0 {
            return None;
        }
        Some(mode.dmDisplayFrequency).filter(|&f| f > 1)
    }
}
