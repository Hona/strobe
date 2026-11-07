//! Frames a headless page's renderer produced (SPEC §7.2 "Headless frame production"): with no display,
//! Chromium still begins frames off a clock of its own (no vsync, no screen) and its compositor submits
//! them to the display compositor, which draws them nowhere a person sees. Their count and each one's
//! production time, from the frame's begin (its `PipelineReporter`, which starts at BeginImplFrame) to the
//! end of the renderer's submit stage (`EndActivateToSubmitCompositorFrame`, or its update of the display
//! tree): the main thread's work, the commit, tile raster (`EndCommitToActivation`), activation, and the
//! waits between them, but not the display compositor's draw after the submit (`StartDrawToSwapStart`).
//! Never a refresh rate, late or dropped frames, pacing, presentation or latency: nothing was presented.
//! Compare them only between two builds on one machine in one mode; on a software adapter the raster in
//! them is software's.

use crate::engine;
use crate::model::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// The renderer's submit stages: the compositor frame (or display tree update) leaves the renderer here.
const SUBMIT: [&str; 2] = ["EndActivateToSubmitCompositorFrame", "EndActivateToSubmitUpdateDisplayTree"];

/// Each run window's produced frames: one production time (ms) per frame, in begin order.
pub fn read(trace: &Path, token: &str) -> Result<BTreeMap<String, Vec<f64>>> {
    let sql = format!(
        r#"
create perfetto table _pm as
  select s.name, s.ts, t.upid from slice s
  join thread_track tt on s.track_id = tt.id join thread t using(utid)
  where s.name glob 'strobe:{token}:*';
create perfetto table _pw as
  select substr(a.name, 1, length(a.name) - 6) as k, a.ts as t0, b.ts as t1, a.upid as upid
  from _pm a join _pm b on b.name = substr(a.name, 1, length(a.name) - 6) || ':end'
  where a.name glob '*:start';
select 'frame' as kind, w.k, s.ts,
  extract_arg(s.arg_set_id, 'frame_reporter.frame_source') || ':' || extract_arg(s.arg_set_id, 'frame_reporter.frame_sequence') as seq,
  max(c.ts + c.dur) as submitted,
  coalesce(extract_arg(s.arg_set_id, 'frame_reporter.frame_type'), '') as type
  from slice s join track tr on s.track_id = tr.id
  join _pw w on extract_arg(tr.dimension_arg_set_id, 'upid') = w.upid and s.ts >= w.t0 and s.ts < w.t1
  join slice c on c.parent_id = s.id and c.name in ('{}', '{}')
  where s.name = 'PipelineReporter' and w.k not glob '*:untimed:*'
  group by s.id, w.k
union all
select case when k glob '*:untimed:*' then 'untimed' else 'win' end, k, t0, null, t1, null from _pw;
"#,
        SUBMIT[0], SUBMIT[1]
    );
    let table = engine::query(trace, &sql)?;
    let num = |c: &Option<String>| c.as_deref().and_then(|s| s.parse::<f64>().ok()).unwrap_or(f64::NAN);
    // The bench file's `ctx.untimed` stretches by their run's window key, as `metrics::read` takes them:
    // a frame begun in one is not the run's.
    let mut untimed: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    for row in table.rows.iter().filter(|r| r[0].as_deref() == Some("untimed")) {
        let key = row[1].clone().unwrap_or_default();
        if let Some((head, rest)) = key.split_once(":untimed:") {
            untimed.entry(format!("{head}:run:{}", rest.split('.').next().unwrap_or(""))).or_default().push((num(&row[2]), num(&row[4])));
        }
    }
    let left_out = |key: &str, ts: f64| untimed.get(key.split(":phase:").next().unwrap_or(key)).is_some_and(|g| g.iter().any(|&(a, b)| ts >= a && ts < b));
    // One frame per sequence: Chromium reports a frame more than once. A FORKED report is the
    // compositor's part, submitted while the main thread's part of the same frame was still pending
    // (16 ms beside a stalled 45 ms one): the frame began at the first begin and was produced when the
    // main thread's report submitted, or with only forked ones, the last of them.
    struct Seen {
        begin: f64,
        main: Option<f64>,
        forked: f64,
    }
    let mut frames: BTreeMap<(String, String), Seen> = BTreeMap::new();
    for row in table.rows.iter().filter(|r| r[0].as_deref() == Some("frame")) {
        let (key, ts, seq, submitted) = (row[1].clone().unwrap_or_default(), num(&row[2]), row[3].clone().unwrap_or_default(), num(&row[4]));
        if !ts.is_finite() || !submitted.is_finite() || submitted < ts || left_out(&key, ts) {
            continue;
        }
        let forked = row[5].as_deref() == Some("FORKED");
        let f = frames.entry((key, seq)).or_insert(Seen { begin: ts, main: None, forked: f64::NEG_INFINITY });
        f.begin = f.begin.min(ts);
        if forked {
            f.forked = f.forked.max(submitted);
        } else {
            f.main = Some(f.main.map_or(submitted, |m| m.max(submitted)));
        }
    }
    // Every window, a run that produced no frame too.
    let mut out: BTreeMap<String, Vec<(f64, f64)>> = table.rows.iter().filter(|r| r[0].as_deref() == Some("win")).map(|r| (r[1].clone().unwrap_or_default(), Vec::new())).collect();
    for ((key, _), f) in frames {
        let submitted = f.main.unwrap_or(f.forked);
        out.entry(key).or_default().push((f.begin, (submitted - f.begin) / 1e6));
    }
    Ok(out
        .into_iter()
        .map(|(k, mut v)| {
            v.sort_by(|a, b| a.0.total_cmp(&b.0));
            (k, v.into_iter().map(|x| x.1).collect())
        })
        .collect())
}

/// The values of the runs whose window key holds `:<prefix>:` (not their phases): `produced.frames`,
/// the frames a run produced (median over runs), and `produced.frame.p50` / `.p95`, each frame's
/// production time (ms) over every run's frames, with `produced.n`, how many frames those are.
pub fn values(runs: &BTreeMap<String, Vec<f64>>, prefix: &str) -> BTreeMap<String, f64> {
    let mine: Vec<&Vec<f64>> = runs.iter().filter(|(k, _)| k.contains(&format!(":{prefix}:")) && !k.contains(":phase:")).map(|(_, v)| v).collect();
    let mut out = BTreeMap::new();
    if mine.is_empty() {
        return out;
    }
    let counts: Vec<f64> = mine.iter().map(|v| v.len() as f64).collect();
    out.insert("produced.frames".to_string(), super::median(&counts));
    let mut all: Vec<f64> = mine.iter().flat_map(|v| v.iter().copied()).collect();
    all.sort_by(|a, b| a.total_cmp(b));
    out.insert("produced.n".to_string(), all.len() as f64);
    if !all.is_empty() {
        out.insert("produced.frame.p50".to_string(), super::percentile(&all, 0.5));
        out.insert("produced.frame.p95".to_string(), super::percentile(&all, 0.95));
    }
    out
}

/// The label they carry wherever they show: on a software adapter, its raster is in them.
pub fn label(software: bool) -> &'static str {
    if software { "headless, software raster, never shown" } else { "headless, never shown" }
}

/// What a recording keeps of them (`produced` in its meta), labelled as what they are.
pub fn kept(values: &BTreeMap<String, f64>, software: bool) -> Value {
    json!({ "values": values, "label": label(software), "note": "frames the renderer produced off its own clock, from begin to submit; not presented, so no rate, pacing or latency" })
}

/// `produced  120 frames · p50 3.1 ms · p95 7.9 ms begin to submit (headless, never shown)`.
pub fn line(kept: &Value) -> Option<String> {
    let v = &kept["values"];
    let n = v["produced.frames"].as_f64()?;
    let label = kept["label"].as_str().unwrap_or(label(false));
    if n == 0.0 {
        return Some(format!("produced 0 frames ({label}): the page asked for none\n"));
    }
    let ms = |k: &str| v[k].as_f64().map_or("?".to_string(), |x| format!("{x:.1} ms"));
    Some(format!("produced {n:.0} frames · p50 {} · p95 {} begin to submit ({label})\n", ms("produced.frame.p50"), ms("produced.frame.p95")))
}
