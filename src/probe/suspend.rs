//! Native stacks without elevation (Windows x64, attribution only, SPEC §13.4): when the deep-mode
//! helper is not installed, strobe samples the target's own threads from its own process, as a
//! debugger would, which a same-user process may do. Every 2 ms it reads the cycle counter of each
//! thread that ran in the last 100 ms, and a thread that used another 2 ms of CPU since its last
//! sample is sampled: suspended, its registers read (`GetThreadContext`, which waits for the suspend
//! to land) and up to 64 KB of its stack copied in one `ReadProcessMemory`, then resumed. Unwinding
//! happens after the resume, on that copy, with the x64 unwind data (`.pdata` and `UNWIND_INFO`) of
//! the modules in the target's memory, as `RtlVirtualUnwind` does in a process for itself. So each
//! sample stands for 2 ms of that thread's CPU, as the kernel's sampled profile does for 200 µs.
//!
//! Caveats, said in the recording: no kernel frames (the user-mode part only); only threads that
//! exist at a refresh (every 100 ms) are sampled; a thread that ran and is waiting when sampled shows
//! where it waits; code without unwind data (a JIT's) ends the stack there; each sample stalls the
//! thread for the time between suspend and resume (measured per sample and reported), and while it is
//! stopped it may hold a lock (the loader's, a heap's) another of the app's threads then waits on.
//! strobe takes no lock of the target and calls nothing in it, so the target cannot stall strobe.
//! Samples are capped at one per thread per 2 ms and 4,000 a second in all.

use crate::etw::{Deep, Module, SuspendStats};
use crate::host;
use crate::model::{Error, Result};
use std::collections::HashMap;
use std::sync::mpsc;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::{CONTEXT, CONTEXT_CONTROL_AMD64, CONTEXT_INTEGER_AMD64, GetThreadContext, ReadProcessMemory};
use windows_sys::Win32::System::Threading::{
    CreateWaitableTimerExW, OpenProcess, OpenThread, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, ResumeThread, SetWaitableTimer, SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION,
    THREAD_SUSPEND_RESUME, WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

/// The sampling interval: one sample per 2 ms of a thread's CPU.
const INTERVAL_US: u64 = 2000;
/// Samples a second at most, over all threads.
const MAX_RATE: u64 = 4000;
/// Stack bytes copied per sample.
const STACK_BYTES: usize = 64 << 10;

pub struct Capture {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<Result<Deep>>>,
}

impl Capture {
    pub fn start(root: u32) -> Result<Capture> {
        if !cfg!(target_arch = "x86_64") {
            return Err(Error::msg("native stacks without the deep helper unwind x64 code only"));
        }
        if host::tree_pids(root).is_empty() {
            return Err(Error::msg(format!("process {root} is not running")));
        }
        let (stop, rx) = mpsc::channel();
        let thread = std::thread::Builder::new().name("strobe-stacks".into()).spawn(move || run(root, rx)).map_err(|e| Error::msg(format!("the stack sampler thread: {e}")))?;
        Ok(Capture { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) -> Result<Deep> {
        let _ = self.stop.send(());
        self.thread.take().ok_or_else(|| Error::msg("no stack sampler thread"))?.join().map_err(|_| Error::msg("the stack sampler thread panicked"))?
    }
}

/// Dropped without `stop`: the sampler ends at its next 2 ms tick (never between a suspend and its
/// resume), closing every thread and process handle, joined within 2 s.
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            super::join_within(t, std::time::Duration::from_secs(2));
        }
    }
}

struct Thread {
    pid: u32,
    handle: HANDLE,
    cycles: u64,
    /// CPU since the last sample, in samples' worth.
    credit: f64,
    stack_base: Option<u64>,
}

impl Drop for Thread {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

struct Process {
    handle: HANDLE,
    modules: Vec<Image>,
    /// When the module list was read (a new module is found on the next miss after 200 ms).
    read_at: std::time::Instant,
    /// Unwind data read so far, by address (None: unreadable).
    unwind: HashMap<u64, Option<Vec<u8>>>,
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

/// One module in the target's memory and its function table, read once.
struct Image {
    base: u64,
    size: u64,
    path: String,
    /// (begin RVA, end RVA, UNWIND_INFO RVA), sorted.
    functions: Option<Vec<(u32, u32, u32)>>,
}

#[repr(C, align(16))]
struct Aligned(CONTEXT);

fn run(root: u32, rx: mpsc::Receiver<()>) -> Result<Deep> {
    let t0 = host::now_ns();
    // A sampler preempted between a suspend and its resume stalls the target's thread with it: run
    // above normal threads (a 75 ms stall once at normal priority).
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    let hz = tsc_hz();
    let per_sample = hz * INTERVAL_US as f64 / 1e6;
    let timer = unsafe { CreateWaitableTimerExW(std::ptr::null(), std::ptr::null(), 0x2, 0x1F0003) };
    let mut processes: HashMap<u32, Process> = HashMap::new();
    let mut threads: HashMap<u32, Thread> = HashMap::new();
    let mut busy: Vec<u32> = Vec::new();
    let mut stalls: Vec<f64> = Vec::new();
    let (mut capped, mut unopened) = (0u64, 0usize);
    let mut stacks: Vec<Vec<u64>> = Vec::new();
    let mut index: HashMap<Vec<u64>, u32> = HashMap::new();
    let mut samples = Vec::new();
    let mut last_refresh: Option<(std::time::Instant, HashMap<u32, u64>)> = None;
    let mut second = (std::time::Instant::now(), 0u64);
    let mut stack = vec![0u8; STACK_BYTES];
    loop {
        match rx.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        // Every 100 ms: the tree's threads, and which of them ran since the last look.
        if last_refresh.as_ref().is_none_or(|(at, _)| at.elapsed() >= std::time::Duration::from_millis(100)) {
            let mut pids = host::tree_pids(root);
            pids.sort_unstable();
            let (now, _) = host::threads(&pids);
            let cpu: HashMap<u32, u64> = now.iter().map(|t| (t.tid, t.cpu_ns)).collect();
            busy = now.iter().filter(|t| last_refresh.as_ref().is_some_and(|(_, before)| before.get(&t.tid).is_none_or(|c| *c < t.cpu_ns))).map(|t| t.tid).collect();
            for t in &now {
                if threads.contains_key(&t.tid) || !busy.contains(&t.tid) {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(e) = processes.entry(t.pid) {
                    let h = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, t.pid) };
                    if h.is_null() || wow64(h) {
                        if !h.is_null() {
                            unsafe { CloseHandle(h) };
                        }
                        unopened += 1;
                        continue;
                    }
                    e.insert(Process { handle: h, modules: images(h), read_at: std::time::Instant::now(), unwind: HashMap::new() });
                }
                let h = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, t.tid) };
                if h.is_null() {
                    unopened += 1;
                    continue;
                }
                let mut cycles = 0;
                unsafe { QueryThreadCycleTime(h, &mut cycles) };
                threads.insert(t.tid, Thread { pid: t.pid, handle: h, cycles, credit: 0.0, stack_base: None });
            }
            threads.retain(|tid, _| cpu.contains_key(tid));
            last_refresh = Some((std::time::Instant::now(), cpu));
        }
        if second.0.elapsed() >= std::time::Duration::from_secs(1) {
            second = (std::time::Instant::now(), 0);
        }
        for tid in &busy {
            let Some(t) = threads.get_mut(tid) else { continue };
            let mut cycles = 0;
            if unsafe { QueryThreadCycleTime(t.handle, &mut cycles) } == 0 {
                continue;
            }
            t.credit = (t.credit + cycles.saturating_sub(t.cycles) as f64 / per_sample).min(2.0);
            t.cycles = cycles;
            if t.credit < 1.0 {
                continue;
            }
            t.credit -= 1.0;
            if second.1 >= MAX_RATE {
                capped += 1;
                continue;
            }
            let Some(p) = processes.get_mut(&t.pid) else { continue };
            let Some((at, stall, regs, len)) = take(t, p.handle, &mut stack) else { continue };
            second.1 += 1;
            stalls.push(stall);
            let frames = unwind(p, regs, &stack[..len]);
            if frames.is_empty() {
                continue;
            }
            let next = stacks.len() as u32;
            let at_stack = *index.entry(frames.clone()).or_insert_with(|| {
                stacks.push(frames);
                next
            });
            samples.push((at, t.pid, *tid, 0u16, at_stack));
        }
        // Wait out the rest of the interval on a high-resolution timer (no change to the system's
        // timer resolution), or sleep where there is none.
        if timer.is_null() {
            std::thread::sleep(std::time::Duration::from_micros(INTERVAL_US));
        } else {
            let due: i64 = -((INTERVAL_US * 10) as i64);
            unsafe {
                SetWaitableTimer(timer, &due, 0, None, std::ptr::null(), 0);
                WaitForSingleObject(timer, 100);
            }
        }
    }
    if !timer.is_null() {
        unsafe { CloseHandle(timer) };
    }
    stalls.sort_by(f64::total_cmp);
    let stats = SuspendStats {
        samples: samples.len() as u64,
        threads: samples.iter().map(|s| s.2).collect::<std::collections::BTreeSet<u32>>().len(),
        stall_us_mean: if stalls.is_empty() { 0.0 } else { stalls.iter().sum::<f64>() / stalls.len() as f64 },
        stall_us_p99: stalls.get((stalls.len() as f64 * 0.99) as usize).or(stalls.last()).copied().unwrap_or(0.0),
        stall_us_max: stalls.last().copied().unwrap_or(0.0),
        capped,
        unopened,
    };
    let mut modules = Vec::new();
    for (pid, p) in &processes {
        modules.extend(p.modules.iter().map(|m| Module { pid: *pid, base: m.base, size: m.size, path: m.path.clone() }));
    }
    let mut pids: Vec<u32> = processes.keys().copied().collect();
    pids.sort_unstable();
    Ok(Deep { interval_us: INTERVAL_US as f64, t0_ns: t0, t1_ns: host::now_ns(), modules, stacks, samples, pids, suspend: Some(stats), ..Deep::default() })
}

/// One sample: suspend, registers, the stack from RSP up, resume. (host ns, stall µs, registers,
/// bytes of stack copied.)
fn take(t: &mut Thread, process: HANDLE, stack: &mut [u8]) -> Option<(u64, f64, [u64; 17], usize)> {
    let base = match t.stack_base {
        Some(b) => b,
        None => {
            let b = stack_base(t.handle, process)?;
            t.stack_base = Some(b);
            b
        }
    };
    let mut ctx: Aligned = unsafe { std::mem::zeroed() };
    ctx.0.ContextFlags = CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64;
    let at = host::now_ns();
    if unsafe { SuspendThread(t.handle) } == u32::MAX {
        return None;
    }
    let ok = unsafe { GetThreadContext(t.handle, &mut ctx.0) } != 0;
    let c = &ctx.0;
    let len = if ok { (base.saturating_sub(c.Rsp) as usize).min(stack.len()) } else { 0 };
    let mut read = 0usize;
    if len > 0 {
        unsafe { ReadProcessMemory(process, c.Rsp as *const _, stack.as_mut_ptr() as *mut _, len, &mut read) };
    }
    unsafe { ResumeThread(t.handle) };
    let stall = (host::now_ns() - at) as f64 / 1e3;
    if !ok {
        return None;
    }
    let regs = [c.Rax, c.Rcx, c.Rdx, c.Rbx, c.Rsp, c.Rbp, c.Rsi, c.Rdi, c.R8, c.R9, c.R10, c.R11, c.R12, c.R13, c.R14, c.R15, c.Rip];
    Some((at, stall, regs, read))
}

/// The top of a thread's stack, from its TEB (`NT_TIB.StackBase`).
fn stack_base(thread: HANDLE, process: HANDLE) -> Option<u64> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    type Query = unsafe extern "system" fn(HANDLE, i32, *mut std::ffi::c_void, u32, *mut u32) -> i32;
    let ntdll: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
    let query: Query = unsafe { std::mem::transmute(GetProcAddress(GetModuleHandleW(ntdll.as_ptr()), c"NtQueryInformationThread".as_ptr() as *const u8)?) };
    // THREAD_BASIC_INFORMATION: ExitStatus, then TebBaseAddress at 8.
    let mut info = [0u64; 6];
    if unsafe { query(thread, 0, info.as_mut_ptr() as *mut _, 48, std::ptr::null_mut()) } < 0 {
        return None;
    }
    read_u64(process, info[1] + 8)
}

fn read_u64(process: HANDLE, at: u64) -> Option<u64> {
    let mut v = 0u64;
    let mut n = 0usize;
    (unsafe { ReadProcessMemory(process, at as *const _, &mut v as *mut u64 as *mut _, 8, &mut n) } != 0 && n == 8).then_some(v)
}

fn read_bytes(process: HANDLE, at: u64, len: usize) -> Option<Vec<u8>> {
    let mut v = vec![0u8; len];
    let mut n = 0usize;
    (unsafe { ReadProcessMemory(process, at as *const _, v.as_mut_ptr() as *mut _, len, &mut n) } != 0 && n == len).then_some(v)
}

fn wow64(process: HANDLE) -> bool {
    use windows_sys::Win32::System::Threading::IsWow64Process;
    let mut w = 0;
    unsafe { IsWow64Process(process, &mut w) != 0 && w != 0 }
}

/// The process's modules (base, size, path); their function tables are read when first needed.
fn images(process: HANDLE) -> Vec<Image> {
    use windows_sys::Win32::System::ProcessStatus::{K32EnumProcessModulesEx, K32GetModuleFileNameExW, K32GetModuleInformation, LIST_MODULES_ALL, MODULEINFO};
    let mut out = Vec::new();
    let mut mods = vec![std::ptr::null_mut(); 2048];
    let mut need = 0u32;
    unsafe {
        if K32EnumProcessModulesEx(process, mods.as_mut_ptr(), (mods.len() * 8) as u32, &mut need, LIST_MODULES_ALL) == 0 {
            return out;
        }
        for &m in mods.iter().take((need as usize / 8).min(mods.len())) {
            let mut info: MODULEINFO = std::mem::zeroed();
            let mut name = vec![0u16; 1024];
            let n = K32GetModuleFileNameExW(process, m, name.as_mut_ptr(), name.len() as u32);
            if n > 0 && K32GetModuleInformation(process, m, &mut info, std::mem::size_of::<MODULEINFO>() as u32) != 0 {
                out.push(Image { base: info.lpBaseOfDll as u64, size: info.SizeOfImage as u64, path: String::from_utf16_lossy(&name[..n as usize]), functions: None });
            }
        }
    }
    out
}

/// A module's `RUNTIME_FUNCTION` table (the exception directory), from its headers in memory.
fn functions(process: HANDLE, base: u64) -> Vec<(u32, u32, u32)> {
    let read = || -> Option<Vec<(u32, u32, u32)>> {
        let pe = base + read_bytes(process, base + 0x3C, 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))? as u64;
        // PE32+ optional header at pe + 24; its data directories at +112; the exception one is 3rd.
        let dir = read_bytes(process, pe + 24 + 112 + 3 * 8, 8)?;
        let (rva, size) = (u32::from_le_bytes([dir[0], dir[1], dir[2], dir[3]]), u32::from_le_bytes([dir[4], dir[5], dir[6], dir[7]]));
        if rva == 0 || size < 12 {
            return Some(Vec::new());
        }
        let raw = read_bytes(process, base + rva as u64, size as usize - size as usize % 12)?;
        let mut f: Vec<(u32, u32, u32)> = raw.as_chunks::<12>().0.iter().map(|c| (u32::from_le_bytes([c[0], c[1], c[2], c[3]]), u32::from_le_bytes([c[4], c[5], c[6], c[7]]), u32::from_le_bytes([c[8], c[9], c[10], c[11]]))).collect();
        f.sort_unstable();
        Some(f)
    };
    read().unwrap_or_default()
}

/// The stack as return addresses, innermost first: x64 unwinding over the copied stack (from
/// `regs[4]`, RSP) with each module's unwind data, until an address outside any module, outside the
/// copy, or 128 frames.
fn unwind(p: &mut Process, mut regs: [u64; 17], stack: &[u8]) -> Vec<u64> {
    let lo = regs[4];
    let mem = |at: u64| -> Option<u64> {
        let off = at.checked_sub(lo)? as usize;
        stack.get(off..off + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
    };
    let mut frames = Vec::new();
    for depth in 0..128 {
        let ip = regs[16];
        if ip == 0 {
            break;
        }
        frames.push(ip);
        // A caller's address is a return address: look up the call instruction before it.
        let at = if depth == 0 { ip } else { ip - 1 };
        let found = p.modules.iter().position(|m| at >= m.base && at < m.base + m.size);
        let i = match found {
            Some(i) => i,
            // A module loaded since the list was read: read it again, at most every 200 ms.
            None if p.read_at.elapsed() > std::time::Duration::from_millis(200) => {
                p.modules = images(p.handle);
                p.read_at = std::time::Instant::now();
                match p.modules.iter().position(|m| at >= m.base && at < m.base + m.size) {
                    Some(i) => i,
                    None => break,
                }
            }
            None => break,
        };
        let (handle, base) = (p.handle, p.modules[i].base);
        let table = p.modules[i].functions.get_or_insert_with(|| functions(handle, base));
        let rva = (at - base) as u32;
        let f = match table.binary_search_by(|f| if rva < f.0 { std::cmp::Ordering::Greater } else if rva >= f.1 { std::cmp::Ordering::Less } else { std::cmp::Ordering::Equal }) {
            Ok(k) => Some(table[k]),
            Err(_) => None,
        };
        let ok = match f {
            // A leaf function (no unwind data): the return address is at RSP.
            None => pop_return(&mut regs, &mem),
            Some((begin, _, info)) => (depth == 0 && epilog(handle, &mut regs, &mem)) || unwind_info(p, base, info, rva - begin, true, &mut regs, &mem).is_some_and(|machine| machine || pop_return(&mut regs, &mem)),
        };
        if !ok || regs[4] < lo {
            break;
        }
    }
    frames
}

fn pop_return(regs: &mut [u64; 17], mem: &dyn Fn(u64) -> Option<u64>) -> bool {
    let Some(ret) = mem(regs[4]) else { return false };
    regs[16] = ret;
    regs[4] += 8;
    true
}

/// Applies a function's unwind codes (those whose prolog instruction already ran, for the primary
/// entry), then a chained entry's. Some(true) when a machine frame set RIP and RSP already.
fn unwind_info(p: &mut Process, base: u64, rva: u32, offset: u32, primary: bool, regs: &mut [u64; 17], mem: &dyn Fn(u64) -> Option<u64>) -> Option<bool> {
    let at = base + rva as u64;
    let handle = p.handle;
    let info = p.unwind.entry(at).or_insert_with(|| {
        let head = read_bytes(handle, at, 4)?;
        let (flags, count) = (head[0] >> 3, head[2] as usize);
        let len = count * 2 + if flags & 4 != 0 { 12 + (count % 2) * 2 } else { 0 };
        let mut all = head;
        all.extend(if len > 0 { read_bytes(handle, at + 4, len)? } else { Vec::new() });
        Some(all)
    });
    let info = info.clone()?;
    let (version, flags, count, frame) = (info[0] & 7, info[0] >> 3, info[2] as usize, info[3]);
    let codes = &info[4..];
    let slot = |k: usize| u16::from_le_bytes([codes[2 * k], codes[2 * k + 1]]);
    let mut k = 0;
    while k < count {
        let (at, op, info) = (codes[2 * k] as u32, codes[2 * k + 1] & 0xF, (codes[2 * k + 1] >> 4) as usize);
        let slots = match op {
            0 | 2 | 3 | 10 => 1,
            1 => if info == 0 { 2 } else { 3 },
            4 | 8 => 2,
            5 | 9 => 3,
            6 => if version >= 2 { 1 } else { 2 },
            7 => if version >= 2 { 2 } else { 3 },
            _ => return None,
        };
        if k + slots > count {
            return None;
        }
        if !primary || at <= offset {
            match op {
                0 => {
                    regs[info] = mem(regs[4])?;
                    regs[4] += 8;
                }
                1 => regs[4] += if info == 0 { slot(k + 1) as u64 * 8 } else { slot(k + 1) as u64 | (slot(k + 2) as u64) << 16 },
                2 => regs[4] += info as u64 * 8 + 8,
                3 => regs[4] = regs[(frame & 0xF) as usize].wrapping_sub(16 * (frame >> 4) as u64),
                4 => regs[info] = mem(regs[4] + slot(k + 1) as u64 * 8)?,
                5 => regs[info] = mem(regs[4] + (slot(k + 1) as u64 | (slot(k + 2) as u64) << 16))?,
                10 => {
                    if info == 1 {
                        regs[4] += 8;
                    }
                    regs[16] = mem(regs[4])?;
                    regs[4] = mem(regs[4] + 24)?;
                    return Some(true);
                }
                _ => {}
            }
        }
        k += slots;
    }
    if flags & 4 != 0 {
        let at = (count + count % 2) * 2;
        let chained = u32::from_le_bytes([codes[at + 8], codes[at + 9], codes[at + 10], codes[at + 11]]);
        return unwind_info(p, base, chained, 0, false, regs, mem);
    }
    Some(false)
}

/// When RIP is in an epilog (`add rsp`/`lea rsp`, pops, then `ret` or a tail `jmp`), runs the rest
/// of it as RtlVirtualUnwind does; false when it is not in one.
fn epilog(process: HANDLE, regs: &mut [u64; 17], mem: &dyn Fn(u64) -> Option<u64>) -> bool {
    let Some(code) = read_bytes(process, regs[16], 32) else { return false };
    let mut r = *regs;
    let mut i = 0;
    let rex = |b: u8| b & 0xF8 == 0x48;
    // The first instruction may restore RSP.
    if code.len() >= 4 && rex(code[0]) && code[1] == 0x83 && code[2] == 0xC4 {
        r[4] = r[4].wrapping_add(code[3] as i8 as i64 as u64);
        i = 4;
    } else if code.len() >= 7 && rex(code[0]) && code[1] == 0x81 && code[2] == 0xC4 {
        r[4] = r[4].wrapping_add(i32::from_le_bytes([code[3], code[4], code[5], code[6]]) as i64 as u64);
        i = 7;
    } else if code.len() >= 4 && rex(code[0]) && code[1] == 0x8D && (code[2] >> 3) & 7 == 4 && code[2] >> 6 != 0 && code[2] >> 6 != 3 {
        // lea rsp, [reg + disp]
        let base = (code[2] & 7) as usize + if code[0] & 1 != 0 { 8 } else { 0 };
        let mut at = 3;
        if code[2] & 7 == 4 {
            at += 1; // SIB (rsp base)
        }
        let disp = if code[2] >> 6 == 1 { code[at] as i8 as i64 } else { i32::from_le_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]]) as i64 };
        r[4] = r[base].wrapping_add(disp as u64);
        i = at + if code[2] >> 6 == 1 { 1 } else { 4 };
    }
    loop {
        match code.get(i..) {
            Some([0x58..=0x5F, ..]) => {
                let reg = (code[i] - 0x58) as usize;
                let Some(v) = mem(r[4]) else { return false };
                r[reg] = v;
                r[4] += 8;
                i += 1;
            }
            Some([0x41, 0x58..=0x5F, ..]) => {
                let reg = (code[i + 1] - 0x58) as usize + 8;
                let Some(v) = mem(r[4]) else { return false };
                r[reg] = v;
                r[4] += 8;
                i += 2;
            }
            // A tail call's jump ends an epilog only after something of one: alone it is a branch.
            Some([0xC3, ..]) | Some([0xC2, ..]) => return finish(regs, r, mem),
            Some([0xE9, ..]) | Some([0x48, 0xFF, 0x25, ..]) | Some([0xFF, 0x25, ..]) if i > 0 => return finish(regs, r, mem),
            _ => return false,
        }
    }
}

/// The epilog's return: RIP from the stack.
fn finish(regs: &mut [u64; 17], mut r: [u64; 17], mem: &dyn Fn(u64) -> Option<u64>) -> bool {
    let Some(ret) = mem(r[4]) else { return false };
    r[16] = ret;
    r[4] += 8;
    *regs = r;
    true
}

/// The time stamp counter's rate (cycles per second), measured over 20 ms.
fn tsc_hz() -> f64 {
    let (c0, t0) = (host::tsc().unwrap_or(0), host::now_ns());
    std::thread::sleep(std::time::Duration::from_millis(20));
    let (c1, t1) = (host::tsc().unwrap_or(0), host::now_ns());
    (c1.saturating_sub(c0)) as f64 / ((t1 - t0) as f64 / 1e9)
}
