//! Video of what a target showed (SPEC §8.8), for people to watch beside the numbers. Only untimed
//! blocks and `record --video` take one: every source costs CPU and GPU time in strobe, in the OS's
//! compositor or encoder, or in the target's own renderer (rule 3).
//!
//! Two kinds, never mixed in one comparison:
//!
//! - `screen`: the OS's copy of the window as it composited it: Windows.Graphics.Capture into Media
//!   Foundation's H.264 encoder (Windows, frame times on the QPC clock), a ScreenCaptureKit stream of
//!   the window into AVAssetWriter (macOS, each frame at its display time), the X server's copy of the
//!   window (`GetImage`) at 30 frames a second into ffmpeg (Linux X11).
//! - `renderer`: a Chromium page's own compositor frames (DevTools `Page.startScreencast`, JPEG), each at
//!   the time its frame was swapped, into ffmpeg at 30 frames a second: works headless and on a locked
//!   session, and is not proof that a frame reached a display.
//!
//! Every frame is stamped on the host clock, so `video.json` maps runs and inputs to media time.

#[cfg(target_os = "macos")]
mod avf;
pub mod ffmpeg;
pub mod screencast;
#[cfg(windows)]
mod wgc;
#[cfg(target_os = "linux")]
mod x11;

use crate::model::{Error, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The longest edge of a video, in pixels: a window is scaled down to it, never up.
pub const MAX_EDGE: u32 = 1280;

/// What a video shows (SPEC §8.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The OS's composited copy of the window: what reached the display.
    Screen,
    /// The browser engine's own compositor frames: what the renderer produced, at a capped rate.
    Renderer,
}

impl Kind {
    pub fn about(self) -> &'static str {
        match self {
            Kind::Screen => {
                "screen: the OS's composited copy of the window. macOS: each frame at its display time, at most 60 a second; Windows: each at the compositor's time, at most 60 a second (one within 1/60 s of the last written is held, up to 16.7 ms late); Linux X11: the window polled 30 times a second, so a change shows up to 33 ms late and frames repeat between changes"
            }
            Kind::Renderer => {
                "renderer: the browser's own compositor frames (DevTools screencast), each at the time it was swapped, written at a constant 30 a second (a frame repeats until the next, up to 17 ms from its time); not proof that a frame reached a display"
            }
        }
    }
}

/// A finished video and what made it.
#[derive(Clone, Debug, Serialize)]
pub struct Video {
    pub kind: Kind,
    /// The finished file (in the run folder until the recording takes it).
    #[serde(skip)]
    pub path: PathBuf,
    /// `mp4` or `webm`.
    pub ext: &'static str,
    pub width: u32,
    pub height: u32,
    /// Host clock at media time 0, and at the video's end.
    pub t0_ns: u64,
    pub t1_ns: u64,
    /// Frames written, and frames the source delivered (a source faster than the video's rate, or an
    /// encoder that fell behind, writes fewer).
    pub frames: u64,
    pub source_frames: u64,
    /// The constant frame rate the encoder wrote (ffmpeg duplicates frames up to it), or None: each
    /// frame at its own time, at most 60 a second.
    pub fps: Option<u32>,
    /// What captured the frames and what encoded them.
    pub source: String,
    pub encoder: String,
    /// strobe's own capture thread's CPU (ms); an encoder in another process (ffmpeg, the OS's) is
    /// counted in `encoder_cpu_ms` where strobe could read it.
    pub cpu_ms: Option<f64>,
    /// strobe's CPU (ms) setting the capture and encoder up, before the first frame: once per video.
    pub setup_cpu_ms: Option<f64>,
    pub encoder_cpu_ms: Option<f64>,
    /// How far a frame's media time may sit from its host time.
    pub precision_ms: f64,
}

/// A video being recorded.
pub enum Capture {
    #[cfg(windows)]
    Screen(wgc::Capture),
    #[cfg(target_os = "macos")]
    Screen(avf::Capture),
    #[cfg(target_os = "linux")]
    Screen(x11::Capture),
    Renderer(screencast::Capture),
}

impl Capture {
    /// The OS's copy of `window` (a top-level window strobe found for the target), into `base` with
    /// the encoder's extension.
    pub fn screen(window: &crate::host::TopWindow, base: &Path) -> Result<Capture> {
        #[cfg(windows)]
        return Ok(Capture::Screen(wgc::Capture::start(window, base)?));
        #[cfg(target_os = "macos")]
        return Ok(Capture::Screen(avf::Capture::start(window, base)?));
        #[cfg(target_os = "linux")]
        return Ok(Capture::Screen(x11::Capture::start(window, base)?));
    }

    /// A Chromium page's own frames over its DevTools session.
    pub fn renderer(page: &crate::target::chromium::Page, base: &Path) -> Result<Capture> {
        Ok(Capture::Renderer(screencast::Capture::start(page.cdp.clone(), &page.session, base)?))
    }

    pub fn kind(&self) -> Kind {
        match self {
            Capture::Renderer(_) => Kind::Renderer,
            #[allow(unreachable_patterns)]
            _ => Kind::Screen,
        }
    }

    pub fn stop(self) -> Result<Video> {
        match self {
            Capture::Screen(c) => c.stop(),
            Capture::Renderer(c) => c.stop(),
        }
    }
}

/// The calling thread's CPU from now on, in ms: its cycles over the cycle clock's rate against the
/// host clock (cycles are ns off Windows). None where cycles are not time.
pub struct ThreadCpu {
    cycles: u64,
    tsc: Option<u64>,
    ns: u64,
}

impl ThreadCpu {
    pub fn start() -> ThreadCpu {
        ThreadCpu { cycles: crate::host::thread_cycles(), tsc: crate::host::tsc(), ns: crate::host::now_ns() }
    }

    pub fn ms(&self) -> Option<f64> {
        let (tsc, ns) = (crate::host::tsc()?.checked_sub(self.tsc?)?, crate::host::now_ns() - self.ns);
        (tsc > 0).then(|| crate::host::thread_cycles().saturating_sub(self.cycles) as f64 * ns as f64 / tsc as f64 / 1e6)
    }
}

/// A video's size for a source of `w` × `h` pixels: at most `MAX_EDGE` on its long edge, both even
/// (4:2:0 video), at least 2.
pub fn fit(w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w.max(2), h.max(2));
    let scale = (MAX_EDGE as f64 / w.max(h) as f64).min(1.0);
    let even = |v: u32| ((v as f64 * scale).round() as u32 & !1).max(2);
    (even(w), even(h))
}

/// A fresh folder for one video's file while it is written, in the run folder (the user's alone):
/// removed with whatever it holds when this is dropped (the recording took the finished file, or the
/// capture failed, delivered nothing, or its block ended early).
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> Result<Scratch> {
        let dir = crate::host::paths::run().join(format!("video-{}", crate::host::paths::csprng_hex(4).map_err(|e| Error::msg(format!("video: {e}")))?));
        crate::host::paths::owner_only_dir(&dir);
        Ok(Scratch(dir))
    }

    /// The file's path without its extension (the encoder adds `mp4` or `webm`).
    pub fn base(&self) -> PathBuf {
        self.0.join("video")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `NoPresentSource` for a video this target or OS cannot give, with the fix.
pub fn refused(summary: impl Into<String>, fix: impl Into<String>) -> Error {
    crate::model::Refusal::new(crate::model::ValidatorId::NoPresentSource, summary).fix(fix).into()
}
