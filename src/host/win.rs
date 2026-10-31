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

/// PIDs of running processes whose executable is exactly `exe`.
pub fn running(exe: &std::path::Path) -> Vec<u32> {
    let name = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = zeroed();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                if wide_to_string(&e.szExeFile).to_lowercase() == name && exe_of(e.th32ProcessID).is_some_and(|p| p.eq_ignore_ascii_case_path(exe)) {
                    out.push(e.th32ProcessID);
                }
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

/// PIDs of running processes whose executable lives under `dir`.
pub fn running_under(dir: &std::path::Path) -> Vec<u32> {
    let prefix = dir.to_string_lossy().to_lowercase();
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = zeroed();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                if exe_of(e.th32ProcessID).is_some_and(|p| p.to_string_lossy().to_lowercase().starts_with(&prefix)) {
                    out.push(e.th32ProcessID);
                }
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

trait PathEq {
    fn eq_ignore_ascii_case_path(&self, other: &std::path::Path) -> bool;
}

impl PathEq for std::path::PathBuf {
    fn eq_ignore_ascii_case_path(&self, other: &std::path::Path) -> bool {
        self.to_string_lossy().eq_ignore_ascii_case(&other.to_string_lossy())
    }
}

/// The executable path of a running process.
pub fn exe_of(pid: u32) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::System::Threading::{PROCESS_NAME_WIN32, QueryFullProcessImageNameW};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        ok.then(|| std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

/// Sets (or with `None`, deletes) a string value under HKEY_CURRENT_USER.
pub fn set_user_value(key: &str, name: &str, value: Option<&str>, expand: bool) -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_EXPAND_SZ, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
    let (k, n) = (wide(key), wide(name));
    unsafe {
        match value {
            Some(v) => {
                let w = wide(v);
                RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), if expand { REG_EXPAND_SZ } else { REG_SZ }, w.as_ptr() as *const _, (w.len() * 2) as u32) == 0
            }
            None => RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr()) == 0,
        }
    }
}

pub fn user_value(key: &str, name: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ};
    let (k, v) = (wide(key), wide(name));
    let mut buf = vec![0u16; 32768];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut size)
    };
    (rc == 0).then(|| wide_to_string(&buf))
}

/// Tells running programs that environment variables changed (new terminals pick up PATH).
pub fn broadcast_env_change() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};
    let env = wide("Environment");
    unsafe {
        SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as isize, SMTO_ABORTIFHUNG, 2000, std::ptr::null_mut());
    }
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

/// One process from `NtQuerySystemInformation(SystemProcessInformation)`.
struct SysProc {
    pid: u32,
    parent: u32,
    name: String,
    create: i64,
    cpu_ns: u64,
    private_bytes: u64,
}

/// The x64 layout of SYSTEM_PROCESS_INFORMATION up to PagefileUsage (private commit).
#[repr(C)]
#[allow(dead_code)]
struct SysProcRaw {
    next: u32,
    threads: u32,
    ws_private: i64,
    hard_faults: u32,
    threads_high: u32,
    cycles: u64,
    create: i64,
    user: i64,
    kernel: i64,
    name_len: u16,
    name_max: u16,
    name: *const u16,
    priority: i32,
    pid: usize,
    parent: usize,
    handles: u32,
    session: u32,
    key: usize,
    peak_virtual: usize,
    virtual_size: usize,
    faults: u32,
    peak_ws: usize,
    ws: usize,
    peak_paged: usize,
    paged: usize,
    peak_nonpaged: usize,
    nonpaged: usize,
    pagefile: usize,
}

fn system_processes() -> Vec<SysProc> {
    use std::cell::RefCell;
    use windows_sys::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
    thread_local! {
        static BUF: RefCell<Vec<u64>> = RefCell::new(vec![0u64; 128 * 1024]);
    }
    BUF.with_borrow_mut(|buf| unsafe {
        loop {
            let mut need = 0u32;
            let rc = NtQuerySystemInformation(SystemProcessInformation, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32, &mut need);
            if rc == 0xC0000004u32 as i32 && buf.len() < 64 * 1024 * 1024 {
                buf.resize((need as usize / 8 + 1).max(buf.len()) * 5 / 4, 0);
                continue;
            }
            if rc < 0 {
                return Vec::new();
            }
            break;
        }
        let mut out = Vec::with_capacity(512);
        let base = buf.as_ptr() as *const u8;
        let mut off = 0usize;
        loop {
            let p = &*(base.add(off) as *const SysProcRaw);
            let name = if p.name.is_null() {
                if p.pid == 0 { "Idle".to_string() } else { String::new() }
            } else {
                String::from_utf16_lossy(std::slice::from_raw_parts(p.name, p.name_len as usize / 2))
            };
            out.push(SysProc {
                pid: p.pid as u32,
                parent: p.parent as u32,
                name,
                create: p.create,
                cpu_ns: (p.user as u64 + p.kernel as u64) * 100,
                private_bytes: p.pagefile as u64,
            });
            if p.next == 0 {
                break;
            }
            off += p.next as usize;
        }
        out
    })
}

pub fn all_processes() -> Vec<ProcTimes> {
    system_processes()
        .into_iter()
        .map(|p| ProcTimes { pid: p.pid, parent: p.parent, name: p.name, cpu_ns: p.cpu_ns, private_bytes: p.private_bytes })
        .collect()
}

pub fn tree_pids(root: u32) -> Vec<u32> {
    let all = system_processes();
    let Some(r) = all.iter().position(|p| p.pid == root) else { return Vec::new() };
    let mut out = vec![root];
    let mut stack = vec![r];
    while let Some(i) = stack.pop() {
        let parent = &all[i];
        for (j, c) in all.iter().enumerate() {
            if c.parent == parent.pid && c.pid != parent.pid && c.create >= parent.create && !out.contains(&c.pid) {
                out.push(c.pid);
                stack.push(j);
            }
        }
    }
    out
}

pub fn thread_cpu_ns() -> u64 {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    unsafe {
        let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = (zeroed(), zeroed(), zeroed(), zeroed());
        if GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) == 0 {
            return 0;
        }
        ft(&k) + ft(&u)
    }
}

pub fn enable_ansi() -> bool {
    use windows_sys::Win32::System::Console::{ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleMode};
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) == 0 {
            return false;
        }
        mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 || SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

pub fn on_ctrl_c(flag: &'static std::sync::atomic::AtomicBool) {
    use std::sync::atomic::{AtomicPtr, Ordering};
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
    static TARGET: AtomicPtr<std::sync::atomic::AtomicBool> = AtomicPtr::new(std::ptr::null_mut());
    unsafe extern "system" fn handler(kind: u32) -> i32 {
        let flag = TARGET.load(Ordering::SeqCst);
        if (kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT) && !flag.is_null() {
            unsafe { (*flag).store(true, Ordering::SeqCst) };
            return 1;
        }
        0
    }
    if TARGET.swap(flag as *const _ as *mut _, Ordering::SeqCst).is_null() {
        unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
    }
}
