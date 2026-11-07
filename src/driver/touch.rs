//! Touch input to an emulated phone's page (SPEC 13.1 "Emulated phones and touch screens", 9.6): a phone
//! is driven by a finger, never a mouse. Chromium gets touches over CDP (`Input.dispatchTouchEvent`),
//! Firefox a WebDriver pointer of type `touch` (`input.performActions`). A tap is a press (touchstart)
//! held as a finger rests, then a release (touchend); a drag and a scroll's swipes are a finger moved
//! one step per display frame, paced on strobe's clock as a mouse drag is. What a phone has no device
//! for (hover, a pointer move without a touch, a wheel, a mouse button other than the primary, a
//! modifier key on a tap) is refused `DeviceMismatch` before anything is sent. Every touch strobe sent
//! must reach the page as trusted touch and pointer events (`SCRIPT`): checked between runs, after
//! setup, at each untimed stretch's edges and before a `goto`, or the block is refused `InputMismatch`.

use super::{Driver, Page, no_selector, sent};
use crate::model::{Error, Refusal, Result, ValidatorId};
use crate::target::a11y::keys::{self, Button, Mods};
use serde_json::{Value, json};
use std::cell::Cell;
use std::time::{Duration, Instant};

/// Whether a page call sends pointer input, which on a phone is a touch or refused: every pointer
/// call, a mouse button's `down` and `up`, and `type` and `press` with an element to tap into first.
/// Keys alone go to the keyboard as on a desktop.
pub fn takes(call: &str, args: &[Value]) -> bool {
    let named = |v: Option<&Value>| v.is_some_and(|v| !no_selector(v));
    match call {
        "click" | "hover" | "move" | "drag" | "scroll" | "wheel" => true,
        "down" | "up" => keys::mouse_button(args.first().and_then(Value::as_str).unwrap_or("")).is_some(),
        "type" => named(args.first()),
        "press" => named(args.get(1).map(|o| &o["selector"])),
        _ => false,
    }
}

/// How long a tap's finger rests on the glass between its press and its release: a person's tap is
/// 50 to 100 ms, and the press's frame shows before the release, as a click's does.
const TAP_HOLD: Duration = Duration::from_millis(60);

/// Between a double tap's taps (release to press): a person's, inside the 300 ms in which Android (and
/// Chromium's gesture detector) takes two taps as one double tap, past its 40 ms minimum.
const DOUBLE_TAP_GAP: Duration = Duration::from_millis(100);

/// At a swipe's end the finger rests before it lifts, so the page does not fling past the distance
/// asked for (`preventFling`, as Chromium's scroll gesture holds it): the fling takes its speed from
/// the moves of the last 100 ms. On vps (60 Hz) a 600 px swipe that rested 60 ms scrolled 650 px; 100
/// and 150 ms, 616 to 617, the finger's travel.
const SWIPE_REST: Duration = Duration::from_millis(100);

/// A swipe's finger stays this far (CSS pixels) inside the element's visible part and the viewport.
const EDGE: f64 = 8.0;

/// The least travel a swipe needs to scroll at all: past the browser's touch slop (Chromium's is 15
/// DIP on a phone; Android's 8 dp), with room to move.
const SWIPE_MIN: f64 = 24.0;

/// A swipe's first move (CSS pixels), just past the touch slop: the browser may scroll only from the
/// move after the one that leaves the slop, so that move's distance is lost. A first move of a whole
/// frame's travel (100 px at 2,000 px/s on a 20 Hz page) left a 600 px scroll at 485; with this
/// lead-in, 601 there and 617 at 60 Hz.
const LEAD: f64 = 16.0;

/// Installed in every document before its scripts: counts the trusted touch starts, touch ends (and
/// cancels), touch pointer downs, touch pointer ups (and cancels) and other pointer downs that reached
/// the page. Passive listeners at the window's capture phase; none on touch or pointer moves, which
/// would send a scroll's every move to the page's main thread. A Firefox drag arms a one-shot move
/// listener (`arm`) to read afterwards whether the page prevented its touch's default (`prevented`),
/// and asks first whether the browser could pan where it starts (`pans`).
pub(super) const SCRIPT: &str = r#"window.__strobeTouch ??= (() => {
  const s = { n: [0, 0, 0, 0, 0] }
  const on = (e, f) => addEventListener(e, (ev) => { if (ev.isTrusted) f(ev) }, { capture: true, passive: true })
  let armed = null
  on("touchstart", (e) => { s.n[0]++; if (armed && !armed[0]) armed[0] = e })
  on("touchend", () => s.n[1]++)
  on("touchcancel", () => s.n[1]++)
  on("pointerdown", (e) => s.n[e.pointerType === "touch" ? 2 : 4]++)
  for (const e of ["pointerup", "pointercancel"]) on(e, (ev) => { if (ev.pointerType === "touch") s.n[3]++ })
  const moved = (e) => { if (e.isTrusted && armed && !armed[1]) armed[1] = e }
  s.arm = () => { armed = [null, null]; addEventListener("touchmove", moved, { capture: true, passive: true, once: true }); return true }
  s.prevented = () => { const p = !!armed && armed.some((e) => e?.defaultPrevented); armed = null; removeEventListener("touchmove", moved, true); return p }
  const up = (p) => p.parentElement ?? p.getRootNode().host ?? null
  const hidden = (el, ov) => /hidden|clip/.test(getComputedStyle(el)[ov])
  // Whether a finger down at (x, y) and moved along x (`ax`) or y (`ay`) could pan: touch-action on the
  // element there and its ancestors leaves that axis to the browser, and it or an ancestor (the document
  // too) can scroll along it.
  s.pans = (x, y, ax, ay) => {
    const at = document.elementFromPoint(x, y)
    let px = true, py = true
    for (let p = at; p; p = up(p)) {
      const t = getComputedStyle(p).touchAction
      if (t === "none") px = py = false
      else if (t !== "auto" && t !== "manipulation") { if (!/pan-(x|left|right)/.test(t)) px = false; if (!/pan-(y|up|down)/.test(t)) py = false }
    }
    const root = document.scrollingElement
    const scrolls = (p, horizontal) => {
      const ov = horizontal ? "overflowX" : "overflowY"
      const room = horizontal ? p.scrollWidth > p.clientWidth : p.scrollHeight > p.clientHeight
      if (p === root) return room && !hidden(document.documentElement, ov) && !(document.body && hidden(document.body, ov))
      return room && /auto|scroll/.test(getComputedStyle(p)[ov])
    }
    const can = (horizontal) => { for (let p = at; p; p = up(p)) if (scrolls(p, horizontal)) return true; return !!root && scrolls(root, horizontal) }
    return (ax && px && can(true)) || (ay && py && can(false))
  }
  return s
})(), true
//# sourceURL=strobe-touch-input.js"#;

/// The page's counts and its document (`performance.timeOrigin`) in one read.
const READ: &str = "JSON.stringify([performance.timeOrigin, ...(window.__strobeTouch?.n ?? [0, 0, 0, 0, 0])])";

/// What `SCRIPT` counts that strobe checks, by index into its `n` and `Touch::posted`: every touch
/// strobe started and ended arrives as a touch start, a touch end, a touch pointer down and a touch
/// pointer up or cancel.
const COUNTED: [&str; 4] = ["touch starts", "touch ends", "touch pointer downs", "touch pointer ups or cancels"];
const START: usize = 0;
const END: usize = 1;
const DOWN: usize = 2;
const UP: usize = 3;
/// Pointer downs of a mouse or pen: strobe sends none on a phone.
const OTHER: usize = 4;

/// A finger's phase, as each engine names it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Start,
    Move,
    End,
}

/// An emulated phone's touch state: where the finger is down (none while lifted), and what strobe
/// touched since the page's counts were last checked, in that document.
#[derive(Default)]
pub struct Touch {
    finger: Cell<Option<(f64, f64)>>,
    posted: Cell<[u64; 4]>,
    /// The checked document and its counts then.
    base: Cell<(f64, [f64; 5])>,
}

/// `DeviceMismatch` for a call a phone has no device for.
fn no_device(call: &str, why: &str, fix: &str) -> Error {
    Refusal::new(ValidatorId::DeviceMismatch, format!("page.{call} on a page that emulates a phone: {why}")).fix(fix).into()
}

/// `EngineUnsupported` for `what` on a Firefox phone: headless Firefox does not pan on touch (no async
/// panning: every touch move goes to the page, none is cancelled for a pan, and a scroll box stayed put
/// while the document scrolled instead, on adais and vps alike), so it would not be a phone's.
fn firefox_pans(call: &str, what: &str) -> Error {
    Refusal::new(ValidatorId::EngineUnsupported, format!("{call} on a Firefox page that emulates a phone: headless Firefox does not pan on touch (every touch move goes to the page, none is cancelled for a pan, and a scroll box stays put while the document scrolls instead), so {what} there is not a phone's"))
        .fix("measure the phone's scrolls and pans in Chromium; in Firefox drag only what the page keeps for itself (touch-action: none, or a touch it prevents)")
        .into()
}

/// The page's counts now: its document and `SCRIPT`'s `n`.
fn read(page: &Page) -> Result<(f64, [f64; 5])> {
    let v = page.eval(READ)?;
    let r: Vec<f64> = v.as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
    Ok((r.first().copied().unwrap_or(0.0), std::array::from_fn(|i| r.get(i + 1).copied().unwrap_or(0.0))))
}

impl Touch {
    /// Installs the counts in every document and in the one the page shows.
    pub fn attach(page: &Page) -> Result<Touch> {
        page.add_init_script(SCRIPT)?;
        page.eval(SCRIPT)?;
        let t = Touch::default();
        t.base.set(read(page)?);
        Ok(t)
    }

    fn post(&self, i: usize) {
        let mut p = self.posted.get();
        p[i] += 1;
        self.posted.set(p);
    }

    /// Takes the page's counts as they are as the base of the next check (a new document's, after a
    /// load strobe made).
    pub fn rebase(&self, page: &Page) -> Result<()> {
        self.posted.set([0; 4]);
        self.base.set(read(page)?);
        Ok(())
    }

    /// Refuses `InputMismatch` unless every touch strobe sent since the last check reached the page:
    /// a touch start, a touch end and a touch pointer down for each, within a second of looking, and no
    /// mouse or pen press. A document that changed meanwhile (a tap on a link) took its counts with it:
    /// nothing to check there.
    pub fn delivered(&self, page: &Page) -> Result<()> {
        let posted = self.posted.get();
        let (origin, base) = self.base.get();
        let until = Instant::now() + Duration::from_secs(1);
        let (now, n) = loop {
            let (now, n) = read(page)?;
            let enough = (0..COUNTED.len()).all(|i| n[i] - base[i] >= posted[i] as f64);
            if now != origin || enough || Instant::now() >= until {
                break (now, n);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        self.posted.set([0; 4]);
        self.base.set((now, n));
        if now != origin {
            return Ok(());
        }
        let refused = |summary: String| -> Error {
            Refusal::new(ValidatorId::InputMismatch, summary)
                .fix("strobe sends an emulated phone touches (CDP's Input.dispatchTouchEvent, WebDriver's touch pointer); a browser that turns them into something else, or drops them, cannot measure the phone: run it in Chromium or Firefox as strobe launches them")
                .into()
        };
        for (i, what) in COUNTED.iter().enumerate() {
            let got = n[i] - base[i];
            if got < posted[i] as f64 {
                return Err(refused(format!("touch input did not reach the page: strobe sent {} {what} and the page received {got} in a second", posted[i])));
            }
        }
        let other = n[OTHER] - base[OTHER];
        if other > 0.0 {
            return Err(refused(format!("the page received {other} mouse or pen pointer downs while strobe sent it only touches")));
        }
        Ok(())
    }
}

impl Driver {
    /// Makes the page's pointer input touch (an emulated phone's page): from now on every pointer call
    /// is a touch or refused (`touch_dispatch`).
    pub fn touch_input(&self) -> Result<()> {
        if self.touch.get().is_none() {
            let _ = self.touch.set(Touch::attach(&self.page)?);
        }
        Ok(())
    }

    /// After a load strobe made itself (not through `goto`): what strobe touches from now on is checked
    /// in that document.
    pub fn touch_loaded(&self) -> Result<()> {
        match self.touch.get() {
            Some(t) => t.rebase(&self.page),
            None => Ok(()),
        }
    }

    /// What strobe touched since the last check reached the page (`Touch::delivered`): after setup and
    /// at each untimed stretch's edges, where a load would otherwise take the counts along unchecked.
    pub fn touch_delivered(&self) -> Result<()> {
        match self.touch.get() {
            Some(t) => t.delivered(&self.page),
            None => Ok(()),
        }
    }

    /// One finger going down, moving or lifting at a viewport point (CSS pixels). `paced`: it goes out
    /// without waiting for the page to handle it (a drag's moves; a finger going down, whose hold or first
    /// move counts from when it went out: Chromium answered a touch start only once its renderer had
    /// handled it, some 100 ms on a busy headless page). A lift is waited for, and with it all before it.
    fn finger(&self, t: &Touch, phase: Phase, (x, y): (f64, f64), paced: bool) -> Result<()> {
        let point = || json!([{ "x": x, "y": y, "id": 0 }]);
        match &self.page {
            Page::Chromium(p) => {
                let (kind, points) = match phase {
                    Phase::Start => ("touchStart", point()),
                    Phase::Move => ("touchMove", point()),
                    Phase::End => ("touchEnd", json!([])),
                };
                let e = json!({ "type": kind, "touchPoints": points });
                if paced { p.post("Input.dispatchTouchEvent", e)? } else { p.send("Input.dispatchTouchEvent", e).map(drop)? }
            }
            Page::Firefox(_) | Page::Webkit(_) => {
                let at = json!({ "type": "pointerMove", "x": x.round() as i64, "y": y.round() as i64, "origin": "viewport" });
                let actions = match phase {
                    Phase::Start => vec![at, json!({ "type": "pointerDown", "button": 0 })],
                    Phase::Move => vec![at],
                    Phase::End => vec![json!({ "type": "pointerUp", "button": 0 })],
                };
                let list = json!([{ "type": "pointer", "id": "finger", "parameters": { "pointerType": "touch" }, "actions": actions }]);
                if paced { self.page.post_actions(list)? } else { self.page.actions(list)? }
            }
        }
        super::touched();
        match phase {
            Phase::Start => {
                t.finger.set(Some((x, y)));
                t.post(START);
                t.post(DOWN);
            }
            Phase::Move => t.finger.set(Some((x, y))),
            Phase::End => {
                t.finger.set(None);
                t.post(END);
                t.post(UP);
            }
        }
        Ok(())
    }

    /// An input time a latency sample is timed from: Chromium's trace times its touches' events itself
    /// (the action log's edges only, as for its clicks); a WebDriver page's are joined to its frames.
    fn touch_sample(&self, scroll: bool, press: Option<u64>) {
        if self.page.cdp().is_some() {
            sent(crate::host::now_ns(), true, press);
        } else {
            self.input_at(scroll, press);
        }
    }

    /// Taps `count` times at a point. A single tap is one sample (its press when the page acted on it,
    /// else its release, SPEC 7.4): the finger rests `TAP_HOLD`, so the press's frame shows before the
    /// release. A double or triple tap's taps follow `DOUBLE_TAP_GAP` apart and are none.
    fn tap(&self, t: &Touch, at: (f64, f64), count: u64) -> Result<()> {
        self.settle(Duration::ZERO)?;
        for k in 1..=count {
            if k > 1 {
                std::thread::sleep(DOUBLE_TAP_GAP);
            }
            let pressed = crate::host::now_ns();
            sent(pressed, false, None);
            self.finger(t, Phase::Start, at, true)?;
            self.settle(TAP_HOLD)?;
            if count == 1 {
                self.touch_sample(false, Some(pressed));
            } else {
                sent(crate::host::now_ns(), false, None);
            }
            self.finger(t, Phase::End, at, false)?;
        }
        Ok(())
    }

    /// A finger along `path` (offsets from `from`) over `duration`, one move per display frame paced on
    /// strobe's clock, as a mouse drag's moves are; kept for `input.rate` and `input.gap.p95` (a touch
    /// start waited for had held the first move: `input.gap.p95` 152 ms against 50 on a 20 Hz page).
    /// With `first`, the first move is a scroll's sample, as a wheel's first turn is. With `held`, the
    /// last move is waited for, so a rest after it is a rest on the browser's clock too, not on strobe's
    /// alone while posted moves still queue.
    fn swipe(&self, t: &Touch, from: (f64, f64), path: &[(f64, f64)], duration: f64, first: bool, held: bool) -> Result<()> {
        let frame = self.frame_period()?;
        let (dx, dy) = keys::path_at(path, 0.0);
        let steps = (duration / frame).ceil().max(1.0) as u32;
        let start = Instant::now();
        let start_at = crate::host::now_ns();
        self.drags.borrow_mut().push(vec![start_at]);
        sent(start_at, false, None);
        self.finger(t, Phase::Start, (from.0 + dx, from.1 + dy), true)?;
        for k in 1..=steps {
            self.pacer.wait(start + Duration::from_secs_f64(frame * k as f64 / 1e3), Duration::from_secs_f64(frame / 1e3));
            let (dx, dy) = keys::path_at(path, k as f64 / steps as f64);
            let now = crate::host::now_ns();
            if k == 1 && first {
                self.touch_sample(true, None);
            } else {
                sent(now, false, None);
            }
            if let Some(d) = self.drags.borrow_mut().last_mut() {
                d.push(now);
            }
            self.finger(t, Phase::Move, (from.0 + dx, from.1 + dy), !(held && k == steps))?;
        }
        Ok(())
    }

    /// `scroll` on a phone: the finger swipes the page the other way at `speed` (CSS px a second), from
    /// the trailing side of the element's visible part (`__strobe.trail`: its bottom for a scroll down,
    /// inset by `EDGE`, where a hit test finds the element; else the aim point), in as many equal swipes
    /// as keep it inside the viewport; each rests `SWIPE_REST` before it lifts, so the page does not
    /// fling. The page scrolls by the finger's distance less what the browser takes as the touch's slop.
    /// Refused `DisplayTooSmall` when the finger has under `SWIPE_MIN` to travel: swipes that never clear
    /// the slop would scroll nothing. Not Chromium's own touch scroll (`Input.synthesizeScrollGesture`
    /// from a touch source): it moved the finger over a headless page without scrolling it.
    fn swipe_scroll(&self, t: &Touch, sel: &crate::target::a11y::Selector, aim: (f64, f64), (dx, dy): (f64, f64), speed: f64) -> Result<()> {
        let v = self.read(&format!("__strobe.trail({}, {dx}, {dy}, {EDGE})", sel.page_spec()), Duration::from_secs(10))?;
        let num = |i: usize| v[i].as_f64();
        let at = num(0).zip(num(1)).unwrap_or(aim);
        let (w, h) = (num(2).unwrap_or(0.0), num(3).unwrap_or(0.0));
        // The room the finger has along an axis it moves by `d` (the other way to the scroll).
        let room = |p: f64, d: f64, size: f64| if d > 0.0 { p - EDGE } else { size - EDGE - p };
        let short = [(dx, at.0, w), (dy, at.1, h)].into_iter().filter(|(d, ..)| *d != 0.0).map(|(d, p, size)| room(p, d, size)).fold(f64::INFINITY, f64::min);
        if short - LEAD < SWIPE_MIN {
            return Err(Refusal::new(
                ValidatorId::DisplayTooSmall,
                format!("page.scroll({}) on a phone: a finger at ({:.0}, {:.0}) has {:.0} px to travel past its lead-in inside the {w:.0}×{h:.0} viewport, too little to clear the browser's touch slop", sel.source, at.0, at.1, short - LEAD),
            )
            .fix("scroll an element with more of itself on the screen, or page.drag the finger along a path that fits")
            .into());
        }
        // Each swipe's finger travels its share of the distance plus the lead-in, inside the room.
        let swipes = |d: f64, p: f64, size: f64| if d == 0.0 { 1.0 } else { (d.abs() / (room(p, d, size) - LEAD)).ceil() };
        let n = swipes(dx, at.0, w).max(swipes(dy, at.1, h)).max(1.0);
        let (sx, sy) = (dx / n, dy / n);
        let len = sx.hypot(sy);
        let (lx, ly) = (sx / len * LEAD, sy / len * LEAD);
        // One keyframe a frame: the lead-in on the first, then evenly to the end at `speed`.
        let frame = self.frame_period()?;
        let steps = ((len / speed.max(1.0) * 1e3) / frame).ceil().max(1.0) as usize + 1;
        let path: Vec<(f64, f64)> = (0..=steps).map(|k| if k == 0 { (0.0, 0.0) } else { let f = (k - 1) as f64 / (steps - 1) as f64; (-lx - sx * f, -ly - sy * f) }).collect();
        self.settle(Duration::ZERO)?;
        for i in 0..n as u32 {
            if i > 0 {
                std::thread::sleep(Duration::from_secs_f64(frame / 1e3));
            }
            self.swipe(t, at, &path, frame * steps as f64 - 1e-6, i == 0, true)?;
            std::thread::sleep(SWIPE_REST);
            sent(crate::host::now_ns(), false, None);
            // Waited for: back once the page has handled every move before it.
            self.finger(t, Phase::End, (at.0 - lx - sx, at.1 - ly - sy), false)?;
        }
        Ok(())
    }

    /// Lifts a finger an interrupted gesture left down, then checks what strobe touched reached the
    /// page (`Touch::delivered`): between runs, and after a reset.
    pub(super) fn touch_release(&self, t: &Touch) -> Result<()> {
        if let Some(at) = t.finger.get() {
            self.finger(t, Phase::End, at, false)?;
        }
        t.delivered(&self.page)
    }

    /// A pointer call on an emulated phone's page (`POINTER`): a touch, or refused `DeviceMismatch`
    /// for input a phone has no device for. Keys (`press`, `type`, `down` and `up` of a key) go to the
    /// keyboard as on a desktop, after a tap into the element they name.
    pub(super) fn touch_dispatch(&self, t: &Touch, call: &str, args: &[Value]) -> Result<Value> {
        let opts = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
        let sel = |i: usize| args.get(i).cloned().unwrap_or(Value::String(String::new()));
        let primary = |o: &Value| -> Result<()> {
            match Button::parse(o["button"].as_str())? {
                Button::Left => Ok(()),
                b => Err(no_device(
                    call,
                    &format!("a finger has no {} button", format!("{b:?}").to_lowercase()),
                    "tap or drag with the finger (no button option); measure right and middle clicks on a desktop target",
                )),
            }
        };
        match call {
            "click" => {
                let o = opts(1);
                primary(&o)?;
                if Mods::parse_list(&o["modifiers"])?.any() || self.mods.get() != 0 {
                    return Err(no_device(
                        call,
                        "a tap carries no modifier keys (given in modifiers, or held by page.down)",
                        "drop the modifiers and let go of held ones (page.up), or measure the modified click on a desktop target",
                    ));
                }
                let at = self.locate(&Self::sel(&sel(0))?, &o)?;
                self.tap(t, at, o["count"].as_u64().unwrap_or(1).clamp(1, 3))?;
                self.until(&o)
            }
            "type" | "press" => {
                let (target, o) = if call == "type" { (sel(0), opts(2)) } else { (opts(1)["selector"].clone(), opts(1)) };
                let (x, y, focused) = self.aim_for(&Self::sel(&target)?, &o, true)?;
                if !focused {
                    // A tap into the field, never a sample; its frames (a caret, a focus ring) settle
                    // before the first key, which is measured.
                    self.settle(Duration::ZERO)?;
                    sent(crate::host::now_ns(), false, None);
                    self.finger(t, Phase::Start, (x, y), true)?;
                    std::thread::sleep(TAP_HOLD);
                    self.finger(t, Phase::End, (x, y), false)?;
                    self.settle(self.refresh.get() * 2)?;
                }
                // The keys then go out as on a desktop, where focus now is.
                let mut args = args.to_vec();
                if call == "type" {
                    args[0] = Value::String(String::new());
                } else if let Some(o) = args.get_mut(1).and_then(Value::as_object_mut) {
                    o.remove("selector");
                }
                self.dispatch(call, &args)
            }
            "hover" | "move" => Err(no_device(call, "a finger does not hover, and moves only while it touches", "tap the element (page.click), or drag across it (page.drag); measure hover on a desktop target")),
            "wheel" => Err(no_device(call, "a phone has no mouse wheel", "scroll it with the finger: page.scroll(selector, { by })")),
            "down" | "up" => Err(no_device(
                call,
                "a phone has no mouse button to hold",
                "tap with page.click, or put the finger down, move it and lift it with page.drag(selector, path); keys still go down and up",
            )),
            "drag" => {
                let o = opts(2);
                primary(&o)?;
                let path = keys::parse_path(&opts(1))?;
                // No finger stays down between calls, so a drag starts on its element.
                if no_selector(&sel(0)) {
                    return Err(Error::usage("page.drag(\"\", …) on a phone: a finger is down only inside a call, so a drag starts on an element: give its selector"));
                }
                let from = self.locate(&Self::sel(&sel(0))?, &o)?;
                // Headless Firefox would scroll by its own rule where a browser pans: the drag must be one
                // the page keeps for itself (touch-action, nothing to scroll, or its touch's default
                // prevented, read after it).
                let (ax, ay) = (path.iter().any(|p| p.0 != 0.0), path.iter().any(|p| p.1 != 0.0));
                let pans = self.page.firefox().is_some() && {
                    self.reads.set(self.reads.get() + 1);
                    self.page.eval(&format!("__strobeTouch.pans({}, {}, {ax}, {ay}) && __strobeTouch.arm()", from.0, from.1))? == Value::Bool(true)
                };
                let duration = o.get("duration").and_then(Value::as_f64).unwrap_or(1000.0).max(1.0);
                self.settle(Duration::ZERO)?;
                self.swipe(t, from, &path, duration, false, false)?;
                let (dx, dy) = keys::path_at(&path, 1.0);
                sent(crate::host::now_ns(), false, None);
                self.finger(t, Phase::End, (from.0 + dx, from.1 + dy), false)?;
                if pans && self.page.eval("__strobeTouch.prevented()")? != Value::Bool(true) {
                    return Err(firefox_pans("page.drag", "a drag where the browser would pan (touch-action leaves it the axis, something there scrolls along it, and the page prevented neither the touch's start nor its first move)"));
                }
                self.until(&o)
            }
            "scroll" if self.page.firefox().is_some() => Err(firefox_pans("page.scroll", "a swipe")),
            "scroll" => {
                let o = opts(1);
                let sel = Self::sel(&sel(0))?;
                let at = self.locate(&sel, &opts(2))?;
                let by = o.get("by").and_then(Value::as_f64).unwrap_or(0.0);
                let by_x = o.get("byX").and_then(Value::as_f64).unwrap_or(0.0);
                let speed = o.get("speed").and_then(Value::as_f64).unwrap_or(2000.0);
                self.swipe_scroll(t, &sel, at, (by_x, by), speed)?;
                self.until(&opts(2))
            }
            other => Err(Error::msg(format!("internal: page.{other} is no pointer call"))),
        }
    }
}
