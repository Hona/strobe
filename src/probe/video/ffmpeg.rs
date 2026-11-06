//! The ffmpeg executable as an encoder (SPEC §8.8, rule 12: a program strobe runs, never a library it
//! links). Found on PATH, then where package managers and Playwright put it; none is a refusal that
//! says how to install one.
//!
//! Input is either JPEG frames in a minimal Matroska stream with each frame's own millisecond time (a
//! renderer's screencast, as Playwright feeds ffmpeg), or raw frames at a constant rate (an X11
//! window). Output is H.264 in MP4 when the build has libx264, else VP9 or VP8 in WebM; either way at
//! a constant 30 frames a second, ffmpeg repeating a frame until the next one's time.

use crate::model::{Error, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The constant rate ffmpeg writes.
pub const FPS: u32 = 30;

/// Where ffmpeg is when it is not on PATH.
fn candidates() -> Vec<PathBuf> {
    let home = crate::host::home();
    let mut v: Vec<PathBuf> = Vec::new();
    // Playwright's own build (VP8/VP9 only), in its browser cache.
    let playwright = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("ms-playwright"))
    } else if cfg!(target_os = "macos") {
        Some(home.join("Library/Caches/ms-playwright"))
    } else {
        Some(home.join(".cache/ms-playwright"))
    };
    let exe = if cfg!(windows) {
        "ffmpeg-win64.exe"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") { "ffmpeg-mac-arm64" } else { "ffmpeg-mac" }
    } else if cfg!(target_arch = "aarch64") {
        "ffmpeg-linux-arm64"
    } else {
        "ffmpeg-linux"
    };
    if cfg!(windows) {
        if let Some(d) = std::env::var_os("LOCALAPPDATA") {
            v.push(PathBuf::from(d).join("Microsoft/WinGet/Links/ffmpeg.exe"));
        }
        v.push(home.join("scoop/shims/ffmpeg.exe"));
        let choco = std::env::var_os("ChocolateyInstall").map_or_else(|| PathBuf::from(r"C:\ProgramData\chocolatey"), PathBuf::from);
        v.push(choco.join(r"bin\ffmpeg.exe"));
    } else {
        v.extend(["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg", "/snap/bin/ffmpeg"].map(PathBuf::from));
    }
    if let Some(dir) = playwright.and_then(|d| std::fs::read_dir(d).ok()) {
        // `ffmpeg-<revision>`, newest revision first (by number: 1013 after 999).
        let mut found: Vec<(u64, PathBuf)> = dir
            .flatten()
            .filter_map(|e| Some((e.file_name().to_string_lossy().strip_prefix("ffmpeg-")?.parse::<u64>().ok()?, e.path().join(exe))))
            .collect();
        found.sort_by_key(|f| std::cmp::Reverse(f.0));
        v.extend(found.into_iter().map(|f| f.1));
    }
    v
}

/// The ffmpeg strobe runs, else `NoPresentSource` with how to get one.
pub fn find() -> Result<PathBuf> {
    crate::host::find_exe("ffmpeg", &[]).or_else(|| candidates().into_iter().find(|p| p.is_file())).ok_or_else(|| {
        super::refused(
            "this video is encoded by the ffmpeg program, and none was found (PATH, the usual package folders, Playwright's cache)",
            if cfg!(windows) {
                "install ffmpeg (winget install Gyan.FFmpeg) and run strobe again"
            } else if cfg!(target_os = "macos") {
                "install ffmpeg (brew install ffmpeg) and run strobe again"
            } else {
                "install ffmpeg from the distribution (apt install ffmpeg, pacman -S ffmpeg) and run strobe again"
            },
        )
    })
}

/// An encoder ffmpeg has, and the container and arguments strobe writes with it.
#[derive(Clone, Copy, Debug)]
pub struct Encoder {
    pub name: &'static str,
    pub ext: &'static str,
    args: &'static [&'static str],
}

const ENCODERS: [Encoder; 3] = [
    Encoder { name: "libx264", ext: "mp4", args: &["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p", "-movflags", "+faststart"] },
    Encoder { name: "libvpx-vp9", ext: "webm", args: &["-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-crf", "30", "-b:v", "0", "-row-mt", "1", "-pix_fmt", "yuv420p"] },
    Encoder { name: "libvpx", ext: "webm", args: &["-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "8", "-b:v", "3M", "-pix_fmt", "yuv420p"] },
];

/// The first of `ENCODERS` this ffmpeg build has (`ffmpeg -encoders`, read once per executable).
pub fn encoder(exe: &Path) -> Result<Encoder> {
    static SEEN: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());
    let listed = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        match seen.iter().find(|(p, _)| p == exe) {
            Some((_, l)) => l.clone(),
            None => {
                let mut cmd = Command::new(exe);
                cmd.args(["-hide_banner", "-encoders"]).stdin(Stdio::null()).stderr(Stdio::null());
                no_window(&mut cmd);
                let l = crate::host::output(&mut cmd).unwrap_or_default();
                seen.push((exe.to_path_buf(), l.clone()));
                l
            }
        }
    };
    ENCODERS
        .into_iter()
        .find(|e| listed.lines().any(|l| l.split_whitespace().nth(1) == Some(e.name)))
        .ok_or_else(|| super::refused(format!("{} has none of the encoders strobe writes video with (libx264, libvpx-vp9, libvpx)", exe.display()), "install a full ffmpeg build (one with libx264)"))
}

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: the windowless daemon must not open a console for it.
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn no_window(_: &mut Command) {}

/// A running ffmpeg reading frames on its stdin.
pub struct Ffmpeg {
    child: Child,
    stdin: Option<ChildStdin>,
    errors: Arc<Mutex<String>>,
    pub out: PathBuf,
    pub encoder: Encoder,
}

impl Ffmpeg {
    /// Starts `exe` on `input` (its input arguments, ending with `-i pipe:0`), filtering with `filter`
    /// and writing `<base>.<ext>` at `FPS`.
    pub fn spawn(exe: &Path, encoder: Encoder, input: &[String], filter: &str, base: &Path) -> Result<Ffmpeg> {
        let out = base.with_extension(encoder.ext);
        let mut cmd = Command::new(exe);
        cmd.args(["-hide_banner", "-loglevel", "error"]);
        cmd.args(input).args(["-y", "-an", "-r", &FPS.to_string(), "-vf", filter, "-threads", "2"]).args(encoder.args).arg(&out);
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        no_window(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| Error::msg(format!("start {}: {e}", exe.display())))?;
        let errors = Arc::new(Mutex::new(String::new()));
        if let Some(mut err) = child.stderr.take() {
            let errors = errors.clone();
            std::thread::Builder::new()
                .name("strobe-ffmpeg-err".into())
                .spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = err.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let mut e = errors.lock().unwrap_or_else(|e| e.into_inner());
                        if e.len() < 8192 {
                            e.push_str(&String::from_utf8_lossy(&buf[..n]));
                        }
                    }
                })
                .ok();
        }
        let stdin = child.stdin.take();
        Ok(Ffmpeg { child, stdin, errors, out, encoder })
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| Error::msg("ffmpeg's input is closed"))?;
        stdin.write_all(bytes).map_err(|e| Error::msg(format!("ffmpeg stopped reading frames: {e}{}", self.said())))
    }

    fn said(&self) -> String {
        let e = self.errors.lock().unwrap_or_else(|e| e.into_inner());
        if e.trim().is_empty() { String::new() } else { format!(" (ffmpeg: {})", e.trim().lines().last().unwrap_or("")) }
    }

    /// Ends the input and waits up to a minute for ffmpeg to finish the file. Returns its CPU (ms)
    /// where the OS says.
    pub fn finish(mut self) -> Result<Option<f64>> {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match reap(&mut self.child) {
                Ok(Some((true, cpu))) if self.out.is_file() => return Ok(cpu),
                Ok(Some((_, _))) => return Err(Error::msg(format!("ffmpeg failed to write the video{}", self.said()))),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return Err(Error::msg("ffmpeg did not finish the video within a minute"));
                }
                Err(e) => return Err(Error::msg(format!("ffmpeg: {e}"))),
            }
        }
    }
}

/// Whether the child exited (and with success), and its CPU (user and system, ms) where the OS says:
/// its own rusage from `wait4` (Linux, macOS), its process handle's times (Windows). None while it runs.
fn reap(child: &mut Child) -> std::io::Result<Option<(bool, Option<f64>)>> {
    #[cfg(unix)]
    {
        let (mut status, mut ru): (libc::c_int, libc::rusage) = (0, unsafe { std::mem::zeroed() });
        let pid = child.id() as libc::pid_t;
        match unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut ru) } {
            0 => Ok(None),
            r if r == pid => {
                let ms = |t: libc::timeval| t.tv_sec as f64 * 1e3 + t.tv_usec as f64 / 1e3;
                Ok(Some((libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, Some(ms(ru.ru_utime) + ms(ru.ru_stime)))))
            }
            _ => Err(std::io::Error::last_os_error()),
        }
    }
    #[cfg(windows)]
    {
        Ok(child.try_wait()?.map(|s| (s.success(), cpu_ms(child))))
    }
}

/// An ffmpeg dropped without `finish` (a failed block) ends with it.
impl Drop for Ffmpeg {
    fn drop(&mut self) {
        if self.stdin.is_some() {
            drop(self.stdin.take());
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// An exited child's CPU (user and system, ms) from its process handle.
#[cfg(windows)]
fn cpu_ms(child: &Child) -> Option<f64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    let z = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut a, mut b, mut k, mut u) = (z(), z(), z(), z());
    let ok = unsafe { windows_sys::Win32::System::Threading::GetProcessTimes(child.as_raw_handle() as _, &mut a, &mut b, &mut k, &mut u) } != 0;
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e4;
    ok.then(|| t(k) + t(u))
}

/// Matroska with one MJPEG track, each frame a cluster at its own millisecond time (Playwright's
/// `ebml.ts` writes the same subset): ffmpeg takes the frames' times from the stream.
pub mod mkv {
    fn vint(value: u64) -> Vec<u8> {
        let mut len = 1;
        while value >= (1u64 << (7 * len)) - 1 {
            len += 1;
        }
        let mut b = vec![0u8; len];
        let mut v = value;
        for i in (0..len).rev() {
            b[i] = (v & 0xff) as u8;
            v >>= 8;
        }
        b[0] |= 1 << (8 - len);
        b
    }

    fn uint(value: u64) -> Vec<u8> {
        let b = value.to_be_bytes();
        let first = b.iter().position(|&x| x != 0).unwrap_or(7);
        b[first..].to_vec()
    }

    fn element(id: &[u8], payload: &[u8]) -> Vec<u8> {
        [id, &vint(payload.len() as u64), payload].concat()
    }

    /// The stream's head: EBML header, a Segment of unknown size, millisecond timestamps, one track.
    pub fn header() -> Vec<u8> {
        let ebml = element(
            &[0x1A, 0x45, 0xDF, 0xA3],
            &[
                element(&[0x42, 0x86], &uint(1)),
                element(&[0x42, 0xF7], &uint(1)),
                element(&[0x42, 0xF2], &uint(4)),
                element(&[0x42, 0xF3], &uint(8)),
                element(&[0x42, 0x82], b"matroska"),
                element(&[0x42, 0x87], &uint(4)),
                element(&[0x42, 0x85], &uint(2)),
            ]
            .concat(),
        );
        let info = element(&[0x15, 0x49, 0xA9, 0x66], &[element(&[0x2A, 0xD7, 0xB1], &uint(1_000_000)), element(&[0x4D, 0x80], b"strobe"), element(&[0x57, 0x41], b"strobe")].concat());
        // A 1×1 placeholder size: the decoder reads each JPEG's own.
        let video = element(&[0xE0], &[element(&[0xB0], &uint(1)), element(&[0xBA], &uint(1))].concat());
        let track =
            element(&[0xAE], &[element(&[0xD7], &uint(1)), element(&[0x73, 0xC5], &uint(1)), element(&[0x83], &uint(1)), element(&[0x9C], &uint(0)), element(&[0x86], b"V_MJPEG"), video].concat());
        let tracks = element(&[0x16, 0x54, 0xAE, 0x6B], &track);
        [ebml, vec![0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], info, tracks].concat()
    }

    /// What goes before a frame of `len` bytes shown at `ms`: its own cluster, a keyframe at offset 0.
    pub fn frame(ms: u64, len: usize) -> Vec<u8> {
        let block = [vec![0xA3], vint(4 + len as u64), vint(1), vec![0x00, 0x00, 0x80]].concat();
        let time = element(&[0xE7], &uint(ms));
        [vec![0x1F, 0x43, 0xB6, 0x75], vint((time.len() + block.len() + len) as u64), time, block].concat()
    }
}
