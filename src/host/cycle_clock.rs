//! The clock Windows counts thread, process and idle cycle times in (SPEC §13.6), so cycles become
//! time. Since Windows 11 24H2 the kernel charges a thread the performance-counter ticks it ran times
//! one factor (Cobalt 100 VMs on 25H2: 339.92 to 339.95 per 100 ns tick; a Ryzen 9950X3D on 26300:
//! 430), so cycles over QPC × factor are exact time. Before, it charged raw cycles: the invariant TSC
//! on x64 (cycles over RDTSC are time), the core's clock cycles (PMCCNTR_EL0) on Arm, whose rate
//! follows the clock and which stop while a core idles: not time.
//!
//! Told apart once per boot, before strobe moves its own affinity (`warm`, from `app::dispatch`): a
//! thread pinned to each CPU, all at once, reads its own cycle time 1000 times, each read bracketed by
//! two QPC reads, with a varying wait between reads. A step fits when its whole tick count
//! k = round(Δcycles / factor) lies within the QPC ticks the bracket allows and Δcycles is within 1.5
//! of k × factor; the factor is seeded from Δcycles over ΔQPC on long steps, never from a rated
//! frequency, so a half or a third of it cannot fit (k would exceed the bracket). The verdict is kept
//! in `<state>/cycle-clock.json`, keyed by the boot, the OS build and strobe's build, so a later
//! invocation whose affinity leaves out CPUs reuses it.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// What the kernel's cycle times count, as checked on every CPU.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    /// Performance-counter ticks × `cycles / ticks`, the same factor on every CPU.
    Counter { cycles: u64, ticks: u64 },
    /// Raw cycles: no step fits whole ticks on any CPU.
    Raw,
    /// CPUs that disagree (some charge ticks and some not, or different factors).
    Mixed { why: String },
}

/// The clock `host::tsc` reads, or why cycles are not time here.
#[derive(Clone, Debug)]
pub struct Clock {
    pub unit: Result<Unit, String>,
    /// How strobe knows: `checked` (this invocation), `cached` (earlier this boot), or `unchecked: why`.
    pub how: String,
}

#[derive(Clone, Copy, Debug)]
pub enum Unit {
    /// QPC × cycles / ticks.
    Counter { cycles: u64, ticks: u64 },
    /// RDTSC (x64 only).
    Tsc,
}

/// The cycle clock, checked (or read from this boot's cache) on first use.
pub fn clock() -> &'static Clock {
    static CLOCK: OnceLock<Clock> = OnceLock::new();
    CLOCK.get_or_init(|| {
        let key = boot_key();
        let path = super::paths::state_dir().join("cycle-clock.json");
        let cached = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()).filter(|v| v["key"] == key.as_str()).and_then(|v| serde_json::from_value::<Verdict>(v["verdict"].clone()).ok());
        let (verdict, how) = match cached {
            Some(v) => (Ok(v), "cached".to_string()),
            None => match check() {
                Ok(v) => {
                    let _ = super::paths::write_atomic(&path, serde_json::json!({ "key": key, "verdict": v }).to_string().as_bytes());
                    (Ok(v), "checked".to_string())
                }
                Err(why) => (Err(why.clone()), format!("unchecked: {why}")),
            },
        };
        Clock { unit: unit(verdict), how }
    })
}

/// What `host::tsc` reads for a verdict (Err: the check could not run).
fn unit(verdict: Result<Verdict, String>) -> Result<Unit, String> {
    match verdict {
        Ok(Verdict::Counter { cycles, ticks }) => Ok(Unit::Counter { cycles, ticks }),
        // x64: the TSC is invariant, so raw cycles over it are time (as before 24H2, or unchecked).
        Ok(Verdict::Raw) | Err(_) if cfg!(target_arch = "x86_64") => Ok(Unit::Tsc),
        Ok(Verdict::Raw) => Err("its cycle times are the cores' clock cycles (PMCCNTR_EL0, Windows before 11 24H2), whose rate follows the clock and which stop while a core idles, so they are not time".into()),
        Ok(Verdict::Mixed { why }) => Err(why),
        Err(why) => Err(format!("the unit of its cycle times could not be checked: {why}")),
    }
}

/// The clock now in cycle units, None when cycles are not time.
pub fn read() -> Option<u64> {
    match clock().unit {
        Ok(Unit::Counter { cycles, ticks }) => Some((qpc() as u128 * cycles as u128 / ticks as u128) as u64),
        #[cfg(target_arch = "x86_64")]
        Ok(Unit::Tsc) => Some(unsafe { std::arch::x86_64::_rdtsc() }),
        #[cfg(not(target_arch = "x86_64"))]
        Ok(Unit::Tsc) => None,
        Err(_) => None,
    }
}

/// For `status`: the clock in words.
pub fn describe() -> String {
    let c = clock();
    let freq = qpf() as f64;
    match &c.unit {
        Ok(Unit::Counter { cycles, ticks }) => format!("performance counter × {:.3} cycles a tick ({:.1} MHz; {})", *cycles as f64 / *ticks as f64, *cycles as f64 / *ticks as f64 * freq / 1e6, c.how),
        Ok(Unit::Tsc) => format!("time stamp counter (raw cycles; {})", c.how),
        Err(why) => format!("none: {why}"),
    }
}

fn qpc() -> u64 {
    let mut c = 0i64;
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut c) };
    c as u64
}

fn qpf() -> u64 {
    let mut f = 0i64;
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut f) };
    f.max(1) as u64
}

/// The boot (its time, from SystemTimeOfDayInformation), the OS build and strobe's build.
fn boot_key() -> String {
    use windows_sys::Wdk::System::SystemInformation::NtQuerySystemInformation;
    let mut info = [0i64; 6];
    let mut len = 0u32;
    unsafe { NtQuerySystemInformation(3, info.as_mut_ptr() as *mut _, 48, &mut len) };
    format!("{} · {} · {}", info[0] / 10_000_000, super::win::os_name(), super::build::build_id())
}

/// Every CPU's steps, read at once, then the verdict. Err: the check could not run.
fn check() -> Result<Verdict, String> {
    use windows_sys::Win32::System::Threading::{GetActiveProcessorCount, GetActiveProcessorGroupCount, GetCurrentProcess, GetCurrentThread, GetProcessAffinityMask, SetThreadAffinityMask};
    if unsafe { GetActiveProcessorGroupCount() } > 1 {
        return Err("more than one processor group (over 64 logical CPUs): the check pins to group 0 only".into());
    }
    let n = (unsafe { GetActiveProcessorCount(0) } as usize).clamp(1, 64);
    let (mut allowed, mut system) = (0usize, 0usize);
    unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut allowed, &mut system) };
    if let Some(cpu) = (0..n).find(|c| allowed & (1 << c) == 0) {
        return Err(format!("strobe's CPU affinity leaves out CPU {cpu}, and no check this boot is cached"));
    }
    let reads: Vec<Option<Vec<(u64, u64, u64)>>> = std::thread::scope(|s| {
        let threads: Vec<_> = (0..n)
            .map(|cpu| {
                s.spawn(move || {
                    if unsafe { SetThreadAffinityMask(GetCurrentThread(), 1 << cpu) } == 0 {
                        return None;
                    }
                    let mut out = Vec::with_capacity(1000);
                    for i in 0..1000u32 {
                        // The wait varies so that steps take many lengths: the cores' own cycles then
                        // land anywhere between whole ticks, where a steady loop could cluster them.
                        for j in 0..i * 7919 % 20011 {
                            std::hint::black_box(j);
                        }
                        let q0 = qpc();
                        let c = super::win::thread_cycles();
                        out.push((q0, c, qpc()));
                    }
                    Some(out)
                })
            })
            .collect();
        threads.into_iter().map(|t| t.join().ok().flatten()).collect()
    });
    let mut per_cpu = Vec::with_capacity(n);
    for (cpu, r) in reads.into_iter().enumerate() {
        let r = r.ok_or_else(|| format!("CPU {cpu} could not be pinned to"))?;
        // (Δcycles, fewest ticks, most ticks) between consecutive reads.
        let steps: Vec<(u64, u64, u64)> = r.windows(2).filter(|w| w[1].1 > w[0].1).map(|w| (w[1].1 - w[0].1, w[1].0.saturating_sub(w[0].2), w[1].2 - w[0].0)).collect();
        if steps.len() < 200 {
            return Err(format!("CPU {cpu} gave {} cycle steps of 1000 reads", steps.len()));
        }
        per_cpu.push(fit(&steps)?);
    }
    let fits: Vec<bool> = per_cpu.iter().map(|c| c.fit * 100 >= c.used * 95).collect();
    if fits.iter().all(|f| !f) {
        return Ok(Verdict::Raw);
    }
    if let Some(cpu) = fits.iter().position(|f| !f) {
        let c = &per_cpu[cpu];
        return Ok(Verdict::Mixed { why: format!("CPU {cpu}'s cycle times are not whole performance-counter ticks ({} of {} steps) while others' are, so a process's cycles mix units", c.fit, c.used) });
    }
    let (cycles, ticks) = per_cpu.iter().fold((0u64, 0u64), |(s, t), c| (s + c.cycles, t + c.ticks));
    let all = cycles as f64 / ticks as f64;
    if let Some((cpu, c)) = per_cpu.iter().enumerate().find(|(_, c)| (c.cycles as f64 / c.ticks as f64 / all - 1.0).abs() > 0.002) {
        let mhz = |f: f64| f * qpf() as f64 / 1e6;
        return Ok(Verdict::Mixed { why: format!("CPUs charge cycles at different rates (CPU {cpu}: {:.1} MHz, all: {:.1} MHz), so a process's cycles mix units", mhz(c.cycles as f64 / c.ticks as f64), mhz(all)) });
    }
    Ok(Verdict::Counter { cycles, ticks })
}

/// One CPU's steps against the best factor: Σ Δcycles and Σ ticks of the steps that fit, how many fit
/// of those used (1 to 128 ticks: a 19.2 MHz counter on a fast core still fits).
struct Fit {
    cycles: u64,
    ticks: u64,
    fit: usize,
    used: usize,
}

fn fit(steps: &[(u64, u64, u64)]) -> Result<Fit, String> {
    // The seed: Δcycles over the bracket's middle on long steps (≥ 16 ticks), the median. About 1% off
    // (a thread is not charged for interrupts), which tells k apart on short steps.
    let mut ratios: Vec<f64> = steps.iter().filter(|s| s.1 >= 16).map(|s| s.0 as f64 / ((s.1 + s.2) as f64 / 2.0)).collect();
    if ratios.len() < 50 {
        return Err(format!("only {} long steps to seed the factor from", ratios.len()));
    }
    ratios.sort_by(f64::total_cmp);
    let mut f = ratios[ratios.len() / 2];
    // Below ~50 cycles a tick, steps of any length land within 1.5 of a whole tick too often to tell.
    if f < 50.0 {
        return Err(format!("{f:.1} cycles a performance-counter tick is too few to tell whole ticks from raw cycles"));
    }
    let ticks = |s: u64, f: f64| (s as f64 / f).round() as u64;
    // Refine on short steps first, where a 1% seed still rounds right, then on longer ones.
    for limit in [8, 32, 128] {
        for _ in 0..2 {
            let (c, k) = steps.iter().map(|s| (s.0, ticks(s.0, f))).filter(|&(_, k)| (1..=limit).contains(&k)).fold((0u64, 0u64), |a, (c, k)| (a.0 + c, a.1 + k));
            if k > 0 {
                f = c as f64 / k as f64;
            }
        }
    }
    let used: Vec<&(u64, u64, u64)> = steps.iter().filter(|s| (1..=128).contains(&ticks(s.0, f))).collect();
    let good: Vec<(u64, u64)> = used
        .iter()
        .map(|s| (s.0, ticks(s.0, f), s.1, s.2))
        .filter(|&(c, k, lo, hi)| k >= lo && k <= hi && (c as f64 - k as f64 * f).abs() <= 1.5)
        .map(|(c, k, _, _)| (c, k))
        .collect();
    let (cycles, ticks) = good.iter().fold((0u64, 0u64), |a, &(c, k)| (a.0 + c, a.1 + k));
    Ok(Fit { cycles, ticks: ticks.max(1), fit: good.len(), used: used.len() })
}
