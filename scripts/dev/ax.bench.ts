// Dev bench for the accessibility driver on macOS (not shipped): a small Cocoa app driven through the AX
// API and Quartz events posted to its process. Needs the Accessibility permission (and Screen Recording
// for frames) for the app strobe runs under; `strobe status` says which. TextEdit and Apple's other
// sandboxed apps keep their data in your ~/Library, so strobe does not launch them. Run from the repo root:
//   swiftc -O scripts/dev/cocoa-app.swift -o /tmp/strobe-cocoa-app
//   strobe bench scripts/dev/ax.bench.ts --head . --check
import { bench, native } from "@hona/strobe"

export default bench({
  targets: { cocoa: native("/tmp/strobe-cocoa-app", { viewport: { width: 640, height: 480 } }) },
  async setup({ page }) {
    console.error("buttons", JSON.stringify(await page.query("role=button")))
    await page.waitFor("add")
  },
  cases: {
    query: { primary: "cpu.work", run: async ({ page }) => void (await page.query("role=button")) },
    // Each click adds a row (the app has no way back), so its runs build on each other: no reset check.
    click: { primary: "cpu.work", metrics: ["ui.cpu", "interaction.p95"], resetCheck: false, run: ({ page }) => page.click("add", { until: { selector: "count", text: "rows: 1" } }) },
    type: {
      primary: "cpu.work",
      metrics: ["interaction.p95"],
      run: ({ page }) => page.type("field", "Ab+1 x", { until: { selector: "echo", text: "echo: Ab+1 x", exact: true } }),
      reset: async ({ page }) => {
        await page.act("field", "setValue", { value: "" })
        await page.waitFor("field", { text: "", exact: true })
      },
    },
    chord: {
      primary: "cpu.work",
      async run({ page }) {
        await page.type("field", "hello", { until: { selector: "echo", text: "echo: hello", exact: true } })
        await page.press("Meta+a", { selector: "field" })
        await page.press("Backspace", { selector: "field", until: { selector: "echo", text: "echo: ", exact: true } })
      },
    },
    toggle: { primary: "cpu.work", run: ({ page }) => page.act("check", "toggle", { until: "idle" }), reset: ({ page }) => page.act("check", "toggle", { until: "idle" }) },
    scroll: { primary: "cpu.work", metrics: ["scroll.latency.p95"], run: ({ page }) => page.scroll("scroll", { by: 600 }, { until: "idle" }) },
    wheel: { primary: "cpu.work", run: ({ page }) => page.wheel("scroll", { dy: -300 }, { until: "idle" }) },
    frames: { primary: "cpu.work", metrics: ["frames.presented.count", "interaction.p95"], resetCheck: false, run: ({ page }) => page.click("add", { until: "frame" }) },
    // Every element, with rects in the window's points (what page.move takes) and actionable by hit test.
    tree: {
      primary: "cpu.work",
      async run({ page }) {
        const all = await page.query()
        console.error("tree", JSON.stringify(all.filter((e) => e.key)))
        const add = all.find((e) => e.key === "add")
        if (!add?.actionable || add.rect[0] < 0 || add.rect[1] < 0 || add.rect[0] > 640) throw new Error(`add: ${JSON.stringify(add)}`)
        if ((await page.query("*", { actionable: true })).some((e) => !e.actionable)) throw new Error("actionable: a listed element is not")
        await page.move(add.rect[0] + add.rect[2] / 2, add.rect[1] + add.rect[3] / 2)
      },
    },
    // The window resized, maximized (to the display's work area, never a full-screen Space) and back in reset.
    window: {
      primary: "cpu.work",
      metrics: ["frames.presented.count"],
      async run({ page }) {
        await page.window({ resize: { width: 800, height: 500 } }, { until: "frame" })
        await page.window("maximize", { until: "frame" })
        await page.click("check", { until: "idle" })
      },
      reset: async ({ page }) => {
        await page.window("restore")
        await page.act("check", "toggle", { until: "idle" })
      },
    },
  },
})
