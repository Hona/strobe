//! Starting a child on given CPUs without moving strobe: the child has the mask before it runs any
//! code, and strobe's own threads (the daemon's other work, its samplers) keep theirs.

use std::process::{Child, Command};

/// Spawns `cmd` restricted to the CPUs in `cpus` (None: it inherits strobe's mask, as any child does).
/// strobe's own affinity never changes. macOS has no CPU affinity: there the child runs anywhere.
/// `flags` are Windows process creation flags (`CREATE_NO_WINDOW`, …), ignored elsewhere: a `Command`
/// keeps only the last flags set on it, so they go through here rather than on `cmd`.
pub fn spawn_pinned(cmd: &mut Command, cpus: Option<u64>, flags: u32) -> std::io::Result<Child> {
    #[cfg(windows)]
    {
        windows(cmd, cpus, flags)
    }
    #[cfg(not(windows))]
    {
        let _ = flags;
        #[cfg(target_os = "linux")]
        if let Some(mask) = cpus {
            use std::os::unix::process::CommandExt;
            // Every CPU the machine has, not strobe's own set: strobe may sit on the other CPUs (a
            // bench keeps it on the runner's), and a count from available_parallelism names the
            // lowest CPUs whatever strobe's set is.
            let configured = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) }.clamp(1, 64) as u32;
            let all = if configured >= 64 { u64::MAX } else { (1u64 << configured) - 1 };
            let target = if mask == 0 { all } else { mask & all };
            if target != 0 {
                let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
                for c in (0..64).filter(|c| target >> c & 1 == 1) {
                    unsafe { libc::CPU_SET(c, &mut set) };
                }
                // Between fork and exec only async-signal-safe calls: the set is built before.
                unsafe {
                    cmd.pre_exec(move || {
                        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
                        Ok(())
                    });
                }
            }
        }
        #[cfg(target_os = "macos")]
        let _ = cpus;
        cmd.spawn()
    }
}

/// Created suspended, given the mask, then resumed: its first instruction already runs on those CPUs.
#[cfg(windows)]
fn windows(cmd: &mut Command, cpus: Option<u64>, flags: u32) -> std::io::Result<Child> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
    use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, GetProcessAffinityMask, OpenThread, ResumeThread, SetProcessAffinityMask, THREAD_SUSPEND_RESUME};
    let Some(mask) = cpus else { return cmd.creation_flags(flags).spawn() };
    let mut child = cmd.creation_flags(flags | CREATE_SUSPENDED).spawn()?;
    let h = child.as_raw_handle();
    unsafe {
        let (mut proc_mask, mut sys_mask) = (0usize, 0usize);
        if GetProcessAffinityMask(h, &mut proc_mask, &mut sys_mask) != 0 {
            let target = if mask == 0 { sys_mask } else { mask as usize & sys_mask };
            if target != 0 {
                SetProcessAffinityMask(h, target);
            }
        }
        // A process created suspended has its one thread: resume it.
        let mut resumed = false;
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap != INVALID_HANDLE_VALUE {
            let mut e: THREADENTRY32 = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut ok = Thread32First(snap, &mut e) != 0;
            while ok {
                if e.th32OwnerProcessID == child.id() {
                    let t = OpenThread(THREAD_SUSPEND_RESUME, 0, e.th32ThreadID);
                    if !t.is_null() {
                        resumed |= ResumeThread(t) != u32::MAX;
                        CloseHandle(t);
                    }
                }
                ok = Thread32Next(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        if !resumed {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other("could not resume the pinned child"));
        }
    }
    Ok(child)
}
