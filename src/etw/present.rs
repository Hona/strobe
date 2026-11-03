//! Presents to the screen from ETW, ported from PresentMon's consumer (GameTechDev/PresentMon,
//! PresentData/PresentMonTraceConsumer.cpp, MIT): the subset that follows DXGI flip model
//! (composition swapchains included), Vulkan and legacy blt presents to their screen time.
//!
//! A present starts at the runtime's `Present_Start` (DXGI) or, without one (Vulkan, DWM), at the
//! first kernel event of the present. The kernel then says how it is shown:
//!
//! - `Composed_Flip`: Win32k gives the present a composition token; DWM composes it (InFrame,
//!   `SCHEDULE_SURFACEUPDATE`) into its own next present, and when DWM's present scans out
//!   (`VSyncDPC` by submit sequence), so does this one.
//! - `Hardware_Independent_Flip`: the token's InFrame says independent flip; the present's own flip
//!   packet scans out (`MMIOFlip`, `VSyncDPC`).
//! - `Hardware_Composed_Independent_Flip`: the same, on a multi-plane overlay plane.
//! - `Hardware_Legacy_Flip`: a fullscreen flip (and DWM's own presents).
//!
//! A present replaced before it was shown (a newer one InFrame for the window, a Discarded token, a
//! later present of the swapchain displayed first) ends `discarded`.

use super::session::{self, Field, Handler, Provider, Running, Schema, uint, uint_at};
use super::{Present, Presents};
use crate::model::Result;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use windows_sys::Win32::System::Diagnostics::Etw::EVENT_RECORD;
use windows_sys::core::GUID;

const DXGI: &str = "CA11C036-0102-4A2D-A6AD-F03CFED5D3C9";
const DXGKRNL: &str = "802EC45A-1E99-4B83-9920-87C98277BA9D";
const WIN32K: &str = "8C416C79-D49B-4F01-A467-E56D3AA8234C";
const DWM: &str = "9E9BBA3C-2E38-40CB-99F4-9E8281425164";

mod id {
    // Microsoft-Windows-DXGI
    pub const PRESENT_START: u16 = 0x2a;
    pub const PRESENT_STOP: u16 = 0x2b;
    pub const PRESENT_MPO_START: u16 = 0x37;
    pub const PRESENT_MPO_STOP: u16 = 0x38;
    // Microsoft-Windows-DxgKrnl
    pub const BLIT: u16 = 0xa6;
    pub const BLIT_CANCEL: u16 = 0x1f5;
    pub const FLIP: u16 = 0xa8;
    pub const FLIP_MPO: u16 = 0xfc;
    pub const INDEPENDENT_FLIP: u16 = 0x10a;
    pub const HSYNC_MPO: u16 = 0x17e;
    pub const VSYNC_MPO: u16 = 0x111;
    pub const MMIO_FLIP: u16 = 0x74;
    pub const MMIO_FLIP_MPO: u16 = 0x103;
    pub const PRESENT_INFO: u16 = 0xb8;
    pub const PRESENT_HISTORY_INFO: u16 = 0xac;
    pub const PRESENT_HISTORY_START: u16 = 0xab;
    pub const PRESENT_HISTORY_DETAILED: u16 = 0xd7;
    pub const QUEUE_START: u16 = 0xb2;
    pub const QUEUE_START_2: u16 = 0xf4;
    pub const QUEUE_STOP: u16 = 0xb4;
    pub const VSYNC: u16 = 0x11;
    // Microsoft-Windows-Win32k
    pub const TOKEN_SURFACE: u16 = 0xc9;
    pub const TOKEN_STATE: u16 = 0x12d;
    // Microsoft-Windows-Dwm-Core
    pub const GET_PRESENT_HISTORY: u16 = 0x40;
    pub const SCHEDULE_PRESENT: u16 = 0x0f;
    pub const SCHEDULE_SURFACEUPDATE: u16 = 0xc4;
    pub const FLIPCHAIN_PENDING: u16 = 0x45;
    pub const FLIPCHAIN_COMPLETE: u16 = 0x46;
    pub const FLIPCHAIN_DIRTY: u16 = 0x65;
}

/// The providers and events PresentMon enables to track presents to the display, with DxgKrnl's
/// heavy Performance keyword left out (PresentMon#111).
pub fn providers() -> Vec<Provider> {
    use id::*;
    vec![
        Provider {
            guid: session::guid(DXGKRNL),
            level: 4,
            any: 0x0800_0001,
            all: 0x1,
            ids: vec![
                PRESENT_HISTORY_START,
                BLIT,
                BLIT_CANCEL,
                FLIP,
                INDEPENDENT_FLIP,
                FLIP_MPO,
                HSYNC_MPO,
                VSYNC_MPO,
                MMIO_FLIP,
                MMIO_FLIP_MPO,
                PRESENT_INFO,
                PRESENT_HISTORY_INFO,
                PRESENT_HISTORY_DETAILED,
                QUEUE_START,
                QUEUE_START_2,
                QUEUE_STOP,
                VSYNC,
            ],
        },
        Provider { guid: session::guid(WIN32K), level: 4, any: 0x8000_0004_0000_1000, all: 0x8000_0000_0000_1000, ids: vec![TOKEN_SURFACE, TOKEN_STATE] },
        Provider {
            guid: session::guid(DWM),
            level: 5,
            any: 0x8000_0000_0000_0081,
            all: 0,
            ids: vec![GET_PRESENT_HISTORY, SCHEDULE_PRESENT, SCHEDULE_SURFACEUPDATE, FLIPCHAIN_PENDING, FLIPCHAIN_COMPLETE, FLIPCHAIN_DIRTY],
        },
        Provider { guid: session::guid(DXGI), level: 0, any: 0x8000_0000_0000_0002, all: 0x8000_0000_0000_0002, ids: vec![PRESENT_START, PRESENT_STOP, PRESENT_MPO_START, PRESENT_MPO_STOP] },
    ]
}

/// What the tracker hands back: presents, and vertical blanks per display path.
type Tracked = (Vec<Present>, Vec<(u64, u32)>);

/// A present session; `stop` returns every present and vertical blank it saw.
pub struct Capture {
    running: Running<Tracked>,
    t0: u64,
    /// Presents as they complete, for waits while the session runs.
    pub live: std::sync::Arc<super::Live>,
}

impl Capture {
    /// Starts tracking presents in this process (needs admin or Performance Log Users). Buffers
    /// flush every 10 ms, so a wait on the next frame hears of it within a frame or two.
    pub fn start() -> Result<Capture> {
        let cfg = session::Config { kind: "presents", kernel: 0, providers: providers(), buffer_kb: 64, buffers: (32, 256), flush_ms: 10 };
        let t0 = crate::host::now_ns();
        let armed = Arc::new(AtomicU64::new(0));
        let tracker_armed = armed.clone();
        let live = std::sync::Arc::new(super::Live::default());
        let l = live.clone();
        let running = Running::start(cfg, |_| Ok(()), move || Tracker::with_armed(tracker_armed, Some(l)))?;
        // Every provider runs from here: presents that start later are seen whole.
        armed.store(session::qpc_now(), Ordering::Release);
        Ok(Capture { running, t0, live })
    }

    pub fn stop(self) -> Result<Presents> {
        let t1 = crate::host::now_ns();
        let ((presents, vsyncs), stats) = self.running.stop()?;
        Ok(Presents { presents, vsyncs, events: stats.events, events_lost: stats.events_lost + stats.buffers_lost, consumer_cpu_ms: stats.consumer_cpu_ms, t0_ns: self.t0, t1_ns: t1 })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Unknown,
    LegacyFlip,
    LegacyCopy,
    IndependentFlip,
    ComposedFlip,
    ComposedCopyGpu,
    ComposedCopyCpu,
    ComposedIndependentFlip,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Unknown => "Unknown",
            Mode::LegacyFlip => "Hardware_Legacy_Flip",
            Mode::LegacyCopy => "Hardware_Legacy_Copy_To_Front_Buffer",
            Mode::IndependentFlip => "Hardware_Independent_Flip",
            Mode::ComposedFlip => "Composed_Flip",
            Mode::ComposedCopyGpu => "Composed_Copy_GPU_GDI",
            Mode::ComposedCopyCpu => "Composed_Copy_CPU_GDI",
            Mode::ComposedIndependentFlip => "Hardware_Composed_Independent_Flip",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Final {
    #[default]
    Unknown,
    Presented,
    Discarded,
}

#[derive(Default)]
struct P {
    serial: u64,
    start: u64,
    pid: u32,
    tid: u32,
    time_in_present: u64,
    ready: u64,
    screen: u64,
    swapchain: u64,
    sync_interval: i32,
    flags: u32,
    hwnd: u64,
    /// Win32k's composition surface (its `CompositionSurfaceLuid`), kept after the token ends.
    luid: u64,
    submit_seq: u32,
    w32k: (u64, u64, u64),
    dxgk_token: u64,
    dxgk_token_data: u64,
    driver_tid: u32,
    dxgi: bool,
    mode: Mode,
    state: Final,
    seen_dxgk_present: bool,
    seen_win32k: bool,
    seen_in_frame: bool,
    wait_for_flip: bool,
    wait_for_mpo_flip: bool,
    wait_for_sync_dpc: bool,
    completed: bool,
    lost: bool,
    failed: bool,
    waiting_for_stop: bool,
    in_dwm_waiting: bool,
    dependents: Vec<Ptr>,
    ring: Option<usize>,
}

type Ptr = Rc<RefCell<P>>;

/// What one present updates on screen, for "one present per surface per composed frame". A window's
/// handle when the present has one; a composition swapchain (DirectComposition, every one with
/// handle 0) by its composition surface, else by process and swapchain. Never handle 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Surface {
    Window(u64),
    Composition(u64),
    Swapchain(u32, u64),
}

fn surface(b: &P) -> Option<Surface> {
    if b.hwnd != 0 {
        Some(Surface::Window(b.hwnd))
    } else if b.luid != 0 {
        Some(Surface::Composition(b.luid))
    } else if b.swapchain != 0 {
        Some(Surface::Swapchain(b.pid, b.swapchain))
    } else {
        None
    }
}

const RING: usize = 2048;
const DXGI_PRESENT_TEST: u32 = 1;
const DXGI_PRESENT_DO_NOT_SEQUENCE: u32 = 2;
const MODEL_REDIRECTED_GDI: u64 = 1;
const MODEL_REDIRECTED_COMPOSITION: u64 = 7;
const PACKET_MMIOFLIP: u64 = 3;
const PACKET_SOFTWARE: u64 = 7;
const FLIP_IMMEDIATE: u64 = 2;
const FLIP_WAIT_VSYNC: u64 = 5;
const FLIP_WAIT_HSYNC: u64 = 15;
const TOKEN_IN_FRAME: u64 = 3;
const TOKEN_CONFIRMED: u64 = 4;
const TOKEN_DISCARDED: u64 = 6;

pub struct Tracker {
    schema: Schema,
    freq: u64,
    guids: [GUID; 4],
    serial: u64,
    ring: Vec<Option<Ptr>>,
    next_ring: usize,
    by_thread: HashMap<u32, Ptr>,
    by_process: HashMap<u32, BTreeMap<(u64, u64), Ptr>>,
    by_submit: HashMap<u32, Vec<(u64, Ptr)>>,
    by_w32k: HashMap<(u64, u64, u64), Ptr>,
    by_dxgk_token: HashMap<u64, Ptr>,
    by_dxgk_token_data: HashMap<u64, Ptr>,
    last_by_window: HashMap<Surface, Ptr>,
    waiting_for_dwm: VecDeque<Ptr>,
    dwm_tid: u32,
    has_completed: bool,
    /// Raw QPC from which every provider runs (0 until the session is up).
    armed: Arc<AtomicU64>,
    done: Vec<Ptr>,
    out: Vec<Present>,
    vsyncs: Vec<(u64, u32)>,
    live: Option<std::sync::Arc<super::Live>>,
}

impl Handler for Tracker {
    type Out = Tracked;

    fn event(&mut self, e: &EVENT_RECORD) {
        let g = &e.EventHeader.ProviderId;
        if session::same(g, &self.guids[1]) {
            self.dxgk(e);
        } else if session::same(g, &self.guids[0]) {
            self.dxgi(e);
        } else if session::same(g, &self.guids[2]) {
            self.win32k(e);
        } else if session::same(g, &self.guids[3]) {
            self.dwm(e);
        }
        if self.done.len() >= 4096 {
            self.drain(false);
        }
    }

    fn finish(mut self) -> Self::Out {
        self.drain(true);
        self.out.sort_by_key(|p| p.start_ns);
        (std::mem::take(&mut self.out), std::mem::take(&mut self.vsyncs))
    }
}

fn hdr(e: &EVENT_RECORD) -> (u64, u32, u32) {
    (e.EventHeader.TimeStamp as u64, e.EventHeader.ProcessId, e.EventHeader.ThreadId)
}

impl Tracker {
    pub fn new() -> Tracker {
        Tracker::with_armed(Arc::new(AtomicU64::new(0)), None)
    }

    /// A tracker for a session whose providers all run from the raw QPC `armed` holds once set;
    /// `live` hears of each present as it completes.
    pub fn with_armed(armed: Arc<AtomicU64>, live: Option<std::sync::Arc<super::Live>>) -> Tracker {
        Tracker {
            armed,
            schema: Schema::default(),
            freq: session::qpc_freq(),
            guids: [session::guid(DXGI), session::guid(DXGKRNL), session::guid(WIN32K), session::guid(DWM)],
            serial: 0,
            ring: vec![None; RING],
            next_ring: 0,
            by_thread: HashMap::new(),
            by_process: HashMap::new(),
            by_submit: HashMap::new(),
            by_w32k: HashMap::new(),
            by_dxgk_token: HashMap::new(),
            by_dxgk_token_data: HashMap::new(),
            last_by_window: HashMap::new(),
            waiting_for_dwm: VecDeque::new(),
            dwm_tid: 0,
            has_completed: false,
            done: Vec::new(),
            out: Vec::new(),
            vsyncs: Vec::new(),
            live,
        }
    }

    /// Moves completed presents whose present call returned (or every one, at the end) to the output.
    fn drain(&mut self, all: bool) {
        let freq = self.freq;
        let ns = |q: u64| if q == 0 { 0 } else { session::ns(q, freq) };
        let mut keep = Vec::new();
        for p in std::mem::take(&mut self.done) {
            let b = p.borrow();
            if b.waiting_for_stop && !all {
                drop(b);
                keep.push(p);
                continue;
            }
            let state = if b.failed {
                "failed"
            } else if b.lost {
                "lost"
            } else if b.state == Final::Presented && b.screen != 0 {
                "displayed"
            } else {
                "discarded"
            };
            self.out.push(Present {
                pid: b.pid,
                tid: b.tid,
                start_ns: ns(b.start),
                stop_ns: if b.time_in_present > 0 { ns(b.start + b.time_in_present) } else { 0 },
                ready_ns: ns(b.ready),
                screen_ns: if state == "displayed" { ns(b.screen) } else { 0 },
                swapchain: b.swapchain,
                sync_interval: b.sync_interval,
                mode: b.mode.name().to_string(),
                state: state.to_string(),
            });
        }
        self.done = keep;
    }

    fn new_present(&mut self, ts: u64, pid: u32, tid: u32) -> Ptr {
        self.serial += 1;
        Rc::new(RefCell::new(P { serial: self.serial, start: ts, pid, tid, ..P::default() }))
    }

    fn set_screen(p: &Ptr, t: u64) {
        let mut b = p.borrow_mut();
        if b.screen == 0 {
            b.screen = t;
            b.state = Final::Presented;
        }
    }

    fn set_thread(&mut self, tid: u32, p: &Ptr) {
        if let Some(old) = self.by_thread.get(&tid).cloned()
            && !Rc::ptr_eq(&old, p)
        {
            self.remove_lost(&old);
        }
        self.by_thread.insert(tid, p.clone());
    }

    fn track(&mut self, p: &Ptr) {
        if let Some(old) = self.ring[self.next_ring].clone() {
            self.remove_lost(&old);
        }
        p.borrow_mut().ring = Some(self.next_ring);
        self.ring[self.next_ring] = Some(p.clone());
        self.next_ring = (self.next_ring + 1) % RING;
        let (pid, start, serial, tid) = {
            let b = p.borrow();
            (b.pid, b.start, b.serial, b.tid)
        };
        self.by_process.entry(pid).or_default().insert((start, serial), p.clone());
        self.set_thread(tid, p);
    }

    fn find_or_create(&mut self, ts: u64, pid: u32, tid: u32) -> Ptr {
        if let Some(p) = self.by_thread.get(&tid) {
            return p.clone();
        }
        let deferred = self.by_process.get(&pid).and_then(|m| {
            m.values()
                .find(|p| {
                    let b = p.borrow();
                    b.driver_tid == 0 && !b.seen_dxgk_present && !b.seen_win32k && b.mode == Mode::Unknown
                })
                .cloned()
        });
        if let Some(p) = deferred {
            p.borrow_mut().driver_tid = tid;
            self.set_thread(tid, &p);
            return p;
        }
        let p = self.new_present(ts, pid, tid);
        self.track(&p);
        p
    }

    fn find_by_submit(&self, seq: u32) -> Option<Ptr> {
        self.by_submit.get(&seq).and_then(|v| v.iter().min_by_key(|(_, p)| p.borrow().start).map(|(_, p)| p.clone()))
    }

    fn remove_from_submit(&mut self, p: &Ptr) {
        let seq = p.borrow().submit_seq;
        if seq == 0 {
            return;
        }
        if let Some(v) = self.by_submit.get_mut(&seq) {
            v.retain(|(_, q)| !Rc::ptr_eq(q, p));
            if v.is_empty() {
                self.by_submit.remove(&seq);
            }
        }
        p.borrow_mut().submit_seq = 0;
    }

    fn stop_tracking(&mut self, p: &Ptr) {
        let (ring, waiting, tid, driver_tid, pid, start, serial, w32k, token, token_data, in_dwm) = {
            let b = p.borrow();
            (b.ring, b.waiting_for_stop, b.tid, b.driver_tid, b.pid, b.start, b.serial, b.w32k, b.dxgk_token, b.dxgk_token_data, b.in_dwm_waiting)
        };
        if let Some(r) = ring {
            self.ring[r] = None;
        }
        let drop_thread = |t: u32, map: &mut HashMap<u32, Ptr>| {
            if map.get(&t).is_some_and(|q| Rc::ptr_eq(q, p)) {
                map.remove(&t);
            }
        };
        if !waiting {
            drop_thread(tid, &mut self.by_thread);
        }
        if driver_tid != 0 {
            drop_thread(driver_tid, &mut self.by_thread);
        }
        if let Some(m) = self.by_process.get_mut(&pid) {
            m.remove(&(start, serial));
        }
        self.remove_from_submit(p);
        if w32k != (0, 0, 0) && self.by_w32k.get(&w32k).is_some_and(|q| Rc::ptr_eq(q, p)) {
            self.by_w32k.remove(&w32k);
        }
        if token != 0 && self.by_dxgk_token.get(&token).is_some_and(|q| Rc::ptr_eq(q, p)) {
            self.by_dxgk_token.remove(&token);
        }
        if token_data != 0 && self.by_dxgk_token_data.get(&token_data).is_some_and(|q| Rc::ptr_eq(q, p)) {
            self.by_dxgk_token_data.remove(&token_data);
        }
        self.last_by_window.retain(|_, q| !Rc::ptr_eq(q, p));
        if in_dwm {
            self.waiting_for_dwm.retain(|q| !Rc::ptr_eq(q, p));
        }
        let mut b = p.borrow_mut();
        b.ring = None;
        b.w32k = (0, 0, 0);
        b.dxgk_token = 0;
        b.dxgk_token_data = 0;
        b.hwnd = 0;
        b.in_dwm_waiting = false;
    }

    fn remove_lost(&mut self, p: &Ptr) {
        p.borrow_mut().lost = true;
        let (completed, waiting, tid, driver_tid) = {
            let b = p.borrow();
            (b.completed, b.waiting_for_stop, b.tid, b.driver_tid)
        };
        if completed {
            if waiting {
                p.borrow_mut().waiting_for_stop = false;
                for t in [tid, driver_tid] {
                    if t != 0 && self.by_thread.get(&t).is_some_and(|q| Rc::ptr_eq(q, p)) {
                        self.by_thread.remove(&t);
                    }
                }
            }
        } else {
            self.complete(p);
        }
    }

    fn complete(&mut self, p: &Ptr) {
        // Presents that started before every provider ran may have missed events, so at the first
        // completion they are dropped (PresentMon drops every present in flight then). Only those:
        // a present that started after the session was up is whole, and dropping it would lose the
        // first frame of an app that was idle until its first measured input.
        if !self.has_completed && !p.borrow().lost {
            let armed = self.armed.load(Ordering::Acquire);
            let pids: Vec<u32> = self.by_process.keys().copied().collect();
            for pid in pids {
                for _ in 0..100_000 {
                    let first = self.by_process.get(&pid).and_then(|m| m.values().next().cloned()).filter(|q| armed == 0 || q.borrow().start < armed);
                    let Some(first) = first else { break };
                    self.remove_lost(&first);
                    if let Some(m) = self.by_process.get_mut(&pid) {
                        let (s, n) = (first.borrow().start, first.borrow().serial);
                        m.remove(&(s, n));
                    }
                }
            }
            // Before the session is up every present may still be partial: the next completion
            // decides again.
            if armed == 0 {
                return;
            }
            self.has_completed = true;
            if p.borrow().lost {
                return;
            }
        }
        if p.borrow().completed {
            return;
        }
        {
            let mut b = p.borrow_mut();
            b.completed = true;
            if !b.lost && b.dxgi && b.time_in_present == 0 {
                b.waiting_for_stop = true;
            }
        }
        let dependents = std::mem::take(&mut p.borrow_mut().dependents);
        if !dependents.is_empty() {
            let (lost, state, screen) = {
                let b = p.borrow();
                (b.lost, b.state, b.screen)
            };
            if lost {
                for d in &dependents {
                    d.borrow_mut().lost = true;
                }
            } else {
                // One present per surface reaches the screen per composed frame: the newest.
                let mut surfaces = HashSet::new();
                for d in dependents.iter().rev() {
                    let mut b = d.borrow_mut();
                    if b.completed {
                        continue;
                    }
                    let key = surface(&b);
                    if matches!(b.mode, Mode::ComposedFlip | Mode::ComposedCopyGpu | Mode::ComposedCopyCpu) && key.is_some_and(|k| !surfaces.insert(k)) {
                        b.state = Final::Discarded;
                        b.screen = 0;
                    } else if b.state != Final::Discarded {
                        b.state = state;
                        if screen != 0 {
                            b.screen = screen;
                        }
                    }
                }
            }
            for d in dependents.iter().rev() {
                if !d.borrow().completed {
                    self.complete(d);
                }
            }
        }
        // Displayed: every earlier present of the same swapchain is done (never shown).
        let (state, pid, start, swapchain, lost) = {
            let b = p.borrow();
            (b.state, b.pid, b.start, b.swapchain, b.lost)
        };
        if state == Final::Presented {
            let earlier: Vec<Ptr> = self.by_process.get(&pid).map(|m| m.range(..(start, 0)).map(|(_, q)| q.clone()).collect()).unwrap_or_default();
            for q in earlier {
                if q.borrow().swapchain == swapchain && !q.borrow().completed {
                    if lost {
                        q.borrow_mut().lost = true;
                    }
                    self.complete(&q);
                }
            }
        }
        self.stop_tracking(p);
        if let Some(l) = &self.live {
            let b = p.borrow();
            let screen = if b.state == Final::Presented && b.screen != 0 && !b.lost { session::ns(b.screen, self.freq) } else { 0 };
            l.push(b.pid, session::ns(b.start, self.freq), screen);
        }
        self.done.push(p.clone());
    }

    fn dxgi(&mut self, e: &EVENT_RECORD) {
        let (ts, pid, tid) = hdr(e);
        match e.EventHeader.EventDescriptor.Id {
            id::PRESENT_START | id::PRESENT_MPO_START => {
                let [sc, flags, sync] = self.schema.fields(e, ["pIDXGISwapChain", "Flags", "SyncInterval"]);
                let flags = uint(e, &flags) as u32;
                if flags & DXGI_PRESENT_TEST != 0 {
                    return;
                }
                let p = self.new_present(ts, pid, tid);
                {
                    let mut b = p.borrow_mut();
                    b.dxgi = true;
                    b.swapchain = uint(e, &sc);
                    b.flags = flags;
                    b.sync_interval = uint(e, &sync) as i32;
                }
                self.track(&p);
            }
            id::PRESENT_STOP | id::PRESENT_MPO_STOP => {
                let [result] = self.schema.fields(e, ["Result"]);
                let result = uint(e, &result) as u32;
                let Some(p) = self.by_thread.get(&tid).cloned() else { return };
                let waiting = {
                    let mut b = p.borrow_mut();
                    b.dxgi = true;
                    b.time_in_present = ts.saturating_sub(b.start).max(1);
                    std::mem::replace(&mut b.waiting_for_stop, false)
                };
                if waiting {
                    self.by_thread.remove(&tid);
                    return;
                }
                if (result as i32) < 0 {
                    p.borrow_mut().failed = true;
                    self.complete(&p);
                    return;
                }
                // DXGI_STATUS_OCCLUDED, _NO_DESKTOP_ACCESS, _MODE_CHANGE_IN_PROGRESS: nothing reached the screen.
                if matches!(result, 0x087A_0001 | 0x087A_0005 | 0x087A_0008) {
                    p.borrow_mut().state = Final::Discarded;
                    self.complete(&p);
                    return;
                }
                self.by_thread.remove(&tid);
            }
            _ => {}
        }
    }

    fn flip(&mut self, ts: u64, pid: u32, tid: u32) -> Option<Ptr> {
        let p = loop {
            if let Some(p) = self.by_thread.get(&tid).cloned() {
                let (seen, seq, tip, dxgi, mode) = {
                    let b = p.borrow();
                    (b.seen_dxgk_present, b.submit_seq, b.time_in_present, b.dxgi, b.mode)
                };
                if seen || (seq != 0 && tip != 0 && dxgi) {
                    self.remove_lost(&p);
                    if self.by_thread.get(&tid).is_some_and(|q| Rc::ptr_eq(q, &p)) {
                        self.by_thread.remove(&tid);
                    }
                    continue;
                }
                if dxgi {
                    if mode != Mode::Unknown {
                        return None;
                    }
                    break p;
                }
                self.by_thread.remove(&tid);
            }
            let p = self.new_present(ts, pid, tid);
            self.track(&p);
            break p;
        };
        p.borrow_mut().mode = Mode::LegacyFlip;
        if tid == self.dwm_tid {
            let waiting: Vec<Ptr> = self.waiting_for_dwm.drain(..).collect();
            for w in &waiting {
                w.borrow_mut().in_dwm_waiting = false;
            }
            p.borrow_mut().dependents = waiting;
        }
        Some(p)
    }

    fn present_history(&mut self, ts: u64, pid: u32, tid: u32, token: u64, token_data: u64, model: u64) {
        let p = loop {
            let p = self.find_or_create(ts, pid, tid);
            if p.borrow().dxgk_token == 0 {
                break p;
            }
            self.remove_lost(&p);
            if self.by_thread.get(&tid).is_some_and(|q| Rc::ptr_eq(q, &p)) {
                self.by_thread.remove(&tid);
            }
        };
        {
            let mut b = p.borrow_mut();
            b.ready = 0;
            b.dxgk_token = token;
            if b.screen != 0 {
                b.screen = 0;
                b.state = Final::Unknown;
            }
        }
        if let Some(old) = self.by_dxgk_token.get(&token).cloned() {
            self.remove_lost(&old);
        }
        self.by_dxgk_token.insert(token, p.clone());
        let mode = p.borrow().mode;
        match mode {
            Mode::LegacyCopy => p.borrow_mut().mode = Mode::ComposedCopyGpu,
            Mode::Unknown => {
                if model == MODEL_REDIRECTED_COMPOSITION {
                    // A DirectComposition surface (atlas): its composition cannot be followed.
                    self.remove_lost(&p);
                    return;
                }
                p.borrow_mut().mode = Mode::ComposedFlip;
            }
            Mode::ComposedCopyCpu => {
                if token_data == 0 {
                    if !p.borrow().in_dwm_waiting {
                        p.borrow_mut().in_dwm_waiting = true;
                        self.waiting_for_dwm.push_back(p.clone());
                    }
                } else {
                    self.by_dxgk_token_data.insert(token_data, p.clone());
                    p.borrow_mut().dxgk_token_data = token_data;
                }
            }
            _ => {}
        }
    }

    fn queue_submit(&mut self, tid: u32, ctx: u64, seq: u32, packet: u64, present: bool) {
        if (packet == PACKET_MMIOFLIP || packet == PACKET_SOFTWARE || present)
            && let Some(p) = self.by_thread.get(&tid).cloned()
            && p.borrow().submit_seq == 0
        {
            p.borrow_mut().submit_seq = seq;
            self.by_submit.entry(seq).or_default().push((ctx, p));
        }
    }

    fn queue_complete(&mut self, ts: u64, ctx: u64, seq: u32) {
        let Some(p) = self.by_submit.get(&seq).and_then(|v| v.iter().find(|(c, _)| *c == ctx)).map(|(_, p)| p.clone()) else { return };
        let (mode, wait_flip, seen) = {
            let b = p.borrow();
            (b.mode, b.wait_for_flip, b.seen_dxgk_present)
        };
        if mode == Mode::LegacyCopy || (mode == Mode::LegacyFlip && !wait_flip) {
            {
                let mut b = p.borrow_mut();
                if b.ready == 0 {
                    b.ready = ts;
                }
            }
            Self::set_screen(&p, ts);
            if seen || mode != Mode::LegacyCopy {
                self.complete(&p);
            }
        }
    }

    fn dxgk(&mut self, e: &EVENT_RECORD) {
        let (ts, pid, tid) = hdr(e);
        let version = e.EventHeader.EventDescriptor.Version;
        match e.EventHeader.EventDescriptor.Id {
            id::PRESENT_HISTORY_START | id::PRESENT_HISTORY_DETAILED => {
                let [token, model, data] = self.schema.fields(e, ["Token", "Model", "TokenData"]);
                let model = uint(e, &model);
                if model != MODEL_REDIRECTED_GDI {
                    self.present_history(ts, pid, tid, uint(e, &token), uint(e, &data), model);
                }
            }
            id::FLIP => {
                let [interval, mmio] = self.schema.fields(e, ["FlipInterval", "MMIOFlip"]);
                let (interval, mmio) = (uint(e, &interval), uint(e, &mmio) != 0);
                if let Some(p) = self.flip(ts, pid, tid) {
                    let mut b = p.borrow_mut();
                    b.sync_interval = interval as i32;
                    if mmio {
                        b.wait_for_flip = true;
                    }
                }
            }
            id::INDEPENDENT_FLIP => {
                let [seq, interval] = self.schema.fields(e, ["SubmitSequence", "FlipInterval"]);
                if let Some(p) = self.find_by_submit(uint(e, &seq) as u32) {
                    let mut b = p.borrow_mut();
                    b.mode = Mode::IndependentFlip;
                    b.sync_interval = uint(e, &interval) as i32;
                }
            }
            id::FLIP_MPO => {
                let [source, layer] = self.schema.fields(e, ["VidPnSourceId", "LayerIndex"]);
                let layer_id = uint(e, &source) << 32 | uint(e, &layer);
                if let Some(p) = self.flip(ts, pid, tid) {
                    let mut b = p.borrow_mut();
                    b.wait_for_flip = true;
                    b.wait_for_mpo_flip = true;
                    if b.swapchain == 0 {
                        b.swapchain = layer_id;
                    }
                }
            }
            id::QUEUE_START => {
                let [packet, seq, ctx, present] = self.schema.fields(e, ["PacketType", "SubmitSequence", "hContext", "bPresent"]);
                self.queue_submit(tid, uint(e, &ctx), uint(e, &seq) as u32, uint(e, &packet), uint(e, &present) != 0);
            }
            id::QUEUE_START_2 => {
                let [ctx, seq] = self.schema.fields(e, ["hContext", "SubmitSequence"]);
                self.queue_submit(tid, uint(e, &ctx), uint(e, &seq) as u32, 4, false);
            }
            id::QUEUE_STOP => {
                let [ctx, seq] = self.schema.fields(e, ["hContext", "SubmitSequence"]);
                self.queue_complete(ts, uint(e, &ctx), uint(e, &seq) as u32);
            }
            id::MMIO_FLIP => {
                let [seq, flags] = self.schema.fields(e, ["FlipSubmitSequence", "Flags"]);
                if let Some(p) = self.find_by_submit(uint(e, &seq) as u32) {
                    let mode = {
                        let mut b = p.borrow_mut();
                        b.ready = ts;
                        if b.mode == Mode::ComposedFlip {
                            b.mode = Mode::IndependentFlip;
                        }
                        b.mode
                    };
                    if uint(e, &flags) & FLIP_IMMEDIATE != 0 {
                        Self::set_screen(&p, ts);
                        if mode == Mode::LegacyFlip {
                            self.complete(&p);
                        }
                    }
                }
            }
            id::MMIO_FLIP_MPO => {
                let [seq, status] = self.schema.fields(e, ["FlipSubmitSequence", "FlipEntryStatusAfterFlip"]);
                let seq = (uint(e, &seq) >> 32) as u32;
                if let Some(p) = self.find_by_submit(seq)
                    && version >= 2
                    && status.found
                {
                    let status = uint(e, &status);
                    if status == FLIP_WAIT_VSYNC || status == FLIP_WAIT_HSYNC {
                        p.borrow_mut().wait_for_sync_dpc = true;
                    }
                    if status != FLIP_WAIT_VSYNC && status != FLIP_WAIT_HSYNC {
                        Self::set_screen(&p, ts);
                        if p.borrow().mode == Mode::LegacyFlip {
                            self.complete(&p);
                        }
                    }
                }
            }
            id::VSYNC => {
                let [fence, target] = self.schema.fields(e, ["FlipFenceId", "VidPnTargetId"]);
                self.vsyncs.push((session::ns(ts, self.freq), uint(e, &target) as u32));
                let fence = uint(e, &fence);
                if fence != 0
                    && let Some(p) = self.find_by_submit((fence >> 32) as u32)
                {
                    Self::set_screen(&p, ts);
                    let (mode, mpo) = {
                        let b = p.borrow();
                        (b.mode, b.wait_for_mpo_flip)
                    };
                    if mode == Mode::LegacyFlip && !mpo {
                        self.complete(&p);
                    }
                }
            }
            id::VSYNC_MPO | id::HSYNC_MPO => {
                let vsync = e.EventHeader.EventDescriptor.Id == id::VSYNC_MPO;
                let addr_name = if vsync && version >= 1 { "PresentIdOrPhysicalAddress" } else { "ScannedPhysicalAddress" };
                let [planes, addrs, entries, seqs, target] = self.schema.fields(e, ["PlaneCount", addr_name, "FlipEntryCount", "FlipSubmitSequence", "VidPnTargetId"]);
                // Multi-plane drivers log their vertical blanks here instead of `VSyncDPC` (an HSync
                // is an immediate flip, not a blank).
                if vsync && target.found {
                    self.vsyncs.push((session::ns(ts, self.freq), uint(e, &target) as u32));
                }
                let (planes, entries) = (uint(e, &planes) as usize, uint(e, &entries) as usize);
                let active = (0..planes.min(addrs.count.max(1)).min(64)).filter(|i| uint_at(e, &addrs, *i) != 0).count();
                for i in 0..entries.min(seqs.count.max(1)).min(64) {
                    let s = uint_at(e, &seqs, i);
                    if s == 0 {
                        continue;
                    }
                    let Some(p) = self.find_by_submit((s >> 32) as u32) else { continue };
                    {
                        let mut b = p.borrow_mut();
                        if active > 1 && matches!(b.mode, Mode::IndependentFlip | Mode::ComposedFlip) {
                            b.mode = Mode::ComposedIndependentFlip;
                        }
                    }
                    if p.borrow().state != Final::Presented {
                        Self::set_screen(&p, ts);
                    }
                    self.complete(&p);
                }
            }
            id::PRESENT_INFO => {
                let Some(p) = self.by_thread.get(&tid).cloned() else { return };
                let [hwnd] = self.schema.fields(e, ["hWindow"]);
                let (dxgi, ptid, mode, screen) = {
                    let mut b = p.borrow_mut();
                    b.seen_dxgk_present = true;
                    if b.hwnd == 0 {
                        b.hwnd = uint(e, &hwnd);
                    }
                    (b.dxgi, b.tid, b.mode, b.screen)
                };
                if !dxgi || ptid != tid {
                    self.by_thread.remove(&tid);
                }
                if mode == Mode::LegacyCopy && screen != 0 {
                    self.complete(&p);
                }
            }
            id::PRESENT_HISTORY_INFO => {
                let [token] = self.schema.fields(e, ["Token"]);
                let token = uint(e, &token);
                let Some(p) = self.by_dxgk_token.remove(&token) else { return };
                let (mode, seen_w32k, key) = {
                    let mut b = p.borrow_mut();
                    b.ready = if b.ready == 0 { ts } else { b.ready.min(ts) };
                    (b.mode, b.seen_win32k, surface(&b))
                };
                if mode == Mode::ComposedFlip && !seen_w32k && !p.borrow().in_dwm_waiting {
                    p.borrow_mut().in_dwm_waiting = true;
                    self.waiting_for_dwm.push_back(p.clone());
                }
                if mode == Mode::ComposedCopyGpu
                    && let Some(k) = key
                {
                    self.last_by_window.insert(k, p.clone());
                }
            }
            id::BLIT => {
                let [hwnd, redirected] = self.schema.fields(e, ["hwnd", "bRedirectedPresent"]);
                let p = loop {
                    let p = self.find_or_create(ts, pid, tid);
                    if p.borrow().mode == Mode::Unknown {
                        break p;
                    }
                    self.remove_lost(&p);
                    if self.by_thread.get(&tid).is_some_and(|q| Rc::ptr_eq(q, &p)) {
                        self.by_thread.remove(&tid);
                    }
                };
                let mut b = p.borrow_mut();
                b.hwnd = uint(e, &hwnd);
                b.mode = if uint(e, &redirected) != 0 { Mode::ComposedCopyCpu } else { Mode::LegacyCopy };
            }
            id::BLIT_CANCEL => {
                if let Some(p) = self.by_thread.get(&tid).cloned() {
                    p.borrow_mut().state = Final::Discarded;
                    self.complete(&p);
                }
            }
            _ => {}
        }
    }

    fn win32k(&mut self, e: &EVENT_RECORD) {
        let (ts, pid, tid) = hdr(e);
        match e.EventHeader.EventDescriptor.Id {
            id::TOKEN_SURFACE => {
                let [luid, count, bind] = self.schema.fields(e, ["CompositionSurfaceLuid", "PresentCount", "BindId"]);
                let key = (uint(e, &luid), uint(e, &count), uint(e, &bind));
                let p = loop {
                    let p = self.find_or_create(ts, pid, tid);
                    if !p.borrow().seen_win32k {
                        break p;
                    }
                    self.remove_lost(&p);
                    if self.by_thread.get(&tid).is_some_and(|q| Rc::ptr_eq(q, &p)) {
                        self.by_thread.remove(&tid);
                    }
                };
                {
                    let mut b = p.borrow_mut();
                    b.mode = Mode::ComposedFlip;
                    b.seen_win32k = true;
                    b.w32k = key;
                    b.luid = key.0;
                }
                self.by_w32k.insert(key, p);
            }
            id::TOKEN_STATE => {
                let [luid, count, bind, state, iflip] = self.schema.fields(e, ["CompositionSurfaceLuid", "PresentCount", "BindId", "NewState", "IndependentFlip"]);
                let key = (uint(e, &luid), uint(e, &count), uint(e, &bind));
                let Some(p) = self.by_w32k.get(&key).cloned() else { return };
                match uint(e, &state) {
                    TOKEN_IN_FRAME => {
                        let key = {
                            let mut b = p.borrow_mut();
                            b.seen_in_frame = true;
                            if uint(e, &iflip) != 0 && b.mode == Mode::ComposedFlip {
                                b.mode = Mode::IndependentFlip;
                            }
                            surface(&b)
                        };
                        // Composing a newer present of the surface discards the one it last showed
                        // that never reached the screen.
                        if let Some(key) = key {
                            match self.last_by_window.get(&key).cloned() {
                                None => {
                                    self.last_by_window.insert(key, p.clone());
                                }
                                Some(prev) if !Rc::ptr_eq(&prev, &p) => {
                                    self.last_by_window.insert(key, p.clone());
                                    if !prev.borrow().wait_for_sync_dpc {
                                        {
                                            let mut b = prev.borrow_mut();
                                            if b.screen == 0 {
                                                b.state = Final::Discarded;
                                            }
                                        }
                                        self.remove_from_submit(&prev);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    TOKEN_CONFIRMED => {
                        let key = {
                            let mut b = p.borrow_mut();
                            if b.state == Final::Unknown && b.flags & DXGI_PRESENT_DO_NOT_SEQUENCE != 0 {
                                b.state = Final::Discarded;
                            }
                            surface(&b)
                        };
                        if p.borrow().state == Final::Discarded {
                            self.remove_from_submit(&p);
                        }
                        if let Some(key) = key {
                            self.last_by_window.remove(&key);
                        }
                    }
                    TOKEN_DISCARDED => {
                        self.by_w32k.remove(&key);
                        let (in_frame, state, mode) = {
                            let mut b = p.borrow_mut();
                            b.w32k = (0, 0, 0);
                            (b.seen_in_frame, b.state, b.mode)
                        };
                        if !in_frame && state == Final::Unknown {
                            p.borrow_mut().state = Final::Discarded;
                            self.complete(&p);
                        } else if mode != Mode::ComposedFlip {
                            self.complete(&p);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn dwm(&mut self, e: &EVENT_RECORD) {
        let (_, _, tid) = hdr(e);
        match e.EventHeader.EventDescriptor.Id {
            id::GET_PRESENT_HISTORY => {
                for p in std::mem::take(&mut self.last_by_window).into_values() {
                    let ok = {
                        let b = p.borrow();
                        matches!(b.mode, Mode::ComposedCopyGpu | Mode::ComposedCopyCpu) && !b.in_dwm_waiting
                    };
                    if ok {
                        p.borrow_mut().in_dwm_waiting = true;
                        self.waiting_for_dwm.push_back(p);
                    }
                }
            }
            id::SCHEDULE_PRESENT => self.dwm_tid = tid,
            id::FLIPCHAIN_PENDING | id::FLIPCHAIN_COMPLETE | id::FLIPCHAIN_DIRTY => {
                let [chain, serial, hwnd] = self.schema.fields(e, ["ulFlipChain", "ulSerialNumber", "hwnd"]);
                let data = (uint(e, &chain) & 0xffff_ffff) << 32 | (uint(e, &serial) & 0xffff_ffff);
                if let Some(p) = self.by_dxgk_token_data.remove(&data) {
                    p.borrow_mut().dxgk_token_data = 0;
                    let hwnd = uint(e, &hwnd);
                    let key = if hwnd != 0 { Some(Surface::Window(hwnd)) } else { surface(&p.borrow()) };
                    if let Some(key) = key {
                        self.last_by_window.insert(key, p);
                    }
                }
            }
            id::SCHEDULE_SURFACEUPDATE => {
                let [luid, count, old_count, bind]: [Field; 4] = self.schema.fields(e, ["luidSurface", "PresentCount", "OutOfFrameDirectFlipPresentCount", "bindId"]);
                let count = if count.found { uint(e, &count) } else { uint(e, &old_count) };
                let key = (uint(e, &luid), count, uint(e, &bind));
                if let Some(p) = self.by_w32k.get(&key).cloned() {
                    let ok = {
                        let b = p.borrow();
                        b.seen_in_frame && !b.in_dwm_waiting
                    };
                    if ok {
                        p.borrow_mut().in_dwm_waiting = true;
                        self.waiting_for_dwm.push_back(p);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker::new()
    }
}
