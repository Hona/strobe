// The commit a build comes from, for `strobe 0.6.0 (a1b2c3d4e5)` in reports: Cargo.toml's version alone
// does not tell a build from main after a release from the release. Empty outside a git checkout.
//
// And the build's identity (`STROBE_BUILD_ID`): a hash of every source file that goes into the
// binaries, the target and the profile. `strobe` and `strobew` come from one run of this script, so
// they carry the same id, where a hash of each executable differed and a daemon (strobew) never
// matched the CLI (strobe) that installed it.
use std::path::Path;
use std::process::Command;

const SOURCES: &[&str] = &["src", "sdk/bench/src", "viewer", "skill", "calibration", "Cargo.toml", "Cargo.lock", "build.rs"];
/// What the helper runs and returns: its requests, the present and stack sessions, `Presents` and `Deep`.
const HELPER_SOURCES: &[&str] = &["src/etw"];

/// FNV-1a, 64 bits: stable across Rust releases, unlike the standard library's hasher.
fn fnv(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

fn walk(path: &Path, files: &mut Vec<std::path::PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path).into_iter().flatten().flatten().map(|e| e.path()).collect();
        entries.sort();
        for e in entries {
            walk(&e, files);
        }
    } else if path.is_file() {
        files.push(path.to_path_buf());
    }
}

fn main() {
    let git = |args: &[&str]| Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let commit = git(&["rev-parse", "--short=10", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=STROBE_COMMIT={commit}");
    // Rebuild when HEAD moves: a commit, a checkout or a rebase writes the worktree's HEAD log.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/logs/HEAD");
    }
    let mut files = Vec::new();
    for s in SOURCES {
        walk(Path::new(s), &mut files);
        println!("cargo:rerun-if-changed={s}");
    }
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for f in &files {
        fnv(&mut h, f.to_string_lossy().replace('\\', "/").as_bytes());
        fnv(&mut h, &std::fs::read(f).unwrap_or_default());
    }
    for k in ["TARGET", "PROFILE", "CARGO_PKG_VERSION"] {
        fnv(&mut h, std::env::var(k).unwrap_or_default().as_bytes());
    }
    println!("cargo:rustc-env=STROBE_BUILD_ID={h:016x}");
    // The deep-mode helper's protocol (`STROBE_HELPER_PROTOCOL`): a hash of the sources that make its
    // operations and what they return. A client refuses a helper that differs, while builds that
    // changed only other files keep using the installed one.
    let mut p = 0xcbf2_9ce4_8422_2325u64;
    for f in files.iter().filter(|f| HELPER_SOURCES.iter().any(|s| f.starts_with(s))) {
        fnv(&mut p, f.to_string_lossy().replace('\\', "/").as_bytes());
        fnv(&mut p, &std::fs::read(f).unwrap_or_default());
    }
    println!("cargo:rustc-env=STROBE_HELPER_PROTOCOL={p:016x}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources();
    }
}

// Windows: each executable carries version information (what Explorer's Details tab, Task Manager and
// antivirus engines read: company, product, description, original file name) and an application
// manifest (asInvoker, Windows 10 and 11). An executable with neither looks like a dropper to
// machine-learning malware classifiers (Defender quarantined unsigned builds as Trojan:Win32/Bearfoos.A!ml).
// The resources are written here as a COFF object with a `.rsrc` section, the shape windres and cvtres
// produce, and handed to the linker: no resource compiler, no C toolchain.
const COMPANY: &str = "Hona";
const COPYRIGHT: &str = "Copyright (c) Hona. MIT License.";
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
"#;
const RT_VERSION: u32 = 16;
const RT_MANIFEST: u32 = 24;
const EN_US: u32 = 0x0409;

fn windows_resources() {
    let (machine, addr32nb) = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => (0x8664u16, 3u16),
        Ok("aarch64") => (0xAA64, 2),
        Ok("x86") => (0x014C, 7),
        _ => return,
    };
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for (bin, description) in [("strobe", "strobe: measure and explain performance"), ("strobew", "strobe (windowless): measure and explain performance")] {
        let version = version_info(bin, description);
        let obj = coff(machine, addr32nb, &[(RT_VERSION, version), (RT_MANIFEST, MANIFEST.as_bytes().to_vec())]);
        let path = out.join(format!("{bin}-resources.o"));
        std::fs::write(&path, obj).unwrap();
        println!("cargo:rustc-link-arg-bin={bin}={}", path.display());
    }
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

fn pad(b: &mut Vec<u8>, to: usize) {
    while !b.len().is_multiple_of(to) {
        b.push(0);
    }
}

/// One VS_VERSIONINFO node: length, value length, type (1 text, 0 binary), key, value, children,
/// each starting on a 32-bit boundary.
fn node(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0, 0];
    b.extend(value_len.to_le_bytes());
    b.extend((text as u16).to_le_bytes());
    b.extend(utf16z(key));
    pad(&mut b, 4);
    b.extend(value);
    for c in children {
        pad(&mut b, 4);
        b.extend(c);
    }
    let n = b.len() as u16;
    b[..2].copy_from_slice(&n.to_le_bytes());
    b
}

fn version_info(bin: &str, description: &str) -> Vec<u8> {
    let part = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0).min(0xFFFF);
    let (major, minor, patch) = (part("CARGO_PKG_VERSION_MAJOR"), part("CARGO_PKG_VERSION_MINOR"), part("CARGO_PKG_VERSION_PATCH"));
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let (ms, ls) = (major << 16 | minor, patch << 16);
    // VS_FIXEDFILEINFO: signature, structure version, file and product version, flags mask, flags,
    // VOS_NT_WINDOWS32, VFT_APP, subtype, date.
    let fixed: Vec<u8> = [0xFEEF_04BD, 0x0001_0000, ms, ls, ms, ls, 0x3F, 0, 0x0004_0004, 1, 0, 0, 0].iter().flat_map(|v: &u32| v.to_le_bytes()).collect();
    let file = format!("{bin}.exe");
    let strings: Vec<Vec<u8>> = [
        ("CompanyName", COMPANY),
        ("FileDescription", description),
        ("FileVersion", &version),
        ("InternalName", bin),
        ("LegalCopyright", COPYRIGHT),
        ("OriginalFilename", &file),
        ("ProductName", "strobe"),
        ("ProductVersion", &version),
    ]
    .iter()
    .map(|(k, v)| node(k, &utf16z(v), (v.encode_utf16().count() + 1) as u16, true, &[]))
    .collect();
    // U.S. English, Unicode (code page 1200).
    let table = node("040904B0", &[], 0, true, &strings);
    let string_info = node("StringFileInfo", &[], 0, true, &[table]);
    let translation = node("Translation", &[0x09, 0x04, 0xB0, 0x04], 4, false, &[]);
    let var_info = node("VarFileInfo", &[], 0, true, &[translation]);
    node("VS_VERSION_INFO", &fixed, fixed.len() as u16, false, &[string_info, var_info])
}

/// A COFF object whose `.rsrc` section holds `resources` (type, data), each as ID 1 in U.S. English:
/// the type, name and language directories, the data entries (their RVAs relocated against the
/// section) and the data.
fn coff(machine: u16, addr32nb: u16, resources: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let n = resources.len();
    let dir = |b: &mut Vec<u8>, entries: &[(u32, u32)]| {
        b.extend([0u8; 12]);
        b.extend(0u16.to_le_bytes());
        b.extend((entries.len() as u16).to_le_bytes());
        for (id, offset) in entries {
            b.extend(id.to_le_bytes());
            b.extend(offset.to_le_bytes());
        }
    };
    let names = 16 + 8 * n;
    let langs = names + 24 * n;
    let entries = langs + 24 * n;
    let mut data_at = Vec::new();
    let mut at = (entries + 16 * n).next_multiple_of(8);
    for (_, d) in resources {
        data_at.push(at);
        at = (at + d.len()).next_multiple_of(8);
    }
    let subdir = 0x8000_0000u32;
    let mut s = Vec::new();
    dir(&mut s, &resources.iter().enumerate().map(|(i, (ty, _))| (*ty, subdir | (names + 24 * i) as u32)).collect::<Vec<_>>());
    for i in 0..n {
        dir(&mut s, &[(1, subdir | (langs + 24 * i) as u32)]);
    }
    for i in 0..n {
        dir(&mut s, &[(EN_US, (entries + 16 * i) as u32)]);
    }
    for (i, (_, d)) in resources.iter().enumerate() {
        for v in [data_at[i] as u32, d.len() as u32, 0, 0] {
            s.extend(v.to_le_bytes());
        }
    }
    for (_, d) in resources {
        pad(&mut s, 8);
        s.extend(d);
    }
    pad(&mut s, 8);

    let raw = 20 + 40;
    let relocs = raw + s.len();
    let symbols = relocs + 10 * n;
    let mut o = Vec::new();
    o.extend(machine.to_le_bytes());
    o.extend(1u16.to_le_bytes());
    o.extend(0u32.to_le_bytes());
    o.extend((symbols as u32).to_le_bytes());
    o.extend(1u32.to_le_bytes());
    o.extend(0u16.to_le_bytes());
    o.extend(0u16.to_le_bytes());
    o.extend(*b".rsrc\0\0\0");
    for v in [0u32, 0, s.len() as u32, raw as u32, relocs as u32, 0] {
        o.extend(v.to_le_bytes());
    }
    o.extend((n as u16).to_le_bytes());
    o.extend(0u16.to_le_bytes());
    // IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ
    o.extend(0x4000_0040u32.to_le_bytes());
    o.extend(&s);
    for i in 0..n {
        o.extend(((entries + 16 * i) as u32).to_le_bytes());
        o.extend(0u32.to_le_bytes());
        o.extend(addr32nb.to_le_bytes());
    }
    // The section's symbol (static), then an empty string table.
    o.extend(*b".rsrc\0\0\0");
    o.extend(0u32.to_le_bytes());
    o.extend(1i16.to_le_bytes());
    o.extend(0u16.to_le_bytes());
    o.extend([3u8, 0]);
    o.extend(4u32.to_le_bytes());
    o
}
