// A fake native app speaking native-bridge-v1 (dev only, not shipped): exercises strobe's bridge server,
// native driver and ui.* metrics without a real toolkit. In window mode it opens a classic Win32 window as
// its child (charmap) so strobe's window checks have something to see; it draws nothing itself.
//
// Env: STROBE_BRIDGE, STROBE_BRIDGE_TOKEN (from strobe), FAKE_MODE=window|headless, FAKE_DEBUG=1,
// FAKE_SKIP=<n> (skipped frames per run), FAKE_UNPACED=<n>, FAKE_GPU=<adapter name>, FAKE_PRESENT=1
// (report present_ns), FAKE_FRAME_MS=<ms of work per frame> (default 2).
import { connect } from "node:net"
import { createInterface } from "node:readline"
import { spawn } from "node:child_process"
import { dlopen, FFIType, ptr } from "bun:ffi"

// strobe's clock: QueryPerformanceCounter in ns (Bun's hrtime starts at the process).
const k32 = dlopen("kernel32.dll", {
  QueryPerformanceCounter: { args: [FFIType.ptr], returns: FFIType.i32 },
  QueryPerformanceFrequency: { args: [FFIType.ptr], returns: FFIType.i32 },
})
const qpc = new BigInt64Array(1)
k32.symbols.QueryPerformanceFrequency(ptr(qpc))
const freq = qpc[0]
const now = () => {
  k32.symbols.QueryPerformanceCounter(ptr(qpc))
  return (qpc[0] * 1_000_000_000n) / freq
}
const env = process.env
const mode = env.FAKE_MODE ?? "window"
const frameMs = Number(env.FAKE_FRAME_MS ?? 2)
// FAKE_NOWINDOW=1: a tray app, no window at all. It says when it is up on stdout (a launch milestone).
if (mode === "window" && !env.FAKE_NOWINDOW) spawn("C:/Windows/System32/charmap.exe", [], { stdio: "ignore" })
setTimeout(() => console.log("fake ready"), 80)

const [host, port] = (env.STROBE_BRIDGE ?? "").split(":")
if (!port) {
  console.log("no STROBE_BRIDGE: running without strobe")
  setInterval(() => {}, 1 << 30)
} else {
  const socket = connect(Number(port), host)
  socket.setNoDelay(true)
  const send = (m: object) => socket.write(JSON.stringify(m, (_, v) => (typeof v === "bigint" ? Number(v) : v)) + "\n")

  // The fake UI: element keys that show, a frame counter, the frame ring and counters since run.begin.
  const shown = new Set(["nav-general", "nav-history", "general-pane", "app-ready"])
  let seq = 0
  let lastCommit = 0n
  let ring: (number | bigint)[][] = []
  let marks: [string, bigint][] = []
  let work = { wall_ms: 0, cpu_ms: 0, view_ms: 0, layout_ms: 0, text_ms: 0, paint_ms: 0, encode_ms: 0, other_ms: 0 }
  let counts = { frames: 0, commits: 0, elements: shown.size, glyphs_shaped: 0 }
  let animating = 0
  const waiters: { test: () => boolean; resolve: () => void }[] = []

  // One frame: busy for frameMs on this thread (the fake's UI work), then a row in the ring.
  const frame = (input: bigint) => {
    const begin = now()
    const until = begin + BigInt(Math.round(frameMs * 1e6))
    let x = 0
    while (now() < until) x++
    const cpuEnd = now()
    const submit = now()
    seq++
    lastCommit = submit
    const present = env.FAKE_PRESENT ? submit + 6_000_000n : 0n
    ring.push([seq, 0, process.pid, begin, cpuEnd, submit, present, input])
    const ms = Number(submit - begin) / 1e6
    work.wall_ms += ms
    work.cpu_ms += ms
    work.view_ms += ms * 0.3
    work.layout_ms += ms * 0.3
    work.text_ms += ms * 0.1
    work.paint_ms += ms * 0.2
    work.encode_ms += ms * 0.1
    counts.frames++
    counts.commits++
    counts.elements = shown.size
    counts.glyphs_shaped += 10 + (x & 0)
    for (const w of [...waiters]) if (w.test()) {
      waiters.splice(waiters.indexOf(w), 1)
      w.resolve()
    }
  }
  const key = (s: any) => (typeof s === "string" ? s : s?.selector ?? "").replace(/:focus$/, "")
  const wait = (test: () => boolean, ms: number) =>
    new Promise<void>((resolve, reject) => {
      if (test()) return resolve()
      const w = { test, resolve: () => (clearTimeout(t), resolve()) }
      const t = setTimeout(() => (waiters.splice(waiters.indexOf(w), 1), reject({ code: "timeout", message: `timed out after ${ms} ms` })), ms)
      waiters.push(w)
    })
  const until = async (u: any, ms = 30000) => {
    if (u === undefined || u === null) return
    if (u === "frame") {
      const s = seq
      setTimeout(() => frame(0n), 1)
      return wait(() => seq > s, ms)
    }
    if (u === "idle") return new Promise((r) => setTimeout(r, 300))
    return wait(() => seq > 0 && shown.has(key(u)), ms)
  }
  // An input: stamped when it enters the runtime, a frame that reflects it, then its `until`.
  const input = async (a: any, change?: () => void) => {
    if (a.selector && !shown.has(key(a.selector))) await wait(() => shown.has(key(a.selector)), a.timeout_ms ?? 30000)
    const at = now()
    change?.()
    frame(at)
    await until(a.until, a.timeout_ms)
    return { input_ns: [at], frame_seq: seq }
  }

  const ops: Record<string, (a: any) => Promise<any> | any> = {
    info: () => ({
      gpu: { name: env.FAKE_GPU ?? "Fake Discrete GPU", backend: "Vulkan", device_type: env.FAKE_GPU?.includes("Basic") ? "Cpu" : "DiscreteGpu", driver: "1.0", vendor: 0 },
      surfaces: [{ key: "main", kind: "window", visible: true, occluded: false, size: [800, 600], scale: 1.5, present_mode: "AutoVsync", refresh_hz: 165 }],
    }),
    query: (a) => ({ elements: shown.has(key(a.selector)) ? [{ key: key(a.selector), path: key(a.selector), role: "button", label: "", text: "", rect: [0, 0, 10, 10], focused: false, enabled: true }] : [] }),
    wait: async (a) => {
      const before = shown.has(key(a.until)) && seq > 0
      await until(a.until, a.timeout_ms)
      return { at_ns: lastCommit, frame_seq: seq, already: before }
    },
    click: (a) =>
      input(a, () => {
        const k = key(a.selector)
        if (k.startsWith("nav-")) {
          for (const s of [...shown]) if (s.endsWith("-pane")) shown.delete(s)
          shown.add(k.slice(4) + "-pane")
          if (k === "nav-history") shown.add("history-search")
        }
      }),
    hover: (a) => input(a),
    move: (a) => input(a),
    down: (a) => input(a),
    up: (a) => input(a),
    press: (a) => input(a),
    type: async (a) => {
      const stamps: bigint[] = []
      for (const _ of a.text ?? "") {
        const at = now()
        stamps.push(at)
        frame(at)
        if (a.delay_ms) await new Promise((r) => setTimeout(r, a.delay_ms))
      }
      await until(a.until, a.timeout_ms)
      return { input_ns: stamps, frame_seq: seq }
    },
    wheel: (a) => input(a),
    scroll: async (a) => {
      // One wheel step per frame at the given speed, 6 ms apart.
      const steps = Math.max(1, Math.round((Math.abs(a.by ?? 0) / (a.speed ?? 3000)) * 165))
      const stamps: bigint[] = []
      for (let i = 0; i < steps; i++) {
        const at = now()
        stamps.push(at)
        frame(at)
        await new Promise((r) => setTimeout(r, 6))
      }
      await until(a.until, a.timeout_ms)
      return { input_ns: stamps, frame_seq: seq }
    },
    window: () => ({}),
    mark: (a) => {
      const at = now()
      marks.push([a.name, at])
      return { at_ns: at }
    },
    "run.begin": () => {
      ring = []
      marks = []
      work = { wall_ms: 0, cpu_ms: 0, view_ms: 0, layout_ms: 0, text_ms: 0, paint_ms: 0, encode_ms: 0, other_ms: 0 }
      counts = { frames: 0, commits: 0, elements: shown.size, glyphs_shaped: 0 }
      return { at_ns: now() }
    },
    "run.end": () => ({
      t0_ns: ring[0]?.[3] ?? now(),
      t1_ns: now(),
      work,
      counts,
      surfaces: { main: { redraws: counts.frames, unpaced: Number(env.FAKE_UNPACED ?? 0), presents: counts.frames, skipped: Number(env.FAKE_SKIP ?? 0), occluded: 0, timeouts: 0, outdated: 0, turns: counts.frames + 2, wakes: 1 } },
      frame_columns: ["seq", "surface", "tid", "begin_ns", "cpu_end_ns", "submit_ns", "present_ns", "input_ns"],
      frames: ring,
      marks,
    }),
    snapshot: async (a) => {
      // A 1x1 RGBA PNG: what a toolkit's offscreen render would write.
      await Bun.write(a.path, Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==", "base64"))
      return { path: a.path, width: 1, height: 1 }
    },
    step: (a) => {
      for (let i = 0; i < (a.frames ?? 1); i++) frame(0n)
      return { virtual_s: seq / 60 }
    },
    quit: () => {
      setTimeout(() => process.exit(0), 20)
      return {}
    },
  }

  socket.on("connect", () => {
    send({ hello: 1, token: env.STROBE_BRIDGE_TOKEN, pid: process.pid, toolkit: "fake", toolkit_version: "0.0.1", mode, debug_build: !!env.FAKE_DEBUG, clock: "qpc", now_ns: now() })
    // The first frame commits a moment after connecting, as a toolkit connects before its first frame.
    setTimeout(() => frame(0n), 50)
    // An idle app still wakes a little: a 250 ms timer, like a tray app's tick.
    setInterval(() => animating++, 250)
  })
  socket.on("close", () => process.exit(0))
  createInterface({ input: socket }).on("line", async (line) => {
    const m = JSON.parse(line)
    const op = ops[m.op]
    if (!op) return send({ id: m.id, err: { code: "unsupported", message: `the fake has no ${m.op}` } })
    try {
      send({ id: m.id, ok: (await op(m.args ?? {})) ?? {} })
    } catch (e: any) {
      send({ id: m.id, err: { code: e?.code ?? "internal", message: e?.message ?? String(e) } })
    }
  })
}
