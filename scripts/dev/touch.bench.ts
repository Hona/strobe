// Dev bench for touch input on an emulated phone (not shipped). Run from the repo root, headless (a phone is):
//   strobe bench scripts/dev/touch.bench.ts --head . --check --headless [--engine firefox]
// Each case asserts what the page got and counts it (`count.<event>_<pointerType>`): a tap is a touch start, a touch
// pointer down and up, a touch end and a click, never a mouse pointer; a drag moves the knob; a scroll swipes the
// page or the list (Chromium; Firefox refuses EngineUnsupported: headless, it does not pan on touch). `hover`,
// `wheel` and `mouse down` are refused DeviceMismatch, before anything is sent. `count.clickdelay` is how long after
// the lift the click came (`fixed`: a page that cannot zoom, so no double-tap wait could apply).
import { bench, url } from "@hona/strobe"

const page = new URL("pages/touch.html", import.meta.url).href
const events = ["pointerdown", "pointerup", "pointercancel", "touchstart", "touchend", "mousedown", "mouseup", "click", "dblclick"]
const kinds = ["touch", "mouse"]
const metrics = ["interaction.n", "interaction.p50", ...events.flatMap((e) => kinds.map((k) => `count.${e}_${k}`)), "count.clickdelay"]
const seen = (p: any): Promise<string[]> => p.eval(() => (window as any).seen.splice(0))
const reset = async ({ page }: any) => {
  await page.eval(() => {
    document.getElementById("tap")!.removeAttribute("data-tapped")
    const knob = document.getElementById("knob")!
    knob.style.left = "10px"
    knob.removeAttribute("data-dragged")
    document.getElementById("list")!.scrollTop = 0
    ;(document.activeElement as HTMLElement | null)?.blur()
    document.getElementById("field")!.setAttribute("value", "")
    ;(document.getElementById("field") as HTMLInputElement).value = ""
    scrollTo({ top: 0, behavior: "instant" })
    ;(window as any).seen = []
  })
}
const expect = (got: string[], want: string[], not: string[] = []) => {
  for (const w of want) if (!got.includes(w)) throw new Error(`the page got no ${w}: ${got.join(" ")}`)
  for (const n of not) if (got.some((g) => g.startsWith(n))) throw new Error(`the page got ${n}: ${got.join(" ")}`)
}
const tapped = ["touchstart:touch", "pointerdown:touch", "pointerup:touch", "touchend:touch", "click:touch"]

export default bench({
  targets: { phone: url(page, { emulate: "mobile" }), fixed: url(page + "?fixed", { emulate: "mobile" }), desktop: url(page) },
  cases: {
    tap: {
      targets: ["phone"],
      primary: "run.duration",
      metrics,
      run: async ({ page }) => {
        await page.click("#tap", { until: "#tap[data-tapped]" })
        expect(await seen(page), tapped, ["pointerdown:mouse"])
      },
      reset,
    },
    // The same tap on a page a person cannot zoom: `count.clickdelay` says how long the click waited after the lift.
    "tap fixed": {
      targets: ["fixed"],
      primary: "run.duration",
      metrics,
      run: async ({ page }) => {
        await page.click("#tap", { until: "#tap[data-tapped]" })
        expect(await seen(page), tapped, ["pointerdown:mouse"])
      },
      reset,
    },
    "double tap": {
      targets: ["phone"],
      primary: "run.duration",
      metrics,
      run: async ({ page }) => {
        await page.click("#tap", { count: 2, until: `#tap[data-tapped="2"]` })
        const got = await seen(page)
        if (got.filter((g) => g === "touchstart:touch").length !== 2) throw new Error(`two taps, not: ${got.join(" ")}`)
      },
      reset,
    },
    type: {
      targets: ["phone"],
      primary: "run.duration",
      metrics,
      run: async ({ page }) => {
        await page.type("#field", "hi")
        const v = await page.eval(() => (document.getElementById("field") as HTMLInputElement).value)
        if (v !== "hi") throw new Error(`typed ${JSON.stringify(v)}`)
        expect(await seen(page), ["touchstart:touch", "pointerdown:touch"], ["pointerdown:mouse"])
      },
      reset,
    },
    drag: {
      targets: ["phone"],
      primary: "run.duration",
      metrics: [...metrics, "input.rate", "input.gap.p95"],
      run: async ({ page }) => {
        await page.drag("#knob", [[0, 0], [150, 0]], { duration: 400, until: "#knob[data-dragged]" })
        const left = await page.eval(() => parseFloat(document.getElementById("knob")!.style.left))
        if (left < 120) throw new Error(`the knob moved to ${left}px, not ~160`)
        expect(await seen(page), ["touchstart:touch", "pointerdown:touch", "touchend:touch"], ["pointerdown:mouse"])
      },
      reset,
    },
    "scroll page": {
      targets: ["phone"],
      primary: "run.duration",
      metrics: [...metrics, "scroll.n", "scroll.latency.p50", "count.scrolled", "input.rate"],
      run: async ({ page, count }) => {
        await page.scroll("#tall", { by: 600 })
        const y = await page.eval(() => scrollY)
        count("scrolled", Math.round(y))
        // 600, give or take the touch slop and the lead-in (601 at 20 Hz, 617 at 60 Hz), and no fling past it.
        if (y < 570 || y > 630) throw new Error(`the page scrolled ${y}px, not ~600: ${(await seen(page)).join(" ")}`)
      },
      reset,
    },
    // A finger dragged up the page: the page scrolls with it (Chromium; Firefox refuses EngineUnsupported where it
    // would pan). On `#swiper`, whose touches the page prevents, Firefox drags too.
    "swipe page": {
      targets: ["phone"],
      primary: "run.duration",
      metrics: [...metrics, "input.rate", "input.gap.p95"],
      run: async ({ page }) => {
        await page.drag("#tall", [[0, 0], [0, -300]], { duration: 500 })
        const y = await page.eval(() => scrollY)
        if (y < 150) throw new Error(`the page scrolled ${y}px, not ~300: ${(await seen(page)).join(" ")}`)
      },
      reset,
    },
    "drag prevented": {
      targets: ["phone"],
      primary: "run.duration",
      metrics: [...metrics, "input.rate"],
      run: async ({ page }) => {
        await page.drag("#swiper", [[0, 0], [0, -60]], { duration: 300 })
        const y = await page.eval(() => scrollY)
        if (y !== 0) throw new Error(`the page scrolled ${y}px under a drag it prevents`)
      },
      reset,
    },
    "scroll list": {
      targets: ["phone"],
      primary: "run.duration",
      metrics: [...metrics, "scroll.n"],
      run: async ({ page }) => {
        await page.scroll("#list", { by: 400 })
        const y = await page.eval(() => document.getElementById("list")!.scrollTop)
        if (y < 250) throw new Error(`the list scrolled ${y}px, not ~400 (the page ${await page.eval(() => scrollY)}px): ${(await seen(page)).join(" ")}`)
      },
      reset,
    },
    // The same drag with a mouse on a desktop page: its pacing for comparison.
    "desktop drag": {
      targets: ["desktop"],
      primary: "run.duration",
      metrics: [...metrics, "input.rate", "input.gap.p95"],
      run: async ({ page }) => {
        await page.drag("#knob", [[0, 0], [150, 0]], { duration: 400, until: "#knob[data-dragged]" })
        expect(await seen(page), ["pointerdown:mouse"], ["pointerdown:touch", "touchstart"])
      },
      reset,
    },
    // Refused DeviceMismatch: a phone has no hover, no wheel and no mouse button.
    hover: { targets: ["phone"], primary: "run.duration", run: ({ page }) => page.hover("#tap") },
    wheel: { targets: ["phone"], primary: "run.duration", run: ({ page }) => page.wheel("#list", { dy: 100 }) },
    "mouse down": { targets: ["phone"], primary: "run.duration", run: ({ page }) => page.down("Mouse", { at: "#tap" }) },
  },
})
