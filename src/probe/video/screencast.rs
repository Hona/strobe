//! A Chromium page's own frames (SPEC §8.8, kind `renderer`): DevTools' `Page.startScreencast` sends
//! each compositor frame as a JPEG with the wall-clock time it was swapped (`metadata.timestamp`),
//! one more after each acknowledgment. Stock Chromium, Electron and WebView2 all have it, headless or
//! headed and on a locked session, since it reads the renderer's compositor rather than the screen.
//!
//! Each frame goes to ffmpeg in a Matroska stream at its own millisecond time (as Playwright's
//! `recordVideo` does), which writes a constant 30 frames a second. The swap times are mapped to the
//! host clock through the offset between the two clocks read at the start, so runs and inputs line
//! up with media time.

use super::ffmpeg::{self, Ffmpeg, mkv};
use super::{Kind, Video};
use crate::model::{Error, Result};
use crate::target::cdp::Cdp;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Capture {
    cdp: Arc<Cdp>,
    session: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Result<Video>>>,
    /// The time of the last frame written, for the end.
    end: Arc<Mutex<u64>>,
}

/// Wall-clock nanoseconds since the Unix epoch.
fn wall_ns() -> i128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i128)
}

/// A JPEG's size from its first start-of-frame marker.
fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 9 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let m = b[i + 1];
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            return Some((u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32, u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32));
        }
        i += 2 + len;
    }
    None
}

impl Capture {
    pub fn start(cdp: Arc<Cdp>, session: &str, base: &Path) -> Result<Capture> {
        let exe = ffmpeg::find()?;
        let encoder = ffmpeg::encoder(&exe)?;
        let events = cdp.events();
        // Host clock minus wall clock, read together: a frame's swap time plus this is host time.
        let (h0, w0) = (crate::host::now_ns() as i128, wall_ns());
        let offset = h0 - w0;
        let max = super::MAX_EDGE;
        cdp.send("Page.startScreencast", json!({ "format": "jpeg", "quality": 85, "maxWidth": max, "maxHeight": max, "everyNthFrame": 1 }), Some(session))
            .map_err(|e| super::refused(format!("the page refused DevTools' screencast: {}", e.message), "record a headed window instead (the OS's screen capture)"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let end = Arc::new(Mutex::new(0u64));
        let (c, s, st, en, base) = (cdp.clone(), session.to_string(), stop.clone(), end.clone(), base.to_path_buf());
        let thread = std::thread::Builder::new()
            .name("strobe-screencast".into())
            .spawn(move || run(&c, &s, events, (h0 as u64, offset), &st, &en, &exe, encoder, &base))
            .map_err(|e| Error::msg(format!("start the screencast thread: {e}")))?;
        Ok(Capture { cdp, session: session.to_string(), stop, thread: Some(thread), end })
    }

    pub fn stop(mut self) -> Result<Video> {
        *self.end.lock().unwrap_or_else(|e| e.into_inner()) = crate::host::now_ns();
        let _ = self.cdp.send_timeout("Page.stopScreencast", json!({}), Some(&self.session), Duration::from_secs(5));
        self.stop.store(true, Ordering::Relaxed);
        let t = self.thread.take().ok_or_else(|| Error::msg("the screencast stopped twice"))?;
        crate::probe::join_within(t, Duration::from_secs(90)).ok_or_else(|| Error::msg("the screencast's encoder did not finish within 90 s"))?
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.cdp.post("Page.stopScreencast", json!({}), Some(&self.session));
            self.stop.store(true, Ordering::Relaxed);
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// `t0` is the host clock when the capture started (media time 0); the first frame stands from there
/// until the next, as the page looked when the screencast began.
fn run(cdp: &Cdp, session: &str, events: std::sync::mpsc::Receiver<Value>, (t0, offset): (u64, i128), stop: &AtomicBool, end: &Mutex<u64>, exe: &Path, encoder: ffmpeg::Encoder, base: &Path) -> Result<Video> {
    let cpu = super::ThreadCpu::start();
    let mut ff: Option<Ffmpeg> = None;
    let mut size = (0u32, 0u32);
    let mut last: Option<(Vec<u8>, u64)> = None;
    let (mut frames, mut unstamped) = (0u64, 0u64);
    loop {
        let ev = match events.recv_timeout(Duration::from_millis(50)) {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) if stop.load(Ordering::Relaxed) => break,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if ev["method"] != "Page.screencastFrame" || ev["sessionId"].as_str() != Some(session) {
            continue;
        }
        let p = &ev["params"];
        // The next frame comes once this one is acknowledged: at once, before it is encoded.
        let _ = cdp.post("Page.screencastFrameAck", json!({ "sessionId": p["sessionId"] }), Some(session));
        if stop.load(Ordering::Relaxed) {
            continue;
        }
        use base64::Engine as _;
        let Ok(jpeg) = base64::engine::general_purpose::STANDARD.decode(p["data"].as_str().unwrap_or("")) else { continue };
        let at = match p["metadata"]["timestamp"].as_f64() {
            Some(s) => (((s * 1e9) as i128) + offset).max(0) as u64,
            None => {
                unstamped += 1;
                crate::host::now_ns()
            }
        };
        if ff.is_none() {
            let Some((w, h)) = jpeg_size(&jpeg) else { continue };
            size = super::fit(w, h);
            let input = ["-f", "matroska", "-fpsprobesize", "0", "-probesize", "32", "-analyzeduration", "0", "-i", "pipe:0"].map(String::from);
            // Each frame on a canvas of the first one's size: a later frame of another size (a resized
            // window) is padded or cropped, never stretched.
            let filter = format!("scale=w='min(iw,{w})':h='min(ih,{h})':force_original_aspect_ratio=decrease,pad={w}:{h}:0:0:gray,crop={w}:{h}:0:0,format=yuv420p", w = size.0, h = size.1);
            let mut f = Ffmpeg::spawn(exe, encoder, &input, &filter, base)?;
            f.write(&mkv::header())?;
            ff = Some(f);
        }
        // Media time only moves forward; the first frame is the page at the start (ffmpeg would
        // otherwise start the video at it).
        let at = if last.is_none() { t0 } else { at.max(last.as_ref().map_or(t0, |l| l.1)) };
        let f = ff.as_mut().ok_or_else(|| Error::msg("no encoder"))?;
        f.write(&mkv::frame(at.saturating_sub(t0) / 1_000_000, jpeg.len()))?;
        f.write(&jpeg)?;
        frames += 1;
        last = Some((jpeg, at));
    }
    let Some(mut f) = ff else {
        return Err(super::refused("the page sent no screencast frame (it drew nothing while the video recorded)", "make sure the page is open and drawing while the video records"));
    };
    // The last frame again at the end, so the video lasts until the recording stopped.
    let t1 = (*end.lock().unwrap_or_else(|e| e.into_inner())).max(last.as_ref().map_or(t0, |l| l.1));
    if let Some((jpeg, at)) = &last
        && t1 > *at
    {
        f.write(&mkv::frame((t1 - t0) / 1_000_000, jpeg.len()))?;
        f.write(jpeg)?;
    }
    let encoder_name = f.encoder.name;
    let (path, ext) = (f.out.clone(), f.encoder.ext);
    let encoder_cpu_ms = f.finish()?;
    Ok(Video {
        kind: Kind::Renderer,
        path,
        ext,
        width: size.0,
        height: size.1,
        t0_ns: t0,
        t1_ns: t1,
        // What the file holds: ffmpeg's constant rate over the span, each source frame repeated until the next.
        frames: (t1.saturating_sub(t0) as f64 * ffmpeg::FPS as f64 / 1e9).ceil() as u64,
        source_frames: frames,
        fps: Some(ffmpeg::FPS),
        source: if unstamped > 0 { format!("DevTools Page.startScreencast ({unstamped} frames without a swap time, stamped on arrival)") } else { "DevTools Page.startScreencast".into() },
        encoder: format!("ffmpeg {encoder_name}"),
        cpu_ms: cpu.ms(),
        setup_cpu_ms: None,
        encoder_cpu_ms,
        // A frame lands on the 30 fps grid: up to half a frame from its swap time.
        precision_ms: 1000.0 / ffmpeg::FPS as f64 / 2.0,
    })
}
