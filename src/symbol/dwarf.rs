//! Native symbols from DWARF (SPEC §12.2) for ELF (Linux) and Mach-O (macOS) modules, with pure-Rust
//! crates (`addr2line` over `gimli`, `object`): functions, files, lines and inline frames.
//!
//! Where the debug info is: the module itself, else its separate debug file (Linux: the build id's
//! file under `/usr/lib/debug/.build-id`, else `.gnu_debuglink`; macOS: a `.dSYM` beside the module or
//! in strobe's cache). A Rust build on macOS leaves its DWARF in the object files it linked from
//! (`split-debuginfo=unpacked`): strobe gathers them once into a cached dSYM with `dsymutil` (the
//! Command Line Tools'), keyed by the module's UUID. The kernel's symbols come from `/proc/kallsyms`.

use super::pdb::{Located, demangle};
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Resolves offsets from a module's start (where its first segment is mapped); each gives its frames
/// innermost first, or none when the module has no debug info or symbol for it.
pub fn resolve(path: &Path, offsets: &[u64]) -> Option<Vec<Vec<Located>>> {
    if path.as_os_str() == "[kernel.kallsyms]" {
        return kallsyms(offsets);
    }
    let bytes = std::fs::read(path).ok()?;
    let obj = object::File::parse(&*bytes).ok()?;
    let base = obj.segments().filter(|s| s.file_range().1 > 0).map(|s| s.address()).min().unwrap_or(0);
    // Mach-O prefixes every C-level name with one underscore.
    let macho = obj.format() == object::BinaryFormat::MachO;
    let mut symbols: Vec<(u64, u64, String)> = obj
        .symbols()
        .chain(obj.dynamic_symbols())
        .filter(|s| s.is_definition() && s.kind() == object::SymbolKind::Text && s.address() > 0)
        .filter_map(|s| {
            let n = s.name().ok()?;
            Some((s.address(), s.size(), if macho { n.strip_prefix('_').unwrap_or(n) } else { n }.to_string()))
        })
        .collect();
    symbols.sort_by_key(|s| s.0);
    // A symbol covers its size; one without a size (Mach-O) covers up to the next. An address past a
    // symbol's end is in code the table does not name (a stripped library's internal function).
    let symbol = |svma: u64| -> Option<String> {
        let i = symbols.partition_point(|s| s.0 <= svma).checked_sub(1)?;
        let (start, size, name) = &symbols[i];
        (*size == 0 || svma < start + size).then(|| demangle(name))
    };
    let debug_bytes = if obj.section_by_name(".debug_info").is_some() { None } else { debug_file(path, &obj).and_then(|p| std::fs::read(p).ok()) };
    let debug_obj = debug_bytes.as_deref().and_then(|b| object::File::parse(b).ok());
    let dwarf_obj = debug_obj.as_ref().unwrap_or(&obj);
    let ctx = context(dwarf_obj);
    Some(
        offsets
            .iter()
            .map(|&off| {
                let svma = base + off;
                let mut out = Vec::new();
                if let Some(ctx) = &ctx
                    && let Ok(mut frames) = ctx.find_frames(svma).skip_all_loads()
                {
                    while let Ok(Some(f)) = frames.next() {
                        let function = f.function.as_ref().and_then(|n| n.raw_name().ok()).map(|n| demangle(&n));
                        let (file, line) = f.location.map(|l| (l.file.map(str::to_string), l.line.unwrap_or(0))).unwrap_or((None, 0));
                        out.push(Located { function, file, line });
                    }
                }
                // The outermost frame without a name takes the symbol table's.
                if let Some(last) = out.last_mut()
                    && last.function.is_none()
                {
                    last.function = symbol(svma);
                }
                if out.is_empty()
                    && let Some(name) = symbol(svma)
                {
                    out.push(Located { function: Some(name), file: None, line: 0 });
                }
                out
            })
            .collect(),
    )
}

type Slice<'a> = gimli::EndianSlice<'a, gimli::RunTimeEndian>;

fn context<'a>(obj: &'a object::File<'a>) -> Option<addr2line::Context<Slice<'a>>> {
    let endian = if obj.is_little_endian() { gimli::RunTimeEndian::Little } else { gimli::RunTimeEndian::Big };
    let load = |id: gimli::SectionId| -> Result<Slice<'a>, gimli::Error> {
        let data = obj.section_by_name(id.name()).and_then(|s| s.uncompressed_data().ok()).unwrap_or(Cow::Borrowed(&[]));
        // Compressed sections are decompressed once and kept for the context's life.
        let data: &'a [u8] = match data {
            Cow::Borrowed(b) => b,
            Cow::Owned(v) => Box::leak(v.into_boxed_slice()),
        };
        Ok(gimli::EndianSlice::new(data, endian))
    };
    let dwarf = gimli::Dwarf::load(load).ok()?;
    addr2line::Context::from_dwarf(dwarf).ok()
}

/// The separate file holding a module's DWARF, when there is one.
fn debug_file(path: &Path, obj: &object::File) -> Option<PathBuf> {
    if let Ok(Some(id)) = obj.build_id() {
        let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
        if hex.len() > 2 {
            let p = PathBuf::from(format!("/usr/lib/debug/.build-id/{}/{}.debug", &hex[..2], &hex[2..]));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Ok(Some((name, _))) = obj.gnu_debuglink() {
        let name = String::from_utf8_lossy(name).to_string();
        let dir = path.parent().unwrap_or(Path::new("/"));
        for p in [dir.join(&name), dir.join(".debug").join(&name), Path::new("/usr/lib/debug").join(dir.strip_prefix("/").unwrap_or(dir)).join(&name)] {
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Ok(Some(uuid)) = obj.mach_uuid() {
        return dsym(path, &uuid);
    }
    None
}

/// A Mach-O module's dSYM DWARF file: beside it, in strobe's cache, or made into the cache now.
fn dsym(path: &Path, uuid: &[u8; 16]) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let inner = |d: &Path| d.join("Contents/Resources/DWARF").join(&name);
    let beside = inner(&PathBuf::from(format!("{}.dSYM", path.display())));
    if beside.is_file() {
        return Some(beside);
    }
    let id: String = uuid.iter().map(|b| format!("{b:02X}")).collect();
    let cached = super::pdb::cache_dir().join("dsym").join(format!("{id}.dSYM"));
    if inner(&cached).is_file() {
        return Some(inner(&cached));
    }
    // System libraries have no debug info to gather; a build that left its objects has.
    if path.starts_with("/usr/lib") || path.starts_with("/System") {
        return None;
    }
    let missing = cached.with_extension("missing");
    if missing.exists() {
        return None;
    }
    let _ = std::fs::create_dir_all(cached.parent()?);
    let run = |dev: Option<&str>| {
        let mut c = std::process::Command::new("dsymutil");
        c.arg("-o").arg(&cached).arg(path).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        if let Some(d) = dev {
            c.env("DEVELOPER_DIR", d);
        }
        c.status().is_ok_and(|s| s.success())
    };
    // An Xcode whose license nobody accepted blocks the xcrun shim; the Command Line Tools still work.
    let ok = run(None) || (Path::new("/Library/Developer/CommandLineTools").is_dir() && run(Some("/Library/Developer/CommandLineTools")));
    if ok && inner(&cached).is_file() {
        return Some(inner(&cached));
    }
    let _ = std::fs::write(missing, b"");
    None
}

/// Kernel addresses (offsets from the kernel's half of the address space) by `/proc/kallsyms`, which
/// shows real addresses only to a process allowed to see them (otherwise every address reads 0).
fn kallsyms(offsets: &[u64]) -> Option<Vec<Vec<Located>>> {
    const KERNEL: u64 = 0xFFFF_8000_0000_0000;
    let text = std::fs::read_to_string("/proc/kallsyms").ok()?;
    let mut syms: Vec<(u64, String)> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let a = u64::from_str_radix(f.next()?, 16).ok()?;
            let kind = f.next()?;
            matches!(kind, "t" | "T" | "w" | "W").then(|| (a, f.next().unwrap_or("").to_string()))
        })
        .filter(|s| s.0 >= KERNEL)
        .collect();
    if syms.is_empty() {
        return None;
    }
    syms.sort_by_key(|s| s.0);
    Some(
        offsets
            .iter()
            .map(|&o| {
                let a = KERNEL + o;
                match syms.partition_point(|s| s.0 <= a).checked_sub(1) {
                    Some(i) => vec![Located { function: Some(syms[i].1.clone()), file: None, line: 0 }],
                    None => Vec::new(),
                }
            })
            .collect(),
    )
}
