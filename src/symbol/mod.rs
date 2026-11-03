//! Native symbols (SPEC §12.2): a recording's native samples (`deep.json.gz`) as located frames
//! and packages, ready for the views that rank JavaScript samples (hotspots, insights, reports).

pub mod pdb;

use crate::etw::Deep;
use crate::model::Result;
use crate::probe::symbols::Frame;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::Path;

const KERNEL: u64 = 0xFFFF_8000_0000_0000;

pub fn read_deep(dir: &Path) -> Result<Option<Deep>> {
    let path = dir.join("deep.json.gz");
    if !path.is_file() {
        return Ok(None);
    }
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&path)?).read_to_string(&mut text)?;
    Ok(Some(serde_json::from_str(&text)?))
}

pub fn write_gz(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(path)?, flate2::Compression::fast());
    gz.write_all(&serde_json::to_vec(value)?)?;
    gz.finish()?;
    Ok(())
}

/// Native samples ranked like a CPU profile: (stack root first, µs, samples), plus what resolved.
pub struct Native {
    pub weights: Vec<(Vec<Frame>, f64, u64)>,
    pub interval_us: f64,
    pub samples: u64,
    /// Samples whose innermost frame has a function name.
    pub resolved: u64,
    /// Modules with samples and no symbols (no PDB found), with their sample counts.
    pub unsymbolized: BTreeMap<String, u64>,
}

/// One resolved address: its frames innermost first (inline frames expanded).
type Resolved = Vec<(String, String, u32, String, bool)>;

/// Symbolizes a recording's native samples (cached beside them in `deep-symbols.json.gz`).
/// `thread`: keep only threads whose name contains it (or whose id it is).
pub fn native(dir: &Path, thread: Option<&str>) -> Result<Option<Native>> {
    let Some(deep) = read_deep(dir)? else { return Ok(None) };
    let resolved = resolve_all(dir, &deep)?;
    let mut by_stack: HashMap<(u32, u32), u64> = HashMap::new();
    for s in &deep.samples {
        if let Some(t) = thread {
            let name = deep.threads.get(&s.2).map(String::as_str).unwrap_or("");
            if !name.contains(t) && t != s.2.to_string() {
                continue;
            }
        }
        *by_stack.entry((s.4, s.1)).or_default() += 1;
    }
    let mut out = Native { weights: Vec::new(), interval_us: deep.interval_us, samples: 0, resolved: 0, unsymbolized: BTreeMap::new() };
    for ((stack, pid), n) in by_stack {
        let Some(addrs) = deep.stacks.get(stack as usize) else { continue };
        let mut frames: Vec<Frame> = Vec::new();
        for (i, &a) in addrs.iter().enumerate() {
            let key = addr_key(pid, a, i == 0);
            let Some(r) = resolved.get(&key) else { continue };
            for (name, file, line, package, ok) in r {
                if i == 0 && frames.is_empty() {
                    if *ok {
                        out.resolved += n;
                    } else {
                        *out.unsymbolized.entry(package.clone()).or_default() += n;
                    }
                }
                frames.push(Frame { name: name.clone(), file: file.clone(), line: *line as i64, column: 0, url: String::new(), mapped: false, name_from: None, package: Some(package.clone()) });
            }
        }
        if frames.is_empty() {
            continue;
        }
        frames.reverse();
        out.samples += n;
        out.weights.push((frames, n as f64 * deep.interval_us, n));
    }
    Ok(Some(out))
}

fn addr_key(pid: u32, addr: u64, leaf: bool) -> String {
    format!("{}:{addr:x}:{}", if addr >= KERNEL { 0 } else { pid }, u8::from(leaf))
}

fn resolve_all(dir: &Path, deep: &Deep) -> Result<HashMap<String, Resolved>> {
    let cache = dir.join("deep-symbols.json.gz");
    if let Ok(f) = std::fs::File::open(&cache) {
        let mut text = String::new();
        if flate2::read::GzDecoder::new(f).read_to_string(&mut text).is_ok()
            && let Ok(v) = serde_json::from_str::<HashMap<String, Resolved>>(&text)
        {
            return Ok(v);
        }
    }
    // Modules per process, by base; the kernel's under pid 0.
    let mut modules: HashMap<u32, Vec<&crate::etw::Module>> = HashMap::new();
    for m in &deep.modules {
        modules.entry(m.pid).or_default().push(m);
    }
    for v in modules.values_mut() {
        v.sort_by_key(|m| m.base);
    }
    let find = |pid: u32, a: u64| -> Option<&crate::etw::Module> {
        let list = modules.get(&if a >= KERNEL { 0 } else { pid })?;
        let i = list.partition_point(|m| m.base <= a).checked_sub(1)?;
        let m = list[i];
        (a < m.base + m.size.max(1)).then_some(m)
    };
    // Every distinct address, grouped by module file; a return address (not the leaf) looks up the call.
    let mut wanted: BTreeMap<String, Vec<(String, u32)>> = BTreeMap::new();
    let mut out: HashMap<String, Resolved> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for s in &deep.samples {
        if !seen.insert((s.4, s.1)) {
            continue;
        }
        for (i, &a) in deep.stacks.get(s.4 as usize).into_iter().flatten().enumerate() {
            let key = addr_key(s.1, a, i == 0);
            if out.contains_key(&key) {
                continue;
            }
            match find(s.1, a) {
                Some(m) => {
                    let rva = (a - m.base).saturating_sub(u64::from(i > 0)) as u32;
                    wanted.entry(m.path.clone()).or_default().push((key.clone(), rva));
                    out.insert(key, Vec::new());
                }
                None => {
                    let kernel = a >= KERNEL;
                    out.insert(key, vec![(if kernel { "(kernel)".into() } else { "(unknown module)".into() }, String::new(), 0, if kernel { "(kernel)".into() } else { "(unknown)".into() }, false)]);
                }
            }
        }
    }
    let servers: Vec<String> = crate::config::load(&crate::config::project_root(&std::env::current_dir().unwrap_or_default()))["symbols"]["servers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect();
    // Each module's PDB is its own read (and maybe download): several at once.
    // (address keys and module-relative addresses, module path, frames per address when its PDB was found)
    type Job = (Vec<(String, u32)>, String, Option<Vec<Vec<pdb::Located>>>);
    let jobs: Vec<(String, Vec<(String, u32)>)> = wanted.into_iter().collect();
    let results: Vec<Job> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .chunks(jobs.len().div_ceil(8).max(1))
            .map(|chunk| {
                let servers = &servers;
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|(path, items)| {
                            let p = Path::new(path);
                            let located = pdb::codeview(p).and_then(|cv| pdb::find(p, &cv, servers)).and_then(|pdb| pdb::resolve(&pdb, &items.iter().map(|x| x.1).collect::<Vec<_>>()));
                            (items.clone(), path.clone(), located)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    for (items, path, located) in results {
        let module = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string();
        let fallback = module_package(&path);
        for (i, (key, rva)) in items.into_iter().enumerate() {
            let frames = located.as_ref().and_then(|l| l.get(i)).filter(|f| !f.is_empty());
            let r: Resolved = match frames {
                Some(fs) => fs
                    .iter()
                    .map(|f| {
                        let name = f.function.clone().unwrap_or_else(|| format!("{module}+0x{rva:x}"));
                        let package = f.file.as_deref().and_then(crate_of).unwrap_or_else(|| fallback.clone());
                        (name, f.file.clone().unwrap_or_default(), f.line, package, f.function.is_some())
                    })
                    .collect(),
                None => vec![(module.clone(), String::new(), 0, fallback.clone(), false)],
            };
            out.insert(key, r);
        }
    }
    let _ = write_gz(&cache, &out);
    Ok(out)
}

/// The Rust crate (or `std`) a source path belongs to, from Cargo's folder layout.
pub fn crate_of(file: &str) -> Option<String> {
    let f = file.replace('\\', "/");
    if f.contains("/rustc/") && f.contains("/library/") {
        return Some("std".into());
    }
    let strip = |dir: &str| -> String {
        match dir.rsplit_once('-') {
            Some((name, v)) if v.starts_with(|c: char| c.is_ascii_digit()) => name.to_string(),
            _ => dir.to_string(),
        }
    };
    if let Some(i) = f.find("/registry/src/") {
        let rest = &f[i + 14..];
        return rest.split('/').nth(1).map(strip);
    }
    let at = f.rfind("/src/")?;
    let parent = &f[..at];
    let dir = parent.rsplit('/').next()?;
    (!dir.is_empty() && !dir.ends_with(':')).then(|| strip(dir))
}

/// A module without source paths: OS modules and GPU drivers say so; others go by file name.
pub fn module_package(path: &str) -> String {
    let lower = path.to_ascii_lowercase().replace('/', "\\");
    let module = lower.rsplit('\\').next().unwrap_or(&lower).to_string();
    let windows = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()).to_ascii_lowercase();
    let gpu = lower.contains("\\driverstore\\") || ["nv", "amd", "ati", "ig", "intel"].iter().any(|p| module.starts_with(p));
    if gpu {
        format!("{module} (gpu driver)")
    } else if lower.starts_with(&windows) {
        format!("{module} (windows)")
    } else {
        module
    }
}

/// Facts about a recording's native samples for its meta and the views.
pub fn summary(n: &Native) -> Value {
    let top: Vec<Value> = {
        let mut v: Vec<(&String, &u64)> = n.unsymbolized.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        v.into_iter().take(10).map(|(m, c)| json!({ "module": m, "samples": c })).collect()
    };
    json!({ "samples": n.samples, "resolved_samples": n.resolved, "resolved_share": if n.samples > 0 { n.resolved as f64 / n.samples as f64 } else { 0.0 }, "unsymbolized": top })
}
