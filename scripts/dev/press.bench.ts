// Dev bench for the press edge and ctx.untimed (not shipped). Controls that act on the mouse press and
// controls that act on the release, in a native Cocoa app (macOS) and in a page (every engine), with
// `ctx.untimed` around the waits between clicks. Run from the repo root:
//   swiftc -O scripts/dev/press-app.swift -o /tmp/strobe-press-app     (macOS, for the native target)
//   strobe bench scripts/dev/press.bench.ts --head . --check --target firefox      (or chromium, webkit)
//   strobe bench scripts/dev/press.bench.ts --head . --check --target cocoa
// `interaction.p50` prints how many of its samples were timed from a press (`interaction.n.press`):
// all of the press cases', none of the release and feedback cases'. `untimed` waits 1 s untimed in a 0.2 s run.
import { bench, native, url } from "@hona/strobe"

const page = `<!doctype html><title>press</title>
<style>body{font:14px system-ui;display:flex;gap:8px;margin:8px}nav{display:flex;flex-direction:column;gap:4px}
button{width:140px;height:28px}#pane{width:440px;height:380px}</style>
<nav><button id="press-a">press-a</button><button id="press-b">press-b</button>
<button id="release-a">release-a</button><button id="release-b">release-b</button>
<button id="feedback-a">feedback-a</button><button id="feedback-b">feedback-b</button></nav>
<div><p id="label">pane: none</p><div id="pane"></div></div>
<script>
const names = ["press-a", "press-b", "release-a", "release-b", "feedback-a", "feedback-b"]
const colours = ["#c33", "#3a3", "#36c", "#e80", "#939", "#099"]
const show = (name) => {
  label.textContent = "pane: " + name
  pane.style.background = colours[names.indexOf(name)]
}
for (const b of document.querySelectorAll("button")) {
  b.addEventListener(b.id.startsWith("press") ? "mousedown" : "click", () => show(b.id))
  // Press feedback in the DOM, as a ripple or a data-pressed look: the click still acts on its release.
  if (b.id.startsWith("feedback")) {
    b.addEventListener("pointerdown", () => { b.dataset.pressed = "" })
    b.addEventListener("pointerup", () => { delete b.dataset.pressed })
  }
}
</script>`

const viewport = { width: 640, height: 480 }
// The pane names the item a click switched to: its accessibility label on the native app, its text on a page.
const pane = (name: string) => ({ selector: "pane", text: `pane: ${name}` })
const onPage = (name: string) => ({ selector: "#label", text: `pane: ${name}` })

/**
 * Two clicks a run on items of one kind, each waited for, with the app left to go idle untimed between.
 * `feedback`: items that act on the release with press feedback in the model (the native app's take
 * keyboard focus on the press; the page's set `data-pressed` on pointerdown), which stay release samples.
 */
const clicks = (kind: "press" | "release" | "feedback") => async ({ page, target, untimed }: any) => {
  const name = kind === "feedback" && target === "cocoa" ? "focus" : kind
  for (const item of [`${name}-a`, `${name}-b`]) {
    const sel = target === "cocoa" ? item : `#${item}`
    await page.click(sel, { until: target === "cocoa" ? pane(item) : onPage(item) })
    await untimed(() => page.idle())
  }
}

/** Six clicks, each waited for, alternating between two items that act on the press, the app left to go idle between (untimed when `timed`). */
const six = async ({ page, target, untimed }: any, timed: boolean) => {
  for (let i = 0; i < 6; i++) {
    const item = i % 2 ? "press-b" : "press-a"
    await page.click(target === "cocoa" ? item : `#${item}`, { until: target === "cocoa" ? pane(item) : onPage(item) })
    await (timed ? untimed(() => page.idle()) : page.idle())
  }
}

export default bench({
  targets: {
    cocoa: native("/tmp/strobe-press-app", { viewport }),
    chromium: url(`data:text/html,${encodeURIComponent(page)}`, { viewport, engine: "chromium" }),
    firefox: url(`data:text/html,${encodeURIComponent(page)}`, { viewport, engine: "firefox" }),
    webkit: url(`data:text/html,${encodeURIComponent(page)}`, { viewport, engine: "webkit" }),
  },
  cases: {
    press: { primary: "interaction.p50", metrics: ["interaction.p95", "run.duration", "cpu.work"], run: clicks("press") },
    release: { primary: "interaction.p50", metrics: ["interaction.p95", "run.duration", "cpu.work"], run: clicks("release") },
    feedback: { primary: "interaction.p50", metrics: ["interaction.p95", "run.duration", "cpu.work"], run: clicks("feedback") },
    // 0.2 s timed around a 1 s untimed wait: run.duration reads about 200 ms, and the CPU of the wait leaves
    // cpu.work. Totals with no reading at the stretch's edges are refused Untimed (gpu.process.3d; net.requests on a
    // page).
    untimed: {
      primary: "run.duration",
      metrics: ["cpu.work", "cpu.app.total", "ui.cpu", "phase.all", "gpu.process.3d", "io.read", "net.requests"],
      async run({ page, phase, untimed }) {
        await phase("all", async () => {
          await page.wait(100)
          await untimed(() => page.wait(1000))
          await page.wait(100)
        })
      },
    },
    // Six clicks untimed, then six timed: the untimed ones' CPU and frames leave the run, and they are no
    // interaction samples (six, not twelve).
    "clicks-untimed": {
      primary: "cpu.work",
      metrics: ["ui.cpu", "interaction.p50", "frames.presented.count", "run.duration"],
      async run(ctx: any) {
        await ctx.untimed(() => six(ctx, false))
        await six(ctx, true)
      },
    },
    "clicks-timed": {
      primary: "cpu.work",
      metrics: ["ui.cpu", "interaction.p50", "frames.presented.count", "run.duration"],
      async run(ctx: any) {
        await six(ctx, true)
        await six(ctx, true)
      },
    },
    // A page busy for 0.5 s untimed (pages only): its CPU leaves cpu.work (a few ms, not 500).
    "untimed-cpu": {
      primary: "cpu.work",
      metrics: ["run.duration", "main.work", "main.busy"],
      targets: ["chromium", "firefox", "webkit"],
      async run({ page, untimed }) {
        await page.wait(100)
        await untimed(() => page.eval(() => { const t = performance.now(); while (performance.now() - t < 500); }))
      },
    },
    // The same run timed whole: its cpu.work holds the 0.5 s.
    "timed-cpu": {
      primary: "cpu.work",
      metrics: ["run.duration", "main.work", "main.busy"],
      targets: ["chromium", "firefox", "webkit"],
      async run({ page }) {
        await page.wait(100)
        await page.eval(() => { const t = performance.now(); while (performance.now() - t < 500); })
      },
    },
  },
})
