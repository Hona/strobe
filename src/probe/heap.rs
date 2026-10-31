//! V8 heap snapshots over CDP (attribution only, SPEC §8.6): post-GC, streamed into a gzip file so a
//! large snapshot never sits in memory whole.

use crate::model::{Error, Result};
use crate::target::chromium::Page;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

/// Collects garbage twice, then writes a heap snapshot of the page's V8 heap to `path` (gzip,
/// owner-only). Returns the file's size. The page pauses while V8 serializes.
pub fn snapshot(page: &Page, path: &Path) -> Result<u64> {
    page.send("HeapProfiler.enable", json!({}))?;
    for _ in 0..2 {
        page.send("HeapProfiler.collectGarbage", json!({}))?;
    }
    let events = page.cdp.events();
    let (done_tx, done) = channel();
    let p = page.clone();
    // The command replies only after the last chunk, so it waits on its own thread while this one
    // drains chunks; otherwise they would pile up in the event channel.
    std::thread::spawn(move || {
        let params = json!({ "reportProgress": false, "captureNumericValue": false, "exposeInternals": false });
        let _ = done_tx.send(p.cdp.send_timeout("HeapProfiler.takeHeapSnapshot", params, Some(&p.session), Duration::from_secs(1800)));
    });
    let mut out = GzEncoder::new(std::io::BufWriter::with_capacity(1 << 20, create_private(path)?), Compression::fast());
    loop {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(ev) => chunk(&mut out, &ev, &page.session)?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(Error::unreachable("cdp closed during a heap snapshot")),
        }
        if let Ok(reply) = done.try_recv() {
            reply?;
            // Every chunk arrived before the reply.
            while let Ok(ev) = events.try_recv() {
                chunk(&mut out, &ev, &page.session)?;
            }
            break;
        }
    }
    out.finish()?.flush()?;
    Ok(std::fs::metadata(path)?.len())
}

fn chunk(out: &mut impl Write, ev: &Value, session: &str) -> Result<()> {
    if ev["method"] == "HeapProfiler.addHeapSnapshotChunk" && ev["sessionId"] == session {
        out.write_all(ev["params"]["chunk"].as_str().unwrap_or("").as_bytes())?;
    }
    // A snapshot can double the renderer's memory; if that kills it, no reply ever comes.
    if (ev["method"] == "Inspector.targetCrashed" && ev["sessionId"] == session) || (ev["method"] == "Target.detachedFromTarget" && ev["params"]["sessionId"] == session) {
        return Err(Error::unreachable("the page crashed or closed during the heap snapshot"));
    }
    Ok(())
}

/// Heap snapshots hold page data (SPEC rule 9): only the owner may read them.
fn create_private(path: &Path) -> Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    Ok(o.open(path)?)
}
