//! Native symbols from PDBs (SPEC §12.2), with pure-Rust crates (`pdb-addr2line` over `pdb2`).
//!
//! A module's PDB is the one its CodeView record names (GUID, age, path): first beside the module (a
//! copied build's own PDB), then the path the build wrote, then strobe's cache, then the symbol
//! servers (Microsoft's, then `strobe.json` `symbols.servers`) through `curl`, cached under
//! `<state>/symbols/<pdb>/<GUID><age>/`. A server miss leaves a `missing` marker for a day.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const MICROSOFT: &str = "https://msdl.microsoft.com/download/symbols";

/// A module's CodeView (RSDS) record: which PDB matches it.
#[derive(Clone, Debug)]
pub struct CodeView {
    pub guid: [u8; 16],
    pub age: u32,
    pub pdb: String,
}

impl CodeView {
    /// `<GUID><age>` as symbol servers index it (GUID fields in hex, uppercase; age in hex).
    pub fn id(&self) -> String {
        let g = &self.guid;
        let d1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
        let d2 = u16::from_le_bytes([g[4], g[5]]);
        let d3 = u16::from_le_bytes([g[6], g[7]]);
        let d4: String = g[8..].iter().map(|b| format!("{b:02X}")).collect();
        format!("{d1:08X}{d2:04X}{d3:04X}{d4}{:X}", self.age)
    }

    pub fn file_name(&self) -> String {
        self.pdb.rsplit(['\\', '/']).next().unwrap_or(&self.pdb).to_string()
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2).map(|x| u16::from_le_bytes([x[0], x[1]]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}

/// `SizeOfImage` of a PE file: how much address space it takes when loaded.
pub fn image_size(path: &Path) -> Option<u32> {
    let mut head = vec![0u8; 1024];
    let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    head.truncate(n);
    let pe = u32_at(&head, 0x3c)? as usize;
    (head.get(pe..pe + 4)? == b"PE\0\0").then_some(())?;
    u32_at(&head, pe + 24 + 56)
}

/// The CodeView record of a PE file (EXE, DLL, SYS), read from its debug directory.
pub fn codeview(path: &Path) -> Option<CodeView> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 4096];
    let n = f.read(&mut head).ok()?;
    head.truncate(n);
    let pe = u32_at(&head, 0x3c)? as usize;
    if head.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let sections = u16_at(&head, pe + 6)? as usize;
    let opt_size = u16_at(&head, pe + 20)? as usize;
    let opt = pe + 24;
    let dirs = match u16_at(&head, opt)? {
        0x20b => opt + 112,
        0x10b => opt + 96,
        _ => return None,
    };
    let (rva, size) = (u32_at(&head, dirs + 6 * 8)?, u32_at(&head, dirs + 6 * 8 + 4)?);
    if rva == 0 || size == 0 {
        return None;
    }
    let table = opt + opt_size;
    let to_file = |rva: u32| -> Option<u64> {
        (0..sections).find_map(|i| {
            let s = table + i * 40;
            let (vsize, vaddr, raw_size, raw_ptr) = (u32_at(&head, s + 8)?, u32_at(&head, s + 12)?, u32_at(&head, s + 16)?, u32_at(&head, s + 20)?);
            (rva >= vaddr && rva < vaddr + vsize.max(raw_size)).then(|| (rva - vaddr + raw_ptr) as u64)
        })
    };
    let at = to_file(rva)?;
    let mut dir = vec![0u8; size.min(28 * 16) as usize];
    f.seek(SeekFrom::Start(at)).ok()?;
    f.read_exact(&mut dir).ok()?;
    for e in dir.as_chunks::<28>().0 {
        // IMAGE_DEBUG_TYPE_CODEVIEW
        if u32_at(e, 12)? != 2 {
            continue;
        }
        let (len, ptr) = (u32_at(e, 16)? as usize, u32_at(e, 24)? as u64);
        let mut cv = vec![0u8; len.min(4096)];
        f.seek(SeekFrom::Start(ptr)).ok()?;
        f.read_exact(&mut cv).ok()?;
        if cv.get(..4)? != b"RSDS" {
            continue;
        }
        let mut guid = [0u8; 16];
        guid.copy_from_slice(cv.get(4..20)?);
        let age = u32_at(&cv, 20)?;
        let name = cv.get(24..)?;
        let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
        return Some(CodeView { guid, age, pdb: String::from_utf8_lossy(&name[..end]).to_string() });
    }
    None
}

/// Whether a PDB file is the one `cv` names: its GUID and its age. An incremental link keeps the
/// GUID and bumps the age, so a GUID alone can pair one build's module with a later build's PDB.
fn matches(path: &Path, cv: &CodeView) -> bool {
    let Ok(f) = std::fs::File::open(path) else { return false };
    let Ok(mut pdb) = pdb_addr2line::pdb::PDB::open(f) else { return false };
    let Ok(info) = pdb.pdb_information() else { return false };
    let (d1, d2, d3, d4) = info.guid.as_fields();
    let g = &cv.guid;
    let guid = d1 == u32::from_le_bytes([g[0], g[1], g[2], g[3]]) && d2 == u16::from_le_bytes([g[4], g[5]]) && d3 == u16::from_le_bytes([g[6], g[7]]) && d4[..] == g[8..];
    // The debug information stream's age is the one the linker writes into the module.
    let age = pdb.debug_information().ok().and_then(|d| d.age()).unwrap_or(info.age);
    guid && age == cv.age
}

pub fn cache_dir() -> PathBuf {
    crate::host::paths::state_dir().join("symbols")
}

/// The matching PDB for a module: local first, then the symbol servers (downloaded once). Beside the
/// module comes before the path the build wrote: a build folder shared by several builds (base and
/// head) holds only the last one's PDB there, while each copied build keeps its own beside it.
pub fn find(module: &Path, cv: &CodeView, servers: &[String]) -> Option<PathBuf> {
    let name = cv.file_name();
    let local = [module.with_file_name(&name), PathBuf::from(&cv.pdb)];
    if let Some(p) = local.into_iter().find(|p| p.is_file() && matches(p, cv)) {
        return Some(p);
    }
    let dir = cache_dir().join(&name).join(cv.id());
    let cached = dir.join(&name);
    if cached.is_file() {
        return Some(cached);
    }
    let missing = dir.join("missing");
    if super::missed_recently(&missing) {
        return None;
    }
    std::fs::create_dir_all(&dir).ok()?;
    let tmp = dir.join(format!("{name}.part{}", std::process::id()));
    for server in std::iter::once(MICROSOFT.to_string()).chain(servers.iter().cloned()) {
        let url = format!("{}/{name}/{}/{name}", server.trim_end_matches('/'), cv.id());
        let ok = std::process::Command::new("curl")
            .args(["-sfL", "--max-time", "300", "-o"])
            .arg(&tmp)
            .arg(&url)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        let msf = std::fs::File::open(&tmp).ok().and_then(|mut f| {
            let mut b = [0u8; 24];
            f.read_exact(&mut b).ok().map(|_| b)
        });
        if ok && msf.is_some_and(|b| &b[..] == b"Microsoft C/C++ MSF 7.00") && std::fs::rename(&tmp, &cached).is_ok() {
            return Some(cached);
        }
        let _ = std::fs::remove_file(&tmp);
    }
    let _ = std::fs::write(&missing, b"");
    None
}

/// One frame of a native address: function, source location, inline frames expanded.
#[derive(Clone, Debug, Default)]
pub struct Located {
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: u32,
}

/// Resolves module-relative addresses with one PDB; each address gives its frames innermost first.
pub fn resolve(pdb_path: &Path, rvas: &[u32]) -> Option<Vec<Vec<Located>>> {
    let f = std::fs::File::open(pdb_path).ok()?;
    let pdb = pdb_addr2line::pdb::PDB::open(f).ok()?;
    let data = pdb_addr2line::ContextPdbData::try_from_pdb(pdb).ok()?;
    let ctx = data.make_context().ok()?;
    Some(
        rvas.iter()
            .map(|&rva| match ctx.find_frames(rva) {
                Ok(Some(ff)) => ff.frames.into_iter().map(|fr| Located { function: fr.function.map(|n| demangle(&n)), file: fr.file.map(|f| f.to_string()), line: fr.line.unwrap_or(0) }).collect(),
                _ => Vec::new(),
            })
            .collect(),
    )
}

/// Readable names for public symbols: Rust (legacy and v0) and MSVC C++ manglings.
pub fn demangle(name: &str) -> String {
    if let Ok(d) = rustc_demangle::try_demangle(name) {
        return format!("{d:#}");
    }
    if name.starts_with('?')
        && let Ok(d) = msvc_demangler::demangle(name, msvc_demangler::DemangleFlags::NAME_ONLY)
    {
        return d;
    }
    name.to_string()
}
