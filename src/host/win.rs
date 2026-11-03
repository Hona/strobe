//! Win32 calls behind `host`. Each function reads an OS API directly; nothing spawns.

use super::{CacheGroup, ProcTimes, Topology};
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE, LocalFree, POINT};
use windows_sys::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromPoint};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, PowerGetActiveScheme, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::System::SystemInformation::{GetLogicalProcessorInformationEx, RelationCache, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessAffinityMask, GetProcessTimes, GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, SetProcessAffinityMask};
use windows_sys::Win32::System::WindowsProgramming::QueryProcessCycleTime;

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
            let (cpu_ns, cycles, private_bytes, working_set) = proc_times(pid);
            out.push(ProcTimes { pid, parent: p.1, name: p.2.clone(), cpu_ns, cycles, private_bytes, working_set });
        }
        for &i in children.get(&pid).map(Vec::as_slice).unwrap_or(&[]) {
            // A reused parent pid can point at an older, unrelated process; children are always newer.
            stack.push(all[i].0);
        }
    }
    out
}

/// CPU time (ns), CPU cycles, private bytes and working set of one process.
fn proc_times(pid: u32) -> (u64, u64, u64, u64) {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return (0, 0, 0, 0);
        }
        let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = (zeroed(), zeroed(), zeroed(), zeroed());
        let cpu = if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0 { ft(&k) + ft(&u) } else { 0 };
        // Cycle time in TSC units, charged at every context switch: exact where process times move in 15.6 ms ticks.
        let mut cycles = 0u64;
        QueryProcessCycleTime(h, &mut cycles);
        let mut m: PROCESS_MEMORY_COUNTERS_EX = zeroed();
        m.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let (private, working_set) = if K32GetProcessMemoryInfo(h, &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb) != 0 { (m.PrivateUsage as u64, m.WorkingSetSize as u64) } else { (0, 0) };
        CloseHandle(h);
        (cpu, cycles, private, working_set)
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

pub fn set_thread_affinity(mask: u64) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
    unsafe {
        SetThreadAffinityMask(GetCurrentThread(), mask as usize);
    }
}

fn reg_string(key: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = vec![0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut size) };
    (rc == 0).then(|| wide_to_string(&buf).trim().to_string())
}

pub fn os_name() -> String {
    let key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build = reg_string(key, "CurrentBuild").unwrap_or_default();
    let version = reg_string(key, "DisplayVersion").unwrap_or_default();
    let name = if build.parse::<u32>().unwrap_or(0) >= 22000 { "Windows 11" } else { "Windows 10" };
    format!("{name} {build}{}", if version.is_empty() { String::new() } else { format!(" ({version})") })
}

/// The hypervisor's name when this machine is a virtual one, from the firmware strings.
pub fn virtual_machine() -> Option<String> {
    let bios = r"HARDWARE\DESCRIPTION\System\BIOS";
    let text = format!("{} {}", reg_string(bios, "SystemManufacturer").unwrap_or_default(), reg_string(bios, "SystemProductName").unwrap_or_default());
    ["VMware", "VirtualBox", "QEMU", "KVM", "Xen", "Parallels", "Virtual Machine", "Bochs"].iter().any(|m| text.contains(m)).then(|| text.trim().to_string())
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
            // A vendor's or the user's own scheme: its name as powercfg shows it.
            _ => super::output(std::process::Command::new("powercfg").arg("/getactivescheme"))
                .and_then(|t| t.split_once('(').and_then(|(_, r)| r.rsplit_once(')')).map(|(n, _)| n.trim().to_string()))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("custom {text}")),
        }
    }
}

pub fn on_battery() -> bool {
    unsafe {
        let mut s: SYSTEM_POWER_STATUS = zeroed();
        GetSystemPowerStatus(&mut s) != 0 && s.ACLineStatus == 0
    }
}

/// Whether this process's session shows the lock screen.
pub fn session_locked() -> bool {
    use windows_sys::Win32::System::RemoteDesktop::{WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTSFreeMemory, WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx};
    unsafe {
        let mut buf: *mut u16 = std::ptr::null_mut();
        let mut bytes = 0u32;
        if WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTSSessionInfoEx, &mut buf, &mut bytes) == 0 || buf.is_null() {
            return false;
        }
        let info = &*(buf as *const WTSINFOEXW);
        let locked = info.Level == 1 && info.Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_LOCK as i32;
        WTSFreeMemory(buf as _);
        locked
    }
}

/// Turns a display that went dark on idle back on: a mouse move of zero pixels, which is user input to
/// Windows but moves nothing. Chromium marks its windows hidden while the display is off.
pub fn wake_display() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput};
    let input = INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 } } };
    unsafe {
        SendInput(1, &input, size_of::<INPUT>() as i32);
    }
}

static DISPLAY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(u8::MAX);
static DISPLAY_CHANGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The console display's state as Windows reports it (`GUID_CONSOLE_DISPLAY_STATE`: 0 off, 1 on,
/// 2 dimmed) and how often it changed, from a power-setting callback registered on first use (it
/// reports the state at once). None when it cannot be registered.
pub fn display_state() -> Option<(u8, u64)> {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::System::Power::{DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, POWERBROADCAST_SETTING, PowerSettingRegisterNotification};
    use windows_sys::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_POWERSETTINGCHANGE};
    const CONSOLE_DISPLAY_STATE: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);
    unsafe extern "system" fn changed(_: *const std::ffi::c_void, kind: u32, setting: *const std::ffi::c_void) -> u32 {
        if kind == PBT_POWERSETTINGCHANGE && !setting.is_null() {
            let s = unsafe { &*(setting as *const POWERBROADCAST_SETTING) };
            if s.DataLength >= 1 {
                let v = s.Data[0];
                if DISPLAY.swap(v, Ordering::Relaxed) != v {
                    DISPLAY_CHANGES.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        0
    }
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    let ok = *REGISTERED.get_or_init(|| unsafe {
        // Lives for the process: the registration is never removed.
        let params = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS { Callback: Some(changed), Context: std::ptr::null_mut() }));
        let mut handle = std::ptr::null_mut();
        PowerSettingRegisterNotification(&CONSOLE_DISPLAY_STATE, DEVICE_NOTIFY_CALLBACK, params as *mut _ as _, &mut handle) == 0
    });
    let v = DISPLAY.load(Ordering::Relaxed);
    (ok && v != u8::MAX).then(|| (v, DISPLAY_CHANGES.load(Ordering::Relaxed)))
}

/// For as long as the calling thread lives (or until changed): keeps the machine awake when `system`,
/// and the display on as well when `display`.
pub fn keep_awake(system: bool, display: bool) {
    use windows_sys::Win32::System::Power::{ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
    let flags = ES_CONTINUOUS | if system { ES_SYSTEM_REQUIRED } else { 0 } | if display { ES_DISPLAY_REQUIRED } else { 0 };
    unsafe {
        SetThreadExecutionState(flags);
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
    let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut size) };
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
        EnumWindows, GetWindowTextLengthW, GetWindowThreadProcessId, HWND_TOPMOST, IsWindowVisible, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowPos,
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
        // Posted, not sent: a window of another process whose UI thread is busy would otherwise block
        // the caller until that thread answers.
        for &h in &state.found {
            SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_ASYNCWINDOWPOS);
        }
    }
    state.found.len()
}

/// See `host::window_pixels`: StretchBlt from the screen with HALFTONE averages each cell.
pub fn window_pixels(pids: &[u32]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HALFTONE, ReleaseDC, SRCCOPY, SelectObject,
        SetStretchBltMode, StretchBlt,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible};
    struct State<'a> {
        pids: &'a [u32],
        best: Option<(i64, RECT)>,
    }
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> i32 {
        let state = unsafe { &mut *(lparam as *mut State) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if state.pids.contains(&pid) && unsafe { IsWindowVisible(hwnd) } != 0 && unsafe { IsIconic(hwnd) } == 0 {
            let mut r: RECT = unsafe { zeroed() };
            if unsafe { GetWindowRect(hwnd, &mut r) } != 0 {
                let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
                if area > 10_000 && state.best.is_none_or(|(a, _)| area > a) {
                    state.best = Some((area, r));
                }
            }
        }
        1
    }
    const N: i32 = 16;
    let mut state = State { pids, best: None };
    unsafe {
        EnumWindows(Some(each), &mut state as *mut State as LPARAM);
        let (_, r) = state.best?;
        let screen = GetDC(std::ptr::null_mut());
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, N, N);
        let old = SelectObject(mem, bmp);
        SetStretchBltMode(mem, HALFTONE);
        let ok = StretchBlt(mem, 0, 0, N, N, screen, r.left, r.top, r.right - r.left, r.bottom - r.top, SRCCOPY) != 0;
        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: N, biHeight: -N, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..zeroed() };
        let mut px = vec![0u8; (N * N * 4) as usize];
        let lines = GetDIBits(mem, bmp, 0, N as u32, px.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS);
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        (ok && lines == N).then_some(px)
    }
}

/// The visible top-level windows of these processes, front to back: handle, title, and whether
/// pointer input passes through it (`WS_EX_TRANSPARENT`).
pub fn top_windows(pids: &[u32]) -> Vec<(isize, String, bool)> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GWL_EXSTYLE, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, WS_EX_TRANSPARENT};
    struct State<'a> {
        pids: &'a [u32],
        out: Vec<(isize, String, bool)>,
    }
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> i32 {
        let state = unsafe { &mut *(lparam as *mut State) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if state.pids.contains(&pid) && unsafe { IsWindowVisible(hwnd) } != 0 {
            let mut t = [0u16; 256];
            let n = unsafe { GetWindowTextW(hwnd, t.as_mut_ptr(), t.len() as i32) }.max(0) as usize;
            let through = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TRANSPARENT != 0;
            state.out.push((hwnd as isize, String::from_utf16_lossy(&t[..n]), through));
        }
        1
    }
    let mut state = State { pids, out: Vec::new() };
    unsafe { EnumWindows(Some(each), &mut state as *mut State as LPARAM) };
    state.out
}

/// What a window draws now, its client area at its own pixels, through the window's own rendering
/// (`PrintWindow` with full content, which composition and flip-model windows answer), as RGBA.
pub fn window_image(hwnd: isize) -> Option<crate::png::Image> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
    };
    use windows_sys::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
    // PW_CLIENTONLY | PW_RENDERFULLCONTENT
    const FLAGS: PRINT_WINDOW_FLAGS = 1 | 2;
    let h = hwnd as windows_sys::Win32::Foundation::HWND;
    unsafe {
        let mut r: RECT = zeroed();
        GetClientRect(h, &mut r);
        let (w, ht) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || ht <= 0 {
            return None;
        }
        let screen = GetDC(std::ptr::null_mut());
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, ht);
        let old = SelectObject(mem, bmp);
        let ok = PrintWindow(h, mem, FLAGS) != 0;
        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -ht, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..zeroed() };
        let mut px = vec![0u8; (w * ht * 4) as usize];
        let lines = GetDIBits(mem, bmp, 0, ht as u32, px.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS);
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        if !ok || lines != ht {
            return None;
        }
        // BGRA to RGBA, opaque.
        for p in px.chunks_mut(4) {
            p.swap(0, 2);
            p[3] = 255;
        }
        Some(crate::png::Image { width: w as usize, height: ht as usize, rgba: px })
    }
}

/// A debug C runtime the executable imports (`ucrtbased.dll`, `vcruntime140d.dll`, …): a debug build
/// of a C or C++ app, or of one that links such code. None when it imports none or is unreadable.
pub fn debug_runtime(exe: &std::path::Path) -> Option<String> {
    let b = std::fs::read(exe).ok()?;
    let u16_at = |o: usize| b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
    let u32_at = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    let pe = u32_at(0x3c)? as usize;
    if b.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let sections = u16_at(pe + 6)? as usize;
    let opt = pe + 24;
    let opt_size = u16_at(pe + 20)? as usize;
    // The import directory: data directory 1, after the PE32 or PE32+ fixed fields.
    let dirs = opt + if u16_at(opt)? == 0x20b { 112 } else { 96 };
    let import_rva = u32_at(dirs + 8)? as usize;
    let rva = |r: usize| -> Option<usize> {
        (0..sections).find_map(|i| {
            let s = opt + opt_size + i * 40;
            let (va, size, raw) = (u32_at(s + 12)? as usize, u32_at(s + 8)?.max(u32_at(s + 16)?) as usize, u32_at(s + 20)? as usize);
            (r >= va && r < va + size).then_some(raw + r - va)
        })
    };
    let mut at = rva(import_rva)?;
    for _ in 0..4096 {
        let name_rva = u32_at(at + 12)? as usize;
        if name_rva == 0 {
            break;
        }
        let o = rva(name_rva)?;
        let end = b[o..].iter().position(|c| *c == 0)? + o;
        let name = String::from_utf8_lossy(&b[o..end]).to_ascii_lowercase();
        if name == "ucrtbased.dll" || (name.starts_with("vcruntime") || name.starts_with("msvcp") || name.starts_with("msvcr")) && name.ends_with("d.dll") {
            return Some(name);
        }
        at += 20;
    }
    None
}

/// The largest visible top-level window of these processes that is not minimized (over 100×100).
pub fn main_window(pids: &[u32]) -> Option<super::TopWindow> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible};
    struct State<'a> {
        pids: &'a [u32],
        best: Option<(i64, HWND, u32)>,
    }
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> i32 {
        let state = unsafe { &mut *(lparam as *mut State) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if state.pids.contains(&pid) && unsafe { IsWindowVisible(hwnd) } != 0 && unsafe { IsIconic(hwnd) } == 0 {
            let mut r: RECT = unsafe { zeroed() };
            if unsafe { GetWindowRect(hwnd, &mut r) } != 0 {
                let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
                if area > 10_000 && state.best.is_none_or(|(a, _, _)| area > a) {
                    state.best = Some((area, hwnd, pid));
                }
            }
        }
        1
    }
    let mut state = State { pids, best: None };
    unsafe { EnumWindows(Some(each), &mut state as *mut State as LPARAM) };
    let (_, hwnd, pid) = state.best?;
    window_state(hwnd as isize).map(|mut w| {
        w.pid = pid;
        w
    })
}

/// A window's place and state now; None once it no longer exists.
pub fn window_state(hwnd: isize) -> Option<super::TopWindow> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible};
    let h = hwnd as windows_sys::Win32::Foundation::HWND;
    unsafe {
        if IsWindow(h) == 0 {
            return None;
        }
        let (mut r, mut c): (RECT, RECT) = (zeroed(), zeroed());
        GetWindowRect(h, &mut r);
        GetClientRect(h, &mut c);
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(h, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut _, 4);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, &mut pid);
        Some(super::TopWindow {
            hwnd,
            pid,
            rect: (r.left, r.top, r.right - r.left, r.bottom - r.top),
            client: (c.right - c.left, c.bottom - c.top),
            visible: IsWindowVisible(h) != 0,
            iconic: IsIconic(h) != 0,
            cloaked: cloaked != 0,
            dpi: GetDpiForWindow(h).max(96),
        })
    }
}

/// Moves a window to `(x, y)` and, with `client`, sizes it so its client area is that many
/// device-independent pixels at the window's DPI; keeps it topmost without activating it. Posted, so
/// a busy UI thread never blocks strobe.
pub fn place_window(w: &super::TopWindow, x: i32, y: i32, client: Option<(u32, u32)>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_TOPMOST, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowPos};
    let border = (w.rect.2 - w.client.0, w.rect.3 - w.client.1);
    let scale = |v: u32| (v as f64 * w.dpi as f64 / 96.0).round() as i32;
    let (cx, cy, size) = match client {
        Some((cw, ch)) => (scale(cw) + border.0, scale(ch) + border.1, 0),
        None => (0, 0, SWP_NOSIZE),
    };
    unsafe {
        SetWindowPos(w.hwnd as _, HWND_TOPMOST, x, y, cx, cy, SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_ASYNCWINDOWPOS | size);
    }
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

pub fn display_of(hwnd: isize) -> Option<super::DisplayPath> {
    use windows_sys::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
        QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
    };
    use windows_sys::Win32::Graphics::Gdi::MonitorFromWindow;
    let text = |w: &[u16]| String::from_utf16_lossy(&w[..w.iter().position(|c| *c == 0).unwrap_or(w.len())]);
    unsafe {
        let mon = MonitorFromWindow(hwnd as _, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFOEXW = zeroed();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(mon, &mut info as *mut _ as *mut _) == 0 {
            return None;
        }
        let device = text(&info.szDevice);
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != 0 {
            return None;
        }
        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = vec![zeroed(); np as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![zeroed(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), std::ptr::null_mut()) != 0 {
            return None;
        }
        paths.truncate(np as usize);
        paths.iter().find_map(|p| {
            let mut name: DISPLAYCONFIG_SOURCE_DEVICE_NAME = zeroed();
            name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            name.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            name.header.adapterId = p.sourceInfo.adapterId;
            name.header.id = p.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut name.header) != 0 || !text(&name.viewGdiDeviceName).eq_ignore_ascii_case(&device) {
                return None;
            }
            let r = p.targetInfo.refreshRate;
            let luid = p.targetInfo.adapterId;
            Some(super::DisplayPath {
                device: device.clone(),
                target: p.targetInfo.id,
                adapter_luid: (luid.HighPart as u32 as u64) << 32 | luid.LowPart as u64,
                hz: if r.Denominator > 0 { r.Numerator as f64 / r.Denominator as f64 } else { 0.0 },
            })
        })
    }
}

/// One process from `NtQuerySystemInformation(SystemProcessInformation)`.
struct SysProc {
    info: super::ProcInfo,
    create: i64,
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
    peak_pagefile: usize,
    private_pages: usize,
    read_ops: i64,
    write_ops: i64,
    other_ops: i64,
    read_bytes: i64,
    write_bytes: i64,
    other_bytes: i64,
}

/// SYSTEM_THREAD_INFORMATION (x64): `threads` of them follow each process record.
#[repr(C)]
#[allow(dead_code)]
struct SysThreadRaw {
    kernel: i64,
    user: i64,
    create: i64,
    wait_time: u32,
    start: usize,
    process: usize,
    thread: usize,
    priority: i32,
    base_priority: i32,
    switches: u32,
    state: u32,
    wait_reason: u32,
}

const _: () = assert!(size_of::<SysProcRaw>() == 0x100 && size_of::<SysThreadRaw>() == 0x50);

/// Calls `f` with every process record of one `NtQuerySystemInformation(SystemProcessInformation)`
/// snapshot and its thread records.
fn each_process(mut f: impl FnMut(&SysProcRaw, &[SysThreadRaw])) -> bool {
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
                return false;
            }
            break;
        }
        let base = buf.as_ptr() as *const u8;
        let mut off = 0usize;
        loop {
            let p = &*(base.add(off) as *const SysProcRaw);
            let threads = std::slice::from_raw_parts(base.add(off + size_of::<SysProcRaw>()) as *const SysThreadRaw, p.threads as usize);
            f(p, threads);
            if p.next == 0 {
                break;
            }
            off += p.next as usize;
        }
        true
    })
}

/// Every thread of `pids` (sorted) from one system snapshot: context switches so far, and each
/// process's IO bytes so far (read, written).
pub fn threads(pids: &[u32]) -> (Vec<super::ThreadInfo>, Vec<(u32, u64, u64)>) {
    let (mut out, mut io) = (Vec::new(), Vec::new());
    each_process(|p, threads| {
        let pid = p.pid as u32;
        if pids.binary_search(&pid).is_err() {
            return;
        }
        io.push((pid, p.read_bytes.max(0) as u64, p.write_bytes.max(0) as u64));
        for t in threads {
            out.push(super::ThreadInfo { pid, tid: t.thread as u32, switches: t.switches, cpu_ns: (t.kernel.max(0) as u64 + t.user.max(0) as u64) * 100 });
        }
    });
    (out, io)
}

/// CPU cycles charged to one thread so far (TSC units), None when it cannot be opened.
pub fn thread_cycles_of(tid: u32) -> Option<u64> {
    use windows_sys::Win32::System::Threading::{OpenThread, THREAD_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;
    unsafe {
        let h = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid);
        if h.is_null() {
            return None;
        }
        let mut c = 0u64;
        let ok = QueryThreadCycleTime(h, &mut c) != 0;
        CloseHandle(h);
        ok.then_some(c)
    }
}

/// The loaded modules of a process: (base, size, path).
fn process_modules(pid: u32) -> Vec<(u64, u64, String)> {
    use windows_sys::Win32::System::ProcessStatus::{K32EnumProcessModulesEx, K32GetModuleFileNameExW, K32GetModuleInformation, LIST_MODULES_ALL, MODULEINFO};
    use windows_sys::Win32::System::Threading::{PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
    let mut out = Vec::new();
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if h.is_null() {
            return out;
        }
        let mut mods = vec![std::ptr::null_mut(); 2048];
        let mut need = 0u32;
        if K32EnumProcessModulesEx(h, mods.as_mut_ptr(), (mods.len() * 8) as u32, &mut need, LIST_MODULES_ALL) != 0 {
            for &m in mods.iter().take((need as usize / 8).min(mods.len())) {
                let mut info: MODULEINFO = zeroed();
                let mut name = vec![0u16; 1024];
                let n = K32GetModuleFileNameExW(h, m, name.as_mut_ptr(), name.len() as u32);
                if n > 0 && K32GetModuleInformation(h, m, &mut info, size_of::<MODULEINFO>() as u32) != 0 {
                    out.push((info.lpBaseOfDll as u64, info.SizeOfImage as u64, String::from_utf16_lossy(&name[..n as usize])));
                }
            }
        }
        CloseHandle(h);
    }
    out
}

/// Where each thread started: the module holding its Win32 start address (`NtQueryInformationThread`
/// `ThreadQuerySetWin32StartAddress`; the system snapshot's start address is `RtlUserThreadStart` for
/// every user thread). Per tid: the module's path.
pub fn thread_modules(threads: &[(u32, u32)]) -> HashMap<u32, String> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::Threading::{OpenThread, THREAD_QUERY_INFORMATION};
    type Query = unsafe extern "system" fn(windows_sys::Win32::Foundation::HANDLE, i32, *mut std::ffi::c_void, u32, *mut u32) -> i32;
    let mut out = HashMap::new();
    let ntdll: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
    let Some(query) = (unsafe { GetProcAddress(GetModuleHandleW(ntdll.as_ptr()), c"NtQueryInformationThread".as_ptr() as *const u8) }) else { return out };
    let query: Query = unsafe { std::mem::transmute(query) };
    let mut modules: HashMap<u32, Vec<(u64, u64, String)>> = HashMap::new();
    for &(pid, tid) in threads {
        let start = unsafe {
            let h = OpenThread(THREAD_QUERY_INFORMATION, 0, tid);
            if h.is_null() {
                continue;
            }
            let mut start = 0usize;
            let ok = query(h, 9, &mut start as *mut usize as *mut _, size_of::<usize>() as u32, std::ptr::null_mut()) >= 0;
            CloseHandle(h);
            if !ok || start == 0 {
                continue;
            }
            start as u64
        };
        let list = modules.entry(pid).or_insert_with(|| process_modules(pid));
        if let Some((_, _, path)) = list.iter().find(|(b, s, _)| start >= *b && start < b + s) {
            out.insert(tid, path.clone());
        }
    }
    out
}

/// A thread's name (`SetThreadDescription`, which Rust's `thread::Builder::name` and most runtimes
/// set), None when it has none or cannot be opened.
pub fn thread_name(tid: u32) -> Option<String> {
    use windows_sys::Win32::System::Threading::{GetThreadDescription, OpenThread, THREAD_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid);
        if h.is_null() {
            return None;
        }
        let mut name: *mut u16 = std::ptr::null_mut();
        let ok = GetThreadDescription(h, &mut name) >= 0 && !name.is_null();
        CloseHandle(h);
        if !ok {
            return None;
        }
        let mut n = 0usize;
        while *name.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(name, n));
        LocalFree(name as _);
        (!s.is_empty()).then_some(s)
    }
}

fn system_processes(names: bool) -> Vec<SysProc> {
    let mut out = Vec::with_capacity(512);
    each_process(|p, _| {
        let name = match (names, p.name.is_null()) {
            (false, _) => String::new(),
            (true, true) => {
                if p.pid == 0 {
                    "Idle".to_string()
                } else {
                    String::new()
                }
            }
            (true, false) => unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(p.name, p.name_len as usize / 2)) },
        };
        out.push(SysProc {
            info: super::ProcInfo {
                pid: p.pid as u32,
                parent: p.parent as u32,
                name,
                cpu_ns: (p.user as u64 + p.kernel as u64) * 100,
                cycles: p.cycles,
                private_bytes: p.pagefile as u64,
                threads: p.threads,
            },
            create: p.create,
        });
    });
    out
}

pub fn all_processes() -> Vec<super::ProcInfo> {
    system_processes(true).into_iter().map(|p| p.info).collect()
}

pub fn tree_pids(root: u32) -> Vec<u32> {
    let all = system_processes(false);
    let Some(r) = all.iter().position(|p| p.info.pid == root) else { return Vec::new() };
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, p) in all.iter().enumerate() {
        if p.info.parent != p.info.pid {
            children.entry(p.info.parent).or_default().push(i);
        }
    }
    let mut out = vec![root];
    let mut stack = vec![r];
    while let Some(i) = stack.pop() {
        let parent = &all[i];
        for &j in children.get(&parent.info.pid).map(Vec::as_slice).unwrap_or(&[]) {
            if all[j].create >= parent.create && out.len() <= all.len() {
                out.push(all[j].info.pid);
                stack.push(j);
            }
        }
    }
    out
}

/// Idle cycles per logical CPU of processor group 0 (SystemProcessorIdleCycleTimeInformation).
pub fn idle_cycles(out: &mut Vec<u64>) -> bool {
    use windows_sys::Wdk::System::SystemInformation::NtQuerySystemInformation;
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(64);
    out.resize(n, 0);
    let mut len = 0u32;
    let rc = unsafe { NtQuerySystemInformation(83, out.as_mut_ptr() as *mut _, (n * 8) as u32, &mut len) };
    out.truncate(len as usize / 8);
    rc >= 0
}

pub fn thread_cycles() -> u64 {
    use windows_sys::Win32::System::Threading::GetCurrentThread;
    use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;
    let mut c = 0u64;
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
    c
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
        unsafe {
            // A process started in a new process group inherits "ignore Ctrl+C": undo that.
            SetConsoleCtrlHandler(None, 0);
            SetConsoleCtrlHandler(Some(handler), 1);
        }
    }
}
